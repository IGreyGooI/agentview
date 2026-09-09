use std::{
    collections::VecDeque,
    convert::Infallible,
    future::Future,
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    task::Poll,
    thread,
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFault, ApplicationFaultKind, ApplicationFaultReason,
            ApplicationFaultStage, ExitReason, Frame, FrameCapabilities, FrameConstraints,
            FrameProfile, ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault,
            ReactionPortFaultCode, ReactionPortFaultReason, RenderedProjection, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::{poll, FutureExt};
use tokio::sync::Notify;

struct PreparationGate {
    started: Notify,
    finished: Notify,
    release: Notify,
}

impl PreparationGate {
    fn new() -> Self {
        Self {
            started: Notify::new(),
            finished: Notify::new(),
            release: Notify::new(),
        }
    }
}

async fn wait_for_preparation_notification<F>(
    operation: &mut Pin<Box<F>>,
    notification: &Notify,
    message: &'static str,
) where
    F: Future,
{
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = notification.notified() => {}
            _ = operation => panic!("{message}"),
        }
    })
    .await
    .expect(message);
}

struct PreparationDropProbe(Arc<AtomicUsize>);

impl Drop for PreparationDropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct RecordingProbe {
    declarations: AtomicUsize,
    submissions: AtomicUsize,
    handoffs: AtomicUsize,
    last_frame: Mutex<Vec<u8>>,
}

impl RecordingProbe {
    fn last_frame_text(&self) -> String {
        String::from_utf8_lossy(&self.last_frame.lock().unwrap()).into_owned()
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

struct RecordingPort {
    declaration: TargetDeclaration,
    probe: Arc<RecordingProbe>,
    continuity_rejections: usize,
    on_continuity_rejection: Option<Arc<dyn Fn() + Send + Sync>>,
    completion_gate: Option<Arc<Notify>>,
    submit_faults: VecDeque<SubmitFault>,
}

#[async_trait]
impl ReactionPort for RecordingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.probe.declarations.fetch_add(1, Ordering::Release);
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.probe.submissions.fetch_add(1, Ordering::Release);
        if let Some(fault) = self.submit_faults.pop_front() {
            return Err(fault);
        }
        if self.continuity_rejections > 0 {
            self.continuity_rejections -= 1;
            if let Some(on_rejection) = &self.on_continuity_rejection {
                on_rejection();
            }
            let next_epoch = self
                .declaration
                .continuity()
                .epoch()
                .get()
                .get()
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .unwrap();
            self.declaration = TargetDeclaration::full(
                self.declaration.identity(),
                TargetEpoch::new(next_epoch),
                self.declaration.profile().clone(),
            );
            frame.check_handoff_precondition(&self.declaration)?;
            unreachable!("changed continuity must reject the prepared frame");
        }
        frame.check_handoff_precondition(&self.declaration)?;
        *self.probe.last_frame.lock().unwrap() = frame.submission().canonical_bytes().to_vec();
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        self.probe.handoffs.fetch_add(1, Ordering::Release);
        let completion_gate = self.completion_gate.clone();
        Ok(Box::pin(futures::stream::once(async move {
            if let Some(gate) = completion_gate {
                gate.notified().await;
            }
            Ok(ProviderFact::ReactionCompleted { primary_text: None })
        })))
    }
}

fn recording_port() -> (RecordingPort, Arc<RecordingProbe>) {
    recording_port_with_continuity_rejection(None)
}

fn recording_port_with_continuity_rejection(
    on_rejection: Option<Arc<dyn Fn() + Send + Sync>>,
) -> (RecordingPort, Arc<RecordingProbe>) {
    let probe = Arc::new(RecordingProbe::default());
    let declaration = TargetDeclaration::full(
        TargetIdentity::new(NonZeroU128::new(1).unwrap()),
        TargetEpoch::new(NonZeroU64::new(1).unwrap()),
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 4_096,
                max_component_bytes: 1_024,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::NONE,
        ),
    );
    (
        RecordingPort {
            declaration,
            probe: Arc::clone(&probe),
            continuity_rejections: usize::from(on_rejection.is_some()),
            on_continuity_rejection: on_rejection,
            completion_gate: None,
            submit_faults: VecDeque::new(),
        },
        probe,
    )
}

fn recording_port_with_completion_gate(
    completion_gate: Arc<Notify>,
) -> (RecordingPort, Arc<RecordingProbe>) {
    let (mut port, probe) = recording_port();
    port.completion_gate = Some(completion_gate);
    (port, probe)
}

fn recording_port_with_submit_faults(
    submit_faults: impl IntoIterator<Item = SubmitFault>,
) -> (RecordingPort, Arc<RecordingProbe>) {
    let (mut port, probe) = recording_port();
    port.submit_faults = submit_faults.into_iter().collect();
    (port, probe)
}

#[derive(Clone, Copy)]
enum PreparationOperation {
    Prepare,
    React,
}

impl PreparationOperation {
    async fn run(self, app: &mut Application<RecordingPort>) -> Result<(), ApplicationFault> {
        let flow = match self {
            Self::Prepare => app.prepare().await,
            Self::React => app.react().await,
        }?;
        assert_continue(flow);
        Ok(())
    }
}

fn assert_continue(flow: ControlFlow<ExitReason>) {
    assert_eq!(flow, ControlFlow::Continue(()));
}

fn recording_port_for_nested_waves() -> (RecordingPort, Arc<RecordingProbe>) {
    let (mut port, probe) = recording_port();
    port.declaration = TargetDeclaration::full(
        port.declaration.identity(),
        port.declaration.continuity().epoch(),
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 65_536,
                max_component_bytes: 16_384,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::NONE,
        ),
    );
    (port, probe)
}

#[component]
fn preparation_view(gate: Arc<PreparationGate>) -> Component {
    let state = use_signal(|| String::from("loading"));
    let ready = state.clone();
    use_preparation(move || async move {
        gate.started.notify_one();
        gate.release.notified().await;
        ready.set(String::from("ready"))
    });
    let value = state.with(Clone::clone).unwrap();
    view! { preparation { "{value}" } }
}

#[component]
fn failing_once_preparation(attempts: Arc<AtomicUsize>) -> Component {
    use_preparation(move || async move {
        if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
            Err("preparation unavailable")
        } else {
            Ok(())
        }
    });
    view! { preparation_retry {} }
}

