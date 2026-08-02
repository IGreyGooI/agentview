//! Minimal chess AgentViewApp with a Stockfish-compatible UCI engine.
//!
//! Run with:
//! `cargo run --example chess_engine_agent`

use std::time::Duration;

use agentview::prelude::*;
use chess_support::legacy_agentview_app::{
    apply_engine_move, apply_player_move, ChessMoveSink, ChessViewModel,
};
use chess_support::{ChessGameSource, ChessView, StockfishEngine};
use serde_json::json;

#[path = "chess/mod.rs"]
mod chess_support;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = ChessGameSource::new();
    let (mut app, awake): (AgentViewApp<ChessViewModel, Turn, ()>, _) = AgentViewApp::new(
        ChessViewModel,
        source.clone(),
        PromptContext::<Turn, ()>::without_system(),
    );
    let system_prompt = ChessViewModel
        .build_system_document(app.session().context(), &source)
        .await?;
    println!(
        "system:\n{}",
        render_pom_document(&resolve_system_document(system_prompt))?
    );

    let snapshot = app.observe("Choose white's next move.").await?;
    print_chess_snapshot("observe", &snapshot)?;

    let update = app
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e4" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await?;

    let after_player = update
        .snapshot()
        .ok_or_else(|| anyhow::anyhow!("chess example expected a full update"))?;
    print_chess_snapshot("act", after_player)?;

    let engine = StockfishEngine::new(
        std::env::var("AGENTVIEW_STOCKFISH_BIN").unwrap_or_else(|_| "stockfish".to_owned()),
    );
    tokio::spawn({
        let source = source.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            apply_engine_move(&source, &awake, &engine).await
        }
    });

    let after_engine = app
        .hook(after_player.view_epoch, "Choose white's next move.")
        .await?;
    print_chess_snapshot("hook", &after_engine)?;

    Ok(())
}

fn print_chess_snapshot(
    event: &str,
    snapshot: &ViewSnapshot<ChessView, ResolvedDocument>,
) -> anyhow::Result<()> {
    println!(
        "{event} epoch={} turn={}",
        snapshot.view_epoch, snapshot.turn_id
    );
    println!("user:\n{}", render_pom_document(&snapshot.user_document)?);

    Ok(())
}
