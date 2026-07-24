use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(diff)]
    context: String,
}

fn main() {
    let _ = UserDocument {
        context: "plain text is not an XML diff root".to_owned(),
    }
    .build_root();
}