#[component]
fn counting_preparation(runs: Arc<AtomicUsize>) -> Component {
    use_preparation(move || async move {
        runs.fetch_add(1, Ordering::AcqRel);
        Ok::<(), Infallible>(())
    });
    view! { preparation_count {} }
}

#[component]
fn earlier_then_failing_preparation(
    earlier_runs: Arc<AtomicUsize>,
    later_runs: Arc<AtomicUsize>,
) -> Component {
    use_preparation(move || async move {
        earlier_runs.fetch_add(1, Ordering::AcqRel);
        Ok::<(), Infallible>(())
    });
    use_preparation(move || async move {
        if later_runs.fetch_add(1, Ordering::AcqRel) == 0 {
            Err("later preparation failed")
        } else {
            Ok(())
        }
    });
    view! { later_failure {} }
}

#[component]
fn cancellable_preparation(
    gate: Arc<PreparationGate>,
    attempts: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
) -> Component {
    use_preparation(move || async move {
        attempts.fetch_add(1, Ordering::AcqRel);
        let _drop_probe = PreparationDropProbe(drops);
        gate.started.notify_one();
        gate.release.notified().await;
        Ok::<(), Infallible>(())
    });
    view! { cancellable_preparation {} }
}

#[component]
fn earlier_then_cancellable_preparation(
    gate: Arc<PreparationGate>,
    earlier_runs: Arc<AtomicUsize>,
    later_runs: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
) -> Component {
    use_preparation(move || async move {
        earlier_runs.fetch_add(1, Ordering::AcqRel);
        Ok::<(), Infallible>(())
    });
    use_preparation(move || async move {
        later_runs.fetch_add(1, Ordering::AcqRel);
        let _drop_probe = PreparationDropProbe(drops);
        gate.started.notify_one();
        gate.release.notified().await;
        Ok::<(), Infallible>(())
    });
    view! { later_cancellation {} }
}

#[component]
fn same_wave_preparation_view(
    first_gate: Arc<PreparationGate>,
    second_gate: Arc<PreparationGate>,
) -> Component {
    let first_state = use_signal(|| String::from("first-pending"));
    let second_state = use_signal(|| String::from("second-pending"));
    let first_ready = first_state.clone();
    let second_ready = second_state.clone();

    use_preparation(move || async move {
        first_gate.started.notify_one();
        first_gate.release.notified().await;
        first_ready
            .set(String::from("first-ready"))
            .expect("first same-wave preparation Signal write");
        first_gate.finished.notify_one();
        Ok::<(), Infallible>(())
    });
    use_preparation(move || async move {
        second_gate.started.notify_one();
        second_gate.release.notified().await;
        second_ready
            .set(String::from("second-ready"))
            .expect("second same-wave preparation Signal write");
        second_gate.finished.notify_one();
        Ok::<(), Infallible>(())
    });

    let first = first_state.with(Clone::clone).unwrap();
    let second = second_state.with(Clone::clone).unwrap();
    view! {
        same_wave_preparation { "{first}" }
        same_wave_preparation { "{second}" }
    }
}

#[component]
fn pending_then_failing_same_wave_preparation(
    pending_gate: Arc<PreparationGate>,
    failing_gate: Arc<PreparationGate>,
    pending_runs: Arc<AtomicUsize>,
    failing_runs: Arc<AtomicUsize>,
    pending_drops: Arc<AtomicUsize>,
) -> Component {
    use_preparation(move || async move {
        pending_runs.fetch_add(1, Ordering::AcqRel);
        let _drop_probe = PreparationDropProbe(pending_drops);
        pending_gate.started.notify_one();
        pending_gate.release.notified().await;
        Ok::<(), Infallible>(())
    });
    use_preparation(move || async move {
        let attempt = failing_runs.fetch_add(1, Ordering::AcqRel);
        failing_gate.started.notify_one();
        failing_gate.release.notified().await;
        if attempt == 0 {
            Err("later same-wave preparation failed")
        } else {
            Ok(())
        }
    });
    view! { pending_then_failing_same_wave {} }
}

#[component]
fn same_wave_cancellable_preparations(
    first_gate: Arc<PreparationGate>,
    second_gate: Arc<PreparationGate>,
    first_attempts: Arc<AtomicUsize>,
    second_attempts: Arc<AtomicUsize>,
    first_drops: Arc<AtomicUsize>,
    second_drops: Arc<AtomicUsize>,
) -> Component {
    use_preparation(move || async move {
        first_attempts.fetch_add(1, Ordering::AcqRel);
        let _drop_probe = PreparationDropProbe(first_drops);
        first_gate.started.notify_one();
        first_gate.release.notified().await;
        Ok::<(), Infallible>(())
    });
    use_preparation(move || async move {
        second_attempts.fetch_add(1, Ordering::AcqRel);
        let _drop_probe = PreparationDropProbe(second_drops);
        second_gate.started.notify_one();
        second_gate.release.notified().await;
        Ok::<(), Infallible>(())
    });
    view! { same_wave_cancellable {} }
}

#[component]
fn nested_child_preparation(loader_order: Arc<Mutex<Vec<&'static str>>>) -> Component {
    let state = use_signal(|| String::from("child-loading"));
    let ready = state.clone();
    use_preparation(move || async move {
        loader_order.lock().unwrap().push("child");
        ready.set(String::from("child-ready"))
    });
    let value = state.with(Clone::clone).unwrap();
    view! { nested_child { "{value}" } }
}

#[component]
fn nested_parent_preparation(loader_order: Arc<Mutex<Vec<&'static str>>>) -> Component {
    let state = use_signal(|| String::from("parent-loading"));
    let show_child = use_signal(|| false);
    let ready = state.clone();
    let mount_child = show_child.clone();
    let parent_order = Arc::clone(&loader_order);
    use_preparation(move || async move {
        parent_order.lock().unwrap().push("parent");
        ready.set(String::from("parent-ready"))?;
        mount_child.set(true)
    });
    let value = state.with(Clone::clone).unwrap();
    let child = if show_child.with(|show| *show).unwrap() {
        nested_child_preparation(loader_order)
    } else {
        view! { nested_child_pending {} }
    };
    view! {
        nested_parent { "{value}" }
        { child }
    }
}

