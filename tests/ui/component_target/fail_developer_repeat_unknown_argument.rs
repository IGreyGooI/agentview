use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[developer(unknown)]
        context { "invalid" }
    };
}
