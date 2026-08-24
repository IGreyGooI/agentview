use std::{
    collections::HashSet,
    io::{BufRead, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

const EXTERNAL_SKILL: &str = include_str!("../skills/agentview-external/SKILL.md");
const TEST_TOKEN: &str = "agentview-external-test-token-0001";
const SERVER_PROOF_LABEL: &[u8] = b"agentview-daemon-server-v1";
const CLIENT_AUTH_LABEL: &[u8] = b"agentview-daemon-client-auth-v1";
const CLIENT_PROOF_LABEL: &[u8] = b"agentview-daemon-client-v1";
static CLAIMED_TEST_ADDRS: OnceLock<Mutex<HashSet<SocketAddr>>> = OnceLock::new();

fn agentview_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_agentview")
        .expect("agentview binary should be built for integration tests")
        .into()
}

struct ReservedLoopbackAddr {
    addr: String,
    listener: Option<TcpListener>,
}

impl ReservedLoopbackAddr {
    fn as_str(&self) -> &str {
        &self.addr
    }

    fn release_for_handoff(&mut self) {
        drop(
            self.listener
                .take()
                .expect("loopback address reservation should be held"),
        );
    }

    fn into_listener(mut self) -> TcpListener {
        self.listener
            .take()
            .expect("loopback address reservation should be held")
    }

    fn was_released(&self) -> bool {
        self.listener.is_none()
    }
}

fn unused_loopback_addr() -> ReservedLoopbackAddr {
    loop {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        if let Some(reserved) = claim_test_listener(listener) {
            return reserved;
        }
    }
}

fn claim_test_listener(listener: TcpListener) -> Option<ReservedLoopbackAddr> {
    let addr = listener.local_addr().expect("read ephemeral port");
    let claimed = CLAIMED_TEST_ADDRS
        .get_or_init(Default::default)
        .lock()
        .expect("lock claimed test addresses")
        .insert(addr);
    if claimed {
        Some(ReservedLoopbackAddr {
            addr: addr.to_string(),
            listener: Some(listener),
        })
    } else {
        None
    }
}

fn run_cli_with_env(addr: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(agentview_bin());
    command
        .env("AGENTVIEW_ADDR", addr)
        .env("AGENTVIEW_TOKEN", TEST_TOKEN)
        .env("AGENTVIEW_SOCKET", "/dev/null/agentview.sock")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_ADMIN_KEY")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_API_BASE")
        .env_remove("OPENAI_ORG_ID")
        .env_remove("OPENAI_PROJECT_ID")
        .env_remove("AGENTVIEW_MODEL")
        .env_remove("AGENTVIEW_STOCKFISH_BIN")
        .args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("agentview command should run")
}

fn run_cli(addr: &str, args: &[&str]) -> Output {
    run_cli_with_env(addr, args, &[])
}

