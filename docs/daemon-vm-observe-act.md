# Daemon AgentView Observe/Act Design

## Goal

Design a daemon-facing agent protocol for callers that may be an LLM agent, a
human CLI user, a script, or a future UI.

The protocol should expose the daemon's AgentView state directly enough for an
outside caller to decide the next step, while keeping the daemon as the owner of
session state, action validity, and control-loop progress.

## Current Code Truth

`agentview` currently defines the first reusable state envelopes and session:
`ViewSnapshot`, `ViewUpdate`, `ViewPatch`, `ControlReply`, `ViewAwake`, and
`AgentViewApp`. The app covers full `observe`, sync `hook`, and
`act_with_sink` with a full-update response. External replies are parsed through
`TurnSink<ControlReply>`, so the same sink concept handles provider streams and
CLI/daemon replies. Each snapshot carries the captured typed view plus a
slot-free POM `ResolvedDocument` for the current user role. `AgentViewApp`
resolves that document against its `UserDocumentCursor` and only publishes the
candidate cursor after a stable view epoch. It does not yet define the patch
decider or any daemon/CLI transport envelope.

The current `agentview::Agent` loop is model-turn shaped:

1. `AgentViewModel` captures an XML-root view and builds separate system/user
   POM `Document` values.
2. Role-specific resolvers and the canonical renderer build
   `AgentTurnRequest.system` / `.user`.
3. `LLMExecutor::execute_llm` runs one model-backed turn.
4. `AgentViewModel::commit_turn` commits the result into `PromptContext`.
5. `TurnFlow` decides whether the loop waits or continues.

This is clear for provider-backed agents, but it is the wrong public shape for a
daemon controlled through a CLI skill. In the CLI-skill case, the caller may be
another LLM agent or a human, and the daemon should expose an AgentView
decision point rather than pretending every step is an LLM provider call.

`forgotten-city` already has a prototype observe/act pattern in
`crates/server/src/agent.rs`:

- `GET /agent/{agent_id}/observe` waits for a complete `AgentObservation`.
- `POST /agent/{agent_id}/act` accepts an opaque `action_id`.
- The server resolves `action_id` through an internal `AgentActionIndex`.
- The resolved action becomes an engine `ClientMsg`.
- The server records an `ObservationExpectation` so the next observation waits
  for the right kind of stable state.

That prototype does not have `ViewUpdate`. `observe` returns a full projected
agent view. `act` currently returns only `{ accepted, action_id }`.

There is JSON Patch machinery for frontend mirroring, but today the server-side
frontend patch is a root replace of the full `FrontendView`, not a bounded
semantic view patch for agent callers.

## Core Decision

The daemon protocol should be AgentView observe/act, not `LLMExecutor`.

`LLMExecutor` can remain the provider-backed execution boundary for the existing
`agentview::Agent` loop. The daemon protocol should be a sibling loop built from
the same lower-level pieces:

- `PromptContext`
- `AgentViewModel`
- typed POM `Document` / `ResolvedDocument`
- `UserDocumentCursor`
- system/user resolution and canonical rendering
- commit policy
- optional `TurnSink` / streaming-tool parsing

The daemon loop exposes stateful AgentView boundaries to the caller. An LLM
completion request is only one possible turn prompt, not the whole protocol.

## Two Loop Mental Model

This design has two loops. Keeping them separate is the main coherence rule.

Loop A is the `AgentView` session loop. It is frontend-like: lazy, reactive, and
state-facing. It owns or references the application source, uses
`AgentViewModel` to capture a view, uses the same view model to build a
user `Document`, tracks `ViewEpoch`, and exposes `observe`, `act`, and `hook`.
It does not run the outside chat agent's LLM.

Loop B is the outside chat/CLI/daemon/skill loop. It has its own history, model,
tools, and reasoning policy. It uses the `AgentView` session through a transport
wrapper: a CLI command, an HTTP endpoint, a local daemon API, or a skill tool.
It calls `observe` to read the view, `act` to apply an external reply/action,
and `hook` to wait for app-side asynchronous work.

The data crossing from Loop A to Loop B is a `ViewSnapshot`:

