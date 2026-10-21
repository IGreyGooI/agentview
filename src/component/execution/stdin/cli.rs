//! Process frontend for the retained stdin application.

use std::{
    env,
    fs::{self, File},
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{sleep_until, Instant};

use super::super::CommandCall;
use super::daemon::{self, request, Operation, Response};
use super::history::{self, History, HistoryReader, HistoryWriter, Stream};
use super::StdinApplicationOptions;
use crate::component::authoring::Component;

const FRAME_IDLE: Duration = Duration::from_millis(100);

enum Invocation {
    Help,
    Daemon,
    Interact,
    Start,
    Status,
    Stop,
    Restart,
    History(Option<String>),
}

impl Invocation {
    fn name(&self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Daemon => "__daemon",
            Self::Interact => "interact",
            Self::Start => "start",
            Self::Status => "status",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::History(_) => "history",
        }
    }
}

pub(super) async fn run(
    root: impl Fn() -> Component + Send + Sync + 'static,
    options: StdinApplicationOptions,
) -> ExitCode {
    let mut history = None;
    let mut code = match invoke(root, options, &mut history).await {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(error) => {
            let message = format!("agentview: {error:#}\n");
            if let Some(history) = &history {
                let _ = history.record_error(&format!("{error:#}"));
                let mut stderr = HistoryWriter::new(io::stderr(), history.clone(), Stream::Stderr);
                if let Err(error) = stderr.write_all(message.as_bytes()) {
                    eprintln!("agentview: could not record diagnostic: {error}");
                }
            } else {
                eprint!("{message}");
            }
            1
        }
    };
    if let Some(history) = history {
        if let Err(error) = history.finish(code) {
            eprintln!(
                "agentview: could not finish history {}: {error}",
                history.path().display()
            );
            code = 1;
        }
    }
    ExitCode::from(code)
}

