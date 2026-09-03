use std::{
    fmt,
    panic::{resume_unwind, AssertUnwindSafe},
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use agentview::component::execution::{
    Application, Frame, ProviderFactStream, ReactionPort, ReactionPortFault, SubmitFault,
    TargetDeclaration,
};
use async_trait::async_trait;
use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece};
use futures::FutureExt;
use tokio::time::{timeout_at, Instant};

use crate::{
    chess_actions::{ChessAction, InvalidActionReason},
    chess_agent::{chess_agent, ChessAgentState, ChessControl, ChessSnapshot},
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
    white: &mut Application<P>,
    control: &ChessControl,
    engine: &mut UciEngine,
    limits: GameLimits,
    progress: Arc<Mutex<GameProgress>>,
    observer: &mut O,
) -> PlayResult
where
    P: ReactionPort,
    O: GameObserver,
{
    let mut board = Board::default();
    let mut accepted_moves = Vec::new();
    let mut validated_black_moves = Vec::new();
    let mut attempts = Vec::new();
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
                feedback.clone(),
            );
            match play_white_ply_observed(
                white,
                control,
                snapshot.clone(),
                turn_id,
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
    pub application_shutdown_observed: bool,
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
    P: ReactionPort,
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
    P: ReactionPort,
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
                application_shutdown_observed: true,
                component_host_id_retained: false,
                component_turn_executions: 0,
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
    let component_turn_executions = Arc::new(AtomicUsize::new(0));
    let initial_snapshot = ChessSnapshot::in_progress(
        Color::White,
        Board::default(),
        Vec::new(),
        ChessFeedback::initial(),
    );
    let initial_state = ChessAgentState::awaiting(
        Arc::new(initial_snapshot),
        ModelAttemptContext::initial(ModelTurnId::for_white_ply(0)),
    );
    let exported_control = Arc::new(Mutex::new(None));
    let root_state = initial_state.clone();
    let root_control = Arc::clone(&exported_control);
    let root_turn_executions = Arc::clone(&component_turn_executions);
    let mut white = match Application::mount(
        move || {
            chess_agent(
                root_state.clone(),
                Arc::clone(&root_control),
                Arc::clone(&root_turn_executions),
            )
        },
        provider,
    ) {
        Ok(application) => application,
        Err(fault) => {
            return finish_before_runtime(
                limits,
                BeforeRuntimeFailure {
                    stage: InfrastructureStage::ModelReaction,
                    reason_code: InfrastructureAbortReason::from_application(&fault),
                    child_shutdown_observed: true,
                    application_shutdown_observed: true,
                    component_host_id_retained: false,
                    component_turn_executions: 0,
                },
                run_started_ms,
                provider_execute_count.load(Ordering::SeqCst),
                effective_provider,
                observer,
            )
            .await;
        }
    };
    let control = lock(&exported_control).clone();
    let Some(control) = control else {
        let application_shutdown = AssertUnwindSafe(white.shutdown()).catch_unwind().await;
        let application_shutdown_observed = match application_shutdown {
            Ok(result) => result.is_ok(),
            Err(payload) => resume_unwind(payload),
        };
        return finish_before_runtime(
            limits,
            BeforeRuntimeFailure {
                stage: InfrastructureStage::AuthoritativeState,
                reason_code: InfrastructureAbortReason::StateUnavailable,
                child_shutdown_observed: true,
                application_shutdown_observed,
                component_host_id_retained: false,
                component_turn_executions: component_turn_executions.load(Ordering::SeqCst),
            },
            run_started_ms,
            provider_execute_count.load(Ordering::SeqCst),
            effective_provider,
            observer,
        )
        .await;
    };

    let spawned = AssertUnwindSafe(UciEngine::spawn_until(
        engine_config,
        limits.engine_timeout,
        deadline,
    ))
    .catch_unwind()
    .await;
    let mut engine = match spawned {
        Ok(Ok(engine)) => engine,
        Ok(Err(failure)) => {
            let component_host_id_retained = control.read_state().is_ok();
            let application_shutdown = AssertUnwindSafe(white.shutdown()).catch_unwind().await;
            let application_shutdown_observed = match application_shutdown {
                Ok(result) => result.is_ok(),
                Err(payload) => resume_unwind(payload),
            };
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
                    application_shutdown_observed,
                    component_host_id_retained,
                    component_turn_executions: component_turn_executions.load(Ordering::SeqCst),
                },
                run_started_ms,
                provider_execute_count.load(Ordering::SeqCst),
                effective_provider,
                observer,
            )
            .await;
        }
        Err(operation_panic) => {
            let _application_shutdown = AssertUnwindSafe(white.shutdown()).catch_unwind().await;
            resume_unwind(operation_panic);
        }
    };

    let progress = Arc::new(Mutex::new(GameProgress::new()));
    let operation = AssertUnwindSafe(async {
        let game = play_game(
            &mut white,
            &control,
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
        (game_result, timeout_observation)
    })
    .catch_unwind()
    .await;

    let provider_execute_count = provider_execute_count.load(Ordering::SeqCst);
    let component_turn_executions = component_turn_executions.load(Ordering::SeqCst);
    let component_host_id_retained = control.read_state().is_ok();
    let engine_reads_bounded = engine.reads_are_bounded();
    let engine_shutdown = AssertUnwindSafe(engine.shutdown_until(deadline))
        .catch_unwind()
        .await;
    let uci_commands = engine.sent_commands().to_vec();
    let application_shutdown = AssertUnwindSafe(white.shutdown()).catch_unwind().await;

    let (game_result, timeout_observation) = match operation {
        Ok(operation) => operation,
        Err(payload) => resume_unwind(payload),
    };
    let engine_shutdown = match engine_shutdown {
        Ok(shutdown) => shutdown,
        Err(payload) => resume_unwind(payload),
    };
    let application_shutdown = match application_shutdown {
        Ok(shutdown) => shutdown,
        Err(payload) => resume_unwind(payload),
    };
    let child_shutdown_observed = matches!(
        engine_shutdown,
        Ok(ShutdownDisposition::Graceful | ShutdownDisposition::ForcedReap)
    );
    let application_shutdown_observed = application_shutdown.is_ok();
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
    let result = if !child_shutdown_observed || !application_shutdown_observed {
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
        application_shutdown_observed,
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
        component_turn_executions,
        effective_provider,
        provider_responses,
        component_host_id_retained,
        uci_commands,
        validated_black_moves: result.validated_black_moves,
        limits,
        engine_reads_bounded,
        child_shutdown_observed,
        application_shutdown_observed,
        trace_artifact_disposition: observer.trace_artifact_disposition(),
        status_output_complete: observer.status_output_complete(),
    })
}

