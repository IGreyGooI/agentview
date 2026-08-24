use std::{
    fmt,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use agentview::component::execution::{
    ComponentReactionRuntime, ProviderEventStream, ProviderFault, ProviderPort, RenderedProjection,
};
use async_trait::async_trait;
use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece};
use tokio::time::{timeout_at, Instant};

use crate::{
    chess_actions::{ChessAction, InvalidActionReason},
    chess_agent::{chess_agent, ChessAgentProps, ChessAgentState, ChessSnapshot},
    chess_feedback::ChessFeedback,
    chess_game_state::DrawState,
    model::{
        play_white_ply_observed, AttemptEvidence, AttemptTransition, InfrastructureAbortReason,
        InfrastructureStage, ModelAttemptContext, ModelTurnId, WhitePlyOutcome,
    },
    observability::{
        CleanupDisposition, GameObserver, LiveObserver, ObservedAttemptResult,
        ObservedInfrastructureReason, ObservedSide, ObservedStockfishResult, ObservedTerminal,
        ObservedTraceArtifactDisposition, RunEvent, SecureTraceCreateError,
        SecureTraceCreateErrorKind, SecureTraceFile, TraceArtifactDisposition,
    },
    uci::{BestMoveError, ShutdownDisposition, UciEngine, UciProcessConfig},
};

fn observe_attempt_transition<O>(
    transition: AttemptTransition,
    ply: usize,
    progress: &Mutex<GameProgress>,
    observer: &mut O,
) -> Result<(), (InfrastructureStage, InfrastructureAbortReason)>
where
    O: GameObserver,
{
    match transition {
        AttemptTransition::Started {
            completed,
            dispatched,
        } => {
            let started_ms = observer.now_ms();
            let active = completed
                .into_iter()
                .chain(std::iter::once(dispatched.abort_evidence(
                    InfrastructureStage::WholeGame,
                    InfrastructureAbortReason::WholeGameTimeout,
                )))
                .collect();
            let mut current = lock(progress);
            *current = current.with_active_white_attempts(active, Some(started_ms));
            drop(current);
            record(
                observer,
                RunEvent::ModelReactionStarted {
                    ply,
                    turn_id: dispatched.turn_id.as_str().to_owned(),
                    attempt_index: dispatched.attempt_index,
                    corrective_reason: dispatched.corrective_reason.map(Into::into),
                },
            )
        }
        AttemptTransition::Completed { completed } => {
            let evidence = completed
                .last()
                .cloned()
                .expect("completed transition carries current attempt");
            let started_ms = lock(progress).active_model_started_ms.unwrap_or_default();
            let duration_ms = observer.now_ms().saturating_sub(started_ms);
            let mut current = lock(progress);
            *current = current.with_active_white_attempts(completed, None);
            drop(current);
            observer
                .record_provider_response_completed(
                    crate::observability::ProviderResponseContext {
                        ply,
                        turn_id: evidence.turn_id.as_str().to_owned(),
                        attempt_index: evidence.attempt_index,
                    },
                    provider_response_completion_required(&evidence.result),
                )
                .map_err(crate::observability::ObservationFailure::classification)?;
            record(
                observer,
                RunEvent::ModelReactionCompleted {
                    ply,
                    turn_id: evidence.turn_id.as_str().to_owned(),
                    attempt_index: evidence.attempt_index,
                    duration_ms,
                    outcome: ObservedAttemptResult::from(evidence.result),
                },
            )
        }
    }
}

fn provider_response_completion_required(result: &crate::model::AttemptResult) -> bool {
    matches!(
        result,
        crate::model::AttemptResult::ActionAccepted(_)
            | crate::model::AttemptResult::Correctable(_)
            | crate::model::AttemptResult::InfrastructureAbort {
                reason_code: InfrastructureAbortReason::StateUnavailable
                    | InfrastructureAbortReason::StateUnfinished
                    | InfrastructureAbortReason::TextIncomplete
                    | InfrastructureAbortReason::StateMismatch,
                ..
            }
    )
}

fn classify_engine_result(
    board: &Board,
    result: Result<ChessMove, BestMoveError>,
) -> (
    Option<ChessMove>,
    ObservedStockfishResult,
    Option<InfrastructureAbortReason>,
) {
    match result {
        Ok(candidate) if MoveGen::new_legal(board).any(|legal| legal == candidate) => (
            Some(candidate),
            ObservedStockfishResult::Candidate {
                uci: candidate.to_string(),
            },
            None,
        ),
        Ok(_) | Err(BestMoveError::Failure) => (
            None,
            ObservedStockfishResult::InfrastructureAbort {
                reason_code: ObservedInfrastructureReason::EngineFailure,
            },
            Some(InfrastructureAbortReason::EngineFailure),
        ),
        Err(BestMoveError::Timeout) => (
            None,
            ObservedStockfishResult::InfrastructureAbort {
                reason_code: ObservedInfrastructureReason::EngineTimeout,
            },
            Some(InfrastructureAbortReason::EngineTimeout),
        ),
    }
}

