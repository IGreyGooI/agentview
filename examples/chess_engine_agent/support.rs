//! Example-only chess AgentView support.
//!
//! This is deliberately not part of the `agentview` library API. It is a demo
//! VM that exercises observe/act/hook against a Stockfish-compatible UCI engine.

use std::process::Stdio;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentview::prelude::*;
use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece, Square, ALL_FILES, ALL_RANKS};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Shared chess game source used by `AgentViewSession`.
#[derive(Debug, Clone)]
pub struct ChessGameSource {
    inner: Arc<Mutex<ChessGameState>>,
}

impl ChessGameSource {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ChessGameState::new())),
        }
    }

    pub fn snapshot(&self) -> ChessGameState {
        self.inner.lock().unwrap().clone()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ChessGameState) -> R) -> R {
        let mut state = self.inner.lock().unwrap();
        f(&mut state)
    }
}

impl Default for ChessGameSource {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ChessGameState {
    board: Board,
    move_history: Vec<String>,
    engine_pending: bool,
    last_engine_move: Option<String>,
    last_error: Option<String>,
}

impl ChessGameState {
    pub fn new() -> Self {
        Self {
            board: Board::default(),
            move_history: Vec::new(),
            engine_pending: false,
            last_engine_move: None,
            last_error: None,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn move_history(&self) -> &[String] {
        &self.move_history
    }

    fn legal_uci_moves(&self) -> Vec<String> {
        legal_uci_moves(&self.board)
    }
}

impl Default for ChessGameState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChessSide {
    White,
    Black,
}

impl ChessSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::White => "white",
            Self::Black => "black",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChessPieceKind {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessPieceView {
    pub side: ChessSide,
    pub kind: ChessPieceKind,
    pub symbol: char,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessSquareView {
    pub square: String,
    pub file: char,
    pub rank: u8,
    pub piece: Option<ChessPieceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessRankView {
    pub rank: u8,
    pub squares: Vec<ChessSquareView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChessBoardView {
    pub ranks: Vec<ChessRankView>,
}

impl ChessBoardView {
    pub fn ascii_diagram(&self) -> String {
        let mut lines = Vec::with_capacity(self.ranks.len() + 1);
        for rank in &self.ranks {
            let mut line = rank.rank.to_string();
            for square in &rank.squares {
                line.push(' ');
                line.push(
                    square
                        .piece
                        .as_ref()
                        .map(|piece| piece.symbol)
                        .unwrap_or('.'),
                );
            }
            lines.push(line);
        }
        lines.push("  a b c d e f g h".to_owned());
        lines.join("\n")
    }
}

/// Full chess VM snapshot for an outside chat/CLI/daemon/skill caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChessView {
    pub board: ChessBoardView,
    pub fen: String,
    pub side_to_move: ChessSide,
    pub legal_uci_moves: Vec<String>,
    pub move_history: Vec<String>,
    pub engine_pending: bool,
    pub last_engine_move: Option<String>,
    pub last_error: Option<String>,
    pub status: String,
}

#[derive(Serialize)]
struct ChessViewTemplateVars<'a> {
    board: &'a ChessBoardView,
    board_ascii_lines: Vec<String>,
    fen: &'a str,
    side_to_move: &'static str,
    legal_uci_moves: &'a [String],
    move_history: &'a [String],
    engine_pending: bool,
    last_engine_move: Option<&'a str>,
    last_error: Option<&'a str>,
    status: &'a str,
}

#[derive(Serialize)]
struct ChessViewUpdateTemplateVars<'a> {
    changed_squares: Vec<&'a ChessSquareView>,
    board_state_changed: bool,
    board_ascii_lines: Vec<String>,
    fen: &'a str,
    side_to_move: &'static str,
    status: &'a str,
    legal_moves_added: Vec<String>,
    legal_moves_removed: Vec<String>,
    move_history_added: Vec<String>,
    move_history_removed: Vec<String>,
    engine_changed: bool,
    engine_pending: bool,
    last_engine_move: Option<&'a str>,
    last_error: Option<&'a str>,
}

const CHESS_VIEW_TEMPLATE: &str = r#"<prompt_board render_mode="full">
  <board_state>
    <board_ascii>
{% for line in board_ascii_lines %}      {{ line }}
{% endfor %}    </board_ascii>
    <fen>{{ fen }}</fen>
    <side_to_move>{{ side_to_move }}</side_to_move>
    <status>{{ status }}</status>
  </board_state>
  <board_squares>
{% for rank in board.ranks %}    <rank n="{{ rank.rank }}">
{% for square in rank.squares %}      <square id="{{ square.square }}" file="{{ square.file }}" rank="{{ square.rank }}">{% if square.piece %}{{ square.piece.symbol }}{% else %}.{% endif %}</square>
{% endfor %}    </rank>
{% endfor %}  </board_squares>
  <legal_moves>
{% for uci in legal_uci_moves %}    <move>{{ uci }}</move>
{% endfor %}  </legal_moves>
  <move_history>
{% for uci in move_history %}    <move>{{ uci }}</move>
{% endfor %}  </move_history>
  <engine>
    <pending>{{ engine_pending }}</pending>
{% if last_engine_move %}    <last_move>{{ last_engine_move }}</last_move>
{% endif %}{% if last_error %}    <last_error>{{ last_error }}</last_error>
{% endif %}  </engine>
</prompt_board>"#;

const CHESS_VIEW_UPDATE_TEMPLATE: &str = r#"<prompt_board render_mode="update">
{% if board_state_changed %}  <board_state>
    <replace>
      <board_ascii>
{% for line in board_ascii_lines %}        {{ line }}
{% endfor %}      </board_ascii>
      <fen>{{ fen }}</fen>
      <side_to_move>{{ side_to_move }}</side_to_move>
      <status>{{ status }}</status>
    </replace>
  </board_state>
{% endif %}{% if changed_squares %}  <board_squares>
    <replace>
{% for square in changed_squares %}      <square id="{{ square.square }}" file="{{ square.file }}" rank="{{ square.rank }}">{% if square.piece %}{{ square.piece.symbol }}{% else %}.{% endif %}</square>
{% endfor %}    </replace>
  </board_squares>
{% endif %}{% if legal_moves_added or legal_moves_removed %}  <legal_moves>
{% if legal_moves_added %}    <added>
{% for uci in legal_moves_added %}      <move>{{ uci }}</move>
{% endfor %}    </added>
{% endif %}{% if legal_moves_removed %}    <removed>
{% for uci in legal_moves_removed %}      <move>{{ uci }}</move>
{% endfor %}    </removed>
{% endif %}  </legal_moves>
{% endif %}{% if move_history_added or move_history_removed %}  <move_history>
{% if move_history_added %}    <added>
{% for uci in move_history_added %}      <move>{{ uci }}</move>
{% endfor %}    </added>
{% endif %}{% if move_history_removed %}    <removed>
{% for uci in move_history_removed %}      <move>{{ uci }}</move>
{% endfor %}    </removed>
{% endif %}  </move_history>
{% endif %}{% if engine_changed %}  <engine>
    <replace>
      <pending>{{ engine_pending }}</pending>
{% if last_engine_move %}      <last_move>{{ last_engine_move }}</last_move>
{% endif %}{% if last_error %}      <last_error>{{ last_error }}</last_error>
{% endif %}    </replace>
  </engine>
{% endif %}
</prompt_board>"#;

impl ChessView {
    pub async fn render_update_since<'a>(
        &'a self,
        prev: &'a Self,
        templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        let (move_history_added, move_history_removed) =
            ordered_list_delta(&prev.move_history, &self.move_history);

        let vars = ChessViewUpdateTemplateVars {
            changed_squares: changed_squares(&prev.board, &self.board),
            board_state_changed: self.board != prev.board
                || self.fen != prev.fen
                || self.side_to_move != prev.side_to_move
                || self.status != prev.status,
            board_ascii_lines: self
                .board
                .ascii_diagram()
                .lines()
                .map(str::to_owned)
                .collect(),
            fen: &self.fen,
            side_to_move: self.side_to_move.as_str(),
            status: &self.status,
            legal_moves_added: list_added(&prev.legal_uci_moves, &self.legal_uci_moves),
            legal_moves_removed: list_removed(&prev.legal_uci_moves, &self.legal_uci_moves),
            move_history_added,
            move_history_removed,
            engine_changed: self.engine_pending != prev.engine_pending
                || self.last_engine_move != prev.last_engine_move
                || self.last_error != prev.last_error,
            engine_pending: self.engine_pending,
            last_engine_move: self.last_engine_move.as_deref(),
            last_error: self.last_error.as_deref(),
        };

        Ok(templates
            .render_template(
                "chess_view_update",
                CHESS_VIEW_UPDATE_TEMPLATE,
                minijinja::Value::from_serialize(&vars),
            )?
            .into())
    }
}

fn list_added(prev: &[String], next: &[String]) -> Vec<String> {
    next.iter()
        .filter(|item| !prev.contains(item))
        .cloned()
        .collect()
}

fn list_removed(prev: &[String], next: &[String]) -> Vec<String> {
    prev.iter()
        .filter(|item| !next.contains(item))
        .cloned()
        .collect()
}

fn ordered_list_delta(prev: &[String], next: &[String]) -> (Vec<String>, Vec<String>) {
    if next.starts_with(prev) {
        (next[prev.len()..].to_vec(), Vec::new())
    } else if prev.starts_with(next) {
        (Vec::new(), prev[next.len()..].to_vec())
    } else {
        (next.to_vec(), prev.to_vec())
    }
}

fn changed_squares<'a>(
    prev: &'a ChessBoardView,
    next: &'a ChessBoardView,
) -> Vec<&'a ChessSquareView> {
    prev.ranks
        .iter()
        .flat_map(|rank| rank.squares.iter())
        .zip(next.ranks.iter().flat_map(|rank| rank.squares.iter()))
        .filter_map(|(prev_square, next_square)| {
            (prev_square != next_square).then_some(next_square)
        })
        .collect()
}

#[async_trait::async_trait]
impl PromptRenderable for ChessView {
    async fn render_full<'a>(
        &'a self,
        templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        let vars = ChessViewTemplateVars {
            board: &self.board,
            board_ascii_lines: self
                .board
                .ascii_diagram()
                .lines()
                .map(str::to_owned)
                .collect(),
            fen: &self.fen,
            side_to_move: self.side_to_move.as_str(),
            legal_uci_moves: &self.legal_uci_moves,
            move_history: &self.move_history,
            engine_pending: self.engine_pending,
            last_engine_move: self.last_engine_move.as_deref(),
            last_error: self.last_error.as_deref(),
            status: &self.status,
        };

