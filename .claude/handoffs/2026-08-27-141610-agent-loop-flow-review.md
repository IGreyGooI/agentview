# Handoff: AgentLoop Flow Review

## Session Metadata
- Created: 2026-08-27 14:16:10
- Project: /home/greygoo/runtime/agentview
- Branch: codex/pom-first-prompt-authoring
- Session duration: Multi-turn architecture review; exact duration not recorded

### Recent Commits (for context)
  - c6de13b refactor(component)!: clarify provider and loop ownership
  - 7f0ed34 feat!: replace mounted runtime with component provider execution
  - 9b9652b refactor(component): bind external control at harness root
  - 141dfb4 feat(chess): own the canonical playable example
  - 0625881 feat(component): add durable POM application runtime

## Handoff Chain

- **Continues from**: None (fresh start)
- **Supersedes**: None

> This is the first handoff for this task.

## Current State Summary

The team is designing the high-level `AgentLoop` public API above the existing one-reaction
`ApplicationHost`. Two specialist review rounds found API, lifetime, wake, cancellation, and
compatibility issues. No `AgentLoop` production implementation exists and no implementation plan
has started. The next session is intentionally scoped to one question only: define the Loop flow.
The user has now clarified that `stop` and sleep must be explicit requests; absence of either means
the Loop normally continues immediately. The role of explicit `continue_now()`, request conflicts,
and whether render-time policy persists are still open and must not be assumed.

## Codebase Understanding

## Architecture Overview

`ApplicationHost` owns exactly one complete Provider reaction. It renders one complete
`RenderedProjection`, calls `ProviderPort::execute`, consumes the Provider stream, drains ToolCall
lanes, finishes handlers, and cleans up. `AgentLoop` is proposed as the long-lived mechanical owner
outside that boundary. The mounted Component tree owns business state and Loop policy. Signal
dirty state, Loop disposition, and the explicit sticky wake epoch are separate. Background tasks
belong to a Component mount and may request progress only through `TaskWakeHandle`.

## Critical Files

| File | Purpose | Relevance |
|------|---------|-----------|
| `docs/agent-loop-public-api-review.md` | Candidate API contract and grouped review worklist | Primary design packet; Loop-flow wording is not frozen |
| `docs/engine.md` | Authoritative Engine architecture | Section 7 contains the earlier AgentLoop design and must be updated only after Loop flow is settled |
| `src/component/execution/application_host.rs` | Executes one complete reaction | Defines the non-reentrant boundary that AgentLoop must wrap |
| `src/component/host.rs` | Retained root Component host | Existing root ABI and render boundary |
| `src/component/signal.rs` | Signal dirty state and mount topology | Its current wake revision must not be reused as the explicit task wake epoch |
| `agentview-derive/src/component_attr.rs` | Rewrites Component hooks | Eventually needs shared `(HookSite, HookKind)` topology, but that is not this session's topic |

### Key Patterns Discovered

- `ProviderPort::execute` always receives a complete projection; dirty tracking is internal only.
- `ApplicationHost` deliberately completes one reaction and must never infer or own Loop policy.
- Render state is transactionally staged; failed candidates must retain the prior committed mount.
- Existing listener bindings are reaction-local, while the proposed hooks need mounted slots.
- Existing Tokio `JoinHandle` ownership requires explicit abort; dropping a handle detaches work.

## Work Completed

### Tasks Finished

- [x] Audited the current Chess and Component execution surfaces.
- [x] Ran API ergonomics, API surface, runtime flow, and wake-state specialist reviews.
- [x] Wrote the candidate public API contract and grouped actionable findings.
- [x] Confirmed that no production implementation of the proposed APIs exists.
- [x] Split the next discussion into a dedicated Loop-flow session.

## Files Modified

| File | Changes | Rationale |
|------|---------|-----------|
| `docs/agent-loop-public-api-review.md` | Added candidate API, runtime contracts, migration matrix, and review worklist | Preserve the architecture discussion for review before implementation |
| `.claude/handoffs/2026-08-27-141610-agent-loop-flow-review.md` | Added this scoped session handoff | Let a fresh session discuss only Loop flow |
| `src/bin/agentview.rs` | Pre-existing user modification; not changed by this work | Must remain untouched |

## Decisions Made

