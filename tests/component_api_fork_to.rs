#[path = "support/pom.rs"]
mod pom_support;

use std::{
    collections::VecDeque,
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    panic::{panic_any, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFault, ApplicationFaultReason, ApplicationFaultStage,
            CommandCall, CommandInput, CommandInputError, CommandOutcome, ContextWindow, Frame,
            FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
            ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall, ReactionPort,
            ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity, ToolDefinition,
        },
        prelude::*,
    },
    pom::ResolvedDocument,
    transcript::{CanonicalInputItem, InstructionAuthority},
};
use async_trait::async_trait;
use futures::{poll, FutureExt};
use pom_support::{combine, projection_document, InspectPom};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(3);
const PARENT_REPLY: &str = "This reply belongs only to the parent's model history.";
static NEXT_TARGET: AtomicU64 = AtomicU64::new(70_000);

#[derive(Clone)]
struct CapturedFrame {
    target: TargetIdentity,
    revision: FrameRevision,
    basis: FrameBasis,
    profile: FrameProfile,
    replay: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    ephemeral: Vec<CanonicalInputItem>,
    tools: Vec<ToolDefinition>,
}

enum Script {
    Facts(Vec<ProviderFact>),
    Gated(Arc<Notify>),
}

impl Script {
    fn completed() -> Self {
        Self::Facts(vec![ProviderFact::ReactionCompleted { primary_text: None }])
    }

    fn adjustment(call_id: &str, amount: i64) -> Self {
        Self::Facts(vec![
            ProviderFact::ToolCall {
                output: ProviderOutputKey::new(1),
                ordinal: 1,
                call: ProviderToolCall::new(
                    call_id,
                    "adjust",
                    json!({ "amount": amount }).to_string(),
                )
                .unwrap(),
            },
            ProviderFact::ReactionCompleted { primary_text: None },
        ])
    }

    fn parent_history_and_adjustment() -> Self {
        let Self::Facts(mut facts) = Self::adjustment("parent-first", 3) else {
            unreachable!()
        };
        facts.splice(
            0..0,
            [
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: PARENT_REPLY.into(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    text: PARENT_REPLY.into(),
                },
            ],
        );
        *facts.last_mut().unwrap() = ProviderFact::ReactionCompleted {
            primary_text: Some(ProviderOutputKey::new(0)),
        };
        Self::Facts(facts)
    }
}

#[derive(Default)]
struct PortProbe {
    declarations: AtomicUsize,
    frames: Mutex<Vec<CapturedFrame>>,
    submitted: Notify,
    shutdown_started: Notify,
    shutdowns_started: AtomicUsize,
    shutdowns_completed: AtomicUsize,
    shutdown_gate: Mutex<Option<Arc<Notify>>>,
}

impl PortProbe {
    fn frames(&self) -> Vec<CapturedFrame> {
        self.frames.lock().unwrap().clone()
    }
}

struct PortCore {
    declaration: Result<TargetDeclaration, ReactionPortFault>,
    scripts: VecDeque<Script>,
    probe: Arc<PortProbe>,
}

impl PortCore {
    fn new(declaration: TargetDeclaration, scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            declaration: Ok(declaration),
            scripts: scripts.into_iter().collect(),
            probe: Arc::default(),
        }
    }

    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.probe.declarations.fetch_add(1, Ordering::SeqCst);
        self.declaration.clone()
    }

    async fn submit(&mut self, frame: Frame) -> Result<ProviderFactStream<'_>, SubmitFault> {
        frame.check_handoff_precondition(self.declaration.as_ref().unwrap())?;
        self.probe.frames.lock().unwrap().push(CapturedFrame {
            target: frame.target(),
            revision: frame.revision(),
            basis: frame.basis(),
            profile: frame.prepared_profile().clone(),
            replay: frame.submission().replay().to_vec(),
            staged: frame.submission().staged_inputs().to_vec(),
            projection: frame.submission().projection().items().to_vec(),
            ephemeral: frame.submission().ephemeral().to_vec(),
            tools: frame.submission().tools().definitions().to_vec(),
        });
        self.declaration = Ok(TargetDeclaration::resume(
            frame.revision(),
            frame.prepared_profile().clone(),
        ));
        self.probe.submitted.notify_one();
        match self.scripts.pop_front().unwrap_or_else(Script::completed) {
            Script::Facts(facts) => Ok(Box::pin(futures::stream::iter(facts.into_iter().map(Ok)))),
            Script::Gated(gate) => Ok(Box::pin(futures::stream::once(async move {
                gate.notified().await;
                Ok(ProviderFact::ReactionCompleted { primary_text: None })
            }))),
        }
    }

    async fn shutdown(&mut self) {
        self.probe.shutdowns_started.fetch_add(1, Ordering::SeqCst);
        self.probe.shutdown_started.notify_one();
        let gate = self.probe.shutdown_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.probe
            .shutdowns_completed
            .fetch_add(1, Ordering::SeqCst);
    }
}

