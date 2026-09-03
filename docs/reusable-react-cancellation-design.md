# Reusable `react()` Cancellation

## Status

Implemented and verified.

## Problem

Before this design was implemented, dropping `Application::react()` had two
outcomes. A pre-handoff drop left the Application reusable, while a post-handoff
drop set a sticky `CancelledAfterHandoff` terminal state. That historical rule
was introduced by FDR-010 because a post-handoff cancellation could drop a
reaction-local native tool lane after its ToolCall entered canonical history,
leaving the session-owned ToolOutput slot unresolved.

The implemented contract is stronger: dropping `react()` cancels only that
reaction. The same `Application` remains usable after both pre-handoff and
post-handoff cancellation. A later reaction is still explicit; cancellation
must never auto-retry or auto-submit.

## Contract

Dropping a pending `react()` future:

1. Stops polling the current provider fact stream, bindings, handlers,
   completion callbacks, and runtime-owned tool lanes.
2. Preserves the already committed outbound Frame, admitted canonical facts,
   completed ToolOutputs, Component writes, and external side effects.
3. Leaves admitted open assistant text represented as `Interrupted`.
4. Closes every admitted ToolCall whose runtime-owned lane did not produce a
   ToolOutput with a fixed runtime-generated cancellation ToolOutput.
5. Leaves the `Application` reusable. The next `react()` performs its normal
   declaration, reconciliation, and Frame preparation.
6. Does not replay a cancelled Component callback and does not resume its
   future.

The runtime-generated result must state only what the runtime knows:

```text
Tool execution was cancelled; its outcome is unknown.
```

It must not claim that the tool did not run or that its external effects were
rolled back.

## State Transitions

### Pre-handoff cancellation

`ReactionPort::submit()` has returned only `Pending`, or the reaction has not
reached `submit()`.

- Drop the prepared candidate and reaction-local work.
- Do not advance FrameSession delivery state or consume ToolOutput receipts.
- Keep the Application ready.

### Post-handoff cancellation before a ToolCall

The Frame and its commit remain authoritative. Dropping the fact stream lets
the port synchronously invalidate or retain its own continuation according to
its existing guard. Admitted partial text remains interrupted. The Application
stays ready; a later declaration may require a higher-epoch Full.

### Post-handoff cancellation with ToolCalls

For each ToolCall admitted during the cancelled reaction:

- retain an already staged real ToolOutput unchanged;
- replace an unresolved slot with the fixed cancellation ToolOutput;
- preserve call identity and provider admission order;
- never accept a late output from the dropped lane.

The next Frame therefore closes every prior ToolCall exactly once. It can be a
Delta only if the port still proves compatible continuity; ports that drop an
unfinished stream normally advance to a higher epoch and require Full.

### Cancellation during normal-finish callbacks

If `ReactionCompleted` was admitted and all ToolCalls were already resolved,
dropping a parser EOF handler or `use_reaction_completion` callback cancels
that callback. It is not replayed on the next reaction. Callback futures are
required to be drop-cancellation-safe; successful Component writes and
external effects before their suspension remain committed.

## Infallible Cancellation Cleanup

Cancellation cleanup runs from `Drop` and therefore cannot await or return an
error. ToolCall admission must make the later fallback deterministic and
infallible:

1. Construct and validate the fallback `CanonicalInputItem::ToolResult` while
   admitting the ToolCall.
2. Reserve exact next-Full count and byte budget for that fallback at the same
   time as the ToolCall replay item.
3. Store the validated fallback and its reserved byte contribution in the
   session-owned ToolOutput slot.
4. If the lane produces a real ToolOutput, atomically validate it and replace
   the fallback reservation with the real result's exact size.
5. Keep cancellation recovery armed while the fact/lane pump is pending. Every
   ordinary `Ok` or `Err` return explicitly disarms it before locals are
   dropped. If the enclosing future is instead dropped while pending,
   materialize fallbacks for only that attempt's unresolved registrations
   before restoring canonical history.
6. Do not materialize cancellation fallbacks during panic unwind. Direct panic
   recovery observes `std::thread::panicking()`; supervised task-panic
   arbitration explicitly suppresses cancellation recovery and the Drop guard
   also checks the shared panic monitor. Both paths leave unresolved slots
   fail-closed while preserving the runtime's existing panic propagation
   semantics. A caller that catches any user-code panic must still discard the
   Application; the runtime does not make that unsupported reuse safe. This
   intentionally differs from a future cancelled by ordinary Drop.

The fallback is a reserve, not a visible ToolOutput while the lane is alive.
Normal completion still requires every lane to resolve normally. No allocation,
canonical validation, budget validation, or provider call may be required to
materialize a fallback during `Drop` beyond moving already owned values.