async fn invoke(
    root: impl Fn() -> Component + Send + Sync + 'static,
    options: StdinApplicationOptions,
    history: &mut Option<History>,
) -> Result<bool> {
    let (socket, invocation) = parse_cli()?;
    if matches!(invocation, Invocation::Help) {
        print!("{}", help());
        return Ok(true);
    }
    let executable = env::current_exe()?;
    if let Invocation::History(selection) = &invocation {
        show_history(&executable, selection.as_deref())?;
        return Ok(true);
    }
    if !matches!(invocation, Invocation::Daemon) {
        *history = Some(History::create(&executable, &socket, invocation.name())?);
    }
    ensure!(
        options
            .idle_refresh_interval
            .is_none_or(|interval| !interval.is_zero()),
        "idle_refresh_interval must be positive"
    );
    // Validate and prepare connection metadata before any request can start a
    // daemon or change application state. Output must not fail later on a path
    // that the frontend could have rejected before executing the action.
    let command = format!(
        "{} --socket {}",
        shell_quote(&executable)?,
        shell_quote(&socket)?
    );
    match invocation {
        Invocation::Help | Invocation::History(_) => {
            unreachable!("read-only command returned before session setup")
        }
        Invocation::Daemon => daemon::serve(&socket, root).await?,
        Invocation::Interact => {
            return interact(
                &socket,
                &command,
                options,
                history.as_ref().expect("foreground history").clone(),
            )
            .await;
        }
        Invocation::Start | Invocation::Restart => {
            if matches!(invocation, Invocation::Restart) {
                request(&socket, &Operation::Stop, false).await?;
            }
            let response = request(&socket, &Operation::Observe, true)
                .await?
                .context("daemon did not start")?;
            print_view(
                Some(&response),
                &command,
                true,
                true,
                history.as_ref().expect("foreground history"),
            )?;
        }
        Invocation::Status => {
            let response = request(&socket, &Operation::Observe, false).await?;
            print_view(
                response.as_ref(),
                &command,
                response.is_some(),
                true,
                history.as_ref().expect("foreground history"),
            )?;
        }
        Invocation::Stop => {
            let response = request(&socket, &Operation::Stop, false).await?;
            print_view(
                response.as_ref(),
                &command,
                false,
                true,
                history.as_ref().expect("foreground history"),
            )?;
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
    let usage = "APP [--socket PATH] [start|status|stop|restart|history [latest|ID]|help]\n\n\
No subcommand: print the initial view, then read action JSON objects and print one resulting view per frame.\n\
Input format: {\"action\":\"<action name>\",\"input\":{}}\n\
Multiple top-level objects may span lines; no enclosing array or wrapper is needed.\n\
A terminating newline followed by 100 ms without input submits a frame.\n\
New input restarts that timer; complete JSON without a final newline keeps waiting.\n\
JSON is parsed after the boundary; incomplete or invalid frames return feedback.\n\
EOF submits the final frame immediately, without requiring a newline.\n\
The view describes available actions and their input schemas.\n\
The initial prepared view arrives without waiting for input; empty EOF adds no duplicate view.\n\
A missing daemon starts automatically. Application state survives EOF.\n\n\
  start    Start or connect to the daemon and show the current view\n\
  status   Show the running view or stopped status without starting a daemon\n\
  stop     Stop the daemon, await cleanup, and show the final view\n\
  restart  Stop the daemon and show a fresh session (discards the old state)\n\
  history  List recent CLI runs; history latest or history ID reads their transcript\n\
  help     Show this usage without starting a daemon\n\n\
stdout carries one view per frame with every action result in its feedback; diagnostics use stderr.\n\
The first view is complete; later views may omit ordinary unchanged content. Repeated content is delivered in full.\n\
Each new CLI invocation receives a complete initial view.\n\
Invalid JSON or action envelopes reject the whole frame before callbacks run.\n\
Invalid action arguments return feedback; later valid actions in the frame still run.\n\
Exit codes: 0 callbacks completed, 2 invalid/disabled input, 1 transport/runtime failure.\n\
Business acceptance or refusal is expressed by the callback's JSON result.\n\
Foreground interactions and lifecycle commands automatically save raw stdin, stdout, stderr, and a timed event log.\n\
History lives in $XDG_STATE_HOME/agentview-APP/history, or $HOME/.local/state/agentview-APP/history.\n\
Reading history does not start a daemon or execute actions.\n\
Use --socket PATH for independent sessions. State lasts until the daemon stops.\n\
Default socket: $XDG_RUNTIME_DIR/agentview-APP/socket, or\n\
                $HOME/.cache/agentview-APP/socket\n";
    usage.replace("APP", &name)
}

fn print_view(
    response: Option<&Response>,
    command: &str,
    running: bool,
    include_context: bool,
    history: &History,
) -> Result<()> {
    write_view(
        &mut HistoryWriter::new(io::stdout().lock(), history.clone(), Stream::Stdout),
        response,
        command,
        running,
        include_context,
    )
}

fn write_view(
    stdout: &mut impl Write,
    response: Option<&Response>,
    command: &str,
    running: bool,
    include_context: bool,
) -> Result<()> {
    // Deliver one prepared view at a time so durable recording does not add
    // disk syncs between individual formatting fragments.
    let mut view = Vec::new();
    let (status, message) = if running {
        (
            "running",
            "Send action JSON objects on stdin. End each batch with a newline, then wait 100 ms without input to submit a frame; EOF submits the final frame immediately. Application state survives EOF. Use status to observe, stop to end the session, or restart for a fresh session.",
        )
    } else {
        (
            "stopped",
            "The daemon is stopped. Use start or send an action to begin a new session. Any view below is the final snapshot of the stopped session.",
        )
    };
    let ok = response.is_none_or(|response| response.output.ok);
    writeln!(view, "<view ok=\"{ok}\">")?;
    if include_context {
        writeln!(view, "<daemon status=\"{status}\">{message}</daemon>\n")?;
        if let Some(response) = response {
            writeln!(view, "<session daemon_pid=\"{}\" />", response.daemon_pid)?;
            writeln!(
                view,
                "<connection command=\"{command}\" input=\"action JSON objects; final newline + 100 ms input idle, or EOF\" output=\"one view with all frame feedback\" />\n"
            )?;
        }
    }
    if let Some(response) = response {
        view.write_all(response.output.view.as_bytes())?;
    }
    writeln!(view, "</view>")?;
    stdout.write_all(&view)?;
    stdout.flush()?;
    Ok(())
}

async fn interact(
    socket: &Path,
    command: &str,
    options: StdinApplicationOptions,
    history: History,
) -> Result<bool> {
    let stdin = HistoryReader::new(super::input::stdin()?, history.clone());
    let mut stdout = HistoryWriter::new(io::stdout(), history, Stream::Stdout);
    interact_io(socket, command, options, stdin, &mut stdout).await
}

async fn interact_io(
    socket: &Path,
    command: &str,
    options: StdinApplicationOptions,
    mut stdin: impl AsyncRead + Unpin,
    stdout: &mut impl Write,
) -> Result<bool> {
    let mut frames = FrameInput::default();
    let mut connection = daemon::Connection::connect(socket, true)
        .await?
        .context("daemon did not start")?;
    let initial = connection.request(&Operation::Observe).await?;
    write_view(stdout, Some(&initial), command, true, true)?;
    let mut succeeded = true;
    loop {
        let operation = match options.idle_refresh_interval {
            Some(interval) => {
                // Cancellation of next() retains its buffered bytes and framing
                // deadline. Do not cancel an in-flight daemon request to poll.
                tokio::select! {
                    biased;
                    operation = frames.next(&mut stdin) => operation?,
                    _ = tokio::time::sleep(interval) => Some(Operation::Observe),
                }
            }
            None => frames.next(&mut stdin).await?,
        };
        let Some(operation) = operation else {
            break;
        };
        // Framing idle limits only input collection, never callback execution.
        let idle_refresh = matches!(&operation, Operation::Observe);
        let response = connection.request(&operation).await?;
        succeeded &= response.output.ok;
        if !idle_refresh || !response.output.view.is_empty() {
            write_view(stdout, Some(&response), command, true, false)?;
        }
    }
    Ok(succeeded)
}

fn invalid_action_message(error: &serde_json::Error) -> String {
    format!(
        "Invalid action frame: {error}. No actions from this frame were executed. Send top-level objects using {{\"action\":\"<action name>\",\"input\":{{}}}}; end the batch with a newline and wait 100 ms without input, or close stdin to submit. Choose an action and its input from the current view."
    )
}

fn decode_frame(bytes: &[u8]) -> std::result::Result<Vec<CommandCall>, serde_json::Error> {
    serde_json::Deserializer::from_slice(bytes)
        .into_iter::<CommandCall>()
        .collect()
}

fn saved_runs(directory: &Path) -> Result<Vec<(PathBuf, serde_json::Value)>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let metadata = path.join("session.json");
        if metadata.is_file() {
            runs.push((
                path,
                serde_json::from_slice::<serde_json::Value>(&fs::read(metadata)?)?,
            ));
        }
    }
    runs.sort_by(|left, right| {
        right.1["started_at_unix_ms"]
            .as_u64()
            .cmp(&left.1["started_at_unix_ms"].as_u64())
            .then_with(|| right.0.cmp(&left.0))
    });
    Ok(runs)
}

