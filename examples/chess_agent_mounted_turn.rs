//! The next step after `hello_world`: a typed streaming chess move.
//!
//! `chess_agent` has the same System/User shape as Hello World. `choose_move`
//! adds an XML contract plus a synchronous, pure reducer. The example host
//! below owns the scripted provider and applies the emitted Live preview.
//! A preview and the final typed Output are move candidates only: legal-move
//! validation, authorization, domain mutation, and outbox delivery remain a
//! host action/commit responsibility.
//!
//! Run with:
//! `cargo run --example chess_agent_mounted_turn`

use agentview::component::prelude::*;
#[cfg(test)]
use chess_contract::ChessSystemPromptView;
use chess_contract::{
    ChessAction, ChessDiagnostic, ChessMoveContract, ChessSystemPolicyPromptView,
};
use support::run_streamed_turns;
#[cfg(test)]
use support::StreamingTraceEvent;

#[path = "support/mounted_prompt_trace.rs"]
mod support;

#[allow(dead_code)]
#[path = "chess/mod.rs"]
mod chess_contract;

#[derive(Clone)]
struct ChessTurnSnapshot {
    fen: String,
    task: String,
}

struct ChessChannels;

#[derive(Default)]
struct ChessMoveStreamState {
    selected: Option<ChessAction>,
    completed: Option<ChessAction>,
    invalid: bool,
    opened: usize,
}

impl TurnChannels for ChessChannels {
    type Output = ChessAction;
    // Live is an immediate, revocable preview. It is not a chess-state write.
    type Live = ChessAction;
    type Commit = Never;
    type Diagnostic = ChessDiagnostic;
}

#[derive(AgentView)]
#[agent_view(document)]
struct ChessUserDocument {
    #[view(paragraph)]
    fen: String,
    #[view(paragraph)]
    task: String,
}

/// A retained prompt contract plus an attempt-local streaming reducer.
///
/// The reducer is synchronous and receives no host service capability. Its
/// mutable state belongs only to this attempt, and it must stay deterministic
/// with respect to external/domain state. The host applies Live effects after
/// the reducer returns.
#[view(component)]
fn choose_move() -> DurableComponent<ChessChannels, ChessTurnSnapshot> {
    StreamingXml::<TurnEmission<ChessChannels>, ChessDiagnostic>::new(
        ChessMoveContract.build_root()?,
    )
    .state_with(ChessMoveStreamState::default)
    .on_open(|state, element| {
        state.opened += 1;
        if state.opened != 1 {
            state.invalid = true;
            return StreamUpdate::from_diagnostic(ChessDiagnostic::InvalidXmlEnvelope);
        }
        match ChessMoveContract.decode_element(element) {
            Ok(action) => {
                state.selected = Some(action.clone());
                StreamUpdate::from_emission(TurnEmission::Live(action))
            }
            Err(diagnostic) => {
                state.invalid = true;
                StreamUpdate::from_diagnostic(diagnostic)
            }
        }
    })
    .on_complete(|state, element| {
        if state.invalid || state.opened != 1 {
            return StreamUpdate::new();
        }
        match ChessMoveContract.decode_element(element) {
            Ok(action) if state.selected.as_ref() == Some(&action) => {
                state.completed = Some(action);
                StreamUpdate::new()
            }
            Ok(_) => {
                state.invalid = true;
                StreamUpdate::from_diagnostic(ChessDiagnostic::InvalidXmlEnvelope)
            }
            Err(diagnostic) => {
                state.invalid = true;
                StreamUpdate::from_diagnostic(diagnostic)
            }
        }
    })
    .on_finish(
        |state| match (state.invalid, state.opened, state.completed.clone()) {
            (false, 1, Some(action)) => StreamUpdate::from_emission(TurnEmission::Output(action)),
            _ => StreamUpdate::from_diagnostic(ChessDiagnostic::NoMoveSelected),
        },
    )
    // Stable identity for this retained contract and reducer factory. It
    // changes with the contract, never with an individual turn.
    .into_durable_component(RuntimeContract::new("example.chess.choose-move", "v2")?)
}

/// Combines retained System/runtime contributions with a fresh User POM.
#[view(component)]
fn chess_agent() -> MountedFeature<ChessChannels, ChessTurnSnapshot> {
    MountedFeature::try_new(
        durable_system((
            ChessSystemPolicyPromptView::default().build_root()?,
            choose_move(),
        )),
        |turn| {
            ChessUserDocument {
                fen: format!("Position (FEN): {}", turn.props().fen),
                task: format!("Task: {}", turn.props().task),
            }
            .build_root()
        },
    )
}

