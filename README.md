# agentview

`agentview` is an AI/AX runtime for language agents.

It is for cases where an agent should act on structured application state, keep turn history, and see either a full view or a delta between turns.

For new POM component authoring, start here:

```rust
use agentview::component::prelude::*;
```

Run the prompt-only mounted example first:

```bash
cargo run --example hello_world
```

The compatibility `Agent` / `ContextViewBuilder` runtime documented later uses
`agentview::prelude::*`. Do not mix its authoring types into a new mounted POM
component.

## POM Component Runtime Status

The `Agent` / `ContextViewBuilder` flow documented below is the current
compatibility runtime. The mounted runtime now has opaque public
`MountedAgent`/`MountedCall` handles and an AgentView-owned
`InMemoryMountedAgentFactory`. Its external lifecycle test executes two calls,
replay, reload, drop/reopen, awaited Live effects, and joined cancellation while
rendering and attaching one retained `DurableSystem` exactly once.

`InMemoryMountedAgentFactory` is deliberately a process-local lifecycle host,
not production persistence. Clones and repeated opens of the same factory share
one store and System epoch. Separately constructed factories are independent,
even when given the same `DurableSessionId`. The host currently requires
`Commit = Never` and does not expose durable reconfiguration, cross-process
recovery, or an outbox worker. Initial sessions should use
`PromptContext::without_system()`; an existing different System is rejected.

Forgotten City's real AgentLoop still uses the compatibility executor, so the
mounted API remains unfrozen. `DurableSystem::into_one_shot_component` is
compatibility behavior rather than a durable reopen API.

For the mounted direction, examples, current guarantees, and remaining host
work, see [POM component authoring](docs/pom-component-authoring-examples.md)
and the [implementation roadmap](kanban.md).

Use the mounted examples in this order:

- `cargo run --example hello_world` for the minimal
  `PromptComponent<Props>` shape: one durable System POM and a fresh User POM
  per turn, with no runtime channels;
- `cargo run --example chess_agent_mounted_turn` for a typed streaming
  component whose split XML chunks produce awaited Live effects and typed
  Output through the public mounted host;
- `cargo run --example chess_engine_mounted` for exact chess prompt migration
  using the same prompt-only authoring shape with a fallible User builder;
- `cargo run --example chess_engine_mounted_external` for the advanced,
  host-owned external reply controller shape: the port receives the one
  System render, persists an explicit delivery receipt/outbox identity, then
  enables typed chess actions, atomic-shaped domain/state/outbox commits, and
  wake-driven fresh User renders. It now directly reproduces the legacy
  example's initial view, post-`e2e4` waiting view, asynchronous real Stockfish
  move, and final hook. `ExternalReply<ChessReplyContract>` binds the
  System-visible reply grammar and pure decoder before the host opens that
  controller. Its in-memory host is a local demonstration, not a durable
  transport or production persistence adapter. Actionable and Passive are now
  distinct frame types: Passive has no action token and never advances the
  baseline. Acknowledged Actionable frames produce real User POM deltas;
- `cargo run --example chess_engine_mounted_agentloop` for the provider-driven
  counterpart: one System attachment, full then delta User POM, real-time
  typed Live preview, strict semantic publication gate, typed Commit/outbox
  replay, and a real Stockfish move between two scripted provider turns.

In each example, the POM types and `#[view(component)]` functions form the
authoring boundary. The referenced shared `support` module is deliberately
example-only host wiring: it captures turn props, runs a scripted provider, and
records lifecycle facts. An application host owns those effects; a component
stays pure.

The first three examples isolate authoring, prompt, and streaming concepts. The
repository CLI and Chess skill now run `observe -> act -> hook` through the
mounted external controller, including acknowledged delta and multiple real
Stockfish rounds. That is still a local proof: a production transactional
`MountedExternalPort`, daemon restart persistence, replacement-consumer resync,
and a real remote provider remain open. The controller never returns raw System
text to an AgentLoop: `MountedExternalPort::complete_epoch` owns delivery and
must retain a durable `ExternalSystemDeliveryReceipt` before an epoch may reopen
as active.

## Core Idea

An LLM-native application does not embed an LLM as a function inside a
predefined workflow. The LLM is the application's semantic operator and
decision-making control plane; the application provides legible state,
composable capabilities, validation, persistence, and resource/safety
boundaries.

```text
Agent != Application + LLM
Agent = Application for LLM
```

`agentview` exists to support this boundary: it gives an LLM a structured view
of the application and turn/session semantics without baking product workflow
into the runtime.

`agentview` treats agent interaction as a ViewModel problem.

Your app exposes state through a `ContextViewBuilder`. `agentview` renders that state into prompts, runs one model-backed turn through your executor, and commits successful results back into durable context.

