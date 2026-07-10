use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(comment, diff)]
    value: String,
}

fn main() {}
