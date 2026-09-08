use std::{
    any::Any,
    collections::HashMap,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use agentview::component::{
    execution::{
        Application, ExternalAct, ExternalControl, ExternalControlFault, ExternalObservation,
        ExternalProviderPort, FrameBasis,
    },
    prelude::*,
};
use anyhow::{ensure, Context, Result};
use futures::FutureExt;

#[cfg(test)]
#[path = "support/frame_workflow_golden.rs"]
mod frame_workflow_golden;

struct TaskDropProbe(Arc<AtomicBool>);

impl Drop for TaskDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct PluginProps {
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
}

#[component]
fn frame_plugin_component(props: PluginProps) -> Component {
    let task_started = Arc::clone(&props.task_started);
    let task_dropped = Arc::clone(&props.task_dropped);
    use_future(move || async move {
        let _drop_probe = TaskDropProbe(task_dropped);
        task_started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
    });
    view! { frame_plugin { "same parent-independent root input" } }
}

struct PluginParent {
    application: Application<ExternalProviderPort>,
    control: ExternalControl,
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
    shutdown_started: Arc<AtomicBool>,
}

fn plugin_parent() -> Result<PluginParent> {
    let task_started = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::new(AtomicBool::new(false));
    let shutdown_started = Arc::new(AtomicBool::new(false));
    let props = PluginProps {
        task_started: Arc::clone(&task_started),
        task_dropped: Arc::clone(&task_dropped),
    };
    let (port, control) = ExternalProviderPort::new()?;
    let application = Application::mount(move || frame_plugin_component(props.clone()), port)?;
    Ok(PluginParent {
        application,
        control,
        task_started,
        task_dropped,
        shutdown_started,
    })
}

enum PluginCompletion<'a> {
    Text(&'a str),
    Empty,
}

async fn exchange(
    application: &mut Application<ExternalProviderPort>,
    control: &ExternalControl,
    completion: PluginCompletion<'_>,
) -> Result<ExternalObservation> {
    let reaction = application.react();
    tokio::pin!(reaction);
    let observation = tokio::select! {
        observation = control.next_observation() => observation?,
        result = &mut reaction => {
            return Err(anyhow::anyhow!("Plugin reaction ended before handoff: {result:?}"));
        }
    };
    match completion {
        PluginCompletion::Text(text) => {
            control
                .act(observation.ingress_generation(), ExternalAct::text(text))
                .await?;
        }
        PluginCompletion::Empty => {
            control.complete(observation.ingress_generation()).await?;
        }
    }
    ensure!(reaction.await?.is_continue(), "Plugin application exited");
    Ok(observation)
}

async fn wait_for_parent_tasks(registry: &HashMap<&str, PluginParent>) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if registry
                .values()
                .all(|parent| parent.task_started.load(Ordering::Acquire))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("Plugin Component tasks did not start")?;
    Ok(())
}

type PanicPayload = Box<dyn Any + Send + 'static>;

struct CleanupOutcome {
    attempted: usize,
    completed: usize,
    first_error: Option<anyhow::Error>,
    first_panic: Option<PanicPayload>,
}

async fn drain_all<Owner, Owners, Shutdown, ShutdownFuture, ShutdownError>(
    owners: Owners,
    mut shutdown: Shutdown,
) -> CleanupOutcome
where
    Owners: IntoIterator<Item = Owner>,
    Shutdown: FnMut(Owner) -> ShutdownFuture,
    ShutdownFuture: Future<Output = std::result::Result<(), ShutdownError>>,
    ShutdownError: Into<anyhow::Error>,
{
    let mut outcome = CleanupOutcome {
        attempted: 0,
        completed: 0,
        first_error: None,
        first_panic: None,
    };
    for owner in owners {
        outcome.attempted += 1;
        let result = AssertUnwindSafe(async { shutdown(owner).await })
            .catch_unwind()
            .await;
        match result {
            Ok(Ok(())) => outcome.completed += 1,
            Ok(Err(error)) if outcome.first_error.is_none() => {
                outcome.first_error = Some(error.into());
            }
            Ok(Err(_)) => {}
            Err(panic) if outcome.first_panic.is_none() => {
                outcome.first_panic = Some(panic);
            }
            Err(_) => {}
        }
    }
    outcome
}

