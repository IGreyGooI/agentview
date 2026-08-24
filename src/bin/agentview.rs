use std::collections::VecDeque;
use std::env;
use std::io::{BufRead as _, Read};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use agentview::component::{
    execution::{
        ExternalAct, ExternalApplication, ExternalObservation, ExternalObservationKind,
        ProviderEvent,
    },
    prelude::*,
    ComponentHost,
};
use anyhow::Context;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::timeout;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const INTERNAL_DAEMON_ARG: &str = "--__agentview-daemon";
const INTERNAL_SHUTDOWN_ARG: &str = "--__agentview-shutdown";
const ADDR_ENV: &str = "AGENTVIEW_ADDR";
const TOKEN_ENV: &str = "AGENTVIEW_TOKEN";
const DEFAULT_ADDR: &str = "127.0.0.1:47631";
const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const DAEMON_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PENDING_AUTHENTICATIONS: usize = 32;
const MAX_DAEMON_AUTH_LINE_BYTES: usize = 4096;
const MAX_PROTOCOL_WIRE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROTOCOL_FRAMES: usize = 65_536;
const MAX_DAEMON_LINE_BYTES: usize = MAX_PROTOCOL_WIRE_BYTES * 6 + 4096;
const MAX_EXTERNAL_CLI_STATE_BYTES: usize = 8 * 1024 * 1024;
const MIN_SESSION_TOKEN_BYTES: usize = 32;
const MAX_SESSION_TOKEN_BYTES: usize = 1024;
const SERVER_PROOF_LABEL: &[u8] = b"agentview-daemon-server-v1";
const CLIENT_AUTH_LABEL: &[u8] = b"agentview-daemon-client-auth-v1";
const CLIENT_PROOF_LABEL: &[u8] = b"agentview-daemon-client-v1";

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct ExternalCliProps;

#[derive(Clone, Default)]
struct ExternalCliState {
    events: VecDeque<String>,
    rendered_bytes: usize,
}

impl ExternalCliState {
    fn rendered(&self) -> String {
        let mut rendered = String::with_capacity(self.rendered_bytes);
        for (index, event) in self.events.iter().enumerate() {
            if index > 0 {
                rendered.push_str(" | ");
            }
            rendered.push_str(event);
        }
        rendered
    }

    fn push(&mut self, entry: String) {
        const SEPARATOR_BYTES: usize = 3;
        let entry = bounded_suffix(entry, MAX_EXTERNAL_CLI_STATE_BYTES);
        let mut added = entry.len() + usize::from(!self.events.is_empty()) * SEPARATOR_BYTES;
        while !self.events.is_empty()
            && self.rendered_bytes.saturating_add(added) > MAX_EXTERNAL_CLI_STATE_BYTES
        {
            let removed = self.events.pop_front().expect("non-empty event queue");
            self.rendered_bytes -= removed.len();
            if !self.events.is_empty() {
                self.rendered_bytes -= SEPARATOR_BYTES;
            }
            added = entry.len() + usize::from(!self.events.is_empty()) * SEPARATOR_BYTES;
        }
        self.rendered_bytes += added;
        self.events.push_back(entry);
    }
}

