use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(kind = "context")]
struct Context {
    value: String,
}

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(block, diff)]
    context: Context,
}

fn main() {}
