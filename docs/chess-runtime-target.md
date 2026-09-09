# AgentView Chess Examples

Last reviewed: 2026-09-08

The current runtime description is [Chess loop explanation](chess-loop-explained.md). The checked-in [workflow diagram](chess-loop.workflow.html) and [sequence diagram](chess-loop.sequence.html) still depict the retired demand-driven driver and are historical until regenerated.

The repository exposes two Chess targets with different jobs:

- [`chess_agentview`](../examples/chess_agentview/main.rs) is the readable application example. The model plays White, Stockfish plays Black, and the mounted Component owns the game workflow.
- [`chess_agentview_live_acceptance`](../tests/acceptance/chess_agentview/main.rs) is the production-evidence integration test. Its live game is ignored by default; the target also retains deterministic coverage for bounded deadlines, JSONL trace, provider-usage correlation, cleanup arbitration, and terminal evidence validation that would obscure the ordinary API example.

## Application Boundary

```text
thin chess_agentview binary
  |-- build provider and immutable ChessConfig
  `-- play_chess(config, provider)
        |-- create stop, result, and cleanup watch channels
        |-- Application::mount(chess_application(...), provider)
        |     `-- mounted chess_application Component
        |           |-- Signal<ChessState> (business authority)
        |           |-- root props: ChessConfig, stop receiver, result sender,
        |           |   and cleanup sender
        |           |-- pure reduce(ChessState, ChessEvent) -> ChessReduction
        |           |-- chess_action_component
        |           |   `-- one multi-element XmlStreamingToolCall attempt
        |           |       |-- shared parser/state for thought, choose_move, and resign
        |           |       |-- accepted publication updates ChessState synchronously
        |           |       `-- rejected continuation updates corrective feedback
        |           |-- use_preparation is the Stockfish readiness barrier
        |           |-- use_application_exit requests normal terminal exit
        |           |-- use_coroutine owns the engine and handles preparation requests
        |           `-- private FrameSession and canonical history
        |-- application.run() uses the default react loop
        |-- stop_stockfish(...) confirms UCI cleanup
        |-- application.shutdown() consumes the Application
        `-- finish_run(...) joins result, cleanup, and shutdown failures
```

`ChessState` owns the authoritative board, committed move history, retry count, feedback, phase, and terminal outcome. Model actions carry a typed attempt key, so duplicate or late actions cannot consume the next attempt. The actor takes one owned snapshot of committed history for each Stockfish search.

The reducer owns Chess policy. It validates actions, applies legal moves, enforces the model retry budget, selects the next side, and detects terminal positions and automatic draw conditions. It returns `Applied` when it consumes an event, including a rejected current model action, or `Ignored` for stale or inapplicable events. The actor reads the next phase and terminal outcome directly from state; there is no separate effect queue. Rejection details live in feedback, alongside the retry count in state.

The long-lived Component coroutine owns `UciEngine` and serializes preparation requests. On `Ready` it applies `Start`; on `AwaitingStockfish` it reads the committed position, runs one engine move, and feeds the result through the reducer; on `AwaitingModel` it only acknowledges readiness. For a terminal request with a live engine, it shuts the engine down, publishes `ChessResult` only when that cleanup succeeds, publishes the cleanup status, acknowledges the preparation request, and exits. Startup failures that leave no child to clean up publish the terminal result and successful cleanup; an unconfirmed child cleanup publishes `ChessActorFailure::StockfishCleanup` without a result. Model actions are committed directly to the same Signal in one synchronous update, without waiting for the engine.

`chess_action_component` mounts one prompt-producing `XmlStreamingToolCall::new` attempt with `thought`, `choose_move`, and `resign` element schemas. Each model reaction creates one parser, one reducer state value, and one terminal decision for all three elements. A response must contain exactly one nonempty `<thought>` text element, then exactly one self-closing `<choose_move uci="..." />` or `<resign />` element. The thought is a concise move evaluation; no text may surround the pair, and foreign XML is rejected by the contract-local strict parser.

The strict attempt parser is independent of other components and contracts. The prompt-free `StreamingXml::tag(...)` API remains available for permissive lifecycle subscriptions without adding a strict action shape to the prompt.

The two targets intentionally use different response contracts. The readable example requires its ordered thought and action pair. The live acceptance harness retains its independent whole-output parser for one empty action element; outside that action, only whitespace is accepted.

The attempt final reducer runs after all provider events, derived XML events, and normal-EOF diagnostics. It settles the shared attempt state exactly once:

