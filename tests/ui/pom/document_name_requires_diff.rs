use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(name = "task", paragraph)]
    task: String,
}

fn main() {}
