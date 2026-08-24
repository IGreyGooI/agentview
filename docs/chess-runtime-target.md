# AgentView Chess Example

Last reviewed: 2026-08-24

[`examples/chess_agentview`](../examples/chess_agentview) is the canonical
runnable Chess example. It is a live application, not a mock Provider demo or
an alternative engine lifecycle.

## Runtime Boundary

```text
Chess match runner
  -> ComponentReactionRuntime
  -> chess_agent Component tree
       -> Agent identity and private reasoning policy
       -> Current game state and semantic diff
       -> Legal actions and response contract
       -> Referee and match feedback
  -> Responses ProviderPort for White
  -> Stockfish UCI process for Black
```

The example owns match policy, Chess rules, UCI orchestration, status output,
and JSONL evidence. The shared runtime owns Component identity, rendering,
event dispatch, Provider history, and reaction cleanup.

Component state is authoritative. Provider continuation, prompt-cache state,
and wire history remain private, discardable Provider optimizations. A model
move is accepted only after parsing and legal-move validation; infrastructure
faults do not become Chess decisions.

## Run

The example makes paid model requests. Configure the Provider credentials in
the ignored `.env` file or process environment and install Stockfish. Then run:

```bash
cargo run --example chess_agentview
```

The default engine path is `/usr/games/stockfish`. Override it with
`AGENTVIEW_STOCKFISH_BIN` when necessary. The example writes a bounded JSONL
trace and reports its path without logging credentials, raw reasoning, or the
complete Provider request.

## Offline Evidence

```bash
cargo check --example chess_agentview
```

The shared Component and Provider contracts are defined by
[`engine.md`](engine.md) and
[`provider-port-application-host-boundary.md`](provider-port-application-host-boundary.md).