```rust
struct ViewSnapshot<View, UserDocument> {
    view_epoch: ViewEpoch,
    turn_id: ViewTurnId,
    view: View,
    user_document: UserDocument,
}
```

The `turn_id` identifies the active user document so `act` can reject stale
replies. The `ResolvedDocument` is the caller-facing prompt/contract. There is
no separate `ControlRequest` layer in `agentview` core. This mirrors the
provider-backed path, where `AgentViewModel::capture_view` and
`AgentViewModel::build_user_document` produce the current user message.

The data crossing from Loop B back to Loop A is a `ControlReply`. The
`AgentView` session feeds that reply into `TurnSink<ControlReply>`, then commits
the sink output into `PromptContext` or application state. The sink is the
domain feedback boundary: parse errors, invalid moves, and other caller mistakes
should normally become sink output or prompt feedback, not infrastructure
errors returned to the caller.

`ViewAwake` is the reactive signal for Loop A. It does not wake the outside chat
agent directly. It only advances the view epoch and releases `hook` waiters so a
fresh snapshot can be lazily captured.

## Feature Split

This design is three features, not one large abstraction:

1. AgentView observe/act protocol:
   - full `observe`;
   - externally supplied `act` reply;
   - stale request rejection and session ownership.
2. `ViewUpdate` and patch decider:
   - partial-or-full `act` update;
   - patch budget and fallback;
   - caller-profile-specific update policy.
3. Awake runtime and sync hooks:
   - let the owner of an `AgentViewModel` source awake the current view loop;
   - wait until awake happens after a known epoch;
   - recapture the view and return a full snapshot.

The existing `agentview::Agent` already covers the model-turn loop. These
features add the missing daemon/session shell around that loop.

Reply parsing is an implementation detail inside `act`. It can use
`TurnSink`-style parsing, a clap-backed command sink, or structured JSON, but it
is not a separate top-level protocol feature.

## Protocol Shape

The base public operations are:

```text
observe(session) -> full ViewSnapshot
act(reply)       -> ViewUpdate
hook(epoch)      -> full ViewSnapshot
```

`observe` always returns a full transport snapshot with the complete typed
view. Its resolved user document still follows the session's committed
`UserDocumentCursor`, so unchanged stateful slots may be omitted. A caller that
has lost that prompt lineage must first reset the cursor (for example through
`AgentSession::reset_user_document_cursor` or a transport-level force-full
operation) and then observe again.

`act` is the normal progress operation. It resumes the daemon with a response to
the current user document, advances the daemon as far as the daemon's loop
chooses, and returns an update.

## Snapshot Invariant

Every `ViewSnapshot` is complete at the transport and typed-view layers.

A caller can inspect the complete current view and the current resolved user
document without private daemon state. Understanding omitted context, however,
can require earlier messages in the same prompt lineage. “Full snapshot”
describes the transport envelope and typed view; it does not mean every
stateful user-document slot is resent in full.

This invariant is evaluated at the root rendered prompt-context boundary. It is
not a rule that every nested view fragment must locally hydrate every reference
it prints. A child fragment may render a stable handle when the handle's target
is introduced by another root-level section in the same full snapshot, or by
earlier prompt context that has already been committed to the agent's history.

For example, a graph view may render `<source_chunk id="src_chunk.1"/>` as a
provenance handle. The graph view does not need to inline the source text next to
that handle. The source workspace or runtime root view is responsible for
rendering newly read source chunks as their own prompt section. After that text
has entered the prompt context, later graph facts can cite the stable
`source_chunk` id without turning it into a dangling pointer.

Current core shape:

```rust
struct ViewSnapshot<View, UserDocument> {
    view_epoch: ViewEpoch,
    turn_id: ViewTurnId,
    view: View,
    user_document: UserDocument,
}
```

Transport-level envelopes may add session ids, status, events, or serialized
metadata. The reusable core type stays focused on the captured view plus the
turn identity, view, and resolved user document.

## Update Invariant

Long term, `act` should be able to default to a partial update, but full
snapshot fallback is always valid. The first `AgentViewApp::act_with_sink`
implementation returns a full update while the patch decider is still separate.

Suggested shape:

