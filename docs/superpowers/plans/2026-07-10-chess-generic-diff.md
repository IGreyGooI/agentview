# Chess Generic Diff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the chess-specific update renderer with field-level generic `AgentView` diff while preserving the chess domain model and turn flow.

**Architecture:** `ChessView` is the only agent-facing state shape. It collects board squares into a flat keyed vector and declares replace, keyed, set, and seq policies on its fields; blanket `ContextView` rendering then owns delta generation. Example, CLI, and player skill consume the same generic protocol.

**Tech Stack:** Rust, `AgentView` proc macro, semantic view tree, Tokio integration tests, Cargo.

## Global Constraints

- Do not add a new diff mode or change generic diff semantics.
- Keep `ChessBoardView`, chess rules, engine flow, and turn prompt behavior unchanged.
- Full board output contains direct `<square>` children under `<board_squares>` and no `<rank>` nodes.
- Delta output uses `rendering_mode="delta"` and `insert`, `remove`, `update`, and `replace` operations.
- Preserve unrelated and pre-existing dirty-worktree changes; do not create implementation commits unless explicitly requested.

---

## File Map

- `examples/chess_engine_agent/support.rs`: Defines the chess domain snapshots, agent-facing view structs, collection, and test accessors. This file loses all update-only structs and helpers.
- `tests/chess_agent_session.rs`: Proves full view shape and real `e2e4` generic delta behavior.
- `examples/chess_engine_agent.rs`: Prints deltas through `ContextView::render_delta`.
- `src/bin/agentview.rs`: Returns generic deltas from the chess daemon path and maps no-change to an empty view payload.
- `tests/agentview_cli.rs`: Proves `observe`, `act`, and `hook` expose the new protocol end to end.
- `examples/agentview-chess-player/SKILL.md`: Teaches the player to consume `rendering_mode="delta"` and generic collection operations.

## Task 1: Flatten the Agent-Facing Board Squares

**Files:**
- Modify: `tests/chess_agent_session.rs:78-93, 170-181`
- Modify: `examples/chess_engine_agent/support.rs:127-152, 381-419, 475-505, 634-736, 780-792`

**Interfaces:**
- Consumes: `ChessBoardView { ranks: Vec<ChessRankView> }` and `ChessSquarePromptView::collect(&ChessSquareView)`.
- Produces: `ChessView::board_squares: Vec<ChessSquarePromptView>` in rank-major board order.

- [x] **Step 1: Write the failing full-render test**

Replace the nested-rank assertion with the desired direct-child shape:

```rust
#[tokio::test]
async fn chess_board_squares_render_as_a_flat_agent_facing_list() {
    let source = ChessGameSource::new();
    let (mut session, _awake) = new_session(source);
    let snapshot = session.observe("Choose white's next move.").await.unwrap();

    let rendered = snapshot
        .view
        .render_full(&TemplateEngine::new())
        .await
        .unwrap()
        .into_string();

    assert!(rendered.contains(
        "\n  <board_squares>\n    <square id=\"a8\" file=\"a\" rank=\"8\">r</square>"
    ));
    assert!(rendered.contains("<square id=\"e1\" file=\"e\" rank=\"1\">K</square>"));
    assert!(!rendered.contains("<rank "));
    assert!(!rendered.contains("<ranks>"));
    assert!(!rendered.contains("<squares>"));
}
```

In `observe_renders_starting_board_and_move_contract`, change the board container assertion to:

```rust
assert!(rendered_view.contains("\n  <board_squares>\n    <square id=\"a8\""));
```

- [x] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test --test chess_agent_session chess_board_squares_render_as_a_flat_agent_facing_list -- --exact
```

Expected: FAIL because current output contains `<board_squares kind="board_squares"><rank ...>` instead of direct square children.

- [x] **Step 3: Implement the flat collection shape**

Delete `ChessRankPromptView`, `ChessBoardSquaresPromptView`, and their `AgentViewCollect` implementations. Change `ChessView` and collection to:

```rust
pub struct ChessView {
    board_state: ChessBoardStatePromptView,
    board_squares: Vec<ChessSquarePromptView>,
    legal_moves: Vec<ChessMovePromptView>,

    #[view(name = "move_history")]
    move_history_view: Vec<ChessMovePromptView>,

    engine: ChessEnginePromptView,
}
```

```rust
board_squares: board
    .ranks
    .iter()
    .flat_map(|rank| rank.squares.iter())
    .map(ChessSquarePromptView::collect)
    .collect(),