// Deliberately different provider types; neither implements fork or reset.
struct SourcePort(PortCore);
struct DestinationPort(PortCore);

macro_rules! reaction_port {
    ($port:ty) => {
        #[async_trait]
        impl ReactionPort for $port {
            fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
                self.0.declare()
            }

            async fn submit<'a>(
                &'a mut self,
                frame: Frame,
            ) -> Result<ProviderFactStream<'a>, SubmitFault> {
                self.0.submit(frame).await
            }

            async fn shutdown(&mut self) {
                self.0.shutdown().await;
            }
        }
    };
}

reaction_port!(SourcePort);
reaction_port!(DestinationPort);

fn source_profile() -> FrameProfile {
    FrameProfile::new(
        FrameConstraints {
            max_frame_bytes: 262_144,
            max_component_bytes: 32_768,
            context_window: ContextWindow::NotApplicable,
            reserved_output_tokens: None,
        },
        FrameCapabilities::new(true)
            .with_native_tool_calls(true)
            .with_text_delta_output(true),
    )
}

fn destination_profile() -> FrameProfile {
    FrameProfile::new(
        FrameConstraints {
            max_frame_bytes: 131_072,
            max_component_bytes: 16_384,
            context_window: ContextWindow::Tokens(NonZeroU64::new(8_192).unwrap()),
            reserved_output_tokens: Some(1_024),
        },
        FrameCapabilities::new(true)
            .with_native_tool_calls(true)
            .with_text_delta_output(true),
    )
}

fn declaration(profile: FrameProfile) -> TargetDeclaration {
    TargetDeclaration::full(
        TargetIdentity::new(
            NonZeroU128::new(u128::from(NEXT_TARGET.fetch_add(1, Ordering::Relaxed))).unwrap(),
        ),
        TargetEpoch::new(NonZeroU64::new(1).unwrap()),
        profile,
    )
}

#[derive(Clone)]
struct BusinessSnapshot {
    value: i64,
    feedback: String,
}

#[derive(Clone)]
struct CounterProps {
    initial: BusinessSnapshot,
    scope: &'static str,
    state: Arc<Mutex<Option<Signal<BusinessSnapshot>>>>,
    demand: Arc<Mutex<Option<ReactionRequest>>>,
    calls: Arc<AtomicUsize>,
    preparations: Arc<AtomicUsize>,
    task_starts: Arc<AtomicUsize>,
    task_started: Arc<Notify>,
}

impl CounterProps {
    fn new(value: i64, scope: &'static str) -> Self {
        Self {
            initial: BusinessSnapshot {
                value,
                feedback: String::from("Ready for an adjustment."),
            },
            scope,
            state: Arc::default(),
            demand: Arc::default(),
            calls: Arc::default(),
            preparations: Arc::default(),
            task_starts: Arc::default(),
            task_started: Arc::default(),
        }
    }

    fn snapshot(&self) -> BusinessSnapshot {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .with(Clone::clone)
            .unwrap()
    }

