//! Minimal chess AgentViewSession with a Stockfish-compatible UCI engine.
//!
//! Run with:
//! `cargo run --example chess_engine_agent`

use std::time::Duration;

use agentview::prelude::*;
use chess_support::{
    apply_engine_move, apply_player_move, ChessGameSource, ChessMoveSink, ChessTurnPrompt,
    ChessView, ChessViewModel, StockfishEngine,
};
use serde_json::json;

#[path = "chess_engine_agent/support.rs"]
mod chess_support;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = ChessGameSource::new();
    let (mut session, awake): (AgentViewSession<ChessViewModel, Turn, ()>, _) =
        AgentViewSession::new(
            ChessViewModel,
            source.clone(),
            PromptContext::<Turn, ()>::without_system(),
        );
    let templates = TemplateEngine::new();

    let snapshot = session.observe("Choose white's next move.").await?;
    print_chess_full_snapshot("observe", &snapshot, &templates).await?;

    let update = session
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
    print_chess_update("act", after_player, &snapshot.view, &templates).await?;

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

    let after_engine = session
        .hook(after_player.view_epoch, "Choose white's next move.")
        .await?;
    print_chess_update("hook", &after_engine, &after_player.view, &templates).await?;

    Ok(())
}

async fn print_chess_full_snapshot(
    event: &str,
    snapshot: &ViewSnapshot<ChessView, ChessTurnPrompt>,
    templates: &TemplateEngine,
) -> anyhow::Result<()> {
    println!(
        "{event} epoch={} turn={}",
        snapshot.view_epoch, snapshot.turn_id
    );
    println!(
        "view:\n{}",
        snapshot.view.render_full(templates).await?.as_str()
    );
    println!(
        "prompt:\n{}",
        snapshot.turn_prompt.render_full(templates).await?.as_str()
    );

    Ok(())
}

async fn print_chess_update(
    event: &str,
    snapshot: &ViewSnapshot<ChessView, ChessTurnPrompt>,
    previous_view: &ChessView,
    templates: &TemplateEngine,
) -> anyhow::Result<()> {
    println!(
        "{event} epoch={} turn={}",
        snapshot.view_epoch, snapshot.turn_id
    );
    println!(
        "view:\n{}",
        snapshot
            .view
            .render_update_since(previous_view, templates)
            .await?
            .as_str()
    );
    println!(
        "prompt:\n{}",
        snapshot.turn_prompt.render_full(templates).await?.as_str()
    );

    Ok(())
}
