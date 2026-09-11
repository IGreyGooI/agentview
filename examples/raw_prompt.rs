//! Render an existing multiline prompt without parsing or escaping its contents.
//!
//! Run `cargo run --no-default-features --example raw_prompt`; no credentials are needed.

use std::io::{self, Write};

use agentview::{
    component::{prelude::*, ComponentHost},
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};

const SYSTEM_PROMPT: &str = r#"# Assistant

Keep the user's original formatting.

- Read the current context.
- Respond using the following example:

```xml
<say>Hello & welcome.</say>
```

The literal <say> tag is part of this prompt.
"#;

#[component]
fn existing_prompt(system: String) -> Component {
    let request = String::from("## Request\n\nSay hello.\n");
    view! {
        #[system_once]
        { system }

        #[user]
        { request }
    }
}

fn main() -> anyhow::Result<()> {
    let mut host = ComponentHost::new_root(existing_prompt, SYSTEM_PROMPT.to_owned());
    let rendered = host.render()?;
    let mut stdout = io::stdout().lock();
    for item in rendered
        .projection()
        .nodes()
        .iter()
        .flat_map(|node| node.items())
    {
        let (role, pom) = match item {
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::System,
                pom,
            } => ("system", pom),
            CanonicalInputItem::Message {
                role: ConversationRole::User,
                pom,
            } => ("user", pom),
            _ => continue,
        };
        writeln!(stdout, "[{role}]")?;
        stdout.write_all(render_pom_document(pom)?.as_bytes())?;
    }
    Ok(())
}
