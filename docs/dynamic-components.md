# Replace definitions, retain instances

`ComponentDefinition` is the public entry point for components implemented by
external Rust code, an interpreter, or a plugin. It enters the existing typed POM,
render, preparation and Frame pipeline. It does not require `#[component]` or
`view!`.

Each definition owns a logical `type_id`, a `compatibility_key`, and a repeatable,
fallible renderer. `definition.instantiate("key")` declares an instance. The
renderer receives `ComponentHooks`, which accesses state and capabilities owned
by that instance's AgentView mount. Clone a definition and instantiate it under
several keys to obtain independent state and task lifecycles.

```rust
use agentview::component::prelude::*;

fn counter_definition(label: &str) -> Result<ComponentDefinition, ComponentDefinitionError> {
    let label = label.to_owned();
    ComponentDefinition::new("example.counter", "u64-v1", move |hooks| {
        let total = hooks.use_signal_at(0, || 0_u64)?;
        let current = total.with(|value| *value)
            .map_err(|error| ComponentDefinitionError::new(error.to_string()))?;
        let increment = NativeToolCall::props()
            .name(String::from("increment"))
            .description("Increase the total by one and return its new value. No inputs.")
            .on_call(move |_context: EventContext| {
                total.update(|value| {
                    *value += 1;
                    *value
                })
            })
            .build();
        Ok(Component::fragment([
            Component::text(format!("{label}: {current}. Use increment to add one.")),
            increment,
        ]))
    })
}
```

The tool's result is recorded through the ordinary native tool protocol. A later
submitted Frame also exposes the updated total. The model can observe the total,
increment it, see the result, and increment again. The callback receives the
original operation's `EventContext`; external implementations can retain it with
business progress and results.

Use `Component::document` for typed POM and `Component::view` for an `AgentView`
value. `Component::text` / `raw_text` preserve an opaque source-text block.
`fragment` composes content, child instances, ordinary tool builders, `Decision`
builders and actions. Placement methods include `user`, `developer`, `assistant`,
`system_once`, and the user/developer `repeat` and `ephemeral_repeat` variants.
System subtrees follow the native restrictions on state and actions. Content
remains typed POM; rendered prompts are never parsed as a data protocol.

`NativeToolCall::props()` accepts owned names and derives input schemas from
typed callbacks. An interpreter that already has a validated `ToolDefinition`
can use `NativeToolCall::from_definition(definition).on_call(handler)`; its raw
callback receives `EventContext` and `ToolCall` and must decode and validate
business arguments before effects. `Decision::new(...).on_result(...).build()`
uses the existing typed question/answer protocol and Frame binding.

`ComponentHooks` exposes native `use_signal_at`, `use_preparation_at`,
`use_future_at`, `use_coroutine_at`, provider handlers, reaction completion,
application identity, exit, command wait and reaction requests. The explicit
`u32` sites are stable hook identities assigned by the external implementation.
Keep their order, site, kind and retained Rust type stable for a compatible
instance. Declaration failures return `ComponentDefinitionError` and reject the
candidate even if the renderer ignores a hook error. User initializer, renderer,
callback and task panics preserve the native panic contract.

## Identity and compatibility

An instance belongs to its owning parent and has an explicit key. Keyed sibling
reordering does not move its retained state into another instance. Keys must be
nonempty, contain no control characters, and be unique under the same parent
and ordinary/System scope. All key segments are owned and length-prefixed in
runtime identities; punctuation and path separators do not create aliases.

The logical type and compatibility key also participate in identity; the
definition's application version does not. Replacing a renderer while retaining
the same keys preserves compatible instances and their Signals. Adding a key
mounts a new instance. Removing a key retires that subtree. Changing the logical
type or compatibility key mounts a fresh subtree, including all descendants.

The compatibility key is an explicit declaration of the retained hook ABI.
Changing hook order, site, kind, count, state type or coroutine message type
under the same key rejects the candidate. Change the compatibility key when
the new implementation requires fresh state. AgentView does not infer a state
migration from a renderer change.

