use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

use super::*;

fn component(name: &str, position: usize) -> ComponentId {
    ComponentId::root().child(name, position)
}

fn render_counter(
    transaction: &mut SignalRenderTransaction<'_>,
    identity: ComponentId,
    initializations: Arc<AtomicUsize>,
) -> Result<Signal<usize>, SignalRenderError> {
    transaction.render_component(identity, |scope| {
        scope.use_signal(|| {
            initializations.fetch_add(1, Ordering::SeqCst);
            1usize
        })
    })
}

#[test]
fn signal_is_retained_and_initializer_runs_once() {
    let runtime = SignalRuntime::new();
    let identity = component("counter", 0);
    let initializations = Arc::new(AtomicUsize::new(0));

    let mut first_render = runtime.begin_render().unwrap();
    let first = render_counter(
        &mut first_render,
        identity.clone(),
        Arc::clone(&initializations),
    )
    .unwrap();
    assert_eq!(first.with(|value| *value).unwrap(), 1);
    first_render.commit();

    first.set(9).unwrap();
    let mut second_render = runtime.begin_render().unwrap();
    let second =
        render_counter(&mut second_render, identity, Arc::clone(&initializations)).unwrap();
    assert_eq!(second.with(|value| *value).unwrap(), 9);
    second_render.commit();

    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    assert_eq!(first.with(|value| *value).unwrap(), 9);
}

#[test]
fn abandoned_render_preserves_committed_shape_and_stales_new_handles() {
    let runtime = SignalRuntime::new();
    let committed_id = component("committed", 0);
    let abandoned_id = component("abandoned", 1);

    let mut initial = runtime.begin_render().unwrap();
    let committed = initial
        .render_component(committed_id.clone(), |scope| scope.use_signal(|| 3usize))
        .unwrap();
    initial.commit();

    let mut failed = runtime.begin_render().unwrap();
    let abandoned = failed
        .render_component(abandoned_id, |scope| scope.use_signal(|| 8usize))
        .unwrap();
    drop(failed);

    assert_eq!(committed.with(|value| *value).unwrap(), 3);
    assert!(matches!(
        abandoned.with(|value| *value),
        Err(SignalAccessError::Stale)
    ));
    assert_eq!(runtime.mounted_components(), 1);
}

#[test]
fn explicit_remount_invalidates_old_handles_and_reinitializes() {
    let runtime = SignalRuntime::new();
    let identity = component("keyed", 0);
    let initializations = Arc::new(AtomicUsize::new(0));

    let mut first_render = runtime.begin_render().unwrap();
    let old = render_counter(
        &mut first_render,
        identity.clone(),
        Arc::clone(&initializations),
    )
    .unwrap();
    first_render.commit();
    old.set(12).unwrap();

    assert!(runtime.remount_component(&identity).unwrap());
    assert!(matches!(
        old.with(|value| *value),
        Err(SignalAccessError::Stale)
    ));

    let mut second_render = runtime.begin_render().unwrap();
    let fresh = render_counter(&mut second_render, identity, Arc::clone(&initializations)).unwrap();
    second_render.commit();
    assert_eq!(fresh.with(|value| *value).unwrap(), 1);
    assert_eq!(initializations.load(Ordering::SeqCst), 2);
}

#[test]
fn whole_host_invalidation_stales_every_mounted_handle() {
    let runtime = SignalRuntime::new();
    let mut initial = runtime.begin_render().unwrap();
    let first = initial
        .render_component(component("first", 0), |scope| scope.use_signal(|| 1usize))
        .unwrap();
    let second = initial
        .render_component(component("second", 1), |scope| scope.use_signal(|| 2usize))
        .unwrap();
    initial.commit();

    runtime.invalidate_all().unwrap();
    assert!(matches!(
        first.with(|value| *value),
        Err(SignalAccessError::Stale)
    ));
    assert!(matches!(
        second.with(|value| *value),
        Err(SignalAccessError::Stale)
    ));
    assert_eq!(runtime.mounted_components(), 0);
    assert!(runtime.is_dirty());
}

