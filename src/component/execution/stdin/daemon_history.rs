//! One durable execution history per daemon lifetime, shared by every frontend.
//!
//! Requests and their complete observations are recorded independently of each
//! connection's incremental Frame cursor. Socket writes do not acknowledge that
//! a frontend read the result. A request without an execution terminal event has
//! an unknown outcome; a later crash does not erase already completed results.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{history, StdinResponse};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SessionMetadata {
    pub version: u32,
    pub id: String,
    pub executable: String,
    pub socket: String,
    pub pid: u32,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: Option<u64>,
    pub outcome: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Event {
    pub sequence: u64,
    pub at_unix_ms: u64,
    pub elapsed_us: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum EventKind {
    Started,
    ConnectionOpened {
        connection_id: u64,
    },
    ConnectionClosed {
        connection_id: u64,
        cli_run_id: Option<String>,
        reason: String,
    },
    RequestAccepted {
        request_id: u64,
        connection_id: u64,
        cli_run_id: Option<String>,
        operation: Value,
    },
    RequestNotExecuted {
        request_id: u64,
        message: String,
    },
    ExecutionStarted {
        request_id: u64,
        execution_index: u64,
    },
    ExecutionCompleted {
        request_id: u64,
        output: StdinResponse,
    },
    ExecutionFailed {
        request_id: u64,
        message: String,
    },
    /// All bytes were written to the socket; the peer may not have read them.
    DeliveryWritten {
        request_id: u64,
        connection_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_error: Option<String>,
    },
    DeliveryFailed {
        request_id: u64,
        connection_id: u64,
        message: String,
    },
    ProtocolRejected {
        connection_id: u64,
        message: String,
    },
    Finished {
        outcome: String,
    },
}

struct State {
    metadata: SessionMetadata,
    started: Instant,
    events: File,
    next_sequence: u64,
    next_connection: u64,
    next_request: u64,
    next_execution: u64,
    failed: Option<String>,
}

impl State {
    fn event(&mut self, kind: EventKind) -> io::Result<()> {
        if let Some(message) = &self.failed {
            return Err(io::Error::other(format!(
                "daemon history is unavailable: {message}"
            )));
        }
        if self.metadata.outcome.is_some() {
            return Err(io::Error::other("daemon history has already finished"));
        }
        let recorded: io::Result<()> = (|| {
            let event = Event {
                sequence: self.next_sequence,
                at_unix_ms: unix_millis()?,
                elapsed_us: self
                    .started
                    .elapsed()
                    .as_micros()
                    .try_into()
                    .map_err(io::Error::other)?,
                kind,
            };
            let mut bytes = serde_json::to_vec(&event).map_err(io::Error::other)?;
            bytes.push(b'\n');
            self.events.write_all(&bytes)?;
            self.events.flush()?;
            self.events.sync_data()?;
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("daemon history sequence exhausted"))?;
            Ok(())
        })();
        if let Err(error) = &recorded {
            // Never append behind a possibly partial line. No subsequent action
            // may run using a recorder whose durability is already unknown.
            self.failed = Some(error.to_string());
        }
        recorded
    }
}

struct Shared {
    directory: PathBuf,
    id: String,
    state: Mutex<State>,
}

#[derive(Clone)]
pub(super) struct History {
    shared: Arc<Shared>,
}

impl History {
    pub(super) fn create(executable: &Path, socket: &Path) -> io::Result<Self> {
        let root = root_for(executable)?;
        if let Some(parent) = root.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        Self::create_in(&root, executable, socket)
    }

