use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[system_once(repeat)]
        context { "invalid" }
    };
}
