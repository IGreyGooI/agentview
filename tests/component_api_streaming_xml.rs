#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises legacy streaming XML event routing"
)]

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{Arc, Mutex},
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum TestDiagnostic {
    InvalidXml(XmlContractDiagnostic),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ContractState {
    values: Vec<u32>,
    diagnostic: Option<TestDiagnostic>,
}

enum ContractAction {
    Decoded(u32),
    Invalid(XmlContractDiagnostic),
}

fn reduce_contract(state: &mut ContractState, action: ContractAction) {
    match action {
        ContractAction::Decoded(value) => state.values.push(value),
        ContractAction::Invalid(diagnostic) => {
            state.diagnostic = Some(TestDiagnostic::InvalidXml(diagnostic));
        }
    }
}

#[derive(Clone)]
struct ApplicationProps {
    state: Arc<Mutex<Option<Signal<ContractState>>>>,
}

impl ApplicationProps {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
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
fn response_contract(state: Signal<ContractState>) -> Component {
    let decoded_state = state.clone();
    let invalid_state = state.clone();

    XmlStreamingToolCall::contract("streaming-xml.selection", "v1")
        .empty_element("selection")
        .required_attribute::<u32>("value")
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
}

#[component]
fn application_root(props: ApplicationProps, _events: EventInput<ProviderEvent>) -> Component {
    let state = use_signal(ContractState::default);
    *props.state.lock().expect("state exposure lock") = Some(state.clone());

    view! {
        #[system_once]
        streaming_xml_protocol { "Return one typed selection." }

        selection_request { "Choose one unsigned integer." }
        response_contract(state)
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
    let mut components = ComponentHost::new(application_root, props.clone());
    let mut host = ApplicationHost::new(ScriptedProvider::once(events));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Provider reaction completes");
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
            values: vec![7],
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
async fn text_events_are_applied_in_stream_order() {
    let state = execute_script(vec![
        ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            r#"<selection value="9""#,
        ))),
        ProviderEvent::Text(TextTurnEvent::TextComplete(String::from(
            r#"<selection value="9" />"#,
        ))),
    ])
    .await;

    assert_eq!(state.values, [9]);
    assert_eq!(state.diagnostic, None);
}

#[tokio::test]
async fn repeated_elements_are_dispatched_in_stream_order() {
    let state = execute_script(vec![ProviderEvent::Text(TextTurnEvent::TextComplete(
        String::from(r#"<selection value="3" /><selection value="5" />"#),
    ))])
    .await;

    assert_eq!(state.values, [3, 5]);
    assert_eq!(state.diagnostic, None);
}

#[derive(Clone)]
struct SharedHubProps {
    events: Arc<Mutex<Vec<String>>>,
}

#[component]
fn shared_hub_application(props: SharedHubProps, _events: EventInput<ProviderEvent>) -> Component {
    let decoded_events = Arc::clone(&props.events);
    let open_events = Arc::clone(&props.events);
    let complete_events = Arc::clone(&props.events);

    view! {
        {
            XmlStreamingToolCall::contract("streaming-xml.shared-selection", "v1")
                .empty_element("selection")
                .required_attribute::<u32>("value")
                .on_decoded(move |value| {
                    let events = Arc::clone(&decoded_events);
                    async move {
                        events.lock().unwrap().push(format!("decoded:{value}"));
                        Ok::<(), Infallible>(())
                    }
                })
                .on_invalid(|_| async { Ok::<(), Infallible>(()) })
        }
        {
            StreamingXml::tag("selection")
                .on_open(move |element| {
                    let events = Arc::clone(&open_events);
                    async move {
                        events.lock().unwrap().push(format!(
                            "open:{}",
                            element.attr("value").expect("selection value")
                        ));
                        Ok::<(), Infallible>(())
                    }
                })
                .on_complete(move |element| {
                    let events = Arc::clone(&complete_events);
                    async move {
                        events.lock().unwrap().push(format!(
                            "complete:{}:{}",
                            element.attr("value").expect("selection value"),
                            element.content
                        ));
                        Ok::<(), Infallible>(())
                    }
                })
        }
    }
}

#[test]
fn lifecycle_subscription_is_prompt_free_beside_typed_contract() {
    let props = SharedHubProps {
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let mut components = ComponentHost::new(shared_hub_application, props);
    let rendered = components.render().expect("shared XML declarations render");
    let projected_items = rendered
        .projection()
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .count();

    assert_eq!(
        projected_items, 1,
        "only XmlStreamingToolCall contributes model-visible syntax"
    );
}

#[tokio::test]
async fn typed_contract_and_lifecycle_subscription_share_the_ordered_route_hub() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let props = SharedHubProps {
        events: Arc::clone(&events),
    };
    let mut components = ComponentHost::new(shared_hub_application, props);
    let mut host = ApplicationHost::new(ScriptedProvider::once(vec![ProviderEvent::Text(
        TextTurnEvent::TextComplete(String::from(
            r#"<selection value="4"/><selection value="6"/>"#,
        )),
    )]));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("shared XML route completes");

    assert_eq!(
        *events.lock().unwrap(),
        [
            "open:4",
            "decoded:4",
            "complete:4:",
            "open:6",
            "decoded:6",
            "complete:6:",
        ]
    );
}
