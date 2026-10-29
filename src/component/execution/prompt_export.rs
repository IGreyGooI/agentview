//! Complete current-view prompt export with an empty conversation history.

use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

use crate::{
    pom::{BlockChildren, ResolvedDocument},
    pom_renderer::{render_pom_document, PomRenderError},
    transcript::{
        AssistantPhase, AssistantTextStatus, CanonicalInputItem, ConversationRole,
        InstructionAuthority, ASSISTANT_OUTPUT_INTERRUPTED_MARKER,
    },
};

use super::{RenderedProjection, ToolDefinition};

/// A current view could not be represented as a provider-neutral prompt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PromptExportError {
    #[error(transparent)]
    Pom(#[from] PomRenderError),
    #[error("provider-private extension cannot be exported as a provider-neutral prompt")]
    UnsupportedProviderExtension,
}

impl RenderedProjection {
    /// Render the complete current view for a recipient with no prior history.
    ///
    /// Includes current System instructions, all current declared content and
    /// action records, complete native action definitions and input schemas,
    /// then current ephemeral prompts. Diff and repeat markers do not omit any
    /// current content. No session history or provider-private state is read.
    ///
    /// This only renders this projection. It does not refresh Component state,
    /// run preparation, submit a reaction, or execute an action. For an
    /// Application, explicitly prepare before taking its projection when newer
    /// state is needed. The returned String can be passed to an external model
    /// or decision API without an AgentView runtime.
    ///
    /// The prompt is a view for the model, not a machine-readable format. Read
    /// typed projection items and native tools when structured data is needed.
    pub fn render_prompt(&self) -> Result<String, PromptExportError> {
        PromptView::from_projection(self).render()
    }
}

/// The complete declaration, before any delivery reconciliation or omission.
/// Kept typed through assembly; rendered prompt text is never parsed.
struct PromptView<'a> {
    items: Vec<CanonicalInputItem>,
    ephemeral: Vec<CanonicalInputItem>,
    tools: Vec<NativeAction<'a>>,
}

#[derive(Serialize)]
struct NativeAction<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
    strict: bool,
}

impl<'a> From<&'a ToolDefinition> for NativeAction<'a> {
    fn from(tool: &'a ToolDefinition) -> Self {
        Self {
            name: tool.name(),
            description: tool.description(),
            input_schema: tool.input_schema(),
            strict: tool.strict(),
        }
    }
}

impl<'a> PromptView<'a> {
    fn from_projection(projection: &'a RenderedProjection) -> Self {
        let mut system = BlockChildren::new();
        let mut items = Vec::new();
        let mut ephemeral = Vec::new();
        for node in projection.nodes() {
            for (index, item) in node.items().iter().enumerate() {
                match item {
                    CanonicalInputItem::Instruction {
                        authority: InstructionAuthority::System,
                        pom,
                    } => system.extend(pom.children().clone()),
                    _ if node.is_ephemeral_item(index) => ephemeral.push(item.clone()),
                    _ => items.push(item.clone()),
                }
            }
        }
        if !system.is_empty() {
            items.insert(
                0,
                CanonicalInputItem::instruction(
                    InstructionAuthority::System,
                    ResolvedDocument::new(system),
                ),
            );
        }
        Self {
            items,
            ephemeral,
            tools: projection
                .native_tools()
                .iter()
                .map(NativeAction::from)
                .collect(),
        }
    }

    fn render(&self) -> Result<String, PromptExportError> {
        let mut sections = self
            .items
            .iter()
            .map(render_prompt_item)
            .collect::<Result<Vec<_>, _>>()?;
        if !self.tools.is_empty() {
            // JSON encoding is for the action schema, never for rendered POM.
            let catalog = serde_json::to_value(&self.tools)
                .expect("native action metadata consists only of valid JSON values");
            sections.push(format!("## Native Actions\n\n{catalog:#}"));
        }
        for item in &self.ephemeral {
            sections.push(render_prompt_item(item)?);
        }
        Ok(sections.join("\n\n"))
    }
}

