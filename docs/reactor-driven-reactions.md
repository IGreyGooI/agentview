# Reactor-driven reactions

Date: 2026-08-28

Status: **superseded by [`frame-driven-runtime-plan.md`](frame-driven-runtime-plan.md); not an implementation or
public API source.**

The external-driver scheduling conclusion remains, but the public `ReactionHandle::react(exchange)` candidate and
the complete-projection Provider boundary were replaced by fixed-port `Application<P>::react()`, private
`FrameSession`, and `ReactionPort::declare/submit`. The authoritative contract is [`engine.md`](engine.md).

This proposal replaces the scheduling part of the earlier `AgentLoop`, `use_loop`, and
public `FramePort` design. Until this proposal is explicitly adopted and folded into
[`engine.md`](engine.md), that document remains the description of the current design.

The review diagram is available as
[`reactor-reaction.sequence.html`](reactor-reaction.sequence.html). Its checked-in source is
[`reactor-reaction.sequence.json`](reactor-reaction.sequence.json).

## 1. Decision in one sentence

The Reactor owns when another reaction is needed. AgentView exposes one structured
`react(exchange)` operation that creates a fresh Frame, exchanges it for a
`ProviderEventStream`, dispatches the stream into the matching Component bindings, and
does not return until the complete reaction and the resulting reconciliation have ended.

There is no public AgentView EventLoop and no public `FramePort` scheduling stream.

## 2. Ownership

```text
Reactor
  owns its scheduling inputs:
    wake/channel/timer/CLI request/plugin invocation
  owns its external exchange:
    ProviderPort/skill transport/plugin transport
  decides when to call ReactionHandle::react(...)

ReactionHandle
  owns the Component reaction gate
  prepares the latest Frame only after the gate is acquired
  retains the exact private reaction bindings
  dispatches the returned ProviderEventStream
  reconciles Component state before returning
```

`Reactor` is a role, not necessarily a public trait. Any async owner that holds a
`ReactionHandle` can implement the role. Its wait loop and wake coalescing policy belong to
that integration, not to the Component runtime.

## 3. Candidate public shape

The first review found that a callback returning `ProviderEventStream<'_>` from captured state
does not express which borrow owns the stream. The candidate therefore passes mutable exchange
state explicitly and names the shared lifetime:

```rust
pub struct ReactionHandle {
    // private Component runtime capability
}

impl ReactionHandle {
    pub async fn react<'a, X, Exchange, ExchangeFuture>(
        &'a mut self,
        exchange_state: &'a mut X,
        exchange: Exchange,
    ) -> Result<(), ApplicationHostFault>
    where
        X: Send + ?Sized + 'a,
        Exchange:
            FnOnce(&'a mut X, Frame) -> ExchangeFuture + Send + 'a,
        ExchangeFuture: Future<
                Output = Result<ProviderEventStream<'a>, ProviderFault>,
            > + Send
            + 'a;
}
```

The Agent call remains small:

```rust
reactions
    .react(&mut provider, |provider, frame| {
        provider.execute(frame.into_projection())
    })
    .await?;
```

This is still a callback API rather than a public Reactor trait. Explicit state only makes the
borrow returned by the existing `ProviderPort::execute(&mut self)` source-level and testable.

`&mut self` is intentional for the first version. One Reactor owns one handle and cannot
start two reactions concurrently through it. A cloneable internally queued handle can be
added later only if a real integration needs concurrent submitters and defines their
ordering.

The framework does not need a public `Reactor` trait. A high-level entry can accept an
ordinary async closure:

```rust
agentview::run(root, |mut reactions| async move {
    loop {
        wake.wait().await;

        reactions
            .react(&mut provider, |provider, frame| {
                provider.execute(frame.into_projection())
            })
            .await?;
    }
})
.await
```

The exact name and ownership shape of `run` are not frozen by this proposal. The contract
being reviewed is `ReactionHandle::react`, not a builder hierarchy.