#[component]
fn remounting_child_preparation(runs: Arc<AtomicUsize>) -> Component {
    use_preparation(move || async move {
        runs.fetch_add(1, Ordering::AcqRel);
        Ok::<(), Infallible>(())
    });
    view! { remounting_child {} }
}

#[component]
fn remounting_parent_preparation(
    parent_runs: Arc<AtomicUsize>,
    child_runs: Arc<AtomicUsize>,
) -> Component {
    let show_child = use_signal(|| false);
    let toggle_child = show_child.clone();
    use_preparation(move || async move {
        parent_runs.fetch_add(1, Ordering::AcqRel);
        let shown = toggle_child.with(|shown| *shown)?;
        toggle_child.set(!shown)
    });
    let child = if show_child.with(|shown| *shown).unwrap() {
        remounting_child_preparation(child_runs)
    } else {
        view! { remounting_child_absent {} }
    };
    view! {
        remounting_parent {}
        { child }
    }
}

#[component]
fn cycling_child_preparation(
    phase: Signal<usize>,
    loader_order: Arc<Mutex<Vec<&'static str>>>,
) -> Component {
    let ready = use_signal(|| false);
    let set_ready = ready.clone();
    use_preparation(move || async move {
        loader_order.lock().unwrap().push("child");
        if phase.with(|phase| *phase)? == 0 {
            phase.set(1)?;
        }
        set_ready.set(true)
    });
    let label = if ready.with(|ready| *ready).unwrap() {
        "remounted-ready"
    } else {
        "remounted-pending"
    };
    view! { cycling_child { "{label}" } }
}

#[component]
fn remount_bridge_preparation(
    phase: Signal<usize>,
    loader_order: Arc<Mutex<Vec<&'static str>>>,
) -> Component {
    use_preparation(move || async move {
        loader_order.lock().unwrap().push("bridge");
        phase.set(2)
    });
    view! { remount_bridge {} }
}

#[component]
fn cycling_parent_preparation(loader_order: Arc<Mutex<Vec<&'static str>>>) -> Component {
    let phase = use_signal(|| 0_usize);
    let parent_order = Arc::clone(&loader_order);
    use_preparation(move || async move {
        parent_order.lock().unwrap().push("parent");
        Ok::<(), Infallible>(())
    });
    let child = if phase.with(|phase| *phase).unwrap() == 1 {
        remount_bridge_preparation(phase, loader_order)
    } else {
        cycling_child_preparation(phase, loader_order)
    };
    view! {
        cycling_parent {}
        { child }
    }
}

#[component]
fn wave_preparation_node(level: usize, depth: usize, runs: Arc<AtomicUsize>) -> Component {
    let ready = use_signal(|| false);
    let set_ready = ready.clone();
    let child_runs = Arc::clone(&runs);
    use_preparation(move || async move {
        runs.fetch_add(1, Ordering::AcqRel);
        set_ready.set(true)
    });
    let ready = ready.with(|ready| *ready).unwrap();
    let label = if ready {
        format!("wave-{level}-ready")
    } else {
        format!("wave-{level}-pending")
    };
    let child = if ready && level < depth {
        wave_preparation_node(level + 1, depth, child_runs)
    } else {
        view! { wave_terminal {} }
    };
    view! {
        wave { "{label}" }
        { child }
    }
}

#[component]
fn preparation_free_view() -> Component {
    view! { preparation_free { "ready" } }
}

#[component]
fn signal_preparation(
    exposed: Arc<Mutex<Option<Signal<String>>>>,
    runs: Arc<AtomicUsize>,
) -> Component {
    let state = use_signal(|| String::from("initial"));
    *exposed.lock().unwrap() = Some(state.clone());
    use_preparation(move || async move {
        runs.fetch_add(1, Ordering::AcqRel);
        Ok::<(), Infallible>(())
    });
    let value = state.with(Clone::clone).unwrap();
    view! { preparation_signal { "{value}" } }
}

#[component]
fn synchronous_factory_signal_access(observed: Arc<AtomicUsize>) -> Component {
    let state = use_signal(|| 1_usize);
    let factory_read = state.clone();
    let factory_write = state.clone();
    use_preparation(move || {
        let value = factory_read
            .with(|value| *value)
            .expect("preparation factory Signal read");
        factory_write
            .set(value + 1)
            .expect("preparation factory Signal write");
        observed.store(value + 1, Ordering::Release);
        async move { Ok::<(), Infallible>(()) }
    });
    let value = state.with(|value| *value).unwrap();
    view! { preparation_factory { "{value}" } }
}

#[component]
fn panicking_preparation_factory() -> Component {
    use_preparation(|| -> std::future::Ready<Result<(), Infallible>> {
        panic!("preparation factory panic")
    });
    view! { panicking_preparation {} }
}

#[component]
fn pending_sibling_before_panicking_preparation_factory(
    sibling_polls: Arc<AtomicUsize>,
    sibling_drops: Arc<AtomicUsize>,
) -> Component {
    use_preparation(move || {
        let drop_probe = PreparationDropProbe(sibling_drops);
        async move {
            sibling_polls.fetch_add(1, Ordering::AcqRel);
            let _drop_probe = drop_probe;
            std::future::pending::<Result<(), Infallible>>().await
        }
    });
    use_preparation(|| -> std::future::Ready<Result<(), Infallible>> {
        panic!("later preparation factory panic")
    });
    view! { pending_sibling_before_panicking_factory {} }
}

#[component]
fn completed_exit_root() -> Component {
    let exit = use_application_exit();
    use_preparation(move || async move {
        exit.request(ExitReason::Completed)
            .expect("mounted application exit handle");
        Ok::<(), Infallible>(())
    });
    view! { completed_exit {} }
}

#[component]
fn exit_after_two_successful_reactions(runs: Arc<AtomicUsize>) -> Component {
    let exit = use_application_exit();
    use_preparation(move || async move {
        if runs.fetch_add(1, Ordering::AcqRel) == 2 {
            exit.request(ExitReason::Completed)
                .expect("mounted application exit handle");
        }
        Ok::<(), Infallible>(())
    });
    view! { exit_after_two_successful_reactions {} }
}