fn show_history(executable: &Path, selection: Option<&str>) -> Result<()> {
    let directory = history::root_for(executable)?;
    let mut stdout = io::stdout().lock();
    let Some(selection) = selection else {
        writeln!(stdout, "History: {}", directory.display())?;
        let runs = saved_runs(&directory)?;
        if runs.is_empty() {
            writeln!(stdout, "No recorded CLI runs.")?;
        }
        for (path, session) in runs.into_iter().take(20) {
            let status = session["exit_code"]
                .as_u64()
                .map(|code| format!("exit={code}"))
                .unwrap_or_else(|| "unfinished".to_owned());
            writeln!(
                stdout,
                "{}  {}  {}  {}",
                session["id"].as_str().unwrap_or_default(),
                session["invocation"].as_str().unwrap_or_default(),
                status,
                path.display()
            )?;
        }
        return Ok(());
    };
    let path = if selection == "latest" {
        saved_runs(&directory)?
            .into_iter()
            .next()
            .map(|(path, _)| path)
            .context("no recorded CLI runs")?
    } else {
        let mut components = Path::new(selection).components();
        ensure!(
            matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none(),
            "history ID must be a single run ID"
        );
        directory.join(selection)
    };
    print_history(&path, &mut stdout)
}

struct HistorySpan {
    stream: String,
    offset: u64,
    length: u64,
    elapsed_us: u64,
}

