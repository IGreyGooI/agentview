# AgentView Chess Runtime Target

Last reviewed: 2026-08-01

## Outcome

One retained Chess component definition must drive both execution modes:

- provider-backed `AgentLoop`, where provider streaming is reduced into the
  typed Chess action;
- external CLI, where a host-owned command envelope is decoded into the same
  canonical `<move uci="..." />` action declared by the System contract.

Both modes must share the same System/User POM, reply contract, domain phase
authorization, legal-move validation, commit semantics, wake behavior, and
delta policy. Only the driver and transport binding may differ.

The target is reached only when a runnable example plays multiple legal moves
against Stockfish. A hard-coded player move or a prompt-only trace is not an
end-to-end proof.

## Current Proof And Remaining Gap

Forgotten City now exposes two engine-owned, runnable session facades. The
external path is `ChessExternalRuntime` / `ChessExternalSession`, backed by one
SQLite transaction authority rather than the AgentView example daemon:

- Actionable and passive-presentation frames are distinct public types;
- System, opaque controller state, immutable User deliveries, acknowledgement,
  reply identity, Chess mutation, Stockfish job, and wake are durable;
- one durable logical consumer id owns the external lineage; it receives
  `InstallSystemOnce` until it acknowledges the System receipt, while reopen
  thereafter exposes only `Attached` and never the System bytes again;
- every Actionable frame carries an opaque serializable action handle. The host
  explicitly acknowledges that exact User delivery when it reaches the
  consumer; `act` requires the same handle and parses the raw XML contract
  before atomically committing the typed move;
- the first Actionable User is full, the Stockfish presentation is full and
  cursor-neutral, and the next Actionable User is a delta whose base is the
  last acknowledged Actionable receipt;
- retry/reopen replays the same receipt and bytes without rerendering the
  current frame;
- `request_user_resync` tombstones a current unacknowledged delta when needed,
  makes only the next Actionable User full, leaves the sole System receipt
  untouched, and resumes delta after that full replacement is acknowledged;
- `hook` only drives Stockfish from `EnginePending`; a newer durable wake is
  replayed immediately, while an unchanged wrong phase or future wake fails
  instead of waiting forever;
- `crates/engine/examples/chess_external.rs` and the repository Chess skill run
  each command in a fresh process with one stable database/session/consumer.
  Their protocol is `attach -> attach-ack -> observe -> ack -> act(handle,
  raw XML) -> hook`; a real `/usr/games/stockfish` trace produced `full ->
  Passive -> delta -> User-only full resync -> Passive -> delta` with exact
  base receipts.

The provider path is exposed as `ChessAgentLoopRuntime` /
`ChessAgentLoopSession` and by `crates/engine/examples/chess_agentloop.rs`:

- `ChessMoveContract`, `ChessAction`, `ChessDiagnostic`, System policy and the
  User POM builder are shared with external control;
- `StreamingXml` emits a revocable Live preview on open. A publication gate
  then requires one exact canonical envelope, one typed Output, and no typed
  diagnostics before Output/Commit may become durable;
- SQLite retains mounted state, provider operation ledger, typed Commit
  outbox, Chess domain, and fenced Stockfish jobs across replacement owners;
- recording OpenAI HTTP tests send one System-bearing conversation Create,
  then full User and structural delta User without a second System, while real
  Stockfish changes the board between provider turns;
- invalid element content, surrounding prose, and a second move abort and
  compensate the Live preview; they enter neither final publication nor the
  Commit outbox, so they cannot advance the provider User baseline;
- deterministic reducer rejection durably stops the failed call and admits a
  successor. A rejected first turn leaves the successor full; a rejection
  after a successful turn retains the last committed baseline. An unclassified
  reducer failure enters `RecoveryRequired` and fences successors.

One session durably claims either `AgentLoop` or `External` control. Reopening
through the same transport is allowed; silently mixing transports is rejected.
There is deliberately no implicit handoff because the two consumers cannot be
assumed to share an acknowledged User baseline.

These are production-shaped vertical slices, not a completed production
installation. Remaining gaps are a recovery-scanned supervisor/server route,
real remote OpenAI credential validation, process-kill fault tests around each
transaction boundary, explicit cross-transport handoff, and one retained
top-level Chess component definition. AgentLoop currently uses the mounted
`chess_feature`, while external control separately projects the same policy,
User builder, and move contract through `external_prompt_component`.

## Runtime Boundaries

The POM AST remains unchanged. Actionability belongs to the compiled/mounted
frame, not to `UserDocument` nodes.

```text
pure component render
        |
        v
compiled frame
  - Actionable: User document plus typed reply channels
  - Passive: presentation/update with no action token
        |
        v
host publication transaction
  - ordered delivery/outbox identity
  - delivery receipt and acknowledgement state
  - optional acknowledged delta baseline
  - actionable reservation, when present
```

The component declares content and typed behavior. The host owns I/O,
persistence, publication, acknowledgement, wake, supersession, and replay.

## Delta Invariants

- The first User delivery to one consumer is full.
- A later delivery is delta only when it names a baseline delivery that the
  same consumer acknowledged.
- On the external path, acknowledgement means that the same logical consumer
  received the immutable Actionable delivery. A later invalid or stale action
  does not erase that delivery fact; domain commit and delivery acknowledgement
  are deliberately separate receipts.
