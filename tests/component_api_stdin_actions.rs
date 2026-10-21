#[path = "support/pom.rs"]
mod pom_support;
use pom_support::*;

use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::component::{
    execution::{
        Application, ApplicationFaultReason, CommandCall, CommandInput, CommandInputError,
        CommandOutcome, CommandResponse, CommandSender, ExternalControl, ExternalProviderPort,
    },
    prelude::*,
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
    let initial = projection_document(harness.app.current_projection().projection());
    assert!(initial.has_field("total", "0"));
    assert!(initial.has_attribute("name", "add"));
    assert!(initial.has_attribute("description", "Add an amount and display the updated total"));
    let schemas = initial.elements("input_schema");
    let schema: Value = serde_json::from_str(&direct_text(schemas[0])).unwrap();
    assert_eq!(schema["required"], json!(["amount"]));
    assert_eq!(
        schema["properties"]["amount"]["description"],
        "Amount to add to the current total."
    );
    assert!(
        schema["properties"].get("delta").is_none(),
        "serde rename defines the input"
    );

    let added = harness.action("add", json!({"amount": 10})).await;
    assert_eq!(
        added.outcome,
        CommandOutcome::Output(json!({"total": 10, "observed": 0, "reason": ""}))
    );
    let updated = projection_document(&added.projection);
    assert!(updated.has_field("total", "10"));
    assert!(updated.has_attribute("enabled", "false"));
    assert!(updated.has_element("command_result"));

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
    let rejected_view = projection_document(&adjusted.projection);
    assert!(rejected_view.has_field("total", "10"));
    assert!(rejected_view.has_attribute("code", "command_disabled"));
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
        let feedback = projection_document(&response.projection);
        assert!(feedback.has_attribute("code", "invalid_arguments"));
        assert!(feedback.has_field("total", "0"));
        assert!(feedback.has_attribute("name", "add"));
    }
    let response = harness.action("add", json!({"amount": 3})).await;
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"total": 3, "observed": 0, "reason": ""}))
    );
    assert!(projection_document(&response.projection).has_field("total", "3"));
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
    }
    let response = harness.action("inspect", json!({})).await;
    assert_eq!(
        response.outcome,
        CommandOutcome::Output(json!({"total": 0}))
    );
    harness.app.shutdown().await.unwrap();
}

#[component]
fn dynamic_action_names() -> Component {
    let generation = use_signal(|| 0_u64);
    let current = generation.with(|generation| *generation).unwrap();
    let name = format!("choice_{current}");
    use_wait_for_command();
    view! {
        Action {
            name: name,
            on_call: move || generation.update(|generation| {
                *generation += 1;
                json!({"generation": *generation})
            }),
        }
    }
}