fn print_history_span(path: &Path, span: HistorySpan, stdout: &mut impl Write) -> Result<()> {
    ensure!(
        matches!(span.stream.as_str(), "stdin" | "stdout" | "stderr"),
        "unknown history stream"
    );
    writeln!(
        stdout,
        "\n--- {} +{}.{:06}s ---",
        span.stream,
        span.elapsed_us / 1_000_000,
        span.elapsed_us % 1_000_000
    )?;
    let mut input = File::open(path.join(format!("{}.raw", span.stream)))?;
    input.seek(SeekFrom::Start(span.offset))?;
    let copied = io::copy(&mut input.take(span.length), stdout)?;
    ensure!(
        copied == span.length,
        "history stream is shorter than its recorded event"
    );
    writeln!(stdout)?;
    Ok(())
}

fn print_history(path: &Path, stdout: &mut impl Write) -> Result<()> {
    let session: serde_json::Value = serde_json::from_slice(&fs::read(path.join("session.json"))?)?;
    writeln!(
        stdout,
        "Run: {}\nCommand: {}\nSocket: {}\nFiles: {}",
        session["id"].as_str().unwrap_or_default(),
        session["invocation"].as_str().unwrap_or_default(),
        session["socket"].as_str().unwrap_or_default(),
        path.display()
    )?;
    let mut events = io::BufReader::new(File::open(path.join("events.jsonl"))?);
    let mut pending: Option<HistorySpan> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if events.read_until(b'\n', &mut line)? == 0 || !line.ends_with(b"\n") {
            // An active writer may still be appending the final event. Only
            // complete JSONL records refer to committed stream ranges.
            break;
        }
        let event: serde_json::Value = serde_json::from_slice(&line)?;
        if event["kind"] == "bytes" {
            let stream = event["stream"].as_str().context("missing history stream")?;
            let offset = event["offset"].as_u64().context("missing history offset")?;
            let length = event["length"].as_u64().context("missing history length")?;
            if let Some(span) = pending.as_mut() {
                if span.stream == stream && span.offset.checked_add(span.length) == Some(offset) {
                    span.length = span
                        .length
                        .checked_add(length)
                        .context("history length overflow")?;
                    continue;
                }
            }
            if let Some(span) = pending.take() {
                print_history_span(path, span, stdout)?;
            }
            pending = Some(HistorySpan {
                stream: stream.to_owned(),
                offset,
                length,
                elapsed_us: event["elapsed_us"].as_u64().unwrap_or_default(),
            });
        } else {
            if let Some(span) = pending.take() {
                print_history_span(path, span, stdout)?;
            }
            if event["kind"] == "error" {
                writeln!(
                    stdout,
                    "\n--- error ---\n{}",
                    event["message"].as_str().unwrap_or_default()
                )?;
            } else if event["kind"] == "finished" {
                writeln!(stdout, "\n--- exit {} ---", event["exit_code"])?;
            }
        }
    }
    if let Some(span) = pending {
        print_history_span(path, span, stdout)?;
    }
    if session["exit_code"].is_null() {
        writeln!(
            stdout,
            "\nNo recorded completion; the run may still be open or was interrupted."
        )?;
    }
    Ok(())
}

#[derive(Default)]
struct FrameInput {
    bytes: Vec<u8>,
    deadline: Option<Instant>,
    eof: bool,
}

impl FrameInput {
    async fn next(&mut self, reader: &mut (impl AsyncRead + Unpin)) -> Result<Option<Operation>> {
        while !self.eof {
            let read = if let Some(deadline) = self.deadline {
                tokio::select! {
                    biased;
                    read = reader.read_buf(&mut self.bytes) => Some(read?),
                    _ = sleep_until(deadline) => None,
                }
            } else {
                Some(reader.read_buf(&mut self.bytes).await?)
            };
            match read {
                Some(0) => {
                    self.eof = true;
                    return Ok(self.take_operation(decode_frame(&self.bytes)));
                }
                Some(_) => self.deadline = Some(Instant::now() + FRAME_IDLE),
                None => {
                    self.deadline = None;
                    if !self.bytes.ends_with(b"\n") {
                        continue;
                    }
                    if let Some(operation) = self.take_operation(decode_frame(&self.bytes)) {
                        return Ok(Some(operation));
                    }
                }
            }
        }
        Ok(None)
    }

