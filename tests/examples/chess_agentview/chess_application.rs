use std::{
    fs::{self, OpenOptions},
    io::Write,
    ops::ControlFlow,
    os::unix::fs::OpenOptionsExt,
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;
use crate::chess_action::{ChessAction, InvalidActionReason};
#[allow(dead_code)]
#[path = "../../support/scripted_provider.rs"]
mod scripted_provider;
use agentview::{
    component::{
        execution::{Application, FrameBasis, ReactionPort},
        ComponentHost,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use scripted_provider::{ScriptedProvider, ScriptedReaction};

static NEXT_FAKE_UCI: AtomicU64 = AtomicU64::new(1);

const BRIEF_THOUGHT: &str = "<thought>brief move evaluation</thought>";

fn assert_continue(flow: ControlFlow<ExitReason>) {
    assert_eq!(flow, ControlFlow::Continue(()));
}

fn thought_then_choose_move(uci: &str) -> String {
    format!("{BRIEF_THOUGHT}<choose_move uci=\"{uci}\" />")
}

fn thought_then_resign() -> String {
    format!("{BRIEF_THOUGHT}<resign />")
}

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
        ChessReduction::Applied
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
    first_go_hold: Option<PathBuf>,
    first_go_release: Option<PathBuf>,
}

impl FakeUciProgram {
    fn create() -> Self {
        Self::create_with_first_go_hold(false)
    }

    fn create_with_first_go_hold(hold_first_go: bool) -> Self {
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
        let first_go_hold = hold_first_go.then(|| {
            let mut hold = path.as_os_str().to_os_string();
            hold.push(".hold-first-go");
            PathBuf::from(hold)
        });
        let first_go_release = hold_first_go.then(|| {
            let mut release = path.as_os_str().to_os_string();
            release.push(".release-first-go");
            PathBuf::from(release)
        });
        if let Some(hold) = &first_go_hold {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(hold)
                .expect("create first Stockfish search hold");
        }
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
            if [ "$go_count" -eq 1 ] && [ -f "${0}.hold-first-go" ]; then
                while [ ! -f "${0}.release-first-go" ]; do
                    sleep 0.01
                done
            fi
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
            first_go_hold,
            first_go_release,
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

    fn release_first_go(&self) {
        let release = self
            .first_go_release
            .as_ref()
            .expect("fake UCI does not hold its first Stockfish search");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(release)
            .expect("release first Stockfish search");
    }
}

impl Drop for FakeUciProgram {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.program);
        let _ = fs::remove_file(&self.transcript);
        if let Some(hold) = &self.first_go_hold {
            let _ = fs::remove_file(hold);
        }
        if let Some(release) = &self.first_go_release {
            let _ = fs::remove_file(release);
        }
    }
}

async fn assert_thought_rejection(invalid_response: impl Into<String>, expected_reason: &str) {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text([invalid_response.into()]),
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), crate::play_chess(config, provider))
        .await
        .expect("a rejected thought must request another model reaction")
        .unwrap();

    assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
    assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    let retry = &frames[1].text;
    assert!(
        retry.contains(&format!("previous_decision=\"rejected:{expected_reason}\"")),
        "{retry}"
    );
    assert!(
        retry.contains(&format!("corrective_reason=\"{expected_reason}\"")),
        "{retry}"
    );
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}

fn stockfish_go_count(program: &FakeUciProgram) -> usize {
    program
        .commands()
        .iter()
        .filter(|command| command.as_str() == "go nodes 10")
        .count()
}

fn mount_chess<P: ReactionPort>(
    config: ChessConfig,
    provider: P,
) -> (
    Application<P>,
    watch::Sender<bool>,
    watch::Receiver<Option<Result<(), ChessActorFailure>>>,
) {
    let (stop, stop_receiver) = watch::channel(false);
    let (result, _) = watch::channel(None);
    let (cleanup, cleanup_receiver) = watch::channel(None);
    let application = Application::mount(
        move || {
            chess_application(
                config.clone(),
                stop_receiver.clone(),
                result.clone(),
                cleanup.clone(),
            )
        },
        provider,
    )
    .unwrap();
    (application, stop, cleanup_receiver)
}