#[test]
fn removing_then_recreating_a_component_fences_its_old_signal_generation() {
    let runtime = SignalRuntime::new();
    let identity = component("conditional-child", 0);

    let mut mounted = runtime.begin_render().unwrap();
    let old = mounted
        .render_component(identity.clone(), |scope| scope.use_signal(|| 1usize))
        .unwrap();
    mounted.commit();

    runtime.begin_render().unwrap().commit();
    assert_eq!(old.set(2), Err(SignalAccessError::Stale));

    let mut recreated = runtime.begin_render().unwrap();
    let fresh = recreated
        .render_component(identity, |scope| scope.use_signal(|| 3usize))
        .unwrap();
    recreated.commit();

    assert_eq!(old.set(4), Err(SignalAccessError::Stale));
    assert_eq!(fresh.with(|value| *value).unwrap(), 3);
}

fn render_conditional(
    transaction: &mut SignalRenderTransaction<'_>,
    identity: ComponentId,
    second: bool,
) -> Result<(), SignalRenderError> {
    transaction.render_component(identity, |scope| {
        let _ = scope.use_signal(|| 0usize)?;
        if second {
            let _ = scope.use_signal(|| false)?;
        }
        Ok(())
    })
}

#[test]
fn hook_count_drift_fails_without_poisoning_committed_state() {
    let runtime = SignalRuntime::new();
    let identity = component("conditional", 0);
    let mut initial = runtime.begin_render().unwrap();
    render_conditional(&mut initial, identity.clone(), true).unwrap();
    initial.commit();

    let mut drifted = runtime.begin_render().unwrap();
    assert!(matches!(
        render_conditional(&mut drifted, identity.clone(), false),
        Err(SignalRenderError::HookCountMismatch {
            expected: 2,
            observed: 1,
            ..
        })
    ));
    drop(drifted);

    let mut retry = runtime.begin_render().unwrap();
    render_conditional(&mut retry, identity, true).unwrap();
    retry.commit();
}

fn render_usize(
    transaction: &mut SignalRenderTransaction<'_>,
    identity: ComponentId,
) -> Result<(), SignalRenderError> {
    transaction.render_component(identity, |scope| {
        let _ = scope.use_signal(|| 0usize)?;
        Ok(())
    })
}

fn render_string(
    transaction: &mut SignalRenderTransaction<'_>,
    identity: ComponentId,
) -> Result<(), SignalRenderError> {
    transaction.render_component(identity, |scope| {
        let _ = scope.use_signal(String::new)?;
        Ok(())
    })
}

#[test]
fn hook_type_drift_fails_closed() {
    let runtime = SignalRuntime::new();
    let identity = component("typed", 0);
    let mut initial = runtime.begin_render().unwrap();
    render_usize(&mut initial, identity.clone()).unwrap();
    initial.commit();

    let mut drifted = runtime.begin_render().unwrap();
    assert!(matches!(
        render_string(&mut drifted, identity),
        Err(SignalRenderError::HookTypeMismatch { slot: 0, .. })
    ));
}

fn render_alternate_site(
    transaction: &mut SignalRenderTransaction<'_>,
    identity: ComponentId,
    alternate: bool,
) -> Result<(), SignalRenderError> {
    transaction.render_component(identity, |scope| {
        if alternate {
            let _ = scope.use_signal(|| 0usize)?;
        } else {
            let _ = scope.use_signal(|| 0usize)?;
        }
        Ok(())
    })
}

#[test]
fn hook_location_drift_fails_closed() {
    let runtime = SignalRuntime::new();
    let identity = component("located", 0);
    let mut initial = runtime.begin_render().unwrap();
    render_alternate_site(&mut initial, identity.clone(), false).unwrap();
    initial.commit();

    let mut drifted = runtime.begin_render().unwrap();
    assert!(matches!(
        render_alternate_site(&mut drifted, identity, true),
        Err(SignalRenderError::HookLocationMismatch { slot: 0, .. })
    ));
}

#[test]
fn signal_is_send_and_sync_when_value_is() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Signal<String>>();
}