        Ok(templates
            .render_template(
                "chess_view_full",
                CHESS_VIEW_TEMPLATE,
                minijinja::Value::from_serialize(&vars),
            )?
            .into())
    }
}

#[async_trait::async_trait]
impl ContextView for ChessView {
    async fn render_delta<'a>(
        &'a self,
        prev: &'a Self,
        templates: &'a TemplateEngine,
    ) -> anyhow::Result<Option<PromptFragment>> {
        if self == prev {
            Ok(None)
        } else {
            self.render_update_since(prev, templates).await.map(Some)
        }
    }
}

/// Prompt/contract for the next chess move.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChessTurnPrompt {
    pub task: String,
    pub reply_schema: serde_json::Value,
}

#[derive(Serialize)]
struct ChessTurnPromptTemplateVars<'a> {
    task: &'a str,
}

const CHESS_TURN_PROMPT_TEMPLATE: &str = r#"<chess_task>
  <task>{{ task }}</task>
  <reasoning_policy>
    <instruction>Think privately about candidate moves before acting.</instruction>
    <instruction>Do not print chain-of-thought; call the CLI only after deciding.</instruction>
  </reasoning_policy>
  <reply_contract transport="cli">
    <command>agentview chess act --piece &lt;piece&gt; --from &lt;from&gt; --to &lt;to&gt; [--promotion &lt;promotion&gt;] --uci &lt;uci&gt;</command>
    <example>agentview chess act --piece P --from e2 --to e4 --uci e2e4</example>
    <promotion_example>agentview chess act --piece P --from e7 --to e8 --promotion q --uci e7e8q</promotion_example>
    <instruction>Choose one legal UCI move from the current view, include the move context flags first, then pass the canonical UCI move with --uci.</instruction>
  </reply_contract>
