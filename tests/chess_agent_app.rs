#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[path = "../examples/chess_engine_agent/support.rs"]
mod chess_support;

use agentview::prelude::*;
use chess_support::{
    apply_engine_move, apply_player_move, ChessGameSource, ChessMoveSink, ChessTaskView, ChessView,
    ChessViewModel, StockfishEngine,
};
use serde_json::json;
use tokio::time::timeout;

fn new_app(source: ChessGameSource) -> (AgentViewApp<ChessViewModel, Turn, ()>, ViewAwakeHandle) {
    AgentViewApp::new(
        ChessViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    )
}

async fn apply_test_move(
    app: &mut AgentViewApp<ChessViewModel, Turn, ()>,
    source: &ChessGameSource,
    snapshot: &ViewSnapshot<ChessView, ChessTaskView>,
    uci: &str,
) -> ViewSnapshot<ChessView, ChessTaskView> {
    app.act_with_sink(
        &snapshot.turn_id,
        ControlReply::structured(json!({ "uci": uci })),
        ChessMoveSink::from_source(source),
        apply_player_move,
        "Continue the test game.",
    )
    .await
    .unwrap()
    .snapshot()
    .unwrap()
    .clone()
}

fn assert_agent_view<T: AgentView>() {}

#[test]
fn captured_chess_view_is_agent_facing_view() {
    assert_agent_view::<ChessView>();
}

#[test]
fn built_chess_task_view_is_agent_facing_view() {
    assert_agent_view::<ChessTaskView>();
}

#[tokio::test]
async fn chess_view_can_be_collected_from_game_state_snapshot() {
    let source = ChessGameSource::new();
    let collected = ChessView::collect(&source.snapshot());

    assert_eq!(collected.side_to_move(), "white");
    assert!(collected.legal_uci_moves().contains(&"e2e4"));
    assert!(render_agent_view_xml(&collected).starts_with("<prompt_board>"));
}

#[tokio::test]
async fn chess_square_leaf_uses_agent_view_derive() {
    let source = ChessGameSource::new();
    let (mut app, _awake) = new_app(source);
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    assert_eq!(
        snapshot.view.render_square_xml_for_test(0, 0).unwrap(),
        r#"<square id="a8" file="a" rank="8">r</square>"#
    );
}

#[tokio::test]
async fn chess_board_state_uses_agent_view_derive() {
    let source = ChessGameSource::new();
    let (mut app, _awake) = new_app(source);
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    let rendered = chess_support::render_board_state_xml_for_test(&snapshot.view);

    assert!(rendered.starts_with("<board_state>"));
    assert!(rendered.contains("<board_ascii>8 r n b q k b n r"));
    assert!(rendered.contains("<fen>"));
    assert!(rendered.contains("<side_to_move>white</side_to_move>"));
    assert!(rendered.contains("<status>ongoing</status>"));
    assert!(rendered.ends_with("</board_state>"));
}

#[tokio::test]
async fn chess_board_squares_render_as_a_flat_agent_facing_list() {
    let source = ChessGameSource::new();
    let (mut app, _awake) = new_app(source);
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    let rendered = snapshot
        .view
        .render_full(&TemplateEngine::new())
        .await
        .unwrap()
        .into_string();

    assert!(rendered
        .contains("\n  <board_squares>\n    <square id=\"a8\" file=\"a\" rank=\"8\">r</square>"));
    assert!(rendered.contains("<square id=\"a8\" file=\"a\" rank=\"8\">r</square>"));
    assert!(rendered.contains("<square id=\"e1\" file=\"e\" rank=\"1\">K</square>"));
    assert!(!rendered.contains("<rank "));
    assert!(!rendered.contains("<ranks>"));
    assert!(!rendered.contains("<squares>"));
    assert_eq!(rendered.matches("<square id=").count(), 64);

    let mut previous_offset = 0;
    for rank in (1..=8).rev() {
        for file in "abcdefgh".chars() {
            let square = format!("<square id=\"{file}{rank}\"");
            assert_eq!(rendered.matches(&square).count(), 1, "square {file}{rank}");
            let offset = rendered.find(&square).unwrap();
            assert!(
                offset >= previous_offset,
                "square {file}{rank} is out of order"
            );
            previous_offset = offset;
        }
    }
}

