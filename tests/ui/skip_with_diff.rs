use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "bad_skip")]
struct BadSkip {
    #[view(skip, diff)]
    runtime_state: String,
}

fn main() {}