#[component]
fn exit_then_failing_preparation() -> Component {
    let exit = use_application_exit();
    use_preparation(move || {
        exit.request(ExitReason::Requested)
            .expect("mounted application exit handle");
        std::future::ready(Err::<(), _>("preparation failure wins exit"))
    });
    view! { exit_then_failing_preparation {} }
}

#[component]
fn exit_then_panicking_preparation() -> Component {
    let exit = use_application_exit();
    use_preparation(move || -> std::future::Ready<Result<(), Infallible>> {
        exit.request(ExitReason::Requested)
            .expect("mounted application exit handle");
        panic!("preparation panic wins exit")
    });
    view! { exit_then_panicking_preparation {} }
}

#[tokio::test]
async fn react_waits_for_preparation_and_submits_its_signal_write() {
    let gate = Arc::new(PreparationGate::new());
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let gate = Arc::clone(&gate);
            move || preparation_view(Arc::clone(&gate))
        },
        port,
    )
    .unwrap();

    let mut reaction = Box::pin(app.react());
    tokio::select! {
        _ = gate.started.notified() => {}
        result = &mut reaction => panic!("preparation did not block handoff: {result:?}"),
    }
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    gate.release.notify_one();
    assert_continue(reaction.await.unwrap());

    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    assert!(probe.last_frame_text().contains("ready"));
    assert!(!probe.last_frame_text().contains("loading"));
    assert!(app.current_projection().is_prepared());
}

#[tokio::test]
async fn same_wave_preparations_start_concurrently_and_wait_for_all_before_handoff() {
    let first_gate = Arc::new(PreparationGate::new());
    let second_gate = Arc::new(PreparationGate::new());
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let first_gate = Arc::clone(&first_gate);
            let second_gate = Arc::clone(&second_gate);
            move || same_wave_preparation_view(Arc::clone(&first_gate), Arc::clone(&second_gate))
        },
        port,
    )
    .unwrap();

    let mut reaction = Box::pin(app.react());
    wait_for_preparation_notification(
        &mut reaction,
        &first_gate.started,
        "the first same-wave preparation did not start",
    )
    .await;
    wait_for_preparation_notification(
        &mut reaction,
        &second_gate.started,
        "the second preparation did not start while the first remained pending",
    )
    .await;
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    second_gate.release.notify_one();
    wait_for_preparation_notification(
        &mut reaction,
        &second_gate.finished,
        "the released second same-wave preparation did not finish",
    )
    .await;
    assert!(matches!(poll!(reaction.as_mut()), Poll::Pending));
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    first_gate.release.notify_one();
    assert_continue(reaction.await.unwrap());

    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    let frame = probe.last_frame_text();
    assert!(frame.contains("first-ready"));
    assert!(frame.contains("second-ready"));
    assert!(!frame.contains("first-pending"));
    assert!(!frame.contains("second-pending"));
}

