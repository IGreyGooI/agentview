use std::{fmt, path::PathBuf, process::Stdio, str::FromStr, time::Duration};

use chess::ChessMove;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{timeout, timeout_at, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UciSpawnFailure {
    child_shutdown_observed: bool,
    deadline_exhausted: bool,
}

impl UciSpawnFailure {
    fn before_child() -> Self {
        Self {
            child_shutdown_observed: true,
            deadline_exhausted: false,
        }
    }

    fn before_expired_deadline() -> Self {
        Self {
            child_shutdown_observed: true,
            deadline_exhausted: true,
        }
    }

    fn after_child_cleanup(child_shutdown_observed: bool) -> Self {
        Self {
            child_shutdown_observed,
            deadline_exhausted: false,
        }
    }

    fn after_deadline_cleanup(child_shutdown_observed: bool) -> Self {
        Self {
            child_shutdown_observed,
            deadline_exhausted: true,
        }
    }

    pub(crate) fn child_shutdown_observed(self) -> bool {
        self.child_shutdown_observed
    }

    pub(crate) fn deadline_exhausted(self) -> bool {
        self.deadline_exhausted
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StrictUciMoveError;

impl fmt::Display for StrictUciMoveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid UCI move")
    }
}

pub(crate) fn parse_strict_uci_move(candidate: &str) -> Result<ChessMove, StrictUciMoveError> {
    strict_shape(candidate)
        .then(|| ChessMove::from_str(candidate).map_err(|_| StrictUciMoveError))
        .ok_or(StrictUciMoveError)?
}

fn strict_shape(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    let squares_are_typed = matches!(bytes.first(), Some(b'a'..=b'h'))
        && matches!(bytes.get(1), Some(b'1'..=b'8'))
        && matches!(bytes.get(2), Some(b'a'..=b'h'))
        && matches!(bytes.get(3), Some(b'1'..=b'8'));
    match bytes.len() {
        4 => squares_are_typed,
        5 => squares_are_typed && matches!(bytes[4], b'b' | b'n' | b'q' | b'r'),
        _ => false,
    }
}

const MAX_LINE_BYTES: usize = 4_096;
const MAX_RESPONSE_LINES: usize = 1_024;

pub struct UciProcessConfig {
    program: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UciConfigError;

impl UciProcessConfig {
    pub(crate) fn stockfish(program: PathBuf) -> Result<Self, UciConfigError> {
        if !program.is_absolute() || program.as_os_str().is_empty() {
            return Err(UciConfigError);
        }
        Ok(Self { program })
    }
}

pub(crate) struct UciEngine {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: AsyncBufReader<ChildStdout>,
    io_timeout: Duration,
    sent_commands: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BestMoveError {
    Timeout,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownDisposition {
    Graceful,
    ForcedReap,
}

impl UciEngine {
    pub(crate) async fn spawn_until(
        config: UciProcessConfig,
        io_timeout: Duration,
        deadline: Instant,
    ) -> Result<Self, UciSpawnFailure> {
        if Instant::now() >= deadline {
            return Err(UciSpawnFailure::before_expired_deadline());
        }
        let mut command = Command::new(&config.program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .env_clear();
        let mut child = command
            .spawn()
            .map_err(|_| UciSpawnFailure::before_child())?;
        let Some(stdin) = child.stdin.take() else {
            let cleanup_observed = kill_and_reap_child(&mut child, io_timeout).await.is_ok();
            return Err(UciSpawnFailure::after_child_cleanup(cleanup_observed));
        };
        let Some(stdout) = child.stdout.take() else {
            let cleanup_observed = kill_and_reap_child(&mut child, io_timeout).await.is_ok();
            return Err(UciSpawnFailure::after_child_cleanup(cleanup_observed));
        };
        let mut engine = Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: AsyncBufReader::new(stdout),
            io_timeout,
            sent_commands: Vec::new(),
        };
        let initialize = async {
            engine.send("uci").await?;
            engine.read_until_exact("uciok").await?;
            engine.send("isready").await?;
            engine.read_until_exact("readyok").await
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let cleanup_reserve = std::cmp::min(io_timeout, remaining / 2);
        let initialization_deadline = deadline
            .checked_sub(cleanup_reserve)
            .unwrap_or_else(Instant::now);
        let (initialized, deadline_exhausted) =
            match timeout_at(initialization_deadline, initialize).await {
                Ok(result) => (result, false),
                Err(_) => (
                    Err(anyhow::anyhow!("UCI initialization deadline elapsed")),
                    true,
                ),
            };
        if initialized.is_err() {
            let cleanup_timeout = std::cmp::min(
                io_timeout,
                deadline.saturating_duration_since(Instant::now()),
            );
            let cleanup_observed = engine
                .force_kill_and_reap_with_timeout(cleanup_timeout)
                .await
                .is_ok();
            return Err(if deadline_exhausted {
                UciSpawnFailure::after_deadline_cleanup(cleanup_observed)
            } else {
                UciSpawnFailure::after_child_cleanup(cleanup_observed)
            });
        }
        Ok(engine)
    }

    pub(crate) async fn best_move(
        &mut self,
        accepted_moves: &[ChessMove],
        nodes: u64,
    ) -> Result<ChessMove, BestMoveError> {
        let history = accepted_moves
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        let deadline = self.io_timeout;
        match timeout(deadline, async {
            self.send_unbounded(&format!("position startpos moves {history}"))
                .await?;
            self.send_unbounded(&format!("go nodes {nodes}")).await?;
            self.read_bestmove().await
        })
        .await
        {
            Ok(Ok(candidate)) => Ok(candidate),
            Ok(Err(_)) => Err(BestMoveError::Failure),
            Err(_) => Err(BestMoveError::Timeout),
        }
    }

    pub(crate) fn reads_are_bounded(&self) -> bool {
        self.io_timeout > Duration::ZERO && MAX_LINE_BYTES > 0 && MAX_RESPONSE_LINES > 0
    }

    pub(crate) fn sent_commands(&self) -> &[String] {
        &self.sent_commands
    }

    pub(crate) async fn shutdown_until(
        &mut self,
        deadline: Instant,
    ) -> anyhow::Result<ShutdownDisposition> {
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        let kill_reserve = std::cmp::min(self.io_timeout, remaining / 2);
        let global_graceful_deadline = deadline.checked_sub(kill_reserve).unwrap_or(now);
        let io_deadline = now.checked_add(self.io_timeout).unwrap_or(deadline);
        let graceful_deadline = std::cmp::min(global_graceful_deadline, io_deadline);
        let quit_sent = matches!(
            timeout_at(graceful_deadline, self.send_unbounded("quit")).await,
            Ok(Ok(()))
        );
        self.stdin.take();
        let Some(mut child) = self.child.take() else {
            anyhow::bail!("UCI child was unavailable during shutdown");
        };
        match timeout_at(graceful_deadline, child.wait()).await {
            Ok(Ok(status)) if quit_sent && status.success() => Ok(ShutdownDisposition::Graceful),
            Ok(Ok(_)) => anyhow::bail!("UCI child did not exit cleanly after quit"),
            Ok(Err(_)) | Err(_) => {
                let cleanup_timeout = std::cmp::min(
                    self.io_timeout,
                    deadline.saturating_duration_since(Instant::now()),
                );
                kill_and_reap_child(&mut child, cleanup_timeout).await?;
                Ok(ShutdownDisposition::ForcedReap)
            }
        }
    }

    async fn force_kill_and_reap_with_timeout(
        &mut self,
        cleanup_timeout: Duration,
    ) -> anyhow::Result<()> {
        self.stdin.take();
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        kill_and_reap_child(&mut child, cleanup_timeout).await
    }

    async fn send(&mut self, command: &str) -> anyhow::Result<()> {
        timeout(self.io_timeout, self.send_unbounded(command))
            .await
            .map_err(|_| anyhow::anyhow!("UCI write timed out"))?
    }

    async fn send_unbounded(&mut self, command: &str) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("UCI child stdin is closed"))?;
        stdin.write_all(command.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        self.sent_commands = self
            .sent_commands
            .iter()
            .cloned()
            .chain(std::iter::once(command.to_owned()))
            .collect();
        Ok(())
    }

    async fn read_until_exact(&mut self, expected: &str) -> anyhow::Result<()> {
        let deadline = self.io_timeout;
        timeout(deadline, async {
            for _ in 0..MAX_RESPONSE_LINES {
                if self.read_bounded_line().await?.trim() == expected {
                    return Ok(());
                }
            }
            anyhow::bail!("UCI response exceeded its line bound")
        })
        .await
        .map_err(|_| anyhow::anyhow!("UCI response timed out"))?
    }

    async fn read_bestmove(&mut self) -> anyhow::Result<ChessMove> {
        for _ in 0..MAX_RESPONSE_LINES {
            let line = self.read_bounded_line().await?;
            let mut words = line.split_whitespace();
            if words.next() == Some("bestmove") {
                let primary = words
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("UCI bestmove omitted its move"))?;
                let primary = parse_strict_uci_move(primary)
                    .map_err(|_| anyhow::anyhow!("UCI bestmove was not typed UCI"))?;
                match words.next() {
                    None => return Ok(primary),
                    Some("ponder") => {
                        let ponder = words
                            .next()
                            .ok_or_else(|| anyhow::anyhow!("UCI ponder omitted its move"))?;
                        parse_strict_uci_move(ponder)
                            .map_err(|_| anyhow::anyhow!("UCI ponder was not typed UCI"))?;
                        if words.next().is_none() {
                            return Ok(primary);
                        }
                    }
                    Some(_) => {}
                }
                anyhow::bail!("UCI bestmove tail was rejected");
            }
        }
        anyhow::bail!("UCI bestmove exceeded its line bound")
    }

    async fn read_bounded_line(&mut self) -> anyhow::Result<String> {
        let mut bytes = Vec::with_capacity(128);
        for _ in 0..=MAX_LINE_BYTES {
            let byte = self.stdout.read_u8().await?;
            if byte == b'\n' {
                return String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("UCI output was not UTF-8"));
            }
            if bytes.len() == MAX_LINE_BYTES {
                anyhow::bail!("UCI output line exceeded its byte bound");
            }
            bytes.push(byte);
        }
        anyhow::bail!("UCI output line exceeded its byte bound")
    }
}

async fn kill_and_reap_child(child: &mut Child, io_timeout: Duration) -> anyhow::Result<()> {
    let _ = child.start_kill();
    match timeout(io_timeout, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(_) => {
            anyhow::bail!("UCI child could not be killed and reaped within its bound")
        }
    }
}

impl Drop for UciEngine {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}
