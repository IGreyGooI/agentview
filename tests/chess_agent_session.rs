#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[path = "../examples/chess_engine_agent/support.rs"]
mod chess_support;

use agentview::prelude::*;
use chess_support::{
    apply_engine_move, apply_player_move, ChessGameSource, ChessMoveSink, ChessViewModel,
    StockfishEngine,
};
use serde_json::json;
use tokio::time::timeout;

fn new_session(
    source: ChessGameSource,
) -> (AgentViewSession<ChessViewModel, Turn, ()>, ViewAwakeHandle) {
    AgentViewSession::new(
        ChessViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    )
}

#[cfg(unix)]
fn mock_stockfish_script(best_move: &str) -> (std::path::PathBuf, String) {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "agentview-mock-stockfish-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let script = dir.join("stockfish");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    uci) echo "id name mockfish"; echo "uciok" ;;
    isready) echo "readyok" ;;
    go*) echo "bestmove {best_move}"; exit 0 ;;
    quit) exit 0 ;;
  esac
done
"#
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    let script_string = script.to_string_lossy().into_owned();
    (dir, script_string)
}

#[tokio::test]
async fn observe_renders_starting_board_and_move_contract() {
    let source = ChessGameSource::new();
    let (mut session, _awake) = new_session(source);

    let snapshot = session.observe("Choose white's next move.").await.unwrap();

    assert_eq!(snapshot.view_epoch, 0);
    assert_eq!(snapshot.turn_id, "turn-1");
    assert_eq!(snapshot.view.side_to_move.as_str(), "white");
    assert_eq!(snapshot.view.board.ranks[0].rank, 8);
    assert_eq!(snapshot.view.board.ranks[0].squares[0].square, "a8");
    assert_eq!(
        snapshot.view.board.ranks[0].squares[0]
            .piece
            .as_ref()
            .unwrap()
            .symbol,
        'r'
    );
    assert_eq!(snapshot.view.board.ranks[7].squares[4].square, "e1");
    assert_eq!(
        snapshot.view.board.ranks[7].squares[4]
            .piece
            .as_ref()
            .unwrap()
            .symbol,
        'K'
    );
    assert!(snapshot.view.legal_uci_moves.contains(&"e2e4".to_owned()));
    assert!(snapshot.view.legal_uci_moves.contains(&"g1f3".to_owned()));
    assert!(snapshot
        .turn_prompt
        .task
        .contains("Choose white's next move."));
    assert_eq!(
        snapshot.turn_prompt.reply_schema["required"],
        json!(["uci"])
    );

    let rendered_view = snapshot
        .view
        .render_full(&TemplateEngine::new())
        .await
        .unwrap()
        .into_string();
    assert!(rendered_view.starts_with("<prompt_board render_mode=\"full\">"));
    assert!(!rendered_view.contains("<rendering_mode"));
    assert!(rendered_view.contains("\n  <board_state>\n    <board_ascii>"));
    assert!(rendered_view.contains("\n  <legal_moves>"));
    assert!(rendered_view.contains("\n    <move>e2e4</move>"));
    assert!(!rendered_view.contains("<chess_view>"));

    let rendered_prompt = snapshot
        .turn_prompt
        .render_full(&TemplateEngine::new())
        .await
        .unwrap()
        .into_string();
    assert!(rendered_prompt.starts_with("<chess_task>"));
    assert!(rendered_prompt.contains("\n  <reasoning_policy>"));
    assert!(rendered_prompt.contains("Think privately about candidate moves before acting."));
    assert!(rendered_prompt
        .contains("Do not print chain-of-thought; call the CLI only after deciding."));
    assert!(rendered_prompt.contains("\n  <reply_contract transport=\"cli\">"));
    assert!(rendered_prompt.contains(
        "<command>agentview chess act --piece &lt;piece&gt; --from &lt;from&gt; --to &lt;to&gt; [--promotion &lt;promotion&gt;] --uci &lt;uci&gt;</command>"
    ));
    assert!(rendered_prompt
        .contains("<example>agentview chess act --piece P --from e2 --to e4 --uci e2e4</example>"));
    assert!(rendered_prompt.contains(
        "<promotion_example>agentview chess act --piece P --from e7 --to e8 --promotion q --uci e7e8q</promotion_example>"
    ));
    assert!(!rendered_prompt.contains("<reply_schema>"));
    assert!(!rendered_prompt.contains("<chess_turn_prompt>"));
}

