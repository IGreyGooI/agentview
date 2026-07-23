use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "not:xml")]
struct InvalidView {
    value: String,
}

fn main() {}