// Everything below is example-host input and observation, not component API.
fn scripted_turn() -> (ChessTurnSnapshot, Vec<String>) {
    (
        ChessTurnSnapshot {
            fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_owned(),
            task: "Choose White's best opening move.".to_owned(),
        },
        // The tag becomes complete in the second chunk. The host must apply
        // the emitted Live preview before it acknowledges that chunk.
        vec!["<move uci=\"e2".to_owned(), "e4\" />".to_owned()],
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Stable identity for the complete durable System/runtime contract.
    let epoch_contract = EpochContractId::new("example/chess-turn/v2")?;
    let harness = chess_agent().into_harness(epoch_contract);
    let trace = run_streamed_turns(harness, vec![scripted_turn()]).await?;

    println!("SYSTEM (attached once)\n{}", trace.system());
    for (index, user) in trace.users().iter().enumerate() {
        println!("\nUSER {}\n{}", index + 1, user);
    }
    for event in trace.events() {
        println!("STREAM {event:?}");
    }
    for selected in trace.live_effects() {
        println!("LIVE preview={}", selected.uci());
    }
    for record in trace.records() {
        for selected in record.outputs() {
            println!("OUTPUT move={}", selected.uci());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streams_live_before_ack_and_returns_typed_moves() -> anyhow::Result<()> {
        let trace = run_streamed_turns(
            chess_agent().into_harness(EpochContractId::new("example/chess-turn/test-v2")?),
            vec![scripted_turn()],
        )
        .await?;

        assert_eq!(trace.users().len(), 1);
        let expected_system = agentview::pom_renderer::render_pom_document(
            &agentview::pom_resolution::resolve_system_document(
                ChessSystemPromptView::default().build_root()?,
            ),
        )?;
        assert_eq!(trace.system(), expected_system);
        assert_eq!(
            trace
                .live_effects()
                .iter()
                .map(ChessAction::uci)
                .collect::<Vec<_>>(),
            ["e2e4"]
        );
        assert_eq!(
            trace
                .records()
                .iter()
                .flat_map(|record| record.outputs())
                .map(ChessAction::uci)
                .collect::<Vec<_>>(),
            ["e2e4"]
        );
        assert!(trace
            .records()
            .iter()
            .all(|record| record.diagnostics().is_empty()));
        assert_eq!(
            trace.events(),
            [
                StreamingTraceEvent::ChunkSubmitted("<move uci=\"e2".to_owned()),
                StreamingTraceEvent::ChunkAccepted("<move uci=\"e2".to_owned()),
                StreamingTraceEvent::ChunkSubmitted("e4\" />".to_owned()),
                StreamingTraceEvent::LiveApplied {
                    route: "xml:move".to_owned(),
                },
                StreamingTraceEvent::ChunkAccepted("e4\" />".to_owned()),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn streaming_reducer_uses_the_shared_typed_diagnostic() -> anyhow::Result<()> {
        let (turn, _) = scripted_turn();
        let trace = run_streamed_turns(
            chess_agent().into_harness(EpochContractId::new("example/chess-turn/invalid-v2")?),
            vec![(turn, vec!["<move uci=\"e2e9\" />".to_owned()])],
        )
        .await?;

        assert!(trace.live_effects().is_empty());
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.diagnostics())
            .any(|diagnostic| matches!(diagnostic, ChessDiagnostic::InvalidUci { .. })));
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.diagnostics())
            .any(|diagnostic| matches!(diagnostic, ChessDiagnostic::NoMoveSelected)));
        Ok(())
    }

    #[tokio::test]
    async fn streaming_completion_rechecks_the_shared_contract() -> anyhow::Result<()> {
        let (turn, _) = scripted_turn();
        let trace = run_streamed_turns(
            chess_agent().into_harness(EpochContractId::new("example/chess-turn/content-v2")?),
            vec![(turn, vec!["<move uci=\"e2e4\">prose</move>".to_owned()])],
        )
        .await?;

        assert_eq!(
            trace
                .live_effects()
                .iter()
                .map(ChessAction::uci)
                .collect::<Vec<_>>(),
            ["e2e4"]
        );
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.outputs())
            .next()
            .is_none());
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.diagnostics())
            .any(|diagnostic| matches!(diagnostic, ChessDiagnostic::UnexpectedContent)));
        Ok(())
    }

    #[tokio::test]
    async fn second_move_invalidates_terminal_output_but_keeps_the_first_live_preview(
    ) -> anyhow::Result<()> {
        let (turn, _) = scripted_turn();
        let trace = run_streamed_turns(
            chess_agent().into_harness(EpochContractId::new("example/chess-turn/multiple-v2")?),
            vec![(
                turn,
                vec!["<move uci=\"e2e4\"/><move uci=\"d2d4\"/>".to_owned()],
            )],
        )
        .await?;

        assert_eq!(
            trace
                .live_effects()
                .iter()
                .map(ChessAction::uci)
                .collect::<Vec<_>>(),
            ["e2e4"]
        );
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.outputs())
            .next()
            .is_none());
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.diagnostics())
            .any(|diagnostic| matches!(diagnostic, ChessDiagnostic::InvalidXmlEnvelope)));
        assert!(trace
            .records()
            .iter()
            .flat_map(|record| record.diagnostics())
            .any(|diagnostic| matches!(diagnostic, ChessDiagnostic::NoMoveSelected)));
        Ok(())
    }
}
