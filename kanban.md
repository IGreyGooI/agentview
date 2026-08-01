# AgentView POM Component Direction and Roadmap

Last reviewed: 2026-08-01

## Current Checkpoint

- The active acceptance target is now the dual-driver Chess runtime described
  in [`docs/chess-runtime-target.md`](docs/chess-runtime-target.md): one shared
  component/contract must play multiple legal moves against Stockfish through
  both provider-backed AgentLoop and external CLI. The target includes typed
  Actionable/Passive frames, host-owned User delivery, acknowledged delta,
  wake supersession, replay, reopen, and visible engine-failure recovery. A
  prompt-only trace or hard-coded `e2e4` does not satisfy this target.

- The dual-driver Chess target now has a local executable proof. The mounted
  external CLI/skill has played multiple rounds against real Stockfish with
  `System attach/ack -> full Actionable ack -> full Passive -> delta
  Actionable`. A stable logical consumer id owns that lineage across process
  reopen; every Actionable frame exposes an exact serializable handle, and the
  CLI submits the real raw XML reply contract against that handle. The external
  `resync` command intentionally retains the delta model: it may tombstone an
  unacknowledged Actionable delta, emits one User-only full replacement without
  another System delivery, and resumes delta only after that replacement is
  acknowledged. An already acknowledged prompt cannot be tombstoned. A fresh
  cross-process skill run on 2026-08-01 played `e2e4 c7c5 g1f3 b8c6` and
  observed `full -> delta -> resync full -> explicit ack -> delta`; the final
  `base_delivery` was the resync full receipt, not either Passive frame.
- The provider example uses the same semantic move contract, a scripted
  provider, a strict streaming/publication gate, typed Commit staging,
  replay-safe local outbox, and real Stockfish between provider turns. Invalid
  content, multiple moves, and surrounding prose compensate Live and produce
  zero final publications, so they cannot advance the provider delta baseline.
- Forgotten City now binds a concrete Chess mounted component to the recording-
  server OpenAI Conversations adapter and SQLite typed outbox. Focused traces
  prove one System-bearing Create, same-owner and schema-v3 replacement-owner
  `full -> delta` without a second System, strict semantic rejection with Live
  compensation, and no cursor/outbox advancement on rejection. A legacy
  schema-v2 row safely begins with one full User resync before rewriting as v3.
  The SQLite outbox now also has fenced claim/lease/ack, retry and dead-letter
  primitives. A separate durable Chess aggregate now applies each player row
  idempotently by stable `OutboxItemId`, creates fenced/retriable Stockfish jobs
  in the same transaction, and atomically commits engine moves. Pending and
  delivering player rows block another User capture; dead letters survive
  reopen as an explicit recovery condition, so the delta cursor cannot race
  ahead of the domain board. Unknown legacy/corrupt outbox statuses also fail
  closed and are constrained by schema/migration guards. Recovery-scanned
  supervision, passive failure publication, daemon restart recovery,
  replacement-consumer resync, and real remote-provider validation remain open.

- Feature audit: 4 `implemented`, 4 `local-proof`, 7 `partial`, and 1
  `internal-proof`; no production consumer migration is complete.
- AgentView's default workspace is green: 301 library tests, 38 public mounted
  authoring tests, and all default workspace targets pass. The public local
  mounted facade is usable as a process-local `Commit = Never` proof, not as a
  production durable host.
- Forgotten City is green at the consumer-shaped proof boundary: engine
  99 passed with 1 ignored real-Stockfish adapter test, mounted SelectIntent
  21/21, mounted Chess 20 passed with that 1 ignored, and the stateful-provider
  scaffold 8/8. The scaffold now also proves that a rehydrated remote session
  may ask for one full-User resync after seeing a normal delta, without a second System
  attach, including a PlayerRuntime-generated call. The opt-in PlayerRuntime
  branch now proves joined cancellation, stale Live withdrawal, and that its
  `Indeterminate` cancellation fences a different
  successor call without a second capture/User/provider execution by entering
  durable recovery. Its domain-issued `DurableYourTurn` path now derives the
  mounted call from a serialized interaction UUID and player-turn sequence,
  rather than a SlotMap key or runtime generation.
- Forgotten City's opt-in mounted selector now asks a session-scoped host for
  an opaque owner by `InteractionSessionId`; it no longer models one process-
  global mounted agent as if it could own every interaction. The seam is
  covered both by a runtime test that observes the persisted domain ID and by
  a `GameEngine` route test that sends `DurableYourTurn` through
  `WorldApi.event_tx -> EventDispatcher -> PlayerEventRouter -> PlayerRuntime`
  before observing the host open. It remains
  deliberately uninstalled in `GameEngine::new`: the current Rig executor is
  stateless and constructs a fresh request with System bytes on every turn, so
  wrapping it would violate the durable System-once contract.
- `GameEngineConfig::mounted_openai` now provides an explicit production-shaped
  Player SelectIntent composition: a session-scoped mounted owner backed by a
  SQLite CAS store and persisted OpenAI operation ledger. Recording-server
  tests prove one remote System attach, ordinary User-only requests, owner
  cache/reopen, and no second System on replacement-host rehydrate. A second
  trace proves `PlayerRuntime` cancellation waits for the remote response
  cancellation before clearing its selection; a third drives a completed
  player turn through a replacement host and confirms no second System attach.
  This is not yet a production-complete migration: it has no real-provider
  validation, transactional domain/outbox policy, or indeterminate-turn
  recovery golden trace.
- Cube Stage Director now compiles through the current POM path: its policy is
  authored as a System document, its current projected state is a User
  document, and its frame implements `AgentViewValue`. `cargo check
  --all-targets` and `cargo test --lib --quiet` pass (80 passed, 18 ignored).
  This is deliberately only a POM authoring migration: the legacy provider
  executor and `ToolServer` sink still own its runtime loop, so it does not yet
  prove mounted System-once transport or durable tool publication.
  PostgreSQL-backed Forgotten City recovery tests remain environment-gated.
- API freeze remains rejected. The provider contract carries typed
  `Unchanged`, `ResumeFrom`, or `Indeterminate` cancellation cursor authority;
  requested/provider reason mismatch is downgraded conservatively. After
  joined Provider/Live cleanup, the managed owner and both local stores now
  execute an exact session/epoch/call/input/turn/request/lease/revision
  settle-or-recover transition. Focused tests prove `Stopped` successor
  release, cursor-preserving successor/reopen, indeterminate recovery fencing,
  provider cancellation timeout, stale-revision conflict, and pending
  publication recovery, epoch replacement, invalid persisted cursor, lease
  expiry, foreign-fence rejection, reason mismatch and invalid `ResumeFrom`.
  The real process-local `InMemoryMountedStore` now has direct parity for those
  faults and exact recovery retry. A crate-private reconciliation controller now
  claims a durable recovery fence, reconstructs the exact provider operation
  from the ledger, and restores only a strongly proven `NeverAccepted` New or
  continuation checkpoint; `Running`, `Completed`, `Cancelled`, and `Unknown`
  preserve `RecoveryRequired` after releasing the claim. Store and durable-CAS
  reload tests cover stale fences, claim/release persistence, and prove that
  recovery does not replay provider, tool, reducer, or Live work. The remaining
  `AV-F12` gates are public id-keyed recovery/control and production
  lease/transport policy. Borrowed raw execution is now a test-only module
  helper; production callers can only use the detached owner actor.
- Provider-complete reducer failures now have an explicit conservative
  settlement contract. `SessionReducer::Err` defaults to `Indeterminate`, which
  compensates Live, publishes no session/User cursor/outbox, durably enters
  `RecoveryRequired`, and fences successors. A reducer may opt into `Reject`
  only for deterministic errors derived from immutable captured/provider data;
  that path durably stops the call and allows a successor while retaining the
  last successfully published delta baseline. Tests cover first-turn reject ->
  full, committed baseline -> reject -> delta from the old baseline, and a
  computed candidate delta cursor remaining tentative during recovery. The API
  remains provisional: `Stopped` does not yet replay the original rejection
  receipt, and cleanup-failure diagnostics need a stronger durable envelope.
- With the local `AV-F12` matrix complete, the release path is a real
  transactional store/outbox policy and real stateful-provider validation
  (`AV-F13`/`AV-F14`), complete Forgotten City selector legacy/mounted golden
  traces (`AV-F16`), and only then an API-freeze decision after independent
  review of the production migration.
- `MountedFeature::map_channels` now projects feature-local contracts into a
  harness root and has an external streaming Live proof. `TurnLoopPolicy` now
  makes the `Wait`/`Continue` limit harness-owned; a `MountedCallInput` may
  only add a smaller `with_turn_cap`. Forgotten City's selector declares its
  legacy three-turn policy and the public lifecycle test proves a larger caller
  cap cannot exceed a two-turn harness. The policy is persisted in the epoch
  manifest and artifact fingerprint, so reopening the same durable epoch with
  a changed policy returns `MountedOpenError::ContractMismatch` without a
  second System render or provider attachment. The retained `MountedFeature`
  identity gate now covers positional siblings, duplicate keys, keyed
  Create/reopen, key drift, and keyed parents with multiple User children.
- The mounted `hello_world` now keeps typed System/User POM in one
  `hello_agent` component. The prompt-only `PromptComponent<Props>` /
  `prompt_component(system, |props| user)` facade removes empty channel types,
  durable carrier assembly, and `UserTurnContext` from the beginner path while
  preserving `MountedFeature` underneath. `try_prompt_component` retains the
  fallible User POM boundary; a public black-box test
  proves such an error stops during preparation before provider execution. The
  executable trace still proves one System attachment and two freshly rendered
  User turns. Example-only provider, capture, reducer, and local-store plumbing
  remains isolated in the shared
  `examples/support/mounted_prompt_trace.rs`; even the direct-props capture is
  host-owned and no longer appears in application authoring. This is an
  ergonomics proof, not a production-host claim. A fresh consumer rewrite
  briefly added a complete typed streaming declaration to this file; an
  independent review rejected that shape because the trace host emits no model
  tokens and the extra channels/reducer/runtime identity made the baseline
  teach unobserved behavior. `hello_world` is therefore intentionally
  prompt-only; streaming starts at the next example tier.
- An independent consumer rewrite has reduced the runnable
  `chess_engine_mounted` path to one `chess_agent` component, one `mount`, and
  two `run_turn` calls. Its prompt-only component now uses the same
  `PromptComponent` lifecycle shape as Hello World, with
  `try_prompt_component` adapting the existing pure chess User-document
  builder. It captures the board before and after `e2e4`, attaches
  a 786-byte System once, and renders 5124/4100-byte User turns. The exact
  legacy `ChessViewModel` comparison now lives in the example's `#[cfg(test)]`
  compatibility module, so the proof remains without obscuring authoring. It
  deliberately does not replace `chess_engine_agent`: the new mounted
  external controller now supplies the local action/reply lifecycle contract,
  but Forgotten City still needs a real transactional port and consumer
  migration before this example can become end to end.
- A separate author-only rewrite now lives in
  `examples/chess_agent_mounted_turn.rs`. It imports only
  `agentview::component::prelude` and puts the durable System POM, per-turn User
  POM, typed streaming XML contract, and synchronous pure reducer in one file.
  Provider, persistence, Live delivery, and mounted call supervision stay in
  the shared example host. That host drives one XML move in two split token
  chunks through the public fallible provider wire. Its trace proves the typed
  Live is awaited between the second chunk's submission and acknowledgement,
  while the terminal mounted outcome retains the matching typed Output. This is the
  recommended streaming example after prompt-only `hello_world`; it still does
  not implement the complete chess `observe -> act -> hook` interaction loop.
- A fresh independent attempt to rewrite the complete `chess_engine_agent`
  rejected a provider/channel workaround as semantically false. The public
  mounted facade at that point had no externally observable User
  snapshot/action token or reply ingress, and its pure `SessionReducer` could
  not atomically mutate `ChessGameSource`. That review produced the acceptance
  contract now implemented by the isolated `MountedExternalController`; the
  real chess host still has not supplied its transactional port, so
  `chess_engine_mounted` remains a prompt-lifecycle proof only.
- `chess_engine_mounted_external` now directly reproduces the compatibility
  chess flow: one System render, initial User, typed `e2e4` commit, a visible
  post-player waiting User, asynchronous real Stockfish execution, and a final
  wake-driven User. Its deterministic UCI test locks the same three domain
  states with `e2e4/e7e5`. Waiting is now a passive presentation with no action
  token. Both frame kinds publish immutable User bytes through the host CAS and
  return only an `ExternalUserDeliveryReceipt`; `act` is fenced until explicit
  consumer acknowledgement. The trace is full Prompt -> full cursor-neutral
  presentation -> delta Prompt based on the first receipt. Reopen/retry replays
  exact receipt/bytes, while safely cancelling an unacknowledged prompt forces
  the successor full. Missing Stockfish publishes a passive full recovery view
  with `last_error` before reporting failure and never exposes a black-to-move
  action token. `MountedExternalChessCli` and the repository skill now extend
  this into an interactive multi-move local proof. Its mutex-backed host remains
  a teaching fake, not durable transport/database or cross-process recovery.
- `chess_engine_mounted_agentloop` now supplies the provider counterpart: the
  shared System/User POM and `ChessMoveContract` drive a real-time Live preview,
  exact-envelope/one-Output/no-Diagnostic gate, typed Commit, atomic local
  outbox publication, replay, full first User and committed delta second User.
  Deterministic semantic rejection now durably retires its call without
  advancing the User cursor/outbox; tests cover both reject-before-baseline ->
  full and reject-after-commit -> delta from the last good baseline. A real
  Stockfish move separates the provider turns. Forgotten City additionally
  runs the same mounted Chess shape through its OpenAI Conversations adapter
  and SQLite outbox, but the executable example is still scripted-provider and
  neither path yet has a production Chess domain transaction.
- A second independent component author wrote
  `examples/mounted_author_review.rs` from the public surface without copying
  host internals. It converged on essentially the same prompt-only shape as
  `hello_world`: one typed System document, one typed User document, one
  prompt component, one epoch id, one mount, and two turns. That negative
  ergonomics evidence motivated the narrow `PromptComponent` facade now used
  by both examples. Before API freeze, ordinary component authors still need a
  curated import surface while
  capture, reducer, provider, persistence, and durable-call identity remain a
  separate host-integration layer. `component::prelude` and the prompt-only
  facade now implement that split without making beginner code name
  `NoTurnChannels`; a positive external test builds a
  prompt-only mounted component from the narrow prelude, while compile-fail
  fixtures prevent `MountedTurnCapture`, `SessionReducer`, and async provider
  dispatcher/factory bindings from leaking into it. The advanced
  `DurableMountedAgentFactory` now has an external consumer
  proof: an application-owned opaque CAS/outbox backend, fingerprint policy,
  stateful provider, reducer, and Commit stager complete Create, atomic outbox
  publication, a fault-injected publication CAS retry without duplicate
  provider/outbox work, cross-factory reopen, replay, durable provider-cursor
  reuse, and v3 persisted-cursor delta continuation after replacement-owner
  recovery without a second System render/attach. The backend adapter now
  distinguishes new `Published`
  from replay `Existing`, so only a new publication enqueues outbox items; it
  rejects a successful CAS response whose generation did not advance. Private
  mutation and provider-result JSON is recursively key-sorted under fingerprint
  schema v2, with final-fingerprint tests for mutation, provider success and
  provider error data. Fingerprints are self-described as `sha256:v2:*`;
  durable state schema 3 rejects v1 before candidate recomputation and accepts
  v2 with one safe full-User migration. Arrays retain semantic order, so hosts
  must
  sort unordered sets before serialization. The host remains responsible for
  canonical typed outbox payload fingerprinting. This closes
  only the facade smoke boundary. API freeze remains rejected until public
  id-keyed recovery/reconfigure, crash supervision, a real outbox worker, a
  real remote-session provider, and Forgotten City production installation
  exist.

## Long-Term Goal

Make the mounted POM path the production AgentLoop path for Forgotten City:
one durable System transmission per epoch, a freshly rendered User document per
turn, typed streaming/tool components with host-owned effect delivery, and
durable recovery across process ownership changes. Public API freeze is allowed
only after a real consumer migration and independent component-author,
consumer-surface, and lifecycle/persistence API reviews have closed their P0
findings.

**Completion definition:** this target is reached only when one real Forgotten
City game/session transaction authority durably owns the mounted epoch, provider
operation receipt, reducer/domain mutation, and recoverable outbox publication;
ordinary turns send only newly rendered User POM; and independent API reviews
confirm that component authors cannot acquire host side effects through the
public authoring surface. A mounted selector installed only with separate state
and provider-ledger databases is progress, not completion.

## North Star

AgentView is a POM component runtime for complete agent harnesses. A component
may contribute prompt POM, reusable runtime bindings, reducers, typed outputs,
diagnostics, and effects. POM remains the only prompt representation; the
component tree only governs composition and lifecycle.

The harness has two deliberately different render lifetimes:

```text
bind(config)                          pure definition construction per process
  -> one ordered DurableSystem {
       POM-only nodes,
       exact-one durable runtime leaves,
     }
  -> preflight runtime projection

open durable epoch
  -> Create                           exactly once per durable epoch
       consume/resolve/render the DurableSystem POM projection
       derive manifest from the same tree's runtime projection
       attach one logical System prompt to the provider
  -> Reopen                           zero System POM execution/render
       filter the same definition to its runtime projection
       rehydrate streaming/native runtimes without System bytes
  -> MountedHarness {
       one logical System prompt,
       reusable binding factories,
       provider capability plan,
       HarnessEpoch,
     }

prepare_turn(props, context)          once per preparation attempt
  -> User View
  -> TurnPlan {
       UserDocument,
       fresh binding/reducer state,
     }
  -> final Ready
  -> fresh binding/reducer instances
  -> bound provider dispatcher/capabilities
  -> provider stream
```

The complete ordered `DurableSystem` is the only durable System authoring
source. Constructing that retained definition is pure and may happen in a
recovery process. Only the winning Create admission executes its deferred POM
nodes and renders System; reopen, ordinary turns, context replacement,
compaction, provider retry, and `TurnFlow::Continue` never do. The User View
owns changing context, artifacts, and task content and is rendered whenever a
new turn plan is needed.

There is no general `request_system_change` event. The model, parser, reducer,
compactor, and agent loop cannot request one. A rare policy or capability
change is an explicit host operation that mounts and validates a new harness
epoch, then atomically replaces the old epoch. It never appends a second System
message.

The intended authoring shape is conceptually:

```rust,ignore
#[view(component)]
fn player_policy(policy: Arc<PlayerPolicy>) -> PomView {
    view(PlayerIntentRules::new(policy.version))
}

fn player_system(
    config: Arc<PlayerConfig>,
) -> DurableSystem<PlayerChannels, PlayerTurnProps> {
    durable_system((
        player_policy(Arc::clone(&config.policy)),
        select_intent(Arc::clone(&config.select_intent)),
        post_intent_rules(),
        world_tools(Arc::clone(&config.world_tools)),
    ))
}

fn player_turn(cx: UserTurnContext<'_, PlayerTurnProps>) -> UserView {
    let props = cx.props();
    user_view(view((
        diff("agent_context", &props.context),
        &props.artifacts,
        &props.task,
    )))
}

let mounted = MountedAgent::builder(PlayerHarness)
    .durable_system(Arc::new(player_system(Arc::new(config))))
    .epoch_contract_id(EpochContractId::new("player/v3")?)
    .provider(Arc::new(executor))
    .persistence(persistence)
    .open()
    .await?; // Create renders System; reopen only rebinds runtime declarations
let call = mounted
    .call(DurableCallId::new("request-42")?, "player-turn")
    .input(
        DurableCallInputId::new("request-42/input-v1")?,
        Arc::new(PlayerCallProps { task, artifacts }),
        Arc::new(source),
    )
    .start()
    .await?;
let outcome = call.wait().await?;
```

The durable lifecycle roots are one retained `DurableSystem` definition and a
`UserTurnContext -> UserView` function. `SystemMountContext -> SystemView`
remains an explicit one-shot compatibility path and must not be used by the
durable owner. `#[view(component)]` defers POM/ordinary component bodies; a
durable return eagerly performs only pure definition construction so its
runtime projection can survive independently of POM rendering. The builder
names above are target vocabulary, not a frozen Rust API.

### Long-term completion target

**Program objective (active):** replace Forgotten City's production prompt
assembly and AgentLoop integration with this runtime, then freeze only the
small author-facing mounted API that survives that migration. The intermediate
component internals are implementation work, not separate product endpoints.

AgentView is complete for this roadmap when forgotten-city's real AgentLoop
uses this mounted POM component runtime end to end: System POM renders once per
durable epoch; User POM is recaptured and rerendered per turn and continuation;
streaming and native tools run as typed provided components with ordered,
awaited Live effects; one synchronous pure reducer produces the durable session
mutation; and one mount-owned persistence binding handles session identity,
contract identity, CAS conflict/reload, publication recovery, reconfigure, and
the Commit outbox. No legacy prompt string builder or side-effecting
`commit_turn` remains on the production path.

Public API freeze requires runnable forgotten-city-style examples, compile-fail
coverage for invalid composition, full workspace tests/lints/docs, and
independent consumer, type-system, and lifecycle/persistence reviews from other
agents. Internal lifecycle proofs alone do not satisfy this target.

### Production-host gate

The mounted player selector must remain an opt-in proof until both halves of
the production host exist together:

1. A single transactional durable store for the game/mounted session. It must
   retain the epoch artifact and manifest, provider receipt and cursor, call
   ledger and lease state, authoritative reducer session, publication receipts,
   and recoverable outbox payloads under one durable session identity.
2. A real stateful remote provider session. Its create-or-get operation must be
   idempotent on `(durable_epoch_id, artifact_fingerprint)`, and only that
   operation may receive System/tool bytes. Rehydrate and ordinary turns are
   structurally System-free.