    fn take_operation(
        &mut self,
        decoded: std::result::Result<Vec<CommandCall>, serde_json::Error>,
    ) -> Option<Operation> {
        self.bytes.clear();
        self.deadline = None;
        match decoded {
            Ok(calls) if calls.is_empty() => None,
            Ok(calls) => Some(Operation::Frame { calls }),
            Err(error) => Some(Operation::Feedback {
                message: invalid_action_message(&error),
            }),
        }
    }
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
        ["history"] => Invocation::History(None),
        ["history", selection] => Invocation::History(Some((*selection).to_owned())),
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

#[cfg(test)]
mod tests {
    use super::{
        decode_frame, invalid_action_message, CommandCall, FrameInput, Operation, FRAME_IDLE,
    };
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn eof_submits_large_multiline_actions_as_one_frame() {
        let first = json!({
            "action": "remember",
            "input": {"text": "长期记忆\\\"\n".repeat(2048)},
        });
        let last = json!({"action": "query", "input": {"text": "最新状态"}});
        let mut input = serde_json::to_vec_pretty(&first).unwrap();
        assert!(input.len() > 8192);
        input.extend_from_slice(b"\n\n");
        input.extend(serde_json::to_vec_pretty(&last).unwrap());
        let mut reader = input.as_slice();
        let mut frames = FrameInput::default();
        let Some(Operation::Frame { calls }) = frames.next(&mut reader).await.unwrap() else {
            panic!("expected one frame");
        };
        assert_eq!(serde_json::to_value(calls).unwrap(), json!([first, last]));
        assert!(frames.next(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn idle_submits_complete_input_and_eof_submits_the_next_frame() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = FrameInput::default();
        let first = json!({"action": "inspect", "input": {}});
        writer
            .write_all(&serde_json::to_vec(&first).unwrap())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        let started = tokio::time::Instant::now();
        let Some(Operation::Frame { calls }) = tokio::time::timeout(
            FRAME_IDLE + Duration::from_secs(2),
            frames.next(&mut reader),
        )
        .await
        .unwrap()
        .unwrap() else {
            panic!("expected a frame after idle");
        };
        assert!(started.elapsed() >= FRAME_IDLE);
        assert_eq!(serde_json::to_value(calls).unwrap(), json!([first]));

        let last = json!({"action": "record", "input": {"text": "second frame"}});
        writer
            .write_all(&serde_json::to_vec(&last).unwrap())
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let Some(Operation::Frame { calls }) = frames.next(&mut reader).await.unwrap() else {
            panic!("expected the final frame at EOF");
        };
        assert_eq!(serde_json::to_value(calls).unwrap(), json!([last]));
        assert!(frames.next(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_an_input_wait_preserves_bytes_and_its_framing_deadline() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = FrameInput::default();
        writer
            .write_all(b"{\"action\":\"inspect\",\"input\":{}}\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(FRAME_IDLE / 2, frames.next(&mut reader))
                .await
                .is_err()
        );
        let Some(Operation::Frame { calls }) =
            tokio::time::timeout(FRAME_IDLE * 3 / 4, frames.next(&mut reader))
                .await
                .expect("refresh cancellation must not restart the 100 ms frame deadline")
                .unwrap()
        else {
            panic!("the buffered action must survive refresh cancellation");
        };
        assert_eq!(
            serde_json::to_value(calls).unwrap(),
            json!([{ "action": "inspect", "input": {} }])
        );
    }

    #[tokio::test]
    async fn complete_input_without_its_final_newline_waits_across_idle() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = FrameInput::default();
        writer
            .write_all(
                b"{\"action\":\"inspect\",\"input\":{}}\n{\"action\":\"inspect\",\"input\":{}}",
            )
            .await
            .unwrap();
        let next = frames.next(&mut reader);
        tokio::pin!(next);
        assert!(tokio::time::timeout(FRAME_IDLE * 2, &mut next)
            .await
            .is_err());
        writer.write_all(b"\n").await.unwrap();
        let Some(Operation::Frame { calls }) =
            tokio::time::timeout(FRAME_IDLE + Duration::from_secs(1), next)
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected both actions after the final newline becomes idle");
        };
        assert_eq!(calls.len(), 2);
    }

    #[tokio::test]
    async fn incomplete_newline_terminated_frame_is_rejected_at_idle_and_can_recover() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = FrameInput::default();
        writer
            .write_all(
                b"{\"action\":\"inspect\",\"input\":{}}\n{\"action\":\"record\",\"input\":{\n",
            )
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(
                FRAME_IDLE + Duration::from_secs(1),
                frames.next(&mut reader)
            )
            .await
            .unwrap()
            .unwrap(),
            Some(Operation::Feedback { .. })
        ));
        writer
            .write_all(br#"{"action":"inspect","input":{}}"#)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let Some(Operation::Frame { calls }) = frames.next(&mut reader).await.unwrap() else {
            panic!("expected a fresh frame after the incomplete frame was rejected");
        };
        assert_eq!(
            serde_json::to_value(calls).unwrap(),
            json!([{ "action": "inspect", "input": {} }])
        );
    }