    pub(super) fn create_in(root: &Path, executable: &Path, socket: &Path) -> io::Result<Self> {
        let started_at_unix_ms = unix_millis()?;
        let executable = utf8_path(executable)?;
        let socket = utf8_path(socket)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let (id, directory) = loop {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let id = format!(
                "{started_at_unix_ms:013}-{:032x}",
                u128::from_be_bytes(random)
            );
            let directory = root.join(&id);
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => break (id, directory),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let metadata = SessionMetadata {
            version: 1,
            id: id.clone(),
            executable,
            socket,
            pid: std::process::id(),
            started_at_unix_ms,
            ended_at_unix_ms: None,
            outcome: None,
        };
        let mut state = State {
            metadata,
            started: Instant::now(),
            events: private_file(&directory.join("events.jsonl"))?,
            next_sequence: 0,
            next_connection: 0,
            next_request: 0,
            next_execution: 0,
            failed: None,
        };
        write_metadata(&directory, &state.metadata)?;
        state.event(EventKind::Started)?;
        File::open(root)?.sync_all()?;
        Ok(Self {
            shared: Arc::new(Shared {
                directory,
                id,
                state: Mutex::new(state),
            }),
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.shared.id
    }

    pub(super) fn path(&self) -> &Path {
        &self.shared.directory
    }

    pub(super) fn open_connection(&self) -> io::Result<u64> {
        let mut state = self.lock()?;
        let connection_id = state.next_connection;
        state.next_connection = connection_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("daemon connection IDs exhausted"))?;
        state.event(EventKind::ConnectionOpened { connection_id })?;
        Ok(connection_id)
    }

    pub(super) fn accept_request(
        &self,
        connection_id: u64,
        cli_run_id: Option<String>,
        operation: Value,
    ) -> io::Result<u64> {
        let mut state = self.lock()?;
        let request_id = state.next_request;
        state.next_request = request_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("daemon request IDs exhausted"))?;
        state.event(EventKind::RequestAccepted {
            request_id,
            connection_id,
            cli_run_id,
            operation,
        })?;
        Ok(request_id)
    }

    pub(super) fn start_execution(&self, request_id: u64) -> io::Result<()> {
        let mut state = self.lock()?;
        let execution_index = state.next_execution;
        state.next_execution = execution_index
            .checked_add(1)
            .ok_or_else(|| io::Error::other("daemon execution indices exhausted"))?;
        state.event(EventKind::ExecutionStarted {
            request_id,
            execution_index,
        })
    }

    pub(super) fn record(&self, kind: EventKind) -> io::Result<()> {
        self.lock()?.event(kind)
    }

    pub(super) fn finish(&self, outcome: &str) -> io::Result<()> {
        let mut state = self.lock()?;
        state.event(EventKind::Finished {
            outcome: outcome.to_owned(),
        })?;
        state.metadata.ended_at_unix_ms = Some(unix_millis()?);
        state.metadata.outcome = Some(outcome.to_owned());
        write_metadata(self.path(), &state.metadata)
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, State>> {
        self.shared
            .state
            .lock()
            .map_err(|_| io::Error::other("daemon history writer panicked"))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn fail_writes_for_test(&self) {
        self.lock().unwrap().events = OpenOptions::new().write(true).open("/dev/full").unwrap();
    }
}

pub(super) fn root_for(executable: &Path) -> io::Result<PathBuf> {
    Ok(history::root_for(executable)?.with_file_name("daemon-history"))
}

fn utf8_path(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "daemon paths must be UTF-8"))
}

fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn write_metadata(directory: &Path, metadata: &SessionMetadata) -> io::Result<()> {
    let temporary = directory.join("session.json.tmp");
    let mut file = private_file(&temporary)?;
    serde_json::to_writer_pretty(&mut file, metadata).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(temporary, directory.join("session.json"))?;
    File::open(directory)?.sync_all()
}

fn unix_millis() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .try_into()
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfinished_session_never_claims_a_successful_shutdown() {
        let root = std::env::temp_dir().join(format!(
            "agentview-daemon-history-{}-{}",
            std::process::id(),
            unix_millis().unwrap()
        ));
        let history = History::create_in(
            &root,
            Path::new("/tmp/example"),
            Path::new("/tmp/example.socket"),
        )
        .unwrap();
        let id = history.id().to_owned();
        let directory = history.path().to_owned();
        let connection = history.open_connection().unwrap();
        let request = history
            .accept_request(
                connection,
                Some("cli-run".to_owned()),
                serde_json::json!({"kind":"observe"}),
            )
            .unwrap();
        history.start_execution(request).unwrap();
        drop(history);
        let metadata: SessionMetadata =
            serde_json::from_slice(&fs::read(directory.join("session.json")).unwrap()).unwrap();
        assert_eq!(metadata.id, id);
        assert!(metadata.ended_at_unix_ms.is_none());
        assert!(metadata.outcome.is_none());
        let events: Vec<Event> = fs::read_to_string(directory.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            matches!(events.last().unwrap().kind, EventKind::ExecutionStarted { request_id, execution_index: 0 } if request_id == request)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
