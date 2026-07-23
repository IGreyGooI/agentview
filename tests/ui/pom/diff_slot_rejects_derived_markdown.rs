use agentview::pom::{DiffSlot, DiffStrategy};
use agentview::AgentView;

#[derive(AgentView)]
#[agent_view(markdown = "paragraph")]
struct Instruction {
    #[view(text)]
    text: String,
}

fn main() {
    let paragraph = Instruction {
        text: "Do the task.".to_owned(),
    }
    .build_root()
    .unwrap();

    DiffSlot::present(DiffStrategy::Recursive, paragraph);
}
