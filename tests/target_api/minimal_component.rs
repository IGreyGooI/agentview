#![allow(
    deprecated,
    reason = "this compatibility fixture intentionally exercises retained Component and Provider APIs"
)]

//! Executable fixture for the retained Component and Provider boundary.

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{
            ApplicationHost, ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort,
            RenderedProjection,
        },
        prelude::*,
        ComponentHost,
    },
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, InstructionAuthority},
};
use async_trait::async_trait;

#[derive(Clone)]
struct TargetProps {
    request: String,
    completions: Arc<Mutex<Vec<String>>>,
}

#[component]
fn application_root(props: TargetProps, events: EventInput<ProviderEvent>) -> Component {
    let request = props.request;
    let completions = props.completions;
    let text = events.select(ProviderEvent::TEXT);

    view! {
        #[system_once]
        protocol { "Return a concise result." }

        #[diff(slot = "request")]
        request { "{request}" }

        {
            EventListener::observe("target.response", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let completions = Arc::clone(&completions);
                    async move {
                        if let TextTurnEvent::TextComplete(text) = event {
                            completions.lock().unwrap().push(text);
                        }
                        Ok::<(), Infallible>(())
                    }
                })
        }
    }
}

struct ProviderState {
    scripts: VecDeque<VecDeque<ProviderEvent>>,
    projections: Vec<RenderedProjection>,
}

#[derive(Clone)]
struct ScriptedProvider {
    state: Arc<Mutex<ProviderState>>,
}

impl ScriptedProvider {
    fn new(scripts: impl IntoIterator<Item = VecDeque<ProviderEvent>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProviderState {
                scripts: scripts.into_iter().collect(),
                projections: Vec::new(),
            })),
        }
    }

    fn projections(&self) -> Vec<RenderedProjection> {
        self.state.lock().unwrap().projections.clone()
    }
}

#[async_trait]
impl ProviderPort for ScriptedProvider {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        let events = {
            let mut state = self.state.lock().unwrap();
            state.projections.push(projection);
            state.scripts.pop_front().unwrap_or_default()
        };
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}

fn projection_text(projection: &RenderedProjection) -> String {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom).unwrap()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn full_projection_and_typed_feedback_use_the_same_component_host() {
    let provider = ScriptedProvider::new([VecDeque::from([ProviderEvent::Text(
        TextTurnEvent::TextComplete("accepted".to_owned()),
    )])]);
    let observer = provider.clone();
    let completions = Arc::new(Mutex::new(Vec::new()));
    let mut components = ComponentHost::new(
        application_root,
        TargetProps {
            request: "first request".to_owned(),
            completions: Arc::clone(&completions),
        },
    );
    let mut application = ApplicationHost::new(provider);

    application
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap();

    assert_eq!(&*completions.lock().unwrap(), &["accepted"]);
    let projections = observer.projections();
    assert_eq!(projections.len(), 1);
    assert!(matches!(
        projections[0]
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .next(),
        Some(CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        })
    ));
    assert!(projection_text(&projections[0]).contains("first request"));
}

#[tokio::test]
async fn every_reaction_sends_a_complete_current_projection() {
    let provider = ScriptedProvider::new([VecDeque::new(), VecDeque::new()]);
    let observer = provider.clone();
    let completions = Arc::new(Mutex::new(Vec::new()));
    let mut components = ComponentHost::new(
        application_root,
        TargetProps {
            request: "first request".to_owned(),
            completions: Arc::clone(&completions),
        },
    );
    let mut application = ApplicationHost::new(provider);

    application
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap();
    components.set_props(TargetProps {
        request: "second request".to_owned(),
        completions,
    });
    application
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap();

    let projections = observer.projections();
    assert_eq!(projections.len(), 2);
    assert!(projection_text(&projections[0]).contains("first request"));
    assert!(projection_text(&projections[1]).contains("second request"));
}
