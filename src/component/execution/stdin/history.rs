//! Durable byte history for one foreground CLI invocation.
//!
//! Rendered POM is opaque here. Events identify exact ranges in raw stream files;
//! recording and replay never need to decode the bytes as text or parse a view.

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, ReadBuf};

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

impl Stream {
    fn file_name(self) -> &'static str {
        match self {
            Self::Stdin => "stdin.raw",
            Self::Stdout => "stdout.raw",
            Self::Stderr => "stderr.raw",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadata {
    version: u32,
    id: String,
    executable: String,
    socket: String,
    invocation: String,
    pid: u32,
    started_at_unix_ms: u64,
    ended_at_unix_ms: Option<u64>,
    exit_code: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Event {
    sequence: u64,
    at_unix_ms: u64,
    elapsed_us: u64,
    #[serde(flatten)]
    kind: EventKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EventKind {
    Started,
    Bytes {
        stream: Stream,
        offset: u64,
        length: u64,
    },
    Error {
        message: String,
    },
    Finished {
        exit_code: u8,
    },
}

struct RawStream {
    file: File,
    offset: u64,
}

impl RawStream {
    fn create(directory: &Path, stream: Stream) -> io::Result<Self> {
        Ok(Self {
            file: private_file(&directory.join(stream.file_name()))?,
            offset: 0,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<(u64, u64)> {
        let length = u64::try_from(bytes.len()).map_err(io::Error::other)?;
        let next = self
            .offset
            .checked_add(length)
            .ok_or_else(|| io::Error::other("history stream offset overflow"))?;
        self.file.write_all(bytes)?;
        self.file.flush()?;
        self.file.sync_data()?;
        let offset = self.offset;
        self.offset = next;
        Ok((offset, length))
    }
}

struct State {
    metadata: SessionMetadata,
    started: Instant,
    next_sequence: u64,
    events: File,
    stdin: RawStream,
    stdout: RawStream,
    stderr: RawStream,
}

impl State {
    fn event(&mut self, kind: EventKind) -> io::Result<()> {
        let event = Event {
            sequence: self.next_sequence,
            at_unix_ms: unix_millis()?,
            elapsed_us: u64::try_from(self.started.elapsed().as_micros())
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
            .ok_or_else(|| io::Error::other("history event sequence overflow"))?;
        Ok(())
    }

    fn ensure_open(&self) -> io::Result<()> {
        if self.metadata.exit_code.is_some() {
            return Err(io::Error::other("CLI history has already finished"));
        }
        Ok(())
    }
}

struct Shared {
    directory: PathBuf,
    state: Mutex<State>,
}

/// One invocation's history. Clones serialize all stream appends into one event
/// order. A successful record has synced its raw bytes before syncing its event.
#[derive(Clone)]
pub(super) struct History {
    shared: Arc<Shared>,
}

impl History {
    pub(super) fn create(executable: &Path, socket: &Path, invocation: &str) -> io::Result<Self> {
        Self::create_in(&root_for(executable)?, executable, socket, invocation)
    }

    fn create_in(
        root: &Path,
        executable: &Path,
        socket: &Path,
        invocation: &str,
    ) -> io::Result<Self> {
        let started_at_unix_ms = unix_millis()?;
        let executable = utf8_path(executable)?;
        let socket = utf8_path(socket)?;
        private_directory(root)?;
        if let Some(parent) = root.parent() {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let (id, directory) = loop {
            let id = format!(
                "{started_at_unix_ms:013}-{}-{:08}",
                std::process::id(),
                NEXT_RUN.fetch_add(1, Ordering::Relaxed)
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
            id,
            executable,
            socket,
            invocation: invocation.to_owned(),
            pid: std::process::id(),
            started_at_unix_ms,
            ended_at_unix_ms: None,
            exit_code: None,
        };
        let mut state = State {
            metadata,
            started: Instant::now(),
            next_sequence: 0,
            events: private_file(&directory.join("events.jsonl"))?,
            stdin: RawStream::create(&directory, Stream::Stdin)?,
            stdout: RawStream::create(&directory, Stream::Stdout)?,
            stderr: RawStream::create(&directory, Stream::Stderr)?,
        };
        write_metadata(&directory, &state.metadata)?;
        state.event(EventKind::Started)?;
        File::open(root)?.sync_all()?;
        Ok(Self {
            shared: Arc::new(Shared {
                directory,
                state: Mutex::new(state),
            }),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.shared.directory
    }

    pub(super) fn record_input(&self, bytes: &[u8]) -> io::Result<()> {
        self.record(Stream::Stdin, bytes)
    }

    pub(super) fn record_stdout(&self, bytes: &[u8]) -> io::Result<()> {
        self.record(Stream::Stdout, bytes)
    }

    pub(super) fn record_stderr(&self, bytes: &[u8]) -> io::Result<()> {
        self.record(Stream::Stderr, bytes)
    }

    fn record(&self, stream: Stream, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut state = self.lock()?;
        state.ensure_open()?;
        let raw = match stream {
            Stream::Stdin => &mut state.stdin,
            Stream::Stdout => &mut state.stdout,
            Stream::Stderr => &mut state.stderr,
        };
        let (offset, length) = raw.append(bytes)?;
        state.event(EventKind::Bytes {
            stream,
            offset,
            length,
        })
    }

    /// Record an error as structured metadata. Its actual stderr bytes should
    /// still pass through `record_stderr` or a stderr `HistoryWriter`.
    pub(super) fn record_error(&self, message: &str) -> io::Result<()> {
        let mut state = self.lock()?;
        state.ensure_open()?;
        state.event(EventKind::Error {
            message: message.to_owned(),
        })
    }

    /// Only an observed normal frontend completion sets these fields. Abrupt
    /// termination leaves them absent; absence makes no claim that a run is live.
    pub(super) fn finish(&self, exit_code: u8) -> io::Result<()> {
        let mut state = self.lock()?;
        if let Some(previous) = state.metadata.exit_code {
            return if previous == exit_code {
                Ok(())
            } else {
                Err(io::Error::other(
                    "CLI history finished with a different exit code",
                ))
            };
        }
        state.event(EventKind::Finished { exit_code })?;
        state.metadata.ended_at_unix_ms = Some(unix_millis()?);
        state.metadata.exit_code = Some(exit_code);
        write_metadata(self.path(), &state.metadata)
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, State>> {
        self.shared
            .state
            .lock()
            .map_err(|_| io::Error::other("CLI history writer panicked"))
    }
}

pub(super) fn root_for(executable: &Path) -> io::Result<PathBuf> {
    let directory = match env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(directory) => PathBuf::from(directory),
        None => PathBuf::from(
            env::var_os("HOME")
                .ok_or_else(|| io::Error::other("set XDG_STATE_HOME or HOME for CLI history"))?,
        )
        .join(".local/state"),
    };
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("history executable name must be UTF-8"))?;
    Ok(directory.join(format!("agentview-{name}/history")))
}

fn utf8_path(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "CLI paths must be UTF-8"))
}

fn private_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
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
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    )
    .map_err(io::Error::other)
}

/// Records only bytes the wrapped reader actually returns. Pending polls do not
/// append anything, so cancelling an input wait never duplicates input history.
pub(super) struct HistoryReader<R> {
    inner: R,
    history: History,
}

impl<R> HistoryReader<R> {
    pub(super) fn new(inner: R, history: History) -> Self {
        Self { inner, history }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for HistoryReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                Poll::Ready(this.history.record_input(&buffer.filled()[before..]))
            }
            other => other,
        }
    }
}

/// A byte tee with ordinary `Write` semantics. Only successfully written bytes
/// are recorded. A history failure is fatal for this invocation; callers must
/// not retry output or application actions after such a failure.
pub(super) struct HistoryWriter<W> {
    inner: W,
    history: History,
    stream: Stream,
}

impl<W> HistoryWriter<W> {
    pub(super) fn new(inner: W, history: History, stream: Stream) -> Self {
        Self {
            inner,
            history,
            stream,
        }
    }
}

impl<W: Write> Write for HistoryWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        match self.stream {
            Stream::Stdin => self.history.record_input(&bytes[..written])?,
            Stream::Stdout => self.history.record_stdout(&bytes[..written])?,
            Stream::Stderr => self.history.record_stderr(&bytes[..written])?,
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::poll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "agentview-history-test-{}-{}-{}",
                std::process::id(),
                unix_millis().unwrap(),
                NEXT_RUN.fetch_add(1, Ordering::Relaxed)
            ));
            private_directory(&path).unwrap();
            Self(path)
        }

        fn history(&self) -> History {
            History::create_in(
                &self.0.join("app/history"),
                Path::new("/bin/app"),
                Path::new("/tmp/app-socket"),
                "interact",
            )
            .unwrap()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn events(history: &History) -> Vec<Event> {
        fs::read(history.path().join("events.jsonl"))
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    #[test]
    fn raw_streams_and_event_offsets_preserve_arbitrary_bytes_in_order() {
        let directory = TestDirectory::new();
        let history = directory.history();
        let expected = [
            (Stream::Stdin, b"\xff\0input\n".as_slice()),
            (Stream::Stdout, b"opaque < > & \xfe\n".as_slice()),
            (Stream::Stderr, b"\x80failure\0".as_slice()),
            (Stream::Stdout, b"second chunk".as_slice()),
        ];
        for (stream, bytes) in expected {
            history.record(stream, bytes).unwrap();
        }
        history.record_error("transport closed").unwrap();
        history.finish(1).unwrap();
        let recorded = events(&history);
        assert_eq!(recorded.len(), 7);
        for (index, event) in recorded.iter().enumerate() {
            assert_eq!(event.sequence, index as u64);
        }
        assert!(recorded
            .windows(2)
            .all(|pair| pair[0].elapsed_us <= pair[1].elapsed_us));
        let byte_events = recorded.iter().filter_map(|event| match event.kind {
            EventKind::Bytes {
                stream,
                offset,
                length,
            } => Some((stream, offset, length)),
            _ => None,
        });
        for ((stream, offset, length), (expected_stream, expected_bytes)) in
            byte_events.zip(expected)
        {
            assert_eq!(stream, expected_stream);
            let raw = fs::read(history.path().join(stream.file_name())).unwrap();
            assert_eq!(
                &raw[offset as usize..(offset + length) as usize],
                expected_bytes
            );
        }
        let metadata: SessionMetadata =
            serde_json::from_slice(&fs::read(history.path().join("session.json")).unwrap())
                .unwrap();
        assert_eq!(metadata.version, 1);
        assert_eq!(metadata.exit_code, Some(1));
        assert!(metadata.ended_at_unix_ms.is_some());
        assert!(matches!(
            recorded.last().unwrap().kind,
            EventKind::Finished { exit_code: 1 }
        ));
        assert!(history.record_stdout(b"too late").is_err());
        history.finish(1).unwrap();
        assert!(history.finish(0).is_err());
    }

    #[test]
    fn invocations_have_unique_private_directories_and_unfinished_metadata() {
        let directory = TestDirectory::new();
        let first = directory.history();
        let second = directory.history();
        assert_ne!(first.path(), second.path());
        for path in [
            first.path().to_owned(),
            first.path().parent().unwrap().to_owned(),
            first.path().parent().unwrap().parent().unwrap().to_owned(),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for name in [
            "session.json",
            "events.jsonl",
            "stdin.raw",
            "stdout.raw",
            "stderr.raw",
        ] {
            assert_eq!(
                fs::metadata(first.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let metadata: SessionMetadata =
            serde_json::from_slice(&fs::read(first.path().join("session.json")).unwrap()).unwrap();
        assert!(metadata.ended_at_unix_ms.is_none());
        assert!(metadata.exit_code.is_none());
    }

    #[test]
    fn cloned_writers_share_one_offset_and_event_order() {
        let directory = TestDirectory::new();
        let history = directory.history();
        std::thread::scope(|scope| {
            for thread in 0..4 {
                let history = history.clone();
                scope.spawn(move || {
                    for _ in 0..4 {
                        history.record_stdout(&[thread; 7]).unwrap();
                    }
                });
            }
        });
        let mut offset = 0;
        let recorded = events(&history);
        for (index, event) in recorded.iter().enumerate() {
            assert_eq!(event.sequence, index as u64);
            if let EventKind::Bytes {
                offset: actual,
                length,
                ..
            } = event.kind
            {
                assert_eq!(actual, offset);
                offset += length;
            }
        }
        assert_eq!(offset, 4 * 4 * 7);
        assert_eq!(
            fs::read(history.path().join("stdout.raw")).unwrap().len(),
            offset as usize
        );
    }

    struct PartialWriter(Vec<u8>);

    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = bytes.len().min(2);
            self.0.extend_from_slice(&bytes[..length]);
            Ok(length)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tee_records_only_the_successfully_written_prefix() {
        let directory = TestDirectory::new();
        let history = directory.history();
        let mut writer =
            HistoryWriter::new(PartialWriter(Vec::new()), history.clone(), Stream::Stderr);
        assert_eq!(writer.write(b"abcdef").unwrap(), 2);
        assert_eq!(fs::read(history.path().join("stderr.raw")).unwrap(), b"ab");
        writer.write_all(b"cdef").unwrap();
        assert_eq!(
            fs::read(history.path().join("stderr.raw")).unwrap(),
            b"abcdef"
        );
        assert_eq!(writer.inner.0, b"abcdef");
    }

    #[tokio::test]
    async fn cancelling_a_pending_read_does_not_duplicate_later_input() {
        let directory = TestDirectory::new();
        let history = directory.history();
        let (mut source, reader) = tokio::io::duplex(32);
        let mut reader = HistoryReader::new(reader, history.clone());
        let mut buffer = [0; 4];
        {
            let read = reader.read(&mut buffer);
            tokio::pin!(read);
            assert!(poll!(read.as_mut()).is_pending());
        }
        assert_eq!(events(&history).len(), 1);
        source.write_all(b"\xffabc\0def").await.unwrap();
        source.shutdown().await.unwrap();
        let mut actual = Vec::new();
        loop {
            let length = reader.read(&mut buffer).await.unwrap();
            if length == 0 {
                break;
            }
            actual.extend_from_slice(&buffer[..length]);
        }
        assert_eq!(actual, b"\xffabc\0def");
        assert_eq!(fs::read(history.path().join("stdin.raw")).unwrap(), actual);
        let recorded = events(&history);
        assert_eq!(recorded.len(), 3);
        assert!(matches!(
            recorded[1].kind,
            EventKind::Bytes {
                offset: 0,
                length: 4,
                ..
            }
        ));
        assert!(matches!(
            recorded[2].kind,
            EventKind::Bytes {
                offset: 4,
                length: 4,
                ..
            }
        ));
    }
}
