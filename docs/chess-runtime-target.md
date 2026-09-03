# AgentView Chess Examples

Last reviewed: 2026-09-03

The repository exposes two Chess binaries with different jobs:

- [`chess_agentview`](../examples/chess_agentview/main.rs) is the readable
  application example. The model plays White, Stockfish plays Black, and the
  mounted Component owns the game workflow.
- [`chess_agentview_live_acceptance`](../examples/chess_agentview_live_acceptance/main.rs)
  is the paid production-evidence harness. It retains the bounded deadlines,
  JSONL trace, provider-usage correlation, cleanup arbitration, and terminal
  evidence validation that would obscure the ordinary API example.

## Application Boundary

```text
thin chess_agentview binary
  |-- build provider and immutable ChessApplicationConfig
  `-- ChessApplication::mount(config, provider)?.run().await
        |-- Application<P>
        |     |-- mounted chess_application Component
        |     |     |-- Signal<ChessState> (business authority)
        |     |     |-- pure reduce(ChessState, ChessEvent) -> ChessEffect
        |     |     |-- chess_action_component
        |     |     |     |-- two sibling XmlStreamingToolCall Components
        |     |     |     |-- reaction-local action-result collector
        |     |     |     `-- use_reaction_completion sends one ChessAttemptInput
        |     |     |-- use_coroutine actor owning Stockfish
        |     |     `-- use_reaction_request for the next model turn
        |     |-- private FrameSession and canonical history
        |     `-- fixed Responses ReactionPort
        |-- mechanical demand -> react loop
        `-- typed actor exit/stop handshake and consuming shutdown
```

`ChessState` owns the authoritative board, committed move history, retry state,
feedback, phase, and terminal outcome. Model actions carry a typed
attempt key, so a duplicate or late action cannot consume the next
attempt. Stockfish requests carry the committed position revision and history.

The reducer owns Chess policy. It validates actions, applies legal moves,
enforces the model retry budget, selects the next side, detects terminal
positions and automatic draw conditions, and emits one of three effects:

- request another model reaction;
- request a Stockfish move;
- publish the terminal outcome.

The long-lived Component coroutine serializes those effects. It starts and
owns `UciEngine`, reads the authoritative history from reducer effects, feeds
Stockfish results back as typed events, and shuts the engine down before
publishing a normal terminal result.

`chess_action_component` mounts two prompt-producing, typed
`XmlStreamingToolCall` declarations: `choose_move` and `resign`.
They register with one parser hub for the mounted reaction's provider text route;
the example passes neither a route nor an `EventInput`. Every matching element
occurrence is dispatched, including repeated occurrences and occurrences for
multiple sibling declarations. The runtime preserves their XML source order
and awaits each async handler before starting the next one.

The prompt-free `StreamingXml::tag(...)` API uses that same per-route hub when
a Component needs `on_open`, cumulative `on_stream`, `on_complete`, or
`on_invalid` lifecycle events without adding another action shape to the prompt.

The two binaries share the same action names and XML shapes, but intentionally
use different surrounding-text policies. The readable example counts registered
action occurrences and allows text outside exactly one action. The live
acceptance harness retains its stricter whole-output parser: outside its one
empty action element, only whitespace is accepted.

Each decoded or invalid occurrence appends one typed result to a collector owned
by that rendered reaction. After all provider events, derived XML events, and
normal-EOF diagnostics have been handled, `use_reaction_completion` settles the
collector exactly once:

- no registered action becomes `MissingAction`;
- one occurrence keeps its decoded action or validation error;
- more than one occurrence becomes `MultipleActions`.

The completion callback sends that single attempt-keyed `ChessAttemptInput`
into the Component-owned FIFO coroutine and awaits its handling receipt. The
coroutine lowers the input to `ChessEvent::ModelAction`, runs the reducer and
its immediate effect chain, then acknowledges the callback. The XML Components
still do not own the board, legality policy, retry loop, or scheduling policy.

The actor is event-driven, not self-ticking. `Start` requests the first model
reaction; every later transition requires a `ChessEvent`. The completion hook
is what turns an otherwise eventless model response, including an empty
reaction, prose, or unknown XML, into the typed `MissingAction` event instead of
leaving the actor blocked on its inbox.

`MissingAction` and `MultipleActions` follow the same reducer-owned corrective
path as other invalid model actions. `ChessState.feedback` records the rejection,
the next Frame exposes it through `previous_decision`, `previous_reason`, and
`corrective_reason`, and the actor requests another reaction. The third
consecutive rejected attempt completes with `ModelForfeit`.

If the provider or driver fails, the facade requests actor stop and waits for
the actor's bounded UCI cleanup result before consuming `Application`.

## Driver And Props

The immutable startup values are passed through the root closure as ordinary
Component arguments. Mutable Chess business state is initialized with
`use_signal`; it is not stored in `Application` props or exported through an
external `ChessControl`.

`ChessApplication` is a small example-local facade over `Application<P>`. Its
driver waits for either terminal completion or a Component reaction request.
After a request it awaits exactly one complete `react()` call. The loop contains
no Chess policy:

```rust,ignore
loop {
    tokio::select! {
        changed = completion.changed() => changed?,
        demand = application.wait_for_reaction_request() => {
            demand?;
            application.react().await?;
        }
    }
}
```

An admitted `react()` is allowed to finish before terminal completion is
observed. Cancelling it after provider handoff would terminally cancel the
underlying `Application`. `ChessApplication::run` always consumes that runtime
with `shutdown().await` before returning. Its stop channel carries lifecycle
only; it cannot mutate `ChessState`, choose a move, schedule a retry, or access
the UCI process.

## Ordinary Paid Run

The ordinary example makes paid OpenAI Responses requests and starts a local
Stockfish process. Configure variables in the process environment or the
ignored `.env` file:

| Variable | Requirement |
| --- | --- |
| `OPENAI_API_KEY` | Required. |
| `OPENAI_BASE_URL` | Optional; defaults to `https://api.openai.com/v1`. |
| `AGENTVIEW_MODEL` | Optional; defaults to `gpt-5.6-terra`. |
| `AGENTVIEW_STOCKFISH_BIN` | Optional absolute path; defaults to `/usr/games/stockfish`. |
| `AGENTVIEW_CHESS_PLY_LIMIT` | Optional positive integer; bounds the ordinary example game. |
| `AGENTVIEW_ENGINE_NODES` | Optional positive integer; controls Stockfish work per move. |
| `AGENTVIEW_ENGINE_TIMEOUT_SECS` | Optional positive integer; bounds engine setup, work, and cleanup. |

