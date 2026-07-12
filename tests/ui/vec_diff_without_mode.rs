use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "bad_list")]
struct BadListView {
    #[view(diff)]
    items: Vec<String>,
}

fn main() {}
