use std::{path::PathBuf, time::Duration};

use agentview::component::{
    execution::{Application, ApplicationFault, ReactionPort},
    prelude::*,
};
use anyhow::Context as _;
use chess::{Board, ChessMove, Color, MoveGen};
use tokio::{sync::watch, time::Instant};

use super::{
    application_state::{
        reduce, ChessEffect, ChessEvent, ChessFeedback, ChessOutcome, ChessPhase, ChessState,
        ModelAttemptKey, StockfishFailure, StockfishRequest,
    },
    chess_action_component::{chess_action_component, ChessAttemptInput},
    chess_draw_state::DrawState,
    uci::{BestMoveError, UciEngine, UciProcessConfig},
};

const WORKFLOW_CAPACITY: usize = 4;

#[derive(Clone)]
pub(crate) struct ChessApplicationConfig {
    stockfish_program: PathBuf,
    engine_timeout: Duration,
    engine_nodes: u64,
    ply_limit: usize,
}

impl ChessApplicationConfig {
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
enum ChessActorFailure {
    #[error("Stockfish cleanup could not be confirmed")]
    StockfishCleanup,
}

#[derive(Clone, Debug)]
enum ChessActorExit {
    Completed(Result<ChessResult, ChessActorFailure>),
    Stopped(Result<(), ChessActorFailure>),
}

pub(crate) struct ChessApplication<P: ReactionPort> {
    runtime: Application<P>,
    stop: watch::Sender<bool>,
    actor_exit: watch::Receiver<Option<ChessActorExit>>,
}

impl<P: ReactionPort> ChessApplication<P> {
    pub(crate) fn mount(
        config: ChessApplicationConfig,
        provider: P,
    ) -> Result<Self, ApplicationFault> {
        let (stop, stop_receiver) = watch::channel(false);
        let (actor_exit, exit_receiver) = watch::channel(None);
        let root_config = config.clone();
        let root_stop = stop_receiver.clone();
        let root_actor_exit = actor_exit.clone();
        let runtime = Application::mount(
            move || {
                chess_application(
                    root_config.clone(),
                    root_stop.clone(),
                    root_actor_exit.clone(),
                )
            },
            provider,
        )?;
        Ok(Self {
            runtime,
            stop,
            actor_exit: exit_receiver,
        })
    }

