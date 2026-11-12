use std::{
    future::pending,
    num::{NonZeroU128, NonZeroU64},
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

use agentview::component::{
    execution::{
        Application, ApplicationFaultKind, ContextWindow, DefinitionUpdateErrorKind, Frame,
        FrameCapabilities, FrameConstraints, FrameProfile, ProviderFact, ProviderFactStream,
        ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch,
        TargetIdentity,
    },
    prelude::*,
};
use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::Notify;

struct OfflinePort {
    declaration: TargetDeclaration,
    submissions: Arc<AtomicUsize>,
}

impl OfflinePort {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let submissions = Arc::new(AtomicUsize::new(0));
        let declaration = TargetDeclaration::full(
            TargetIdentity::new(NonZeroU128::new(719_880).unwrap()),
            TargetEpoch::new(NonZeroU64::MIN),
            FrameProfile::new(
                FrameConstraints {
                    max_frame_bytes: 65_536,
                    max_component_bytes: 16_384,
                    context_window: ContextWindow::NotApplicable,
                    reserved_output_tokens: None,
                },
                FrameCapabilities::NONE,
            ),
        );
        (
            Self {
                declaration,
                submissions: submissions.clone(),
            },
            submissions,
        )
    }
}

#[async_trait]
impl ReactionPort for OfflinePort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.submissions.fetch_add(1, Ordering::SeqCst);
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        Ok(Box::pin(futures::stream::iter([Ok(
            ProviderFact::ReactionCompleted { primary_text: None },
        )])))
    }
}

#[derive(Default)]
struct DestructorGate {
    entered: Notify,
    released: Mutex<bool>,
    proceed: Condvar,
}

impl DestructorGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.proceed.notify_all();
    }

    fn enter(&self) {
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.proceed.wait(released).unwrap();
        }
    }
}

// Release the intentionally blocked destructor even when a test assertion
// unwinds, so the Tokio runtime can still shut down.
struct ReleaseOnDrop(Arc<DestructorGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Clone, Default)]
struct TaskProbe {
    state: Arc<Mutex<Option<Signal<u64>>>>,
    started: Arc<Notify>,
    starts: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    renders: Arc<AtomicUsize>,
    gate: Option<Arc<DestructorGate>>,
}

struct TaskDestructor {
    drops: Arc<AtomicUsize>,
    gate: Option<Arc<DestructorGate>>,
}

