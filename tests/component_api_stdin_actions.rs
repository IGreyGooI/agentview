use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFaultReason, CommandCall, CommandInput, CommandInputError,
            CommandOutcome, CommandResponse, CommandSender, ExternalControl, ExternalProviderPort,
            RenderedProjection,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use futures::poll;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddInput {
    /// Amount to add to the current total.
    #[serde(rename = "amount")]
    delta: i64,
    #[serde(default)]
    reason: String,
}

#[component]
fn add_action(total: Signal<i64>) -> Component {
    let observed = total.with(|total| *total).unwrap();
    view! {
        Action {
            // Metadata appearing after on_call must be evaluated before the
            // callback takes ownership of its captures.
            on_call: move |input: AddInput| {
                total.update(|total| {
                    *total += input.delta;
                    json!({"total": *total, "observed": observed, "reason": input.reason})
                })
            },
            enabled: total.with(|total| *total < 10).unwrap(),
            description: "Add an amount and display the updated total",
            name: "add",
        }
    }
}

#[component]
fn counter() -> Component {
    let total = use_signal(|| 0_i64);
    let current = total.with(|total| *total).unwrap();
    use_wait_for_command();
    view! {
        counter { total { "{current}" } }
        add_action(total.clone())
        Action {
            on_call: move || total.with(|total| json!({"total": *total})),
            name: "inspect",
        }
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

struct Harness {
    app: Application<ExternalProviderPort>,
    sender: CommandSender,
    _control: ExternalControl,
}

impl Harness {
    fn mount() -> Self {
        Self::with_root(counter)
    }

    fn with_root(root: impl Fn() -> Component + Send + Sync + 'static) -> Self {
        let (port, control) = ExternalProviderPort::new().unwrap();
        let (sender, input) = CommandInput::channel(8);
        Self {
            app: Application::mount_with_commands(root, port, input).unwrap(),
            sender,
            _control: control,
        }
    }

    async fn action(&mut self, name: &str, input: Value) -> CommandResponse {
        let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(
                self.app.prepare(),
                self.sender.submit(CommandCall::new(name, input))
            )
        })
        .await
        .expect("action completes");
        assert!(prepared.unwrap().is_continue());
        response.unwrap()
    }
}

#[tokio::test]
async fn inline_actions_derive_discovery_and_deliver_updated_state_for_the_next_action() {
    let mut harness = Harness::mount();
    let initial = projection_text(harness.app.current_projection().projection());
    assert!(initial.contains("<total>0</total>"));
    assert!(initial.contains("name=\"add\""));
    assert!(initial.contains("Add an amount and display the updated total"));
    assert!(initial.contains("\"action\":\"add\""));
    assert!(initial.contains("\"required\":[\"amount\"]"), "{initial}");
    assert!(initial.contains("Amount to add to the current total."));
    assert!(
        !initial.contains("\"delta\""),
        "serde rename defines the input"
    );
    assert!(!initial.contains("--stdin"));
    assert!(!initial.contains("--amount"));

    let added = harness.action("add", json!({"amount": 10})).await;
    assert_eq!(
        added.outcome,
        CommandOutcome::Output(json!({"total": 10, "observed": 0, "reason": ""}))
    );
    let updated = projection_text(&added.projection);
    assert!(updated.contains("<total>10</total>"));
    assert!(updated.contains("enabled=\"false\""));
    assert!(updated.contains("command_result"));

    let inspected = harness.action("inspect", json!({})).await;
    assert_eq!(
        inspected.outcome,
        CommandOutcome::Output(json!({"total": 10}))
    );

    // Disabled actions remain discoverable but cannot invoke the callback.
    let adjusted = harness
        .action("add", json!({"amount": -4, "reason": "adjust"}))
        .await;
    assert!(matches!(
        adjusted.outcome,
        CommandOutcome::Rejected { ref code, .. } if code == "command_disabled"
    ));
    let rejected_view = projection_text(&adjusted.projection);
    assert!(rejected_view.contains("<total>10</total>"));
    assert!(rejected_view.contains("command_disabled"));
    assert_eq!(
        harness.action("inspect", json!({})).await.outcome,
        CommandOutcome::Output(json!({"total": 10}))
    );
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn typed_input_errors_remain_visible_and_allow_a_following_valid_action() {
    let mut harness = Harness::mount();
    for input in [
        json!({}),
        json!({"amount": "many"}),
        json!({"amount": 1, "unexpected": true}),
        json!([1]),
    ] {
        let response = harness.action("add", input).await;
        assert!(matches!(
            response.outcome,
            CommandOutcome::Rejected { ref code, .. } if code == "invalid_arguments"
        ));
        let feedback = projection_text(&response.projection);
        assert!(feedback.contains("invalid_arguments"));
        assert!(feedback.contains("<total>0</total>"));
        assert!(feedback.contains("name=\"add\""));
    }
    let response = harness.action("add", json!({"amount": 3})).await;
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"total": 3, "observed": 0, "reason": ""}))
    );
    assert!(projection_text(&response.projection).contains("<total>3</total>"));
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn zero_argument_actions_accept_only_an_empty_object() {
    let mut harness = Harness::mount();
    for input in [json!({"unexpected": true}), Value::Null, json!([])] {
        let response = harness.action("inspect", input).await;
        assert!(matches!(
            response.outcome,
            CommandOutcome::Rejected { ref code, .. } if code == "invalid_arguments"
        ));
        assert!(projection_text(&response.projection).contains("empty JSON object"));
    }
    let response = harness.action("inspect", json!({})).await;
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"total": 0}))
    );
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn action_input_schema_must_describe_an_object() {
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (_sender, input) = CommandInput::channel(1);
    let result = Application::mount_with_commands(
        || {
            view! {
                Action {
                    name: "invalid",
                    on_call: |input: String| Ok::<_, Infallible>(input),
                }
            }
        },
        port,
        input,
    );
    let fault = result.err().expect("non-object action input is invalid");
    assert_eq!(fault.reason(), ApplicationFaultReason::ComponentInvariant);
}

