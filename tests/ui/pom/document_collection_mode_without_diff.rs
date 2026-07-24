use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(append)]
    items: Vec<String>,
}

fn main() {}
