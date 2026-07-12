//! Application API for externally controlled AgentView loops.
//!
//! This is the chat/CLI/daemon/skill sibling of the provider-backed
//! `AgentTurn` path. It does not call a model provider. It captures the
//! application view, builds the current turn prompt, accepts an external reply,
//! and commits that reply through caller-provided parsing/apply functions.

use std::marker::PhantomData;

use thiserror::Error;

use crate::agent::AgentViewModel;
use crate::agent_session::AgentSession;
use crate::control::ControlReply;
use crate::llm_call::TurnSink;
use crate::prompt_context::{PromptContext, Turn};
use crate::view_awake::{ViewAwake, ViewAwakeHandle, ViewEpoch};
use crate::view_state::{ViewSnapshot, ViewTurnId, ViewUpdate};
use crate::StorageString;

/// Stateful AgentView app controlled by an external chat, CLI, daemon, or skill loop.
pub struct AgentViewApp<VM, I = Turn, TurnOutput = ()>
where
    VM: AgentViewModel<I, TurnOutput>,
{
    view_model: VM,
    source: VM::Source,
    session: AgentSession<I, VM::ContextState, VM::View>,
    awake: ViewAwake,
    latest_turn_id: Option<ViewTurnId>,
    next_turn_index: u64,
    _turn_output: PhantomData<fn() -> TurnOutput>,
}

impl<VM, I, TurnOutput> AgentViewApp<VM, I, TurnOutput>
where
    VM: AgentViewModel<I, TurnOutput>,
{
    /// Create a session and the app-side awake handle for the underlying source.
    pub fn new(
        view_model: VM,
        source: VM::Source,
        ctx: PromptContext<I, VM::ContextState>,
    ) -> (Self, ViewAwakeHandle) {
        let (awake, awake_handle) = ViewAwake::new();
        (
            Self {
                view_model,
                source,
                session: AgentSession::new(ctx),
                awake,
                latest_turn_id: None,
                next_turn_index: 1,
                _turn_output: PhantomData,
            },
            awake_handle,
        )
    }

    /// Current awake epoch observed by the session.
    pub fn current_epoch(&self) -> ViewEpoch {
        self.awake.current_epoch()
    }

    /// The latest turn id emitted by `observe`, `hook`, or `act_with_sink`.
    pub fn latest_turn_id(&self) -> Option<&str> {
        self.latest_turn_id.as_deref()
    }

    /// The durable prompt session owned by this app.
    pub fn session(&self) -> &AgentSession<I, VM::ContextState, VM::View> {
        &self.session
    }

    /// Mutable access to the durable prompt session owned by this app.
    pub fn session_mut(&mut self) -> &mut AgentSession<I, VM::ContextState, VM::View> {
        &mut self.session
    }

    /// Capture a full view snapshot and current turn prompt.
    pub async fn observe(
        &mut self,
        task: impl Into<String>,
    ) -> anyhow::Result<ViewSnapshot<VM::View, VM::TurnPrompt>> {
        self.capture_snapshot(task.into()).await
    }

    /// Wait until the application awakes the view source after `epoch`, then capture a full snapshot.
    pub async fn hook(
        &mut self,
        epoch: ViewEpoch,
        task: impl Into<String>,
    ) -> anyhow::Result<ViewSnapshot<VM::View, VM::TurnPrompt>> {
        self.awake.wait_after(epoch).await;
        self.capture_snapshot(task.into()).await
    }

    /// Parse an external reply, apply it to the app source/context, and return the next full update.
    pub async fn act_with_sink<S, F>(
        &mut self,
        turn_id: &str,
        reply: ControlReply,
        sink: S,
        apply: F,
        next_task: impl Into<String>,
    ) -> anyhow::Result<ViewUpdate<VM::View, VM::TurnPrompt>>
    where
        S: TurnSink<ControlReply>,
        F: FnOnce(
            &mut AgentSession<I, VM::ContextState, VM::View>,
            &VM::Source,
            S::Output,
        ) -> anyhow::Result<()>,
    {
        self.ensure_current_turn(turn_id)?;

        let base_epoch = self.awake.current_epoch();
        let mut sink = Box::new(sink);
        sink.on_event(reply).await;
        let sink_output = sink.finish().await;

        apply(&mut self.session, &self.source, sink_output)?;
        self.awake.handle().awake();

        let snapshot = self.capture_snapshot(next_task.into()).await?;
        Ok(ViewUpdate::full(base_epoch, snapshot))
    }

    fn ensure_current_turn(&self, turn_id: &str) -> Result<(), AgentViewAppError> {
        let Some(expected) = &self.latest_turn_id else {
            return Err(AgentViewAppError::NoActiveTurn);
        };

        if expected == turn_id {
            Ok(())
        } else {
            Err(AgentViewAppError::StaleTurnId {
                expected: expected.clone(),
                actual: turn_id.into(),
            })
        }
    }

    async fn capture_snapshot(
        &mut self,
        task: String,
    ) -> anyhow::Result<ViewSnapshot<VM::View, VM::TurnPrompt>> {
        loop {
            let view_epoch = self.awake.current_epoch();
            let turn_id = self.next_turn_id();
            let view = self.view_model.capture_view(&self.source).await;
            let turn_prompt = self
                .view_model
                .build_turn_prompt(self.session.context(), &turn_id, task.clone())
                .await?;

            if self.awake.current_epoch() == view_epoch {
                self.session.set_view_cursor(view.clone());
                self.latest_turn_id = Some(turn_id.clone());
                self.next_turn_index += 1;
                return Ok(ViewSnapshot::new(view_epoch, turn_id, view, turn_prompt));
            }
        }
    }

    fn next_turn_id(&self) -> StorageString {
        format!("turn-{}", self.next_turn_index).into()
    }
}

#[derive(Debug, Error)]
pub enum AgentViewAppError {
    #[error("no active turn")]
    NoActiveTurn,

    #[error("stale turn id: expected {expected}, got {actual}")]
    StaleTurnId {
        expected: ViewTurnId,
        actual: StorageString,
    },
}