```

Preserve the index-based test API with one test-only lookup:

```rust
#[cfg(test)]
fn square_for_test(
    &self,
    rank_index: usize,
    square_index: usize,
) -> Option<&ChessSquarePromptView> {
    let index = rank_index.checked_mul(8)?.checked_add(square_index)?;
    self.board_squares.get(index)
}
```

Make `rank_for_test`, `square_id_for_test`, `piece_symbol_for_test`, and `render_square_xml_for_test` delegate to `square_for_test`. Remove `render_board_squares_xml_for_test`. Temporarily update the old helper signature so the file still compiles before Task 3:

```rust
fn changed_squares(
    prev: &[ChessSquarePromptView],
    next: &[ChessSquarePromptView],
) -> Vec<ChessSquarePromptView> {
    prev.iter()
        .zip(next)
        .filter_map(|(prev_square, next_square)| {
            (prev_square != next_square).then(|| next_square.clone())
        })
        .collect()
}
```

- [x] **Step 4: Run the focused full-view tests and verify GREEN**

Run:

```bash
cargo test --test chess_agent_session chess_board_squares_render_as_a_flat_agent_facing_list -- --exact
cargo test --test chess_agent_session observe_renders_starting_board_and_move_contract -- --exact
cargo test --test chess_agent_session chess_square_leaf_uses_agent_view_derive -- --exact
```

Expected: all three tests PASS; full XML has 64 direct square children and no rank wrapper.

## Task 2: Drive Chess Delta Through Generic Field Policies

**Files:**
- Modify: `tests/chess_agent_session.rs:302-352`
- Modify: `examples/chess_engine_agent/support.rs:631-643`

**Interfaces:**
- Consumes: blanket `ContextView::render_delta` and existing `replace`, keyed, set, and seq diff implementations.
- Produces: generic delta XML directly from `ChessView` with no chess-specific renderer involved in the test.

- [x] **Step 1: Write the failing generic-delta integration test**

Rename the existing partial-update test and render through `ContextView`:

```rust
#[tokio::test]
async fn chess_view_uses_generic_field_diff_for_a_player_move() {
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
    assert!(rendered.contains(
        "\n    <update>\n      <square id=\"e4\" file=\"e\" rank=\"4\">P</square>"
    ));
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
```

- [x] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test --test chess_agent_session chess_view_uses_generic_field_diff_for_a_player_move -- --exact
```

Expected: FAIL because changed unmarked fields currently force a full `<prompt_board>` render.

- [x] **Step 3: Declare the field policies on `ChessView`**

Use exactly the approved field annotations:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, AgentView)]
#[agent_view(kind = "prompt_board")]
pub struct ChessView {
    #[view(diff(replace))]
    board_state: ChessBoardStatePromptView,

    #[view(diff(key = "id"))]
    board_squares: Vec<ChessSquarePromptView>,

    #[view(diff(set))]
    legal_moves: Vec<ChessMovePromptView>,

    #[view(name = "move_history", diff(seq))]
    move_history_view: Vec<ChessMovePromptView>,

    #[view(diff(replace))]
    engine: ChessEnginePromptView,
}
```

- [x] **Step 4: Run the chess integration tests and verify GREEN**

Run:

```bash
cargo test --test chess_agent_session chess_view_uses_generic_field_diff_for_a_player_move -- --exact
cargo test --test chess_agent_session
```

Expected: the focused test and the entire chess session test target PASS.

## Task 3: Migrate Consumers and Remove the Old Update Protocol

**Files:**
- Modify: `tests/agentview_cli.rs:131-222`
- Modify: `examples/chess_engine_agent.rs:88-110`
- Modify: `src/bin/agentview.rs:756-781`
- Modify: `examples/chess_engine_agent/support.rs:190-314, 508-600, 738-792`
- Modify: `examples/agentview-chess-player/SKILL.md:33, 63-69`

**Interfaces:**
- Consumes: `ContextView::render_delta(&self, &previous, &TemplateEngine) -> Result<Option<PromptFragment>>`.
- Produces: one delta protocol across the library integration test, example binary, daemon CLI, and chess-player instructions.

- [x] **Step 1: Record the current skill-protocol failure (RED)**

Give a fresh subagent the current `examples/agentview-chess-player/SKILL.md` plus this delta and ask how to update the prior position:

```xml
<prompt_board rendering_mode="delta">
  <board_squares rendering_mode="delta">
    <update><square id="e2" file="e" rank="2">.</square></update>
    <update><square id="e4" file="e" rank="4">P</square></update>
  </board_squares>
