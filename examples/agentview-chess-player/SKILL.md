---
name: agentview-chess-player
description: Use when an agent needs to play or test AgentView's daemon-backed in-memory Chess reference through the agentview CLI, including System/User acknowledgement, exact-handle XML actions, hooks, resync, and prompt deltas.
---

# AgentView Chess Player

## Scope

Play the mounted AgentView Chess reference as White against a configured
Stockfish-compatible engine. The public surface is AgentView's own
`agentview chess` CLI, not Forgotten City's SQLite CLI.

The first CLI call starts a loopback daemon when necessary. Separate CLI client
processes share its in-memory Chess session while that daemon remains alive, so
`attach`, `observe`, `ack`, `act`, `hook`, and `resync` can be issued from
separate subprocesses. This is a runnable reference host, not durable storage:
killing or restarting the daemon loses the System receipt, User deliveries,
action handles, delta baseline, board, and Stockfish work. After a daemon loss,
start a fresh `attach` flow; never reuse an old receipt or handle.

Forgotten City's SQLite Chess implementation remains a consumer integration and
durability test. It is not the entrypoint for this skill.

## Setup

Run from the AgentView repo:

```bash
cd /home/greygoo/runtime/agentview
cargo build --bin agentview
export AGENTVIEW_STOCKFISH_BIN=/usr/games/stockfish
```

All commands in one game must use the same loopback daemon address. The default
is `127.0.0.1:47631`; set `AGENTVIEW_ADDR` to isolate concurrent games:

```bash
export AGENTVIEW_ADDR=127.0.0.1:47631
```

If `stockfish` is on `PATH`, `AGENTVIEW_STOCKFISH_BIN` may name it directly.

## Attach System Once

Before observing User frames, retrieve the daemon epoch's System delivery:

```bash
target/debug/agentview chess attach
```

The first attachment returns a `delivery_id` and System document. Deliver the
document once to the retained model conversation, then acknowledge that exact
delivery:

```bash
target/debug/agentview chess attach-ack <system-delivery-id>
```

Within the same daemon lifetime, a later `attach` returns the attached receipt
without another System document. `observe`, `ack`, `act`, `hook`, and `resync`
are fenced until System acknowledgement succeeds.

## Game Loop

1. Observe the current frame:

```bash
target/debug/agentview chess observe
```

2. A view response has `kind: "chess_frame"`; read protocol fields from its
   nested `frame`. Continue only when `frame.action_handle` is non-null; an
   Actionable frame then also has `frame.prompt_mode`. Retain those values and
   `frame.delivery_receipt` exactly. The User document is `frame.prompt`. The
   first Actionable prompt is full. A later Actionable prompt uses delta when
   its acknowledged baseline is available: apply it to the named
   `base_delivery`; if that baseline is unavailable, do not acknowledge the
   frame and use `resync`.

3. Once the full prompt or reconstructed delta has reached the consumer,
   explicitly acknowledge that immutable delivery:

```bash
target/debug/agentview chess ack <action-handle>
```

Only an acknowledged Actionable delivery advances the delta baseline. A Passive
frame has no action handle and never becomes that baseline.

4. Choose one UCI move from the prompt's legal move list and submit the raw XML
   reply against the same handle:

```bash
target/debug/agentview chess act <action-handle> '<move uci="e2e4" />'
```

For promotion, use a lower-case promotion suffix, for example
`<move uci="e7e8q" />`. Bare UCI, surrounding prose, multiple moves, stale
handles, and illegal moves are rejected. Repeating the same handle and exact
raw XML is replay-safe; changing reply bytes for that handle is a collision.

5. A successful action normally publishes a full Passive `engine_pending`
   presentation. Wait for the next view:

```bash
target/debug/agentview chess hook
```

6. Repeat `observe` or `hook`, then `ack` and `act`, until the game is terminal
   or the user stops.

## Reading Updates

The initial Actionable `<agent_context>` is full. The Stockfish waiting frame is
a full Passive presentation. A later Actionable frame normally contains
`<agent_context rendering_mode="delta">`; its `base_delivery` identifies the
acknowledged Actionable prompt to which the update applies. Delta is the normal
steady-state delivery protocol, not a display-only optimization; full is used
for the initial delivery and the explicit resync fallback.

- The **Actionable prompt baseline** is the last Actionable delivery confirmed
  by `ack`. Apply a later delta only to its named baseline, then retain the
  reconstructed result as the next baseline.
- The **Passive presentation** is a complete status display. Never promote it
  to the prompt baseline or apply a later Actionable delta on top of it.
- Apply `<replace>` by replacing the named field, list `<insert>`/`<remove>` to
  the prior list, and keyed `<update>` to the item with the stable attribute.
- When the phase is `engine_pending`, call `hook` rather than making
  another White move.

## Recovery

If the current daemon is alive but this consumer has lost the baseline required
by an unacknowledged delta, request one User-only full replacement:

```bash
target/debug/agentview chess resync
```

Resync may tombstone the current unacknowledged delta, emits one full
Actionable prompt without another System delivery, and resumes delta only after
the replacement is acknowledged. It is not a routine refresh.

If a CLI client command is interrupted, rerun the command against the same
`AGENTVIEW_ADDR`; the daemon retains the current reference state. If the daemon
has exited, the reference state is gone: use `attach`, `attach-ack`, and a new
game flow rather than `resync`.