    pub(crate) async fn run(mut self) -> anyhow::Result<ChessResult> {
        let result = drive_application(&mut self.runtime, &mut self.actor_exit).await;
        let actor_cleanup = if result.is_err() && self.actor_exit.borrow().is_none() {
            stop_actor(&self.stop, &mut self.actor_exit).await
        } else {
            Ok(())
        };
        let shutdown = self.runtime.shutdown().await;
        finish_run(result, actor_cleanup, shutdown)
    }
}

fn finish_run(
    result: anyhow::Result<ChessResult>,
    actor_cleanup: anyhow::Result<()>,
    application_shutdown: Result<(), ApplicationFault>,
) -> anyhow::Result<ChessResult> {
    match result {
        Ok(result) => {
            actor_cleanup.context("Chess actor cleanup failed")?;
            application_shutdown.context("Application shutdown failed")?;
            Ok(result)
        }
        Err(operation) => {
            if let Err(cleanup) = actor_cleanup {
                anyhow::bail!(
                    "Chess operation failed ({operation:#}); Chess actor cleanup also failed ({cleanup:#})"
                );
            }
            if let Err(shutdown) = application_shutdown {
                anyhow::bail!(
                    "Chess operation failed ({operation:#}); Application shutdown also failed ({shutdown})"
                );
            }
            Err(operation)
        }
    }
}

async fn stop_actor(
    stop: &watch::Sender<bool>,
    actor_exit: &mut watch::Receiver<Option<ChessActorExit>>,
) -> anyhow::Result<()> {
    stop.send_replace(true);
    loop {
        if let Some(exit) = actor_exit.borrow().clone() {
            return match exit {
                ChessActorExit::Completed(result) => result.map(|_| ()).map_err(Into::into),
                ChessActorExit::Stopped(result) => result.map_err(Into::into),
            };
        }
        actor_exit
            .changed()
            .await
            .context("Chess actor stopped without cleanup confirmation")?;
    }
}

async fn drive_application<P: ReactionPort>(
    runtime: &mut Application<P>,
    actor_exit: &mut watch::Receiver<Option<ChessActorExit>>,
) -> anyhow::Result<ChessResult> {
    loop {
        if let Some(exit) = actor_exit.borrow().clone() {
            return completed_result(exit);
        }

        tokio::select! {
            biased;
            changed = actor_exit.changed() => {
                changed.context("Chess workflow ended without a terminal result")?;
            }
            demand = runtime.wait_for_reaction_request() => {
                demand.context("Chess Component reaction request failed")?;
                // Complete an admitted reaction before observing completion. Cancelling
                // react() after provider handoff would terminally cancel Application.
                runtime.react().await.context("Chess model reaction failed")?;
            }
        }
    }
}

fn completed_result(exit: ChessActorExit) -> anyhow::Result<ChessResult> {
    match exit {
        ChessActorExit::Completed(result) => result.map_err(Into::into),
        ChessActorExit::Stopped(result) => {
            result?;
            anyhow::bail!("Chess actor stopped before producing a terminal result")
        }
    }
}

#[component]
fn chess_application(
    config: ChessApplicationConfig,
    stop: watch::Receiver<bool>,
    actor_exit: watch::Sender<Option<ChessActorExit>>,
) -> Component {
    let state = use_signal(|| ChessState::new(config.ply_limit));
    let reaction = use_reaction_request();

    let actor_state = state.clone();
    let actor_reaction = reaction.clone();
    let actor_config = config.clone();
    let actor_stop = stop.clone();
    let actor_exit = actor_exit.clone();
    let workflow: Coroutine<ChessAttemptInput> =
        use_coroutine(WORKFLOW_CAPACITY, move |inbox| async move {
            run_chess_actor(
                inbox,
                actor_state,
                actor_reaction,
                actor_config,
                actor_stop,
                actor_exit,
            )
            .await;
        });

    let rendered = state.with(Clone::clone).expect("mounted Chess state");
    let attempt = rendered.current_attempt();
    let action = attempt
        .map(|attempt| chess_action_component(attempt, workflow))
        .unwrap_or_else(|| view! {});

    view! {
        { chess_projection(rendered) }
        #[developer]
        { action }
    }
}

async fn run_chess_actor(
    mut inbox: CoroutineInbox<ChessAttemptInput>,
    state: Signal<ChessState>,
    reaction: ReactionRequest,
    config: ChessApplicationConfig,
    mut stop: watch::Receiver<bool>,
    actor_exit: watch::Sender<Option<ChessActorExit>>,
) {
    let process = match UciProcessConfig::stockfish(config.stockfish_program.clone()) {
        Ok(process) => process,
        Err(_) => {
            publish_without_engine(
                &state,
                ChessEvent::EngineFailed(StockfishFailure::Unavailable),
                &actor_exit,
            );
            return;
        }
    };
    let spawn_deadline = Instant::now() + config.engine_timeout;
    let mut engine =
        match UciEngine::spawn_until(process, config.engine_timeout, spawn_deadline).await {
            Ok(engine) => engine,
            Err(failure) => {
                if !failure.child_shutdown_observed() {
                    publish_actor_exit(
                        &actor_exit,
                        ChessActorExit::Completed(Err(ChessActorFailure::StockfishCleanup)),
                    );
                    return;
                }
                let reason = if failure.deadline_exhausted() {
                    StockfishFailure::TimedOut
                } else {
                    StockfishFailure::Unavailable
                };
                publish_without_engine(&state, ChessEvent::EngineFailed(reason), &actor_exit);
                return;
            }
        };

    let mut event = ChessEvent::Start;
    let mut handled = None;
    loop {
        let effects = reduce_signal(&state, event);
        match effects.as_slice() {
            [] => {}
            [ChessEffect::RequestReaction] => reaction
                .request()
                .expect("mounted Chess reaction capability"),
            [ChessEffect::RequestStockfish(request)] => {
                event = stockfish_event(&mut engine, request, config.engine_nodes).await;
                continue;
            }
            [ChessEffect::Complete(outcome)] => {
                let shutdown = shutdown_engine(&mut engine, config.engine_timeout).await;
                let exit = shutdown.map(|()| chess_result(&state, outcome.clone()));
                publish_actor_exit(&actor_exit, ChessActorExit::Completed(exit));
                acknowledge_action(&mut handled);
                drain_finished_events(&mut inbox, &state, &mut stop).await;
                return;
            }
            _ => panic!("Chess reducer emitted an unsupported effect batch"),
        }
        acknowledge_action(&mut handled);

        tokio::select! {
            biased;
            changed = stop.changed() => {
                let _ = changed;
                let exit = shutdown_engine(&mut engine, config.engine_timeout).await;
                publish_actor_exit(&actor_exit, ChessActorExit::Stopped(exit));
                return;
            }
            next = inbox.recv() => {
                let Some(ChessAttemptInput { attempt, result, handled: next_handled }) = next else {
                    let exit = shutdown_engine(&mut engine, config.engine_timeout).await;
                    publish_actor_exit(&actor_exit, ChessActorExit::Stopped(exit));
                    return;
                };
                event = ChessEvent::ModelAction { attempt, result };
                handled = Some(next_handled);
            }
        };
    }
}

async fn drain_finished_events(
    inbox: &mut CoroutineInbox<ChessAttemptInput>,
    state: &Signal<ChessState>,
    stop: &mut watch::Receiver<bool>,
) {
    while !*stop.borrow() {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                let _ = changed;
                return;
            }
            next = inbox.recv() => {
                let Some(ChessAttemptInput { attempt, result, handled }) = next else {
                    return;
                };
                debug_assert!(reduce_signal(
                    state,
                    ChessEvent::ModelAction { attempt, result },
                )
                .is_empty());
                let _ = handled.send(());
            }
        }
    }
}