    #[tokio::test]
    async fn new_bytes_restart_the_idle_deadline() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = FrameInput::default();
        let started = tokio::time::Instant::now();
        let write = async {
            writer
                .write_all(b"{\"action\":\"inspect\",\"input\":{}}\n")
                .await
                .unwrap();
            tokio::time::sleep(FRAME_IDLE / 2).await;
            writer
                .write_all(b" {\"action\":\"inspect\",\"input\":{}}\n")
                .await
                .unwrap();
        };
        let (operation, ()) = tokio::join!(
            tokio::time::timeout(FRAME_IDLE * 3, frames.next(&mut reader)),
            write,
        );
        let Some(Operation::Frame { calls }) = operation.unwrap().unwrap() else {
            panic!("expected the combined frame after the latest bytes become idle");
        };
        assert_eq!(calls.len(), 2);
        assert!(started.elapsed() >= FRAME_IDLE + FRAME_IDLE / 2);
    }

    #[tokio::test]
    async fn malformed_idle_frame_is_rejected_and_the_next_frame_can_run() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let mut frames = FrameInput::default();
        writer
            .write_all(b"{\"action\":\"inspect\",\"input\":{}} {not JSON}\n")
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(FRAME_IDLE * 2, frames.next(&mut reader))
                .await
                .unwrap()
                .unwrap(),
            Some(Operation::Feedback { .. })
        ));
        writer
            .write_all(br#"{"action":"inspect","input":{}}"#)
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        let Some(Operation::Frame { calls }) = frames.next(&mut reader).await.unwrap() else {
            panic!("expected a fresh valid frame after the rejected frame");
        };
        assert_eq!(calls.len(), 1);
    }

    #[tokio::test]
    async fn empty_input_is_not_a_frame_and_unfinished_eof_rejects_the_whole_frame() {
        let mut empty = &b" \n\t\r"[..];
        assert!(FrameInput::default()
            .next(&mut empty)
            .await
            .unwrap()
            .is_none());
        let mut unfinished = &br#"{"action":"inspect","input":{}} {"action":"record""#[..];
        assert!(matches!(
            FrameInput::default().next(&mut unfinished).await.unwrap(),
            Some(Operation::Feedback { .. })
        ));
    }

    #[test]
    fn stream_decoder_rejects_malformed_later_envelopes_without_returning_a_prefix() {
        for tail in [
            r#"{not JSON}"#,
            r#"{"action":"record","action":"inspect","input":{}}"#,
            r#"{"action":"record","input":{"text":"old","text":"new"}}"#,
            r#"{"action":"record","input":[]}"#,
        ] {
            let input = format!(r#"{{"action":"inspect","input":{{}}}} {tail}"#);
            assert!(decode_frame(input.as_bytes()).is_err());
        }
        // Typed argument validation belongs to dispatch, after envelope decoding.
        assert_eq!(
            decode_frame(
                br#"{"action":"record","input":{"text":42}} {"action":"inspect","input":{}}"#
            )
            .unwrap()
            .len(),
            2
        );
    }

    #[test]
    fn invalid_action_feedback_preserves_the_parser_diagnostic() {
        let key = "<unknown> & `*_[]#` C:\\memory\\graph\n\"quoted\"";
        let error = serde_json::from_value::<CommandCall>(json!({key: null})).unwrap_err();
        let message = invalid_action_message(&error);
        assert!(message.contains(&error.to_string()));
        assert!(message.contains(key));
    }
}
