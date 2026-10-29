use std::{
    collections::VecDeque,
    future::Future,
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    panic::{panic_any, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{poll, stream, FutureExt};
use tokio::sync::Notify;

use super::{Application, ApplicationFault, ApplicationFaultReason, ApplicationFaultStage};
use crate::{
    component::{
        execution::{
            CommandCall, CommandInput, CommandOutcome, ContextWindow, ForkableReactionPort, Frame,
            FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
            ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall, ReactionPort,
            ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
        task::TaskSupervisorStatus,
    },
    test_support::{element_text, projection_items},
    transcript::{CanonicalInputItem, InstructionAuthority},
};

const TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_TARGET: AtomicU64 = AtomicU64::new(50_000);

#[derive(Clone)]
struct CapturedFrame {
    target: TargetIdentity,
    revision: FrameRevision,
    basis: FrameBasis,
    replay: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    tools: Vec<String>,
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
                    serde_json::json!({ "amount": amount }).to_string(),
                )
                .unwrap(),
            },
            ProviderFact::ReactionCompleted { primary_text: None },
        ])
    }
}

#[derive(Default)]
struct PortProbe {
    frames: Mutex<Vec<CapturedFrame>>,
    scripts: Mutex<VecDeque<Script>>,
    submitted: Notify,
    forks: AtomicUsize,
    fork_gate: Mutex<Option<Arc<Notify>>>,
    fork_started: Notify,
    validations: AtomicUsize,
    reject_preflight: AtomicBool,
    shutdown_gate: Mutex<Option<Arc<Notify>>>,
    shutdown_started: Notify,
    shutdowns_started: AtomicUsize,
    shutdowns_completed: AtomicUsize,
    panic_on_shutdown: AtomicBool,
}

impl PortProbe {
    fn scripted(scripts: impl IntoIterator<Item = Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            ..Self::default()
        })
    }

    fn frames(&self) -> Vec<CapturedFrame> {
        self.frames.lock().unwrap().clone()
    }
}

struct ForkPort {
    declaration: TargetDeclaration,
    history: Vec<CanonicalInputItem>,
    probe: Arc<PortProbe>,
    fork_probe: Option<Arc<PortProbe>>,
}

fn new_declaration(profile: FrameProfile) -> TargetDeclaration {
    TargetDeclaration::full(
        TargetIdentity::new(
            NonZeroU128::new(u128::from(NEXT_TARGET.fetch_add(1, Ordering::Relaxed))).unwrap(),
        ),
        TargetEpoch::new(NonZeroU64::new(1).unwrap()),
        profile,
    )
}

fn port(parent: Arc<PortProbe>, child: Arc<PortProbe>) -> ForkPort {
    ForkPort {
        declaration: new_declaration(FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 262_144,
                max_component_bytes: 32_768,
                context_window: ContextWindow::NotApplicable,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        )),
        history: Vec::new(),
        probe: parent,
        fork_probe: Some(child),
    }
}

fn incompatible_snapshot() -> ReactionPortFault {
    ReactionPortFault::retryable(
        ReactionPortFaultCode::Rejected,
        ReactionPortFaultReason::ForkSnapshotIncompatible,
    )
}

#[async_trait]
impl ForkableReactionPort for ForkPort {
    async fn fork(&mut self) -> Result<Self, ReactionPortFault> {
        self.probe.forks.fetch_add(1, Ordering::SeqCst);
        self.probe.fork_started.notify_one();
        let gate = self.probe.fork_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(Self {
            declaration: new_declaration(self.declaration.profile().clone()),
            history: self.history.clone(),
            probe: self.fork_probe.clone().expect("configured child probe"),
            fork_probe: None,
        })
    }

    fn validate_fork_frame(&self, frame: &Frame) -> Result<(), ReactionPortFault> {
        self.probe.validations.fetch_add(1, Ordering::SeqCst);
        frame
            .check_handoff_precondition(&self.declaration)
            .map_err(|_| incompatible_snapshot())?;
        if self.probe.reject_preflight.load(Ordering::SeqCst)
            || frame.basis() != FrameBasis::Full
            || !frame.submission().replay().starts_with(&self.history)
        {
            return Err(incompatible_snapshot());
        }
        Ok(())
    }
}

