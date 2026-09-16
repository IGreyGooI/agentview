//! Process frontend for the retained stdin application.

use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{ensure, Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};

use super::super::CommandCall;
use super::daemon::{self, request, Operation, Response};
use crate::component::authoring::Component;

const MAX_INPUT_BYTES: usize = 4096;

enum Invocation {
    Help,
    Daemon,
    Interact,
    Start,
    Status,
    Stop,
    Restart,
}

pub(super) async fn run(root: impl Fn() -> Component + Send + Sync + 'static) -> ExitCode {
    match invoke(root).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => {
            eprintln!("agentview: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn invoke(root: impl Fn() -> Component + Send + Sync + 'static) -> Result<bool> {
    let (socket, invocation) = parse_cli()?;
    if matches!(invocation, Invocation::Help) {
        print!("{}", help());
        return Ok(true);
    }
    // Validate and prepare connection metadata before any request can start a
    // daemon or change application state. Output must not fail later on a path
    // that the frontend could have rejected before executing the action.
    let command = format!(
        "{} --socket {}",
        shell_quote(&env::current_exe()?)?,
        shell_quote(&socket)?
    );
    match invocation {
        Invocation::Help => unreachable!("help returned before session setup"),
        Invocation::Daemon => daemon::serve(&socket, root).await?,
        Invocation::Interact => return interact(&socket, &command).await,
        Invocation::Start | Invocation::Restart => {
            if matches!(invocation, Invocation::Restart) {
                request(&socket, &Operation::Stop, false).await?;
            }
            let response = request(&socket, &Operation::Observe, true)
                .await?
                .context("daemon did not start")?;
            print_view(Some(&response), &command, true)?;
        }
        Invocation::Status => {
            let response = request(&socket, &Operation::Observe, false).await?;
            print_view(response.as_ref(), &command, response.is_some())?;
        }
        Invocation::Stop => {
            let response = request(&socket, &Operation::Stop, false).await?;
            print_view(response.as_ref(), &command, false)?;
        }
    }
    Ok(true)
}

fn help() -> String {
    let name = env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "app".to_owned());
    let usage = "APP [--socket PATH] [start|status|stop|restart|help]\n\n\
No subcommand: read one JSON action per stdin line and print each resulting view.\n\
Input format: {\"action\":\"<action name>\",\"input\":{}}\n\
The view describes available actions and their input schemas.\n\
Empty stdin shows the current view. At a terminal, the initial view appears first.\n\
A missing daemon starts automatically. Closing stdin keeps the application running.\n\n\
  start    Start or connect to the daemon and show the current view\n\
  status   Show the running view or stopped status without starting a daemon\n\
  stop     Stop the daemon, await cleanup, and show the final view\n\
  restart  Stop the daemon and show a fresh session (discards the old state)\n\
  help     Show this usage without starting a daemon\n\n\
stdout carries callback JSON results, views and feedback; diagnostics use stderr.\n\
Invalid actions return feedback and later input lines still run.\n\
Exit codes: 0 callbacks completed, 2 invalid/disabled input, 1 transport/runtime failure.\n\
Business acceptance or refusal is expressed by the callback's JSON result.\n\
Use --socket PATH for independent sessions. State lasts until the daemon stops.\n\
Default socket: $XDG_RUNTIME_DIR/agentview-APP/socket, or\n\
                $HOME/.cache/agentview-APP/socket\n";
    usage.replace("APP", &name)
}

fn print_view(response: Option<&Response>, command: &str, running: bool) -> Result<()> {
    let (status, message) = if running {
        ("running", "Send an action as JSON on stdin. Closing this connection preserves application state. Use status to observe, stop to end the session, or restart for a fresh session.")
    } else {
        ("stopped", "The daemon is stopped. Use start or send an action to begin a new session. Any view below is the final snapshot of the stopped session.")
    };
    let mut stdout = io::stdout().lock();
    let ok = response.is_none_or(|response| response.output.ok);
    writeln!(stdout, "<view ok=\"{ok}\">")?;
    writeln!(stdout, "<daemon status=\"{status}\">{message}</daemon>\n")?;
    if let Some(response) = response {
        writeln!(stdout, "<session daemon_pid=\"{}\" />", response.daemon_pid)?;
        writeln!(stdout, "<connection command=\"{}\" input=\"one JSON action per stdin line\" output=\"callback JSON result and current view after each action\" />\n", escape_xml_text(command).replace('"', "&quot;"))?;
        if let Some(result) = &response.output.result {
            writeln!(
                stdout,
                "<action_result ok=\"{}\">{}</action_result>\n",
                response.output.ok,
                escape_xml_text(&serde_json::to_string(result)?)
            )?;
        }
        stdout.write_all(response.output.view.as_bytes())?;
    }
    writeln!(stdout, "</view>")?;
    stdout.flush()?;
    Ok(())
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        // JSON permits these scalar values but XML does not. JSON escapes
        // preserve the exact result when the action_result body is decoded.
        .replace('\u{fffe}', "\\ufffe")
        .replace('\u{ffff}', "\\uffff")
}

async fn interact(socket: &Path, command: &str) -> Result<bool> {
    let mut observed = false;
    if io::stdin().is_terminal() {
        let response = request(socket, &Operation::Observe, true).await?;
        print_view(response.as_ref(), command, true)?;
        observed = true;
    }
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut succeeded = true;
    while let Some(line) = input_line(&mut stdin).await? {
        let operation = match line {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => continue,
            Ok(bytes) => match serde_json::from_slice::<CommandCall>(&bytes) {
                Ok(call) => Operation::Action { call },
                Err(error) => Operation::Feedback {
                    message: invalid_action_message(&error),
                },
            },
            Err(message) => Operation::Feedback { message },
        };
        let response = request(socket, &operation, true)
            .await?
            .context("daemon did not start")?;
        succeeded &= response.output.ok;
        print_view(Some(&response), command, true)?;
        observed = true;
    }
    if !observed {
        let response = request(socket, &Operation::Observe, true).await?;
        print_view(response.as_ref(), command, true)?;
    }
    Ok(succeeded)
}

fn invalid_action_message(error: &serde_json::Error) -> String {
    // Escape arbitrary field names, then bound the displayed diagnostic. This
    // leaves room for JSON quoting and guidance within MAX_REQUEST_BYTES.
    const MAX_DIAGNOSTIC_BYTES: usize = 1024;
    let error = error.to_string();
    let mut diagnostic = String::new();
    for character in error.escape_debug() {
        if diagnostic.len() + character.len_utf8() > MAX_DIAGNOSTIC_BYTES {
            diagnostic.push_str("… (truncated)");
            break;
        }
        diagnostic.push(character);
    }
    format!("Invalid action JSON: {diagnostic}. Send one object per line, using {{\"action\":\"<action name>\",\"input\":{{}}}}. Choose an action and its input from the current view.")
}

// Keep one buffered reader for the entire connection so multiple lines are not
// lost to read-ahead. Drain an oversized line without allocating the full input.
async fn input_line(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Result<Vec<u8>, String>>> {
    let mut bytes = Vec::new();
    (&mut *reader)
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_INPUT_BYTES {
        if bytes.last() != Some(&b'\n') {
            loop {
                let buffer = reader.fill_buf().await?;
                let newline = buffer.iter().position(|byte| *byte == b'\n');
                let consumed = newline.map_or(buffer.len(), |index| index + 1);
                reader.consume(consumed);
                if consumed == 0 || newline.is_some() {
                    break;
                }
            }
        }
        return Ok(Some(Err(format!(
            "Action input exceeds {MAX_INPUT_BYTES} bytes. Send one smaller JSON object per line."
        ))));
    }
    Ok(Some(Ok(bytes)))
}

fn parse_cli() -> Result<(PathBuf, Invocation)> {
    let mut arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let socket = if arguments.first().is_some_and(|arg| arg == "--socket") {
        ensure!(arguments.len() >= 2, "--socket requires a path");
        let socket = PathBuf::from(arguments.remove(1));
        arguments.remove(0);
        socket
    } else {
        default_socket()?
    };
    let socket = if socket.is_absolute() {
        socket
    } else {
        env::current_dir()?.join(socket)
    };
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().context("command arguments must be UTF-8"))
        .collect::<Result<Vec<_>>>()?;
    let invocation = match args.as_slice() {
        [] => Invocation::Interact,
        ["help" | "--help" | "-h"] => Invocation::Help,
        ["__daemon"] => Invocation::Daemon,
        ["start"] => Invocation::Start,
        ["status"] => Invocation::Status,
        ["stop"] => Invocation::Stop,
        ["restart"] => Invocation::Restart,
        _ => anyhow::bail!("unexpected daemon command\n\n{}", help()),
    };
    Ok((socket, invocation))
}

fn default_socket() -> Result<PathBuf> {
    let directory = match env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(directory) => PathBuf::from(directory),
        None => PathBuf::from(env::var_os("HOME").context("set --socket or HOME")?).join(".cache"),
    };
    let executable = env::current_exe()?;
    let name = executable
        .file_name()
        .context("executable has no name")?
        .to_str()
        .context("executable name must be UTF-8")?;
    Ok(directory.join(format!("agentview-{name}/socket")))
}

fn shell_quote(path: &Path) -> Result<String> {
    let value = path.to_str().context("CLI paths must be UTF-8")?;
    ensure!(
        !value.chars().any(char::is_control),
        "CLI paths must not contain control characters"
    );
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}
