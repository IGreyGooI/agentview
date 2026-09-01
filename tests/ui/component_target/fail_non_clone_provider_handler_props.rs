use std::convert::Infallible;

use agentview::component::prelude::*;

struct NonCloneProps {
    label: String,
}

#[component]
fn provider_handler_component(props: NonCloneProps) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let label = props.label.clone();
        async move {
            let _ = label;
            Ok::<(), Infallible>(())
        }
    });

    view! {}
}

fn main() {}
