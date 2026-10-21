# Example Prompts

## Callback Actions

`native_tool` and `streaming_callbacks` declare actions inside `view!`, with callbacks
capturing the same state rendered by their Component:

```sh
cargo run --locked --no-default-features --example native_tool
cargo run --locked --no-default-features --example streaming_callbacks
```

Both use the shared live provider configuration: `OPENAI_API_KEY` and a positive
`AGENTVIEW_CONTEXT_WINDOW_TOKENS` are required in the environment or `.env`. The window
is bound to `AGENTVIEW_MODEL` in a `ModelSpec`, which the request options retain. The native
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
| no subcommand | Start a missing daemon and immediately deliver a Full initial view, then read top-level JSON action objects, including multiline objects. Submit a newline-terminated buffer after 100 ms without new bytes, or immediately at EOF without requiring a newline; then decode the entire frame and return its resulting Delta view. Empty EOF adds no view. |
| `start` | Start or connect, then return the current view without resetting the game. |
| `status` | Return the running view, or report stopped without starting a daemon. |
| `stop` | Return the final view and stopped status after Application cleanup and socket removal. Already stopped is a successful no-op. |
| `restart` | Stop, start a fresh game, and return its view. The old game is discarded. |
| `help` | Explain the protocol and lifecycle commands without starting a daemon. |

stdout starts with one `<view ok="true|false">...</view>` before input, followed by
one view per input frame. The initial
delivery contains the current board, FEN, legal moves, retained action feedback,
JSON input schemas, and daemon status. Later views on the same connection omit
unchanged state, context, and schemas through the existing Frame compiler;
updated state and every new action result remain visible. A new frontend gets
its own full initial delivery. There is no separate `<action_result>` copy.
The three actions in the heredoc above form one frame and return one result view after
EOF, following the initial view. Callers can keep stdin open: read the initial view,
send a complete batch ending in a newline
(`\n`) in one write, wait for its view after 100 ms without new bytes, then send
the next batch. A newline alone does not submit immediately; without a final
newline, even complete JSON remains buffered. Two writes less than 100 ms apart
may combine if the caller does not wait for a response; a pause longer than
100 ms after any newline can split a batch, including inside a multiline object. The
application does not see write-call boundaries. No end marker or `actions`
wrapper is required. The idle interval groups input and does not limit action
execution. Input and results have no fixed line-size or transport byte cap.

Applications may opt into `StdinApplication::run_with_options` with a positive
`idle_refresh_interval`. The command feedback CLI example observes once per
second, delivering changes from another reader while stdin remains open.
Unchanged idle observations emit no view and preserve partial stdin and feedback.

Runtime diagnostics use stderr. JSON completeness does not determine framing.
After newline plus idle or EOF fixes the boundary, incomplete or malformed JSON
or an invalid action envelope rejects the whole frame without executing its
prefix and clears the buffer; later input starts a new frame.
An unknown action, invalid typed input, or disabled action
instead records an ordinary rejection and allows later actions in the accepted
frame to run. An illegal chess move is a business result, with the same
continuation behavior. Action names, diagnostics, and result JSON are preserved in
full, without fixed stdin/socket byte caps or feedback truncation. Existing model
Frame/provider budgets remain explicit rejection boundaries when used.
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
automatically, refreshes bindings between actions, and returns one final view
after all actions in the frame.

Observations and empty batches do not invoke callbacks or clear action feedback.
Each nonempty frame replaces the preceding feedback batch. Its actions execute
in order without another queued frame or observation interleaving. Accepted
batches continue if a frontend stops waiting, and are never replayed. Each daemon
connection owns a private `FrameSession`; the delivery baseline commits only
after writing its response successfully. A failed write closes the connection.
The example needs no model provider. Closing stdin closes only the foreground
connection; daemon state persists until it is stopped.

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
The frontend waits up to five minutes for queued work and callbacks before a
response starts; socket transfers retain a separate ten-second deadline. A client
timeout or interruption does not cancel or replay accepted work. An idle
connection can wait for its next request's first byte without a transfer timeout.