#[derive(Clone, Default)]
struct AsyncProbe {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

struct CallbackDrop(Arc<AtomicBool>);

impl Drop for CallbackDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[component]
fn async_counter(probe: AsyncProbe) -> Component {
    use_wait_for_command();
    let total = use_signal(|| 0_i64);
    let current = total.with(|total| *total).unwrap();
    let added_total = total.clone();
    view! {
        counter { total { "{current}" } }
        Action {
            name: "add",
            on_call: move |input: AddInput| {
                probe.calls.fetch_add(1, Ordering::SeqCst);
                let probe = probe.clone();
                let total = added_total.clone();
                async move {
                    let _drop = CallbackDrop(probe.dropped.clone());
                    let result = total.update(|total| {
                        *total += input.delta;
                        *total
                    }).unwrap();
                    probe.entered.notify_one();
                    probe.release.notified().await;
                    if input.reason == "fail" {
                        return Err("storage unavailable");
                    }
                    probe.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"total": result}))
                }
            },
        }
        Action {
            name: "inspect",
            on_call: move || {
                let total = total.clone();
                async move {
                    tokio::task::yield_now().await;
                    total.with(|total| json!({"total": *total}))
                }
            },
        }
    }
}

#[tokio::test]
async fn async_typed_and_zero_argument_actions_deliver_only_their_completed_results() {
    let probe = AsyncProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::with_root(move || async_counter(root_probe.clone()));
    let mut response = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("add", json!({"amount": 4}))),
    );
    assert!(poll!(&mut response).is_pending());
    let mut preparation = Box::pin(harness.app.prepare());
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = probe.entered.notified() => {}
            result = &mut preparation => panic!("callback should wait: {result:?}"),
        }
    })
    .await
    .unwrap();
    assert!(poll!(&mut response).is_pending());
    probe.release.notify_one();
    assert!(preparation.await.unwrap().is_continue());
    let response = response.await.unwrap();
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"total": 4}))
    );
    assert!(projection_text(&response.projection).contains("<total>4</total>"));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.action("inspect", json!({})).await.outcome,
        CommandOutcome::Output(json!({"total": 4}))
    );
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_an_async_action_drops_it_without_replay_and_allows_the_next_action() {
    let probe = AsyncProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::with_root(move || async_counter(root_probe.clone()));
    let mut response = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("add", json!({"amount": 4}))),
    );
    assert!(poll!(&mut response).is_pending());
    let mut preparation = Box::pin(harness.app.prepare());
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = probe.entered.notified() => {}
            result = &mut preparation => panic!("callback should wait: {result:?}"),
        }
    })
    .await
    .unwrap();
    drop(preparation);
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(poll!(&mut response).is_pending());
    let fault = harness.app.prepare().await.unwrap_err();
    assert_eq!(fault.reason(), ApplicationFaultReason::CommandHandler);
    assert!(matches!(
        response.await,
        Err(CommandInputError::Interrupted)
    ));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.action("inspect", json!({})).await.outcome,
        CommandOutcome::Output(json!({"total": 4})),
        "writes before cancellation remain visible"
    );
    probe.release.notify_one();
    assert_eq!(
        harness.action("add", json!({"amount": 2})).await.outcome,
        CommandOutcome::Output(json!({"total": 6}))
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn async_handler_fault_keeps_prior_writes_and_allows_a_following_observation() {
    let probe = AsyncProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::with_root(move || async_counter(root_probe.clone()));
    probe.release.notify_one();
    let (prepared, response) = tokio::join!(
        harness.app.prepare(),
        harness.sender.submit(CommandCall::new(
            "add",
            json!({"amount": 4, "reason": "fail"})
        ))
    );
    assert_eq!(
        prepared.unwrap_err().reason(),
        ApplicationFaultReason::CommandHandler
    );
    assert!(
        matches!(response, Err(CommandInputError::Handler(message)) if message == "storage unavailable")
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.action("inspect", json!({})).await.outcome,
        CommandOutcome::Output(json!({"total": 4}))
    );
    harness.app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct ScopeProbe {
    visible: Arc<Mutex<Option<Signal<bool>>>>,
    local: Arc<Mutex<Option<Signal<u8>>>>,
    sync_started: Arc<Notify>,
    async_started: Arc<Notify>,
    sync_dropped: Arc<AtomicBool>,
    async_dropped: Arc<AtomicBool>,
}

#[component]
fn scoped_action(probe: ScopeProbe) -> Component {
    let local = use_signal(|| 0_u8);
    *probe.local.lock().unwrap() = Some(local.clone());
    view! {
        Action {
            name: "start",
            on_call: move || {
                let sync_probe = probe.clone();
                spawn(async move {
                    let _drop = CallbackDrop(sync_probe.sync_dropped);
                    sync_probe.sync_started.notify_one();
                    std::future::pending::<()>().await;
                }).expect("callback construction has mount context");
                let probe = probe.clone();
                let local = local.clone();
                async move {
                    tokio::task::yield_now().await;
                    local.set(1).unwrap();
                    spawn(async move {
                        let _drop = CallbackDrop(probe.async_dropped);
                        probe.async_started.notify_one();
                        std::future::pending::<()>().await;
                    }).expect("callback polling has mount context");
                    Ok::<_, Infallible>("started")
                }
            },
        }
    }
}

#[component]
fn scoped_application(probe: ScopeProbe) -> Component {
    use_wait_for_command();
    let visible = use_signal(|| true);
    *probe.visible.lock().unwrap() = Some(visible.clone());
    let action = if visible.with(|visible| *visible).unwrap() {
        scoped_action(probe)
    } else {
        view! { state { "action removed" } }
    };
    view! {
        { action }
        Action { name: "inspect", on_call: || Ok::<_, Infallible>(true), }
    }
}

#[tokio::test]
async fn async_action_factory_and_polls_share_mount_context_and_unmount_retires_tasks() {
    let probe = ScopeProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::with_root(move || scoped_application(root_probe.clone()));
    assert_eq!(
        harness.action("start", json!({})).await.outcome,
        CommandOutcome::Output(json!("started"))
    );
    tokio::time::timeout(TIMEOUT, async {
        probe.sync_started.notified().await;
        probe.async_started.notified().await;
    })
    .await
    .expect("both callback phases can start mount tasks");
    let local = probe.local.lock().unwrap().clone().unwrap();
    assert_eq!(local.with(|value| *value).unwrap(), 1);
    probe
        .visible
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set(false)
        .unwrap();
    harness.action("inspect", json!({})).await;
    assert!(probe.sync_dropped.load(Ordering::SeqCst));
    assert!(probe.async_dropped.load(Ordering::SeqCst));
    assert!(local.with(|value| *value).is_err());
    harness.app.shutdown().await.unwrap();
}
