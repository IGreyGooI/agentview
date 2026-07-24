use agentview::agent_view::AgentView;

#[derive(agentview::AgentView)]
#[agent_view(markdown = "paragraph")]
struct NestedParagraph {
    #[view(text)]
    text: String,
}

#[derive(agentview::AgentView)]
#[agent_view(markdown = "paragraph")]
struct InvalidParagraph {
    #[view(xml)]
    nested: NestedParagraph,
}

fn main() {
    let _ = InvalidParagraph {
        nested: NestedParagraph {
            text: "not XML".to_owned(),
        },
    }
    .build_root();
}
