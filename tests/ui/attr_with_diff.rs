use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(attr, diff)]
    value: String,
}

fn main() {}
