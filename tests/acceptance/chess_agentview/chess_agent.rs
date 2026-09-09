use std::sync::{atomic::AtomicUsize, Arc, Mutex};

use agentview::component::prelude::*;
use chess::{Board, ChessMove, Color};

use super::{
    chess_actions::{chess_actions, ChessAction, InvalidActionReason},
    chess_feedback::{chess_feedback, ChessFeedback},
    chess_game_state::{chess_game_state, DrawState},
    chess_player::chess_player,
    model::ModelAttemptContext,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchPhase {
    InProgress,
    Finished,
}

impl MatchPhase {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChessSnapshot {
    agent_side: Color,
    board: Board,
    committed_moves: Vec<ChessMove>,
    draw_state: DrawState,
    match_phase: MatchPhase,
    feedback: ChessFeedback,
}

impl ChessSnapshot {
    pub(crate) fn in_progress(
        agent_side: Color,
        board: Board,
        committed_moves: Vec<ChessMove>,
        feedback: ChessFeedback,
    ) -> Self {
        let draw_state = DrawState::from_history(&committed_moves);
        Self {
            agent_side,
            board,
            committed_moves,
            draw_state,
            match_phase: MatchPhase::InProgress,
            feedback,
        }
    }

    pub(crate) fn for_retry(&self, reason: InvalidActionReason) -> Self {
        Self {
            feedback: ChessFeedback::rejected(reason),
            ..self.clone()
        }
    }

    pub(crate) fn agent_side(&self) -> Color {
        self.agent_side
    }

    pub(crate) fn board(&self) -> Board {
        self.board
    }

    pub(crate) fn committed_moves(&self) -> &[ChessMove] {
        &self.committed_moves
    }

    pub(crate) fn draw_state(&self) -> DrawState {
        self.draw_state
    }

    pub(crate) fn match_phase(&self) -> MatchPhase {
        self.match_phase
    }

    pub(crate) fn feedback(&self) -> &ChessFeedback {
        &self.feedback
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChessAgentState {
    snapshot: Arc<ChessSnapshot>,
    context: ModelAttemptContext,
    action: Option<ChessAction>,
    diagnostic: Option<InvalidActionReason>,
    text_complete: bool,
    finished: bool,
}

impl ChessAgentState {
    pub(crate) fn awaiting(snapshot: Arc<ChessSnapshot>, context: ModelAttemptContext) -> Self {
        Self {
            snapshot,
            context,
            action: None,
            diagnostic: None,
            text_complete: false,
            finished: false,
        }
    }

    pub(crate) fn completed(&self, parsed: Result<ChessAction, InvalidActionReason>) -> Self {
        let (action, diagnostic) = match parsed {
            Ok(action) => (Some(action), None),
            Err(reason) => (None, Some(reason)),
        };
        Self {
            action,
            diagnostic,
            text_complete: true,
            finished: true,
            ..self.clone()
        }
    }

    pub(crate) fn snapshot(&self) -> &Arc<ChessSnapshot> {
        &self.snapshot
    }

    pub(crate) fn context(&self) -> &ModelAttemptContext {
        &self.context
    }

    pub(crate) fn action(&self) -> Option<ChessAction> {
        self.action
    }

    pub(crate) fn diagnostic(&self) -> Option<InvalidActionReason> {
        self.diagnostic.clone()
    }

    pub(crate) fn text_complete(&self) -> bool {
        self.text_complete
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished
    }
}

#[derive(Clone)]
pub(crate) struct ChessControl {
    state: Signal<ChessAgentState>,
}

impl ChessControl {
    pub(crate) fn begin_attempt(
        &self,
        snapshot: ChessSnapshot,
        context: ModelAttemptContext,
    ) -> Result<(), SignalAccessError> {
        self.state
            .set(ChessAgentState::awaiting(Arc::new(snapshot), context))
    }

    pub(crate) fn read_state(&self) -> Result<ChessAgentState, SignalAccessError> {
        self.state.with(Clone::clone)
    }
}

#[component]
fn chess_attempt_context(context: ModelAttemptContext) -> Component {
    let turn_id = context.turn_id.as_str().to_owned();
    let attempt_index = context.attempt_index;
    let corrective_reason = context
        .corrective_reason
        .map(|reason| reason.code())
        .unwrap_or("none");

    view! {
        model_attempt_context {
            turn_id { "{turn_id}" }
            attempt_index { "{attempt_index}" }
            corrective_reason { "{corrective_reason}" }
        }
    }
}

#[component]
pub(crate) fn chess_agent(
    initial_state: ChessAgentState,
    exported_control: Arc<Mutex<Option<ChessControl>>>,
    component_turn_executions: Arc<AtomicUsize>,
) -> Component {
    let state = use_signal(move || initial_state);
    *exported_control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ChessControl {
        state: state.clone(),
    });
    let attempt_state = state
        .with(Clone::clone)
        .expect("mounted Chess attempt state");
    let snapshot = Arc::clone(attempt_state.snapshot());

    view! {
        chess_attempt_context(attempt_state.context().clone())
        chess_player(Arc::clone(&snapshot))
        chess_game_state(Arc::clone(&snapshot))
        chess_actions(
            Arc::clone(&snapshot),
            attempt_state,
            state,
            component_turn_executions,
        )
        chess_feedback(snapshot)
    }
}
