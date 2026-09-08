use std::{
    future::Future,
    num::{NonZeroU64, NonZeroU128},
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use crate::component::{
    authoring::{InternalEventInput as EventInput, InternalEventListener as EventListener},
    prelude::*,
};
use futures::{FutureExt, StreamExt, task::noop_waker};
use tokio::sync::Notify;

use super::{
    ExternalAct, ExternalApplication, ExternalApplicationFault, ExternalControlFault,
    ExternalObservationKind, ExternalProviderPort, ExternalSubmitBarrier,
    MAX_EXTERNAL_PROTOCOL_FRAMES, MAX_EXTERNAL_PROTOCOL_WIRE_BYTES, MAX_EXTERNAL_TEXT_BYTES,
};
use crate::component::execution::{
    ProviderEvent, ProviderFault,
    application::{Application, ApplicationFault},
    reaction::{
        Frame, FrameBasis, FrameRevision, FrameSubmission, ProjectionSubmission, ProviderFact,
        ProviderFactStream, ReactionPort, ReactionPortFault, ReactionPortFaultCode,
        ReactionPortFaultKind, ReactionPortFaultReason, SubmitFault, TargetContinuity,
        TargetDeclaration, TargetEpoch, ToolCatalog,
    },
};

#[derive(Clone)]
struct ExternalProps {
    values: Arc<Mutex<Vec<String>>>,
    log: Arc<Mutex<Vec<String>>>,
    handler_started: Arc<Notify>,
    handler_release: Arc<Notify>,
    pending_drops: Arc<AtomicUsize>,
}

impl ExternalProps {
    fn new() -> Self {
        Self {
            values: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(Mutex::new(Vec::new())),
            handler_started: Arc::new(Notify::new()),
            handler_release: Arc::new(Notify::new()),
            pending_drops: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Clone)]
struct LateTaskPanicProps {
    external: ExternalProps,
    starts: Arc<AtomicUsize>,
    release: Arc<Notify>,
    sibling_dropped: Arc<AtomicBool>,
}

struct LateTaskPanicSiblingDrop(Arc<AtomicBool>);

impl Drop for LateTaskPanicSiblingDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[component]
fn late_task_panic_root(props: LateTaskPanicProps, events: EventInput<ProviderEvent>) -> Component {
    let primary_starts = Arc::clone(&props.starts);
    let release = Arc::clone(&props.release);
    use_future(move || async move {
        primary_starts.fetch_add(1, Ordering::AcqRel);
        release.notified().await;
        panic!("external late Component task panic");
    });

    let sibling_starts = Arc::clone(&props.starts);
    let sibling_dropped = Arc::clone(&props.sibling_dropped);
    use_future(move || async move {
        let _drop = LateTaskPanicSiblingDrop(sibling_dropped);
        sibling_starts.fetch_add(1, Ordering::AcqRel);
        std::future::pending::<()>().await;
    });

    external_root(props.external, events)
}

struct PendingHandlerDrop(Arc<AtomicUsize>);

impl Drop for PendingHandlerDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct ShutdownTaskDrop {
    dropped: Arc<AtomicBool>,
    gate: Arc<ShutdownDropGate>,
}

impl Drop for ShutdownTaskDrop {
    fn drop(&mut self) {
        self.gate.entered.store(true, Ordering::Release);
        let mut released = self.gate.released.lock().unwrap();
        while !*released {
            released = self.gate.changed.wait(released).unwrap();
        }
        self.dropped.store(true, Ordering::Release);
    }
}

struct ShutdownDropGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl ShutdownDropGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

#[derive(Clone)]
struct ShutdownProps {
    exported: Arc<Mutex<Option<Signal<bool>>>>,
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    gate: Arc<ShutdownDropGate>,
}

#[component]
fn shutdown_root(props: ShutdownProps, _events: EventInput<ProviderEvent>) -> Component {
    let visible = use_signal(|| true);
    *props.exported.lock().unwrap() = Some(visible);
    let started = Arc::clone(&props.started);
    let dropped = Arc::clone(&props.dropped);
    let gate = Arc::clone(&props.gate);
    use_future(move || async move {
        let _drop = ShutdownTaskDrop { dropped, gate };
        started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
    });
    view! { shutdown_root { "mounted" } }
}

#[component]
fn external_root(props: ExternalProps, events: EventInput<ProviderEvent>) -> Component {
    let rendered_values = format!("{:?}", *props.values.lock().unwrap());
    let values = Arc::clone(&props.values);
    let log = Arc::clone(&props.log);
    let handler_started = Arc::clone(&props.handler_started);
    let handler_release = Arc::clone(&props.handler_release);
    let pending_drops = Arc::clone(&props.pending_drops);

    view! {
        #[system_once]
        protocol { "External protocol." }
        state { "{rendered_values}" }
        {
            EventListener::observe("test.external.text", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let values = Arc::clone(&values);
                    let log = Arc::clone(&log);
                    let handler_started = Arc::clone(&handler_started);
                    let handler_release = Arc::clone(&handler_release);
                    let pending_drops = Arc::clone(&pending_drops);
                    async move {
                        let (kind, value) = match event {
                            TextTurnEvent::TextDelta(text) => ("delta", text),
                            TextTurnEvent::TextComplete(text) => ("complete", text),
                        };
                        log.lock().unwrap().push(format!("{kind}:{value}:start"));
                        let _pending = PendingHandlerDrop(pending_drops);
                        if value == "blocked" {
                            handler_started.notify_one();
                            handler_release.notified().await;
                        }
                        values.lock().unwrap().push(value.clone());
                        log.lock().unwrap().push(format!("{kind}:{value}:end"));
                        Ok::<(), std::convert::Infallible>(())
                    }
                })
        }
    }
}

fn wrapper(props: &ExternalProps) -> ExternalApplication {
    let props = props.clone();
    ExternalApplication::new_with_event_input(move |events| external_root(props.clone(), events))
        .unwrap()
}

type ShutdownWaiter =
    Pin<Box<dyn Future<Output = Result<(), ExternalApplicationFault>> + Send + 'static>>;

#[test]
fn shutdown_waiter_fails_closed_when_moved_before_cleanup_is_polled() {
    let runtime_a = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime_b = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let waiter: ShutdownWaiter = {
        let _runtime = runtime_a.enter();
        let props = ExternalProps::new();
        Box::pin(wrapper(&props).shutdown())
    };

    let outcome = runtime_b.block_on(async {
        tokio::time::timeout(std::time::Duration::from_millis(100), waiter).await
    });
    assert!(matches!(
        outcome,
        Ok(Err(ExternalApplicationFault::ShutdownTaskFailed))
    ));

    runtime_a.block_on(async {
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    });
}

#[test]
fn shutdown_waiter_fails_closed_when_moved_after_cleanup_is_pending() {
    let runtime_a = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime_b = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cleanup_pending = Arc::new(Notify::new());
    let reaction_release = Arc::new(Notify::new());
    let pending_probe = Arc::clone(&cleanup_pending);
    let reaction_gate = Arc::clone(&reaction_release);
    let mut waiter: ShutdownWaiter = {
        let _runtime = runtime_a.enter();
        let props = ExternalProps::new();
        let mut external = wrapper(&props);
        let owner = external.owner.take().unwrap();
        let (cancellation, cancelled) = tokio::sync::oneshot::channel();
        external.reaction = Some(super::ExternalReaction {
            state: super::ExternalReactionState::AwaitingObservation,
            cancellation: Some(cancellation),
            task: tokio::spawn(async move {
                let _ = cancelled.await;
                pending_probe.notify_one();
                reaction_gate.notified().await;
                super::ExternalReactionCompletion {
                    owner,
                    outcome: super::ExternalReactionOutcome::Cancelled,
                }
            }),
        });
        Box::pin(external.shutdown())
    };

    runtime_a.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            cleanup_pending.notified(),
        )
        .await
        .expect("cleanup must wait for the held reaction");
        std::future::poll_fn(|context| {
            assert!(matches!(waiter.as_mut().poll(context), Poll::Pending));
            Poll::Ready(())
        })
        .await;
    });

    let outcome = runtime_b.block_on(async {
        tokio::time::timeout(std::time::Duration::from_millis(100), waiter).await
    });
    assert!(matches!(
        outcome,
        Ok(Err(ExternalApplicationFault::ShutdownTaskFailed))
    ));

    reaction_release.notify_one();
    runtime_a.block_on(async {
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_shutdown_waiter_still_fences_and_drains_an_active_application() {
    let exported = Arc::new(Mutex::new(None));
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(ShutdownDropGate::new());
    let props = ShutdownProps {
        exported: Arc::clone(&exported),
        started: Arc::clone(&started),
        dropped: Arc::clone(&dropped),
        gate: Arc::clone(&gate),
    };
    let mut external = ExternalApplication::new_with_event_input(move |events| {
        shutdown_root(props.clone(), events)
    })
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bootstrap task must start");
    let observation = external.observe().await.unwrap();
    let ingress = observation.ingress_generation();
    let control = external.control();
    let visible = exported.lock().unwrap().clone().unwrap();

    let shutdown = external.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !gate.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup must reach the mount task destructor");
    assert!(matches!(visible.set(false), Err(SignalAccessError::Stale)));
    assert_eq!(
        control.complete(ingress).await,
        Err(ExternalControlFault::StaleIngress),
        "Application shutdown must begin only after the active reaction has returned its owner"
    );
    assert!(!dropped.load(Ordering::Acquire));
    drop(shutdown);
    gate.release();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup must outlive its cancelled waiter and drain the mount task");
}

fn text_protocol(events: Vec<TextTurnEvent>) -> ExternalAct {
    ExternalAct::from_text_protocol(futures::stream::iter(events.into_iter().map(Ok)))
}

fn completed_text(text: impl Into<String>) -> ExternalAct {
    text_protocol(vec![TextTurnEvent::TextComplete(text.into())])
}

fn json_lines_protocol(lines: &[&str]) -> ExternalAct {
    ExternalAct::__from_cli_json_lines(lines.join("\n"))
}

fn assert_panic_payload(payload: Box<dyn std::any::Any + Send>, expected: &str) {
    if let Some(message) = payload.downcast_ref::<&str>() {
        assert_eq!(*message, expected);
        return;
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        assert_eq!(message, expected);
        return;
    }
    panic!("unexpected non-string panic payload");
}

fn test_frame(declaration: &TargetDeclaration, sequence: u64, payload: &str) -> Frame {
    let epoch = declaration.continuity().epoch();
    let revision = FrameRevision::new(
        NonZeroU128::new(909).unwrap(),
        declaration.identity(),
        epoch,
        NonZeroU64::new(sequence).unwrap(),
    );
    let basis = declaration
        .continuity()
        .accepted_revision()
        .map(FrameBasis::DeltaFrom)
        .unwrap_or(FrameBasis::Full);
    Frame::from_compiled(
        revision,
        declaration.identity(),
        epoch,
        declaration.continuity().clone(),
        declaration.profile().clone(),
        basis,
        FrameSubmission::from_compiled(
            Vec::new(),
            Vec::new(),
            ProjectionSubmission::new(Vec::new()),
            ToolCatalog::new(Vec::new()).unwrap(),
            payload.as_bytes().to_vec(),
        ),
    )
    .unwrap()
}

struct FullResetOnDropPort {
    declaration: TargetDeclaration,
}

struct FullResetOnDropStream<'a> {
    declaration: &'a mut TargetDeclaration,
}

impl futures::Stream for FullResetOnDropStream<'_> {
    type Item = Result<ProviderFact, ReactionPortFault>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for FullResetOnDropStream<'_> {
    fn drop(&mut self) {
        let next_epoch = self
            .declaration
            .continuity()
            .epoch()
            .get()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .expect("test epoch space");
        *self.declaration = TargetDeclaration::full(
            self.declaration.identity(),
            TargetEpoch::new(next_epoch),
            self.declaration.profile().clone(),
        );
    }
}