#[tokio::test]
async fn chess_task_view_escapes_task_text() {
    let prompt = ChessTaskView::new("Choose <e2e4> & verify.", json!({}));

    let rendered = prompt
        .render_full(&TemplateEngine::new())
        .await
        .unwrap()
        .into_string();

    assert!(rendered.contains("<task>Choose &lt;e2e4&gt; &amp; verify.</task>"));
    assert!(!rendered.contains("<task>Choose <e2e4> & verify.</task>"));
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
    let (mut app, _awake) = new_app(source);

    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    assert_eq!(snapshot.view_epoch, 0);
    assert_eq!(snapshot.turn_id, "turn-1");
    assert_eq!(snapshot.view.side_to_move(), "white");
    assert_eq!(snapshot.view.rank_for_test(0), Some(8));
    assert_eq!(snapshot.view.square_id_for_test(0, 0), Some("a8"));
    assert_eq!(snapshot.view.piece_symbol_for_test(0, 0), Some('r'));
    assert_eq!(snapshot.view.square_id_for_test(7, 4), Some("e1"));
    assert_eq!(snapshot.view.piece_symbol_for_test(7, 4), Some('K'));
    assert!(snapshot.view.legal_uci_moves().contains(&"e2e4"));
    assert!(snapshot.view.legal_uci_moves().contains(&"g1f3"));
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
    assert!(rendered_view.starts_with("<prompt_board>"));
    assert!(!rendered_view.contains("render_mode=\"full\""));
    assert!(!rendered_view.contains("<rendering_mode"));
    assert!(rendered_view.contains("\n  <board_state kind=\"board_state\">\n    <board_ascii>"));
    assert!(rendered_view.contains("\n  <board_squares>\n    <square id=\"a8\""));
    assert!(rendered_view.contains("\n  <legal_moves>"));
    assert!(rendered_view.contains("\n    <move>e2e4</move>"));
    assert!(rendered_view.contains("\n  <engine kind=\"engine\">\n    <pending>false</pending>"));
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
    let (mut app, awake) = new_app(source.clone());
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    let update = app
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
    assert_eq!(after_player.view.move_history(), vec!["e2e4"]);
    assert_eq!(after_player.view.side_to_move(), "black");
    assert!(after_player.view.engine_pending());

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
        app.hook(after_player.view_epoch, "Choose white's next move."),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(after_engine.view_epoch, 2);
    assert_eq!(after_engine.view.move_history(), vec!["e2e4", "e7e5"]);
    assert_eq!(after_engine.view.side_to_move(), "white");
    assert!(!after_engine.view.engine_pending());

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn stockfish_engine_applies_bestmove_from_uci_process() {
    let source = ChessGameSource::new();
    let (mut app, awake) = new_app(source.clone());
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    let update = app
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e4" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
        .unwrap();
    assert!(update.snapshot().unwrap().view.engine_pending());

    let (dir, script) = mock_stockfish_script("e7e5");
    let engine = StockfishEngine::new(script);
    apply_engine_move(&source, &awake, &engine).await.unwrap();

    let after_engine = timeout(
        Duration::from_secs(1),
        app.hook(
            update.snapshot().unwrap().view_epoch,
            "Choose white's next move.",
        ),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(after_engine.view.move_history(), vec!["e2e4", "e7e5"]);
    assert_eq!(after_engine.view.last_engine_move(), Some("e7e5"));
    assert!(!after_engine.view.engine_pending());
    assert!(after_engine.view.last_error().is_none());

    let _ = fs::remove_dir_all(dir);
}

#[tokio::test]
async fn chess_view_uses_generic_field_diff_for_a_player_move() {
    let source = ChessGameSource::new();
    let (mut app, _awake) = new_app(source.clone());
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    let update = app
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "uci": "e2e4" })),
            ChessMoveSink::from_source(&source),
            apply_player_move,
            "Wait for the engine reply.",
        )
        .await
        .unwrap();

    let rendered = update
        .snapshot()
        .unwrap()
        .view
        .render_delta(&snapshot.view, &TemplateEngine::new())
        .await
        .unwrap()
        .unwrap()
        .into_string();

    assert!(rendered.starts_with("<prompt_board rendering_mode=\"delta\">"));
    assert!(rendered.contains(
        "\n  <board_state rendering_mode=\"delta\">\n    <replace>\n      <board_state kind=\"board_state\">"
    ));
    assert!(rendered.contains(
        "\n  <board_squares rendering_mode=\"delta\">\n    <update>\n      <square id=\"e2\" file=\"e\" rank=\"2\">.</square>"
    ));
    assert!(rendered
        .contains("\n    <update>\n      <square id=\"e4\" file=\"e\" rank=\"4\">P</square>"));
    assert_eq!(rendered.matches("\n    <update>").count(), 2);
    assert_eq!(rendered.matches("<square id=").count(), 2);
    assert!(!rendered.contains("<square id=\"a8\""));
    assert!(rendered.contains("\n  <legal_moves rendering_mode=\"delta\">"));
    assert!(rendered.contains("\n    <insert>"));
    assert!(rendered.contains("\n    <remove>"));
    assert!(rendered.contains("\n  <move_history rendering_mode=\"delta\">"));
    assert!(rendered.contains("<move>e2e4</move>"));
    assert!(rendered.contains("\n  <engine rendering_mode=\"delta\">\n    <replace>"));
    assert!(rendered.contains("<pending>true</pending>"));
    assert!(!rendered.contains("render_mode="));
    assert!(!rendered.contains("<added>"));
    assert!(!rendered.contains("<removed>"));
}

