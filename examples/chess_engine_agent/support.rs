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

/// Shared chess game source used by `AgentViewApp`.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "square")]
struct ChessSquarePromptView {
    id: String,
    file: char,
    rank: u8,

    #[view(text)]
    piece: char,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "board_state")]
struct ChessBoardStatePromptView {
    #[view(element)]
    board_ascii: String,

    #[view(element)]
    fen: String,

    #[view(element)]
    side_to_move: String,

    #[view(element)]
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "move")]
struct ChessMovePromptView {
    #[view(text)]
    uci: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "engine")]
struct ChessEnginePromptView {
    #[view(element)]
    pending: bool,

    #[view(flatten)]
    last_move: Option<ChessLastMovePromptView>,

    #[view(flatten)]
    last_error: Option<ChessLastErrorPromptView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "last_move")]
struct ChessLastMovePromptView {
    #[view(text)]
    uci: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "last_error")]
struct ChessLastErrorPromptView {
    #[view(text)]
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "instruction")]
struct ChessInstructionPromptView {
    #[view(text)]
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "reasoning_policy")]
struct ChessReasoningPolicyPromptView {
    #[view(flatten)]
    instructions: Vec<ChessInstructionPromptView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "reply_contract")]
struct ChessReplyContractPromptView {
    transport: String,

    #[view(element)]
    command: String,

    #[view(element)]
    example: String,

    #[view(element)]
    promotion_example: String,

    #[view(element)]
    instruction: String,
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

impl AgentViewCollect<ChessSquareView> for ChessSquarePromptView {
    fn collect(square: &ChessSquareView) -> Self {
        Self {
            id: square.square.clone(),
            file: square.file,
            rank: square.rank,
            piece: square
                .piece
                .as_ref()
                .map(|piece| piece.symbol)
                .unwrap_or('.'),
        }
    }
}

struct ChessBoardStateSource<'a> {
    board: &'a ChessBoardView,
    fen: &'a str,
    side_to_move: ChessSide,
    status: &'a str,
}

impl AgentViewCollect<ChessBoardStateSource<'_>> for ChessBoardStatePromptView {
    fn collect(source: &ChessBoardStateSource<'_>) -> Self {
        Self {
            board_ascii: source.board.ascii_diagram(),
            fen: source.fen.to_owned(),
            side_to_move: source.side_to_move.as_str().to_owned(),
            status: source.status.to_owned(),
        }
    }
}

#[cfg(test)]
fn render_chess_board_state_xml(view: &ChessView) -> String {
    render_agent_view_xml(&view.board_state)
}

#[cfg(test)]
#[allow(dead_code)]
pub fn render_board_state_xml_for_test(view: &ChessView) -> String {
    render_chess_board_state_xml(view)
}

fn chess_move_prompt_view(uci: &str) -> ChessMovePromptView {
    ChessMovePromptView {
        uci: uci.to_owned(),
    }
}

fn chess_move_prompt_views(moves: &[String]) -> Vec<ChessMovePromptView> {
    moves
        .iter()
        .map(|uci| chess_move_prompt_view(uci))
        .collect()
}

fn chess_last_move_prompt_view(uci: &str) -> ChessLastMovePromptView {
    ChessLastMovePromptView {
        uci: uci.to_owned(),
    }
}

fn chess_last_error_prompt_view(message: &str) -> ChessLastErrorPromptView {
    ChessLastErrorPromptView {
        message: message.to_owned(),
    }
}

impl AgentViewCollect<ChessGameState> for ChessView {
    fn collect(state: &ChessGameState) -> Self {
        let board = board_view(&state.board);
        let fen = state.board.to_string();
        let side_to_move = side_view(state.board.side_to_move());
        let legal_uci_moves = state.legal_uci_moves();
        let status = board_status_name(state.board.status()).to_owned();

        Self {
            board_state: ChessBoardStatePromptView::collect(&ChessBoardStateSource {
                board: &board,
                fen: &fen,
                side_to_move,
                status: &status,
            }),
            board_squares: board
                .ranks
                .iter()
                .flat_map(|rank| rank.squares.iter())
                .map(ChessSquarePromptView::collect)
                .collect(),
            legal_moves: chess_move_prompt_views(&legal_uci_moves),
            move_history_view: chess_move_prompt_views(&state.move_history),
            engine: ChessEnginePromptView {
                pending: state.engine_pending,
                last_move: state
                    .last_engine_move
                    .as_deref()
                    .map(chess_last_move_prompt_view),
                last_error: state
                    .last_error
                    .as_deref()
                    .map(chess_last_error_prompt_view),
            },
        }
    }
}

fn chess_instruction_prompt_view(text: &str) -> ChessInstructionPromptView {
    ChessInstructionPromptView {
        text: text.to_owned(),
    }
}

fn chess_reasoning_policy_prompt_view() -> ChessReasoningPolicyPromptView {
    ChessReasoningPolicyPromptView {
        instructions: vec![
            chess_instruction_prompt_view("Think privately about candidate moves before acting."),
            chess_instruction_prompt_view(
                "Do not print chain-of-thought; call the CLI only after deciding.",
            ),
        ],
    }
}