fn run_cli_with_input(addr: &str, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(agentview_bin())
        .env("AGENTVIEW_ADDR", addr)
        .env("AGENTVIEW_TOKEN", TEST_TOKEN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentview command should start");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("protocol input should be written");
    child
        .wait_with_output()
        .expect("agentview command should finish")
}

fn run_cli_until_exit_with_input_still_open(addr: &str, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(agentview_bin())
        .env("AGENTVIEW_ADDR", addr)
        .env("AGENTVIEW_TOKEN", TEST_TOKEN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentview command should start");
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin
        .write_all(input.as_bytes())
        .expect("protocol input should be written");

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if child.try_wait().expect("poll agentview command").is_some() {
            drop(stdin);
            return child
                .wait_with_output()
                .expect("agentview command should finish");
        }
        thread::sleep(Duration::from_millis(10));
    }

    child.kill().expect("kill blocked agentview command");
    drop(stdin);
    let output = child.wait_with_output().expect("collect blocked command");
    panic!("agentview kept polling after explicit complete: {output:?}");
}

fn run_cli_with_token(addr: &str, token: &str, args: &[&str]) -> Output {
    Command::new(agentview_bin())
        .env("AGENTVIEW_ADDR", addr)
        .env("AGENTVIEW_TOKEN", token)
        .args(args)
        .output()
        .expect("agentview command should run")
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut padded_key = [0_u8; BLOCK_BYTES];
    padded_key[..key.len()].copy_from_slice(key);
    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
        inner_pad[index] ^= padded_key[index];
        outer_pad[index] ^= padded_key[index];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in parts {
        inner.update(part);
    }
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn authenticated_connection(addr: &str, request_json: &str) -> TcpStream {
    let mut connection = TcpStream::connect(addr).expect("connect authenticated client");
    let client_nonce = [7_u8; 32];
    let hello = serde_json::json!({
        "kind": "client_hello",
        "nonce": client_nonce,
    });
    writeln!(connection, "{hello}").expect("write client hello");

    let mut reader = std::io::BufReader::new(
        connection
            .try_clone()
            .expect("clone authenticated connection"),
    );
    let mut proof_line = String::new();
    reader
        .read_line(&mut proof_line)
        .expect("read daemon identity proof");
    let proof: Value = serde_json::from_str(&proof_line).expect("parse daemon identity proof");
    assert_eq!(proof["kind"], "server_hello");
    let challenge = serde_json::from_value::<[u8; 32]>(proof["challenge"].clone())
        .expect("fixed daemon challenge");
    let actual_proof =
        serde_json::from_value::<[u8; 32]>(proof["proof"].clone()).expect("fixed daemon proof");
    let session_key: [u8; 32] = Sha256::digest(TEST_TOKEN.as_bytes()).into();
    let expected_proof = hmac_sha256(
        &session_key,
        &[SERVER_PROOF_LABEL, &client_nonce, &challenge],
    );
    assert_eq!(actual_proof, expected_proof, "daemon identity proof");

    let client_proof = hmac_sha256(
        &session_key,
        &[CLIENT_AUTH_LABEL, &challenge, &client_nonce],
    );
    let client_authentication = serde_json::json!({
        "kind": "client_proof",
        "proof": client_proof,
    });
    writeln!(connection, "{client_authentication}").expect("write client identity proof");

    let authentication = hmac_sha256(
        &session_key,
        &[CLIENT_PROOF_LABEL, &challenge, request_json.as_bytes()],
    );
    let request_fields = request_json.strip_prefix('{').expect("request JSON object");
    writeln!(
        connection,
        "{{\"authentication\":{},{request_fields}",
        serde_json::to_string(&authentication).unwrap()
    )
    .expect("write authenticated request");
    connection
}

fn json_output(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|fault| {
        panic!(
            "agentview must emit one JSON response: {fault}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn shutdown(addr: &str) {
    let _ = run_cli(addr, &["--__agentview-shutdown"]);
}

struct DaemonGuard(ReservedLoopbackAddr);

impl DaemonGuard {
    fn new(addr: ReservedLoopbackAddr) -> Self {
        Self(addr)
    }

    fn addr(&self) -> &str {
        self.0.as_str()
    }

    fn start(&mut self) -> Output {
        self.0.release_for_handoff();
        run_cli(self.addr(), &["observe"])
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if self.0.was_released() {
            shutdown(self.0.as_str());
        }
    }
}

#[test]
fn daemon_test_address_remains_reserved_before_handoff() {
    let mut addr = unused_loopback_addr();

    let duplicate = TcpListener::bind(addr.as_str());

    assert!(
        duplicate.is_err(),
        "daemon test address was reusable before daemon handoff"
    );

    addr.release_for_handoff();
    let duplicate = TcpListener::bind(addr.as_str()).expect("bind released daemon address");
    assert!(
        claim_test_listener(duplicate).is_none(),
        "daemon test address claim was reusable during daemon handoff"
    );
}

#[test]
fn help_hides_internal_daemon_mode() {
    let addr = unused_loopback_addr();
    let output = run_cli(addr.as_str(), &["--help"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("observe"), "{stdout}");
    assert!(stdout.contains("act"), "{stdout}");
    assert!(stdout.contains("--full-re-render"), "{stdout}");
    assert!(stdout.contains("--protocol"), "{stdout}");
    assert!(stdout.contains("AGENTVIEW_ADDR"), "{stdout}");
    assert!(stdout.contains("AGENTVIEW_TOKEN"), "{stdout}");
    assert!(!stdout.to_ascii_lowercase().contains("daemon"), "{stdout}");
    assert!(!stdout.contains("__agentview"), "{stdout}");
}

#[test]
fn observe_requires_an_explicit_session_token() {
    let addr = unused_loopback_addr();
    let output = Command::new(agentview_bin())
        .env("AGENTVIEW_ADDR", addr.as_str())
        .env_remove("AGENTVIEW_TOKEN")
        .arg("observe")
        .output()
        .expect("agentview command should run");

    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("AGENTVIEW_TOKEN"),
        "{output:?}"
    );
}

#[test]
fn observe_rejects_an_invalid_session_token_before_connecting() {
    let addr = unused_loopback_addr();

    for token in ["too-short", "agentview-external-token-with-control\n"] {
        let output = run_cli_with_token(addr.as_str(), token, &["observe"]);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("AGENTVIEW_TOKEN"),
            "{output:?}"
        );
    }
}

#[test]
fn wrong_session_token_cannot_observe_act_or_shutdown_the_application() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let first = json_output(&daemon.start());

    for args in [
        vec!["observe"],
        vec!["act", "must-not-be-injected"],
        vec!["--__agentview-shutdown"],
    ] {
        let rejected =
            run_cli_with_token(daemon.addr(), "wrong-agentview-external-token-0002", &args);
        assert!(!rejected.status.success(), "{rejected:?}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("not authorized"),
            "{rejected:?}"
        );
    }

    let next = json_output(&run_cli(daemon.addr(), &["observe"]));
    assert_eq!(next["base_generation"], first["generation"]);
    assert!(!next["content"]
        .as_str()
        .unwrap()
        .contains("must-not-be-injected"));
}

#[test]
fn external_skill_documents_the_verified_cli_protocol() {
    for command in [
        "target/debug/agentview observe",
        "target/debug/agentview observe --full-re-render",
        "target/debug/agentview act --protocol",
    ] {
        assert!(EXTERNAL_SKILL.contains(command), "missing {command}");
    }
    for frame in ["text_delta", "text_complete", "disconnect"] {
        assert!(EXTERNAL_SKILL.contains(frame), "missing {frame}");
    }
    assert!(EXTERNAL_SKILL.contains("AGENTVIEW_TOKEN"));
    assert!(!EXTERNAL_SKILL.contains("observation_id"));
    assert!(!EXTERNAL_SKILL.contains("action_handle"));
}

#[test]
fn malformed_connection_does_not_restart_or_lose_the_external_application() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let first = json_output(&daemon.start());

    let mut connection = TcpStream::connect(daemon.addr()).expect("connect to running daemon");
    connection
        .write_all(&[0xff, b'\n'])
        .expect("write malformed UTF-8 request");
    connection
        .shutdown(Shutdown::Both)
        .expect("close malformed connection");

    let next = json_output(&run_cli(daemon.addr(), &["observe"]));
    assert_eq!(next["mode"], "delta");
    assert_eq!(next["base_generation"], first["generation"]);
    assert_ne!(next["generation"], first["generation"]);
}

#[test]
fn concurrent_incomplete_requests_do_not_starve_the_daemon() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let first = json_output(&daemon.start());
    let mut stalled = Vec::new();
    for _ in 0..6 {
        let mut connection = TcpStream::connect(daemon.addr()).expect("connect stalled client");
        connection
            .write_all(b"{")
            .expect("write incomplete request");
        stalled.push(connection);
    }

    let (sender, receiver) = mpsc::channel();
    let addr = daemon.addr().to_owned();
    thread::spawn(move || {
        sender
            .send(run_cli(&addr, &["observe"]))
            .expect("send observe output");
    });

    let output = receiver
        .recv_timeout(Duration::from_secs(3))
        .expect("concurrent stalled requests should not starve valid clients");
    let next = json_output(&output);
    assert_eq!(next["base_generation"], first["generation"]);
}

