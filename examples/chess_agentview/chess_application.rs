use std::{path::PathBuf, time::Duration};

use agentview::component::{execution::ExitReason, prelude::*};
use anyhow::Context as _;
use chess::{Board, ChessMove, Color, MoveGen};
use tokio::{
    sync::{oneshot, watch},
    time::Instant,
};

use super::{
    application_state::{
        reduce, ChessEvent, ChessFeedback, ChessOutcome, ChessPhase, ChessReduction, ChessState,
        ModelAttemptKey, StockfishFailure,
    },
    chess_action_component::chess_action_component,
    chess_draw_state::DrawState,
    uci::{BestMoveError, UciEngine, UciProcessConfig},
};

const PREPARATION_CAPACITY: usize = 1;

#[derive(Clone)]
pub(crate) struct ChessConfig {
    stockfish_program: PathBuf,
    engine_timeout: Duration,
    engine_nodes: u64,
    ply_limit: usize,
}

impl ChessConfig {
    pub(crate) fn new(
        stockfish_program: PathBuf,
        engine_timeout: Duration,
        engine_nodes: u64,
        ply_limit: usize,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            stockfish_program.is_absolute(),
            "Stockfish path must be absolute"
        );
        anyhow::ensure!(!engine_timeout.is_zero(), "engine timeout must be positive");
        anyhow::ensure!(engine_nodes > 0, "engine nodes must be positive");
        anyhow::ensure!(ply_limit > 0, "ply limit must be positive");
        Ok(Self {
            stockfish_program,
            engine_timeout,
            engine_nodes,
            ply_limit,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChessResult {
    pub(crate) outcome: ChessOutcome,
    pub(crate) final_board: Board,
    pub(crate) committed_moves: Vec<ChessMove>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ChessActorFailure {
    #[error("Stockfish cleanup could not be confirmed")]
    StockfishCleanup,
}

struct ChessPreparationRequest {
    ready: oneshot::Sender<()>,
}

pub(crate) async fn stop_stockfish(
    stop: &watch::Sender<bool>,
    cleanup: &mut watch::Receiver<Option<Result<(), ChessActorFailure>>>,
) -> anyhow::Result<()> {
    stop.send_replace(true);
    loop {
        if let Some(result) = *cleanup.borrow() {
            return result.map_err(Into::into);
        }
        cleanup
            .changed()
            .await
            .context("Chess actor stopped without cleanup confirmation")?;
    }
}

#[component]
pub(crate) fn chess_application(
    config: ChessConfig,
    stop: watch::Receiver<bool>,
    result: watch::Sender<Option<ChessResult>>,
    cleanup: watch::Sender<Option<Result<(), ChessActorFailure>>>,
) -> Component {
    let state = use_signal(|| ChessState::new(config.ply_limit));
    let application_exit = use_application_exit();

    let actor_state = state.clone();
    let actor_config = config.clone();
    let actor_stop = stop.clone();
    let engine_result = result.clone();
    let engine_cleanup = cleanup.clone();
    let engine: Coroutine<ChessPreparationRequest> =
        use_coroutine(PREPARATION_CAPACITY, move |inbox| async move {
            run_stockfish_actor(
                inbox,
                actor_state,
                actor_config,
                actor_stop,
                engine_result,
                engine_cleanup,
            )
            .await;
        });

    let preparation_state = state.clone();
    use_preparation(move || {
        let engine = engine.clone();
        let state = preparation_state.clone();
        let result = result.clone();
        let cleanup = cleanup.clone();
        let application_exit = application_exit.clone();
        async move {
            if request_completed_exit(&cleanup, &result, &application_exit)? {
                return Ok::<_, anyhow::Error>(());
            }
            let (ready, prepared) = oneshot::channel();
            let preparation = async move {
                engine
                    .send(ChessPreparationRequest { ready })
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "Chess actor rejected preparation without terminal completion"
                        )
                    })?;
                prepared
                    .await
                    .context("Chess actor stopped before completing preparation")
            }
            .await;
            if request_completed_exit(&cleanup, &result, &application_exit)? {
                return Ok(());
            }
            preparation?;
            let phase = state.with(ChessState::phase)?;
            anyhow::ensure!(
                phase == ChessPhase::AwaitingModel,
                "Chess preparation completed in unexpected phase {phase:?}"
            );
            Ok(())
        }
    });

    let rendered = state.with(Clone::clone).expect("mounted Chess state");
    let attempt = rendered.current_attempt();
    let action = attempt
        .map(|attempt| chess_action_component(attempt, state.clone()))
        .unwrap_or_else(|| view! {});

    view! {
        { chess_projection(rendered) }
        #[developer]
        { action }
    }
}

fn request_completed_exit(
    cleanup: &watch::Sender<Option<Result<(), ChessActorFailure>>>,
    result: &watch::Sender<Option<ChessResult>>,
    application_exit: &ApplicationExitHandle,
) -> anyhow::Result<bool> {
    match *cleanup.borrow() {
        Some(Ok(())) => {
            anyhow::ensure!(
                result.borrow().is_some(),
                "Chess actor stopped before producing a terminal result"
            );
            application_exit
                .request(ExitReason::Completed)
                .context("Chess actor completed but application exit could not be requested")?;
            Ok(true)
        }
        Some(Err(error)) => {
            anyhow::bail!("Chess actor completed with an error during preparation: {error}");
        }
        None => Ok(false),
    }
}

async fn run_stockfish_actor(
    mut inbox: CoroutineInbox<ChessPreparationRequest>,
    state: Signal<ChessState>,
    config: ChessConfig,
    mut stop: watch::Receiver<bool>,
    result: watch::Sender<Option<ChessResult>>,
    cleanup: watch::Sender<Option<Result<(), ChessActorFailure>>>,
) {
    let process = match UciProcessConfig::stockfish(config.stockfish_program.clone()) {
        Ok(process) => process,
        Err(_) => {
            publish_without_engine(
                &state,
                ChessEvent::EngineFailed(StockfishFailure::Unavailable),
                &result,
            );
            cleanup.send_replace(Some(Ok(())));
            return;
        }
    };
    let spawn_deadline = Instant::now() + config.engine_timeout;
    let mut engine =
        match UciEngine::spawn_until(process, config.engine_timeout, spawn_deadline).await {
            Ok(engine) => engine,
            Err(failure) => {
                if !failure.child_shutdown_observed() {
                    cleanup.send_replace(Some(Err(ChessActorFailure::StockfishCleanup)));
                    return;
                }
                let reason = if failure.deadline_exhausted() {
                    StockfishFailure::TimedOut
                } else {
                    StockfishFailure::Unavailable
                };
                publish_without_engine(&state, ChessEvent::EngineFailed(reason), &result);
                cleanup.send_replace(Some(Ok(())));
                return;
            }
        };

    loop {
        let request = tokio::select! {
            biased;
            _ = stop.changed() => None,
            next = inbox.recv() => next,
        };
        let Some(request) = request else {
            let shutdown = shutdown_engine(&mut engine, config.engine_timeout).await;
            cleanup.send_replace(Some(shutdown));
            return;
        };

        if state.with(ChessState::phase).expect("mounted Chess state") == ChessPhase::Ready {
            reduce_signal(&state, ChessEvent::Start);
        }
        let stockfish_history = state
            .with(|state| {
                (state.phase() == ChessPhase::AwaitingStockfish)
                    .then(|| state.committed_moves().to_vec())
            })
            .expect("mounted Chess state");
        if let Some(history) = stockfish_history {
            let event = stockfish_event(&mut engine, &history, config.engine_nodes).await;
            reduce_signal(&state, event);
        }
        let outcome = state
            .with(|state| state.outcome().cloned())
            .expect("mounted Chess state");
        if let Some(outcome) = outcome {
            let shutdown = shutdown_engine(&mut engine, config.engine_timeout).await;
            if shutdown.is_ok() {
                result.send_replace(Some(chess_result(&state, outcome)));
            }
            cleanup.send_replace(Some(shutdown));
            let _ = request.ready.send(());
            return;
        }
        let _ = request.ready.send(());
    }
}

fn publish_without_engine(
    state: &Signal<ChessState>,
    event: ChessEvent,
    result: &watch::Sender<Option<ChessResult>>,
) {
    assert_eq!(reduce_signal(state, event), ChessReduction::Applied);
    let outcome = state
        .with(|state| state.outcome().cloned())
        .expect("mounted Chess state")
        .expect("engine startup failure must complete the Chess reducer");
    result.send_replace(Some(chess_result(state, outcome)));
}

pub(crate) fn reduce_signal(state: &Signal<ChessState>, event: ChessEvent) -> ChessReduction {
    state
        .update(|current| reduce(current, event))
        .expect("mounted Chess state")
}

fn chess_result(state: &Signal<ChessState>, outcome: ChessOutcome) -> ChessResult {
    let (state_outcome, final_board, committed_moves) = state
        .with(|current| {
            (
                current.outcome().cloned(),
                current.board(),
                current.committed_moves().to_vec(),
            )
        })
        .expect("mounted Chess actor state");
    debug_assert_eq!(state_outcome.as_ref(), Some(&outcome));
    ChessResult {
        outcome,
        final_board,
        committed_moves,
    }
}

async fn shutdown_engine(
    engine: &mut UciEngine,
    timeout: Duration,
) -> Result<(), ChessActorFailure> {
    engine
        .shutdown_until(Instant::now() + timeout)
        .await
        .map(|_| ())
        .map_err(|_| ChessActorFailure::StockfishCleanup)
}

async fn stockfish_event(
    engine: &mut UciEngine,
    committed_moves: &[ChessMove],
    nodes: u64,
) -> ChessEvent {
    let result = engine
        .best_move(committed_moves, nodes)
        .await
        .map_err(|failure| match failure {
            BestMoveError::Timeout => StockfishFailure::TimedOut,
            BestMoveError::Failure => StockfishFailure::Protocol,
        });
    ChessEvent::StockfishCompleted(result)
}

#[component]
fn chess_projection(state: ChessState) -> Component {
    let board = state.board();
    let phase = phase_name(state.phase());
    let side_to_move = side_name(board.side_to_move());
    let agent_side = side_name(state.agent_side());
    let fen = standard_fen(&board, state.committed_moves());
    let legal_moves = legal_moves(&board);
    let history = move_history(state.committed_moves());
    let attempt_index = state
        .current_attempt()
        .map(|attempt| attempt.attempt_index)
        .unwrap_or(state.retry_attempts());
    let turn_id = state
        .current_attempt()
        .map(attempt_name)
        .unwrap_or_else(|| "none".to_owned());
    let current_feedback = state.feedback();
    let corrective_reason = match &current_feedback {
        ChessFeedback::Rejected(reason) => reason.code(),
        ChessFeedback::Initial | ChessFeedback::Accepted(_) => "none",
    };
    let FeedbackProjection {
        previous_decision,
        previous_action,
        previous_uci,
        previous_uci_truncated,
        previous_reason,
    } = feedback(current_feedback);

    view! {
        #[system_once]
        chess_player {
            identity { "You are the chess agent playing {agent_side}." }
            objective { "Evaluate the authoritative position briefly, then choose one legal action." }
        }
        #[developer]
        chess_game_state {
            phase: phase,
            authority { "ChessState in the mounted Component is authoritative." }
            side_to_move { "{side_to_move}" }
            fen { "{fen}" }
            legal_moves { notation: "canonical_lowercase_uci", values: "{legal_moves}", }
            history { notation: "uci", values: "{history}", }
        }
        #[developer]
        model_attempt {
            previous_decision: previous_decision,
            corrective_reason: corrective_reason,
            turn_id { "{turn_id}" }
            attempt_index { "{attempt_index}" }
            previous_action {
                kind: previous_action,
                uci: previous_uci,
                uci_truncated: previous_uci_truncated,
            }
            previous_reason { "{previous_reason}" }
        }
        #[developer(repeat)]
        chess_action_policy {
            chess_action_instructions {
                response {
                    "Respond with exactly two XML elements and no other text: first one nonempty <thought>...</thought> with a concise move evaluation, then exactly one registered self-closing XML action element. The rule elements below are instructions, not valid output."
                }
                thought_rule {
                    output_element: "thought",
                    purpose { "State one concise move evaluation before the action." }
                    requirement { "Use nonempty plain text without nested XML." }
                }
                action_rule {
                    output_element: "choose_move",
                    purpose { "Play one legal move." }
                    attribute {
                        name: "uci",
                        source {
                            xml_path: "/chess_game_state/legal_moves/@values",
                        }
                        requirement {
                            "Choose exactly one space-delimited canonical lowercase UCI token from this XML attribute and copy it unchanged into the action's uci attribute."
                        }
                    }
                }
                action_rule {
                    output_element: "resign",
                    purpose { "Concede the game immediately." }
                }
            }
        }
    }
}

fn standard_fen(board: &Board, committed_moves: &[ChessMove]) -> String {
    let position = board
        .to_string()
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    let halfmove_clock = DrawState::from_history(committed_moves).halfmove_clock();
    let fullmove_number = committed_moves.len() / 2 + 1;
    format!("{position} {halfmove_clock} {fullmove_number}")
}

fn phase_name(phase: ChessPhase) -> &'static str {
    match phase {
        ChessPhase::Ready => "ready",
        ChessPhase::AwaitingModel => "awaiting_model",
        ChessPhase::AwaitingStockfish => "awaiting_stockfish",
        ChessPhase::Finished => "finished",
    }
}