async fn shutdown_parent(parent: PluginParent) -> Result<()> {
    let PluginParent {
        application,
        control,
        task_started: _,
        task_dropped: _,
        shutdown_started,
    } = parent;
    shutdown_started.store(true, Ordering::Release);
    let result = application.shutdown().await.map_err(anyhow::Error::from);
    drop(control);
    result
}

async fn shutdown_registry(registry: HashMap<&'static str, PluginParent>) -> CleanupOutcome {
    drain_all(registry.into_values(), shutdown_parent).await
}

async fn build_plugin_registry_with<Create>(
    mut create: Create,
) -> Result<HashMap<&'static str, PluginParent>>
where
    Create: FnMut() -> Result<PluginParent>,
{
    let mut registry = HashMap::new();
    for parent_id in ["parent-a", "parent-b"] {
        match catch_unwind(AssertUnwindSafe(&mut create)) {
            Ok(Ok(parent)) => {
                registry.insert(parent_id, parent);
            }
            Ok(Err(error)) => {
                let mut cleanup = shutdown_registry(registry).await;
                if let Some(panic) = cleanup.first_panic.take() {
                    resume_unwind(panic);
                }
                return match cleanup.first_error.take() {
                    Some(cleanup_error) => Err(error.context(format!(
                        "partial Plugin registry cleanup also failed: {cleanup_error}"
                    ))),
                    None => Err(error),
                };
            }
            Err(primary_panic) => {
                let _cleanup = shutdown_registry(registry).await;
                resume_unwind(primary_panic);
            }
        }
    }
    Ok(registry)
}

fn resolve_after_cleanup<T>(
    operation: std::thread::Result<Result<T>>,
    mut cleanup: CleanupOutcome,
) -> Result<T> {
    let operation = match operation {
        Ok(operation) => operation,
        Err(primary_panic) => resume_unwind(primary_panic),
    };
    if let Some(cleanup_panic) = cleanup.first_panic.take() {
        resume_unwind(cleanup_panic);
    }
    match (operation, cleanup.first_error.take()) {
        (Ok(value), None) => Ok(value),
        (Err(operation), None) => Err(operation),
        (Ok(_), Some(cleanup)) => Err(cleanup),
        (Err(operation), Some(cleanup)) => {
            Err(operation.context(format!("Plugin cleanup also failed: {cleanup}")))
        }
    }
}

#[derive(Debug)]
struct PluginEvidence {
    parent_count: usize,
    distinct_targets: bool,
    first_payloads_equal: bool,
    old_text_stale: bool,
    old_complete_stale: bool,
    second_same_parent_is_delta: bool,
    second_same_parent_kept_target: bool,
    second_frame_saw_first_text: bool,
    other_parent_remained_idle: bool,
    shutdown_count: usize,
    all_tasks_dropped: bool,
}