pub(super) fn render_prompt_item(item: &CanonicalInputItem) -> Result<String, PromptExportError> {
    let (heading, content) = match item {
        CanonicalInputItem::Instruction { authority, pom } => {
            let heading = match authority {
                InstructionAuthority::System => "System",
                InstructionAuthority::Developer => "Developer",
            };
            (heading.to_owned(), render_pom_document(pom)?)
        }
        CanonicalInputItem::Message { role, pom } => {
            let heading = match role {
                ConversationRole::User => "User",
                ConversationRole::Assistant => "Assistant",
            };
            (heading.to_owned(), render_pom_document(pom)?)
        }
        CanonicalInputItem::AssistantText {
            text,
            phase,
            status,
        } => {
            let heading = match phase {
                Some(AssistantPhase::Commentary) => "Assistant Commentary",
                Some(AssistantPhase::FinalAnswer) | None => "Assistant",
            };
            let content = match status {
                AssistantTextStatus::Sealed => text.clone(),
                AssistantTextStatus::Interrupted => {
                    format!("{text}\n\n## User\n\n{ASSISTANT_OUTPUT_INTERRUPTED_MARKER}")
                }
            };
            (heading.to_owned(), content)
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
        CanonicalInputItem::ProviderExtension(_) => {
            return Err(PromptExportError::UnsupportedProviderExtension);
        }
    };
    let mut rendered = String::with_capacity(heading.len() + content.len() + 5);
    writeln!(&mut rendered, "## {heading}").expect("writing to String cannot fail");
    rendered.push('\n');
    rendered.push_str(&content);
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        component::execution::RenderedProjectionNode, pom::Document,
        pom_resolution::resolve_artifact_document, transcript::ProviderExtension,
    };
    use serde_json::json;

    fn document(text: &str) -> ResolvedDocument {
        resolve_artifact_document(Document::from_raw_text(text.to_owned())).unwrap()
    }

    #[test]
    fn complete_view_normalizes_system_keeps_records_and_moves_ephemeral_last() {
        let first_system = document("Use only the current ticket.");
        let second_system = document("Check the request version before acting.");
        let ephemeral = CanonicalInputItem::message(
            ConversationRole::User,
            document("Current workspace: <& no history needed"),
        );
        let policy = CanonicalInputItem::instruction(
            InstructionAuthority::Developer,
            document("Choose one of the declared actions."),
        );
        let state = CanonicalInputItem::message(
            ConversationRole::User,
            document("Ticket t-7, version 3, verdict pending."),
        );
        let example = CanonicalInputItem::message(
            ConversationRole::Assistant,
            document("An explicitly authored example."),
        );
        let call = CanonicalInputItem::tool_call(
            "previous-call",
            "submit_verdict",
            r#"{"request_id":"t-7","version":2}"#,
        )
        .unwrap();
        let result = CanonicalInputItem::tool_result(
            "previous-call",
            r#"{"accepted":false,"reason":"stale_version"}"#,
        )
        .unwrap();
        let schema = json!({
            "type": "object",
            "properties": {
                "request_id": {"type":"string"},
                "version": {"type":"integer"}
            },
            "required": ["request_id", "version"],
            "additionalProperties": false
        });
        let tool = ToolDefinition::new(
            "submit_verdict",
            "Return a verdict for the displayed request and version.",
            schema.clone(),
        )
        .unwrap()
        .with_strict(true);
        let projection = RenderedProjection::with_native_tools(
            vec![
                RenderedProjectionNode::new(
                    "parent",
                    vec![
                        ephemeral.clone(),
                        policy.clone(),
                        CanonicalInputItem::instruction(
                            InstructionAuthority::System,
                            first_system.clone(),
                        ),
                        state.clone(),
                    ],
                )
                .with_ephemeral_items(vec![0])
                .with_repeat_items(vec![1]),
                RenderedProjectionNode::new(
                    "child",
                    vec![
                        CanonicalInputItem::instruction(
                            InstructionAuthority::System,
                            second_system.clone(),
                        ),
                        example.clone(),
                        call.clone(),
                        result.clone(),
                    ],
                ),
            ],
            vec![tool],
        )
        .unwrap();
        let before = projection.clone();
        let view = PromptView::from_projection(&projection);

        let mut combined = first_system.children().clone();
        combined.extend(second_system.children().clone());
        assert_eq!(
            view.items,
            vec![
                CanonicalInputItem::instruction(
                    InstructionAuthority::System,
                    ResolvedDocument::new(combined)
                ),
                policy,
                state,
                example,
                call,
                result,
            ]
        );
        assert_eq!(view.ephemeral, vec![ephemeral]);
        let [action] = view.tools.as_slice() else {
            panic!("complete action catalog")
        };
        assert_eq!(action.name, "submit_verdict");
        assert_eq!(
            action.description,
            "Return a verdict for the displayed request and version."
        );
        assert_eq!(action.input_schema, &schema);
        assert!(action.strict);
        view.render().unwrap();
        projection.render_prompt().unwrap();
        assert_eq!(
            projection, before,
            "export cannot change the complete declaration"
        );
    }

    #[test]
    fn opaque_extension_is_rejected_instead_of_silently_losing_context() {
        let extension = CanonicalInputItem::provider_extension(
            ProviderExtension::new("vendor", "opaque", 1, json!({"proof":"private"})).unwrap(),
        );
        let projection = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "root",
            vec![extension],
        )])
        .unwrap();
        assert_eq!(
            projection.render_prompt(),
            Err(PromptExportError::UnsupportedProviderExtension)
        );
    }
}
