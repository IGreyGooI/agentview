use std::net::TcpListener;
use std::process::{Command, Output};

use serde_json::Value;

#[cfg(unix)]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{SystemTime, UNIX_EPOCH},
};

fn agentview_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_agentview")
        .expect("agentview binary should be built for integration tests")
        .into()
}

fn unused_loopback_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test should bind an ephemeral port");
    listener
        .local_addr()
        .expect("test listener should have a local address")
        .to_string()
}

fn run_cli_with_env(addr: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(agentview_bin());
    command
        .env("AGENTVIEW_ADDR", addr)
        .env("AGENTVIEW_SOCKET", "/dev/null/agentview.sock")
        .args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("agentview command should run")
}

fn run_cli(addr: &str, args: &[&str]) -> Output {
    run_cli_with_env(addr, args, &[])
}

#[cfg(unix)]
fn mock_stockfish_script(moves: &[&str]) -> (std::path::PathBuf, String) {
    assert!(!moves.is_empty(), "the mock engine needs at least one move");
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "agentview-cli-mock-stockfish-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let script = dir.join("stockfish");
    let moves_file = dir.join("stockfish.moves");
    fs::write(&moves_file, format!("{}\n", moves.join("\n"))).unwrap();
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    uci) echo "id name mockfish"; echo "uciok" ;;
    isready) echo "readyok" ;;
    go*)
      state="$0.count"
      moves="$0.moves"
      index=0
      if [ -f "$state" ]; then index=$(cat "$state"); fi
      line_number=$((index + 1))
      move=$(sed -n "${line_number}p" "$moves")
      if [ -z "$move" ]; then move=$(tail -n 1 "$moves"); fi
      printf '%s\n' "$line_number" > "$state"
      echo "bestmove $move"
      exit 0 ;;
    quit) exit 0 ;;
  esac
done
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    let script_string = script.to_string_lossy().into_owned();
    (dir, script_string)
}

fn shutdown(addr: &str) {
    let _ = run_cli(addr, &["--__agentview-shutdown"]);
}

struct DaemonGuard(String);

impl DaemonGuard {
    fn new(addr: String) -> Self {
        Self(addr)
    }

    fn addr(&self) -> &str {
        &self.0
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        shutdown(&self.0);
    }
}

fn json_response(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Chess CLI must emit one structured JSON response: {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn chess_json(addr: &str, envs: &[(&str, &str)], args: &[&str]) -> Value {
    json_response(run_cli_with_env(addr, args, envs))
}

fn chess_error(addr: &str, envs: &[(&str, &str)], args: &[&str]) -> String {
    let output = run_cli_with_env(addr, args, envs);
    assert!(!output.status.success(), "{output:?}");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn required_string<'a>(value: &'a Value, path: &str) -> &'a str {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected string at {path} in {value}"))
}

fn chess_frame<'a>(response: &'a Value, event: &str) -> &'a Value {
    assert_eq!(
        required_string(response, "/kind"),
        "chess_frame",
        "{response}"
    );
    assert_eq!(required_string(response, "/event"), event, "{response}");
    response
        .pointer("/frame")
        .unwrap_or_else(|| panic!("expected Chess frame in {response}"))
}

fn resync_frame(response: &Value) -> &Value {
    assert_eq!(
        required_string(response, "/kind"),
        "user_resync",
        "{response}"
    );
    assert_eq!(
        required_string(response, "/status"),
        "requested",
        "{response}"
    );
    response
        .pointer("/frame")
        .unwrap_or_else(|| panic!("expected resync replacement frame in {response}"))
}

fn assert_chess_frame(frame: &Value, actionable: bool) {
    assert!(
        frame
            .pointer("/delivery_receipt")
            .and_then(Value::as_str)
            .is_some(),
        "{frame}"
    );
    assert_eq!(
        frame
            .pointer("/action_handle")
            .and_then(Value::as_str)
            .is_some(),
        actionable,
        "{frame}"
    );
    assert_eq!(
        frame
            .pointer("/prompt_mode")
            .and_then(Value::as_object)
            .is_some(),
        actionable,
        "{frame}"
    );
}

