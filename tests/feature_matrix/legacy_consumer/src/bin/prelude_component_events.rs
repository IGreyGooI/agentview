use agentview::component::prelude::ComponentEvents;

#[derive(ComponentEvents)]
enum LegacyEvent {
    Message(String),
}

fn main() {}
