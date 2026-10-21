use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[user(repeat, ephemeral = true)]
        "Invalid."
    };
}
