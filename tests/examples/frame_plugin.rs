use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use agentview::component::execution::{ExternalAct, ExternalObservationKind, FrameBasis};
use agentview::component::prelude::*;
use anyhow::{Context, Result};
use futures::FutureExt;

use super::*;

fn panic_message(payload: &PanicPayload) -> Option<&str> {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
}

#[derive(Clone)]
struct RetainedRootProps {
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

struct TaskDrop(Arc<AtomicBool>);

impl Drop for TaskDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[component]
fn retained_root(props: RetainedRootProps) -> Component {
    let started = Arc::clone(&props.started);
    let dropped = Arc::clone(&props.dropped);
    use_future(move || async move {
        let _drop = TaskDrop(dropped);
        started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
    });
    view! { retained_plugin_root { "cleanup probe" } }
}

async fn retained_parent() -> Result<(PluginParent, Arc<AtomicBool>)> {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let props = RetainedRootProps {
        started: Arc::clone(&started),
        dropped: Arc::clone(&dropped),
    };
    let application = ExternalApplication::new_root(move || retained_root(props.clone()))?;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok((PluginParent { application }, dropped))
}

#[tokio::test]
async fn plugin_parents_keep_independent_external_sessions() {
    let mut registry = build_plugin_registry().await.unwrap();

    let first_a = registry
        .get_mut("parent-a")
        .context("missing parent-a")
        .unwrap()
        .application
        .observe()
        .await
        .unwrap();
    let first_b = registry
        .get_mut("parent-b")
        .context("missing parent-b")
        .unwrap()
        .application
        .observe()
        .await
        .unwrap();

    assert_eq!(first_a.kind(), ExternalObservationKind::Full);
    assert_eq!(first_b.kind(), ExternalObservationKind::Full);
    assert_ne!(first_a.frame().target(), first_b.frame().target());
    assert!(first_a.content().contains("parent-a"));
    assert!(first_b.content().contains("parent-b"));

    let second_a = registry
        .get_mut("parent-a")
        .unwrap()
        .application
        .act(ExternalAct::text("parent-a update"))
        .await
        .unwrap();
    let second_b = registry
        .get_mut("parent-b")
        .unwrap()
        .application
        .act(ExternalAct::text("parent-b update"))
        .await
        .unwrap();

    assert!(matches!(second_a.frame().basis(), FrameBasis::DeltaFrom(_)));
    assert!(matches!(second_b.frame().basis(), FrameBasis::DeltaFrom(_)));
    assert_eq!(second_a.frame().target(), first_a.frame().target());
    assert_eq!(second_b.frame().target(), first_b.frame().target());
    assert!(second_a.content().contains("parent-a update"));
    assert!(second_b.content().contains("parent-b update"));

    let shutdown = shutdown_registry(registry).await;
    assert!(shutdown.first_error.is_none());
    assert!(shutdown.first_panic.is_none());
}

#[tokio::test]
async fn partial_registry_setup_consumes_the_already_mounted_parent() {
    let (parent, dropped) = retained_parent().await.unwrap();
    let mut first_parent = Some(parent);
    let result = build_plugin_registry_with(move |_parent_id| {
        first_parent
            .take()
            .context("injected parent-b creation failure")
    })
    .await;

    let error = match result {
        Ok(_) => panic!("injected parent-b creation failure unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("injected parent-b creation failure"));
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn panic_during_registry_setup_still_consumes_the_already_mounted_parent() {
    let (parent, dropped) = retained_parent().await.unwrap();
    let mut first_parent = Some(parent);
    let result = AssertUnwindSafe(build_plugin_registry_with(move |_parent_id| {
        Ok(first_parent
            .take()
            .expect("injected parent-b creation panic"))
    }))
    .catch_unwind()
    .await;

    let panic = match result {
        Ok(_) => panic!("injected parent-b creation panic unexpectedly returned"),
        Err(panic) => panic,
    };
    assert_eq!(
        panic_message(&panic),
        Some("injected parent-b creation panic")
    );
    assert!(dropped.load(Ordering::Acquire));
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
    let operation: BoundaryResult<()> = Err(Box::new(String::from("primary operation panic")));

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = resolve_operation_cleanup(
            operation,
            cleanup.into_boundary_result(),
            "Plugin cleanup also failed after the operation",
        );
    }))
    .unwrap_err();

    assert_eq!(*attempted.lock().unwrap(), [1, 2]);
    assert_eq!(panic_message(&panic), Some("primary operation panic"));
}

#[tokio::test]
async fn parent_operation_error_keeps_its_error_when_shutdown_also_fails() {
    let operation: BoundaryResult<()> = Ok(Err(anyhow::anyhow!("model operation failed")));
    let cleanup: BoundaryResult<()> = Ok(Err(anyhow::anyhow!("parent shutdown failed")));

    let error = resolve_operation_cleanup(
        operation,
        cleanup,
        "parent Application shutdown also failed after the model operation",
    )
    .unwrap_err();
    assert!(error
        .chain()
        .any(|cause| cause.to_string().contains("model operation failed")));
    assert!(error
        .chain()
        .any(|cause| cause.to_string().contains("parent shutdown failed")));
}

#[tokio::test]
async fn registry_operation_error_keeps_its_error_when_cleanup_also_fails() {
    let operation: BoundaryResult<()> = Ok(Err(anyhow::anyhow!("plugin operation failed")));
    let cleanup = CleanupOutcome {
        first_error: Some(anyhow::anyhow!("registry shutdown failed")),
        first_panic: None,
    };

    let error = resolve_operation_cleanup(
        operation,
        cleanup.into_boundary_result(),
        "Plugin registry cleanup also failed after the model operation",
    )
    .unwrap_err();
    assert!(error
        .chain()
        .any(|cause| cause.to_string().contains("plugin operation failed")));
    assert!(error
        .chain()
        .any(|cause| cause.to_string().contains("registry shutdown failed")));
}

#[tokio::test]
async fn registry_shutdown_cancels_each_parent_with_an_open_ingress() {
    let mut registry = build_plugin_registry().await.unwrap();
    let first_a = registry
        .get_mut("parent-a")
        .unwrap()
        .application
        .observe()
        .await
        .unwrap();
    let first_b = registry
        .get_mut("parent-b")
        .unwrap()
        .application
        .observe()
        .await
        .unwrap();

    assert_eq!(first_a.kind(), ExternalObservationKind::Full);
    assert_eq!(first_b.kind(), ExternalObservationKind::Full);
    let shutdown = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        shutdown_registry(registry),
    )
    .await
    .unwrap();
    assert!(shutdown.first_error.is_none());
    assert!(shutdown.first_panic.is_none());
}
