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
use super::{StdinApplication, StdinDelivery, StdinProjectionResponse, StdinResponse};
use crate::component::authoring::Component;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Response {
    pub output: StdinResponse,
    pub daemon_pid: u32,
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

struct Request {
    pub operation: Operation,
    pub reply: oneshot::Sender<Result<PreparedResponse, String>>,
}

#[derive(Debug)]
struct PreparedResponse {
    output: StdinProjectionResponse,
    daemon_pid: u32,
}

type ShutdownReply = (
    oneshot::Sender<Result<PreparedResponse, String>>,
    PreparedResponse,
);

/// One frontend's delivery session. Never reconnect after submitting a request:
/// application effects may already have happened when its response is lost.
pub(super) struct Connection {
    stream: BufReader<UnixStream>,
}

impl Connection {
    pub(super) async fn connect(socket: &Path, autostart: bool) -> Result<Option<Self>> {
        Ok(connect(socket, autostart).await?.map(|stream| Self {
            stream: BufReader::new(stream),
        }))
    }

    pub(super) async fn request(&mut self, operation: &Operation) -> Result<Response> {
        exchange(&mut self.stream, operation).await
    }
}

pub(super) async fn request(
    socket: &Path,
    operation: &Operation,
    autostart: bool,
) -> Result<Option<Response>> {
    let Some(mut connection) = Connection::connect(socket, autostart).await? else {
        return Ok(None);
    };
    connection.request(operation).await.map(Some)
}

async fn exchange(stream: &mut BufReader<UnixStream>, operation: &Operation) -> Result<Response> {
    // Connect retries precede submission. Never replay a submitted action after
    // a write or response failure: it may already have changed application state.
    timeout(IO_TIMEOUT, write_json(stream.get_mut(), operation))
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
    let (sender, receiver) = mpsc::channel(16);
    let mut application = tokio::spawn(run(receiver, root));
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
                clients.spawn(async move {
                    if let Err(error) = handle_client(stream, sender).await {
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
    if !application_finished {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        joined_application(application.await)?;
    }
    // Release the listening endpoint and lock before acknowledging shutdown,
    // so an immediately following CLI invocation can start a fresh session.
    drop(socket_owner);
    let outcome = match outcome {
        Ok(Some((reply, response))) => {
            let _ = reply.send(Ok(response));
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    // In particular, let the shutdown response reach its client after cleanup.
    while let Some(result) = clients.join_next().await {
        result.context("client task failed")?;
    }
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

async fn handle_client(stream: UnixStream, sender: mpsc::Sender<Request>) -> Result<()> {
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
            operation = timeout(IO_TIMEOUT, read_json::<Operation>(&mut stream)) => {
                operation.context("command read timed out")?
            }
        };
        let stopping = matches!(&operation, Ok(Operation::Stop));
        let prepared: Result<PreparedResponse, String> = match operation {
            Ok(operation) => {
                let (reply, received) = oneshot::channel();
                match sender.send(Request { operation, reply }).await {
                    Ok(()) => received
                        .await
                        .unwrap_or_else(|_| Err("Application stopped".to_owned())),
                    Err(_) => Err("Application stopped".to_owned()),
                }
            }
            Err(error) => Err(format!("invalid command: {error}")),
        };
        let (response, commit): (Result<Response, String>, _) = match prepared {
            Ok(prepared) => match delivery.prepare(prepared.output) {
                Ok((output, commit)) => (
                    Ok(Response {
                        output,
                        daemon_pid: prepared.daemon_pid,
                    }),
                    Some(commit),
                ),
                Err(error) => (Err(error.to_string()), None),
            },
            Err(error) => (Err(error), None),
        };
        let close = stopping || response.is_err();
        timeout(IO_TIMEOUT, write_json(stream.get_mut(), &response))
            .await
            .context("response write timed out")??;
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

async fn run(
    mut receiver: mpsc::Receiver<Request>,
    root: impl Fn() -> Component + Send + Sync + 'static,
) -> Result<Option<ShutdownReply>> {
    let mut application = StdinApplication::mount(root)?;
    let operation = async {
        while let Some(Request { operation, reply }) = receiver.recv().await {
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
            });
            match response {
                Ok(response) if stopping => return Ok(Some((reply, response))),
                Ok(response) => {
                    let _ = reply.send(Ok(response));
                }
                Err(error) => {
                    let _ = reply.send(Err(error.to_string()));
                    return Err(anyhow::Error::from(error));
                }
            }
        }
        Ok(None)
    }
    .await;
    let cleanup = application.shutdown().await;
    match (operation, cleanup) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(Some((reply, _))), Err(error)) => {
            let _ = reply.send(Err(format!("Application shutdown failed: {error}")));
            Err(error.into())
        }
        (Ok(None), Err(error)) => Err(error.into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Application shutdown also failed: {cleanup}")))
        }
    }
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
            let Request { operation, reply } = receiver.recv().await.unwrap();
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
}
