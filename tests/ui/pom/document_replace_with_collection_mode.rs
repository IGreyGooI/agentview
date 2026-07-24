use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(diff(replace, append))]
    items: Vec<String>,
}

fn main() {}
