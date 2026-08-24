use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[diff]
        context { "invalid" }
    };
}
