use std::env;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentview::agent::DefaultContextState;
use agentview::prelude::*;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[path = "../../examples/chess_engine_agent/support.rs"]
mod chess_support;

use chess_support::{
    apply_engine_move, apply_player_move, ChessGameSource, ChessMoveSink, ChessViewModel,
    StockfishEngine,
};

const INTERNAL_DAEMON_ARG: &str = "--__agentview-daemon";
const INTERNAL_SHUTDOWN_ARG: &str = "--__agentview-shutdown";
const ADDR_ENV: &str = "AGENTVIEW_ADDR";
const STOCKFISH_BIN_ENV: &str = "AGENTVIEW_STOCKFISH_BIN";
const DEFAULT_ADDR: &str = "127.0.0.1:47631";
const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const DAEMON_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct HelloState {
    greeting: String,
    name: Option<String>,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "hello")]
struct HelloView {
    greeting: String,

    #[view(diff)]
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct HelloViewBuilder;

#[async_trait::async_trait]
impl ContextViewBuilder for HelloViewBuilder {
    type Source = Arc<Mutex<HelloState>>;
    type View = HelloView;

    async fn capture(&self, source: &Self::Source) -> Self::View {
        let state = source.lock().unwrap();
        HelloView {
            greeting: state.greeting.clone(),
            name: state.name.clone(),
        }
    }
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct HelloSystemDocument {
    #[view(paragraph)]
    instructions: &'static str,
}

#[derive(Default)]
struct NameSink {
    name: Option<String>,
}

#[async_trait::async_trait]
impl TurnSink<ControlReply> for NameSink {
    type Output = Option<String>;

    async fn on_event(&mut self, reply: ControlReply) {
        self.name = match reply {
            ControlReply::Text(text) => Some(text.trim().to_owned()),
            ControlReply::Structured(_) => None,
        };
    }

