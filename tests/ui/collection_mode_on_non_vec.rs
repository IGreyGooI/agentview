use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "bad_scalar")]
struct BadScalar {
    #[view(diff(key = "id"))]
    item: String,
}

fn main() {}
