#[path = "support/pom.rs"]
mod pom_support;

use std::{
    collections::VecDeque,
    convert::Infallible,
    future::IntoFuture,
    num::{NonZeroU128, NonZeroU64},
    panic::{panic_any, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, ForkableReactionPort, Frame, FrameBasis, FrameCapabilities,
            FrameConstraints, FrameProfile, FrameRevision, ProviderFact, ProviderFactStream,
            ProviderOutputKey, ProviderToolCall, ReactionKind, ReactionOutcome, ReactionPort,
            ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
        EventContext, ReactionId,
    },
    llm_call::TextTurnEvent,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::{poll, FutureExt, StreamExt};
use pom_support::{combine, InspectPom};
use tokio::sync::Notify;

const DEADLINE: Duration = Duration::from_secs(5);
const OUTPUT: ProviderOutputKey = ProviderOutputKey::new(1);
static NEXT_TARGET: AtomicU64 = AtomicU64::new(80_000);

#[derive(Clone)]
struct CapturedFrame {
    label: &'static str,
    target: TargetIdentity,
    revision: FrameRevision,
    basis: FrameBasis,
    replay: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    tools: Vec<String>,
}

impl CapturedFrame {
    fn items(&self) -> impl Iterator<Item = &CanonicalInputItem> {
        self.replay
            .iter()
            .chain(&self.staged)
            .chain(&self.projection)
    }

    fn assistant_text(&self) -> Vec<&str> {
        self.items()
            .filter_map(|item| match item {
                CanonicalInputItem::AssistantText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn native_items(&self) -> Vec<CanonicalInputItem> {
        self.items()
            .filter(|item| {
                matches!(
                    item,
                    CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
                )
            })
            .cloned()
            .collect()
    }

    fn observes_total(&self, total: i64) -> bool {
        combine(self.items().filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
            _ => None,
        }))
        .has_field("total", &total.to_string())
    }
}

struct Script {
    label: &'static str,
    head: Vec<ProviderFact>,
    gate: Option<Arc<Notify>>,
    tail: Vec<ProviderFact>,
}

impl Script {
    fn text(label: &'static str) -> Self {
        Self {
            label,
            head: text_facts(label),
            gate: None,
            tail: Vec::new(),
        }
    }

    fn gated_text(label: &'static str, gate: Arc<Notify>) -> Self {
        Self {
            label,
            head: vec![ProviderFact::TextDelta {
                output: OUTPUT,
                phase: None,
                delta: label.to_owned(),
            }],
            gate: Some(gate),
            tail: vec![
                ProviderFact::TextSealed {
                    output: OUTPUT,
                    phase: None,
                    text: label.to_owned(),
                },
                ProviderFact::ReactionCompleted {
                    primary_text: Some(OUTPUT),
                },
            ],
        }
    }

    fn adjustment(
        label: &'static str,
        call_id: &str,
        amount: i64,
        gate: Option<Arc<Notify>>,
    ) -> Self {
        Self {
            label,
            head: vec![ProviderFact::ToolCall {
                output: OUTPUT,
                ordinal: 1,
                call: ProviderToolCall::new(
                    call_id,
                    "adjust",
                    serde_json::json!({"amount": amount}).to_string(),
                )
                .unwrap(),
            }],
            gate,
            tail: vec![ProviderFact::ReactionCompleted { primary_text: None }],
        }
    }
}

fn text_facts(text: &str) -> Vec<ProviderFact> {
    vec![
        ProviderFact::TextDelta {
            output: OUTPUT,
            phase: None,
            delta: text.to_owned(),
        },
        ProviderFact::TextSealed {
            output: OUTPUT,
            phase: None,
            text: text.to_owned(),
        },
        ProviderFact::ReactionCompleted {
            primary_text: Some(OUTPUT),
        },
    ]
}

#[derive(Default)]
struct Probe {
    scripts: Mutex<VecDeque<Script>>,
    frames: Mutex<Vec<CapturedFrame>>,
    events: Mutex<Vec<(String, EventContext)>>,
    tools: Mutex<Vec<(i64, EventContext)>>,
    completions: Mutex<Vec<EventContext>>,
    forked_targets: Mutex<Vec<TargetIdentity>>,
    shutdowns: Mutex<Vec<TargetIdentity>>,
    changed: Notify,
    force_main_full: AtomicBool,
    panic_fork: AtomicBool,
    panic_declare: AtomicBool,
    panic_payload: Arc<()>,
    maintenance_entered: Notify,
    maintenance_release: Notify,
}

impl Probe {
    fn scripted(scripts: impl IntoIterator<Item = Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            ..Self::default()
        })
    }

    async fn frame_count(&self, count: usize) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.frames.lock().unwrap().len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("reactions start without being awaited");
    }

    async fn tool_count(&self, count: usize) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.tools.lock().unwrap().len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("native actions are dispatched while streams remain active");
    }

    async fn completed(&self, reaction: ReactionId) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self
                    .completions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|context| context.reaction_id() == Some(reaction))
                {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("an application owns work after its last handle is dropped");
    }

    fn frame(&self, label: &str) -> CapturedFrame {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .find(|frame| frame.label == label)
            .expect("script was submitted")
            .clone()
    }
}

