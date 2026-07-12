//! Durable prompt state shared by provider-backed agents and external view apps.

use crate::prompt_context::PromptContext;

/// The last successfully committed prompt context and its rendered view baseline.
#[derive(Debug, Clone)]
pub struct AgentSession<I, CS, V> {
    context: PromptContext<I, CS>,
    view_cursor: Option<V>,
}

impl<I, CS, V> AgentSession<I, CS, V> {
    /// Start a session from a prompt context with no rendered view baseline.
    pub fn new(context: PromptContext<I, CS>) -> Self {
        Self {
            context,
            view_cursor: None,
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

    /// Replace the mutable working set without changing the view baseline.
    pub fn replace_working_set(&mut self, items: Vec<I>) {
        self.context.replace_working_set(items);
    }

    /// Clear the mutable working set without changing the view baseline.
    pub fn clear_working_set(&mut self) {
        self.context.clear_working_set();
    }

    /// The last view successfully rendered into this prompt lineage.
    pub fn view_cursor(&self) -> Option<&V> {
        self.view_cursor.as_ref()
    }

    /// Replace durable history and start a new rendered-view lineage.
    pub fn replace_history(&mut self, history: Vec<I>) {
        self.context.replace_history(history);
        self.view_cursor = None;
    }

    pub(crate) fn set_view_cursor(&mut self, view: V) {
        self.view_cursor = Some(view);
    }
}

#[cfg(test)]
mod tests {
    use super::AgentSession;
    use crate::prompt_context::{PromptContext, Turn};

    #[test]
    fn replacing_history_preserves_context_and_invalidates_the_view_cursor() {
        let mut context = PromptContext::<Turn, usize>::new("system");
        context.push_history(Turn::user("old history"));
        context.push_working_set(Turn::user("working context"));
        *context.context_state_mut() = 7;
        let mut session = AgentSession::new(context);
        session.set_view_cursor("view".to_owned());

        session.replace_history(vec![Turn::user("summary")]);

        assert_eq!(session.context().system(), Some("system"));
        assert_eq!(&*session.context().history()[0].text, "summary");
        assert_eq!(&*session.context().working_set()[0].text, "working context");
        assert_eq!(*session.context().context_state(), 7);
        assert!(session.view_cursor().is_none());
    }
}
