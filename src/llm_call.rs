//! Agent turn abstraction — [`AgentTurn`] builder and the [`TurnSink`] trait.
//!
//! `AgentTurn` owns one model-backed agent interaction. It is the transaction
//! boundary around a single request/response: take an already-rendered
//! [`AgentTurnRequest`], ask an application-provided [`LLMExecutor`] to execute
//! it, route output into turn sinks, and return an [`AgentTurnOutcome`].
//!
//! It is deliberately **not** the agent loop, not application/world state, and
//! not a provider adapter.
//!
//! Every model-backed agent turn goes through an application-provided
//! [`LLMExecutor`], which owns provider streaming and observability details for
//! this crate. The turn routes executor-defined events into one primary
//! [`TurnSink`] plus optional side-effect sinks and returns an
//! [`AgentTurnOutcome`].
//!
//! ## Responsibility split
//!
//! - [`AgentTurn`] owns the executor/sink transaction for one model request.
//!   It does not mutate prompt history.
//! - [`LLMExecutor`] owns provider capabilities: streaming text, non-streaming
//!   completion, native provider tools, provider hooks, retries, API shape, and
//!   the concrete event type emitted to sinks.
//! - [`TurnSink`] owns per-turn output state for an executor-defined event type.
//!   `agentview` provides [`TextTurnEvent`] for plain text streaming, but native
//!   tool-call event schemas belong to the application/provider adapter.
//! - [`AgentTurnObserver`] is passive observability. It must not change control
//!   flow. Future native tool control points should use a separate hook API.
//!
//! ## TurnSink contract
//!
//! - [`on_event`][TurnSink::on_event] — called for each executor-defined turn event
//! - [`finish`][TurnSink::finish] — called once the executor succeeds;
//!   consumes the sink and returns its typed per-turn output
//!
//! Executors may emit zero events. They still return an [`ExecutorCommit`] so
//! the caller's view model can decide what becomes durable history.
//!
//! ## Ownership transfer
//!
//! `AgentTurn` does not own prompt history. The caller's
//! [`AgentViewModel`](crate::agent::AgentViewModel) owns request construction
//! and commit policy.