#[test]
fn server_identity_is_verified_before_sending_a_stateful_request() {
    let listener = unused_loopback_addr().into_listener();
    let addr = listener.local_addr().unwrap().to_string();
    let server = thread::spawn(move || {
        let (mut connection, _) = listener.accept().expect("accept first request");
        let mut reader = std::io::BufReader::new(
            connection
                .try_clone()
                .expect("clone fake daemon connection"),
        );
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("read client hello");
        let hello: Value = serde_json::from_str(&hello).expect("parse client hello");
        assert_eq!(hello["kind"], "client_hello");
        assert!(hello.get("token").is_none(), "{hello}");
        assert!(hello.get("op").is_none(), "{hello}");

        let false_proof = serde_json::json!({
            "kind": "server_hello",
            "challenge": vec![0_u8; 32],
            "proof": vec![0_u8; 32],
        });
        writeln!(connection, "{false_proof}").expect("write false daemon proof");
        connection
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("bound fake daemon read");
        let mut stateful_request = String::new();
        reader
            .read_to_string(&mut stateful_request)
            .expect("client should close after false proof");
        assert!(stateful_request.is_empty(), "{stateful_request}");
        drop(connection);

        listener
            .set_nonblocking(true)
            .expect("make replay detector nonblocking");
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut connections = 1;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((connection, _)) => {
                    connections += 1;
                    drop(connection);
                }
                Err(fault) if fault.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(fault) => panic!("replay detector failed: {fault}"),
            }
        }
        connections
    });

    let output = run_cli(&addr, &["observe"]);
    let connections = server.join().expect("fake daemon should finish");

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(connections, 1, "failed authentication was retried");
}

