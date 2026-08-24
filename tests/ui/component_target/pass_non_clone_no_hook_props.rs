use agentview::component::prelude::*;

struct NonCloneProps {
    label: String,
}

#[component]
fn no_hooks(props: NonCloneProps) -> Component {
    let label = props.label;
    view! { message { label: label, } }
}

fn main() {
    let _component = no_hooks(NonCloneProps {
        label: String::from("owned once"),
    });
}