struct CountingProvider<P> {
    inner: P,
    submit_count: Arc<AtomicUsize>,
}

impl<P> CountingProvider<P> {
    fn new(inner: P, submit_count: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            submit_count,
        }
    }
}

#[async_trait]
impl<P> ReactionPort for CountingProvider<P>
where
    P: ReactionPort,
{
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.inner.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.submit_count.fetch_add(1, Ordering::SeqCst);
        self.inner.submit(frame).await
    }
}

#[derive(Clone)]
struct BeforeRuntimeFailure {
    stage: InfrastructureStage,
    reason_code: InfrastructureAbortReason,
    child_shutdown_observed: bool,
    application_shutdown_observed: bool,
    component_host_id_retained: bool,
    component_turn_executions: usize,
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
    let outcome = if failure.child_shutdown_observed && failure.application_shutdown_observed {
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
        application_shutdown_observed: failure.application_shutdown_observed,
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
        component_turn_executions: failure.component_turn_executions,
        effective_provider,
        provider_responses,
        component_host_id_retained: failure.component_host_id_retained,
        uci_commands: Vec::new(),
        validated_black_moves: Vec::new(),
        limits,
        engine_reads_bounded: false,
        child_shutdown_observed: failure.child_shutdown_observed,
        application_shutdown_observed: failure.application_shutdown_observed,
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        num::{NonZeroU128, NonZeroU64},
        panic::AssertUnwindSafe,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc,
        },
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use agentview::component::execution::{
        ApplicationFaultCode, ApplicationFaultKind, ApplicationFaultReason, ApplicationFaultStage,
        Frame, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision, ProviderFact,
        ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultReason, SubmitFault, TargetDeclaration,
        TargetEpoch, TargetIdentity,
    };
    use async_trait::async_trait;
    use futures::FutureExt;

