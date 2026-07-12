use agentview::prelude::{PromptContext, Turn};

fn main() {
    let mut context = PromptContext::<Turn>::new("system");
    context.replace_history(Vec::new());
}
