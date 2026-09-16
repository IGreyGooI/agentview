use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, CommandCall, CommandInput, CommandInputError, CommandOutcome,
            ExternalProviderPort, RenderedProjection,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use futures::poll;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(2);

fn text(projection: &RenderedProjection) -> String {
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

struct State {
    value: usize,
    loading: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IncrementInput {
    amount: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            value: 0,
            loading: false,
        }
    }
}

#[derive(Clone, Default)]
struct Probe {
    state: Arc<Mutex<Option<Signal<State>>>>,
    gate: Arc<Notify>,
}

impl Probe {
    fn state(&self) -> Signal<State> {
        self.state.lock().unwrap().clone().unwrap()
    }
}

#[component]
fn loader(gate: Arc<Notify>) -> Component {
    use_preparation(move || async move {
        gate.notified().await;
        Ok::<_, std::convert::Infallible>(())
    });
    view! {}
}

#[component]
fn counter_view(probe: Probe) -> Component {
    let state = use_signal(State::default);
    let typed_state = state.clone();
    *probe.state.lock().unwrap() = Some(state.clone());
    let (value, loading) = state.with(|state| (state.value, state.loading)).unwrap();
    let loader = if loading {
        loader(probe.gate)
    } else {
        view! {}
    };
    view! {
        count { "{value}" }
        { loader }
        Action {
            name: "increment",
            description: "Increment the counter",
            on_call: move || state.update(|state| { state.value += 1; state.value }),
        }
        Action {
            name: "increment_by",
            on_call: move |input: IncrementInput| typed_state.update(|state| {
                state.value += input.amount;
                state.value
            }),
        }
    }
}

#[component]
fn counter(probe: Probe) -> Component {
    use_wait_for_command();
    view! { counter_view(probe) }
}

#[component]
fn feedback_counter(probe: Probe, large_output: String) -> Component {
    use_wait_for_command();
    view! {
        counter_view(probe)
        Action {
            name: "large_output",
            on_call: move || Ok::<_, std::convert::Infallible>(large_output.clone()),
        }
    }
}