#[tokio::test]
async fn successful_write_marks_dirty_and_wakes_revision_waiter() {
    let runtime = SignalRuntime::new();
    let identity = component("wake", 0);
    let mut render = runtime.begin_render().unwrap();
    let signal = render
        .render_component(identity.clone(), |scope| scope.use_signal(|| 1usize))
        .unwrap();
    render.commit();
    assert!(!runtime.is_dirty());
    assert!(!runtime.is_component_dirty(&identity));

    let revision = runtime.wake_revision();
    signal.update(|value| *value += 1).unwrap();
    assert!(runtime.is_dirty());
    assert!(runtime.is_component_dirty(&identity));
    assert_eq!(runtime.wait_for_wake_after(revision).await, revision + 1);
    assert!(runtime.take_dirty());
    assert!(!runtime.take_dirty());
}

#[test]
fn render_owner_can_read_but_cannot_write() {
    let runtime = SignalRuntime::new();
    let identity = component("render-access", 0);
    let mut render = runtime.begin_render().unwrap();
    let signal = render
        .render_component(identity, |scope| scope.use_signal(|| 5usize))
        .unwrap();
    assert_eq!(signal.with(|value| *value).unwrap(), 5);
    assert!(matches!(
        signal.set(6),
        Err(SignalAccessError::WriteDuringRender { .. })
    ));
    render.commit();
    assert_eq!(signal.with(|value| *value).unwrap(), 5);
}

#[test]
fn external_write_waits_for_render_gate() {
    let runtime = SignalRuntime::new();
    let identity = component("gate", 0);
    let initializations = Arc::new(AtomicUsize::new(0));
    let mut initial = runtime.begin_render().unwrap();
    let signal =
        render_counter(&mut initial, identity.clone(), Arc::clone(&initializations)).unwrap();
    initial.commit();

    let mut render = runtime.begin_render().unwrap();
    let _ = render_counter(&mut render, identity, initializations).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        signal.set(7).unwrap();
        finished_tx.send(()).unwrap();
    });

    started_rx.recv().unwrap();
    assert!(finished_rx.recv_timeout(Duration::from_millis(40)).is_err());
    render.commit();
    finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    writer.join().unwrap();
}

#[test]
fn nested_same_signal_write_completes_or_fails_closed() {
    let runtime = Arc::new(SignalRuntime::new());
    let identity = component("nested-same-slot", 0);
    let mut initial = runtime.begin_render().unwrap();
    let signal = initial
        .render_component(identity, |scope| scope.use_signal(|| 1usize))
        .unwrap();
    initial.commit();
    let revision = runtime.wake_revision();

    let (callback_entered_tx, callback_entered_rx) = mpsc::channel();
    let (nested_done_tx, nested_done_rx) = mpsc::channel();
    let callback_signal = signal.clone();
    let callback_runtime = Arc::clone(&runtime);
    let callback_thread = std::thread::spawn(move || {
        let _runtime_lifetime_guard = callback_runtime;
        let nested = callback_signal.with(|_| {
            callback_entered_tx.send(()).unwrap();
            callback_signal.set(2)
        });
        nested_done_tx.send(nested).unwrap();
    });

    callback_entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Signal callback entered");
    let nested = nested_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("same-slot nested Signal write deadlocked");
    match nested {
        Ok(Ok(())) => {
            assert_eq!(signal.with(|value| *value).unwrap(), 2);
            assert_eq!(runtime.wake_revision(), revision + 1);
        }
        Ok(Err(SignalAccessError::ReentrantAccess { .. })) => {
            assert_eq!(signal.with(|value| *value).unwrap(), 1);
            assert_eq!(runtime.wake_revision(), revision);
        }
        Ok(Err(error)) => panic!("nested Signal write returned an unrelated error: {error}"),
        Err(error) => panic!("outer Signal access unexpectedly failed: {error}"),
    }
    callback_thread.join().unwrap();
}