- a missing thought becomes `MissingThought`;
- an empty, malformed, repeated, or late thought becomes its typed thought rejection;
- one valid thought without an action becomes `MissingAction`;
- one valid thought followed by one valid action becomes the staged accepted action;
- more than one action becomes `MultipleActions`.

An accepted final decision applies its attempt-keyed `ChessEvent::ModelAction` synchronously. An attempt-local journal records Published or NotPublished, so repeating publication does not repeat the state transition and stale attempts cannot consume a newer turn. Publication returns before any Stockfish search. The rejection continuation applies its typed failure directly and returns `Complete`; the next fixed `react()` call owns any successor provider frame.

## Driver And Props

`play_chess(config, provider)` is the ordinary example's free async owner of `Application<P>`. It creates a stop `watch::Sender<bool>`, a result `watch::Sender<Option<ChessResult>>`, and a cleanup `watch::Sender<Option<Result<(), ChessActorFailure>>>`, then mounts the root with `ChessConfig`, the stop receiver, and those two senders. Mutable Chess business state is initialized with `use_signal`; it is not stored in `Application` props or exported through an external `ChessControl`.

```rust,ignore
let result = application
    .run()
    .await
    .context("Chess model reaction failed")
    .and_then(|_| {
        result_receiver
            .borrow()
            .clone()
            .context("Chess application exited before producing a terminal result")
    });
let cleanup = stop_stockfish(&stop, &mut cleanup_receiver).await;
let shutdown = application.shutdown().await;
finish_run(result, cleanup, shutdown)
```

`Application::run()` repeatedly calls `react()` until normal exit returns an `ExitReason` or a reaction returns a fault. It borrows the Application and leaves shutdown to `play_chess`. After `run()` returns, `play_chess` reads the terminal result channel, invokes `stop_stockfish` to await UCI cleanup, consumes the Application with `shutdown()`, and lets `finish_run` preserve both operation and cleanup failures. Direct `react()` remains available for manual single steps.

`react()` runs preparation before it constructs a provider frame. The root `use_preparation` hook checks cleanup status before sending a request, then checks it again after the send/reply operation before propagating any error. Successful cleanup requests `ExitReason::Completed` only when the result channel contains `Some(ChessResult)`; a `ChessActorFailure`, or successful cleanup without a result, is a preparation failure. With no cleanup status, it permits a frame only when the phase is `AwaitingModel`. `Break` therefore means there is no active reaction and no further provider submission.

There is no `use_reaction_request` and no `requested_attempt` in this example. A successful outer `react()` can submit one frame; the next outer `react()` either retries the model or advances Stockfish during its preparation phase. Repeated preparation and a cancelled preparation do not duplicate Stockfish because the actor checks the current phase before each request.

After every `run()` outcome, `play_chess` requests the actor stop and awaits its bounded UCI cleanup result before `shutdown()`. A normal terminal actor has already published cleanup success, so that wait returns immediately. An external `Requested` exit without a published `ChessResult` is an error, followed by the same stop, cleanup, and shutdown sequence rather than being interpreted as a game result.

## Ordinary Live Run

The ordinary example makes live OpenAI Responses requests and starts a local
Stockfish process. Configure variables in the process environment or the
ignored `.env` file:

| Variable | Requirement |
| --- | --- |
| `OPENAI_API_KEY` | Required. |
| `OPENAI_BASE_URL` | Optional versioned API base ending in `/v1`; defaults to `https://api.openai.com/v1`. Do not append `/responses`. |
| `SSL_CERT_FILE` | Optional CA bundle path for a custom TLS certificate. |
| `AGENTVIEW_MODEL` | Optional; defaults to `gpt-5.6-terra`. |
| `AGENTVIEW_STOCKFISH_BIN` | Optional absolute Stockfish executable path; defaults to `/usr/games/stockfish`. |
| `AGENTVIEW_CHESS_PLY_LIMIT` | Optional positive integer; bounds the ordinary example game. |
| `AGENTVIEW_ENGINE_NODES` | Optional positive integer; controls Stockfish work per move. |
| `AGENTVIEW_ENGINE_TIMEOUT_SECS` | Optional positive integer; bounds engine setup, work, and cleanup. |

For a gateway with a custom CA, set its API base and CA bundle before running:

```bash
export OPENAI_BASE_URL=https://gateway.example/v1
export SSL_CERT_FILE=/path/to/gateway-ca.crt
```