fn current_chess_projection<P: ReactionPort>(application: &Application<P>) -> String {
    let snapshot = application.current_projection();
    snapshot
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

async fn shutdown_chess<P: ReactionPort>(
    application: Application<P>,
    stop: watch::Sender<bool>,
    mut cleanup: watch::Receiver<Option<Result<(), ChessActorFailure>>>,
) {
    stop_stockfish(&stop, &mut cleanup)
        .await
        .expect("stop raw Chess actor");
    application
        .shutdown()
        .await
        .expect("shutdown raw Chess reactor");
}

#[tokio::test]
async fn prepare_initializes_chess_without_submitting_a_provider_frame() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([]).unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();
    let (mut application, stop, cleanup) = mount_chess(config, provider);

    let flow = tokio::time::timeout(Duration::from_secs(1), application.prepare())
        .await
        .expect("initial Chess preparation must complete")
        .unwrap();
    assert_continue(flow);

    assert_eq!(capture.submission_count(), 0);
    assert!(fake_uci.commands().iter().any(|command| command == "uci"));
    assert_eq!(stockfish_go_count(&fake_uci), 0);

    shutdown_chess(application, stop, cleanup).await;
}

#[tokio::test]
async fn unavailable_engine_completes_without_submitting_a_provider_frame() {
    let sequence = NEXT_FAKE_UCI.fetch_add(1, Ordering::Relaxed);
    let missing_program = std::env::temp_dir().join(format!(
        "agentview-missing-stockfish-{}-{sequence}",
        std::process::id()
    ));
    assert!(!missing_program.exists(), "test path must not exist");
    let (provider, capture) = ScriptedProvider::new([]).unwrap();
    let config = ChessConfig::new(missing_program, Duration::from_secs(2), 10, 4).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), crate::play_chess(config, provider))
        .await
        .expect("unavailable engine must finish preparation")
        .unwrap();

    assert_eq!(
        result.outcome,
        ChessOutcome::StockfishFailed(StockfishFailure::Unavailable)
    );
    assert!(result.committed_moves.is_empty());
    assert_eq!(capture.submission_count(), 0);
}

#[tokio::test]
async fn move_publication_defers_stockfish_until_the_next_react_preparation() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
        ScriptedReaction::empty(),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();
    let (mut application, stop, cleanup) = mount_chess(config, provider);

    assert_continue(application.react().await.unwrap());

    assert_eq!(capture.submission_count(), 1);
    assert_eq!(stockfish_go_count(&fake_uci), 0);
    assert!(current_chess_projection(&application)
        .contains("<history notation=\"uci\" values=\"e2e4\" />"));

    assert_continue(application.react().await.unwrap());

    assert_eq!(capture.submission_count(), 2);
    assert_eq!(stockfish_go_count(&fake_uci), 1);
    assert!(current_chess_projection(&application)
        .contains("<history notation=\"uci\" values=\"e2e4 e7e5\" />"));

    shutdown_chess(application, stop, cleanup).await;
}

