use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(attr, name = "not:xml")]
    value: String,
}

fn main() {}
