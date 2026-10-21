use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[user(ephemeral, repeat, ephemeral)]
        "Invalid."
    };
}
