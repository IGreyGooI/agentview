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
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{sleep, timeout, Instant},
};

use super::super::CommandCall;
use super::{StdinApplication, StdinResponse};
use crate::component::authoring::Component;

const MAX_REQUEST_BYTES: usize = 8192;
const MAX_RESPONSE_BYTES: usize = 128 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    Stop,
    Feedback { message: String },
}

struct Request {
    pub operation: Operation,
    pub reply: oneshot::Sender<Result<Response, String>>,
}

type ShutdownReply = (oneshot::Sender<Result<Response, String>>, Response);

pub(super) async fn request(
    socket: &Path,
    operation: &Operation,
    autostart: bool,
) -> Result<Option<Response>> {
    let Some(mut stream) = connect(socket, autostart).await? else {
        return Ok(None);
    };
    // Connect retries precede submission. Never replay a submitted action after
    // a write or response failure: it may already have changed application state.
    let response: Result<Response, String> = timeout(REQUEST_TIMEOUT, async {
        write_json(&mut stream, operation).await?;
        read_json(&mut stream, MAX_RESPONSE_BYTES).await
    })
    .await
    .context("response timed out; inspect application state before retrying the action")??;
    response.map(Some).map_err(anyhow::Error::msg)
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

async fn handle_client(mut stream: UnixStream, sender: mpsc::Sender<Request>) -> Result<()> {
    let operation = timeout(
        REQUEST_TIMEOUT,
        read_json::<Operation>(&mut stream, MAX_REQUEST_BYTES),
    )
    .await
    .context("command read timed out")?;
    let response: Result<Response, String> = match operation {
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
    timeout(REQUEST_TIMEOUT, write_json(&mut stream, &response))
        .await
        .context("response write timed out")??;
    Ok(())
}

async fn read_json<T: DeserializeOwned>(stream: &mut UnixStream, maximum: usize) -> Result<T> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take((maximum + 1) as u64))
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(bytes.len() <= maximum, "message exceeds size limit");
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
                Operation::Action { call } => application.submit(call).await,
                Operation::Observe | Operation::Stop => application.observe().await,
                Operation::Feedback { message } => {
                    application.feedback("invalid_input", message).await
                }
            };
            let response = response.map(|output| Response {
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