Forgotten City's current server owns one in-memory `GameSession`, while its
Rig/OpenRouter/OpenAI-compatible executor constructs a new stateless request
with `request.system` on every turn. It is therefore not an eligible mounted
backend. Wrapping it would make the local proof look production-ready while
violating the System-once transport invariant.

The next implementation boundary belongs to the advanced host integration,
not normal component authors: AgentView must offer a narrow production owner
adapter without publishing its raw store/actor internals; Forgotten City must
then bind that adapter to one durable game-session transaction domain and an
actual remote-session protocol. Only after crash/reopen and duplicate-call
golden traces pass may `GameEngine::new` install `PlayerIntentMountedAgent`.

Recovery uses **semantic session persistence plus an acknowledged durable User
cursor**. `PromptContext`, history, context state, provider receipt/cursor,
call ledger, publication state, and the mounted User baseline are durable.
`AgentSession` still omits `UserDocumentCursor` from its generic serialization;
the schema-v3 mounted-state envelope stores it atomically with the accepted
session mutation and outbox instead. A replacement factory or reload therefore
continues the acknowledged delta lineage while System remains neither rendered
nor attached again. Existing schema-v2 rows have no baseline, so they safely
start with one full User document and are rewritten as v3.

Internal and external-factory tests prove that the v3 backend JSON contains the
cursor and its sentinel POM content, that a replacement owner restores the
same baseline and emits a changed structured delta, and that a v2 row falls
back to full before migration. This is not yet the production gate: a
crash/reopen trace must still prove no second System attach, the same provider
receipt/cursor, no duplicate execution/outbox row, and provider-requested
resync behavior.

The runtime exposes an explicit force-full handshake as
`ContextPreparation::ResyncUserDocument`. It clears only the draft
`UserDocumentCursor`, repeats bounded preparation, and leaves the System epoch,
history, working state, and provider attachment intact. Compatibility and
mounted tests prove that a provider-requested resync sends full User POM while
System attachment remains exactly once. This supplies a recovery protocol
primitive; it does not by itself create a durable store or remote-session
implementation.

For a durably admitted mounted call, the owner also derives one
`ProviderOperationIdentity` from the persisted session, epoch, call, input,
turn, and publication-request tuple. The provider receives it only on its
ordinary System-free request; diagnostic labels are never used as remote
idempotency keys. Forgotten City's stateful fake proves that normal delta
preparation and a provider-requested full-User resync retain the same
operation key, while separate calls receive different keys and System remains
attached once. Forgotten City's stateful adapter rejects a missing identity;
a production adapter must additionally persist it with its remote execution
receipt.

Forgotten City's domain now has a serialized `InteractionSessionId` and
`PlayerTurnId`; the conversation issues the sequence before it emits
`DurableYourTurn`, and the mounted runtime verifies the current interaction ID
before deriving `player-intent/{interaction}/{turn}`. It rejects the legacy
`YourTurn` compatibility event when a mounted selector is installed, so the
production-shaped route cannot fall back to a SlotMap key or runtime
generation. This is still an in-memory semantic proof: a durable game/session
store, restored world snapshot, and semantic (not raw POM) continuation
feedback remain prerequisites for recovery after a process restart.

### Long-Term Product Goal

Make mounted POM the production AgentLoop contract for Forgotten City, rather
than a parallel prompt/lifecycle proof. A harness composes durable System
policy, fresh User state, typed streaming/native-tool components, and their
pure reducer contracts from one component vocabulary. The host binds those
components to provider I/O, Live effects, persistence, cancellation, and
outbox delivery without allowing component authoring to acquire those side
effects directly.

The durable invariants are non-negotiable: render and physically attach one
System document exactly once per durable epoch; render User only for each
logical turn/retry/continuation; preserve typed real-time reducer delivery; and
recover owner/call/epoch state without replaying unsafe provider work. The
target is met only when Forgotten City's real `observe -> act -> hook` loop
uses this owner with a transactional session/outbox store and a stateful remote
provider session. Public API freeze is a final gate after that consumer proof
and independent authoring, host, and lifecycle API reviews, not an intermediate
milestone.

### Public Facade: Local Proof Implemented, Deliberately Unfrozen

The narrow `component::mounted` facade now exists without exposing the raw
owner. Its current shape is:

```text
DurableSystem + UserTurnContext -> opaque MountedAgent
  open / reopen
  -> start_call(owned call input) -> opaque MountedCall
  -> wait() -> Executed | Replayed
  -> scoped cancel() that joins cleanup

host-only reconfigure
  -> durable submission id
  -> query / watch semantic durable status
```

The normal authoring path may expose only the retained `DurableSystem`, the
per-turn `UserTurnContext -> UserView` root, owned call inputs, stable terminal
outcomes, and stable recovery categories. It must not expose the current
`MountedAgentBinding`, `MountedOwnerStore`, actor/receiver handles, leases,
revisions, raw publication requests, runtime registry, or attempt typestates.

The current durable-provider adapter seam is an advanced integration boundary,
not the normal authoring API. Its public construction errors now use
`ProviderAdapterError`, and provider-facing tool descriptors expose only name,
description, and input schema; capability/version manifest metadata remains
internal. An external black-box test exercises this seam through the real
mounted owner, but a production Forgotten City adapter is still required.

### Public Root And Local Host: Implemented, Under Review

`component::mounted::DurableEpochDefinition<C, Props>` owns exactly one
`DurableSystem<C, Props>` by value and its `EpochContractId`.
`MountedHarnessDefinition<C, Props>` combines one such replaceable epoch with
one stable `UserTurnContext<Props> -> UserView` renderer. This deliberately
makes the future reconfigure input an epoch value, not a second System callback
or a replacement for User rendering, store, reducer, request-id, or provider
policy. Neither public type has a System render method, host configuration,
persistence/store capability, lease, revision, actor, or attempt state.
`UserTurnContext::new(&props)` is a pure renderer helper only; it does not open
a provider epoch or create a durable call.

The definition is deliberately non-`Clone`. A second durable epoch must be
constructed from a fresh pure `DurableSystem` definition, so two sessions
cannot consume or race one linear System POM projection.

The public opaque owner now consumes the retained System POM only after Create
admission and uses the same definition's POM-free runtime projection on reopen.
`InMemoryMountedAgentFactory::open`, owned `MountedCallInput`, `start`, `wait`,
`cancel`, and `reload` execute through the real bound owner. External tests
prove two calls, exact replay, input collision, dropped wait, awaited typed Live
effects, joined cancellation, provider cursor rehydration, and drop/reopen
without another System render or attachment.

`MountedTurnCapture` owns its `Transcript` associated type, so
`MountedHarnessDefinition::with_capture(capture)` infers the durable transcript
instead of making authors repeat it as an unrelated generic. The definition's
raw `render_user` method is crate-private: an external host may author the pure
`UserTurnContext -> UserView` function, but only the mounted owner may schedule
capture, render, admission, and provider work as one turn lifecycle.

`DurableEpochDefinition::new` now takes only the epoch contract identity and
durable System tree. The mounted host binding supplies the host-configuration
and host-runtime compatibility fingerprints from its provider, persistence,
reducer, and Live-runtime configuration. Runtime leaves retain their own
author-declared `RuntimeContract(id, version)`. A component author therefore
supplies only the durable System tree, User renderer, and declared contract
identity; they cannot smuggle owner compatibility policy into a reconfigurable
epoch definition.

This closes the local-facade proof only. One factory instance, its clones, and
its reopened handles share a process-local store. Two separately constructed
factories are separate stores even if their `DurableSessionId` strings match.
The public-authoring integration test locks this scope by opening two such
factories and observing two independent Create/System attachments.
The local host requires `Commit = Never`, rejects reconfiguration, and has no
cross-process recovery or outbox worker. Raw mounted factory/driver traits are
crate-owned; a compile-fail test prevents external code from fabricating an
owner that bypasses System admission. Future extensibility enters through
provider and persistence adapters behind another AgentView-owned host.

For reconfiguration, an acknowledgement becomes public only after a durable
intent or durable rejection record exists. A local task accepting a candidate
is not a durable admission and must remain private. Query/watch must project
semantic statuses such as `InFlight`, `Activated`, `Rejected`,
`RecoveryRequired`, and `ReopenRequired`; it must not expose a destructive
cancel operation. A persisted terminal tombstone is retained long enough to
prevent the same id from authorizing a second System render.

Implementation order:

1. **Completed for the local host:** prove the opaque mounted open/reopen and
   owned-call facade, rooted in `MountedHarnessDefinition`, from an external
   consumer crate.
2. Add a production persistence adapter with durable call recovery, lease
   policy, outbox recovery, and id-keyed reconfiguration status retention.
3. **Scaffold/local proof complete:** Forgotten City now has a stateful
   provider-session adapter and fake transport proof; AgentView's generic
   coordinator and the consumer fake both pass the lost-attachment-reply resume
   proof. The ordinary attacher is now separate from the durable executor. Bind
   a real remote-session factory and repeat the same transport proofs there.
4. Migrate one Forgotten City harness before considering API freeze.
5. **Completed (2026-07-31):** gate the raw compatibility IR behind the
   non-default `raw-component-ir` feature, keep its isolated examples/tests on
   that explicit compatibility path, and compile-fail ordinary consumers that
   try to import it. It is not part of the default mounted authoring surface.

#### Provider Transport Decision Gate

Physical System-once is a provider transport property, not a POM rendering
property. The current Forgotten City `RigLLMExecutor` creates a new Rig agent
from `request.system` for every OpenRouter chat-completions request, so it is
not a candidate for the mounted production adapter.

Before the first production host is selected, its transport must satisfy all of
the following:

1. Create or select a remote provider session idempotently from the durable
   epoch/artifact identity, accepting the canonical System bytes and tool
   catalog only in that Create operation.
2. Return an opaque epoch receipt and a per-turn cursor that can be persisted
   atomically with the accepted session mutation.
3. Rehydrate after process loss from receipt/cursor alone; that request must
   have no System or tool accessors.
4. Execute ordinary turns using only session/cursor, history/User data, model,
   and limits. It must have no route that silently reconstructs a System
   preamble.
5. Cooperate with call-scoped cancellation by stopping and joining the remote
   operation before the mounted owner compensates Live effects or reports a
   terminal result.

The acceptance test uses a recording transport around the real adapter and
proves: one Create receives System exactly once; ordinary turns, retries,
continuations, reload, and reopen receive none; cursor continuity survives
reopen; and cancelled operations join before Live compensation completes. A
session proxy that retains System only locally but resends it to a stateless
upstream does not meet this definition.

#### First Forgotten City Migration: Player Intent Selector

The first production candidate is Forgotten City's
`crates/engine/src/player/agent.rs::PlayerIntentComponent`, specifically its
`SelectIntentTool` contract. It has the intended lifetime split already: the
rules plus XML output contract are durable System policy, while
`PlayerIntentSnapshot`, artifacts, retry feedback, and task are per-turn User
data. It is **not migrated** by the existing `ComponentTextAgent` wrapper.

The cutover must be an end-to-end owner replacement, with these required
boundaries:

1. Author one `DurableSystem` containing the selector policy and a typed
   `select_intent` streaming component, then capture the current snapshot into
   owned turn props and render User POM for each selection/retry.
2. Move `SelectIntentTool::on_open` validation into a synchronous pure reducer
   using an immutable intent-pool snapshot carried by the turn props. It must
   not read `WorldApi` or send on `task_output_tx` from the reducer.
3. Represent accepted selections as typed output and/or explicit ordered Live
   effects. A PlayerRuntime-owned Live runtime may send the existing
   `PlayerRuntimeTaskOutput`, with a defined abort/compensation policy and the
   current batch-generation fence.
4. Replace `RigLLMExecutor`'s per-call
   `make_agent(model, request.system, ...)` path with a stateful provider
   session adapter. Create attaches the canonical System once; ordinary turns
   use the opaque provider session/cursor, and reopen rehydrates it without
   receiving System bytes.
5. Keep the mounted owner in `PlayerRuntime` under a durable session/epoch
   identity. Replace raw `JoinHandle::abort` with call-scoped mounted cancel
   that joins provider and Live cleanup, and prove retry, continuation,
   drop/reopen, and no-second-System transport against a recording adapter.

The old `ComponentTextAgent` compatibility wrapper may remain during the
transition, but it is not evidence for this milestone: its `AgentViewModel`
still builds System per execution and the current Rig executor creates a new
provider agent from `request.system` on each call.

**Current pre-migration proof:** Forgotten City's
`crates/engine/src/player/mounted_select_intent.rs` now extracts the selector
as one `DurableComponent`. Its per-attempt reducer receives only an immutable
captured intent-pool predicate catalog, validates handles, verbs, object and
recipient scopes without `WorldApi` or a sender, and emits a typed
`PlayerIntentLiveEffect::Submit` alongside typed output. A host-only
`LiveEffectRuntimeFactory` binds the final attempt identity, call batch, and
effect sink after the User snapshot is selected. The first PlayerRuntime sink
adapter adds immediate ordered delivery plus a revocable ownership fence so an
aborted attempt can retract queued or visible options. The one-attempt Live
runtime owns each returned fence itself, so successful attempts do not leave
lookup state accumulated in the shared sink. Twenty focused tests now cover
accepted output, stale/malformed handles, duplicate and five-item limits,
verb/object/recipient template validation, legacy-equivalent accepted-stream
callback ordering, mounted definition assembly, real
provider-wire ordering, success without compensation, reverse abort
compensation, best-effort cleanup after one retraction fails, and a
drop/reopen recording-adapter proof: exactly one System-bearing durable
attachment, then rehydration plus ordinary User-only provider requests. The
new legacy/mounted prompt golden assertions require the same System bytes plus
the same initial and retry User bytes; they also lock the mounted call budget to
the legacy three-turn selector limit. The last proof validates the selector
harness against the mounted provider
contract; it does not make the current stateless Rig transport a production
physical System-once adapter. `PlayerRuntime` now has an opt-in mounted
selector branch that starts typed User calls and uses call-scoped cancel plus a
cleanup join; no production host installs an opened mounted owner into that
branch yet. A separate stateful-session scaffold and fake remote prove one
physical System installation, cursor-only rehydration, User-only ordinary
turns, and joined cancellation. They do not supply a real remote-session
factory or production installation, so this is still not a production
migration or physical System-once claim for the current Rig transport.

A runtime-level selector test now starts that branch through the real
`PlayerRuntime` queue, waits for a streamed Live submission, clears the turn,
and proves that cancellation joins provider/Live cleanup before a queued
submission can revive the cleared selection. Its fake provider reports an
`Indeterminate` cursor, so a second event is correctly fenced before capture,
User render, preparation, or provider execution; `PlayerRuntime` also clears
the transient empty selection after that admission error. AgentView's local
owner now additionally proves that a provider reporting `Unchanged` or a valid
`ResumeFrom` cursor is settled to `Stopped` under the exact Running fence and
allows a successor. Forgotten City has not yet supplied that authority through
a production transport, so the opt-in branch remains a lifecycle proof rather
than a game-loop cutover.

The remaining cancellation gap is production recovery/control, not the normal
local settlement mechanism. Owner tests plus seven direct tests against the
actual process-local `InMemoryMountedStore` prove that pending publication,
`Indeterminate`, and every injected epoch/lease/revision/cursor conflict stay
`RecoveryRequired`; they also prove same-fence recovery retry idempotency. The
store remains intentionally hidden behind the public facade rather than becoming
an author-facing fault-injection API. Borrowed raw execution is test-only and
module-private. The public facade now has lease-free call-id lookup and an
owner-instance-local, single-handle reattachment path after an observer drops
its handle. The production host still needs durable-id keyed cross-owner/process
control and recovery, lease policy, and a concrete persistence/outbox adapter.
`PlayerRuntime` must not bypass those owner rules.

#### Completion checks

This is one product goal, not a sequence of unrelated library releases. It is
met only when all of the following are true in the Forgotten City production
path:

- [ ] Harness authors compose System, User, streaming, and native-tool behavior
  through the POM component vocabulary rather than a legacy prompt builder.
- [ ] A durable System tree is rendered and attached exactly once for each
  durable epoch; recovery, retry, compaction, and continuation cannot append a
  second System message.
- [ ] Each logical turn captures a fresh User view, while a typed provided
  component can reduce streaming events in real time and produce ordered Live
  effects plus one pure durable mutation.
- [ ] The persistence binding can recover owner/session identity, epoch
  transitions, CAS reloads, reconfiguration, pending publication, and outbox
  delivery without replaying provider work unsafely.
- [ ] The public mounted facade is proven by an external consumer, accepted by
  independent authoring, consumer, and lifecycle reviewers, and does not expose
  the private actor, lease, revision, store, or typestate machinery.
- [x] Raw compatibility IR is excluded from the default mounted authoring
  surface. It is available only through the explicit `raw-component-ir`
  migration feature, with an external compile-fail proof for ordinary users.

### Independent API Review Gate

The mounted API is intentionally **not frozen** until all three independent
reviews below are complete and their P0 findings are closed or explicitly
deferred behind non-public compatibility boundaries.

#### Latest Independent Review Pass (2026-07-31, in progress)

- [x] **Independent component-author API review (2026-08-01): reject API
  freeze.** A fresh external author can build a prompt-only mounted feature
  using only `component::prelude`, and the prelude/compile-fail tests keep
  async provider-dispatcher binding, persistence, raw IR, and legacy-turn
  machinery out of that path.
  This validates the intended pure `MountedFeature` System/User authoring
  shape; it does not validate a production host. The review found two release
  P0 gates: (1) `RecoveryRequired` is public but there is no public id-keyed
  cross-owner/process recover or cancel control after a process loses its
  `MountedCall` handle; and (2) mounted lacked an external `observe -> act ->
  hook` facade with a stable action token. The second finding is now closed by
  the mounted external controller plus Forgotten City's SQLite facade and
  exact `action_handle`; the first remains open. `advanced::persistence` still exports
  lease/ledger/raw-publication typestate alongside the intended opaque backend
  ports, and `MountedHostRuntime` has no session/call drain supervisor; these
  are P1 public-surface and shutdown-governance work. Channel mapping is also
  still four-lane boilerplate for independently authored features, a P1
  ergonomics issue rather than a reason to weaken typed contracts.

  **Shared runtime boundary: completed (2026-08-01).**
  `component::advanced::host::MountedHostRuntime` borrows a caller-owned Tokio
  handle, and `InMemoryMountedAgentFactory::new_with_runtime` plus
  `DurableMountedAgentFactory::new_with_runtime` schedule on it without taking
  shutdown ownership. The legacy `new` constructors retain their local
  AgentView-owned two-thread runtime for compatibility. The external durable
  factory fixture constructs two independent factories from one current-thread
  host runtime, reopens the same durable epoch, and records that both provider
  executions run on the caller's thread, and after the mounted factories/calls
  drop the caller can still drive its runtime. This closes the narrow factory
  runtime-ownership P1; it deliberately does not add a host shutdown/drain
  supervisor, public id-keyed recovery/control, the complete AgentLoop facade,
  or a narrower persistence export.

  **Author/host separation proof: completed; usability freeze gate remains
  open (2026-08-01).**
  `tests/component_authoring_streaming_prelude.rs` splits a real external
  consumer into an `author` module that imports only `component::prelude` and
  a host module that owns lifecycle and Live execution. The author composes a
  typed System POM, fresh User POM, and `StreamingXml` durable component with
  a declared `RuntimeContract` plus typed Output/Live/Diagnostic lanes. The
  executable test sends `<choose index="7" />`, observes the synchronous Live
  reducer and typed Output, and verifies typed validation diagnostics for
  `<choose />`. `RuntimeContract` is now part of the ordinary component
  prelude because it is pure author-declared contract identity, not host
  plumbing. This proves the author/host separation and ergonomic streaming
  vocabulary; it is not a production-host claim or proof that the public
  surface is yet minimal. A later novice rewrite found accidental burden in
  `NoTurnChannels`, explicit harness assembly, and heavy streaming generics.
  The prompt-only burden is now reduced by `PromptComponent`; explicit epoch
  assembly remains at the host boundary, while typed streaming deliberately
  retains its channel semantics. Keep `hello_world` as the small baseline and
  do not count its example-only recording host as production evidence.

- [x] **Mounted public API review:** **reject API freeze.** The opaque public
  facade correctly hides the raw owner/lease/revision/store machinery. An
  advanced `DurableMountedAgentFactory` now lets an external host bind a typed
  CAS/outbox backend, stateful provider, reducer, Live factory, Commit stager,
  fingerprint policy, and caller-owned runtime to that owner without exposing
  its actor internals. Its external fixture proves Create, publication, replay,
  reopen, lease-free call-id lookup, and same-owner dropped-handle reattachment.
  The latter is deliberately local and exclusive: an already-attached, terminal,
  or cross-owner/process call only yields a read-only observation. It does not
  provide recovery action or cancellation after the owner/process loses the
  `MountedCall` handle. The required remedy is a narrow durable-id keyed
  cross-owner/process control/recovery facade, not publication of raw
  actor/store internals. Forgotten City's mounted branch
  also remains an opt-in proof only. Its call
  and input IDs now derive from a serialized interaction UUID plus persisted
  player-turn sequence, and mounted mode rejects the legacy `YourTurn` event
  before provider preparation. `GameEngine::new` still installs the legacy Rig
  path. Production installation still requires the transactional game/session
  store/outbox and a real stateful provider session.
  The review also records two pre-freeze contract decisions: production
  adapters must reject a missing `provider_operation`, and remote idempotency
  must use an explicitly documented/enforced scope across durable session and
  epoch rather than treating the publication request id as globally unique.
  The latter now has a local contract implementation:
  `ProviderOperationIdentity::remote_idempotency_key()` is a versioned opaque
  digest of the exact `(session, epoch, call, input, turn, request)` tuple,
  while `publication_request_id()` is explicitly local-only. AgentView's unit
  and external-consumer tests, plus Forgotten City's stateful-adapter scaffold,
  prove its stable retry/resync behavior. This narrows the adapter contract; it
  does not make the scaffold a real remote provider or production host.