fn chess_reply_contract_prompt_view() -> ChessReplyContractPromptView {
    ChessReplyContractPromptView {
        transport: "cli".to_owned(),
        command: "agentview chess act --piece <piece> --from <from> --to <to> [--promotion <promotion>] --uci <uci>".to_owned(),
        example: "agentview chess act --piece P --from e2 --to e4 --uci e2e4".to_owned(),
        promotion_example: "agentview chess act --piece P --from e7 --to e8 --promotion q --uci e7e8q".to_owned(),
        instruction:
            "Choose one legal UCI move from the current view, include the move context flags first, then pass the canonical UCI move with --uci."
                .to_owned(),
    }
}

/// Full chess VM snapshot for an outside chat/CLI/daemon/skill caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "prompt_board")]
pub struct ChessView {
    #[view(diff(replace))]
    board_state: ChessBoardStatePromptView,

    #[view(diff(key = "id"))]
    board_squares: Vec<ChessSquarePromptView>,

    #[view(diff(set))]
    legal_moves: Vec<ChessMovePromptView>,

    #[view(name = "move_history", diff(seq))]
    move_history_view: Vec<ChessMovePromptView>,

    #[view(diff(replace))]
    engine: ChessEnginePromptView,
}

impl ChessView {
    #[allow(dead_code)]
    pub fn side_to_move(&self) -> &str {
        &self.board_state.side_to_move
    }

    #[allow(dead_code)]
    pub fn legal_uci_moves(&self) -> Vec<&str> {
        self.legal_moves
            .iter()
            .map(|move_view| move_view.uci.as_str())
            .collect()
    }

    #[allow(dead_code)]
    pub fn move_history(&self) -> Vec<&str> {
        self.move_history_view
            .iter()
            .map(|move_view| move_view.uci.as_str())
            .collect()
    }

    #[allow(dead_code)]
    pub fn engine_pending(&self) -> bool {
        self.engine.pending
    }

    #[allow(dead_code)]
    pub fn last_engine_move(&self) -> Option<&str> {
        self.engine
            .last_move
            .as_ref()
            .map(|move_view| move_view.uci.as_str())
    }

    #[allow(dead_code)]
    pub fn last_error(&self) -> Option<&str> {
        self.engine
            .last_error
            .as_ref()
            .map(|error| error.message.as_str())
    }

    #[cfg(test)]
    fn square_for_test(
        &self,
        rank_index: usize,
        square_index: usize,
    ) -> Option<&ChessSquarePromptView> {
        let index = rank_index
            .checked_mul(ALL_FILES.len())?
            .checked_add(square_index)?;
        self.board_squares.get(index)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn rank_for_test(&self, rank_index: usize) -> Option<u8> {
        self.square_for_test(rank_index, 0)
            .map(|square| square.rank)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn square_id_for_test(&self, rank_index: usize, square_index: usize) -> Option<&str> {
        self.square_for_test(rank_index, square_index)
            .map(|square| square.id.as_str())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn piece_symbol_for_test(&self, rank_index: usize, square_index: usize) -> Option<char> {
        self.square_for_test(rank_index, square_index)
            .map(|square| square.piece)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn render_square_xml_for_test(
        &self,
        rank_index: usize,
        square_index: usize,
    ) -> Option<String> {
        let square = self.square_for_test(rank_index, square_index)?;
        Some(render_agent_view_xml(square))
    }
}

/// Agent-facing task/contract for the next chess move.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "chess_task")]
pub struct ChessTaskView {
    #[view(element)]
    pub task: String,

    #[view(skip)]
    pub reply_schema: serde_json::Value,

    #[view(flatten)]
    reasoning_policy: ChessReasoningPolicyPromptView,

    #[view(flatten)]
    reply_contract: ChessReplyContractPromptView,
}

impl ChessTaskView {
    pub fn new(task: impl Into<String>, reply_schema: serde_json::Value) -> Self {
        Self {
            task: task.into(),
            reply_schema,
            reasoning_policy: chess_reasoning_policy_prompt_view(),
            reply_contract: chess_reply_contract_prompt_view(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChessViewModel;

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for ChessViewModel {
    type Source = ChessGameSource;
    type View = ChessView;
    type SystemPrompt = ChessTaskView;
    type TurnPrompt = ChessTaskView;
    type ContextState = ();

    async fn build_system_prompt(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Self::SystemPrompt> {
        Ok(ChessTaskView::new(
            "You are choosing legal chess moves from the rendered board.",
            move_reply_schema(),
        ))
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        ChessView::collect(&source.snapshot())
    }

    async fn build_turn_prompt(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        call_id: &str,
        task: String,
    ) -> anyhow::Result<Self::TurnPrompt> {
        Ok(ChessTaskView::new(
            format!("{task} Active turn id: {call_id}."),
            move_reply_schema(),
        ))
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
    session: &mut AgentSession<Turn, (), ChessView>,
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
            session.push_history(Turn::user(format!("player_move = {}", player_move.uci)));
        }
        ChessMoveOutput::Rejected { message } => {
            source.with_state(|state| {
                state.engine_pending = false;
                state.last_error = Some(message.clone());
            });
            session.push_history(Turn::user(format!("rejected_player_move = {message}")));
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