#[tokio::test]
async fn host_exit_interrupts_blocked_preparation_without_provider_submission() {
    let gate = Arc::new(PreparationGate::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let gate = Arc::clone(&gate);
            let attempts = Arc::clone(&attempts);
            let drops = Arc::clone(&drops);
            move || {
                cancellable_preparation(
                    Arc::clone(&gate),
                    Arc::clone(&attempts),
                    Arc::clone(&drops),
                )
            }
        },
        port,
    )
    .unwrap();
    let exit = app.exit_handle();

    let mut reaction = Box::pin(app.react());
    tokio::select! {
        _ = gate.started.notified() => {}
        result = &mut reaction => panic!("preparation did not block reaction: {result:?}"),
    }

    exit.request(ExitReason::Requested).unwrap();
    assert_eq!(
        reaction.await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while drops.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exit must cancel the blocked preparation");
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    assert_eq!(
        app.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn component_completed_exit_is_sticky_and_never_submits() {
    let (port, probe) = recording_port();
    let mut app = Application::mount(completed_exit_root, port).unwrap();

    assert_eq!(
        app.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Completed)
    );
    assert_eq!(
        app.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Completed)
    );
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_after_provider_handoff_waits_for_eof_then_stops_without_a_second_submit() {
    let completion_gate = Arc::new(Notify::new());
    let (port, probe) = recording_port_with_completion_gate(Arc::clone(&completion_gate));
    let mut app = Application::mount(preparation_free_view, port).unwrap();
    let exit = app.exit_handle();
    let mut reaction = Box::pin(app.react());
    assert!(matches!(poll!(reaction.as_mut()), Poll::Pending));

    tokio::time::timeout(Duration::from_secs(1), async {
        while probe.handoffs.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider handoff must begin");
    exit.request(ExitReason::Requested).unwrap();
    assert!(matches!(poll!(reaction.as_mut()), Poll::Pending));
    assert_eq!(probe.submissions.load(Ordering::Acquire), 1);

    completion_gate.notify_one();
    assert_eq!(
        reaction.await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    assert_eq!(
        app.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    assert_eq!(probe.submissions.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn preparation_error_and_panic_are_not_converted_to_normal_exit() {
    let (port, probe) = recording_port();
    let mut app = Application::mount(exit_then_failing_preparation, port).unwrap();

    let fault = app.react().await.unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);

    let (port, probe) = recording_port();
    let mut app = Application::mount(exit_then_panicking_preparation, port).unwrap();
    let panic = AssertUnwindSafe(app.react())
        .catch_unwind()
        .await
        .expect_err("preparation panic must not become a normal exit");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("preparation panic wins exit")
    );
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn host_exit_handles_close_after_shutdown_and_application_drop() {
    let (port, _) = recording_port();
    let app = Application::mount(preparation_free_view, port).unwrap();
    let shutdown_handle = app.exit_handle();
    app.shutdown().await.unwrap();
    assert_eq!(
        shutdown_handle.request(ExitReason::Completed),
        Err(ApplicationExitError::Closed)
    );

    let dropped_handle = {
        let (port, _) = recording_port();
        let app = Application::mount(preparation_free_view, port).unwrap();
        app.exit_handle()
    };
    assert_eq!(
        dropped_handle.request(ExitReason::Completed),
        Err(ApplicationExitError::Closed)
    );
}

#[tokio::test]
async fn run_reacts_twice_before_component_preparation_exits_without_a_third_submit() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let runs = Arc::clone(&runs);
            move || exit_after_two_successful_reactions(Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    let mut run = Box::pin(app.run());
    assert!(matches!(poll!(run.as_mut()), Poll::Pending));
    assert_eq!(run.await.unwrap(), ExitReason::Completed);
    assert_eq!(runs.load(Ordering::Acquire), 3);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 2);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_returns_preparation_fault_without_retry_and_keeps_the_owner_usable() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let attempts = Arc::clone(&attempts);
            move || failing_once_preparation(Arc::clone(&attempts))
        },
        port,
    )
    .unwrap();

    let fault = tokio::time::timeout(Duration::from_secs(1), app.run())
        .await
        .expect("run must return the first preparation fault")
        .unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);

    assert_continue(app.react().await.unwrap());
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_returns_reaction_fault_without_retry_and_keeps_the_owner_usable() {
    let (port, probe) =
        recording_port_with_submit_faults([SubmitFault::Rejected(ReactionPortFault::retryable(
            ReactionPortFaultCode::Unavailable,
            ReactionPortFaultReason::Transport,
        ))]);
    let mut app = Application::mount(preparation_free_view, port).unwrap();

    let fault = tokio::time::timeout(Duration::from_secs(1), app.run())
        .await
        .expect("run must return the first reaction fault")
        .unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::Submit);
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(
        fault.reason(),
        ApplicationFaultReason::Port(ReactionPortFaultReason::Transport)
    );
    assert_eq!(probe.submissions.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    assert_continue(app.react().await.unwrap());
    assert_eq!(probe.submissions.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_host_exit_interrupts_pending_preparation_and_leaves_the_owner_for_shutdown() {
    let gate = Arc::new(PreparationGate::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let gate = Arc::clone(&gate);
            let attempts = Arc::clone(&attempts);
            let drops = Arc::clone(&drops);
            move || {
                cancellable_preparation(
                    Arc::clone(&gate),
                    Arc::clone(&attempts),
                    Arc::clone(&drops),
                )
            }
        },
        port,
    )
    .unwrap();
    let exit = app.exit_handle();
    let mut run = Box::pin(app.run());

    tokio::select! {
        _ = gate.started.notified() => {}
        result = &mut run => panic!("run ended before preparation was interrupted: {result:?}"),
    }
    exit.request(ExitReason::Requested).unwrap();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("host exit must interrupt run")
            .unwrap(),
        ExitReason::Requested
    );
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    app.shutdown().await.unwrap();
}

#[test]
fn preparation_free_bootstrap_projection_is_prepared() {
    let (port, probe) = recording_port();
    let app = Application::mount(preparation_free_view, port).unwrap();

    let snapshot = app.current_projection();
    assert!(snapshot.is_prepared());
    assert!(!snapshot.is_dirty());
    assert!(projection_text(snapshot.projection()).contains("ready"));
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn ordinary_preparation_error_keeps_the_same_application_retryable() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let attempts = Arc::clone(&attempts);
            move || failing_once_preparation(Arc::clone(&attempts))
        },
        port,
    )
    .unwrap();

    let fault = app.react().await.unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    assert_continue(app.react().await.unwrap());
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn every_explicit_react_reruns_successful_preparation() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let runs = Arc::clone(&runs);
            move || counting_preparation(Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    assert_continue(app.react().await.unwrap());
    assert_continue(app.react().await.unwrap());

    assert_eq!(runs.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn prepare_then_react_reruns_successful_preparation() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let runs = Arc::clone(&runs);
            move || counting_preparation(Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    assert_continue(app.prepare().await.unwrap());
    assert_eq!(runs.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    assert_continue(app.react().await.unwrap());
    assert_eq!(runs.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn successful_earlier_preparation_reruns_after_a_later_preparation_fails() {
    for operation in [PreparationOperation::Prepare, PreparationOperation::React] {
        let earlier_runs = Arc::new(AtomicUsize::new(0));
        let later_runs = Arc::new(AtomicUsize::new(0));
        let (port, probe) = recording_port();
        let mut app = Application::mount(
            {
                let earlier_runs = Arc::clone(&earlier_runs);
                let later_runs = Arc::clone(&later_runs);
                move || {
                    earlier_then_failing_preparation(
                        Arc::clone(&earlier_runs),
                        Arc::clone(&later_runs),
                    )
                }
            },
            port,
        )
        .unwrap();

        let fault = operation.run(&mut app).await.unwrap_err();
        assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
        assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
        assert_eq!(earlier_runs.load(Ordering::Acquire), 1);
        assert_eq!(later_runs.load(Ordering::Acquire), 1);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

        operation.run(&mut app).await.unwrap();
        assert_eq!(earlier_runs.load(Ordering::Acquire), 2);
        assert_eq!(later_runs.load(Ordering::Acquire), 2);
    }
}

#[tokio::test]
async fn dropping_pending_react_cancels_preparation_without_handoff_and_retries() {
    let gate = Arc::new(PreparationGate::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let gate = Arc::clone(&gate);
            let attempts = Arc::clone(&attempts);
            let drops = Arc::clone(&drops);
            move || {
                cancellable_preparation(
                    Arc::clone(&gate),
                    Arc::clone(&attempts),
                    Arc::clone(&drops),
                )
            }
        },
        port,
    )
    .unwrap();

    let mut reaction = Box::pin(app.react());
    tokio::select! {
        _ = gate.started.notified() => {}
        result = &mut reaction => panic!("preparation did not remain pending: {result:?}"),
    }
    drop(reaction);

    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    gate.release.notify_one();
    assert_continue(app.react().await.unwrap());
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn successful_earlier_preparation_reruns_after_a_later_preparation_is_cancelled() {
    for operation in [PreparationOperation::Prepare, PreparationOperation::React] {
        let gate = Arc::new(PreparationGate::new());
        let earlier_runs = Arc::new(AtomicUsize::new(0));
        let later_runs = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let (port, probe) = recording_port();
        let mut app = Application::mount(
            {
                let gate = Arc::clone(&gate);
                let earlier_runs = Arc::clone(&earlier_runs);
                let later_runs = Arc::clone(&later_runs);
                let drops = Arc::clone(&drops);
                move || {
                    earlier_then_cancellable_preparation(
                        Arc::clone(&gate),
                        Arc::clone(&earlier_runs),
                        Arc::clone(&later_runs),
                        Arc::clone(&drops),
                    )
                }
            },
            port,
        )
        .unwrap();

        let mut preparation = Box::pin(operation.run(&mut app));
        tokio::select! {
            _ = gate.started.notified() => {}
            result = &mut preparation => panic!("later preparation did not remain pending: {result:?}"),
        }
        drop(preparation);

        assert_eq!(earlier_runs.load(Ordering::Acquire), 1);
        assert_eq!(later_runs.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

        gate.release.notify_one();
        operation.run(&mut app).await.unwrap();
        assert_eq!(earlier_runs.load(Ordering::Acquire), 2);
        assert_eq!(later_runs.load(Ordering::Acquire), 2);
    }
}

#[tokio::test]
async fn later_same_wave_error_returns_and_cancels_an_earlier_pending_sibling() {
    let pending_gate = Arc::new(PreparationGate::new());
    let failing_gate = Arc::new(PreparationGate::new());
    let pending_runs = Arc::new(AtomicUsize::new(0));
    let failing_runs = Arc::new(AtomicUsize::new(0));
    let pending_drops = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let pending_gate = Arc::clone(&pending_gate);
            let failing_gate = Arc::clone(&failing_gate);
            let pending_runs = Arc::clone(&pending_runs);
            let failing_runs = Arc::clone(&failing_runs);
            let pending_drops = Arc::clone(&pending_drops);
            move || {
                pending_then_failing_same_wave_preparation(
                    Arc::clone(&pending_gate),
                    Arc::clone(&failing_gate),
                    Arc::clone(&pending_runs),
                    Arc::clone(&failing_runs),
                    Arc::clone(&pending_drops),
                )
            }
        },
        port,
    )
    .unwrap();

    let mut reaction = Box::pin(app.react());
    wait_for_preparation_notification(
        &mut reaction,
        &pending_gate.started,
        "the pending same-wave preparation did not start",
    )
    .await;
    wait_for_preparation_notification(
        &mut reaction,
        &failing_gate.started,
        "the later failing preparation did not start while its sibling was pending",
    )
    .await;

    failing_gate.release.notify_one();
    let fault = tokio::time::timeout(Duration::from_secs(1), reaction.as_mut())
        .await
        .expect("a later same-wave error was blocked by a permanently pending sibling")
        .unwrap_err();
    drop(reaction);

    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
    assert_eq!(pending_runs.load(Ordering::Acquire), 1);
    assert_eq!(failing_runs.load(Ordering::Acquire), 1);
    assert_eq!(pending_drops.load(Ordering::Acquire), 1);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    pending_gate.release.notify_one();
    failing_gate.release.notify_one();
    assert_continue(app.react().await.unwrap());
    assert_eq!(pending_runs.load(Ordering::Acquire), 2);
    assert_eq!(failing_runs.load(Ordering::Acquire), 2);
    assert_eq!(pending_drops.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn dropping_pending_same_wave_preparation_cancels_all_siblings_and_retries() {
    for operation in [PreparationOperation::Prepare, PreparationOperation::React] {
        let first_gate = Arc::new(PreparationGate::new());
        let second_gate = Arc::new(PreparationGate::new());
        let first_attempts = Arc::new(AtomicUsize::new(0));
        let second_attempts = Arc::new(AtomicUsize::new(0));
        let first_drops = Arc::new(AtomicUsize::new(0));
        let second_drops = Arc::new(AtomicUsize::new(0));
        let (port, probe) = recording_port();
        let mut app = Application::mount(
            {
                let first_gate = Arc::clone(&first_gate);
                let second_gate = Arc::clone(&second_gate);
                let first_attempts = Arc::clone(&first_attempts);
                let second_attempts = Arc::clone(&second_attempts);
                let first_drops = Arc::clone(&first_drops);
                let second_drops = Arc::clone(&second_drops);
                move || {
                    same_wave_cancellable_preparations(
                        Arc::clone(&first_gate),
                        Arc::clone(&second_gate),
                        Arc::clone(&first_attempts),
                        Arc::clone(&second_attempts),
                        Arc::clone(&first_drops),
                        Arc::clone(&second_drops),
                    )
                }
            },
            port,
        )
        .unwrap();

        let mut preparation = Box::pin(operation.run(&mut app));
        wait_for_preparation_notification(
            &mut preparation,
            &first_gate.started,
            "the first cancellable same-wave preparation did not start",
        )
        .await;
        wait_for_preparation_notification(
            &mut preparation,
            &second_gate.started,
            "the second cancellable preparation did not start while its sibling was pending",
        )
        .await;
        drop(preparation);

        assert_eq!(first_attempts.load(Ordering::Acquire), 1);
        assert_eq!(second_attempts.load(Ordering::Acquire), 1);
        assert_eq!(first_drops.load(Ordering::Acquire), 1);
        assert_eq!(second_drops.load(Ordering::Acquire), 1);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

        first_gate.release.notify_one();
        second_gate.release.notify_one();
        operation.run(&mut app).await.unwrap();
        assert_eq!(first_attempts.load(Ordering::Acquire), 2);
        assert_eq!(second_attempts.load(Ordering::Acquire), 2);
        assert_eq!(first_drops.load(Ordering::Acquire), 2);
        assert_eq!(second_drops.load(Ordering::Acquire), 2);
        assert_eq!(
            probe.handoffs.load(Ordering::Acquire),
            match operation {
                PreparationOperation::Prepare => 0,
                PreparationOperation::React => 1,
            }
        );
    }
}

#[tokio::test]
async fn host_exit_cancels_all_started_same_wave_preparations() {
    let first_gate = Arc::new(PreparationGate::new());
    let second_gate = Arc::new(PreparationGate::new());
    let first_attempts = Arc::new(AtomicUsize::new(0));
    let second_attempts = Arc::new(AtomicUsize::new(0));
    let first_drops = Arc::new(AtomicUsize::new(0));
    let second_drops = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let first_gate = Arc::clone(&first_gate);
            let second_gate = Arc::clone(&second_gate);
            let first_attempts = Arc::clone(&first_attempts);
            let second_attempts = Arc::clone(&second_attempts);
            let first_drops = Arc::clone(&first_drops);
            let second_drops = Arc::clone(&second_drops);
            move || {
                same_wave_cancellable_preparations(
                    Arc::clone(&first_gate),
                    Arc::clone(&second_gate),
                    Arc::clone(&first_attempts),
                    Arc::clone(&second_attempts),
                    Arc::clone(&first_drops),
                    Arc::clone(&second_drops),
                )
            }
        },
        port,
    )
    .unwrap();
    let exit = app.exit_handle();

    let mut reaction = Box::pin(app.react());
    wait_for_preparation_notification(
        &mut reaction,
        &first_gate.started,
        "the first same-wave preparation did not start before host exit",
    )
    .await;
    wait_for_preparation_notification(
        &mut reaction,
        &second_gate.started,
        "the second same-wave preparation did not start before host exit",
    )
    .await;

    exit.request(ExitReason::Requested).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), reaction.as_mut())
            .await
            .expect("host exit did not interrupt same-wave preparations")
            .unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    drop(reaction);

    assert_eq!(first_attempts.load(Ordering::Acquire), 1);
    assert_eq!(second_attempts.load(Ordering::Acquire), 1);
    assert_eq!(first_drops.load(Ordering::Acquire), 1);
    assert_eq!(second_drops.load(Ordering::Acquire), 1);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    assert_eq!(
        app.react().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn preparation_stabilizes_nested_mounts_before_the_first_handoff() {
    let loader_order = Arc::new(Mutex::new(Vec::new()));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let loader_order = Arc::clone(&loader_order);
            move || nested_parent_preparation(Arc::clone(&loader_order))
        },
        port,
    )
    .unwrap();

    assert_continue(app.react().await.unwrap());

    assert_eq!(*loader_order.lock().unwrap(), ["parent", "child"]);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    let frame = probe.last_frame_text();
    assert!(frame.contains("parent-ready"));
    assert!(frame.contains("child-ready"));
    assert!(!frame.contains("loading"));
    assert!(!frame.contains("nested_child_pending"));
}

#[tokio::test]
async fn newly_remounted_nested_preparation_runs_in_its_new_mount_generation() {
    let parent_runs = Arc::new(AtomicUsize::new(0));
    let child_runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let parent_runs = Arc::clone(&parent_runs);
            let child_runs = Arc::clone(&child_runs);
            move || remounting_parent_preparation(Arc::clone(&parent_runs), Arc::clone(&child_runs))
        },
        port,
    )
    .unwrap();

    assert_continue(app.prepare().await.unwrap());
    assert_eq!(parent_runs.load(Ordering::Acquire), 1);
    assert_eq!(child_runs.load(Ordering::Acquire), 1);
    assert!(
        !projection_text(app.current_projection().projection()).contains("remounting_child_absent")
    );

    assert_continue(app.prepare().await.unwrap());
    assert_eq!(parent_runs.load(Ordering::Acquire), 2);
    assert_eq!(child_runs.load(Ordering::Acquire), 2);
    assert!(
        projection_text(app.current_projection().projection()).contains("remounting_child_absent")
    );

    assert_continue(app.prepare().await.unwrap());
    assert_eq!(parent_runs.load(Ordering::Acquire), 3);
    assert_eq!(child_runs.load(Ordering::Acquire), 3);
    assert!(
        !projection_text(app.current_projection().projection()).contains("remounting_child_absent")
    );
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn remount_within_one_operation_reruns_the_child_but_not_the_parent() {
    let loader_order = Arc::new(Mutex::new(Vec::new()));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let loader_order = Arc::clone(&loader_order);
            move || cycling_parent_preparation(Arc::clone(&loader_order))
        },
        port,
    )
    .unwrap();

    assert_continue(app.react().await.unwrap());

    assert_eq!(
        *loader_order.lock().unwrap(),
        ["parent", "child", "bridge", "child"]
    );
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    assert!(probe.last_frame_text().contains("remounted-ready"));
    assert!(!probe.last_frame_text().contains("remounted-pending"));
    assert!(!probe.last_frame_text().contains("remount_bridge"));
}

#[tokio::test]
async fn sixteen_preparation_waves_can_write_final_data_and_stabilize() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port_for_nested_waves();
    let mut app = Application::mount(
        {
            let runs = Arc::clone(&runs);
            move || wave_preparation_node(1, 16, Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    assert_continue(app.react().await.unwrap());

    assert_eq!(runs.load(Ordering::Acquire), 16);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    let frame = probe.last_frame_text();
    assert!(frame.contains("wave-16-ready"));
    assert!(!frame.contains("wave-16-pending"));
    assert!(app.current_projection().is_prepared());
}

#[tokio::test]
async fn seventeenth_preparation_wave_fails_without_handoff() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port_for_nested_waves();
    let mut app = Application::mount(
        {
            let runs = Arc::clone(&runs);
            move || wave_preparation_node(1, 17, Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    let fault = app.react().await.unwrap_err();

    assert_eq!(runs.load(Ordering::Acquire), 16);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
    assert_eq!(
        fault.reason(),
        ApplicationFaultReason::PreparationGraphUnstable
    );
}

#[tokio::test]
async fn continuity_retry_in_the_same_react_does_not_rerun_preparation() {
    let exposed = Arc::new(Mutex::new(None::<Signal<String>>));
    let runs = Arc::new(AtomicUsize::new(0));
    let change_signal = {
        let exposed = Arc::clone(&exposed);
        Arc::new(move || {
            exposed
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .set(String::from("continuity-retry"))
                .unwrap();
        }) as Arc<dyn Fn() + Send + Sync>
    };
    let (port, probe) = recording_port_with_continuity_rejection(Some(change_signal));
    let mut app = Application::mount(
        {
            let exposed = Arc::clone(&exposed);
            let runs = Arc::clone(&runs);
            move || signal_preparation(Arc::clone(&exposed), Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    assert_continue(app.react().await.unwrap());

    assert_eq!(runs.load(Ordering::Acquire), 1);
    assert_eq!(probe.submissions.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn prepare_resolves_bootstrap_preparations_without_provider_handoff() {
    let gate = Arc::new(PreparationGate::new());
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let gate = Arc::clone(&gate);
            move || preparation_view(Arc::clone(&gate))
        },
        port,
    )
    .unwrap();

    let bootstrap = app.current_projection();
    assert!(projection_text(bootstrap.projection()).contains("loading"));
    assert!(!bootstrap.is_prepared());

    let mut preparation = Box::pin(app.prepare());
    tokio::select! {
        _ = gate.started.notified() => {}
        result = &mut preparation => panic!("preparation did not await loader: {result:?}"),
    }
    assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    gate.release.notify_one();
    assert_continue(preparation.await.unwrap());

    assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    let prepared = app.current_projection();
    assert!(prepared.is_prepared());
    assert!(!prepared.is_dirty());
    assert!(projection_text(prepared.projection()).contains("ready"));
    assert!(!projection_text(prepared.projection()).contains("loading"));
}

#[tokio::test]
async fn failed_prepare_keeps_the_unprepared_application_retryable() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let attempts = Arc::clone(&attempts);
            move || failing_once_preparation(Arc::clone(&attempts))
        },
        port,
    )
    .unwrap();

    let fault = app.prepare().await.unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
    assert!(!app.current_projection().is_prepared());
    assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    assert_continue(app.prepare().await.unwrap());
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert!(app.current_projection().is_prepared());
    assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn dropping_pending_prepare_keeps_the_unprepared_application_retryable() {
    let gate = Arc::new(PreparationGate::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let gate = Arc::clone(&gate);
            let attempts = Arc::clone(&attempts);
            let drops = Arc::clone(&drops);
            move || {
                cancellable_preparation(
                    Arc::clone(&gate),
                    Arc::clone(&attempts),
                    Arc::clone(&drops),
                )
            }
        },
        port,
    )
    .unwrap();

    let mut preparation = Box::pin(app.prepare());
    tokio::select! {
        _ = gate.started.notified() => {}
        result = &mut preparation => panic!("preparation did not remain pending: {result:?}"),
    }
    drop(preparation);

    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert!(!app.current_projection().is_prepared());
    assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    gate.release.notify_one();
    assert_continue(app.prepare().await.unwrap());
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert!(app.current_projection().is_prepared());
    assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn signal_write_keeps_the_previous_prepared_checkpoint_visible_while_dirty() {
    let exposed = Arc::new(Mutex::new(None));
    let runs = Arc::new(AtomicUsize::new(0));
    let (port, probe) = recording_port();
    let mut app = Application::mount(
        {
            let exposed = Arc::clone(&exposed);
            let runs = Arc::clone(&runs);
            move || signal_preparation(Arc::clone(&exposed), Arc::clone(&runs))
        },
        port,
    )
    .unwrap();

    assert_continue(app.prepare().await.unwrap());
    let first = app.current_projection();
    assert!(first.is_prepared());
    assert!(!first.is_dirty());
    assert!(projection_text(first.projection()).contains("initial"));
    assert_eq!(runs.load(Ordering::Acquire), 1);

    exposed
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set(String::from("updated"))
        .unwrap();
    let dirty = app.current_projection();
    assert!(dirty.is_prepared());
    assert!(dirty.is_dirty());
    assert!(projection_text(dirty.projection()).contains("initial"));

    assert_continue(app.prepare().await.unwrap());
    let second = app.current_projection();
    assert!(second.is_prepared());
    assert!(!second.is_dirty());
    assert!(projection_text(second.projection()).contains("updated"));
    assert_eq!(runs.load(Ordering::Acquire), 2);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
}

#[test]
fn synchronous_preparation_factory_can_read_and_write_a_signal_without_deadlock() {
    let observed = Arc::new(AtomicUsize::new(0));
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn({
        let observed = Arc::clone(&observed);
        move || {
            let completed = catch_unwind(AssertUnwindSafe(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build preparation test runtime");
                runtime.block_on(async {
                    let (port, _) = recording_port();
                    let mut app = Application::mount(
                        {
                            let observed = Arc::clone(&observed);
                            move || synchronous_factory_signal_access(Arc::clone(&observed))
                        },
                        port,
                    )
                    .expect("mount factory access Component");
                    matches!(app.prepare().await, Ok(ControlFlow::Continue(())))
                        && app.current_projection().is_prepared()
                        && projection_text(app.current_projection().projection()).contains("2")
                })
            }))
            .unwrap_or(false);
            let _ = done_tx.send(completed);
        }
    });

    let completed = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("synchronous preparation factory Signal access deadlocked");
    assert!(completed, "synchronous preparation factory access failed");
    worker.join().expect("preparation factory worker panicked");
    assert_eq!(observed.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn panicking_preparation_factory_unwinds_without_provider_handoff() {
    for operation in [PreparationOperation::Prepare, PreparationOperation::React] {
        let (port, probe) = recording_port();
        let mut app = Application::mount(panicking_preparation_factory, port).unwrap();

        let panic = AssertUnwindSafe(operation.run(&mut app))
            .catch_unwind()
            .await
            .expect_err("preparation factory panic must unwind");

        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("preparation factory panic")
        );
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn later_factory_panic_drops_an_earlier_unpolled_same_wave_future() {
    for operation in [PreparationOperation::Prepare, PreparationOperation::React] {
        let sibling_polls = Arc::new(AtomicUsize::new(0));
        let sibling_drops = Arc::new(AtomicUsize::new(0));
        let (port, probe) = recording_port();
        let mut app = Application::mount(
            {
                let sibling_polls = Arc::clone(&sibling_polls);
                let sibling_drops = Arc::clone(&sibling_drops);
                move || {
                    pending_sibling_before_panicking_preparation_factory(
                        Arc::clone(&sibling_polls),
                        Arc::clone(&sibling_drops),
                    )
                }
            },
            port,
        )
        .unwrap();

        let panic = tokio::time::timeout(
            Duration::from_secs(1),
            AssertUnwindSafe(operation.run(&mut app)).catch_unwind(),
        )
        .await
        .expect("a later preparation factory panic was blocked by an earlier sibling")
        .expect_err("later preparation factory panic must unwind");

        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("later preparation factory panic")
        );
        assert_eq!(sibling_polls.load(Ordering::Acquire), 0);
        assert_eq!(sibling_drops.load(Ordering::Acquire), 1);
        assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    }
}
