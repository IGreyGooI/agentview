use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[user(repeat, repeat)]
        context { "invalid" }
    };
}
