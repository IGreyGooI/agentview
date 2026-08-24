use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use agentview::{
    component::{
        execution::{ComponentReactionProps, ProviderEvent},
        prelude::*,
    },
    llm_call::TextTurnEvent,
};
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
    pending_draw_offer: Option<Color>,
    draw_state: DrawState,
    match_phase: MatchPhase,
    feedback: ChessFeedback,
}

impl ChessSnapshot {
    pub(crate) fn in_progress(
        agent_side: Color,
        board: Board,
        committed_moves: Vec<ChessMove>,
        pending_draw_offer: Option<Color>,
        feedback: ChessFeedback,
    ) -> Self {
        let draw_state = DrawState::from_history(&committed_moves);
        Self {
            agent_side,
            board,
            committed_moves,
            pending_draw_offer,
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

    pub(crate) fn pending_draw_offer(&self) -> Option<Color> {
        self.pending_draw_offer
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
    fn awaiting(snapshot: Arc<ChessSnapshot>, context: ModelAttemptContext) -> Self {
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
        self.diagnostic
    }

    pub(crate) fn text_complete(&self) -> bool {
        self.text_complete
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished
    }
}

#[derive(Clone)]
pub(crate) struct ChessAgentProps {
    snapshot: Arc<ChessSnapshot>,
    context: ModelAttemptContext,
    component_render_count: Arc<AtomicUsize>,
}

impl ChessAgentProps {
    pub(crate) fn new(
        snapshot: ChessSnapshot,
        context: ModelAttemptContext,
        component_render_count: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            context,
            component_render_count,
        }
    }

    pub(crate) fn for_attempt(
        &self,
        snapshot: ChessSnapshot,
        context: ModelAttemptContext,
    ) -> Self {
        Self::new(snapshot, context, Arc::clone(&self.component_render_count))
    }

    pub(crate) fn matches_attempt(
        &self,
        snapshot: &ChessSnapshot,
        context: &ModelAttemptContext,
    ) -> bool {
        self.snapshot.as_ref() == snapshot && &self.context == context
    }
}

#[component]
pub(crate) fn chess_agent(
    props: ComponentReactionProps<ChessAgentProps, ChessAgentState>,
    events: EventInput<ProviderEvent>,
) -> Component {
    props
        .value()
        .component_render_count
        .fetch_add(1, Ordering::SeqCst);
    let snapshot = Arc::clone(&props.value().snapshot);
    let context = props.value().context.clone();
    let attempt_state = ChessAgentState::awaiting(Arc::clone(&snapshot), context);
    let initial_state = attempt_state.clone();
    let state = use_signal(move || initial_state);
    props
        .publish(state.clone())
        .expect("authoritative chess reaction publication");
    let text: EventInput<TextTurnEvent> = events.select(ProviderEvent::TEXT);

    view! {
        chess_player(Arc::clone(&snapshot))
        chess_game_state(Arc::clone(&snapshot))
        chess_actions(Arc::clone(&snapshot), attempt_state, state, text)
        chess_feedback(snapshot)
    }
}
