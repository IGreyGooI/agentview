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
    assert!(stdout.contains("AGENTVIEW_ADDR"), "{stdout}");
    assert!(!stdout.to_ascii_lowercase().contains("daemon"), "{stdout}");
    assert!(!stdout.contains("__agentview"), "{stdout}");
}

#[test]
fn chess_act_help_describes_uci_and_context_flags() {
    let addr = unused_loopback_addr();

    let output = run_cli(&addr, &["chess", "act", "--help"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agentview chess act"), "{stdout}");
    assert!(stdout.contains("--uci <uci>"), "{stdout}");
    assert!(stdout.contains("--piece <piece>"), "{stdout}");
    assert!(stdout.contains("host-owned CLI envelope"), "{stdout}");
    assert!(!stdout.contains("positional <uci>"), "{stdout}");
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

#[test]
fn chess_commands_share_an_implicit_server_session() {
    let addr = unused_loopback_addr();
    let (dir, script) = mock_stockfish_script("e7e5");
    let envs = [("AGENTVIEW_STOCKFISH_BIN", script.as_str())];

    let observe = run_cli_with_env(&addr, &["chess", "observe"], &envs);
    assert!(observe.status.success(), "{observe:?}");
    let observe_stdout = String::from_utf8_lossy(&observe.stdout);
    assert!(observe_stdout.contains("observe epoch=0 turn=turn-1"));
    assert!(observe_stdout.contains("<prompt_board>"));
    assert!(!observe_stdout.contains("render_mode=\"full\""));
    assert!(!observe_stdout.contains("<rendering_mode"));
    assert!(observe_stdout.contains("<board_state kind=\"board_state\">"));
    assert!(observe_stdout.contains("<board_squares>"));
    assert!(!observe_stdout.contains("<board_squares kind="));
    // `observe` exposes a freshly rendered User document. The stable CLI
    // grammar is System POM and must not be repeated in every observation.
    assert!(
        !observe_stdout.contains("<reply_contract"),
        "{observe_stdout}"
    );
    assert!(
        !observe_stdout.contains("agentview chess act --piece"),
        "{observe_stdout}"
    );

    // Retrying delivery before an action returns the same durable Actionable
    // frame; it does not make a new prompt or advance the delta cursor.
    let observe_replay = run_cli_with_env(&addr, &["chess", "observe"], &envs);
    assert!(observe_replay.status.success(), "{observe_replay:?}");
    assert_eq!(observe_replay.stdout, observe.stdout);

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
    let act_prompt = act_stdout
        .split_once("prompt:\n")
        .expect("act response should contain a prompt")
        .1;
    // The player reply acknowledges the prior Actionable prompt. Its successor
    // is a Passive Stockfish presentation, which is deliberately full and
    // cursor-neutral rather than a delta against that prompt.
    assert!(act_prompt.contains("<agent_context kind=\"prompt_board\">"));
    assert!(!act_prompt.contains("rendering_mode=\"delta\""));
    assert!(act_prompt.contains("<board_state kind=\"board_state\">"));
    assert!(act_prompt.contains("<square id=\"a8\" file=\"a\" rank=\"8\">r</square>"));
    assert!(act_prompt.contains("<square id=\"e2\" file=\"e\" rank=\"2\">.</square>"));
    assert!(act_prompt.contains("<square id=\"e4\" file=\"e\" rank=\"4\">P</square>"));
    assert!(act_prompt.contains("<move>e2e4</move>"));
    assert!(act_prompt.contains("<pending>true</pending>"));
    assert!(act_prompt.contains("Wait for the engine reply."));

    // The same canonical reply id is replayable while the waiting
    // presentation is current; it does not make a second domain move.
    let replay = run_cli_with_env(
        &addr,
        &[
            "chess", "act", "--piece", "P", "--from", "e2", "--to", "e4", "--uci", "e2e4",
        ],
        &envs,
    );
    assert!(replay.status.success(), "{replay:?}");
    assert_eq!(replay.stdout, act.stdout);

    let hook = run_cli_with_env(&addr, &["chess", "hook", "1"], &envs);

    shutdown(&addr);

    assert!(hook.status.success(), "{hook:?}");
    let hook_stdout = String::from_utf8_lossy(&hook.stdout);
    assert!(hook_stdout.contains("hook epoch=2 turn=turn-3"));
    let hook_prompt = hook_stdout
        .split_once("prompt:\n")
        .expect("hook response should contain a prompt")
        .1;
    assert!(hook_prompt.contains("<agent_context rendering_mode=\"delta\" kind=\"prompt_board\">"));
    assert!(!hook_prompt.contains("<prompt_board_update>"));
    assert!(hook_prompt.contains("<board_state rendering_mode=\"delta\">"));
    assert!(hook_prompt.contains("<board_squares rendering_mode=\"delta\">"));
    assert!(hook_prompt.contains("<update>"));
    assert!(hook_prompt.contains("<square id=\"e7\" file=\"e\" rank=\"7\">.</square>"));
    assert!(hook_prompt.contains("<square id=\"e5\" file=\"e\" rank=\"5\">p</square>"));
    assert!(!hook_prompt.contains("<square id=\"a8\""));
    assert!(!hook_prompt.contains("<rank n="));
    assert!(!hook_prompt.contains("<changed_sections>"));
    assert!(hook_prompt.contains("<legal_moves rendering_mode=\"delta\">"));
    assert!(hook_prompt.contains("<insert>"));
    assert!(hook_prompt.contains("<remove>"));
    assert!(hook_prompt.contains("<move_history rendering_mode=\"delta\">"));
    assert!(hook_prompt.contains("<move>e7e5</move>"));
    assert!(hook_prompt.contains("<engine rendering_mode=\"delta\">"));
    assert!(hook_prompt.contains("<replace>"));
    assert!(hook_prompt.contains("<pending>false</pending>"));
    assert!(!hook_prompt.contains("render_mode="));
    assert!(!hook_prompt.contains("<added>"));
    assert!(!hook_prompt.contains("<removed>"));

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
