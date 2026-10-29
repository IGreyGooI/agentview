#![cfg(unix)]

// Build the real process fixture before running this suite:
// cargo build --locked --no-default-features --example command_feedback_cli
// AGENTVIEW_STDIN_CLI_TEST_BINARY may select an explicitly built fixture.
// Views are opaque protocol fields; no rendered POM is parsed or snapshotted.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
    process::{Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agentview::component::execution::StdinResponse;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    time::timeout,
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(25);
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    binary: PathBuf,
    directory: PathBuf,
    socket: PathBuf,
    state: PathBuf,
}

struct Record {
    metadata: Value,
    events: Vec<Value>,
}

struct Wire {
    stream: BufReader<UnixStream>,
}

impl Wire {
    async fn send(&mut self, operation: &Value) {
        let mut bytes = serde_json::to_vec(operation).unwrap();
        bytes.push(b'\n');
        timeout(PROCESS_TIMEOUT, self.stream.get_mut().write_all(&bytes))
            .await
            .expect("send operation before timeout")
            .unwrap();
    }

    async fn request(&mut self, operation: &Value) -> Value {
        self.send(operation).await;
        let mut bytes = Vec::new();
        timeout(PROCESS_TIMEOUT, self.stream.read_until(b'\n', &mut bytes))
            .await
            .expect("receive structured daemon response before timeout")
            .unwrap();
        let response: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("decode daemon response: {error}"));
        response
            .get("Ok")
            .unwrap_or_else(|| panic!("daemon operation failed: {response}"))
            .clone()
    }
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
        let directory = std::env::temp_dir().join(format!(
            "av-daemon-history-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        if std::env::var_os("AGENTVIEW_DAEMON_HISTORY_TEST_KEEP").is_some() {
            eprintln!("daemon history fixture: {}", directory.display());
        }
        Self {
            binary: binary.canonicalize().unwrap(),
            socket: directory.join("socket"),
            state: directory.join("state"),
            directory,
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

    fn root(&self, scope: &str) -> PathBuf {
        self.state.join(format!(
            "agentview-{}/{scope}",
            self.binary.file_name().unwrap().to_str().unwrap()
        ))
    }

    fn directories(&self, scope: &str) -> BTreeSet<PathBuf> {
        match fs::read_dir(self.root(scope)) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => panic!("read {scope}: {error}"),
        }
    }

    fn daemon_ids(&self) -> BTreeSet<String> {
        self.directories("daemon-history")
            .into_iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
            .collect()
    }

    fn only_daemon_id(&self) -> String {
        let ids = self.daemon_ids();
        assert_eq!(ids.len(), 1);
        ids.into_iter().next().unwrap()
    }

    fn record(&self, id: &str) -> Record {
        let directory = self.root("daemon-history").join(id);
        let metadata =
            serde_json::from_slice(&fs::read(directory.join("session.json")).unwrap()).unwrap();
        // A live writer may currently be appending a line. Only complete JSONL
        // records establish a durable event; no prefix is treated as completed.
        let events = fs::read(directory.join("events.jsonl"))
            .unwrap()
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| line.last() == Some(&b'\n'))
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        Record { metadata, events }
    }

    fn record_bytes(&self, id: &str) -> (Vec<u8>, Vec<u8>) {
        let directory = self.root("daemon-history").join(id);
        (
            fs::read(directory.join("session.json")).unwrap(),
            fs::read(directory.join("events.jsonl")).unwrap(),
        )
    }

    fn foreground_daemon_ids(&self, run_id: &str) -> Vec<String> {
        let directory = self.root("history").join(run_id);
        let metadata: Value =
            serde_json::from_slice(&fs::read(directory.join("session.json")).unwrap()).unwrap();
        let sessions = metadata["daemon_session_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let events = fs::read(directory.join("events.jsonl"))
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .filter(|event| event["kind"] == "daemon_session")
            .map(|event| event["daemon_session_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(sessions, events);
        sessions
    }

    async fn wait_record(&self, id: &str, predicate: impl Fn(&Record) -> bool) -> Record {
        timeout(PROCESS_TIMEOUT, async {
            loop {
                let record = self.record(id);
                if predicate(&record) {
                    return record;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let log = fs::read_to_string(self.socket.with_extension("log")).unwrap_or_default();
            panic!("daemon history condition timed out for {id}; daemon log: {log}")
        })
    }

    async fn wait_closed(&self, id: &str) {
        self.wait_record(id, |record| {
            let opened = record
                .events
                .iter()
                .filter(|event| event["kind"] == "connection_opened")
                .map(|event| event["connection_id"].as_u64().unwrap())
                .collect::<BTreeSet<_>>();
            let closed = record
                .events
                .iter()
                .filter(|event| event["kind"] == "connection_closed")
                .map(|event| event["connection_id"].as_u64().unwrap())
                .collect::<BTreeSet<_>>();
            opened == closed
        })
        .await;
    }

    async fn wait_finished(&self, id: &str) -> Record {
        self.wait_record(id, |record| {
            !record.metadata["outcome"].is_null()
                && record
                    .events
                    .last()
                    .is_some_and(|event| event["kind"] == "finished")
        })
        .await
    }

    async fn foreground(&self, arguments: &[&str], input: &[u8]) -> String {
        let before = self.directories("history");
        let mut child = self
            .command(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).await.unwrap();
        drop(stdin);
        let output = timeout(PROCESS_TIMEOUT, child.wait_with_output())
            .await
            .expect("foreground invocation completes")
            .unwrap();
        assert!(
            output.status.success(),
            "foreground {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let after = self.directories("history");
        let added = after.difference(&before).collect::<Vec<_>>();
        assert_eq!(added.len(), 1, "one invocation records one foreground run");
        let metadata: Value =
            serde_json::from_slice(&fs::read(added[0].join("session.json")).unwrap()).unwrap();
        assert_eq!(metadata["exit_code"], 0);
        metadata["id"].as_str().unwrap().to_owned()
    }

    async fn read_history(&self, arguments: &[&str]) -> Output {
        let before_cli = self.directories("history");
        let before_daemons = self.daemon_ids();
        let output = timeout(PROCESS_TIMEOUT, self.command(arguments).output())
            .await
            .expect("history read completes")
            .unwrap();
        assert!(
            output.status.success(),
            "history {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(self.directories("history"), before_cli);
        assert_eq!(self.daemon_ids(), before_daemons);
        output
    }

    async fn connect(&self) -> Wire {
        Wire {
            stream: BufReader::new(UnixStream::connect(&self.socket).await.unwrap()),
        }
    }

    async fn protocol(&self, operation: &Value) -> Value {
        self.connect().await.request(operation).await
    }

    async fn inspect(&self) -> Value {
        self.protocol(&action("inspect", json!({}))).await
    }

    async fn stop(&self, id: &str) -> Record {
        self.foreground(&["stop"], b"").await;
        self.wait_finished(id).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Stop only this fixture's private socket. No user daemon is addressed,
        // including when an assertion fails before normal lifecycle cleanup.
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.socket) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            if stream.write_all(b"{\"kind\":\"stop\"}\n").is_ok() {
                let mut reader = std::io::BufReader::new(stream);
                let _ = std::io::BufRead::read_until(&mut reader, b'\n', &mut Vec::new());
            }
        }
        if std::env::var_os("AGENTVIEW_DAEMON_HISTORY_TEST_KEEP").is_none() {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn action(name: &str, input: Value) -> Value {
    json!({"kind": "action", "call": {"action": name, "input": input}})
}

fn record_action(text: &str) -> Value {
    action("record", json!({"text": text}))
}

fn is_record(event: &Value, text: &str) -> bool {
    let operation = &event["operation"];
    event["kind"] == "request_accepted"
        && match operation["kind"].as_str() {
            Some("action") => operation["call"] == record_action(text)["call"],
            Some("frame") => operation["calls"] == json!([record_action(text)["call"]]),
            _ => false,
        }
}

fn accepted<'a>(record: &'a Record, text: &str) -> &'a Value {
    let requests = record
        .events
        .iter()
        .filter(|event| is_record(event, text))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1, "a submitted operation is not replayed");
    requests[0]
}

fn completed<'a>(record: &'a Record, request_id: &Value) -> &'a Value {
    let terminal = record
        .events
        .iter()
        .filter(|event| {
            event["kind"] == "execution_completed" && event["request_id"] == *request_id
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1);
    terminal[0]
}

fn verify_history(fixture: &Fixture, id: &str, record: &Record, finished: bool) {
    let metadata = &record.metadata;
    assert_eq!(metadata["version"], 1);
    assert_eq!(metadata["id"], id);
    assert_eq!(metadata["executable"], fixture.binary.to_str().unwrap());
    assert_eq!(metadata["socket"], fixture.socket.to_str().unwrap());
    assert!(metadata["pid"].as_u64().unwrap() > 0);
    let started_at = metadata["started_at_unix_ms"].as_u64().unwrap();
    assert_eq!(record.events.first().unwrap()["kind"], "started");
    if finished {
        assert!(metadata["ended_at_unix_ms"].as_u64().unwrap() >= started_at);
        assert!(metadata["outcome"].as_str().is_some());
        assert_eq!(record.events.last().unwrap()["kind"], "finished");
        assert_eq!(
            record.events.last().unwrap()["outcome"],
            metadata["outcome"]
        );
    } else {
        assert!(metadata["ended_at_unix_ms"].is_null());
        assert!(metadata["outcome"].is_null());
        assert!(record
            .events
            .iter()
            .all(|event| event["kind"] != "finished"));
    }

    let mut elapsed = 0;
    let mut requests = BTreeMap::new();
    let mut executing = BTreeSet::new();
    let mut not_executed = BTreeSet::new();
    let mut terminal = BTreeSet::new();
    let mut execution_index = 0;
    for (sequence, event) in record.events.iter().enumerate() {
        assert_eq!(event["sequence"], sequence as u64);
        assert!(event["at_unix_ms"].as_u64().is_some());
        let current = event["elapsed_us"].as_u64().unwrap();
        assert!(current >= elapsed);
        elapsed = current;
        match event["kind"].as_str().unwrap() {
            "request_accepted" => {
                let id = event["request_id"].as_u64().unwrap();
                assert!(requests.insert(id, event).is_none());
                assert!(event["connection_id"].as_u64().is_some());
                assert!(event["operation"].is_object());
            }
            "execution_started" => {
                let id = event["request_id"].as_u64().unwrap();
                assert!(requests.contains_key(&id));
                assert!(executing.insert(id));
                assert_eq!(event["execution_index"], execution_index);
                execution_index += 1;
            }
            "execution_completed" | "execution_failed" => {
                let id = event["request_id"].as_u64().unwrap();
                assert!(executing.contains(&id));
                assert!(
                    terminal.insert(id),
                    "one terminal execution fact per request"
                );
                if event["kind"] == "execution_completed" {
                    let _: StdinResponse = serde_json::from_value(event["output"].clone()).unwrap();
                }
            }
            "request_not_executed" => {
                let id = event["request_id"].as_u64().unwrap();
                assert!(requests.contains_key(&id));
                assert!(!executing.contains(&id));
                assert!(not_executed.insert(id));
                assert!(terminal.insert(id));
            }
            "delivery_written" | "delivery_failed" => {
                let id = event["request_id"].as_u64().unwrap();
                assert!(
                    terminal.contains(&id),
                    "execution is recorded before socket delivery"
                );
                assert_eq!(event["connection_id"], requests[&id]["connection_id"]);
            }
            _ => {}
        }
    }
    if finished {
        assert_eq!(requests.len(), terminal.len());
        assert_eq!(executing.len() + not_executed.len(), terminal.len());
    }
}

#[tokio::test]
async fn one_session_orders_connections_and_links_foreground_runs_to_complete_results() {
    let fixture = Fixture::new();
    let start_run = fixture.foreground(&["start"], b"").await;
    let session = fixture.only_daemon_id();
    let first = fixture
        .foreground(
            &[],
            &serde_json::to_vec(&record_action("cli first")["call"]).unwrap(),
        )
        .await;
    let second = fixture
        .foreground(
            &[],
            &serde_json::to_vec(&record_action("cli second")["call"]).unwrap(),
        )
        .await;
    let mut left = fixture.connect().await;
    let mut right = fixture.connect().await;
    let left_operation = record_action("left connection");
    let right_operation = record_action("right connection");
    let (left_reply, right_reply) = tokio::join!(
        left.request(&left_operation),
        right.request(&right_operation),
    );
    for reply in [&left_reply, &right_reply] {
        assert_eq!(reply["daemon_session_id"], session);
    }
    assert_eq!(
        [
            left_reply["output"]["result"]["count"].as_u64().unwrap(),
            right_reply["output"]["result"]["count"].as_u64().unwrap()
        ]
        .into_iter()
        .collect::<BTreeSet<_>>(),
        BTreeSet::from([3, 4])
    );
    let final_reply = left.request(&record_action("same connection again")).await;
    assert_eq!(final_reply["output"]["result"]["count"], 5);
    drop(left);
    drop(right);
    let full_observation = fixture.protocol(&json!({"kind": "observe"})).await;
    assert_eq!(full_observation["daemon_session_id"], session);
    let live = fixture.record(&session);
    let last = completed(
        &live,
        &accepted(&live, "same connection again")["request_id"],
    );
    assert_eq!(last["output"]["result"], final_reply["output"]["result"]);
    // A fresh reader receives the same complete view recorded by the daemon,
    // independently of the incremental cursor on the previous connection.
    assert_eq!(last["output"]["view"], full_observation["output"]["view"]);
    assert_eq!(fixture.inspect().await["output"]["result"]["count"], 5);
    let record = fixture.stop(&session).await;
    verify_history(&fixture, &session, &record, true);
    assert_eq!(fixture.daemon_ids(), BTreeSet::from([session.clone()]));
    assert!(record
        .events
        .iter()
        .any(|event| { event["kind"] == "request_accepted" && event["cli_run_id"] == start_run }));
    for (run, text) in [(first, "cli first"), (second, "cli second")] {
        assert_eq!(accepted(&record, text)["cli_run_id"], run);
        assert_eq!(
            fixture.foreground_daemon_ids(&run).as_slice(),
            std::slice::from_ref(&session)
        );
    }
    assert!(accepted(&record, "left connection")["cli_run_id"].is_null());
    assert_ne!(
        accepted(&record, "left connection")["connection_id"],
        accepted(&record, "right connection")["connection_id"]
    );
    assert_eq!(
        accepted(&record, "left connection")["connection_id"],
        accepted(&record, "same connection again")["connection_id"]
    );
    let by_id = record
        .events
        .iter()
        .filter(|event| event["kind"] == "request_accepted")
        .map(|event| (event["request_id"].as_u64().unwrap(), event))
        .collect::<BTreeMap<_, _>>();
    let counts = record
        .events
        .iter()
        .filter(|event| event["kind"] == "execution_started")
        .filter_map(|event| {
            let request = by_id[&event["request_id"].as_u64().unwrap()];
            let operation = &request["operation"];
            let output = &completed(&record, &event["request_id"])["output"];
            match operation["kind"].as_str() {
                Some("action") if operation["call"]["action"] == "record" => {
                    output["result"]["count"].as_u64()
                }
                Some("frame") if operation["calls"][0]["action"] == "record" => {
                    output["result"][0]["result"]["count"].as_u64()
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, [1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn an_accepted_request_keeps_its_full_result_after_the_client_disconnects() {
    let fixture = Fixture::new();
    fixture.foreground(&["start"], b"").await;
    let session = fixture.only_daemon_id();
    let note = "原文 < > & \"quoted\" \\path\0\n".repeat(16384);
    let mut disconnected = fixture.connect().await;
    disconnected.send(&record_action(&note)).await;
    fixture
        .wait_record(&session, |record| {
            record.events.iter().any(|event| is_record(event, &note))
        })
        .await;
    drop(disconnected);
    let record = fixture
        .wait_record(&session, |record| {
            let Some(request) = record.events.iter().find(|event| is_record(event, &note)) else {
                return false;
            };
            record.events.iter().any(|event| {
                event["kind"] == "execution_completed"
                    && event["request_id"] == request["request_id"]
            })
        })
        .await;
    let request_id = &accepted(&record, &note)["request_id"];
    let result = &completed(&record, request_id)["output"];
    assert_eq!(result["ok"], true);
    assert_eq!(result["result"], json!({"count": 1, "latest": note}));
    let inspected = fixture.inspect().await;
    assert_eq!(inspected["output"]["result"], result["result"]);
    let continued = fixture
        .protocol(&record_action("continue after disconnect"))
        .await;
    assert_eq!(continued["output"]["result"]["count"], 2);
    let record = fixture.stop(&session).await;
    verify_history(&fixture, &session, &record, true);
    let request = accepted(&record, &note);
    assert!(record.events.iter().any(|event| {
        matches!(
            event["kind"].as_str(),
            Some("delivery_written" | "delivery_failed")
        ) && event["request_id"] == request["request_id"]
    }));
    assert_eq!(
        completed(&record, &request["request_id"])["output"]["result"]["latest"],
        note
    );
}

#[tokio::test]
async fn a_crashed_daemon_stays_incomplete_and_a_new_lifetime_never_reuses_its_session() {
    let fixture = Fixture::new();
    fixture.foreground(&["start"], b"").await;
    let old = fixture.only_daemon_id();
    fixture.protocol(&record_action("before crash")).await;
    let reply = fixture.inspect().await;
    assert_eq!(reply["daemon_session_id"], old);
    assert_eq!(reply["output"]["result"]["count"], 1);
    fixture.wait_closed(&old).await;
    let pid = reply["daemon_pid"].as_u64().unwrap();
    assert_eq!(fixture.record(&old).metadata["pid"], pid);
    let before_crash = fixture.record_bytes(&old);
    let status = Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    timeout(PROCESS_TIMEOUT, async {
        loop {
            if UnixStream::connect(&fixture.socket).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the killed fixture daemon releases its listener");
    verify_history(&fixture, &old, &fixture.record(&old), false);
    assert_eq!(fixture.record_bytes(&old), before_crash);
    fixture.foreground(&["start"], b"").await;
    let ids = fixture.daemon_ids();
    assert_eq!(ids.len(), 2);
    let new = ids.iter().find(|id| **id != old).unwrap();
    let reply = fixture.inspect().await;
    assert_eq!(reply["daemon_session_id"], *new);
    assert_ne!(reply["daemon_pid"], pid);
    assert_eq!(reply["output"]["result"]["count"], 0);
    let record = fixture.stop(new).await;
    verify_history(&fixture, new, &record, true);
    verify_history(&fixture, &old, &fixture.record(&old), false);
    assert_eq!(fixture.record_bytes(&old), before_crash);
}

#[tokio::test]
async fn restart_links_both_lifetimes_and_daemon_history_reads_never_execute_saved_operations() {
    let fixture = Fixture::new();
    fixture.foreground(&["start"], b"").await;
    let old = fixture.only_daemon_id();
    fixture
        .protocol(&record_action("history must never replay this action"))
        .await;
    let restart_run = fixture.foreground(&["restart"], b"").await;
    let ids = fixture.daemon_ids();
    assert_eq!(ids.len(), 2);
    let new = ids.iter().find(|id| **id != old).unwrap();
    assert_eq!(
        fixture.foreground_daemon_ids(&restart_run),
        [old.clone(), new.clone()]
    );
    let old_record = fixture.wait_finished(&old).await;
    verify_history(&fixture, &old, &old_record, true);
    let observed = fixture.inspect().await;
    assert_eq!(observed["daemon_session_id"], *new);
    assert_eq!(observed["output"]["result"]["count"], 0);
    fixture.wait_closed(new).await;
    let new_record = fixture.record(new);
    for (record, kind) in [(&old_record, "stop"), (&new_record, "observe")] {
        assert!(record.events.iter().any(|event| {
            event["kind"] == "request_accepted"
                && event["operation"]["kind"] == kind
                && event["cli_run_id"] == restart_run
        }));
    }
    let old_bytes = fixture.record_bytes(&old);
    let active_bytes = fixture.record_bytes(new);
    assert!(!fixture
        .read_history(&["history", "--daemon"])
        .await
        .stdout
        .is_empty());
    let latest = fixture
        .read_history(&["history", "--daemon", "latest"])
        .await;
    let by_id = fixture.read_history(&["history", "--daemon", new]).await;
    assert_eq!(
        latest.stdout, by_id.stdout,
        "latest selects the newest lifetime"
    );
    fixture.read_history(&["history", "--daemon", &old]).await;
    assert_eq!(fixture.record_bytes(&old), old_bytes);
    assert_eq!(
        fixture.record_bytes(new),
        active_bytes,
        "history does not connect to the daemon"
    );
    let after_history = fixture.inspect().await;
    assert_eq!(after_history["daemon_pid"], observed["daemon_pid"]);
    assert_eq!(
        after_history["output"]["result"],
        observed["output"]["result"]
    );
    let record = fixture.stop(new).await;
    verify_history(&fixture, new, &record, true);
    assert!(!fixture.socket.exists());
    let final_bytes = fixture.record_bytes(new);
    fixture
        .read_history(&["history", "--daemon", "latest"])
        .await;
    fixture.read_history(&["history", "--daemon", &old]).await;
    assert!(
        !fixture.socket.exists(),
        "offline history never starts a daemon"
    );
    assert_eq!(fixture.record_bytes(&old), old_bytes);
    assert_eq!(fixture.record_bytes(new), final_bytes);
}
