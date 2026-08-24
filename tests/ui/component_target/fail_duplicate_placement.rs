use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[user]
        #[developer]
        message { "invalid" }
    };
}