## 4. One `react` call

```text
1. Acquire the single-reaction gate.
2. Reconcile dirty Component state.
3. Prepare one atomic private bundle:
     public Frame
     exact generation-local reaction bindings
4. Invoke exchange(Frame) exactly once.
5. Consume the returned ProviderEventStream.
6. Dispatch every event through the retained bindings.
7. Drain ToolCall lanes, finalize streaming parsers, and dispatch EOF diagnostics.
8. Reconcile Component state changed by the reaction.
9. Commit the latest complete RenderedProjection.
10. Release the gate and return.
```

The callback is invoked only after the Frame and its bindings have been successfully
prepared. A stale or failed preparation therefore cannot accidentally start an external
Provider request.

Dropping or cancelling `react()` drops its event stream and generation-local bindings.
Already committed Component writes and irreversible external effects are not rolled back.
Cancellation does not force a new reconciliation. The previous successful committed projection
remains readable, possibly with dirty state waiting behind it, and the next `react()` reconciles
before invoking its exchange callback.

## 5. Frame identity

The public call does not need a `FrameId`:

```rust
reactions
    .react(&mut exchange_state, |exchange_state, frame| {
        exchange_state.exchange(frame)
    })
    .await
```

The structured call already preserves causality. Runtime code holds the private bindings
while the callback owns the corresponding public Frame, and it consumes the returned stream
before `react()` completes. Version 1 does not allow multiple outstanding Frames or delayed
submission of a previously observed Frame.

An internal reaction identity can remain for tracing. A transport may also add its own wire
request id when multiplexing, but neither id is an authority token in the public Component
API.

`Frame` is not `RenderedProjection`:

- `RenderedProjection` is the latest complete, immutable committed Component projection and
  can be read without starting a reaction. It remains readable when newer Signal state is dirty.
- `Frame` is the per-reaction value handed to one exchange callback.
- A Frame contains a complete projection. It never contains Provider-specific history or a
  precomputed `#[diff]` payload.
- Private reaction bindings remain in the Runtime and never enter the public Frame.

## 6. Provider boundary

`ProviderPort` remains the lower model-backend boundary:

```rust
pub trait ProviderPort: Send {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault>;
}
```

An Agent Reactor connects it to `react`:

```text
ReactionHandle::react
  -> Frame
  -> ProviderPort::execute(complete projection)
  -> ProviderEventStream
  -> Component reaction bindings
```

The ProviderPort continues to own Provider history, compaction, and the previous baseline
used for `#[diff]`. Frame creation, committed-projection reads, and failed preparation do not
advance that baseline.

## 7. Integration shapes

All integrations use the same `react(exchange)` primitive. They differ only in what their
Reactor awaits and how the callback exchanges a Frame for events.

| Integration | Reactor waits for | Exchange callback |
|---|---|---|
| Autonomous Agent | wake, timer, policy, or external request | `ProviderPort::execute` |
| Skill | a skill invocation that requires a model reaction | skill transport exchange |
| Other-agent Plugin | parent-agent invocation | plugin protocol exchange |

A bare Skill read of the latest committed projection does not call `react()`. Component-defined
commands that only update local state also remain separate from model reaction scheduling.
If such a command wants another model turn, it must signal the owning Reactor through an
integration-defined channel; writing a Signal alone does not start a reaction.

Signals that arrive while `react()` is running are buffered or coalesced by the Reactor's own
channel policy. The Reactor normally awaits the current `react()` before issuing the next one,
so the next Frame necessarily includes the reconciliation from the previous reaction.

The current `SignalRuntime::wake_revision` cannot be used for this channel because every Signal
write advances it. Component-owned background work instead needs a separate notifier created by
the particular Reactor, conceptually no larger than:

```rust
Arc<dyn Fn() + Send + Sync>
```

Calling this notifier records integration-specific demand; AgentView still does not call
`react()` automatically. Its authoring API and injection path remain a follow-up decision.