#[async_trait::async_trait]
impl ReactionPort for FullResetOnDropPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        Ok(Box::pin(FullResetOnDropStream {
            declaration: &mut self.declaration,
        }))
    }
}

#[test]
fn third_party_unfinished_stream_drop_makes_the_next_declaration_truthful() {
    let (mut seed, _control) = ExternalProviderPort::new().unwrap();
    let initial = seed.declare().unwrap();
    let initial_epoch = initial.continuity().epoch();
    let mut port = FullResetOnDropPort {
        declaration: initial.clone(),
    };
    let stream = futures::executor::block_on(port.submit(test_frame(&initial, 1, "first")))
        .expect("first Frame handoff");

    drop(stream);

    let next = port.declare().unwrap();
    assert!(matches!(
        next.continuity(),
        TargetContinuity::FullRequired { epoch } if *epoch > initial_epoch
    ));
}

#[test]
fn crossing_poll_synchronously_enqueues_the_exact_frame() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, r#"{"exact":"full"}"#);
    let expected_revision = frame.revision();
    let mut submission = Box::pin(port.submit(frame));
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);

    let stream = match submission.as_mut().poll(&mut context) {
        Poll::Ready(Ok(stream)) => stream,
        Poll::Ready(Err(fault)) => panic!("crossing poll rejected exact Frame: {fault:?}"),
        Poll::Pending => panic!("available external queue did not cross in one poll"),
    };
    drop(submission);

    let observation = futures::executor::block_on(control.next_observation()).unwrap();
    assert_eq!(observation.frame().revision(), expected_revision);
    assert_eq!(observation.frame().basis(), FrameBasis::Full);
    assert_eq!(
        observation.frame().submission().canonical_bytes(),
        br#"{"exact":"full"}"#
    );
    drop(stream);
}