    fn request_reaction(&self) {
        self.demand
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .request()
            .unwrap();
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Adjustment {
    amount: i64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct AdjustmentResult {
    value: i64,
    observed_value: i64,
}

#[component]
fn counter(props: CounterProps) -> Component {
    let state = use_signal(move || props.initial);
    *props.state.lock().unwrap() = Some(state.clone());
    *props.demand.lock().unwrap() = Some(use_reaction_request());
    use_preparation(move || {
        let preparations = props.preparations.clone();
        async move {
            preparations.fetch_add(1, Ordering::SeqCst);
            Ok::<(), std::convert::Infallible>(())
        }
    });
    use_future(move || {
        props.task_starts.fetch_add(1, Ordering::SeqCst);
        props.task_started.notify_one();
        async { std::future::pending::<()>().await }
    });
    let snapshot = state.with(Clone::clone).unwrap();
    let observed_value = snapshot.value;
    let value = snapshot.value;
    let feedback = snapshot.feedback;
    let scope = props.scope;
    view! {
        #[system_once]
        policy { "Apply changes only in the {scope} application." }
        #[developer(ephemeral, repeat)]
        current_scope { "{scope}" }
        counter_value { "{value}" }
        action_result { "{feedback}" }
        next_action { "Call adjust with an integer amount, then inspect the actual result before continuing." }
        NativeToolCall {
            name: "adjust",
            description: "Add an integer amount to this application's counter and return its actual value.",
            on_call: move |_ctx, input: Adjustment| {
                props.calls.fetch_add(1, Ordering::SeqCst);
                state.update(|snapshot| {
                    snapshot.value += input.amount;
                    snapshot.feedback = format!("Adjusted by {}; value is {}.", input.amount, snapshot.value);
                    AdjustmentResult { value: snapshot.value, observed_value }
                })
            },
        }
    }
}

fn documents(items: &[CanonicalInputItem]) -> ResolvedDocument {
    combine(items.iter().filter_map(|item| match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            Some(pom.clone())
        }
        _ => None,
    }))
}

fn result(frame: &CapturedFrame, call_id: &str) -> AdjustmentResult {
    let [CanonicalInputItem::ToolResult {
        call_id: actual,
        content,
    }] = frame.staged.as_slice()
    else {
        panic!("expected exactly one delivered tool result")
    };
    assert_eq!(actual, call_id);
    serde_json::from_str(content).unwrap()
}

fn rejected<T>(result: Result<T, ApplicationFault>) -> ApplicationFault {
    match result {
        Ok(_) => panic!("fork accepted an invalid destination or source"),
        Err(fault) => fault,
    }
}

#[tokio::test]
async fn cross_provider_fork_keeps_staged_parent_work_and_delivers_independent_child_interactions()
{
    let release_parent = Arc::new(Notify::new());
    let source = SourcePort(PortCore::new(
        declaration(source_profile()),
        [
            Script::parent_history_and_adjustment(),
            Script::Gated(release_parent.clone()),
            Script::adjustment("parent-next", 7),
            Script::completed(),
        ],
    ));
    let source_probe = source.0.probe.clone();
    let parent_props = CounterProps::new(10, "parent");
    let root_props = parent_props.clone();
    let mut parent = Application::mount(move || counter(root_props.clone()), source).unwrap();
    assert!(parent.react(false).unwrap().await.unwrap().is_continue());
    assert_eq!(parent_props.snapshot().value, 13);
    parent_props.request_reaction();
    let declared = parent.current_projection().projection().clone();
    let revision = parent.current_projection().revision();
    let declarations = source_probe.declarations.load(Ordering::SeqCst);
    let preparations = parent_props.preparations.load(Ordering::SeqCst);
    let destination = DestinationPort(PortCore::new(
        declaration(destination_profile()),
        [
            Script::adjustment("child-first", 2),
            Script::adjustment("child-next", 4),
            Script::completed(),
        ],
    ));
    let destination_probe = destination.0.probe.clone();
    let mut child_props = CounterProps::new(0, "child");
    child_props.initial = parent_props.snapshot();
    let fork_props = child_props.clone();
    let mut child: Application<DestinationPort> = parent
        .fork_to(move || counter(fork_props.clone()), destination)
        .await
        .unwrap();

    assert_eq!(
        source_probe.declarations.load(Ordering::SeqCst),
        declarations
    );
    assert_eq!(source_probe.frames().len(), 1);
    assert!(destination_probe.frames().is_empty());
    assert_eq!(
        parent_props.preparations.load(Ordering::SeqCst),
        preparations
    );
    assert_eq!(child_props.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(parent.current_projection().revision(), revision);
    assert_eq!(parent.current_projection().projection(), &declared);
    assert!(!child.current_projection().is_prepared());
    assert!(parent.take_reaction_request().unwrap());
    assert!(!child.take_reaction_request().unwrap());
    child_props.request_reaction();
    assert!(!parent.take_reaction_request().unwrap());
    assert!(child.take_reaction_request().unwrap());
    assert_eq!(child_props.snapshot().value, 13);
    assert_eq!(child_props.calls.load(Ordering::SeqCst), 0);
    assert!(!child
        .current_projection()
        .projection()
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .any(|item| {
            matches!(
                item,
                CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
            )
        }));
    tokio::time::timeout(TIMEOUT, child_props.task_started.notified())
        .await
        .unwrap();
    assert_eq!(child_props.task_starts.load(Ordering::SeqCst), 1);
    assert_eq!(parent_props.task_starts.load(Ordering::SeqCst), 1);

    let pending_parent_control = parent.react(false).unwrap();

    let mut pending_parent = Box::pin(std::future::IntoFuture::into_future(
        pending_parent_control.clone(),
    ));
    tokio::time::timeout(TIMEOUT, async {
        while source_probe.frames().len() < 2 {
            tokio::select! {
                result = pending_parent.as_mut() => panic!("parent completed before its gated response: {result:?}"),
                _ = source_probe.submitted.notified() => {},
            }
        }
    }).await.unwrap();
    assert!(poll!(pending_parent.as_mut()).is_pending());
    for _ in 0..3 {
        assert!(child.react(false).unwrap().await.unwrap().is_continue());
    }

    let frames = destination_probe.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(frames[0].replay.is_empty());
    assert!(frames[0].staged.is_empty());
    assert_ne!(frames[0].target, source_probe.frames()[0].target);
    assert_ne!(frames[0].revision, source_probe.frames()[0].revision);
    assert!(documents(&frames[0].projection)
        .has_field("policy", "Apply changes only in the child application."));
    assert!(documents(&frames[0].projection).has_field("counter_value", "13"));
    assert!(documents(&frames[0].projection).has_element("next_action"));
    for frame in &frames {
        assert_eq!(frame.profile, destination_profile());
        assert_ne!(frame.profile, source_profile());
        assert!(documents(&frame.ephemeral).has_field("current_scope", "child"));
        assert_eq!(frame.tools.len(), 1);
        assert_eq!(frame.tools[0].name(), "adjust");
        assert_eq!(
            frame.tools[0].input_schema()["properties"]["amount"]["type"],
            "integer"
        );
        assert_eq!(frame.tools[0].input_schema()["additionalProperties"], false);
    }
    for (index, call_id, value, observed) in [(1, "child-first", 15, 13), (2, "child-next", 19, 15)]
    {
        assert_eq!(
            frames[index].basis,
            FrameBasis::DeltaFrom(frames[index - 1].revision)
        );
        assert_eq!(
            result(&frames[index], call_id),
            AdjustmentResult {
                value,
                observed_value: observed
            }
        );
        let delivered = documents(&frames[index].projection);
        assert!(delivered.has_field("counter_value", &value.to_string()));
        assert!(delivered.has_field(
            "action_result",
            &format!("Adjusted by {}; value is {}.", value - observed, value)
        ));
        assert!(!frames[index].projection.iter().any(|item| matches!(
            item,
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::System,
                ..
            }
        )));
    }
    assert_eq!(parent_props.snapshot().value, 13);
    assert_eq!(child_props.snapshot().value, 19);
    assert_eq!(parent_props.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child_props.calls.load(Ordering::SeqCst), 2);
    let source_frames = source_probe.frames();
    assert_eq!(
        source_frames[1].basis,
        FrameBasis::DeltaFrom(source_frames[0].revision)
    );
    assert_eq!(
        source_frames[1].replay,
        vec![
            CanonicalInputItem::assistant_text(PARENT_REPLY, None),
            CanonicalInputItem::tool_call("parent-first", "adjust", r#"{"amount":3}"#).unwrap(),
        ]
    );
    assert_eq!(
        result(&source_frames[1], "parent-first"),
        AdjustmentResult {
            value: 13,
            observed_value: 10
        }
    );

    child.exit_handle().request(ExitReason::Requested).unwrap();
    assert_eq!(
        child.react(false).unwrap().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    child.shutdown().await.unwrap();
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.shutdowns_started.load(Ordering::SeqCst), 0);
    assert!(poll!(pending_parent.as_mut()).is_pending());
    release_parent.notify_one();
    assert!(pending_parent.await.unwrap().is_continue());
    assert!(parent.react(false).unwrap().await.unwrap().is_continue());
    assert!(parent.react(false).unwrap().await.unwrap().is_continue());
    assert_eq!(parent_props.snapshot().value, 20);
    let frames = source_probe.frames();
    assert_eq!(
        result(&frames[3], "parent-next"),
        AdjustmentResult {
            value: 20,
            observed_value: 13
        }
    );
    assert!(documents(&frames[3].projection).has_field("counter_value", "20"));
    assert!(frames.iter().flat_map(|frame| frame.replay.iter().chain(&frame.staged)).all(|item| {
        !matches!(item, CanonicalInputItem::ToolCall { call_id, .. } | CanonicalInputItem::ToolResult { call_id, .. } if call_id.starts_with("child-"))
    }));
    parent.shutdown().await.unwrap();
    assert_eq!(source_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_destination_is_rejected_before_root_and_tasks_and_is_shut_down() {
    let seed = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    let seed_probe = seed.0.probe.clone();
    let mut seed_app =
        Application::mount(|| view! { "Seed a distinct accepted target." }, seed).unwrap();
    assert!(seed_app.react(false).unwrap().await.unwrap().is_continue());
    let accepted =
        TargetDeclaration::resume(seed_probe.frames()[0].revision, destination_profile());
    seed_app.shutdown().await.unwrap();

    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let same_identity = source.0.declaration.as_ref().unwrap().identity();
    let source_probe = source.0.probe.clone();
    let props = CounterProps::new(10, "parent");
    let root_props = props.clone();
    let mut parent = Application::mount(move || counter(root_props.clone()), source).unwrap();
    props.request_reaction();
    for target in [
        TargetDeclaration::full(
            same_identity,
            TargetEpoch::new(NonZeroU64::new(2).unwrap()),
            destination_profile(),
        ),
        accepted,
    ] {
        let destination = DestinationPort(PortCore::new(target, []));
        let probe = destination.0.probe.clone();
        let child_props = CounterProps::new(10, "unreachable child");
        let fork_props = child_props.clone();
        let roots = Arc::new(AtomicUsize::new(0));
        let root_calls = roots.clone();
        let fault = rejected(
            parent
                .fork_to(
                    move || {
                        root_calls.fetch_add(1, Ordering::SeqCst);
                        counter(fork_props.clone())
                    },
                    destination,
                )
                .await,
        );
        assert_eq!(fault.stage(), ApplicationFaultStage::Fork);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::ForkSnapshotIncompatible
        );
        assert_eq!(roots.load(Ordering::SeqCst), 0);
        assert_eq!(child_props.task_starts.load(Ordering::SeqCst), 0);
        assert_eq!(child_props.preparations.load(Ordering::SeqCst), 0);
        assert_eq!(probe.shutdowns_completed.load(Ordering::SeqCst), 1);
        assert!(probe.frames().is_empty());
    }
    assert_eq!(source_probe.declarations.load(Ordering::SeqCst), 1);
    assert_eq!(props.preparations.load(Ordering::SeqCst), 0);
    assert!(source_probe.frames().is_empty());
    assert!(parent.take_reaction_request().unwrap());
    assert!(parent.react(false).unwrap().await.unwrap().is_continue());
    assert_eq!(source_probe.frames()[0].basis, FrameBasis::Full);
    parent.shutdown().await.unwrap();
}

#[tokio::test]
async fn exiting_source_rejects_before_root_and_still_shuts_down_the_owned_destination() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let mut parent = Application::mount(|| view! { "Source is exiting." }, source).unwrap();
    parent.exit_handle().request(ExitReason::Requested).unwrap();
    let destination = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    let destination_probe = destination.0.probe.clone();
    let roots = Arc::new(AtomicUsize::new(0));
    let root_calls = roots.clone();
    let fault = rejected(
        parent
            .fork_to(
                move || {
                    root_calls.fetch_add(1, Ordering::SeqCst);
                    view! { "Unreachable child." }
                },
                destination,
            )
            .await,
    );
    assert_eq!(fault.stage(), ApplicationFaultStage::Fork);
    assert_eq!(fault.reason(), ApplicationFaultReason::ForkNotReady);
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    assert_eq!(source_probe.declarations.load(Ordering::SeqCst), 1);
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert!(destination_probe.frames().is_empty());
    assert!(source_probe.frames().is_empty());
    parent.shutdown().await.unwrap();
}

#[tokio::test]
async fn destination_cleanup_survives_a_cancelled_waiter_and_parent_shutdown_drains_it() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let props = CounterProps::new(10, "parent");
    let root_props = props.clone();
    let mut parent = Application::mount(move || counter(root_props.clone()), source).unwrap();
    props.request_reaction();
    let mut destination = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    destination.0.declaration = Err(ReactionPortFault::retryable(
        ReactionPortFaultCode::Unavailable,
        ReactionPortFaultReason::Transport,
    ));
    let destination_probe = destination.0.probe.clone();
    let release_cleanup = Arc::new(Notify::new());
    *destination_probe.shutdown_gate.lock().unwrap() = Some(release_cleanup.clone());
    let roots = Arc::new(AtomicUsize::new(0));
    let root_calls = roots.clone();
    let mut fork = Box::pin(parent.fork_to(
        move || {
            root_calls.fetch_add(1, Ordering::SeqCst);
            view! { "Unreachable child." }
        },
        destination,
    ));
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = destination_probe.shutdown_started.notified() => {},
            _ = fork.as_mut() => panic!("failed fork returned before destination shutdown completed"),
        }
    }).await.unwrap();
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    drop(fork);
    assert_eq!(
        destination_probe.shutdowns_started.load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        0
    );
    assert_eq!(source_probe.declarations.load(Ordering::SeqCst), 1);
    assert!(parent.take_reaction_request().unwrap());
    assert!(parent.react(false).unwrap().await.unwrap().is_continue());
    assert_eq!(source_probe.frames()[0].basis, FrameBasis::Full);
    let mut shutdown = Box::pin(parent.shutdown());
    assert!(poll!(shutdown.as_mut()).is_pending());
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        0
    );
    release_cleanup.notify_one();
    tokio::time::timeout(TIMEOUT, shutdown)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn destination_budget_failure_starts_no_child_tasks_and_leaves_parent_usable() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let mut parent = Application::mount(|| view! { source { "ready" } }, source).unwrap();
    let mut profile = destination_profile();
    profile.constraints.max_component_bytes = 128;
    profile.constraints.max_frame_bytes = 4_096;
    let destination = DestinationPort(PortCore::new(declaration(profile), []));
    let destination_probe = destination.0.probe.clone();
    let child_props = CounterProps::new(10, "child");
    let fork_props = child_props.clone();
    let roots = Arc::new(AtomicUsize::new(0));
    let root_calls = roots.clone();
    let fault = rejected(
        parent
            .fork_to(
                move || {
                    root_calls.fetch_add(1, Ordering::SeqCst);
                    counter(fork_props.clone())
                },
                destination,
            )
            .await,
    );
    assert_eq!(fault.stage(), ApplicationFaultStage::Fork);
    assert_eq!(fault.reason(), ApplicationFaultReason::FrameBudget);
    assert!(roots.load(Ordering::SeqCst) > 0);
    assert_eq!(child_props.task_starts.load(Ordering::SeqCst), 0);
    assert_eq!(child_props.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(child_props.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.declarations.load(Ordering::SeqCst), 1);
    assert!(source_probe.frames().is_empty());
    assert!(destination_probe.frames().is_empty());
    assert!(parent.react(false).unwrap().await.unwrap().is_continue());
    parent.shutdown().await.unwrap();
}

