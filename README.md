# agentview

`agentview` is a Component runtime for rendering typed application state into
provider-neutral Frames and admitting ordered model facts back into
application-owned state.

Coding agents: start with [HELP.md](HELP.md) for business POM and diff authoring,
native tools, the tree fold contract, and focused verification.

## Runtime Boundary

One `Application<P>` owns one logical target session:

```text
external owner
  | mount / inspect / run / shutdown
  v
Application<P>
  |-- mounted Component tree and retained Signal state
  |-- private FrameSession
  |     canonical history, projection/diff baseline, staged inputs
  |-- Component task supervisor and coalesced reaction requests
  `-- one fixed P: ReactionPort
          Frame -> Provider, Skill, or Plugin target
          ordered ProviderFact stream -> canonical admission -> handlers
```

`Application::run()` is the default fixed driver. An integration that needs
manual scheduling can call `react()` for one reaction at a time. Mounting,
reading state, writing a `Signal`, completing a Component task, or requesting
driver attention does not render another projection or submit a Frame by
itself.

The public lifecycle is:

1. `Application::mount` declares the fixed target, mounts an ordinary Component
   root, and commits its initial complete projection. It does not submit.
2. `current_projection()` returns a read-only view of the latest committed,
   complete Component projection, its revision, and whether newer Component
   state is waiting to be reconciled.
3. `run().await` is the default fixed driver. It repeatedly runs Component
   preparation and reactions until normal exit returns an `ExitReason`, or the
   first `ApplicationFault` occurs. It does not clean up business resources or
   call `shutdown()`.
4. `react().await` is the manual single-reaction interface. It returns
   `ControlFlow::Continue(())` after one reaction, or `Break(reason)` on normal
   application exit and prevents subsequent Frame submissions.
5. `wait_for_reaction_request().await` and `take_reaction_request()` optionally
   let a driver consume coalesced Component demand. They do not call `react()`.
6. `shutdown().await` consumes the `Application`, fences the mount, and drains
   Component-owned tasks.

## Component Authoring

Import the Component authoring surface from the prelude. Retained business
state lives in `Signal<T>`, and provider facts are handled declaratively with
`use_provider_event_handler`:

```rust,ignore
use agentview::component::{
    execution::{Application, ApplicationFault, DebugProviderPort},
    prelude::*,
};

#[derive(Clone)]
struct AppProps {
    task: String,
}

#[component]
fn application_root(props: AppProps) -> Component {
    let answer = use_signal(String::new);
    let answer_for_events = answer.clone();

    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let answer = answer_for_events.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                answer.set(text)?;
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    let current_answer = answer.with(Clone::clone).expect("mounted Signal");
    let task = props.task;
    view! {
        #[system_once]
        policy { "Answer the current task." }
        task { "{task}" }
        answer { "{current_answer}" }
    }
}

async fn run() -> Result<(), ApplicationFault> {
    let props = AppProps {
        task: "Summarize the release.".to_owned(),
    };
    let (port, capture) = DebugProviderPort::new();
    let mut application = Application::mount(
        move || application_root(props.clone()),
        port,
    )?;

    assert!(!application.current_projection().is_dirty());
    assert_eq!(application.react().await?, std::ops::ControlFlow::Continue(()));
    assert_eq!(capture.frame_snapshots().len(), 1);
    application.shutdown().await
}
```

The Debug port makes this lifecycle credential-free. This example deliberately
uses the manual single-step interface to inspect one Frame. A fixed application
instead retains the result from `run()`, cleans up its business resources, then
consumes the Application:

```rust,ignore
let drive_result = application.run().await;
// Stop and acknowledge application-owned business resources here.
let shutdown_result = application.shutdown().await;
let _reason = drive_result?;
shutdown_result?;
```

For text admission and state publication, see
[`signal_reaction`](examples/signal_reaction.rs).

### Preparation Before Model Work

Use `use_preparation` for work required before the next Frame: waiting for an
inbox item, loading account data, or checking a business prerequisite. The outer
owner only calls `application.run().await`; Components own readiness and call
`use_application_exit` when their work is complete.

[`support_preparation`](examples/support_preparation.rs) processes a support
inbox with real asynchronous channel and JSON file reads. It waits for a ticket,
loads its account in a newly mounted child Component, and loads a session policy
in another Component. No Frame is submitted until every preparation succeeds.
Normal reaction completion stores a reply draft; closing the drained inbox
ends the application without an extra model request.

The runnable support example uses the live OpenAI Responses provider. Its
deterministic provider fixtures live under `tests/examples`; see [the
walkthrough](docs/preparation-examples.md) for the data flow, caching, retry
behavior, and how these boundaries map to services.

### Business History

Render complete business state as typed POM. Use `#[diff(slot = "state")]` to
declare a stable diff boundary, `#[view(diff)]` for changing fields, and
`#[view(diff(append))]` for growing business records. The Frame compiler selects
complete values, supported deltas, or omission from its retained baseline.