    use super::*;
    use crate::observability::{
        EffectiveProviderConfig, ObservationFailure, ProviderEndpointClass, RunEvent,
    };

    fn repeated_knight_history(plies: usize) -> (Board, Vec<ChessMove>) {
        let cycle = ["g1f3", "g8f6", "f3g1", "f6g8"]
            .map(|uci| uci.parse::<ChessMove>().expect("test move is valid UCI"));
        let mut board = Board::default();
        let mut history = Vec::with_capacity(plies);
        for candidate in cycle.into_iter().cycle().take(plies) {
            assert!(MoveGen::new_legal(&board).any(|legal| legal == candidate));
            board = board.make_move_new(candidate);
            history.push(candidate);
        }
        (board, history)
    }

    #[test]
    fn terminal_outcome_keeps_all_referee_owned_draws() {
        let stalemate = "7k/5K2/6Q1/8/8/8/8/8 b - - 0 1"
            .parse::<Board>()
            .expect("test stalemate FEN is valid");
        assert_eq!(
            terminal_outcome(&stalemate, &[]),
            Some(GameOutcome::Stalemate)
        );

        let dead = "7k/8/8/8/8/8/8/K7 w - - 0 1"
            .parse::<Board>()
            .expect("test dead-position FEN is valid");
        assert_eq!(
            terminal_outcome(&dead, &[]),
            Some(GameOutcome::AutomaticDraw {
                reason: AutomaticDrawReason::DeadPosition,
            })
        );

        let (fivefold_board, fivefold_history) = repeated_knight_history(16);
        assert_eq!(
            terminal_outcome(&fivefold_board, &fivefold_history),
            Some(GameOutcome::AutomaticDraw {
                reason: AutomaticDrawReason::FivefoldRepetition,
            })
        );

        let (seventy_five_board, seventy_five_history) = repeated_knight_history(150);
        assert_eq!(
            terminal_outcome(&seventy_five_board, &seventy_five_history),
            Some(GameOutcome::AutomaticDraw {
                reason: AutomaticDrawReason::SeventyFiveMoveRule,
            })
        );
    }

    struct SetupPort {
        identity: TargetIdentity,
        epoch: TargetEpoch,
        profile: FrameProfile,
        accepted: Option<FrameRevision>,
        submissions: Arc<AtomicUsize>,
    }

    impl SetupPort {
        fn new(submissions: Arc<AtomicUsize>) -> Self {
            Self {
                identity: TargetIdentity::new(
                    NonZeroU128::new((10_u128 << 64) | 1).expect("non-zero setup target identity"),
                ),
                epoch: TargetEpoch::new(NonZeroU64::MIN),
                profile: FrameProfile::new(
                    FrameConstraints {
                        max_frame_bytes: 1024 * 1024,
                        max_component_bytes: 256 * 1024,
                        context_window_tokens: None,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true),
                ),
                accepted: None,
                submissions,
            }
        }

        fn declaration(&self) -> TargetDeclaration {
            match self.accepted {
                Some(revision) => TargetDeclaration::resume(revision, self.profile.clone()),
                None => TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
            }
        }
    }

