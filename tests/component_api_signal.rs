use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Barrier, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{prelude::*, ComponentHost, SignalAccessError},
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};

#[derive(Clone)]
struct SignalProps {
    initial: String,
    label: String,
    initializations: Arc<AtomicUsize>,
    renders: Arc<AtomicUsize>,
    exposed: Arc<Mutex<Option<Signal<String>>>>,
    block_render: Arc<AtomicBool>,
    render_entered: Arc<Barrier>,
    release_render: Arc<Barrier>,
}

impl SignalProps {
    fn new(initial: &str, label: &str) -> Self {
        Self {
            initial: initial.to_owned(),
            label: label.to_owned(),
            initializations: Arc::new(AtomicUsize::new(0)),
            renders: Arc::new(AtomicUsize::new(0)),
            exposed: Arc::new(Mutex::new(None)),
            block_render: Arc::new(AtomicBool::new(false)),
            render_entered: Arc::new(Barrier::new(2)),
            release_render: Arc::new(Barrier::new(2)),
        }
    }

    fn with_values(&self, initial: &str, label: &str) -> Self {
        Self {
            initial: initial.to_owned(),
            label: label.to_owned(),
            ..self.clone()
        }
    }

    fn signal(&self) -> Signal<String> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component did not expose its Signal")
    }
}

#[component]
fn retained_signal_application(props: SignalProps) -> Component {
    let initializations = Arc::clone(&props.initializations);
    let initial = props.initial.clone();
    let state = use_signal(move || {
        initializations.fetch_add(1, Ordering::SeqCst);
        initial
    });
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());

    let value = state.with(Clone::clone).expect("mounted Signal read");
    let label = props.label.clone();
    props.renders.fetch_add(1, Ordering::SeqCst);
    if props.block_render.load(Ordering::SeqCst) {
        props.render_entered.wait();
        props.release_render.wait();
    }

    view! {
        retained_state {
            label { "{label}" }
            value { "{value}" }
        }
    }
}

fn rendered_projection_text(render: &agentview::component::PreparedRender) -> String {
    projection_text(render.projection())
}

