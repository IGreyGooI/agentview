use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[assistant(repeat)]
        context { "invalid" }
    };
}
