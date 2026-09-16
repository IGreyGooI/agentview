use std::{
    convert::Infallible,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFaultKind, ApplicationFaultReason, ApplicationFaultStage,
            CommandCall, CommandInput, CommandInputError, CommandOutcome, CommandParseError,
            CommandParser, CommandResponse, CommandSender, ExitReason, ExternalControl,
            ExternalProviderPort, RenderedProjection,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, InstructionAuthority},
};
use futures::poll;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddInput {
    delta: i64,
}

const ADD: Command<AddInput> = Command::new(
    "add",
    "Adjust the retained counter",
    Some(CommandArgument {
        name: "delta",
        description: "a signed counter adjustment",
        example: "3",
    }),
);
const INSPECT: Command<NoInput> = Command::new("inspect", "Show the current counter", None);
const START: Command<NoInput> = Command::new("start", "Start application work", None);
const FAIL: Command<NoInput> = Command::new("fail", "Exercise a runtime handler failure", None);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInput {
    #[serde(rename = "text")]
    _text: String,
}

const NOTE: Command<TextInput> = Command::new(
    "note",
    "Store a note",
    Some(CommandArgument {
        name: "text",
        description: "the note text",
        example: "hello",
    }),
);

const PARSER: CommandParser<'static> =
    CommandParser::new(&[INSPECT.definition(), NOTE.definition()]);

struct UnreadInput;

impl std::io::Read for UnreadInput {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        panic!("argv parsing must not read stdin")
    }
}