## Application update boundary

Mount a definition directly with
`Application::mount_definition(definition, port)`. The commands variant is
`mount_definition_with_commands(definition, port, input)`. Normal
`Application::mount` roots can also be replaced by `update_definition`.

```rust,ignore
let mut app = Application::mount_definition(counter_definition("Total")?, port)?;
app.react(false)?.await?;

let update = app.update_definition(counter_definition("Current total")?).await?;
let installed = update.effective_version();
// The instance retains its total; its next observation uses the new label.
app.react(false)?.await?;
app.shutdown().await?;
```

An update supplies a complete root declaration, including its child definitions.
It is not an in-place mutation of a shared definition registry. Captured
definitions are owned snapshots; update the root to replace the child snapshot.

`DefinitionVersion` is application-local, starts at one, and advances only when
a replacement is accepted. It is separate from `FrameRevision` and projection
revision. Success returns `previous_version()` and `effective_version()`.
`DefinitionUpdateError` exposes `kind()`, `message()` and the actual
`effective_version()`; `app.definition_version()` reads the installed version.

The application accepts updates only while Main and all Temporary reactions are
settled. An active reaction, another maintenance operation or unfinished
reconcile returns `Busy`. The caller retries explicitly after settlement.
AgentView neither interrupts an old reaction nor starts another one to install
an update. This boundary lets every submitted Frame complete with its original
callbacks, tool schemas and `EventContext`, including its original Frame and
reaction identities. No event is rebound to the replacement renderer.

The runtime stages the full candidate and checks rendering, keyed identities,
hook compatibility and Frame preparation against the current target/session
before retiring any mount. `Rejected` retains the installed definition, state,
tasks, current projection and version. This validation constructs no submitted
Frame and advances no history, receipts or Frame diff baseline. Signal writes
during rendering are rejected. Arbitrary external effects performed by user
renderers or initializers are not transactional and cannot be rolled back.

After validation, the runtime commits the topology and fences removed or
incompatible mounts, installs the definition and advances its version, aborts
and joins retired tasks, and then
publishes the replacement projection and starts new mount tasks. A lifecycle
infrastructure failure after this commit is terminal `Runtime`, rather than a
recoverable definition rejection. The error reports the accepted version;
already fenced capabilities or external effects are not restored. User panics
unwind according to the native terminal panic contract.

Once an update has been admitted, AgentView owns its completion. Dropping its
waiter does not cancel retirement or publication. Dropping an unpolled update
future has not admitted an update. Await success before relying on publication;
after losing the waiter, read the installed version at a later boundary. During
retirement that accepted version can be newer than the published projection;
the projection revision advances only when publication finishes.

Installing an update does not execute preparation or submit model input. A
projection with preparation declarations remains provisional until the next
explicit `prepare()` or reaction runs the normal preparation barrier. Every
new operation still runs all active preparation hooks; an installed version is
not a readiness cache. Feedback reaches the model in the next submitted Frame.

## Tasks and explicit wakeups

Compatible instances retain their original running `use_future_at` tasks and
coroutine service/inbox. Passing a replacement factory does not restart them;
the unused factory is dropped. Their captured code can continue to run after a
compatible definition update and is released when the task finishes or its
mount retires. Handlers retained by a completed Frame are released as their
reaction and any admitted work finish; they are not stored in a mutable global
callback registry.

Removed or incompatible mounts use native retirement: fence Signals, callbacks,
reaction requests, spawn and coroutine capabilities first, then abort and await
owned tasks before exposing replacement mounts. Late work cannot use an old
capability to affect a fresh mount. There is no implicit task transfer or
arbitrary future migration. Implement a business handoff through retained state
or an explicit coroutine protocol before requesting an incompatible update.

Task completion does not submit a Frame. As with native components, publish the
actual result in state and explicitly call a retained
`ReactionRequest::request()` when the driver should wake. The request remains
sticky, coalesced and mount-fenced; it does not render, re-enter a reaction or
submit a Frame. A compatible update keeps that capability valid; retirement
makes it stale.