- System attachment acknowledgement and Actionable User acknowledgement are
  separate facts. Neither one substitutes for the other, and Passive delivery
  advances neither User baseline nor action authority.
- Rendering alone never advances the baseline.
- Missing acknowledgement after a safely cancelled delivery or baseline
  mismatch forces full. A replacement process owner reuses durable receipts.
  If the same logical external consumer loses its acknowledged baseline, it
  requests a User-only full resync; this never authorizes another System.
  Replacement logical-consumer and cross-transport handoff remain explicit
  host policy rather than inferred cursor reuse.
- Missing acknowledgement is not evidence of non-delivery. A successor is
  allowed only after the host proves cancellation/tombstoning; ambiguous
  delivery must replay the same receipt or enter recovery.
- A passive presentation sent only to an observer/UI never advances the model
  provider's User baseline.
- Retrying one delivery identity reuses the exact payload and baseline; it does
  not rerender.
- On the provider path, acknowledgement means provider completion, strict
  reducer completion, and successful mounted publication. HTTP acceptance by
  itself never advances the User cursor. A semantic-invalid output aborts Live
  and final publication, retaining the last successfully published baseline;
  the next request is full only when no successful baseline exists, otherwise
  it remains delta from that retained baseline.
- A schema-v3 replacement mounted owner restores the exact retained POM
  baseline and continues delta; legacy schema-v2 state or an explicit provider
  resync starts full. System is still rehydrated rather than resent.
- A published player Commit is a domain-admission barrier. Until its outbox row
  is delivered, a successor User document cannot render from the old board.
  `pending` and `delivering` block capture; `dead_letter` requires explicit
  recovery. None of those states rewinds the already published User cursor.

## Required Chess Trace

| Phase | Frame | Runtime behavior |
| --- | --- | --- |
| Epoch open | System | Render once; host records the sole ordered System receipt. |
| White to move | Actionable User | Deliver full on first contact; expose the typed Chess reply contract. |
| White commit | None | Revalidate phase/revision/legal move and atomically commit domain/state/outbox/wake. |
| Stockfish running | Passive | Publish/store without an action token; wait for durable wake. |
| Stockfish result | None | Commit engine move or failure and supersede the passive frame. |
| White to move again | Actionable User | Deliver delta from the last acknowledged model baseline, otherwise full. |

## Acceptance Matrix

| Requirement | External CLI | AgentLoop |
| --- | --- | --- |
| One System per durable epoch | Consumer attach/ack proven in SQLite; reopen returns only its receipt and no System bytes | Proven locally and over recorded OpenAI HTTP |
| Same User POM and Chess semantic contract | Proven | Semantic contract proven; top-level binding is still separately projected in Forgotten City |
| Typed legal action commit | Proven by shared CLI adapter plus host revalidation | Proven by strict streaming reducer, typed outbox, stable-id domain dedup and authoritative legal-move revalidation |
| Passive Stockfish wait has no action token | Proven | Observer lane still needs final application binding |
| User delivery receipt and idempotent replay | Explicit User ack, exact action handle, durable receipt/reply ledger and process reopen proven | Durable call/outbox replay plus apply-before-ack domain dedup proven; no provider delivery receipt type yet |
| Acknowledged delta after first full User | Explicit consumer delivery ack proven | Completion + strict output gate + publication ack proven |
| Reconnect/reopen baseline | SQLite controller/delivery lineage and User-only resync proven | SQLite/OpenAI recording proof of v3 delta continuation; v2/provider-resync full fallback |
| Stockfish failure returns a visible recoverable frame | Proven | Failure is durably stored as `EngineFailed`; provider observer lane is not yet bound |
| Multiple legal moves against real Stockfish | Proven through durable cross-process CLI/skill | Proven with recording/scripted provider and real Stockfish; real remote provider credentials not yet run |

## Implementation Order

1. [done locally] Separate action reservation from passive presentation and
   add durable publication/supersession.
2. [done locally] Move User publication behind the host and add delivery
   receipt, acknowledgement, replay, and one-consumer baseline state.
3. [done locally] Migrate the repository CLI/skill to mounted external control,
   including successful/failing Stockfish, replay and consecutive delta tests.
4. [done locally] Share the typed Chess semantic contract and add a provider
   AgentLoop with strict real-time reducer, Commit staging and replay-safe
   outbox publication.
5. [done locally] Run multi-move real-Stockfish games through both drivers and
   complete independent protocol/API reviews.
6. [done locally] Bind Chess to the OpenAI Conversations adapter, SQLite typed
   outbox, idempotent domain repository, and durable Stockfish jobs. Admission
   blocks stale User capture and surfaces player dead letters.
7. [done locally] Add engine-owned AgentLoop and external session facades; the
   examples consume typed outcomes without exposing AgentView persistence
   types.
8. [done locally] Move the external CLI/skill to SQLite process reopen; add
   durable logical-consumer identity, System attach/ack, exact action handles,
   explicit User delivery ack, and User-only baseline resync. Replacement-
   consumer and cross-transport handoff are still deliberately open.
9. [next] Install recovery-scanned supervision and server routing, validate a
   real remote provider, then collapse both driver projections behind one
   retained Chess component definition before freezing the authoring API.
