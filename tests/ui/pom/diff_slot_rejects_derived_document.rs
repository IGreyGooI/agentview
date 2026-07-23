use agentview::pom::{DiffSlot, DiffStrategy};
use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(document)]
struct SystemPrompt {
    #[view(paragraph)]
    instruction: String,
}

fn main() {
    let document = SystemPrompt {
        instruction: "Do the task.".to_owned(),
    }
    .build_root()
    .unwrap();

    DiffSlot::present(DiffStrategy::Recursive, document);
}