- [x] **Component-author/type-system re-review (2026-08-01): reject API
  freeze.** The narrow `component::prelude` is now a credible ordinary author
  boundary: a separate consumer can compose typed System POM, fresh User POM,
  and a synchronous `StreamingXml` reducer without importing provider,
  persistence, host, or raw IR contracts. The focused author/public/trybuild
  suite passes. The boundary is nevertheless incomplete for the central
  provided-tool use case. `ProviderCapabilityDeclaration` still retains an
  `instantiate_dispatcher` closure beside author schema and contract data, and
  `durable_provider_tools*` requires that closure at authoring time. In
  addition, `RuntimeContract::implementation_version` currently represents the
  author declaration and dispatcher behavior together, so it cannot validate a
  host-only implementation upgrade during POM-free reopen.

  This is a P0 for the unified component contract: add a pure durable tool
  declaration containing only prompt POM, ordered tool specs, stable
  declaration id, and author-contract version; bind it through a host-owned
  dispatcher registry with its own implementation version. The mounted open
  path must validate missing, duplicate, author-version, schema, and host
  implementation mismatches before Create renders or attaches System, and
  reopen must validate the persisted manifest without executing System POM.
  Retain dispatcher-instantiating builders only under the explicit advanced
  compatibility surface until migrated consumers use the new contract.

  The review also keeps two P1 items open: the author prelude still exposes
  low-level `binding_factory*` spelling that depends on non-prelude runtime
  traits, and the type-dependent eager/deferred behavior of
  `#[view(component)]` needs a concise lifecycle explanation before API
  freeze. Neither justifies weakening typed channel mapping or the pure reducer
  rule.
- [x] **Pure provider-contract follow-up re-review (2026-08-01): close the
  preceding provider-closure P0 for the canonical mounted path.**
  `ProviderCapabilityContract` now contains only the author-owned declaration
  id, contract version, and ordered tool schemas; a process-local
  `ProviderDispatcherRegistry` supplies the host implementation version and
  fresh per-attempt dispatcher factory. Open and internal reconfigure both
  bind and validate that registry from a POM-free runtime projection before a
  durable store load, System render, provider attachment, or rehydration.
  The durable artifact persists descriptors, POM, rendered bytes, and provider
  receipt, not a registry or dispatcher closure. Focused public authoring,
  durable-factory, compile-fail, default-workspace, and raw-compatibility
  suites pass.

  This does **not** approve an API freeze. The remaining P1 surface work is
  explicit: the legacy dispatcher-instantiating `durable_provider_tools*`
  constructors are still reachable through the broad `component` root and
  `advanced::provider` compatibility surface, although they are absent from
  the documented author prelude; decide whether the root re-export is removed
  or permanently documented as an escape hatch after consumer migration. The
  direct missing-registry reconfiguration regression now proves that candidate
  System rendering, provider attachment, rebase, and durable admission do not
  begin before this preflight fails.

  The runnable host-bound pure-tool proof now lives in
  `examples/pom_feature_composition.rs`: author code declares only the POM
  feature tree and `ProviderCapabilityContract`, while the host binds a
  `ProviderDispatcherRegistry` through `MountedHostBindings`, owns capture,
  provider wire, reducer, and in-memory factory, then drives two native-tool
  turns. The program checks one System attachment with fresh User POM and one
  native-tool result on each turn. This closes the example ergonomics gap only;
  it is not evidence for a production provider or persistence adapter.
- [x] **Lifecycle/persistence re-review:** **reject API freeze.** The local
  cancellation/persistence proof is materially sound: every cancellation
  settlement carries the exact
  `(session, active epoch, contract, call, input, turn, request, lease,
  revision)` fence, and the store atomically writes either `Stopped` only for
  authoritative cursor continuity or `RecoveryRequired` for every ambiguous,
  conflicting, expired, or publication-pending outcome. The owner applies that
  result before admitting a successor. Focused owner/store coverage exercises
  stale revision, foreign lease, invalid cursor, indeterminate cancellation,
  pending publication, and idempotent exact-fence recovery. Forgotten City's
  `PlayerRuntime` validates the durable session identity and rejects duplicate
  or older same-session `PlayerTurnId` values before it joins cancellation, so
  a delayed event cannot revoke a live mounted provider call.
  This is not a production approval: `InMemoryMountedAgentFactory` is
  process-local and `Commit = Never`; no transactional game/session store and
  recoverable outbox, production lease policy, id-keyed remote control/reopen,
  or real stateful remote provider receipt exists. The mounted player selector
  remains opt-in rather than the production AgentLoop. A production adapter
  must reject a missing `provider_operation` and atomically retain that remote
  idempotency key with its execution receipt.

  **Caller-owned runtime follow-up (2026-08-01): reject API freeze.**
  `MountedHostRuntime` is intentionally only a borrowed Tokio scheduling handle:
  its lease drop cannot shut down the application runtime, and the external
  current-thread fixture proves that factories and calls may be dropped without
  stopping it. It is not a shutdown/control plane. If the application stops its
  runtime during detached work, the durable owner must later recover from store
  state. `MountedAgent::lookup(call_id)` now provides a lease-free status
  snapshot, including owner-local admitted progress. The admitting owner
  instance also supports one attached call handle after the previous handle is
  dropped, but there is still no cross-owner/process recovery or cancellation
  route. The
  fixed internal timeout/cancellation policy and Tokio-visible advanced type
  are acceptable while experimental, but not an API-freeze contract. A
  production supervisor must define orderly drain and
  shutdown-during-active-call recovery semantics.

  **Durable-id supervisor contract (P0 design, 2026-08-01).**
  `MountedAgent::lookup(call_id)` is intentionally only a lease-free
  observation: it may return owner-local admitted `Preparing`, `Running`, or
  `AwaitingContinuation` progress without waiting behind streaming work, or a
  store-backed terminal replay proof, `Paused`, `Stopped`, or
  `RecoveryRequired`. It grants no mutation authority. The production control
  plane must remain host-owned and keyed by the durable
  `(session_id, call_id, input_id)` tuple:

  1. `open_or_reopen` restores one owner from the retained epoch artifact and
     provider receipt without rendering or transmitting System again.
  2. `lookup` returns a semantic snapshot only. It never leaks the CAS
     generation, lease, revision, provider cursor, raw publication request, or
     actor handle.
  3. `reattach` is not a retry: the admitting owner instance can mint one live
     call handle only when no handle is currently attached; otherwise it
     returns a read-only observation. A settled or cross-owner call also
     returns its durable observation and never restarts provider work.
  4. `recover` and `cancel` must first reconcile the persisted
     `ProviderOperationIdentity` with the remote provider. They may only write
     a successor state through the existing fenced session transaction. An
     unknown or ambiguous remote operation stays `RecoveryRequired`; the
     supervisor must never infer that it is safe to rerun model/tool/Live work.
  5. `drain` first stops new admissions, then gives active local calls their
     joined cancellation/reconciliation path. Runtime shutdown without a
     completed drain is a restart-recovery event, not implicit successful
     cancellation.

  This is deliberately not a public Rust API proposal yet. It defines the
  minimum behavior a real Forgotten City host and stateful remote adapter must
  prove before `lookup`, reattach, recovery, or cancellation contracts can be
  frozen.

  **Provider-operation transport proof (2026-08-01).** The advanced,
  POM-free `DurableMountedProviderOperationController` now carries only a
  framework-derived `ProviderOperationIdentity` and exposes remote
  `inspect_operation` / `cancel_operation`. Its status contract distinguishes
  strong `NeverAccepted`, `Running`, `Completed`, cursor-qualified `Cancelled`,
  and ambiguous `Unknown`; `Cancelled` means the remote execution has joined.
  Forgotten City's stateful fake now keeps an operation ledger keyed by that
  identity. Its external control tests observe `Running`, send a remote
  cancellation, wait for the active execution to exit, and then observe the
  authoritative `Cancelled { Unchanged }` result; they also prove a completed
  operation remains `Completed` when cancellation is requested. Both paths
  remain System-free. This proves the adapter seam and fixes the consumer
  compile regression introduced by the new trait. It is not supervisor
  recovery: operation identity is still reconstructed only from the
  authoritative session/epoch/call fence. `Reserved -> Running ->
  RecoveryRequired` now retains the exact reservation origin, including a
  legacy-safe ambiguous state when an older record lacks it. The remaining
  work is a durable reconciliation claim/fence that consumes a strong
  `NeverAccepted` proof, a transactional host store, a real stateful provider,
  and a cross-owner/process supervisor before control can be exposed or frozen.

  **Internal reconciliation controller proof (2026-08-01).** The remaining
  claim/fence slice is now implemented only below the public facade. The owner
  atomically claims a `RecoveryRequired` ledger record before inspection,
  reconstructs the exact `(session, epoch, call, input, turn, request)` remote
  operation, and lets the store consume a strong `NeverAccepted` proof only
  while that exact claim is live. A New call is removed, a continuation restores
  its prior committed request checkpoint, and every other provider status
  releases only the claim while retaining `RecoveryRequired`. Focused tests
  prove stale claims cannot restore or release, and the durable backend persists
  both claim and release across reload. This is host infrastructure, not the
  future `MountedExternalController`: it still has no public action/reply
  ingress, cross-owner control, production store, or real provider ledger.

  **Lifecycle/persistence review of the pure provider-contract split
  (2026-08-01): reject API freeze.** The new
  `ProviderCapabilityContract -> ProviderDispatcherRegistry` path is sound at
  the local proof boundary: `open` and internal reconfiguration bind and
  validate the host registry before any System render, provider attach, or
  rehydrate; missing, author-version, schema/order, and host-implementation
  drift fail closed. The durable blob contains only the serializable epoch
  artifact/manifest, session state, receipt, and cursor; registry factories
  remain process-local runtime values and a fresh dispatcher is instantiated
  only for a final-ready provider attempt. External public-authoring and
  durable-factory tests cover Create, reopen, host-version rejection,
  per-attempt construction, and a native provider-tool round trip.

  The follow-up `MountedHostBindings<C, TurnProps>` closes the generic-factory
  P1: `MountedAgentFactory::open` now receives one extensible host-owned binding
  value, and both local/durable factories expose `open_with_bindings` while
  keeping `open_with_provider_dispatchers` as convenience spelling. The
  `pom_feature_composition` consumer now constructs this same binding value
  after final props projection. This does not close the release gate.
  The older dispatcher-carrying `durable_provider_tool(s)` APIs must remain
  visibly advanced compatibility until real consumers have migrated. The P0
  durable-id cross-owner recovery/control and external action/reply controller
  remain unchanged.

  **Independent authoring review refresh (2026-08-01): reject API freeze.**
  The System/User POM plus typed streaming model is coherent, but provider-tool
  authoring still exposes an async dispatcher factory alongside prompt/schema
  declaration. The next component-contract slice must publish a pure tool
  capability declaration and bind dispatcher implementations through a
  host-owned registry; retain the factory-carrying form under advanced
  compatibility until a real host consumes the split. The review also records
  that `#[view(component)]` must not obscure eager durable-definition versus
  deferred POM behavior, and that "functional" means the author API receives
  no host capability, not an unenforceable promise that arbitrary closures have
  no side effects. Reusable streaming components need a first-class instance
  slot/identity helper before authoring ergonomics can be frozen.

The legacy `AgentTurnAuthor` / `PreparedTurn` bridge is now `#[doc(hidden)]`
and absent from `prelude`; a default-feature external compile-fail test keeps
it out of ordinary component authoring. It remains an advanced compatibility
constraint because the legacy public generic `Agent` names it in its bounds;
it is not a mounted production path.

#### Independent Review Refresh (2026-07-31)

- [x] **Component-author/type-system review:** no P0 in the narrow nominal
  `PomView` / `DurableComponent` / `DurableSystem` authoring boundary; do not
  freeze. The review found that `component::advanced::experimental` publicly
  exposed raw compatibility `View`, `system`, `user`, and `binding`, which was
  a second authoring model that bypassed the intended lifecycle roots. That
  surface is now gated behind the non-default `raw-component-ir` feature, and
  a default external consumer compile-fail test prevents accidental import.
  The only public
  executable host remains process-local and `Commit = Never`. Finally,
  `EpochContractId` is author-declared semantic versioning, so a System-only
  policy change must intentionally mount a new epoch; this needs explicit
  author guidance and migration coverage before freeze. The refresh also
  accepts `MountedFeature::compose/project_props/into_harness` as a correct
  pure local carrier, but not yet as a frozen `#[view(component)]` boundary:
  the macro currently evaluates this return eagerly and discards its component
  name, so keyed identity and component-scoped diagnostics remain a P1 design
  decision. `MountedFeature::map_channels` now projects a feature-local channel
  contract into the harness root and its external streaming Live test passes;
  this closes the reuse finding without resolving keyed identity. The loop
  policy is now explicit: `MountedHarnessDefinition` defaults to one turn,
  `TurnLoopPolicy` is selected by the harness author, and a call can only
  narrow it with `with_turn_cap`. The policy is persisted in the durable epoch
  manifest and artifact fingerprint, so policy drift for the same durable
  epoch is rejected before System rerender. It is immutable owner policy;
  ordinary epoch reconfiguration cannot replace it.
  Forgotten City's mounted selector carries its legacy three-turn policy and
  public black-box coverage proves both caller tightening and that a larger cap
  cannot expand a two-turn harness.
- [x] **Public consumer-surface review:** the opaque local facade and pure
  borrowed child props projection are usable from an external consumer; no new
  local-facade P0 was found, but do not freeze. Durable call recovery/control,
  production persistence and remote-session bindings, the real AgentLoop
  install, and golden traces remain release gates.
- [x] **Lifecycle/persistence review:** exact cancellation settlement is
  materially sound as a local proof, but freeze remains rejected. The request
  fences session/epoch/call/input/turn/request/lease/revision; both local stores
  validate active epoch, cursor lineage, pending publication and cleanup
  disposition before writing `Stopped` or `RecoveryRequired`. Focused tests
  cover all three provider cursor dispositions. Private exact-fence recovery
  retry is now idempotent, and seven direct `InMemoryMountedStore` tests close
  local fault parity; public read-only id-keyed lookup now projects a
  lease-free status and has external create/reopen plus active-call
  `Unavailable` coverage, while production persistence, lease policy, reattach
  and recovery/control remain open.

The preliminary outcome is therefore unchanged: the local facade is useful for
development and test, but no API may be described as frozen or production-ready.

#### Independent Review Decision (2026-07-31)

**Decision: continue consumer migration; reject public API freeze.** The three
independent review perspectives agree that the default mounted authoring path
has no release-blocking P0 for local use, but that the present facade is only a
development/test host.

- The component-author review accepts `MountedFeature::compose`,
  `.project_props`, `.map_channels`, and `.into_harness` as a usable pure
  composition path. Its follow-up retained-identity gate is now closed for
  positional/keyed composition and POM-free reopen. Freeze remains blocked on
  host-owned reconfiguration/status and a real consumer migration, not on the
  `MountedFeature` carrier.
  Loop ownership is decided: `TurnLoopPolicy` is harness-owned, calls only
  narrow it, and Forgotten City's selector declares its legacy three-turn
  budget.
- The public-facade review accepts the opaque local `open -> start -> wait /
  cancel -> reload` path without exposing store, lease, revision, actor, or
  attempt typestate. It rejects freeze because `Commit = Never`, local factory
  identity is process-scoped, there is no durable id-keyed recovery/control or
  production reconfiguration/status, and the only stateful provider is a fake
  consumer scaffold.
- The lifecycle/persistence review accepts exact-fence cancellation as a local
  proof: joined cleanup can stop only on authoritative cursor continuity, and
  every ambiguous or conflicting path remains recovery-fenced. A different
  call blocked by that fence now receives the precise public
  `MountedStartError::RecoveryRequired { call_id }` category rather than a
  generic rejection. Borrowed raw execution is now private to
  `mounted_agent.rs` and compiled only for its unit tests; every non-test
  caller enters a detached owner actor before durable admission. This removes
  raw-future drop from the production entry surface without claiming that the
  test helper itself is a durable control API.

The next product slice is therefore not more POM vocabulary. It is a real
Forgotten City mounted host: durable store/outbox, stateful provider-session
factory keyed by epoch/artifact, and PlayerRuntime installation with
legacy/mounted golden traces. A stateless Rig executor that resends System each
turn must remain on the compatibility path; it cannot be presented as this
host.

#### Local Facade Review Round (2026-07-31)

Independent authoring, consumer-surface, and lifecycle/persistence review of
the concrete local host produced the following decisions:

- The public path is `InMemoryMountedAgentFactory::open`, followed by owned
  `MountedCallInput`, `start`, `wait`, `cancel`, and `reload`. The factory
  enters the authoritative bound owner; the external black-box suite proves
  Create, POM-free reopen, exact replay/input collision, cursor rehydration,
  dropped wait, real-time awaited Live effects, and joined cancellation.
- Raw mounted factory/driver traits and `MountedAgent::open` are crate-owned.
  A public trait implementation could otherwise claim System-once semantics
  while returning an arbitrary driver. Compile-fail coverage also prevents
  external direct `MountedHarnessDefinition::render_user`, so capture and User
  rendering remain owner-controlled.
- `MountedTurnCapture` owns its `Transcript` associated type. This eliminates
  the disconnected caller generic that could describe a transcript different
  from the capture contract.
- The local host guarantee is deliberately factory-scoped: clones/reuse share
  one local store; two fresh factories with an equal `DurableSessionId` do not.
  This is documented and tested rather than presented as session-id durability.
- The host-only per-attempt Live factory passed lifecycle re-review with no new
  P0. Binding occurs only for the final `Ready` candidate; replay exits before
  capture/bind/provider work; and one generated attempt identity is shared by
  the factory, streaming attempt, and abort path. The public black-box suite
  now proves identity equality at abort, zero binds for discarded
  `ReplaceHistory` candidates, and bind-failure reservation cleanup with zero
  provider execution plus same-identity retry. Synchronous side-effect-free
  `bind` remains a trusted advanced-host contract rather than a Rust-enforced
  component purity rule.
- Independent mounted public-facade review is complete: the opaque
  `InMemoryMountedAgentFactory` path is usable by an external consumer without
  exposing a store, lease, revision, actor, or attempt typestate. The review
  accepts it as a local development/test host and found no new facade P0 after
  the public Create, replay, POM-free reload/reopen, continuation, Live, and
  joined-cancellation tests. It explicitly rejects an API freeze: the public
  factory is `Commit = Never` and process-local by construction, while raw
  compatibility/lifecycle/persistence modules remain explicitly advanced and
  only a fake stateful provider-session scaffold exists; the real remote
  factory, durable store/outbox, and AgentLoop adapter do not exist yet.

**Verdict: local facade accepted as a test/development host; public API freeze
rejected.** `Commit = Never`, no durable reconfiguration/status API, no
cross-process store/outbox recovery, no real production provider session, no
id-keyed call recovery/control, and no Forgotten City AgentLoop migration
remain material blockers. The provider and persistence adapter shape must be
exercised by the first real migration before the author-facing API is frozen.

The latest public/lifecycle review also records six concrete gates:

- Dropping the owned start future is guarded both before admission and after
  reservation; focused tests prove no provider start and reservation release.
  After provider start, joined cancellation now settles the exact lease to
  `Stopped` only for authoritative `Unchanged`/`ResumeFrom`; all uncertain
  paths enter `RecoveryRequired`. Public lookup and same-owner reattachment are
  now available as lease-free local observations, but durable-id keyed
  cross-owner/process reattach, cancel-by-id, and recovery operations are still
  required before the facade can be called durable.
- [x] `MountedProviderEpochAttacher` now owns the ordinary one-shot attach path
  separately; `DurableMountedProviderExecutor` no longer requires a dummy
  runtime-failing ordinary attach implementation.
- [x] The generic coordinator and Forgotten City fake remote now prove
  `Rendered -> lost attachment reply ->
  ResumeAttachment`: they relinquish the failed attempt's exact fence, reuse
  the rendered artifact, and does not rerender or physically reinstall System
  in their recording providers. Repeat this through the real transport and
  enforce the durable epoch/artifact pair as a server-side idempotency key
  before claiming production physical System-once.
- [x] The raw `View`, role, binding, and compiler entry points are now behind
  the non-default `raw-component-ir` compatibility feature. They remain
  available for migration tests and isolated examples, but are absent from
  ordinary authoring and cannot silently become a lifecycle bypass.
- [x] `MountedFeature` now provides the reusable pure feature carrier: it
  composes ordered durable System/runtime contributions with per-turn User POM
  fragments, supports borrowed `.project_props(...)`, and converts linearly to
  one `MountedHarnessDefinition`. An external local-owner test proves two
  projected features attach one ordered System, instantiate their local
  binding/dispatcher props, and render both User fragments on each call. A
  second feature-specific test proves drop/reopen performs receipt rehydrate
  without executing either System or User renderer. Capture deliberately
  remains host-owned async work, not a feature I/O hook.
- [x] Retain `MountedFeature` authoring identity under `#[view(component)]`.
  The macro-provided component name and optional `.key(...)` now wrap the same
  retained durable System scope and one retained fresh-User subtree. The runtime-only
  reopen projection preserves that scope, so Create/reopen produce equal
  `BindingId`s. Public black-box tests prove stable keyed identity after reopen
  and reject a changed key under the same `EpochContractId` before provider
  attach; they also cover distinct same-name positional siblings, duplicate-key
  rejection, and a keyed parent composing multiple User fragments without
  repeating the parent key. The internal durable-tree test also covers an
  intervening POM-only component scope.
- [x] `examples/pom_feature_composition.rs` now uses the default public
  `MountedFeature -> into_harness -> with_capture` path. It demonstrates two
  projected features contributing both durable System/runtime declarations and
  fresh User fragments, while keeping async capture visibly host-owned.
- System-policy versioning is currently an author/host responsibility through
  `EpochContractId`. A public local lifecycle test now proves changed local POM
  under the same id preserves the already-persisted System; an explicit new
  epoch id/reconfigure remains the only replacement path.

#### Current API decision