```rust
struct ViewUpdate<View, UserDocument> {
    base_epoch: ViewEpoch,
    view_epoch: ViewEpoch,
    body: ViewUpdateBody<View, UserDocument>,
}

enum ViewUpdateBody<View, UserDocument> {
    Partial(ViewPatch),
    Full(ViewSnapshot<View, UserDocument>),
}
```

The daemon should still capture the complete next typed view and resolve the
current user document internally. A transport adapter may render the
`ResolvedDocument` when it needs provider text. Partial updates are response
shaping, not the source of truth.

## Patch Decider

Patch selection is a separate feature from the observe/act protocol.

The daemon needs a decider because patch usefulness depends on the caller. A
human UI, a script, and an LLM agent do not consume patches the same way. For an
LLM agent, patch complexity is not just bytes. Too many scattered changes can be
harder to use than a fresh full snapshot.

Initial conservative policy:

- `observe` always returns full.
- `act` tries to return partial by default.
- `act` falls back to full when the patch is too large or too scattered.
- For LLM-agent callers, start with a low patch limit, roughly 10-20 operations.
- Full fallback is not an error.

Possible decider inputs:

```rust
struct ViewUpdateDecisionInput {
    caller_profile: CallerProfile,
    base_view_epoch: ViewEpoch,
    next_view_epoch: ViewEpoch,
    patch_op_count: usize,
    patch_byte_count: usize,
    sections_touched: usize,
}
```

Possible output:

```rust
enum ViewUpdateDecision {
    ReturnPartial,
    ReturnFull,
}
```

The first implementation can be intentionally simple. Real tuning should come
from traces of outer LLM agents and humans using the protocol.

## User Document Model

The typed view tells the outside caller what the daemon sees. The resolved user
document decides what enters the current prompt turn: a stateful context slot,
task, typed reply contract, artifacts, feedback, or other Markdown/XML blocks.

This reuses the current `AgentViewModel` contract:

```rust
async fn capture_view(&self, source: &Self::Source) -> Self::View;

async fn build_user_document(
    &self,
    ctx: &PromptContext<I, Self::ContextState>,
    call_id: &str,
    task: StorageString,
    current_view: &Self::View,
) -> anyhow::Result<Document>;
```

The provider-backed `Agent` path resolves this document against the session
cursor, renders the resulting `ResolvedDocument`, and sends it in
`AgentTurnRequest.user`.

The observe/act path returns the captured view plus resolved user document as a
`ViewSnapshot`. The outside chat/CLI/daemon/skill loop reads that snapshot in
its own context and may later call `act` with a `ControlReply`.

The important point is negative: `agentview` core should not invent a separate
`ControlRequest` layer. The reusable core has `turn_id` for stale-reply
protection. If a target needs status, schemas, or transport metadata, those
belong either in typed POM content or in a transport/session envelope around
`ViewSnapshot`.

## Tool Abstraction

Tools should be abstract protocol data.

The current code has two tool models:

- `StreamingTool` parses XML-like assistant text through `TurnSink`.
- Rig native tools are registered on a Rig agent and dispatched inside
  `LLMExecutor`.

For the daemon/CLI/skill protocol, both forms need to become visible at the
control boundary. The daemon should be able to ask the caller for a model
response, expose available tools, receive tool calls or assistant text, and then
resume with parsed results.

`ToolDefinition` is not a Rig tool. It is a provider-neutral description of a
capability. A Rig tool, a CLI command, an MCP call, a daemon-local operation, or
an XML-style streaming tool can be adapted into this shape.

Suggested common definition:

```rust
struct ToolDefinition {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    execution: ToolExecution,
}

enum ToolExecution {
    Daemon,
    Caller,
}

struct ToolCall {
    call_id: ToolCallId,
    name: String,
    args: serde_json::Value,
}

struct ToolResult {
    call_id: ToolCallId,
    ok: bool,
    content: serde_json::Value,
}
```

The application user defines tools by supplying:

- the externally visible name;
- a description for the caller/model;
- an input schema;
- where the tool executes;
- optionally a local handler when `execution == Daemon`.

The first trait can be small:

```rust
trait VmTool {
    fn definition(&self) -> ToolDefinition;

    async fn call(&self, args: serde_json::Value) -> anyhow::Result<serde_json::Value>;
}
```

