use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "bad_replace")]
struct BadReplace {
    #[view(replace)]
    state: String,
}

fn main() {}