`spawn_reconfigure_durable_with` is deliberately crate-private. Its private
proof now runs pure preflight/rebase and the first store transaction before it
returns: a new/resumed operation returns only after the store has durably
admitted that exact id, while an already-activated retry and a pre-admission
failure return a typed start disposition rather than a detached handle. The
terminal durable outcome is still observed through `wait()`. Independent
reviews agree that this one-shot handle stays private: it exposes process-local
completion mechanics and is not a public query/watch API. The future public
submission boundary may acknowledge only `ActorAccepted { id }` after durable
admission; durable progress and replayable terminal state are queried or
watched by that id. The exact Rust method names remain unfrozen, but public code
must not collapse process-local actor ownership into durable store acceptance.

Pre-admission rejection is now also a durable fact: a failed history rebase
records a session-scoped `Rejected { reason, candidate_epoch_contract_id }`
tombstone before any candidate System POM can render. Retrying that id cannot
authorize a later render, and a detached actor persists the tombstone even when
its final observer has gone away. Reopening the owner retains the same query
result without rerendering System. The store rejects wrong-session writes,
fingerprint/base changes, and collisions with in-flight, activated, or aborted
operations while treating an identical rejection write as idempotent. A new
record also requires its expected base to remain active and the cross-process
reconfiguration lane to be empty, so a stale owner cannot write through a
concurrent replacement. This closes the history-rebase ambiguity for the
private store/query proof; failures
or process loss before the first store transaction can still legitimately read
as `NotDurablyAdmitted`, and the query API is not public yet.

The checked review entries below are a chronological decision ledger. Earlier
findings such as "no public facade" remain for audit history and are superseded
by the `Independent Review Refresh` and `Feature Audit Snapshot` above when a
later slice closed them; a checked box means the review occurred, not that its
then-current blocker is still open.

- [x] Component-author/type-system re-review (2026-07-31): **private slice
  accepted; no public freeze**. No new P0 remains in the sealed durable-tree
  boundary. The nominal exact-one `DurableComponent`, ordered `DurableSystem`, and
  synchronous pure streaming reducer are accepted. The previous dual System
  source is now removed: durable open mounts only `RuntimeBinder::durable_system`
  and derives Create plus reopen projections from that one retained tree.
  Compile-fail tests reject durability erasure through ordinary `component` or
  POM `view`, and reject ordinary runtime Components under `durable_system`.
  A behavior test now executes all four mapped lanes through both Create and
  POM-free reopen; a structural test covers ordered `Option`, `Vec`, array, and
  nested durable-tree composition.
- [x] External component-author review (2026-07-31): **authoring boundary
  accepted; no mounted API freeze**. The public integration test compiles an
  independently authored durable tree through only `prelude` APIs, and the
  compile-fail suite proves that `PomView`, `Component`, and `DurableSystem`
  cannot be interchanged across their lifecycle boundaries. Two public-design
  constraints remain explicit: `mount_system_epoch_with_contract` is only an
  isolated one-shot compatibility mount, despite carrying a contract id, and
  must not become the durable owner admission API; and the stable mounted
  facade still needs a root harness contract that binds an immutable durable
  System definition to a per-turn User renderer without reopening the raw
  `experimental` composition path. Durable leaves now derive a collision-free
  structural key from `RuntimeContract` by default; explicit `_with_key`
  helpers remain only as a placement-identity override and do not change the
  durable declaration id/version.
- [x] Consumer review (2026-07-31): **no freeze**. The mounted owner remains
  crate-private and internal outcomes/errors expose receipts, revisions,
  actors, and boxed phases. A private `start_owned(Arc<Props>, Arc<Source>)`
  proof now separates admission from `wait`, survives observer drop, and has
  call-scoped cancellation. The public facade must still narrow that proof to
  stable `Executed | Replayed` outcomes and error categories. It also needs an
  explicit policy for replaying application result projections, because a
  terminal receipt alone does not reconstruct typed Output/Diagnostic values,
  plus an external-consumer compile test; existing internal types must not
  simply be made public.
- [x] Public result-vocabulary slice (2026-07-31): `MountedCallOutcome<C>` now
  exposes `Executed { records, publications, result } | Replayed { result }`
  with a version-free `MountedCallResult` and `MountedTurnPublication` summary.
  Store revisions, request ids, fingerprints, receipts, and leases remain in
  the private `StoredMountedCallOutcome<C, Version>` used by the owner. An
  external consumer compiles the public pattern match and cannot require a
  persistence version to inspect the terminal call identity or publication
  facts. A replay intentionally carries no reconstructed typed
  Output/Diagnostic/Live values. This is only the terminal value contract: no
  public mounted owner, input handle, error taxonomy, or persistence adapter
  exists yet, so it is not an API-freeze approval.
- [x] Public durable-provider adapter slice (2026-07-31): an external adapter
  can now implement `DurableMountedProviderExecutor`. The first attachment
  receives only durable epoch identity, canonical System bytes, and tool
  schemas; it returns an opaque receipt bound to the same artifact
  fingerprint. Rehydration receives only the identity/fingerprint/receipt
  tuple, and a compile-fail test proves that its request has no `system()`
  accessor. The API still exposes neither a mounted owner nor any store,
  lease, revision, actor, or typestate machinery. It is an integration seam
  for the future production adapter, not a freeze approval. Before freeze, a
  black-box facade test must prove that durable Create calls only
  `attach_durable_epoch` and reopen calls only `rehydrate_durable_epoch`; authors
  must not have to guess how that lifecycle relates to the optional ordinary
  `MountedProviderEpochAttacher::attach_epoch` method.
- [x] Lifecycle/persistence review (2026-07-31): the crate-private reopen P0 is
  closed. One store now atomically initializes the durable session seed, owns
  epoch admission/activation, and performs a strict active-artifact snapshot
  read before owner construction. Tests cover differing process-local seeds,
  missing/changed active artifacts, POM-free streaming/native rebind, and one
  System render. The exact-fence `RenderStarted` abort/recovery slice has passed an
  independent re-review: the transaction tombstones the id, advances revision,
  returns epoch A's authoritative snapshot, and the owner installs it while
  holding its configuration/turn locks. `Rendered` remains resume-only.
  A crate-private reconfiguration actor now owns its candidate/rebase/runtime
  lease after process-local spawn, but the start method returns a handle only
  after exact-id durable admission. Dropping the start observer, a later wait
  observer, or the final accepted handle cannot abandon the actor's durable
  responsibility, and it intentionally has no destructive cancel operation.
  **Public freeze remains blocked** on stable public reconfiguration results and
  errors, post-activation local reconstruction or an explicit reopen contract,
  running-call lease renewal or a strict provider deadline, recoverable pending
  mutation/outbox payloads, and a production persistence adapter.
- [x] Managed-reconfiguration admission re-review (2026-07-31): **private
  actor accepted; do not publish its handle**. The actor takes process-local
  ownership immediately, but a successful start now waits for `Create` or
  `ResumeAttachment` durable admission. Pure projection/manifest failures return
  before rebase/store/System render; a failed rebase first persists `Rejected`;
  and an already-activated retry returns a typed terminal start disposition
  instead of another accepted handle. Cancelling the start observer does not
  cancel the actor. A cancelled pending `wait()` returns its
  receiver to the still-live handle, so a later `wait()` can observe the same
  terminal result; dropping the final handle still leaves only durable state.
  The public contract must distinguish local `ActorAccepted` from durable
  admission, expose query/watch by reconfiguration id with replayable terminal
  states (`Activated`, `Rejected`, `RecoveryRequired`, `ReopenRequired`), and
  omit destructive `cancel`; observer detachment carries no authority over the
  durable transition. A stable `ReopenRequired` host contract or B
  reconstruction worker is required when B may be authoritative after a lost
  local reply/install.
- [ ] Public reconfiguration must avoid rerunning application rebase work for a
  known id. The current private proof executes its synchronous pure rebase
  before `acquire_epoch_reconfiguration`, including exact-id retries. Before
  exposure, add an identity-complete durable probe/reservation (base, manifest,
  revision, and id) or replace the closure with declarative binding-owned rebase
  input. Existing `Activated`, `Rejected`, in-flight, and recovery records must
  resolve without invoking host work again.
- [x] Public-surface author review (2026-07-31): **no freeze**. The minimum root
  contract is one retained `DurableSystem` plus one per-turn
  `UserTurnContext<Props> -> UserView` renderer, behind an opaque mounted owner
  with owned call inputs. Provider and persistence adapters remain advanced;
  ordinary authors must not name owner-store, lease, revision, actor, or
  typestate contracts. The review's raw-IR freeze blocker has since been
  addressed by the non-default `raw-component-ir` feature and a default
  external compile-fail proof; it remains a compatibility path, not an
  ordinary authoring escape hatch.
- [x] Reconfiguration terminal-rejection re-review (2026-07-31): **private
  contract accepted; no public freeze**. A failed history rebase now records a
  session-scoped `Rejected` tombstone before candidate System POM can render.
  The id-keyed status projects its rejection reason and candidate contract;
  matching retries cannot render later, and dropping the detached actor's final
  handle does not lose the terminal record. The active epoch, owner session,
  and revision remain unchanged. This closes the private status ambiguity, but
  public query/watch vocabulary, retention policy, and the production store
  adapter remain required before submission can be exposed.
- [x] Independent mounted-public-API re-review (2026-07-31): **reject
  freeze**. The new split is directionally correct: public
  `DurableEpochDefinition` owns only the replaceable durable System epoch, and
  `MountedHarnessDefinition` binds it to the per-turn `UserTurnRenderer`.
  This prevents an epoch replacement from also replacing owner policy. It is
  still an authoring definition, however, not a usable mounted runtime: there
  is no public opaque `MountedAgent`, owned call input, `open`/`reopen`,
  `start`, `wait`, or call-scoped `cancel`; the corresponding owner remains
  crate-private. The external consumer test currently proves only definition
  construction and direct User rendering, not Create -> two calls -> duplicate
  replay -> reopen behavior. The broader `component` surface also still
  exports `experimental` raw IR plus lease, ledger/snapshot, raw publication,
  and store contracts, so an ordinary author can still name machinery that the
  mounted facade is supposed to hide. Finally, the durable provider receipt is
  immutable and has no persisted per-turn provider cursor; this cannot prove
  physical System-once transport for a stateful continuation provider. The
  existing Forgotten City Rig executor rebuilds an agent from `request.system`
  for every turn. The minimum next public slice is an opaque facade backed by
  the existing private owner, semantic non-exhaustive errors/outcomes, and one
  external black-box lifecycle test; keep persistence/provider integration in
  an explicitly advanced boundary and persist a private provider turn cursor
  atomically with accepted turn state before claiming transport-level
  System-once.
- [x] Lifecycle/persistence facade re-review (2026-07-31): **P0, reject
  freeze**. `MountedHarnessDefinition` is still only an immutable definition
  ([`src/component/mounted.rs:180`](src/component/mounted.rs)); the only
  `open`, owned-call, `wait`, `cancel`, and reload implementation remains
  crate-private in [`src/component/mounted_agent.rs:1859`](src/component/mounted_agent.rs).
  Consequently the external test can construct a definition and render a User
  view, but cannot prove Create -> ordinary calls -> duplicate replay ->
  drop/reopen through the public surface. The first facade must expose opaque
  handles and external black-box tests before any System-once claim becomes a
  public guarantee.
- [x] Lifecycle/persistence facade re-review (2026-07-31): **P1, public
  recovery contract incomplete**. Public terminal values are appropriately
  revision-free at [`src/component/mounted_contract.rs:492`](src/component/mounted_contract.rs),
  but the executable errors are still private and contain boxed phase failures
  ([`src/component/mounted_agent.rs:3510`](src/component/mounted_agent.rs)).
  The facade needs stable non-exhaustive open/start/wait/reload recovery
  categories; it must not export those private error types. The direct
  `component` re-exports of call leases, ledgers, snapshots, and raw
  publication types at [`src/component/mod.rs:53`](src/component/mod.rs) also
  remain a freeze blocker for the claimed ordinary-author boundary.
- [x] Lifecycle/persistence cursor re-review (2026-07-31): **P1, keep the
  public freeze blocked**. `ProviderTurnCursor` now binds adapter/schema,
  durable epoch, artifact fingerprint, and opaque adapter state. A provider
  may return it through `MountedProviderExit::completed_with_cursor`; the
  mounted mutation carries it with the accepted session/call/outbox state,
  `MountedProviderRequest` receives it on later turns, and direct coordinator
  tests prove valid rehydrate, wrong-cursor rejection, and no System rerender.
  `BoundMountedAgent::open` now reloads the authoritative snapshot after each
  provider rehydrate and catches up through a bounded stable cursor sequence;
  the owner test rehydrates C1 -> C2 -> C3 and verifies the next ordinary
  request receives C3. Reconfiguration also clears A's cursor before B activation and
  B reopen. The owner-level race proof now advances a real concurrent turn
  after the final open snapshot and proves the stale opener becomes
  `ReloadRequired` before User preparation or provider work. A subsequent
  `reload()` rehydrates a fresh provider binding whenever the committed cursor
  changed; its C1 -> C2 test proves the new binding is installed before the
  next ordinary request. `cargo test --lib` passes 301 tests. The persistence
  boundary is now closed by:

  - [x] Active-artifact/cursor persistence fence: `publish_claimed_turn` now
    receives the expected `ActiveEpochArtifact`; the publication adapter binds
    that artifact at call start, and the store compares it with its active
    pointer while validating both the previous and next provider cursor in the
    same publication transaction. Strict active-owner snapshot reads also
    validate the persisted cursor before returning it. Focused tests prove a
    replaced System epoch cannot commit a stale turn or mutate session/outbox,
    and a corrupt cursor cannot escape either ordinary reopen or
    reconfiguration-recovery snapshot loading.
  Required end-to-end proof: a provider returns `completed_with_cursor`, the
  accepted publication persists it, a dropped/reopened owner rehydrates with
  exactly that value and sends it on its next ordinary request; a mismatched
  cursor rejects without publication; reconfiguration clears it; concurrent
  publication/open cannot leave a stale provider binding runnable; and reload
  rebuilds the provider binding before an advanced cursor can resume work.

The post-review correction splits immutable mounted owner policy from the
replaceable epoch. `MountedAgentBinding` retains session/store, reducer,
request-id, publication, attempt policy, and host/runtime compatibility
fingerprints. `MountedEpochDefinition` alone contains the epoch contract,
System POM, and runtime/capability projection. Reconfiguration accepts only
the latter, so it cannot replace owner policy by construction.

Epoch-scoped provider System transport, private cross-process rehydration, the
single-source durable leaf, compiler-derived manifest/rebind projection, and
private `RenderStarted` recovery are implemented. `MountedCallOutcome` now
fixes the result/replay distinction, and the opaque public local facade now
exposes owned input plus stable open/start/wait/reload recovery categories.
The next work is a production persistence/outbox adapter, a stateful provider
session, and Forgotten City AgentLoop migration. None of the current
`pub(crate)` owner/store/typestate contracts will be promoted wholesale.

The durable tree invariant is now enforced at compile, mount, and artifact
boundaries: a durable declaration id can belong to only one binding or provider
capability leaf in a mount plan, and a persisted manifest rejects an id shared
between a binding and a provider capability. A provider capability group may
still expose multiple tool schemas under its one identity. Create preserves POM
and durable-leaf author order; reopen filters that same tree without executing
any POM node.

### Verified Baseline

- The provider receives System POM and native-tool schemas only through one
  epoch attachment. Ordinary preparation/execution requests cannot carry them.
- Store-backed call admission is two-phase: `Reserved` before capture/render,
  then `Running` immediately before provider/tool/Live I/O. Pre-start failure
  restores or removes the reservation. Joined cancellation settles an exact
  Running fence to `Stopped` only with authoritative cursor continuity;
  expiry, ambiguity and fence conflicts enter model-recovery state.
- Durable epoch artifacts use format v4. The manifest includes the
  harness-owned turn-loop limit, capability-group metadata used for POM-free
  provider rebind, and compiled component-binding identities. Earlier v1/v2/v3
  artifacts are intentionally unsupported; a host must mount a fresh epoch
  rather than infer dispatcher grouping, `Wait`/`Continue` behavior, or a
  component tree identity.
- All 122 crate-private mounted-agent tests pass and cover continuation,
  reservation cleanup, concurrent owner, publication recovery, cancellation,
  provider-epoch reopen, durable initial-session reuse, active-artifact fault
  injection, awaited real-time Live delivery after rebind, and durable
  reconfiguration concurrency/crash/cancellation boundaries.
- Durable reconfiguration has 13 focused passing tests: 10 direct transaction
  and recovery tests plus 3 owned-actor contract tests.
  It proves same-owner call pinning, cross-owner call fencing, POM-free
  Activated retry, exact-fence abort and tombstone recovery after
  `RenderStarted`, attachment resume without abort, owner-A reload, and
  poison-plus-reopen when caller cancellation occurs after activation, a lost
  post-commit activation response that fences A before POM-free reopen B, and
  a detached reconfiguration actor that reaches B after its observer and final
  handle are dropped.
- On 2026-08-01, default `cargo test --workspace --all-targets` passes,
  including 301 library tests, all default integration/example targets, 31 component compile-fail
  cases, nine public-boundary compile-fail cases (raw-IR default gating, the
  durable-provider attachment split, no legacy turn bridge, host integration,
  or async provider-dispatcher binding in the author prelude, and no System
  access during provider rehydrate),
  and 22 POM compile-fail cases. `cargo check --examples`, `cargo clippy
  --workspace --all-targets --all-features -- -D warnings`, rustdoc with
  `-D warnings`, `cargo test --doc`, `cargo fmt --all -- --check`, and
  `git diff --check` also pass. The separate `cargo test --all-targets
  --features raw-component-ir` compatibility matrix also passes. The prelude-only
  `pom_durable_authoring` example compiles as an external-authoring proof.
  Forgotten City `cargo check -p engine --all-targets`, its 88-passed/1-ignored
  engine library suite, the 21-test mounted SelectIntent suite, and the
  eight-test stateful-provider scaffold also pass against this state.
  The combined reopen proof preserves `policy -> streaming -> post-rules ->
  native-tool` System order, renders it once, and reopens both runtime kinds
  without executing POM. The public local facade itself now has 38 passing
  external-authoring tests, including keyed feature identity across reopen and
  key-change contract rejection. Full freeze remains blocked on durable call
  recovery/control, production persistence/provider bindings, and Forgotten
  City migration.
- Forgotten City `cargo check -p engine -p agent_runtime --all-targets` passes
  without warnings. The stateful-provider scaffold explicitly allows dead code,
  so this remains evidence that the consumer-shaped path compiles and tests,
  not that it is installed in production.
- Forgotten City's database-free semantic-graph suite passes 210 tests. The
  PostgreSQL-backed commit/resume suite remains environment-gated and is not
  counted as verified production persistence.

### Feature Audit Snapshot (2026-07-31)

This board uses the status definitions from
[`docs/agentview-feature-list.md`](docs/agentview-feature-list.md). A type or
isolated unit test is not a production claim.

