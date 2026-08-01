---
name: agentview-chess-player
description: Use when an agent needs to play or test the durable AgentView Chess application through Forgotten City's SQLite CLI, including System attach, User delivery acknowledgement, exact-handle act/hook, User resync, Stockfish replies, and prompt deltas.
---

# AgentView Chess Player

## Overview

Play the mounted AgentView Chess application as White against a configured
Stockfish-compatible engine. The tool surface is Forgotten City's durable
external facade: observe the current frame, choose one legal UCI move, act,
then hook while Stockfish owns the turn. System attachment and User delivery
acknowledgement happen explicitly before those actions.

SQLite retains the mounted System epoch, immutable User deliveries, reply
ledger, Chess domain, Stockfish jobs, wake cursor, and AgentView delta state.
Every CLI invocation may run in a new process. The first Actionable User is
full. The host explicitly acknowledges the exact Actionable delivery after it
has reached this logical consumer; only that receipt may become a delta
baseline. `act` then answers the same opaque action handle with the canonical
XML reply. The Stockfish waiting frame is Passive and full; it never becomes a
prompt baseline. A later Actionable frame is a delta from the last acknowledged
Actionable delivery.

## Setup

Run from the Forgotten City repo:

```bash
cd /home/greygoo/runtime/forgotten-city
cargo build -p engine --example chess_external
export FORGOTTEN_CITY_CHESS_DATABASE_URL=sqlite:///home/greygoo/runtime/forgotten-city/target/agentview-chess-skill.sqlite3
export FORGOTTEN_CITY_CHESS_SESSION=agentview-chess-player/example-1
export FORGOTTEN_CITY_CHESS_CONSUMER=agentview-chess-player/consumer-1
export AGENTVIEW_STOCKFISH_BIN=/usr/games/stockfish
```

Use a fresh `FORGOTTEN_CITY_CHESS_SESSION` for a new game. Reuse the same
database URL, session, and consumer id to reopen an existing game. The consumer
id is durable protocol identity, not a process id: a different consumer cannot
inherit the acknowledged delta lineage. Do not alternate this external CLI and
AgentLoop on one session: the durable domain intentionally fences mixed control
transports. If `stockfish` is on `PATH`, its path may be adjusted accordingly.

## Attach System Once

Before observing User frames, inspect the durable System attachment:

```bash
target/debug/examples/chess_external attach
```

For a new consumer this returns `status: "install_system_once"`, a
`delivery_id`, and the System `document`. Deliver that document once to the
consumer's retained model conversation. Only after that delivery succeeds,
acknowledge the exact receipt:

```bash
target/debug/examples/chess_external attach-ack <system-delivery-id>
```

Reopening with the same consumer then returns `status: "attached"` and the
receipt only; System bytes are intentionally absent. Never synthesize another
System message from that receipt. `observe`, `ack`, `act`, `hook`, and `resync`
are fenced until System is acknowledged.

## Game Loop

1. Observe the current frame:

```bash
target/debug/examples/chess_external observe
```

2. Continue only when `kind` is `actionable`. Retain its exact `action_handle`.
   On the first turn, `prompt_mode` is `{"mode":"full"}`. For a delta, verify
   that `prompt_mode.base_delivery` names the Actionable baseline retained by
   this consumer and apply the update as described below. If that baseline is
   unavailable, do not acknowledge the frame; use `resync` instead.

3. Once this consumer has actually received and accepted the full or applicable
   delta document, acknowledge that exact immutable delivery:

```bash
target/debug/examples/chess_external ack <action-handle>
```

Only acknowledged Actionable deliveries advance the delta baseline. Repeating
the same `ack` is safe and returns `already_acknowledged`. Passive frames have
no action handle and must never be acknowledged as a prompt baseline.

4. Read `snapshot.legal_player_moves` and choose exactly one listed UCI move.
   Submit the canonical XML reply against the same action handle:

```bash
target/debug/examples/chess_external act <action-handle> '<move uci="e2e4" />'
```

For promotion, append the lower-case promotion piece inside `uci`, for example
`<move uci="e7e8q" />`. Bare `e2e4`, surrounding prose, multiple moves, stale
handles, and illegal moves are rejected. Retrying the same handle with the
same raw XML is an idempotent replay; reusing it with different reply bytes is
a collision.

5. A successful `act` normally returns a full Passive frame with
   `phase: "engine_pending"`. Drive or join the durable Stockfish job:

```bash
target/debug/examples/chess_external hook
```

6. Read the returned Actionable frame. Repeat `ack`, `act`, and `hook` until the
   snapshot phase is terminal or the user stops.

There is no daemon to shut down. Each command closes normally; durable state
remains in SQLite.

## Reading Updates

The first observe is a full Actionable `<agent_context>`. `act` returns a full
Passive `<agent_context>` while Stockfish is pending. A successor Actionable
frame can contain `<agent_context rendering_mode="delta">` with only the
fields that changed. Its `prompt_mode.base_delivery` identifies the exact
acknowledged Actionable receipt to which the delta applies. Delta is the normal
steady-state protocol, not an optional display optimization.

Keep two views of state:

- The **Actionable prompt baseline** is the last Actionable frame acknowledged
  by `ack` (or by the equivalent host delivery acknowledgement). Apply a later
  Actionable delta only to the receipt named by
  `base_delivery`, then retain the reconstructed result as the next baseline.
- The **Passive presentation** is a complete status display only. Read it to see that Stockfish is pending, but never promote it to the Actionable delta baseline or apply the next Actionable delta on top of it.

- For the first `hook` delta, apply it to the initial Actionable observe frame,
  not the full waiting response from `act`.
- Apply `<replace>` by replacing the named field with the value inside it.
- Apply list `<insert>` and `<remove>` operations to the prior list.
- Apply keyed `<update>` operations by replacing the item with the same stable attribute. Board squares use `id`; their text is the current piece symbol, and `.` means empty.
- When the snapshot phase is `engine_pending`, do not make another White move;
  call `hook`.
- When the snapshot phase is `player_turn`, choose from the newly published
  legal-move list.

## Move Rules

- Always choose a move that appears in `<legal_moves>`.
- Build the XML `uci` attribute as `from + to + optional promotion`, such as
  `e2e4` or `e7e8q`.
- Think privately about candidate moves before acting, but make the CLI call only after deciding. Do not print hidden reasoning as part of the game reply.

## Recovery

If this consumer has lost the Actionable baseline named by an unacknowledged
delta, request a User-only resync:

```bash
target/debug/examples/chess_external resync
```

This may tombstone the current unacknowledged delta and then publish a full
Actionable replacement. It does not reinstall or redeliver System. A frame
already acknowledged as delivered cannot be tombstoned: replay it and finish
or recover that exact action instead. After the replacement full frame is
explicitly acknowledged and acted on, later Actionable frames resume delta.
Do not use resync as a routine refresh; ordinary `observe` replays the exact
current receipt and bytes.

If a command is interrupted, rerun `attach` and then `observe` with the same
database, session, and consumer. An attached consumer receives no second
System bytes. If the current frame is Passive, rerun `hook`; durable claims and
reply identities prevent duplicate Chess moves. If Stockfish cannot start,
set `AGENTVIEW_STOCKFISH_BIN` to the installed binary path, commonly
`/usr/games/stockfish` on Debian/Ubuntu.