#[test]
fn response_loss_after_a_stateful_request_is_not_replayed() {
    let listener = unused_loopback_addr().into_listener();
    let addr = listener.local_addr().unwrap().to_string();
    let server = thread::spawn(move || {
        let (mut connection, _) = listener.accept().expect("accept stateful request");
        let mut reader = std::io::BufReader::new(
            connection
                .try_clone()
                .expect("clone response-loss connection"),
        );
        let mut hello_line = String::new();
        reader
            .read_line(&mut hello_line)
            .expect("read client hello");
        let hello: Value = serde_json::from_str(&hello_line).expect("parse client hello");
        let client_nonce =
            serde_json::from_value::<[u8; 32]>(hello["nonce"].clone()).expect("fixed client nonce");
        let challenge = [11_u8; 32];
        let session_key: [u8; 32] = Sha256::digest(TEST_TOKEN.as_bytes()).into();
        let proof = hmac_sha256(
            &session_key,
            &[SERVER_PROOF_LABEL, &client_nonce, &challenge],
        );
        writeln!(
            connection,
            "{}",
            serde_json::json!({
                "kind": "server_hello",
                "challenge": challenge,
                "proof": proof,
            })
        )
        .expect("write valid daemon proof");

        let mut client_proof_line = String::new();
        reader
            .read_line(&mut client_proof_line)
            .expect("read client proof");
        let client_proof: Value =
            serde_json::from_str(&client_proof_line).expect("parse client proof");
        let expected_client_proof = hmac_sha256(
            &session_key,
            &[CLIENT_AUTH_LABEL, &challenge, &client_nonce],
        );
        assert_eq!(client_proof["kind"], "client_proof");
        assert_eq!(
            serde_json::from_value::<[u8; 32]>(client_proof["proof"].clone()).unwrap(),
            expected_client_proof
        );

        let mut request = String::new();
        reader
            .read_line(&mut request)
            .expect("read stateful request");
        assert!(request.contains(r#""op":"observe""#), "{request}");
        drop(connection);

        listener
            .set_nonblocking(true)
            .expect("make replay detector nonblocking");
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut connections = 1;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((connection, _)) => {
                    connections += 1;
                    drop(connection);
                }
                Err(fault) if fault.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(fault) => panic!("replay detector failed: {fault}"),
            }
        }
        connections
    });

    let output = run_cli(&addr, &["observe"]);
    let connections = server.join().expect("response-loss daemon should finish");

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(connections, 1, "stateful request was replayed");
}