#[derive(Debug, PartialEq)]
struct PanicMarker(u64);

#[tokio::test]
async fn child_root_panic_preserves_its_payload_and_parent_shutdown_drains_destination_cleanup() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let mut parent = Application::mount(|| view! { source { "ready" } }, source).unwrap();
    let destination = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    let destination_probe = destination.0.probe.clone();
    let release_cleanup = Arc::new(Notify::new());
    *destination_probe.shutdown_gate.lock().unwrap() = Some(release_cleanup.clone());
    let payload = match AssertUnwindSafe(parent.fork_to(|| panic_any(PanicMarker(81)), destination))
        .catch_unwind()
        .await
    {
        Err(payload) => payload,
        Ok(_) => panic!("child root panic was converted into a fork result"),
    };
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(81))
    );
    tokio::time::timeout(TIMEOUT, destination_probe.shutdown_started.notified())
        .await
        .unwrap();
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        parent.react(false).unwrap_err().reason(),
        ApplicationFaultReason::ForkCleanup
    );
    assert_eq!(source_probe.declarations.load(Ordering::SeqCst), 1);
    assert!(source_probe.frames().is_empty());
    assert!(destination_probe.frames().is_empty());
    let mut shutdown = Box::pin(parent.shutdown());
    assert!(poll!(shutdown.as_mut()).is_pending());
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        0
    );
    release_cleanup.notify_one();
    let fault = tokio::time::timeout(TIMEOUT, shutdown)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(fault.reason(), ApplicationFaultReason::ForkCleanup);
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[component]
fn panicking_source(gate: Arc<Notify>) -> Component {
    use_future(move || async move {
        gate.notified().await;
        panic_any(PanicMarker(82));
    });
    view! { source { "Waiting for supervised work." } }
}

