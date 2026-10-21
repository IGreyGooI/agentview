#![cfg(unix)]

// Build the real process fixture before running this suite:
// cargo build --locked --no-default-features --example command_feedback_cli
// AGENTVIEW_STDIN_CLI_TEST_BINARY may select an explicitly built fixture.
// Rendered views are opaque bytes throughout these tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
    process::{Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    time::timeout,
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(12);
static NEXT_SESSION: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    binary: PathBuf,
    directory: PathBuf,
    socket: PathBuf,
    state: PathBuf,
}

struct Run {
    output: Output,
    pid: u32,
    directory: PathBuf,
    metadata: Value,
    events: Vec<Value>,
}

impl Fixture {
    fn new() -> Self {
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
        let binary = binary.canonicalize().unwrap();
        let directory = std::env::temp_dir().join(format!(
            "av-history-{}-{}",
            std::process::id(),
            NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        Self {
            socket: directory.join("socket"),
            state: directory.join("state"),
            directory,
            binary,
        }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("--socket")
            .arg(&self.socket)
            .args(arguments)
            .env("XDG_STATE_HOME", &self.state)
            .kill_on_drop(true);
        command
    }

    fn history_root(&self) -> PathBuf {
        self.state.join(format!(
            "agentview-{}/history",
            self.binary.file_name().unwrap().to_str().unwrap()
        ))
    }

    fn records(&self) -> BTreeSet<PathBuf> {
        match fs::read_dir(self.history_root()) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => panic!("read CLI history: {error}"),
        }
    }

    async fn run(&self, arguments: &[&str], input: &[u8]) -> Run {
        let before = self.records();
        let mut child = self
            .command(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).await.unwrap();
        drop(stdin);
        self.finish(child, pid, before).await
    }

    async fn finish(&self, child: Child, pid: u32, before: BTreeSet<PathBuf>) -> Run {
        let output = timeout(PROCESS_TIMEOUT, child.wait_with_output())
            .await
            .expect("foreground CLI completes")
            .unwrap();
        let after = self.records();
        let added = after.difference(&before).collect::<Vec<_>>();
        assert_eq!(
            added.len(),
            1,
            "one foreground invocation creates one record"
        );
        let directory = added[0].clone();
        let metadata =
            serde_json::from_slice(&fs::read(directory.join("session.json")).unwrap()).unwrap();
        let events = fs::read(directory.join("events.jsonl"))
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        Run {
            output,
            pid,
            directory,
            metadata,
            events,
        }
    }

    async fn unrecorded(&self, arguments: &[&str]) -> Output {
        let before = self.records();
        let output = timeout(PROCESS_TIMEOUT, self.command(arguments).output())
            .await
            .expect("history/help command completes")
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(self.records(), before, "reading history creates no new run");
        output
    }

    async fn protocol(&self, operation: Value) -> Value {
        timeout(PROCESS_TIMEOUT, async {
            let mut stream = BufReader::new(UnixStream::connect(&self.socket).await.unwrap());
            let mut request = serde_json::to_vec(&operation).unwrap();
            request.push(b'\n');
            stream.get_mut().write_all(&request).await.unwrap();
            let mut bytes = Vec::new();
            stream.read_until(b'\n', &mut bytes).await.unwrap();
            let response: Value = serde_json::from_slice(&bytes).unwrap();
            response
                .get("Ok")
                .unwrap_or_else(|| panic!("daemon operation failed: {response}"))
                .clone()
        })
        .await
        .expect("structured daemon operation completes")
    }

    async fn inspect(&self) -> Value {
        self.protocol(json!({"kind": "action", "call": {"action": "inspect", "input": {}}}))
            .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Clean up through the structured socket protocol, without creating an
        // extra foreground history record or interpreting rendered POM.
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            if stream.write_all(b"{\"kind\":\"stop\"}\n").is_ok() {
                let mut reader = std::io::BufReader::new(stream);
                let _ = std::io::BufRead::read_until(&mut reader, b'\n', &mut Vec::new());
            }
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn verify_record(fixture: &Fixture, run: &Run, invocation: &str, input: &[u8], code: i32) {
    assert_eq!(run.output.status.code(), Some(code));
    let metadata = &run.metadata;
    assert_eq!(metadata["version"], 1);
    assert_eq!(
        metadata["id"],
        run.directory.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(metadata["executable"], fixture.binary.to_str().unwrap());
    assert_eq!(metadata["socket"], fixture.socket.to_str().unwrap());
    assert_eq!(metadata["invocation"], invocation);
    assert_eq!(metadata["pid"], run.pid);
    assert_eq!(metadata["exit_code"], code);
    let started = metadata["started_at_unix_ms"].as_u64().unwrap();
    let ended = metadata["ended_at_unix_ms"].as_u64().unwrap();
    assert!(ended >= started);

    let streams = [
        ("stdin", input),
        ("stdout", run.output.stdout.as_slice()),
        ("stderr", run.output.stderr.as_slice()),
    ];
    let raw = streams
        .iter()
        .map(|(name, expected)| {
            let bytes = fs::read(run.directory.join(format!("{name}.raw"))).unwrap();
            assert_eq!(
                &bytes, expected,
                "{name} preserves the actual process bytes"
            );
            (*name, bytes)
        })
        .collect::<BTreeMap<_, _>>();
    let mut reconstructed = BTreeMap::<&str, Vec<u8>>::new();
    let mut elapsed = 0;
    assert_eq!(run.events.first().unwrap()["kind"], "started");
    assert_eq!(run.events.last().unwrap()["kind"], "finished");
    assert_eq!(run.events.last().unwrap()["exit_code"], code);
    for (index, event) in run.events.iter().enumerate() {
        assert_eq!(event["sequence"], index as u64);
        assert!(event["at_unix_ms"].as_u64().is_some());
        let current_elapsed = event["elapsed_us"].as_u64().unwrap();
        assert!(current_elapsed >= elapsed);
        elapsed = current_elapsed;
        if event["kind"] == "bytes" {
            let name = event["stream"].as_str().unwrap();
            let offset = event["offset"].as_u64().unwrap() as usize;
            let length = event["length"].as_u64().unwrap() as usize;
            assert!(length > 0);
            let bytes = raw.get(name).expect("known recorded stream");
            let assembled = reconstructed.entry(name).or_default();
            assert_eq!(offset, assembled.len(), "stream offsets are contiguous");
            assembled.extend_from_slice(&bytes[offset..offset + length]);
        }
    }
    for (name, bytes) in raw {
        assert_eq!(
            reconstructed
                .get(name)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            bytes,
            "event ranges reconstruct every {name} byte exactly once"
        );
    }
}

#[tokio::test]
async fn multiple_input_writes_and_eof_preserve_live_metadata_and_all_stream_bytes() {
    let fixture = Fixture::new();
    let before = fixture.records();
    let mut child = fixture
        .command(&[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let note = "多次写入 🗝️\r\nopaque < > & quoted \" text";
    let mut input =
        serde_json::to_vec(&json!({"action": "record", "input": {"text": note}})).unwrap();
    input.extend_from_slice(b"\n{\"action\":\"inspect\",\"input\":{}}");
    let split = input.iter().position(|byte| !byte.is_ascii()).unwrap() + 1;
    stdin.write_all(&input[..split]).await.unwrap();
    stdin.flush().await.unwrap();

    let pending = timeout(PROCESS_TIMEOUT, async {
        loop {
            let records = fixture.records();
            if let Some(directory) = records.difference(&before).next() {
                if fs::metadata(directory.join("stdin.raw")).is_ok_and(|m| m.len() > 0) {
                    break directory.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("partial input is recorded before EOF");
    let active: Value =
        serde_json::from_slice(&fs::read(pending.join("session.json")).unwrap()).unwrap();
    assert!(active["ended_at_unix_ms"].is_null());
    assert!(active["exit_code"].is_null());
    assert_eq!(fs::read(pending.join("stdin.raw")).unwrap(), input[..split]);

    stdin
        .write_all(&input[split..input.len() - 5])
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    tokio::task::yield_now().await;
    stdin.write_all(&input[input.len() - 5..]).await.unwrap();
    drop(stdin);
    let run = fixture.finish(child, pid, before).await;
    verify_record(&fixture, &run, "interact", &input, 0);
    assert!(!run.output.stdout.is_empty());
    let observed = fixture.inspect().await;
    assert_eq!(
        observed["output"]["result"],
        json!({"count": 1, "latest": note})
    );
    assert_eq!(
        fixture.records().len(),
        1,
        "daemon does not record a foreground run"
    );
}

#[tokio::test]
async fn invalid_action_input_records_the_rejected_run_and_exit_code_without_effects() {
    let fixture = Fixture::new();
    let input = b"{\"action\":\"record\",\"input\":{\"text\":\"must not run\"}} {bad JSON}\n";
    let run = fixture.run(&[], input).await;
    verify_record(&fixture, &run, "interact", input, 2);
    assert!(!run.output.stdout.is_empty());
    assert_eq!(fixture.inspect().await["output"]["result"]["count"], 0);
}

#[tokio::test]
async fn lifecycle_runs_are_unique_and_history_reads_do_not_record_or_execute_them() {
    let fixture = Fixture::new();
    let mut ids = BTreeSet::new();
    for invocation in ["start", "status", "restart"] {
        let run = fixture.run(&[invocation], b"").await;
        verify_record(&fixture, &run, invocation, b"", 0);
        assert!(ids.insert(run.metadata["id"].as_str().unwrap().to_owned()));
    }
    let note = "history retrieval must not execute saved input";
    let input = serde_json::to_vec(&json!({"action": "record", "input": {"text": note}})).unwrap();
    let run = fixture.run(&[], &input).await;
    verify_record(&fixture, &run, "interact", &input, 0);
    assert!(ids.insert(run.metadata["id"].as_str().unwrap().to_owned()));
    let observed = fixture.inspect().await;
    let list = fixture.unrecorded(&["history"]).await;
    assert!(!list.stdout.is_empty());
    let latest = fixture.unrecorded(&["history", "latest"]).await;
    let by_id = fixture
        .unrecorded(&["history", run.metadata["id"].as_str().unwrap()])
        .await;
    assert!(!latest.stdout.is_empty());
    assert_eq!(latest.stdout, by_id.stdout);
    let after_history = fixture.inspect().await;
    assert_eq!(after_history["daemon_pid"], observed["daemon_pid"]);
    assert_eq!(
        after_history["output"]["result"],
        observed["output"]["result"]
    );

    let stop = fixture.run(&["stop"], b"").await;
    verify_record(&fixture, &stop, "stop", b"", 0);
    assert!(ids.insert(stop.metadata["id"].as_str().unwrap().to_owned()));
    assert_eq!(fixture.records().len(), ids.len());
    assert!(
        !fixture.socket.exists(),
        "stop releases the daemon endpoint"
    );
    assert!(!fixture.unrecorded(&["history"]).await.stdout.is_empty());
    let latest = fixture.unrecorded(&["history", "latest"]).await;
    let by_id = fixture
        .unrecorded(&["history", stop.metadata["id"].as_str().unwrap()])
        .await;
    assert_eq!(latest.stdout, by_id.stdout);
    fixture.unrecorded(&["help"]).await;
    assert!(
        !fixture.socket.exists(),
        "history/help never starts a daemon"
    );
}

#[tokio::test]
async fn runtime_failure_records_actual_stderr_and_the_structured_error() {
    let mut fixture = Fixture::new();
    let blocked = fixture.directory.join("not-a-directory");
    fs::write(&blocked, b"occupied").unwrap();
    fixture.socket = blocked.join("socket");
    let run = fixture.run(&["status"], b"").await;
    verify_record(&fixture, &run, "status", b"", 1);
    assert!(!run.output.stderr.is_empty());
    assert!(run.events.iter().any(|event| {
        event["kind"] == "error"
            && event["message"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
    }));
    assert!(!fixture.socket.exists());
}