Only daemon-owned tools need to implement `call`. Caller-owned tools only need a
definition because execution happens outside the daemon and returns later as a
`ToolResult`.

`ToolExecution::Daemon` means the daemon owns the implementation and will
execute after validating the call. `ToolExecution::Caller` means the CLI skill
or outer agent environment must execute it and resume the daemon with a
`ToolResult`.

This keeps the model-control path provider-neutral. A provider-native tool call,
a CLI skill tool call, and an XML streaming tool can all be represented in the
same protocol.

## CLI Prompt Sink

The daemon protocol needs a sink that can parse the caller's response, not just a
sink that parses provider-streamed text.

Current `StreamingToolRunner` implements `TurnSink<TextTurnEvent>` and parses
assistant XML from streamed model text. In the daemon protocol, the response may
come from a CLI skill call as one JSON payload, as plain text, or as tool-call
JSON produced by an outer agent.

That should reuse the same sink abstraction with a different event type:

```rust
trait TurnSink<E> {
    type Output;

    async fn on_event(&mut self, event: E);
    async fn finish(self: Box<Self>) -> Self::Output;
}
```

For daemon/CLI turns:

```rust
impl TurnSink<ControlReply> for MyReplyParser {
    type Output = MyReplyOutput;
}

enum ControlReply {
    Text(StorageString),
    Structured(serde_json::Value),
}
```

The sink should handle ordinary parsing and validation failures internally. Its
`Output` can be accepted domain state, rejection feedback, tool requests, or
other per-turn facts. The `AgentViewApp` caller should only see errors for
session/infrastructure failures such as a stale `turn_id` or a failed apply
step.

The first implementation can wrap existing streaming-tool parsing:

```text
ControlReply::Text
  -> TextTurnEvent::TextComplete
  -> StreamingToolRunner
```

But the public daemon protocol should not require XML text. It should allow
structured tool calls and tool results directly.

## Awake Runtime And Sync Hooks

Hooks are not tools.

A tool is a callable capability exposed through a turn prompt or reply grammar.
The awake runtime is a reactive signal primitive. It does not detect rendered
view changes and it does not classify why the application changed.

The owner of the `AgentViewModel::Source` decides when a loop waiting on that
source may continue and calls `awake`. The daemon/session runtime waits for an
epoch newer than the one in the caller's snapshot, then captures the view again.

Current implementation shape:

```rust
struct ViewAwake;
struct ViewAwakeHandle;
struct ViewAwakeSubscription;
type ViewEpoch = u64;

impl ViewAwake {
    fn handle(&self) -> ViewAwakeHandle;
    fn subscribe(&self) -> ViewAwakeSubscription;
    fn current_epoch(&self) -> ViewEpoch;
    async fn wait_after(&self, epoch: ViewEpoch) -> ViewEpoch;
}

impl ViewAwakeHandle {
    fn awake(&self) -> ViewEpoch;
    fn current_epoch(&self) -> ViewEpoch;
}
```

There is only one signal:

```text
awake
```

No reason enum is part of this layer. `act` commit, tool completion, user input,
file watcher events, engine events, and subscription matches are all
application-level causes. If the application decides one of them makes the view
loop observable again, it calls `awake`.

The current code has a Rig-specific hook in the GM path. `GmHook` observes Rig
tool results and completion responses for debug logging. That hook is tied to
Rig's provider loop, not to the AgentView protocol. That is closer to a debug
observer than the hook being designed here.

Sync hooks are built on top of the awake runtime:

```rust
struct ViewHookRequest {
    session_id: SessionId,
    after_epoch: ViewEpoch,
    condition: ViewHookCondition,
}

enum ViewHookCondition {
    Awoken,
    UserDocumentChanged,
    ToolResultAvailable { call_id: ToolCallId },
    StatusChanged,
    Custom(serde_json::Value),
}

struct ViewHookResult {
    session_id: SessionId,
    base_epoch: ViewEpoch,
    epoch: ViewEpoch,
    snapshot: ViewSnapshot,
}
```

