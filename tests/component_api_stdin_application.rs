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
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadableSchemaInput {
    #[schemars(
        description = "Entity -> State; <example> & literal &gt; and ]]>; C:\\memory\\graph\nA second line with `*_[]#`."
    )]
    text: String,
    settings: ReadableSchemaSettings,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadableSchemaSettings {
    enabled: bool,
}

#[component]
fn readable_schema_actions() -> Component {
    use_wait_for_command();
    let count = use_signal(|| 0_usize);
    let value = count.with(|count| *count).unwrap();
    view! {
        count { "{value}" }
        Action {
            name: "record",
            on_call: move |input: ReadableSchemaInput| {
                count.update(|count| {
                    *count += 1;
                    json!({"count": *count, "text": input.text, "enabled": input.settings.enabled})
                })
            },
        }
    }
}

#[tokio::test]
async fn typed_inputs_and_results_preserve_data_through_rejection_and_recovery() {
    tokio::time::timeout(TIMEOUT, async {
        let mut app = StdinApplication::mount(readable_schema_actions).unwrap();
        let initial = app.observe().await.unwrap();
        assert!(initial.ok);

        let rejected = app
            .submit(CommandCall::new(
                "record",
                json!({"text": "missing settings"}),
            ))
            .await
            .unwrap();
        assert!(!rejected.ok);

        let text =
            "<input_schema> & literal &gt; -> \\memory\\graph\nsecond line ]]> `*_[]#` \"quoted\"";
        let recorded = app
            .submit(CommandCall::new(
                "record",
                json!({"text": text, "settings": {"enabled": true}}),
            ))
            .await
            .unwrap();
        assert!(recorded.ok);
        assert_eq!(
            recorded.result,
            Some(json!({"count": 1, "text": text, "enabled": true}))
        );

        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn large_json_actions_and_responses_preserve_complete_results_and_next_actions() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut app = StdinApplication::mount(readable_schema_actions).unwrap();
        let text = format!(
            "{}complete payload",
            "<record> & literal &gt; \\\n\0界🦀".repeat(8192)
        );
        let mut line = serde_json::to_vec(&CommandCall::new(
            "record",
            json!({"text": text, "settings": {"enabled": true}}),
        ))
        .unwrap();
        line.push(b'\n');
        assert!(line.len() > 4096);
        let recorded = app
            .submit(serde_json::from_slice(&line).unwrap())
            .await
            .unwrap();
        let expected = json!({"count": 1, "text": text, "enabled": true});
        assert!(recorded.ok);
        assert_eq!(recorded.result.as_ref(), Some(&expected));

        let mut wire = serde_json::to_vec(&recorded).unwrap();
        wire.push(b'\n');
        assert!(wire.len() > 128 * 1024);
        let delivered: StdinResponse = serde_json::from_slice(&wire).unwrap();
        assert_eq!(delivered.result, Some(expected));

        let next = app
            .submit(CommandCall::new(
                "record",
                json!({"text": "next action", "settings": {"enabled": false}}),
            ))
            .await
            .unwrap();
        assert!(next.ok);
        assert_eq!(
            next.result,
            Some(json!({"count": 2, "text": "next action", "enabled": false}))
        );

        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_frame_keeps_ordered_results_and_rejections_until_the_next_nonempty_frame() {
    tokio::time::timeout(TIMEOUT, async {
        let mut app = StdinApplication::mount(readable_schema_actions).unwrap();

        let first = json!({"count": 1, "text": "first frame note", "enabled": true});
        let second = json!({"count": 2, "text": "second frame note", "enabled": false});
        let response = app
            .submit_frame(vec![
                CommandCall::new(
                    "record",
                    json!({"text": "first frame note", "settings": {"enabled": true}}),
                ),
                CommandCall::new("record", json!({"text": "missing settings"})),
                CommandCall::new("unknown_action", json!({})),
                CommandCall::new(
                    "record",
                    json!({"text": "second frame note", "settings": {"enabled": false}}),
                ),
            ])
            .await
            .unwrap();
        assert!(!response.ok);
        let results = response.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(
            results[0],
            json!({"action": "record", "ok": true, "result": first})
        );
        assert_eq!(results[1]["action"], "record");
        assert_eq!(results[1]["ok"], false);
        assert_eq!(results[1]["result"]["code"], "invalid_arguments");
        assert_eq!(results[2]["action"], "unknown_action");
        assert_eq!(results[2]["ok"], false);
        assert_eq!(results[2]["result"]["code"], "command_unavailable");
        assert_eq!(
            results[3],
            json!({"action": "record", "ok": true, "result": second})
        );

        let empty = app.submit_frame(Vec::new()).await.unwrap();
        assert!(empty.ok);
        assert_eq!(empty.result, None);

        let next = app
            .submit_frame(vec![CommandCall::new(
                "record",
                json!({"text": "new frame note", "settings": {"enabled": true}}),
            )])
            .await
            .unwrap();
        assert!(next.ok);
        assert_eq!(
            next.result,
            Some(json!([{
                "action": "record", "ok": true,
                "result": {"count": 3, "text": "new frame note", "enabled": true},
            }]))
        );

        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

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

        let code = format!("invalid_input-{}", "x".repeat(256));
        let diagnostic = format!("bad\0{}complete diagnostic", "&".repeat(140 * 1024));
        let invalid = app
            .feedback(code.clone(), diagnostic.clone())
            .await
            .unwrap();
        assert!(!invalid.ok);

        let wire = serde_json::to_vec(&invalid).unwrap();
        let delivered: StdinResponse = serde_json::from_slice(&wire).unwrap();
        assert_eq!(delivered.ok, invalid.ok);
        assert_eq!(delivered.result, invalid.result);

        let changed = app
            .submit(CommandCall::new("increment", json!({})))
            .await
            .unwrap();
        assert_eq!(changed.result, Some(json!({"count": 1})));

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

#[tokio::test]
async fn frame_results_keep_distinct_actions_even_when_their_outputs_are_equal() {
    tokio::time::timeout(TIMEOUT, async {
        let mut app = StdinApplication::mount(null_result_actions).unwrap();
        let response = app
            .submit_frame(vec![
                CommandCall::new("unit", json!({})),
                CommandCall::new("unit", json!({})),
                CommandCall::new("null", json!({})),
            ])
            .await
            .unwrap();
        assert!(response.ok);
        assert_eq!(
            response.result,
            Some(json!([
                {"action": "unit", "ok": true, "result": null},
                {"action": "unit", "ok": true, "result": null},
                {"action": "null", "ok": true, "result": null},
            ]))
        );

        let delivered: StdinResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(delivered.result, response.result);

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
        assert!(observed.ok);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_a_frame_waiter_still_runs_its_remaining_actions_and_retains_their_feedback() {
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
            let submitted = app.submit_frame(vec![
                CommandCall::new("slow", json!({})),
                CommandCall::new("slow", json!({})),
            ]);
            tokio::pin!(submitted);
            tokio::select! {
                result = &mut submitted => panic!("frame finished before release: {result:?}"),
                _ = started.notified() => {}
            }
        }
        release.notify_one();
        started.notified().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        release.notify_one();

        let observed = app.observe().await.unwrap();
        assert!(observed.ok);
        assert_eq!(observed.result, None);

        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let next = {
            let next = app.submit_frame(vec![CommandCall::new("slow", json!({}))]);
            tokio::pin!(next);
            tokio::select! {
                result = &mut next => panic!("next frame finished before release: {result:?}"),
                _ = started.notified() => {}
            }
            release.notify_one();
            next.await.unwrap()
        };
        assert_eq!(
            next.result,
            Some(json!([{"action": "slow", "ok": true, "result": 3}]))
        );

        assert_eq!(calls.load(Ordering::SeqCst), 3);

        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_accepted_frame_finishes_before_a_later_frame_can_run() {
    use futures::poll;

    tokio::time::timeout(TIMEOUT, async {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (root_started, root_release, root_calls) =
            (started.clone(), release.clone(), calls.clone());
        let mut app = StdinApplication::mount(move || {
            slow_action(root_started.clone(), root_release.clone(), root_calls.clone())
        })
        .unwrap();
        {
            let first = app.submit_frame(vec![
                CommandCall::new("slow", json!({})),
                CommandCall::new("slow", json!({})),
            ]);
            tokio::pin!(first);
            tokio::select! {
                result = &mut first => panic!("first frame finished before release: {result:?}"),
                _ = started.notified() => {}
            }
        }
        let next = {
            let next = app.submit_frame(vec![CommandCall::new("slow", json!({}))]);
            tokio::pin!(next);
            assert!(poll!(next.as_mut()).is_pending(), "enqueue the next frame");

            for expected_calls in [2, 3] {
                release.notify_one();
                tokio::select! {
                    result = &mut next => panic!("later frame ran before the accepted batch finished: {result:?}"),
                    _ = started.notified() => {}
                }
                assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            }
            release.notify_one();
            next.await.unwrap()
        };
        assert_eq!(next.result, Some(json!([{"action": "slow", "ok": true, "result": 3}])));

        assert_eq!(calls.load(Ordering::SeqCst), 3);
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

#[derive(Clone, Default)]
struct ExternalChoiceProbe {
    value: Arc<AtomicUsize>,
    preparations: Arc<AtomicUsize>,
    callbacks: Arc<AtomicUsize>,
}

#[component]
fn externally_loaded_choices(probe: ExternalChoiceProbe) -> Component {
    let observed = use_signal(|| 0_usize);
    let loaded = observed.clone();
    let preparation_probe = probe.clone();
    use_preparation(move || async move {
        loaded.set(preparation_probe.value.load(Ordering::SeqCst))?;
        preparation_probe
            .preparations
            .fetch_add(1, Ordering::SeqCst);
        Ok::<_, anyhow::Error>(())
    });
    use_wait_for_command();
    let current = observed.with(|value| *value).unwrap();
    let name = format!("choose_{current}");
    view! {
        external_value { "{current}" }
        Action {
            name: name,
            on_call: move || {
                probe.callbacks.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(current)
            },
        }
    }
}

#[tokio::test]
async fn each_stdin_observation_loads_external_state_after_its_request_arrives() {
    tokio::time::timeout(TIMEOUT, async {
        let probe = ExternalChoiceProbe::default();
        let root_probe = probe.clone();
        let mut app =
            StdinApplication::mount(move || externally_loaded_choices(root_probe.clone())).unwrap();
        app.observe().await.unwrap();
        tokio::task::yield_now().await;
        probe.value.store(7, Ordering::SeqCst);
        let refreshed = app.observe().await.unwrap();
        assert!(refreshed.ok);

        assert_eq!(probe.preparations.load(Ordering::SeqCst), 2);
        tokio::task::yield_now().await;
        assert_eq!(probe.preparations.load(Ordering::SeqCst), 2);
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stdin_refreshes_external_choices_before_dispatching_a_displayed_action() {
    tokio::time::timeout(TIMEOUT, async {
        let probe = ExternalChoiceProbe::default();
        let root_probe = probe.clone();
        let mut app =
            StdinApplication::mount(move || externally_loaded_choices(root_probe.clone())).unwrap();
        app.observe().await.unwrap();
        tokio::task::yield_now().await;
        probe.value.store(9, Ordering::SeqCst);
        let stale = app
            .submit(CommandCall::new("choose_0", json!({})))
            .await
            .unwrap();
        assert!(!stale.ok);
        assert_eq!(
            stale.result.as_ref().unwrap()["code"],
            "command_unavailable"
        );
        assert_eq!(probe.callbacks.load(Ordering::SeqCst), 0);

        let current = app
            .submit(CommandCall::new("choose_9", json!({})))
            .await
            .unwrap();
        assert!(current.ok);
        assert_eq!(current.result, Some(json!(9)));
        assert_eq!(probe.callbacks.load(Ordering::SeqCst), 1);
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn idle_stdin_shutdown_wakes_ingress_without_starting_preparation() {
    tokio::time::timeout(TIMEOUT, async {
        let probe = ExternalChoiceProbe::default();
        let root_probe = probe.clone();
        let app =
            StdinApplication::mount(move || externally_loaded_choices(root_probe.clone())).unwrap();
        tokio::task::yield_now().await;
        app.shutdown().await.unwrap();
        assert_eq!(probe.preparations.load(Ordering::SeqCst), 0);
    })
    .await
    .unwrap();
}

#[component]
fn idle_panicking_task(release: Arc<Notify>) -> Component {
    use_wait_for_command();
    use_future(move || async move {
        release.notified().await;
        std::panic::panic_any(321_u32);
    });
    view! { idle_task { "waiting" } }
}

#[tokio::test]
async fn an_idle_stdin_driver_preserves_a_supervised_task_panic() {
    use futures::FutureExt;
    tokio::time::timeout(TIMEOUT, async {
        let release = Arc::new(Notify::new());
        let root_release = release.clone();
        let mut app =
            StdinApplication::mount(move || idle_panicking_task(root_release.clone())).unwrap();
        app.observe().await.unwrap();
        release.notify_one();
        tokio::task::yield_now().await;
        let payload = std::panic::AssertUnwindSafe(app.observe())
            .catch_unwind()
            .await
            .expect_err("idle ingress must preserve the original task panic");
        assert_eq!(payload.downcast_ref::<u32>(), Some(&321));
        app.shutdown().await.unwrap();
    })
    .await
    .unwrap();
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
