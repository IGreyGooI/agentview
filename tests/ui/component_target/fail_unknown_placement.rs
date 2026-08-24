use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[assistant]
        message { "invalid" }
    };
}
