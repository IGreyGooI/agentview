use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(markdown = "paragraph")]
struct MarkdownContext {
    #[view(text)]
    text: String,
}

#[derive(AgentView)]
#[agent_view(document)]
struct UserDocument {
    #[view(diff)]
    context: MarkdownContext,
}

fn main() {
    let _ = UserDocument {
        context: MarkdownContext {
            text: "Markdown cannot be a diff boundary".to_owned(),
        },
    }
    .build_root();
}
