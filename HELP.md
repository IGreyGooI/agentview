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
| Run an application | `Application::mount(...)`, then `app.run().await` |
| Wait for required business inputs | `use_preparation` inside the owning Component |
| End normal application work | `use_application_exit`, then owner calls `shutdown()` |
| Drive a single model turn | `app.react().await` |
| Receive ordinary provider output | `use_provider_event_handler` |
| Declare a native model tool | `#[tool]` and `NativeToolCall::new(tool)` |
| Encode a provider request or retain remote state | a `ReactionPort`, outside the business Component |

## Business History

Render the complete, current business POM on every Component render. Keep domain facts in Signals
or durable application state; do not construct a per-Component transcript or emit past provider
messages yourself.

The runtime derives new or changed input from that complete projection. Its private `FrameSession`
owns canonical causal history, including admitted tool calls and results. A `ReactionPort` owns wire
encoding, remote cursors, and other provider-private state.

Full versus Delta is a delivery decision below the Component API. A Full frame can replay required
input without asking business code to reconstruct old provider output. Signal writes only mark a
projection dirty; they never start a request. The default `run()` loop calls `react()`;
Components use preparation to wait for inputs and an exit handle to end the loop.

Keep System instructions as a separate snapshot using `#[system_once]` on ordinary POM, beside the
Component that renders business state.

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

## Stable Identity and Ordinary Tree Fold

Keep a Component mounted at a stable identity and keep each diff slot address stable if its baseline
and occurrence ledger must continue. Moving, remounting, or conditionally replacing a Component
changes that assumption.

This is the established occurrence fold for **ordinary canonical input items after POM lowering**.
It does not describe `#[diff]` operations, System replacement, native tools, or provider wire state.

| Current node lists | Newly submitted ordinary items | Accumulated submitted history |
| --- | --- | --- |
| `left: [A]`, `right: [O]` | `[A, O]` | `[A, O]` |
| `left: [A, B]`, `right: [O, P]` | `[B, P]` | `[A, O, B, P]` |
| `left: [A, B, C]`, `right: [O]` | `[C]` | `[A, O, B, P, C]` |
| unchanged | `[]` | `[A, O, B, P, C]` |

At row two, the current projection flattens to `[A, B, O, P]`, while submitted history remains
`[A, O, B, P]`. Visit nodes in projection order and items within each node in order. First claim a
matching submitted occurrence in the same node; then claim an unclaimed matching provider occurrence
from the same execution scope; append only what remains.

A `#[view(diff(append))]` tail is deliberately forced through as a diff submission and does not take
part in ordinary occurrence deduplication. Count equal ordinary occurrences rather than treating
values as a set: `[A]` becoming `[A, A]` appends one `A`. Later omission, insertion, or reordering
does not retract or reorder prior submissions.

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

`#[tool(name = "...", description = "...")]` can override metadata. Otherwise the function
name and documentation supply it; parameter documentation becomes schema field descriptions.
Complex argument types implement `serde::Deserialize` and `schemars::JsonSchema`; successful
return values implement `serde::Serialize`. Missing or invalid typed arguments produce an
`invalid_arguments` tool result without running the handler. A handler `Err` remains a reaction
fault, so represent expected business failures as an explicit successful return value.

Each mounted `NativeToolCall` records an admitted `ToolCall` before invoking the handler, then
appends its `ToolResult`. It retains the latest two provider responses containing calls to that
tool, including every call and result in each response. Pending rounds remain until completed.
Rendering, preparation, and responses without calls to that tool do not advance this window.
Render declares the complete retained record sequence in that tool's projection node.
Re-rendering never executes or appends a call again.
The Frame compiler claims already-admitted calls and staged results instead of submitting them
twice. Global history preserves provider call order and results are submitted in call order even
when handlers finish concurrently. Older records leaving the projection produce no deletion
patch; the port manages provider history and compaction. Unmounting removes future tool
availability while accepted session history remains. Cancellation appends the runtime's
unknown-outcome result to the original component record as well as staging it for the next reaction.

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
and input schema. The raw name-only API retains its permissive object schema. The Chat Completions
target is text-only: a nonempty native ToolCatalog is rejected before handoff.

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
