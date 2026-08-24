use agentview::component::prelude::*;

fn child() -> Component {
    view! { child { "value" } }
}

fn main() {
    let component = child();
    let _ = view! {
        #[diff(slot = "context")]
        {component}
    };
}
