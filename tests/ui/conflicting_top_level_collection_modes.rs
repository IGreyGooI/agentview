use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(diff, append, set)]
    items: Vec<String>,
}

fn main() {}