This is a blocking or long-poll-style operation. It is sync in the protocol
sense: the caller asks the daemon to wait until the view source is awoken after
a known epoch, and the daemon returns a fresh full snapshot after
recapturing the view.

How sync hooks relate to the VM:

```text
observe
  -> capture view and resolve user document
  -> build full ViewSnapshot
  -> return snapshot

act(reply)
  -> parse reply
  -> if reply contains tool calls:
       execute/proxy tool
  -> resume daemon state
  -> capture/resolve next VM update
  -> return update

hook(condition)
  -> wait until the view owner awakes after after_epoch
  -> capture view and resolve user document
  -> build full ViewSnapshot
  -> optionally check a condition over the recaptured snapshot
  -> return hook result
```

Hooks do not replace `observe` or `act`. They add a wait-for-change operation for
callers that need to synchronize with daemon progress instead of polling full
observations repeatedly. The first useful condition can simply be `Awoken`;
richer conditions can be added after real callers need them.

Passive tracing should use a separate observer concept, not `ViewHook`.

### Existing GM Precedent

`forgotten-city` already implements a specialized version of this runtime around
the GM agent.

The GM runtime has:

- a wake channel: `mpsc::Sender<GmWake>` / `mpsc::Receiver<GmWake>`;
- an event hook: `GmEventHook` listens to game events, checks subscriptions, and
  sends `GmWake::Subscription`;
- a long-lived agent task: `GmAgent::run` waits on `wake_rx.recv()`;
- wake coalescing: after the first wake, it drains pending wakes with
  `try_recv`;
- an outer loop: `run_cycle` executes `agentview::Agent` steps;
- a stop condition: the `done` tool sets `world.gm_done_flag`, and
  `commit_turn` returns `TurnFlow::Wait` when done or after `GM_MAX_STEPS`.

That shape is close to the generic awake/hook runtime:

```text
external event
  -> application decides the view source is stale
  -> ViewAwakeHandle::awake()
  -> hook waiter wakes
  -> runtime captures AgentViewModel again
  -> full ViewSnapshot returns to the caller
```

The Rig `PromptHook` used by `GmHook` is different. It is a provider/debug hook
for logging tool results and model responses. It is not the sync hook runtime.

## Control Loop

The daemon owns the loop.

The caller only sees:

```text
observe -> full view snapshot
act     -> partial or full view update
```

Internally the daemon may:

1. parse the caller response with `TurnSink<ControlReply>`;
2. wake or resume session code;
3. process events;
4. update `PromptContext` or application state;
5. capture the next view and resolve its user document;
6. ask the patch decider whether to return partial or full.

The important KISS rule is that the public API does not expose a separate
`Pending` result. Pending/running/waiting state lives inside the VM.

## Relationship To Existing `agentview`

The existing `Agent` loop should not be stretched into this by forcing the
daemon to implement `LLMExecutor`.

`AgentViewApp` is the observe/act-oriented sibling that reuses the POM and state
building blocks. The provider-backed loop remains useful for ordinary agents;
`AgentViewApp` serves stateful daemon sessions driven by an external caller
through CLI or skill transport and can continue evolving with patch selection
and transport-specific recovery.

The extraction point is around request/view preparation and commit semantics.
The first `AgentViewApp` pulls out the external observe/hook/act path.
Further implementation may need reusable helpers that can:

- capture the current AgentView view;
- resolve and validate the current user document for external observation;
- build the current user document;
- parse an externally supplied control reply;
- commit parsed control output;
- publish the user-document cursor and view epoch at their success boundaries.

## Open Questions

- What is the exact serialized `VmView` shape for the first daemon target?
- Should `ViewPatch` be JSON Patch, a semantic patch, or both?
- How should callers declare their profile: LLM agent, human CLI, UI, script?
- What is the first patch limit for LLM-agent callers: 10, 12, 20 operations?
- How much of `AgentViewModel::commit_turn` should be reused directly versus
  wrapped by a daemon-specific commit trait?
- Which reply-output shapes should examples use for accepted/rejected/tool-call
  outcomes?
- How much of Rig's native `Tool` definition can be translated directly into
  `ToolDefinition` without leaking Rig-specific behavior?
- Which sync hook conditions are needed after the initial `Awoken` case?
