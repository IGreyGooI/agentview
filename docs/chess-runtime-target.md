# AgentView Chess Runtime Target

Last reviewed: 2026-08-01

## Outcome

The target is one retained Chess component definition driving three
deliberately different surfaces:

- AgentView's runnable external reference: `target/debug/agentview chess ...`;
- AgentView's scripted provider-backed AgentLoop reference;
- Forgotten City's SQLite-backed consumer integration and durability tests.

All three already share the System/User POM, XML move contract, domain-phase
authorization, legal-move validation, Actionable/Passive distinction, and delta
rules. Their top-level external and provider bindings are still separate, and
they do not make the same persistence claim.

The canonical playable entrypoint is AgentView's own CLI and repository skill.
Forgotten City validates the consumer-shaped SQLite integration; it is not the
user-facing Chess example command.

## Runnable References

### AgentView external CLI and skill

Build the binary in this repository:

```bash
cargo build --bin agentview
target/debug/agentview chess attach
target/debug/agentview chess attach-ack <system-delivery-id>
target/debug/agentview chess observe
target/debug/agentview chess ack <action-handle>
target/debug/agentview chess act <action-handle> '<move uci="e2e4" />'
target/debug/agentview chess hook
target/debug/agentview chess resync
```

The binary starts a loopback daemon when needed. Independent CLI client
processes share one in-memory reference session while they use the same
`AGENTVIEW_ADDR` and the daemon remains alive. This proves the public command
shape and in-process lifecycle, including:

- `attach` returns one System document until `attach-ack` confirms its exact
  receipt; later attachment in the same daemon returns only that receipt;
- an Actionable User prompt carries an exact serializable action handle;
- `ack` explicitly records delivery of that Actionable prompt before it becomes
  a delta baseline;
- `act` binds the same handle to the raw `<move uci="..." />` XML contract;
- a Passive Stockfish frame is complete but cursor-neutral; `hook` waits for
  its successor;
- `resync` replaces an unacknowledged delta with one full User prompt and does
  not redeliver System.

CLI view replies use `kind: "chess_frame"`; `delivery_receipt`,
`action_handle`, `prompt_mode`, `prompt`, and `view` belong to the nested
`frame` object. The skill and any caller must read those nested fields rather
than assuming a flat response shape.

The daemon is an in-memory reference host. Its state may span CLI subprocesses,
but it is lost when the daemon exits: System attachment, delivery receipts,
handles, delta cursor, board, and engine work are not durable. A new daemon
requires a fresh System attach/ack and must not be treated as a reopen or
resync of the former session.

### AgentView scripted AgentLoop

`examples/chess_engine_mounted_agentloop.rs` is the provider counterpart. It
uses a scripted provider and the same XML contract to prove real-time
`StreamingXml` reduction, revocable Live output, the exact-envelope/one
Output/no-Diagnostic publication gate, typed Commit staging, and first-full then
committed-delta behavior. It can use a configured Stockfish-compatible engine
between provider turns, but it is not a real remote-provider installation.

### Forgotten City consumer integration

Forgotten City's Chess code is the production-shaped consumer integration, not
the canonical example interface. Its SQLite tests exercise the same protocol
under durable System/User delivery, reply ledger, domain transaction, Stockfish
job, outbox, wake, reopen, and replacement-owner conditions. Those tests are
where durable logical-consumer identity, process reopen, reply replay/collision,
and storage failure behavior belong.

The remaining gaps are managed remote transport and recovery supervision,
process-kill fault coverage, real remote-provider credentials, explicit
cross-transport handoff, and one retained top-level component source shared by
the two drivers.

## Runtime Boundaries

The POM AST remains unchanged. Actionability belongs to the compiled/mounted
frame, not to `UserDocument` nodes.

```text
pure component render
        |
        v
compiled frame
  - Actionable: User document plus typed reply channel
  - Passive: presentation/update with no action handle
        |
        v
host publication
  - System/User delivery identity
  - acknowledgement state
  - optional acknowledged delta baseline
  - actionable reservation, when present
```

