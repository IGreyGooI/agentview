# AgentView Help for Agents

Use this file when changing Rust Components in AgentView. [docs/engine.md](docs/engine.md)
is the authoritative runtime contract; this file is its authoring guide.

## The LLM Is the User

AgentView builds a user interface for the LLM. Components present current state,
available actions, action results, and feedback as declared by the application.
Interactivity is a primary design requirement. Build the experience around the
model's successive observations and actions:

1. Present the relevant state and available actions, with the inputs and
   constraints the model needs to use them.
2. Handle the selected action against real application state.
3. Expose what happened to that action: its result, progress, or failure, together
   with the resulting state. Distinguish work that is pending from work completed.
4. Present the updated view and next available actions so the model can continue,
   inspect, choose a different action, or finish the task.

For example, a search interaction presents matching items with usable identities
and an action to inspect one. Inspecting an item reveals its current details and
available operations. Performing an operation then exposes its actual outcome.
Each observation gives the model enough information to take the next step.

Feedback matters during successful work as well as after mistakes. Decide what
the model needs to see next and when that information becomes useful. Store the
result in application state and render it in the Component's current POM. The
next submitted Frame delivers the updated view; a Signal write or log entry alone
does not reach the model. Whether and when to drive a subsequent reaction follows
the application's existing driver and preparation rules.