</chess_task>"#;

#[async_trait::async_trait]
impl PromptRenderable for ChessTurnPrompt {
    async fn render_full<'a>(
        &'a self,
        templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        let vars = ChessTurnPromptTemplateVars { task: &self.task };

        Ok(templates
            .render_template(
                "chess_turn_prompt",
                CHESS_TURN_PROMPT_TEMPLATE,
                minijinja::Value::from_serialize(&vars),
            )?
            .into())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChessViewModel;

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for ChessViewModel {
    type Source = ChessGameSource;
    type View = ChessView;
    type SystemPrompt = ChessTurnPrompt;
    type TurnPrompt = ChessTurnPrompt;
    type ContextState = ();

    async fn build_system_prompt(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Self::SystemPrompt> {
        Ok(ChessTurnPrompt {
            task: "You are choosing legal chess moves from the rendered board.".to_owned(),
            reply_schema: move_reply_schema(),
        })
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        let state = source.snapshot();
        ChessView {
            board: board_view(&state.board),
            fen: state.board.to_string(),
            side_to_move: side_view(state.board.side_to_move()),
            legal_uci_moves: state.legal_uci_moves(),
            move_history: state.move_history,
            engine_pending: state.engine_pending,
            last_engine_move: state.last_engine_move,
            last_error: state.last_error,
            status: board_status_name(state.board.status()).to_owned(),
        }
    }

    async fn build_turn_prompt(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        call_id: &str,
        task: String,
    ) -> anyhow::Result<Self::TurnPrompt> {
        Ok(ChessTurnPrompt {
            task: format!("{task} Active turn id: {call_id}."),
            reply_schema: move_reply_schema(),
        })
    }

    async fn commit_turn(
        &self,
        _ctx: &mut PromptContext<Turn, Self::ContextState>,
        _request: &AgentTurnRequest<Turn>,
        _executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut (),
    ) -> anyhow::Result<TurnFlow> {
        Ok(TurnFlow::Wait)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerMove {
    pub uci: StorageString,
    chess_move: ChessMove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChessMoveOutput {
    Accepted(PlayerMove),
    Rejected { message: String },
}

/// Parses and validates a move supplied by an external caller.
pub struct ChessMoveSink {
    board: Board,
    output: Option<ChessMoveOutput>,
}

impl ChessMoveSink {
    pub fn from_source(source: &ChessGameSource) -> Self {
        Self {
            board: source.snapshot().board,
            output: None,
        }
    }

    fn parse_reply(reply: ControlReply) -> Result<String, String> {
        match reply {
            ControlReply::Text(text) => Ok(text.trim().to_owned()),
            ControlReply::Structured(value) => value
                .get("uci")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|uci| !uci.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| "expected structured reply with non-empty `uci`".to_owned()),
        }
    }

    fn reject(&mut self, message: impl Into<String>) {
        self.output = Some(ChessMoveOutput::Rejected {
            message: message.into(),
        });
    }
}

#[async_trait::async_trait]
impl TurnSink<ControlReply> for ChessMoveSink {
    type Output = ChessMoveOutput;

    async fn on_event(&mut self, reply: ControlReply) {
        let uci = match Self::parse_reply(reply) {
            Ok(uci) if !uci.is_empty() => uci,
            Ok(_) => {
                self.reject("expected non-empty chess move");
                return;
            }
            Err(message) => {
                self.reject(message);
                return;
            }
        };

        let chess_move = match ChessMove::from_str(&uci) {
            Ok(chess_move) => chess_move,
            Err(err) => {
                self.reject(format!("invalid UCI chess move `{uci}`: {err}"));
                return;
            }
        };

        if !self.board.legal(chess_move) {
            self.reject(format!("illegal chess move `{uci}` for current board"));
            return;
        }

        self.output = Some(ChessMoveOutput::Accepted(PlayerMove {
            uci: uci.into(),
            chess_move,
        }));
    }

    async fn finish(self: Box<Self>) -> Self::Output {
        self.output.unwrap_or_else(|| ChessMoveOutput::Rejected {
            message: "no chess move reply was provided".to_owned(),
        })
    }
}

pub fn apply_player_move(
    ctx: &mut PromptContext<Turn, ()>,
    source: &ChessGameSource,
    output: ChessMoveOutput,
) -> anyhow::Result<()> {
    match output {
        ChessMoveOutput::Accepted(player_move) => {
            source.with_state(|state| {
                state.board = state.board.make_move_new(player_move.chess_move);
                state.move_history.push(player_move.uci.to_string());
                state.engine_pending = state.board.status() == BoardStatus::Ongoing;
                state.last_error = None;
            });
            ctx.push_history(Turn::user(format!("player_move = {}", player_move.uci)));
        }
        ChessMoveOutput::Rejected { message } => {
            source.with_state(|state| {
                state.engine_pending = false;
                state.last_error = Some(message.clone());
            });
            ctx.push_history(Turn::user(format!("rejected_player_move = {message}")));
        }
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StockfishEngine {
    command: String,
    movetime: Duration,
}

#[allow(dead_code)]
impl StockfishEngine {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            movetime: Duration::from_millis(100),
        }
    }

    pub async fn best_move(&self, state: &ChessGameState) -> anyhow::Result<String> {
        let mut child = Command::new(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                anyhow::anyhow!("failed to start stockfish `{}`: {err}", self.command)
            })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("stockfish stdin was unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stockfish stdout was unavailable"))?;
        let mut lines = BufReader::new(stdout).lines();

        write_uci_line(&mut stdin, "uci").await?;
        wait_for_uci_line(&mut lines, "uciok").await?;
        write_uci_line(&mut stdin, "isready").await?;
        wait_for_uci_line(&mut lines, "readyok").await?;
        write_uci_line(&mut stdin, &format!("position fen {}", state.board)).await?;
        write_uci_line(
            &mut stdin,
            &format!("go movetime {}", self.movetime.as_millis()),
        )
        .await?;

        let best_move = wait_for_bestmove(&mut lines).await?;
        let _ = write_uci_line(&mut stdin, "quit").await;
        let _ = timeout(Duration::from_millis(100), child.wait()).await;

        Ok(best_move)
    }
}

#[allow(dead_code)]
async fn write_uci_line(stdin: &mut tokio::process::ChildStdin, line: &str) -> anyhow::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

#[allow(dead_code)]
async fn wait_for_uci_line(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected: &str,
) -> anyhow::Result<()> {
    timeout(Duration::from_secs(2), async {
        while let Some(line) = lines.next_line().await? {
            if line.trim() == expected {
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("stockfish exited before `{expected}`")
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for stockfish `{expected}`"))?
}

#[allow(dead_code)]
async fn wait_for_bestmove(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> anyhow::Result<String> {
    timeout(Duration::from_secs(5), async {
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("bestmove ") {
                let best_move = rest
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("stockfish returned empty bestmove"))?;
                if best_move == "(none)" {
                    anyhow::bail!("stockfish returned no legal move");
                }
                return Ok(best_move.to_owned());
            }
        }
        anyhow::bail!("stockfish exited before bestmove")
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for stockfish bestmove"))?
}

#[allow(dead_code)]
pub async fn apply_engine_move(
    source: &ChessGameSource,
    awake: &ViewAwakeHandle,
    engine: &StockfishEngine,
) -> anyhow::Result<String> {
    let engine_move = match engine.best_move(&source.snapshot()).await {
        Ok(engine_move) => engine_move,
        Err(err) => {
            let message = format!("stockfish failed: {err}");
            source.with_state(|state| {
                state.last_error = Some(message);
                state.engine_pending = false;
            });
            awake.awake();
            return Err(err);
        }
    };

    commit_engine_move(source, awake, "stockfish", engine_move)
}

fn commit_engine_move(
    source: &ChessGameSource,
    awake: &ViewAwakeHandle,
    engine_name: &str,
    engine_move: String,
) -> anyhow::Result<String> {
    let chess_move = match ChessMove::from_str(&engine_move) {
        Ok(chess_move) => chess_move,
        Err(err) => {
            let message = format!("{engine_name} produced invalid move `{engine_move}`: {err}");
            source.with_state(|state| {
                state.last_error = Some(message.clone());
                state.engine_pending = false;
            });
            awake.awake();
            anyhow::bail!(message);
        }
    };

    let legal = source.with_state(|state| {
        if !state.board.legal(chess_move) {
            state.last_error = Some(format!(
                "{engine_name} produced illegal move `{engine_move}`"
            ));
            state.engine_pending = false;
            return false;
        }

        state.board = state.board.make_move_new(chess_move);
        state.move_history.push(engine_move.clone());
        state.last_engine_move = Some(engine_move.clone());
        state.engine_pending = false;
        state.last_error = None;
        true
    });
    awake.awake();
    if !legal {
        anyhow::bail!("{engine_name} produced illegal move `{engine_move}`");
    }
    Ok(engine_move)
}

fn legal_uci_moves(board: &Board) -> Vec<String> {
    let mut moves = MoveGen::new_legal(board)
        .map(|chess_move| chess_move.to_string())
        .collect::<Vec<_>>();
    moves.sort();
    moves
}

fn move_reply_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["uci"],
        "properties": {
            "uci": {
                "type": "string",
                "description": "A legal move in UCI long algebraic notation, such as e2e4 or e7e8q."
            }
        }
    })
}

fn board_status_name(status: BoardStatus) -> &'static str {
    match status {
        BoardStatus::Ongoing => "ongoing",
        BoardStatus::Stalemate => "stalemate",
        BoardStatus::Checkmate => "checkmate",
    }
}

fn side_view(color: Color) -> ChessSide {
    match color {
        Color::White => ChessSide::White,
        Color::Black => ChessSide::Black,
    }
}

fn kind_view(piece: Piece) -> ChessPieceKind {
    match piece {
        Piece::Pawn => ChessPieceKind::Pawn,
        Piece::Knight => ChessPieceKind::Knight,
        Piece::Bishop => ChessPieceKind::Bishop,
        Piece::Rook => ChessPieceKind::Rook,
        Piece::Queen => ChessPieceKind::Queen,
        Piece::King => ChessPieceKind::King,
    }
}

fn board_view(board: &Board) -> ChessBoardView {
    let ranks = ALL_RANKS
        .iter()
        .rev()
        .map(|rank| {
            let rank_number = (rank.to_index() + 1) as u8;
            let squares = ALL_FILES
                .iter()
                .map(|file| {
                    let square = Square::make_square(*rank, *file);
                    let piece = board.piece_on(square).map(|piece| {
                        let side = side_view(board.color_on(square).unwrap());
                        ChessPieceView {
                            side,
                            kind: kind_view(piece),
                            symbol: piece_char(piece, side),
                        }
                    });
                    ChessSquareView {
                        square: square.to_string(),
                        file: file_char(file.to_index()),
                        rank: rank_number,
                        piece,
                    }
                })
                .collect();
            ChessRankView {
                rank: rank_number,
                squares,
            }
        })
        .collect();
    ChessBoardView { ranks }
}

fn file_char(index: usize) -> char {
    (b'a' + index as u8) as char
}

fn piece_char(piece: Piece, side: ChessSide) -> char {
    let ch = match piece {
        Piece::Pawn => 'p',
        Piece::Knight => 'n',
        Piece::Bishop => 'b',
        Piece::Rook => 'r',
        Piece::Queen => 'q',
        Piece::King => 'k',
    };

    match side {
        ChessSide::White => ch.to_ascii_uppercase(),
        ChessSide::Black => ch,
    }
}
