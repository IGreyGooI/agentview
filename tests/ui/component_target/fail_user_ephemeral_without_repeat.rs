use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[user(ephemeral)]
        "Invalid."
    };
}
