# Example Prompts

Use Markdown strings for instructions and XML nodes for structured application
state. Both belong in the same Component projection:

```rust
use agentview::component::prelude::*;

#[component]
fn review_prompt(status: String) -> Component {
    let instructions = "# Review assistant\n\nReport the current review status in one sentence.";
    view! {
        #[system_once]
        { instructions }

        review_state {
            status { "{status}" }
        }
    }
}
```

| Content | Authoring | Rendering and updates |
| --- | --- | --- |
| Instructions, policies, user requests | `{ prompt }` with `String`, `&String`, or `&str` | Preserves Markdown, whitespace, and literal XML examples. Changed text is sent in full. |
| Structured state, records, action feedback | XML nodes such as `review_state { status { "{status}" } }` | Escapes field values and supports semantic XML updates and removals. |
| XML response examples inside instructions | A fenced XML example within `{ prompt }` | Remains literal text; it does not become a state node or register an action. |

Use `{ prompt }` for a complete multiline string. `"{prompt}"` is a formatted
paragraph with inline validation, and `state { "{value}" }` is text inside an XML
node. A disappearing ordinary user/developer raw string does not retract earlier
instructions from model history. System strings follow the separate
`#[system_once]` snapshot replacement and clearing rules. `#[developer(repeat)]`
resends the whole current instruction on every submitted frame.

Run the credential-free example to inspect both raw Markdown and escaped XML:

```sh
cargo run --no-default-features --example raw_prompt
```

- [`raw_prompt.rs`](raw_prompt.rs) puts a literal `<say>` response example in a
  Markdown instruction and escapes the recipient value in XML state.
- [`debug_prompt.rs`](debug_prompt.rs) uses Markdown instructions beside XML task
  state and captures the exact Responses request.
- [`support_preparation.rs`](support_preparation.rs) prepares Markdown support
  policy and structured account context before the model turn.
- [`chess_agentview/chess_application.rs`](chess_agentview/chess_application.rs)
  uses Markdown action rules beside XML board state and attempt feedback. Model
  actions still follow the registered XML output protocol.