</prompt_board>
```

Expected baseline: the current skill describes `render_mode="update"`, `<added>`, and `<removed>`, and does not correctly state the generic keyed `update` protocol. Save the exact failure summary in the implementation notes for this task.

- [x] **Step 2: Write the failing CLI protocol assertions**

In `chess_commands_share_an_implicit_server_session`, require full output to contain direct board squares:

```rust
assert!(observe_stdout.contains("<board_squares>"));
assert!(!observe_stdout.contains("<board_squares kind="));
```

For both `act_stdout` and `hook_stdout`, replace old update assertions with this shape (using `e2/e4` for act and `e7/e5` for hook):

```rust
assert!(act_stdout.contains("<prompt_board rendering_mode=\"delta\">"));
assert!(act_stdout.contains("<board_state rendering_mode=\"delta\">"));
assert!(act_stdout.contains("<board_squares rendering_mode=\"delta\">"));
assert!(act_stdout.contains("<update>"));
assert!(act_stdout.contains("<square id=\"e2\" file=\"e\" rank=\"2\">.</square>"));
assert!(act_stdout.contains("<square id=\"e4\" file=\"e\" rank=\"4\">P</square>"));
assert!(act_stdout.contains("<legal_moves rendering_mode=\"delta\">"));
assert!(act_stdout.contains("<insert>"));
assert!(act_stdout.contains("<remove>"));
assert!(act_stdout.contains("<move_history rendering_mode=\"delta\">"));
assert!(act_stdout.contains("<engine rendering_mode=\"delta\">"));
assert!(act_stdout.contains("<replace>"));
assert!(!act_stdout.contains("render_mode="));
assert!(!act_stdout.contains("<added>"));
assert!(!act_stdout.contains("<removed>"));
```

- [x] **Step 3: Run the CLI test and verify RED**

Run:

```bash
cargo test --test agentview_cli chess_commands_share_an_implicit_server_session -- --exact
```

Expected: FAIL because the daemon still calls `render_update_since` and emits `render_mode="update"` with chess-specific wrappers.

- [x] **Step 4: Switch both runtime consumers to `ContextView::render_delta`**

In the example, only print a view block when a delta exists:

```rust
if let Some(view) = snapshot
    .view
    .render_delta(previous_view, templates)
    .await?
{
    println!("view:\n{}", view.as_str());
}
```

In the daemon, map `None` to an empty view payload while preserving existing error handling:

```rust
Some(prev) => match snapshot.view.render_delta(prev, templates).await {
    Ok(Some(view)) => view.into_string(),
    Ok(None) => String::new(),
    Err(err) => {
        return DaemonResponse::Error {
            message: err.to_string(),
        };
    }
},
```

- [x] **Step 5: Delete the chess-specific update implementation**

Remove all of these private types and functions from `support.rs`:

```text
ChessBoardStateReplaceView
ChessBoardStateUpdateView
ChessSquaresReplaceView
ChessBoardSquaresUpdateView
ChessMovesAddedView
ChessMovesRemovedView
ChessLegalMovesUpdateView
ChessMoveHistoryUpdateView
ChessEngineReplaceView
ChessEngineUpdateView
ChessPromptBoardUpdateView
chess_board_state_replace_view
chess_board_state_update_view
chess_board_squares_update_view
chess_moves_added_view
chess_moves_removed_view
chess_legal_moves_update_view
chess_move_history_update_view
chess_engine_update_view
chess_prompt_board_update_view
render_chess_prompt_board_update_xml
ChessView::render_update_since
list_added
list_removed
ordered_list_delta
changed_squares
```

No import rewrite is required: `support.rs` uses `agentview::prelude::*`, and the remaining session/view-model code still consumes that prelude.

- [x] **Step 6: Update the chess-player skill with the generic protocol (GREEN)**

Change the usage guidance to say:

```markdown
The first observe is a full `<prompt_board>` render. Later `act` and `hook`
responses usually contain `<prompt_board rendering_mode="delta">` and only
the fields that changed.

- Apply `<replace>` by replacing the named field.
- Apply list `<insert>` and `<remove>` operations to the prior list.
- Apply keyed `<update>` operations by replacing the item with the same stable
  attribute, such as a chess square's `id`.
```

Also remove the obsolete instruction to look for `render_mode="full"`.

- [x] **Step 7: Re-run the skill scenario and verify GREEN**

Give a fresh subagent the updated skill and the same `e2/e4` delta. Expected: it says to replace squares `e2` and `e4` by stable `id`, without looking for `added`, `removed`, or `render_mode="update"`.

- [x] **Step 8: Run consumer tests and verify GREEN**

Run:

```bash
cargo test --test agentview_cli chess_commands_share_an_implicit_server_session -- --exact
cargo test --test agentview_cli
cargo test --example chess_engine_agent
```

Expected: all commands PASS and no source reference to `render_update_since` or old update tags remains outside the historical design document.

## Final Verification

Review follow-up added two focused safeguards before final verification:

- The daemon advances `last_view` only after response rendering produces a `DaemonResponse::Snapshot`; an error response leaves the previous diff baseline intact.
- A real castling sequence verifies that keyed board diff emits all four changed squares (`e1`, `f1`, `g1`, and `h1`), in addition to the two-square `e2e4` case.

- [x] Run formatting and static diff checks:

```bash
cargo fmt --check
git diff --check
```

- [x] Run the complete test suite:

```bash
cargo test
```

- [x] Confirm old protocol code is gone:

```bash
rg -n "render_update_since|prompt_board render_mode=|<added>|<removed>|ChessBoardSquaresPromptView|ChessRankPromptView" . --glob '!target/**' --glob '!.git/**'
```

Expected: only explanatory design/plan references and negative regression assertions may remain; implementation code and the chess-player skill contain no old protocol identifiers or positive old-format guidance.

- [x] Review `git diff --stat` and `git status --short` to confirm only scoped files changed and pre-existing dirty files remain untouched beyond this migration.
