# agentview

`agentview` is a Component runtime for rendering typed application state into
provider-neutral Frames and admitting ordered model facts back into
application-owned state.

## Runtime Boundary

One `Application<P>` owns one logical target session:

```text
external driver
  | mount / inspect / react / shutdown
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

The external driver decides when a reaction happens. Mounting, reading state,
writing a `Signal`, completing a Component task, or requesting driver attention
does not render another projection or submit a Frame by itself.

The public lifecycle is:

1. `Application::mount` declares the fixed target, mounts an ordinary Component
   root, and commits its initial complete projection. It does not submit.
2. `current_projection()` returns a read-only view of the latest committed,
   complete Component projection, its revision, and whether newer Component
   state is waiting to be reconciled.
3. `react().await` reconciles dirty state and completes exactly one explicit
   reaction. It hands off at most one compiler-produced Full or Delta Frame.
4. `wait_for_reaction_request().await` and `take_reaction_request()` optionally
   let a driver consume coalesced Component demand. They do not call `react()`.
5. `shutdown().await` consumes the `Application`, fences the mount, and drains
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
    application.react().await?;
    assert_eq!(capture.frame_snapshots().len(), 1);
    application.shutdown().await
}
```

The Debug port makes this lifecycle credential-free. For text admission and
state publication, see [`signal_reaction`](examples/signal_reaction.rs).

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

Use `XmlStreamingToolCall::contract` for a typed XML action declaration. It
projects model-visible example syntax, decodes matching empty elements, and
reports contract diagnostics. Both APIs register with the same parser hub for
the mounted provider-text route, so matching events retain XML source order and
each async handler is awaited before the next event is dispatched.

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

## Offline Examples

These commands require no credentials or external network access:

```bash
cargo run --no-default-features --example hello_world
cargo run --no-default-features --example frame_agent
cargo run --no-default-features --example frame_skill
cargo run --no-default-features --example frame_plugin
cargo run --no-default-features --example signal_reaction
cargo run --no-default-features --example provider_port_visual_acceptance
```

The executables prove different driver behavior over the same runtime:

- [`frame_agent`](examples/frame_agent.rs): Component demand wakes the external
  driver for a later Full-to-Delta reaction after state publication.
- [`frame_skill`](examples/frame_skill.rs): latest-read and a typed command stay
  passive; only an explicit exchange observes the dirty state.
- [`frame_plugin`](examples/frame_plugin.rs): one `Application` is owned per
  parent; stale ingress is fenced, same-parent continuity is retained, and all
  owners are consumed during cleanup.
- [`signal_reaction`](examples/signal_reaction.rs): native text facts update a
  Signal, while the updated state reaches the target only on the second
  explicit reaction.
- [`provider_port_visual_acceptance`](examples/provider_port_visual_acceptance.rs):
  a loopback Responses server validates complete Component state, Full/Delta
  lowering, provider-private wire history, fresh-target Full behavior, and
  cleanup. It ends with `ACCEPTANCE PASSED` only after every assertion passes.

The ordinary Chess executable has deterministic offline tests and a
credential-free help path:

```bash
cargo test --no-default-features --example chess_agentview
cargo check --no-default-features --example chess_agentview
cargo run --no-default-features --example chess_agentview -- --help
```

[`chess_agentview`](docs/chess-runtime-target.md) uses one native
`Application<P>` with a Component-owned `Signal<ChessState>`. A pure reducer
owns legal moves, retries, terminal decisions, and turn policy; a retained
`use_coroutine` actor owns Stockfish. The thin `ChessApplication` driver only
mounts, waits for Component demand, completes requested reactions, observes the
terminal result, and shuts down. Offline verification uses reducer, scripted
port, and fake-UCI fixtures without an API key or installed Stockfish.

## Paid Chess Runs

The readable application example makes paid model requests and starts
Stockfish:

```bash
cargo run --no-default-features --example chess_agentview
```

Configure `OPENAI_API_KEY` and the optional provider/model/Stockfish variables
first. The ordinary example also accepts optional ply, engine-node, and engine
timeout limits.

Run the separate production-evidence harness when JSONL, usage correlation,
deadlines, cleanup arbitration, and post-terminal validation are required:

```bash
cargo run --no-default-features --example chess_agentview_live_acceptance
```

See [`docs/chess-runtime-target.md`](docs/chess-runtime-target.md) for the exact
environment variables, ownership boundary, UCI lifecycle, and evidence
contract. Neither paid command is part of credential-free validation.

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
keep invocation policy in an external adapter. The external driver still owns
when `react()` is called, and the runtime does not expose mutable port, session,
history, or Component props access from an Application.

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