The important boundary is the root prompt context, not every nested view
fragment. A root view should render enough information for the agent to
understand the current turn, while nested fragments may use stable handles for
things already rendered elsewhere in the root view or in earlier committed
turns.

The main pieces are:

- `Agent`: provider-backed turn runner
- `AgentSession`: committed prompt context plus its rendered view cursor
- `AgentViewApp`: externally controlled `observe` / `hook` / `act` loop
- `PromptContext`: system prompt, history, working set, and view-model state
- `ContextView` / `ContextViewBuilder`: what the agent sees
- `LLMExecutor`: your provider adapter
- `TurnSink`: per-turn output handling
- `StreamingToolRunner`: XML-style parsing over streamed text

## Typical Flow

1. Capture app state with `ContextViewBuilder`.
2. Render a full context block on the first turn, then a delta or empty block on later turns.
3. Add the current task with `.with_user(...)`.
4. Run the turn through your `LLMExecutor`.
5. Parse or observe output with a `TurnSink`.
6. Commit successful results and the new view cursor as one `AgentSession`.

Each model turn works on a private session draft. Preparation, compaction,
rendering, provider execution, and `commit_turn` may modify that draft, but the
agent only publishes it after the whole turn succeeds. Errors and cancellation
leave the previously committed session unchanged.

## Minimal Usage

For a normal text agent, provide:

- a `ContextViewBuilder`
- a `PromptLayout`
- `PromptSystemVars`
- an `LLMExecutor`
- optionally a `TurnTransform`

Create an agent:

```rust,ignore
use agentview::prelude::*;

let agent: TextAgent<MyContextBuilder, MyExecutor, IdentityTransform> = Agent::new(
    my_context_builder,
    my_prompt_layout,
    PromptSystemVars {
        instructions: "You are a helpful agent.".to_string(),
        output_schema: None,
    },
    "my-model",
    512,
    IdentityTransform,
);
```

Run one turn:

```rust,ignore
agent
    .call("PlanTurn")
    .with_user("Review the latest state and propose the next action.")
    .execute(&source, &executor)
    .await?;
```

## Structured Streaming Output

If your model emits XML-like tags in streamed text, use `StreamingToolRunner` as the sink.

Example output:

```xml
<verify_intent_budget scope="demo"/>
<select local_id="a1"/>
<select local_id="a7"/>
```

In that setup:

- `LLMExecutor` still owns provider streaming
- `StreamingToolRunner` is the `TurnSink`
- each `StreamingTool` handles one tag
- your `ParseContext` stores parsed state and prompt artifacts

Example shape:

```rust,ignore
let parse_ctx = agent
    .call("DemoSelect")
    .with_user("Select 1-3 valid intents.")
    .execute_with_sink(
        &source,
        &executor,
        StreamingToolRunner::new(DemoParseContext::default())
            .with_tool(VerifyIntentBudgetTool)
            .with_tool(SelectTool),
    )
    .await?;
```

## Put Logic In The Right Layer

- app state capture and rendering: `ContextViewBuilder`, `ContextView`
- provider calls and streaming: `LLMExecutor`
- per-turn parsing and side effects: `TurnSink`
- commit policy and loop control: `AgentViewModel::commit_turn`

Avoid putting provider networking into `Agent`, history mutation into `TurnSink`, or native provider tools into `StreamingToolRunner`.

## Best Reference

For mounted authoring, start with
[examples/hello_world.rs](/home/greygoo/runtime/agentview/examples/hello_world.rs),
then read
[examples/chess_agent_mounted_turn.rs](/home/greygoo/runtime/agentview/examples/chess_agent_mounted_turn.rs).
The legacy end-to-end compatibility path remains in
[examples/agent_streaming_tool_loop.rs](/home/greygoo/runtime/agentview/examples/agent_streaming_tool_loop.rs).

Useful source files:

- [docs/semantic-agent-view.md](/home/greygoo/runtime/agentview/docs/semantic-agent-view.md)
- [src/lib.rs](/home/greygoo/runtime/agentview/src/lib.rs)
- [src/agent.rs](/home/greygoo/runtime/agentview/src/agent.rs)
- [src/llm_call.rs](/home/greygoo/runtime/agentview/src/llm_call.rs)
- [src/streaming_tool.rs](/home/greygoo/runtime/agentview/src/streaming_tool.rs)
- [src/stream_parser.rs](/home/greygoo/runtime/agentview/src/stream_parser.rs)

## Running The Example

Default feature path:

```bash
OPENROUTER_API_KEY=... cargo run --example agent_streaming_tool_loop
```

OpenAI-compatible path:

```bash
cargo run --no-default-features --example agent_streaming_tool_loop
```

That path reads `OHMYGPT_API_KEY` and optionally `OHMYGPT_BASE_URL`.

You can also override the model:

```bash
AGENT_EXAMPLE_MODEL=your-model cargo run --example agent_streaming_tool_loop
```