The [live example configuration in the README](../README.md#live-examples)
includes the configured Lazycat gateway paths for container and host processes.

Run it with:

```bash
# Set this when Stockfish is not installed at /usr/games/stockfish.
export AGENTVIEW_STOCKFISH_BIN=/absolute/path/to/stockfish
cargo run --no-default-features --example chess_agentview
```

The terminal output reports only the business result: outcome, final FEN, and committed moves. UCI commands, bounded-read checks, shutdown evidence, and provider usage belong to the separate acceptance target.

## Live Acceptance Test

`chess_agentview_live_acceptance` is an integration test target. Its normal
invocation runs the deterministic evidence coverage and skips the ignored live
game:

```bash
cargo test --no-default-features --test chess_agentview_live_acceptance
```

To intentionally execute the live game, configure `OPENAI_API_KEY`, the same
optional `OPENAI_BASE_URL`, `SSL_CERT_FILE`, `AGENTVIEW_MODEL`, and absolute
`AGENTVIEW_STOCKFISH_BIN` values, then run:

```bash
cargo test --no-default-features --test chess_agentview_live_acceptance -- --ignored --exact live_acceptance
```

Its reaction, engine, whole-game, and evidence limits are intentionally fixed
by the acceptance harness rather than by the ordinary example's tuning
variables.

When the live test runs, the harness writes a bounded trace to
`target/agentview-chess-live-<run-id>.jsonl`. Schema-versioned events cover run
configuration, turns, model attempts, provider usage when reported, Stockfish
work, committed moves, terminal outcome, and cleanup evidence. It emits a
terminal summary only after post-terminal consistency validation. Incomplete
runs attempt to remove their trace artifact.

The trace and status output exclude credentials, raw model reasoning, complete provider requests, and provider-private continuation state.

## Deterministic Tests And Checks

Test implementations and fixture providers live under
[`tests/examples/chess_agentview`](../tests/examples/chess_agentview) and
[`tests/acceptance/chess_agentview`](../tests/acceptance/chess_agentview).
Reducer tests, scripted-provider tests, fake-UCI lifecycle tests, retry paths,
stale-attempt rejection, terminal idempotence, cleanup, and panic priority run
without credentials, network access, or an installed Stockfish binary. The
ignored live acceptance game is not included in these commands:

```bash
cargo test --no-default-features --example chess_agentview
cargo test --no-default-features --test chess_agentview_live_acceptance
cargo check --no-default-features --example chess_agentview
cargo check --no-default-features --test chess_agentview_live_acceptance
cargo run --no-default-features --example chess_agentview -- --help
```

The help path exits before provider, credential, or UCI setup. Fixtures are
verification infrastructure; `cargo run --example chess_agentview` always uses
the live provider.

## Source Map

- [`main.rs`](../examples/chess_agentview/main.rs): direct `play_chess` driver, result extraction, cleanup, and shutdown.
- [`chess_application.rs`](../examples/chess_agentview/chess_application.rs): `ChessConfig`, Component root, actor, and projection.
- [`chess_action_component.rs`](../examples/chess_agentview/chess_action_component.rs): one multi-element streaming XML attempt, final cardinality settlement, and synchronous state publication/rejection adapters.
- [`chess_action.rs`](../examples/chess_agentview/chess_action.rs): typed Chess actions shared by the readable example and acceptance test.
- [`application_state.rs`](../examples/chess_agentview/application_state.rs): authoritative state, typed events/effects, and reducer; focused tests live in [`tests/examples/chess_agentview/application_state.rs`](../tests/examples/chess_agentview/application_state.rs).
- [`uci.rs`](../examples/chess_agentview/uci.rs): bounded UCI process adapter.
- [`live_provider.rs`](../examples/chess_agentview/live_provider.rs): ordinary live-run environment and provider construction.
- [`main.rs`](../tests/acceptance/chess_agentview/main.rs), [`chess_actions.rs`](../tests/acceptance/chess_agentview/chess_actions.rs), [`game.rs`](../tests/acceptance/chess_agentview/game.rs), [`live.rs`](../tests/acceptance/chess_agentview/live.rs), and [`observability.rs`](../tests/acceptance/chess_agentview/observability.rs): acceptance machinery linked by the `chess_agentview_live_acceptance` test target, not by the ordinary binary.

The shared runtime protocol remains specified by [`engine.md`](engine.md).
