use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[diff(slot = String::from("context"))]
        context { "invalid" }
    };
}