| Feature | Status | Implemented / verified now | Next release gate |
|---|---|---|---|
| `AV-F01` Application Definition | `local-proof` | One retained `DurableSystem`; public `MountedFeature` composes ordered durable System/runtime plus per-turn User fragments; Create renders/attaches once and feature-specific reopen is POM-free; macro component name/key are retained across Create/reopen. | Production owner/transport and a real consumer migration. |
| `AV-F02` Typed Application View | `implemented` | Typed Markdown/XML POM, derive, role resolution, canonical render, and compile-fail boundaries. | Consumer migrations off legacy prompt APIs. |
| `AV-F03` Semantic View Update | `implemented` | Full/delta/delete, stable collection identity, transactional `UserDocumentCursor`, and rollback tests. | Consumer delta traces and daemon patch policy. |
| `AV-F04` Turn Composition | `implemented` | Authored context/artifact/feedback/task order plus fresh owned capture/render for preparation and Continue; pure child props projection is externally tested for factory/provider contexts and composed feature System/User roots; `MountedFeature::map_channels` has an external feature-local streaming Live proof, and macro component name/key are retained across System/User projections. | Real AgentLoop mounted User root; capture/provider/store remain host-owned boundaries. |
| `AV-F05` Action Surface and Validation | `partial` | Typed XML/native declarations, schemas, dispatch, captured props, and terminal/expected error policy. | Consumer authoritative revalidation and stale-action feedback. |
| `AV-F06` Structured Streaming | `local-proof` | Strict incremental parser and fresh reducers; Forgotten City mounted SelectIntent focused suite is 21/21, including legacy-equivalent initial/retry User POM, accepted-stream callback ordering, mounted loop policy, drop/reopen no-second-System and runtime-drop joined cancellation. | Production selector install and Phrase migration. |
| `AV-F07` Provider-native Tools | `local-proof` | Pure `ProviderCapabilityContract`, versioned host `ProviderDispatcherRegistry`, generic `MountedHostBindings`, Create/reopen manifest preflight, grouped ordered dispatch, correlation, result routing, attempt replay, and collision tests. | Real Provider/Cube/Forgotten City adapters, legacy builder quarantine, and durable cross-attempt replay policy. |
| `AV-F08` Effect Lifecycle | `partial` | Typed lanes, awaited Live, compensation, private Commit staging; selector proves ordered/revocable Live delivery. Forgotten City's SQLite host now supports stable-id player Commit dedup, atomic board/job transaction, fenced Stockfish completion, expiry reclaim, retry/dead-letter, and capture admission across pending/delivering/dead-letter states. | Recovery-scanned supervisor, passive engine-failure observer, child scopes, and authoritative production effect adapter. |
| `AV-F09` Reactive Observe/Act | `partial` | Public legacy observe/hook/act has epoch and stale-turn fencing. The advanced mounted external controller adds typed Actionable/Passive frames, stable action/reply/wake/source and User-delivery identities, host-owned System/User outbox publication, ack-before-act, acknowledged cursor delta, exact replay, safe cancellation/full-resync, typed decode, hook and recovery fencing. Forgotten City's SQLite facade and repository CLI/skill add stable consumer identity, System attach/ack, exact action handles and raw XML, and exercise multi-round full -> passive full -> delta against real Stockfish. | Managed remote/server transport, bounded durable receipt/reply retention, process-kill recovery, replacement-consumer/cross-transport handoff, and production supervision/migration. |
| `AV-F10` Model Turn Loop | `implemented` | Compatibility transactional loop and public local mounted Continue recapture are executable. `TurnLoopPolicy` fixes the mounted loop bound at the durable harness; call inputs can only lower it, same-id policy drift is rejected, and the selector preserves its three-turn migration budget. | Production mounted loop and durable continuation ownership. |
| `AV-F11` Session/Fork/Isolation | `partial` | Compatibility fork/cursor and factory-scoped mounted isolation are tested. | Durable child sessions, scope ownership, and production registry. |
| `AV-F12` Safe Call Lifecycle | `local-proof` | Admission/replay/collision, start-future guards, dropped wait, joined cancel, Live cleanup, and FC stale-delivery withdrawal are covered. Exact-fence owner and actual process-local store tests prove all cursor dispositions, reason mismatch, timeout, revision/publication/epoch/stored-or-replacement-cursor/lease recovery, foreign-fence rejection, and same-fence recovery retry without another revision. The internal reconciliation controller now claims a persisted recovery fence, reconstructs the exact remote operation, restores only a live-fenced `NeverAccepted` checkpoint, and retains recovery for every other status; stale-fence and durable-backend reload tests cover this transition without provider/tool/Live replay. Public `MountedAgent::lookup` projects a lease-free durable call snapshot before and after reopen and reports admitted progress from the same owner instance without waiting behind an active call. That owner can restore one attached handle after handle loss; overlapping or cross-owner reattach remains read-only. Borrowed raw execution is test-only/module-private, so production entry always has a detached owner. | Bind observation into a production supervisor; add fenced cross-owner/process control and recovery, production lease policy, a concrete durable store/transport adapter, and a real provider operation ledger. |
| `AV-F13` Durable Lifecycle | `partial` | Private ledger/lease/CAS/publication/reconfiguration recovery state machines plus an advanced public `DurableMountedAgentFactory`; an external backend proof covers atomic outbox, conflict retry, cross-factory reopen/replay, cursor reuse, and v3 replacement-owner delta continuation (with a v2 full-resync migration). Forgotten City's SQLite proof additionally covers apply-before-ack replay, stable item-id domain dedup, fenced Stockfish jobs, poison-row quarantine, per-session ordering, dead-letter recovery admission, and structured delta after replacement-owner reopen. | Crash/indeterminate supervisor, true process-restart trace, passive recovery publication, public reconfigure/recovery and production installation. |
| `AV-F14` Provider Independence | `partial` | Public ordinary/durable attach split; generic and FC fake lost-reply resume are no-second-System; FC adapter is 8/8 for resume, cursor, User-only resync after rehydrate, a PlayerRuntime-generated resync, joined cancel, Running inspection, terminal-cancel fencing, and the real mounted selector composition path. | Real remote factory/install and production transport proof. |
| `AV-F15` Reconfiguration | `internal-proof` | Private admission, tombstones, activation, supersession, crash recovery, and POM-free retry; public local same-id reopen preserves the first System artifact. | Public id-keyed submit/query/watch and production retention. |
| `AV-F16` Inspection/Replay | `partial` | Compatibility observer events and internal mounted identities exist; Forgotten City SelectIntent compares System, initial/retry User, and accepted XML callback Output/Live ordering across legacy and mounted paths. | Public epoch/turn lookup, unified mounted trace, and complete invalid-input/abort/tool/world/durable-side-effect golden tests. |

Current aggregate: 4 `implemented`, 4 `local-proof`, 7 `partial`, and 1
`internal-proof`; zero production
consumer migrations are complete. Status labels are intentionally conservative:
the immediate P0 is production id-keyed `AV-F12` recovery/control, followed by the production
portions of `AV-F13`/`AV-F14` and the remaining `AV-F16` golden comparison.

Architecture map: [POM Component Runtime](docs/pom-component-runtime.html),
generated from `docs/pom-component-runtime.architecture.json`. The current map
covers the isolated mounted POM runtime, private erased epoch storage, and
object-safe combined-attempt proof. Expanding the provider-capability
declaration details and the future managed AgentLoop owner remains pending.

Target authoring examples and independent review findings:
[POM Component Authoring Examples](docs/pom-component-authoring-examples.md).

## Vocabulary and Ownership

| Concept | Responsibility | Lifetime / side effects |
|---|---|---|
| POM | Structured prompt data | Immutable values; no side effects |
| `#[view(component)]` | Synchronous composition of POM and declarations | Pure |
| `DurableSystem<C, Props>` | One ordered source for stable System POM and durable runtime leaves | Definition per process; POM consumed once per durable epoch |
| `SystemView<C, Props>` | Explicit one-shot compatibility root | Never used by durable reopen |
| User View | Author application-selected context, artifacts, and task | Per preparation attempt |
| `ComponentNode` | Short-lived authoring IR | Exists only during its render |
| `PomView` | Pure POM-only child that can compose under either lifecycle root | Deferred and side-effect free |
| `DurableComponent<C, Props>` | POM plus exactly one typed runtime/capability declaration | Retained for Create and POM-free reopen |
| mount compiler | Compile System POM and reusable binding factories | Once per epoch; pure |
| mounted binding | Own contract/session identity, one `Arc<DurableSystem>`, store, and reducer policy | Stable for one durable owner |
| `BindingFactory` | Create a fresh binding/reducer instance | Reused across turns; no I/O |
| `BindingPlan` | Concrete bindings selected for one prepared turn | Discarded on replacement/abort |
| streaming reducer | Reduce parser events into typed values as the stream arrives | Real-time and pure |
| live-effect runtime | Interpret effects that must be visible during the active stream | Explicit host I/O and cancellation |
| managed attempt owner | Keep one erased pre-publication attempt alive across caller cancellation and transfer it to durable publication | Private actor; explicit abort acknowledgement; `Transferred` only after the new actor starts |
| mounted turn preparation owner | Pin one epoch and logical turn across context replacement, rerender only User, and start exactly one final-ready attempt | Crate-private proof; returns a publication-pending completion and does not mutate the authoritative session |
| provider capability plan | Epoch-static native tool/schema declarations plus per-attempt grouped dispatcher factory | Out-of-band provider metadata; never rendered as POM text |
| Commit stager | Purely encode one typed Commit into one versioned durable payload | Synchronous, one-to-one, installed before root erasure |
| publication store | CAS the session revision and insert ordered outbox rows in one transaction | Host I/O; keyed by a preallocated durable request id |
| outbox worker | Deliver already-persisted Commit payloads after publication | At-least-once external I/O with item-level idempotency |
| fallible provider wire | Serial Text/native-tool ingress with acknowledged success or terminal failure | Per attempt; separate from legacy `TurnSink` |
| `Agent` | Preparation, provider execution, cursor, session publication, and epoch coordination | Host-owned |

`ComponentHarness` is not itself a Component. Async state capture, provider I/O,
session publication, and effect delivery are host lifecycle operations outside
the functional component boundary.

## Architectural Invariants

### System Epoch

- Exactly one logical System prompt exists in one active harness epoch.
- The binding-owned `DurableSystem` receives stable owned/`Arc` configuration
  only. It cannot read history, task, call props, current view, artifacts, or
  mutable Agent Context.
- Durable System authoring is one ordered tree. Policy POM, streaming contracts,
  intervening POM, and native-tool contracts may interleave without a second
  authoring callback.
- A successful Create consumes the System POM projection exactly once, resolves
  and renders it once, and validates runtime declarations from the same tree.
- Ordinary turns retrieve the stored System result; they never traverse or
  render the System subtree.
- Context compaction changes history and the next User POM only. It does not
  remount System.
- The provider executor receives System bytes and the native-tool catalog only
  through `attach_epoch(MountedProviderEpoch)` when the epoch opens. An
  ordinary `MountedProviderRequest` has no System or tool fields, so context
  replacement, retry, and continuation cannot reattach a second System prompt.
- The returned provider epoch binding is opaque to AgentView, immutable and
  share-safe by contract. A fork shares that binding; an explicit successful
  reconfiguration attaches one new binding only after its history rebase has
  succeeded.
- This is an AgentView logical/provider-binding guarantee. A provider adapter
  that internally rebuilds stateless HTTP payloads can still copy its retained
  System bytes; a physical transmit-once requirement needs a stateful provider
  session adapter plus an integration test.
- An explicit host reconfiguration creates a new `HarnessEpoch`. System POM,
  prompt-facing contracts, and binding factories switch atomically.
- If history continues across an epoch change, the host must rebase/compact it
  before the next provider call. A required policy change never waits for
  incidental token-pressure compaction.

#### Durable open and rehydration

`DurableSystem` definition construction and POM rendering are different
phases. `RuntimeBinder::durable_system` returns the one complete retained tree.
Create consumes its POM projection; reopen asks that same tree only for its
runtime projection. The reopen path cannot inspect, resolve, render, or
transport POM, and it validates every rebuilt declaration against the persisted
epoch artifact before a turn can start. The one-shot `SystemView` callback is
not part of this durable path.

The durable open state machine is:

```text
Acquire (store-owned CAS)
  -> Create { durable_epoch_id, fence }     exactly one winner
       -> RenderStarted                     consume System POM projection once
       -> ArtifactStored                    canonical System + manifest
       -> Attaching                         durable_epoch_id is idempotency key
       -> Active { provider_receipt }
  -> Existing { artifact, provider_receipt }
       -> DurableSystem::runtime_projection no POM and no System transport
       -> provider.rehydrate_epoch          receipt only
  -> InFlight                               wait/retry; never render
  -> Conflict / RecoveryRequired            explicit host action
```

- `DurableEpochId` is allocated by the authoritative store. The existing
  `HarnessEpochId` remains process-local diagnostic identity and must never be
  persisted or used for provider idempotency.
- The pre-render manifest projection covers the epoch contract, runtime
  declaration ids/routes/versions, provider tool schemas, and binder contract.
  Create then derives the compiled manifest from the same retained tree and
  requires an exact match before activation.
- `EpochArtifact` persists the durable epoch id, manifest, canonical rendered
  System identity/content, normalized runtime/tool descriptors, provider
  attachment state, and rehydration receipt.
- Initial provider attachment receives System and tools plus
  `DurableEpochId`. Retrying an indeterminate attachment uses that same key and
  must be idempotent. Rehydration receives only the durable id, verified
  artifact identity, and provider receipt; its request type has no System or
  tool payload.
- Under the strict at-most-once contract, a crash after `RenderStarted` but
  before an artifact is durably stored yields `EpochRecoveryRequired`. The
  runtime does not silently consume/render System POM again. After the exact
  store fence expires, recovery may atomically tombstone that reconfiguration,
  discard only its staged rebase, advance revision, and restore epoch A from
  the returned authoritative snapshot. A later System attempt requires a new
  reconfiguration id and a new epoch fence.
- A runtime binder mismatch, including the same contract id with changed
  factory version, route, tool schema, or provider catalog, rejects reopen
  before provider or reducer work. It never falls back to remounting System.
- Reconfiguration creates and attaches a new durable epoch, then atomically
  commits its artifact, rebased session/history, and active-epoch pointer with
  one store transaction. Crate-private `reconfigure_durable_with` now performs
  this proof and accepts only a `MountedEpochDefinition`; immutable owner
  policy cannot be replaced. A same-owner call holds the old complete bundle
  until publication, while a store-owned call from another process fences the
  candidate before System render.
- A completed reconfiguration id is store-idempotent and reopens POM-free. A
  crash at `RenderStarted` never rerenders System POM. Its fenced abort is now a
  durable store transition with an `Aborted` tombstone; `Rendered` can only
  resume attachment. The current conservative cancellation guard still poisons
  an old process owner if B activated before local installation and requires a
  fresh open against B. The public API also still needs a managed operation
  whose accepted work is not cancelled by dropping its waiter.

### User Turns

- The application defines the contents and order inside `user(...)`.
  `context`, `artifacts`, and `task` are examples, not framework-injected
  sections.
- Only explicit `DiffSlot` values use full/delta/omitted behavior.
- Context replacement discards the entire candidate `TurnPlan`, including
  concrete binding instances, then rerenders the User View against the new
  draft context.
- `TurnFlow::Continue` renders another User turn and creates fresh bindings
  while keeping the same mounted System epoch.
- Typed call props are borrowed across preparation attempts and loop
  iterations. They never enter `PromptContext`, history, `DiffSlot`, or the
  `UserDocumentCursor`.

### Streaming and Effects

- Streaming is selected by mounting a provided component such as
  `StreamingXml`; AgentLoop contains no streaming-specific branch.
- A streaming component contributes its prompt-facing contract and its
  reusable binding factory in the same System mount.
- Each executable provider attempt after final Ready creates new parser/reducer
  state from that factory. Finished, failed, or aborted state is never reused.
- Every executable provider attempt has an internal runtime identity. User call
  labels are observability strings, not resource or idempotency keys.
- Reducers run in real time on `open`, `stream`, and `complete` parser events.
  They are not delayed until the full provider output has completed.
- Reducers remain deterministic and perform no application I/O. They emit
  typed diagnostics, output values, `LiveEffect` values, or `CommitEffect`
  values.
- `LiveEffect` values are interpreted during the active stream and therefore
  require explicit finish, cancellation, timeout, and compensation semantics.
- The default live runtime is awaited inside the current parser event before
  the next event is reduced. This preserves callback order and backpressure;
  fire-and-forget delivery requires an explicit application policy.
- `CommitEffect` values are accumulated privately during the attempt, then a
  typed, synchronous `CommitStager<C>` encodes them before publication.
- A stable `PublicationRequestId` and `(request id, item index)` identities are
  allocated before the store call. `ProviderAttemptIdentity` remains diagnostic
  and is never converted into a durable key.
- The publication store compare-and-swaps the expected session revision and
  writes the complete prepared session mutation plus every staged outbox row in
  one transaction. Published means durably queued, not externally delivered.
- A definitely rejected write may return to the abortable Finished state. A
  cancelled or indeterminate write must resolve the same request id before the
  owner may abort or retry.
- A live tool that returns a result to the active model loop is a real-time
  binding/runtime capability, not a post-commit effect.
- A provided component may contribute POM, provider-facing schema metadata, or
  both. A native tool schema is itself a prompt-facing provider contract; it is
  not rendered into a third POM document and does not require duplicate prose
  unless the application deliberately authors that prose.
- Native tool schemas are mounted as groups. Each final-ready attempt creates
  fresh grouped dispatchers with the same `ProviderAttemptIdentity` and live
  scope as its XML bindings; calls are serialized in provider order.
- Native dispatcher updates use the same awaited `LiveEffectRuntime` and private
  Commit buffer as XML reducers. A provider result is recorded for publication,
  while Commit values remain unavailable until publication succeeds.
- An identical native invocation id, result correlation, and payload replays
  its in-memory result without I/O or effects; reusing that invocation id with
  a different call is terminal. The optional provider result-correlation id is
  preserved separately through replay and publication. Unknown names and
  malformed arguments or invocation identities are model-visible
  `ProviderToolResponse::Error` values; dispatcher, Live, finish, and abort
  infrastructure failures are terminal and retain provider call metadata where
  one exists.
- `ProviderToolCatalog` is an immutable, data-only snapshot of mounted schemas.
  It crosses the provider boundary only inside `MountedProviderEpoch` during
  `attach_epoch`; preparation and execution receive only the returned opaque
  epoch binding. Dispatcher factories and turn props never cross that boundary.
- The mounted owner creates a `ProviderCancellationSource`, passes its token to
  `MountedProviderExecutor::execute`, and joins the executor future after
  cancellation. The adapter returns a terminal `MountedProviderExit`; dropping
  the execute future is not cancellation acknowledgement.
- Provider cancellation is retained state, not an edge-triggered wakeup. A
  token created or first-polled after cancellation must still observe the exact
  first reason, and every concurrent waiter must wake.
- The managed active attempt implements the separate fallible wire port. Every
  submitted event is serialized and acknowledged only after typed staging and
  awaited Live handling; terminal host failure is returned immediately.

### Typed Authoring and Runtime Type Erasure

- A root type such as `PlayerChannels` is a zero-sized compile-time contract for
  its `Output`, `Live`, `Commit`, and `Diagnostic` associated types. Provider
  ingress is normalized inside the mounted host and is not component state.
- Child channels remain fully typed through component authoring and are lifted
  through `ChannelMap<Local, Root>` before erasure. Type erasure does not replace
  root-channel normalization or move mapping failures into AgentLoop.
- Concrete reducer/factory implementation types may already be erased inside a
  typed carrier such as `FactoryDeclaration<C>`; that does not erase its channel
  contract. The normalized root channel contract is erased exactly once, when
  `MountPlan<Root>` is installed into the host runtime.
- An erased mounted epoch owns only reusable factories and static provider
  capability declarations. Fresh binding instances and dispatchers belong to a
  separate per-provider-attempt runtime, which owns event serialization and is
  finished or acknowledged-aborted before disposal.
- `MountedEpoch<C, Props>` now wraps one non-generic private epoch storage shape;
  two different root contracts can coexist there. A crate-private object-safe
  attempt driver now proves the combined text/XML/native path can also coexist
  in non-generic active/finished/published storage. Installing that driver into
  AgentLoop is still pending, and attempt state must never move into the epoch.
- The erased representation retains a channel descriptor containing in-process
  `TypeId` and diagnostic `type_name` information for the root contract and all
  five associated types. Persisted or cross-binary identity must use an explicit
  stable contract key; neither `TypeId` nor `type_name` is a durable identifier.
- Type metadata is not an executable contract. The mounted-epoch adapter owns
  its private downcast; the attempt driver instead installs a monomorphized
  typed update interpreter before erasure and sends no lane payload through
  `Any`.
- The legacy object-safe attempt proof carries only closed provider wire input and
  provider-neutral tool results. Output/Diagnostic remain typed in the shim,
  Live is awaited before the driver returns, Commit stays retained behind its
  process-local publication gate, and terminal control never becomes a value lane.
- `Any`, unchecked casts, and manual downcasts never appear in the component or
  application authoring API. The typed `MountedEpoch<Root, Props>` handle wraps
  erased internal storage and recovers the installed state through a private
  monomorphized adapter.
- `BindingInstance` and `ProviderDispatcher` now have executable isolated
  contracts for event/dispatch, finish, failure, and explicit abort.
  `ChannelTypeInfo`, one-time mounted-epoch storage erasure, and the isolated
  object-safe attempt vtable are implemented. A crate-private managed actor now
  owns Active/Finished state across cancellation and automatically aborts when
  its last client disappears. The erased Published typestate also has a typed,
  attempt-local Commit interpreter that retains a failed batch for retry.
  A separate durable-publication proof now stages typed Commit values before
  erasure, distinguishes rejected from indeterminate writes, and has a private
  actor that completes or resolves an in-flight store call after caller
  cancellation. Real AgentLoop ownership, a concrete durable session store,
  crash recovery, and an outbox worker remain later boundaries.

## Explicit Non-goals

- Copying Dioxus VDOM, reconciliation, signals, scheduler, or hooks runtime
- A third `Provided Components` prompt document
- Rendering the full component tree every turn and merely ignoring System
- Dynamic System contracts driven by turn props or compacted history
- Letting the model or compactor request a System mutation
- Async component render functions
- Erasing the local channel contract before parent/root mapping
- Exposing an `Any`-valued effect bag as the public component API
- A second POM diff implementation
- Hiding live-effect cancellation behind `Drop`
- Claiming exactly-once external effects without a durable outbox

## Current Implementation Gap

The repository now contains both the compatibility combined-render path and an
executable mounted path. The mounted path proves the intended System/User and
runtime lifecycles locally; the remaining gap is production ownership and
consumer cutover, not the absence of a component/runtime prototype.

| Current implementation | Required change |
|---|---|
| `#[view(component)]` now creates a deferred typed node with owned/`Arc` props | Route that deferred call through the future mount/User lifecycle compilers rather than the compatibility combined compiler |
| `ComponentHarness::render` builds System, User, and hooks together | Split once-only System mount from per-attempt User render |
| `PreparedTurn` and `TurnPlan` carry a System document every turn | Mounted state owns System; `TurnPlan` carries User plus binding instances |
| `AgentTurnAuthor::prepare_turn` runs inside every context-preparation attempt | Component authoring path mounts before the turn loop |
| successful turns call `set_system_snapshot` repeatedly | Initialize one immutable epoch System; remove per-turn overwrite |
| `SystemPromptRendered` is emitted for every turn | Emit a mount event once; distinguish it from wire-request assembly |
| Compatibility `StreamingBinding` owns one state value and an `FnOnce` finish reducer | The isolated mounted path now stores reusable `Fn` declarations and creates a fresh instance per provider attempt; migrate AgentLoop only after typed turn-prop initialization and fallible runtime control exist |
| Mounted reducers emit typed `StreamUpdate`; the isolated attempt now awaits `LiveEffectRuntime` in callback order, hides Commit until publication, and has separate binding/live terminal failures | Adapt the fallible attempt into the executor sink without losing backpressure or cancellation |
| `TurnSink::on_event` returns `()` | Use the new independent `MountedProviderExecutor + FallibleProviderWirePort` path; do not adapt it through the legacy sink |
| legacy compiler records placement but does not enforce it; parallel `MountPlan<C>` rejects non-System declarations | Seal the rule in explicit System/User lifecycle root types |
| `PomView` is strict and `MountProvidedView<C>` proves normalized heterogeneous declarations, while `ProvidedView<B>` remains the compatibility carrier | Freeze the final public `ProvidedView<C>` spelling when reusable factory inputs are known |
| mounted XML factories and native provider dispatcher groups now receive exact typed props, are created under one attempt identity, and route typed output/live/commit/diagnostic updates through the isolated runtime | Adapt the unified attempt into AgentLoop/executor without losing replay, collision, terminal failure, abort, or publication semantics |
| `MountedEpoch<C, Props>` is a typed handle over non-generic private storage; a crate-private object-safe driver now preserves Active -> Finished -> Published/Aborted for the combined text/native path and installs the typed update interpreter before erasure | Make AgentLoop own that driver, automatically route every terminal failure to async abort, and define cancellation of in-flight consuming transitions |
| `CommitStager`, staged outbox identities, CAS store contract, resolution state machine, and cancellation owner are executable isolated proofs | Replace async mutating `commit_turn` with a pure complete session mutation, implement a real store/worker, and install the owner in `MountedAgent` |
| isolated `MountedEpoch<C>` now owns raw/resolved/rendered System plus factory/capability plans | Replace the compatibility `ComponentHarness::render` path with this lifecycle only after streaming declarations become executable factories |
| crate-private `MountedAgent` owns the in-memory epoch/session snapshot, borrowed logical-call props, per-candidate async capture, final-only attempt creation, runtime leases, provider cancellation/timeout poison, fork, and atomic reconfigure | Convert successful completion into one pure durable mutation, hand it to publication, write back the committed session, and keep the same epoch across `TurnFlow::Continue` |
| System and User share one positional component identity root | Give mount and turn trees independent identity roots |
| tests put history and call props into the System contract | Replace them with one-shot System count and invariant tests |
| System text can exist in both mounted config and mutable prompt context | Choose one authoritative epoch bundle and define persistence/resume checks |