#[test]
fn slow_response_reader_does_not_block_the_daemon() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    json_output(&daemon.start());
    let large_delta = "x".repeat(4 * 1024 * 1024 - 4096);
    let protocol = format!("{{\"type\":\"text_delta\",\"text\":\"{large_delta}\"}}\n");
    json_output(&run_cli_with_input(
        daemon.addr(),
        &["act", "--protocol"],
        &protocol,
    ));

    let stalled =
        authenticated_connection(daemon.addr(), r#"{"op":"observe","full_re_render":true}"#);
    thread::sleep(Duration::from_millis(50));

    let (sender, receiver) = mpsc::channel();
    let addr = daemon.addr().to_owned();
    thread::spawn(move || {
        sender
            .send(run_cli(&addr, &["observe"]))
            .expect("send observe output");
    });

    let output = receiver
        .recv_timeout(Duration::from_secs(7))
        .expect("slow response reader should be evicted");
    drop(stalled);
    let next = json_output(&output);
    assert_eq!(next["mode"], "delta", "daemon state was restarted: {next}");
}

#[test]
#[ignore = "maximum-size external observe/act behavior is deferred with the CLI contract"]
fn repeated_maximum_escaped_acts_keep_observations_bounded() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    json_output(&daemon.start());
    let large_delta = "&".repeat(4 * 1024 * 1024 - 4096);
    let protocol = format!("{{\"type\":\"text_delta\",\"text\":\"{large_delta}\"}}\n");

    for _ in 0..3 {
        json_output(&run_cli_with_input(
            daemon.addr(),
            &["act", "--protocol"],
            &protocol,
        ));
    }

    json_output(&run_cli(daemon.addr(), &["observe", "--full-re-render"]));
}

#[test]
fn protocol_frame_limit_is_enforced_before_daemon_dispatch() {
    const MAX_PROTOCOL_FRAMES: usize = 65_536;
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let first = json_output(&daemon.start());
    let frame = r#"{"type":"text_delta","text":""}
"#;
    let protocol = frame.repeat(MAX_PROTOCOL_FRAMES + 1);

    let rejected = run_cli_with_input(daemon.addr(), &["act", "--protocol"], &protocol);

    assert!(!rejected.status.success(), "{rejected:?}");
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("external text protocol exceeded configured frame limit"),
        "{rejected:?}"
    );
    let next = json_output(&run_cli(daemon.addr(), &["observe"]));
    assert_eq!(next["base_generation"], first["generation"]);
}

#[test]
fn observe_then_protocol_act_share_one_external_application() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let observe = daemon.start();

    let observe = json_output(&observe);
    assert_eq!(observe["kind"], "observation");
    assert_eq!(observe["event"], "observe");
    assert_eq!(observe["mode"], "full");
    assert!(observe["base_generation"].is_null());
    let first_generation = observe["generation"].as_str().unwrap().to_owned();
    assert!(observe["content"]
        .as_str()
        .unwrap()
        .contains("External CLI protocol"));

    let normal_eof = run_cli_with_input(
        daemon.addr(),
        &["act", "--protocol"],
        concat!(
            r#"{"type":"text_delta","text":"left"}"#,
            "\n",
            r#"{"type":"text_delta","text":"right"}"#,
            "\n",
        ),
    );
    let normal_eof = json_output(&normal_eof);
    assert_eq!(normal_eof["event"], "act");
    assert_eq!(normal_eof["mode"], "delta");
    assert_eq!(normal_eof["base_generation"], first_generation);
    let normal_eof_content = normal_eof["content"].as_str().unwrap();
    let left = normal_eof_content.find("delta:left").unwrap();
    let right = normal_eof_content.find("delta:right").unwrap();
    let complete = normal_eof_content.find("complete:leftright").unwrap();
    assert!(left < right && right < complete, "{normal_eof_content}");

    let explicit_complete = run_cli_with_input(
        daemon.addr(),
        &["act", "--protocol"],
        concat!(
            r#"{"type":"text_delta","text":"done"}"#,
            "\n",
            r#"{"type":"text_complete","text":"done"}"#,
            "\n",
            r#"{"type":"disconnect"}"#,
            "\n",
        ),
    );
    let explicit_complete = json_output(&explicit_complete);
    let explicit_content = explicit_complete["content"].as_str().unwrap();
    assert!(
        explicit_content.contains("delta:done"),
        "{explicit_content}"
    );
    assert!(
        explicit_content.contains("complete:done"),
        "{explicit_content}"
    );
}

