use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece};

use super::{
    chess_action::{ActionUnavailableReason, ChessAction, ChessActionKind, InvalidActionReason},
    chess_draw_state::DrawState,
};

pub(crate) const MAX_MODEL_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessPhase {
    Ready,
    AwaitingModel,
    AwaitingStockfish,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetryState {
    attempts: u8,
    last_rejection: Option<InvalidActionReason>,
}

impl RetryState {
    fn fresh() -> Self {
        Self {
            attempts: 0,
            last_rejection: None,
        }
    }

    pub(crate) fn attempts(self) -> u8 {
        self.attempts
    }

    pub(crate) fn last_rejection(self) -> Option<InvalidActionReason> {
        self.last_rejection
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelAttemptKey {
    pub(crate) ply: usize,
    pub(crate) attempt_index: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessFeedback {
    Initial,
    Accepted(ChessAction),
    Rejected(InvalidActionReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StockfishFailure {
    Unavailable,
    TimedOut,
    Protocol,
    IllegalMove(ChessMove),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrawClaimBasis {
    ThreefoldRepetition,
    FiftyMoveRule,
    ThreefoldRepetitionAndFiftyMoveRule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutomaticDrawReason {
    FivefoldRepetition,
    SeventyFiveMoveRule,
    DeadPosition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChessOutcome {
    Checkmate {
        winner: Color,
    },
    Stalemate,
    Resignation {
        resigned: Color,
        winner: Color,
    },
    DrawAccepted,
    DrawClaimed {
        basis: DrawClaimBasis,
    },
    AutomaticDraw {
        reason: AutomaticDrawReason,
    },
    ModelForfeit {
        final_reason: InvalidActionReason,
        attempts: u8,
    },
    StockfishFailed(StockfishFailure),
    PlyLimitReached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChessState {
    agent_side: Color,
    board: Board,
    committed_moves: Vec<ChessMove>,
    retry: RetryState,
    feedback: ChessFeedback,
    pending_draw_offer: Option<Color>,
    phase: ChessPhase,
    outcome: Option<ChessOutcome>,
    ply_limit: usize,
}

impl ChessState {
    pub(crate) fn new(ply_limit: usize) -> Self {
        Self::for_agent(Color::White, ply_limit)
    }

    pub(crate) fn for_agent(agent_side: Color, ply_limit: usize) -> Self {
        Self {
            agent_side,
            board: Board::default(),
            committed_moves: Vec::new(),
            retry: RetryState::fresh(),
            feedback: ChessFeedback::Initial,
            pending_draw_offer: None,
            phase: ChessPhase::Ready,
            outcome: None,
            ply_limit,
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

    pub(crate) fn retry(&self) -> RetryState {
        self.retry
    }

    pub(crate) fn feedback(&self) -> ChessFeedback {
        self.feedback
    }

    pub(crate) fn current_attempt(&self) -> Option<ModelAttemptKey> {
        (self.phase == ChessPhase::AwaitingModel).then_some(ModelAttemptKey {
            ply: self.committed_moves.len(),
            attempt_index: self.retry.attempts,
        })
    }

    pub(crate) fn pending_draw_offer(&self) -> Option<Color> {
        self.pending_draw_offer
    }

    pub(crate) fn phase(&self) -> ChessPhase {
        self.phase
    }

    pub(crate) fn outcome(&self) -> Option<&ChessOutcome> {
        self.outcome.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChessEvent {
    Start,
    ModelAction {
        attempt: ModelAttemptKey,
        result: Result<ChessAction, InvalidActionReason>,
    },
    StockfishCompleted(Result<ChessMove, StockfishFailure>),
    EngineFailed(StockfishFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChessEffect {
    RequestReaction,
    RequestStockfish(StockfishRequest),
    Complete(ChessOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StockfishRequest {
    pub(crate) position_revision: usize,
    pub(crate) committed_moves: Vec<ChessMove>,
}

pub(crate) fn reduce(state: &mut ChessState, event: ChessEvent) -> Vec<ChessEffect> {
    if state.phase == ChessPhase::Finished {
        return Vec::new();
    }

    match event {
        ChessEvent::Start if state.phase == ChessPhase::Ready => start(state),
        ChessEvent::ModelAction { attempt, result }
            if state.phase == ChessPhase::AwaitingModel
                && state.current_attempt() == Some(attempt) =>
        {
            complete_model(state, result)
        }
        ChessEvent::StockfishCompleted(result) if state.phase == ChessPhase::AwaitingStockfish => {
            complete_stockfish(state, result)
        }
        ChessEvent::EngineFailed(failure) => finish(state, ChessOutcome::StockfishFailed(failure)),
        ChessEvent::Start | ChessEvent::ModelAction { .. } | ChessEvent::StockfishCompleted(_) => {
            Vec::new()
        }
    }
}

fn start(state: &mut ChessState) -> Vec<ChessEffect> {
    if let Some(outcome) = position_outcome(state) {
        return finish(state, outcome);
    }
    request_next_actor(state)
}

fn complete_model(
    state: &mut ChessState,
    result: Result<ChessAction, InvalidActionReason>,
) -> Vec<ChessEffect> {
    let action = match result.and_then(|action| validate_model_action(state, action)) {
        Ok(action) => action,
        Err(reason) => return reject_model_action(state, reason),
    };

    state.retry = RetryState::fresh();
    state.feedback = ChessFeedback::Accepted(action);
    match action {
        ChessAction::ChooseMove(candidate) => {
            state.pending_draw_offer = None;
            commit_move(state, candidate)
        }
        ChessAction::MoveAndOfferDraw(candidate) => {
            state.pending_draw_offer = Some(state.agent_side);
            commit_move(state, candidate)
        }
        ChessAction::Resign => {
            let resigned = state.agent_side;
            finish(
                state,
                ChessOutcome::Resignation {
                    resigned,
                    winner: opposite(resigned),
                },
            )
        }
        ChessAction::AcceptDraw => finish(state, ChessOutcome::DrawAccepted),
        ChessAction::ClaimDraw => {
            let basis = draw_claim_basis(DrawState::from_history(&state.committed_moves));
            finish(state, ChessOutcome::DrawClaimed { basis })
        }
    }
}

fn complete_stockfish(
    state: &mut ChessState,
    result: Result<ChessMove, StockfishFailure>,
) -> Vec<ChessEffect> {
    let candidate = match result {
        Ok(candidate) if is_legal(&state.board, candidate) => candidate,
        Ok(candidate) => {
            return finish(
                state,
                ChessOutcome::StockfishFailed(StockfishFailure::IllegalMove(candidate)),
            )
        }
        Err(failure) => return finish(state, ChessOutcome::StockfishFailed(failure)),
    };

    // Playing a move declines and expires the opponent's pending draw offer.
    state.pending_draw_offer = None;
    commit_move(state, candidate)
}

fn commit_move(state: &mut ChessState, candidate: ChessMove) -> Vec<ChessEffect> {
    state.board = state.board.make_move_new(candidate);
    state.committed_moves.push(candidate);

    if let Some(outcome) = position_outcome(state) {
        return finish(state, outcome);
    }
    if state.committed_moves.len() >= state.ply_limit {
        return finish(state, ChessOutcome::PlyLimitReached);
    }
    request_next_actor(state)
}

fn request_next_actor(state: &mut ChessState) -> Vec<ChessEffect> {
    if state.board.side_to_move() == state.agent_side {
        state.retry = RetryState::fresh();
        state.phase = ChessPhase::AwaitingModel;
        vec![ChessEffect::RequestReaction]
    } else {
        state.phase = ChessPhase::AwaitingStockfish;
        vec![ChessEffect::RequestStockfish(StockfishRequest {
            position_revision: state.committed_moves.len(),
            committed_moves: state.committed_moves.clone(),
        })]
    }
}

fn reject_model_action(state: &mut ChessState, reason: InvalidActionReason) -> Vec<ChessEffect> {
    state.retry = RetryState {
        attempts: state.retry.attempts.saturating_add(1),
        last_rejection: Some(reason),
    };
    state.feedback = ChessFeedback::Rejected(reason);
    if state.retry.attempts < MAX_MODEL_ATTEMPTS {
        vec![ChessEffect::RequestReaction]
    } else {
        finish(
            state,
            ChessOutcome::ModelForfeit {
                final_reason: reason,
                attempts: MAX_MODEL_ATTEMPTS,
            },
        )
    }
}

fn validate_model_action(
    state: &ChessState,
    action: ChessAction,
) -> Result<ChessAction, InvalidActionReason> {
    let unavailable = |reason| InvalidActionReason::ActionUnavailable {
        action: action_kind(action),
        reason,
    };
    if state.phase == ChessPhase::Finished {
        return Err(unavailable(ActionUnavailableReason::MatchFinished));
    }
    if state.board.status() != BoardStatus::Ongoing
        || DrawState::from_history(&state.committed_moves).automatic_draw()
    {
        return Err(unavailable(ActionUnavailableReason::PositionTerminal));
    }
    if state.board.side_to_move() != state.agent_side {
        return Err(unavailable(ActionUnavailableReason::NotAgentTurn));
    }

    match action {
        ChessAction::ChooseMove(_) | ChessAction::MoveAndOfferDraw(_)
            if MoveGen::new_legal(&state.board).next().is_none() =>
        {
            Err(unavailable(ActionUnavailableReason::NoLegalMoves))
        }
        ChessAction::ChooseMove(candidate) | ChessAction::MoveAndOfferDraw(candidate)
            if !is_legal(&state.board, candidate) =>
        {
            Err(InvalidActionReason::IllegalMove(action))
        }
        ChessAction::AcceptDraw if state.pending_draw_offer != Some(opposite(state.agent_side)) => {
            Err(unavailable(
                ActionUnavailableReason::NoPendingOpponentDrawOffer,
            ))
        }
        ChessAction::ClaimDraw if !DrawState::from_history(&state.committed_moves).claimable() => {
            Err(unavailable(ActionUnavailableReason::PositionNotClaimable))
        }
        ChessAction::ChooseMove(_)
        | ChessAction::MoveAndOfferDraw(_)
        | ChessAction::Resign
        | ChessAction::AcceptDraw
        | ChessAction::ClaimDraw => Ok(action),
    }
}

fn action_kind(action: ChessAction) -> ChessActionKind {
    match action {
        ChessAction::ChooseMove(_) => ChessActionKind::ChooseMove,
        ChessAction::MoveAndOfferDraw(_) => ChessActionKind::MoveAndOfferDraw,
        ChessAction::Resign => ChessActionKind::Resign,
        ChessAction::AcceptDraw => ChessActionKind::AcceptDraw,
        ChessAction::ClaimDraw => ChessActionKind::ClaimDraw,
    }
}

fn is_legal(board: &Board, candidate: ChessMove) -> bool {
    MoveGen::new_legal(board).any(|legal| legal == candidate)
}

fn finish(state: &mut ChessState, outcome: ChessOutcome) -> Vec<ChessEffect> {
    state.phase = ChessPhase::Finished;
    state.outcome = Some(outcome.clone());
    vec![ChessEffect::Complete(outcome)]
}

fn position_outcome(state: &ChessState) -> Option<ChessOutcome> {
    match state.board.status() {
        BoardStatus::Checkmate => Some(ChessOutcome::Checkmate {
            winner: opposite(state.board.side_to_move()),
        }),
        BoardStatus::Stalemate => Some(ChessOutcome::Stalemate),
        BoardStatus::Ongoing => {
            let draw_state = DrawState::from_history(&state.committed_moves);
            if draw_state.halfmove_clock() >= 150 {
                Some(ChessOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::SeventyFiveMoveRule,
                })
            } else if draw_state.current_position_repetitions() >= 5 {
                Some(ChessOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::FivefoldRepetition,
                })
            } else if is_dead_position(&state.board) {
                Some(ChessOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::DeadPosition,
                })
            } else {
                None
            }
        }
    }
}

fn draw_claim_basis(draw_state: DrawState) -> DrawClaimBasis {
    match (
        draw_state.current_position_repetitions() >= 3,
        draw_state.halfmove_clock() >= 100,
    ) {
        (true, true) => DrawClaimBasis::ThreefoldRepetitionAndFiftyMoveRule,
        (true, false) => DrawClaimBasis::ThreefoldRepetition,
        (false, true) => DrawClaimBasis::FiftyMoveRule,
        (false, false) => unreachable!("validated draw claim must have a legal basis"),
    }
}

fn is_dead_position(board: &Board) -> bool {
    if board.pieces(Piece::Pawn).popcnt() > 0
        || board.pieces(Piece::Rook).popcnt() > 0
        || board.pieces(Piece::Queen).popcnt() > 0
    {
        return false;
    }

    let knights = board.pieces(Piece::Knight).popcnt();
    let bishops = board.pieces(Piece::Bishop).popcnt();
    if knights + bishops <= 1 {
        return true;
    }
    if knights > 0 {
        return false;
    }

    let mut bishop_colors = (*board.pieces(Piece::Bishop))
        .into_iter()
        .map(|square| (square.get_file().to_index() + square.get_rank().to_index()) % 2);
    let Some(first) = bishop_colors.next() else {
        return true;
    };
    bishop_colors.all(|color| color == first)
}

fn opposite(color: Color) -> Color {
    match color {
        Color::White => Color::Black,
        Color::Black => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_(uci: &str) -> ChessMove {
        uci.parse().expect("test move is valid UCI")
    }

    fn started() -> ChessState {
        let mut state = ChessState::new(100);
        assert_eq!(
            reduce(&mut state, ChessEvent::Start),
            vec![ChessEffect::RequestReaction]
        );
        state
    }

    fn model_event(
        state: &ChessState,
        result: Result<ChessAction, InvalidActionReason>,
    ) -> ChessEvent {
        ChessEvent::ModelAction {
            attempt: state.current_attempt().expect("model turn is active"),
            result,
        }
    }

    #[test]
    fn invalid_model_output_retries_then_forfeits_on_the_third_attempt() {
        let mut state = started();

        for expected_attempts in 1..MAX_MODEL_ATTEMPTS {
            let event = model_event(&state, Err(InvalidActionReason::InvalidXml));
            assert_eq!(
                reduce(&mut state, event),
                vec![ChessEffect::RequestReaction]
            );
            assert_eq!(state.retry().attempts(), expected_attempts);
            assert_eq!(
                state.retry().last_rejection(),
                Some(InvalidActionReason::InvalidXml)
            );
            assert_eq!(state.phase(), ChessPhase::AwaitingModel);
        }

        let outcome = ChessOutcome::ModelForfeit {
            final_reason: InvalidActionReason::InvalidXml,
            attempts: MAX_MODEL_ATTEMPTS,
        };
        let event = model_event(&state, Err(InvalidActionReason::InvalidXml));
        assert_eq!(
            reduce(&mut state, event),
            vec![ChessEffect::Complete(outcome.clone())]
        );
        assert_eq!(state.phase(), ChessPhase::Finished);
        assert_eq!(state.outcome(), Some(&outcome));
    }

    #[test]
    fn accepted_white_move_commits_authoritative_state_then_requests_stockfish() {
        let mut state = started();
        let white = move_("e2e4");
        let event = model_event(&state, Ok(ChessAction::ChooseMove(white)));

        assert_eq!(
            reduce(&mut state, event),
            vec![ChessEffect::RequestStockfish(StockfishRequest {
                position_revision: 1,
                committed_moves: vec![white],
            })]
        );
        assert_eq!(state.committed_moves(), &[white]);
        assert_eq!(state.board(), Board::default().make_move_new(white));
        assert_eq!(state.board().side_to_move(), Color::Black);
        assert_eq!(state.phase(), ChessPhase::AwaitingStockfish);
    }

    #[test]
    fn accepted_black_result_commits_authoritative_state_then_requests_reaction() {
        let mut state = started();
        let white = move_("e2e4");
        let black = move_("e7e5");
        let event = model_event(&state, Ok(ChessAction::MoveAndOfferDraw(white)));
        assert_eq!(
            reduce(&mut state, event),
            vec![ChessEffect::RequestStockfish(StockfishRequest {
                position_revision: 1,
                committed_moves: vec![white],
            })]
        );
        assert_eq!(state.pending_draw_offer(), Some(Color::White));

        assert_eq!(
            reduce(&mut state, ChessEvent::StockfishCompleted(Ok(black))),
            vec![ChessEffect::RequestReaction]
        );
        assert_eq!(state.committed_moves(), &[white, black]);
        assert_eq!(state.pending_draw_offer(), None);
        assert_eq!(state.phase(), ChessPhase::AwaitingModel);
        assert_eq!(state.retry().attempts(), 0);
    }

    #[test]
    fn terminal_completion_is_emitted_once_and_later_events_have_no_effect() {
        let mut state = started();
        let outcome = ChessOutcome::Resignation {
            resigned: Color::White,
            winner: Color::Black,
        };
        let event = model_event(&state, Ok(ChessAction::Resign));

        assert_eq!(
            reduce(&mut state, event),
            vec![ChessEffect::Complete(outcome.clone())]
        );
        let terminal_state = state.clone();

        assert!(reduce(&mut state, ChessEvent::Start).is_empty());
        assert!(reduce(
            &mut state,
            ChessEvent::ModelAction {
                attempt: ModelAttemptKey {
                    ply: 0,
                    attempt_index: 0,
                },
                result: Err(InvalidActionReason::InvalidXml),
            },
        )
        .is_empty());
        assert!(reduce(
            &mut state,
            ChessEvent::StockfishCompleted(Err(StockfishFailure::TimedOut)),
        )
        .is_empty());
        assert_eq!(state, terminal_state);
        assert_eq!(state.outcome(), Some(&outcome));
    }

    #[test]
    fn an_action_from_an_old_attempt_cannot_consume_the_next_attempt() {
        let mut state = started();
        let first_attempt = state.current_attempt().unwrap();
        let first = ChessEvent::ModelAction {
            attempt: first_attempt,
            result: Err(InvalidActionReason::InvalidXml),
        };
        assert_eq!(
            reduce(&mut state, first.clone()),
            vec![ChessEffect::RequestReaction]
        );
        assert_eq!(state.retry().attempts(), 1);

        let after_first = state.clone();
        assert!(reduce(&mut state, first).is_empty());
        assert_eq!(state, after_first);
        assert_eq!(
            state.current_attempt(),
            Some(ModelAttemptKey {
                ply: 0,
                attempt_index: 1,
            })
        );
    }
}