A long-lived WebSocket that implements Provider or Plugin transport belongs to the outer
Reactor. A WebSocket that is genuinely Component business state may be owned by a
Component-scoped coroutine; it updates Signals and uses the separate notifier when the owning
Reactor should consider another reaction. Component unmount cancellation and transport
reconnect policy remain separate concerns.

## 8. Required invariants

1. One `react()` call creates at most one Frame and starts at most one external exchange.
2. Frame preparation and installation of its private reaction bindings are one transaction.
3. The exchange callback is not invoked unless that transaction succeeds.
4. Events returned by the callback can only reach the bindings prepared for that call.
5. `react()` is single-flight and does not return before Provider EOF, ToolCall lanes, terminal
   handlers, and post-reaction reconciliation complete.
6. Signal dirty never starts `react()` by itself.
7. Reading the latest committed projection never starts `react()`.
8. Each callback receives a complete projection; Provider diff state advances only inside the
   concrete ProviderPort at its real transport handoff boundary.
9. A clean Component may reuse retained rendering, but every reaction receives fresh one-shot
   dispatch state derived from mounted handler registrations.
10. Detached Component tasks are outside the reaction completion barrier. They must signal the
    Reactor explicitly if their state change should cause another reaction.
11. The last successful committed projection remains readable while Component state is dirty.
12. Normal reaction completion reconciles and commits handler updates before `react()` returns;
    cancellation may leave dirty state for the next call.

## 9. Changes from the previous AgentLoop proposal

This candidate removes these proposed public concepts:

- framework-owned `AgentLoop` scheduling policy;
- Component-owned `Continue < Sleep < Stop` disposition;
- `use_loop()` as a way to control Host progression;
- framework-owned sticky wake epoch as the universal scheduling policy;
- public `FramePort` or EventLoop abstraction.

Component-scoped async task ownership is still useful, but task execution and cancellation do
not imply a universal wake policy. The API for sending an integration-specific signal from a
Component task to its owning Reactor remains a separate decision.

The existing low-level `ComponentReactionRuntime` and `ApplicationHost` remain implementation
inputs. The likely internal change is to separate "prepare a Frame and bindings" from "dispatch
an already prepared reaction" instead of rendering and calling `ProviderPort` in one method.

## 10. Focused review questions

The architecture review answered the original questions as follows:

| Question | Review conclusion |
|---|---|
| Borrowed Provider stream | Accepted after changing to the explicit named-lifetime exchange-state signature in section 3. |
| Single-flight and cancellation | `&mut ReactionHandle` is sufficient for v1 if the handle is non-cloneable. |
| Exact Frame/bindings association | Keeping the atomic prepared bundle private prevents stale-generation dispatch races. |
| Skill/Plugin correlation without `FrameId` | Accepted. Request context or multiplexing ids belong to the callback transport. |
| Component background notification | Use a separate Reactor-owned notifier; its authoring injection API is still open. |

The existing `ExternalProviderPort` is evidence for the correlation result: each exchange uses
its own request-local oneshot sender without a Component Frame id.

### Required implementation changes

The review found two changes that must exist before the public API can claim this contract:

1. Split current `ApplicationHost` execution so a successfully prepared Frame/bindings bundle can
   be held across the exchange callback, then perform a normal-completion reconciliation after
   Provider EOF, lanes, and EOF diagnostics.
2. Change `ComponentHost::current_projection()` so dirty state does not hide the previous successful
   commit. Dirty means reconciliation is pending, not that the committed projection disappeared.

The first source-level callback shown in this document was also rejected because its anonymous
stream lifetime was not expressible. Section 3 contains the reviewed replacement.

### Remaining API question

Only the Component-to-Reactor notification capability remains intentionally unresolved. It must
be separate from Signal dirtiness and must not restore a framework-owned loop policy.

Naming, a generalized task API, command schema syntax, retries, and a new fault taxonomy are not
blockers for this review.
