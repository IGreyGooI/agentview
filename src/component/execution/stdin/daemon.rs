//! Unix daemon transport and application lifetime. No business logic lives here.

use std::{
    env,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{sleep, timeout, Instant},
};

use super::super::CommandCall;
use super::daemon_history::{EventKind, History};
use super::{StdinApplication, StdinDelivery, StdinProjectionResponse, StdinResponse};
use crate::component::authoring::Component;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Response {
    pub output: StdinResponse,
    pub daemon_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_session_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Operation {
    Observe,
    Action { call: CommandCall },
    Frame { calls: Vec<CommandCall> },
    Stop,
    Feedback { message: String },
}

/// Flattening preserves the legacy Operation wire format. Old clients simply
/// omit the optional foreground run association.
#[derive(Debug, Serialize, Deserialize)]
struct SubmittedOperation<T> {
    #[serde(flatten)]
    operation: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli_run_id: Option<String>,
}

struct Request {
    pub operation: Operation,
    pub request_id: Option<u64>,
    pub reply: oneshot::Sender<Result<PreparedResponse, String>>,
}

#[derive(Debug)]
struct PreparedResponse {
    output: StdinProjectionResponse,
    daemon_pid: u32,
    daemon_session_id: Option<String>,
    request_id: Option<u64>,
}

type ShutdownReply = (
    oneshot::Sender<Result<PreparedResponse, String>>,
    PreparedResponse,
);

/// One frontend's delivery session. Never reconnect after submitting a request:
/// application effects may already have happened when its response is lost.
pub(super) struct Connection {
    stream: BufReader<UnixStream>,
    cli_run_id: Option<String>,
}

impl Connection {
    pub(super) async fn connect_for_run(
        socket: &Path,
        autostart: bool,
        cli_run_id: Option<&str>,
    ) -> Result<Option<Self>> {
        Ok(connect(socket, autostart).await?.map(|stream| Self {
            stream: BufReader::new(stream),
            cli_run_id: cli_run_id.map(str::to_owned),
        }))
    }

    pub(super) async fn request(&mut self, operation: &Operation) -> Result<Response> {
        exchange_for_run(&mut self.stream, operation, self.cli_run_id.as_deref()).await
    }
}

pub(super) async fn request_for_run(
    socket: &Path,
    operation: &Operation,
    autostart: bool,
    cli_run_id: Option<&str>,
) -> Result<Option<Response>> {
    let Some(mut connection) = Connection::connect_for_run(socket, autostart, cli_run_id).await?
    else {
        return Ok(None);
    };
    connection.request(operation).await.map(Some)
}

#[cfg(test)]
async fn exchange(stream: &mut BufReader<UnixStream>, operation: &Operation) -> Result<Response> {
    exchange_for_run(stream, operation, None).await
}

async fn exchange_for_run(
    stream: &mut BufReader<UnixStream>,
    operation: &Operation,
    cli_run_id: Option<&str>,
) -> Result<Response> {
    // Connect retries precede submission. Never replay a submitted action after
    // a write or response failure: it may already have changed application state.
    let submitted = SubmittedOperation {
        operation,
        cli_run_id: cli_run_id.map(str::to_owned),
    };
    timeout(IO_TIMEOUT, write_json(stream.get_mut(), &submitted))
        .await
        .context(
            "command write timed out; inspect application state before retrying the action",
        )??;

    // A quiet connection while an async callback runs is not stalled socket I/O.
    // Bound the queue/callback wait separately, then retain the short transfer
    // timeout once the daemon starts delivering its response.
    let first_bytes = timeout(RESPONSE_TIMEOUT, stream.fill_buf())
        .await
        .context(
        "response timed out after 5 minutes; inspect application state before retrying the action",
    )??;
    ensure!(
        !first_bytes.is_empty(),
        "application connection closed; inspect application state before retrying the action"
    );
    let response: Result<Response, String> =
        timeout(IO_TIMEOUT, read_json(stream)).await.context(
            "response read timed out; inspect application state before retrying the action",
        )??;
    response.map_err(anyhow::Error::msg)
}

async fn connect(socket: &Path, autostart: bool) -> Result<Option<UnixStream>> {
    match UnixStream::connect(socket).await {
        Ok(stream) => return Ok(Some(stream)),
        Err(error) if absent(&error) && autostart => {}
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => return Err(error).context("connect to application daemon"),
    }
    create_directory(socket)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(sidecar(socket, "log"))?;
    let mut child = Command::new(env::current_exe()?)
        .arg("--socket")
        .arg(socket)
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("start application daemon")?;
    let connection = async {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => return Ok(Some(stream)),
                Err(error) if absent(&error) && Instant::now() < deadline => {
                    if let Some(status) = child.try_wait()? {
                        ensure!(
                            status.success(),
                            "daemon startup failed; inspect {}",
                            sidecar(socket, "log").display()
                        );
                    }
                    sleep(Duration::from_millis(25)).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "daemon unavailable; inspect {}",
                            sidecar(socket, "log").display()
                        )
                    })
                }
            }
        }
    }
    .await;
    // Reap exited daemons and losing starters while the frontend stays open.
    // Tokio's default kill_on_drop=false lets the daemon survive frontend EOF.
    tokio::spawn(async move {
        if let Err(error) = child.wait().await {
            eprintln!("agentview: reap daemon: {error}");
        }
    });
    connection
}

