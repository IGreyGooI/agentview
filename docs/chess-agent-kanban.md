# Chess Agent Vs Engine Kanban

## Goal

Build a small chess example that proves the AgentView observe/act/hook model
against a real chess engine.

The first version should not build a chess engine. It should expose a chess
game as an `AgentViewModel`, accept a caller move through `act`, let a UCI engine
reply through an app-side async task, and wake the view so the caller can
observe the new board.

This is an AgentView example, not an `LLMExecutor` example. The outside caller
may be a human CLI user, a daemon client, or another LLM agent using a skill.
That caller owns the model loop and tools. The chess VM owns game state,
validation, rendering, and engine-opponent progress.

## Current Code Fit

The repo already has the right base pieces:

- `AgentViewSession::observe` returns a full `ViewSnapshot`.
- `AgentViewSession::act_with_sink` feeds an external `ControlReply` into
  `TurnSink<ControlReply>`, applies the sink output, and returns a full
  `ViewUpdate`.
- `AgentViewSession::hook` waits for `ViewAwakeHandle::awake` and then returns a
  full `ViewSnapshot`.
- `ViewUpdate` already has a `Partial(ViewPatch)` variant, but the session
  currently returns full updates only.
- `src/bin/agentview.rs` proves a hidden loopback daemon plus thin CLI, but it
  only supports the hello-world `observe` and `act` path.

The main current gap is async engine work. `act_with_sink` takes a synchronous
apply closure, so a UCI engine reply should not be hidden inside that closure in
the first version. The clean first slice is:

1. `act` applies the caller's legal chess move.
2. `act` schedules or triggers engine work outside the session apply closure.
3. `act` returns the board after the caller move.
4. `hook` returns after the engine move updates the source and calls `awake`.

That flow exercises both loops without changing the core API too early.

## External Chess Choices

Preferred first choices:

- Board/legal move library: `chess`
  - It is MIT-licensed, which fits this MIT crate better than GPL chess
    libraries.
  - It supports legal move generation, FEN parsing/rendering, move application,
    and UCI-style `ChessMove` parsing.
  - Source checked: https://docs.rs/chess/latest/chess/
- Engine protocol: UCI over a spawned Stockfish-compatible process
  - Stockfish documents UCI as the standard text protocol for GUI/tools.
  - Source checked:
    https://official-stockfish.github.io/docs/stockfish-wiki/UCI-%26-Commands.html

Local lookup result:

- No chess DLL/shared-object engine artifact was found under this repo.
- The current implementation uses a Stockfish-compatible UCI process.
- Tests use a mock UCI executable fixture so they do not depend on the real
  Stockfish package.

Alternatives to keep in mind:

- `shakmaty` has a richer chess vocabulary and variants, but it is GPL-3.0, so
  it is not the default choice for this crate.
  - Source checked: https://docs.rs/shakmaty/latest/shakmaty/
- `cozy-chess` is a focused move-generation library if we want a smaller or
  faster board dependency later.
  - Source checked: https://crates.io/crates/cozy-chess
- `pleco_engine` exists, but its docs say it is mostly useful as a direct
  executable and not intended as a dependency.
  - Source checked: https://docs.rs/pleco_engine/latest/pleco_engine/

## Architecture Sketch

The example should have four small layers.

`ChessGameSource` owns app state:

- current position;
- side controlled by the AgentView caller;
- legal moves;
- move history;
- last engine status or error;
- game result;
- pending engine task flag.

`ChessViewModel` captures the structured AgentView:

- board ranks, squares, and pieces;
- FEN;
- side to move;
- legal UCI moves;
- last move pair;
- engine/opponent status;
- compact future-looking prompt, such as candidate moves and tactical notes the
  caller should consider.

The prompt renderer may turn that view into a board diagram, but the view
itself should not be only a rendered string.

`ChessMoveSink` parses the caller reply:

- accept structured JSON first: `{ "uci": "e2e4" }`;
- optionally accept text `e2e4` for CLI ergonomics;
- validate against legal moves before mutating source;
- return a domain output like `PlayerMove { uci }`.

`UciEngineClient` owns process interaction:

