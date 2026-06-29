use std::fs;
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
fn mock_stockfish_script(best_move: &str) -> (std::path::PathBuf, String) {
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
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    uci) echo "id name mockfish"; echo "uciok" ;;
    isready) echo "readyok" ;;
    go*) echo "bestmove {best_move}"; exit 0 ;;
    quit) exit 0 ;;
  esac
done
"#
        ),
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

#[test]
fn help_hides_internal_daemon_mode() {
    let addr = unused_loopback_addr();

    let output = run_cli(&addr, &["--help"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("observe"), "{stdout}");
    assert!(stdout.contains("act"), "{stdout}");
    assert!(!stdout.to_ascii_lowercase().contains("daemon"), "{stdout}");
    assert!(!stdout.contains("__agentview"), "{stdout}");
}

#[test]
fn observe_then_act_share_an_implicit_server_session() {
    let addr = unused_loopback_addr();

    let observe = run_cli(&addr, &["observe"]);

    assert!(observe.status.success(), "{observe:?}");
    let observe_stdout = String::from_utf8_lossy(&observe.stdout);
    assert!(observe_stdout.contains("observe epoch=0 turn=turn-1"));
    assert!(observe_stdout.contains("view: Hello, stranger."));
    assert!(observe_stdout.contains("prompt: Ask the caller for their name."));

    let act = run_cli(&addr, &["act", "world"]);

    shutdown(&addr);

    assert!(act.status.success(), "{act:?}");
    let act_stdout = String::from_utf8_lossy(&act.stdout);
    assert!(act_stdout.contains("update epoch=1 turn=turn-2"));
    assert!(act_stdout.contains("view: Hello, world!"));
    assert!(act_stdout.contains("prompt: Say hello to the named caller."));
}

#[test]
fn chess_commands_share_an_implicit_server_session() {
    let addr = unused_loopback_addr();
    let (dir, script) = mock_stockfish_script("e7e5");
    let envs = [("AGENTVIEW_STOCKFISH_BIN", script.as_str())];

    let observe = run_cli_with_env(&addr, &["chess", "observe"], &envs);
    assert!(observe.status.success(), "{observe:?}");
    let observe_stdout = String::from_utf8_lossy(&observe.stdout);
    assert!(observe_stdout.contains("observe epoch=0 turn=turn-1"));
    assert!(observe_stdout.contains("<prompt_board render_mode=\"full\">"));
    assert!(!observe_stdout.contains("<rendering_mode"));
    assert!(observe_stdout.contains(
        "<command>agentview chess act --piece &lt;piece&gt; --from &lt;from&gt; --to &lt;to&gt; [--promotion &lt;promotion&gt;] --uci &lt;uci&gt;</command>"
    ));
    assert!(observe_stdout
        .contains("<example>agentview chess act --piece P --from e2 --to e4 --uci e2e4</example>"));

    let act = run_cli_with_env(
        &addr,
        &[
            "chess", "act", "--piece", "P", "--from", "e2", "--to", "e4", "--uci", "e2e4",
        ],
        &envs,
    );
    assert!(act.status.success(), "{act:?}");
    let act_stdout = String::from_utf8_lossy(&act.stdout);
    assert!(act_stdout.contains("act epoch=1 turn=turn-2"));
    assert!(act_stdout.contains("<prompt_board render_mode=\"update\">"));
    assert!(!act_stdout.contains("<prompt_board_update>"));
    assert!(!act_stdout.contains("<rendering_mode"));
    assert!(act_stdout.contains("<board_state>"));
    assert!(act_stdout.contains("<board_state>\n    <replace>"));
    assert!(!act_stdout.contains("<board_state>\n    <board_ascii>"));
    assert!(act_stdout.contains("<board_squares>"));
    assert!(act_stdout.contains("<board_squares>\n    <replace>"));
    assert!(act_stdout.contains("<square id=\"e2\" file=\"e\" rank=\"2\">.</square>"));
    assert!(act_stdout.contains("<square id=\"e4\" file=\"e\" rank=\"4\">P</square>"));
    assert!(!act_stdout.contains("<square id=\"a8\""));
    assert!(!act_stdout.contains("<rank n="));
    assert!(!act_stdout.contains("<changed_sections>"));
    assert!(act_stdout.contains("<legal_moves>"));
    assert!(act_stdout.contains("<added>"));
    assert!(act_stdout.contains("<removed>"));
    assert!(!act_stdout.contains("<legal_moves op="));
    assert!(act_stdout.contains("<move_history>"));
    assert!(!act_stdout.contains("<move_history op="));
    assert!(act_stdout.contains("<move>e2e4</move>"));
    assert!(act_stdout.contains("<move>e7e5</move>"));
    assert!(act_stdout.contains("<engine>"));
    assert!(act_stdout.contains("<replace>"));
    assert!(!act_stdout.contains("<engine op="));
    assert!(!act_stdout.contains("op=\""));
    assert!(act_stdout.contains("<pending>true</pending>"));

    let hook = run_cli_with_env(&addr, &["chess", "hook", "1"], &envs);

    shutdown(&addr);

    assert!(hook.status.success(), "{hook:?}");
    let hook_stdout = String::from_utf8_lossy(&hook.stdout);
    assert!(hook_stdout.contains("hook epoch=2 turn=turn-3"));
    assert!(hook_stdout.contains("<prompt_board render_mode=\"update\">"));
    assert!(!hook_stdout.contains("<prompt_board_update>"));
    assert!(!hook_stdout.contains("<rendering_mode"));
    assert!(hook_stdout.contains("<board_state>"));
    assert!(hook_stdout.contains("<board_state>\n    <replace>"));
    assert!(!hook_stdout.contains("<board_state>\n    <board_ascii>"));
    assert!(hook_stdout.contains("<board_squares>"));
    assert!(hook_stdout.contains("<board_squares>\n    <replace>"));
    assert!(hook_stdout.contains("<square id=\"e7\" file=\"e\" rank=\"7\">.</square>"));
    assert!(hook_stdout.contains("<square id=\"e5\" file=\"e\" rank=\"5\">p</square>"));
    assert!(!hook_stdout.contains("<square id=\"a8\""));
    assert!(!hook_stdout.contains("<rank n="));
    assert!(!hook_stdout.contains("<changed_sections>"));
    assert!(hook_stdout.contains("<legal_moves>"));
    assert!(hook_stdout.contains("<added>"));
    assert!(hook_stdout.contains("<removed>"));
    assert!(!hook_stdout.contains("<legal_moves op="));
    assert!(hook_stdout.contains("<move_history>"));
    assert!(!hook_stdout.contains("<move_history op="));
    assert!(hook_stdout.contains("<move>e7e5</move>"));
    assert!(hook_stdout.contains("<engine>"));
    assert!(hook_stdout.contains("<replace>"));
    assert!(!hook_stdout.contains("<engine op="));
    assert!(!hook_stdout.contains("op=\""));
    assert!(hook_stdout.contains("<pending>false</pending>"));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn chess_daemon_can_use_stockfish_engine_command() {
    let addr = unused_loopback_addr();
    let (dir, script) = mock_stockfish_script("e7e5");
    let envs = [("AGENTVIEW_STOCKFISH_BIN", script.as_str())];

    let observe = run_cli_with_env(&addr, &["chess", "observe"], &envs);
    assert!(observe.status.success(), "{observe:?}");

    let act = run_cli_with_env(
        &addr,
        &[
            "chess", "act", "--piece", "P", "--from", "e2", "--to", "e4", "--uci", "e2e4",
        ],
        &envs,
    );
    assert!(act.status.success(), "{act:?}");

    let hook = run_cli_with_env(&addr, &["chess", "hook", "1"], &envs);
    shutdown(&addr);

    assert!(hook.status.success(), "{hook:?}");
    let hook_stdout = String::from_utf8_lossy(&hook.stdout);
    assert!(hook_stdout.contains("hook epoch=2 turn=turn-3"));
    assert!(hook_stdout.contains("<move>e7e5</move>"));
    assert!(hook_stdout.contains("<last_move>e7e5</last_move>"));
    assert!(hook_stdout.contains("<pending>false</pending>"));

    let _ = fs::remove_dir_all(dir);
}