#[tokio::test]
async fn source_task_panic_interrupts_failed_destination_cleanup_without_detaching_it() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let panic_gate = Arc::new(Notify::new());
    let root_gate = panic_gate.clone();
    let mut parent =
        Application::mount(move || panicking_source(root_gate.clone()), source).unwrap();
    let mut destination = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    destination.0.declaration = Err(ReactionPortFault::retryable(
        ReactionPortFaultCode::Unavailable,
        ReactionPortFaultReason::Declaration,
    ));
    let destination_probe = destination.0.probe.clone();
    let release_cleanup = Arc::new(Notify::new());
    *destination_probe.shutdown_gate.lock().unwrap() = Some(release_cleanup.clone());
    let roots = Arc::new(AtomicUsize::new(0));
    let root_calls = roots.clone();
    let mut fork = Box::pin(
        AssertUnwindSafe(parent.fork_to(
            move || {
                root_calls.fetch_add(1, Ordering::SeqCst);
                view! { "Unreachable child." }
            },
            destination,
        ))
        .catch_unwind(),
    );
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = destination_probe.shutdown_started.notified() => {},
            _ = fork.as_mut() => panic!("failed fork returned before gated cleanup finished"),
        }
    })
    .await
    .unwrap();
    panic_gate.notify_one();
    let payload = match tokio::time::timeout(TIMEOUT, fork).await.unwrap() {
        Err(payload) => payload,
        Ok(_) => panic!("source task panic was converted into a fork result"),
    };
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(82))
    );
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    assert_eq!(
        destination_probe.shutdowns_started.load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        parent.react(false).unwrap_err().reason(),
        ApplicationFaultReason::ComponentRuntime
    );
    assert!(source_probe.frames().is_empty());
    assert!(destination_probe.frames().is_empty());
    let mut shutdown = Box::pin(parent.shutdown());
    assert!(poll!(shutdown.as_mut()).is_pending());
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        0
    );
    release_cleanup.notify_one();
    assert!(tokio::time::timeout(TIMEOUT, shutdown)
        .await
        .unwrap()
        .is_err());
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

