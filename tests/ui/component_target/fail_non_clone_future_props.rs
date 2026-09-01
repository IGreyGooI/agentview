use agentview::component::prelude::*;

struct NonCloneProps {
    label: String,
}

#[component]
fn future_component(props: NonCloneProps) -> Component {
    use_future(|| async {});
    let label = props.label;
    view! { future_component { "{label}" } }
}

fn main() {}