fn projection_text(projection: &agentview::component::execution::RenderedProjection) -> String {
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
async fn spawned_signal_write_is_retained_until_the_next_explicit_render() {
    let props = SignalProps::new("A", "first");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());

    let first = host.render().expect("initial render");
    assert!(rendered_projection_text(&first).contains("<value>A</value>"));
    assert_eq!(props.initializations.load(Ordering::SeqCst), 1);
    assert_eq!(props.renders.load(Ordering::SeqCst), 1);

    let state = props.signal();
    tokio::spawn(async move { state.set("B".to_owned()) })
        .await
        .expect("signal task joined")
        .expect("signal write succeeded");

    assert_eq!(
        props.renders.load(Ordering::SeqCst),
        1,
        "Signal writes mark state dirty but must not render automatically"
    );

    let second = host.render().expect("explicit rerender");
    assert!(rendered_projection_text(&second).contains("<value>B</value>"));
    assert_eq!(props.initializations.load(Ordering::SeqCst), 1);
    assert_eq!(props.renders.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cloned_signal_updates_from_a_spawned_task() {
    let props = SignalProps::new("A", "clone");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");

    let signal = props.signal();
    tokio::spawn(async move { signal.set("B".to_owned()) })
        .await
        .expect("Signal task joined")
        .expect("Signal write succeeded");

    let rendered = host.render().expect("render after Signal write");
    assert!(rendered_projection_text(&rendered).contains("<value>B</value>"));
}

#[test]
fn replacing_props_preserves_signal_slots_for_the_same_mount() {
    let props = SignalProps::new("A", "first");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    props.signal().set("retained".to_owned()).unwrap();

    host.set_props(props.with_values("new initializer", "second"));
    assert_eq!(props.renders.load(Ordering::SeqCst), 1);

    let rendered = host.render().expect("render with replaced props");
    let text = rendered_projection_text(&rendered);
    assert!(text.contains("<label>second</label>"));
    assert!(text.contains("<value>retained</value>"));
    assert_eq!(props.initializations.load(Ordering::SeqCst), 1);
}

#[test]
fn current_projection_retains_the_latest_commit_while_state_is_dirty() {
    let props = SignalProps::new("A", "first");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());

    host.render().expect("initial render");
    assert!(!host.is_dirty());
    assert!(projection_text(host.current_projection().unwrap()).contains("<value>A</value>"));

    props.signal().set("B".to_owned()).unwrap();
    assert!(host.is_dirty());
    assert!(
        projection_text(host.current_projection().unwrap()).contains("<value>A</value>"),
        "a Signal write must leave the latest committed projection readable"
    );

    host.render().expect("render updated Signal state");
    assert!(!host.is_dirty());
    assert!(projection_text(host.current_projection().unwrap()).contains("<value>B</value>"));

    host.set_props(props.with_values("unused", "second"));
    assert!(host.is_dirty());
    assert!(
        projection_text(host.current_projection().unwrap()).contains("<value>B</value>"),
        "new props must leave the latest committed projection readable"
    );
}

#[test]
fn signal_with_callback_reentrant_render_fails_closed_without_deadlock() {
    let props = SignalProps::new("A", "reentrant-render");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    let signal = props.signal();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    let worker = std::thread::spawn(move || {
        let result = signal.with(|_| {
            entered_tx.send(()).unwrap();
            host.render().is_err()
        });
        done_tx
            .send((result, host.current_projection().is_some()))
            .unwrap();
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Signal::with callback was not scheduled");
    let (nested_render_failed, projection_remained_current) = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("same-host render deadlocked inside Signal::with");
    let nested_render_failed = nested_render_failed.expect("outer Signal read remains valid");
    assert!(nested_render_failed, "same-host render must fail closed");
    assert!(
        projection_remained_current,
        "a rejected read-only render must preserve the current projection"
    );
    worker.join().unwrap();
}

struct LockingCloneProps {
    clone_lock: Arc<Mutex<()>>,
    exposed: Arc<Mutex<Option<Signal<usize>>>>,
}

impl Clone for LockingCloneProps {
    fn clone(&self) -> Self {
        let _clone_guard = self.clone_lock.lock().expect("props clone lock");
        Self {
            clone_lock: Arc::clone(&self.clone_lock),
            exposed: Arc::clone(&self.exposed),
        }
    }
}

#[component]
fn locking_clone_application(props: LockingCloneProps) -> Component {
    let state = use_signal(|| 1usize);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(|value| *value).expect("mounted Signal read");
    view! { value { "{value}" } }
}

#[test]
fn reentrant_render_is_rejected_before_props_clone() {
    let props = LockingCloneProps {
        clone_lock: Arc::new(Mutex::new(())),
        exposed: Arc::new(Mutex::new(None)),
    };
    let mut host = ComponentHost::new_root(locking_clone_application, props.clone());
    let first_generation = host.render().expect("initial render").generation();
    let expected_next_generation = first_generation + 1;
    let signal = props
        .exposed
        .lock()
        .expect("signal exposure lock")
        .clone()
        .expect("component exposed its Signal");
    let clone_lock = Arc::clone(&props.clone_lock);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    let worker = std::thread::spawn(move || {
        let result = signal.with(|_| {
            let _application_guard = clone_lock.lock().expect("application lock");
            entered_tx.send(()).unwrap();
            host.render().is_err()
        });
        let next_generation = host.render().map(|render| render.generation());
        done_tx.send((result, next_generation)).unwrap();
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Signal::with callback was not scheduled");
    let (render_failed, next_generation) = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("same-host render reached Props::clone and deadlocked");
    let render_failed = render_failed.expect("outer Signal read remains valid");
    assert!(
        render_failed,
        "same-host render must fail before Props::clone"
    );
    assert_eq!(
        next_generation.expect("render after rejected reentry"),
        expected_next_generation,
        "a rejected render must not consume a render generation"
    );
    worker.join().unwrap();
}

#[test]
fn signal_update_callback_reentrant_remount_fails_closed_without_deadlock() {
    let props = SignalProps::new("A", "reentrant-remount");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    let signal = props.signal();
    let remounted_props = props.with_values("B", "must-not-remount");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    let worker = std::thread::spawn(move || {
        let result = signal.update(|value| {
            value.push_str("-updated");
            entered_tx.send(()).unwrap();
            host.remount(remounted_props).is_err()
        });
        let retained = signal.with(Clone::clone);
        done_tx
            .send((
                result,
                retained,
                host.current_projection().is_some(),
                host.is_dirty(),
            ))
            .unwrap();
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Signal::update callback was not scheduled");
    let (nested_remount_failed, retained, projection_retained, host_dirty) = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("same-host remount deadlocked inside Signal::update");
    let nested_remount_failed = nested_remount_failed.expect("outer Signal update remains valid");
    assert!(nested_remount_failed, "same-host remount must fail closed");
    assert_eq!(retained.unwrap(), "A-updated");
    assert!(
        projection_retained,
        "a rejected remount must preserve the latest committed projection"
    );
    assert!(host_dirty, "the successful outer update must remain dirty");
    worker.join().unwrap();
}

#[test]
fn dropping_host_inside_signal_callback_does_not_deadlock() {
    let props = SignalProps::new("A", "reentrant-drop");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    let signal = props.signal();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    let worker = std::thread::spawn(move || {
        let retained = signal.clone();
        let result = signal.with(move |_| {
            entered_tx.send(()).unwrap();
            drop(host);
        });
        done_tx.send((result, retained.with(Clone::clone))).unwrap();
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Signal::with callback was not scheduled");
    let (outer, after_drop) = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("same-host drop deadlocked inside a Signal callback");
    outer.expect("outer Signal read remains valid");
    assert_eq!(after_drop, Err(SignalAccessError::RuntimeInactive));
    worker.join().unwrap();
}

#[test]
fn panicking_signal_update_retains_projection_and_poisons_state() {
    let props = SignalProps::new("A", "panicking-update");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    assert!(host.current_projection().is_some());
    let signal = props.signal();
    let revision = host.wake_revision();

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = signal.update(|value| {
            value.push_str("-changed-before-panic");
            panic!("update callback panic");
        });
    }));

    assert!(
        panic.is_err(),
        "Signal::update must propagate callback panic"
    );
    assert!(
        host.is_dirty(),
        "panicking update must leave the host dirty"
    );
    assert_eq!(host.wake_revision(), revision + 1);
    assert!(
        projection_text(host.current_projection().unwrap()).contains("<value>A</value>"),
        "a panicking update must retain the latest successful projection"
    );
    assert!(matches!(
        signal.with(Clone::clone),
        Err(SignalAccessError::StatePoisoned { .. })
    ));
    assert!(matches!(
        signal.update(|value| value.push('!')),
        Err(SignalAccessError::StatePoisoned { .. })
    ));
}

#[test]
fn explicit_remount_invalidates_old_handles_and_initializes_new_slots() {
    let props = SignalProps::new("A", "first");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    let stale = props.signal();

    host.remount(props.with_values("C", "remounted"))
        .expect("explicit remount");
    assert!(host.current_projection().is_none());
    assert!(host.is_dirty());
    assert!(matches!(
        stale.with(Clone::clone),
        Err(SignalAccessError::Stale)
    ));
    assert!(matches!(
        stale.set("must not reach the new slot".to_owned()),
        Err(SignalAccessError::Stale)
    ));

    let rendered = host.render().expect("remounted render");
    let text = rendered_projection_text(&rendered);
    assert!(text.contains("<label>remounted</label>"));
    assert!(text.contains("<value>C</value>"));
    assert_eq!(props.initializations.load(Ordering::SeqCst), 2);
}

#[test]
fn signal_writes_wait_for_a_coherent_render_snapshot() {
    let props = SignalProps::new("A", "serialized");
    let mut host = ComponentHost::new_root(retained_signal_application, props.clone());
    host.render().expect("initial render");
    let state = props.signal();
    props.block_render.store(true, Ordering::SeqCst);

    let (start_write_tx, start_write_rx) = mpsc::channel();
    let (write_done_tx, write_done_rx) = mpsc::channel();
    let write_thread = std::thread::spawn(move || {
        start_write_rx.recv().unwrap();
        let result = state.set("B".to_owned());
        write_done_tx.send(result).unwrap();
    });

    let render_entered = Arc::clone(&props.render_entered);
    let release_render = Arc::clone(&props.release_render);
    let coordinator = std::thread::spawn(move || {
        render_entered.wait();
        start_write_tx.send(()).unwrap();
        let early_write = write_done_rx.recv_timeout(Duration::from_millis(100));
        let write_was_blocked = early_write.is_err();
        release_render.wait();
        let write_result = early_write.unwrap_or_else(|_| {
            write_done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("write completed after render")
        });
        (write_was_blocked, write_result)
    });

    let blocked_render = host.render().expect("blocked render");
    let blocked_projection = rendered_projection_text(&blocked_render);
    let (write_was_blocked, write_result) = coordinator.join().unwrap();
    assert!(
        write_was_blocked,
        "a write must not commit while render holds the snapshot gate"
    );
    write_result.expect("signal write succeeded");
    write_thread.join().unwrap();
    assert!(blocked_projection.contains("<value>A</value>"));

    props.block_render.store(false, Ordering::SeqCst);
    let next = host.render().expect("render after serialized write");
    assert!(rendered_projection_text(&next).contains("<value>B</value>"));
}

#[derive(Clone)]
struct DriftProps {
    add_hook: bool,
}

#[component]
fn drifting_application(props: DriftProps) -> Component {
    let stable = use_signal(|| String::from("stable"));
    if props.add_hook {
        let _candidate = use_signal(|| String::from("candidate"));
    }
    let rendered = stable.with(Clone::clone).unwrap();
    view! { stable { "{rendered}" } }
}

#[test]
fn hook_drift_panics_without_publishing_the_candidate_topology() {
    let mut host = ComponentHost::new_root(drifting_application, DriftProps { add_hook: false });
    host.render().expect("initial stable topology");
    let committed = host.current_projection().unwrap().clone();

    host.set_props(DriftProps { add_hook: true });
    let panic = catch_unwind(AssertUnwindSafe(|| host.render()));
    assert!(panic.is_err(), "hook-count drift must panic");
    assert!(host.is_dirty());
    assert_eq!(host.current_projection(), Some(&committed));
}

#[derive(Clone)]
struct OrderDriftProps {
    alternate: bool,
}

#[component]
fn same_type_order_drift(props: OrderDriftProps) -> Component {
    let (first, second) = if props.alternate {
        (use_signal(|| 10_u64), use_signal(|| 20_u64))
    } else {
        (use_signal(|| 1_u64), use_signal(|| 2_u64))
    };
    let rendered = (
        first.with(|value| *value).unwrap(),
        second.with(|value| *value).unwrap(),
    );
    view! { order { "{rendered:?}" } }
}

#[test]
fn same_typed_hook_order_drift_panics_without_publishing_the_candidate() {
    let mut host =
        ComponentHost::new_root(same_type_order_drift, OrderDriftProps { alternate: false });
    host.render().expect("initial hook order");
    let committed = host.current_projection().unwrap().clone();

    host.set_props(OrderDriftProps { alternate: true });
    let panic = catch_unwind(AssertUnwindSafe(|| host.render()));
    assert!(
        panic.is_err(),
        "changing same-typed hook callsites must panic"
    );
    assert!(host.is_dirty());
    assert_eq!(host.current_projection(), Some(&committed));
}

#[derive(Clone)]
struct NominalIdentityProps {
    render_first: bool,
    first: Arc<Mutex<Option<Signal<u64>>>>,
    second: Arc<Mutex<Option<Signal<u64>>>>,
}

struct FirstNominalComponent;

impl FirstNominalComponent {
    #[component]
    fn child(exposed: Arc<Mutex<Option<Signal<u64>>>>) -> Component {
        let state = use_signal(|| 1_u64);
        *exposed.lock().expect("first Signal exposure lock") = Some(state.clone());
        let value = state.with(|value| *value).unwrap();
        view! { first { "{value}" } }
    }
}

struct SecondNominalComponent;

impl SecondNominalComponent {
    #[component]
    fn child(exposed: Arc<Mutex<Option<Signal<u64>>>>) -> Component {
        let state = use_signal(|| 2_u64);
        *exposed.lock().expect("second Signal exposure lock") = Some(state.clone());
        let value = state.with(|value| *value).unwrap();
        view! { second { "{value}" } }
    }
}

#[component]
fn nominal_identity_application(props: NominalIdentityProps) -> Component {
    if props.render_first {
        FirstNominalComponent::child(props.first)
    } else {
        SecondNominalComponent::child(props.second)
    }
}

#[test]
fn distinct_nominal_components_cannot_share_signal_slots() {
    let first = Arc::new(Mutex::new(None));
    let second = Arc::new(Mutex::new(None));
    let initial = NominalIdentityProps {
        render_first: true,
        first: Arc::clone(&first),
        second: Arc::clone(&second),
    };
    let mut host = ComponentHost::new_root(nominal_identity_application, initial.clone());

    host.render().expect("first nominal Component renders");
    let stale = first
        .lock()
        .expect("first Signal exposure lock")
        .clone()
        .expect("first Component exposed its Signal");
    stale.set(99).unwrap();

    host.set_props(NominalIdentityProps {
        render_first: false,
        ..initial
    });
    host.render().expect("second nominal Component renders");
    let current = second
        .lock()
        .expect("second Signal exposure lock")
        .clone()
        .expect("second Component exposed its Signal");

    assert_eq!(current.with(|value| *value).unwrap(), 2);
    assert_eq!(stale.set(100), Err(SignalAccessError::Stale));
}