#[test]
fn command_parser_shares_typed_metadata_between_flags_stdin_and_help() {
    let inspect = PARSER.parse(&["inspect"], UnreadInput, 0).unwrap();
    assert_eq!(inspect.name(), INSPECT.name());
    assert_eq!(inspect.input(), &json!({}));

    let note = PARSER
        .parse(&["note", "--text", "hello"], UnreadInput, 0)
        .unwrap();
    assert_eq!(note.name(), NOTE.name());
    assert_eq!(note.input(), &json!({"text": "hello"}));
    PARSER.validate(&note).unwrap();

    let payload = br#"{"text":"hello"}"#;
    for maximum in [payload.len(), usize::MAX] {
        let stdin = PARSER
            .parse(&["note", "--stdin"], payload.as_slice(), maximum)
            .unwrap();
        assert_eq!(stdin.name(), note.name());
        assert_eq!(stdin.input(), note.input());
    }

    let help = PARSER.help();
    assert!(help.contains("inspect"));
    assert!(help.contains(INSPECT.description()));
    assert!(help.contains("note --text hello"));
    assert!(help.contains(NOTE.description()));
    assert!(help.contains("note --stdin"));
    assert!(help.contains(r#"{"text":"hello"}"#));
}

#[test]
fn command_parser_rejects_invalid_arguments_and_bounds_stdin() {
    for args in [
        vec![],
        vec!["missing"],
        vec!["inspect", "extra"],
        vec!["inspect", "--stdin"],
        vec!["note"],
        vec!["note", "--text"],
        vec!["note", "--unknown", "hello"],
        vec!["note", "--text", "hello", "--text", "again"],
        vec!["note", "--stdin", "extra"],
    ] {
        assert!(PARSER.parse(&args, UnreadInput, 1024).is_err(), "{args:?}");
    }
    for input in [
        "",
        "null",
        "[]",
        "true",
        r#""hello""#,
        "{}",
        r#"{"text":3}"#,
        r#"{"text":"hello","extra":true}"#,
        r#"{"text":"hello","text":"again"}"#,
        r#"{"text":"hello"} {}"#,
    ] {
        assert!(
            PARSER
                .parse(&["note", "--stdin"], input.as_bytes(), 1024)
                .is_err(),
            "{input}"
        );
    }
    assert!(matches!(
        PARSER.parse(&["note", "--stdin"], std::io::repeat(b' '), 16),
        Err(CommandParseError::InputTooLarge(16))
    ));
    for input in [json!([]), json!({"text": 3}), json!({"extra": "hello"})] {
        assert!(matches!(
            PARSER.validate(&CommandCall::new("note", input)),
            Err(CommandParseError::InvalidInput(_))
        ));
    }
}

#[test]
fn command_parser_rejects_invalid_catalogs_before_selecting_or_reading_input() {
    const INVALID_NAME: Command<NoInput> = Command::new("invalid name", "Invalid", None);
    const INVALID_ARGUMENT: Command<TextInput> = Command::new(
        "bad_argument",
        "Invalid",
        Some(CommandArgument {
            name: "--text",
            description: "Invalid",
            example: "hello",
        }),
    );
    const RESERVED_ARGUMENT: Command<TextInput> = Command::new(
        "reserved_argument",
        "Invalid",
        Some(CommandArgument {
            name: "stdin",
            description: "Invalid",
            example: "hello",
        }),
    );
    for invalid in [
        NOTE.definition(),
        INVALID_NAME.definition(),
        INVALID_ARGUMENT.definition(),
        RESERVED_ARGUMENT.definition(),
    ] {
        let commands = [NOTE.definition(), invalid];
        let parser = CommandParser::new(&commands);
        assert!(matches!(
            parser.parse(&["note", "--stdin"], UnreadInput, 1024),
            Err(CommandParseError::InvalidCatalog(_))
        ));
        assert!(matches!(
            parser.validate(&CommandCall::new("note", json!({"text": "hello"}))),
            Err(CommandParseError::InvalidCatalog(_))
        ));
    }
}

#[test]
fn serialized_command_calls_preserve_the_strict_input_object_boundary() {
    let call: CommandCall =
        serde_json::from_str(r#"{"command":"note","input":{"text":"hello"}}"#).unwrap();
    PARSER.validate(&call).unwrap();
    assert_eq!(
        serde_json::to_value(&call).unwrap(),
        json!({"action": "note", "input": {"text": "hello"}})
    );
    for request in [
        r#"{"command":"note","input":null}"#,
        r#"{"command":"note","input":[]}"#,
        r#"{"command":"note","input":{"text":"one","text":"two"}}"#,
        r#"{"command":"note","command":"inspect","input":{}}"#,
        r#"{"command":"inspect","input":{},"input":{}}"#,
        r#"{"command":"inspect","input":{},"extra":true}"#,
        r#"{"command":"inspect"}"#,
    ] {
        assert!(
            serde_json::from_str::<CommandCall>(request).is_err(),
            "{request}"
        );
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
    fn mount(root: impl Fn() -> Component + Send + Sync + 'static) -> Self {
        let (port, control) = ExternalProviderPort::new().unwrap();
        let (sender, input) = CommandInput::channel(8);
        Self {
            app: Application::mount_with_commands(root, port, input).unwrap(),
            sender,
            _control: control,
        }
    }

    async fn command(&mut self, name: &str, input: Value) -> CommandResponse {
        let mut response = Box::pin(self.sender.submit(CommandCall::new(name, input)));
        assert!(
            poll!(&mut response).is_pending(),
            "submit waits for preparation"
        );
        assert!(tokio::time::timeout(TIMEOUT, self.app.prepare())
            .await
            .expect("command preparation completes")
            .unwrap()
            .is_continue());
        assert!(self.app.current_projection().is_prepared());
        tokio::time::timeout(TIMEOUT, response)
            .await
            .expect("prepared command has a response")
            .unwrap()
    }
}

#[derive(Clone)]
struct CounterState {
    value: i64,
    calls: usize,
    show_add: bool,
    pending_work: bool,
}

impl Default for CounterState {
    fn default() -> Self {
        Self {
            value: 0,
            calls: 0,
            show_add: true,
            pending_work: false,
        }
    }
}

#[derive(Clone, Default)]
struct CounterProbe(Arc<Mutex<Option<Signal<CounterState>>>>);

impl CounterProbe {
    fn signal(&self) -> Signal<CounterState> {
        self.0.lock().unwrap().clone().expect("mounted counter")
    }

    fn state(&self) -> CounterState {
        self.signal().with(Clone::clone).unwrap()
    }
}

#[component]
fn add_action(state: Signal<CounterState>, observed: i64) -> Component {
    view! {
        CliCommand {
            command: ADD,
            enabled: observed < 10,
            on_call: move |input: AddInput| {
                state.update(|state| {
                    state.calls += 1;
                    state.value += input.delta;
                    json!({"value": state.value, "observed": observed})
                })
            },
        }
    }
}

#[component]
fn counter_view(state: Signal<CounterState>) -> Component {
    let current = state.with(Clone::clone).unwrap();
    let value = current.value;
    let action = if current.show_add {
        add_action(state.clone(), value)
    } else {
        view! {}
    };
    view! {
        counter { value { "{value}" } }
        { action }
        CliCommand {
            command: INSPECT,
            on_call: move |_: NoInput| {
                state.with(|state| json!({"value": state.value, "calls": state.calls}))
            },
        }
    }
}

#[component]
fn counter(probe: CounterProbe) -> Component {
    let state = use_signal(CounterState::default);
    *probe.0.lock().unwrap() = Some(state.clone());
    use_wait_for_command();
    view! { counter_view(state) }
}

#[tokio::test]
async fn parsed_commands_reach_current_callbacks_and_return_the_updated_view() {
    const PARSER: CommandParser<'static> =
        CommandParser::new(&[ADD.definition(), INSPECT.definition()]);
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || counter(root_probe.clone()));
    let add = PARSER
        .parse(&["add", "--stdin"], br#"{"delta":3}"#.as_slice(), 64)
        .unwrap();
    let inspect = PARSER.parse(&["inspect"], UnreadInput, 0).unwrap();
    for (call, outcome) in [
        (add, json!({"value": 3, "observed": 0})),
        (inspect, json!({"value": 3, "calls": 1})),
    ] {
        // An adapter can forward the same parsed call as a serialized request.
        let wire = serde_json::to_vec(&call).unwrap();
        let call = serde_json::from_slice(&wire).unwrap();
        PARSER.validate(&call).unwrap();
        let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(harness.app.prepare(), harness.sender.submit(call))
        })
        .await
        .expect("parsed command completes");
        assert!(prepared.unwrap().is_continue());
        let response = response.unwrap();
        assert_eq!(response.outcome, CommandOutcome::Output(outcome));
        let projection = projection_text(&response.projection);
        assert!(projection.contains("<value>3</value>"));
        assert!(projection.contains("command_result"));
        assert!(projection.contains("add --stdin"));
    }
    assert_eq!(probe.state().calls, 1);
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn plain_view_collects_child_actions_and_replies_with_updated_state_and_callbacks() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || counter(root_probe.clone()));
    assert_eq!(probe.state().calls, 0, "render does not execute a callback");
    assert!(!harness.app.current_projection().is_prepared());
    let initial = projection_text(harness.app.current_projection().projection());
    assert!(initial.contains("add --delta TOKEN"));
    assert!(initial.contains("add --stdin"));
    assert!(initial.contains("signed counter adjustment"));

    let first = harness.command("add", json!({"delta": 10})).await;
    assert_eq!(first.call.name(), "add");
    assert_eq!(
        first.outcome,
        CommandOutcome::Output(json!({"value": 10, "observed": 0}))
    );
    let first_view = projection_text(&first.projection);
    assert!(first_view.contains("<value>10</value>"));
    assert!(first_view.contains("enabled=\"false\""));
    assert!(first_view.contains("command_result"));

    let second = harness.command("add", json!({"delta": -4})).await;
    assert!(matches!(
        second.outcome,
        CommandOutcome::Rejected { ref code, .. } if code == "command_disabled"
    ));
    assert!(projection_text(&second.projection).contains("<value>10</value>"));
    assert_eq!(
        probe.state().calls,
        1,
        "disabled commands do not invoke their callback"
    );
    probe.signal().update(|state| state.value = 4).unwrap();
    let third = harness.command("add", json!({"delta": 2})).await;
    assert_eq!(
        third.outcome,
        CommandOutcome::Output(json!({"value": 6, "observed": 4}))
    );
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_arguments_and_unknown_commands_deliver_feedback_then_allow_valid_input() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || counter(root_probe.clone()));
    for input in [
        json!(3),
        json!({"delta": "bad"}),
        json!({"delta": 1, "extra": 2}),
    ] {
        let response = harness.command("add", input).await;
        assert!(
            matches!(response.outcome, CommandOutcome::Rejected { ref code, .. } if code == "invalid_arguments")
        );
        assert!(projection_text(&response.projection).contains("invalid_arguments"));
    }
    let unknown = harness.command("missing", json!({})).await;
    assert!(
        matches!(unknown.outcome, CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable")
    );
    let feedback = projection_text(&unknown.projection);
    assert!(feedback.contains("command=\"missing\""));
    assert!(feedback.contains("not currently mounted"));
    assert_eq!(probe.state().calls, 0);

    let recovered = harness.command("add", json!({"delta": 3})).await;
    assert_eq!(
        recovered.outcome,
        CommandOutcome::Output(json!({"value": 3, "observed": 0}))
    );
    assert!(projection_text(&recovered.projection).contains("<value>3</value>"));
    harness.app.shutdown().await.unwrap();
}

#[component]
fn command_barrier() -> Component {
    use_wait_for_command();
    view! {}
}

#[component]
fn counter_without_barrier(probe: CounterProbe) -> Component {
    let state = use_signal(CounterState::default);
    *probe.0.lock().unwrap() = Some(state.clone());
    view! {
        counter_view(state)
    }
}

#[tokio::test]
async fn commands_wait_in_the_inbox_until_a_component_explicitly_declares_the_barrier() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let waiting = Arc::new(AtomicBool::new(false));
    let root_waiting = waiting.clone();
    let mut harness = Harness::mount(move || {
        if root_waiting.load(Ordering::SeqCst) {
            counter(root_probe.clone())
        } else {
            counter_without_barrier(root_probe.clone())
        }
    });
    let mut response = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("add", json!({"delta": 5}))),
    );
    assert!(poll!(&mut response).is_pending());
    assert!(harness.app.prepare().await.unwrap().is_continue());
    assert!(poll!(&mut response).is_pending());
    assert_eq!(probe.state().calls, 0);

    waiting.store(true, Ordering::SeqCst);
    assert!(tokio::time::timeout(TIMEOUT, harness.app.prepare())
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let response = response.await.unwrap();
    assert!(projection_text(&response.projection).contains("<value>5</value>"));
    assert_eq!(probe.state().calls, 1);
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_an_empty_inbox_wait_leaves_the_next_operation_usable() {
    let probe = CounterProbe::default();
    let mut harness = Harness::mount(move || counter(probe.clone()));
    let mut preparation = Box::pin(harness.app.prepare());
    assert!(poll!(&mut preparation).is_pending());
    drop(preparation);
    let response = harness.command("add", json!({"delta": 2})).await;
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"value": 2, "observed": 0}))
    );
    harness.app.shutdown().await.unwrap();
}