struct ProbePort {
    declaration: TargetDeclaration,
    probe: Arc<Probe>,
    main: bool,
}

fn declaration(profile: FrameProfile) -> TargetDeclaration {
    TargetDeclaration::full(
        TargetIdentity::new(
            NonZeroU128::new(u128::from(NEXT_TARGET.fetch_add(1, Ordering::Relaxed))).unwrap(),
        ),
        TargetEpoch::new(NonZeroU64::MIN),
        profile,
    )
}

impl ProbePort {
    fn new(probe: Arc<Probe>) -> Self {
        Self {
            declaration: declaration(FrameProfile::new(
                FrameConstraints {
                    max_frame_bytes: 1_048_576,
                    max_component_bytes: 262_144,
                    context_window: ContextWindow::NotApplicable,
                    reserved_output_tokens: None,
                },
                FrameCapabilities::new(true)
                    .with_native_tool_calls(true)
                    .with_text_delta_output(true),
            )),
            probe,
            main: true,
        }
    }
}

#[async_trait]
impl ReactionPort for ProbePort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        if !self.main && self.probe.panic_declare.swap(false, Ordering::AcqRel) {
            panic_any(Arc::clone(&self.probe.panic_payload));
        }
        if self.main && self.probe.force_main_full.swap(false, Ordering::AcqRel) {
            self.declaration = TargetDeclaration::full(
                self.declaration.identity(),
                TargetEpoch::new(
                    NonZeroU64::new(self.declaration.continuity().epoch().get().get() + 1).unwrap(),
                ),
                self.declaration.profile().clone(),
            );
        }
        Ok(self.declaration.clone())
    }

    fn fork_temporary(&mut self) -> Result<Box<dyn ReactionPort>, ReactionPortFault> {
        if self.probe.panic_fork.swap(false, Ordering::AcqRel) {
            panic_any(Arc::clone(&self.probe.panic_payload));
        }
        let branch = Self {
            declaration: declaration(self.declaration.profile().clone()),
            probe: Arc::clone(&self.probe),
            main: false,
        };
        self.probe
            .forked_targets
            .lock()
            .unwrap()
            .push(branch.declaration.identity());
        Ok(Box::new(branch))
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        let script = self
            .probe
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("one test script per request");
        self.probe.frames.lock().unwrap().push(CapturedFrame {
            label: script.label,
            target: frame.target(),
            revision: frame.revision(),
            basis: frame.basis(),
            replay: frame.submission().replay().to_vec(),
            staged: frame.submission().staged_inputs().to_vec(),
            projection: frame.submission().projection().items().to_vec(),
            tools: frame.submission().tools().names().to_vec(),
        });
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        self.probe.changed.notify_waiters();
        let tail = futures::stream::once(async move {
            if let Some(gate) = script.gate {
                gate.notified().await;
            }
            script.tail
        })
        .flat_map(futures::stream::iter);
        Ok(Box::pin(
            futures::stream::iter(script.head).chain(tail).map(Ok),
        ))
    }

    async fn shutdown(&mut self) {
        self.probe
            .shutdowns
            .lock()
            .unwrap()
            .push(self.declaration.identity());
        self.probe.changed.notify_waiters();
    }
}

#[async_trait]
impl ForkableReactionPort for ProbePort {
    async fn fork(&mut self) -> Result<Self, ReactionPortFault> {
        self.probe.maintenance_entered.notify_one();
        self.probe.maintenance_release.notified().await;
        Ok(Self {
            declaration: declaration(self.declaration.profile().clone()),
            probe: Arc::clone(&self.probe),
            main: false,
        })
    }