Declare native tools with `#[tool]` and mount them using `NativeToolCall::new(add)`.
Each tool Component declares all calls and results from its latest two tool
interaction rounds, also retaining pending rounds. The runtime merges those
records into canonical conversation history without
resubmitting them on re-render. The port owns provider encoding, history compaction,
and private session state. Raw handlers can still use `NativeToolCall::named(...).on_call(...)`.

See [HELP.md: Business History](HELP.md#business-history) for code, delta
requirements, native tool usage, and the established tree fold rules.

### Streaming XML

Use `StreamingXml::tag` when a Component only needs lifecycle events for one
tag from the provider text stream. The subscription is prompt-free: it does not
render instructions or example XML into the Component projection.

```rust,ignore
StreamingXml::tag("speak")
    .on_open(handle_open)
    .on_stream(handle_cumulative_content)
    .on_complete(handle_complete)
    .on_invalid(handle_invalid)
```

Use `XmlStreamingToolCall::new::<Channels>(...)` for a strict typed streaming
attempt. One contract declares every top-level action element that shares state,
cardinality, and one final decision; it owns an independent parser for each
provider reaction. Separate contracts may independently parse the same selected
provider text. The legacy `XmlStreamingToolCall::contract` and `StreamingXml`
surfaces remain parser-hub compatibility APIs.

## Frame And Reaction Semantics

Components remain provider-neutral and render complete state. `#[diff]` marks
semantic regions, but it does not make Components produce patches. The private
Frame compiler owns reconciliation, the accepted diff baseline, canonical
history, budget checks, and Full versus `DeltaFrom` selection.

A `ReactionPort` declares stable target identity, continuity, profile, and
limits, then accepts one exact move-only `Frame`:

```rust,ignore
#[async_trait]
pub trait ReactionPort: Send {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault>;

    async fn submit<'a>(
        &'a mut self,
        frame: Frame,
    ) -> Result<ProviderFactStream<'a>, SubmitFault>;
}
```

The port checks the Frame handoff precondition at the crossing poll. A
successful submit handoff commits the Application's outbound canonical input,
Frame revision, and diff baseline synchronously before any returned fact is
observed. Provider facts are validated in stream order, and their canonical
effects are admitted before matching Component handlers run. Handler failure
does not roll back facts that were already admitted.

Provider wire continuation, response identifiers, prompt-cache artifacts,
wire compaction, HTTP payloads, and tokenizer limits remain private to the
port. A port cannot mutate shared canonical history or the retained Component
projection. The repository includes native Responses, Chat Completions, Debug,
and External `ReactionPort` implementations.

## Live Examples

Runnable examples use the OpenAI Responses API, so `cargo run` makes
token-bearing requests. Set `OPENAI_API_KEY` in the environment or `.env`.
`AGENTVIEW_MODEL` defaults to `gpt-5.6-terra`; `OPENAI_BASE_URL` defaults to
`https://api.openai.com/v1`.

When present, `.env` entries override existing values with the same name,
including Cargo-injected certificate defaults. Without `.env`, the examples use
the launching process environment unchanged.

Repository convention: executable examples use configured real credentials and
demonstrate end-to-end behavior. Test implementations and mock fixtures belong
under `tests/`; example files retain only `#[cfg(test)]` module links to them.

`OPENAI_BASE_URL` is the versioned API base and must end in `/v1`; do not set
it to a `/responses` endpoint. For a gateway with a custom TLS certificate,
point `SSL_CERT_FILE` at its CA bundle:

```dotenv
OPENAI_BASE_URL=https://gateway.example/v1
SSL_CERT_FILE=/path/to/gateway-ca.crt
```

For the configured Lazycat gateway, use this base URL and the CA path visible
to the process. In a container:

```dotenv
OPENAI_BASE_URL=https://10.8.8.139:4000/v1
SSL_CERT_FILE=/opt/bootstrap/pki/lazycat-ai-gateway-ca.crt
```

On the host:

```dotenv
OPENAI_BASE_URL=https://10.8.8.139:4000/v1
SSL_CERT_FILE=/srv/xiaohei-agent/assets/pki/lazycat-ai-gateway-ca.crt
```

### Hello World

[`hello_world`](examples/hello_world.rs) runs one agent reaction, prints the
model's greeting, and exits:

```bash
cargo run --no-default-features --example hello_world
```

The other live examples use the same environment:

```bash
cargo run --no-default-features --example component_composition
cargo run --no-default-features --example frame_agent
cargo run --no-default-features --example frame_skill
cargo run --no-default-features --example frame_plugin
cargo run --no-default-features --example signal_reaction
cargo run --no-default-features --example native_tool
cargo run --no-default-features --example support_preparation
```

The examples start from Component roots and cover composition plus different
driver behavior over the same runtime:

- [`component_composition`](examples/component_composition.rs): several
  business Components contribute ordered canonical input to one root.
- [`frame_agent`](examples/frame_agent.rs): a minimal `run()` loop; the Component
  publishes new state after the first reaction and exits after the second.
- [`frame_skill`](examples/frame_skill.rs): a typed command updates state, then
  `ExternalApplication::observe()` provides the explicit exchange.
- [`frame_plugin`](examples/frame_plugin.rs): one `ExternalApplication` per
  parent, driven by `observe()` and `act()` with independent session continuity
  and cleanup of every owner. Low-level ingress checks stay in tests.
- [`signal_reaction`](examples/signal_reaction.rs): native text facts update a
  Signal; `run()` submits the updated review state on the next Frame before the
  Component exits.
- [`native_tool`](examples/native_tool.rs): the live model calls `#[tool] add`
  mounted with `NativeToolCall::new(add)`, then receives the result and answers `42`.
- [`support_preparation`](examples/support_preparation.rs): inbox waiting,
  account and policy loading, reply drafts, and normal queue-drained exit using
  `use_preparation` and `run()`.

## Deterministic Tests

Test implementations and fixtures live under `tests/`, rather than being a
runtime provider choice in the examples. Examples with deterministic behavioral
coverage can be run without API credentials:

```bash
cargo test --no-default-features --example frame_agent
cargo test --no-default-features --example frame_plugin
cargo test --no-default-features --example signal_reaction
cargo test --no-default-features --example support_preparation
cargo test --no-default-features --test component_runtime_visual_acceptance
```

The ordinary Chess executable uses a live provider and Stockfish. Its
deterministic tests and credential-free check/help paths are:

```bash
cargo test --no-default-features --example chess_agentview
cargo check --no-default-features --example chess_agentview
cargo run --no-default-features --example chess_agentview -- --help
```

[`chess_agentview`](docs/chess-runtime-target.md) uses one native
`Application<P>` with a Component-owned `Signal<ChessState>`. A pure reducer
owns legal moves, retries, terminal decisions, and turn policy; a retained
`use_coroutine` owns Stockfish, and `use_preparation` waits for its work before
the next model turn. One strict multi-element streaming
contract requires a nonempty `thought` before one `choose_move` or `resign`;
the top-level `play_chess` function mounts and runs the `Application`, then
waits for Stockfish cleanup before consuming it with `shutdown()`.
The Component owns preparation and requests normal exit after terminal engine
cleanup. Its test coverage uses reducer, scripted-provider, and fake-UCI
fixtures outside the runnable example.

## Live Chess Runs

The readable application example makes live model requests and starts
Stockfish. Configure `OPENAI_API_KEY`, the optional provider/model settings,
and an absolute `AGENTVIEW_STOCKFISH_BIN` when Stockfish is not installed at
`/usr/games/stockfish`:

```bash
export AGENTVIEW_STOCKFISH_BIN=/absolute/path/to/stockfish
cargo run --no-default-features --example chess_agentview
```

The separate production-evidence target contains deterministic coverage and an
ignored live game test for JSONL, usage correlation, deadlines, cleanup
arbitration, and post-terminal validation:

```bash
cargo test --no-default-features --test chess_agentview_live_acceptance
# Intentionally execute the live game after configuring credentials and Stockfish:
cargo test --no-default-features --test chess_agentview_live_acceptance -- --ignored --exact live_acceptance
```

See [`docs/chess-runtime-target.md`](docs/chess-runtime-target.md) for the exact
environment variables, ownership boundary, UCI lifecycle, and evidence
contract. Of these acceptance-test commands, only the ignored test sends live
Chess requests.

## Compatibility

The curated native surface is selected with `--no-default-features`.

For one minor release, the default-enabled `legacy-provider-port` feature
retains deprecated compatibility APIs: `ProviderPort`, `ApplicationHost`,
`ComponentReactionRuntime`, `EventInput`, `EventListener`, and the old
event-taking Host constructors. They are not the current architecture. New
code should use `Application<P>`, `ReactionPort`, ordinary Component roots, and
`use_provider_event_handler`.

`Application<P>` is an in-process owner, not a durable workflow or checkpoint.
The runtime does not impose one universal business loop: a Component may own
policy and retained state, as in Chess, while Skill or Plugin integrations may
keep invocation policy in an external adapter. `run()` is the default driver;
an external adapter controls scheduling only when it intentionally calls
`react()` itself. The runtime does not expose mutable port, session, history,
or Component props access from an Application.

The detailed runtime contract is in [`docs/engine.md`](docs/engine.md).

## Validation

Run the documentation and feature gates serially:

```bash
cargo test --doc
cargo check --examples --all-features
cargo check --examples --no-default-features
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --all-features
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --no-default-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --workspace --all-targets --no-default-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```