## Dependency Roadmap

```text
P0 POM foundation
  -> P1 Component authoring/compiler prototype
  -> P2 Provisional agent transaction integration
  -> P3 Mounted System + per-turn User lifecycle
  -> P4 Real-time runtime + commit effect boundary
  -> P5 AgentView vertical reference
  -> P6 Forgotten City SelectIntent
  -> P7 Forgotten City PhraseTool / live effects
  -> P8 Cube Stage structured-tool harness
  -> P9 Remaining migration and legacy removal
```

## Proving Grounds

| Proving ground | What it must prove | Target phase |
|---|---|---|
| AgentView `pom_feature_showcase` | Existing typed POM coverage remains the only prompt representation | P5 |
| AgentView `pom_component_composition` | Strict pure/provided composition, local binding mapping, and identity | P3 |
| AgentView `pom_streaming_channels` | One XML tag can emit multiple mapped lanes and preserve closing-chunk text | P3/P4 |
| AgentView `pom_mount_plan` | Heterogeneous factories and provider tools normalize into validated plans | P3 |
| AgentView `pom_mounted_lifecycle` | A non-capturing System root mounts once; POM-only User roots render independently per attempt | P3 |
| AgentView `pom_mounted_streaming` | Fresh reducer state, shared-parser wire order, awaited Live effects, compensation, and publication-gated Commit | P3/P4 |
| AgentView `pom_durable_publication` | Typed Commit staging, stable outbox ids, session-revision CAS, and a published result with no in-memory Commit escape hatch | P4/P5 |
| AgentView streaming example | One mounted contract creates fresh real-time reducers on every turn | P5 |
| Forgotten City `SelectIntentTool` | Production XML streaming preserves event timing and produces typed effects | P6 |
| Forgotten City `PhraseTool` | Incremental visible output has explicit finish, abort, and compensation | P7 |
| Cube Stage Director | Provider tools, results, external I/O, and multi-turn loops compose under one epoch | P8 |

Cube Stage is not another small parser example. Its current live sink calls a
ToolServer and returns results to the active model loop. The provided component
must preserve that real-time round trip rather than convert it into a
post-commit callback.

### P0 - POM Foundation: Complete

- [x] System and User prompts are typed `Document` values.
- [x] Role-specific resolution and canonical rendering are established.
- [x] `DiffSlot` and `UserDocumentCursor` commit with the agent session.
- [x] Turn artifacts are resolved POM rather than trusted raw prompt text.

Exit gate: prompt construction no longer depends on string templates for the
new path.

### P1 - Component Authoring and Compiler Prototype: Complete

- [x] Implement `ComponentNode`, fragments, role placement, child mounting,
      stable identity, and declarative bindings.
- [x] Diagnose unplaced POM, nested roles, invalid keys, duplicate child keys,
      and duplicate bindings.
- [x] Implement `#[view(component)]` for synchronous Rust functions.
- [x] Add compile-fail coverage for async functions and invalid signatures.
- [x] Keep component authoring free of VDOM/runtime scheduler concepts.

Exit gate: nested functional components can be compiled and tested without an
Agent. The compiler output will be split by lifecycle in P3.

### P2 - Provisional Agent Transaction Integration: Complete

- [x] Add typed prepared plans and late sink binding.
- [x] Preserve legacy `AgentViewModel` through the empty-binding mode.
- [x] Add component authoring without changing legacy type defaults.
- [x] Drop stale plans when context preparation replaces history.
- [x] Bind only after final `ContextPreparation::Ready`.
- [x] Rebind a fresh plan for every `TurnFlow::Continue` iteration.
- [x] Prove provider failure does not commit output or the POM cursor.
- [x] Add ephemeral typed `.with_props(...)` input.

Exit gate: the prototype proves transaction placement. P3 replaces its
per-attempt combined System/User render with the mounted lifecycle.

### P3 - Mounted System and Per-turn User: In Progress

- [x] Add nominal exact-one `DurableComponent<C, Props>` leaves and one ordered
      `DurableSystem<C, Props>` root containing interleaved POM-only nodes and
      runtime leaves.
- [x] Make `RuntimeBinder::durable_system` the durable owner's only System
      source. Create and POM-free reopen projections come from this same tree;
      the durable owner accepts no mount props or System callback.
- [x] Reject implicit durability erasure through ordinary `component`, POM
      `view`, and `durable_system(ordinary_runtime_component)` at compile time.
- [x] Change `#[view(component)]` from an immediately executed wrapper to a
      deferred typed component call with stable identity and props.
- [x] Require owned/`Arc` props for deferred child calls and add borrowed-prop
      compile-fail coverage.
- [x] Add strict POM-only `PomView` and the compatible binding-bearing
      `ProvidedView<B>` name; reject provided children in a POM-only component.
- [x] Prove pure/provided tuple composition, two local binding mappings,
      `Option`, `Vec`, nested component identity, keyed dynamic siblings,
      source order, and POM error propagation without changing AgentLoop.
- [x] Add `TurnChannels`, lane-preserving `ChannelMap<Local, Root>`, and
      `StreamUpdate`; prove one tag can emit live, output, commit, and a
      diagnostic. Single-lane mapping exists only on nominal
      `StreamingValueView<E, D>`; `StreamingChannelsView<C>` cannot recover
      those shortcuts through generic composition, and XML streaming is sealed
      to text events.
- [x] Add explicit child-props projection so reusable components do not require
      every sibling and root to share the same `Props` type. `.project_props`
      is pure, borrowed, and separate from channel mapping; external ordinary
      and durable tests prove projected binding/dispatcher contexts.
- [x] Add a parallel `MountPlan<C>` compiler that splits heterogeneous factory
      and provider-capability declarations after root-channel normalization,
      validates System placement/routes/tool names, and does not touch AgentLoop.
- [x] Require low-level `binding_factory[_with_context]` to carry its
      prompt-facing POM `Document` in the same view as the runtime declaration;
      hidden streaming routes can no longer be registered without prompt POM.
- [x] Add isolated nominal `SystemView<C, TurnProps>`/`UserView` roots and prove a
      non-capturing System function compiles, resolves, and renders one atomic
      `MountedEpoch<C>` without touching AgentLoop.
- [x] Add `SystemMountContext` with mount props only; captured closure state and
      turn-only context access fail to compile.
- [x] Add isolated per-attempt `UserTurnPlan` compilation that cannot contain
      runtime declarations and never traverses the mounted System tree.
- [x] Convert `StreamingXml` into an isolated reusable mounted factory path;
      derive its POM tag, component key, and XML route from one contract.
- [x] Instantiate all mounted XML factories for an attempt behind one shared
      parser, preserve cross-route wire order, and map every typed lane after
      local instance creation.
- [x] Separate `finish_stream`, successful local completion, and pure reducer
      abort so a publication failure can still abort after parser finish.
- [x] Parameterize mounted declarations with typed turn props; create opaque
      epoch, logical-turn, provider-attempt, and live-scope identities only at
      the host lifecycle boundary.
- [x] Add fallible, atomic factory initialization through `TurnBindingCx`; a
      failed factory aborts previously initialized local state before any parser
      callback or provider request can run.
- [x] Return `PreparedUserTurn` from User rendering and let only that handle
      start provider attempts, so User POM and factory initialization cannot use
      different values of the same turn-props type.
- [x] Define the executable typed XML binding callbacks and preserve terminal
      failure identity/phase through root channel mapping.
- [x] Define executable grouped `ProviderDispatcher` methods for dispatch,
      finish, and explicit abort; create each group from exact prepared-turn
      props and map every returned lane/diagnostic to root channels.
- [x] Run native tool calls under the same attempt identity and live scope as
      XML bindings; retain tool results for publication, replay exact invocation
      duplicates, and make invocation collisions or infrastructure failures terminal.
- [x] Add root `ChannelTypeInfo` and a private erased mounted-epoch adapter;
      keep `Any` and downcasts private to framework adapters.
- [x] Prove different `MountPlan<Root>` contracts install into one non-generic
      epoch storage shape while typed `MountedEpoch<Root, Props>` handles remain public.
- [x] Add a crate-private object-safe combined text/native attempt driver with
      non-generic Active/Finished/Published storage. Install the typed update
      interpreter before erasure; return a tool result only after typed update
      delivery; retain an abortable state in finish/publication failures.
- [x] Add a crate-private managed pre-publication attempt owner. Once a command
      reaches its mailbox, caller cancellation cannot drop active or finished
      state; terminal drive/finish failures and final-handle loss await abort.
- [ ] Install that driver in the real AgentLoop owner. Terminal drive failures
      must abort automatically, and cancellation of `finish`/publication
      futures must not silently drop an async-cleanup obligation.
- [x] Keep erased factories/capabilities in the epoch and fresh erased
      instances/dispatchers in a distinct provider-attempt runtime.
- [ ] Replace the temporary homogeneous `HookPlan<B>` consumption path only
      after reusable streaming factories and the mounted lifecycle are proven.
- [x] Converge ephemeral/one-shot runtime authoring on ordinary `Component`,
      while keeping durable runtime authoring nominal as `DurableComponent` so
      its reopen contract cannot be erased implicitly. Streaming XML and native
      tools share channel-mapping vocabulary in both paths.
- [x] Restore ergonomic fallible component authoring without adding a second
      carrier. `#[view(component)]` now installs an internal fallible render
      boundary, so dynamic POM names, routes, and tool specs can use `?` while
      the declared return type remains `Component` or `PomView`; focused tests
      prove both successful deferral and compile-time error propagation.
- [x] Remove provider ingress plumbing from the author-facing effect contract.
      `TurnChannels` now declares only Output, Live, Commit, and Diagnostic;
      `TextTurnEvent` remains an internal mounted text-wire input. Channel
      mapping no longer requires parent/child event equality, erased channel
      metadata no longer records an Event slot, and the obsolete event-mismatch
      compile-fail fixtures were removed.
- [ ] Replace the compatibility combined compiler with the proven System/User
      lifecycle roots while keeping ordinary child components role-agnostic.
- [x] Expose a public binding-owned local mounted facade around `DurableSystem`
      without promoting the private owner/store/actor types wholesale. It is
      process-local, `Commit = Never`, and not the production host.
- [x] Prove a reusable pure feature carrier that contributes durable System POM
      and runtime declarations together with its per-turn User fragment.
      `MountedFeature` composes/project props in author order, and public tests
      cover two calls plus drop/reopen without System/User rerender. Async
      capture deliberately remains a host boundary.
- [x] Make `MountedFeature` a true retained keyed/diagnostic
      `#[view(component)]` node. Its macro component name and optional key are
      retained in both System and User projections; keyed reopen and
      same-epoch key-change rejection are covered by public tests.
- [x] Add `MountedFeature::map_channels`; an external test composes a
      feature-local streaming Live contract into a different harness root
      without rebuilding the feature against that root.
- [x] Migrate `pom_feature_composition.rs` to the public `MountedFeature`
      composition path while keeping capture and provider/persistence host
      ownership explicit.
- [x] Define `UserTurnContext<Props>` as the sole typed application input for a
      User render. The framework does not inject or order context, artifacts,
      task, call id, or captured view; applications place the values they need
      in `Props` and author their POM order explicitly.
- [x] Keep `SystemView` compilation as an explicit one-shot compatibility path.
- [x] Compile the binding-owned `DurableSystem` once on Create and store raw POM,
      resolved POM, rendered bytes, factories, capabilities, and manifest as one
      durable epoch artifact; reopen filters declarations without POM access.
- [ ] Make that isolated bundle the authoritative System source in AgentLoop.
- [ ] Change per-attempt `TurnPlan` to `UserDocument + BindingPlan`.
- [x] Make the `Wait`/`Continue` bound a `TurnLoopPolicy` on the durable
      harness. `MountedCallInput::with_turn_cap` may only lower that policy;
      the epoch manifest and artifact fingerprint persist it, reopening the
      same durable epoch rejects policy drift before System rerender, ordinary
      epoch reconfiguration keeps it fixed, public lifecycle coverage proves
      both tightening and that a larger cap cannot expand it, and the mounted
      Forgotten City selector declares its legacy three-turn budget.
- [x] Instantiate fresh binding/reducer/dispatcher state only after final Ready
      in the crate-private mounted owner.
- [x] Keep System and its provider binding fixed across ordinary turns, history
      replacement, provider failure, cancellation, and `TurnFlow::Continue` in
      the crate-private mounted owner. Real AgentLoop migration remains open.
- [ ] Validate static output tags and factory identities at mount time.
- [ ] Remove per-turn `set_system_snapshot` and rename the observer event.
- [x] Define crate-private `forked()` to share the immutable epoch while cloning
      the session history and User cursor snapshot.
- [x] Add crate-private offline candidate validation and atomic System-epoch
      replacement under the turn lock.
- [x] Require explicit pure history rebase on reconfigure and reset the User
  and provider cursors only on successful atomic replacement.
- [x] Split immutable owner policy from replaceable `MountedEpochDefinition`;
      durable reconfigure cannot carry a different store, reducer, request-id,
      publication, or attempt policy.
- [x] Persist the rebased session before System render and atomically activate
      the artifact, one System snapshot, User/provider cursor reset, and
      revision.
- [x] Cover same-owner pinning, cross-owner fencing, attachment resume,
      Activated retry, `RenderStarted` no-rerender, and cancellation after
      activation followed by POM-free reopen.
- [x] Add store-owned exact-fence abort for abandoned `RenderStarted`, a durable
      tombstone, atomic epoch-A snapshot return, owner reload, and non-destructive
      `Rendered` attachment resume.
- [x] Add a crate-private managed reconfiguration operation that survives
      waiter and final-handle cancellation without cancelling durable work.
- [ ] Reconstruct B in process after post-activation snapshot/local-install
      failure, or make close-and-reopen the explicit public recovery contract.
- [ ] Keep the legacy authoring path behavior isolated during migration.

Required tests:

- [x] Normal, compaction-shaped, continuation-shaped, and cross-process reopen
      proofs render the durable System POM exactly once.
- [x] User View counts increase with isolated preparation attempts.
- [x] System mount rejects capturing closures and exposes no task/history/turn
      accessors; User roots reject runtime declarations at compile time.
- [ ] Repeat those lifecycle assertions through real AgentLoop preparation,
      history replacement, provider retry, and `TurnFlow::Continue`.
- [x] Every isolated executable provider attempt receives fresh streaming state after a
      previous finish, failure, or abort.
- [x] Repeated call labels and provider retries receive distinct internal
      turn/provider-attempt/live-scope identities in the isolated lifecycle.
- [x] Per-attempt grouped dispatchers preserve provider order, expected-error
      results, terminal infrastructure failures, awaited Live backpressure,
      publication-gated Commit, replay, and collision behavior in isolation.
- [ ] Repeat identity assertions across AgentLoop forks and retries.
- [x] Crate-private reconfiguration can expose only `{System A, Factories A}` or
      `{System B, Factories B}`, never a mixed pair.
- [x] Failed crate-private reconfiguration preserves the old epoch unchanged.
- [x] Isolated erasure preserves typed Output/Diagnostic delivery, awaited Live,
      publication-retained Commit, and terminal phase; a contract mismatch
      fails before reducer/dispatcher instantiation.
- [x] Two mounted harnesses with different root channel contracts share the
      same non-generic active/finished/published driver storage without passing
      lane payloads through `Any`.

Exit gate: normal execution has no code path that can render or replace System,
and every turn still gets fresh runtime bindings.

### P4 - Real-time Runtime and Effect Boundaries: In Progress

- [x] The prototype incrementally feeds `TextDelta` into `HermesParser` and
      invokes `open/stream/complete` reducers while the stream is active.
- [x] Preserve reducer callback order in the isolated reusable mounted path,
      including cross-route wire order and closing-chunk append-before-close.
- [x] Define typed output/live/commit emissions and independent non-terminal
      diagnostics; prove a callback can emit values and diagnostics together.
- [x] Add terminal reducer/parser failure as a separate control path carrying
      binding identity and callback phase.
- [x] Route both XML callbacks and native dispatcher updates through one
      isolated host-selected live-effect runtime; each call waits for its Live
      effects before returning its provider result.
- [x] Await live-effect interpretation inside the parser callback so
      reducer order and stream backpressure match `StreamingToolRunner`.
- [ ] Evolve the sink/executor boundary so live-runtime failure can terminate
      the current turn instead of being hidden until `finish()`.
- [x] Define a separate `MountedProviderExecutor` and
      `FallibleProviderWirePort`; expose an immutable epoch tool catalog and
      adapt the managed attempt actor without changing legacy `TurnSink`.
- [x] Add owner-issued provider cancellation source/token and a joined terminal
      execution outcome. The owner must await executor exit before attempt abort.
- [x] Add a crate-private mounted preparation/provider owner. It pins one
      `MountedEpoch` and `TurnInstanceId` across history replacement, rebuilds
      application props and rerenders only User, creates an attempt only after
      final Ready, then joins provider execution before returning a retained
      Finished state or acknowledged cancellation.
- [x] Cover cancellation while `finish()` is blocked in awaited Live work and
      completion-receiver loss after provider success. Both paths explicitly
      await attempt cleanup; neither relies on dropping Finished state.
- [x] Make cancellation notification lost-wakeup-safe and cover cancellation
      before first poll plus multiple concurrent token waiters.
- [ ] Install that cancellation/join contract in a production provider adapter
      and AgentLoop. The internal owner now covers hung preparation, provider
      grace timeout/poison, and wire-fault cleanup; production transport tests
      remain pending.
- [ ] Define explicit live `open`, `append`, `parsed-close`, `complete`,
      `cancel`, timeout, and compensation phases.
- [x] Add managed public local-call `cancel().await` semantics; raw task abort
      or `Drop` is not the async cleanup contract. Production id-keyed control
      and recovery remain open.
- [x] Add `ProviderCancellationCursorDisposition::{Unchanged, ResumeFrom,
      Indeterminate}` to the provider exit contract and retain it through
      joined managed cleanup. A normal completion or provider error racing with
      owner cancellation is conservatively downgraded to `Indeterminate`.
- [x] Cover provider reason mismatch: a result whose reason does not match the
      owner request is downgraded to `Indeterminate` and cannot authorize cursor
      reuse. Managed-owner tests also cover all three normal dispositions.
- [x] Add one exact-fenced store transition after Provider and Live cleanup:
      `Unchanged` or a validated `ResumeFrom` may terminally stop the exact
      Running call and update its cursor; `Indeterminate`, pending publication,
      cleanup/join failure, or any epoch/lease/revision/cursor conflict must
      atomically enter or preserve `RecoveryRequired`.
- [x] Complete actual process-local store fault parity and exact recovery retry.
      Seven direct `InMemoryMountedStore` tests cover revision, pending
      publication, epoch, stored/replacement-cursor, expired-lease and foreign
      fence cases plus same-fence recovery idempotency. The store stays hidden;
      the public suite covers reachable invalid/valid `ResumeFrom` outcomes, and
      Forgotten City covers cleanup failure as recovery-required.
- [x] In the legacy isolated mounted path, keep Commit values private through stream
      and finish, require an awaited `TurnPublisher`, and release them only in
      `PublishedStreamingAttempt` / `PublishedProviderAttempt` with a
      framework-issued receipt and provider results recorded for publication.
- [ ] Split the current AgentLoop `commit_turn`: private draft mutation cannot
      deliver external I/O before session publication.
- [x] Add a crate-private typed post-publication Commit interpreter. It runs
      only after publication, retains the complete batch on failure, and can
      retry without rerunning publication or provider work.
- [x] Define durable publication request/publication/outbox item identities,
      a one-to-one typed Commit stager, expected-revision CAS store contract,
      and explicit rejected/indeterminate/resolve semantics.
- [x] Add a host-owned `PublicationCandidateFingerprint` to every staged
      candidate, request, receipt, and resolve query. Same `(request id,
      fingerprint)` is idempotent; a same-id/different-fingerprint collision is
      a definite rejection and can never reuse a receipt or become `NotCommitted`.
- [x] Add an after-staging pure fingerprint factory so the host canonicalizes
      the actual outbox contracts and payloads without duplicating a
      `CommitStager`; framework code must still not serialize generic values.
- [x] Treat a definite fingerprint collision during resolution as terminal for
      that candidate: return it to Ready, report the collision, and let a
      disconnected owner abort with `PublishFailure` without retrying it as
      ordinary transport uncertainty.
- [x] Add a crate-private managed publication actor. Caller cancellation cannot
      cancel an accepted store command; final-handle loss resolves an
      indeterminate request and only aborts after authoritative non-commit.
