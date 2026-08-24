use agentview::component::prelude::*;

fn child() -> Component {
    view! { child { "value" } }
}

fn main() {
    let _ = view! {
        #[diff(slot = "context")]
        child()
    };
}
