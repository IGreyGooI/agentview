use agentview::component::prelude::*;

#[component]
fn invalid() -> Component {
    let state = use_signal(|| String::from("borrowed"));
    let _borrowed: &str = state.with(|value| value.as_str()).unwrap();

    view! { invalid { "borrow escaped" } }
}

fn main() {}
