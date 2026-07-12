use agentview::prelude::{AgentSession, PromptContext, Turn};

fn main() {
    let context = PromptContext::<Turn>::new("system");
    let mut session = AgentSession::<Turn, (), String>::new(context);
    let replacement = PromptContext::<Turn>::new("replacement");
    let _old = std::mem::replace(session.context_mut(), replacement);
}