fn record<O>(
    observer: &mut O,
    event: RunEvent,
) -> Result<(), (InfrastructureStage, InfrastructureAbortReason)>
where
    O: GameObserver,
{
    observer
        .record(event)
        .map_err(crate::observability::ObservationFailure::classification)
}

fn observe_interrupted_work<O>(
    progress: &GameProgress,
    observer: &mut O,
) -> Result<(), (InfrastructureStage, InfrastructureAbortReason)>
where
    O: GameObserver,
{
    if let (Some(evidence), Some(started_ms)) = (
        progress.active_white_attempts.last(),
        progress.active_model_started_ms,
    ) {
        let duration_ms = observer.now_ms().saturating_sub(started_ms);
        observer
            .record_provider_response_completed(
                crate::observability::ProviderResponseContext {
                    ply: progress.accepted_moves.len(),
                    turn_id: evidence.turn_id.as_str().to_owned(),
                    attempt_index: evidence.attempt_index,
                },
                false,
            )
            .map_err(crate::observability::ObservationFailure::classification)?;
        return record(
            observer,
            RunEvent::ModelReactionCompleted {
                ply: progress.accepted_moves.len(),
                turn_id: evidence.turn_id.as_str().to_owned(),
                attempt_index: evidence.attempt_index,
                duration_ms,
                outcome: ObservedAttemptResult::from(evidence.result.clone()),
            },
        );
    }
    if let Some(active) = progress.active_stockfish {
        let duration_ms = observer.now_ms().saturating_sub(active.started_ms);
        return record(
            observer,
            RunEvent::StockfishRequestCompleted {
                ply: active.ply,
                duration_ms,
                outcome: ObservedStockfishResult::InfrastructureAbort {
                    reason_code: ObservedInfrastructureReason::WholeGameTimeout,
                },
            },
        );
    }
    Ok(())
}

