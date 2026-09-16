use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use agentview::component::{
    execution::{CommandCall, StdinApplication, StdinApplicationError, StdinResponse},
    prelude::*,
};
use serde_json::json;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(3);

#[component]
fn child_actions(count: Signal<usize>) -> Component {
    let increment = count.clone();
    let value = count.with(|count| *count).unwrap();
    view! {
        count { "{value}" }
        Action {
            name: "increment",
            on_call: move || {
                let count = increment.clone();
                async move {
                    tokio::task::yield_now().await;
                    count.update(|count| { *count += 1; json!({"count": *count}) })
                }
            },
        }
        Action {
            name: "refuse",
            on_call: || Ok::<_, Infallible>(json!({"accepted": false, "message": "Business refusal"})),
        }
    }
}

#[component]
fn counter() -> Component {
    use_wait_for_command();
    let count = use_signal(|| 0_usize);
    view! { child_actions(count) }
}

#[tokio::test]
async fn owner_drives_root_wait_and_returns_outputs_feedback_and_next_view() {
    tokio::time::timeout(TIMEOUT, async {
        let mut app = StdinApplication::mount(counter).unwrap();
        let first = app.observe().await.unwrap();
        assert_eq!(first.result, None);
        assert!(first.view.contains("<count>0</count>"));
        assert!(first.view.contains("name=\"increment\""));

        let rejected = app
            .submit(CommandCall::new("refuse", json!({})))
            .await
            .unwrap();
        assert!(
            rejected.ok,
            "the handler ran and returned its business result"
        );
        assert_eq!(
            rejected.result,
            Some(json!({"accepted": false, "message": "Business refusal"}))
        );

        let invalid = app
            .feedback("invalid_input", format!("bad\0{}", "&".repeat(20_000)))
            .await
            .unwrap();
        assert!(!invalid.ok);
        assert!(invalid.view.contains("<input_feedback"));
        assert!(invalid.view.contains("truncated"));
        assert!(!invalid.view.contains('\0'));
        assert!(serde_json::to_vec(&invalid).unwrap().len() < 128 * 1024);
        assert_eq!(app.observe().await.unwrap().view, invalid.view);

        let changed = app
            .submit(CommandCall::new("increment", json!({})))
            .await
            .unwrap();
        assert_eq!(changed.result, Some(json!({"count": 1})));
        assert!(changed.view.contains("<count>1</count>"));
        assert!(!changed.view.contains("<input_feedback"));
        assert_eq!(app.observe().await.unwrap().view, changed.view);
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[component]
fn null_result_actions() -> Component {
    use_wait_for_command();
    view! {
        Action { name: "unit", on_call: || Ok::<_, Infallible>(()), }
        Action { name: "null", on_call: || Ok::<_, Infallible>(json!(null)), }
    }
}

#[tokio::test]
async fn response_encoding_distinguishes_null_callback_results_from_observations() {
    tokio::time::timeout(TIMEOUT, async {
        let mut app = StdinApplication::mount(null_result_actions).unwrap();
        for name in ["unit", "null"] {
            let response = app.submit(CommandCall::new(name, json!({}))).await.unwrap();
            let wire = serde_json::to_value(&response).unwrap();
            assert_eq!(wire.get("result"), Some(&json!(null)));
            let delivered: StdinResponse = serde_json::from_value(wire).unwrap();
            assert_eq!(delivered.result, Some(json!(null)));
            assert!(delivered.ok);

            let observation = app.observe().await.unwrap();
            assert_eq!(observation.view, response.view);
            let wire = serde_json::to_value(&observation).unwrap();
            assert!(wire.get("result").is_none());
            let delivered: StdinResponse = serde_json::from_value(wire).unwrap();
            assert_eq!(delivered.result, None);
        }
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[component]
fn slow_action(started: Arc<Notify>, release: Arc<Notify>, calls: Arc<AtomicUsize>) -> Component {
    use_wait_for_command();
    let count = use_signal(|| 0_usize);
    let value = count.with(|count| *count).unwrap();
    view! {
        count { "{value}" }
        Action {
            name: "slow",
            on_call: move || {
                let (started, release, calls, count) = (started.clone(), release.clone(), calls.clone(), count.clone());
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    count.update(|count| { *count += 1; *count })
                }
            },
        }
    }
}

#[tokio::test]
async fn cancelling_a_caller_does_not_replay_or_cancel_an_accepted_action() {
    tokio::time::timeout(TIMEOUT, async {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (root_started, root_release, root_calls) =
            (started.clone(), release.clone(), calls.clone());
        let mut app = StdinApplication::mount(move || {
            slow_action(
                root_started.clone(),
                root_release.clone(),
                root_calls.clone(),
            )
        })
        .unwrap();
        {
            let submitted = app.submit(CommandCall::new("slow", json!({})));
            tokio::pin!(submitted);
            tokio::select! {
                result = &mut submitted => panic!("action finished before release: {result:?}"),
                _ = started.notified() => {}
            }
        }
        release.notify_one();
        let observed = app.observe().await.unwrap();
        assert!(observed.view.contains("<count>1</count>"));
        assert!(observed.view.contains("status=\"completed\">1"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[component]
fn pending_action(started: Arc<Notify>, dropped: Arc<AtomicBool>) -> Component {
    use_wait_for_command();
    view! {
        Action {
            name: "pending",
            on_call: move || {
                let (started, dropped) = (started.clone(), dropped.clone());
                async move {
                    let _drop = Dropped(dropped);
                    started.notify_one();
                    std::future::pending::<()>().await;
                    Ok::<_, Infallible>(())
                }
            },
        }
    }
}

#[tokio::test]
async fn cancelling_shutdown_waiter_still_finishes_callback_cleanup() {
    tokio::time::timeout(TIMEOUT, async {
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let (root_started, root_dropped) = (started.clone(), dropped.clone());
        let mut app = StdinApplication::mount(move || {
            pending_action(root_started.clone(), root_dropped.clone())
        })
        .unwrap();
        {
            let submitted = app.submit(CommandCall::new("pending", json!({})));
            tokio::pin!(submitted);
            tokio::select! {
                _ = &mut submitted => panic!("pending callback completed"),
                _ = started.notified() => {}
            }
        }
        let shutdown = app.shutdown();
        // Cleanup begins at the call, even with an unpolled waiter held alive.
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        drop(shutdown);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn missing_root_wait_is_rejected_at_mount() {
    let result = StdinApplication::mount(|| view! { "No wait" });
    assert!(matches!(result, Err(StdinApplicationError::MissingWait)));
}

#[component]
fn fail_next_preparation(preparations: Arc<AtomicUsize>) -> Component {
    use_wait_for_command();
    use_preparation(move || async move {
        if preparations.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err("next preparation failed")
        }
    });
    view! {
        Action { name: "answer", on_call: || Ok::<_, Infallible>(42) }
    }
}

#[tokio::test]
async fn completed_response_precedes_a_fault_in_the_following_preparation() {
    tokio::time::timeout(TIMEOUT, async {
        let preparations = Arc::new(AtomicUsize::new(0));
        let mut app =
            StdinApplication::mount(move || fail_next_preparation(preparations.clone())).unwrap();
        let response = app
            .submit(CommandCall::new("answer", json!({})))
            .await
            .unwrap();
        assert_eq!(response.result, Some(json!(42)));
        assert!(app.observe().await.is_err());
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[component]
fn failing_actions() -> Component {
    use_wait_for_command();
    view! {
        Action {
            name: "fault",
            on_call: || Err::<(), _>("storage unavailable"),
        }
        Action {
            name: "panic",
            on_call: || -> Result<(), Infallible> { std::panic::panic_any(1234_u32) },
        }
    }
}

#[tokio::test]
async fn callback_fault_keeps_its_diagnostic_and_panic_keeps_its_payload() {
    use futures::FutureExt;
    tokio::time::timeout(TIMEOUT, async {
        let mut app = StdinApplication::mount(failing_actions).unwrap();
        let error = app
            .submit(CommandCall::new("fault", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("storage unavailable"), "{error}");
        assert!(app.shutdown().await.is_err());

        let mut app = StdinApplication::mount(failing_actions).unwrap();
        let payload =
            std::panic::AssertUnwindSafe(app.submit(CommandCall::new("panic", json!({}))))
                .catch_unwind()
                .await
                .expect_err("original callback panic must unwind");
        assert_eq!(payload.downcast_ref::<u32>(), Some(&1234));
    })
    .await
    .unwrap();
}