#[tokio::test]
#[cfg(unix)]
async fn act_applies_player_move_and_hook_observes_stockfish_reply() {
    let source = ChessGameSource::new();
    let (mut session, awake) = new_session(source.clone());
    let snapshot = session.observe("Choose white's next move.").await.unwrap();

    let update = session
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e4" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
        .unwrap();

    let after_player = update.snapshot().unwrap();
    assert_eq!(after_player.view_epoch, 1);
    assert_eq!(after_player.view.move_history, vec!["e2e4"]);
    assert_eq!(after_player.view.side_to_move.as_str(), "black");
    assert!(after_player.view.engine_pending);

    let (dir, script) = mock_stockfish_script("e7e5");
    let engine = StockfishEngine::new(script);
    tokio::spawn({
        let source = source.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            apply_engine_move(&source, &awake, &engine).await.unwrap();
        }
    });

    let after_engine = timeout(
        Duration::from_secs(1),
        session.hook(after_player.view_epoch, "Choose white's next move."),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(after_engine.view_epoch, 2);
    assert_eq!(after_engine.view.move_history, vec!["e2e4", "e7e5"]);
    assert_eq!(after_engine.view.side_to_move.as_str(), "white");
    assert!(!after_engine.view.engine_pending);

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn stockfish_engine_applies_bestmove_from_uci_process() {
    let source = ChessGameSource::new();
    let (mut session, awake) = new_session(source.clone());
    let snapshot = session.observe("Choose white's next move.").await.unwrap();

    let update = session
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e4" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
        .unwrap();
    assert!(update.snapshot().unwrap().view.engine_pending);

    let (dir, script) = mock_stockfish_script("e7e5");
    let engine = StockfishEngine::new(script);
    apply_engine_move(&source, &awake, &engine).await.unwrap();

    let after_engine = timeout(
        Duration::from_secs(1),
        session.hook(
            update.snapshot().unwrap().view_epoch,
            "Choose white's next move.",
        ),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(after_engine.view.move_history, vec!["e2e4", "e7e5"]);
    assert_eq!(after_engine.view.last_engine_move.as_deref(), Some("e7e5"));
    assert!(!after_engine.view.engine_pending);
    assert!(after_engine.view.last_error.is_none());

    let _ = fs::remove_dir_all(dir);
}

#[tokio::test]
async fn chess_view_partial_update_renders_changed_board_squares_and_sections() {
    let source = ChessGameSource::new();
    let (mut session, _awake) = new_session(source.clone());
    let snapshot = session.observe("Choose white's next move.").await.unwrap();

    let update = session
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e4" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
        .unwrap();

    let after_player = update.snapshot().unwrap();
    let rendered_update = after_player
        .view
        .render_update_since(&snapshot.view, &TemplateEngine::new())
        .await
        .unwrap()
        .into_string();

    assert!(rendered_update.starts_with("<prompt_board render_mode=\"update\">"));
    assert!(!rendered_update.contains("<rendering_mode"));
    assert!(rendered_update.contains("\n  <board_state>\n    <replace>\n      <board_ascii>"));
    assert!(!rendered_update.contains("\n  <board_state>\n    <board_ascii>"));
    assert!(rendered_update.contains("\n  <board_squares>\n    <replace>"));
    assert!(rendered_update.contains("<square id=\"e2\" file=\"e\" rank=\"2\">.</square>"));
    assert!(rendered_update.contains("<square id=\"e4\" file=\"e\" rank=\"4\">P</square>"));
    assert!(!rendered_update.contains("<square id=\"a8\""));
    assert!(!rendered_update.contains("<rank n="));
    assert!(!rendered_update.contains("<changed_sections>"));
    assert!(rendered_update.contains("\n  <legal_moves>"));
    assert!(rendered_update.contains("\n    <added>"));
    assert!(rendered_update.contains("\n    <removed>"));
    assert!(!rendered_update.contains("<legal_moves op="));
    assert!(rendered_update.contains("\n  <move_history>"));
    assert!(rendered_update.contains("\n    <added>"));
    assert!(!rendered_update.contains("<move_history op="));
    assert!(rendered_update.contains("<move>e2e4</move>"));
    assert!(rendered_update.contains("<move>e7e5</move>"));
    assert!(rendered_update.contains("\n  <engine>\n    <replace>"));
    assert!(!rendered_update.contains("<engine op="));
    assert!(!rendered_update.contains("op=\""));
    assert!(rendered_update.contains("<pending>true</pending>"));
    assert!(!rendered_update.contains("<prompt_board_update>"));
}

#[tokio::test]
async fn act_rejects_illegal_chess_move() {
    let source = ChessGameSource::new();
    let (mut session, _awake) = new_session(source.clone());
    let snapshot = session.observe("Choose white's next move.").await.unwrap();

    let update = session
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e5" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
        .unwrap();

    let next = update.snapshot().unwrap();
    assert!(next
        .view
        .last_error
        .as_deref()
        .unwrap()
        .contains("illegal chess move"));
    assert!(!next.view.engine_pending);
    assert!(source.snapshot().move_history().is_empty());
}
