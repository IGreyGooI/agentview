---
name: agentview-external
description: Use when an agent needs to drive AgentView's Frame-native ExternalApplication through the daemon-backed observe/act CLI, including streamed text, completion, EOF, disconnect handling, and explicit Full recovery.
---

# AgentView External

## Setup

Build the CLI from the AgentView repository and choose one loopback address for
the whole session:

```bash
cargo build --bin agentview
export AGENTVIEW_ADDR=127.0.0.1:47631
export AGENTVIEW_TOKEN="$(openssl rand -hex 32)"
```

The first public command starts the loopback daemon when necessary. The daemon
owns one bounded in-memory business snapshot and the current
`ExternalApplication`; restarting it loses the snapshot, pending reaction, and
target continuity. Keep `AGENTVIEW_TOKEN` private and unchanged for the whole
session; the daemon rejects requests from a different token.

## Observe

Start the first reaction or finish the pending reaction with normal Provider
EOF and return the next observation:

```bash
target/debug/agentview observe
```

The JSON response has `kind: "observation"`, `mode`, `generation`, optional
`base_generation`, and `content`. `content` is the exact canonical Frame
submission accepted by the external queue. A Full has no base; a Delta names
the accepted Frame revision it extends. These generations are the Frame
delivery lineage, not a second CLI-owned rendering baseline or an act token.

To finish the current ingress, advance target continuity, and explicitly start
a new reaction whose Frame is Full:

```bash
target/debug/agentview observe --full-re-render
```

## Act Protocol

Use `act --protocol` for an external text protocol stream. Supply one JSON
object per stdin line. Stdin EOF is normal protocol EOF:

```bash
target/debug/agentview act --protocol <<'EOF'
{"type":"text_delta","text":"left"}
{"type":"text_delta","text":"right"}
EOF
```

The deltas become ordered Provider facts. Because this stream has no explicit
completion, normal EOF seals the accumulated text as `leftright` and completes
the reaction. The command waits for all reaction handlers, consumes the old
application, and returns a Full from a distinct replacement target carrying
the bounded business snapshot. A later `observe` continues normally from that
new target with a Delta.

An explicit completion is optional and stops further frame polling:

```bash
target/debug/agentview act --protocol <<'EOF'
{"type":"text_delta","text":"done"}
{"type":"text_complete","text":"done"}
EOF
```

Use a `disconnect` frame only to report abnormal protocol termination. It
returns an error and never synthesizes completion:

```bash
target/debug/agentview act --protocol <<'EOF'
{"type":"text_delta","text":"partial"}
{"type":"disconnect"}
EOF
```

After an abnormal act error, call `observe` to receive the pending replacement
Full before acting again. Signal writes admitted before the disconnect remain
committed in its bounded snapshot; an unadmitted event is never added. The old
application and ingress are consumed even though the act itself returns an
error.

For a one-piece reply, this convenience command sends one delta and ends with
normal EOF through the same adapter:

```bash
target/debug/agentview act 'complete reply text'
```

Do not invent or pass a turn id, action handle, or observation id. The daemon
correlates an act only with the current ingress whose observation it returned;
it never retries an uncertain act across target rotation.
