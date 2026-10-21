use agentview::component::prelude::*;

fn main() {
    let _ = view! {
        #[system_once(ephemeral, repeat)]
        "Invalid."
    };
}