#[test]
fn explicit_complete_stops_live_cli_stdin_polling() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    json_output(&daemon.start());

    let output = run_cli_until_exit_with_input_still_open(
        daemon.addr(),
        &["act", "--protocol"],
        concat!(
            r#"{"type":"text_delta","text":"done"}"#,
            "\n",
            r#"{"type":"text_complete","text":"done"}"#,
            "\n",
        ),
    );

    let observation = json_output(&output);
    assert!(observation["content"]
        .as_str()
        .unwrap()
        .contains("complete:done"));
}

#[test]
fn full_re_render_reuses_the_pending_cli_generation() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let observe = json_output(&daemon.start());
    let generation = observe["generation"].clone();
    let content = observe["content"].clone();

    let rerender = json_output(&run_cli(daemon.addr(), &["observe", "--full-re-render"]));

    assert_eq!(rerender["event"], "observe");
    assert_eq!(rerender["mode"], "full");
    assert_eq!(rerender["generation"], generation);
    assert!(rerender["base_generation"].is_null());
    assert_eq!(rerender["content"], content);
}

#[test]
fn ordinary_observe_rolls_over_and_reports_the_generation_lineage() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let first = json_output(&daemon.start());

    let second = json_output(&run_cli(daemon.addr(), &["observe"]));

    assert_eq!(second["event"], "observe");
    assert_eq!(second["mode"], "delta");
    assert_eq!(second["base_generation"], first["generation"]);
    assert_ne!(second["generation"], first["generation"]);
}

#[test]
fn text_argument_is_a_normal_eof_convenience_path() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    json_output(&daemon.start());

    let acted = json_output(&run_cli(daemon.addr(), &["act", "convenient"]));

    let content = acted["content"].as_str().unwrap();
    assert!(content.contains("delta:convenient"), "{content}");
    assert!(content.contains("complete:convenient"), "{content}");
}

#[test]
fn abnormal_protocol_disconnect_fails_and_the_daemon_recovers() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    json_output(&daemon.start());

    let disconnected = run_cli_with_input(
        daemon.addr(),
        &["act", "--protocol"],
        concat!(
            r#"{"type":"text_delta","text":"partial"}"#,
            "\n",
            r#"{"type":"disconnect"}"#,
            "\n",
        ),
    );

    assert!(!disconnected.status.success(), "{disconnected:?}");
    assert!(
        String::from_utf8_lossy(&disconnected.stderr)
            .contains("external text protocol disconnected abnormally"),
        "{disconnected:?}"
    );

    let recovered = json_output(&run_cli(daemon.addr(), &["observe"]));
    assert_eq!(recovered["kind"], "observation");
    assert!(recovered["content"]
        .as_str()
        .unwrap()
        .contains("delta:partial"));
}

#[test]
fn malformed_protocol_frame_is_redacted_and_the_daemon_recovers() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    json_output(&daemon.start());

    let malformed = run_cli_with_input(
        daemon.addr(),
        &["act", "--protocol"],
        r#"{"type":"unknown","secret":"do-not-echo"}
"#,
    );

    assert!(!malformed.status.success(), "{malformed:?}");
    let stderr = String::from_utf8_lossy(&malformed.stderr);
    assert!(
        stderr.contains("external text protocol frame was invalid"),
        "{stderr}"
    );
    assert!(!stderr.contains("do-not-echo"), "{stderr}");

    let recovered = json_output(&run_cli(daemon.addr(), &["observe"]));
    assert_eq!(recovered["kind"], "observation");
}

#[test]
fn xml_invalid_protocol_text_is_rejected_without_poisoning_the_daemon() {
    let mut daemon = DaemonGuard::new(unused_loopback_addr());
    let first = json_output(&daemon.start());
    let invalid = run_cli_with_input(
        daemon.addr(),
        &["act", "--protocol"],
        "{\"type\":\"text_delta\",\"text\":\"\\u0000\"}\n",
    );

    assert!(!invalid.status.success(), "{invalid:?}");
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("not allowed in XML content"),
        "{invalid:?}"
    );

    let recovered = json_output(&run_cli(daemon.addr(), &["observe"]));
    assert_eq!(recovered["base_generation"], first["generation"]);
    assert!(!recovered["content"].as_str().unwrap().contains("\\u{0}"));
}
