use std::{
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFault, ApplicationFaultKind, ApplicationFaultReason,
            ApplicationFaultStage, Frame, FrameCapabilities, FrameConstraints, FrameProfile,
            ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault, RenderedProjection,
            SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::Notify;

struct PreparationGate {
    started: Notify,
    release: Notify,
}

impl PreparationGate {
    fn new() -> Self {
        Self {
            started: Notify::new(),
            release: Notify::new(),
        }
    }
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
}

#[async_trait]
impl ReactionPort for RecordingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.probe.declarations.fetch_add(1, Ordering::Release);
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.probe.submissions.fetch_add(1, Ordering::Release);
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
        Ok(Box::pin(futures::stream::once(async {
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
        },
        probe,
    )
}

#[derive(Clone, Copy)]
enum PreparationOperation {
    Prepare,
    React,
}

impl PreparationOperation {
    async fn run(self, app: &mut Application<RecordingPort>) -> Result<(), ApplicationFault> {
        match self {
            Self::Prepare => app.prepare().await,
            Self::React => app.react().await,
        }
    }
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
    reaction.await.unwrap();

    assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    assert!(probe.last_frame_text().contains("ready"));
    assert!(!probe.last_frame_text().contains("loading"));
    assert!(app.current_projection().is_prepared());
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

    app.react().await.unwrap();
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

    app.react().await.unwrap();
    app.react().await.unwrap();

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

    app.prepare().await.unwrap();
    assert_eq!(runs.load(Ordering::Acquire), 1);
    assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

    app.react().await.unwrap();
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
    app.react().await.unwrap();
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

    app.react().await.unwrap();

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

    app.prepare().await.unwrap();
    assert_eq!(parent_runs.load(Ordering::Acquire), 1);
    assert_eq!(child_runs.load(Ordering::Acquire), 1);
    assert!(
        !projection_text(app.current_projection().projection()).contains("remounting_child_absent")
    );

    app.prepare().await.unwrap();
    assert_eq!(parent_runs.load(Ordering::Acquire), 2);
    assert_eq!(child_runs.load(Ordering::Acquire), 2);
    assert!(
        projection_text(app.current_projection().projection()).contains("remounting_child_absent")
    );

    app.prepare().await.unwrap();
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

    app.react().await.unwrap();

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

    app.react().await.unwrap();

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

    app.react().await.unwrap();

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
    preparation.await.unwrap();

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

    app.prepare().await.unwrap();
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
    app.prepare().await.unwrap();
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

    app.prepare().await.unwrap();
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

    app.prepare().await.unwrap();
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
                    app.prepare().await.is_ok()
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