#[tokio::test]
async fn state_changes_while_waiting_refresh_removed_commands_and_callback_captures() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || counter(root_probe.clone()));
    let mut preparation = Box::pin(harness.app.prepare());
    assert!(poll!(&mut preparation).is_pending());
    probe
        .signal()
        .update(|state| state.show_add = false)
        .unwrap();
    let mut response = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("add", json!({"delta": 1}))),
    );
    assert!(poll!(&mut response).is_pending());
    assert!(tokio::time::timeout(TIMEOUT, preparation)
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let response = response.await.unwrap();
    assert!(
        matches!(response.outcome, CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable")
    );
    assert!(!projection_text(&response.projection).contains("add --delta TOKEN"));
    assert_eq!(probe.state().calls, 0);

    probe
        .signal()
        .update(|state| state.show_add = true)
        .unwrap();
    let mut preparation = Box::pin(harness.app.prepare());
    assert!(poll!(&mut preparation).is_pending());
    probe.signal().update(|state| state.value = 4).unwrap();
    let mut response = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("add", json!({"delta": 2}))),
    );
    assert!(poll!(&mut response).is_pending());
    assert!(tokio::time::timeout(TIMEOUT, preparation)
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let response = response.await.unwrap();
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"value": 6, "observed": 4}))
    );
    harness.app.shutdown().await.unwrap();
}