fn run_started_event(
    limits: GameLimits,
    provider: crate::observability::EffectiveProviderConfig,
) -> RunEvent {
    RunEvent::RunStarted {
        provider,
        initial_fen: Board::default().to_string(),
        reaction_timeout_ms: duration_ms(limits.reaction_timeout),
        engine_timeout_ms: duration_ms(limits.engine_timeout),
        whole_game_deadline_ms: duration_ms(limits.whole_game_deadline),
        ply_limit: limits.ply_limit,
        engine_nodes: limits.engine_nodes,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn finalize_observed_result<O>(
    result: PlayResult,
    limits: GameLimits,
    model_calls: usize,
    run_started_ms: u64,
    cleanup: CleanupDisposition,
    observer: &mut O,
) -> PlayResult
where
    O: GameObserver,
{
    let outcome = finalize_outcome(
        TerminalSnapshot {
            outcome: result.outcome.clone(),
            final_board: result.final_board,
            committed_plies: result.accepted_moves.len(),
            model_calls,
            limits,
            run_started_ms,
            cleanup,
            trace_artifact_disposition: observer.trace_artifact_disposition(),
        },
        observer,
    );
    result.with_outcome(outcome)
}

#[derive(Clone)]
struct TerminalSnapshot {
    outcome: GameOutcome,
    final_board: Board,
    committed_plies: usize,
    model_calls: usize,
    limits: GameLimits,
    run_started_ms: u64,
    cleanup: CleanupDisposition,
    trace_artifact_disposition: Option<TraceArtifactDisposition>,
}

fn finalize_outcome<O>(snapshot: TerminalSnapshot, observer: &mut O) -> GameOutcome
where
    O: GameObserver,
{
    let snapshot = match observer.validate_provider_response_evidence() {
        Ok(()) => snapshot,
        Err(failure) => {
            let (stage, reason_code) = failure.classification();
            TerminalSnapshot {
                outcome: preserve_shutdown_failure(
                    snapshot.outcome.clone(),
                    GameOutcome::InfrastructureAbort { stage, reason_code },
                ),
                ..snapshot
            }
        }
    };
    let duration_ms = observer.now_ms().saturating_sub(snapshot.run_started_ms);
    let terminal = terminal_event(
        snapshot.clone(),
        duration_ms,
        &observer.provider_response_evidence(),
    );
    match observer.record_terminal(terminal) {
        Ok(()) => snapshot.outcome,
        Err(crate::observability::ObservationFailure::StatusWrite) => snapshot.outcome,
        Err(failure) if failure.terminal_outcome_published() => {
            preserve_published_terminal(snapshot, duration_ms, observer)
        }
        Err(failure) => finalize_terminal_failure(snapshot, duration_ms, failure, observer),
    }
}

fn preserve_published_terminal<O>(
    snapshot: TerminalSnapshot,
    duration_ms: u64,
    observer: &mut O,
) -> GameOutcome
where
    O: GameObserver,
{
    let published = TerminalSnapshot {
        trace_artifact_disposition: observer.trace_artifact_disposition(),
        ..snapshot.clone()
    };
    let terminal = terminal_event(
        published,
        duration_ms,
        &observer.provider_response_evidence(),
    );
    match observer.record_terminal(terminal) {
        Ok(()) | Err(crate::observability::ObservationFailure::StatusWrite) => snapshot.outcome,
        Err(crate::observability::ObservationFailure::ProviderResponseCapture) => snapshot.outcome,
        Err(crate::observability::ObservationFailure::TraceWrite { .. }) => snapshot.outcome,
    }
}

fn finalize_terminal_failure<O>(
    snapshot: TerminalSnapshot,
    duration_ms: u64,
    failure: crate::observability::ObservationFailure,
    observer: &mut O,
) -> GameOutcome
where
    O: GameObserver,
{
    let (stage, reason_code) = failure.classification();
    let abort = preserve_shutdown_failure(
        snapshot.outcome.clone(),
        GameOutcome::InfrastructureAbort { stage, reason_code },
    );
    let fallback = TerminalSnapshot {
        outcome: abort.clone(),
        trace_artifact_disposition: observer.trace_artifact_disposition(),
        ..snapshot.clone()
    };
    let terminal = terminal_event(
        fallback,
        duration_ms,
        &observer.provider_response_evidence(),
    );
    match observer.record_terminal(terminal) {
        Ok(()) => abort,
        Err(fallback_failure) => {
            let (stage, reason_code) = fallback_failure.classification();
            preserve_shutdown_failure(
                snapshot.outcome,
                GameOutcome::InfrastructureAbort { stage, reason_code },
            )
        }
    }
}

fn preserve_shutdown_failure(proven: GameOutcome, fallback: GameOutcome) -> GameOutcome {
    if matches!(
        proven,
        GameOutcome::InfrastructureAbort {
            stage: InfrastructureStage::Shutdown,
            reason_code: InfrastructureAbortReason::ShutdownFailure,
        }
    ) {
        proven
    } else {
        fallback
    }
}

fn terminal_event(
    snapshot: TerminalSnapshot,
    duration_ms: u64,
    provider_responses: &[crate::observability::ProviderResponseEvidence],
) -> RunEvent {
    RunEvent::Terminal {
        outcome: ObservedTerminal::from_outcome(snapshot.outcome, snapshot.limits.ply_limit),
        final_fen: snapshot.final_board.to_string(),
        committed_plies: snapshot.committed_plies,
        model_calls: snapshot.model_calls,
        duration_ms,
        cleanup: snapshot.cleanup,
        provider_responses: crate::observability::ProviderResponseSummary::from_responses(
            provider_responses,
        ),
        trace_artifact_disposition: snapshot
            .trace_artifact_disposition
            .map(ObservedTraceArtifactDisposition::from),
    }
}

async fn play_game<P, O>(
    white: &mut ComponentReactionRuntime<P, ChessAgentProps, ChessAgentState>,
    engine: &mut UciEngine,
    limits: GameLimits,
    progress: Arc<Mutex<GameProgress>>,
    observer: &mut O,
) -> PlayResult
where
    P: ProviderPort,
    O: GameObserver,
{
    let mut board = Board::default();
    let mut accepted_moves = Vec::new();
    let mut validated_black_moves = Vec::new();
    let mut attempts = Vec::new();
    let mut pending_draw_offer = None;
    let mut feedback = ChessFeedback::initial();

    loop {
        let ply = accepted_moves.len();
        let side = ObservedSide::from(board.side_to_move());
        let white_turn_id = (side == ObservedSide::White).then(|| ModelTurnId::for_white_ply(ply));
        let turn_event = RunEvent::TurnStarted {
            ply,
            side,
            turn_id: white_turn_id
                .as_ref()
                .map(|turn_id| turn_id.as_str().to_owned()),
            fen: board.to_string(),
        };
        if let Err((stage, reason_code)) = record(observer, turn_event) {
            return infrastructure_result(
                board,
                accepted_moves,
                validated_black_moves,
                attempts,
                stage,
                reason_code,
            );
        }
        let candidate = if board.side_to_move() == Color::White {
            let turn_id = white_turn_id.expect("White turn has one closed turn id");
            let attempt_progress = Arc::clone(&progress);
            let snapshot = ChessSnapshot::in_progress(
                Color::White,
                board,
                accepted_moves.clone(),
                pending_draw_offer,
                feedback.clone(),
            );
            match play_white_ply_observed(
                white,
                snapshot.clone(),
                turn_id,
                accepted_moves.is_empty(),
                limits.reaction_timeout,
                |transition| {
                    observe_attempt_transition(transition, ply, &attempt_progress, observer)
                },
            )
            .await
            {
                WhitePlyOutcome::Action {
                    action,
                    attempts: white_attempts,
                } => {
                    attempts = appended_attempts(&attempts, &white_attempts);
                    match action {
                        ChessAction::ChooseMove(candidate) => {
                            feedback = ChessFeedback::accepted(
                                action,
                                "The referee accepted the legal move and committed it.",
                            );
                            pending_draw_offer = None;
                            candidate
                        }
                        ChessAction::MoveAndOfferDraw(candidate) => {
                            feedback = ChessFeedback::accepted(
                                action,
                                "The referee accepted the legal move and delivered the draw offer to the opponent.",
                            );
                            pending_draw_offer = Some(Color::White);
                            candidate
                        }
                        ChessAction::Resign => {
                            return PlayResult {
                                outcome: GameOutcome::Resignation {
                                    resigned: Color::White,
                                    winner: Color::Black,
                                },
                                final_board: board,
                                accepted_moves,
                                validated_black_moves,
                                attempts,
                            };
                        }
                        ChessAction::AcceptDraw
                            if snapshot.pending_draw_offer() == Some(Color::Black) =>
                        {
                            return PlayResult {
                                outcome: GameOutcome::DrawAccepted,
                                final_board: board,
                                accepted_moves,
                                validated_black_moves,
                                attempts,
                            };
                        }
                        ChessAction::ClaimDraw if snapshot.draw_state().claimable() => {
                            return PlayResult {
                                outcome: GameOutcome::DrawClaimed {
                                    basis: draw_claim_basis(snapshot.draw_state()),
                                },
                                final_board: board,
                                accepted_moves,
                                validated_black_moves,
                                attempts,
                            };
                        }
                        ChessAction::AcceptDraw | ChessAction::ClaimDraw => {
                            return infrastructure_result(
                                board,
                                accepted_moves,
                                validated_black_moves,
                                attempts,
                                InfrastructureStage::AuthoritativeState,
                                InfrastructureAbortReason::StateMismatch,
                            );
                        }
                    }
                }
                WhitePlyOutcome::ModelForfeit {
                    final_reason,
                    attempts: white_attempts,
                } => {
                    attempts = appended_attempts(&attempts, &white_attempts);
                    return PlayResult {
                        outcome: GameOutcome::ModelForfeit {
                            final_reason,
                            attempts: 3,
                        },
                        final_board: board,
                        accepted_moves,
                        validated_black_moves,
                        attempts,
                    };
                }
                WhitePlyOutcome::InfrastructureAbort {
                    stage,
                    reason_code,
                    attempts: white_attempts,
                } => {
                    attempts = appended_attempts(&attempts, &white_attempts);
                    return PlayResult {
                        outcome: GameOutcome::InfrastructureAbort { stage, reason_code },
                        final_board: board,
                        accepted_moves,
                        validated_black_moves,
                        attempts,
                    };
                }
            }
        } else {
            let started_ms = observer.now_ms();
            {
                let mut current = lock(&progress);
                *current = current.with_active_stockfish(ply, started_ms);
            }
            if let Err((stage, reason_code)) = record(
                observer,
                RunEvent::StockfishRequestStarted {
                    ply,
                    fen: board.to_string(),
                    nodes: limits.engine_nodes,
                },
            ) {
                return infrastructure_result(
                    board,
                    accepted_moves,
                    validated_black_moves,
                    attempts,
                    stage,
                    reason_code,
                );
            }
            let engine_result = engine.best_move(&accepted_moves, limits.engine_nodes).await;
            let (candidate, observed_result, engine_failure) =
                classify_engine_result(&board, engine_result);
            let duration_ms = observer.now_ms().saturating_sub(started_ms);
            {
                let mut current = lock(&progress);
                *current = current.without_active_stockfish();
            }
            if let Err((stage, reason_code)) = record(
                observer,
                RunEvent::StockfishRequestCompleted {
                    ply,
                    duration_ms,
                    outcome: observed_result,
                },
            ) {
                return infrastructure_result(
                    board,
                    accepted_moves,
                    validated_black_moves,
                    attempts,
                    stage,
                    reason_code,
                );
            }
            if let Some(reason_code) = engine_failure {
                return infrastructure_result(
                    board,
                    accepted_moves,
                    validated_black_moves,
                    attempts,
                    InfrastructureStage::Engine,
                    reason_code,
                );
            }
            let candidate = candidate.expect("successful engine result has one candidate");
            validated_black_moves = appended(&validated_black_moves, candidate);
            if pending_draw_offer == Some(Color::White) {
                if let Some(offered_move) = accepted_moves.last().copied() {
                    feedback = ChessFeedback::accepted(
                        ChessAction::MoveAndOfferDraw(offered_move),
                        "The move and draw offer were accepted by the referee; the opponent played a move, so the draw offer expired.",
                    );
                }
            }
            pending_draw_offer = None;
            candidate
        };

        let fen_before = board.to_string();
        let next_board = match independently_apply_legal_move(&board, candidate) {
            Ok(board) => board,
            Err(_) => {
                return infrastructure_result(
                    board,
                    accepted_moves,
                    validated_black_moves,
                    attempts,
                    InfrastructureStage::AuthoritativeState,
                    InfrastructureAbortReason::StateMismatch,
                )
            }
        };
        board = next_board;
        accepted_moves = appended(&accepted_moves, candidate);
        let next_progress = GameProgress {
            board,
            accepted_moves: accepted_moves.clone(),
            validated_black_moves: validated_black_moves.clone(),
            attempts: attempts.clone(),
            active_white_attempts: Vec::new(),
            active_model_started_ms: None,
            active_stockfish: None,
        };
        *lock(&progress) = next_progress;
        if let Err((stage, reason_code)) = record(
            observer,
            RunEvent::MoveCommitted {
                ply,
                side,
                uci: candidate.to_string(),
                fen_before,
                fen_after: board.to_string(),
            },
        ) {
            return infrastructure_result(
                board,
                accepted_moves,
                validated_black_moves,
                attempts,
                stage,
                reason_code,
            );
        }

        if let Some(outcome) = terminal_outcome(&board, &accepted_moves) {
            return PlayResult {
                outcome,
                final_board: board,
                accepted_moves,
                validated_black_moves,
                attempts,
            };
        }
        if accepted_moves.len() >= limits.ply_limit {
            return PlayResult {
                outcome: GameOutcome::PlyLimitReached,
                final_board: board,
                accepted_moves,
                validated_black_moves,
                attempts,
            };
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TraceSetupFailure {
    kind: TraceSetupFailureKind,
    artifact_disposition: TraceArtifactDisposition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraceSetupFailureKind {
    CreationValidation,
    IncompleteCleanup,
}

impl TraceSetupFailure {
    pub(crate) fn artifact_disposition(self) -> TraceArtifactDisposition {
        self.artifact_disposition
    }

    fn incomplete_cleanup(artifact_disposition: TraceArtifactDisposition) -> Self {
        Self {
            kind: TraceSetupFailureKind::IncompleteCleanup,
            artifact_disposition,
        }
    }
}

impl fmt::Display for TraceSetupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("trace setup failed")
    }
}

impl std::error::Error for TraceSetupFailure {}

pub(crate) fn random_run_id() -> anyhow::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| anyhow::anyhow!("run identifier unavailable"))?;
    Ok(random
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) fn create_trace(path: &Path) -> anyhow::Result<SecureTraceFile> {
    SecureTraceFile::create(path).map_err(map_trace_create_error)
}

fn map_trace_create_error(failure: SecureTraceCreateError) -> anyhow::Error {
    match failure.kind() {
        SecureTraceCreateErrorKind::TargetRejected => {
            anyhow::anyhow!("trace target could not be created safely")
        }
        SecureTraceCreateErrorKind::ValidationFailed => anyhow::Error::new(TraceSetupFailure {
            kind: TraceSetupFailureKind::CreationValidation,
            artifact_disposition: failure.artifact_disposition(),
        }),
    }
}

pub(crate) fn finish_or_discard_incomplete<S, C>(
    result: anyhow::Result<GameEvidence>,
    observer: LiveObserver<S, SecureTraceFile, C>,
) -> anyhow::Result<GameEvidence> {
    match result {
        Ok(evidence) => Ok(evidence),
        Err(error) => match observer.discard_trace() {
            TraceArtifactDisposition::Removed => Err(error),
            disposition => Err(anyhow::Error::new(TraceSetupFailure::incomplete_cleanup(
                disposition,
            ))),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameLimits {
    pub reaction_timeout: Duration,
    pub engine_timeout: Duration,
    pub whole_game_deadline: Duration,
    pub ply_limit: usize,
    pub engine_nodes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameOutcome {
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
    InfrastructureAbort {
        stage: InfrastructureStage,
        reason_code: InfrastructureAbortReason,
    },
    PlyLimitReached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawClaimBasis {
    ThreefoldRepetition,
    FiftyMoveRule,
    ThreefoldRepetitionAndFiftyMoveRule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomaticDrawReason {
    FivefoldRepetition,
    SeventyFiveMoveRule,
    DeadPosition,
}

#[derive(Debug)]
pub struct GameEvidence {
    pub outcome: GameOutcome,
    pub final_board: Board,
    pub accepted_moves: Vec<ChessMove>,
    pub attempts: Vec<AttemptEvidence>,
    pub provider_execute_count: usize,
    pub component_turn_executions: usize,
    pub(crate) effective_provider: crate::observability::EffectiveProviderConfig,
    pub(crate) provider_responses: Vec<crate::observability::ProviderResponseEvidence>,
    pub component_host_id_retained: bool,
    pub uci_commands: Vec<String>,
    pub validated_black_moves: Vec<ChessMove>,
    pub limits: GameLimits,
    pub engine_reads_bounded: bool,
    pub child_shutdown_observed: bool,
    pub(crate) trace_artifact_disposition: Option<super::observability::TraceArtifactDisposition>,
    pub(crate) status_output_complete: bool,
}

pub(crate) async fn run_provider_game_with_observer_until<P, O>(
    provider: P,
    engine_config: UciProcessConfig,
    limits: GameLimits,
    deadline: Instant,
    effective_provider: crate::observability::EffectiveProviderConfig,
    observer: &mut O,
) -> anyhow::Result<GameEvidence>
where
    P: ProviderPort,
    O: GameObserver,
{
    run_provider_game(
        provider,
        engine_config,
        limits,
        deadline,
        effective_provider,
        observer,
    )
    .await
}

async fn run_provider_game<P, O>(
    provider: P,
    engine_config: UciProcessConfig,
    limits: GameLimits,
    deadline: Instant,
    effective_provider: crate::observability::EffectiveProviderConfig,
    observer: &mut O,
) -> anyhow::Result<GameEvidence>
where
    P: ProviderPort,
    O: GameObserver,
{
    validate_limits(limits)?;
    let run_started_ms = observer.now_ms();
    if let Err(failure) = observer.record(run_started_event(limits, effective_provider.clone())) {
        let (stage, reason_code) = failure.classification();
        return finish_before_runtime(
            limits,
            BeforeRuntimeFailure {
                stage,
                reason_code,
                child_shutdown_observed: true,
            },
            run_started_ms,
            0,
            effective_provider,
            observer,
        )
        .await;
    }
    let provider_execute_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider::new(provider, Arc::clone(&provider_execute_count));
    let spawned = UciEngine::spawn_until(engine_config, limits.engine_timeout, deadline).await;
    let mut engine = match spawned {
        Ok(engine) => engine,
        Err(failure) => {
            let (stage, reason_code) = if failure.deadline_exhausted() {
                (
                    InfrastructureStage::WholeGame,
                    InfrastructureAbortReason::WholeGameTimeout,
                )
            } else {
                (
                    InfrastructureStage::Engine,
                    InfrastructureAbortReason::EngineFailure,
                )
            };
            return finish_before_runtime(
                limits,
                BeforeRuntimeFailure {
                    stage,
                    reason_code,
                    child_shutdown_observed: failure.child_shutdown_observed(),
                },
                run_started_ms,
                provider_execute_count.load(Ordering::SeqCst),
                effective_provider,
                observer,
            )
            .await;
        }
    };
    let component_render_count = Arc::new(AtomicUsize::new(0));
    let initial_snapshot = ChessSnapshot::in_progress(
        Color::White,
        Board::default(),
        Vec::new(),
        None,
        ChessFeedback::initial(),
    );
    let mut white = ComponentReactionRuntime::new(
        provider,
        chess_agent,
        ChessAgentProps::new(
            initial_snapshot,
            ModelAttemptContext::initial(ModelTurnId::for_white_ply(0)),
            Arc::clone(&component_render_count),
        ),
    );
    let component_host_id = white.component_host_id();
    let progress = Arc::new(Mutex::new(GameProgress::new()));

    let game = play_game(
        &mut white,
        &mut engine,
        limits,
        Arc::clone(&progress),
        observer,
    );
    let remaining = deadline.saturating_duration_since(Instant::now());
    let cleanup_reserve = std::cmp::min(limits.engine_timeout, remaining / 2);
    let play_deadline = deadline
        .checked_sub(cleanup_reserve)
        .unwrap_or_else(Instant::now);
    let game_result = timeout_at(play_deadline, game).await;
    let timeout_observation = if game_result.is_err() {
        observe_interrupted_work(&lock(&progress), observer)
    } else {
        Ok(())
    };
    let shutdown = engine.shutdown_until(deadline).await;
    let child_shutdown_observed = matches!(
        shutdown,
        Ok(ShutdownDisposition::Graceful | ShutdownDisposition::ForcedReap)
    );
    let provider_execute_count = provider_execute_count.load(Ordering::SeqCst);
    let uci_commands = engine.sent_commands().to_vec();
    let component_host_id_retained = white.component_host_id() == component_host_id;
    let deadline_exhausted = Instant::now() >= deadline;
    let result = match game_result {
        Ok(result) => result,
        Err(_) => PlayResult::from_progress(
            &lock(&progress),
            GameOutcome::InfrastructureAbort {
                stage: InfrastructureStage::WholeGame,
                reason_code: InfrastructureAbortReason::WholeGameTimeout,
            },
        ),
    };
    let result = match timeout_observation {
        Ok(()) => result,
        Err((stage, reason_code)) => {
            result.with_outcome(GameOutcome::InfrastructureAbort { stage, reason_code })
        }
    };
    let result = if !child_shutdown_observed {
        result.with_outcome(GameOutcome::InfrastructureAbort {
            stage: InfrastructureStage::Shutdown,
            reason_code: InfrastructureAbortReason::ShutdownFailure,
        })
    } else if deadline_exhausted {
        result.with_outcome(GameOutcome::InfrastructureAbort {
            stage: InfrastructureStage::WholeGame,
            reason_code: InfrastructureAbortReason::WholeGameTimeout,
        })
    } else {
        result
    };
    let cleanup = CleanupDisposition {
        engine_shutdown_observed: child_shutdown_observed,
    };
    let result = finalize_observed_result(
        result,
        limits,
        provider_execute_count,
        run_started_ms,
        cleanup,
        observer,
    );
    let provider_responses = observer.provider_response_evidence();

    Ok(GameEvidence {
        outcome: result.outcome,
        final_board: result.final_board,
        accepted_moves: result.accepted_moves,
        attempts: result.attempts,
        provider_execute_count,
        component_turn_executions: component_render_count.load(Ordering::SeqCst),
        effective_provider,
        provider_responses,
        component_host_id_retained,
        uci_commands,
        validated_black_moves: result.validated_black_moves,
        limits,
        engine_reads_bounded: engine.reads_are_bounded(),
        child_shutdown_observed,
        trace_artifact_disposition: observer.trace_artifact_disposition(),
        status_output_complete: observer.status_output_complete(),
    })
}

struct CountingProvider<P> {
    inner: P,
    execute_count: Arc<AtomicUsize>,
}

impl<P> CountingProvider<P> {
    fn new(inner: P, execute_count: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            execute_count,
        }
    }
}

#[async_trait]
impl<P> ProviderPort for CountingProvider<P>
where
    P: ProviderPort,
{
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.execute_count.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(projection).await
    }
}

#[derive(Clone)]
struct BeforeRuntimeFailure {
    stage: InfrastructureStage,
    reason_code: InfrastructureAbortReason,
    child_shutdown_observed: bool,
}

async fn finish_before_runtime<O>(
    limits: GameLimits,
    failure: BeforeRuntimeFailure,
    run_started_ms: u64,
    provider_execute_count: usize,
    effective_provider: crate::observability::EffectiveProviderConfig,
    observer: &mut O,
) -> anyhow::Result<GameEvidence>
where
    O: GameObserver,
{
    let outcome = if failure.child_shutdown_observed {
        GameOutcome::InfrastructureAbort {
            stage: failure.stage,
            reason_code: failure.reason_code,
        }
    } else {
        GameOutcome::InfrastructureAbort {
            stage: InfrastructureStage::Shutdown,
            reason_code: InfrastructureAbortReason::ShutdownFailure,
        }
    };
    let cleanup = CleanupDisposition {
        engine_shutdown_observed: failure.child_shutdown_observed,
    };
    let outcome = finalize_outcome(
        TerminalSnapshot {
            outcome,
            final_board: Board::default(),
            committed_plies: 0,
            model_calls: provider_execute_count,
            limits,
            run_started_ms,
            cleanup,
            trace_artifact_disposition: observer.trace_artifact_disposition(),
        },
        observer,
    );
    let provider_responses = observer.provider_response_evidence();
    Ok(GameEvidence {
        outcome,
        final_board: Board::default(),
        accepted_moves: Vec::new(),
        attempts: Vec::new(),
        provider_execute_count,
        component_turn_executions: 0,
        effective_provider,
        provider_responses,
        component_host_id_retained: false,
        uci_commands: Vec::new(),
        validated_black_moves: Vec::new(),
        limits,
        engine_reads_bounded: false,
        child_shutdown_observed: failure.child_shutdown_observed,
        trace_artifact_disposition: observer.trace_artifact_disposition(),
        status_output_complete: observer.status_output_complete(),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone)]
struct GameProgress {
    board: Board,
    accepted_moves: Vec<ChessMove>,
    validated_black_moves: Vec<ChessMove>,
    attempts: Vec<AttemptEvidence>,
    active_white_attempts: Vec<AttemptEvidence>,
    active_model_started_ms: Option<u64>,
    active_stockfish: Option<ActiveStockfish>,
}

#[derive(Clone, Copy)]
struct ActiveStockfish {
    ply: usize,
    started_ms: u64,
}

impl GameProgress {
    fn new() -> Self {
        Self {
            board: Board::default(),
            accepted_moves: Vec::new(),
            validated_black_moves: Vec::new(),
            attempts: Vec::new(),
            active_white_attempts: Vec::new(),
            active_model_started_ms: None,
            active_stockfish: None,
        }
    }

    fn with_active_white_attempts(
        &self,
        active_white_attempts: Vec<AttemptEvidence>,
        active_model_started_ms: Option<u64>,
    ) -> Self {
        Self {
            active_white_attempts,
            active_model_started_ms,
            ..self.clone()
        }
    }

    fn with_active_stockfish(&self, ply: usize, started_ms: u64) -> Self {
        Self {
            active_stockfish: Some(ActiveStockfish { ply, started_ms }),
            ..self.clone()
        }
    }

    fn without_active_stockfish(&self) -> Self {
        Self {
            active_stockfish: None,
            ..self.clone()
        }
    }
}

struct PlayResult {
    outcome: GameOutcome,
    final_board: Board,
    accepted_moves: Vec<ChessMove>,
    validated_black_moves: Vec<ChessMove>,
    attempts: Vec<AttemptEvidence>,
}

impl PlayResult {
    fn from_progress(progress: &GameProgress, outcome: GameOutcome) -> Self {
        Self {
            outcome,
            final_board: progress.board,
            accepted_moves: progress.accepted_moves.clone(),
            validated_black_moves: progress.validated_black_moves.clone(),
            attempts: appended_attempts(&progress.attempts, &progress.active_white_attempts),
        }
    }

    fn with_outcome(self, outcome: GameOutcome) -> Self {
        Self { outcome, ..self }
    }
}

fn infrastructure_result(
    final_board: Board,
    accepted_moves: Vec<ChessMove>,
    validated_black_moves: Vec<ChessMove>,
    attempts: Vec<AttemptEvidence>,
    stage: InfrastructureStage,
    reason_code: InfrastructureAbortReason,
) -> PlayResult {
    PlayResult {
        outcome: GameOutcome::InfrastructureAbort { stage, reason_code },
        final_board,
        accepted_moves,
        validated_black_moves,
        attempts,
    }
}

fn appended_attempts(
    attempts: &[AttemptEvidence],
    next: &[AttemptEvidence],
) -> Vec<AttemptEvidence> {
    attempts
        .iter()
        .cloned()
        .chain(next.iter().cloned())
        .collect()
}

fn independently_apply_legal_move(board: &Board, candidate: ChessMove) -> anyhow::Result<Board> {
    if !MoveGen::new_legal(board).any(|legal| legal == candidate) {
        anyhow::bail!("candidate move was independently rejected as illegal");
    }
    Ok(board.make_move_new(candidate))
}

fn appended(moves: &[ChessMove], candidate: ChessMove) -> Vec<ChessMove> {
    let mut next = Vec::with_capacity(moves.len() + 1);
    next.extend_from_slice(moves);
    next.push(candidate);
    next
}

fn terminal_outcome(board: &Board, history: &[ChessMove]) -> Option<GameOutcome> {
    match board.status() {
        BoardStatus::Ongoing => {
            let draw_state = DrawState::from_history(history);
            if draw_state.halfmove_clock() >= 150 {
                Some(GameOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::SeventyFiveMoveRule,
                })
            } else if draw_state.current_position_repetitions() >= 5 {
                Some(GameOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::FivefoldRepetition,
                })
            } else if is_dead_position(board) {
                Some(GameOutcome::AutomaticDraw {
                    reason: AutomaticDrawReason::DeadPosition,
                })
            } else {
                None
            }
        }
        BoardStatus::Stalemate => Some(GameOutcome::Stalemate),
        BoardStatus::Checkmate => Some(GameOutcome::Checkmate {
            winner: if board.side_to_move() == Color::White {
                Color::Black
            } else {
                Color::White
            },
        }),
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
        (false, false) => unreachable!("a validated draw claim has one legal basis"),
    }
}

pub(crate) fn is_dead_position(board: &Board) -> bool {
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
    knights == 0
        && (*board.pieces(Piece::Bishop))
            .map(|square| (square.get_file().to_index() + square.get_rank().to_index()) % 2)
            .all_equal()
}

trait AllEqual: Iterator {
    fn all_equal(mut self) -> bool
    where
        Self: Sized,
        Self::Item: PartialEq,
    {
        let Some(first) = self.next() else {
            return true;
        };
        self.all(|item| item == first)
    }
}

impl<I: Iterator> AllEqual for I {}

fn validate_limits(limits: GameLimits) -> anyhow::Result<()> {
    if limits.reaction_timeout.is_zero()
        || limits.engine_timeout.is_zero()
        || limits.whole_game_deadline.is_zero()
        || limits.ply_limit == 0
        || limits.engine_nodes == 0
    {
        anyhow::bail!("all game bounds must be positive");
    }
    Ok(())
}
