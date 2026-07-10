use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(text, diff)]
    value: String,
}

fn main() {}