fn bounded_suffix(value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn ensure_xml_renderable_text(value: &str) -> anyhow::Result<()> {
    if let Some(invalid) = value.chars().find(|character| {
        let code_point = u32::from(*character);
        !matches!(code_point, 0x9 | 0xA | 0xD)
            && !(0x20..=0xD7FF).contains(&code_point)
            && !(0xE000..=0xFFFD).contains(&code_point)
            && !(0x10000..=0x10FFFF).contains(&code_point)
    }) {
        anyhow::bail!(
            "external text character U+{:04X} is not allowed in XML content",
            u32::from(invalid)
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct ExternalProtocolFrameHeader {
    #[serde(rename = "type")]
    kind: String,
}

#[component]
fn external_cli_application(
    _props: ExternalCliProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let state = use_signal(ExternalCliState::default);
    let rendered = state
        .with(ExternalCliState::rendered)
        .expect("mounted external CLI Signal");
    let event_state = state.clone();

    view! {
        #[system_once]
        protocol { "External CLI protocol. Return text through the advertised act stream." }
        external_state { "{rendered}" }
        {
            EventListener::observe("agentview.cli.external.text", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let state = event_state.clone();
                    async move {
                        let entry = match event {
                            TextTurnEvent::TextDelta(text) => format!("delta:{text}"),
                            TextTurnEvent::TextComplete(text) => format!("complete:{text}"),
                        };
                        ensure_xml_renderable_text(&entry)?;
                        state.update(|state| state.push(entry))?;
                        Ok::<(), anyhow::Error>(())
                    }
                })
        }
    }
}

type ExternalCliApplication = ExternalApplication<ExternalCliProps>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Help,
    Observe { full_re_render: bool },
    ActText { text: String },
    ActProtocol,
    InternalDaemon,
    InternalShutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum DaemonRequest {
    Observe { full_re_render: bool },
    Act { protocol: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum DaemonAuthFrame {
    ClientHello {
        nonce: [u8; 32],
    },
    ServerHello {
        challenge: [u8; 32],
        proof: [u8; 32],
    },
    ClientProof {
        proof: [u8; 32],
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthenticatedDaemonEnvelope {
    authentication: [u8; 32],
    #[serde(flatten)]
    request: DaemonRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DaemonResponse {
    Observation {
        event: String,
        mode: String,
        generation: String,
        base_generation: Option<String>,
        content: String,
    },
    Ok,
    Error {
        message: String,
    },
}

enum SendOnceFault {
    Unavailable(anyhow::Error),
    NoRetry(anyhow::Error),
}

impl SendOnceFault {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Unavailable(fault) | Self::NoRetry(fault) => fault,
        }
    }
}

struct DaemonState {
    external: ExternalCliApplication,
}

struct AuthenticatedDaemonConnection {
    stream: TcpStream,
    request: DaemonRequest,
}

#[tokio::main]
async fn main() {
    if let Err(fault) = run().await {
        eprintln!("{fault}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    match parse_cli(env::args().skip(1))? {
        CliCommand::Help => {
            print!("{}", help_text());
            Ok(())
        }
        CliCommand::Observe { full_re_render } => {
            let token = session_token()?;
            let response =
                request_with_autostart(&token, &DaemonRequest::Observe { full_re_render }).await?;
            print_response(response)
        }
        CliCommand::ActText { text } => {
            let token = session_token()?;
            let protocol = encode_text_delta(&text)?;
            let response = request_with_autostart(&token, &DaemonRequest::Act { protocol }).await?;
            print_response(response)
        }
        CliCommand::ActProtocol => {
            let token = session_token()?;
            let protocol = read_protocol_stdin()?;
            let response = request_with_autostart(&token, &DaemonRequest::Act { protocol }).await?;
            print_response(response)
        }
        CliCommand::InternalDaemon => {
            let token = session_token()?;
            run_daemon(daemon_addr()?, token_digest(&token)).await
        }
        CliCommand::InternalShutdown => shutdown_daemon(&session_token()?).await,
    }
}

fn parse_cli(args: impl IntoIterator<Item = String>) -> anyhow::Result<CliCommand> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(CliCommand::Help),
        [arg] if is_help_arg(arg) => Ok(CliCommand::Help),
        [arg] if arg == INTERNAL_DAEMON_ARG => Ok(CliCommand::InternalDaemon),
        [arg] if arg == INTERNAL_SHUTDOWN_ARG => Ok(CliCommand::InternalShutdown),
        [arg] if arg == "observe" => Ok(CliCommand::Observe {
            full_re_render: false,
        }),
        [cmd, flag] if cmd == "observe" && flag == "--full-re-render" => Ok(CliCommand::Observe {
            full_re_render: true,
        }),
        [cmd, flag] if cmd == "act" && flag == "--protocol" => Ok(CliCommand::ActProtocol),
        [cmd, text] if cmd == "act" => Ok(CliCommand::ActText { text: text.clone() }),
        [cmd] if cmd == "act" => {
            anyhow::bail!(
                "usage: agentview act <text> | agentview act --protocol\n\n{}",
                help_text()
            )
        }
        _ => anyhow::bail!("{}", help_text()),
    }
}

fn is_help_arg(argument: &str) -> bool {
    matches!(argument, "--help" | "-h" | "help")
}

fn help_text() -> &'static str {
    concat!(
        "agentview\n",
        "\n",
        "USAGE:\n",
        "  agentview observe [--full-re-render]\n",
        "  agentview act <text>\n",
        "  agentview act --protocol\n",
        "\n",
        "COMMANDS:\n",
        "  observe    Finish the current reaction and return the next observation\n",
        "  act        Submit text or a JSON-lines text protocol on stdin, then observe\n",
        "  help       Print this help\n",
        "\n",
        "PROTOCOL FRAMES:\n",
        "  {\"type\":\"text_delta\",\"text\":\"...\"}\n",
        "  {\"type\":\"text_complete\",\"text\":\"...\"}  optional, stops polling\n",
        "  {\"type\":\"disconnect\"}                            abnormal termination\n",
        "  stdin EOF                                               normal termination\n",
        "\n",
        "ENVIRONMENT:\n",
        "  AGENTVIEW_ADDR=127.0.0.1:<port>  Isolate concurrent CLI sessions\n",
        "  AGENTVIEW_TOKEN=<32+ byte secret>  Authenticate one CLI session\n",
    )
}

fn encode_text_delta(text: &str) -> anyhow::Result<String> {
    let frame = serde_json::json!({ "type": "text_delta", "text": text });
    Ok(format!("{}\n", serde_json::to_string(&frame)?))
}

fn read_protocol_stdin() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    read_protocol(&mut stdin.lock())
}

fn read_protocol(reader: &mut impl std::io::BufRead) -> anyhow::Result<String> {
    let mut protocol = String::new();
    let mut frames = 0_usize;
    loop {
        let start = protocol.len();
        let remaining = MAX_PROTOCOL_WIRE_BYTES + 1 - start.min(MAX_PROTOCOL_WIRE_BYTES + 1);
        if remaining == 0 {
            anyhow::bail!("external text protocol exceeded configured wire limit");
        }
        let read = reader
            .take(remaining as u64)
            .read_line(&mut protocol)
            .context("failed to read external text protocol from stdin")?;
        if protocol.len() > MAX_PROTOCOL_WIRE_BYTES {
            anyhow::bail!("external text protocol exceeded configured wire limit");
        }
        if read > 0 {
            frames += 1;
            if frames > MAX_PROTOCOL_FRAMES {
                anyhow::bail!("external text protocol exceeded configured frame limit");
            }
        }
        if read == 0 || protocol_frame_stops_input(&protocol[start..]) {
            break;
        }
    }
    Ok(protocol)
}

fn protocol_frame_stops_input(line: &str) -> bool {
    serde_json::from_str::<ExternalProtocolFrameHeader>(line)
        .is_ok_and(|frame| matches!(frame.kind.as_str(), "text_complete" | "disconnect"))
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

fn session_token() -> anyhow::Result<String> {
    let token = env::var(TOKEN_ENV)
        .with_context(|| format!("{TOKEN_ENV} must be set to a private session token"))?;
    if !(MIN_SESSION_TOKEN_BYTES..=MAX_SESSION_TOKEN_BYTES).contains(&token.len()) {
        anyhow::bail!(
            "{TOKEN_ENV} must contain between {MIN_SESSION_TOKEN_BYTES} and \
             {MAX_SESSION_TOKEN_BYTES} bytes"
        );
    }
    if token.chars().any(char::is_control) {
        anyhow::bail!("{TOKEN_ENV} must not contain control characters");
    }
    Ok(token)
}

fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn random_nonce() -> anyhow::Result<[u8; 32]> {
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)
        .context("failed to generate external daemon authentication nonce")?;
    Ok(nonce)
}

fn protocol_mac(key: &[u8; 32], label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a 32-byte session key");
    mac.update(label);
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

fn protocol_mac_matches(
    key: &[u8; 32],
    label: &[u8],
    parts: &[&[u8]],
    candidate: &[u8; 32],
) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a 32-byte session key");
    mac.update(label);
    for part in parts {
        mac.update(part);
    }
    mac.verify_slice(candidate).is_ok()
}

async fn request_with_autostart(
    token: &str,
    request: &DaemonRequest,
) -> anyhow::Result<DaemonResponse> {
    let addr = daemon_addr()?;
    match send_once(addr, token, request).await {
        Ok(response) => return Ok(response),
        Err(SendOnceFault::NoRetry(fault)) => return Err(fault),
        Err(SendOnceFault::Unavailable(_)) => {}
    }

    spawn_daemon(addr, token)?;

    let mut last_fault = None;
    for _ in 0..100 {
        match send_once(addr, token, request).await {
            Ok(response) => return Ok(response),
            Err(SendOnceFault::Unavailable(fault)) => last_fault = Some(fault),
            Err(SendOnceFault::NoRetry(fault)) => return Err(fault),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Err(last_fault.unwrap_or_else(|| anyhow::anyhow!("failed to reach agentview server")))
}

async fn shutdown_daemon(token: &str) -> anyhow::Result<()> {
    match send_once(daemon_addr()?, token, &DaemonRequest::Shutdown).await {
        Ok(DaemonResponse::Ok) => Ok(()),
        Ok(DaemonResponse::Error { message }) => anyhow::bail!("{message}"),
        Ok(response) => anyhow::bail!("unexpected shutdown response: {response:?}"),
        Err(fault) => Err(fault.into_error()),
    }
}

fn spawn_daemon(addr: SocketAddr, token: &str) -> anyhow::Result<()> {
    let current_exe = env::current_exe()?;
    let mut command = Command::new(current_exe);
    configure_daemon_environment(&mut command, addr, token);
    command
        .arg(INTERNAL_DAEMON_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);

    command
        .spawn()
        .context("failed to launch hidden agentview server")?;
    Ok(())
}

fn configure_daemon_environment(command: &mut Command, addr: SocketAddr, token: &str) {
    #[cfg(windows)]
    let windows_launch_environment = ["SystemRoot", "WINDIR"]
        .map(|name| (name, env::var_os(name)))
        .into_iter()
        .filter_map(|(name, value)| value.map(|value| (name, value)));

    command.env_clear();
    #[cfg(windows)]
    command.envs(windows_launch_environment);
    command
        .env(ADDR_ENV, addr.to_string())
        .env(TOKEN_ENV, token);
}

async fn send_once(
    addr: SocketAddr,
    token: &str,
    request: &DaemonRequest,
) -> Result<DaemonResponse, SendOnceFault> {
    let session_key = token_digest(token);
    let request_json = serde_json::to_vec(request)
        .context("failed to encode external daemon request")
        .map_err(SendOnceFault::NoRetry)?;
    if request_json.len() > MAX_DAEMON_LINE_BYTES {
        return Err(SendOnceFault::NoRetry(anyhow::anyhow!(
            "external daemon request exceeded configured wire limit"
        )));
    }
    let mut stream = timeout(DAEMON_CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))
        .and_then(|stream| stream.with_context(|| format!("failed to connect to {addr}")))
        .map_err(SendOnceFault::Unavailable)?;

    let response = timeout(DAEMON_RESPONSE_TIMEOUT, async move {
        let client_nonce = random_nonce()?;
        let hello = serde_json::to_vec(&DaemonAuthFrame::ClientHello {
            nonce: client_nonce,
        })?;
        stream.write_all(&hello).await?;
        stream.write_all(b"\n").await?;

        let mut reader = BufReader::new(stream);
        let proof =
            read_bounded_line(&mut reader, MAX_DAEMON_AUTH_LINE_BYTES, "server proof").await?;
        let (challenge, server_proof) = match serde_json::from_str::<DaemonAuthFrame>(&proof)
            .context("external daemon proof was invalid")?
        {
            DaemonAuthFrame::ServerHello { challenge, proof } => (challenge, proof),
            DaemonAuthFrame::ClientHello { .. } | DaemonAuthFrame::ClientProof { .. } => {
                anyhow::bail!("external daemon proof was invalid")
            }
        };
        if !protocol_mac_matches(
            &session_key,
            SERVER_PROOF_LABEL,
            &[&client_nonce, &challenge],
            &server_proof,
        ) {
            anyhow::bail!("not authorized");
        }

        let client_proof = protocol_mac(
            &session_key,
            CLIENT_AUTH_LABEL,
            &[&challenge, &client_nonce],
        );
        let client_proof = serde_json::to_vec(&DaemonAuthFrame::ClientProof {
            proof: client_proof,
        })?;
        reader.get_mut().write_all(&client_proof).await?;
        reader.get_mut().write_all(b"\n").await?;

        let authentication = protocol_mac(
            &session_key,
            CLIENT_PROOF_LABEL,
            &[&challenge, &request_json],
        );
        let line = serde_json::to_vec(&AuthenticatedDaemonEnvelope {
            authentication,
            request: request.clone(),
        })?;
        if line.len() > MAX_DAEMON_LINE_BYTES {
            anyhow::bail!("external daemon request exceeded configured wire limit");
        }
        reader.get_mut().write_all(&line).await?;
        reader.get_mut().write_all(b"\n").await?;
        reader.get_mut().shutdown().await?;

        read_bounded_line(&mut reader, MAX_DAEMON_LINE_BYTES, "response").await
    })
    .await
    .with_context(|| format!("timed out waiting for agentview server at {addr}"))
    .and_then(|response| response)
    .map_err(SendOnceFault::NoRetry)?;

    if response.is_empty() {
        return Err(SendOnceFault::NoRetry(anyhow::anyhow!(
            "agentview server closed without a response"
        )));
    }
    serde_json::from_str(&response)
        .context("external daemon response was invalid")
        .map_err(SendOnceFault::NoRetry)
}

async fn read_bounded_line<R>(
    reader: &mut R,
    limit: usize,
    label: &'static str,
) -> anyhow::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .with_context(|| format!("failed to read external daemon {label}"))?;
    if bytes.len() > limit {
        anyhow::bail!("external daemon {label} exceeded configured wire limit");
    }
    String::from_utf8(bytes).with_context(|| format!("external daemon {label} was not UTF-8"))
}

fn print_response(response: DaemonResponse) -> anyhow::Result<()> {
    match response {
        DaemonResponse::Error { message } => anyhow::bail!("{message}"),
        response => {
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
    }
}

async fn run_daemon(addr: SocketAddr, token_digest: [u8; 32]) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let mut state = new_daemon_state();
    let mut authentications = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept(), if authentications.len() < MAX_PENDING_AUTHENTICATIONS => {
                let (stream, _) = accepted?;
                authentications.spawn(authenticate_connection(token_digest, stream));
            }
            completed = authentications.join_next(), if !authentications.is_empty() => {
                match completed.expect("non-empty authentication set") {
                    Ok(Ok(connection)) => {
                        match handle_authenticated_connection(&mut state, connection).await {
                            Ok(true) => {
                                authentications.abort_all();
                                break;
                            }
                            Ok(false) => {}
                            Err(fault) => {
                                tracing::warn!(error = %fault, "external daemon connection failed");
                            }
                        }
                    }
                    Ok(Err(fault)) => {
                        tracing::warn!(error = %fault, "external daemon connection failed");
                    }
                    Err(fault) => {
                        tracing::warn!(error = %fault, "external daemon authentication task failed");
                    }
                }
            }
        }
    }
    Ok(())
}

fn new_daemon_state() -> DaemonState {
    DaemonState {
        external: ExternalApplication::new(ComponentHost::new(
            external_cli_application,
            ExternalCliProps,
        )),
    }
}

async fn authenticate_connection(
    token_digest: [u8; 32],
    stream: TcpStream,
) -> anyhow::Result<AuthenticatedDaemonConnection> {
    let mut reader = BufReader::new(stream);
    let hello = timeout(
        DAEMON_REQUEST_TIMEOUT,
        read_bounded_line(&mut reader, MAX_DAEMON_AUTH_LINE_BYTES, "client hello"),
    )
    .await
    .context("timed out reading external daemon client hello")??;
    let client_nonce = match serde_json::from_str::<DaemonAuthFrame>(&hello) {
        Ok(DaemonAuthFrame::ClientHello { nonce }) => nonce,
        _ => anyhow::bail!("external daemon client hello was invalid"),
    };
    let challenge = random_nonce()?;
    let proof = protocol_mac(
        &token_digest,
        SERVER_PROOF_LABEL,
        &[&client_nonce, &challenge],
    );
    write_auth_frame(
        reader.get_mut(),
        &DaemonAuthFrame::ServerHello { challenge, proof },
    )
    .await?;

    let client_proof = timeout(
        DAEMON_REQUEST_TIMEOUT,
        read_bounded_line(&mut reader, MAX_DAEMON_AUTH_LINE_BYTES, "client proof"),
    )
    .await
    .context("timed out reading external daemon client proof")??;
    let client_proof = match serde_json::from_str::<DaemonAuthFrame>(&client_proof) {
        Ok(DaemonAuthFrame::ClientProof { proof }) => proof,
        _ => anyhow::bail!("external daemon client proof was invalid"),
    };
    if !protocol_mac_matches(
        &token_digest,
        CLIENT_AUTH_LABEL,
        &[&challenge, &client_nonce],
        &client_proof,
    ) {
        write_response(
            reader.into_inner(),
            &DaemonResponse::Error {
                message: "not authorized".to_owned(),
            },
        )
        .await?;
        anyhow::bail!("external daemon client was not authorized");
    }

    let request = timeout(
        DAEMON_REQUEST_TIMEOUT,
        read_bounded_line(&mut reader, MAX_DAEMON_LINE_BYTES, "authenticated request"),
    )
    .await
    .context("timed out reading external daemon authenticated request")??;
    let envelope = serde_json::from_str::<AuthenticatedDaemonEnvelope>(&request)
        .context("external daemon authenticated request was invalid")?;
    let request_json = serde_json::to_vec(&envelope.request)?;
    if !protocol_mac_matches(
        &token_digest,
        CLIENT_PROOF_LABEL,
        &[&challenge, &request_json],
        &envelope.authentication,
    ) {
        write_response(
            reader.into_inner(),
            &DaemonResponse::Error {
                message: "not authorized".to_owned(),
            },
        )
        .await?;
        anyhow::bail!("external daemon request was not authorized");
    }

    Ok(AuthenticatedDaemonConnection {
        stream: reader.into_inner(),
        request: envelope.request,
    })
}

async fn handle_authenticated_connection(
    state: &mut DaemonState,
    connection: AuthenticatedDaemonConnection,
) -> anyhow::Result<bool> {
    let response = match connection.request {
        DaemonRequest::Observe { full_re_render } => {
            observe_external(&mut state.external, full_re_render).await
        }
        DaemonRequest::Act { protocol } => act_external(&mut state.external, protocol).await,
        DaemonRequest::Shutdown => {
            write_response(connection.stream, &DaemonResponse::Ok).await?;
            return Ok(true);
        }
    };

    write_response(connection.stream, &response).await?;
    Ok(false)
}

async fn observe_external(
    application: &mut ExternalCliApplication,
    full_re_render: bool,
) -> DaemonResponse {
    let observation = if full_re_render {
        application.full_re_render()
    } else {
        application.observe().await
    };
    observation_response("observe", observation)
}

async fn act_external(
    application: &mut ExternalCliApplication,
    protocol: String,
) -> DaemonResponse {
    if protocol.len() > MAX_PROTOCOL_WIRE_BYTES {
        return DaemonResponse::Error {
            message: "external text protocol exceeded configured wire limit".to_owned(),
        };
    }
    observation_response(
        "act",
        application
            .act(ExternalAct::__from_cli_json_lines(protocol))
            .await,
    )
}

fn observation_response<E>(
    event: &str,
    observation: Result<ExternalObservation, E>,
) -> DaemonResponse
where
    E: std::fmt::Display,
{
    match observation {
        Ok(observation) => {
            let mode = match observation.kind() {
                ExternalObservationKind::Full => "full",
                ExternalObservationKind::Delta => "delta",
                _ => {
                    return DaemonResponse::Error {
                        message: "unsupported external observation kind".to_owned(),
                    };
                }
            };
            DaemonResponse::Observation {
                event: event.to_owned(),
                mode: mode.to_owned(),
                generation: format!("{:?}", observation.generation()),
                base_generation: observation
                    .base_generation()
                    .map(|generation| format!("{generation:?}")),
                content: observation.content().to_owned(),
            }
        }
        Err(fault) => DaemonResponse::Error {
            message: fault.to_string(),
        },
    }
}

async fn write_response(mut stream: TcpStream, response: &DaemonResponse) -> anyhow::Result<()> {
    let line = serde_json::to_string(response)?;
    if line.len() > MAX_DAEMON_LINE_BYTES {
        anyhow::bail!("external daemon response exceeded configured wire limit");
    }
    timeout(DAEMON_RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        anyhow::Ok(())
    })
    .await
    .context("timed out writing external daemon response")??;
    Ok(())
}

