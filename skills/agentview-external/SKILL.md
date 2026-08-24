---
name: agentview-external
description: Use when an agent needs to drive AgentView's ExternalApplication through the daemon-backed observe/act CLI, including streamed deltas, completion, EOF, disconnect handling, and full re-render recovery.
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
owns one in-memory `ExternalApplication`; restarting it loses the pending
reaction and rendering baseline. Keep `AGENTVIEW_TOKEN` private and unchanged
for the whole session; the daemon rejects requests from a different token.

## Observe

Start the first reaction or finish the pending reaction with normal Provider
EOF and return the next observation:

```bash
target/debug/agentview observe
```

The JSON response has `kind: "observation"`, `mode`, `generation`, optional
`base_generation`, and `content`. A Full has no base. Apply a Delta only to its
named base generation. Generations recover rendering state and are not action or
observation identities.

To resend the current pending reaction as Full without finishing it:

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

The deltas are dispatched in order. Because this stream has no explicit
completion, normal EOF synthesizes one `text_complete` with `leftright`. The
command waits for all reaction handlers and returns the next observation.

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

After an abnormal act error, call `observe` to start a recoverable next
reaction. Signal writes completed before the disconnect remain committed.

For a one-piece reply, this convenience command sends one delta and ends with
normal EOF through the same adapter:

```bash
target/debug/agentview act 'complete reply text'
```

Do not invent or pass a turn id, action handle, or observation id. The daemon's
single `ExternalApplication` owns current-reaction correlation.
