use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::component::{
    execution::{
        ApplicationHost, ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort,
        RenderedProjection,
    },
    prelude::*,
    ComponentHost,
};
use async_trait::async_trait;
use tokio::sync::Notify;

#[derive(Clone, Debug, PartialEq, Eq)]
enum TestDiagnostic {
    InvalidXml(XmlContractDiagnostic),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ContractState {
    value: Option<u32>,
    diagnostic: Option<TestDiagnostic>,
    finished: bool,
}

enum ContractAction {
    Decoded(u32),
    Invalid(XmlContractDiagnostic),
    Finish,
}

fn reduce_contract(state: &mut ContractState, action: ContractAction) {
    match action {
        ContractAction::Decoded(value) => state.value = Some(value),
        ContractAction::Invalid(diagnostic) => {
            state.diagnostic = Some(TestDiagnostic::InvalidXml(diagnostic));
        }
        ContractAction::Finish => {
            state.finished = true;
        }
    }
}

#[derive(Clone)]
struct ApplicationProps {
    state: Arc<Mutex<Option<Signal<ContractState>>>>,
    finished: Arc<Notify>,
}

impl ApplicationProps {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            finished: Arc::new(Notify::new()),
        }
    }

    fn snapshot(&self) -> ContractState {
        self.state
            .lock()
            .expect("state exposure lock")
            .clone()
            .expect("Component exposed its retained Signal")
            .with(Clone::clone)
            .expect("retained Signal remains mounted")
    }
}

#[component]
fn response_contract(
    state: Signal<ContractState>,
    finished: Arc<Notify>,
    text: EventInput<TextTurnEvent>,
) -> Component {
    let decoded_state = state.clone();
    let invalid_state = state.clone();
    let finish_state = state;

    view! {
        {
            XmlStreamingToolCall::contract("streaming-xml.selection", "v1")
                .empty_element("selection")
                .required_attribute::<u32>("value")
                .exactly_one()
                .listen_to(text)
                .on_decoded(move |value| {
                    let state = decoded_state.clone();
                    async move {
                        state.update(|state| reduce_contract(state, ContractAction::Decoded(value)))
                    }
                })
                .on_invalid(move |diagnostic| {
                    let state = invalid_state.clone();
                    async move {
                        state.update(|state| {
                            reduce_contract(state, ContractAction::Invalid(diagnostic));
                        })
                    }
                })
                .on_finish(move || async move {
                    finish_state.update(|state| reduce_contract(state, ContractAction::Finish))?;
                    finished.notify_one();
                    Ok::<(), agentview::component::SignalAccessError>(())
                })
        }
    }
}

#[component]
fn application_root(props: ApplicationProps, events: EventInput<ProviderEvent>) -> Component {
    let text = events.select(ProviderEvent::TEXT);
    let state = use_signal(ContractState::default);
    *props.state.lock().expect("state exposure lock") = Some(state.clone());

    view! {
        #[system_once]
        streaming_xml_protocol { "Return one typed selection." }

        selection_request { "Choose one unsigned integer." }
        response_contract(state, props.finished, text)
    }
}

enum ProviderScript {
    Finite(Vec<ProviderEvent>),
    Pending,
}

struct ScriptedProvider {
    scripts: VecDeque<ProviderScript>,
}

impl ScriptedProvider {
    fn once(events: Vec<ProviderEvent>) -> Self {
        Self {
            scripts: VecDeque::from([ProviderScript::Finite(events), ProviderScript::Pending]),
        }
    }
}

#[async_trait]
impl ProviderPort for ScriptedProvider {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        match self.scripts.pop_front().unwrap_or(ProviderScript::Pending) {
            ProviderScript::Finite(events) => {
                Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
            }
            ProviderScript::Pending => Ok(Box::pin(futures::stream::pending())),
        }
    }
}

async fn execute_script(events: Vec<ProviderEvent>) -> ContractState {
    let props = ApplicationProps::new();
    let finished = Arc::clone(&props.finished);
    let mut components = ComponentHost::new(application_root, props.clone());
    let mut host = ApplicationHost::new(ScriptedProvider::once(events));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Provider reaction completes");
    tokio::time::timeout(Duration::from_secs(1), finished.notified())
        .await
        .expect("normal Provider EOF completes generation-local processors");
    props.snapshot()
}

#[tokio::test]
async fn valid_stream_updates_authoritative_signal_state() {
    let state = execute_script(vec![ProviderEvent::Text(TextTurnEvent::TextComplete(
        String::from(r#"<selection value="7" />"#),
    ))])
    .await;

    assert_eq!(
        state,
        ContractState {
            value: Some(7),
            finished: true,
            ..ContractState::default()
        }
    );
}

#[tokio::test]
async fn invalid_attribute_updates_a_typed_diagnostic() {
    let state = execute_script(vec![ProviderEvent::Text(TextTurnEvent::TextComplete(
        String::from(r#"<selection value="seven" />"#),
    ))])
    .await;

    assert!(state.finished);
    assert!(matches!(
        state.diagnostic,
        Some(TestDiagnostic::InvalidXml(
            XmlContractDiagnostic::InvalidAttributeValue {
                contract: "streaming-xml.selection",
                attribute: "value",
                value,
                ..
            }
        )) if value == "seven"
    ));
}

#[tokio::test]
async fn text_events_are_applied_in_stream_order_before_finish() {
    let state = execute_script(vec![
        ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            r#"<selection value="9""#,
        ))),
        ProviderEvent::Text(TextTurnEvent::TextComplete(String::from(
            r#"<selection value="9" />"#,
        ))),
    ])
    .await;

    assert_eq!(state.value, Some(9));
    assert!(state.finished);
    assert_eq!(state.diagnostic, None);
}