    async fn finish(self: Box<Self>) -> Self::Output {
        self.name.filter(|name| !name.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Help,
    ChessHelp,
    ChessActHelp,
    ChessHookHelp,
    Observe,
    Act { text: String },
    ChessObserve,
    ChessAct { uci: String },
    ChessHook { epoch: ViewEpoch },
    InternalDaemon,
    InternalShutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum DaemonRequest {
    Observe,
    Act { text: String },
    ChessObserve,
    ChessAct { uci: String },
    ChessHook { epoch: ViewEpoch },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DaemonResponse {
    Snapshot {
        event: String,
        epoch: ViewEpoch,
        turn_id: String,
        view: String,
        prompt: String,
    },
    Ok,
    Error {
        message: String,
    },
}

type HelloViewModel =
    DefaultAgentViewModel<HelloViewBuilder, HelloSystemDocument, IdentityTransform>;
type HelloApp = AgentViewApp<HelloViewModel, Turn, ()>;
type ChessApp = AgentViewApp<ChessViewModel, Turn, ()>;

struct DaemonState {
    hello: HelloApp,
    chess: ChessRuntime,
}

struct ChessRuntime {
    app: ChessApp,
    source: ChessGameSource,
    awake: ViewAwakeHandle,
    engine: StockfishEngine,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    match parse_cli(env::args().skip(1))? {
        CliCommand::Help => {
            print!("{}", help_text());
            Ok(())
        }
        CliCommand::ChessHelp => {
            print!("{}", chess_help_text());
            Ok(())
        }
        CliCommand::ChessActHelp => {
            print!("{}", chess_act_help_text());
            Ok(())
        }
        CliCommand::ChessHookHelp => {
            print!("{}", chess_hook_help_text());
            Ok(())
        }
        CliCommand::Observe => {
            let response = request_with_autostart(&DaemonRequest::Observe).await?;
            print_response(response)
        }
        CliCommand::Act { text } => {
            let response = request_with_autostart(&DaemonRequest::Act { text }).await?;
            print_response(response)
        }
        CliCommand::ChessObserve => {
            let response = request_with_autostart(&DaemonRequest::ChessObserve).await?;
            print_response(response)
        }
        CliCommand::ChessAct { uci } => {
            let response = request_with_autostart(&DaemonRequest::ChessAct { uci }).await?;
            print_response(response)
        }
        CliCommand::ChessHook { epoch } => {
            let response = request_with_autostart(&DaemonRequest::ChessHook { epoch }).await?;
            print_response(response)
        }
        CliCommand::InternalDaemon => run_daemon(daemon_addr()?).await,
        CliCommand::InternalShutdown => shutdown_daemon().await,
    }
}

fn parse_cli(args: impl IntoIterator<Item = String>) -> anyhow::Result<CliCommand> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(CliCommand::Help),
        [arg] if arg == "--help" || arg == "-h" || arg == "help" => Ok(CliCommand::Help),
        [arg] if arg == INTERNAL_DAEMON_ARG => Ok(CliCommand::InternalDaemon),
        [arg] if arg == INTERNAL_SHUTDOWN_ARG => Ok(CliCommand::InternalShutdown),
        [arg] if arg == "observe" => Ok(CliCommand::Observe),
        [cmd, text] if cmd == "act" => Ok(CliCommand::Act { text: text.clone() }),
        [cmd] if cmd == "act" => anyhow::bail!("usage: agentview act <text>\n\n{}", help_text()),
        [scope, arg] if scope == "chess" && is_help_arg(arg) => Ok(CliCommand::ChessHelp),
        [scope, cmd] if scope == "chess" && cmd == "observe" => Ok(CliCommand::ChessObserve),
        [scope, cmd, arg] if scope == "chess" && cmd == "act" && is_help_arg(arg) => {
            Ok(CliCommand::ChessActHelp)
        }
        [scope, cmd, rest @ ..] if scope == "chess" && cmd == "act" && !rest.is_empty() => {
            Ok(CliCommand::ChessAct {
                uci: parse_chess_act_uci(rest)?,
            })
        }
        [scope, cmd, arg] if scope == "chess" && cmd == "hook" && is_help_arg(arg) => {
            Ok(CliCommand::ChessHookHelp)
        }
        [scope, cmd, epoch] if scope == "chess" && cmd == "hook" => Ok(CliCommand::ChessHook {
            epoch: epoch
                .parse::<ViewEpoch>()
                .with_context(|| format!("failed to parse chess hook epoch `{epoch}`"))?,
        }),
        [scope, cmd] if scope == "chess" && cmd == "act" => {
            anyhow::bail!("usage: agentview chess act <uci>\n\n{}", help_text())
        }
        [scope, cmd] if scope == "chess" && cmd == "hook" => {
            anyhow::bail!("usage: agentview chess hook <epoch>\n\n{}", help_text())
        }
        _ => anyhow::bail!("{}", help_text()),
    }
}

fn is_help_arg(arg: &str) -> bool {
    arg == "--help" || arg == "-h" || arg == "help"
}

fn parse_chess_act_uci(args: &[String]) -> anyhow::Result<String> {
    let mut uci = None;
    let mut positional = None;
    let mut saw_context_flag = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if let Some(value) = arg.strip_prefix("--uci=") {
            set_once(&mut uci, "uci", value.to_owned())?;
        } else if arg == "--uci" {
            i += 1;
            let value = args
                .get(i)
                .with_context(|| "missing value after --uci")?
                .to_owned();
            set_once(&mut uci, "uci", value)?;
        } else if let Some(flag) = context_chess_act_flag(arg) {
            saw_context_flag = true;
            i += 1;
            args.get(i)
                .with_context(|| format!("missing value after --{flag}"))?;
        } else if let Some(flag) = context_chess_act_assignment(arg) {
            saw_context_flag = true;
            if arg.ends_with('=') {
                anyhow::bail!("expected non-empty value for --{flag}");
            }
        } else if arg.starts_with("--") {
            anyhow::bail!("unknown chess act option `{arg}`");
        } else {
            set_once(&mut positional, "positional uci", arg.clone())?;
        }

        i += 1;
    }

    match (uci, positional, saw_context_flag) {
        (Some(uci), None, _) if !uci.trim().is_empty() => Ok(uci),
        (Some(_), Some(_), _) => {
            anyhow::bail!("pass the chess move once, either as --uci <uci> or positional <uci>")
        }
        (None, Some(uci), false) if !uci.trim().is_empty() => Ok(uci),
        (None, Some(_), true) => {
            anyhow::bail!("when passing chess context flags, include the move as --uci <uci>")
        }
        _ => anyhow::bail!("usage: agentview chess act [--piece <piece>] [--from <square>] [--to <square>] [--promotion <piece>] --uci <uci>\n\n{}", help_text()),
    }
}

fn context_chess_act_flag(arg: &str) -> Option<&'static str> {
    match arg {
        "--piece" => Some("piece"),
        "--from" => Some("from"),
        "--to" => Some("to"),
        "--promotion" => Some("promotion"),
        _ => None,
    }
}

fn context_chess_act_assignment(arg: &str) -> Option<&'static str> {
    for flag in ["piece", "from", "to", "promotion"] {
        if arg.starts_with(&format!("--{flag}=")) {
            return Some(flag);
        }
    }
    None
}

fn set_once(slot: &mut Option<String>, label: &str, value: String) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("expected non-empty {label}");
    }
    if slot.replace(value).is_some() {
        anyhow::bail!("duplicate {label}");
    }
    Ok(())
}

