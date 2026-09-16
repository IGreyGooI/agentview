# Example Prompts

## Callback Actions

`native_tool` and `streaming_callbacks` declare actions inside `view!`, with callbacks
capturing the same state rendered by their Component:

```sh
cargo run --locked --no-default-features --example native_tool
cargo run --locked --no-default-features --example streaming_callbacks
```

Both use the shared live provider configuration (`.env` or `OPENAI_API_KEY`). The native
example adjusts a counter, receives the actual updated value, then undoes the change.
The XML example streams and saves notes, exposes the saved count, and lets the model
confirm it before acknowledging the result. Only the action available in the current
notebook state is mounted. Ordinary XML callbacks run in the current reaction and need
no channel types or publisher; their effects are retained if later input is invalid.

## Interactive Chess CLI

`chess_cli` is a standalone Unix example (Linux/macOS) for any model that can
run a process and read/write stdin/stdout. A background daemon retains one
Application and one game across invocations. No model API or Stockfish is needed.

The binary starts the business root through the framework runner:

```rust
mod app;
mod game;

use agentview::component::execution::StdinApplication;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    StdinApplication::run(app::chess_application).await
}
```

`StdinApplication::run` owns CLI parsing, daemon discovery and lifetime, stdin/stdout,
the preparation loop, and cleanup. It supplies session and connection metadata;
the root Component declares the chess view and actions.

```sh
cargo build --locked --no-default-features --example chess_cli
# Observe. Repeated start calls retain the same game.
target/debug/examples/chess_cli start
# Business actions are JSON on stdin. This also starts a missing daemon.
target/debug/examples/chess_cli <<'EOF'
{"action":"move","input":{"uci":"e2e4"}}
{"action":"move","input":{"uci":"e7e5"}}
{"action":"undo","input":{}}
EOF
target/debug/examples/chess_cli status
target/debug/examples/chess_cli stop
```

The CLI controls the daemon; business actions are declared once inside `view!`:

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MoveInput {
    /// One canonical lowercase UCI token from the current legal_moves.
    uci: String,
}