fn side_name(side: Color) -> &'static str {
    match side {
        Color::White => "white",
        Color::Black => "black",
    }
}

fn attempt_name(attempt: ModelAttemptKey) -> String {
    format!("white-ply-{}", attempt.ply)
}

struct FeedbackProjection {
    previous_decision: String,
    previous_action: &'static str,
    previous_uci: String,
    previous_uci_truncated: bool,
    previous_reason: &'static str,
}

fn feedback(feedback: ChessFeedback) -> FeedbackProjection {
    match feedback {
        ChessFeedback::Initial => FeedbackProjection {
            previous_decision: "none".to_owned(),
            previous_action: "none",
            previous_uci: "none".to_owned(),
            previous_uci_truncated: false,
            previous_reason: "No previous model action.",
        },
        ChessFeedback::Accepted(action) => {
            let candidate = action
                .move_candidate()
                .map(|move_| move_.to_string())
                .unwrap_or_else(|| "none".to_owned());
            FeedbackProjection {
                previous_decision: format!("accepted:{}", action.kind().code()),
                previous_action: action.kind().code(),
                previous_uci: candidate,
                previous_uci_truncated: false,
                previous_reason: "The previous action was accepted.",
            }
        }
        ChessFeedback::Rejected(reason) => {
            let action = reason
                .action_kind()
                .map(|kind| kind.code())
                .unwrap_or("none");
            let (candidate, truncated) = match reason.rejected_uci() {
                Some((submitted, truncated)) => (submitted.to_owned(), truncated),
                None => (
                    reason
                        .rejected_action()
                        .and_then(|action| action.move_candidate())
                        .map(|move_| move_.to_string())
                        .unwrap_or_else(|| "none".to_owned()),
                    false,
                ),
            };
            FeedbackProjection {
                previous_decision: format!("rejected:{}", reason.code()),
                previous_action: action,
                previous_uci: candidate,
                previous_uci_truncated: truncated,
                previous_reason: reason.description(),
            }
        }
    }
}

fn legal_moves(board: &Board) -> String {
    let mut moves = MoveGen::new_legal(board)
        .map(|candidate| candidate.to_string())
        .collect::<Vec<_>>();
    moves.sort();
    moves.join(" ")
}

fn move_history(moves: &[ChessMove]) -> String {
    if moves.is_empty() {
        "none".to_owned()
    } else {
        moves
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(crate) fn outcome_name(outcome: &ChessOutcome) -> &'static str {
    match outcome {
        ChessOutcome::Checkmate { .. } => "checkmate",
        ChessOutcome::Stalemate => "stalemate",
        ChessOutcome::Resignation { .. } => "resignation",
        ChessOutcome::AutomaticDraw { .. } => "automatic_draw",
        ChessOutcome::ModelForfeit { .. } => "model_forfeit",
        ChessOutcome::StockfishFailed(_) => "stockfish_failed",
        ChessOutcome::PlyLimitReached => "ply_limit_reached",
    }
}

#[cfg(all(test, unix))]
#[path = "../../tests/examples/chess_agentview/chess_application.rs"]
mod tests;