- [ ] Replace `commit_turn` with a pure complete session mutation, implement a
      concrete durable store plus recovery-scanned outbox worker, and install
      the managed publication owner in the real AgentLoop.
- [ ] Define worker lease/backoff/dead-letter telemetry and receiver
      idempotency for partial external delivery.
- [ ] Keep reducer closures from capturing application services.
- [x] Distinguish reducer diagnostics from terminal parser/factory failure.
- [x] Strictly reject incomplete registered XML at EOF before any finish or
      Commit emission; a failed callback stops later tags in the same chunk.
- [x] Keep mounted reducer state available after parser finish and finish
      failure so publish failure can still execute pure local abort logic.
- [x] Await live-scope compensation before every local abort reducer, retain
      both sides in `StreamingAbortReport`, and still run local teardown when
      compensation fails.

The target production owner uses this ordering. The contract and in-memory
store tests now prove the shape, but the real AgentLoop still does not produce
the complete durable session mutation or use a production store:

```text
final Ready -> bind fresh runtime -> provider stream + real-time reducers
-> parser/reducer finish -> pure complete session mutation
-> typed Commit staging with stable item ids
-> CAS session + insert outbox in one transaction -> durable receipt
-> independent outbox worker -> external delivery
```

The old post-publication interpreter remains only as an attempt-local proof.
The durable path returns no pending Commit values. External delivery remains
at-least-once and must use stable `OutboxItemId` deduplication.

Exit gate: real-time behavior matches the existing streaming tool, and
commit-only external I/O cannot occur on provider failure, parser failure, or
turn abort.

### P5 - AgentView Vertical Reference: In Progress

- [x] Keep `examples/pom_feature_showcase.rs` as the complete offline POM
      projection/golden reference; its four example-target tests pass.
- [x] Add a self-contained isolated streaming example on the mounted lifecycle;
      AgentLoop migration remains gated on the fallible/live runtime.
- [x] Add a durable-publication example whose store atomically records a
      prepared session mutation and versioned outbox payload, and rejects a
      request-id collision with a different candidate fingerprint.
- [x] Demonstrate one System mount across multiple User turns, continuation,
      replay, reload, and drop/reopen through the public local facade; a
      production compaction trace remains pending.
- [x] Rewrite `examples/hello_world.rs` around the current mounted public path.
      Its prompt-only author-facing file contains one `hello_agent` component
      with typed System/User POM, `PromptComponent`, `prompt_component`, epoch
      assembly and two calls; recording
      provider/store/reducer scaffolding are isolated in the shared
      `examples/support/mounted_prompt_trace.rs` example host. Direct-props
      capture is also owned by that host rather than the application example.
      The runnable trace prints exactly one System and two User prompts. A
      public test also locks the fallible User boundary: authoring failure is a
      preparation error and cannot start provider I/O. An independent
      full-streaming rewrite compiled but was rejected as the baseline because
      this trace host does not emit tokens; its unused channels and reducer
      obscured rather than proved the author/host boundary.
- [x] Add `examples/chess_engine_mounted.rs` as a behavior-preserving prompt
      migration beside the external-control chess example. An independent
      consumer rewrite keeps its runnable path focused on the component and
      two mounted turns; the component uses `try_prompt_component` to adapt its
      existing fallible User-document builder without exposing empty channels
      or `UserTurnContext`. A `#[cfg(test)]` compatibility module compares exact
      System/User bytes to the legacy `ChessViewModel`. Keep the original
      `observe/act/hook` runtime until `AV-F09` has a mounted external-reply
      boundary; do not hide that gap by deleting the Stockfish or action flow.
- [x] Add `examples/chess_agent_mounted_turn.rs` as the author-only step after
      `hello_world`: one narrow prelude, one durable System/User feature, and
      one typed streaming reducer, with no provider, persistence, or host
      control vocabulary in the author component. The shared example host now
      feeds one XML token stream in two chunks through `FallibleProviderWirePort`,
      records `LiveApplied` before the second chunk's wire acknowledgement, and
      asserts the matching typed Output in `MountedCallOutcome::Executed.records`.
      This completes the public local streaming example, not the missing
      external chess controller or production provider migration.
- [x] Have an independent consumer attempt the complete chess rewrite and stop
      at the first semantic API gap. The review rejected a provider-channel
      workaround: mounted currently has no external User observation/action
      ticket/reply ingress, while a pure `SessionReducer` cannot atomically
      mutate `ChessGameSource`. Its required state/test matrix is now the
      acceptance contract for `AV-F09`.
- [x] Define the host-owned external transaction port before implementing a
      controller. `component::advanced::external` now contains opaque action,
      reply, wake, source-revision, epoch and commit identities plus
      `MountedExternalPort<Action>`; its CAS operation requires the real host
      to atomically revalidate/mutate domain state, persist AgentView state and
      reply ledger, insert outbox rows, and advance wake state.
- [x] Add `component::advanced::external::MountedExternalController` with opaque
      serializable `ExternalActionToken`, idempotent `ExternalReplyId`, opaque
      `ExternalWakeCursor`, and `observe/act/hook/cancel/lookup/recover`. Persist
      ticket identity across durable session, epoch generation, call/input,
      turn, action route, source revision, User fingerprint and reply
      fingerprint; ambiguous apply/commit remains fenced in
  `RecoveryRequired`. `tests/component_external.rs` is a 9/9 black-box
      local proof for System-once/reopen, typed reply validation, atomic fake
      domain/state/outbox mutation, replay/collision, hook, source staleness,
      `NotCommitted` retry, `Unknown` fencing, decoder contract drift, a
      cross-owner cancel/commit race, and a non-advancing CAS generation that
      fails closed rather than exposing a phantom ticket/action. It also proves
      that source revision and props are captured together. Schema v5 now
      persists `AwaitingDelivery`, the acknowledged User cursor, and a
      full-resync fence; it rejects schema v4 rather than inventing historical
      User receipts. The ninth test proves User-only full resync after a consumer
      loses an undelivered delta, with System still delivered once and delta
      resuming only after the replacement acknowledgement. This AgentView
      black-box port remains fake; Forgotten City's SQLite facade is the real
      transactional consumer reference. A managed remote/server transport,
      retention policy, supervisor and process-kill matrix remain pending.
- [x] Move System delivery behind the external host port. `complete_epoch` now
      receives the sole rendered System bytes and returns a durable
      `ExternalSystemDeliveryReceipt` only after the host has retained a remote
      acknowledgement or ordered outbox identity. `MountedExternalOpen` exposes
      the receipt but never raw System text, and `Existing` carries the same
      receipt; a port with pending/ambiguous delivery must return `InFlight` or
      `RecoveryRequired`, never `Existing`. The local test port records the
      one System delivery internally and reopens with the same receipt. This is
      an API/black-box contract, not a real database or transport proof.
- [x] Bind prompt-facing external reply grammar and typed decoder in one pure
      `ExternalReply<Contract>` provided component. It appends its grammar to
      the retained System POM and consumes into `MountedExternalHarnessDefinition`;
      `MountedExternalController::open` no longer accepts a separately chosen
      decoder or separately chosen grammar. `ExternalReplyContract` owns its
      `type System` and `system()` contribution, so the binding site is only
      `ExternalReply::new(contract)`. `ExternalReply` and its synchronous pure
      contract are available to component authors, while observation,
      controller, port, domain mutation, and outbox remain advanced host
      concerns. The public compile-fail boundary locks that split. The mounted
      chess proof now decodes only the exact CLI form shown in its System
      grammar and rejects JSON, bare UCI, omitted required context, and context
      that disagrees with UCI. This is an executable semantic coupling test for
      that component; POM text and a handwritten parser are not generically
      machine-proven equivalent.
- [ ] Close the external-control API gates before treating
      `MountedExternalController` as a migration surface:
      - [x] one nominal captured-observation value carries the exact immutable
        props together with its source revision, rather than letting hosts pair
        parallel arguments accidentally;
      - [ ] production ports must implement the durable System-delivery
        receipt/outbox contract against their actual transport. The local
        receipt proof prevents raw-System leakage, but is not evidence of one
        physical network transmission;
      - [ ] terminal action replay data needs an explicit retention and compaction
        contract. An unbounded terminal vector is not a long-lived session
        design.
      These are pre-freeze migration gates, not permission to expose host
      transaction internals to ordinary component authors.
- [x] Add `examples/chess_engine_mounted_external.rs` as the local complete
      mounted chess interaction trace: System-once open, User observe, typed
      CLI reply bound to its System grammar, host-owned player commit, visible
      passive waiting presentation, asynchronous Stockfish wake, and fresh
      delta User hook. A missing engine produces a passive recovery view before
      the error is reported, with no invalid black-to-move action token.
      Its in-memory host intentionally demonstrates the transaction shape only;
      it does not claim a durable database, reliable transport, interactive CLI
      input, or a multi-move provider loop.
- [x] Add a non-actionable external User publication contract.
      `ExternalUserView` compiles pure props into either Actionable Prompt or
      Passive Presentation; only the former creates an `ExternalActionToken`.
      Passive frames are durable, replayable, and supersedable without cancel.
- [x] Move external User delivery behind the host port. Publish CAS mutations
      carry an immutable `ExternalUserDeliveryCandidate` ordered after the
      retained System receipt. Public frames expose only the resulting User
      delivery receipt; exact bytes remain in the host outbox.
- [x] Add transport-acknowledged User cursor ownership. Actionable delivery
      starts in `AwaitingDelivery`, `act` fails before host acknowledgement,
      and ack atomically promotes its candidate cursor. Black-box and Chess
      tests prove first full, passive full/non-advancing, then delta with the
      acknowledged base receipt; unacknowledged cancellation requires host
      tombstoning and forces the successor full. This is a single-consumer
      local proof, not a production transport implementation.
- [x] Bind that chess trace to Forgotten City's SQLite transactional host and
      cross-process CLI protocol. The facade durably owns one logical consumer,
      System attach/ack, immutable User delivery and explicit ack, exact action
      handle/raw XML reply, reply replay/collision, hook wake, acknowledged
      delta cursor, User-only resync, create-lease takeover, and transport
      conflict fencing. Focused tests cover same-consumer reopen and reject a
      replacement consumer from inheriting the lineage.
- [ ] Bind the same protocol to a production remote transport and recovery-
      scanned supervisor. Add process-kill tests around System/User outbox
      publication, bounded receipt/reply retention, wake tokens bound to
      session/delivery, and explicit replacement-consumer or cross-transport
      handoff. CLI stdout plus SQLite is the executable reference protocol, not
      proof of a managed server delivery path.
- [x] Have an independent component author implement a mounted application
      from the public API. `examples/mounted_author_review.rs` runs one System
      attachment and two fresh User turns, but independently converges on the
      same conceptual floor as prompt-only `hello_world`. That feedback is now
      reflected by the public `PromptComponent<Props>` alias and
      `prompt_component` builder; epoch assembly remains explicit at the host
      boundary so the durable identity is not hidden.
- [x] Split the ordinary component-author imports from host-integration
      imports, and provide a public no-output `TurnChannels` contract so a
      prompt-only component does not invent four `Never` lanes.
      `component::prelude` builds both minimal examples; an external positive
      test and compile-fail host-leak fixture lock the boundary. Re-run the
      independent rewrite once the production mount facade is usable, then
      remove the redundant review example if it contributes no distinct proof.
- [x] Remove dispatcher-instantiating `provider_tool*` builders and
      `ProviderDispatcher` from `component::prelude`; they now live under the
      explicit `component::advanced::provider` compatibility boundary, and an
      external compile-fail fixture locks that absence. This closes the author
      prelude leak only; it does not yet provide the final pure tool contract.
- [x] Add a pure provider-tool component contract containing prompt POM,
      ordered schemas, stable declaration identity, and author contract
      version. Its dispatcher binds through a host-owned registry keyed by the
      declaration id with a separately versioned host implementation.
      `component_public_authoring` proves missing/duplicate/version/schema
      failures occur before System attach, successful reopen reuses the
      retained System, and each final-ready attempt creates a fresh dispatcher.
      Its public provider-wire round trip also proves a provider-native Tool
      event reaches that host dispatcher exactly once and receives the matching
      model-visible result/correlation identity before the attempt completes.
- [x] Show a fresh real-time reducer instance per final-ready turn through the
      mounted streaming and public local-host tests.
- [ ] Remove separate contract cloning, parser registration, and
      `.with_tool(...)` wiring from the migrated path.
- [ ] Document migration from legacy `StreamingToolRunner`.

Exit gate: a new user can learn mount, turn, streaming, diagnostics, and effects
from one runnable example.

### P6 - Forgotten City SelectIntent: In Progress

- [x] Extract `SelectIntentTool` as a mounted `DurableComponent` while
      preserving XML open-event ordering in `mounted_select_intent.rs`; it is
      not yet the production `PlayerRuntime` owner.
- [x] Keep its policy/output contract in the retained once-mounted System
      component definition.
- [x] Move the per-attempt intent catalog snapshot, batch, feedback, and Agent
      Context into typed call props/User POM only.
- [x] Replace direct service/channel capture inside the reducer with typed
      effects and an explicit host Live runtime.
- [x] Prove immediate ordered selection delivery, reverse compensation, and
      ownership-fence revocation for provider/parser/turn abort in the extracted
      runtime adapter.
- [x] Add an opt-in `PlayerRuntime` mounted-selector branch that starts typed
      User calls and replaces selector-task abort with call-scoped mounted
      cancellation plus joined cleanup.
- [x] Add a `PlayerRuntime`-level integration test for the opt-in mounted
      selector path: it starts one turn through the runtime queue, observes a
      streamed Live delivery, clears the turn through call-scoped cancellation,
      waits for provider/Live cleanup, then drains the queued revoked delivery
      without reviving a cleared selection. A follow-up event test proves a
      different successor call is rejected before capture/User/provider work
      because that fake provider reports an `Indeterminate` cancellation, and
      the failed mounted admission clears its transient frontend selection.
      AgentView local-owner tests separately prove that authoritative
      `Unchanged`/`ResumeFrom` cancellation stops the call and admits a
      successor; the real Forgotten City transport has not supplied this proof.
- [ ] Install that branch from a durable mounted provider/persistence host; the
      production runtime must no longer fall back to `ComponentTextAgent` for
      selector turns.
- [x] Derive `DurableCallId` and `DurableCallInputId` from the persisted
      interaction UUID plus domain-issued player-turn sequence rather than the
      process-local `PlayerRuntime` generation. Runtime tests cover stale,
      duplicate, and successor events; the mounted owner remains responsible
      for deterministic replay/rejection after restart.
- [x] Exercise the installed opt-in seam through the real engine event path:
      `WorldApi.event_tx -> GameEngine::drain_events -> EventDispatcher ->
      PlayerEventRouter -> PlayerRuntime::tick -> open_for_session`. The test
      stops at an unavailable host, so it proves routing and durable identity,
      not production provider attachment.
- [x] Add a stateful provider session/cursor scaffold and fake remote proof for
      one System install, lost-reply `ResumeAttachment`, cursor-only rehydrate,
      User-only turns, and joined cancellation; ordinary attachment is now a
      separate optional provider trait.
- [ ] Bind a real remote-session factory and repeat the fake adapter's transport
      proofs; the real integration must show System bytes only at epoch Create.
- [x] Implement and locally prove exact-lease settle-or-recover for
      provider-started cancellation, including authoritative successor/cursor
      continuity and indeterminate successor fencing.
- [ ] Decide and lock the invalid-selector migration policy. The mounted pure
      reducer rejects a verb/topic/knowledge/recipient that is outside the
      captured intent template, while the legacy XML callback only checks that
      the handle exists before it queues work for `PlayerRuntime`. The current
      golden trace covers inputs both paths accept; production cutover needs
      either legacy-compatible normalization or an explicit product-approved
      validation tightening with accepted and rejected trace coverage.
- [ ] Exercise that settlement through the real Forgotten City provider/store;
      bind existing durable call-id lookup and add reattach/control through the
      production host.
- [ ] Compare prompt, parser, event, and world traces before removing legacy
      wiring.

Exit gate: production SelectIntent has one contract source, fresh per-attempt
reducer state, no application I/O hidden inside reducer closures, no residual
phraser/option after a failed selector attempt, a recording-adapter contract
trace, and a real stateful-provider trace proving physical System-once
transport.

### P7 - Forgotten City PhraseTool and Live Effects: Pending

- [ ] Migrate incremental visible output through the live-effect runtime.
- [ ] Preserve token/event timing relative to the existing streaming tool.
- [ ] Treat XML close as tentative `ParsedClose`; mark an option ready only
      after provider success and session publication.
- [ ] Test provider error, parser error, cancellation, timeout, and host failure.
- [ ] Replace implicit `Drop` cancellation with explicit compensation.

Exit gate: no live output remains active after abort, and cleanup is observable
and testable.

### P8 - Cube Stage Structured-tool Harness: In Progress

- [x] Restore Cube Stage compilation by migrating obsolete raw prompt APIs to
      POM authoring; do not restore removed APIs as a shortcut. The Director
      now builds typed System/User POM and `AgentViewValue`, and `cargo check
      --all-targets` plus the 80-pass library suite are green.
- [x] Keep legacy `PromptRenderable`/`ContextView` only as compatibility test
      support; the active Director request path obtains its prompt bytes from
      `build_system_document` and `build_user_document`.
- [x] Model provider-native tool declarations and tool-call/result routing as a
      provided component capability separate from `StreamingXml`; the isolated
      runtime supports grouped dispatchers, typed result/error policy, and
      in-memory replay/collision handling.
- [ ] Connect the mounted `ProviderCapabilityPlan` and its bound per-attempt
      dispatcher to the real executor/AgentLoop boundary.
- [ ] Mount Director policy and tool contracts once; render the board/task in
      User POM on each loop iteration.
- [ ] Migrate Cube Stage ToolServer calls onto the isolated runtime's awaited
      Live path, immediate typed results, and publication-recorded transcript.
- [ ] Define durable ToolServer/event-store idempotency, replay, and outbox
      policy; current invocation-id replay is in-memory and attempt-local only.
- [ ] Compare prompts, tool transcripts, observer events, and SQL-visible
      behavior against the legacy harness.

Exit gate: Cube Stage proves that Component governs a complex real-time harness,
not only XML parsing.

### P9 - Full Migration and Cleanup: Pending

- [ ] Add a component-plan adapter for the external `AgentViewApp` path if it
      needs declarative reply bindings.
- [ ] Migrate NPC, GM, player, graph, and remaining harnesses.
- [ ] Keep compatibility adapters until production callers have moved.
- [ ] Remove side-effecting `TurnComponent` and legacy streaming adapters.
- [ ] Evaluate persistent component state only after multiple production
      migrations establish a need.

Exit gate: every prompt is authored with POM Components and legacy split
contract paths can be removed without behavior changes.

## Current Release Slice

P0-P2 prove the basic POM component and transaction path. The isolated P3/P4
runtime now includes the single-source durable provided-component leaf,
compiler-derived manifest/rebind projection, mounted epoch storage, an
object-safe attempt driver, and a crate-private authoritative owner that backs
the opaque public local `MountedAgent`/`MountedCall` facade. That local facade
proves Create/reopen, owned call admission, replay, reload, awaited Live,
joined cancellation, and same-epoch continuation, but it remains process-local,
`Commit = Never`, and deliberately rejects reconfiguration.

The public `MountedFeature` carrier now also proves reusable feature-level
composition across the durable System/runtime tree and fresh per-turn User POM,
including POM-free drop/reopen. Its macro-provided name and optional key remain
in both projections, with Create/reopen identity equality and same-epoch
key-change rejection covered by tests. The public API itself remains unfrozen
for the separate production host and consumer-migration gates.

The next implementation slice is a production host rather than another public
surface expansion. Typed cancellation cursor authority, reason validation and
the exact-lease settlement/recovery boundary are implemented; normal
`Unchanged`, `ResumeFrom` and `Indeterminate` paths have focused owner tests.
Next are public id-keyed recovery/control, a durable persistence/outbox adapter,
a real stateful provider session
behind the proven fake/scaffold contract, and end-to-end migration of the
Forgotten City player intent selector. Production effect interpreters and
application migrations must not stay on the compatibility combined per-turn
System/User render. API freeze remains blocked until those consumer migrations
and independent review gates are complete.

The smallest implementation order is:

1. [x] Spike strict `PomView + ProvidedView<B>` composition outside AgentLoop.
2. [x] Prove single-lane and complete multi-lane channel mapping for existing
       streaming bindings, including closing-chunk delivery.
3. [x] Normalize heterogeneous factories/provider capabilities to root channels
       in an isolated parallel `MountPlan<C>` compiler.
4. [x] Spike deferred owned/`Arc` props and borrowed-prop compile failures.
5. [x] Spike separate mount/turn roots and contexts outside AgentLoop.
6. [x] Compile, resolve, render, and store one isolated System epoch bundle.
7. [x] Convert streaming declarations into reusable factories and prove fresh
       state plus one shared parser per provider attempt.
8. [x] Spike the executable typed XML binding contract with typed props,
       fallible initialization/callbacks, strict EOF, and publication-pending
       state.
9. [x] Add awaited Live interpretation, terminal host failure, compensation,
       private pending Commit, and an awaited publication typestate outside
       AgentLoop.
10. [x] Define executable grouped provider dispatch with typed results/error
        policy, replay/collision handling, awaited Live, and publication-gated
        Commit outside AgentLoop.
11. [x] Prove one-time mounted-epoch storage erasure with `ChannelTypeInfo`,
        private adapters, typed handles, and two distinct root contracts.
12. [x] Prove a crate-private object-safe combined text/native driver with
        typed update interpretation, contract preflight, explicit typestate,
        retained failure ownership, and two distinct root contracts.
12a. [x] Add a crate-private managed actor for pre-publication Active/Finished
         attempts. A cancelled caller cannot silently drop the live scope;
         losing the final owner triggers acknowledged abort.
12b. [x] Add a crate-private typed Commit delivery shim over Published state.
         Delivery is publication-gated; failure retains the complete batch and
         interpreter for attempt-local retry without replaying provider work.