async fn run_plugin() -> Result<PluginEvidence> {
    let mut registry = build_plugin_registry_with(plugin_parent).await?;
    let task_drops = registry
        .values()
        .map(|parent| Arc::clone(&parent.task_dropped))
        .collect::<Vec<_>>();
    let shutdown_starts = registry
        .values()
        .map(|parent| Arc::clone(&parent.shutdown_started))
        .collect::<Vec<_>>();

    let operation = AssertUnwindSafe(async {
        wait_for_parent_tasks(&registry).await?;

        let first_a = {
            let parent = registry.get_mut("parent-a").context("missing parent-a")?;
            exchange(
                &mut parent.application,
                &parent.control,
                PluginCompletion::Text("from-parent-a"),
            )
            .await?
        };
        let first_b = {
            let parent = registry.get_mut("parent-b").context("missing parent-b")?;
            exchange(
                &mut parent.application,
                &parent.control,
                PluginCompletion::Empty,
            )
            .await?
        };

        let first_a_target = first_a.frame().target();
        let first_a_ingress = first_a.ingress_generation();
        let first_payloads_equal = first_a.frame().submission().canonical_bytes()
            == first_b.frame().submission().canonical_bytes();
        let distinct_targets = first_a_target != first_b.frame().target();

        let parent_a = registry.get("parent-a").context("missing parent-a")?;
        let old_text_stale = parent_a
            .control
            .act(first_a_ingress, ExternalAct::text("late"))
            .await
            == Err(ExternalControlFault::StaleIngress);
        let old_complete_stale = parent_a.control.complete(first_a_ingress).await
            == Err(ExternalControlFault::StaleIngress);

        let second_a = {
            let parent = registry.get_mut("parent-a").context("missing parent-a")?;
            exchange(
                &mut parent.application,
                &parent.control,
                PluginCompletion::Empty,
            )
            .await?
        };
        let second_same_parent_is_delta =
            matches!(second_a.frame().basis(), FrameBasis::DeltaFrom(_));
        let second_same_parent_kept_target = second_a.frame().target() == first_a_target;
        let second_frame_saw_first_text = second_a.content().contains("from-parent-a");
        let other_parent_remained_idle = registry
            .get("parent-b")
            .context("missing parent-b")?
            .control
            .next_observation()
            .now_or_never()
            .is_none();

        ensure!(
            distinct_targets,
            "each Plugin parent needs a distinct target"
        );
        ensure!(
            first_payloads_equal,
            "parent-independent roots must compile identical first payloads"
        );
        ensure!(
            old_text_stale && old_complete_stale,
            "an old Plugin ingress must reject both text and empty completion"
        );
        ensure!(
            second_same_parent_is_delta
                && second_same_parent_kept_target
                && second_frame_saw_first_text,
            "a same-parent second exchange must retain its target and canonical history"
        );
        ensure!(
            other_parent_remained_idle,
            "driving one parent must not submit for another"
        );

        Ok::<_, anyhow::Error>(PluginEvidence {
            parent_count: registry.len(),
            distinct_targets,
            first_payloads_equal,
            old_text_stale,
            old_complete_stale,
            second_same_parent_is_delta,
            second_same_parent_kept_target,
            second_frame_saw_first_text,
            other_parent_remained_idle,
            shutdown_count: 0,
            all_tasks_dropped: false,
        })
    })
    .catch_unwind()
    .await;

    let cleanup = shutdown_registry(registry).await;
    let cleanup_attempted = cleanup.attempted;
    let cleanup_completed = cleanup.completed;
    let all_shutdowns_started = shutdown_starts
        .iter()
        .all(|started| started.load(Ordering::Acquire));
    let all_tasks_dropped = task_drops
        .iter()
        .all(|dropped| dropped.load(Ordering::Acquire));
    let mut evidence = resolve_after_cleanup(operation, cleanup)?;

    ensure!(
        cleanup_attempted == 2 && cleanup_completed == 2 && all_shutdowns_started,
        "every Plugin parent must consume shutdown"
    );
    ensure!(
        all_tasks_dropped,
        "Plugin shutdown must drain every Component task"
    );
    evidence.shutdown_count = cleanup_completed;
    evidence.all_tasks_dropped = true;
    Ok(evidence)
}