#[tokio::test]
async fn cancelled_react_preparation_does_not_duplicate_stockfish_work() {
    let fake_uci = FakeUciProgram::create_with_first_go_hold(true);
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
        ScriptedReaction::empty(),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();
    let (mut application, stop, cleanup) = mount_chess(config, provider);

    assert_continue(application.react().await.unwrap());

    let mut reaction = Box::pin(application.react());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            tokio::select! {
                result = &mut reaction => {
                    panic!("held Stockfish reaction completed unexpectedly: {result:?}");
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if stockfish_go_count(&fake_uci) == 1 {
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("react preparation must reach the held Stockfish search");
    drop(reaction);

    fake_uci.release_first_go();
    let flow = tokio::time::timeout(Duration::from_secs(1), application.react())
        .await
        .expect("a retried react must finish after the held search")
        .unwrap();
    assert_continue(flow);

    assert_eq!(capture.submission_count(), 2);
    assert_eq!(stockfish_go_count(&fake_uci), 1);

    shutdown_chess(application, stop, cleanup).await;
}

#[tokio::test]
async fn requested_exit_during_react_preparation_stops_the_actor_without_another_frame() {
    let fake_uci = FakeUciProgram::create_with_first_go_hold(true);
    let (provider, capture) =
        ScriptedProvider::new([ScriptedReaction::text([thought_then_choose_move("e2e4")])])
            .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();
    let (mut application, stop, cleanup) = mount_chess(config, provider);
    let exit = application.exit_handle();
    let mut run = Box::pin(async move {
        let result = application.run().await;
        shutdown_chess(application, stop, cleanup).await;
        result
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            tokio::select! {
                result = &mut run => {
                    panic!("Chess run ended before the held Stockfish preparation: {result:?}");
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if fake_uci.transcript.exists() && stockfish_go_count(&fake_uci) == 1 {
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("second react must reach the held Stockfish search");

    exit.request(ExitReason::Requested).unwrap();
    fake_uci.release_first_go();

    let reason = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("requested exit must finish after actor cleanup")
        .unwrap();

    assert_eq!(reason, ExitReason::Requested);
    assert_eq!(capture.submission_count(), 1);
    assert_eq!(stockfish_go_count(&fake_uci), 1);
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}

#[tokio::test]
async fn repeated_preparation_and_react_preparation_do_not_repeat_stockfish() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
        ScriptedReaction::text([thought_then_choose_move("d2d4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();
    let (mut application, stop, cleanup) = mount_chess(config, provider);

    assert_continue(application.react().await.unwrap());
    assert_continue(application.prepare().await.unwrap());
    assert_eq!(stockfish_go_count(&fake_uci), 1);

    assert_continue(application.prepare().await.unwrap());
    assert_eq!(stockfish_go_count(&fake_uci), 1);

    assert_continue(application.react().await.unwrap());
    assert_eq!(capture.submission_count(), 2);
    assert_eq!(stockfish_go_count(&fake_uci), 1);

    shutdown_chess(application, stop, cleanup).await;
}

#[tokio::test]
async fn stopping_before_terminal_preparation_does_not_report_completion() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) =
        ScriptedProvider::new([ScriptedReaction::text([thought_then_choose_move("e2e4")])])
            .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();
    let (mut application, stop, mut cleanup) = mount_chess(config, provider);

    assert_continue(application.react().await.unwrap());
    stop_stockfish(&stop, &mut cleanup).await.unwrap();

    let prepared = application.prepare().await;
    application.shutdown().await.unwrap();

    let error = prepared.expect_err("stopping the actor must not publish a terminal result");
    assert_eq!(
        error.stage(),
        agentview::component::execution::ApplicationFaultStage::Preparation
    );
    assert_eq!(capture.submission_count(), 1);
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}

#[tokio::test]
async fn terminal_preparation_does_not_submit_a_second_provider_reaction() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) =
        ScriptedProvider::new([ScriptedReaction::text([thought_then_choose_move("e2e4")])])
            .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 2).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), crate::play_chess(config, provider))
        .await
        .expect("terminal preparation must complete the Chess run")
        .unwrap();

    assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
    assert_eq!(
        result.committed_moves,
        vec!["e2e4".parse().unwrap(), "e7e5".parse().unwrap()]
    );
    assert_eq!(capture.submission_count(), 1);
    assert_eq!(stockfish_go_count(&fake_uci), 1);
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}

#[tokio::test]
async fn preparation_and_contract_drive_the_model_and_stockfish_turns() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
        ScriptedReaction::text([thought_then_choose_move("d2d4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

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
        frames[1].text.matches("<chess_action_policy>").count(),
        1,
        "developer(repeat) sends the current action policy once on every turn"
    );
    assert!(!frames[1].text.contains("<remove>"));
    assert!(!frames[1].text.contains("<chess_player>"));
    assert_eq!(
        frames[1].replay,
        [CanonicalInputItem::assistant_text(
            thought_then_choose_move("e2e4"),
            None,
        )],
        "the previous model reply is retained by the history path"
    );
    assert!(frames[1].projection.iter().all(|item| matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: agentview::transcript::InstructionAuthority::Developer,
            ..
        }
    )));
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
fn chess_prompt_fen_uses_history_derived_halfmove_and_fullmove_counters() {
    let prompt = render_chess_prompt(state_after_committed_moves(&[
        "e2e4", "e7e5", "g1f3", "b8c6",
    ]));

    assert!(prompt
        .contains("<fen>r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3</fen>"));
}

#[tokio::test]
async fn split_thought_then_move_is_accepted() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([ScriptedReaction::text([
        "<thought>brief ",
        "move evaluation</thought>",
        "<choose_move uci=\"e2e4\" />",
    ])])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

    assert_eq!(result.outcome, ChessOutcome::PlyLimitReached);
    assert_eq!(result.committed_moves, vec!["e2e4".parse().unwrap()]);
    assert_eq!(capture.frames().len(), 1);
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}

#[tokio::test]
async fn action_without_a_thought_is_rejected() {
    assert_thought_rejection("<choose_move uci=\"e2e4\" />", "missing_thought").await;
}

#[tokio::test]
async fn thought_after_an_action_is_rejected() {
    assert_thought_rejection(
        format!("<choose_move uci=\"e2e4\" />{BRIEF_THOUGHT}"),
        "thought_after_action",
    )
    .await;
}

#[tokio::test]
async fn duplicate_thoughts_are_rejected() {
    assert_thought_rejection(
        format!(
            "{BRIEF_THOUGHT}<thought>second move evaluation</thought><choose_move uci=\"e2e4\" />"
        ),
        "multiple_thoughts",
    )
    .await;
}

#[tokio::test]
async fn empty_thought_is_rejected() {
    assert_thought_rejection(
        "<thought></thought><choose_move uci=\"e2e4\" />",
        "invalid_thought",
    )
    .await;
}

#[tokio::test]
async fn malformed_thought_is_rejected() {
    assert_thought_rejection("<thought>brief move evaluation", "invalid_thought").await;
}

#[tokio::test]
async fn thought_then_resign_is_accepted() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) =
        ScriptedProvider::new([ScriptedReaction::text([thought_then_resign()])]).unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

    assert_eq!(
        result.outcome,
        ChessOutcome::Resignation {
            resigned: Color::White,
            winner: Color::Black,
        }
    );
    assert!(result.committed_moves.is_empty());
    assert_eq!(capture.frames().len(), 1);
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}