#[tokio::test]
async fn chess_view_keyed_diff_emits_all_four_castling_square_updates() {
    let source = ChessGameSource::new();
    let (mut app, _awake) = new_app(source.clone());
    let mut snapshot = app.observe("Set up castling.").await.unwrap();

    for uci in ["e2e4", "e7e5", "g1f3", "b8c6", "f1e2", "g8f6"] {
        snapshot = apply_test_move(&mut app, &source, &snapshot, uci).await;
    }

    let after_castling = apply_test_move(&mut app, &source, &snapshot, "e1g1").await;
    let rendered = after_castling
        .view
        .render_delta(&snapshot.view, &TemplateEngine::new())
        .await
        .unwrap()
        .unwrap()
        .into_string();

    assert_eq!(rendered.matches("\n    <update>").count(), 4);
    assert_eq!(rendered.matches("<square id=").count(), 4);
    assert!(rendered.contains("<square id=\"e1\" file=\"e\" rank=\"1\">.</square>"));
    assert!(rendered.contains("<square id=\"f1\" file=\"f\" rank=\"1\">R</square>"));
    assert!(rendered.contains("<square id=\"g1\" file=\"g\" rank=\"1\">K</square>"));
    assert!(rendered.contains("<square id=\"h1\" file=\"h\" rank=\"1\">.</square>"));
}

#[tokio::test]
async fn act_rejects_illegal_chess_move() {
    let source = ChessGameSource::new();
    let (mut app, _awake) = new_app(source.clone());
    let snapshot = app.observe("Choose white's next move.").await.unwrap();

    let update = app
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
        .last_error()
        .unwrap()
        .contains("illegal chess move"));
    assert!(!next.view.engine_pending());
    assert!(source.snapshot().move_history().is_empty());
}