12c. [x] Add typed Commit staging and the durable publication contract:
         preallocated request id, stable item ids, schema version, expected
         session revision, host-owned candidate fingerprint, atomic store
         request, definite collision rejection, and indeterminate resolution.
12d. [x] Add the independent fallible provider wire/catalog contract and a
         crate-private managed publication owner that survives caller
         cancellation through publish/resolve.
12e. [x] Add a pure host fingerprint factory that runs after typed Commit
         staging, sees the exact mutation/output/results/outbox candidate, and
         returns the finished attempt plus unchanged staging plan on failure.
12f. [x] Install a monomorphized durable-staging adapter before root-channel
         erasure and add the cancellation-safe `BeginDurablePublication`
         actor-to-actor handoff. The old attempt becomes `Transferred` only
         after the publication actor starts successfully.
13a. [x] Add the crate-private mounted preparation/provider owner. One logical
         preparation pins its epoch and turn identity, rerenders only User after
         history replacement, instantiates state only for final Ready, joins
         provider cancellation, and retains the exact request, draft session,
         next cursor, executor commit, and publication-pending Finished owner.
13b1. [x] Add the crate-private authoritative in-memory `MountedAgent`. It owns
          the epoch/session snapshot, validates the single System slot, exposes
          borrowed logical-call props, captures owned turn props before every
          pure User render, mounts only final-ready state, and owns fork plus
          atomic reconfigure. A keeper-thread runtime lease covers detached
          actors on current-thread or multi-thread runtimes. Caller cancellation
          during complete/cancel/Finished abort cannot release the turn lock
          before cleanup acknowledgement, including a reply already queued when
          its receiver disappears. Failed Live compensation or dispatcher abort
          poisons the owner even when local teardown returned an abort report.
          Context preparation has a drop-safe
          deadline contract; a provider missing its cancellation grace returns
          `ProviderJoinTimedOut` and permanently poisons calls/reconfigure.
          Reconfigure is intentionally System-epoch plus history rebase only;
          replacing capture/User host behavior requires mounting a new agent.
13b2. [x] Convert a successful completion into one pure complete session
          mutation, transfer its Finished state to the managed durable
          publication actor, and write the accepted session/cursor back to the
          authoritative owner. `TurnFlow::Continue` must then recapture and
          rerender User while retaining the same pinned epoch. This path must
          use `MountedProviderExecutor`, not legacy `LLMExecutor + TurnSink`.
          The mounted owner now also owns the durable revision: expected CAS is
          captured with the pinned session, and only a validated receipt may
          atomically replace both. Publication cancellation, indeterminate
          resolution, authoritative NotCommitted, rejection/collision, and
          non-arithmetic revision continuation are covered by owner tests.
13b3. [ ] Define the public mounted harness boundary. Session reducer and
          persistence adapter are installed once at mount; typed Output and
          Diagnostic accumulation have an explicit reducer contract; a call
          builder hides pinned calls, Finished state, request revisions,
          publication plans, and actor handles from component users. The
          persistence binding must own one durable session identity, store,
          revision/conflict policy, and stable epoch-contract identity. A CAS
          conflict moves the owner to `ReloadRequired`; System reconfigure must
          have a durable receipt/reload story rather than only changing memory.
          The builder owns the complete Wait/Continue loop and defines caller
          cancellation after a committed Continue as an explicit durable
          continuation or an explicit stop; it cannot silently lose the flow.
          The stable prelude must expose this facade and component authoring,
          not compatibility compilers, raw epoch attachment, leases, attempt
          typestates, dispatch factories, or publication actors. Isolated
          lifecycle proofs may remain under an explicitly experimental module.
13b3a. [x] Split revision CAS conflict from ordinary durable rejection. A
           conflict now moves the crate-private owner to `ReloadRequired`,
           blocks call and reconfigure, and can only be cleared by atomically
           replacing session + revision. Mounted tests cover conflict cleanup,
           blocked reuse, reload, handoff cancellation, and cancellation after
           receipt but before in-memory writeback.
13b3b. [x] Introduce the crate-private mount-owned persistence/reducer proof. It
           owns one durable session identity, epoch-contract identity, store,
           Commit stager, fingerprint policy, request-id allocator,
           live-runtime factory, and pure session reducer; no Continue
           iteration can replace any of them. Open and reload retain one exact
           store instance; the binding supplies only immutable policy and an
           initial session seed. The store atomically initializes that seed and
           reads session plus active-epoch identity in one snapshot.
13b3c. [ ] Carry typed Output/Diagnostic observations linearly from the
           monomorphized attempt into the pure reducer, then implement the
           public call builder that owns the full Wait/Continue loop and hides
           every internal typestate/actor/revision/publication plan. The typed
           `TurnRecord<C>` and private full-loop proof are complete; the public
           builder remains blocked on durable continuation ownership.
13b3d. [ ] Persist stable harness-contract identity and implement durable
           System reconfigure/reload plus explicit fork/session-identity rules.
           Stable session/call/epoch identities now travel through private
           snapshots and mounted mutations. The reconfigure transaction,
           exact-fence `RenderStarted` abort, tombstone, epoch-A owner reload,
           and managed reconfiguration ownership are proven; durable
           child-session creation is still missing.
           Freeze these public APIs only after independent consumer,
           type-system, and lifecycle/persistence review.
13b3e. [x] Add the private stable-boundary logical-call checkpoint. Each bound
           mutation now persists `DurableCallId`, `DurableCallInputId`, epoch
           contract, next turn index, last request id, and
           AwaitingContinuation/Settled status. Open/reload restores it; the
           same call resumes at the saved index, a different input or pending
           call is rejected before User render/provider I/O, and settled retry
           cannot rerun the provider. A restart-stable AwaitingContinuation
           checkpoint is covered by a new owner resuming turn 1.
13b3f. [x] Add private store-backed call admission before making resume public.
           One mount-owned `MountedSessionStore` now performs both atomic claim
           and lease-fenced publication. Every turn preallocates its request id
           and atomically claims `(session, epoch, call, input, turn, request)`
           as a leased reservation before User capture/render. Final Ready
           atomically promotes the same fence to Running immediately before
           provider/tool/Live I/O; pre-provider failure or cancellation restores
           the prior continuation checkpoint (or removes a new call) without
           model recovery. Publication validates the same unexpired Running
           lease while replacing
           it with AwaitingContinuation/Settled plus session/outbox. Two owners
           cannot enter one provider turn. Expiry of a Running lease at claim
           or publish durably becomes RecoveryRequired and never auto-replays;
           exact cancellation settlement now uses the same conservative
           recovery boundary for ambiguous cleanup/cursor/fence state. The retained ledger
           validates uniqueness/single-active invariants and preserves settled
          calls after later calls. Reserved-expiry tests prove that a new call
          is deleted and a continuation restores `AwaitingContinuation`, while
          only a Running lease may become RecoveryRequired during claim expiry.
          This remains a
          crate-private proof, not a public persistence API or concrete
          production store.
13b3g. [ ] Finish durable terminal/recovery records and the owned production
           loop. A
           private settled duplicate now loads its durable terminal receipt and
           returns without provider work. The private owner now uses an
           `Executed { .. } | Replayed { .. }` outcome instead of
           `replayed: bool`; the opaque local facade exposes that sum type.
           Provider-complete candidate identity is now persisted before
           first publish, and open/reload atomically reconciles Published,
           NotCommitted, InFlight, and collision outcomes without model replay.
           The full unpublished mutation/outbox payload is not persisted, so an
           authoritative NotCommitted candidate becomes RecoveryRequired rather
           than being republished after restart. A private `start_owned` actor
           now holds `Arc` props/source, returns only after durable admission,
           returns a cancelled pending `wait()` receiver to its still-live
           handle, and supports call-scoped cancellation with joined cleanup.
           External local-facade proof is complete. Exact-lease cancellation
           settlement now writes `Stopped` only for authoritative cursor
           continuity and otherwise writes `RecoveryRequired`; normal variants
           have focused owner tests. The public facade now has read-only
           id-keyed lookup plus owner-instance-local, single-handle reattach;
           the production API still needs cross-owner/process reattach/control,
           a concrete production-store parity suite, and lease renewal owned by
           that loop (or a provider deadline proven shorter
           than the durable lease lifetime).
13b3h. [x] Separate mounted provider epoch attachment from ordinary turns.
           `MountedProviderEpochAttacher::attach_epoch(MountedProviderEpoch)`
           receives rendered System bytes and the epoch's tool catalog exactly
           once for a successful ordinary mounted epoch; it returns an opaque
           share-safe epoch binding retained with that epoch. Durable adapters
           implement only `MountedProviderExecutor +
           DurableMountedProviderExecutor`, so they cannot accidentally fall
           back to this weaker attachment contract. The ordinary
           `MountedProviderRequest` and the mounted session-reducer request
           contain only call/history/User/model/budget data, never System or
           tool schemas. Context replacement and `TurnFlow::Continue` reuse
           the same binding; a successful explicit reconfigure installs one
           new attachment after its rebase, while a rejected rebase attaches
           nothing. Tests cover context replacement, `TurnFlow::Continue`, and
           both successful and rejected reconfiguration. This closes the wire-model
           ambiguity; the opaque local facade now drives the owner, while
           production persistence/provider integration remains an advanced
           boundary. A crate-private durable
           owner now opens an `Existing` artifact through POM-free runtime
           rebind and provider receipt rehydration, with a test proving no
           second System render or System-bearing attachment. Durable declaration
           identity is still duplicated between System authoring and the POM-free
           registry; 13b3j owns that remaining authoring/compiler gap.
13b3i. [x] Integrate durable epoch rehydration into the real mounted owner.
           Stable declaration ids plus implementation versions now project the
           manifest from both the preflight POM-free registry and the compiled
           System component. `Existing` rebuilds only that registry and
           receipt-rehydrates the provider; owner tests prove create/drop/reopen
           with one System render, one normal attachment, and no second
           System-bearing attachment. The mount-owned store now initializes the
           durable session seed before epoch admission and performs a strict
           active-artifact/session snapshot before owner construction. Owner tests
           cover a different reopen seed plus missing/changed artifact faults.
           `ProviderTurnCursor` is now persisted with an accepted mutation and
           accepted by direct coordinator rehydration. Owner tests cover bounded
           concurrent cursor catch-up, post-snapshot claim fencing, reload-time
           provider rehydration, atomic active-artifact/cursor publication,
           corrupt snapshot rejection, and reconfiguration reset. Production
           store and indeterminate-attachment recovery policy remain integration
           work, not a reason to reopen this private proof.
13b3j. [x] Replace aggregate `Component::runtime_contract` and the duplicated
           owner registry with one nominal durable provided-component leaf. The
           first slice is implemented: `DurableComponent<C, Props>` owns exactly
           one contract id/version, one linear System POM projection, and one
           factory or capability-group declaration; the crate-private durable
           catalog derives both first-mount compilation and POM-free rebind from
           that same leaf. The manifest retains capability-group identity and
           enforces global declaration-id uniqueness across binding/capability
           kinds. Structural keys now derive from the runtime contract by
           default, with an explicit placement-only override. The retained
           source now drives the public local facade, and external-consumer
           reopen coverage proves POM-free rebind. Raw compatibility IR is now
           behind the non-default `raw-component-ir` feature with a default
           external compile-fail proof, so it is not an alternative durable
           source for ordinary authors.
13b3k. [x] Add private abandoned-reconfiguration recovery. `MountedOwnerStore`
           atomically validates session, reconfiguration id, complete base
           artifact, exact `EpochOpenFence`, phase, and staged revision. Only an
           expired `RenderStarted` may be aborted; the store advances revision,
           tombstones the id, and returns epoch A's authoritative snapshot.
           The owner installs it under configuration/turn locks and remains
           poisoned if that local handoff is interrupted. `Rendered` remains
           POM-free attachment-resume state. Tests cover predicate mismatch,
           concurrent recovery, stale writer rejection after clock rollback,
           same-id retirement, owner call after reload, and new-id replacement.
13b3l. [x] Move durable reconfiguration into a crate-private mount-owned actor.
           It owns the definition/rebase input and runtime lease after
           process-local spawn, while the start method acknowledges a new or
           resumed handle only after the first durable admission transaction.
           Preflight failure and durable `Rejected` return before a handle, and
           an already-activated retry returns a terminal start disposition. A
           dropped start observer does not cancel the detached actor; an
           in-progress `wait()` observer and the final accepted handle may both
           be dropped while provider attachment is gated, then
           the actor still reaches one terminal result and activates B without
           a second System render. Its `cancel()` is deliberately
           non-destructive: after actor ownership starts it returns
           `NotCancellable`, so a `Rendered` artifact cannot be discarded or
           authorize another System render. A cancelled pending `wait()` now
           returns its receiver to a still-live handle, while final-handle drop
           remains non-cancelling. Independent public-freeze review accepts
           this private proof but rejects publishing its one-shot handle. Public
           design must use id-keyed durable status/replay, expose a stable reopen
           outcome, and omit the no-op cancellation vocabulary.
13b3m. [ ] Finish the durable id-keyed reconfiguration status contract before
           exposing submission publicly. The private store now returns the current
           active epoch and the requested operation record atomically; the
           mounted projection distinguishes `NotDurablyAdmitted`, `InFlight`,
           `RecoveryRequired`, `Activated`, `Superseded`, `Rejected`, and
           `Aborted`, plus
           local `Current`, `TransitionInProgress`, or `ReopenRequired` owner
           disposition. Completed ids survive later reconfigurations, so an
           A -> B -> C history still reports B as superseded and rejects reuse
           of B's id without another System render. A failed history rebase now
           persists an unambiguous `Rejected` tombstone before System render;
           matching retries stay rejected and a detached actor records it after
           observer drop. Reopen retains the rejection without rerendering
           System, and direct store tests cover wrong-session writes,
           idempotence, changed base/manifest, and collisions with every other
           operation owner. New writes atomically require the expected base to
           remain active and no concurrent reconfiguration to own the session
           lane. Remaining work is to expose query/watch through the
           future facade, define the pre-first-transaction/process-loss result,
           and implement the retention policy in a production adapter. Terminal
           payloads may be compacted after policy expiry, but a minimal
           session-scoped id tombstone must remain until atomic session deletion
           so garbage collection can never authorize id reuse or a second
           System render. In-flight, recovery-required, and current-active
           records are never eligible for terminal compaction.
14. [ ] Replace side-effecting `commit_turn` with a pure complete session
        mutation and install a concrete CAS session/outbox store.
15. [ ] Add managed cancellation plus one-shot System, retry, loop, failure,
        ambiguous publication resolution, and atomic reconfigure tests through
        the real owner.
16. [ ] Install the recovery-scanned outbox/Stockfish supervisor. The storage
        primitives are complete locally: stable item-id dedup, leases, fencing,
        expiry reclaim, backoff, dead-letter state, poison quarantine, and
        capture admission all have tests. Remaining work is continuous scan,
        wake integration, passive failure publication, telemetry, shutdown and
        real process-restart recovery.

The completed steps 8-13b3h are still narrower than the final runtime contract.
`.try_state_with` receives typed props plus opaque identity and returns fresh
owned state; `HermesParser` now has a parallel fallible callback path and strict
EOF. `MountedStreamingAttempt` now requires a root-selected `LiveEffectRuntime`,
awaits each Live value before the next callback, and compensates the scope on
abort. Native calls use the same attempt identity, Live runtime, private Commit
buffer, and publication typestate; exact invocation-id replay is intentionally only
in-memory within that attempt. The crate-private driver preserves those states
without exposing root lane payloads. A private actor prevents cancellation from
silently dropping pre-publication Active/Finished state. The old private
Published shim still proves only in-memory delivery. The new durable path stages
Commit before publication, passes a complete generic mutation plus outbox and a
host-owned canonical candidate fingerprint to one CAS store contract, exposes no
pending Commit after success, and retains publish/resolve in a second private actor
after caller cancellation. The internal `MountedAgent` now proves authoritative
in-memory epoch/session ownership, a borrowed call-props façade, per-replacement
async capture into owned turn props, final-only attempt initialization,
cancellation-safe transition owners, bounded preparation/provider waits, poison
on unacknowledged provider exit, fork, reconfigure, complete pure mutation,
durable handoff, receipt-gated session/revision writeback, and same-epoch
`TurnFlow::Continue`. A second private proof now binds reducer, persistence,
durable identities, factories, and executor once at mount and passes typed
Output/Diagnostic/provider observations to one pure reducer. A combined private
session-store contract now atomically claims every turn, fences final publication
with the same lease, rolls back expired reservations, persists RecoveryRequired
only for expired Running leases, and retains the complete call ledger. The same
owner attaches System/tool metadata once and passes only an opaque provider epoch
binding to ordinary requests. It now reopens an `Existing` durable artifact by
rebuilding only a POM-free runtime registry and receipt-rehydrating the provider:
the owner test proves no second System render or System-bearing attachment. It
remains crate-private: terminal replay and pending-publication reconciliation
return internal report/recovery shapes, the full unpublished candidate cannot be
republished after restart, props/source are borrowed, the session active-epoch
pointer still needs a production atomic transaction, and no concrete production
store ships. The real
  AgentLoop does not use this owner, only one-shot durable workers exist (no
  installed recovery-scanned supervisor), and the legacy executor/sink remains
  in use. Do not claim a production migration until those host boundaries are
  connected.

## Risk Register

| Risk | Required policy |
|---|---|
| Turn data leaks into System | System mount types cannot access turn inputs |
| System text and factories diverge | Store and replace one atomic epoch bundle |
| A finished reducer is reused | Factory creates fresh state for every final-ready turn |
| Streaming becomes post-processing | Reducers run on each parser event in real time |
| Live effect survives abort | Explicit cancellation and compensation are mandatory |
| Raw task abort skips async cleanup | Expose managed cancellation with an acknowledged `AbortReport` |
| Cancel settlement trusts stale or ambiguous provider state | Settle the exact session/epoch/call/input/turn/request/lease/revision only after Provider/Live cleanup joins. `Unchanged` or validated `ResumeFrom` may write `Stopped`; mismatch, `Indeterminate`, pending publication, timeout, cleanup failure or any fence conflict must write/preserve `RecoveryRequired` and keep successors fenced. Complete fault injection before production. |
| Reducer captures a service | Pure reducer API plus explicit host runtimes |
| Legacy and mounted reducers disagree on invalid input | Treat the difference as an explicit migration policy: preserve legacy normalization or approve stricter captured-template validation, then lock both accepted and rejected traces before cutover. |
| Hook plan from discarded preparation | Drop the entire candidate TurnPlan before binding |
| Duplicate streaming tag | Validate stable factory declarations at mount |
| Reconfigure mixes old history with new policy | Explicit history rebase and cursor rule |
| Commit effect duplicates on retry | Stable `(PublicationRequestId, item index)` rows plus receiver-side idempotency; an outbox gives atomic enqueue, not exactly-once external delivery |
| Request id is reused for a different candidate | Persist and byte-compare host-owned `PublicationCandidateFingerprint`; reject a mismatch and preserve the first session/outbox record |
| Store response is lost after commit | Enter Resolving before await and query the same durable request id and fingerprint; never abort or replay the model first |
| Continue swaps to a different store/session identity | Bind persistence once at mount; revision has meaning only together with its durable session identity and store |
| External CAS conflict leaves a stale local owner runnable | Classify conflict as `ReloadRequired` and reject new calls until session + revision are atomically reloaded |
| Identical System text hides a changed runtime contract | Persist a stable epoch-contract identity covering System POM, binding factories, and provider capabilities |
| System POM changes while its manually assigned epoch id does not | Preserve the durable installed System and require an explicit new `EpochContractId`; public local lifecycle test and author docs enforce this migration responsibility |
| In-memory reconfigure is mistaken for durable policy change | Commit/reload the new epoch identity and rebased session through an explicit durable reconfigure transaction |
| Caller disappears after receipt commits `TurnFlow::Continue` | Let the mounted loop owner persist/execute the continuation obligation, or durably record an explicit stop policy before releasing the turn lane |
| Restart or retry reuses a call id from turn zero | Persist the logical call cursor and resolve existing call/request state before provider or tool execution |
| Two owners resume the same pending call | Atomically claim a store-backed lease before User render, provider, tool, or Live work |
| A valid long provider turn outlives its Running lease | Renew the fenced lease while the owner is alive, or enforce a provider deadline strictly below the durable lease lifetime |
| Resume silently changes task/permissions | Persist and validate a `DurableCallInputId` before recapturing User props |
| Process exits with an indeterminate publication | Persist request id, fingerprint, candidate/recovery intent and resolve it before any model replay |
| Repeated call label aliases resources | Use epoch/turn/provider-attempt/binding identities internally |
| Provider schema diverges from System epoch | Store provider capabilities in the same atomic epoch bundle |
| Raw isolated APIs bypass one-attachment ownership | Let the stable mounted facade exclusively own epoch attachment; keep manual lifecycle/attempt APIs experimental |
| Logical System-once is mistaken for physical network-once | Require a stateful provider-session adapter and transport-level integration test when physical single transmission matters |
| Attachment retry installs System twice after a lost response | Key remote create-or-get by durable epoch plus artifact fingerprint; retain the passing generic resume proof and repeat it through the real consumer transport |
| Durable provider adapter is forced through ordinary attach | Keep durable execution separate from the optional `MountedProviderEpochAttacher`; the split is now compile-tested |
| Early channel-contract erasure hides invalid composition | Normalize local channels to the typed root before one-time root-channel-contract erasure |
| Runtime type metadata is mistaken for behavior or durable identity | Pair descriptors with typed adapters; use explicit stable keys across process/build boundaries |
| Component abstraction grows into a UI runtime | Keep VDOM/signals/scheduler as explicit non-goals |

## Validation Gate

Every phase must keep these green:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo test --doc
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
git diff --check
```

Migration phases additionally require golden prompt comparison, real-time event
ordering assertions, failure/abort tests, and proof that every external effect
runs only at its declared host lifecycle boundary.
