//! Markdown strings carry instructions; XML nodes carry structured state.
//! Raw strings preserve literal XML examples, while XML node text is escaped.
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

- Read the recipient from the current `greeting_context` state.
- Say hello using the following response format:

```xml
<say>Hello &amp; welcome.</say>
```

Escape XML special characters in the response text.
"#;

#[component]
fn existing_prompt(system: String) -> Component {
    let request = String::from("## Request\n\nSay hello.\n");
    let recipient = "Mina <ops> & team";
    view! {
        #[system_once]
        { system }

        #[user]
        { request }

        #[user]
        greeting_context {
            recipient { "{recipient}" }
            language { "English" }
        }
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