#[tokio::test]
async fn dynamic_action_names_dispatch_and_renaming_retires_the_previous_choice() {
    let mut harness = Harness::with_root(dynamic_action_names);
    assert_eq!(
        harness.action("choice_0", json!({})).await.outcome,
        CommandOutcome::Output(json!({"generation": 1}))
    );
    assert!(matches!(
        harness.action("choice_0", json!({})).await.outcome,
        CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable"
    ));
    assert_eq!(
        harness.action("choice_1", json!({})).await.outcome,
        CommandOutcome::Output(json!({"generation": 2}))
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
    assert!(projection_document(&response.projection).has_field("total", "4"));
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

#[cfg(unix)]
mod process_frames {
    // Build this fixture before running the process tests:
    // cargo build --locked --no-default-features --example command_feedback_cli
    // AGENTVIEW_STDIN_CLI_TEST_BINARY can select an explicitly built fixture.
    use std::{
        path::PathBuf,
        process::{ExitStatus, Stdio},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use agentview::component::execution::StdinResponse;
    use serde::Deserialize;
    use serde_json::{json, Value};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        process::{Child, ChildStdin, ChildStdout, Command},
        task::{JoinHandle, JoinSet},
        time::timeout,
    };

    const PROCESS_TIMEOUT: Duration = Duration::from_secs(25);
    const IO_TIMEOUT: Duration = Duration::from_secs(6);
    const EOF_TIMEOUT: Duration = Duration::from_millis(1500);
    static NEXT_SESSION: AtomicUsize = AtomicUsize::new(0);

    struct Session {
        binary: PathBuf,
        directory: PathBuf,
        socket: PathBuf,
        daemon: Child,
        proxy: JoinHandle<()>,
        completed_feedback: Arc<AtomicUsize>,
    }

    impl Session {
        async fn start() -> Self {
            let binary = std::env::var_os("AGENTVIEW_STDIN_CLI_TEST_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::current_exe()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("examples/command_feedback_cli")
                });
            assert!(
                binary.is_file(),
                "build the fixture with cargo build --locked --no-default-features --example command_feedback_cli; missing {}",
                binary.display()
            );
            let directory = std::env::temp_dir().join(format!(
                "av-stdin-{}-{}",
                std::process::id(),
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let daemon_socket = directory.join("daemon-socket");
            let daemon = Command::new(&binary)
                .env("XDG_STATE_HOME", directory.join("state"))
                .arg("--socket")
                .arg(&daemon_socket)
                .arg("__daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            timeout(IO_TIMEOUT, async {
                loop {
                    if UnixStream::connect(&daemon_socket).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("daemon listens before the frontend starts");
            let socket = directory.join("socket");
            let listener = UnixListener::bind(&socket).unwrap();
            let completed_feedback = Arc::new(AtomicUsize::new(0));
            let feedback = Arc::clone(&completed_feedback);
            // Observe native daemon acknowledgements without inspecting the
            // rendered POM carried as an opaque field in each JSON response.
            let proxy = tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (frontend, _) = accepted.unwrap();
                            let daemon_socket = daemon_socket.clone();
                            let feedback = Arc::clone(&feedback);
                            connections.spawn(async move {
                                let mut frontend = BufReader::new(frontend);
                                let mut daemon = BufReader::new(
                                    UnixStream::connect(daemon_socket).await.unwrap()
                                );
                                loop {
                                    let mut request = Vec::new();
                                    if frontend.read_until(b'\n', &mut request).await.unwrap() == 0 {
                                        return;
                                    }
                                    let operation: Value = serde_json::from_slice(&request).unwrap();
                                    daemon.get_mut().write_all(&request).await.unwrap();
                                    let mut response = Vec::new();
                                    if daemon.read_until(b'\n', &mut response).await.unwrap() == 0 {
                                        return;
                                    }
                                    frontend.get_mut().write_all(&response).await.unwrap();
                                    if operation["kind"] == "feedback" {
                                        feedback.fetch_add(1, Ordering::SeqCst);
                                    }
                                }
                            });
                        }
                        Some(result) = connections.join_next() => result.unwrap(),
                    }
                }
            });
            let session = Self {
                binary,
                socket,
                directory,
                daemon,
                proxy,
                completed_feedback,
            };
            session.lifecycle("start").await;
            session
        }

        fn command(&self) -> Command {
            let mut command = Command::new(&self.binary);
            command
                .env("XDG_STATE_HOME", self.directory.join("state"))
                .arg("--socket")
                .arg(&self.socket)
                .kill_on_drop(true);
            command
        }

        async fn lifecycle(&self, action: &str) {
            let output = timeout(IO_TIMEOUT, self.command().arg(action).output())
                .await
                .expect("lifecycle command completes")
                .unwrap();
            assert!(
                output.status.success(),
                "{action}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        async fn interact(&self) -> Frontend {
            let mut child = self
                .command()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut frontend = Frontend {
                stdin: child.stdin.take(),
                stdout: child.stdout.take().unwrap(),
                child,
            };
            // Output is available before the caller supplies any input. A pipe
            // read does not identify a view boundary or a completed write.
            frontend.output_available().await;

            frontend
        }

        async fn inspect(&self) -> Value {
            #[derive(Deserialize)]
            struct Response {
                output: StdinResponse,
            }
            let mut connection = BufReader::new(UnixStream::connect(&self.socket).await.unwrap());
            let mut request = serde_json::to_vec(&json!({
                "kind": "action", "call": {"action": "inspect", "input": {}}
            }))
            .unwrap();
            request.push(b'\n');
            connection.get_mut().write_all(&request).await.unwrap();
            let mut reply = Vec::new();
            timeout(IO_TIMEOUT, connection.read_until(b'\n', &mut reply))
                .await
                .unwrap()
                .unwrap();
            let response: Result<Response, String> = serde_json::from_slice(&reply).unwrap();
            let response = response.unwrap().output;
            assert!(response.ok);
            response
                .result
                .expect("inspect returns current business state")
        }

        async fn wait_for_state(&self, expected: Value) {
            timeout(IO_TIMEOUT, async {
                loop {
                    if self.inspect().await == expected {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the frontend submitted its actions while stdin remained open");
        }

        async fn wait_for_rejected_frame(&self) {
            timeout(IO_TIMEOUT, async {
                while self.completed_feedback.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the daemon acknowledged the rejected input frame");
        }

        async fn stop(&mut self) {
            self.lifecycle("stop").await;
            assert!(timeout(IO_TIMEOUT, self.daemon.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.proxy.abort();
            let _ = self.daemon.start_kill();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    struct Frontend {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: ChildStdout,
    }

    impl Frontend {
        async fn write(&mut self, input: &str) {
            let stdin = self.stdin.as_mut().expect("stdin is open");
            stdin.write_all(input.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        async fn output_available(&mut self) {
            let mut output = [0; 4096];
            let read = timeout(IO_TIMEOUT, self.stdout.read(&mut output))
                .await
                .unwrap()
                .unwrap();
            assert!(read > 0, "the frontend produces opaque output bytes");
        }

        async fn finish(mut self) -> ExitStatus {
            self.stdin.take();
            let mut extra = Vec::new();
            timeout(IO_TIMEOUT, self.stdout.read_to_end(&mut extra))
                .await
                .unwrap()
                .unwrap();
            timeout(IO_TIMEOUT, self.child.wait())
                .await
                .unwrap()
                .unwrap()
        }

        async fn finish_at_eof(self) -> ExitStatus {
            timeout(EOF_TIMEOUT, self.finish())
                .await
                .expect("EOF completes the pending input without waiting for idle")
        }
    }

    #[tokio::test]
    async fn stdout_failure_exits_while_stdin_remains_open_without_replaying_the_action() {
        timeout(PROCESS_TIMEOUT, async {
            let mut session = Session::start().await;
            let mut frontend = session.interact().await;
            let mut stdin = frontend.stdin.take().unwrap();
            let mut child = frontend.child;
            // The callback must finish before stdout discovers its reader is
            // gone. Keep the stdin writer alive throughout the process wait.
            drop(frontend.stdout);
            stdin
                .write_all(
                    b"{\"action\":\"record\",\"input\":{\"text\":\"one accepted action\"}}\n",
                )
                .await
                .unwrap();
            stdin.flush().await.unwrap();
            let status = timeout(IO_TIMEOUT, child.wait())
                .await
                .expect("a failed frontend exits without waiting for stdin EOF")
                .unwrap();
            assert_eq!(status.code(), Some(1));
            drop(stdin);

            session.lifecycle("status").await;
            assert_eq!(
                session.inspect().await,
                json!({"count": 1, "latest": "one accepted action"})
            );

            session.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn idle_flushes_ordered_frames_without_closing_the_stdin_connection() {
        timeout(PROCESS_TIMEOUT, async {
            let mut session = Session::start().await;
            let mut frontend = session.interact().await;
            frontend
                .write(concat!(
                    "{\n  \"action\": \"record\",\n  \"input\": {\"text\": \"first idle note\"}\n}\n",
                    "{\"action\":\"inspect\",\"input\":{}}\n",
                ))
                .await;

            session
                .wait_for_state(json!({"count": 1, "latest": "first idle note"}))
                .await;
            frontend.output_available().await;

            assert!(frontend.child.try_wait().unwrap().is_none());

            frontend.write("{\"action\":\"record\",\"input\":{\"text\":\"second idle note\"}}\n").await;
            session
                .wait_for_state(json!({"count": 2, "latest": "second idle note"}))
                .await;
            frontend.output_available().await;

            assert!(frontend.child.try_wait().unwrap().is_none());

            frontend.write("{\"action\":\"record\",\"input\":{\"text\":\"EOF note\"}}").await;
            assert!(frontend.finish_at_eof().await.success());
            assert_eq!(session.inspect().await, json!({"count": 3, "latest": "EOF note"}));

            let observation = session.interact().await;

            assert!(observation.finish().await.success());
            session.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_frontend_can_idle_past_the_transfer_timeout_and_stop_with_stdin_open() {
        timeout(PROCESS_TIMEOUT, async {
            let mut session = Session::start().await;
            let mut frontend = session.interact().await;
            frontend
                .write("{\"action\":\"record\",\"input\":{\"text\":\"before thinking\"}}\n")
                .await;

            session
                .wait_for_state(json!({"count": 1, "latest": "before thinking"}))
                .await;
            frontend.output_available().await;
            // Thinking between batches is not an in-progress socket transfer.
            tokio::time::sleep(Duration::from_secs(11)).await;
            frontend
                .write("{\"action\":\"record\",\"input\":{\"text\":\"after thinking\"}}\n")
                .await;
            session
                .wait_for_state(json!({"count": 2, "latest": "after thinking"}))
                .await;
            frontend.output_available().await;

            // An idle connection must not hold daemon shutdown open.
            session.stop().await;
            assert!(frontend.finish().await.success());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_unterminated_final_line_keeps_its_complete_prefix_buffered() {
        timeout(PROCESS_TIMEOUT, async {
            let mut session = Session::start().await;
            let mut frontend = session.interact().await;
            frontend
                .write(concat!(
                    "{\"action\":\"record\",\"input\":{\"text\":\"complete prefix\"}}\n",
                    "{\"action\":\"record\",\"input\":{\"text\":\"partial suffix",
                ))
                .await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(session.inspect().await["count"], 0);
            assert!(frontend.child.try_wait().unwrap().is_none());
            frontend.write("\"}}\n").await;
            assert!(frontend.finish_at_eof().await.success());
            assert_eq!(session.inspect().await["count"], 2);

            session.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn newline_terminated_incomplete_json_rejects_the_frame_and_allows_recovery() {
        timeout(PROCESS_TIMEOUT, async {
            let mut session = Session::start().await;
            let mut frontend = session.interact().await;
            frontend
                .write(concat!(
                    "{\"action\":\"record\",\"input\":{\"text\":\"must not execute\"}}\n",
                    "{\"action\":\"record\",\"input\":{\n",
                ))
                .await;
            session.wait_for_rejected_frame().await;
            assert_eq!(session.inspect().await["count"], 0);

            assert!(frontend.child.try_wait().unwrap().is_none());

            frontend
                .write("{\"action\":\"record\",\"input\":{\"text\":\"after rejection\"}}\n")
                .await;
            session
                .wait_for_state(json!({"count": 1, "latest": "after rejection"}))
                .await;
            frontend.output_available().await;

            assert_eq!(frontend.finish().await.code(), Some(2));

            session.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn invalid_frames_have_no_prefix_effects_but_typed_rejections_allow_later_actions() {
        timeout(PROCESS_TIMEOUT, async {
            let mut session = Session::start().await;
            for invalid_suffix in [
                "{not JSON}",
                "{\"action\":\"record\",\"input\":[]}",
                "{\"action\":\"record\",\"input\":{\"text\":\"unfinished",
            ] {
                let mut frontend = session.interact().await;
                frontend
                    .write("{\"action\":\"record\",\"input\":{\"text\":\"must not execute\"}}\n")
                    .await;
                frontend.write(invalid_suffix).await;
                assert_eq!(frontend.finish_at_eof().await.code(), Some(2));
                assert_eq!(session.inspect().await["count"], 0);
            }

            let mut frontend = session.interact().await;
            frontend
                .write(concat!(
                    "{\"action\":\"record\",\"input\":{\"text\":\"accepted before rejection\"}}\n",
                    "{\"action\":\"record\",\"input\":{}}\n",
                    "{\"action\":\"record\",\"input\":{\"text\":\"accepted after rejection\"}}\n",
                ))
                .await;
            assert_eq!(frontend.finish_at_eof().await.code(), Some(2));
            assert_eq!(session.inspect().await["count"], 2);

            session.stop().await;
        })
        .await
        .unwrap();
    }
}
