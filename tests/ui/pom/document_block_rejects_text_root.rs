use agentview::agent_view::AgentView;

#[derive(agentview::AgentView)]
#[agent_view(document)]
struct InvalidDocument {
    #[view(block)]
    text: String,
}

fn main() {
    let _ = InvalidDocument {
        text: "not a block".to_owned(),
    }
    .build_root();
}