fn help_text() -> &'static str {
    concat!(
        "agentview\n",
        "\n",
        "USAGE:\n",
        "  agentview observe\n",
        "  agentview act <text>\n",
        "  agentview chess observe\n",
        "  agentview chess act [--piece <piece>] [--from <square>] [--to <square>] [--promotion <piece>] --uci <uci>\n",
        "  agentview chess hook <epoch>\n",
        "\n",
        "COMMANDS:\n",
        "  observe    Print the current AgentView snapshot\n",
        "  act        Send a text reply for the latest turn\n",
        "  chess      Drive the chess AgentView example\n",
        "  help       Print this help\n",
        "\n",
        "ENVIRONMENT:\n",
        "  AGENTVIEW_ADDR=127.0.0.1:<port>  Isolate concurrent CLI sessions\n",
    )
}

fn chess_help_text() -> &'static str {
    concat!(
        "agentview chess\n",
        "\n",
        "USAGE:\n",
        "  agentview chess observe\n",
        "  agentview chess act [--piece <piece>] [--from <square>] [--to <square>] [--promotion <piece>] --uci <uci>\n",
        "  agentview chess hook <epoch>\n",
        "\n",
        "COMMANDS:\n",
        "  observe    Print the current chess AgentView snapshot\n",
        "  act        Send a chess move for the latest turn\n",
        "  hook       Wait for a later chess view epoch\n",
        "  help       Print this help\n",
    )
}

fn chess_act_help_text() -> &'static str {
    concat!(
        "agentview chess act\n",
        "\n",
        "USAGE:\n",
        "  agentview chess act <uci>\n",
        "  agentview chess act [--piece <piece>] [--from <square>] [--to <square>] [--promotion <piece>] --uci <uci>\n",
        "\n",
        "OPTIONS:\n",
        "  --uci <uci>              UCI move, such as e2e4 or e7e8q\n",
        "  --piece <piece>          Context flag for the moving piece\n",
        "  --from <square>          Context flag for the source square\n",
        "  --to <square>            Context flag for the target square\n",
        "  --promotion <piece>      Context flag for promotion piece\n",
        "\n",
        "NOTES:\n",
        "  Context flags are prompt context only; the submitted move is --uci <uci>.\n",
        "  When passing context flags, include the move as --uci <uci>.\n",
        "  Without context flags, positional <uci> is accepted.\n",
    )
}

fn chess_hook_help_text() -> &'static str {
    concat!(
        "agentview chess hook\n",
        "\n",
        "USAGE:\n",
        "  agentview chess hook <epoch>\n",
        "\n",
        "Wait for a chess view update after <epoch>.\n",
    )
}

