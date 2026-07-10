use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(flatten, diff)]
    value: String,
}

fn main() {}