#[tokio::test]
async fn invalid_action_requests_a_fresh_model_attempt() {
    let fake_uci = FakeUciProgram::create();
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text([thought_then_choose_move("E2E4")]),
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

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
        ScriptedReaction::text([format!("{BRIEF_THOUGHT}{invalid_action}")]),
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

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
        ScriptedReaction::text([BRIEF_THOUGHT]),
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), crate::play_chess(config, provider))
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
        ScriptedReaction::text([format!("{BRIEF_THOUGHT}<choose_move uci=\"e2e4\"")]),
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

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
        ScriptedReaction::text([format!(
            "{BRIEF_THOUGHT}<resign /><choose_move uci=\"d2d4\" />"
        )]),
        ScriptedReaction::text([thought_then_choose_move("e2e4")]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = crate::play_chess(config, provider).await.unwrap();

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
        ScriptedReaction::text([BRIEF_THOUGHT]),
        ScriptedReaction::text([BRIEF_THOUGHT]),
        ScriptedReaction::text([BRIEF_THOUGHT]),
    ])
    .unwrap();
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 1).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), crate::play_chess(config, provider))
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
    let config = ChessConfig::new(fake_uci.path(), Duration::from_secs(2), 10, 4).unwrap();

    let error = crate::play_chess(config, provider).await.unwrap_err();

    assert!(error.to_string().contains("Chess model reaction failed"));
    assert_eq!(fake_uci.commands().last().map(String::as_str), Some("quit"));
}
