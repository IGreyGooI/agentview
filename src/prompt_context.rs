//! Cache-friendly prompt state for agent conversations.
//!
//! [`PromptContext`] owns durable context state:
//! - **FROZEN ZONE**: stable system prompt + append-only history.
//! - **WORKING SET**: mutable, not-yet-frozen context items managed by an
//!   [`AgentViewModel`](crate::agent::AgentViewModel) implementation.
//!
//! `agentview` owns the transaction lifecycle. The concrete transcript item
//! type `I` is application/provider-defined.

use crate::StorageString;
use std::sync::Arc;

use anyhow::Error as AnyError;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

// ── Default text transcript item ──────────────────────────────────────────────

/// Default text-only transcript item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: StorageString,
}

impl Turn {
    pub fn user(text: impl Into<StorageString>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }

    pub fn assistant(text: impl Into<StorageString>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
        }
    }
}

/// Conversation participant role for the default [`Turn`] transcript item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

// ── Text commit policy ────────────────────────────────────────────────────────

/// Controls how text user/assistant messages are committed by the default text
/// [`AgentViewModel`](crate::agent::DefaultAgentViewModel).
pub trait TurnTransform {
    /// Transform the rendered user message before storing.
    /// Return `None` to omit the user text from history.
    fn transform_user(&self, user_text: &str) -> Option<String> {
        Some(user_text.to_owned())
    }

    /// Transform the raw assistant text before storing.
    /// Return `None` to omit the assistant text from history.
    fn transform_assistant(&self, raw_response: &str) -> Option<String> {
        Some(raw_response.to_owned())
    }
}

/// Stores both user and assistant text turns unchanged.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct IdentityTransform;

impl TurnTransform for IdentityTransform {}

// ── PromptContext ─────────────────────────────────────────────────────────────

/// Durable prompt/transcript state.
///
/// `I` is the app/provider-defined transcript item type. The default `I = Turn`
/// preserves the text-only behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptContext<I = Turn, CS = ()> {
    system: Option<StorageString>,
    history: Vec<I>,
    working_set: Vec<I>,
    context_state: CS,
}

impl<I, CS> PromptContext<I, CS>
where
    CS: Default,
{
    /// Create a new context with the given stable system prompt.
    pub fn new(system: impl Into<StorageString>) -> Self {
        Self {
            system: Some(system.into()),
            history: Vec::new(),
            working_set: Vec::new(),
            context_state: CS::default(),
        }
    }

    /// Create a context whose stable system prompt will be rendered lazily.
    pub fn without_system() -> Self {
        Self {
            system: None,
            history: Vec::new(),
            working_set: Vec::new(),
            context_state: CS::default(),
        }
    }
}

impl<I, CS> PromptContext<I, CS> {
    /// Set the stable system prompt if it has not been initialized yet.
    pub fn set_system_once(&mut self, system: impl Into<StorageString>) {
        if self.system.is_none() {
            self.system = Some(system.into());
        }
    }

    pub fn has_system(&self) -> bool {
        self.system.is_some()
    }

    /// The stable system prompt.
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// The committed, append-only history.
    pub fn history(&self) -> &[I] {
        &self.history
    }

    /// Mutable context items not yet frozen into history.
    pub fn working_set(&self) -> &[I] {
        &self.working_set
    }

    /// VM-owned prompt/session state.
    pub fn context_state(&self) -> &CS {
        &self.context_state
    }

    /// Mutable VM-owned prompt/session state.
    pub fn context_state_mut(&mut self) -> &mut CS {
        &mut self.context_state
    }

    pub fn push_history(&mut self, item: I) {
        self.history.push(item);
    }

    pub fn extend_history(&mut self, items: impl IntoIterator<Item = I>) {
        self.history.extend(items);
    }

    pub fn push_working_set(&mut self, item: I) {
        self.working_set.push(item);
    }

    pub fn replace_working_set(&mut self, items: Vec<I>) {
        self.working_set = items;
    }

    pub fn clear_working_set(&mut self) {
        self.working_set.clear();
    }

    /// Wrap this `PromptContext` in a [`SharedPromptContext`].
    pub fn into_shared(self) -> SharedPromptContext<I, CS> {
        Arc::new(TokioMutex::new(self))
    }
}

impl<CS> PromptContext<Turn, CS>
where
    CS: Default,
{
    /// Minimal text context: system + one working-set user item.
    pub fn simple(system: impl Into<StorageString>, user: impl Into<StorageString>) -> Self {
        let mut ctx = Self::new(system);
        ctx.push_user(user);
        ctx
    }

    /// Compatibility helper for text-only contexts.
    pub fn push_user(&mut self, text: impl Into<StorageString>) {
        self.working_set.push(Turn::user(text));
    }

    /// Compatibility helper for text-only contexts.
    pub fn inject_history_turn(&mut self, role: Role, text: impl Into<StorageString>) {
        match role {
            Role::User => self.history.push(Turn::user(text)),
            Role::Assistant => self.history.push(Turn::assistant(text)),
        }
    }
}

// ── SharedPromptContext ───────────────────────────────────────────────────────

/// A `PromptContext` stored behind `Arc<tokio::sync::Mutex<>>`.
pub type SharedPromptContext<I = Turn, CS = ()> = Arc<TokioMutex<PromptContext<I, CS>>>;

// ── AgentTurnError ───────────────────────────────────────────────────────────

/// Error returned by `AgentTurn::execute()`.
///
/// Always carries the `PromptContext` back so the caller can restore state or
/// retry. The context is at its pre-call state.
pub struct AgentTurnError<I = Turn, CS = ()> {
    pub ctx: PromptContext<I, CS>,
    pub source: AnyError,
}

impl<I, CS> std::fmt::Debug for AgentTurnError<I, CS> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentTurnError: {:#}", self.source)
    }
}

impl<I, CS> std::fmt::Display for AgentTurnError<I, CS> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_context_push_user_goes_to_working_set() {
        let mut ctx: PromptContext = PromptContext::new("sys");
        ctx.push_user("memory snapshot");
        ctx.push_user("player says hello");
        assert_eq!(ctx.working_set.len(), 2);
        assert_eq!(ctx.working_set[0].role, Role::User);
        assert_eq!(&*ctx.working_set[0].text, "memory snapshot");
    }

    #[test]
    fn history_grows_with_explicit_append() {
        let mut ctx: PromptContext = PromptContext::new("sys");
        ctx.extend_history([Turn::user("turn 1"), Turn::assistant("reply 1")]);
        ctx.extend_history([Turn::user("turn 2"), Turn::assistant("reply 2")]);
        assert_eq!(ctx.history.len(), 4);
    }

    #[test]
    fn simple_factory() {
        let ctx: PromptContext = PromptContext::simple("system", "user message");
        assert_eq!(ctx.system(), Some("system"));
        assert!(ctx.history.is_empty());
        assert_eq!(ctx.working_set.len(), 1);
        assert_eq!(&*ctx.working_set[0].text, "user message");
    }

    #[test]
    fn into_shared_roundtrip() {
        let ctx: PromptContext = PromptContext::new("test system");
        let shared = ctx.into_shared();
        let inner = shared.blocking_lock();
        assert_eq!(inner.system(), Some("test system"));
    }
}