#[test]
fn pending_reserve_cancellation_is_zero_handoff_and_zero_acceptance() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let first_declaration = port.declare().unwrap();
    let first = test_frame(&first_declaration, 1, "first");
    let first_stream = futures::executor::block_on(port.submit(first)).unwrap();
    drop(first_stream);

    let pending_declaration = port.declare().unwrap();
    assert!(matches!(
        pending_declaration.continuity(),
        TargetContinuity::FullRequired { epoch }
            if *epoch > first_declaration.continuity().epoch()
    ));
    let pending = test_frame(&pending_declaration, 2, "must-not-send");
    let mut submission = Box::pin(port.submit(pending));
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        submission.as_mut().poll(&mut context),
        Poll::Pending
    ));
    drop(submission);

    assert_eq!(port.declare().unwrap(), pending_declaration);
    let stale_first = futures::executor::block_on(control.next_observation()).unwrap();
    assert_eq!(stale_first.content(), "first");

    let retry = test_frame(&pending_declaration, 2, "retry");
    let retry_stream = futures::executor::block_on(port.submit(retry)).unwrap();
    let accepted = futures::executor::block_on(control.next_observation()).unwrap();
    assert_eq!(accepted.content(), "retry");
    drop(retry_stream);
}

#[tokio::test]
async fn receiver_failure_after_queue_acceptance_is_a_stream_fault() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "accepted");
    let mut facts = port.submit(frame).await.unwrap();

    drop(control);
    let fault = facts.next().await.unwrap().unwrap_err();
    assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
    assert_eq!(fault.code(), ReactionPortFaultCode::Unavailable);
    assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
    assert!(facts.next().await.is_none());
    drop(facts);
    assert_eq!(port.declare().unwrap_err(), fault);
}