fn absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn create_directory(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .context("socket path needs a parent directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    Ok(())
}

fn sidecar(socket: &Path, suffix: &str) -> PathBuf {
    let mut path = socket.as_os_str().to_owned();
    path.push(format!(".{suffix}"));
    PathBuf::from(path)
}

struct SocketOwner {
    path: PathBuf,
    _lock: File,
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) async fn serve(
    socket: &Path,
    root: impl Fn() -> Component + Send + Sync + 'static,
) -> Result<()> {
    create_directory(socket)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(sidecar(socket, "lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(error).context("lock daemon socket"),
    }
    // Only the lock owner may remove a stale endpoint. Keep the lock file itself
    // in place so concurrent launchers always lock the same inode.
    match fs::symlink_metadata(socket) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket(),
                "socket path is occupied by a non-socket file"
            );
            fs::remove_file(socket)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect socket path"),
    }
    let listener = UnixListener::bind(socket).context("bind application socket")?;
    let socket_owner = SocketOwner {
        path: socket.to_owned(),
        _lock: lock,
    };
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let history = History::create(&env::current_exe()?, socket)
        .context("create daemon history before mounting application")?;
    let (sender, receiver) = mpsc::channel(16);
    let mut application = tokio::spawn(run_with_history(receiver, root, Some(history.clone())));
    let mut clients = JoinSet::new();
    let mut application_finished = false;
    let outcome = loop {
        tokio::select! {
            result = &mut application => {
                application_finished = true;
                break joined_application(result);
            }
            incoming = listener.accept() => {
                let (stream, _) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => break Err(error.into()),
                };
                let sender = sender.clone();
                let history = history.clone();
                clients.spawn(async move {
                    if let Err(error) = handle_client_with_history(stream, sender, Some(history)).await {
                        eprintln!("agentview client: {error:#}");
                    }
                });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result {
                    eprintln!("agentview client task: {error}");
                }
            }
            result = tokio::signal::ctrl_c() => break result.context("receive interrupt").map(|()| None),
        }
    };
    drop(listener);
    drop(sender);
    let normal_stop = matches!(&outcome, Ok(Some(_)));
    let mut outcome = outcome;
    if !application_finished {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        if let Err(error) = joined_application(application.await) {
            outcome = Err(error);
        }
    }
    // Release the listening endpoint and lock before acknowledging shutdown,
    // so an immediately following CLI invocation can start a fresh session.
    drop(socket_owner);
    let mut outcome = match outcome {
        Ok(Some((reply, response))) => {
            let _ = reply.send(Ok(response));
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    // In particular, let the shutdown response reach its client after cleanup.
    while let Some(result) = clients.join_next().await {
        if let Err(error) = result {
            outcome = Err(anyhow::Error::from(error).context("client task failed"));
        }
    }
    history
        .finish(if outcome.is_err() {
            "failed"
        } else if normal_stop {
            "stopped"
        } else {
            "interrupted"
        })
        .context("finish daemon history")?;
    outcome
}

fn joined_application(
    result: std::result::Result<Result<Option<ShutdownReply>>, tokio::task::JoinError>,
) -> Result<Option<ShutdownReply>> {
    match result {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(error).context("Application task was cancelled"),
    }
}

#[cfg(test)]
async fn handle_client(stream: UnixStream, sender: mpsc::Sender<Request>) -> Result<()> {
    handle_client_with_history(stream, sender, None).await
}

async fn handle_client_with_history(
    stream: UnixStream,
    sender: mpsc::Sender<Request>,
    history: Option<History>,
) -> Result<()> {
    let connection_id = match &history {
        Some(history) => history
            .open_connection()
            .context("record daemon connection")?,
        None => 0,
    };
    let mut cli_run_id = None;
    let result = handle_requests(
        stream,
        sender,
        history.as_ref(),
        connection_id,
        &mut cli_run_id,
    )
    .await;
    if let Some(history) = &history {
        history
            .record(EventKind::ConnectionClosed {
                connection_id,
                cli_run_id,
                reason: match &result {
                    Ok(()) => "closed".to_owned(),
                    Err(error) => format!("{error:#}"),
                },
            })
            .context("record daemon connection closure")?;
    }
    result
}

async fn handle_requests(
    stream: UnixStream,
    sender: mpsc::Sender<Request>,
    history: Option<&History>,
    connection_id: u64,
    cli_run_id: &mut Option<String>,
) -> Result<()> {
    let mut stream = BufReader::new(stream);
    let mut delivery = StdinDelivery::new()?;
    loop {
        // Waiting for a caller to send its next frame is not stalled socket I/O.
        // Receiver closure also wakes idle connections when the daemon stops.
        let first_bytes = tokio::select! {
            biased;
            _ = sender.closed() => return Ok(()),
            first_bytes = stream.fill_buf() => first_bytes?,
        };
        if first_bytes.is_empty() {
            return Ok(());
        }
        let operation = tokio::select! {
            biased;
            _ = sender.closed() => return Ok(()),
            operation = timeout(IO_TIMEOUT, read_json::<SubmittedOperation<Operation>>(&mut stream)) => {
                operation.context("command read timed out")?
            }
        };
        let stopping = matches!(
            &operation,
            Ok(SubmittedOperation {
                operation: Operation::Stop,
                ..
            })
        );
        let mut request_id = None;
        let prepared: Result<PreparedResponse, String> = match operation {
            Ok(submitted) => {
                *cli_run_id = submitted.cli_run_id;
                if let Some(history) = history {
                    request_id = Some(
                        history
                            .accept_request(
                                connection_id,
                                cli_run_id.clone(),
                                serde_json::to_value(&submitted.operation)?,
                            )
                            .context(
                                "record request before execution; request was not submitted",
                            )?,
                    );
                }
                let (reply, received) = oneshot::channel();
                match sender
                    .send(Request {
                        operation: submitted.operation,
                        request_id,
                        reply,
                    })
                    .await
                {
                    Ok(()) => received
                        .await
                        .unwrap_or_else(|_| Err("Application stopped".to_owned())),
                    Err(_) => {
                        if let (Some(history), Some(request_id)) = (history, request_id) {
                            history.record(EventKind::RequestNotExecuted {
                                request_id,
                                message: "Application stopped before accepting this request into its execution queue".to_owned(),
                            }).context("record request rejected by the closed execution queue")?;
                        }
                        Err("Application stopped before this request executed".to_owned())
                    }
                }
            }
            Err(error) => {
                let message = format!("invalid command: {error}");
                if let Some(history) = history {
                    history
                        .record(EventKind::ProtocolRejected {
                            connection_id,
                            message: message.clone(),
                        })
                        .context("record rejected daemon protocol message")?;
                }
                Err(message)
            }
        };
        let (response, commit): (Result<Response, String>, _) = match prepared {
            Ok(prepared) => match delivery.prepare(prepared.output) {
                Ok((output, commit)) => (
                    Ok(Response {
                        output,
                        daemon_pid: prepared.daemon_pid,
                        daemon_session_id: prepared.daemon_session_id,
                    }),
                    Some(commit),
                ),
                Err(error) => (Err(error.to_string()), None),
            },
            Err(error) => (Err(error), None),
        };
        let close = stopping || response.is_err();
        let written = timeout(IO_TIMEOUT, write_json(stream.get_mut(), &response))
            .await
            .context("response write timed out")
            .and_then(|result| result);
        if let (Some(history), Some(request_id)) = (history, request_id) {
            history
                .record(match &written {
                    Ok(()) => EventKind::DeliveryWritten {
                        request_id,
                        connection_id,
                        response_error: response.as_ref().err().cloned(),
                    },
                    Err(error) => EventKind::DeliveryFailed {
                        request_id,
                        connection_id,
                        message: format!("{error:#}"),
                    },
                })
                .context("record socket write outcome; inspect daemon history before retrying")?;
        }
        written?;
        if let Some(commit) = commit {
            // No await between successful delivery and baseline publication.
            delivery.commit(commit);
        }
        if close {
            return Ok(());
        }
    }
}

async fn read_json<T: DeserializeOwned>(stream: &mut (impl AsyncBufRead + Unpin)) -> Result<T> {
    let mut bytes = Vec::new();
    stream.read_until(b'\n', &mut bytes).await?;
    ensure!(
        bytes.last() == Some(&b'\n'),
        "expected one newline-terminated JSON message"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write_json(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    Ok(())
}

#[cfg(test)]
async fn run(
    receiver: mpsc::Receiver<Request>,
    root: impl Fn() -> Component + Send + Sync + 'static,
) -> Result<Option<ShutdownReply>> {
    run_with_history(receiver, root, None).await
}

async fn run_with_history(
    mut receiver: mpsc::Receiver<Request>,
    root: impl Fn() -> Component + Send + Sync + 'static,
    history: Option<History>,
) -> Result<Option<ShutdownReply>> {
    let mut application = StdinApplication::mount(root)?;
    let operation = async {
        while let Some(Request { operation, request_id, reply }) = receiver.recv().await {
            if let (Some(history), Some(request_id)) = (&history, request_id) {
                if let Err(error) = history.start_execution(request_id) {
                    let message = format!("daemon history unavailable; this request was not executed: {error}");
                    let _ = reply.send(Err(message.clone()));
                    return Err(anyhow::anyhow!(message));
                }
            }
            let stopping = matches!(&operation, Operation::Stop);
            let response = match operation {
                Operation::Action { call } => application.submit_projection(call).await,
                Operation::Frame { calls } => application.submit_frame_projection(calls).await,
                Operation::Observe | Operation::Stop => application.observe_projection().await,
                Operation::Feedback { message } => {
                    application
                        .feedback_projection("invalid_input", message)
                        .await
                }
            };
            let response = response.map(|output| PreparedResponse {
                output,
                daemon_pid: std::process::id(),
                daemon_session_id: history.as_ref().map(|history| history.id().to_owned()),
                request_id,
            });
            match response {
                Ok(response) if stopping => return Ok(Some((reply, response))),
                Ok(response) => {
                    if let Err(error) = record_execution_completed(history.as_ref(), &response) {
                        let _ = reply.send(Err(format!("{error:#}")));
                        return Err(error);
                    }
                    let _ = reply.send(Ok(response));
                }
                Err(error) => {
                    let mut message = error.to_string();
                    if let (Some(history), Some(request_id)) = (&history, request_id) {
                        if let Err(recording) = history.record(EventKind::ExecutionFailed {
                            request_id,
                            message: message.clone(),
                        }) {
                            message = format!("{message}; could not persist the execution failure: {recording}; effects may have occurred, inspect state before retrying");
                        }
                    }
                    let _ = reply.send(Err(message));
                    return Err(anyhow::Error::from(error));
                }
            }
        }
        Ok(None)
    }
    .await;
    // Once this owner exits its execution loop, no remaining queue entry can
    // run. Distinguish that known outcome from requests interrupted by a crash.
    receiver.close();
    let mut rejection_failure = None;
    while let Some(Request {
        request_id, reply, ..
    }) = receiver.recv().await
    {
        let mut message = "Application stopped before this request executed".to_owned();
        if let (Some(history), Some(request_id)) = (&history, request_id) {
            if let Err(error) = history.record(EventKind::RequestNotExecuted {
                request_id,
                message: message.clone(),
            }) {
                message = format!("{message}; could not persist the queue rejection: {error}");
                rejection_failure.get_or_insert(error);
            }
        }
        let _ = reply.send(Err(message));
    }
    let cleanup = application.shutdown().await;
    if let Some(recording) = rejection_failure {
        let error =
            anyhow::Error::from(recording).context("could not persist rejected queued requests");
        if let Ok(Some((reply, _))) = operation {
            let _ = reply.send(Err(format!("{error:#}")));
        }
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => {
                Err(error.context(format!("Application shutdown also failed: {cleanup}")))
            }
        };
    }
    match (operation, cleanup) {
        (Ok(Some((reply, response))), Ok(())) => {
            if let Err(error) = record_execution_completed(history.as_ref(), &response) {
                let _ = reply.send(Err(format!("{error:#}")));
                return Err(error);
            }
            Ok(Some((reply, response)))
        }
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(Some((reply, response))), Err(error)) => {
            let mut message = format!("Application shutdown failed: {error}");
            if let (Some(history), Some(request_id)) = (&history, response.request_id) {
                if let Err(recording) = history.record(EventKind::ExecutionFailed {
                    request_id,
                    message: message.clone(),
                }) {
                    message =
                        format!("{message}; could not persist the shutdown failure: {recording}");
                }
            }
            let _ = reply.send(Err(message));
            Err(error.into())
        }
        (Ok(None), Err(error)) => Err(error.into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Application shutdown also failed: {cleanup}")))
        }
    }
}

fn record_execution_completed(
    history: Option<&History>,
    response: &PreparedResponse,
) -> Result<()> {
    if let (Some(history), Some(request_id)) = (history, response.request_id) {
        // The retained projection is a complete observation, before any private
        // connection cursor lowers it to a Delta. Rendering never parses POM.
        let output = StdinResponse {
            ok: response.output.ok,
            result: response.output.result.clone(),
            view: super::render(&response.output.projection).context(
                "could not record the complete observation after execution; effects may have occurred, inspect state before retrying",
            )?,
        };
        history.record(EventKind::ExecutionCompleted { request_id, output }).context(
            "could not persist the result after execution; effects may have occurred, inspect state and daemon history before retrying",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use crate::component::prelude::*;
    use serde_json::json;

    use super::*;

    #[component]
    fn transport_actions(calls: Arc<AtomicUsize>, delay: Duration) -> Component {
        use_wait_for_command();
        let completed = use_signal(|| 0_usize);
        let count = completed.with(|completed| *completed).unwrap();
        view! {
            completed { "{count}" }
            Action {
                name: "work",
                on_call: move || {
                    let calls = calls.clone();
                    let completed = completed.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        sleep(delay).await;
                        completed.update(|completed| {
                            *completed += 1;
                            json!({"completed": *completed})
                        })
                    }
                },
            }
        }
    }

    async fn round_trip(sender: mpsc::Sender<Request>, operation: &Operation) -> Response {
        let (frontend, daemon) = UnixStream::pair().unwrap();
        let mut frontend = BufReader::new(frontend);
        let connection = tokio::spawn(handle_client(daemon, sender));
        let response = exchange(&mut frontend, operation).await.unwrap();
        drop(frontend);
        connection.await.unwrap().unwrap();
        response
    }

    #[tokio::test]
    async fn large_frame_and_result_cross_the_socket_without_losing_content() {
        let input = json!({"text": "长期记忆\\\"\n".repeat(2048)});
        let operation = Operation::Frame {
            calls: vec![
                CommandCall::new("remember", input.clone()),
                CommandCall::new("inspect", json!({})),
            ],
        };
        assert!(serde_json::to_vec(&operation).unwrap().len() > 8192);
        let result = json!({"text": "完整查询结果\\\"\n".repeat(16 * 1024)});
        let view = "完整视图\\\"\n".repeat(16 * 1024);
        assert!(serde_json::to_vec(&result).unwrap().len() > 128 * 1024);
        assert!(view.len() > 128 * 1024);
        let expected_result = result.clone();
        let (sender, mut receiver) = mpsc::channel::<Request>(1);
        let application = tokio::spawn(async move {
            let Request {
                operation, reply, ..
            } = receiver.recv().await.unwrap();
            match operation {
                Operation::Frame { calls } => {
                    assert_eq!(
                        serde_json::to_value(calls).unwrap(),
                        json!([
                            {"action": "remember", "input": input},
                            {"action": "inspect", "input": {}},
                        ]),
                    );
                }
                other => panic!("expected a frame, received {other:?}"),
            }
            let projection = crate::component::execution::RenderedProjection::from_nodes(vec![
                crate::component::execution::RenderedProjectionNode::new(
                    "large-response",
                    vec![crate::transcript::CanonicalInputItem::message(
                        crate::transcript::ConversationRole::User,
                        crate::pom_resolution::resolve_artifact_document(
                            crate::pom::Document::from_raw_text(view),
                        )
                        .unwrap(),
                    )],
                ),
            ])
            .unwrap();
            reply
                .send(Ok(PreparedResponse {
                    output: StdinProjectionResponse {
                        ok: true,
                        result: Some(result),
                        projection,
                    },
                    daemon_pid: 42,
                    daemon_session_id: None,
                    request_id: None,
                }))
                .unwrap();
            assert!(receiver.recv().await.is_none());
        });

        let response = round_trip(sender, &operation).await;
        assert!(response.output.ok);
        assert_eq!(response.output.result, Some(expected_result));
        assert_eq!(response.daemon_pid, 42);
        application.await.unwrap();
    }

    #[tokio::test]
    async fn a_frame_keeps_its_order_and_rejections_before_the_next_request() {
        timeout(Duration::from_secs(3), async {
            let calls = Arc::new(AtomicUsize::new(0));
            let root_calls = calls.clone();
            let (sender, receiver) = mpsc::channel(16);
            let application = tokio::spawn(run(receiver, move || {
                transport_actions(root_calls.clone(), Duration::from_millis(20))
            }));
            let (frame_reply, frame_response) = oneshot::channel();
            sender
                .send(Request {
                    operation: Operation::Frame {
                        calls: vec![
                            CommandCall::new("work", json!({})),
                            CommandCall::new("work", json!({"unexpected": true})),
                            CommandCall::new("missing", json!({})),
                            CommandCall::new("work", json!({})),
                        ],
                    },
                    reply: frame_reply,
                    request_id: None,
                })
                .await
                .unwrap();
            let (next_reply, next_response) = oneshot::channel();
            sender
                .send(Request {
                    operation: Operation::Action {
                        call: CommandCall::new("work", json!({})),
                    },
                    reply: next_reply,
                    request_id: None,
                })
                .await
                .unwrap();

            let frame = frame_response.await.unwrap().unwrap();
            assert!(!frame.output.ok);
            let outcomes = frame.output.result.unwrap();
            let outcomes = outcomes.as_array().unwrap();
            assert_eq!(outcomes.len(), 4);
            assert_eq!(
                outcomes
                    .iter()
                    .map(|outcome| outcome["action"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["work", "work", "missing", "work"]
            );
            assert_eq!(outcomes[0]["result"], json!({"completed": 1}));
            assert_eq!(outcomes[1]["ok"], false);
            assert_eq!(outcomes[1]["result"]["code"], "invalid_arguments");
            assert_eq!(outcomes[2]["ok"], false);
            assert_eq!(outcomes[3]["result"], json!({"completed": 2}));
            let next = next_response.await.unwrap().unwrap();
            assert_eq!(next.output.result, Some(json!({"completed": 3})));
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            drop(sender);
            assert!(application.await.unwrap().unwrap().is_none());
        })
        .await
        .expect("a frame completes before the next queued request");
    }

    #[tokio::test]
    async fn a_callback_longer_than_the_io_timeout_delivers_its_result_once() {
        timeout(IO_TIMEOUT + Duration::from_secs(5), async {
            let calls = Arc::new(AtomicUsize::new(0));
            let root_calls = calls.clone();
            let (sender, receiver) = mpsc::channel(16);
            let application = tokio::spawn(run(receiver, move || {
                transport_actions(root_calls.clone(), IO_TIMEOUT + Duration::from_secs(1))
            }));

            let started = Instant::now();
            let result = round_trip(
                sender.clone(),
                &Operation::Action {
                    call: CommandCall::new("work", json!({})),
                },
            )
            .await;
            assert!(started.elapsed() >= IO_TIMEOUT);
            assert_eq!(result.output.result, Some(json!({"completed": 1})));
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            let observed = round_trip(sender.clone(), &Operation::Observe).await;
            assert_eq!(observed.output.result, None);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            drop(sender);
            assert!(application.await.unwrap().unwrap().is_none());
        })
        .await
        .expect("a slow callback completes within the separate response deadline");
    }

    #[tokio::test]
    async fn a_disconnected_frontend_does_not_cancel_or_replay_an_accepted_action() {
        timeout(Duration::from_secs(3), async {
            let calls = Arc::new(AtomicUsize::new(0));
            let root_calls = calls.clone();
            let (sender, receiver) = mpsc::channel(16);
            let application = tokio::spawn(run(receiver, move || {
                transport_actions(root_calls.clone(), Duration::from_millis(20))
            }));
            let (mut frontend, daemon) = UnixStream::pair().unwrap();
            let connection = tokio::spawn(handle_client(daemon, sender.clone()));
            write_json(
                &mut frontend,
                &Operation::Action {
                    call: CommandCall::new("work", json!({})),
                },
            )
            .await
            .unwrap();
            drop(frontend);

            assert!(
                connection.await.unwrap().is_err(),
                "the response has no reader"
            );
            let observed = round_trip(sender.clone(), &Operation::Observe).await;
            assert_eq!(observed.output.result, None);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            drop(sender);
            assert!(application.await.unwrap().unwrap().is_none());
        })
        .await
        .expect("a lost response leaves the daemon available for inspection");
    }

    #[test]
    fn foreground_association_preserves_legacy_operation_messages() {
        let legacy: SubmittedOperation<Operation> =
            serde_json::from_value(json!({"kind":"observe"})).unwrap();
        assert!(matches!(legacy.operation, Operation::Observe));
        assert!(legacy.cli_run_id.is_none());
        let associated: SubmittedOperation<Operation> = serde_json::from_value(json!({
            "kind":"action", "call":{"action":"work","input":{}}, "cli_run_id":"frontend-run"
        }))
        .unwrap();
        assert!(matches!(associated.operation, Operation::Action { .. }));
        assert_eq!(associated.cli_run_id.as_deref(), Some("frontend-run"));
    }

    #[cfg(target_os = "linux")]
    fn isolated_history() -> (History, PathBuf) {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let directory = env::temp_dir().join(format!(
            "agentview-daemon-io-failure-{:032x}",
            u128::from_be_bytes(random)
        ));
        let history = History::create_in(
            &directory,
            Path::new("/tmp/example"),
            Path::new("/tmp/example.socket"),
        )
        .unwrap();
        (history, directory)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn history_failure_before_execution_prevents_callback_effects() {
        let (history, directory) = isolated_history();
        let connection_id = history.open_connection().unwrap();
        let request_id = history
            .accept_request(connection_id, None, json!({"kind":"action"}))
            .unwrap();
        history.fail_writes_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let root_calls = calls.clone();
        let (sender, receiver) = mpsc::channel(16);
        let application = tokio::spawn(run_with_history(
            receiver,
            move || transport_actions(root_calls.clone(), Duration::ZERO),
            Some(history.clone()),
        ));
        let (reply, received) = oneshot::channel();
        sender
            .send(Request {
                operation: Operation::Action {
                    call: CommandCall::new("work", json!({})),
                },
                request_id: Some(request_id),
                reply,
            })
            .await
            .unwrap();
        assert!(received.await.unwrap().is_err());
        drop(sender);
        assert!(application.await.unwrap().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let events: Vec<super::super::daemon_history::Event> =
            fs::read_to_string(history.path().join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert!(!events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ExecutionStarted { .. })));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn history_failure_after_execution_leaves_the_outcome_unknown_and_does_not_replay() {
        let (history, directory) = isolated_history();
        let connection_id = history.open_connection().unwrap();
        let request_id = history
            .accept_request(connection_id, None, json!({"kind":"action"}))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let root_calls = calls.clone();
        let (sender, receiver) = mpsc::channel(16);
        let application = tokio::spawn(run_with_history(
            receiver,
            move || transport_actions(root_calls.clone(), Duration::from_millis(50)),
            Some(history.clone()),
        ));
        let (reply, received) = oneshot::channel();
        sender
            .send(Request {
                operation: Operation::Action {
                    call: CommandCall::new("work", json!({})),
                },
                request_id: Some(request_id),
                reply,
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        history.fail_writes_for_test();
        assert!(received.await.unwrap().is_err());
        drop(sender);
        assert!(application.await.unwrap().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let events: Vec<super::super::daemon_history::Event> =
            fs::read_to_string(history.path().join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert!(events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ExecutionStarted { .. })));
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            EventKind::ExecutionCompleted { .. } | EventKind::ExecutionFailed { .. }
        )));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_records_queued_requests_as_not_executed() {
        let (history, directory) = isolated_history();
        let connection_id = history.open_connection().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let root_calls = calls.clone();
        let (sender, receiver) = mpsc::channel(16);
        let mut replies = Vec::new();
        let mut ids = Vec::new();
        for operation in [
            Operation::Action {
                call: CommandCall::new("work", json!({})),
            },
            Operation::Stop,
            Operation::Action {
                call: CommandCall::new("work", json!({})),
            },
        ] {
            let request_id = history
                .accept_request(
                    connection_id,
                    None,
                    serde_json::to_value(&operation).unwrap(),
                )
                .unwrap();
            let (reply, received) = oneshot::channel();
            sender
                .send(Request {
                    operation,
                    request_id: Some(request_id),
                    reply,
                })
                .await
                .unwrap();
            replies.push(received);
            ids.push(request_id);
        }
        let application = tokio::spawn(run_with_history(
            receiver,
            move || transport_actions(root_calls.clone(), Duration::ZERO),
            Some(history.clone()),
        ));
        let (reply, response) = application.await.unwrap().unwrap().unwrap();
        reply.send(Ok(response)).unwrap();
        let mut replies = replies.into_iter();
        assert!(replies.next().unwrap().await.unwrap().is_ok());
        assert!(replies.next().unwrap().await.unwrap().is_ok());
        assert!(replies.next().unwrap().await.unwrap().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let events: Vec<super::super::daemon_history::Event> =
            fs::read_to_string(history.path().join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let started: Vec<_> = events
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::ExecutionStarted { request_id, .. } => Some(request_id),
                _ => None,
            })
            .collect();
        assert_eq!(started, ids[..2]);
        assert!(events.iter().any(|event| matches!(event.kind, EventKind::RequestNotExecuted { request_id, .. } if request_id == ids[2])));
        drop(sender);
        fs::remove_dir_all(directory).unwrap();
    }
}