Custom transports can use `StdinApplication::mount(root)` with `observe`, `submit`,
`submit_frame`, `feedback`, and consuming `shutdown`; the standard binary needs
only `run(root)`. `StdinResponse.result` retains the direct result for `submit`
and an array of `{action, ok, result}` records for `submit_frame`. Those custom
transport methods continue to return complete view snapshots.

Implementation: [binary entry point](chess_cli/main.rs),
[root and inline actions](chess_cli/app.rs),
[chess state and view](chess_cli/game.rs), and
the framework's [application owner](../src/component/execution/stdin.rs),
[CLI and stdin/stdout](../src/component/execution/stdin/cli.rs), and
[Unix daemon](../src/component/execution/stdin/daemon.rs).

## Command Feedback and Delivery

These two examples share one small application with typed `record { text }` and
`inspect {}` actions. The application declares fixed context and changing state
as separate POM roots; the framework supplies feedback. Neither example needs
model credentials or a database.

```sh
cargo build --locked --no-default-features --example command_feedback_cli --example command_feedback_frames
target/debug/examples/command_feedback_cli <<'JSONL'
{"action":"record","input":{"text":"SQLite keeps long-term memory"}}
{"action":"inspect","input":{}}
JSONL
target/debug/examples/command_feedback_cli stop
target/debug/examples/command_feedback_frames target/command-feedback-frames
```

The heredoc delimiter is shell syntax; EOF submits both top-level action objects
as one frame, with no final newline required. Objects can span lines. With stdin
kept open, end the whole batch with a newline (`\n`); 100 ms without new bytes
then submits the buffer as a frame, followed by JSON validation. Send the entire
intended batch in one write and wait for one view before sending the next.
Use `--socket PATH` with the CLI
to select an isolated session. Its daemon retains the example's in-memory state
until stopped.

| Example | Actual delivery |
| --- | --- |
| [`command_feedback_cli.rs`](command_feedback_cli.rs) | The standard stdin runner prints an initial Full view, then Delta deliveries on the same connection using the existing Frame compiler. Unchanged context/schema is omitted; all new action outcomes remain in feedback, with no separate direct-result copy. |
| [`command_feedback_frames.rs`](command_feedback_frames.rs) | Captures real native Frames with `DebugProviderPort`, without a model. The first frame contains both action results; an unchanged frame has no projection items; a new batch sends complete feedback; changing application state omits unchanged context and Action schema POM. |

The Frame example writes canonical JSON, rendered views, and a summary. A zero-byte
rendered projection still has a JSON Frame envelope. Full/Delta is transport
metadata; a Delta Frame can carry a complete replacement feedback root. A newer
action remains a distinct result even if it returns the same JSON value.

See the [implementation guide](../docs/command-feedback-delivery.md),
[incremental stdin captures](../docs/examples/command-feedback/incremental/README.md),
[earlier captured stdin/stdout and Frames](../docs/examples/command-feedback/current/README.md),
and shared [application source](command_feedback/app.rs).

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
| Structured state, records, action feedback | XML nodes such as `review_state { status { "{status}" } }` | Preserves field values verbatim and supports semantic XML updates and removals. |
| XML response examples inside instructions | A fenced XML example within `{ prompt }` | Remains literal text; it does not become a state node or register an action. |

Use `{ prompt }` for a complete multiline string. `"{prompt}"` is a formatted
paragraph with inline validation, and `state { "{value}" }` is text inside an XML
node. A disappearing ordinary user/developer raw string does not retract earlier
instructions from model history. System strings follow the separate
`#[system_once]` snapshot replacement and clearing rules. `#[developer(repeat)]`
resends the whole current instruction on every submitted frame.

Run the credential-free example to inspect both raw Markdown and structured XML:

```sh
cargo run --no-default-features --example raw_prompt
```

- [`raw_prompt.rs`](raw_prompt.rs) puts a literal `<say>` response example in a
  Markdown instruction and preserves the recipient value in XML state.
- [`debug_prompt.rs`](debug_prompt.rs) uses Markdown instructions beside XML task
  state and captures the exact Responses request.
- [`support_preparation.rs`](support_preparation.rs) prepares Markdown support
  policy and structured account context before the model turn.
- [`chess_agentview/chess_application.rs`](chess_agentview/chess_application.rs)
  uses Markdown action rules beside XML board state and attempt feedback. Model
  actions still follow the registered XML output protocol.