#[test]
fn dropping_control_before_mount_fails_declaration_without_rendering() {
    let (port, control) = ExternalProviderPort::new().unwrap();
    drop(control);
    let renders = Arc::new(AtomicUsize::new(0));
    let rendered = Arc::clone(&renders);

    let mounted = Application::mount(
        move || {
            rendered.fetch_add(1, Ordering::SeqCst);
            agentview_derive::view! {}
        },
        port,
    );

    assert!(mounted.is_err());
    assert_eq!(renders.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dropping_control_after_completed_reaction_is_sticky_terminal() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "accepted");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();
    control
        .act(observation.ingress_generation(), completed_text("done"))
        .await
        .unwrap();
    while facts.next().await.is_some() {}
    drop(facts);

    drop(control);

    let first = port.declare().unwrap_err();
    let second = port.declare().unwrap_err();
    assert_eq!(first, second);
    assert_eq!(first.kind(), ReactionPortFaultKind::Terminal);
    assert_eq!(first.code(), ReactionPortFaultCode::Unavailable);
    assert_eq!(first.reason(), ReactionPortFaultReason::Declaration);
}

#[tokio::test]
async fn cancelled_claimed_act_is_reported_as_a_stream_fault() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "accepted");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();
    let protocol_started = Arc::new(Notify::new());
    let started = Arc::clone(&protocol_started);
    let act = ExternalAct::from_text_protocol(futures::stream::once(async move {
        started.notify_one();
        futures::future::pending::<Result<TextTurnEvent, ProviderFault>>().await
    }));
    let act_control = control.clone();
    let act_task =
        tokio::spawn(async move { act_control.act(observation.ingress_generation(), act).await });
    protocol_started.notified().await;

    act_task.abort();
    assert!(act_task.await.unwrap_err().is_cancelled());

    let fault = facts.next().await.unwrap().unwrap_err();
    assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
    assert_eq!(fault.reason(), ReactionPortFaultReason::StreamTransport);
    assert!(facts.next().await.is_none());
    drop(facts);

    let recovered = port.declare().unwrap();
    assert!(recovered.continuity().accepted_revision().is_none());
    assert!(
        recovered.continuity().epoch().get() > declaration.continuity().epoch().get(),
        "an interrupted ingress must advance target continuity"
    );
}

#[tokio::test]
async fn cancelling_the_last_claimed_control_reports_the_sticky_terminal_fault() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "accepted");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();
    let protocol_started = Arc::new(Notify::new());
    let started = Arc::clone(&protocol_started);
    let act = ExternalAct::from_text_protocol(futures::stream::once(async move {
        started.notify_one();
        futures::future::pending::<Result<TextTurnEvent, ProviderFault>>().await
    }));
    let act_task =
        tokio::spawn(async move { control.act(observation.ingress_generation(), act).await });
    protocol_started.notified().await;

    act_task.abort();
    assert!(act_task.await.unwrap_err().is_cancelled());

    let fault = facts.next().await.unwrap().unwrap_err();
    assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
    assert_eq!(fault.code(), ReactionPortFaultCode::Unavailable);
    assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
    assert!(facts.next().await.is_none());
    drop(facts);
    assert_eq!(port.declare().unwrap_err(), fault);
}

