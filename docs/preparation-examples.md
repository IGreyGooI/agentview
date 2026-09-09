# Preparation In Business Examples

`use_preparation` declares business work required before a model sees its next
Frame. The caller drives the application with `run()` and consumes it with
`shutdown()` afterward. Each mounted Component can contribute a preparation;
the runtime awaits all of them and reconciles their Signal writes before
submitting to the provider.

## Support Inbox

The [support example](../examples/support_preparation.rs) uses the live OpenAI
Responses provider. Configure `OPENAI_API_KEY` and then run it; for custom
gateways, follow the [live example configuration](../README.md#live-examples),
including an `OPENAI_BASE_URL` ending in `/v1` and `SSL_CERT_FILE` when the
gateway uses a custom CA:

```sh
cargo run --no-default-features --example support_preparation
```

It produces one JSON reply draft per fixture ticket on successful model
completion. The inbox uses a real Tokio channel and the account and policy
loaders use asynchronous filesystem reads. The model response is live, so draft
text and token use vary by run. Drafts remain in memory and are printed; the
example does not send customer messages.

The [fixtures](../examples/data/support) describe two tickets from different
accounts: an expired Team export and a recent Starter export that should still
be available.

| Component | Required work | Lifetime |
| --- | --- | --- |
| `support_agent` | Await the next inbox ticket when none is active | Retains the ticket until normal reply completion |
| `account_context` | Read account data and find the ticket's account | New mount for each ticket |
| `support_policy` | Read the export recovery and escalation rules | Cached snapshot for this application session |

The root begins with no active ticket. Its preparation awaits `recv()`; it does
not call the model while the inbox is empty. When a ticket arrives, it writes a
Signal and reconciliation mounts the account Component. The account Component's
new preparation also runs before the first handoff. A missing account or an
unreadable policy returns a preparation error with no model request.

Preparations in the same execution wave run concurrently. Independent
Components contribute to the same readiness barrier, and the runtime does not
reconcile a later wave or submit a Frame until every loader in the current wave
succeeds. The first observed loader error drops unfinished sibling futures.
Dependent work stays sequential inside one loader or is expressed by mounting a
child after its inputs exist, as the ticket does for its account context here.

The text handler retains the model's completed text. `use_reaction_completion`
validates it and adds a draft only after the entire reaction has completed
normally. It then clears the active ticket; reconciliation unmounts its account
Component. The next `run()` iteration waits for another ticket and loads that
ticket's account, so cached account data cannot leak from the previous ticket.

When every sender is dropped and the inbox has drained, `recv()` returns `None`.
The root requests `ExitReason::Completed`, and `run()` exits without submitting
an empty or terminal Frame. A host can also use `application.exit_handle()` to
request `Requested` while the inbox is still open; this interrupts the pending
preparation. The owner still calls `shutdown()`.

## Retry And Service Boundaries

The example retains a dequeued ticket before awaiting any context loads. A
preparation error or cancellation therefore leaves that ticket available to a
subsequent `run()` on the same Application. A cancelled empty-inbox wait does
not consume a ticket. A new reaction clears any uncommitted answer from a
previous failed reaction. `run()` returns errors; it does not retry them itself.

The policy cache is an explicit session snapshot, not a cache supplied by the
hook. For changing policy, use a version or expiry check in the loader. A
successfully loaded account is cached for that ticket mount. Failed account or
policy loads leave the cache unset, so a later `run()` retries them on the same
mount. Neither cache is durable across process restarts.

These boundaries map directly to service integrations:

- Replace the in-process sender with an application adapter that supplies webhook
  or queue work. For durable delivery, acknowledge the job only after persisting
  the reply draft, using the ticket ID as an idempotency key.
- Replace `read_json` with repository or service reads while keeping Signal
  publication after successful loading.
- The runnable example already uses a real `ReactionPort`; readiness remains in
  the Components. Keep customer delivery as an explicit business operation
  separate from drafting.

Deterministic tests, including the scripted provider and fixture scenarios, are
kept outside the runnable source under
[`tests/examples/support_preparation.rs`](../tests/examples/support_preparation.rs).
They cover both accounts, blocked inbox cancellation, context-load failure and
retry, normal queue drain, host exit, and incomplete reply handling without
API credentials:

```sh
cargo test --no-default-features --example support_preparation
```

For a smaller lifecycle example, see [`frame_agent`](../examples/frame_agent.rs).
For an engine-owned coroutine whose work gates each model turn, see
[`chess_agentview`](chess-loop-explained.md). Skill and Plugin examples use
`ExternalApplication::observe()` and `act()` for explicit caller exchanges.