#[derive(Default)]
struct WorkGate {
    started: Notify,
    release: Notify,
}

#[component]
fn finish_work(state: Signal<CounterState>, gate: Arc<WorkGate>) -> Component {
    use_preparation(move || async move {
        gate.started.notify_one();
        gate.release.notified().await;
        state.update(|state| {
            state.value = 100;
            state.pending_work = false;
        })
    });
    view! { work { "pending" } }
}

#[component]
fn work_application(probe: CounterProbe, gate: Arc<WorkGate>) -> Component {
    let state = use_signal(CounterState::default);
    *probe.0.lock().unwrap() = Some(state.clone());
    use_wait_for_command();
    let pending = if state.with(|state| state.pending_work).unwrap() {
        finish_work(state.clone(), gate)
    } else {
        view! {}
    };
    let started = state.clone();
    view! {
        { pending }
        counter_view(state)
        CliCommand {
            command: START,
            on_call: move |_: NoInput| {
                started.update(|state| {
                    state.calls += 1;
                    state.pending_work = true;
                    "started"
                })
            },
        }
    }
}

#[tokio::test]
async fn cancellation_after_callback_retains_its_result_until_new_preparation_finishes() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let gate = Arc::new(WorkGate::default());
    let root_gate = Arc::clone(&gate);
    let mut harness =
        Harness::mount(move || work_application(root_probe.clone(), Arc::clone(&root_gate)));
    let mut first = Box::pin(harness.sender.submit(CommandCall::new("start", json!({}))));
    assert!(poll!(&mut first).is_pending());
    let mut preparation = Box::pin(harness.app.prepare());
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = gate.started.notified() => {}
            result = &mut preparation => panic!("work preparation should wait: {result:?}"),
        }
    })
    .await
    .unwrap();
    drop(preparation);
    assert_eq!(probe.state().calls, 1);
    assert!(
        poll!(&mut first).is_pending(),
        "an unprepared view must not be delivered"
    );
    let mut second = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("inspect", json!({}))),
    );
    assert!(poll!(&mut second).is_pending());

    gate.release.notify_one();
    assert!(tokio::time::timeout(TIMEOUT, harness.app.prepare())
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let response = first.await.unwrap();
    assert_eq!(response.outcome, CommandOutcome::Output(json!("started")));
    assert!(projection_text(&response.projection).contains("<value>100</value>"));
    assert_eq!(
        probe.state().calls,
        1,
        "a completed callback is never replayed"
    );
    assert!(
        poll!(&mut second).is_pending(),
        "one preparation consumes one command"
    );
    assert!(tokio::time::timeout(TIMEOUT, harness.app.prepare())
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    assert_eq!(
        second.await.unwrap().outcome,
        CommandOutcome::Output(json!({"value": 100, "calls": 1}))
    );
    harness.app.shutdown().await.unwrap();
}

#[component]
fn failing_application(probe: CounterProbe) -> Component {
    let state = use_signal(CounterState::default);
    *probe.0.lock().unwrap() = Some(state.clone());
    use_wait_for_command();
    let failed = state.clone();
    view! {
        counter_view(state)
        CliCommand {
            command: FAIL,
            on_call: move |_: NoInput| {
                failed.update(|state| {
                    state.calls += 1;
                    state.value += 1;
                }).unwrap();
                Err::<(), _>("storage unavailable")
            },
        }
    }
}

#[tokio::test]
async fn callback_failure_preserves_prior_writes_and_is_not_replayed_on_retry() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || failing_application(root_probe.clone()));
    let mut response = Box::pin(harness.sender.submit(CommandCall::new("fail", json!({}))));
    assert!(poll!(&mut response).is_pending());
    let fault = harness.app.prepare().await.unwrap_err();
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.reason(), ApplicationFaultReason::CommandHandler);
    assert!(
        matches!(response.await, Err(CommandInputError::Handler(message)) if message == "storage unavailable")
    );
    assert_eq!(probe.state().value, 1);
    let response = harness.command("inspect", json!({})).await;
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"value": 1, "calls": 1}))
    );
    assert!(projection_text(&response.projection).contains("<value>1</value>"));
    harness.app.shutdown().await.unwrap();
}

