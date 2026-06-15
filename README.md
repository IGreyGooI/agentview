# agentview

`agentview` is an AI/AX runtime for language agents.

It is for cases where an agent should act on structured application state, keep turn history, and see either a full view or a delta between turns.

Start here:

```rust
use agentview::prelude::*;
```

## Core Idea

`agentview` treats agent interaction as a ViewModel problem.

Your app exposes state through a `ContextViewBuilder`. `agentview` renders that state into prompts, runs one model-backed turn through your executor, and commits successful results back into durable context.

The main pieces are:

- `Agent`: long-lived session state
- `PromptContext`: system prompt, history, and view-model state
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
6. Commit successful results back into `PromptContext`.

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

Read [examples/agent_streaming_tool_loop.rs](/home/greygoo/runtime/agentview/examples/agent_streaming_tool_loop.rs) for the clearest end-to-end example.

Useful source files:

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
