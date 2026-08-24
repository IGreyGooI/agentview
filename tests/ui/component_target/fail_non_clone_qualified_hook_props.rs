use agentview::component::prelude::{component, view, Component};

struct NonCloneProps {
    label: String,
}

#[component]
fn signal_component(props: NonCloneProps) -> Component {
    let state = agentview::component::prelude::use_signal(|| 0_u64);
    let count = state.with(|count| *count).unwrap();
    let label = props.label;

    view! { counter { label: label, count: count, } }
}

fn main() {}