fn assert_system_gate(error: &str) {
    assert!(
        error.contains("System must be acknowledged"),
        "expected System attachment gate, got: {error}"
    );
}

#[test]
fn help_hides_internal_daemon_mode() {
    let addr = unused_loopback_addr();

    let output = run_cli(&addr, &["--help"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("observe"), "{stdout}");
    assert!(stdout.contains("act"), "{stdout}");
    assert!(stdout.contains("AGENTVIEW_ADDR"), "{stdout}");
    assert!(!stdout.to_ascii_lowercase().contains("daemon"), "{stdout}");
    assert!(!stdout.contains("__agentview"), "{stdout}");
}

#[test]
fn chess_help_describes_the_exact_handle_and_raw_xml_protocol() {
    let addr = unused_loopback_addr();

    let output = run_cli(&addr, &["chess", "--help"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agentview chess attach"), "{stdout}");
    assert!(
        stdout.contains("attach-ack <system-delivery-id>"),
        "{stdout}"
    );
    assert!(stdout.contains("ack <action-handle>"), "{stdout}");
    assert!(
        stdout.contains("act <action-handle> '<move uci=\"...\" />'"),
        "{stdout}"
    );
    assert!(stdout.contains("agentview chess hook\n"), "{stdout}");
    assert!(stdout.contains("agentview chess resync"), "{stdout}");
    assert!(!stdout.contains("--piece"), "{stdout}");
    assert!(!stdout.contains("hook <epoch>"), "{stdout}");
}

#[test]
fn observe_then_act_share_an_implicit_server_session() {
    let addr = unused_loopback_addr();

    let observe = run_cli(&addr, &["observe"]);

    assert!(observe.status.success(), "{observe:?}");
    let observe_stdout = String::from_utf8_lossy(&observe.stdout);
    assert!(observe_stdout.contains("observe epoch=0 turn=turn-1"));
    assert!(observe_stdout.contains(r#"view: <hello greeting="Hello" />"#));
    assert!(observe_stdout.contains("prompt:\n<agent_context kind=\"hello\" greeting=\"Hello\" />"));
    assert!(observe_stdout.contains("\n\nAsk the caller for their name."));

    let act = run_cli(&addr, &["act", "world"]);

    shutdown(&addr);

    assert!(act.status.success(), "{act:?}");
    let act_stdout = String::from_utf8_lossy(&act.stdout);
    assert!(act_stdout.contains("update epoch=1 turn=turn-2"));
    assert!(
        act_stdout.contains("view:\n<hello greeting=\"Hello\">\n  <name>world</name>\n</hello>"),
        "{act_stdout}"
    );
    assert!(act_stdout.contains(
        "prompt:\n<agent_context rendering_mode=\"delta\" kind=\"hello\">\n  <name>world</name>\n</agent_context>"
    ));
    assert!(act_stdout.contains("\n\nSay hello to the named caller."));
}

#[cfg(unix)]
#[test]
fn chess_external_cli_enforces_system_receipts_handles_and_delta_lineage() {
    let daemon = DaemonGuard::new(unused_loopback_addr());
    let (dir, script) = mock_stockfish_script(&["e7e5", "b8c6"]);
    let envs = [("AGENTVIEW_STOCKFISH_BIN", script.as_str())];

    // Every User operation is fenced until the exact System attachment is
    // acknowledged. The client never receives a second System after that ack.
    for args in [
        &["chess", "observe"][..],
        &["chess", "ack", "missing-handle"][..],
        &["chess", "act", "missing-handle", "<move uci=\"e2e4\" />"][..],
        &["chess", "hook"][..],
        &["chess", "resync"][..],
    ] {
        assert_system_gate(&chess_error(daemon.addr(), &envs, args));
    }

    let attachment = chess_json(daemon.addr(), &envs, &["chess", "attach"]);
    assert_eq!(required_string(&attachment, "/kind"), "system_attachment");
    assert_eq!(
        required_string(&attachment, "/attachment/status"),
        "install_system_once"
    );
    let system_delivery_id = required_string(&attachment, "/attachment/delivery_id").to_owned();
    let system_document = required_string(&attachment, "/attachment/document").to_owned();
    assert!(
        system_document.contains("# Chess move agent"),
        "{system_document}"
    );
    assert!(
        system_document.contains("<move uci=\"...\" />"),
        "{system_document}"
    );

    let attachment_replay = chess_json(daemon.addr(), &envs, &["chess", "attach"]);
    assert_eq!(
        required_string(&attachment_replay, "/attachment/delivery_id"),
        system_delivery_id
    );
    assert_eq!(
        required_string(&attachment_replay, "/attachment/document"),
        system_document
    );

    let attached = chess_json(
        daemon.addr(),
        &envs,
        &["chess", "attach-ack", &system_delivery_id],
    );
    assert_eq!(required_string(&attached, "/kind"), "system_attachment");
    assert_eq!(required_string(&attached, "/attachment/status"), "attached");
    assert_eq!(
        required_string(&attached, "/attachment/delivery_id"),
        system_delivery_id
    );
    assert!(
        attached.pointer("/attachment/document").is_none(),
        "{attached}"
    );

    let attached_reopen = chess_json(daemon.addr(), &envs, &["chess", "attach"]);
    assert_eq!(
        required_string(&attached_reopen, "/attachment/status"),
        "attached"
    );
    assert_eq!(
        required_string(&attached_reopen, "/attachment/delivery_id"),
        system_delivery_id
    );
    assert!(
        attached_reopen.pointer("/attachment/document").is_none(),
        "System must not be returned after attachment: {attached_reopen}"
    );

    let first_response = chess_json(daemon.addr(), &envs, &["chess", "observe"]);
    let first = chess_frame(&first_response, "observe");
    assert_chess_frame(first, true);
    let first_delivery = required_string(first, "/delivery_receipt").to_owned();
    let first_handle = required_string(first, "/action_handle").to_owned();
    assert_eq!(first_handle, first_delivery);
    assert_eq!(required_string(first, "/prompt_mode/mode"), "full");
    assert!(
        first.pointer("/prompt_mode/base_delivery").is_none(),
        "{first}"
    );
    assert!(
        !required_string(first, "/prompt").contains("rendering_mode=\"delta\""),
        "{first}"
    );
    assert!(
        !required_string(first, "/prompt").contains("<reply_contract"),
        "User must not repeat the System contract: {first}"
    );

    let before_ack = chess_error(
        daemon.addr(),
        &envs,
        &["chess", "act", &first_handle, "<move uci=\"e2e4\" />"],
    );
    assert!(
        before_ack.contains("acknowledged"),
        "act must require the explicit User delivery acknowledgement: {before_ack}"
    );

    let ack = chess_json(daemon.addr(), &envs, &["chess", "ack", &first_handle]);
    assert_eq!(required_string(&ack, "/kind"), "user_acknowledgement");
    assert_eq!(required_string(&ack, "/action_handle"), first_handle);
    assert_eq!(required_string(&ack, "/status"), "acknowledged");
    let repeated_ack = chess_json(daemon.addr(), &envs, &["chess", "ack", &first_handle]);
    assert_eq!(
        required_string(&repeated_ack, "/status"),
        "already_acknowledged"
    );

    let bare_uci = chess_error(
        daemon.addr(),
        &envs,
        &["chess", "act", &first_handle, "e2e4"],
    );
    assert!(
        bare_uci.contains("reply rejected"),
        "raw replies must be parsed through the System XML contract: {bare_uci}"
    );

    let waiting_response = chess_json(
        daemon.addr(),
        &envs,
        &["chess", "act", &first_handle, "<move uci=\"e2e4\" />"],
    );
    let waiting = chess_frame(&waiting_response, "act");
    assert_chess_frame(waiting, false);
    assert!(
        waiting.pointer("/action_handle").unwrap().is_null(),
        "{waiting}"
    );
    assert!(
        waiting.pointer("/prompt_mode").unwrap().is_null(),
        "{waiting}"
    );
    assert!(
        !required_string(&waiting, "/prompt").contains("rendering_mode=\"delta\""),
        "Passive must be self-contained: {waiting}"
    );
    assert!(required_string(&waiting, "/prompt").contains("<pending>true</pending>"));

    let delta_response = chess_json(daemon.addr(), &envs, &["chess", "hook"]);
    let delta = chess_frame(&delta_response, "hook");
    assert_chess_frame(delta, true);
    let delta_delivery = required_string(delta, "/delivery_receipt").to_owned();
    let delta_handle = required_string(delta, "/action_handle").to_owned();
    assert_eq!(required_string(delta, "/prompt_mode/mode"), "delta");
    assert_eq!(
        required_string(delta, "/prompt_mode/base_delivery"),
        first_delivery,
        "Passive must never become a delta baseline: {delta}"
    );
    assert!(
        required_string(delta, "/prompt").contains("rendering_mode=\"delta\""),
        "{delta}"
    );
    assert!(required_string(delta, "/prompt").contains("<move>e7e5</move>"));

    // The delta has not been acknowledged. Resync therefore tombstones this
    // exact delivery and makes the replacement full without reattaching System.
    let full_resync_response = chess_json(daemon.addr(), &envs, &["chess", "resync"]);
    let full_resync = resync_frame(&full_resync_response);
    assert_chess_frame(full_resync, true);
    let resync_delivery = required_string(full_resync, "/delivery_receipt").to_owned();
    let resync_handle = required_string(full_resync, "/action_handle").to_owned();
    assert_ne!(resync_delivery, delta_delivery);
    assert_ne!(resync_handle, delta_handle);
    assert_eq!(required_string(full_resync, "/prompt_mode/mode"), "full");
    assert!(
        full_resync.pointer("/prompt_mode/base_delivery").is_none(),
        "{full_resync}"
    );
    assert!(
        !required_string(&full_resync, "/prompt").contains("rendering_mode=\"delta\""),
        "{full_resync}"
    );

    let system_after_resync = chess_json(daemon.addr(), &envs, &["chess", "attach"]);
    assert_eq!(
        required_string(&system_after_resync, "/attachment/status"),
        "attached"
    );
    assert_eq!(
        required_string(&system_after_resync, "/attachment/delivery_id"),
        system_delivery_id
    );
    assert!(system_after_resync
        .pointer("/attachment/document")
        .is_none());

    let resync_ack = chess_json(daemon.addr(), &envs, &["chess", "ack", &resync_handle]);
    assert_eq!(required_string(&resync_ack, "/status"), "acknowledged");
    let second_waiting_response = chess_json(
        daemon.addr(),
        &envs,
        &["chess", "act", &resync_handle, "<move uci=\"g1f3\" />"],
    );
    let second_waiting = chess_frame(&second_waiting_response, "act");
    assert_chess_frame(second_waiting, false);

    let delta_after_resync_response = chess_json(daemon.addr(), &envs, &["chess", "hook"]);
    let delta_after_resync = chess_frame(&delta_after_resync_response, "hook");
    assert_chess_frame(delta_after_resync, true);
    assert_eq!(
        required_string(delta_after_resync, "/prompt_mode/mode"),
        "delta"
    );
    assert_eq!(
        required_string(delta_after_resync, "/prompt_mode/base_delivery"),
        resync_delivery,
        "the acknowledged full resync must restart the delta chain: {delta_after_resync}"
    );

    let _ = fs::remove_dir_all(dir);
}
