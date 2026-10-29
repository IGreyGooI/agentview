#[path = "support/pom.rs"]
mod pom_support;

use std::{
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, Frame, FrameBasis, FrameCapabilities, FrameConstraints,
            FrameProfile, FrameRevision, ProviderFact, ProviderFactStream, ProviderOutputKey,
            ProviderToolCall, ReactionPort, ReactionPortFault, RenderedProjection, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity, ToolDefinition,
        },
        prelude::*,
    },
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use pom_support::{combine, projection_document, InspectPom};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const PROVIDER_TEXT: &str = "This answer exists only in the model conversation.";
const CALL_ID: &str = "export-adjustment";
const ARGUMENTS: &str = r#"{"amount":3}"#;

#[derive(Debug, Clone, PartialEq)]
struct Delivery {
    full: bool,
    replay: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    ephemeral: Vec<CanonicalInputItem>,
    tools: Vec<ToolDefinition>,
}

#[derive(Default)]
struct PortProbe {
    declarations: AtomicUsize,
    frames: Mutex<Vec<(FrameRevision, FrameBasis, Delivery)>>,
}

struct ScriptedPort {
    declaration: TargetDeclaration,
    probe: Arc<PortProbe>,
}

impl ScriptedPort {
    fn new() -> (Self, Arc<PortProbe>) {
        let probe = Arc::new(PortProbe::default());
        let declaration = TargetDeclaration::full(
            TargetIdentity::new(NonZeroU128::new(97).unwrap()),
            TargetEpoch::new(NonZeroU64::new(1).unwrap()),
            FrameProfile::new(
                FrameConstraints {
                    max_frame_bytes: 262_144,
                    max_component_bytes: 32_768,
                    context_window: ContextWindow::NotApplicable,
                    reserved_output_tokens: None,
                },
                FrameCapabilities::new(true),
            ),
        );
        (
            Self {
                declaration,
                probe: probe.clone(),
            },
            probe,
        )
    }
}

#[async_trait]
impl ReactionPort for ScriptedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.probe.declarations.fetch_add(1, Ordering::SeqCst);
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        let turn = {
            let mut frames = self.probe.frames.lock().unwrap();
            let turn = frames.len();
            frames.push((
                frame.revision(),
                frame.basis(),
                Delivery {
                    full: frame.basis() == FrameBasis::Full,
                    replay: frame.submission().replay().to_vec(),
                    staged: frame.submission().staged_inputs().to_vec(),
                    projection: frame.submission().projection().items().to_vec(),
                    ephemeral: frame.submission().ephemeral().to_vec(),
                    tools: frame.submission().tools().definitions().to_vec(),
                },
            ));
            turn
        };
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let facts = if turn == 0 {
            vec![
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: PROVIDER_TEXT.into(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    text: PROVIDER_TEXT.into(),
                },
                ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(1),
                    ordinal: 1,
                    call: ProviderToolCall::new(CALL_ID, "adjust", ARGUMENTS).unwrap(),
                },
                ProviderFact::ReactionCompleted {
                    primary_text: Some(ProviderOutputKey::new(0)),
                },
            ]
        } else {
            vec![ProviderFact::ReactionCompleted { primary_text: None }]
        };
        Ok(Box::pin(futures::stream::iter(facts.into_iter().map(Ok))))
    }
}

#[derive(Clone, Default)]
struct PreparationProbe {
    runs: Arc<AtomicUsize>,
    state: Arc<Mutex<Option<Signal<String>>>>,
    demand: Arc<Mutex<Option<ReactionRequest>>>,
}

#[component]
fn prepared_view(probe: PreparationProbe) -> Component {
    let state = use_signal(|| String::from("bootstrap"));
    *probe.state.lock().unwrap() = Some(state.clone());
    *probe.demand.lock().unwrap() = Some(use_reaction_request());
    let prepared_state = state.clone();
    let runs = probe.runs;
    use_preparation(move || {
        let state = prepared_state.clone();
        let runs = runs.clone();
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            state.set(String::from("prepared"))
        }
    });
    let value = state.with(Clone::clone).unwrap();
    view! { status { "{value}" } }
}