#[component]
fn two_waits() -> Component {
    use_wait_for_command();
    use_wait_for_command();
    view! {}
}

#[tokio::test]
async fn multiple_wait_hooks_are_rejected_before_consuming_commands() {
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (_sender, input) = CommandInput::channel(8);
    let fault = Application::mount_with_commands(two_waits, port, input)
        .err()
        .expect("an application cannot have competing command barriers");
    assert_eq!(fault.stage(), ApplicationFaultStage::Bootstrap);
    assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
}

#[component]
fn child_wait() -> Component {
    use_wait_for_command();
    view! {}
}

#[component]
fn root_with_child_wait() -> Component {
    view! { child_wait() }
}

#[tokio::test]
async fn command_barrier_must_be_declared_by_the_root_component() {
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (_sender, input) = CommandInput::channel(8);
    let fault = Application::mount_with_commands(root_with_child_wait, port, input)
        .err()
        .expect("a child Component cannot own the application command barrier");
    assert_eq!(fault.stage(), ApplicationFaultStage::Bootstrap);
    assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
    assert_eq!(fault.reason(), ApplicationFaultReason::ComponentInvariant);

    // The low-level host retains the authoring diagnostic. ApplicationFault
    // intentionally exposes only its sanitized classification.
    let mut host = agentview::component::ComponentHost::new_root(|()| root_with_child_wait(), ());
    let diagnostic = host.render().err().expect("child wait rejects rendering");
    assert!(diagnostic
        .to_string()
        .contains("use_wait_for_command must be declared by the root Component"));
}

