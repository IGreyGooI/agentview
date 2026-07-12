use agentview::prelude::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "bad_replace_list")]
struct BadReplaceList {
    #[view(diff(replace, seq))]
    items: Vec<String>,
}

fn main() {}