    fn validate_fork_frame(&self, frame: &Frame) -> Result<(), ReactionPortFault> {
        frame
            .check_handoff_precondition(&self.declaration)
            .expect("the independent fork has a fresh target and complete first Frame");
        assert_eq!(frame.basis(), FrameBasis::Full);
        Ok(())
    }
}

struct MainOnlyPort(ProbePort);

#[async_trait]
impl ReactionPort for MainOnlyPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.0.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.0.submit(frame).await
    }

    async fn shutdown(&mut self) {
        self.0.shutdown().await;
    }
}

#[component]
fn observed_application(probe: Arc<Probe>) -> Component {
    let total = use_signal(|| 0_i64);
    let event_probe = Arc::clone(&probe);
    use_provider_event_handler(ProviderEvent::TEXT, move |context, event| {
        let probe = Arc::clone(&event_probe);
        async move {
            let text = match event {
                TextTurnEvent::TextDelta(text) | TextTurnEvent::TextComplete(text) => text,
            };
            probe.events.lock().unwrap().push((text, context));
            probe.changed.notify_waiters();
            Ok::<(), Infallible>(())
        }
    });
    let completion_probe = Arc::clone(&probe);
    use_reaction_completion(move |context| async move {
        completion_probe.completions.lock().unwrap().push(context);
        completion_probe.changed.notify_waiters();
        Ok::<(), Infallible>(())
    });
    let current = total.with(|value| *value).unwrap();
    view! {
        state {
            total { "{current}" }
            instruction { "Use adjust to change the total, then observe its actual result." }
        }
        {
            NativeToolCall::named("adjust").on_call(move |context, call| {
                let amount = serde_json::from_str::<serde_json::Value>(call.raw_arguments())
                    .unwrap()["amount"]
                    .as_i64()
                    .unwrap();
                let result = total.update(|value| {
                    *value += amount;
                    *value
                }).unwrap();
                probe.tools.lock().unwrap().push((amount, context));
                probe.changed.notify_waiters();
                async move { Ok::<_, Infallible>(call.output(result.to_string())) }
            })
        }
    }
}

