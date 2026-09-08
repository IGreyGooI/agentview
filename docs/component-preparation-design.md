# Component Preparation (`use_preparation`)

## Status

后续提案：[用 use_preparation 驱动模型回合](preparation-driven-react-proposal.md)，讨论固定 react 循环、并发准备和应用退出；尚未实现。

Implemented and verified on 2026-09-06. The implementation replaces keyed
`use_dependency` with operation-scoped `use_preparation` while preserving
synchronous mount, explicit preparation, and the existing reaction lifecycle.

## Purpose

A Component can require asynchronous work before its projection may cross the
provider handoff. That work belongs to the Component that declares it, rather
than to a root-managed registry of child futures.

Preparation is operation-scoped. Every explicit `Application::prepare()` or
`Application::react()` runs the active Component preparations for that
operation before handoff. It is not a cache, a resource subsystem, or an
invalidation mechanism.

## Public API

```rust
pub fn use_preparation<Factory, PreparationFuture, Error>(factory: Factory)
where
    Factory: FnOnce() -> PreparationFuture + Send + 'static,
    PreparationFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: Display + Send + 'static;
```

There is no preparation key and no `use_dependency` compatibility contract.
The factory is declared during synchronous render, but it is not invoked
there. Render performs no I/O. The operation later invokes the factory and
directly awaits its returned future before provider handoff.

```rust
#[component]
fn conversation_history(props: HistoryProps) -> Component {
    let history = use_signal(HistoryView::empty);
    let preparation_target = history.clone();
    let repo = props.repo.clone();

    use_preparation(move || {
        let repo = repo.clone();
        let preparation_target = preparation_target.clone();

        async move {
            let snapshot = repo.load().await?;
            preparation_target.set(snapshot)?;
            Ok(())
        }
    });

    let history = history.with(Clone::clone)?;
    view! {
        conversation_history { history }
    }
}
```

The factory and future are ordinary direct user code. They are not spawned as
Component-owned background work, and the runtime makes no guarantee for work
the factory detaches separately.

## Operation-Scoped Execution

Each call to `prepare()` or `react()` creates a distinct preparation operation.
Within that operation, the runtime identifies a hook by its current mount
generation and lexical hook slot.

- Every active `(mount generation, lexical slot)` runs once per operation.
- A rerender during that operation skips slots that already completed in that
  operation.
- A newly mounted Component contributes new slots, which run in that same
  operation.
- A later explicit `prepare()` or `react()` starts fresh and runs every active
  slot again, including slots that completed in an earlier successful, failed,
  or cancelled operation.
- Therefore, `prepare().await?` followed by `react().await?` runs the active
  preparations in both calls.
- A pre-handoff continuity retry inside one `react()` keeps the same
  operation-local completion set and does not rerun preparations.

Operation-local completion is discarded when the operation returns, fails, or
is cancelled. It is never stored as a key, readiness bit, or other
cross-operation cache.

## Declaration Snapshots and Dependencies

When the runtime dispatches a slot, it uses the factory captured by that
slot's current rendered declaration. Completing that slot marks only that
operation-local slot complete.

Another hook can write a Signal that changes the inputs captured by a later
render of an already completed hook. That change does not rerun the completed
hook in the same operation. Preparation does not infer a dependency graph from
Signal reads, captures, or changed inputs.

Dependent I/O must be expressed deliberately:

- Put the dependent steps in one factory when they must run together.
- Mount a nested Component when a child preparation should be discovered only
  after a parent preparation writes the state that enables it.

An explicit later operation is the boundary that runs the active declarations
again with newly rendered inputs.

## Stabilization and Handoff

Preparation handles mounts discovered from preparation Signal writes with a
bounded loop:

```text
for waves 1 through 16:
    render/reconcile and snapshot active declarations
    dispatch and await unfinished operation-local slots in structural/lexical order
    discard unused declarations for slots already completed in this operation
    if the projection is clean:
        succeed

render/reconcile once more, without dispatching any preparation
if unfinished declarations remain:
    fail with PreparationGraphUnstable
discard unused declarations
if the projection is clean:
    succeed
otherwise fail with PreparationGraphUnstable
```

The final reconcile is conditional: it runs only after a dirty sixteenth
execution wave and never dispatches a seventeenth factory or future. Success
always requires both a clean projection and no unfinished hooks. If the wave
limit leaves an unfinished declaration, a new mount that needs preparation, or
an ongoing remount graph that would require another wave, the operation fails
terminally with
`ApplicationFaultReason::PreparationGraphUnstable`.

All preparations needed by the submitted projection finish before
`FrameSession::prepare()` and `ReactionPort::submit()`. A projection with
unfinished preparations never crosses provider handoff.

## Mount Fencing, Effects, and Panics

Factories and futures run outside the mount fence. Before dispatch, the runtime
authorizes the current mount generation; when the future completes, it verifies
that the generation is still mounted before accepting that completion into the
operation. A retired generation cannot record completion or publish a late
Signal write. Signal writes published before retirement remain authoritative.

Preparation is nontransactional. A Signal write that succeeds is authoritative,
even if a later preparation fails, the operation is cancelled, or stabilization
becomes unstable. The runtime does not roll back Signal writes or external
effects.

Factory and future panics retain the normal direct user-code behavior: they
unwind through the active `prepare()` or `react()` call and are not converted
to `ApplicationFault`. The existing supervised mount-task panic behavior is
unchanged.

## Failure, Cancellation, and Retry

An ordinary preparation error is a retryable
`ApplicationFault` with `ApplicationFaultStage::Preparation` and
`ApplicationFaultReason::Preparation`. The same `Application` remains
available for a later explicit `prepare()` or `react()`.

Dropping a pending pre-handoff operation is cancellation, not a successful
preparation. The caller receives no implicit retry, and all operation-local
completion is discarded. The next explicit operation runs every active hook
anew, not only the hook that failed or was cancelled.

Preparation loaders, meaning the factory and its returned future, have no
exactly-once guarantee. They must tolerate repeated execution, partial
execution, and cancellation. External effects need caller-provided business
idempotency and durable deduplication.

`PreparationGraphUnstable` uses the same preparation stage and is terminal.
Neither failure nor cancellation automatically starts `prepare()` or `react()`.

## Projection Checkpoints

`ProjectionSnapshot::is_prepared()` remains an observational checkpoint for a
published projection, and dirty state remains tracked separately. A synchronous
mount can publish a complete but provisional projection when it contains
preparation declarations. A rendered projection containing such declarations
is provisional until the current operation has prepared it.

After successful stabilization, the resulting projection is marked prepared.
A later Signal write can leave that checkpoint readable while marking it dirty.
Publishing another render with preparation declarations makes that new
projection provisional until its operation completes.

The prepared bit is not hook readiness and never skips work in a future
explicit operation. In particular, a prepared snapshot does not make a later
`react()` omit preparation.

## Legacy Boundary and Non-Goals

The legacy `ApplicationHost` / `ProviderPort` path does not run Component
preparation. If a render declares one, it fails closed before provider
execution with `ApplicationHostFault::ComponentPreparationsUnsupported`.

This design does not add:

- a global cache, resource subsystem, or readiness store;
- a root-maintained registry of descendant futures;
- automatic reactions after preparation success, failure, or cancellation;
- rollback for completed Signal writes or external effects; or
- a guarantee for detached work started by a factory.
