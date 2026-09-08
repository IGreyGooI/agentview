use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece};

use super::{
    chess_action::{ActionUnavailableReason, ChessAction, InvalidActionReason},
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
pub(crate) struct ModelAttemptKey {
    pub(crate) ply: usize,
    pub(crate) attempt_index: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    retry_attempts: u8,
    feedback: ChessFeedback,
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
            retry_attempts: 0,
            feedback: ChessFeedback::Initial,
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

    pub(crate) fn retry_attempts(&self) -> u8 {
        self.retry_attempts
    }

    pub(crate) fn feedback(&self) -> ChessFeedback {
        self.feedback.clone()
    }

    pub(crate) fn current_attempt(&self) -> Option<ModelAttemptKey> {
        (self.phase == ChessPhase::AwaitingModel).then_some(ModelAttemptKey {
            ply: self.committed_moves.len(),
            attempt_index: self.retry_attempts,
        })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessReduction {
    Applied,
    Ignored,
}

pub(crate) fn reduce(state: &mut ChessState, event: ChessEvent) -> ChessReduction {
    if state.phase == ChessPhase::Finished {
        return ChessReduction::Ignored;
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
            return ChessReduction::Ignored;
        }
    }
    ChessReduction::Applied
}

fn start(state: &mut ChessState) {
    if let Some(outcome) = position_outcome(state) {
        return finish(state, outcome);
    }
    advance_turn(state)
}

fn complete_model(state: &mut ChessState, result: Result<ChessAction, InvalidActionReason>) {
    let action = match result.and_then(|action| validate_model_action(state, action)) {
        Ok(action) => action,
        Err(reason) => return reject_model_action(state, reason),
    };

    state.retry_attempts = 0;
    state.feedback = ChessFeedback::Accepted(action);
    match action {
        ChessAction::ChooseMove(candidate) => commit_move(state, candidate),
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
    }
}

fn complete_stockfish(state: &mut ChessState, result: Result<ChessMove, StockfishFailure>) {
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

    commit_move(state, candidate)
}

fn commit_move(state: &mut ChessState, candidate: ChessMove) {
    state.board = state.board.make_move_new(candidate);
    state.committed_moves.push(candidate);

    if let Some(outcome) = position_outcome(state) {
        return finish(state, outcome);
    }
    if state.committed_moves.len() >= state.ply_limit {
        return finish(state, ChessOutcome::PlyLimitReached);
    }
    advance_turn(state)
}

fn advance_turn(state: &mut ChessState) {
    if state.board.side_to_move() == state.agent_side {
        state.retry_attempts = 0;
        state.phase = ChessPhase::AwaitingModel;
    } else {
        state.phase = ChessPhase::AwaitingStockfish;
    }
}

fn reject_model_action(state: &mut ChessState, reason: InvalidActionReason) {
    state.retry_attempts = state.retry_attempts.saturating_add(1);
    state.feedback = ChessFeedback::Rejected(reason.clone());
    if state.retry_attempts >= MAX_MODEL_ATTEMPTS {
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
        action: action.kind(),
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
        ChessAction::ChooseMove(_) if MoveGen::new_legal(&state.board).next().is_none() => {
            Err(unavailable(ActionUnavailableReason::NoLegalMoves))
        }
        ChessAction::ChooseMove(candidate) if !is_legal(&state.board, candidate) => {
            Err(InvalidActionReason::IllegalMove(action))
        }
        ChessAction::ChooseMove(_) | ChessAction::Resign => Ok(action),
    }
}

fn is_legal(board: &Board, candidate: ChessMove) -> bool {
    MoveGen::new_legal(board).any(|legal| legal == candidate)
}

fn finish(state: &mut ChessState, outcome: ChessOutcome) {
    state.phase = ChessPhase::Finished;
    state.outcome = Some(outcome);
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
    use std::collections::HashMap;

    use super::*;

    fn move_(uci: &str) -> ChessMove {
        uci.parse().expect("test move is valid UCI")
    }

    fn started() -> ChessState {
        let mut state = ChessState::new(100);
        assert_eq!(
            reduce(&mut state, ChessEvent::Start),
            ChessReduction::Applied
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

    fn play(state: &mut ChessState, candidate: ChessMove) -> ChessReduction {
        if state.board().side_to_move() == state.agent_side() {
            let event = model_event(state, Ok(ChessAction::ChooseMove(candidate)));
            reduce(state, event)
        } else {
            reduce(state, ChessEvent::StockfishCompleted(Ok(candidate)))
        }
    }

    fn quiet_history(plies: usize) -> Vec<ChessMove> {
        let mut board = Board::default();
        let mut visits = HashMap::from([(board.get_hash(), 1_usize)]);
        let mut history = Vec::with_capacity(plies);

        for _ in 0..plies {
            let mut candidates = MoveGen::new_legal(&board)
                .filter(|candidate| {
                    board.piece_on(candidate.get_source()) != Some(Piece::Pawn)
                        && board.piece_on(candidate.get_dest()).is_none()
                })
                .filter_map(|candidate| {
                    let next = board.make_move_new(candidate);
                    let previous_visits = visits.get(&next.get_hash()).copied().unwrap_or(0);
                    (next.status() == BoardStatus::Ongoing && previous_visits < 4).then_some((
                        previous_visits,
                        candidate.to_string(),
                        candidate,
                        next,
                    ))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
            let (_, _, candidate, next) = candidates
                .into_iter()
                .next()
                .expect("a quiet legal continuation exists for the test history");

            *visits.entry(next.get_hash()).or_default() += 1;
            history.push(candidate);
            board = next;
        }

        history
    }

    #[test]
    fn invalid_model_output_retries_then_forfeits_on_the_third_attempt() {
        let mut state = started();

        for expected_attempts in 1..MAX_MODEL_ATTEMPTS {
            let event = model_event(&state, Err(InvalidActionReason::InvalidXml));
            assert_eq!(reduce(&mut state, event), ChessReduction::Applied);
            assert_eq!(state.retry_attempts(), expected_attempts);
            assert_eq!(
                state.feedback(),
                ChessFeedback::Rejected(InvalidActionReason::InvalidXml)
            );
            assert_eq!(state.phase(), ChessPhase::AwaitingModel);
        }

        let outcome = ChessOutcome::ModelForfeit {
            final_reason: InvalidActionReason::InvalidXml,
            attempts: MAX_MODEL_ATTEMPTS,
        };
        let event = model_event(&state, Err(InvalidActionReason::InvalidXml));
        assert_eq!(reduce(&mut state, event), ChessReduction::Applied);
        assert_eq!(state.phase(), ChessPhase::Finished);
        assert_eq!(state.outcome(), Some(&outcome));
    }

    #[test]
    fn accepted_white_move_commits_authoritative_state_then_awaits_stockfish() {
        let mut state = started();
        let white = move_("e2e4");
        let event = model_event(&state, Ok(ChessAction::ChooseMove(white)));

        assert_eq!(reduce(&mut state, event), ChessReduction::Applied);
        assert_eq!(state.committed_moves(), &[white]);
        assert_eq!(state.board(), Board::default().make_move_new(white));
        assert_eq!(state.board().side_to_move(), Color::Black);
        assert_eq!(state.phase(), ChessPhase::AwaitingStockfish);
    }

    #[test]
    fn accepted_black_result_commits_authoritative_state_then_awaits_model() {
        let mut state = started();
        let white = move_("e2e4");
        let black = move_("e7e5");
        let event = model_event(&state, Ok(ChessAction::ChooseMove(white)));
        assert_eq!(reduce(&mut state, event), ChessReduction::Applied);

        assert_eq!(
            reduce(&mut state, ChessEvent::StockfishCompleted(Ok(black))),
            ChessReduction::Applied
        );
        assert_eq!(state.committed_moves(), &[white, black]);
        assert_eq!(state.phase(), ChessPhase::AwaitingModel);
        assert_eq!(state.retry_attempts(), 0);
    }

    #[test]
    fn referee_completes_stalemate_and_dead_positions_before_requesting_a_turn() {
        let cases = [
            ("7k/5K2/6Q1/8/8/8/8/8 b - - 0 1", ChessOutcome::Stalemate),
            (
                "7k/8/8/8/8/8/8/K7 w - - 0 1",
                ChessOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::DeadPosition,
                },
            ),
        ];

        for (fen, outcome) in cases {
            let mut state = ChessState::new(200);
            state.board = fen.parse().expect("test FEN is valid");

            assert_eq!(
                reduce(&mut state, ChessEvent::Start),
                ChessReduction::Applied
            );
            assert_eq!(state.phase(), ChessPhase::Finished);
            assert_eq!(state.outcome(), Some(&outcome));
        }
    }

    #[test]
    fn fifth_position_repetition_is_an_automatic_draw() {
        let cycle = [move_("g1f3"), move_("g8f6"), move_("f3g1"), move_("f6g8")];
        let history = cycle.into_iter().cycle().take(16).collect::<Vec<_>>();
        let mut state = ChessState::new(200);
        assert_eq!(
            reduce(&mut state, ChessEvent::Start),
            ChessReduction::Applied
        );

        for candidate in history.iter().copied().take(15) {
            assert_eq!(play(&mut state, candidate), ChessReduction::Applied);
            assert_ne!(state.phase(), ChessPhase::Finished);
        }

        let outcome = ChessOutcome::AutomaticDraw {
            reason: AutomaticDrawReason::FivefoldRepetition,
        };
        assert_eq!(
            play(&mut state, *history.last().unwrap()),
            ChessReduction::Applied
        );
        assert_eq!(state.outcome(), Some(&outcome));
    }

    #[test]
    fn one_hundred_fifty_quiet_halfmoves_trigger_the_seventy_five_move_rule() {
        let history = quiet_history(150);
        let mut state = ChessState::new(200);
        assert_eq!(
            reduce(&mut state, ChessEvent::Start),
            ChessReduction::Applied
        );

        for candidate in history.iter().copied().take(149) {
            assert_eq!(play(&mut state, candidate), ChessReduction::Applied);
            assert_ne!(state.phase(), ChessPhase::Finished);
        }

        let outcome = ChessOutcome::AutomaticDraw {
            reason: AutomaticDrawReason::SeventyFiveMoveRule,
        };
        assert_eq!(
            play(&mut state, *history.last().unwrap()),
            ChessReduction::Applied
        );
        assert_eq!(state.outcome(), Some(&outcome));
    }

    #[test]
    fn terminal_completion_is_applied_once_and_later_events_are_ignored() {
        let mut state = started();
        let outcome = ChessOutcome::Resignation {
            resigned: Color::White,
            winner: Color::Black,
        };
        let event = model_event(&state, Ok(ChessAction::Resign));

        assert_eq!(reduce(&mut state, event), ChessReduction::Applied);
        let terminal_state = state.clone();

        assert_eq!(
            reduce(&mut state, ChessEvent::Start),
            ChessReduction::Ignored
        );
        assert_eq!(
            reduce(
                &mut state,
                ChessEvent::ModelAction {
                    attempt: ModelAttemptKey {
                        ply: 0,
                        attempt_index: 0,
                    },
                    result: Err(InvalidActionReason::InvalidXml),
                },
            ),
            ChessReduction::Ignored
        );
        assert_eq!(
            reduce(
                &mut state,
                ChessEvent::StockfishCompleted(Err(StockfishFailure::TimedOut)),
            ),
            ChessReduction::Ignored
        );
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
        assert_eq!(reduce(&mut state, first.clone()), ChessReduction::Applied);
        assert_eq!(state.retry_attempts(), 1);

        let after_first = state.clone();
        assert_eq!(reduce(&mut state, first), ChessReduction::Ignored);
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
