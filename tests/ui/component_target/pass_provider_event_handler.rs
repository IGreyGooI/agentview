use std::convert::Infallible;

use agentview::component::prelude::*;

#[component]
fn provider_handlers(label: String) -> Component {
    let first_label = label.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let label = first_label.clone();
        async move {
            let _ = label;
            Ok::<(), Infallible>(())
        }
    });

    agentview::component::prelude::use_provider_event_handler(
        ProviderEvent::TEXT,
        move |_event| {
            let label = label.clone();
            async move {
                let _ = label;
                Ok::<(), Infallible>(())
            }
        },
    );

    view! { provider_handlers {} }
}

fn main() {
    let _component = provider_handlers(String::from("capture"));
}
