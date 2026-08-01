//! Committed prompt state shared by provider-backed agents and external view apps.

use crate::pom::XmlName;
use crate::pom_cursor::UserDocumentCursor;
use crate::prompt_context::PromptContext;

/// The last successfully committed prompt context and local render baseline.
///
/// The prompt context is semantic durable state. The User-document cursor is
/// an owner-local POM rendering cache: it is deliberately omitted from serde
/// so a process replacement starts with one full User snapshot instead of
/// treating internal POM nodes as a long-lived storage schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentSession<I, CS> {
    context: PromptContext<I, CS>,
    #[serde(skip)]
    user_document_cursor: UserDocumentCursor,
}

impl<I, CS> AgentSession<I, CS> {
    /// Start a session from a prompt context with no user-document baseline.
    pub fn new(context: PromptContext<I, CS>) -> Self {
        Self {
            context,
            user_document_cursor: UserDocumentCursor::default(),
        }
    }

    /// The durable prompt context associated with this session.
    pub fn context(&self) -> &PromptContext<I, CS> {
        &self.context
    }

    pub(crate) fn context_mut(&mut self) -> &mut PromptContext<I, CS> {
        &mut self.context
    }

    /// Mutable view-model state without exposing the whole prompt context.
    pub fn context_state_mut(&mut self) -> &mut CS {
        self.context.context_state_mut()
    }

    /// Initialize the stable system prompt if it has not been set yet.
    pub fn set_system_once(&mut self, system: impl Into<crate::StorageString>) {
        self.context.set_system_once(system);
    }

    pub(crate) fn set_system_snapshot(&mut self, system: impl Into<crate::StorageString>) {
        self.context.set_system_snapshot(system);
    }

    /// Append one item to committed history.
    pub fn push_history(&mut self, item: I) {
        self.context.push_history(item);
    }

    /// Append items to committed history.
    pub fn extend_history(&mut self, items: impl IntoIterator<Item = I>) {
        self.context.extend_history(items);
    }

    /// Append one item to the mutable working set.
    pub fn push_working_set(&mut self, item: I) {
        self.context.push_working_set(item);
    }

    /// Replace the mutable working set without changing user-document baselines.
    pub fn replace_working_set(&mut self, items: Vec<I>) {
        self.context.replace_working_set(items);
    }

    /// Clear the mutable working set without changing user-document baselines.
    pub fn clear_working_set(&mut self) {
        self.context.clear_working_set();
    }

    /// Baselines for explicitly marked stateful slots in the last committed
    /// user-document lineage.
    pub fn user_document_cursor(&self) -> &UserDocumentCursor {
        &self.user_document_cursor
    }

    /// Forget every stateful user-document slot so the next publication sends
    /// each present slot in full.
    pub fn reset_user_document_cursor(&mut self) {
        self.user_document_cursor.clear();
    }

    /// Forget one stateful user-document slot by its prompt-facing role.
    pub fn forget_user_document_slot(&mut self, role: &XmlName) -> bool {
        self.user_document_cursor.remove(role)
    }

    /// Replace durable history and start a new user-document lineage.
    pub fn replace_history(&mut self, history: Vec<I>) {
        self.context.replace_history(history);
        self.reset_user_document_cursor();
    }

    pub(crate) fn set_user_document_cursor(&mut self, cursor: UserDocumentCursor) {
        self.user_document_cursor = cursor;
    }
}

#[cfg(test)]
mod tests {
    use super::AgentSession;
    use crate::{
        pom::{DiffSlot, DiffStrategy, Document, XmlNode},
        pom_cursor::UserDocumentCursor,
        pom_resolution::resolve_user_document,
        prompt_context::{PromptContext, Turn},
    };

    #[test]
    fn replacing_history_preserves_context_and_invalidates_the_user_document_cursor() {
        let mut context = PromptContext::<Turn, usize>::new("system");
        context.push_history(Turn::user("old history"));
        context.push_working_set(Turn::user("working context"));
        *context.context_state_mut() = 7;
        let mut session = AgentSession::new(context);
        let source = Document::build(|blocks| {
            blocks.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                XmlNode::try_build("agent_context", |_| Ok(())).unwrap(),
            ));
        });
        let (_, cursor) = resolve_user_document(source, &UserDocumentCursor::default()).unwrap();
        session.set_user_document_cursor(cursor);

        session.replace_history(vec![Turn::user("summary")]);

        assert_eq!(session.context().system(), Some("system"));
        assert_eq!(&*session.context().history()[0].text, "summary");
        assert_eq!(&*session.context().working_set()[0].text, "working context");
        assert_eq!(*session.context().context_state(), 7);
        assert!(session.user_document_cursor().is_empty());
    }

    #[test]
    fn session_can_forget_one_slot_or_reset_the_whole_cursor() {
        let context = PromptContext::<Turn>::without_system();
        let mut session = AgentSession::new(context);
        let source = Document::build(|blocks| {
            for role in ["agent_context", "workspace_session"] {
                blocks.xml_slot(DiffSlot::present(
                    DiffStrategy::Recursive,
                    XmlNode::try_build(role, |_| Ok(())).unwrap(),
                ));
            }
        });
        let (_, cursor) = resolve_user_document(source, &UserDocumentCursor::default()).unwrap();
        session.set_user_document_cursor(cursor);

        let agent_context = crate::pom::XmlName::try_from("agent_context").unwrap();
        assert!(session.forget_user_document_slot(&agent_context));
        assert_eq!(session.user_document_cursor().len(), 1);

        session.reset_user_document_cursor();
        assert!(session.user_document_cursor().is_empty());
    }

    #[test]
    fn serialized_session_round_trips_context_and_resets_user_cursor() {
        let mut context = PromptContext::<Turn, usize>::new("stable system");
        context.push_history(Turn::user("committed"));
        context.push_working_set(Turn::assistant("working"));
        *context.context_state_mut() = 9;
        let mut session = AgentSession::new(context);
        let source = Document::build(|blocks| {
            blocks.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                XmlNode::try_build("agent_context", |children| {
                    children.text(crate::pom::TextNode::new("snapshot"));
                    Ok(())
                })
                .unwrap(),
            ));
        });
        let (_, cursor) = resolve_user_document(source, &UserDocumentCursor::default()).unwrap();
        session.set_user_document_cursor(cursor);

        let encoded = serde_json::to_vec(&session).unwrap();
        let decoded: AgentSession<Turn, usize> = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded.context().system(), Some("stable system"));
        assert_eq!(decoded.context(), session.context());
        assert!(decoded.user_document_cursor().is_empty());
        assert!(!String::from_utf8(encoded)
            .unwrap()
            .contains("user_document_cursor"));
    }
}
