# AGENT.md

## Purpose

`agentview` is a library crate for building agent-facing runtimes in Rust. It is not an app shell, provider SDK, or product-specific workflow layer. When making changes, prefer preserving clean boundaries and reusable abstractions over adding one-off helpers for a single integration.

The recommended public entry point is `agentview::prelude::*`. Module paths stay public for now, but the crate-level docs make it clear that `prelude` is the stable mental model.

## Read This Repo In This Order

1. `src/lib.rs`
2. `src/agent.rs`
3. `src/llm_call.rs`
4. `src/prompt_context.rs`
5. `src/templates.rs`
6. `src/stream_parser.rs`
7. `src/streaming_tool.rs`
8. `examples/agent_streaming_tool_loop.rs`

That order mirrors the actual layering: public exports, long-lived agent/session logic, one-turn execution, durable prompt state, rendering, low-level streaming parse, higher-level streaming tools, and finally the end-to-end example.

## Code Map

### `src/lib.rs`

- Exposes the crate modules.
- Defines `StorageString`.
- Re-exports the main API through `prelude`.

### `src/agent.rs`

- Owns the long-lived `Agent` session object.
- Defines `AgentViewModel`, `DefaultAgentViewModel`, `TextAgent`, and `TurnFlow`.
- Builds requests by combining:
  - durable prompt context from `PromptContext`
  - the latest captured context view
  - rendered prompt templates
  - per-turn task text
- Commits successful turns back into history through `AgentViewModel::commit_turn`.
- Stores the last rendered view in `view_cursor` so later turns can render full, delta, or empty context blocks.

Important invariant: `Agent` owns session state, but a single model-backed request is still delegated to `AgentTurn` from `src/llm_call.rs`.

### `src/llm_call.rs`

- Defines `AgentTurnRequest`, `ExecutorCommit`, `AgentTurnOutcome`, and the observer event types.
- Defines the boundary traits:
  - `LLMExecutor`: provider-specific execution belongs here.
  - `TurnSink`: per-turn event accumulation/parsing belongs here.
- `AgentTurn` is one model-backed transaction. It should not own prompt-history mutation.

Important invariant: provider-native tools belong behind `LLMExecutor`, not in the generic parser/tooling layers.

### `src/prompt_context.rs`

- Defines the durable prompt/session state model.
- `history` is committed, append-only transcript state.
- `working_set` is mutable staging state owned by the view-model layer.
- `TurnTransform` controls how default text turns are committed.
- `IdentityTransform` preserves the default text-only behavior.

Important invariant: if you change commit behavior, check `transform_user`, `transform_assistant`, lazy system-prompt persistence, and feedback reset behavior together.

### `src/templates.rs`

- Defines the prompt rendering surface:
  - `TemplateEngine`
  - `PromptRenderable`
  - `ContextView`
  - `ContextViewBuilder`
  - `TurnArtifact`
- Supports full render, delta render, and agent-visible per-turn artifacts.

Important invariant: if a view can render a delta, it should still behave correctly when there is no previous snapshot or no meaningful change.

### `src/stream_parser.rs`

- Low-level Hermes-style XML streaming parser.
- Exposes `on_open`, `on_stream`, and `on_complete` hooks.
- Is intentionally reusable outside the higher-level tool runner.

Important invariant: parser changes must be validated against chunk boundaries, incomplete closing tags, self-closing tags, and raw angle brackets in content.

### `src/streaming_tool.rs`

- Higher-level tool runner built on top of `HermesParser`.
- Registers handlers by XML tag.
- Updates a concrete `ParseContext`.
- Converts validation/execution failures into prompt artifacts instead of crashing the whole parse path.

Important invariant: streaming tools should update parse context and artifacts, but they should not take over provider execution or global agent-loop control flow.

### `examples/agent_streaming_tool_loop.rs`

- Best end-to-end reference in the repo.
- Shows how `LLMExecutor`, `TurnSink`, `StreamingToolRunner`, `ContextView`, and `Agent` fit together.
- Also shows the expected environment split:
  - default feature uses OpenRouter
  - non-`openrouter` path uses an OpenAI-compatible base URL

## Architecture Rules

### Keep the boundaries clean

- `Agent` owns session state and request/commit orchestration.
- `AgentTurn` owns one executor + sink transaction.
- `LLMExecutor` owns provider I/O, retries, streaming shape, and native tool execution.
- `TurnSink` owns per-turn parsing/aggregation.
- `AgentViewModel::commit_turn` owns durable history decisions and `TurnFlow`.

If a change blurs those lines, it is probably going in the wrong file.

### Prefer generic runtime concepts over app-specific ones

This crate is deliberately generic. Avoid baking NPC/game/product-specific semantics into the core modules unless they are clearly reusable abstractions.

### Trust the implementation over stale comments

The crate docs in `src/agent.rs` still mention a two-phase snapshot pattern, but the current implementation captures the view inside turn execution via `capture_view(source)` and updates `view_cursor` after commit. When comments and code diverge, treat the current code as the source of truth and update the docs.

## Change Guidance

### If you touch `src/agent.rs`

- Verify request construction and commit semantics together.
- Check observer notifications for success, failure, abort, and loop flow.
- Be careful with lock scope: the current implementation intentionally avoids holding locks across executor calls.

### If you touch `src/llm_call.rs`

- Preserve the separation between executor output and context commit.
- Keep `TurnSink::finish` semantics clear: sinks may own per-turn state and return it only after executor success.

### If you touch `src/prompt_context.rs`

- Preserve the distinction between `history` and `working_set`.
- Do not accidentally make committed transcript mutation depend on provider-specific assumptions.

### If you touch `src/templates.rs`

- Verify full-context, delta-context, and unchanged-context behavior.
- Remember that `RenderedTurnArtifact.rendered` is treated as trusted prompt markup.

### If you touch `src/stream_parser.rs` or `src/streaming_tool.rs`

- Add or update tests for split chunks and malformed/incomplete XML.
- Preserve the current behavior where tool errors become artifacts instead of short-circuiting the whole response by default.

## Validation

Run these before wrapping up:

- `cargo test`
- `cargo test stream_parser`
- `cargo test streaming_tool`

If you changed the example or provider integration path, also exercise:

- `cargo run --example agent_streaming_tool_loop`

That example needs provider credentials in the environment.

## Practical Notes For Future Agents

- Start from the example when you need an integration reference.
- Start from `src/agent.rs` when you need control-flow truth.
- Start from `src/stream_parser.rs` tests when debugging streaming XML edge cases.
- Keep public docs and examples aligned with the implementation; this repo is small enough that drift is noticeable quickly.
