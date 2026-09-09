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

fn model_event(state: &ChessState, result: Result<ChessAction, InvalidActionReason>) -> ChessEvent {
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
