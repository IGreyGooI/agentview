# Kanban

## Todo

- [ ] Preserve panic semantics at the external CLI speculative-worker boundary.
  `src/bin/agentview.rs:2888-2953` currently converts a panicked
  `spawn_blocking` candidate into ordinary speculative taint and continues with
  a rebuilt replacement. Distinguish panic `JoinError` from ordinary candidate
  failure, clean up the owned sessions, and propagate the original panic
  payload instead of continuing the owner lifecycle.
- [ ] Preserve panic semantics at the daemon authentication-task boundary.
  `src/bin/agentview.rs:2439-2467` currently logs every authentication
  `JoinError` and continues serving with the same daemon state. Distinguish
  cancellation from panic and ensure a task panic terminates or unwinds the
  daemon owner rather than being downgraded to a connection failure.

## Done

- [x] Implement and verify operation-scoped Component preparation
  (`use_preparation`). See
  [Component Preparation](docs/component-preparation-design.md).
- [x] Run preparation factories and futures outside the mount fence, with
  synchronous Signal-access, stale-mount, and direct-panic regression coverage.
- [x] Reconcile after 16 preparation waves without dispatching a 17th wave;
  verify both final-write convergence and the no-handoff limit failure.
- [x] Superseded changed-key readiness invalidation and `A -> B -> A` retry
  coverage: `use_preparation` has no key or cross-operation readiness cache.