struct PanicWitness(Arc<Notify>);

impl Drop for PanicWitness {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[component]
fn source_with_panic_witness(gate: Arc<Notify>, sibling_dropped: Arc<Notify>) -> Component {
    use_future(move || async move {
        gate.notified().await;
        panic_any(PanicMarker(83));
    });
    use_future(move || {
        let witness = PanicWitness(sibling_dropped);
        async move {
            let _witness = witness;
            std::future::pending::<()>().await;
        }
    });
    view! { source { "Waiting for supervised tasks." } }
}

#[tokio::test]
async fn a_preexisting_source_panic_keeps_its_payload_and_runtime_fault_classification() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let panic_gate = Arc::new(Notify::new());
    let root_gate = panic_gate.clone();
    let sibling_dropped = Arc::new(Notify::new());
    let root_sibling = sibling_dropped.clone();
    let mut parent = Application::mount(
        move || source_with_panic_witness(root_gate.clone(), root_sibling.clone()),
        source,
    )
    .unwrap();
    panic_gate.notify_one();
    // The supervisor aborts this sibling only after latching the source panic.
    tokio::time::timeout(TIMEOUT, sibling_dropped.notified())
        .await
        .unwrap();
    let destination = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    let destination_probe = destination.0.probe.clone();
    let roots = Arc::new(AtomicUsize::new(0));
    let root_calls = roots.clone();
    let payload = match AssertUnwindSafe(parent.fork_to(
        move || {
            root_calls.fetch_add(1, Ordering::SeqCst);
            view! { "Unreachable child." }
        },
        destination,
    ))
    .catch_unwind()
    .await
    {
        Err(payload) => payload,
        Ok(_) => panic!("preexisting source panic was converted into a fork result"),
    };
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(83))
    );
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    assert_eq!(destination_probe.declarations.load(Ordering::SeqCst), 0);
    assert_eq!(
        parent.react(false).unwrap_err().reason(),
        ApplicationFaultReason::ComponentRuntime
    );
    assert!(tokio::time::timeout(TIMEOUT, parent.shutdown())
        .await
        .unwrap()
        .is_err());
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
    assert!(source_probe.frames().is_empty());
    assert!(destination_probe.frames().is_empty());
}

