use std::fmt::Write as _;

use crate::{
    pom_renderer::render_pom_document,
    transcript::{AssistantPhase, CanonicalInputItem, ConversationRole, InstructionAuthority},
};

use super::{ProviderFault, RenderedProjection};

pub(crate) fn render_projection_prompt(
    projection: &RenderedProjection,
) -> Result<String, ProviderFault> {
    let capacity = projection
        .nodes()
        .iter()
        .map(|node| node.items().len())
        .sum();
    let mut sections = Vec::with_capacity(capacity);
    for node in projection.nodes() {
        for item in node.items() {
            sections.push(render_prompt_item(item)?);
        }
    }
    Ok(sections.join("\n\n"))
}

fn render_prompt_item(item: &CanonicalInputItem) -> Result<String, ProviderFault> {
    let (heading, content) = match item {
        CanonicalInputItem::Instruction { authority, pom } => {
            let heading = match authority {
                InstructionAuthority::System => "System",
                InstructionAuthority::Developer => "Developer",
            };
            (heading.to_owned(), render_pom(pom)?)
        }
        CanonicalInputItem::Message { role, pom } => {
            let heading = match role {
                ConversationRole::User => "User",
                ConversationRole::Assistant => "Assistant",
            };
            (heading.to_owned(), render_pom(pom)?)
        }
        CanonicalInputItem::AssistantText { text, phase } => {
            let heading = match phase {
                Some(AssistantPhase::Commentary) => "Assistant Commentary",
                Some(AssistantPhase::FinalAnswer) | None => "Assistant",
            };
            (heading.to_owned(), text.clone())
        }
        CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } => (
            format!("Tool Call {name} ({call_id})"),
            raw_arguments.clone(),
        ),
        CanonicalInputItem::ToolResult { call_id, content } => {
            (format!("Tool Result ({call_id})"), content.clone())
        }
        CanonicalInputItem::ProviderExtension(extension) => {
            return Err(ProviderFault::model_rejected(format!(
                "provider-neutral prompt renderer cannot render provider extension {}:{} v{}",
                extension.provider(),
                extension.capability(),
                extension.schema_version()
            )));
        }
    };

    let mut rendered = String::with_capacity(heading.len() + content.len() + 5);
    writeln!(&mut rendered, "## {heading}").expect("writing to String cannot fail");
    rendered.push('\n');
    rendered.push_str(&content);
    Ok(rendered)
}

fn render_pom(pom: &crate::pom::ResolvedDocument) -> Result<String, ProviderFault> {
    render_pom_document(pom).map_err(|_| {
        ProviderFault::model_rejected(
            "provider-neutral prompt renderer could not render canonical POM",
        )
    })
}