#[component]
fn duplicate_command() -> Component {
    view! {
        CliCommand {
            command: INSPECT,
            on_call: |_: NoInput| Ok::<_, Infallible>(true),
        }
    }
}

#[tokio::test]
async fn duplicate_names_across_child_components_reject_mount() {
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let (_sender, input) = CommandInput::channel(8);
    let result = Application::mount_with_commands(
        || view! { duplicate_command() duplicate_command() },
        port,
        input,
    );
    let fault = result
        .err()
        .expect("duplicate commands must reject the tree");
    assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
}

#[tokio::test]
async fn an_explicit_barrier_without_application_input_returns_a_typed_fault() {
    let (port, _control) = ExternalProviderPort::new().unwrap();
    let mut app = Application::mount(command_barrier, port).unwrap();
    let fault = app.prepare().await.unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::Preparation);
    assert_eq!(fault.reason(), ApplicationFaultReason::CommandInput);
    assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnecting_all_senders_finishes_the_waiting_application() {
    let Harness {
        mut app,
        sender,
        _control,
    } = Harness::mount(command_barrier);
    let mut preparation = Box::pin(app.prepare());
    assert!(poll!(&mut preparation).is_pending());
    drop(sender);
    assert_eq!(
        tokio::time::timeout(TIMEOUT, preparation)
            .await
            .unwrap()
            .unwrap(),
        ControlFlow::Break(ExitReason::Completed)
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_queued_requests_and_future_submissions() {
    let Harness {
        app,
        sender,
        _control,
    } = Harness::mount(command_barrier);
    let mut queued = Box::pin(sender.submit(CommandCall::new("inspect", json!({}))));
    assert!(poll!(&mut queued).is_pending());
    app.shutdown().await.unwrap();
    assert!(matches!(queued.await, Err(CommandInputError::Closed)));
    assert!(matches!(
        sender.submit(CommandCall::new("inspect", json!({}))).await,
        Err(CommandInputError::Closed)
    ));
}

#[tokio::test]
async fn dropping_the_client_response_does_not_retract_its_already_queued_command() {
    let probe = CounterProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || counter(root_probe.clone()));
    let mut response = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("add", json!({"delta": 7}))),
    );
    assert!(poll!(&mut response).is_pending());
    drop(response);
    assert!(harness.app.prepare().await.unwrap().is_continue());
    assert_eq!(probe.state().value, 7);
    let observed = harness.command("inspect", json!({})).await;
    assert_eq!(
        observed.outcome,
        CommandOutcome::Output(json!({"value": 7, "calls": 1}))
    );
    harness.app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct SpawnProbe {
    visible: Arc<Mutex<Option<Signal<bool>>>>,
    local: Arc<Mutex<Option<Signal<usize>>>>,
    started: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

struct TaskDrop(Arc<AtomicBool>);

impl Drop for TaskDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[component]
fn spawning_command(probe: SpawnProbe) -> Component {
    let local = use_signal(|| 0_usize);
    *probe.local.lock().unwrap() = Some(local);
    view! {
        CliCommand {
            command: START,
            on_call: move |_: NoInput| {
                let probe = probe.clone();
                spawn(async move {
                    let _guard = TaskDrop(probe.dropped);
                    probe.started.notify_one();
                    std::future::pending::<()>().await;
                })
            },
        }
    }
}

#[component]
fn spawning_application(probe: SpawnProbe) -> Component {
    let visible = use_signal(|| true);
    *probe.visible.lock().unwrap() = Some(visible.clone());
    use_wait_for_command();
    let action = if visible.with(|value| *value).unwrap() {
        spawning_command(probe)
    } else {
        view! { state { "action removed" } }
    };
    view! {
        { action }
        CliCommand {
            command: INSPECT,
            on_call: |_: NoInput| Ok::<_, Infallible>(true),
        }
    }
}

#[tokio::test]
async fn callbacks_inherit_task_context_and_unmount_awaits_their_spawned_task_cleanup() {
    let probe = SpawnProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || spawning_application(root_probe.clone()));
    harness.command("start", json!({})).await;
    tokio::time::timeout(TIMEOUT, probe.started.notified())
        .await
        .expect("callback starts its mount-owned task");
    let local = probe.local.lock().unwrap().clone().unwrap();
    probe
        .visible
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .set(false)
        .unwrap();
    let response = harness.command("inspect", json!({})).await;
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(local.with(|value| *value).is_err());
    assert!(projection_text(&response.projection).contains("action removed"));
    assert!(!projection_text(&response.projection).contains("command=\"start\" description="));
    harness.app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct SwitchProbe {
    mode: Arc<AtomicBool>,
    a_calls: Arc<AtomicBool>,
    b_calls: Arc<AtomicBool>,
}

#[component]
fn switch_owner_a(probe: SwitchProbe) -> Component {
    let calls = probe.a_calls;
    use_wait_for_command();
    view! {
        CliCommand {
            command: INSPECT,
            on_call: move |_: NoInput| {
                calls.store(true, Ordering::SeqCst);
                Ok::<_, Infallible>("owner-a")
            },
        }
    }
}

#[component]
fn switch_owner_b(probe: SwitchProbe) -> Component {
    let calls = probe.b_calls;
    use_wait_for_command();
    view! {
        CliCommand {
            command: INSPECT,
            on_call: move |_: NoInput| {
                calls.store(true, Ordering::SeqCst);
                Ok::<_, Infallible>("owner-b")
            },
        }
    }
}

fn switch_application(probe: SwitchProbe) -> Component {
    if probe.mode.load(Ordering::SeqCst) {
        view! { switch_owner_b(probe) }
    } else {
        view! { switch_owner_a(probe) }
    }
}

#[tokio::test]
async fn command_received_by_an_old_wait_owner_cannot_run_a_replacement_same_name() {
    let probe = SwitchProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || switch_application(root_probe.clone()));
    let mut preparation = Box::pin(harness.app.prepare());
    assert!(poll!(&mut preparation).is_pending());
    let mut request = Box::pin(
        harness
            .sender
            .submit(CommandCall::new("inspect", json!({}))),
    );
    let _ = poll!(&mut request);
    // The waiting operation still holds owner A's declaration. Change the
    // rendered owner before polling it again; receive records A as the origin
    // and the subsequent reconciliation observes replacement owner B.
    probe.mode.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(TIMEOUT, preparation).await.unwrap();
    let response = request.await.unwrap();
    assert!(
        matches!(response.outcome, CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable")
    );
    assert!(!probe.a_calls.load(Ordering::SeqCst));
    assert!(!probe.b_calls.load(Ordering::SeqCst));
    harness.app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct FailingPrepProbe {
    state: Arc<Mutex<Option<Signal<(bool, bool)>>>>,
}

impl FailingPrepProbe {
    fn signal(&self) -> Signal<(bool, bool)> {
        self.state.lock().unwrap().clone().expect("mounted state")
    }

    fn snapshot(&self) -> (bool, bool) {
        self.signal().with(Clone::clone).unwrap()
    }
}

#[component]
fn failing_after_receive_child(state: Signal<(bool, bool)>) -> Component {
    use_preparation(move || async move {
        if state.with(|state| state.1).unwrap() {
            Err::<(), String>("new preparation failed".to_owned())
        } else {
            Ok::<(), String>(())
        }
    });
    view! { child { "mounted" } }
}

#[component]
fn failing_after_receive_app(probe: FailingPrepProbe) -> Component {
    let state = use_signal(|| (false, true));
    *probe.state.lock().unwrap() = Some(state.clone());
    use_wait_for_command();
    let child = if state.with(|state| state.0).unwrap() {
        failing_after_receive_child(state.clone())
    } else {
        view! {}
    };
    let trigger = state.clone();
    view! {
        { child }
        CliCommand {
            command: START,
            on_call: move |_: NoInput| {
                trigger.update(|state| state.0 = true).unwrap();
                Ok::<_, Infallible>("triggered")
            },
        }
    }
}

#[tokio::test]
async fn newly_mounted_preparation_failure_clears_the_received_command_before_retry() {
    let probe = FailingPrepProbe::default();
    let root_probe = probe.clone();
    let mut harness = Harness::mount(move || failing_after_receive_app(root_probe.clone()));
    let mut first = Box::pin(harness.sender.submit(CommandCall::new("start", json!({}))));
    assert!(poll!(&mut first).is_pending());
    let fault = harness.app.prepare().await.unwrap_err();
    assert_eq!(fault.reason(), ApplicationFaultReason::Preparation);
    assert!(matches!(
        first.await,
        Err(CommandInputError::PreparationFailed)
    ));
    assert!(
        probe.snapshot().1,
        "the newly mounted preparation remains the failing gate"
    );

    probe.signal().update(|state| state.1 = false).unwrap();
    let second = harness.command("start", json!({})).await;
    assert_eq!(second.outcome, CommandOutcome::Output(json!("triggered")));
    harness.app.shutdown().await.unwrap();
}

#[component]
fn repeated_command_owner() -> Component {
    use_wait_for_command();
    view! {
        CliCommand {
            command: INSPECT,
            on_call: |_: NoInput| Ok::<_, Infallible>(true),
        }
    }
}

fn repeated_command_app() -> Component {
    view! {
        #[developer(repeat)]
        repeated_command_owner()
    }
}

#[tokio::test]
async fn repeated_developer_command_owner_places_feedback_in_developer_instructions() {
    let mut harness = Harness::mount(repeated_command_app);
    let response = harness.command("inspect", json!({})).await;
    let mut found = false;
    for node in response.projection.nodes() {
        for item in node.items() {
            if let CanonicalInputItem::Instruction { authority, pom } = item {
                let text = render_pom_document(pom).unwrap();
                if text.contains("command_feedback") {
                    found = true;
                    assert_eq!(*authority, InstructionAuthority::Developer);
                    assert!(!text.contains("<user>"));
                }
            }
        }
    }
    assert!(
        found,
        "command feedback is included in the prepared projection"
    );
    harness.app.shutdown().await.unwrap();
}
