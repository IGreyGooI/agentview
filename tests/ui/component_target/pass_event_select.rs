use agentview::component::{prelude::*, ComponentHost};

#[derive(Clone, Copy)]
struct Props;

fn routes(_text: EventInput<TextTurnEvent>) -> Component {
    view! { routes_ready { "selected" } }
}

#[component]
fn turn(_props: Props, events: EventInput<ProviderEvent>) -> Component {
    let text = events.select(ProviderEvent::TEXT);

    view! {
        routes(text)
    }
}

#[component]
fn application_root(props: Props, events: EventInput<ProviderEvent>) -> Component {
    view! { turn(props, events) }
}

fn main() {
    let _components = ComponentHost::new(application_root, Props);
}
