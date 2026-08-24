use agentview::component::prelude::*;

struct NonCloneProps {
    label: String,
}

#[component]
fn signal_component(props: NonCloneProps) -> Component {
    let state = use_signal(|| 0_u64);
    let count = state.with(|count| *count).unwrap();
    let label = props.label;

    view! { counter { label: label, count: count, } }
}

fn main() {}