- spawn an engine command, defaulting to `stockfish`;
- initialize with `uci` and `isready`;
- send `position fen ...` plus `go movetime ...` or `go depth ...`;
- parse `bestmove`;
- expose a simple async `best_move(fen) -> UciMove`.

## Kanban

### Done

- [x] Create the chess planning and kanban doc.
- [x] Pick the first implementation boundary for the chess example.
- [x] Core `ViewSnapshot`, `ViewUpdate`, and `ViewAwake` types exist.
- [x] `AgentViewSession` supports full `observe`, full-update `act`, and full
  `hook`.
- [x] Hidden daemon plus thin CLI pattern exists for hello world.
- [x] Loopback TCP transport exists for the local daemon prototype.
- [x] Add MIT-licensed chess move-generation dependency for the example and
  demo CLI.
- [x] Define `ChessGameSource`, `ChessView`, `ChessTurnPrompt`, and
  `ChessViewModel` under example support, not `src`.
- [x] Write unit/integration tests for move parsing, illegal-move rejection,
  observe, act, and hook.
- [x] Use a mock Stockfish-compatible UCI process in tests so test runs do not
  require the real engine binary.
- [x] Implement `examples/chess_engine_agent.rs` with observe, act, and hook.
- [x] Render the chess turn contract as a CLI call:
  `agentview chess act <uci>`.
- [x] Add a hidden-daemon CLI path for the chess example without exposing daemon
  commands in help.
- [x] Add `agentview chess hook <epoch>` for the Stockfish awake path.
- [x] Implement the Stockfish UCI subprocess adapter and remove the in-process
  non-Stockfish engine path.

### Doing

- [ ] Add configurable Stockfish strength/Elo to the VM and turn prompt.

### Ready

- [ ] Add configurable command, depth, move time, and optional target Elo for
  Stockfish.
- [ ] Show a clear runtime error when the configured Stockfish binary is missing.

### Next

- [x] Add integration tests using a mock UCI process fixture.
- [ ] Add a debug transcript format similar to hello world:
  `observe`, `act`, `hook`, with epoch and turn ids.
- [ ] Add `ViewUpdate` patching only after the full chess view is stable.
- [ ] Add a caller profile for LLM-friendly patch budgets later.

### Later

- [ ] Add a reusable chess skill surface that can be embedded in another chat.
- [ ] Add a player-facing board viewer.
- [ ] Add engine strength controls using UCI options such as skill level, depth,
  or move time.
- [ ] Add match runner support for multiple games.
- [ ] Add a rough rating/eval report. Treat it as approximate unless we build a
  real evaluation harness with enough games and fixed time controls.
- [ ] Add PGN export and replay.
- [ ] Add analysis mode where the engine comments on blunders after the game.

## First Implementation Slice

The first slice should be deliberately small:

```text
observe
  -> full chess board snapshot
  -> turn_prompt asks for a legal UCI move through:
     agentview chess act <uci>

agentview chess act e2e4
  -> validate legal move
  -> apply caller move
  -> start Stockfish engine task
  -> return full update after caller move

agentview chess hook epoch
  -> wait until engine task updates the source and calls awake
  -> return full snapshot after engine move
```

This proves the important shape: the caller controls its own loop, while the
AgentView source can independently progress and wake the view.

## Test Plan

- Unit: parse structured move reply.
- Unit: parse text move reply.
- Unit: reject illegal move.
- Unit: render full chess snapshot from a starting position.
- Unit: turn prompt includes `turn_id` and legal move contract.
- Integration: observe returns starting board.
- Integration: act applies caller move and advances epoch.
- Integration: hook waits for a Stockfish-compatible UCI engine move and returns
  the next board.
- CLI: public help hides daemon/internal commands.
- CLI: observe/act/hook work against the implicit local server.

## Open Questions

- If the chess example becomes useful beyond a demo, should it move into a
  separate crate or an explicitly gated module?
- Do we want `AgentViewSession` to support async apply closures later, or is
  async source progress through `hook` the intended style?
- How much of the chess view should be rendered as structured JSON versus
  human-readable text?
- For rating, do we care about approximate demo numbers or a real repeatable
  harness?
