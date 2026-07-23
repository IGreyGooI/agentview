use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(text, name = "ignored")]
    value: String,
}

fn main() {}
