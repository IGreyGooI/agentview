use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(attr, element)]
    value: String,
}

fn main() {}