The component declares content and typed behavior. The host owns I/O,
publication, acknowledgement, wake, supersession, and replay. AgentView's CLI
reference keeps that host state in daemon memory; Forgotten City's consumer
port persists it in SQLite.

## Delta Invariants

- The first Actionable User delivery for a host/consumer lineage is full.
- A later Actionable delivery is delta only when it names an acknowledged
  Actionable baseline from that same lineage.
- System attachment acknowledgement and User delivery acknowledgement are
  separate facts. Passive delivery advances neither action authority nor the
  User baseline.
- Rendering alone never advances a baseline. Exact retry reuses a delivery's
  bytes and base rather than rerendering it.
- Cancellation or resync can replace an unacknowledged Actionable delta with a
  full successor. After that successor is acknowledged, later Actionable frames
  may resume delta. Resync never adds another System delivery in the same epoch.
- The AgentView CLI reference enforces these rules only while its daemon lives.
  Daemon exit destroys the lineage; no receipt, handle, or delta can be reused.
- Forgotten City's SQLite integration additionally proves the durable form:
  same-consumer reopen, receipt/reply replay, domain/outbox fencing, and
  User-only resync without a second System.
- On the scripted/provider path, baseline advancement requires provider
  completion, strict reducer completion, and successful publication. Invalid
  XML aborts Live/final publication and retains the preceding baseline.

## Required Trace

| Phase | AgentView external reference | Scripted AgentLoop | Forgotten City integration |
| --- | --- | --- | --- |
| Epoch open | `attach`, deliver System once, then `attach-ack` | Mount one System for the scripted call sequence | Persist/attach one System receipt in SQLite |
| White to move | `observe` returns a full Actionable prompt and handle | First User request is full | Durable full User delivery |
| User receipt | `ack <handle>` promotes the reference baseline | Strict completion/publication promotes provider baseline | Durable consumer acknowledgement promotes baseline |
| White commit | `act <handle> '<move ... />'` | Stream reduction stages typed Commit | SQLite transaction revalidates and commits |
| Engine pending | Full Passive frame; `hook` waits | Scripted/provider turn waits for engine result | Durable wake/job handling |
| Next White turn | Delta from acknowledged prompt while daemon lives | Delta from committed provider baseline | Durable delta after reopen when valid |
| Daemon/process loss | Fresh reference session, not recovery | Local example proof only | Durable recovery/reopen is tested |

## Acceptance Matrix

| Requirement | AgentView external CLI + skill | AgentView scripted AgentLoop | Forgotten City consumer integration |
| --- | --- | --- | --- |
| Canonical playable command | `agentview chess ...` | Example executable | No public example entrypoint |
| One System | Once per live daemon epoch | Once per scripted mounted epoch | Once per durable SQLite epoch/reopen lineage |
| Exact User action | Explicit `ack`, handle, raw XML | Strict streaming XML contract | Durable receipt, replay/collision, and revalidation |
| Passive frame | Full and non-advancing | Model-visible effect/publication behavior | Durable presentation/wake proof |
| Full/delta/resync | Reference proof until daemon exit | Provider cursor proof | Durable reopen and User-only resync proof |
| Crash/process recovery | Not supported | Not supported | Consumer integration tests only; managed production service remains open |

## Implementation Order

1. [done locally] Keep AgentView's `agentview chess` daemon and
   `agentview-chess-player` skill as the canonical playable external reference.
2. [done locally] Keep `chess_engine_mounted_agentloop` as the scripted
   provider/reference counterpart with real-time reducer and Commit proof.
3. [done locally] Use Forgotten City only for SQLite consumer integration,
   durable reopen/replay/resync, domain/outbox, and Stockfish-job tests.
4. [next] Add managed remote transport/supervision, real provider validation,
   process-kill coverage, explicit transport handoff, and a single retained
   Chess component source before freezing the authoring API.
