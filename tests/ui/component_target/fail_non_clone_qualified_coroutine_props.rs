use agentview::component::prelude::{component, view, Component, CoroutineInbox};

struct NonCloneProps {
    label: String,
}

#[component]
fn coroutine_component(props: NonCloneProps) -> Component {
    let _coroutine =
        agentview::component::prelude::use_coroutine(1, |_inbox: CoroutineInbox<()>| async {});
    let label = props.label;
    view! { coroutine_component { "{label}" } }
}

fn main() {}
