//! Mounted chess prompt lifecycle.
//!
//! Read `chess_agent` first. It reuses the chess POM from the compatibility
//! example but has the same authoring shape as `hello_world`: a durable System
//! POM and a fresh User POM from each captured board snapshot.
//!
//! This intentionally stops at prompt lifecycle. The compatibility
//! `chess_engine_agent` retains the full `observe -> act -> hook` loop until a
//! real transactional `MountedExternalPort` and consumer migration exist.
//!
//! Run with:
//! `cargo run --example chess_engine_mounted`

use agentview::{component::prelude::*, semantic_view::AgentViewCollect};
use chess_support::{chess_user_document, ChessGameSource, ChessSystemPromptView, ChessView};
use mounted_support::mount;

#[allow(dead_code)]
#[path = "chess/mod.rs"]
mod chess_support;
#[cfg_attr(not(test), allow(dead_code))]
#[path = "support/mounted_prompt_trace.rs"]
mod mounted_support;

#[derive(Debug, Clone)]
struct ChessTurnSnapshot {
    context: ChessView,
    task: String,
    turn_id: String,
}

#[view(component)]
fn chess_agent() -> PromptComponent<ChessTurnSnapshot> {
    try_prompt_component(
        ChessSystemPromptView::default(),
        |props: &ChessTurnSnapshot| {
            chess_user_document(
                props.context.clone(),
                props.task.clone(),
                props.turn_id.clone(),
            )
        },
    )
}

fn capture_turn(
    source: &ChessGameSource,
    task: impl Into<String>,
    turn_id: impl Into<String>,
) -> ChessTurnSnapshot {
    ChessTurnSnapshot {
        context: ChessView::collect(&source.snapshot()),
        task: task.into(),
        turn_id: turn_id.into(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = ChessGameSource::new();
    let mut mounted =
        mount(chess_agent().into_harness(EpochContractId::new("example/chess-prompt/v1")?)).await?;

    mounted
        .run_turn(capture_turn(
            &source,
            "Choose white's next move.",
            "chess-turn-1",
        ))
        .await?;

    // This stands in for the host's typed-result/action boundary. The next
    // call captures a new board snapshot; it does not rebuild or resend the
    // System POM.
    source.apply_legal_uci("e2e4")?;
    mounted
        .run_turn(capture_turn(
            &source,
            "Choose black's next move.",
            "chess-turn-2",
        ))
        .await?;

    let trace = mounted.trace()?;
    println!("SYSTEM attached once ({} bytes)", trace.system().len());
    for (index, user) in trace.users().iter().enumerate() {
        println!("USER {} rendered ({} bytes)", index + 1, user.len());
    }
    Ok(())
}

#[cfg(test)]
mod compatibility {
    use super::*;
    use agentview::prelude::*;
    use chess_support::legacy_agentview_app::ChessViewModel;
    use mounted_support::run_turns;

    async fn render_legacy(
        source: &ChessGameSource,
        turns: &[ChessTurnSnapshot],
    ) -> anyhow::Result<(String, Vec<String>)> {
        let view_model = ChessViewModel;
        let context = PromptContext::<Turn, ()>::without_system();
        let system_document = view_model.build_system_document(&context, source).await?;
        let system = render_pom_document(&resolve_system_document(system_document))?;
        let mut cursor = UserDocumentCursor::default();
        let mut users = Vec::with_capacity(turns.len());
        for turn in turns {
            let document = view_model
                .build_user_document(
                    &context,
                    &turn.turn_id,
                    turn.task.clone().into(),
                    &turn.context,
                )
                .await?;
            let (resolved, next_cursor) = resolve_user_document(document, &cursor)?;
            users.push(render_pom_document(&resolved)?);
            cursor = next_cursor;
        }
        Ok((system, users))
    }

    #[tokio::test]
    async fn mounted_prompt_trace_matches_legacy_chess_pom() -> anyhow::Result<()> {
        let source = ChessGameSource::new();
        let first = capture_turn(&source, "Choose white's next move.", "chess-turn-1");
        source.apply_legal_uci("e2e4")?;
        let second = capture_turn(&source, "Choose black's next move.", "chess-turn-2");
        let turns = vec![first, second];
        let (legacy_system, legacy_users) = render_legacy(&source, &turns).await?;

        let mounted = run_turns(
            chess_agent().into_harness(EpochContractId::new("example/chess-prompt/v1")?),
            turns,
        )
        .await?;

        assert_eq!(mounted.system(), legacy_system);
        assert_eq!(mounted.users(), legacy_users);
        Ok(())
    }
}