#[tokio::test]
async fn exporting_a_dirty_snapshot_does_not_prepare_render_submit_or_consume_demand() {
    let probe = PreparationProbe::default();
    let root_probe = probe.clone();
    let (port, provider) = ScriptedPort::new();
    let mut application =
        Application::mount(move || prepared_view(root_probe.clone()), port).unwrap();
    let state = probe.state.lock().unwrap().clone().unwrap();
    let demand = probe.demand.lock().unwrap().clone().unwrap();
    state.set(String::from("unreconciled")).unwrap();
    demand.request().unwrap();

    let snapshot = application.current_projection();
    let revision = snapshot.revision();
    let declared = snapshot.projection().clone();
    assert!(snapshot.is_dirty());
    assert!(!snapshot.is_prepared());
    snapshot.render_prompt().unwrap();
    snapshot.projection().render_prompt().unwrap();
    assert_eq!(snapshot.projection(), &declared);
    assert_eq!(snapshot.revision(), revision);
    assert!(projection_document(snapshot.projection()).has_field("status", "bootstrap"));
    assert_eq!(state.with(Clone::clone).unwrap(), "unreconciled");
    assert_eq!(probe.runs.load(Ordering::SeqCst), 0);
    assert_eq!(provider.declarations.load(Ordering::SeqCst), 1);
    assert!(provider.frames.lock().unwrap().is_empty());
    assert!(application.take_reaction_request().unwrap());
    assert!(!application.take_reaction_request().unwrap());

    assert!(application.prepare().await.unwrap().is_continue());
    let snapshot = application.current_projection();
    assert!(snapshot.is_prepared());
    assert!(!snapshot.is_dirty());
    assert!(projection_document(snapshot.projection()).has_field("status", "prepared"));
    snapshot.render_prompt().unwrap();
    assert_eq!(probe.runs.load(Ordering::SeqCst), 1);
    assert_eq!(provider.declarations.load(Ordering::SeqCst), 1);
    assert!(provider.frames.lock().unwrap().is_empty());
    application.shutdown().await.unwrap();
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Adjustment {
    /// Integer amount to add to the current counter.
    amount: i64,
}

#[derive(Serialize)]
struct AdjustmentResult {
    value: i64,
}

#[component]
fn counter(calls: Arc<AtomicUsize>) -> Component {
    let state = use_signal(|| 2_i64);
    let current = state.with(|value| *value).unwrap();
    view! {
        #[system_once]
        "Use the currently declared counter and native actions."
        #[assistant]
        "Add only the requested amount."
        #[developer(ephemeral, repeat)]
        observation { "This is the current counter observation." }
        #[diff(slot = "counter")]
        counter { value { "{current}" } }
        next_action { "Call adjust with an integer amount, then inspect its actual result." }
        NativeToolCall {
            name: "adjust",
            description: "Add an integer amount and return the actual updated counter.",
            on_call: move |input: Adjustment| {
                calls.fetch_add(1, Ordering::SeqCst);
                state.update(|value| {
                    *value += input.amount;
                    AdjustmentResult { value: *value }
                })
            },
        }
    }
}

fn native_records(projection: &RenderedProjection) -> Vec<CanonicalInputItem> {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter(|item| {
            matches!(
                item,
                CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
            )
        })
        .cloned()
        .collect()
}

fn export_without_effects(
    application: &Application<ScriptedPort>,
    provider: &PortProbe,
    calls: &AtomicUsize,
) {
    let declarations = provider.declarations.load(Ordering::SeqCst);
    let submissions = provider.frames.lock().unwrap().len();
    let handled = calls.load(Ordering::SeqCst);
    let snapshot = application.current_projection();
    let revision = snapshot.revision();
    let declared = snapshot.projection().clone();
    snapshot.render_prompt().unwrap();
    snapshot.projection().render_prompt().unwrap();
    assert_eq!(snapshot.projection(), &declared);
    assert_eq!(snapshot.revision(), revision);
    assert_eq!(provider.declarations.load(Ordering::SeqCst), declarations);
    assert_eq!(provider.frames.lock().unwrap().len(), submissions);
    assert_eq!(calls.load(Ordering::SeqCst), handled);
}

async fn interact(export: bool) -> Vec<Delivery> {
    let calls = Arc::new(AtomicUsize::new(0));
    let root_calls = calls.clone();
    let (port, provider) = ScriptedPort::new();
    let mut application = Application::mount(move || counter(root_calls.clone()), port).unwrap();
    if export {
        export_without_effects(&application, &provider, &calls);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(application.react().await.unwrap().is_continue());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let expected_call = CanonicalInputItem::tool_call(CALL_ID, "adjust", ARGUMENTS).unwrap();
    let expected_result = CanonicalInputItem::tool_result(CALL_ID, r#"{"value":5}"#).unwrap();
    let expected_records = vec![expected_call.clone(), expected_result.clone()];
    let snapshot = application.current_projection();
    assert_eq!(native_records(snapshot.projection()), expected_records);
    assert!(projection_document(snapshot.projection()).has_field("value", "5"));
    assert!(!snapshot.projection().nodes().iter().any(|node| {
        node.items().iter().any(|item| {
            matches!(item, CanonicalInputItem::AssistantText { text, .. } if text == PROVIDER_TEXT)
        })
    }));
    let tools = snapshot.projection().native_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "adjust");
    assert_eq!(tools[0].input_schema()["type"], "object");
    assert_eq!(
        tools[0].input_schema()["properties"]["amount"]["type"],
        "integer"
    );
    assert_eq!(tools[0].input_schema()["additionalProperties"], false);
    if export {
        // A real result is still staged here: exporting cannot acknowledge it.
        export_without_effects(&application, &provider, &calls);
    }

    assert!(application.react().await.unwrap().is_continue());
    assert_eq!(
        native_records(application.current_projection().projection()),
        expected_records
    );
    if export {
        export_without_effects(&application, &provider, &calls);
    }
    assert!(application.react().await.unwrap().is_continue());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let deliveries = {
        let frames = provider.frames.lock().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].1, FrameBasis::Full);
        assert_eq!(frames[1].1, FrameBasis::DeltaFrom(frames[0].0));
        assert_eq!(frames[2].1, FrameBasis::DeltaFrom(frames[1].0));
        for (frame, value) in [(&frames[0].2, "2"), (&frames[1].2, "5")] {
            let delivered = combine(frame.projection.iter().filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
                _ => None,
            }));
            assert!(delivered.has_field("value", value));
            assert_eq!(frame.tools.len(), 1);
            assert_eq!(frame.tools[0].name(), "adjust");
        }
        assert_eq!(frames[0].2.tools, frames[1].2.tools);
        assert_eq!(
            frames[1].2.replay,
            vec![
                CanonicalInputItem::assistant_text(PROVIDER_TEXT, None),
                expected_call
            ]
        );
        assert_eq!(frames[1].2.staged, vec![expected_result]);
        assert!(frames[2].2.replay.is_empty());
        assert!(frames[2].2.staged.is_empty());
        assert!(frames[2].2.projection.is_empty());
        frames
            .iter()
            .map(|(_, _, delivery)| delivery.clone())
            .collect()
    };
    application.shutdown().await.unwrap();
    deliveries
}

#[tokio::test]
async fn exports_preserve_retained_tool_results_and_the_accepted_full_delta_baseline() {
    let baseline = interact(false).await;
    let exported = interact(true).await;
    assert_eq!(exported, baseline);
}
