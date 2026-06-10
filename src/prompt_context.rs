//! Cache-friendly prompt state for LLM conversations.
//!
//! [`PromptContext`] manages the two-zone model:
//! - **FROZEN ZONE**: stable system prompt + append-only history (provider-cached prefix)
//! - **LIVE ZONE**: pending user turn being assembled for the next call
//!
//! [`SharedPromptContext`] wraps `PromptContext` in `Arc<tokio::sync::Mutex<>>` so that
//! the context lives outside any future's stack frame.  When a [`tokio::task`] is
//! aborted the `Arc` clone held by the future is dropped, but the original `Arc`
//! (stored in `Agent.ctx`) is unaffected and the mutex is released automatically.
//! This eliminates the `mem::replace`-then-restore dance previously required.
//!
//! Panics are not handled — we don't care about state on panic.

use crate::StorageString;
use std::sync::Arc;

use anyhow::Error as AnyError;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

// ── Turn / Role ───────────────────────────────────────────────────────────────

/// A single committed message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: StorageString,
}

/// Conversation participant role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

// ── TurnTransform trait ───────────────────────────────────────────────────────

/// Controls how both sides of an exchange are stored in history after `commit()`.
///
/// Implement this to customise what gets appended to the frozen history zone.
/// Both methods default to identity (store as-is).  Return `None` to omit that
/// side from history entirely.
pub trait TurnTransform {
    /// Transform the pending user message before storing.
    /// Return `None` to omit the user turn from history.
    fn transform_user(&self, user_text: &str) -> Option<String> {
        Some(user_text.to_owned())
    }

    /// Transform the raw LLM response before storing as the assistant turn.
    /// Return `None` to omit the assistant turn from history.
    fn transform_assistant(&self, raw_response: &str) -> Option<String> {
        Some(raw_response.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitTransformStatus {
    Unchanged,
    Changed,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    pub user: CommitTransformStatus,
    pub assistant: CommitTransformStatus,
}

// ── Built-in transforms ───────────────────────────────────────────────────────

/// Stores both user and assistant turns unchanged.
/// Use for utility calls (tag/extract/intents) or any call where you want the
/// full raw response in history.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct IdentityTransform;

impl TurnTransform for IdentityTransform {}

// ── PromptContext ─────────────────────────────────────────────────────────────

/// Stateful prompt builder for a single NPC conversation session.
///
/// Invariants:
/// - `system` is set once and never modified (stable provider-cache prefix).
/// - `history` is append-only (committed turns are never changed or removed).
/// - Dynamic context is assembled in `pending` before each call, then
///   committed to `history` by [`crate::llm_call::AgentTurn::execute`].
///
/// Serde-serializable so it can be stored on `NpcState` and survive world
/// state snapshots / save-load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptContext {
    system: Option<StorageString>,
    history: Vec<Turn>,
    pending: Option<String>,
}

impl PromptContext {
    /// Create a new context with the given stable system prompt.
    pub fn new(system: impl Into<StorageString>) -> Self {
        Self {
            system: Some(system.into()),
            history: Vec::new(),
            pending: None,
        }
    }

    /// Create a context whose stable system prompt will be rendered lazily.
    pub fn without_system() -> Self {
        Self {
            system: None,
            history: Vec::new(),
            pending: None,
        }
    }

    /// Set the stable system prompt if it has not been initialized yet.
    pub fn set_system_once(&mut self, system: impl Into<StorageString>) {
        if self.system.is_none() {
            self.system = Some(system.into());
        }
    }

    pub fn has_system(&self) -> bool {
        self.system.is_some()
    }

    /// Minimal context for utility calls: system + single user message.
    /// Equivalent to `new(system)` followed by `push_user(user)`.
    pub fn simple(system: impl Into<StorageString>, user: impl Into<StorageString>) -> Self {
        let mut ctx = Self::new(system);
        ctx.push_user(user);
        ctx
    }

    /// Append `text` to the pending user turn.
    ///
    /// Multiple calls are joined with `"\n"`.  Creates a new pending turn if
    /// none exists.  The pending turn is sent as the final user message in
    /// [`build_request`].
    pub fn push_user(&mut self, text: impl Into<StorageString>) {
        let t: String = text.into().into();
        match &mut self.pending {
            Some(p) => {
                p.push('\n');
                p.push_str(&t);
            }
            None => self.pending = Some(t),
        }
    }

    /// Directly inject a committed turn into history (no pending involvement).
    ///
    /// Use this to pre-load prior conversation turns when re-using a `PromptContext`
    /// across sessions or when reconstructing context from `NpcState.dialogue_history`.
    /// Unlike `push_user` + `commit`, this does not affect `pending`.
    pub fn inject_history_turn(&mut self, role: Role, text: impl Into<StorageString>) {
        self.history.push(Turn {
            role,
            text: text.into(),
        });
    }

    /// Commit the current exchange to history using `transform`.
    ///
    /// - `transform.transform_user(pending)` → optional user turn in history
    /// - `transform.transform_assistant(raw_response)` → optional assistant turn
    /// - `pending` is cleared after commit
    ///
    /// Called automatically by [`crate::llm_call::AgentTurn::execute`] —
    /// callers rarely need this directly.
    pub fn commit(&mut self, raw_response: &str, transform: &dyn TurnTransform) -> CommitReport {
        let user = if let Some(pending) = self.pending.take() {
            match transform.transform_user(&pending) {
                Some(user_text) => {
                    let status = if user_text == pending {
                        CommitTransformStatus::Unchanged
                    } else {
                        CommitTransformStatus::Changed
                    };
                    self.history.push(Turn {
                        role: Role::User,
                        text: user_text.into(),
                    });
                    status
                }
                None => CommitTransformStatus::Dropped,
            }
        } else {
            CommitTransformStatus::Dropped
        };

        let assistant = if let Some(asst_text) = transform.transform_assistant(raw_response) {
            let status = if asst_text == raw_response {
                CommitTransformStatus::Unchanged
            } else {
                CommitTransformStatus::Changed
            };
            self.history.push(Turn {
                role: Role::Assistant,
                text: asst_text.into(),
            });
            status
        } else {
            CommitTransformStatus::Dropped
        };

        CommitReport { user, assistant }
    }

    /// Build the request parts for `llm_client::stream_text`.
    ///
    /// Returns `(system, prior_history, current_user_message)`.
    ///
    /// # Panics
    ///
    /// Panics if `pending` is `None` — you must call `push_user` before
    /// `build_request`.
    pub fn build_request(&self) -> (&str, &[Turn], &str) {
        let system = self.system.as_deref().expect(
            "PromptContext::build_request called with no system prompt; call set_system_once first",
        );
        let user_msg = self.pending.as_deref().expect(
            "PromptContext::build_request called with no pending user message; call push_user first",
        );
        (system, &self.history, user_msg)
    }

    /// The stable system prompt (immutable).
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// The committed history (append-only).
    pub fn history(&self) -> &[Turn] {
        &self.history
    }

    /// Wrap this `PromptContext` in a [`SharedPromptContext`].
    ///
    /// Shorthand for `Arc::new(TokioMutex::new(self))`.
    pub fn into_shared(self) -> SharedPromptContext {
        Arc::new(TokioMutex::new(self))
    }
}

// ── SharedPromptContext ───────────────────────────────────────────────────────

/// A `PromptContext` stored behind `Arc<tokio::sync::Mutex<>>`.
///
/// Used by [`crate::agent::Agent`] so that the context outlives any `JoinHandle::abort()`.
/// When the BT runtime aborts a task, all `Arc` clones held by the future are dropped
/// and the mutex is released — the original `Arc` stored in `Agent.ctx` remains intact.
///
/// ## Serde
///
/// Serializes/deserializes as a plain [`PromptContext`] by locking the mutex.
/// Use `#[serde(with = "crate::prompt_context::shared_ctx_serde")]` on the field.
pub type SharedPromptContext = Arc<TokioMutex<PromptContext>>;

// ── AgentTurnError ───────────────────────────────────────────────────────────

/// Error returned by `AgentTurn::execute()`.
///
/// Always carries the `PromptContext` back so the caller can restore NPC state
/// or retry.  The context is at its pre-call state (uncommitted).
///
/// TODO: derive `thiserror::Error` once we need a proper error enum for matching.
/// For now a plain struct keeps the dependency surface minimal.
pub struct AgentTurnError {
    /// The context as it was before the failed call (uncommitted).
    /// Restore this to `NpcState.context` to continue the conversation.
    pub ctx: PromptContext,
    /// The underlying failure.
    pub source: AnyError,
}

impl std::fmt::Debug for AgentTurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentTurnError: {:#}", self.source)
    }
}

impl std::fmt::Display for AgentTurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_user_joins_with_newline() {
        let mut ctx = PromptContext::new("sys");
        ctx.push_user("memory snapshot");
        ctx.push_user("player says hello");
        let (_, _, user) = ctx.build_request();
        assert_eq!(user, "memory snapshot\nplayer says hello");
    }

