---
name: agentview-chess-player
description: Use when an agent needs to play or test the AgentView chess example through the repo CLI, including observe/act/hook turns, Stockfish replies, partial board updates, and daemon shutdown.
---

# AgentView Chess Player

## Overview

Play the AgentView chess example as White against the configured Stockfish-compatible engine. Treat the CLI as the tool surface: observe the VM, choose one legal move, act with rich move context, then hook until the engine reply updates the VM.

## Setup

Run from the repo root:

```bash
cd /home/greygoo/runtime/agentview
cargo build --bin agentview
export AGENTVIEW_ADDR=127.0.0.1:49731
export AGENTVIEW_STOCKFISH_BIN=/usr/games/stockfish
```

Use a fresh `AGENTVIEW_ADDR` port per game/test so hidden daemon state does not leak between runs. If `stockfish` is already on `PATH`, `AGENTVIEW_STOCKFISH_BIN` can be omitted.

## Game Loop

1. Observe the current full VM snapshot:

```bash
target/debug/agentview chess observe
```

2. Read the full `<prompt_board>`, `<legal_moves>`, and `<chess_task>`. Choose exactly one legal UCI move from `<legal_moves>`.

3. Act with context flags plus the canonical UCI move:

```bash
target/debug/agentview chess act --piece P --from e2 --to e4 --uci e2e4
```

For promotion, include the promotion flag before `--uci`:

```bash
target/debug/agentview chess act --piece P --from e7 --to e8 --promotion q --uci e7e8q
```

4. Read the `act epoch=N` response. If the engine field contains `<pending>true</pending>`, wait for the engine reply:

```bash
target/debug/agentview chess hook N
```

5. Read the `hook` update. Continue only when the prompt asks White to choose the next move. Repeat act/hook until the game ends or the user stops.

6. Shut down the hidden daemon when finished:

```bash
target/debug/agentview --__agentview-shutdown
```

## Reading Updates

The first observe is a full `<prompt_board>` render. Later `act` and `hook` responses usually contain `<prompt_board rendering_mode="delta">` with only the fields that changed.

- Maintain the current position from the last full snapshot plus updates.
- Apply `<replace>` by replacing the named field with the value inside it.
- Apply list `<insert>` and `<remove>` operations to the prior list.
- Apply keyed `<update>` operations by replacing the item with the same stable attribute. Board squares use `id`; their text is the current piece symbol, and `.` means empty.
- When `<pending>true</pending>`, do not make another White move. Call `hook` with the latest epoch.
- When `<pending>false</pending>` and the prompt asks for White's next move, pick the next legal move.

## Move Rules

- Always choose a move that appears in `<legal_moves>`.
- Build `--uci` as `from + to + optional promotion`, such as `e2e4` or `e7e8q`.
- Set `--piece` from the source square in the maintained board. White pieces are uppercase: `P`, `N`, `B`, `R`, `Q`, `K`.
- Set `--from` and `--to` to the UCI source and target squares.
- Think privately about candidate moves before acting, but make the CLI call only after deciding. Do not print hidden reasoning as part of the game reply.

## Recovery

If the CLI reports no active chess turn, run `target/debug/agentview chess observe` on the same `AGENTVIEW_ADDR`. If Stockfish cannot be started, set `AGENTVIEW_STOCKFISH_BIN` to the installed binary path, commonly `/usr/games/stockfish` on Debian/Ubuntu.