#[tokio::test]
async fn last_control_drop_after_reserve_rejects_before_handoff() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "must-not-handoff");
    let barrier = Arc::new(ExternalSubmitBarrier::default());
    port.submit_barrier = Some(Arc::clone(&barrier));
    let submit = tokio::spawn(async move {
        let result = match port.submit(frame).await {
            Ok(stream) => {
                drop(stream);
                Ok(())
            }
            Err(fault) => Err(fault),
        };
        (port, result)
    });
    barrier.reached.notified().await;

    drop(control);
    barrier.release.notify_one();

    let (mut port, result) = tokio::time::timeout(std::time::Duration::from_secs(1), submit)
        .await
        .expect("submit must not hang after the last control closes")
        .unwrap();
    assert!(matches!(
        result,
        Err(SubmitFault::Rejected(fault))
            if fault.kind() == ReactionPortFaultKind::Terminal
                && fault.code() == ReactionPortFaultCode::Unavailable
                && fault.reason() == ReactionPortFaultReason::Declaration
    ));
    let terminal = port.declare().unwrap_err();
    assert_eq!(terminal.kind(), ReactionPortFaultKind::Terminal);
    assert_eq!(terminal.code(), ReactionPortFaultCode::Unavailable);
    assert_eq!(terminal.reason(), ReactionPortFaultReason::Declaration);
}

#[tokio::test]
async fn outstanding_submit_permit_keeps_observation_channel_open_until_handoff() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "held-permit");
    let barrier = Arc::new(ExternalSubmitBarrier::default());
    port.submit_barrier = Some(Arc::clone(&barrier));

    assert_eq!(control.inner.frames.lock().await.sender_strong_count(), 1);

    let submit = tokio::spawn(async move {
        let result = match port.submit(frame).await {
            Ok(stream) => {
                drop(stream);
                Ok(())
            }
            Err(fault) => Err(fault),
        };
        (port, result)
    });
    barrier.reached.notified().await;

    assert_eq!(
        control.inner.frames.lock().await.sender_strong_count(),
        2,
        "the outstanding OwnedPermit must retain its submit-time sender clone"
    );
    let mut observation = Box::pin(control.next_observation());
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        observation.as_mut().poll(&mut context),
        Poll::Pending
    ));

    barrier.release.notify_one();
    let observation = tokio::time::timeout(std::time::Duration::from_secs(1), observation)
        .await
        .expect("handoff must wake the pending observation")
        .unwrap();
    assert_eq!(observation.content(), "held-permit");

    let (port, result) = submit.await.unwrap();
    result.unwrap();
    assert_eq!(
        control.inner.frames.lock().await.sender_strong_count(),
        1,
        "permit.send must synchronously release its temporary sender clone"
    );
    drop(port);
    assert!(matches!(
        control.next_observation().await,
        Err(ExternalControlFault::ObservationChannelClosed)
    ));
}

#[tokio::test]
async fn observe_captures_full_then_act_captures_exact_delta_frame() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);

    let first = external.observe().await.unwrap();
    assert_eq!(first.kind(), ExternalObservationKind::Full);
    assert_eq!(first.base_generation(), None);
    assert_eq!(first.frame().basis(), FrameBasis::Full);
    assert!(first.content().contains("External protocol."));
    assert_eq!(
        first.content().as_bytes(),
        first.frame().submission().canonical_bytes()
    );

    let second = external.act(completed_text("one")).await.unwrap();
    assert_eq!(second.kind(), ExternalObservationKind::Delta);
    assert_eq!(second.base_generation(), Some(first.generation()));
    assert_eq!(
        second.frame().basis(),
        FrameBasis::DeltaFrom(first.frame().revision())
    );
    assert!(second.content().contains("one"));
    assert_eq!(
        *props.log.lock().unwrap(),
        ["complete:one:start", "complete:one:end"]
    );
}

#[tokio::test]
async fn one_act_publishes_ordered_facts_and_stops_after_explicit_completion() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let observed_polls = Arc::clone(&polls);
    let mut events = std::collections::VecDeque::from([
        Ok(TextTurnEvent::TextDelta(String::from("a"))),
        Ok(TextTurnEvent::TextDelta(String::from("b"))),
        Ok(TextTurnEvent::TextComplete(String::from("ab"))),
        Ok(TextTurnEvent::TextDelta(String::from("must-not-poll"))),
    ]);
    let protocol = futures::stream::poll_fn(move |_| {
        observed_polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(events.pop_front())
    });

    external
        .act(ExternalAct::from_text_protocol(protocol))
        .await
        .unwrap();

    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:a:start",
            "delta:a:end",
            "delta:b:start",
            "delta:b:end",
            "complete:ab:start",
            "complete:ab:end",
        ]
    );
}

#[tokio::test]
async fn normal_protocol_eof_seals_accumulated_text_before_completion() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    external
        .act(text_protocol(vec![
            TextTurnEvent::TextDelta(String::from("left")),
            TextTurnEvent::TextDelta(String::from("right")),
        ]))
        .await
        .unwrap();

    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:left:start",
            "delta:left:end",
            "delta:right:start",
            "delta:right:end",
            "complete:leftright:start",
            "complete:leftright:end",
        ]
    );
}