use crate::prompt_context::Turn;
use crate::stream_parser::HermesParser;
use crate::StorageString;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AgentTurnRequest<I = Turn> {
    pub call_id: StorageString,
    pub system: String,
    pub history: Vec<I>,
    pub user: String,
    pub model: StorageString,
    pub max_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ExecutorCommit<I = Turn> {
    pub append: Vec<I>,
}

impl<I> ExecutorCommit<I> {
    pub fn new(append: impl IntoIterator<Item = I>) -> Self {
        Self {
            append: append.into_iter().collect(),
        }
    }

    pub fn empty() -> Self {
        Self { append: Vec::new() }
    }
}

impl ExecutorCommit<Turn> {
    pub fn text(text: impl Into<StorageString>) -> Self {
        Self {
            append: vec![Turn::assistant(text)],
        }
    }
}

pub struct AgentTurnOutcome<I, O> {
    pub executor_commit: ExecutorCommit<I>,
    pub sink_output: O,
}

#[derive(Debug, Clone)]
pub enum TextTurnEvent {
    TextDelta(String),
    TextComplete(String),
}

#[derive(Debug, Clone)]
pub enum AgentTurnEvent {
    SystemPromptRendered {
        call_id: StorageString,
        text: String,
    },
    UserPromptRendered {
        call_id: StorageString,
        text: String,
    },
    AssistantCompleted {
        call_id: StorageString,
        text: String,
    },
    Failed {
        call_id: StorageString,
        error: String,
    },
    Aborted {
        call_id: StorageString,
    },
    IgnoredStreamItem {
        call_id: StorageString,
        item: String,
    },
}

#[async_trait::async_trait]
pub trait AgentTurnObserver: Send + Sync {
    async fn on_agent_turn_event(&self, event: AgentTurnEvent);
}

pub type AgentTurnObserverHandle = Arc<dyn AgentTurnObserver>;

#[async_trait::async_trait]
pub trait LLMExecutor<I = Turn, E = TextTurnEvent>: Send + Sync {
    /// Execute one model turn and return the transcript items to append.
    ///
    /// Implementors may emit any application/provider event type `E` into the
    /// primary sink and side sinks. Completion-style executors may emit no
    /// events and simply return an [`ExecutorCommit`]. `AgentTurn` is
    /// responsible for calling `finish(...)`; context commit is owned by the
    /// caller's view model.
    ///
    /// Native provider tools belong behind this boundary. If an adapter wants
    /// sinks to observe tool calls/results, it defines `E` accordingly.
    async fn execute_llm<S>(
        &self,
        request: AgentTurnRequest<I>,
        sink: &mut S,
        side_sinks: &mut Vec<Box<dyn TurnSink<E, Output = ()>>>,
    ) -> anyhow::Result<ExecutorCommit<I>>
    where
        S: TurnSink<E> + Send;
}

#[cfg(debug_assertions)]
struct AgentTurnDropGuard {
    call_id: StorageString,
    observers: Vec<AgentTurnObserverHandle>,
    completed: bool,
}

#[cfg(debug_assertions)]
impl AgentTurnDropGuard {
    fn new(call_id: StorageString, observers: Vec<AgentTurnObserverHandle>) -> Self {
        Self {
            call_id,
            observers,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

#[cfg(debug_assertions)]
impl Drop for AgentTurnDropGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        let call_id = self.call_id.clone();
        let observers = self.observers.clone();
        tokio::spawn(async move {
            notify_observers(&observers, AgentTurnEvent::Aborted { call_id }).await;
        });
    }
}

pub(crate) async fn notify_observers(observers: &[AgentTurnObserverHandle], event: AgentTurnEvent) {
    for observer in observers {
        observer.on_agent_turn_event(event.clone()).await;
    }
}

/// Stateful receiver for one model-backed agent turn.
///
/// Pure side-effect sinks can use `Output = ()`. Parsing sinks can return typed
/// per-turn state for the agent loop to inspect after history commit.
#[async_trait::async_trait]
pub trait TurnSink<E = TextTurnEvent>: Send {
    type Output: Send;

    /// Called for each executor-defined turn event.
    async fn on_event(&mut self, event: E);

    /// Called once the executor succeeds. Consumes the sink so owned per-turn
    /// state can be returned.
    async fn finish(self: Box<Self>) -> Self::Output;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTurnSink;

#[async_trait::async_trait]
impl<E: Send + 'static> TurnSink<E> for NoopTurnSink {
    type Output = ();

    async fn on_event(&mut self, _event: E) {}

    async fn finish(self: Box<Self>) -> Self::Output {}
}

/// [`HermesParser`] as a [`TurnSink`].
///
/// Each chunk is fed to the parser incrementally (`feed`).
/// On finish, `finalize()` is called to flush remaining buffered state.
/// All meaningful output comes via hooks registered on the parser before it
/// is handed to [`AgentTurn`].
#[async_trait::async_trait]
impl TurnSink<TextTurnEvent> for HermesParser {
    type Output = ();

    async fn on_event(&mut self, event: TextTurnEvent) {
        match event {
            TextTurnEvent::TextDelta(chunk) => self.feed(&chunk).await,
            TextTurnEvent::TextComplete(_) => {}
        }
    }

    async fn finish(mut self: Box<Self>) -> Self::Output {
        self.finalize().await;
    }
}

/// A pending agent turn created by an application runtime.
///
/// `T: TurnTransform` controls how both sides of the exchange are committed to
/// the [`PromptContext`]'s history after a successful call.
pub struct AgentTurn<R, S = NoopTurnSink, I = Turn, E = TextTurnEvent>
where
    R: LLMExecutor<I, E>,
    S: TurnSink<E>,
{
    runtime: R,
    request: AgentTurnRequest<I>,
    sink: S,
    side_sinks: Vec<Box<dyn TurnSink<E, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
    event: std::marker::PhantomData<E>,
}

impl<R, I, E> AgentTurn<R, NoopTurnSink, I, E>
where
    R: LLMExecutor<I, E> + Clone + Send + Sync + 'static,
    I: Send + 'static,
    E: Send + 'static,
{
    pub fn new(runtime: R, request: AgentTurnRequest<I>) -> Self {
        Self {
            runtime,
            request,
            sink: NoopTurnSink,
            side_sinks: Vec::new(),
            observers: Vec::new(),
            event: std::marker::PhantomData,
        }
    }
}

impl<R, S, I, E> AgentTurn<R, S, I, E>
where
    R: LLMExecutor<I, E> + Clone + Send + Sync + 'static,
    S: TurnSink<E> + Send + 'static,
    I: Send + 'static,
    E: Send + 'static,
{
    pub fn with_sink<S2>(self, sink: S2) -> AgentTurn<R, S2, I, E>
    where
        S2: TurnSink<E> + Send + 'static,
    {
        AgentTurn {
            runtime: self.runtime,
            request: self.request,
            sink,
            side_sinks: self.side_sinks,
            observers: self.observers,
            event: std::marker::PhantomData,
        }
    }

    pub(crate) fn with_observers(
        mut self,
        observers: impl IntoIterator<Item = AgentTurnObserverHandle>,
    ) -> Self {
        self.observers.extend(observers);
        self
    }

    /// Register a side-effect sink that receives streamed text but returns no
    /// typed output to the caller.
    pub fn with_side_sink(mut self, sink: impl TurnSink<E, Output = ()> + 'static) -> Self {
        self.side_sinks.push(Box::new(sink));
        self
    }

    /// Register a pre-boxed side-effect sink.
    pub fn with_side_sink_boxed(
        mut self,
        sink: Box<dyn TurnSink<E, Output = ()> + 'static>,
    ) -> Self {
        self.side_sinks.push(sink);
        self
    }

    /// Execute the agent turn, drive the stream through sinks, and return the
    /// executor commit plus sink output.
    pub async fn execute(mut self) -> anyhow::Result<AgentTurnOutcome<I, S::Output>> {
        let call_id = self.request.call_id.clone();
        #[cfg(debug_assertions)]
        let mut drop_guard = AgentTurnDropGuard::new(call_id.clone(), self.observers.clone());

        notify_observers(
            &self.observers,
            AgentTurnEvent::UserPromptRendered {
                call_id: call_id.clone(),
                text: self.request.user.clone(),
            },
        )
        .await;

        let executor_commit = match self
            .runtime
            .execute_llm(self.request, &mut self.sink, &mut self.side_sinks)
            .await
        {
            Ok(executor_commit) => executor_commit,
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(call_id = %call_id, error = %msg, "agent turn failed");
                notify_observers(
                    &self.observers,
                    AgentTurnEvent::Failed {
                        call_id: call_id.clone(),
                        error: msg,
                    },
                )
                .await;
                return Err(e);
            }
        };

        for sink in self.side_sinks.drain(..) {
            sink.finish().await;
        }
        let sink_output = Box::new(self.sink).finish().await;

        #[cfg(debug_assertions)]
        {
            drop_guard.complete();
        }

        Ok(AgentTurnOutcome {
            executor_commit,
            sink_output,
        })
    }
}
