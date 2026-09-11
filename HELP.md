# AgentView Help for Agents

Use this file when changing Rust Components in AgentView. [docs/engine.md](docs/engine.md)
is the authoritative runtime contract; this file is its authoring guide.

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
| Declare a native model tool | `#[tool]` and `NativeToolCall::new(tool)` |
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

## Native Tools

Declare a typed tool with `#[tool]`, then mount its definition with `NativeToolCall::new`.
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
