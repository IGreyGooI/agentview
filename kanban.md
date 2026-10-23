# Kanban

## Todo

- [ ] Fix conditional native-tool parameters in `#[tool]` (P2).
  Reproduction: `#[tool] fn example(#[cfg(any())] value: i32) -> Result<(), ToolError> { Ok(()) }`.
  This fails with `E0609`: the generated Args field is removed by `cfg`, but
  the private handler loses the attribute and the invocation still reads
  `args.value`. Define consistent handling for parameter-level `cfg` and
  `cfg_attr`, either rejecting unsupported conditions explicitly or preserving
  them across the Args fields, private handler, and invocation. Add a compile
  regression in `tests/ui/tool`. `Option<T>` parameters remain supported and
  do not remove fields. Implementation: `agentview-derive/src/tool_attr.rs`.
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
- [x] Record durable history for each retained stdin daemon lifetime across
  client connections and foreground CLI invocations (implementation `a7c22a2`).
  `history --daemon [latest|SESSION_ID]` reads sessions without starting a daemon
  or replaying actions. CLI run/session IDs link the daemon records with raw
  foreground histories; complete results and views retain execution order
  independently of each connection's view diff. Execution outcomes and socket
  writes are separate; a successful write does not confirm a client read.
  Accepted or pending requests are not completed actions: `request_not_executed`
  records a known rejection, while missing execution outcomes remain unknown.
  Missing session completion preserves already recorded results. See
  [Stdin Actions](HELP.md#stdin-actions), the
  [runtime contract](docs/engine.md#skill--cli-frontend), and
  [daemon history process tests](tests/component_api_daemon_history.rs).
  Validation: 32 focused stdin tests and 8 process tests passed; packaged
  Semantic Memory CLI evidence is in
  [daemon history verification](../forgotten-city/artifacts/daemon-history-20261023T093343Z/README.md).