#[tokio::test]
async fn large_retained_feedback_leaves_views_deliverable_for_following_actions_and_observations() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let output = "    &<>\"'\\`*_[]#-+~.)\t\n\r\0\u{ffff}界🦀".repeat(2048);
    let root_output = output.clone();
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(
        move || feedback_counter(root_probe.clone(), root_output.clone()),
        port,
        input,
    )
    .unwrap();

    let assert_deliverable = |view: &str, ok: bool| {
        // The adapter wraps the rendered view in JSON. Test actual encoded
        // bytes: a character count misses XML entities and JSON backslashes.
        let wire = serde_json::to_vec(&json!({"ok": ok, "output": view})).unwrap();
        assert!(wire.len() < 128 * 1024, "view needs {} bytes", wire.len());
    };
    for sequence in 1..=8 {
        let unknown = format!("missing-{sequence}-{}", "&".repeat(4000));
        let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(
                app.prepare(),
                sender.submit(CommandCall::new(&unknown, json!({})))
            )
        })
        .await
        .unwrap();
        assert!(prepared.unwrap().is_continue());
        let response = response.unwrap();
        assert_eq!(response.call.name(), unknown);
        let CommandOutcome::Rejected { code, message } = &response.outcome else {
            panic!("unknown input must be rejected");
        };
        assert_eq!(code, "command_unavailable");
        assert!(message.contains(&unknown), "retain the complete diagnostic");
        let view = text(&response.projection);
        assert_deliverable(&view, false);
        assert!(view.contains("truncated"));
        assert!(view.contains("<count>0</count>"));
        assert_eq!(view.matches("<command_result ").count(), sequence);
    }

    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            app.prepare(),
            sender.submit(CommandCall::new("large_output", json!({})))
        )
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let response = response.unwrap();
    assert_eq!(response.outcome, CommandOutcome::Output(json!(output)));
    assert_deliverable(&text(&response.projection), true);

    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            app.prepare(),
            sender.submit(CommandCall::new("increment", json!({})))
        )
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let response = response.unwrap();
    assert_eq!(response.outcome, CommandOutcome::Output(json!(1)));
    assert_deliverable(&text(&response.projection), true);

    let (prepared, observed) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(app.prepare(), sender.observe())
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let view = text(&observed.unwrap());
    assert_deliverable(&view, true);
    assert!(view.contains("<count>1</count>"));
    assert!(view.contains("command=\"increment\" status=\"completed\">1"));
    assert!(view.contains("<command_feedback sequence=\"10\""));
    assert_eq!(view.matches("<command_result ").count(), 8);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_action_characters_remain_visible_without_poisoning_following_views() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (sender, input) = CommandInput::channel(8);
    let mut app =
        Application::mount_with_commands(move || counter(root_probe.clone()), port, input).unwrap();
    let unknown = "missing\0\u{8}\u{b}\u{c}\u{e}\u{1f}\u{fffe}\u{ffff}";
    for (name, input, expected_code) in [
        (unknown, json!({}), "command_unavailable"),
        (
            "increment_by",
            json!({"invalid\u{ffff}field": 1}),
            "invalid_arguments",
        ),
    ] {
        let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(app.prepare(), sender.submit(CommandCall::new(name, input)))
        })
        .await
        .unwrap();
        assert!(prepared.unwrap().is_continue());
        let response = response.unwrap();
        assert_eq!(response.call.name(), name, "retain the original invocation");
        let CommandOutcome::Rejected { code, message } = &response.outcome else {
            panic!("invalid input must be rejected");
        };
        assert_eq!(code, expected_code);
        assert!(
            message.contains('\u{ffff}'),
            "retain the original diagnostic"
        );
        let view = text(&response.projection);
        assert!(view.contains(expected_code));
        assert!(view.contains("\\u{ffff}"));
        assert!(!view.contains('\u{ffff}'));
        assert!(!view.contains('\0'));
        assert!(view.contains("<count>0</count>"));
    }

    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            app.prepare(),
            sender.submit(CommandCall::new("increment", json!({})))
        )
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    assert_eq!(response.unwrap().outcome, CommandOutcome::Output(json!(1)));
    let (prepared, observed) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(app.prepare(), sender.observe())
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let view = text(&observed.unwrap());
    assert!(view.contains("<count>1</count>"));
    assert_eq!(view.matches("<command_result ").count(), 3);
    assert!(view.contains("\\u{0}"));
    assert!(view.contains("\\u{fffe}"));
    assert!(view.contains("\\u{ffff}"));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn observations_refresh_state_and_preserve_the_last_action_feedback_in_queue_order() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (sender, input) = CommandInput::channel(8);
    let mut app =
        Application::mount_with_commands(move || counter(root_probe.clone()), port, input).unwrap();

    let (prepared, observed) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(app.prepare(), sender.observe())
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let initial = text(&observed.unwrap());
    assert!(initial.contains("<count>0</count>"));
    assert!(!initial.contains("<command_result"));

    let mut action = Box::pin(sender.submit(CommandCall::new("increment", json!({}))));
    let mut observation = Box::pin(sender.observe());
    assert!(poll!(&mut action).is_pending());
    assert!(poll!(&mut observation).is_pending());
    assert!(tokio::time::timeout(TIMEOUT, app.prepare())
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let action_view = text(&action.await.unwrap().projection);
    assert!(action_view.contains("<count>1</count>"));
    assert!(action_view.contains("<command_feedback sequence=\"1\""));
    assert!(
        poll!(&mut observation).is_pending(),
        "one prepare consumes one queued request"
    );

    let mut preparing = Box::pin(app.prepare());
    // An already queued observation may complete immediately. Changing state
    // before polling preparation must still be reflected in its returned view.
    probe.state().update(|state| state.value = 9).unwrap();
    assert!(tokio::time::timeout(TIMEOUT, &mut preparing)
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    drop(preparing);
    let observed = text(&observation.await.unwrap());
    assert!(observed.contains("<count>9</count>"));
    assert!(observed.contains("<command_feedback sequence=\"1\""));
    assert_eq!(observed.matches("<command_result ").count(), 1);
    assert!(app.current_projection().is_prepared());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn observations_wait_for_an_explicit_barrier_and_close_with_the_application() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let waiting = Arc::new(AtomicBool::new(false));
    let root_waiting = waiting.clone();
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(
        move || {
            if root_waiting.load(Ordering::SeqCst) {
                counter(root_probe.clone())
            } else {
                counter_view(root_probe.clone())
            }
        },
        port,
        input,
    )
    .unwrap();
    let mut observation = Box::pin(sender.observe());
    assert!(poll!(&mut observation).is_pending());
    assert!(app.prepare().await.unwrap().is_continue());
    assert!(poll!(&mut observation).is_pending());

    waiting.store(true, Ordering::SeqCst);
    probe.state().update(|_| ()).unwrap();
    assert!(tokio::time::timeout(TIMEOUT, app.prepare())
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    assert!(text(&observation.await.unwrap()).contains("<count>0</count>"));

    let mut queued = Box::pin(sender.observe());
    assert!(poll!(&mut queued).is_pending());
    app.shutdown().await.unwrap();
    assert!(matches!(queued.await, Err(CommandInputError::Closed)));
    assert!(matches!(
        sender.observe().await,
        Err(CommandInputError::Closed)
    ));
}

#[tokio::test]
async fn cancelling_preparation_retains_a_received_observation_until_retry_is_ready() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (sender, input) = CommandInput::channel(8);
    let mut app =
        Application::mount_with_commands(move || counter(root_probe.clone()), port, input).unwrap();

    let mut preparing = Box::pin(app.prepare());
    assert!(poll!(&mut preparing).is_pending());
    // The new preparation mounts after the observation is received, during the
    // reconciliation that guarantees the observation sees current state.
    probe.state().update(|state| state.loading = true).unwrap();
    let mut observation = Box::pin(sender.observe());
    assert!(poll!(&mut observation).is_pending());
    assert!(poll!(&mut preparing).is_pending());
    drop(preparing);
    assert!(poll!(&mut observation).is_pending());

    probe.state().update(|state| state.value = 7).unwrap();
    probe.gate.notify_one();
    assert!(tokio::time::timeout(TIMEOUT, app.prepare())
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let observed = text(&observation.await.unwrap());
    assert!(observed.contains("<count>7</count>"));
    assert!(!observed.contains("<command_result"));
    app.shutdown().await.unwrap();
}

#[test]
fn stdin_action_envelopes_require_objects_and_reject_duplicate_fields() {
    let action: CommandCall = serde_json::from_str(r#"{"action":"increment","input":{}}"#).unwrap();
    assert_eq!(
        serde_json::to_value(&action).unwrap(),
        json!({"action": "increment", "input": {}})
    );
    for value in [
        r#"["increment",{}]"#,
        r#"{"action":"increment","command":"increment","input":{}}"#,
        r#"{"action":"increment","action":"increment","input":{}}"#,
        r#"{"action":"increment","input":{"value":1,"value":2}}"#,
        r#"{"action":"increment","input":{},"input":{}}"#,
        r#"{"action":"increment","input":{},"extra":true}"#,
        r#"{"action":"increment","input":null}"#,
    ] {
        assert!(
            serde_json::from_str::<CommandCall>(value).is_err(),
            "{value}"
        );
    }
}