#[tokio::test]
async fn late_act_is_explicitly_rejected_by_ingress_generation() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let observation = external.observe().await.unwrap();
    let generation = observation.ingress_generation();
    let control = external.control();

    control
        .act(generation, completed_text("one"))
        .await
        .unwrap();
    let next = external.observe().await.unwrap();
    assert_ne!(next.ingress_generation(), generation);
    assert!(next.content().contains("one"));

    assert_eq!(
        control.act(generation, completed_text("late")).await,
        Err(ExternalControlFault::StaleIngress)
    );
}

#[tokio::test]
async fn observe_full_uses_a_new_epoch_and_a_new_full_frame() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let first = external.observe().await.unwrap();

    let full = external.observe_full().await.unwrap();

    assert_eq!(full.kind(), ExternalObservationKind::Full);
    assert_eq!(full.base_generation(), None);
    assert_ne!(full.frame().epoch(), first.frame().epoch());
    assert_ne!(full.generation(), first.generation());
    assert_ne!(full.ingress_generation(), first.ingress_generation());
}

#[tokio::test]
async fn abnormal_protocol_resets_continuity_for_the_next_explicit_reaction() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let fault = external
        .act(ExternalAct::from_text_protocol(futures::stream::iter([
            Ok(TextTurnEvent::TextDelta(String::from("partial"))),
            Err(ProviderFault::retryable_transport("disconnect")),
        ])))
        .await
        .unwrap_err();
    assert!(matches!(fault, ExternalApplicationFault::Application));

    let recovered = external.observe().await.unwrap();
    assert_eq!(recovered.kind(), ExternalObservationKind::Full);
    assert!(recovered.content().contains("partial"));
    assert_eq!(
        *props.log.lock().unwrap(),
        ["delta:partial:start", "delta:partial:end"]
    );
}

#[tokio::test]
async fn cancelling_post_handoff_act_allows_the_next_observe_to_recover() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let cancelled = external.observe().await.unwrap();
    let cancelled_generation = cancelled.ingress_generation();
    let control = external.control();

    let mut act = Box::pin(external.act(completed_text("blocked")));
    tokio::select! {
        () = props.handler_started.notified() => {}
        result = act.as_mut() => panic!("act completed before handler blocked: {result:?}"),
    }
    drop(act);

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while props.pending_drops.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let recovered = external.observe().await.unwrap();
    assert_eq!(recovered.kind(), ExternalObservationKind::Full);
    assert!(recovered.content().contains("blocked"));
    assert!(matches!(
        control.complete(cancelled_generation).await,
        Err(ExternalControlFault::StaleIngress)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_component_task_panic_after_cancellation_crosses_external_boundary() {
    let external_props = ExternalProps::new();
    let starts = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let sibling_dropped = Arc::new(AtomicBool::new(false));
    let root_props = LateTaskPanicProps {
        external: external_props.clone(),
        starts: Arc::clone(&starts),
        release: Arc::clone(&release),
        sibling_dropped: Arc::clone(&sibling_dropped),
    };
    let mut external = ExternalApplication::new_with_event_input(move |events| {
        late_task_panic_root(root_props.clone(), events)
    })
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while starts.load(Ordering::Acquire) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both Component tasks must start");
    external.observe().await.unwrap();

    let mut act = Box::pin(external.act(completed_text("blocked")));
    tokio::select! {
        () = external_props.handler_started.notified() => {}
        result = act.as_mut() => panic!("act completed before handler blocked: {result:?}"),
    }
    drop(act);

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while external_props.pending_drops.load(Ordering::Acquire) == 0
            || !external
                .reaction
                .as_ref()
                .is_some_and(|reaction| reaction.task.is_finished())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-handoff cancellation must return the Application owner");
    external.recover_finished_reaction().await.unwrap();

    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !sibling_dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervisor must latch the panic and abort its sibling");

    let panic = AssertUnwindSafe(external.observe())
        .catch_unwind()
        .await
        .expect_err("ExternalApplication must resume the original task panic");
    assert_panic_payload(panic, "external late Component task panic");
    assert!(matches!(
        external.observe().await,
        Err(ExternalApplicationFault::OwnerUnavailable)
    ));
}

#[tokio::test]
async fn blocked_act_injection_is_interrupted_by_the_reaction_panic() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let owner = external.owner.take().unwrap();
    let (fact_sender, _facts) = tokio::sync::mpsc::channel(1);
    let generation = {
        let mut shared = external
            .control
            .inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = shared.allocate_ingress().unwrap();
        shared.ingress = Some(super::ExternalIngressState {
            generation,
            sender: Some(fact_sender),
        });
        generation
    };
    let protocol_started = Arc::new(Notify::new());
    let reaction_trigger = Arc::clone(&protocol_started);
    let act_trigger = Arc::clone(&protocol_started);
    let act = ExternalAct::from_text_protocol(futures::stream::once(async move {
        act_trigger.notify_one();
        futures::future::pending::<Result<TextTurnEvent, ProviderFault>>().await
    }));
    let (cancellation, _cancelled) = tokio::sync::oneshot::channel();
    external.reaction = Some(super::ExternalReaction {
        state: super::ExternalReactionState::AwaitingInput(generation),
        cancellation: Some(cancellation),
        task: tokio::spawn(async move {
            let _owner = owner;
            reaction_trigger.notified().await;
            panic!("blocked act reaction panic")
        }),
    });

    let panic = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        AssertUnwindSafe(external.act(act)).catch_unwind(),
    )
    .await
    .expect("reaction panic must interrupt a blocked act injection")
    .expect_err("reaction panic must unwind through ExternalApplication::act");
    assert_panic_payload(panic, "blocked act reaction panic");
}