Run it with:

```bash
cargo run --no-default-features --example chess_agentview
```

The terminal output reports only the business result: outcome, final FEN, and
committed moves. UCI commands, bounded-read checks, shutdown evidence, and
provider usage belong to the separate acceptance target.

## Paid Live Acceptance

Run the separate evidence target when validating the production harness:

```bash
cargo run --no-default-features --example chess_agentview_live_acceptance
```

This target uses `OPENAI_API_KEY` and the same optional `OPENAI_BASE_URL`,
`AGENTVIEW_MODEL`, and `AGENTVIEW_STOCKFISH_BIN` values. Its reaction, engine,
whole-game, and evidence limits are intentionally fixed by the acceptance
harness rather than by the ordinary example's tuning variables.

The harness writes a bounded trace to
`target/agentview-chess-live-<run-id>.jsonl`. Schema-versioned events cover run
configuration, turns, model attempts, provider usage when reported, Stockfish
work, committed moves, terminal outcome, and cleanup evidence. It emits a
terminal summary only after post-terminal consistency validation. Incomplete
runs attempt to remove their trace artifact.

The trace and status output exclude credentials, raw model reasoning, complete
provider requests, and provider-private continuation state.

## Offline Verification

Compilation, reducer tests, scripted provider tests, fake-UCI lifecycle tests,
retry paths, stale-attempt rejection, terminal idempotence, cleanup, and panic
priority can be checked without credentials, network access, or an installed
Stockfish binary:

```bash
cargo test --no-default-features --example chess_agentview
cargo test --no-default-features --example chess_agentview_live_acceptance
cargo check --no-default-features --example chess_agentview
cargo check --no-default-features --example chess_agentview_live_acceptance
cargo run --no-default-features --example chess_agentview -- --help
cargo run --no-default-features --example chess_agentview_live_acceptance -- --help
```

The help paths exit before provider, credential, or UCI setup. Offline fixtures
are verification infrastructure; they are not an unauthenticated production
provider configuration.

## Source Map

- [`chess_application.rs`](../examples/chess_agentview/chess_application.rs):
  facade, Component root, actor, mechanical driver, and projection.
- [`chess_action_component.rs`](../examples/chess_agentview/chess_action_component.rs):
  two streaming XML action Components, reaction-local collection, and
  cardinality settlement at normal reaction completion.
- [`chess_action.rs`](../examples/chess_agentview/chess_action.rs): typed Chess
  actions shared by the readable example and acceptance binary.
- [`application_state.rs`](../examples/chess_agentview/application_state.rs):
  authoritative state, typed events/effects, reducer, and focused tests.
- [`uci.rs`](../examples/chess_agentview/uci.rs): bounded UCI process adapter.
- [`live_provider.rs`](../examples/chess_agentview/live_provider.rs): ordinary
  paid-run environment and provider construction.
- [`chess_actions.rs`](../examples/chess_agentview_live_acceptance/chess_actions.rs),
  [`game.rs`](../examples/chess_agentview_live_acceptance/game.rs),
  [`live.rs`](../examples/chess_agentview_live_acceptance/live.rs), and
  [`observability.rs`](../examples/chess_agentview_live_acceptance/observability.rs): heavy
  acceptance machinery linked by `chess_agentview_live_acceptance`, not by the
  ordinary binary.

The shared runtime protocol remains specified by [`engine.md`](engine.md).