    #[test]
    fn identity_transform_stores_both_turns() {
        let mut ctx = PromptContext::new("sys");
        ctx.push_user("hello");
        ctx.commit("world", &IdentityTransform);
        assert_eq!(ctx.history.len(), 2);
        assert_eq!(ctx.history[0].role, Role::User);
        assert_eq!(&*ctx.history[0].text, "hello");
        assert_eq!(ctx.history[1].role, Role::Assistant);
        assert_eq!(&*ctx.history[1].text, "world");
        assert!(ctx.pending.is_none());
    }

    #[test]
    fn history_grows_across_turns() {
        let mut ctx = PromptContext::new("sys");
        ctx.push_user("turn 1");
        ctx.commit("reply 1", &IdentityTransform);
        ctx.push_user("turn 2");
        ctx.commit("reply 2", &IdentityTransform);
        assert_eq!(ctx.history.len(), 4);
    }

    #[test]
    fn simple_factory() {
        let ctx = PromptContext::simple("system", "user message");
        let (sys, hist, user) = ctx.build_request();
        assert_eq!(sys, "system");
        assert!(hist.is_empty());
        assert_eq!(user, "user message");
    }

    #[test]
    #[should_panic(expected = "no pending user message")]
    fn build_request_panics_without_pending() {
        let ctx = PromptContext::new("sys");
        ctx.build_request();
    }

    #[test]
    fn into_shared_roundtrip() {
        let ctx = PromptContext::new("test system");
        let shared = ctx.into_shared();
        let inner = shared.blocking_lock();
        assert_eq!(inner.system(), Some("test system"));
    }
}