#[tokio::test]
async fn ready_reaction_panic_wins_ready_cancellation() {
    async fn panic_reaction() -> Result<(), ApplicationFault> {
        panic!("ready reaction panic")
    }

    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    cancel.send(()).unwrap();

    let result = AssertUnwindSafe(super::await_reaction_or_cancellation(
        panic_reaction(),
        cancelled,
    ))
    .catch_unwind()
    .await;
    let panic = match result {
        Ok(_) => panic!("a ready cancellation suppressed a ready reaction panic"),
        Err(panic) => panic,
    };
    assert_panic_payload(panic, "ready reaction panic");
}

#[tokio::test]
async fn act_without_a_current_reaction_fails_closed() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);

    assert!(matches!(
        external.act(completed_text("orphaned")).await,
        Err(ExternalApplicationFault::NoActiveReaction)
    ));
}

#[tokio::test]
async fn closed_observation_waits_for_reaction_completion_before_classifying() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let owner = external.owner.take().unwrap();
    let owner_dropped = Arc::new(Notify::new());
    let release_panic = Arc::new(Notify::new());
    let task_owner_dropped = Arc::clone(&owner_dropped);
    let task_release_panic = Arc::clone(&release_panic);
    let (cancellation, _cancelled) = tokio::sync::oneshot::channel();
    external.reaction = Some(super::ExternalReaction {
        state: super::ExternalReactionState::AwaitingObservation,
        cancellation: Some(cancellation),
        task: tokio::spawn(async move {
            drop(owner);
            task_owner_dropped.notify_one();
            task_release_panic.notified().await;
            panic!("delayed external reaction panic")
        }),
    });

    owner_dropped.notified().await;
    let mut observation = Box::pin(external.await_next_observation());
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        observation.as_mut().poll(&mut context),
        Poll::Pending
    ));

    release_panic.notify_one();
    let panic = AssertUnwindSafe(observation)
        .catch_unwind()
        .await
        .expect_err("channel closure must wait for and propagate the reaction panic");
    assert_panic_payload(panic, "delayed external reaction panic");
    assert!(matches!(
        external.observe().await,
        Err(ExternalApplicationFault::OwnerUnavailable)
    ));
}

#[tokio::test]
async fn aborted_reaction_after_observation_closure_is_a_bounded_task_failure() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let owner = external.owner.take().unwrap();
    let task_started = Arc::new(Notify::new());
    let started = Arc::clone(&task_started);
    let (cancellation, _cancelled) = tokio::sync::oneshot::channel();
    external.reaction = Some(super::ExternalReaction {
        state: super::ExternalReactionState::AwaitingObservation,
        cancellation: Some(cancellation),
        task: tokio::spawn(async move {
            let _owner = owner;
            started.notify_one();
            std::future::pending::<super::ExternalReactionCompletion>().await
        }),
    });
    task_started.notified().await;
    external.reaction.as_ref().unwrap().task.abort();

    let fault = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        external.await_next_observation(),
    )
    .await
    .expect("an aborted reaction must not leave observation arbitration pending")
    .unwrap_err();
    assert!(matches!(
        fault,
        ExternalApplicationFault::ReactionTaskFailed
    ));
    assert!(matches!(
        external.observe().await,
        Err(ExternalApplicationFault::OwnerUnavailable)
    ));
}

#[tokio::test]
async fn ready_reaction_task_panic_wins_observation_and_resumes_original_payload() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let owner = external.owner.take().unwrap();
    let (cancellation, _cancelled) = tokio::sync::oneshot::channel();
    external.reaction = Some(super::ExternalReaction {
        state: super::ExternalReactionState::AwaitingObservation,
        cancellation: Some(cancellation),
        task: tokio::spawn(async move {
            let _owner = owner;
            panic!("external reaction task panic")
        }),
    });

    while !external.reaction.as_ref().unwrap().task.is_finished() {
        tokio::task::yield_now().await;
    }

    let panic = AssertUnwindSafe(external.await_next_observation())
        .catch_unwind()
        .await
        .expect_err("the ready reaction task panic must win the closed observation channel");
    assert_panic_payload(panic, "external reaction task panic");
    assert!(matches!(
        external.observe().await,
        Err(ExternalApplicationFault::OwnerUnavailable)
    ));
}

