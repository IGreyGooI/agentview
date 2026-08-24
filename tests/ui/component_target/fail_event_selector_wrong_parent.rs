use agentview::component::prelude::*;

#[derive(ComponentEvents)]
enum FirstEvent {
    Text(String),
}

#[derive(ComponentEvents)]
enum SecondEvent {
    Clock(u64),
}

#[component]
fn invalid(events: EventInput<FirstEvent>) -> Component {
    let _ = events.select(SecondEvent::CLOCK);
    view! {}
}

fn main() {}