| Decision | Options Considered | Rationale |
|----------|-------------------|-----------|
| Component tree owns Loop policy | Host-owned reducer, separate application reducer, Component-owned policy | Keeps business lifecycle decisions with business state |
| `AgentLoop` is a mechanical outer owner | Put the loop inside `ApplicationHost`, add another middleware, outer owner | Preserves the existing one-reaction `ApplicationHost` boundary |
| Keep the authoring vocabulary | Reviewer renames versus `use_loop`, `continue_now`, `continue_on_wake`, `use_task`, `submit`, `wake.wake()` | These names were explicitly selected in prior discussion |
| Stop and sleep are explicit | Implicit stop, implicit sleep, explicit stop/sleep with immediate fallback | The user explicitly clarified this requirement |
| Review one topic per session | Resolve all review findings together versus scoped sessions | The combined review packet is too large to reason about safely in one discussion |

## Pending Work

## Immediate Next Steps

1. Draw the smallest Loop state machine using only `Continue`, `Sleep`, `Stop`, reaction completion,
   wake, and fault; do not discuss observer or error-type API details yet.
2. Decide whether `continue_now()` is needed when immediate continuation is already the fallback,
   and if so exactly which prior request it may override.
3. Decide conflict and persistence semantics: Stop versus Sleep, requests from multiple Components,
   render-time policy versus callback-time requests, and when all requests reset.

### Blockers/Open Questions

- [ ] Does a reaction start with an implicit `Continue`, or is `Continue` only a fallback applied
      after observing that no Component explicitly requested Stop or Sleep?
- [ ] What useful semantic remains for public `continue_now()` if Continue is already the fallback?
- [ ] If Stop and Sleep are both explicitly requested, which wins and why?
- [ ] Can one Component cancel another Component's explicit Sleep or Stop request?
- [ ] Is a render-time Sleep a persistent mounted policy, or only a request for the current reaction?
- [ ] At exactly which boundary are requests sampled and reset?

### Deferred Items

- Stale-wake observability and whether `wake()` returns a value: separate session after Loop flow.
- Public fault shape and diagnostic context: separate session.
- `run(self)` drop/cancellation contract: separate session.
- `ProviderEventSelector<E>` alias/newtype compatibility: separate session.
- Runtime race proofs, render transaction details, and migration sequencing: later implementation
  planning sessions after the public flow is frozen.

## Context for Resuming Agent

## Important Context

The user does not want all review findings discussed in one session. Keep the next session strictly
on Loop flow. Do not start implementation, write an implementation plan, or pull in observer,
selector, macro, Chess migration, or fault-surface decisions. Treat these as already established:
Component owns policy; `ApplicationHost` is one reaction; AgentLoop does not advance because a
Signal became dirty; wake is explicit and sticky; no call may re-enter an active reaction. Treat
this new user statement as authoritative: Stop and Sleep must be explicitly requested. Do not
silently retain the earlier fixed priority `Stop > ContinueNow > ContinueOnWake`; the next session
must derive the conflict rules from the flow model.

## Assumptions Made

- The user's phrase means that absence of explicit Stop/Sleep produces normal immediate progress.
- The exact role of `continue_now()` is deliberately open rather than inferred from its existing name.
- A new session will read this handoff before changing the candidate contract.

## Potential Gotchas

- `docs/agent-loop-public-api-review.md` previously stated a fixed disposition priority. Do not cite
  that as approved user intent; it is being reopened by this handoff.
- Render defaults and callback requests were introduced to solve retained-callback fencing, but they
  may be unnecessary or need different names after the Loop flow is simplified.
- Signal writes must never act as Continue or Wake.
- A wake during an active reaction may affect what happens after cleanup, but cannot re-enter it.
- The working tree contains an unrelated user change in `src/bin/agentview.rs`; do not revert it.

## Environment State

### Tools/Services Used

- Rust/Cargo workspace tooling for baseline verification by reviewers.
- Read-only specialist review agents for API, runtime, and concurrency analysis.
- `session-handoff` skill using its scaffold and validator scripts.

### Active Processes

- None.

### Environment Variables

- None required for this documentation-only session.

## Related Resources

- `docs/agent-loop-public-api-review.md`
- `docs/engine.md`, especially Section 7
- `src/component/execution/application_host.rs`
- `src/component/host.rs`
- Commit `c6de13b95edf2e599152e03392162d1b67e66d12`

---

**Security Reminder**: Before finalizing, run `validate_handoff.py` to check for accidental secret exposure.