#[async_trait]
impl ReactionPort for ForkPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        if frame.basis() == FrameBasis::Full {
            assert!(frame.submission().replay().starts_with(&self.history));
            self.history = frame.submission().replay().to_vec();
        } else {
            assert!(self.history.ends_with(frame.submission().replay()));
        }
        self.history.extend(
            frame
                .submission()
                .staged_inputs()
                .iter()
                .chain(frame.submission().projection().items())
                .filter(|item| {
                    !matches!(
                        item,
                        CanonicalInputItem::Instruction {
                            authority: InstructionAuthority::System,
                            ..
                        }
                    )
                })
                .cloned(),
        );
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        self.probe.frames.lock().unwrap().push(CapturedFrame {
            target: frame.target(),
            revision: frame.revision(),
            basis: frame.basis(),
            replay: frame.submission().replay().to_vec(),
            staged: frame.submission().staged_inputs().to_vec(),
            projection: frame.submission().projection().items().to_vec(),
            tools: frame.submission().tools().names().to_vec(),
        });
        self.probe.submitted.notify_one();
        let script = self
            .probe
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(Script::completed);
        match script {
            Script::Facts(facts) => {
                for fact in &facts {
                    match fact {
                        ProviderFact::TextSealed { phase, text, .. } => self
                            .history
                            .push(CanonicalInputItem::assistant_text(text.clone(), *phase)),
                        ProviderFact::ToolCall { call, .. } => {
                            self.history.push(
                                CanonicalInputItem::tool_call(
                                    call.call_id(),
                                    call.name(),
                                    call.raw_arguments(),
                                )
                                .unwrap(),
                            );
                        }
                        _ => {}
                    }
                }
                Ok(Box::pin(stream::iter(facts.into_iter().map(Ok))))
            }
            Script::Gated(gate) => Ok(Box::pin(stream::once(async move {
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
        if self.probe.panic_on_shutdown.load(Ordering::SeqCst) {
            panic_any(PanicMarker(76));
        }
    }
}

#[derive(Clone)]
struct CounterSnapshot {
    value: i64,
    feedback: String,
}

#[derive(Clone)]
struct CounterProps {
    initial: CounterSnapshot,
    calls: Arc<AtomicUsize>,
    exported: Arc<Mutex<Option<Signal<CounterSnapshot>>>>,
    demand: Arc<Mutex<Option<ReactionRequest>>>,
    tool_gate: Option<Arc<Notify>>,
    tool_started: Arc<Notify>,
}

impl CounterProps {
    fn new(value: i64) -> Self {
        Self {
            initial: CounterSnapshot {
                value,
                feedback: "Ready for an adjustment.".to_owned(),
            },
            calls: Arc::default(),
            exported: Arc::default(),
            demand: Arc::default(),
            tool_gate: None,
            tool_started: Arc::default(),
        }
    }

    fn snapshot(&self) -> CounterSnapshot {
        self.exported
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .with(Clone::clone)
            .unwrap()
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Adjustment {
    amount: i64,
}

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct AdjustmentResult {
    value: i64,
    observed_value: i64,
}

#[component]
fn counter(props: CounterProps) -> Component {
    let state = use_signal(move || props.initial);
    *props.exported.lock().unwrap() = Some(state.clone());
    *props.demand.lock().unwrap() = Some(use_reaction_request());
    let snapshot = state.with(Clone::clone).unwrap();
    let observed_value = snapshot.value;
    let value = snapshot.value;
    let feedback = snapshot.feedback;
    view! {
        counter_value { "{value}" }
        action_result { "{feedback}" }
        next_action { "Call adjust with an integer amount to change the current value." }
        NativeToolCall {
            name: "adjust",
            description: "Apply an integer adjustment and return its actual result.",
            on_call: move |input: Adjustment| {
                let state = state.clone();
                let calls = props.calls.clone();
                let started = props.tool_started.clone();
                let gate = props.tool_gate.clone();
                async move {
                    started.notify_one();
                    if let Some(gate) = gate {
                        gate.notified().await;
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    state.update(|snapshot| {
                        snapshot.value += input.amount;
                        snapshot.feedback = format!("Adjusted by {}; value is {}.", input.amount, snapshot.value);
                        AdjustmentResult { value: snapshot.value, observed_value }
                    })
                }
            },
        }
    }
}

fn staged_result(frame: &CapturedFrame, call_id: &str) -> AdjustmentResult {
    let [CanonicalInputItem::ToolResult {
        call_id: actual,
        content,
    }] = frame.staged.as_slice()
    else {
        panic!("expected one delivered tool result")
    };
    assert_eq!(actual, call_id);
    serde_json::from_str(content).unwrap()
}

async fn drive_until_handoff<F: Future>(
    future: &mut Pin<Box<F>>,
    probe: &PortProbe,
    expected: usize,
) {
    tokio::time::timeout(TIMEOUT, async {
        while probe.frames.lock().unwrap().len() < expected {
            tokio::select! {
                _ = future.as_mut() => panic!("reaction returned before expected handoff"),
                _ = probe.submitted.notified() => {}
            }
        }
    })
    .await
    .expect("reaction reaches its handoff");
}

#[tokio::test]
async fn fork_inherits_history_and_delivers_independent_interactions_while_parent_is_pending() {
    let release_parent = Arc::new(Notify::new());
    let parent_probe = PortProbe::scripted([
        Script::adjustment("source-adjust", 3),
        Script::completed(),
        Script::Gated(release_parent.clone()),
        Script::completed(),
    ]);
    let child_probe = PortProbe::scripted([
        Script::adjustment("child-first", 2),
        Script::adjustment("child-next", 4),
        Script::completed(),
    ]);
    let parent_props = CounterProps::new(10);
    let root_props = parent_props.clone();
    let mut parent = Application::mount(
        move || counter(root_props.clone()),
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();
    assert!(parent.react().await.unwrap().is_continue());
    assert!(parent.react().await.unwrap().is_continue());
    let common_history = parent.session.canonical_history.transcript.items().to_vec();
    let parent_frames = parent_probe.frames();
    let parent_head = parent_frames.last().unwrap().revision;
    let mut child_props = CounterProps::new(0);
    child_props.initial = parent_props.snapshot();
    let fork_props = child_props.clone();
    let mut child = parent
        .fork(move || counter(fork_props.clone()))
        .await
        .unwrap();
    assert_eq!(child_probe.validations.load(Ordering::SeqCst), 1);
    assert!(child_probe.frames().is_empty());
    assert_eq!(parent_props.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child_props.calls.load(Ordering::SeqCst), 0);

    let mut pending_parent = Box::pin(parent.react());
    drive_until_handoff(&mut pending_parent, &parent_probe, 3).await;
    assert!(poll!(pending_parent.as_mut()).is_pending());
    for _ in 0..3 {
        assert!(child.react().await.unwrap().is_continue());
    }
    let child_frames = child_probe.frames();
    assert_eq!(child_frames[0].basis, FrameBasis::Full);
    assert_eq!(child_frames[0].replay, common_history);
    assert_ne!(child_frames[0].target, parent_frames[0].target);
    assert_eq!(child_frames[0].tools, ["adjust"]);
    assert_eq!(
        element_text(&child_frames[0].projection, "counter_value"),
        ["13"]
    );
    assert_eq!(
        element_text(&child_frames[0].projection, "next_action"),
        ["Call adjust with an integer amount to change the current value."]
    );
    for (index, call_id, value, observed) in [(1, "child-first", 15, 13), (2, "child-next", 19, 15)]
    {
        assert_eq!(
            child_frames[index].basis,
            FrameBasis::DeltaFrom(child_frames[index - 1].revision)
        );
        assert_eq!(
            staged_result(&child_frames[index], call_id),
            AdjustmentResult {
                value,
                observed_value: observed
            }
        );
        assert_eq!(
            element_text(&child_frames[index].projection, "counter_value"),
            [value.to_string().as_str()]
        );
        assert_eq!(
            element_text(&child_frames[index].projection, "action_result"),
            [format!("Adjusted by {}; value is {}.", value - observed, value).as_str()]
        );
    }
    assert_eq!(parent_props.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child_props.calls.load(Ordering::SeqCst), 2);
    assert_eq!(parent_props.snapshot().value, 13);
    assert_eq!(child_props.snapshot().value, 19);
    let child_exit = child.exit_handle();
    child_exit.request(ExitReason::Requested).unwrap();
    assert_eq!(
        child.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    child.shutdown().await.unwrap();
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
    assert_eq!(parent_probe.shutdowns_started.load(Ordering::SeqCst), 0);
    assert!(poll!(pending_parent.as_mut()).is_pending());
    release_parent.notify_one();
    assert!(pending_parent.await.unwrap().is_continue());
    assert!(parent.react().await.unwrap().is_continue());
    assert_eq!(
        parent_probe.frames()[2].basis,
        FrameBasis::DeltaFrom(parent_head)
    );
    assert!(!parent.session.canonical_history.transcript.items().iter().any(|item| {
        matches!(item, CanonicalInputItem::ToolCall { call_id, .. } if call_id.starts_with("child-"))
    }));
    parent.shutdown().await.unwrap();
    assert_eq!(parent_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fork_rejects_staged_results_and_cancellation_fallback_without_consuming_work() {
    for cancelled in [false, true] {
        let parent_probe =
            PortProbe::scripted([Script::adjustment("unsettled-call", 3), Script::completed()]);
        let child_probe = PortProbe::scripted([]);
        let mut props = CounterProps::new(10);
        if cancelled {
            props.tool_gate = Some(Arc::new(Notify::new()));
        }
        let root_props = props.clone();
        let mut parent = Application::mount(
            move || counter(root_props.clone()),
            port(parent_probe.clone(), child_probe.clone()),
        )
        .unwrap();
        if cancelled {
            let mut reaction = Box::pin(parent.react());
            tokio::time::timeout(TIMEOUT, async {
                tokio::select! {
                    _ = props.tool_started.notified() => {},
                    _ = reaction.as_mut() => panic!("blocked handler completed"),
                }
            })
            .await
            .unwrap();
            drop(reaction);
        } else {
            assert!(parent.react().await.unwrap().is_continue());
        }
        props
            .demand
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .request()
            .unwrap();
        let history = parent.session.canonical_history.transcript.clone();
        let staged = parent
            .session
            .target_delivery
            .tool_outputs
            .ordered_outputs()
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>();
        let error = match parent.fork(|| view! { "unreachable child" }).await {
            Ok(_) => panic!("fork accepted unsettled tool work"),
            Err(error) => error,
        };
        assert_eq!(error.stage(), ApplicationFaultStage::Fork);
        assert_eq!(error.reason(), ApplicationFaultReason::ForkNotReady);
        assert_eq!(parent_probe.frames().len(), 1);
        assert_eq!(parent_probe.forks.load(Ordering::SeqCst), 0);
        assert_eq!(parent.session.canonical_history.transcript, history);
        assert_eq!(
            parent
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .map(|(_, item)| item.clone())
                .collect::<Vec<_>>(),
            staged
        );
        assert!(parent.take_reaction_request().unwrap());
        assert!(parent.react().await.unwrap().is_continue());
        assert_eq!(parent_probe.frames()[1].staged, staged);
        if cancelled {
            assert!(
                matches!(staged.as_slice(), [CanonicalInputItem::ToolResult { content, .. }]
                if content == "Tool execution was cancelled; its outcome is unknown.")
            );
            assert_eq!(props.calls.load(Ordering::SeqCst), 0);
        }
        let mut fork_props = CounterProps::new(0);
        fork_props.initial = props.snapshot();
        let child = parent
            .fork(move || counter(fork_props.clone()))
            .await
            .unwrap();
        child.shutdown().await.unwrap();
        parent.shutdown().await.unwrap();
    }
}

#[component]
fn task_probe(factories: Arc<AtomicUsize>) -> Component {
    use_future(move || {
        factories.fetch_add(1, Ordering::SeqCst);
        async { std::future::pending::<()>().await }
    });
    view! { status { "A new mount task may begin after snapshot validation." } }
}

#[tokio::test]
async fn failed_preflight_starts_no_component_tasks_and_cleanup_survives_a_dropped_waiter() {
    let parent_probe = PortProbe::scripted([Script::completed()]);
    let child_probe = PortProbe::scripted([]);
    child_probe.reject_preflight.store(true, Ordering::SeqCst);
    let cleanup_gate = Arc::new(Notify::new());
    *child_probe.shutdown_gate.lock().unwrap() = Some(cleanup_gate.clone());
    let factories = Arc::new(AtomicUsize::new(0));
    let mut parent = Application::mount(
        || view! { source { "ready" } },
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();
    let child_factories = factories.clone();
    let mut fork = Box::pin(parent.fork(move || task_probe(child_factories.clone())));
    let result = tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            result = fork.as_mut() => Some(result),
            _ = child_probe.shutdown_started.notified() => None,
        }
    })
    .await
    .unwrap();
    if let Some(result) = result {
        let error: ApplicationFault = match result {
            Ok(_) => panic!("preflight rejection returned an Application"),
            Err(error) => error,
        };
        assert_eq!(error.stage(), ApplicationFaultStage::Fork);
        assert!(matches!(
            error.reason(),
            ApplicationFaultReason::ForkSnapshotIncompatible
                | ApplicationFaultReason::Port(ReactionPortFaultReason::ForkSnapshotIncompatible)
        ));
    }
    drop(fork);
    if child_probe.shutdowns_started.load(Ordering::SeqCst) == 0 {
        tokio::time::timeout(TIMEOUT, child_probe.shutdown_started.notified())
            .await
            .unwrap();
    }
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert_eq!(child_probe.validations.load(Ordering::SeqCst), 1);
    assert!(child_probe.frames().is_empty());
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 0);
    assert!(parent.react().await.unwrap().is_continue());
    assert_eq!(parent_probe.frames().len(), 1);
    cleanup_gate.notify_one();
    tokio::time::timeout(TIMEOUT, parent.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
    assert_eq!(parent_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[component]
fn command_counter(initial: i64) -> Component {
    let value = use_signal(move || initial);
    let current = value.with(|value| *value).unwrap();
    use_wait_for_command();
    view! {
        counter_value { "{current}" }
        Action {
            name: "adjust",
            description: "Adjust this application's counter.",
            on_call: move |input: Adjustment| value.update(|value| { *value += input.amount; *value }),
        }
    }
}

#[tokio::test]
async fn fork_with_commands_uses_an_independent_inbox_and_shutdown_scope() {
    let parent_probe = PortProbe::scripted([]);
    let child_probe = PortProbe::scripted([]);
    let (parent_sender, parent_input) = CommandInput::channel(2);
    let mut parent = Application::mount_with_commands(
        || command_counter(5),
        port(parent_probe.clone(), child_probe.clone()),
        parent_input,
    )
    .unwrap();
    let mut queued_parent = Box::pin(parent_sender.submit(CommandCall::new(
        "adjust",
        serde_json::json!({ "amount": 4 }),
    )));
    assert!(poll!(queued_parent.as_mut()).is_pending());
    let (child_sender, child_input) = CommandInput::channel(2);
    let mut child = parent
        .fork_with_commands(|| command_counter(5), child_input)
        .await
        .unwrap();
    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            child.prepare(),
            child_sender.submit(CommandCall::new(
                "adjust",
                serde_json::json!({ "amount": 2 }),
            ))
        )
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let response = response.unwrap();
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(serde_json::json!(7))
    );
    assert_eq!(
        element_text(projection_items(&response.projection), "counter_value"),
        ["7"]
    );
    assert!(poll!(queued_parent.as_mut()).is_pending());
    child.shutdown().await.unwrap();
    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(parent.prepare(), queued_parent)
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    assert_eq!(
        response.unwrap().outcome,
        CommandOutcome::Output(serde_json::json!(9))
    );
    assert!(parent_probe.frames().is_empty());
    assert!(child_probe.frames().is_empty());
    parent.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_exit_during_port_fork_prevents_a_child_from_starting() {
    let parent_probe = PortProbe::scripted([]);
    let child_probe = PortProbe::scripted([]);
    let release_fork = Arc::new(Notify::new());
    *parent_probe.fork_gate.lock().unwrap() = Some(release_fork.clone());
    let factories = Arc::new(AtomicUsize::new(0));
    let mut parent = Application::mount(
        || view! { source { "ready" } },
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();
    let exit = parent.exit_handle();
    let child_factories = factories.clone();
    let mut fork = Box::pin(parent.fork(move || task_probe(child_factories.clone())));
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = parent_probe.fork_started.notified() => {},
            _ = fork.as_mut() => panic!("port fork returned before release"),
        }
    })
    .await
    .unwrap();
    exit.request(ExitReason::Requested).unwrap();
    release_fork.notify_one();
    let error = match tokio::time::timeout(TIMEOUT, fork).await.unwrap() {
        Ok(_) => panic!("fork published a child after the source exited"),
        Err(error) => error,
    };
    assert_eq!(error.stage(), ApplicationFaultStage::Fork);
    assert_eq!(error.reason(), ApplicationFaultReason::ForkNotReady);
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert!(child_probe.frames().is_empty());
    assert_eq!(
        parent.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    parent.shutdown().await.unwrap();
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[derive(Debug, PartialEq)]
struct PanicMarker(u64);

#[tokio::test]
async fn direct_child_render_panic_preserves_its_payload_cleans_the_child_and_closes_the_source() {
    let parent_probe = PortProbe::scripted([]);
    let child_probe = PortProbe::scripted([]);
    let mut parent = Application::mount(
        || view! { source { "ready" } },
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();
    let result = AssertUnwindSafe(parent.fork(|| panic_any(PanicMarker(73))))
        .catch_unwind()
        .await;
    let payload = match result {
        Err(payload) => payload,
        Ok(_) => panic!("child root panic was converted into a fork result"),
    };
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(73))
    );
    let error = parent.react().await.unwrap_err();
    assert_eq!(error.reason(), ApplicationFaultReason::ForkCleanup);
    assert!(parent_probe.frames().is_empty());
    assert!(child_probe.frames().is_empty());
    assert_eq!(child_probe.validations.load(Ordering::SeqCst), 0);
    let error = tokio::time::timeout(TIMEOUT, parent.shutdown())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.reason(), ApplicationFaultReason::ForkCleanup);
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
    assert_eq!(parent_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[component]
fn panicking_source(gate: Arc<Notify>) -> Component {
    use_future(move || async move {
        gate.notified().await;
        panic_any(PanicMarker(74));
    });
    view! { source { "waiting for supervised work" } }
}

#[tokio::test]
async fn supervised_source_panic_interrupts_a_pending_port_fork_with_the_original_payload() {
    let parent_probe = PortProbe::scripted([]);
    let child_probe = PortProbe::scripted([]);
    *parent_probe.fork_gate.lock().unwrap() = Some(Arc::new(Notify::new()));
    let panic_gate = Arc::new(Notify::new());
    let root_gate = panic_gate.clone();
    let mut parent = Application::mount(
        move || panicking_source(root_gate.clone()),
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();
    let mut fork =
        Box::pin(AssertUnwindSafe(parent.fork(|| view! { child { "ready" } })).catch_unwind());
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = parent_probe.fork_started.notified() => {},
            _ = fork.as_mut() => panic!("port fork returned before release"),
        }
    })
    .await
    .unwrap();
    panic_gate.notify_one();
    let payload = match tokio::time::timeout(TIMEOUT, fork).await.unwrap() {
        Err(payload) => payload,
        Ok(_) => panic!("supervised task panic was converted into a fork result"),
    };
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(74))
    );
    assert_eq!(
        parent.react().await.unwrap_err().reason(),
        ApplicationFaultReason::ComponentRuntime
    );
    assert!(parent_probe.frames().is_empty());
    assert_eq!(child_probe.validations.load(Ordering::SeqCst), 0);
    assert_eq!(child_probe.shutdowns_started.load(Ordering::SeqCst), 0);
    assert!(tokio::time::timeout(TIMEOUT, parent.shutdown())
        .await
        .unwrap()
        .is_err());
    assert_eq!(parent_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_drains_cancelled_fork_cleanup_before_rethrowing_the_original_source_panic() {
    let parent_probe = PortProbe::scripted([]);
    let child_probe = PortProbe::scripted([]);
    child_probe.reject_preflight.store(true, Ordering::SeqCst);
    child_probe.panic_on_shutdown.store(true, Ordering::SeqCst);
    let cleanup_gate = Arc::new(Notify::new());
    *child_probe.shutdown_gate.lock().unwrap() = Some(cleanup_gate.clone());
    let panic_gate = Arc::new(Notify::new());
    let root_gate = panic_gate.clone();
    let mut parent = Application::mount(
        move || panicking_source(root_gate.clone()),
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();

    // The failed child already has an independent cleanup owner. Cancel only
    // its waiter while the child port is still waiting to acknowledge shutdown.
    let mut fork = Box::pin(parent.fork(|| view! { child { "rejected before publication" } }));
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = child_probe.shutdown_started.notified() => {},
            _ = fork.as_mut() => panic!("failed fork returned before child cleanup finished"),
        }
    })
    .await
    .unwrap();
    drop(fork);
    assert_eq!(parent.fork_cleanups.len(), 1);
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 0);

    // This panic is fresh: no parent driver has observed or consumed it yet.
    let monitor = parent.tasks.panic_monitor();
    panic_gate.notify_one();
    tokio::time::timeout(TIMEOUT, monitor.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.status(), TaskSupervisorStatus::Panicked);
    let mut shutdown = Box::pin(AssertUnwindSafe(parent.shutdown()).catch_unwind());
    assert!(poll!(shutdown.as_mut()).is_pending());
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 0);

    // Child cleanup now finishes its gated work and panics with a different
    // payload. It must be drained without replacing the original source panic.
    cleanup_gate.notify_one();
    let payload = tokio::time::timeout(TIMEOUT, shutdown)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(74))
    );
    assert_eq!(child_probe.shutdowns_started.load(Ordering::SeqCst), 1);
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
    assert!(parent_probe.frames().is_empty());
    assert!(child_probe.frames().is_empty());
}

struct SiblingDrop(Arc<AtomicUsize>);

impl Drop for SiblingDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct CleanupTasksProps {
    panic_gate: Arc<Notify>,
    sibling_started: Arc<Notify>,
    sibling_dropped: Arc<AtomicUsize>,
}

#[component]
fn cleanup_tasks(props: CleanupTasksProps) -> Component {
    use_future(move || async move {
        props.panic_gate.notified().await;
        panic_any(PanicMarker(75));
    });
    use_future(move || {
        let guard = SiblingDrop(props.sibling_dropped);
        async move {
            let _guard = guard;
            props.sibling_started.notify_one();
            std::future::pending::<()>().await;
        }
    });
    view! { child { "wait for mount tasks to settle before port shutdown acknowledgement" } }
}

#[tokio::test]
async fn failed_child_cleanup_drains_siblings_and_port_before_propagating_its_task_panic() {
    let parent_probe = PortProbe::scripted([]);
    let child_probe = PortProbe::scripted([]);
    let shutdown_gate = Arc::new(Notify::new());
    *child_probe.shutdown_gate.lock().unwrap() = Some(shutdown_gate.clone());
    let props = CleanupTasksProps {
        panic_gate: Arc::default(),
        sibling_started: Arc::default(),
        sibling_dropped: Arc::default(),
    };
    let root_props = props.clone();
    let mut parent = Application::mount(
        || view! { source { "ready" } },
        port(parent_probe.clone(), child_probe.clone()),
    )
    .unwrap();
    let child = parent
        .fork(move || cleanup_tasks(root_props.clone()))
        .await
        .unwrap();
    tokio::time::timeout(TIMEOUT, props.sibling_started.notified())
        .await
        .unwrap();
    let monitor = child.tasks.panic_monitor();
    props.panic_gate.notify_one();
    tokio::time::timeout(TIMEOUT, monitor.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.status(), TaskSupervisorStatus::Panicked);

    let mut cleanup = Box::pin(AssertUnwindSafe(child.shutdown_failed_fork()).catch_unwind());
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = child_probe.shutdown_started.notified() => {},
            _ = cleanup.as_mut() => panic!("cleanup propagated panic before the port acknowledged shutdown"),
        }
    })
    .await
    .unwrap();
    assert_eq!(props.sibling_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 0);
    assert!(poll!(cleanup.as_mut()).is_pending());
    shutdown_gate.notify_one();
    let payload = tokio::time::timeout(TIMEOUT, cleanup)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        payload.downcast_ref::<PanicMarker>(),
        Some(&PanicMarker(75))
    );
    assert_eq!(child_probe.shutdowns_completed.load(Ordering::SeqCst), 1);
    assert!(parent.react().await.unwrap().is_continue());
    parent.shutdown().await.unwrap();
}