Review and verify the whole observe -> act -> feedback -> next action flow. Check
that handling an action records its result or resulting state, that a subsequent
submitted Frame exposes its actual outcome, and that the model can use the view to
continue. Parsing input or producing a correct final answer alone does not
demonstrate interactivity. Error handling is one part of this experience; see
[Streaming XML Feedback](#streaming-xml-feedback) for that specific case.

## Choose the API

| Need | Use |
| --- | --- |
| Render current business state | `#[component]`, `view!`, and typed `AgentView` POM |
| Keep retained business state | `use_signal`, then `Signal::with` |
| Mark a semantic state region | `#[diff(slot = "...")]` around one POM root |
| Describe field changes | `#[view(diff)]`, `#[view(diff(append))]`, or another field strategy |
| Publish System instructions | `#[system_once]` on ordinary POM |
| Insert an existing multiline prompt unchanged | `{ prompt_string }` as a `view!` root |
| Resend current policy or context on every submitted frame | `#[developer(repeat)]` or `#[user(repeat)]` |
| Declare an authored assistant message | `#[assistant]` on ordinary POM or a Component call |
| Run an application | `Application::mount(...)`, then `app.run().await` |
| Wait for required business inputs | `use_preparation` inside the owning Component |
| End normal application work | `use_application_exit`, then owner calls `shutdown()` |
| Drive a single model turn | `app.react().await` |
| Receive ordinary provider output | `use_provider_event_handler` |
| Declare a native model action with captured state | `NativeToolCall { name, description, on_call }` inside `view!` |
| Declare a streaming XML action with captured state | `XmlStreamingToolCall { element, on_delta, on_complete, ... }` inside `view!` |
| Run a stdin/stdout application | `StdinApplication::run(root).await` inside a Tokio main; the framework owns CLI, daemon, input, output, and cleanup |
| Wait for application-owned CLI input | One `use_wait_for_command()` in the root Component; root and child Components declare inline `Action { name, description, on_call }` in `view!` |
| Encode a provider request or retain remote state | a `ReactionPort`, outside the business Component |

## Business History

On every Component render, declare the complete current state this Component must deliver to the
LLM. This can include rules, context, business state, records, or explicitly authored messages.
"Complete" applies only to that current Component delivery requirement. It is not a transcript of
everything the LLM knows, canonical history, or the final provider request. Keep domain facts in
Signals or durable application state. Ordinary assistant output belongs to the runtime/provider
history path; a Component does not need to repeat past model replies. An explicitly authored
assistant message instead belongs to the Component projection.

The runtime derives new or changed input from that complete projection. Its private `FrameSession`
owns canonical causal history, including admitted tool calls and results. A `ReactionPort` owns wire
encoding, remote cursors, and other provider-private state.
Together they use effective history and current input to satisfy the Component declaration. That
history can contain prior state, earlier conversation, provider assistant output, and admitted tool
facts. Its retention and actual wire representation are not Component-visible guarantees. Reusing
history is a best-effort delivery optimization; the complete Component declaration remains the
content contract.

An item disappearing from a projection does not by itself make the LLM forget prior history. A
user/developer XML `<remove>` declares the old XML state semantically invalid; it does not erase
historical messages. System snapshots, compaction, and context reset retain their dedicated rules.

Full versus Delta is a delivery decision below the Component API. A Full frame can replay required
input without asking business code to reconstruct old provider output. Signal writes only mark a
projection dirty; they never start a request. The default `run()` loop calls `react()`;
Components use preparation to wait for inputs and an exit handle to end the loop.

Keep System instructions as a separate snapshot using `#[system_once]` on ordinary POM, beside the
Component that renders business state.

Use `#[assistant]` when the application itself supplies an assistant message, such as an example
answer. It follows the same placement and contiguous-merge rules as `#[user]` and `#[developer]`:

```rust
use agentview::component::prelude::*;

#[component]
fn answer_example() -> Component {
    view! {
        #[user]
        "What is the status?"

        #[assistant]
        "The request is pending."
    }
}
```

Authored assistant messages compare against the previous complete projection. Changed ordinary
messages send their complete current POM; omission sends no deletion patch, and reappearance sends
the message again. Existing `#[diff]` field strategies can also be combined with `#[assistant]`.
Provider output remains a separate canonical fact even when it has identical rendered text.

## Existing Prompt Strings

Put an existing complete prompt in a dynamic root. `String`, `&String`, and `&str` become a POM
`RawTextNode`, preserving multiline Markdown, XML examples, indentation, and line endings:

```rust
use agentview::component::prelude::*;

#[component]
fn existing_prompt(system: String) -> Component {
    view! {
        #[system_once]
        { system }

        { String::from("## Request\n\nContinue.\n") }
    }
}
```

The string is one opaque POM block. It is not parsed as Markdown or XML and has no added wrapper.
At document level its contents render verbatim. Adjacent blocks still use the normal blank-line
separator. Ordinary user/developer strings compare as complete values; unchanged content is omitted,
changed content is sent in full, and disappearance produces no XML removal even if the string contains
XML-looking text. `#[diff]` retains whole-value comparison; `repeat` still sends the whole current value.
System strings use the existing System snapshot rules.

Quoted `"{system}"` remains a formatted paragraph with inline validation. Use `{ system }` or
`{ format!("{system}") }` for a complete multiline prompt. Text inside explicit Markdown/XML nodes
retains its existing validation and escaping. Low-level callers can use `Document::from_raw_text(...)`.

Run `cargo run --no-default-features --example raw_prompt` for a credential-free example.

## Repeated Content and Placement Scope

Use `#[developer(repeat)]` or `#[user(repeat)]` when the complete current content must be sent on
every submitted frame, even when it is unchanged:

```rust
use agentview::component::prelude::*;

#[component]
fn repeated_policy() -> Component {
    view! {
        #[developer(repeat)]
        policy { "Answer using the current workspace state." }

        #[developer]
        context { "The workspace is read-only." }
    }
}
```

Each placement applies to the immediately following `view!` declaration node and its subtree.
On a Component call it applies to the Component's output subtree, including child Components.
It does not affect following siblings or automatically cover an entire `RenderedProjectionNode`.
The outer placement takes precedence over inner placements, including their repeat setting.

Contiguous content with the same role and repeat setting can merge into one item. Ordinary and
repeated content never merge together, even with the same role. The example therefore produces
two developer items: `policy` repeats, while unchanged `context` is omitted after its first frame.

Repeat takes precedence over `#[diff]` when choosing output: send the complete current POM while
still maintaining the committed diff baseline. Rendering or preparing alone does not submit it.
Disappearing repeat content follows the ordinary user/developer XML removal rule. Repeat is private
delivery metadata; it adds no POM wrapper. Assistant and System placements do not accept `repeat`.

## Complete State and Diff

Put a stable outer `#[diff(slot = "...")]` around one typed POM root and always render its complete
current value. Mark fields whose semantic changes can lower independently; the runtime owns the
accepted baseline and decides whether the next delivery is Full, Delta, or omission.

This compilable root has two meaningful children: an `objective` element and an append-only
`records` field. It always supplies the complete `WorkState`.

```rust
use agentview::component::prelude::*;

#[derive(Clone, AgentView)]
#[agent_view(kind = "record")]
struct Record {
    text: String,
}

#[derive(AgentView)]
#[agent_view(kind = "work_state")]
struct WorkState {
    #[view(element)]
    objective: &'static str,

    #[view(diff(append))]
    records: Vec<Record>,
}

#[component]
fn work_state_component() -> Component {
    let records = use_signal(|| {
        vec![Record {
            text: String::from("Initial observation"),
        }]
    });
    let records = records
        .with(Clone::clone)
        .expect("mounted records signal");

    view! {
        #[diff(slot = "work_state")]
        {
            WorkState {
                objective: "Keep the current work state accurate.",
                records,
            }
        }
    }
}
```

Capture a `Signal` in an event handler, task, or application callback to append a record outside
render. Do not write a Signal during render. On the next reaction, render the complete
value again; the accepted baseline determines whether the tail becomes an `append` operation.

Use `append` only for an append-only domain collection. Select the strategy that matches other
collection semantics. Keep every diff slot address literal, unique, and stable; never derive one
from a record count, current content, or a transient request ID.

A single-child root can legitimately fall back to its complete root when that child changes.
Preserve the truthful POM shape; do not add dummy fields solely to force a Delta.

## Stable Identity and Adjacent Snapshots

Keep a Component mounted at a stable identity and keep each diff slot address stable to retain its
baseline. The Frame compiler compares ordinary items with the previous successfully handed-off
complete projection. A render or a cancelled preparation does not advance that snapshot.

Ordinary output needs no `#[diff]` annotation for this comparison. Items remain ordered values,
including duplicate occurrences; there is no additional item syntax or slot to declare.

| Previous node items | Current node items | Submission |
| --- | --- | --- |
| none | `[A]` | complete A |
| `[A]` | `[A]` | omitted |
| `[A]` | `[B]` | compare A's POM with B's POM |
| `[B]` | `[A]` | compare B's POM with A's POM, even if A appeared earlier |
| `[A]` | `[A, A]` | one new complete A |
| `[A, B]` | `[A]` | removal POM for B's XML, using B's old role |
| `[]` | `[A]` | complete A again |

The compiler retains an equal prefix. When equal-length sequences differ at only one item of the
same role, that item compares its POM and the equal suffix stays in place. Other changed suffixes
remove old XML and resend complete current items in order. Node insertion
or reordering likewise refreshes the affected suffix. This favors correct current content over a
minimal patch. Node identities remain internal and are not rendered as wrappers.

XML changes emit a semantic patch or a complete current root. A disappearing user/developer XML root emits
`<remove>...</remove>` around its old content. Mixed documents are supported; ambiguous root layouts
remove their old XML roots and emit the complete current document. Plain text and Markdown changes
emit complete values, and their disappearance emits no retraction. Use XML for state that must
express removal or invalidation. Canonical history remains append-only; these updates do not remove
or reorder old messages, and equal XML content does not identify a particular historical occurrence.

Existing `#[diff]` field strategies still produce authored semantic deltas. Removed diff addresses
leave the committed baseline, so reappearance sends a complete value. Provider/tool occurrences
retain separate provenance claims, and System retains its independent replace/clear behavior.
Disappearance of ToolCall, ToolResult, assistant, provider-extension, or System items produces no
deletion patch.

Parent content is grouped by node: source `before`, child, `after` produces parent `[before, after]`
followed by the child node. Do not use parent/child source interleaving as a business-history primitive.

## Streaming XML Feedback

Declare ordinary streaming actions with callbacks directly in `view!`:

```rust
use agentview::component::prelude::*;

#[component]
fn notebook() -> Component {
    let notes = use_signal(Vec::<String>::new);
    let saved = notes.with(|notes| notes.join("\n")).expect("mounted notes");
    view! {
        notes { "{saved}" }
        XmlStreamingToolCall {
            element: XmlToolElement::text("note"),
            description: "Save a text note; saved notes appear in the next view.",
            on_complete: move |text: String| {
                notes.update(|notes| notes.push(text))
            },
        }
    }
}
```

`element` and at least one of `on_open`, `on_delta`, or `on_complete` are required. A text
draft defaults to a `String` value; a self-closing draft defaults to `()`. Use `.decode(...)`
on an element for typed attributes and a custom completed value. `on_open` receives an
`Arc<Head>`, `on_delta` receives decoded new text as `String`, and `on_complete` receives
the decoded value once the element closes successfully. Properties can appear in any order.
Callbacks return `Result<T, E>` or a `Send` future producing it; successful return values are
discarded, so write actual outcomes to retained state and render them.

The owning Component mount retains application state and the action's diagnostic view. Each
reaction owns a fresh strict parser and callback bindings from its prepared render. Simple
actions share that parser and execute in source order, awaiting one callback before the next.
Rendering and preparation do not invoke callbacks. The execution runs directly inside
`Application::react()`; cancellation drops its current callback future and remaining input.
Earlier state writes and external effects remain, and callbacks are never replayed. An
incomplete or invalid element does not receive completion. Callback `Err` is a runtime fault,
not an ordinary model-input diagnostic. Callbacks may use `spawn`; explicitly spawned tasks
belong to the action Component mount and are retired when that mount is removed.

`on_invalid` optionally receives an `XmlCallbackDiagnostic`. Diagnostics are also retained in
the action Component's next projection, even without that handler (up to 32, with a count of
additional diagnostics). Errors without a registered target, such as unknown tags, belong to
the first declared action; valid sibling tags route to their own callbacks. All simple action
names in one application must be distinct. The next submitted Frame delivers updated state
and feedback. Run [`streaming_callbacks`](examples/streaming_callbacks.rs) to see a model save
notes, inspect the actual saved count, confirm it, and acknowledge the result.

