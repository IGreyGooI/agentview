# agentview

`agentview` is an AI/AX runtime: Agent Interface plus Agent Experience for language agents.

At its core, `agentview` treats agent interaction as a ViewModel problem. It renders structured views from application state, computes diffs between turns, routes stream and tool events into stateful sinks, validates typed actions, and commits successful turns into history.

## Quick Start

```rust
use agentview::prelude::*;
```

The `prelude` is the recommended stable entry point. Module paths remain public for advanced adapters and escape hatches while the crate is still evolving.
