use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(diff)]
    items: Vec<String>,
}

fn main() {}