#[test]
fn nested_cross_signal_read_completes_while_a_render_is_queued() {
    let runtime = Arc::new(SignalRuntime::new());
    let identity = component("nested-cross-slot", 0);
    let mut initial = runtime.begin_render().unwrap();
    let (outer, inner) = initial
        .render_component(identity, |scope| {
            let outer = scope.use_signal(|| 1usize)?;
            let inner = scope.use_signal(|| 2usize)?;
            Ok((outer, inner))
        })
        .unwrap();
    initial.commit();

    let (callback_entered_tx, callback_entered_rx) = mpsc::channel();
    let (continue_callback_tx, continue_callback_rx) = mpsc::channel();
    let (nested_attempted_tx, nested_attempted_rx) = mpsc::channel();
    let (nested_done_tx, nested_done_rx) = mpsc::channel();
    let callback_runtime = Arc::clone(&runtime);
    let callback_thread = std::thread::spawn(move || {
        let _runtime_lifetime_guard = callback_runtime;
        let nested = outer.with(|_| {
            callback_entered_tx.send(()).unwrap();
            continue_callback_rx.recv().unwrap();
            nested_attempted_tx.send(()).unwrap();
            inner.with(|value| *value)
        });
        nested_done_tx.send(nested).unwrap();
    });

    callback_entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Signal callback entered");

    let (release_gate_tx, release_gate_rx) = mpsc::channel();
    let (gate_held_tx, gate_held_rx) = mpsc::channel();
    let gate_runtime = Arc::clone(&runtime);
    let gate_thread = std::thread::spawn(move || {
        let _gate = gate_runtime.core.read_gate();
        gate_held_tx.send(()).unwrap();
        release_gate_rx.recv().unwrap();
    });
    gate_held_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("independent Signal access held the render gate");

    let (render_done_tx, render_done_rx) = mpsc::channel();
    let render_runtime = Arc::clone(&runtime);
    let render_thread = std::thread::spawn(move || {
        let result = render_runtime.begin_render().map(drop);
        render_done_tx.send(result).unwrap();
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    while runtime.waiting_renderers() == 0 {
        assert!(
            Instant::now() < deadline,
            "render did not reach the write gate"
        );
        std::thread::yield_now();
    }
    assert!(
        render_done_rx
            .recv_timeout(Duration::from_millis(40))
            .is_err(),
        "render must be queued behind active Signal access"
    );

    continue_callback_tx.send(()).unwrap();
    nested_attempted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("callback attempted nested Signal access");
    release_gate_tx.send(()).unwrap();
    gate_thread.join().unwrap();

    let nested = nested_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("nested Signal access deadlocked behind the queued render");
    match nested {
        Ok(Ok(2)) | Ok(Err(SignalAccessError::ReentrantAccess { .. })) => {}
        Ok(Ok(value)) => panic!("nested Signal read returned {value}, expected 2"),
        Ok(Err(error)) => panic!("nested Signal read returned an unrelated error: {error}"),
        Err(error) => panic!("outer Signal access unexpectedly failed: {error}"),
    }

    render_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("queued render completed after the callback")
        .unwrap();
    callback_thread.join().unwrap();
    render_thread.join().unwrap();
}

#[test]
fn stale_write_from_a_new_render_owner_fails_as_stale() {
    let runtime = SignalRuntime::new();
    let identity = component("stale-render-owner", 0);
    let mut initial = runtime.begin_render().unwrap();
    let stale = initial
        .render_component(identity.clone(), |scope| scope.use_signal(|| 1usize))
        .unwrap();
    initial.commit();

    assert!(runtime.remount_component(&identity).unwrap());
    let mut remount = runtime.begin_render().unwrap();
    remount
        .render_component(identity, |scope| {
            let _fresh = scope.use_signal(|| 2usize)?;
            assert_eq!(stale.set(3), Err(SignalAccessError::Stale));
            Ok(())
        })
        .unwrap();
    remount.commit();
}

#[test]
fn with_can_return_a_borrow_from_the_callback_environment() {
    let runtime = SignalRuntime::new();
    let mut render = runtime.begin_render().unwrap();
    let signal = render
        .render_component(component("external-borrow", 0), |scope| {
            scope.use_signal(|| 1usize)
        })
        .unwrap();
    render.commit();

    let external = String::from("borrowed outside Signal state");
    let borrowed = signal.with(|_| external.as_str()).unwrap();
    assert_eq!(borrowed, "borrowed outside Signal state");
}
