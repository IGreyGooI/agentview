use agentview::component::prelude::*;

fn diff_prompt() -> Component {
    view! {
        #[user]
        #[diff(slot = "agent_context")]
        chess_context { "ready" }
    }
}

fn main() {
    let _ = diff_prompt();
}
