# AgentView Chess Examples

Last reviewed: 2026-09-07

Detailed walkthrough: [Chess loop explanation](chess-loop-explained.md),
[workflow diagram](chess-loop.workflow.html), and
[sequence diagram](chess-loop.sequence.html).

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
        |     |     |     `-- one multi-element XmlStreamingToolCall attempt
        |     |     |         |-- shared parser/state for thought, choose_move, and resign
        |     |     |         |-- accepted publication updates ChessState synchronously
        |     |     |         `-- rejected continuation updates corrective feedback
        |     |     |-- use_preparation awaits the Stockfish readiness barrier
        |     |     |-- use_coroutine owns the engine and handles preparation requests
        |     |     `-- use_reaction_request once per prepared model attempt
        |     |-- private FrameSession and canonical history
        |     `-- fixed Responses ReactionPort
        |-- mechanical prepare -> completion/demand -> react loop
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

The long-lived Component coroutine owns `UciEngine` and serializes preparation
requests. `use_preparation` awaits its reply before the Component is ready.
On the first request it applies `Start`; when the state is `AwaitingStockfish`,
it reads the committed position, awaits one engine move, and feeds the result
back through the reducer. At a terminal state it shuts the engine down before
publishing completion. Model actions are committed directly to the same Signal
in one synchronous update, without waiting for the engine.

`chess_action_component` mounts one prompt-producing
`XmlStreamingToolCall::new` attempt with `thought`, `choose_move`, and `resign`
element schemas. Each model reaction creates one parser, one reducer state
value, and one terminal decision for all three elements. A response must contain
exactly one nonempty `<thought>` text element, then exactly one self-closing
`<choose_move uci="..." />` or `<resign />` element. The thought is a concise
move evaluation, not unconstrained prose: no text may surround the two elements,
and foreign XML is rejected by the contract-local strict parser.

The strict attempt parser is independent of other components and contracts.
The prompt-free `StreamingXml::tag(...)` API remains available for permissive
lifecycle subscriptions without adding a strict action shape to the prompt.

The two binaries intentionally use different response contracts. The readable
example requires its ordered thought and action pair. The live acceptance
harness retains its independent whole-output parser for one empty action
element; outside that action, only whitespace is accepted.

The attempt final reducer runs after all provider events, derived XML events,
and normal-EOF diagnostics. It settles the shared attempt state exactly once:

- a missing thought becomes `MissingThought`;
- an empty, malformed, repeated, or late thought becomes its typed thought
  rejection;
- one valid thought without an action becomes `MissingAction`;
- one valid thought followed by one valid action becomes the staged accepted
  action;
- more than one action becomes `MultipleActions`.

An accepted final decision applies its attempt-keyed `ChessEvent::ModelAction`
synchronously. An attempt-local journal records Published or NotPublished, so
repeating publication does not repeat the state transition and stale attempts
cannot consume a newer turn. Publication returns before any Stockfish search.
The rejection continuation applies its typed failure directly and returns
`Complete`; all successor requests belong to preparation.

After the streaming attempt has settled, the next preparation advances the
engine work and examines the resulting state. For `AwaitingModel` it requests
one reaction per `ModelAttemptKey`; for a terminal state it reports completion.
Repeated explicit preparation and the preparation inside `react()` do not
repeat a move or schedule the same attempt again. If a preparation waiter is
cancelled, the coroutine finishes its already-started work; the next request
observes the resulting phase instead of replaying it.

Thought and action rejections follow the same reducer-owned corrective path.
`ChessState.feedback` records the rejection, the next Frame exposes it through
`previous_decision`, `previous_reason`, and `corrective_reason`, and
preparation schedules the next allowed attempt. The third consecutive
rejected attempt completes with `ModelForfeit`.

If the provider or driver fails, the facade requests actor stop and waits for
the actor's bounded UCI cleanup result before consuming `Application`.

## Driver And Props

The immutable startup values are passed through the root closure as ordinary
Component arguments. Mutable Chess business state is initialized with
`use_signal`; it is not stored in `Application` props or exported through an
external `ChessControl`.

`ChessApplication` is a small example-local facade over `Application<P>`. Its
driver first awaits all Component preparations, then observes completion or
consumes one Component reaction request. It never inspects board phases or
chooses a move. The completion check after the barrier prevents an engine move
that ends the game from causing another provider submission:

```rust,ignore
loop {
    application.prepare().await?;
    if let Some(result) = completion.borrow().clone() {
        return completed_result(result);
    }
    tokio::select! {
        biased;
        changed = completion.changed() => {
            changed?;
            continue;
        }
        demand = application.wait_for_reaction_request() => demand?,
    }
    if let Some(result) = completion.borrow().clone() {
        return completed_result(result);
    }
    application.react().await?;
}
```

`prepare()` and `react()` each run preparation hooks. The Component's phase and
attempt guards make that safe; readiness is not cached across operations.
An admitted `react()` is awaited in full before completion is observed.
`ChessApplication::run` always consumes that runtime
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
  one multi-element streaming XML attempt, final cardinality settlement, and
  synchronous state publication/rejection adapters.
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
