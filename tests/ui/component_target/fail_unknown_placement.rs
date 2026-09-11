use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[unknown]
        message { "invalid" }
    };
}
