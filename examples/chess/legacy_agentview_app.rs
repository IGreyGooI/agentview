//! Compatibility bridge for the pre-mounted `AgentViewApp` chess example.
//!
//! New Chess components use the POM, reply contract, and host boundaries from
//! the parent module directly. This module exists only so the historical
//! `observe -> act_with_sink -> hook` example and its regression tests retain
//! their original behavior during migration.

use std::str::FromStr;

use agentview::prelude::*;
use chess::{Board, BoardStatus, ChessMove};

use super::{
    chess_user_document, ChessAction, ChessGameSource, ChessSystemPromptView, ChessView,
    StockfishEngine,
};

#[derive(Debug, Clone, Default)]
pub struct ChessViewModel;

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for ChessViewModel {
    type Source = ChessGameSource;
    type View = ChessView;
    type ContextState = ();

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        let prompt = ChessSystemPromptView::default();
        Ok(prompt.build_root()?)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        ChessView::collect(&source.snapshot())
    }

    async fn build_user_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(chess_user_document(current_view.clone(), task, call_id)?)
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

/// Parses and validates a move supplied through the legacy external-control
/// sink. Mounted external replies use `ChessReplyContract` instead.
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

    fn parse_reply(reply: ControlReply) -> Result<ChessAction, String> {
        let uci = match reply {
            ControlReply::Text(text) => Ok(text.trim().to_owned()),
            ControlReply::Structured(value) => value
                .get("uci")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|uci| !uci.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| "expected structured reply with non-empty `uci`".to_owned()),
        }?;
        ChessAction::from_uci(uci).map_err(|diagnostic| diagnostic.to_string())
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
        let action = match Self::parse_reply(reply) {
            Ok(action) => action,
            Err(message) => {
                self.reject(message);
                return;
            }
        };

        let chess_move =
            ChessMove::from_str(action.uci()).expect("a ChessAction always has UCI syntax");

        if !self.board.legal(chess_move) {
            self.reject(format!(
                "illegal chess move `{}` for current board",
                action.uci()
            ));
            return;
        }

        self.output = Some(ChessMoveOutput::Accepted(PlayerMove {
            uci: action.uci.into(),
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
    session: &mut AgentSession<Turn, ()>,
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