#[tokio::test]
async fn handles_start_eagerly_and_each_clone_waits_without_owning_cancellation() {
    let first_gate = Arc::new(Notify::new());
    let detached_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::gated_text("first", Arc::clone(&first_gate)),
        Script::gated_text("detached", Arc::clone(&detached_gate)),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();

    let first = app.react(false).unwrap();
    assert_eq!(first.kind(), ReactionKind::Main);
    let id = first.id();
    probe.frame_count(1).await;
    let observer = first.clone();
    let second_observer = first.clone();
    let mut abandoned_waiter = Box::pin(first.clone().into_future());
    assert!(poll!(abandoned_waiter.as_mut()).is_pending());
    drop(abandoned_waiter);
    drop(first);
    first_gate.notify_one();
    let (first_result, second_result) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(observer.into_future(), second_observer.into_future())
    })
    .await
    .unwrap();
    assert_eq!(first_result.unwrap(), ReactionOutcome::Completed);
    assert_eq!(second_result.unwrap(), ReactionOutcome::Completed);
    probe.completed(id).await;

    let detached = app.react(false).unwrap();
    let detached_id = detached.id();
    assert_ne!(id, detached_id);
    assert_eq!(detached.kind(), ReactionKind::Main);
    drop(detached);
    probe.frame_count(2).await;
    detached_gate.notify_one();
    probe.completed(detached_id).await;
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_ephemeral_reactions_preserve_main_history_and_delivery_baseline() {
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::text("decision-one"),
        Script::text("decision-two"),
        Script::text("decision-three"),
        Script::text("next-main"),
        Script::text("history-check"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    app.react(false).unwrap().await.unwrap();
    let checkpoint = probe.frame("checkpoint");
    let mut targets = Vec::new();
    for label in ["decision-one", "decision-two", "decision-three"] {
        let reaction = app.react(true).unwrap();
        assert_eq!(reaction.kind(), ReactionKind::Temporary);
        assert_eq!(reaction.await.unwrap(), ReactionOutcome::Completed);
        let frame = probe.frame(label);
        assert_eq!(frame.basis, FrameBasis::Full);
        assert_eq!(frame.assistant_text(), ["checkpoint"]);
        assert_ne!(frame.target, checkpoint.target);
        assert!(!targets.contains(&frame.target));
        targets.push(frame.target);
    }

    let next = app.react(false).unwrap();
    assert_eq!(next.kind(), ReactionKind::Main);
    assert_eq!(next.await.unwrap(), ReactionOutcome::Completed);
    let continued = probe.frame("next-main");
    assert_eq!(continued.target, checkpoint.target);
    assert_eq!(continued.basis, FrameBasis::DeltaFrom(checkpoint.revision));

    probe.force_main_full.store(true, Ordering::Release);
    app.react(false).unwrap().await.unwrap();
    let replay = probe.frame("history-check");
    assert_eq!(replay.basis, FrameBasis::Full);
    assert_eq!(replay.assistant_text(), ["checkpoint", "next-main"]);
    assert!(probe.events.lock().unwrap().iter().all(|(text, context)| {
        !text.starts_with("decision-") || context.reaction_kind() == Some(ReactionKind::Temporary)
    }));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn ephemeral_without_a_checkpoint_is_rejected_without_claiming_main() {
    let main_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::gated_text("first-main", Arc::clone(&main_gate)),
        Script::text("first-temporary"),
        Script::text("next-main"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    assert!(app.react(true).is_err());
    assert!(probe.frames.lock().unwrap().is_empty());
    assert!(probe.forked_targets.lock().unwrap().is_empty());

    let main = app.react(false).unwrap();
    assert_eq!(main.kind(), ReactionKind::Main);
    probe.frame_count(1).await;
    assert!(app.react(true).is_err());
    assert_eq!(probe.frames.lock().unwrap().len(), 1);
    main_gate.notify_one();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);

    let temporary = app.react(true).unwrap();
    assert_eq!(temporary.kind(), ReactionKind::Temporary);
    assert_eq!(temporary.await.unwrap(), ReactionOutcome::Completed);
    let main = app.react(false).unwrap();
    assert_eq!(main.kind(), ReactionKind::Main);
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
    assert_eq!(
        probe.frame("next-main").target,
        probe.frame("first-main").target
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_idle_ephemeral_reaction_leaves_main_free_and_cancellation_stays_local() {
    let main_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("idle-temporary", Arc::new(Notify::new())),
        Script::text("idle-sibling"),
        Script::gated_text("main", Arc::clone(&main_gate)),
        Script::text("active-main-sibling"),
        Script::text("new-checkpoint-temporary"),
        Script::text("next-main"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    app.react(false).unwrap().await.unwrap();
    let temporary = app.react(true).unwrap();
    assert_eq!(temporary.kind(), ReactionKind::Temporary);
    probe.frame_count(2).await;

    let sibling = app.react(true).unwrap();
    assert_eq!(sibling.kind(), ReactionKind::Temporary);
    assert_eq!(sibling.await.unwrap(), ReactionOutcome::Completed);
    assert_eq!(probe.frame("idle-sibling").assistant_text(), ["checkpoint"]);

    let main = app.react(false).unwrap();
    assert_eq!(main.kind(), ReactionKind::Main);
    probe.frame_count(4).await;
    temporary.interrupt().unwrap();
    assert_eq!(temporary.await.unwrap(), ReactionOutcome::Interrupted);
    let mut waiting_main = Box::pin(main.clone().into_future());
    assert!(poll!(waiting_main.as_mut()).is_pending());
    drop(waiting_main);

    // Cancelling the Temporary must not release the active Main's execution slot.
    let sibling = app.react(false).unwrap();
    assert_eq!(sibling.kind(), ReactionKind::Temporary);
    assert_eq!(sibling.await.unwrap(), ReactionOutcome::Completed);
    assert_eq!(
        probe.frame("active-main-sibling").assistant_text(),
        ["checkpoint"]
    );
    main_gate.notify_one();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);

    let temporary = app.react(true).unwrap();
    assert_eq!(temporary.kind(), ReactionKind::Temporary);
    assert_eq!(temporary.await.unwrap(), ReactionOutcome::Completed);
    assert_eq!(
        probe.frame("new-checkpoint-temporary").assistant_text(),
        ["checkpoint", "main"]
    );
    probe.force_main_full.store(true, Ordering::Release);
    app.react(false).unwrap().await.unwrap();
    assert_eq!(
        probe.frame("next-main").assistant_text(),
        ["checkpoint", "main"]
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn later_events_start_siblings_and_interrupt_only_the_selected_reaction() {
    let main_gate = Arc::new(Notify::new());
    let temporary_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::clone(&main_gate)),
        Script::gated_text("interrupted-temporary", temporary_gate),
        Script::text("completed-temporary"),
        Script::text("next-main"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    let application_id = app.application_id();
    app.react(false).unwrap().await.unwrap();

    let main = app.react(false).unwrap();
    let main_id = main.id();
    probe.frame_count(2).await;
    let temporary = app.react(false).unwrap();
    let interrupted_id = temporary.id();
    assert_eq!(temporary.kind(), ReactionKind::Temporary);
    probe.frame_count(3).await;
    let later = app.react(false).unwrap();
    let later_id = later.id();
    assert_eq!(later.kind(), ReactionKind::Temporary);
    assert_ne!(main_id, interrupted_id);
    assert_ne!(interrupted_id, later_id);
    assert_eq!(later.await.unwrap(), ReactionOutcome::Completed);

    temporary.interrupt().unwrap();
    assert_eq!(
        temporary.clone().await.unwrap(),
        ReactionOutcome::Interrupted
    );
    temporary.interrupt().unwrap();
    let mut main_wait = Box::pin(main.clone().into_future());
    assert!(poll!(main_wait.as_mut()).is_pending());
    drop(main_wait);
    main_gate.notify_one();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);

    probe.force_main_full.store(true, Ordering::Release);
    let next = app.react(false).unwrap();
    assert_eq!(next.kind(), ReactionKind::Main);
    assert_eq!(next.await.unwrap(), ReactionOutcome::Completed);
    let resumed = probe.frame("next-main");
    assert_eq!(resumed.basis, FrameBasis::Full);
    assert_eq!(resumed.assistant_text(), ["checkpoint", "main"]);
    let first_temporary = probe.frame("interrupted-temporary");
    let second_temporary = probe.frame("completed-temporary");
    assert_eq!(first_temporary.assistant_text(), ["checkpoint"]);
    assert_eq!(second_temporary.assistant_text(), ["checkpoint"]);
    assert_ne!(first_temporary.target, resumed.target);
    assert_ne!(first_temporary.target, second_temporary.target);
    {
        let events = probe.events.lock().unwrap();
        for (text, context) in events.iter() {
            assert_eq!(context.application_id(), application_id);
            let (id, kind, frame) = match text.as_str() {
                "main" => (main_id, ReactionKind::Main, probe.frame("main")),
                "interrupted-temporary" => (
                    interrupted_id,
                    ReactionKind::Temporary,
                    first_temporary.clone(),
                ),
                "completed-temporary" => {
                    (later_id, ReactionKind::Temporary, second_temporary.clone())
                }
                _ => continue,
            };
            assert_eq!(context.reaction_id(), Some(id));
            assert_eq!(context.reaction_kind(), Some(kind));
            assert_eq!(context.frame_revision(), Some(frame.revision));
        }
        let completions = probe.completions.lock().unwrap();
        assert!(completions
            .iter()
            .any(|context| context.reaction_id() == Some(later_id)));
        assert!(!completions
            .iter()
            .any(|context| context.reaction_id() == Some(interrupted_id)));
    }
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_actions_keep_reaction_local_protocol_and_deliver_actual_feedback() {
    let main_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::adjustment("main-adjust", "same-call", 1, Some(Arc::clone(&main_gate))),
        Script::adjustment("temporary-adjust", "same-call", 10, None),
        Script::adjustment("next-action", "next-call", -1, None),
        Script::text("feedback"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    let application_id = app.application_id();
    app.react(false).unwrap().await.unwrap();

    let main = app.react(false).unwrap();
    let main_id = main.id();
    probe.tool_count(1).await;
    let temporary = app.react(false).unwrap();
    let temporary_id = temporary.id();
    assert_eq!(temporary.kind(), ReactionKind::Temporary);
    assert_eq!(temporary.await.unwrap(), ReactionOutcome::Completed);
    probe.tool_count(2).await;
    main_gate.notify_one();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);

    probe.force_main_full.store(true, Ordering::Release);
    app.react(false).unwrap().await.unwrap();
    app.react(false).unwrap().await.unwrap();
    let next = probe.frame("next-action");
    assert!(next.observes_total(11));
    assert_eq!(next.tools, ["adjust"]);
    assert_eq!(
        next.native_items(),
        [
            CanonicalInputItem::tool_call("same-call", "adjust", r#"{"amount":1}"#).unwrap(),
            CanonicalInputItem::tool_result("same-call", "1").unwrap(),
        ]
    );
    let feedback = probe.frame("feedback");
    assert!(feedback.observes_total(10));
    assert!(feedback
        .native_items()
        .contains(&CanonicalInputItem::tool_result("next-call", "10").unwrap()));
    {
        let tools = probe.tools.lock().unwrap();
        assert_eq!(
            tools.iter().map(|(amount, _)| *amount).collect::<Vec<_>>(),
            [1, 10, -1]
        );
        for (index, id, kind) in [
            (0, main_id, ReactionKind::Main),
            (1, temporary_id, ReactionKind::Temporary),
        ] {
            let context = &tools[index].1;
            assert_eq!(context.application_id(), application_id);
            assert_eq!(context.reaction_id(), Some(id));
            assert_eq!(context.reaction_kind(), Some(kind));
        }
    }
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn temporary_handles_continue_their_own_tool_feedback_after_main_finishes() {
    let main_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::clone(&main_gate)),
        Script::adjustment("temporary-first", "temporary-call", 10, None),
        Script::adjustment("temporary-next", "temporary-next-call", -3, None),
        Script::text("temporary-feedback"),
        Script::text("next-main"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    app.react(false).unwrap().await.unwrap();
    let main = app.react(false).unwrap();
    probe.frame_count(2).await;
    let first = app.react(false).unwrap();
    assert_eq!(first.clone().await.unwrap(), ReactionOutcome::Completed);

    main_gate.notify_one();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
    let next = first.react().unwrap();
    assert_ne!(next.id(), first.id());
    assert_eq!(next.kind(), ReactionKind::Temporary);
    assert_eq!(next.clone().await.unwrap(), ReactionOutcome::Completed);
    let observed = probe.frame("temporary-next");
    assert!(observed.observes_total(10));
    assert_eq!(
        observed.staged,
        [CanonicalInputItem::tool_result("temporary-call", "10").unwrap()]
    );
    assert!(!observed.assistant_text().contains(&"main"));

    let feedback = next.react().unwrap();
    assert_eq!(feedback.kind(), ReactionKind::Temporary);
    assert_eq!(feedback.await.unwrap(), ReactionOutcome::Completed);
    let observed = probe.frame("temporary-feedback");
    assert!(observed.observes_total(7));
    assert_eq!(
        observed.staged,
        [CanonicalInputItem::tool_result("temporary-next-call", "7").unwrap()]
    );

    probe.force_main_full.store(true, Ordering::Release);
    let main = app.react(false).unwrap();
    assert_eq!(main.kind(), ReactionKind::Main);
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
    let observed = probe.frame("next-main");
    assert!(observed.observes_total(7));
    assert!(observed.native_items().is_empty());
    assert_eq!(observed.assistant_text(), ["checkpoint", "main"]);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_interrupts_and_drains_every_owned_reaction() {
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::new(Notify::new())),
        Script::gated_text("temporary", Arc::new(Notify::new())),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    app.react(false).unwrap().await.unwrap();
    let main = app.react(false).unwrap();
    probe.frame_count(2).await;
    let temporary = app.react(false).unwrap();
    probe.frame_count(3).await;
    let main_target = probe.frame("main").target;
    let temporary_target = probe.frame("temporary").target;

    tokio::time::timeout(DEADLINE, app.shutdown())
        .await
        .expect("shutdown cancels pending provider streams")
        .unwrap();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Interrupted);
    assert_eq!(temporary.await.unwrap(), ReactionOutcome::Interrupted);
    let shutdowns = probe.shutdowns.lock().unwrap();
    assert!(shutdowns.contains(&main_target));
    assert!(shutdowns.contains(&temporary_target));
}

#[tokio::test]
async fn retaining_a_handle_does_not_keep_a_dropped_application_running() {
    let probe = Probe::scripted([Script::gated_text("main", Arc::new(Notify::new()))]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    let reaction = app.react(false).unwrap();
    let observer = reaction.clone();
    probe.frame_count(1).await;
    let target = probe.frame("main").target;
    drop(app);
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction)
            .await
            .expect("Application drop owns cancellation independently of retained handles")
            .unwrap(),
        ReactionOutcome::Interrupted
    );
    assert_eq!(
        observer.clone().await.unwrap(),
        ReactionOutcome::Interrupted
    );
    observer.interrupt().unwrap();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let changed = probe.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if probe.shutdowns.lock().unwrap().contains(&target) {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("the retained terminal handle owns no live provider resource");
}

#[tokio::test]
async fn a_main_only_port_rejects_temporary_admission_without_disrupting_main() {
    let main_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::clone(&main_gate)),
        Script::text("next-main"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        MainOnlyPort(ProbePort::new(Arc::clone(&probe))),
    )
    .unwrap();
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ReactionOutcome::Completed
    );
    let main = app.react(false).unwrap();
    probe.frame_count(2).await;
    assert!(app.react(false).is_err());
    assert_eq!(probe.frames.lock().unwrap().len(), 2);
    main_gate.notify_one();
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
    let next = app.react(false).unwrap();
    assert_eq!(next.kind(), ReactionKind::Main);
    assert_eq!(next.await.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct PreparationGate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[component]
fn preparing_application(gate: PreparationGate) -> Component {
    use_preparation(move || async move {
        gate.entered.notify_one();
        gate.release.notified().await;
        Ok::<(), Infallible>(())
    });
    view! { preparing { "Waiting for required input." } }
}

#[tokio::test]
async fn interrupting_idle_ephemeral_before_handoff_keeps_the_main_checkpoint() {
    let gate = PreparationGate::default();
    let root_gate = gate.clone();
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::text("main-after-cancel"),
    ]);
    let mut app = Application::mount(
        move || preparing_application(root_gate.clone()),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    gate.release.notify_one();
    app.react(false).unwrap().await.unwrap();
    tokio::time::timeout(DEADLINE, gate.entered.notified())
        .await
        .unwrap();
    let checkpoint = probe.frame("checkpoint");

    let queued = app.react(true).unwrap();
    assert_eq!(queued.kind(), ReactionKind::Temporary);
    queued.interrupt().unwrap();
    assert_eq!(queued.await.unwrap(), ReactionOutcome::Interrupted);
    assert_eq!(probe.frames.lock().unwrap().len(), 1);

    let preparing = app.react(true).unwrap();
    assert_eq!(preparing.kind(), ReactionKind::Temporary);
    tokio::time::timeout(DEADLINE, gate.entered.notified())
        .await
        .expect("an idle Temporary still owns its preparation");
    preparing.interrupt().unwrap();
    assert_eq!(
        tokio::time::timeout(DEADLINE, preparing)
            .await
            .unwrap()
            .unwrap(),
        ReactionOutcome::Interrupted
    );
    assert_eq!(probe.frames.lock().unwrap().len(), 1);

    gate.release.notify_one();
    let main = app.react(false).unwrap();
    assert_eq!(main.kind(), ReactionKind::Main);
    assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
    let continued = probe.frame("main-after-cancel");
    assert_eq!(continued.target, checkpoint.target);
    assert_eq!(continued.basis, FrameBasis::DeltaFrom(checkpoint.revision));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupt_during_preparation_does_not_submit_a_frame() {
    let gate = PreparationGate::default();
    let root_gate = gate.clone();
    let probe = Probe::scripted([Script::text("must-not-submit")]);
    let mut app = Application::mount(
        move || preparing_application(root_gate.clone()),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    let reaction = app.react(false).unwrap();
    assert_eq!(reaction.kind(), ReactionKind::Main);
    tokio::time::timeout(DEADLINE, gate.entered.notified())
        .await
        .expect("preparation starts before awaiting the handle");
    reaction.interrupt().unwrap();
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction.into_future())
            .await
            .unwrap()
            .unwrap(),
        ReactionOutcome::Interrupted
    );
    assert!(probe.frames.lock().unwrap().is_empty());
    app.shutdown().await.unwrap();
}

async fn panic_callback(payload: Arc<()>) -> Result<(), Infallible> {
    panic_any(payload)
}

#[component]
fn panicking_application(payload: Arc<()>) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, move |_context, _event| {
        panic_callback(Arc::clone(&payload))
    });
    view! { state { "The callback's original panic must reach the owner." } }
}

#[tokio::test]
async fn the_original_panic_is_rethrown_once_and_later_waiters_observe_terminal_failure() {
    let payload = Arc::new(());
    let root_payload = Arc::clone(&payload);
    let probe = Probe::scripted([Script::text("panic")]);
    let mut app = Application::mount(
        move || panicking_application(Arc::clone(&root_payload)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    let reaction = app.react(false).unwrap();
    let other_waiter = reaction.clone();
    let panic = AssertUnwindSafe(reaction.into_future())
        .catch_unwind()
        .await
        .expect_err("a user panic must not become an ordinary cloneable fault");
    let received = panic.downcast::<Arc<()>>().expect("original payload type");
    assert!(Arc::ptr_eq(&payload, &received));
    assert!(other_waiter.await.is_err());
    assert!(app.react(false).is_err());
    assert!(app.shutdown().await.is_err());
    assert!(probe
        .shutdowns
        .lock()
        .unwrap()
        .contains(&probe.frame("panic").target));
}

async fn assert_temporary_admission_panic(declare: bool) {
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::new(Notify::new())),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    app.react(false).unwrap().await.unwrap();
    let main = app.react(false).unwrap();
    probe.frame_count(2).await;
    if declare {
        probe.panic_declare.store(true, Ordering::Release);
    } else {
        probe.panic_fork.store(true, Ordering::Release);
    }

    let panic = std::panic::catch_unwind(AssertUnwindSafe(|| app.react(false)))
        .expect_err("a port capability panic must retain its original payload");
    let received = panic.downcast::<Arc<()>>().expect("original payload type");
    assert!(Arc::ptr_eq(&probe.panic_payload, &received));
    let result = tokio::time::timeout(DEADLINE, main)
        .await
        .expect("a capability panic also stops the active main stream");
    assert!(matches!(result, Err(_) | Ok(ReactionOutcome::Interrupted)));
    assert!(app.react(false).is_err());
    assert!(app.shutdown().await.is_err());
    let shutdowns = probe.shutdowns.lock().unwrap();
    assert!(shutdowns.contains(&probe.frame("main").target));
    for target in probe.forked_targets.lock().unwrap().iter() {
        assert!(
            shutdowns.contains(target),
            "cleanup includes the checkpoint and any half-created temporary target"
        );
    }
}

#[tokio::test]
async fn a_temporary_fork_panic_keeps_its_payload_and_fails_the_application_closed() {
    assert_temporary_admission_panic(false).await;
}

#[tokio::test]
async fn a_temporary_declaration_panic_also_shuts_down_the_half_created_target() {
    assert_temporary_admission_panic(true).await;
}

#[test]
fn runtime_teardown_does_not_leave_an_active_handle_or_shutdown_hanging() {
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::new(Notify::new())),
    ]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (app, reaction) = runtime.block_on(async {
        let root_probe = Arc::clone(&probe);
        let mut app = Application::mount(
            move || observed_application(Arc::clone(&root_probe)),
            ProbePort::new(Arc::clone(&probe)),
        )
        .unwrap();
        app.react(false).unwrap().await.unwrap();
        let reaction = app.react(false).unwrap();
        probe.frame_count(2).await;
        (app, reaction)
    });
    drop(runtime);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            assert!(tokio::time::timeout(DEADLINE, reaction)
                .await
                .expect("a dropped driver publishes a terminal handle result")
                .is_err());
            assert!(tokio::time::timeout(DEADLINE, app.shutdown())
                .await
                .expect("shutdown recovers ownership after its original runtime disappears")
                .is_err());
        });
}

#[tokio::test]
async fn settled_temporary_continuations_are_rejected_while_fork_owns_maintenance() {
    let main_gate = Arc::new(Notify::new());
    let probe = Probe::scripted([
        Script::text("checkpoint"),
        Script::gated_text("main", Arc::clone(&main_gate)),
        Script::text("temporary"),
        Script::text("continued"),
    ]);
    let root_probe = Arc::clone(&probe);
    let mut app = Application::mount(
        move || observed_application(Arc::clone(&root_probe)),
        ProbePort::new(Arc::clone(&probe)),
    )
    .unwrap();
    app.react(false).unwrap().await.unwrap();
    let main = app.react(false).unwrap();
    probe.frame_count(2).await;
    let temporary = app.react(false).unwrap();
    assert_eq!(temporary.clone().await.unwrap(), ReactionOutcome::Completed);
    main_gate.notify_one();
    main.await.unwrap();

    let root_probe = Arc::clone(&probe);
    let mut fork = Box::pin(app.fork(move || observed_application(Arc::clone(&root_probe))));
    tokio::select! {
        _ = probe.maintenance_entered.notified() => {}
        result = &mut fork => panic!("the fork should remain gated: {}", result.is_ok()),
    }
    assert!(temporary.react().is_err());
    assert_eq!(probe.frames.lock().unwrap().len(), 3);
    probe.maintenance_release.notify_one();
    let child = fork.await.unwrap();
    assert_eq!(
        temporary.react().unwrap().await.unwrap(),
        ReactionOutcome::Completed
    );
    child.shutdown().await.unwrap();
    app.shutdown().await.unwrap();
}
