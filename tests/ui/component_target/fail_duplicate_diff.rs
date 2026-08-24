use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[diff(slot = "first")]
        #[diff(slot = "second")]
        context { "invalid" }
    };
}