async fn write_auth_frame(stream: &mut TcpStream, frame: &DaemonAuthFrame) -> anyhow::Result<()> {
    let line = serde_json::to_string(frame)?;
    if line.len() > MAX_DAEMON_AUTH_LINE_BYTES {
        anyhow::bail!("external daemon authentication frame exceeded configured wire limit");
    }
    timeout(DAEMON_RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        anyhow::Ok(())
    })
    .await
    .context("timed out writing external daemon authentication proof")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAEMON_ENV_PROBE_ENV: &str = "AGENTVIEW_DAEMON_ENV_PROBE";
    const DAEMON_ENV_SENTINEL: &str = "AGENTVIEW_TEST_AMBIENT_SECRET";
    const DAEMON_ENV_PROBE_ADDR: &str = "127.0.0.1:47639";
    const DAEMON_ENV_PROBE_TOKEN: &str = "test-only-daemon-token";
    const LLVM_PROFILE_RUNTIME_ENV: &str = "__LLVM_PROFILE_RT_INIT_ONCE";

    #[test]
    fn daemon_probe_accepts_only_the_exact_llvm_runtime_marker() {
        assert!(daemon_probe_environment_name_is_allowed(
            std::ffi::OsStr::new(LLVM_PROFILE_RUNTIME_ENV)
        ));
        assert!(!daemon_probe_environment_name_is_allowed(
            std::ffi::OsStr::new("__LLVM_PROFILE_RT_INIT_ONCE_LOOKALIKE")
        ));
    }

    #[test]
    fn spawned_daemon_command_clears_ambient_environment_and_keeps_launch_allowlist() {
        let mut command = Command::new(env::current_exe().expect("current test executable"));
        command.env(DAEMON_ENV_SENTINEL, "must-not-reach-daemon");
        configure_daemon_environment(
            &mut command,
            DAEMON_ENV_PROBE_ADDR.parse().expect("probe address"),
            DAEMON_ENV_PROBE_TOKEN,
        );
        command
            .env(DAEMON_ENV_PROBE_ENV, "1")
            .arg("--exact")
            .arg("tests::daemon_environment_probe")
            .arg("--nocapture");

        let output = command.output().expect("run daemon environment probe");
        assert!(
            output.status.success(),
            "daemon environment probe failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn daemon_environment_probe() {
        if env::var_os(DAEMON_ENV_PROBE_ENV).is_none() {
            return;
        }

        assert!(
            env::var_os(DAEMON_ENV_SENTINEL).is_none(),
            "daemon inherited the ambient sentinel"
        );
        assert!(
            env::var_os(ADDR_ENV).as_deref() == Some(std::ffi::OsStr::new(DAEMON_ENV_PROBE_ADDR)),
            "daemon address was not preserved"
        );
        assert!(
            env::var_os(TOKEN_ENV).as_deref() == Some(std::ffi::OsStr::new(DAEMON_ENV_PROBE_TOKEN)),
            "daemon token was not preserved"
        );

        let unexpected = env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| !daemon_probe_environment_name_is_allowed(name))
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            unexpected.is_empty(),
            "daemon inherited unexpected environment names: {unexpected:?}"
        );
    }

    fn daemon_probe_environment_name_is_allowed(name: &std::ffi::OsStr) -> bool {
        let name = name.to_string_lossy();
        [ADDR_ENV, TOKEN_ENV, DAEMON_ENV_PROBE_ENV]
            .iter()
            .any(|allowed| environment_name_matches(&name, allowed))
            || environment_name_matches(&name, LLVM_PROFILE_RUNTIME_ENV)
            || (cfg!(windows)
                && ["SystemRoot", "WINDIR"]
                    .iter()
                    .any(|allowed| environment_name_matches(&name, allowed)))
    }

    fn environment_name_matches(actual: &str, expected: &str) -> bool {
        if cfg!(windows) {
            actual.eq_ignore_ascii_case(expected)
        } else {
            actual == expected
        }
    }

    #[test]
    fn help_describes_external_protocol_without_internal_mode() {
        let help = help_text();

        assert!(help.contains("--full-re-render"));
        assert!(help.contains("--protocol"));
        assert!(help.contains("text_delta"));
        assert!(help.contains("text_complete"));
        assert!(help.contains("disconnect"));
        assert!(help.contains(TOKEN_ENV));
        assert!(!help.to_ascii_lowercase().contains("daemon"));
        assert!(!help.contains("__agentview"));
    }

    #[test]
    fn parses_public_commands() {
        assert_eq!(parse_cli(Vec::<String>::new()).unwrap(), CliCommand::Help);
        assert_eq!(
            parse_cli(["observe".to_owned()]).unwrap(),
            CliCommand::Observe {
                full_re_render: false
            }
        );
        assert_eq!(
            parse_cli(["observe".to_owned(), "--full-re-render".to_owned()]).unwrap(),
            CliCommand::Observe {
                full_re_render: true
            }
        );
        assert_eq!(
            parse_cli(["act".to_owned(), "--protocol".to_owned()]).unwrap(),
            CliCommand::ActProtocol
        );
        assert_eq!(
            parse_cli(["act".to_owned(), "world".to_owned()]).unwrap(),
            CliCommand::ActText {
                text: "world".to_owned()
            }
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
