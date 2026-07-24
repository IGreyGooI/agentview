# AGENT.md

## Purpose

`agentview` is a library crate for building agent-facing runtimes in Rust. It is not an app shell, provider SDK, or product-specific workflow layer. When making changes, prefer preserving clean boundaries and reusable abstractions over adding one-off helpers for a single integration.

The recommended public entry point is `agentview::prelude::*`. Module paths stay public for now, but the crate-level docs make it clear that `prelude` is the stable mental model.

## Read This Repo In This Order

1. `src/lib.rs`
2. `src/agent.rs`
3. `src/agent_view.rs`
4. `src/pom/`
5. `src/pom_resolution.rs`
6. `src/pom_renderer.rs`
7. `src/llm_call.rs`
8. `src/prompt_context.rs`
9. `src/streaming_tool.rs`
10. `examples/agent_streaming_tool_loop.rs`

That order mirrors the active layering: public exports, long-lived
agent/session logic, Rust-to-POM projection, typed prompt objects,
resolution/rendering, one-turn execution, durable state, streaming tools, and
finally the end-to-end example.

## Code Map

### `src/lib.rs`

- Exposes the crate modules.
- Defines `StorageString`.
- Re-exports the main API through `prelude`.

### `src/agent.rs`

- Owns the long-lived `Agent` session object.
- Defines `AgentViewModel`, `DefaultAgentViewModel`, `TextAgent`, and `TurnFlow`.
- Builds requests from separate typed system/user POM `Document` values.
- Resolves user `DiffSlot` edges against `UserDocumentCursor`, then renders only
  the slot-free `ResolvedDocument`.
- Commits successful turns back into history through `AgentViewModel::commit_turn`.
- Commits the candidate user-document cursor only with a successful turn.

Important invariant: `Agent` owns session state, but a single model-backed request is still delegated to `AgentTurn` from `src/llm_call.rs`.

### `src/agent_view.rs`, `src/pom/`, and POM resolution

- `AgentView::build_root` projects Rust values into a statically known POM root.
- `Document` can mix typed Markdown and XML; only `XmlNode` values can be
  `DiffSlot` payloads.
- System resolution materializes slots without diffing.
- User resolution returns a `ResolvedDocument` plus a candidate
  `UserDocumentCursor`.
- The canonical renderer accepts only `ResolvedDocument`.

Important invariant: derive/build, diff, resolution, and rendering are separate
stages. Business code should not construct prompt markup strings.

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

- Keeps `TemplateEngine`, `PromptRenderable`, and `ContextView` only as legacy
  compatibility APIs.
- Defines the current `ContextViewBuilder` projection boundary and typed
  `TurnArtifact`.

Important invariant: new request paths use POM documents. A `TurnArtifact`
stores a slot-free typed document, never trusted pre-rendered markup.

### `src/stream_parser.rs`

- Low-level Hermes-style XML streaming parser.
- Exposes `on_open`, `on_stream`, and `on_complete` hooks.
- Is intentionally reusable outside the higher-level tool runner.

Important invariant: parser changes must be validated against chunk boundaries, incomplete closing tags, self-closing tags, and raw angle brackets in content.

### `src/streaming_tool.rs`

- Higher-level tool runner built on top of `HermesParser`.
- Registers `<tool>` contracts by their derived `name` attribute and other
  contracts by their derived XML root name.
- Updates a concrete `ParseContext`.
- Converts validation/execution failures into prompt artifacts instead of crashing the whole parse path.

Important invariant: streaming tools should update parse context and artifacts, but they should not take over provider execution or global agent-loop control flow.

### `examples/agent_streaming_tool_loop.rs`

- Best end-to-end reference in the repo.
- Shows how derived system/user documents, typed streaming-tool contracts,
  `LLMExecutor`, `TurnSink`, `StreamingToolRunner`, and `Agent` fit together.
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

### Keep documentation on the POM-first path

When comments and code diverge, treat the current code as the source of truth
and update the docs. Do not reintroduce fixed `## View` / `## Turn Prompt`
envelopes, manual XML formatting, or a rendered-string AST boundary.

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

- Keep legacy rendering isolated from active request assembly.
- Verify that `TurnArtifact` composition stays typed and rejects unresolved
  `DiffSlot` edges.

### If you touch POM derive, resolution, or rendering

- Verify full, changed, unchanged, deletion, and every collection strategy.
- Verify cursor rollback on provider/commit/render failure and epoch retry.
- Add compile-fail tests when an invalid field-mode combination should be
  rejected statically.

### If you touch `src/stream_parser.rs` or `src/streaming_tool.rs`

- Add or update tests for split chunks and malformed/incomplete XML.
- Preserve the current behavior where tool errors become artifacts instead of short-circuiting the whole response by default.

## Validation

Run these before wrapping up:

- `cargo fmt --all -- --check`
- `cargo test --workspace --all-targets`
- `cargo test --doc`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`

If you changed the example or provider integration path, also exercise:

- `cargo run --example agent_streaming_tool_loop`

That example needs provider credentials in the environment.

## Practical Notes For Future Agents

- Start from the example when you need an integration reference.
- Start from `src/agent.rs` when you need control-flow truth.
- Start from `src/stream_parser.rs` tests when debugging streaming XML edge cases.
- Keep public docs and examples aligned with the implementation; this repo is small enough that drift is noticeable quickly.