#[tokio::main]
async fn main() -> Result<()> {
    let evidence = run_plugin().await?;
    println!(
        "frame_plugin parents={} distinct_targets={} equal_first_payloads={} old_text_stale={} old_complete_stale={} second_delta={} same_target={} replayed_text={} other_idle={} shutdowns={} tasks_dropped={}",
        evidence.parent_count,
        evidence.distinct_targets,
        evidence.first_payloads_equal,
        evidence.old_text_stale,
        evidence.old_complete_stale,
        evidence.second_same_parent_is_delta,
        evidence.second_same_parent_kept_target,
        evidence.second_frame_saw_first_text,
        evidence.other_parent_remained_idle,
        evidence.shutdown_count,
        evidence.all_tasks_dropped,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    use super::*;

    fn panic_message(payload: &PanicPayload) -> Option<&str> {
        payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    }

    #[tokio::test]
    async fn public_plugin_driver_matches_the_shared_exact_first_frame_golden() {
        let (port, control) = ExternalProviderPort::new().unwrap();
        let mut application =
            Application::mount(frame_workflow_golden::shared_frame_workflow_root, port).unwrap();

        let operation = exchange(&mut application, &control, PluginCompletion::Empty).await;
        let shutdown = application.shutdown().await;
        let observation = operation.unwrap();
        shutdown.unwrap();

        assert_eq!(observation.frame().basis(), FrameBasis::Full);
        frame_workflow_golden::assert_exact_first_frame(
            observation.frame().submission().canonical_bytes(),
        );
    }

    #[tokio::test]
    async fn partial_registry_setup_consumes_the_already_mounted_parent() {
        let calls = Arc::new(AtomicUsize::new(0));
        let first_shutdown = Arc::new(Mutex::new(None));
        let result = build_plugin_registry_with({
            let calls = Arc::clone(&calls);
            let first_shutdown = Arc::clone(&first_shutdown);
            move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let parent = plugin_parent()?;
                    *first_shutdown.lock().unwrap() = Some(Arc::clone(&parent.shutdown_started));
                    Ok(parent)
                } else {
                    Err(anyhow::anyhow!("injected parent-b creation failure"))
                }
            }
        })
        .await;

        let error = match result {
            Ok(_) => panic!("injected parent-b creation failure unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("injected parent-b creation failure"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(first_shutdown
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire));
    }

    #[derive(Clone, Copy)]
    enum FakeShutdown {
        Ok,
        Error,
        Panic,
    }

    #[tokio::test]
    async fn cleanup_attempts_every_owner_after_failure_and_panic() {
        let attempted = Arc::new(Mutex::new(Vec::new()));
        let owners = vec![
            (1_u8, FakeShutdown::Ok),
            (2, FakeShutdown::Panic),
            (3, FakeShutdown::Error),
            (4, FakeShutdown::Ok),
        ];
        let mut cleanup = drain_all(owners, {
            let attempted = Arc::clone(&attempted);
            move |(id, outcome)| {
                let attempted = Arc::clone(&attempted);
                async move {
                    attempted.lock().unwrap().push(id);
                    match outcome {
                        FakeShutdown::Ok => Ok(()),
                        FakeShutdown::Error => Err(anyhow::anyhow!("injected cleanup error")),
                        FakeShutdown::Panic => panic!("injected cleanup panic"),
                    }
                }
            }
        })
        .await;

        assert_eq!(*attempted.lock().unwrap(), [1, 2, 3, 4]);
        assert_eq!(cleanup.attempted, 4);
        assert_eq!(cleanup.completed, 2);
        assert!(cleanup
            .first_error
            .take()
            .unwrap()
            .to_string()
            .contains("injected cleanup error"));
        assert_eq!(
            panic_message(cleanup.first_panic.as_ref().unwrap()),
            Some("injected cleanup panic")
        );
    }

    #[tokio::test]
    async fn primary_operation_panic_is_resumed_after_all_cleanup_attempts() {
        let attempted = Arc::new(Mutex::new(Vec::new()));
        let cleanup = drain_all(vec![(1_u8, FakeShutdown::Panic), (2, FakeShutdown::Ok)], {
            let attempted = Arc::clone(&attempted);
            move |(id, outcome)| {
                let attempted = Arc::clone(&attempted);
                async move {
                    attempted.lock().unwrap().push(id);
                    match outcome {
                        FakeShutdown::Ok => Ok(()),
                        FakeShutdown::Error => Err(anyhow::anyhow!("injected cleanup error")),
                        FakeShutdown::Panic => panic!("secondary cleanup panic"),
                    }
                }
            }
        })
        .await;
        let operation: std::thread::Result<Result<()>> =
            Err(Box::new(String::from("primary operation panic")));

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = resolve_after_cleanup(operation, cleanup);
        }))
        .unwrap_err();

        assert_eq!(*attempted.lock().unwrap(), [1, 2]);
        assert_eq!(panic_message(&panic), Some("primary operation panic"));
    }

    #[tokio::test]
    async fn registry_isolates_parent_applications_and_fences_old_ingress() {
        let evidence = run_plugin().await.unwrap();

        assert_eq!(evidence.parent_count, 2);
        assert!(evidence.distinct_targets);
        assert!(evidence.first_payloads_equal);
        assert!(evidence.old_text_stale);
        assert!(evidence.old_complete_stale);
        assert!(evidence.second_same_parent_is_delta);
        assert!(evidence.second_same_parent_kept_target);
        assert!(evidence.second_frame_saw_first_text);
        assert!(evidence.other_parent_remained_idle);
        assert_eq!(evidence.shutdown_count, 2);
        assert!(evidence.all_tasks_dropped);
    }
}