    #[async_trait]
    impl ReactionPort for SetupPort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            Ok(self.declaration())
        }

        async fn submit<'a>(
            &'a mut self,
            frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            frame.check_handoff_precondition(&self.declaration())?;
            self.accepted = Some(frame.revision());
            self.submissions.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::once(async {
                Ok(ProviderFact::ReactionCompleted { primary_text: None })
            })))
        }
    }

    #[derive(Default)]
    struct OfflineObserver {
        elapsed_ms: u64,
        events: Vec<RunEvent>,
        panic_on_model_reaction_started: bool,
    }

    impl GameObserver for OfflineObserver {
        fn now_ms(&mut self) -> u64 {
            self.elapsed_ms = self.elapsed_ms.saturating_add(1);
            self.elapsed_ms
        }

        fn record(&mut self, event: RunEvent) -> Result<(), ObservationFailure> {
            if self.panic_on_model_reaction_started
                && matches!(event, RunEvent::ModelReactionStarted { .. })
            {
                std::panic::panic_any(String::from("task-5 operation panic"));
            }
            self.events.push(event);
            Ok(())
        }

        fn trace_artifact_disposition(&self) -> Option<TraceArtifactDisposition> {
            None
        }

        fn status_output_complete(&self) -> bool {
            true
        }
    }

    const GAME_TEXT_OUTPUT: ProviderOutputKey = ProviderOutputKey::new(1);
    const GAME_TEST_TARGET_DOMAIN: u128 = 11_u128 << 64;
    static NEXT_GAME_TEST_TARGET: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Copy)]
    enum GameScript {
        Text(&'static str),
        PendingAfterHandoff,
    }

    struct ScriptedGamePort {
        identity: TargetIdentity,
        epoch: TargetEpoch,
        profile: FrameProfile,
        accepted: Option<FrameRevision>,
        scripts: VecDeque<GameScript>,
        submissions: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
        reject_before_handoff: Arc<AtomicBool>,
        panic_on_drop: bool,
    }

    impl ScriptedGamePort {
        fn new(
            scripts: impl IntoIterator<Item = GameScript>,
            submissions: Arc<AtomicUsize>,
            dropped: Arc<AtomicBool>,
            reject_before_handoff: Arc<AtomicBool>,
            panic_on_drop: bool,
        ) -> Self {
            let instance = NEXT_GAME_TEST_TARGET.fetch_add(1, Ordering::Relaxed);
            Self {
                identity: TargetIdentity::new(
                    NonZeroU128::new(GAME_TEST_TARGET_DOMAIN | u128::from(instance))
                        .expect("non-zero game target identity"),
                ),
                epoch: TargetEpoch::new(NonZeroU64::MIN),
                profile: FrameProfile::new(
                    FrameConstraints {
                        max_frame_bytes: 1024 * 1024,
                        max_component_bytes: 256 * 1024,
                        context_window_tokens: None,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true),
                ),
                accepted: None,
                scripts: scripts.into_iter().collect(),
                submissions,
                dropped,
                reject_before_handoff,
                panic_on_drop,
            }
        }

        fn declaration(&self) -> TargetDeclaration {
            match self.accepted {
                Some(revision) => TargetDeclaration::resume(revision, self.profile.clone()),
                None => TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
            }
        }
    }

    impl Drop for ScriptedGamePort {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
            if self.panic_on_drop {
                panic!("task-5 cleanup panic");
            }
        }
    }

    #[async_trait]
    impl ReactionPort for ScriptedGamePort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            Ok(self.declaration())
        }

        async fn submit<'a>(
            &'a mut self,
            frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            frame.check_handoff_precondition(&self.declaration())?;
            if self.reject_before_handoff.load(Ordering::SeqCst) {
                return Err(SubmitFault::Rejected(ReactionPortFault::retryable(
                    ReactionPortFaultCode::Unavailable,
                    ReactionPortFaultReason::Transport,
                )));
            }
            let script = self.scripts.front().copied().ok_or_else(|| {
                SubmitFault::Rejected(ReactionPortFault::retryable(
                    ReactionPortFaultCode::Unavailable,
                    ReactionPortFaultReason::Other,
                ))
            })?;
            let facts: ProviderFactStream<'a> = match script {
                GameScript::Text(text) => Box::pin(futures::stream::iter([
                    Ok(ProviderFact::TextDelta {
                        output: GAME_TEXT_OUTPUT,
                        phase: None,
                        delta: text.to_owned(),
                    }),
                    Ok(ProviderFact::TextSealed {
                        output: GAME_TEXT_OUTPUT,
                        phase: None,
                        text: text.to_owned(),
                    }),
                    Ok(ProviderFact::ReactionCompleted {
                        primary_text: Some(GAME_TEXT_OUTPUT),
                    }),
                ])),
                GameScript::PendingAfterHandoff => Box::pin(futures::stream::pending()),
            };

            self.scripts
                .pop_front()
                .expect("inspected scripted game response remains at the crossing poll");
            self.accepted = Some(frame.revision());
            self.submissions.fetch_add(1, Ordering::SeqCst);
            Ok(facts)
        }
    }

    #[cfg(unix)]
    struct FakeUciProcess {
        program: PathBuf,
        quit_marker: PathBuf,
    }

    #[cfg(unix)]
    impl FakeUciProcess {
        fn create() -> std::io::Result<Self> {
            static NEXT_FAKE_UCI: AtomicU64 = AtomicU64::new(1);
            let instance = NEXT_FAKE_UCI.fetch_add(1, Ordering::Relaxed);
            let base = format!("agentview-task-5-uci-{}-{instance}", std::process::id());
            let program = std::env::temp_dir().join(&base);
            let quit_marker = std::env::temp_dir().join(format!("{base}.quit"));
            let script = format!(
                "#!/bin/sh\nwhile IFS= read -r command; do\n  case \"$command\" in\n    uci) printf '%s\\n' uciok ;;\n    isready) printf '%s\\n' readyok ;;\n    go*) printf '%s\\n' 'bestmove e7e5' ;;\n    quit) printf '%s\\n' quit > '{}'; exit 0 ;;\n  esac\ndone\n",
                quit_marker.display()
            );
            fs::write(&program, script)?;
            let mut permissions = fs::metadata(&program)?.permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&program, permissions)?;
            Ok(Self {
                program,
                quit_marker,
            })
        }

        fn config(&self) -> UciProcessConfig {
            UciProcessConfig::stockfish(self.program.clone()).expect("fake UCI path is absolute")
        }

        fn quit_observed(&self) -> bool {
            self.quit_marker.is_file()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeUciProcess {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.program);
            let _ = fs::remove_file(&self.quit_marker);
        }
    }

    fn game_test_limits(reaction_timeout: Duration) -> GameLimits {
        GameLimits {
            reaction_timeout,
            engine_timeout: Duration::from_millis(250),
            whole_game_deadline: Duration::from_secs(2),
            ply_limit: 4,
            engine_nodes: 1,
        }
    }

    fn offline_effective_provider() -> EffectiveProviderConfig {
        EffectiveProviderConfig::new(
            "offline-script".to_owned(),
            ProviderEndpointClass::LoopbackHttp,
            "offline.invalid".to_owned(),
        )
    }

    fn assert_complete_owner_cleanup(evidence: &GameEvidence, dropped: &AtomicBool) {
        assert!(evidence.component_host_id_retained);
        assert!(evidence.child_shutdown_observed);
        assert!(evidence.application_shutdown_observed);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn uci_setup_failure_after_mount_consumes_application_without_submitting() {
        let submissions = Arc::new(AtomicUsize::new(0));
        let provider = SetupPort::new(Arc::clone(&submissions));
        let missing_uci = PathBuf::from(format!(
            "/__agentview_task_5_missing_uci_{}",
            std::process::id()
        ));
        let engine_config = UciProcessConfig::stockfish(missing_uci)
            .expect("absolute missing UCI path is valid configuration");
        let limits = GameLimits {
            reaction_timeout: Duration::from_millis(100),
            engine_timeout: Duration::from_millis(100),
            whole_game_deadline: Duration::from_secs(1),
            ply_limit: 2,
            engine_nodes: 1,
        };
        let effective_provider = EffectiveProviderConfig::new(
            "offline-script".to_owned(),
            ProviderEndpointClass::LoopbackHttp,
            "offline.invalid".to_owned(),
        );
        let mut observer = OfflineObserver::default();

        let evidence = run_provider_game_with_observer_until(
            provider,
            engine_config,
            limits,
            Instant::now() + limits.whole_game_deadline,
            effective_provider,
            &mut observer,
        )
        .await
        .expect("UCI setup failure produces terminal evidence");

        assert!(matches!(
            evidence.outcome,
            GameOutcome::InfrastructureAbort {
                stage: InfrastructureStage::Engine,
                reason_code: InfrastructureAbortReason::EngineFailure,
            }
        ));
        assert_eq!(submissions.load(Ordering::SeqCst), 0);
        assert_eq!(evidence.provider_execute_count, 0);
        assert_eq!(evidence.component_turn_executions, 0);
        assert!(evidence.component_host_id_retained);
        assert!(evidence.child_shutdown_observed);
        assert!(evidence.application_shutdown_observed);
        assert!(observer
            .events
            .iter()
            .any(|event| matches!(event, RunEvent::Terminal { .. })));
    }

    #[cfg(unix)]
    async fn run_completed_scripted_game(
        scripts: impl IntoIterator<Item = GameScript>,
        reaction_timeout: Duration,
    ) -> (GameEvidence, Arc<AtomicUsize>, Arc<AtomicBool>) {
        run_completed_scripted_game_with_rejection(scripts, reaction_timeout, false).await
    }

    #[cfg(unix)]
    async fn run_completed_scripted_game_with_rejection(
        scripts: impl IntoIterator<Item = GameScript>,
        reaction_timeout: Duration,
        reject_before_handoff: bool,
    ) -> (GameEvidence, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let fake_uci = FakeUciProcess::create().expect("fake UCI process installs");
        let submissions = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let reject_before_handoff = Arc::new(AtomicBool::new(reject_before_handoff));
        let provider = ScriptedGamePort::new(
            scripts,
            Arc::clone(&submissions),
            Arc::clone(&dropped),
            reject_before_handoff,
            false,
        );
        let limits = game_test_limits(reaction_timeout);
        let mut observer = OfflineObserver::default();
        let evidence = run_provider_game_with_observer_until(
            provider,
            fake_uci.config(),
            limits,
            Instant::now() + limits.whole_game_deadline,
            offline_effective_provider(),
            &mut observer,
        )
        .await
        .expect("offline scripted game produces evidence");
        assert!(fake_uci.quit_observed());
        (evidence, submissions, dropped)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_resignation_cleans_up_uci_and_application_owners() {
        let (evidence, submissions, dropped) = run_completed_scripted_game(
            [GameScript::Text("<resign />")],
            Duration::from_millis(100),
        )
        .await;

        assert!(matches!(
            evidence.outcome,
            GameOutcome::Resignation {
                resigned: Color::White,
                winner: Color::Black,
            }
        ));
        assert_eq!(submissions.load(Ordering::SeqCst), 1);
        assert_eq!(evidence.provider_execute_count, 1);
        assert_eq!(evidence.component_turn_executions, 1);
        assert_eq!(evidence.attempts.len(), 1);
        assert_eq!(evidence.uci_commands, ["uci", "isready", "quit"]);
        assert_complete_owner_cleanup(&evidence, &dropped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_game_preserves_legal_uci_turn_and_provider_call_counting() {
        let (evidence, submissions, dropped) = run_completed_scripted_game(
            [
                GameScript::Text("<choose_move uci=\"e2e4\" />"),
                GameScript::Text("<resign />"),
            ],
            Duration::from_millis(100),
        )
        .await;

        assert!(
            matches!(
                evidence.outcome,
                GameOutcome::Resignation {
                    resigned: Color::White,
                    winner: Color::Black,
                }
            ),
            "unexpected multi-turn outcome: {:?}",
            evidence.outcome
        );
        assert_eq!(
            evidence
                .accepted_moves
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["e2e4", "e7e5"]
        );
        assert_eq!(
            evidence
                .validated_black_moves
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["e7e5"]
        );
        assert_eq!(
            evidence.uci_commands,
            [
                "uci",
                "isready",
                "position startpos moves e2e4",
                "go nodes 1",
                "quit",
            ]
        );
        assert_eq!(submissions.load(Ordering::SeqCst), 2);
        assert_eq!(evidence.provider_execute_count, 2);
        assert_eq!(evidence.component_turn_executions, 2);
        assert_eq!(evidence.attempts.len(), 2);
        assert_complete_owner_cleanup(&evidence, &dropped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_rejection_after_mount_still_cleans_up_both_owners() {
        let (evidence, submissions, dropped) = run_completed_scripted_game_with_rejection(
            [GameScript::Text("<resign />")],
            Duration::from_millis(100),
            true,
        )
        .await;

        assert!(matches!(
            evidence.outcome,
            GameOutcome::InfrastructureAbort {
                stage: InfrastructureStage::ModelReaction,
                reason_code: InfrastructureAbortReason::Application {
                    stage: ApplicationFaultStage::Submit,
                    kind: ApplicationFaultKind::Retryable,
                    code: ApplicationFaultCode::Unavailable,
                    reason: ApplicationFaultReason::Port(ReactionPortFaultReason::Transport),
                },
            }
        ));
        assert_eq!(submissions.load(Ordering::SeqCst), 0);
        assert_eq!(evidence.provider_execute_count, 1);
        assert_eq!(evidence.component_turn_executions, 0);
        assert_eq!(evidence.attempts.len(), 1);
        assert_complete_owner_cleanup(&evidence, &dropped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_reaction_timeout_cleans_up_uci_and_terminal_application_owner() {
        let (evidence, submissions, dropped) = run_completed_scripted_game(
            [GameScript::PendingAfterHandoff],
            Duration::from_millis(25),
        )
        .await;

        assert!(matches!(
            evidence.outcome,
            GameOutcome::InfrastructureAbort {
                stage: InfrastructureStage::ModelReaction,
                reason_code: InfrastructureAbortReason::ReactionTimeout,
            }
        ));
        assert_eq!(submissions.load(Ordering::SeqCst), 1);
        assert_eq!(evidence.provider_execute_count, 1);
        assert_eq!(evidence.component_turn_executions, 0);
        assert_eq!(evidence.attempts.len(), 1);
        assert_complete_owner_cleanup(&evidence, &dropped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_retry_exhaustion_cleans_up_both_game_owners() {
        let (evidence, submissions, dropped) = run_completed_scripted_game(
            [
                GameScript::Text("<bogus />"),
                GameScript::Text("<bogus />"),
                GameScript::Text("<bogus />"),
            ],
            Duration::from_millis(100),
        )
        .await;

        assert!(matches!(
            evidence.outcome,
            GameOutcome::ModelForfeit {
                final_reason: InvalidActionReason::InvalidXml,
                attempts,
            } if attempts == crate::model::MAX_MODEL_ATTEMPTS
        ));
        assert_eq!(submissions.load(Ordering::SeqCst), 3);
        assert_eq!(evidence.provider_execute_count, 3);
        assert_eq!(evidence.component_turn_executions, 3);
        assert_eq!(evidence.attempts.len(), 3);
        assert_complete_owner_cleanup(&evidence, &dropped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn operation_panic_outranks_cleanup_panic_after_both_cleanup_attempts() {
        let fake_uci = FakeUciProcess::create().expect("fake UCI process installs");
        let submissions = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let provider = ScriptedGamePort::new(
            [GameScript::Text("<resign />")],
            Arc::clone(&submissions),
            Arc::clone(&dropped),
            Arc::new(AtomicBool::new(false)),
            true,
        );
        let limits = game_test_limits(Duration::from_millis(100));
        let mut observer = OfflineObserver {
            panic_on_model_reaction_started: true,
            ..OfflineObserver::default()
        };

        let panic = AssertUnwindSafe(run_provider_game_with_observer_until(
            provider,
            fake_uci.config(),
            limits,
            Instant::now() + limits.whole_game_deadline,
            offline_effective_provider(),
            &mut observer,
        ))
        .catch_unwind()
        .await
        .expect_err("operation panic must propagate after cleanup");

        assert_eq!(
            panic.downcast_ref::<String>().map(String::as_str),
            Some("task-5 operation panic")
        );
        assert_eq!(submissions.load(Ordering::SeqCst), 0);
        assert!(fake_uci.quit_observed());
        assert!(dropped.load(Ordering::Acquire));
    }
}