#[component]
fn command_counter(initial: i64) -> Component {
    let state = use_signal(move || initial);
    let value = state.with(|value| *value).unwrap();
    use_wait_for_command();
    view! {
        counter_value { "{value}" }
        Action {
            name: "adjust",
            description: "Adjust this application's counter and inspect the resulting value.",
            on_call: move |_ctx, input: Adjustment| state.update(|value| { *value += input.amount; *value }),
        }
    }
}

#[tokio::test]
async fn cross_provider_fork_with_commands_has_its_own_inbox_and_shutdown_scope() {
    let source = SourcePort(PortCore::new(declaration(source_profile()), []));
    let source_probe = source.0.probe.clone();
    let (parent_sender, parent_input) = CommandInput::channel(2);
    let mut parent =
        Application::mount_with_commands(|| command_counter(5), source, parent_input).unwrap();
    let mut queued_parent =
        Box::pin(parent_sender.submit(CommandCall::new("adjust", json!({ "amount": 4 }))));
    assert!(poll!(queued_parent.as_mut()).is_pending());
    let destination = DestinationPort(PortCore::new(declaration(destination_profile()), []));
    let destination_probe = destination.0.probe.clone();
    let (child_sender, child_input) = CommandInput::channel(2);
    let mut child = parent
        .fork_to_with_commands(|| command_counter(5), destination, child_input)
        .await
        .unwrap();
    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            child.prepare(),
            child_sender.submit(CommandCall::new("adjust", json!({ "amount": 2 })))
        )
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let response = response.unwrap();
    assert_eq!(response.outcome, CommandOutcome::Output(json!(7)));
    assert!(projection_document(&response.projection).has_field("counter_value", "7"));
    assert!(projection_document(&response.projection).has_element("command_result"));
    assert!(poll!(queued_parent.as_mut()).is_pending());
    child.shutdown().await.unwrap();
    assert!(matches!(
        child_sender
            .submit(CommandCall::new("adjust", json!({ "amount": 1 })))
            .await,
        Err(CommandInputError::Closed)
    ));
    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(parent.prepare(), queued_parent)
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let response = response.unwrap();
    assert_eq!(response.outcome, CommandOutcome::Output(json!(9)));
    assert!(projection_document(&response.projection).has_field("counter_value", "9"));
    assert_eq!(source_probe.declarations.load(Ordering::SeqCst), 1);
    assert!(source_probe.frames().is_empty());
    assert!(destination_probe.frames().is_empty());
    assert_eq!(
        destination_probe.shutdowns_completed.load(Ordering::SeqCst),
        1
    );
    assert_eq!(source_probe.shutdowns_started.load(Ordering::SeqCst), 0);
    parent.shutdown().await.unwrap();
}