If a real ToolOutput is produced but cannot replace the fallback reservation,
for example because its exact encoded size exceeds the Full reserve, the
reaction returns its existing real-output validation fault and fails closed.
The hidden fallback remains unmaterialized: the runtime must not report the
completed tool as cancelled merely because its real result was invalid or too
large.

## Application Lifecycle

The implementation removes the post-handoff cancellation poison path:

- `ReactionCancellationGuard` and
  `APPLICATION_TERMINATED_AFTER_CANCELLATION` are no longer needed;
- `CancelledAfterHandoff` is removed because cancellation no longer produces
  that fault;
- task-panic terminality remains unchanged;
- normal terminal port and protocol faults remain unchanged.

`Application::shutdown()` remains the operation that consumes the Application,
fences mounts, and aborts and joins mount-scoped tasks. Cancelling `react()`
does not perform Application shutdown.

## Provider Continuation

No new public cancellation method is added to `ReactionPort`, but its public
fact-stream contract is strengthened. Dropping an unfinished
`ProviderFactStream` must synchronously leave the subsequent `declare()` in
exactly one truthful state:

- compatible `Accepted`, only if the retained provider-private state still
  supports that exact continuation;
- a higher-epoch `FullRequired`, if the port has a valid recovery strategy; or
- a terminal declaration fault.

A port must not continue declaring stale `Accepted` continuity after its
unfinished stream and required continuation state have been dropped. Built-in
stream Drop guards remain responsible for provider-private state:

- sealed required causal artifacts are retained;
- unsealed private output may be aborted;
- disposable request, connection, SSE, and cache-hint state may be discarded;
- loss of accepted continuation advances the target epoch and yields
  `FullRequired` when the port has a valid recovery strategy;
- otherwise the port fails closed on the next declaration.

Application reusability does not promise that every provider session is
recoverable. It promises that cancellation itself does not poison the
Application; the next explicit `react()` receives the port's typed recovery or
terminal result.

## External Application

The external adapter may continue cancelling an inner `react()` by selecting
on its oneshot and dropping the reaction future. Once the owner is returned,
the next explicit `observe()` starts the recovery reaction. A new `act()` is
not a recovery entry point because there is no active observation generation
to accept its payload. Existing ingress generation fencing prevents late input
from the cancelled stream from entering the new reaction.

## Documentation Alignment

`docs/engine.md` now keeps all cancellation sections aligned:

- both pre- and post-handoff cancellation leave Application reusable;
- handoff still determines which Frame and facts remain committed;
- unfinished native ToolCalls receive the standard unknown-outcome result;
- callbacks are drop-cancellable and are not replayed;
- no automatic reaction is started;
- provider recovery remains port-specific and may fail closed.

The historical review records that this supersedes the terminal choice in
FDR-010 while retaining its underlying unresolved-ToolCall finding.

## Verification

Focused regression coverage verifies:

1. Pre-handoff cancellation remains zero-handoff and reusable.
2. Post-handoff cancellation before any fact is reusable and the next Frame is
   Full after continuity loss.
3. Interrupted text is replayed once in the recovery Full.
4. A pending tool lane is dropped, receives exactly one cancellation result,
   and the next Frame closes its ToolCall.
5. A completed lane keeps its real output while a sibling pending lane receives
   the cancellation result, preserving ToolCall order.
6. Cancellation fallback budget is reserved at ToolCall admission; an
   insufficient reserve fails before exposing the ToolCall event.
7. A real ToolOutput replaces, rather than adds to, fallback reserve usage.
8. Cancellation while an ordinary event handler, XML lifecycle handler, EOF
   handler, or reaction-completion callback is pending drops it and does not
   replay it.
9. ExternalApplication can observe another Full after cancelling a post-handoff
   act; stale ingress remains rejected.
10. Responses, Chat Completions, Debug, External, Skill, and Plugin ports retain
    or adopt the strengthened stream-drop continuity behavior. A custom-port
    contract test rejects stale `Accepted` continuity after an unfinished
    stream is dropped.
11. Component task panic remains sticky terminal and continues to outrank a
    concurrent cancellation.
12. Callback, tool-lane, and provider-stream panic unwind does not materialize
    cancellation fallbacks, even when the caller catches the panic.
13. Retained compatible continuity can produce a valid Delta containing the
    ToolOutput that closes the cancelled ToolCall.
14. A materialized cancellation fallback survives a later Frame preparation
    fault and a later pre-handoff submit cancellation without duplication.
15. Two consecutive cancellation and recovery cycles leave no stale
    registrations and emit exactly one result for every admitted ToolCall.

Run both default and `--no-default-features` all-target checks and the focused
application, admission, frame, provider, and external test suites.

## Non-goals

- Resuming a cancelled callback future.
- Replaying an admitted event into a fresh callback binding.
- Rolling back Component state or external tool effects.
- Guaranteeing that a remote model stops computing when its local stream is
  dropped.
- Automatically starting a recovery reaction.
- Adding a cancellation token to Component callbacks.
