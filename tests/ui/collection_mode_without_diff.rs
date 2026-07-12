use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "bad_list")]
struct BadList {
    #[view(set)]
    items: Vec<String>,
}

fn main() {}