fn acknowledge_action(handled: &mut Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(handled) = handled.take() {
        let _ = handled.send(());
    }
}

fn publish_without_engine(
    state: &Signal<ChessState>,
    event: ChessEvent,
    actor_exit: &watch::Sender<Option<ChessActorExit>>,
) {
    let effects = reduce_signal(state, event);
    let [ChessEffect::Complete(outcome)] = effects.as_slice() else {
        panic!("engine startup failure must complete the Chess reducer");
    };
    publish_actor_exit(
        actor_exit,
        ChessActorExit::Completed(Ok(chess_result(state, outcome.clone()))),
    );
}

fn reduce_signal(state: &Signal<ChessState>, event: ChessEvent) -> Vec<ChessEffect> {
    let (next, effects) = state
        .with(|current| {
            let mut next = current.clone();
            let effects = reduce(&mut next, event);
            (next, effects)
        })
        .expect("mounted Chess actor state");
    if !effects.is_empty() {
        state.set(next).expect("mounted Chess actor state");
    }
    effects
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

fn publish_actor_exit(actor_exit: &watch::Sender<Option<ChessActorExit>>, exit: ChessActorExit) {
    actor_exit.send_replace(Some(exit));
}

async fn stockfish_event(
    engine: &mut UciEngine,
    request: &StockfishRequest,
    nodes: u64,
) -> ChessEvent {
    debug_assert_eq!(
        request.position_revision,
        request.committed_moves.len(),
        "Stockfish request must describe one committed position revision"
    );
    let result = engine
        .best_move(&request.committed_moves, nodes)
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
        .unwrap_or(state.retry().attempts());
    let turn_id = state
        .current_attempt()
        .map(attempt_name)
        .unwrap_or_else(|| "none".to_owned());
    let FeedbackProjection {
        previous_decision,
        previous_action,
        previous_uci,
        previous_uci_truncated,
        previous_reason,
    } = feedback(state.feedback());
    let corrective_reason = state
        .retry()
        .last_rejection()
        .map(|reason| reason.code())
        .unwrap_or("none");

    view! {
        #[system_once]
        chess_player {
            identity { "You are the chess agent playing {agent_side}." }
            objective { "Choose one legal action for the authoritative position." }
            private_reasoning {
                "Reason privately about checks, captures, threats, king safety, tactics, strategy, and automatic draw conditions."
            }
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
        #[developer]
        chess_action_policy {
            chess_action_instructions {
                response {
                    "Your response must contain exactly one registered self-closing XML action element. Text outside that action element is allowed. The rule elements below are instructions, not valid output."
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
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::OpenOptionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::chess_action::{ChessAction, InvalidActionReason};
    use crate::scripted_provider::{ScriptedProvider, ScriptedReaction};
    use agentview::{
        component::{execution::FrameBasis, ComponentHost},
        pom_renderer::render_pom_document,
        transcript::CanonicalInputItem,
    };

    static NEXT_FAKE_UCI: AtomicU64 = AtomicU64::new(1);

    fn render_chess_prompt(state: ChessState) -> String {
        let mut components = ComponentHost::new_root(chess_projection, state);
        let rendered = components.render().expect("Chess projection renders");
        rendered
            .projection()
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => {
                    Some(render_pom_document(pom).expect("Chess POM renders"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn state_after_committed_moves(moves: &[&str]) -> ChessState {
        let mut state = ChessState::new(moves.len() + 1);
        assert_eq!(
            reduce(&mut state, ChessEvent::Start),
            vec![ChessEffect::RequestReaction]
        );

        for uci in moves {
            let candidate = uci.parse().expect("test move is valid UCI");
            let event = match state.board().side_to_move() {
                Color::White => ChessEvent::ModelAction {
                    attempt: state.current_attempt().expect("model turn is active"),
                    result: Ok(ChessAction::ChooseMove(candidate)),
                },
                Color::Black => ChessEvent::StockfishCompleted(Ok(candidate)),
            };
            reduce(&mut state, event);
        }

        state
    }

    struct FakeUciProgram {
        program: PathBuf,
        transcript: PathBuf,
    }

    impl FakeUciProgram {
        fn create() -> Self {
            let sequence = NEXT_FAKE_UCI.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_exe()
                .expect("resolve Cargo example test executable")
                .parent()
                .expect("Cargo example test executable has a parent")
                .join(format!(
                    "agentview-chess-application-{}-{sequence}.sh",
                    std::process::id()
                ));
            let mut transcript = path.as_os_str().to_os_string();
            transcript.push(".commands");
            let transcript = PathBuf::from(transcript);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&path)
                .expect("create fake UCI program");
            file.write_all(
                br#"#!/bin/sh
transcript="${0}.commands"
: > "$transcript"
go_count=0
while IFS= read -r command; do
    printf '%s\n' "$command" >> "$transcript"
    case "$command" in
        uci) printf 'uciok\n' ;;
        isready) printf 'readyok\n' ;;
        "go nodes 10")
            go_count=$((go_count + 1))
            if [ "$go_count" -eq 1 ]; then
                printf 'bestmove e7e5\n'
            else
                printf 'bestmove g8f6\n'
            fi
            ;;
        quit) exit 0 ;;
    esac
done
"#,
            )
            .expect("write fake UCI program");
            file.flush().expect("flush fake UCI program");
            Self {
                program: path,
                transcript,
            }
        }

        fn path(&self) -> PathBuf {
            self.program.clone()
        }

        fn commands(&self) -> Vec<String> {
            fs::read_to_string(&self.transcript)
                .expect("read fake UCI transcript")
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for FakeUciProgram {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.program);
            let _ = fs::remove_file(&self.transcript);
        }
    }

    #[tokio::test]
    async fn component_actor_owns_the_full_model_and_stockfish_turn() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::text(["<choose_move uci=\"e2e4\" />"]),
            ScriptedReaction::text(["<choose_move uci=\"d2d4\" />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();

        let result = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        assert_eq!(
            result.committed_moves,
            vec![
                "e2e4".parse().unwrap(),
                "e7e5".parse().unwrap(),
                "d2d4".parse().unwrap(),
                "g8f6".parse().unwrap(),
            ]
        );
        let frames = capture.frames();
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0].basis, FrameBasis::Full));
        assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
        assert!(frames[0]
            .text
            .contains("fen>rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"));
        assert!(frames[1]
            .text
            .contains("fen>rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2"));
        assert_eq!(
            frames[1]
                .text
                .matches(
                    "Your response must contain exactly one registered self-closing XML action element. Text outside that action element is allowed. The rule elements below are instructions, not valid output."
                )
                .count(),
            1,
            "a delta repeats the policy without relying on stable action examples"
        );
        assert!(!frames[1].text.contains("shown after this policy"));
        assert!(frames[1].text.contains("d2d4"));
        for action_syntax in ["<choose_move uci=\"...\" />", "<resign />"] {
            assert_eq!(
                frames[0].text.matches(action_syntax).count(),
                1,
                "typed action syntax should be projected exactly once: {action_syntax}"
            );
        }
        for removed_action_syntax in [
            "<move_and_offer_draw uci=\"...\" />",
            "<accept_draw />",
            "<claim_draw />",
        ] {
            assert!(!frames[0].text.contains(removed_action_syntax));
        }
        assert!(frames[0].text.contains("<chess_action_policy>"));
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[test]
    fn chess_prompt_policy_allows_text_without_stable_action_examples() {
        let prompt = render_chess_prompt(ChessState::new(1));

        assert_eq!(prompt.matches("<chess_action_instructions>").count(), 1);
        assert!(!prompt.contains("Return no analysis or commentary."));
        assert_eq!(
            prompt
                .matches(
                    "Your response must contain exactly one registered self-closing XML action element. Text outside that action element is allowed. The rule elements below are instructions, not valid output."
                )
                .count(),
            1
        );
        assert!(!prompt.contains("shown after this policy"));
        assert!(!prompt.contains("Output no other text."));
        for action_name in ["choose_move", "resign"] {
            let action = format!("<action_rule output_element=\"{action_name}\">");
            assert_eq!(
                prompt.matches(&action).count(),
                1,
                "each XML action should have one prompt rule naming its output element: {action_name}"
            );
        }
        for action_purpose in ["Play one legal move.", "Concede the game immediately."] {
            assert_eq!(
                prompt.matches(action_purpose).count(),
                1,
                "each XML action should explain its purpose exactly once: {action_purpose}"
            );
        }
        assert_eq!(
            prompt
                .matches("<source xml_path=\"/chess_game_state/legal_moves/@values\" />")
                .count(),
            1,
            "the move action should identify the exact legal-move prompt field"
        );
        assert_eq!(
            prompt
                .matches(
                    "Choose exactly one space-delimited canonical lowercase UCI token from this XML attribute and copy it unchanged into the action's uci attribute."
                )
                .count(),
            1,
            "the move action should explain how the XML source maps to the output attribute"
        );
        for removed_draw_contract in [
            "move_and_offer_draw",
            "accept_draw",
            "claim_draw",
            "pending_draw_offer",
            "claim_draw_basis",
            "offering a draw",
        ] {
            assert!(
                !prompt.contains(removed_draw_contract),
                "the prompt must not advertise voluntary draw behavior: {removed_draw_contract}"
            );
        }
    }

    #[test]
    fn chess_prompt_fen_uses_history_derived_halfmove_and_fullmove_counters() {
        let prompt = render_chess_prompt(state_after_committed_moves(&[
            "e2e4", "e7e5", "g1f3", "b8c6",
        ]));

        assert!(prompt.contains(
            "<fen>r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3</fen>"
        ));
    }

    #[test]
    fn chess_prompt_renders_code_like_retry_values_as_unescaped_attributes() {
        let mut state = state_after_committed_moves(&[]);
        let rejected = ChessEvent::ModelAction {
            attempt: state.current_attempt().expect("model turn is active"),
            result: Err(InvalidActionReason::MultipleActions),
        };
        assert_eq!(
            reduce(&mut state, rejected),
            vec![ChessEffect::RequestReaction]
        );

        let prompt = render_chess_prompt(state);

        assert!(prompt.contains("<chess_game_state phase=\"awaiting_model\""));
        assert!(prompt.contains("<model_attempt previous_decision=\"rejected:multiple_actions\""));
        assert!(prompt.contains("corrective_reason=\"multiple_actions\""));
        assert!(!prompt.contains("awaiting\\_model"));
        assert!(!prompt.contains("multiple\\_actions"));
    }

    #[tokio::test]
    async fn text_outside_one_registered_action_is_allowed() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([ScriptedReaction::text([
            "I will play the king's pawn. ",
            "<choose_move uci=\"e2e4\" />",
            " Your turn.",
        ])])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
        assert_eq!(capture.frames().len(), 1);
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[tokio::test]
    async fn invalid_action_requests_a_fresh_model_attempt() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::text(["<choose_move uci=\"E2E4\" />"]),
            ScriptedReaction::text(["<choose_move uci=\"e2e4\" />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
        let frames = capture.frames();
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0].basis, FrameBasis::Full));
        assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
        assert!(
            frames[1].text.contains(
                "<previous_action kind=\"choose_move\" uci=\"E2E4\" uci_truncated=\"false\" />"
            ),
            "{}",
            frames[1].text
        );
        assert!(
            !frames[1].text.contains("choose\\_move"),
            "{}",
            frames[1].text
        );
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[tokio::test]
    async fn oversized_invalid_uci_is_bounded_in_retry_feedback() {
        let fake_uci = FakeUciProgram::create();
        let oversized = "x".repeat(256);
        let invalid_action = format!("<choose_move uci=\"{oversized}\" />");
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::text([invalid_action]),
            ScriptedReaction::text(["<choose_move uci=\"e2e4\" />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        let retry = &capture.frames()[1].text;
        assert!(
            retry.contains("<previous_action kind=\"choose_move\""),
            "{retry}"
        );
        assert!(retry.contains("uci=\"xxxxxxxx"), "{retry}");
        assert!(retry.contains("uci_truncated=\"true\""), "{retry}");
        assert!(!retry.contains(&oversized), "{retry}");
    }

    #[tokio::test]
    async fn missing_action_is_rejected_and_requests_a_fresh_model_attempt() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::empty(),
            ScriptedReaction::text(["<choose_move uci=\"e2e4\" />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            ChessApplication::mount(config, provider).unwrap().run(),
        )
        .await
        .expect("a missing action must request another model reaction")
        .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
        let frames = capture.frames();
        assert_eq!(frames.len(), 2);
        assert!(
            frames[1]
                .text
                .contains("previous_decision=\"rejected:missing_action\""),
            "{}",
            frames[1].text
        );
        assert!(
            frames[1]
                .text
                .contains("corrective_reason=\"missing_action\""),
            "{}",
            frames[1].text
        );
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[tokio::test]
    async fn incomplete_action_is_rejected_after_eof_before_completion_settlement() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::text(["<choose_move uci=\"e2e4\""]),
            ScriptedReaction::text(["<choose_move uci=\"e2e4\" />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
        let frames = capture.frames();
        assert_eq!(frames.len(), 2);
        assert!(frames[1]
            .text
            .contains("previous_decision=\"rejected:invalid_xml\""));
        assert!(!frames[1]
            .text
            .contains("previous_decision=\"rejected:missing_action\""));
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[tokio::test]
    async fn multiple_model_actions_are_rejected_as_one_attempt() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::text([concat!("<resign />", "<choose_move uci=\"d2d4\" />",)]),
            ScriptedReaction::text(["<choose_move uci=\"e2e4\" />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap();

        assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
        assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
        let frames = capture.frames();
        assert_eq!(frames.len(), 2);
        assert!(frames[1]
            .text
            .contains("previous_decision=\"rejected:multiple_actions\""));
        assert!(frames[1]
            .text
            .contains("corrective_reason=\"multiple_actions\""));
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[tokio::test]
    async fn three_reactions_without_registered_actions_forfeit_the_model() {
        let fake_uci = FakeUciProgram::create();
        let (provider, capture) = ScriptedProvider::new([
            ScriptedReaction::empty(),
            ScriptedReaction::text(["I cannot choose a move."]),
            ScriptedReaction::text(["<unknown />"]),
        ])
        .unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            ChessApplication::mount(config, provider).unwrap().run(),
        )
        .await
        .expect("missing actions must exhaust the bounded retry policy")
        .unwrap();

        assert_eq!(
            result.outcome,
            ChessOutcome::ModelForfeit {
                final_reason: InvalidActionReason::MissingAction,
                attempts: 3,
            }
        );
        assert!(result.committed_moves.is_empty());
        let frames = capture.frames();
        assert_eq!(frames.len(), 3);
        assert!(frames[1]
            .text
            .contains("previous_decision=\"rejected:missing_action\""));
        assert!(frames[2]
            .text
            .contains("previous_decision=\"rejected:missing_action\""));
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }

    #[tokio::test]
    async fn provider_failure_stops_and_reaps_component_owned_stockfish() {
        let fake_uci = FakeUciProgram::create();
        let (provider, _) = ScriptedProvider::new([]).unwrap();
        let config =
            ChessApplicationConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();

        let error = ChessApplication::mount(config, provider)
            .unwrap()
            .run()
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Chess model reaction failed"));
        assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
    }
}
