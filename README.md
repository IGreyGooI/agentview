# agentview

`agentview` is a Component runtime for rendering typed application state into
provider-neutral prompts and routing model output back into application-owned
state.

The current runtime boundary is:

```text
ComponentHost (business authority)
  props + retained Signal state
          |
          | render
          v
RenderedProjection (ordered Component nodes)
          |
          v
ApplicationHost (one reaction)
          |
          | ProviderPort::execute
          v
ProviderEvent stream -> reaction-local Component consumers -> Signal updates
```

Component state is authoritative. Provider continuation, submitted history,
prompt-cache state, compaction state, and artifact representation are private,
discardable optimizations below `ProviderPort`.

## Component Authoring

Import the Component authoring surface from one prelude:

```rust
use agentview::component::prelude::*;
```

A root Component receives typed props and the framework-owned Provider event
input. Retained business state lives in `Signal<T>`:

```rust,ignore
#[derive(Clone)]
struct AppProps {
    task: String,
}

#[component]
fn application_root(
    props: AppProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let answer = use_signal(String::new);
    let answer_for_events = answer.clone();

    view! {
        #[system_once]
        policy { "Answer the current task." }

        task { "{props.task}" }

        {
            EventListener::observe("app.answer", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let answer = answer_for_events.clone();
                    async move {
                        if let TextTurnEvent::TextComplete(text) = event {
                            answer.set(text)?;
                        }
                        Ok::<(), SignalAccessError>(())
                    }
                })
        }
    }
}
```

`ComponentHost<Props>` retains props, mounted Component identities, Signals,
and the current complete projection. `set_props` and Signal writes mark the
host dirty but never call a model automatically.

`ApplicationHost<P>` runs one explicit `dispatch_llm_reaction`: render once,
call the selected Provider once, dispatch Provider events in order, and await
all matching handlers before returning. Dropping that future cancels the
reaction; Signal writes that already completed remain committed.

## Provider Boundary

All model backends implement one non-generic trait:

```rust,ignore
#[async_trait]
pub trait ProviderPort: Send {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault>;
}
```

`RenderedProjection` contains an ordered `Vec<RenderedProjectionNode>`. Each
node preserves its runtime identity and ordered `Vec<CanonicalInputItem>`.
There is no duplicated flat item list or cross-node source-interleaving index.

The repository currently includes:

- OpenAI Responses and Chat Completions ProviderPorts;
- `ExternalProviderPort`, driven through the outer `ExternalApplication`
  observe/act wrapper used by the CLI skill;
- `DebugProviderPort`, which captures a readable complete prompt without model
  I/O or implicit logging.

Responses and Chat keep submitted history, continuation, assistant output, and
accepted semantic-diff baselines privately. If that context is lost, they start
Fresh from the current complete Component projection.

## Diff

`#[diff(slot = "...")]` marks a complete POM fragment that a Provider may lower
to full, delta, or omitted submission. Components always render complete
business state; accepted baselines and lowering are Provider-owned
optimizations. Atomic or single-field changes fall back to the complete current
value. Structured multi-field roots may omit stable fields and submit a
semantic field delta.

## Examples

Render a minimal Component tree without Provider I/O:

```bash
cargo run --example hello_world
```

Run the canonical live Chess example, where the configured model plays White
and Stockfish plays Black:

```bash
cargo run --example chess_agentview
```

This is a paid live example. It requires the Provider environment documented in
[`docs/chess-runtime-target.md`](docs/chess-runtime-target.md) and a local
Stockfish executable.

Inspect ordered Component nodes, complete prompts, Responses submissions,
semantic deltas, retained wire history, and Fresh recovery without an API key
or external network access:

```bash
cargo run --example provider_port_visual_acceptance
```

The report ends with `ACCEPTANCE PASSED` only after its exact assertions pass.

Build the daemon-backed External CLI:

```bash
cargo build --bin agentview
export AGENTVIEW_ADDR=127.0.0.1:47631
export AGENTVIEW_TOKEN="$(openssl rand -hex 32)"
target/debug/agentview observe
target/debug/agentview act 'model output'
```

See [`skills/agentview-external/SKILL.md`](skills/agentview-external/SKILL.md)
for the JSON-lines text protocol and Full/Delta recovery rules.

## Current Scope

The current Component/ApplicationHost path intentionally does not add
checkpoint, CAS, an outer AgentLoop, or durable business workflow above
`ProviderPort`. The separately exported RecordStore surface is compatibility
infrastructure and is not owned or advanced by ApplicationHost. The Responses
native-tool path currently supports name-only declaration, completed-call
dispatch, and one handler-returned `ToolOutput` staged for the next Provider
Input Gate. Chat Completions fails locally on native-tool declarations until it
has its own lowering. Native tool input-schema authoring, request-level
`parallel_tool_calls` policy, and public keyed Component identity remain
deferred.

The authoritative boundary is
[`docs/provider-port-application-host-boundary.md`](docs/provider-port-application-host-boundary.md).
Open public-contract questions are tracked in
[`docs/provider-port-application-host-open-questions.md`](docs/provider-port-application-host-open-questions.md),
and [`docs/engine.md`](docs/engine.md) is authoritative runtime design only.

## Validation

```bash
cargo test --workspace --all-targets
cargo test --doc
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --all-features
git diff --check
```