When combining ordinary callbacks with managed contracts in the same response, use distinct
element names and `ignore_unknown_elements` on the managed contracts that should skip sibling
actions. The ordinary callback parser skips registered managed tags but still reports typos.

The advanced `XmlStreamingToolCall::<Channels> { ... }` form retains explicit attempt state,
reducers, and managed live/publication adapters. Use it when accepted-batch publication or
compensation is required; ordinary callbacks do not acquire those guarantees. See the
[streaming authoring examples](docs/streaming-tool-api-design.md#1-decision).

Ordinary model mistakes are expected interaction inputs. For a contract declaring
independent `say` actions, consider:

```xml
<say>First sentence.</say>
<saya>mistake</saya>
<say>Continue speaking.</say>
```

Record an unknown-element diagnostic for `saya` and skip its subtree. The valid
`say` elements still reach their normal validation and handlers, whether they
arrive in the same delta or later ones. Show the model which actions succeeded,
which were ignored, and what input is supported.

With managed `XmlStreamingToolCall::<Channels>`, read `summary.diagnostics` in `finish` and preserve
useful feedback in application state or publication. Accepted attempt reports
also carry diagnostics; `on_rejected` only covers rejected attempts. Render useful
feedback in the next view even when other actions were accepted.

Choose acceptance from the business result. Do not use a nonempty diagnostics
list as a universal reason to reject every action. Partial acceptance with
feedback is valid. `react()` returning `Continue(())` describes runtime progress;
the Component presents the actual business results.

Business admission, dependencies, and explicit atomic operations still apply.
Runtime/protocol faults, hard limits, cancellation, and uncertain external effects
retain their existing failure and recovery semantics. See
[streaming tool diagnostics](docs/streaming-tool-api-design.md#82-contract-diagnostics).

## Stdin Actions

Start a stdin/stdout application with one framework call:

```rust
use agentview::component::execution::StdinApplication;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    StdinApplication::run(app).await
}
```

The Unix runner handles CLI arguments, daemon discovery and lifetime, stdin JSON
lines, stdout views, and stderr diagnostics. It adds session and connection
metadata, so the root Component needs only its business state and actions.

Declare the business interface once in `view!`. The input structure supplies
JSON decoding and the schema used to advertise the action:

```rust
use agentview::component::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MoveInput {
    /// One canonical lowercase UCI token from the current legal_moves.
    uci: String,
}

// In the root or any child Component. Only the root declares use_wait_for_command().
view! {
    Action {
        name: "move",
        description: "Play one legal move",
        on_call: move |input: MoveInput| game.update(|state| state.play(&input.uci)),
    }
    Action {
        name: "undo",
        on_call: move || undone_game.update(GameState::undo),
    }
}
```

The stdin envelope is `{"action":"move","input":{"uci":"e2e4"}}`. No-argument
callbacks use `||` and require an empty input object. Metadata and callbacks belong
to the same mounted declaration; there is no separate command catalog or `NoInput`
structure. `enabled: false` returns `command_disabled` and the current view without
invoking the callback. Enabled callbacks still validate current business state.
Callbacks return either `Result<Output, Error>` or a `Send` future producing it.
The serializable `Output` is the action's direct stdout result, including any
business refusal; callback `Err` is a runtime fault. No special outcome wrapper
or boolean return convention is required. The view also shows updated state.

Declare the explicit barrier once in the root. Child Components contribute actions
without their own wait hook:

```rust
#[component]
fn app() -> Component {
    use_wait_for_command();
    view! { board() history_actions() }
}
```

`run` also owns provider setup, the inbox, the preparation loop, view rendering,
and shutdown. The CLI commands `start`, `status`, `stop`, and `restart` manage the
daemon; business actions arrive on stdin. `start` returns the current view, and a
default invocation automatically starts a missing daemon. The default socket is
`$XDG_RUNTIME_DIR/agentview-<executable-name>/socket`, falling back to
`$HOME/.cache/agentview-<executable-name>/socket`; `--socket PATH` selects another
session. See the [complete chess interaction](examples/README.md#interactive-chess-cli).

For embedding in a custom transport, `mount` exposes the same application owner
without the CLI or daemon:

```rust
use agentview::component::execution::{CommandCall, StdinApplication};

let mut app = StdinApplication::mount(app)?;
let initial = app.observe().await?;
let response = app.submit(CommandCall::new("undo", serde_json::json!({}))).await?;
// response.result is the callback output; response.view is the complete view.
// response.ok reports dispatch success, without interpreting the business output.
app.shutdown().await?;
```

`feedback(code, message)` delivers input parsing errors through the root's view.
Observations preserve feedback; the next action replaces the input diagnostic.
Calling `shutdown()` starts cleanup immediately; cancelling its waiter does not
cancel cleanup. A custom transport acknowledges shutdown only after awaiting it;
the standard runner handles this itself.

An async Action uses the same `on_call` property. Clone captured handles before
returning `async move { ... }`, as for native callbacks. The returned future runs
inside the current preparation and its Component task scope. Cancelling low-level
preparation drops an unfinished callback and reports interruption on resumption;
it never replays effects. Cancelling a `StdinApplication::submit` waiter merely
stops waiting: the independently owned driver completes the accepted action.

Lower-level integrations can still install `CommandInput` with
`Application::mount_with_commands` and drive `prepare()` themselves. Component
props need neither an inbox nor a provider.

The standard runner returns each action's result and complete view, flushes stdout,
and preserves daemon state when foreground stdin closes. Invalid input produces
feedback and allows the next valid action to continue.

The existing `CliCommand` / `CommandParser` descriptor API remains available for
adapters that deliberately expose business subcommands. Stdin actions do not
require that API or clap.

## Native Tools

Declare a native action beside the state it operates on. `NativeToolCall` properties inside
`view!` bind a typed callback; the callback captures Component state just like a UI event handler.
Only its input type becomes the model's argument schema:

```rust
use agentview::component::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddInput {
    /// Amount to add to the current total.
    amount: i32,
}

#[component]
fn counter() -> Component {
    let total = use_signal(|| 0_i64);
    let current = total.with(|value| *value).expect("mounted total");
    view! {
        counter { value: current, }
        NativeToolCall {
            name: "add",
            description: "Add an amount to the current total and return the new total.",
            on_call: move |input: AddInput| {
                total.update(|value| {
                    *value += i64::from(input.amount);
                    *value
                })
            },
        }
    }
}
```

`name` and `on_call` are required; `description` is optional but should explain the action.
Properties may appear in any order. Callbacks accept an owned `Deserialize + JsonSchema` input
or use `||` for an empty input object, and return either `Result<Output, Error>` or a `Send` future producing that result. Async
callbacks can clone captured handles before returning `async move { ... }`. Input structs use
`#[serde(deny_unknown_fields)]` when extra fields should be rejected. Captured state never becomes
a model argument. Rendering binds the callback without executing it; updated captures are bound
on subsequent renders while the mounted tool retains its call/result history.

Keep actual outcomes and current business state in the Component view. A callback's result is
associated with its native call automatically; the next submitted Frame delivers it together
with the updated view. See the live [`native_tool`](examples/native_tool.rs) example for an
increment followed by an undo using the same retained state.

For a standalone function, `#[tool]` and `NativeToolCall::new(tool)` remain available.
The macro exports a tool definition value under the original function name, generates an owned
argument struct and JSON Schema, and supports synchronous and asynchronous handlers returning
`Result<T, E>`. It does not preserve an ordinary callable function under that name.

Run the live [`native_tool`](examples/native_tool.rs) example with
`cargo run --no-default-features --example native_tool` to see a complete call/result exchange.

```rust
use agentview::component::prelude::*;

/// Add two integers.
#[tool]
fn add(a: i32, b: i32) -> Result<i32, ToolError> {
    Ok(a + b)
}

#[component]
fn calculator() -> Component {
    view! { { NativeToolCall::new(add) } }
}
```

The tool name is always the Rust function name; `#[tool(name = "...")]` is rejected.
Unicode function names are preserved in canonical history and encoded request JSON.
Tool names have no separate byte-length limit; Frame and request size budgets still apply.
`#[tool(description = "...")]` can override the description supplied by function documentation.
Parameter documentation becomes schema field descriptions.
Complex argument types implement `serde::Deserialize` and `schemars::JsonSchema`; successful
return values implement `serde::Serialize`. Missing or invalid typed arguments produce an
`invalid_arguments` tool result without running the handler. A handler `Err` remains a reaction
fault, so represent expected business failures as an explicit successful return value.

Each mounted `NativeToolCall` records an admitted `ToolCall` before invoking the handler, then
appends its `ToolResult`. It retains the latest two provider responses containing calls to that
tool, including every call and result in each response. Pending rounds remain until completed.
Rendering, preparation, and responses without calls to that tool do not advance this window.
Render declares the complete retained record sequence in that tool's projection node.
Within each round, calls precede results, and results follow call order even after a context reset.
Re-rendering never executes or appends a call again.
The Frame compiler claims already-admitted calls and staged results instead of submitting them
twice. Records restored by a context reset retain this ownership as the history window advances.
During normal continuation, accepted history preserves provider call order. Results are submitted
in call order even when handlers finish concurrently. Older records leaving the projection produce no deletion
patch; the port manages provider history and compaction. Unmounting removes future tool
availability while accepted session history remains. Cancellation appends the runtime's
unknown-outcome result to the original component record as well as staging it for the next reaction.

An explicit `reset_model_context()` rebuilds history from the current Component projection.
Across tools, this snapshot follows Component structural order. Business facts that depend on
the original cross-tool chronology must be projected explicitly.

Mount the tool in the same tree as the state it serves. One `react()` can admit a model tool call,
run its handler, and stage output; it does not start another provider request. The default
`run()` loop submits the staged result on its next reaction. A manual driver can do the same:

```rust
# use agentview::component::execution::{Application, ApplicationFault, ReactionPort};
# async fn continue_after_tool<P: ReactionPort>(app: &mut Application<P>) -> Result<(), ApplicationFault> {
app.react().await?; // handles the tool call and stages its result
app.react().await?; // submits that staged result on the next reaction
# Ok(())
# }
```

For raw argument handling, `NativeToolCall::named(name).on_call(handler)` remains available and
uses the same component history. Return `call.output(...)` to preserve the original call ID.
Make external effects idempotent with a business key when retries matter.

For a live business example with an inbox and prepared account context, see
[`support_preparation`](examples/support_preparation.rs) and its
[walkthrough](docs/preparation-examples.md). Its deterministic provider
fixtures are kept under `tests/examples`.

The Responses target receives each mounted tool's complete definition, including its description
and input schema. The raw name-only API retains its permissive object schema. Public OpenAI
integrations use the Responses API; the internal Chat Completions adapter is not a public provider.

## Check the Contract

Use these sources when modifying this area:

- [Runtime ownership, frames, diffs, and tools](docs/engine.md)
- [Diff lowering and ordinary occurrence reconciliation](src/component/execution/projection_diff.rs)
- [Native tool declaration](src/component/authoring/native_tool.rs)
- [Next-reaction staged-tool test](src/component/execution/application.rs)
- [Projection-tree order tests](tests/component_api_projection_tree.rs)
- [Large linear item-order test](tests/component_api_transcript_linear_construction.rs)

After changing semantic diffing or ordinary reconciliation, assert actual ordered item arrays, not
only counts or membership, and run focused tests:

```sh
cargo test --no-default-features --test component_api_projection_tree
cargo test --no-default-features --test component_api_transcript_linear_construction
cargo test --no-default-features --lib projection_diff
```