view! {
    Action {
        name: "move",
        description: "Play one legal move for the current side",
        enabled: can_move,
        on_call: move |input: MoveInput| {
            played_game.update(|game| game.play(&input.uci))
        },
    }
    Action {
        name: "undo",
        description: "Undo the previous move",
        on_call: move || undone_game.update(GameState::undo),
    }
}
```

AgentView collects mounted actions, generates their JSON input schemas and action
help, decodes inputs, and invokes the current callback. There are no separate
command constants, parser catalogs, or empty argument structs. `enabled: false`
rejects the action with feedback before invoking its callback. Enabled callbacks
validate actual business state. A successful render
refreshes captures and available actions, and duplicate action names reject the
candidate before input is consumed.

| Invocation | Behavior |
| --- | --- |
| no subcommand | Read one JSON action per stdin line, automatically starting a missing daemon. Each action returns a complete view. Empty stdin observes; at a terminal the initial view appears before input. |
| `start` | Start or connect, then return the current view without resetting the game. |
| `status` | Return the running view, or report stopped without starting a daemon. |
| `stop` | Return the final view and stopped status after Application cleanup and socket removal. Already stopped is a successful no-op. |
| `restart` | Stop, start a fresh game, and return its view. The old game is discarded. |
| `help` | Explain the protocol and lifecycle commands without starting a daemon. |

stdout contains the callback's JSON result in `<action_result>`, followed by the
readable board, current FEN, legal moves, retained feedback, JSON input schemas,
and daemon status. Each response is enclosed in a
`<view ok="true|false">...</view>` block and flushed before reading the next action,
so a caller can keep stdin open and act on each result. Runtime diagnostics use stderr. An illegal
move, unknown action, invalid typed input, malformed JSON, or oversized input
produces feedback and still permits subsequent valid input lines. A line is bounded
to 4096 bytes; the final line may end at EOF. Long diagnostic text and action names
are abbreviated with a truncation marker so error feedback remains deliverable.
`ok` and exit codes describe dispatch: 0 when all callbacks completed, 2 if any
input was invalid or action unavailable/disabled, and 1 for transport/runtime
failure. A business refusal is an ordinary callback result; chess returns
`{"accepted":false,"code":"illegal_move","message":"..."}` directly, without the
adapter interpreting that result. Observing preserves
previous action feedback. Checkmate and stalemate are recognized; other draw rules
are not adjudicated in this example.

The root Component declares `use_wait_for_command()` once; child Components
declare actions without another wait. Actions support synchronous and async
callbacks, with typed input or `||`. The framework runs `Application::prepare()`
automatically and returns the prepared view after each action.

Observations do not invoke callbacks or advance action feedback. Accepted actions
continue if a frontend stops waiting, and are never replayed. The example submits
no provider Frames. Closing stdin closes only the foreground connection; daemon
state persists until it is stopped.

The runner derives the default socket directory from the executable name. For
`chess_cli`, the socket is `$XDG_RUNTIME_DIR/agentview-chess_cli/socket`, falling back
to `$HOME/.cache/agentview-chess_cli/socket`. Put `--socket PATH` before the lifecycle command,
or use it alone with stdin actions, for an independent game. Concurrent starters
use a lock at `PATH.lock`; only the lock owner may remove a stale socket. The socket
is mode 0600 and daemon diagnostics go to `PATH.log`. Transport failures after
submission are never retried automatically because the action may have executed.
While the foreground process stays open, it asynchronously reaps exited daemons
and concurrent starters. Foreground EOF leaves a running daemon alive.
Lifecycle requests share the action queue: an unfinished async action can make
`stop` or `restart` time out while the daemon remains running.

Custom transports can use `StdinApplication::mount(root)` with `observe`, `submit`,
`feedback`, and consuming `shutdown`; the standard binary needs only `run(root)`.

Implementation: [binary entry point](chess_cli/main.rs),
[root and inline actions](chess_cli/app.rs),
[chess state and view](chess_cli/game.rs), and
the framework's [application owner](../src/component/execution/stdin.rs),
[CLI and stdin/stdout](../src/component/execution/stdin/cli.rs), and
[Unix daemon](../src/component/execution/stdin/daemon.rs).

## Prompt Authoring

Use Markdown strings for instructions and XML nodes for structured application
state. Both belong in the same Component projection:

```rust
use agentview::component::prelude::*;

#[component]
fn review_prompt(status: String) -> Component {
    let instructions = "# Review assistant\n\nReport the current review status in one sentence.";
    view! {
        #[system_once]
        { instructions }

        review_state {
            status { "{status}" }
        }
    }
}
```

| Content | Authoring | Rendering and updates |
| --- | --- | --- |
| Instructions, policies, user requests | `{ prompt }` with `String`, `&String`, or `&str` | Preserves Markdown, whitespace, and literal XML examples. Changed text is sent in full. |
| Structured state, records, action feedback | XML nodes such as `review_state { status { "{status}" } }` | Escapes field values and supports semantic XML updates and removals. |
| XML response examples inside instructions | A fenced XML example within `{ prompt }` | Remains literal text; it does not become a state node or register an action. |

Use `{ prompt }` for a complete multiline string. `"{prompt}"` is a formatted
paragraph with inline validation, and `state { "{value}" }` is text inside an XML
node. A disappearing ordinary user/developer raw string does not retract earlier
instructions from model history. System strings follow the separate
`#[system_once]` snapshot replacement and clearing rules. `#[developer(repeat)]`
resends the whole current instruction on every submitted frame.

Run the credential-free example to inspect both raw Markdown and escaped XML:

```sh
cargo run --no-default-features --example raw_prompt
```

- [`raw_prompt.rs`](raw_prompt.rs) puts a literal `<say>` response example in a
  Markdown instruction and escapes the recipient value in XML state.
- [`debug_prompt.rs`](debug_prompt.rs) uses Markdown instructions beside XML task
  state and captures the exact Responses request.
- [`support_preparation.rs`](support_preparation.rs) prepares Markdown support
  policy and structured account context before the model turn.
- [`chess_agentview/chess_application.rs`](chess_agentview/chess_application.rs)
  uses Markdown action rules beside XML board state and attempt feedback. Model
  actions still follow the registered XML output protocol.