impl Drop for TaskDestructor {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.enter();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn task_definition(probe: TaskProbe) -> ComponentDefinition {
    ComponentDefinition::new("tracked-root", "v1", move |hooks| {
        probe.renders.fetch_add(1, Ordering::SeqCst);
        let state = hooks.use_signal_at(0, || 4_u64)?;
        *probe.state.lock().unwrap() = Some(state.clone());
        let task_probe = probe.clone();
        hooks.use_future_at(1, move || async move {
            let _destructor = TaskDestructor {
                drops: task_probe.drops,
                gate: task_probe.gate,
            };
            task_probe.starts.fetch_add(1, Ordering::SeqCst);
            task_probe.started.notify_one();
            pending::<()>().await;
        })?;
        Ok(Component::text(state.with(ToString::to_string).unwrap()))
    })
    .unwrap()
}

fn replacement() -> ComponentDefinition {
    ComponentDefinition::new("replacement-root", "v1", |_| {
        Ok(Component::text("replacement"))
    })
    .unwrap()
}

async fn notified(notification: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), notification.notified())
        .await
        .expect("lifecycle notification must arrive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn cancelling_update_waiter_keeps_retirement_and_publication_owned() {
    let gate = Arc::new(DestructorGate::default());
    let _release_on_failure = ReleaseOnDrop(gate.clone());
    let probe = TaskProbe {
        gate: Some(gate.clone()),
        ..TaskProbe::default()
    };
    let (port, submissions) = OfflinePort::new();
    let mut app = Application::mount_definition(task_definition(probe.clone()), port).unwrap();
    notified(&probe.started).await;
    let previous_projection = app.current_projection().revision();
    let old_state = probe.state.lock().unwrap().clone().unwrap();

    let mut update = Box::pin(app.update_definition(replacement()));
    tokio::select! {
        _ = notified(&gate.entered) => {}
        outcome = &mut update => panic!("update acknowledged before task destruction: {outcome:?}"),
    }
    drop(update);

    assert_eq!(app.current_projection().revision(), previous_projection);
    assert_eq!(probe.drops.load(Ordering::SeqCst), 0);
    assert!(matches!(old_state.set(7), Err(SignalAccessError::Stale)));
    assert!(
        app.react(false).is_err(),
        "retirement still owns the boundary"
    );
    assert_eq!(
        app.update_definition(replacement())
            .await
            .unwrap_err()
            .kind(),
        DefinitionUpdateErrorKind::Busy
    );

    gate.release();
    assert!(tokio::time::timeout(Duration::from_secs(3), app.prepare())
        .await
        .expect("owned update must finish after destructor release")
        .unwrap()
        .is_continue());
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
    assert_eq!(app.definition_version().get(), 2);
    assert!(app.current_projection().revision() > previous_projection);
    assert_eq!(submissions.load(Ordering::SeqCst), 0);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_candidate_keeps_old_task_and_discards_new_task_factory() {
    let probe = TaskProbe::default();
    let (port, submissions) = OfflinePort::new();
    let mut app = Application::mount_definition(task_definition(probe.clone()), port).unwrap();
    notified(&probe.started).await;
    let old_state = probe.state.lock().unwrap().clone().unwrap();
    let revision = app.current_projection().revision();
    let attempted_starts = Arc::new(AtomicUsize::new(0));
    let starts = attempted_starts.clone();
    let candidate = ComponentDefinition::new("replacement-root", "v1", move |hooks| {
        let starts = starts.clone();
        hooks.use_future_at(0, move || {
            starts.fetch_add(1, Ordering::SeqCst);
            async {}
        })?;
        Err(ComponentDefinitionError::new(
            "external validation rejected candidate",
        ))
    })
    .unwrap();

    let error = app.update_definition(candidate).await.unwrap_err();
    assert_eq!(error.kind(), DefinitionUpdateErrorKind::Rejected);
    assert_eq!(error.effective_version().get(), 1);
    assert_eq!(app.definition_version().get(), 1);
    assert_eq!(app.current_projection().revision(), revision);
    assert_eq!(attempted_starts.load(Ordering::SeqCst), 0);
    assert_eq!(probe.drops.load(Ordering::SeqCst), 0);
    old_state.set(17).unwrap();
    assert!(app.prepare().await.unwrap().is_continue());
    assert_eq!(probe.renders.load(Ordering::SeqCst), 2);
    assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
    assert_eq!(old_state.with(|value| *value).unwrap(), 17);
    assert_eq!(submissions.load(Ordering::SeqCst), 0);
    app.shutdown().await.unwrap();
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
}

#[derive(Debug, PartialEq, Eq)]
struct OriginalPanic(u64);

#[tokio::test]
async fn definition_renderer_panic_preserves_payload_and_terminalizes_application() {
    let probe = TaskProbe::default();
    let (port, submissions) = OfflinePort::new();
    let mut app = Application::mount_definition(task_definition(probe.clone()), port).unwrap();
    notified(&probe.started).await;
    let candidate = ComponentDefinition::new("replacement-root", "v1", |_| {
        std::panic::panic_any(OriginalPanic(812))
    })
    .unwrap();

    let payload = AssertUnwindSafe(app.update_definition(candidate))
        .catch_unwind()
        .await
        .expect_err("original renderer panic must reach the update caller");
    assert_eq!(
        payload.downcast_ref::<OriginalPanic>(),
        Some(&OriginalPanic(812))
    );
    assert_eq!(app.definition_version().get(), 1);
    assert_eq!(
        app.react(false).unwrap_err().kind(),
        ApplicationFaultKind::Terminal
    );
    let error = app.update_definition(replacement()).await.unwrap_err();
    assert_eq!(error.kind(), DefinitionUpdateErrorKind::Runtime);
    assert_eq!(error.effective_version().get(), 1);
    assert_eq!(submissions.load(Ordering::SeqCst), 0);
    assert!(app.shutdown().await.is_err());
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
}