fn daemon_addr() -> anyhow::Result<SocketAddr> {
    let raw = env::var(ADDR_ENV).unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let addr = raw
        .parse::<SocketAddr>()
        .with_context(|| format!("failed to parse {ADDR_ENV}={raw:?} as host:port"))?;
    if !addr.ip().is_loopback() {
        anyhow::bail!("agentview daemon address must be loopback, got {addr}");
    }
    Ok(addr)
}

async fn request_with_autostart(request: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let addr = daemon_addr()?;
    if let Ok(response) = send_once(addr, request).await {
        return Ok(response);
    }

    spawn_daemon(addr)?;

    let mut last_err = None;
    for _ in 0..100 {
        match send_once(addr, request).await {
            Ok(response) => return Ok(response),
            Err(err) => last_err = Some(err),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to reach agentview server")))
}

async fn shutdown_daemon() -> anyhow::Result<()> {
    match send_once(daemon_addr()?, &DaemonRequest::Shutdown).await {
        Ok(DaemonResponse::Ok) | Err(_) => Ok(()),
        Ok(DaemonResponse::Error { message }) => anyhow::bail!("{message}"),
        Ok(response) => anyhow::bail!("unexpected shutdown response: {response:?}"),
    }
}

fn spawn_daemon(addr: SocketAddr) -> anyhow::Result<()> {
    let current_exe = env::current_exe()?;
    let mut command = Command::new(current_exe);
    command
        .arg(INTERNAL_DAEMON_ARG)
        .env(ADDR_ENV, addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);

    command
        .spawn()
        .with_context(|| "failed to launch hidden agentview server")?;
    Ok(())
}

async fn send_once(addr: SocketAddr, request: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    let mut stream = timeout(DAEMON_CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
        .with_context(|| format!("failed to connect to {addr}"))?;
    let line = serde_json::to_string(request)?;

    let response = timeout(DAEMON_RESPONSE_TIMEOUT, async move {
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.shutdown().await?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).await?;
        anyhow::Ok(response)
    })
    .await
    .with_context(|| format!("timed out waiting for agentview server at {addr}"))??;

    if response.is_empty() {
        anyhow::bail!("agentview server closed without a response");
    }

    Ok(serde_json::from_str(&response)?)
}

fn print_response(response: DaemonResponse) -> anyhow::Result<()> {
    match response {
        DaemonResponse::Snapshot {
            event,
            epoch,
            turn_id,
            view,
            prompt,
        } => {
            println!("{event} epoch={epoch} turn={turn_id}");
            print_block("view", &view);
            print_block("prompt", &prompt);
            Ok(())
        }
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => anyhow::bail!("{message}"),
    }
}

fn print_block(label: &str, text: &str) {
    if text.contains('\n') {
        println!("{label}:\n{text}");
    } else {
        println!("{label}: {text}");
    }
}

async fn run_daemon(addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let mut state = new_daemon_state();

    loop {
        let (stream, _) = listener.accept().await?;
        if handle_connection(&mut state, stream).await? {
            break;
        }
    }

    Ok(())
}

fn new_daemon_state() -> DaemonState {
    DaemonState {
        hello: new_hello_app(),
        chess: new_chess_runtime(),
    }
}

fn new_hello_app() -> HelloApp {
    let source = Arc::new(Mutex::new(HelloState {
        greeting: "Hello".to_owned(),
        name: None,
    }));

    let view_model = DefaultAgentViewModel::new(
        HelloViewBuilder,
        HelloSystemDocument {
            instructions: "Ask for a name, then say hello.",
        },
        IdentityTransform,
    );

    let (app, _awake): (HelloApp, _) = AgentViewApp::new(
        view_model,
        source,
        PromptContext::<Turn, DefaultContextState>::without_system(),
    );
    app
}

fn new_chess_runtime() -> ChessRuntime {
    let source = ChessGameSource::new();
    let (app, awake) = AgentViewApp::new(
        ChessViewModel,
        source.clone(),
        PromptContext::<Turn, ()>::without_system(),
    );

    ChessRuntime {
        app,
        source,
        awake,
        engine: StockfishEngine::new(stockfish_command()),
    }
}

fn stockfish_command() -> String {
    env::var(STOCKFISH_BIN_ENV).unwrap_or_else(|_| "stockfish".to_owned())
}

fn render_snapshot_response<V>(
    event: &str,
    snapshot: &ViewSnapshot<V, ResolvedDocument>,
) -> DaemonResponse
where
    V: AgentView<Root = XmlNode>,
{
    let rendered = (|| -> anyhow::Result<(String, String)> {
        let view = render_pom_document(&resolve_system_document(Document::from_xml(
            snapshot.view.build_root()?,
        )))?;
        let prompt = render_pom_document(&snapshot.user_document)?;
        Ok((view, prompt))
    })();

    match rendered {
        Ok((view, prompt)) => DaemonResponse::Snapshot {
            event: event.to_owned(),
            epoch: snapshot.view_epoch,
            turn_id: snapshot.turn_id.to_string(),
            view,
            prompt,
        },
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    }
}

async fn handle_connection(state: &mut DaemonState, stream: TcpStream) -> anyhow::Result<bool> {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader.read_line(&mut request).await?;

    let response = match serde_json::from_str::<DaemonRequest>(&request) {
        Ok(DaemonRequest::Observe) => observe_hello(&mut state.hello).await,
        Ok(DaemonRequest::Act { text }) => act_hello(&mut state.hello, text).await,
        Ok(DaemonRequest::ChessObserve) => observe_chess(&mut state.chess).await,
        Ok(DaemonRequest::ChessAct { uci }) => act_chess(&mut state.chess, uci).await,
        Ok(DaemonRequest::ChessHook { epoch }) => hook_chess(&mut state.chess, epoch).await,
        Ok(DaemonRequest::Shutdown) => {
            write_response(reader.into_inner(), &DaemonResponse::Ok).await?;
            return Ok(true);
        }
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    };

    write_response(reader.into_inner(), &response).await?;
    Ok(false)
}

async fn observe_hello(app: &mut HelloApp) -> DaemonResponse {
    match app.observe("Ask the caller for their name.").await {
        Ok(snapshot) => render_snapshot_response("observe", &snapshot),
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    }
}

async fn act_hello(app: &mut HelloApp, text: String) -> DaemonResponse {
    let Some(turn_id) = app.latest_turn_id().map(ToOwned::to_owned) else {
        return DaemonResponse::Error {
            message: "no active turn; run `agentview observe` first".to_owned(),
        };
    };

    match app
        .act_with_sink(
            &turn_id,
            ControlReply::text(text),
            NameSink::default(),
            |session, source, name| {
                if let Some(name) = name {
                    source.lock().unwrap().name = Some(name.clone());
                    session.push_history(Turn::user(format!("name = {name}")));
                }
                Ok(())
            },
            "Say hello to the named caller.",
        )
        .await
    {
        Ok(update) => match update.snapshot() {
            Some(snapshot) => render_snapshot_response("update", snapshot),
            None => DaemonResponse::Error {
                message: "expected a full view update".to_owned(),
            },
        },
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    }
}

async fn observe_chess(runtime: &mut ChessRuntime) -> DaemonResponse {
    match runtime.app.observe("Choose white's next move.").await {
        Ok(snapshot) => render_snapshot_response("observe", &snapshot),
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    }
}

async fn act_chess(runtime: &mut ChessRuntime, uci: String) -> DaemonResponse {
    let Some(turn_id) = runtime.app.latest_turn_id().map(ToOwned::to_owned) else {
        return DaemonResponse::Error {
            message: "no active chess turn; run `agentview chess observe` first".to_owned(),
        };
    };

    match runtime
        .app
        .act_with_sink(
            &turn_id,
            ControlReply::structured(json!({ "uci": uci })),
            ChessMoveSink::from_source(&runtime.source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
    {
        Ok(update) => match update.snapshot() {
            Some(snapshot) => {
                if snapshot.view.engine_pending() {
                    schedule_chess_engine(runtime);
                }
                render_snapshot_response("act", snapshot)
            }
            None => DaemonResponse::Error {
                message: "expected a full chess view update".to_owned(),
            },
        },
        Err(err) => DaemonResponse::Error {
            message: err.to_string(),
        },
    }
}

async fn hook_chess(runtime: &mut ChessRuntime, epoch: ViewEpoch) -> DaemonResponse {
    match timeout(
        Duration::from_secs(5),
        runtime.app.hook(epoch, "Choose white's next move."),
    )
    .await
    {
        Ok(Ok(snapshot)) => render_snapshot_response("hook", &snapshot),
        Ok(Err(err)) => DaemonResponse::Error {
            message: err.to_string(),
        },
        Err(_) => DaemonResponse::Error {
            message: format!("timed out waiting for chess view epoch after {epoch}"),
        },
    }
}

fn schedule_chess_engine(runtime: &ChessRuntime) {
    let source = runtime.source.clone();
    let awake = runtime.awake.clone();
    let engine = runtime.engine.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = apply_engine_move(&source, &awake, &engine).await;
    });
}

async fn write_response(mut stream: TcpStream, response: &DaemonResponse) -> anyhow::Result<()> {
    let line = serde_json::to_string(response)?;
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_does_not_show_internal_mode() {
        let help = help_text();

        assert!(help.contains("observe"));
        assert!(help.contains("act"));
        assert!(!help.to_ascii_lowercase().contains("daemon"));
        assert!(!help.contains("__agentview"));
    }

    #[test]
    fn parses_public_commands() {
        assert_eq!(parse_cli(Vec::<String>::new()).unwrap(), CliCommand::Help);
        assert_eq!(
            parse_cli(["observe".to_owned()]).unwrap(),
            CliCommand::Observe
        );
        assert_eq!(
            parse_cli(["act".to_owned(), "world".to_owned()]).unwrap(),
            CliCommand::Act {
                text: "world".to_owned()
            }
        );
        assert_eq!(
            parse_cli(["chess".to_owned(), "observe".to_owned()]).unwrap(),
            CliCommand::ChessObserve
        );
        assert_eq!(
            parse_cli(["chess".to_owned(), "act".to_owned(), "e2e4".to_owned()]).unwrap(),
            CliCommand::ChessAct {
                uci: "e2e4".to_owned()
            }
        );
        assert_eq!(
            parse_cli([
                "chess".to_owned(),
                "act".to_owned(),
                "--piece".to_owned(),
                "P".to_owned(),
                "--from".to_owned(),
                "e2".to_owned(),
                "--to".to_owned(),
                "e4".to_owned(),
                "--uci".to_owned(),
                "e2e4".to_owned()
            ])
            .unwrap(),
            CliCommand::ChessAct {
                uci: "e2e4".to_owned()
            }
        );
        assert_eq!(
            parse_cli([
                "chess".to_owned(),
                "act".to_owned(),
                "--piece=P".to_owned(),
                "--from=e7".to_owned(),
                "--to=e8".to_owned(),
                "--promotion=q".to_owned(),
                "--uci=e7e8q".to_owned()
            ])
            .unwrap(),
            CliCommand::ChessAct {
                uci: "e7e8q".to_owned()
            }
        );
        assert_eq!(
            parse_cli(["chess".to_owned(), "hook".to_owned(), "1".to_owned()]).unwrap(),
            CliCommand::ChessHook { epoch: 1 }
        );
    }

    #[test]
    fn parses_internal_commands_without_listing_them() {
        assert_eq!(
            parse_cli([INTERNAL_DAEMON_ARG.to_owned()]).unwrap(),
            CliCommand::InternalDaemon
        );
        assert_eq!(
            parse_cli([INTERNAL_SHUTDOWN_ARG.to_owned()]).unwrap(),
            CliCommand::InternalShutdown
        );
    }
}
