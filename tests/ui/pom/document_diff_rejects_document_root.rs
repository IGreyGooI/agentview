use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct NestedDocument {
    #[view(paragraph)]
    text: String,
}

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(diff)]
    context: NestedDocument,
}

fn main() {
    let _ = UserDocument {
        context: NestedDocument {
            text: "a document cannot be a diff slot value".to_owned(),
        },
    }
    .build_root();
}