#[tokio::test]
async fn cli_json_lines_preserve_order_and_completion_cutoff() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    external
        .act(json_lines_protocol(&[
            r#"{"type":"text_delta","text":"left"}"#,
            r#"{"type":"text_delta","text":"right"}"#,
            r#"{"type":"text_complete","text":"leftright"}"#,
            r#"{"type":"disconnect"}"#,
        ]))
        .await
        .unwrap();

    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:left:start",
            "delta:left:end",
            "delta:right:start",
            "delta:right:end",
            "complete:leftright:start",
            "complete:leftright:end",
        ]
    );
}

#[tokio::test]
async fn cli_protocol_limits_are_checked_before_ingress() {
    let mut oversized =
        ExternalAct::__from_cli_json_lines("x".repeat(MAX_EXTERNAL_PROTOCOL_WIRE_BYTES + 1));
    assert!(oversized.protocol.next().await.unwrap().is_err());

    let protocol = "\n".repeat(MAX_EXTERNAL_PROTOCOL_FRAMES + 1);
    let mut too_many = ExternalAct::__from_cli_json_lines(protocol);
    assert!(too_many.protocol.next().await.unwrap().is_err());
}

#[tokio::test]
async fn mismatched_explicit_seal_fails_admission_and_forces_next_full() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let fault = external
        .act(text_protocol(vec![
            TextTurnEvent::TextDelta(String::from("partial")),
            TextTurnEvent::TextComplete(String::from("different")),
        ]))
        .await
        .unwrap_err();
    assert!(matches!(fault, ExternalApplicationFault::Application));

    let next = external.observe().await.unwrap();
    assert_eq!(next.frame().basis(), FrameBasis::Full);
}

#[tokio::test]
async fn direct_control_fact_order_is_delta_seal_completion() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "facts");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();
    control
        .act(
            observation.ingress_generation(),
            text_protocol(vec![
                TextTurnEvent::TextDelta(String::from("a")),
                TextTurnEvent::TextComplete(String::from("a")),
            ]),
        )
        .await
        .unwrap();

    assert!(matches!(
        facts.next().await.unwrap().unwrap(),
        ProviderFact::TextDelta { delta, .. } if delta == "a"
    ));
    assert!(matches!(
        facts.next().await.unwrap().unwrap(),
        ProviderFact::TextSealed { text, .. } if text == "a"
    ));
    assert!(matches!(
        facts.next().await.unwrap().unwrap(),
        ProviderFact::ReactionCompleted {
            primary_text: Some(_)
        }
    ));
    assert!(facts.next().await.is_none());
}

#[tokio::test]
async fn public_text_constructor_emits_exact_seal_then_completion_and_claims_once() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "typed-text");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();
    let generation = observation.ingress_generation();

    control
        .act(generation, ExternalAct::text("typed"))
        .await
        .unwrap();
    assert_eq!(
        control.act(generation, ExternalAct::text("repeat")).await,
        Err(ExternalControlFault::StaleIngress)
    );

    let output = match facts.next().await.unwrap().unwrap() {
        ProviderFact::TextSealed { output, text, .. } => {
            assert_eq!(text, "typed");
            output
        }
        fact => panic!("typed text must seal first, got {fact:?}"),
    };
    assert!(matches!(
        facts.next().await.unwrap().unwrap(),
        ProviderFact::ReactionCompleted {
            primary_text: Some(primary)
        } if primary == output
    ));
    assert!(facts.next().await.is_none());
}

#[tokio::test]
async fn public_empty_completion_emits_only_no_primary_terminal_and_claims_once() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "empty");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();
    let generation = observation.ingress_generation();

    control.complete(generation).await.unwrap();
    assert_eq!(
        control.complete(generation).await,
        Err(ExternalControlFault::StaleIngress)
    );
    assert!(matches!(
        facts.next().await.unwrap().unwrap(),
        ProviderFact::ReactionCompleted { primary_text: None }
    ));
    assert!(facts.next().await.is_none());
    assert_eq!(
        control.complete(generation).await,
        Err(ExternalControlFault::StaleIngress)
    );
}

#[tokio::test]
async fn public_text_constructor_reuses_the_external_output_limit() {
    let (mut port, control) = ExternalProviderPort::new().unwrap();
    let declaration = port.declare().unwrap();
    let frame = test_frame(&declaration, 1, "oversized");
    let mut facts = port.submit(frame).await.unwrap();
    let observation = control.next_observation().await.unwrap();

    control
        .act(
            observation.ingress_generation(),
            ExternalAct::text("x".repeat(MAX_EXTERNAL_TEXT_BYTES + 1)),
        )
        .await
        .unwrap();

    let fault = facts.next().await.unwrap().unwrap_err();
    assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
    assert_eq!(fault.code(), ReactionPortFaultCode::Limit);
    assert_eq!(fault.reason(), ReactionPortFaultReason::OutputLimit);
    assert!(facts.next().await.is_none());
}
