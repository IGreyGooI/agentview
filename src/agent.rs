//! [`Agent<B, E, T>`] — stateful LLM conversation with typed context
//! view model, executor, and transform.
//!
//! ## Design
//!
//! Stable prompt/history parameters live on the agent. Per-call turn sinks are
//! passed explicitly when starting a call.
//!
//! `Agent` is the long-lived session holder: prompt context, pending feedback,
//! model config, transform, and an [`AgentViewModel`] that renders what the
//! language agent sees.
//! [`crate::llm_call::AgentTurn`] is the per-request transaction. The loop in
//! [`AgentTurnBuilder::execute_loop`] is still an implementation shape: it repeats
//! `AgentTurn`s until a concrete parser context decides `Continue` or `Sleep`.
//! We intentionally keep loop observability as tracing for now; a future
//! `AgentApp`/runtime primitive can get its own observer once that concept
//! settles.
//!
//! | Generic | Bound | Example |
//! |---------|-------|---------|
//! | `B` | [`ContextViewBuilder`] | `AppContextBuilder` |
//! | `T` | [`TurnTransform`] | `AppTurnTransform` |
//!
//! ## Two-phase call pattern
//!
//! ```rust,ignore
//! // Phase 1 — capture current context through the app runtime/source:
//! agent.update_context_snapshot(&runtime).await;
//!
//! // Phase 2 — async LLM call (no lock held):
//! agent
//!     .call("ThinkAndSay")
//!     .with_user(format!("玩家说：「{player_intent}」"))
//!     .execute(&source, &executor)
//!     .await?;
//! ```
//!
//! ## Concrete type aliases
//!
//! ```rust,ignore
//! pub type DialogueAgent = Agent<AppContextBuilder, AppLLMExecutor, AppTurnTransform>;
//! ```
//!
//! ## Serialization
//!
//! `ctx`, view-model state, `model`, `max_tokens`, and `transform` are serialized.
//! The app runtime/source is **not stored** — it is passed at call time.

use std::any::type_name;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::llm_call::{
    notify_observers, AgentTurn, AgentTurnEvent, AgentTurnObserverHandle, AgentTurnOutcome,
    AgentTurnRequest, ExecutorCommit, LLMExecutor, NoopTurnSink, TextTurnEvent, TurnSink,
};
use crate::prompt_context::{IdentityTransform, PromptContext, Role, Turn, TurnTransform};
use crate::templates::{
    ContextBlockKind, ContextView, ContextViewBuilder, PromptLayout, PromptRenderable,
    PromptSystemVars, PromptUserVars, RenderedTurnArtifact, TemplateEngine, TurnArtifact,
    AGENT_SYSTEM_LAYOUT_TEMPLATE, AGENT_USER_LAYOUT_TEMPLATE,
};
use crate::StorageString;

// ── AgentViewModel ────────────────────────────────────────────────────────────

/// Builds agent-facing requests and commits successful turns back into context.
///
/// `AgentViewModel` is the boundary where application state becomes an
/// agent-readable view and where executor/sink results become durable prompt
/// history. The transcript item type `I` stays application-defined.
#[async_trait::async_trait]
pub trait AgentViewModel<I = Turn>: Send + Sync {
    type Source: Sync;
    type View: Clone + Send + 'static;
    type ContextState: Default + Clone + Send + 'static;

    async fn render_system(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        source: &Self::Source,
    ) -> anyhow::Result<String>;

    fn history(&self, ctx: &PromptContext<I, Self::ContextState>) -> Vec<I>
    where
        I: Clone,
    {
        let mut history = ctx.history().to_vec();
        history.extend(ctx.working_set().iter().cloned());
        history
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View;

    async fn render_user(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        current_view: &Self::View,
        previous_view: Option<&Self::View>,
        call_id: &str,
        task: String,
    ) -> anyhow::Result<String>;

    async fn commit_turn<SO>(
        &self,
        ctx: &mut PromptContext<I, Self::ContextState>,
        request: &AgentTurnRequest<I>,
        executor_commit: ExecutorCommit<I>,
        sink_output: &mut SO,
    ) -> anyhow::Result<TurnFlow>
    where
        SO: Send + Sync;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultContextState {
    pub feedback: DefaultAgentFeedback,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultAgentFeedback {
    pub artifacts: Vec<RenderedTurnArtifact>,
    pub task: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnFlow {
    Wait,
    Continue,
}

impl Default for TurnFlow {
    fn default() -> Self {
        Self::Wait
    }
}

#[derive(Default)]
pub struct DefaultTurnUpdate {
    pub flow: TurnFlow,
    pub artifacts: Vec<TurnArtifact>,
    pub task: Option<String>,
}

/// Sink output hook used by the default text stack to commit per-turn parser
/// facts into [`DefaultContextState`].
pub trait DefaultTurnOutput {
    fn drain_default_turn_update(&mut self) -> DefaultTurnUpdate {
        DefaultTurnUpdate::default()
    }
}

impl DefaultTurnOutput for () {}

struct CommittedAgentTurn<O> {
    flow: TurnFlow,
    sink_output: O,
}

/// Captures and renders the app surface shown to a language agent.
pub struct DefaultAgentViewModel<B, T = IdentityTransform>
where
    B: ContextViewBuilder,
{
    /// Builder that knows how to capture the concrete root context view.
    pub context_builder: B,

    /// Stable variables used to lazily render the system prompt.
    pub system_vars: PromptSystemVars,

    /// Template source for the stable system prompt envelope.
    pub system_template: StorageString,

    /// Template source for each user prompt envelope.
    pub user_template: StorageString,

    /// Per-agent prompt template registry.
    pub templates: TemplateEngine,

    /// Text-only default commit policy.
    pub transform: T,
}

impl<B, T> fmt::Debug for DefaultAgentViewModel<B, T>
where
    B: ContextViewBuilder + fmt::Debug,
    B::View: fmt::Debug,
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultAgentViewModel")
            .field("context_builder", &self.context_builder)
            .field("system_vars", &self.system_vars)
            .field("system_template", &"<template-source>")
            .field("user_template", &"<template-source>")
            .field("templates", &"<template-engine>")
            .field("transform", &self.transform)
            .finish()
    }
}

impl<B, T> Clone for DefaultAgentViewModel<B, T>
where
    B: ContextViewBuilder + Clone,
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            context_builder: self.context_builder.clone(),
            system_vars: self.system_vars.clone(),
            system_template: self.system_template.clone(),
            user_template: self.user_template.clone(),
            templates: self.templates.clone(),
            transform: self.transform.clone(),
        }
    }
}

pub struct RenderedAgentView<V> {
    pub context_snapshot: V,
    pub context_kind: ContextBlockKind,
    pub context_block: String,
    pub rendered_artifacts: Vec<RenderedTurnArtifact>,
    pub user_prompt: String,
}

impl<B, T> DefaultAgentViewModel<B, T>
where
    B: ContextViewBuilder,
    B::View: Clone + Send + 'static,
    T: TurnTransform,
{
    pub fn new(
        layout: impl PromptLayout,
        context_builder: B,
        system_vars: PromptSystemVars,
        transform: T,
    ) -> Self {
        Self {
            context_builder,
            system_vars,
            system_template: layout.system_template().into(),
            user_template: layout.user_template().into(),
            templates: TemplateEngine::new(),
            transform,
        }
    }

    pub async fn render_user_prompt(
        &self,
        current_view: &B::View,
        previous_view: Option<&B::View>,
        call_id: &str,
        task: String,
        rendered_artifacts: Vec<RenderedTurnArtifact>,
    ) -> anyhow::Result<RenderedAgentView<B::View>> {
        let (context_kind, context_block) = match previous_view {
            None => {
                tracing::debug!(
                    target: "agentview::agent",
                    call_id,
                    "rendering full agent context"
                );
                (
                    ContextBlockKind::Full,
                    current_view.render_full(&self.templates).await?,
                )
            }
            Some(prev) => match current_view.render_delta(prev, &self.templates).await {
                Ok(Some(delta)) => {
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        "rendering delta agent context"
                    );
                    (ContextBlockKind::Delta, delta)
                }
                Ok(None) => {
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        "agent context unchanged"
                    );
                    (ContextBlockKind::Empty, String::new())
                }
                Err(e) => return Err(e.into()),
            },
        };

        let user_prompt = self.templates.render_template(
            AGENT_USER_LAYOUT_TEMPLATE,
            &self.user_template,
            minijinja::Value::from_serialize(&PromptUserVars {
                context_kind,
                context_block: context_block.clone(),
                artifacts: rendered_artifacts.clone(),
                task,
            }),
        )?;

        Ok(RenderedAgentView {
            context_snapshot: current_view.clone(),
            context_kind,
            context_block,
            rendered_artifacts,
            user_prompt,
        })
    }

    pub fn render_system_prompt(&self) -> anyhow::Result<String> {
        Ok(self.templates.render_template(
            AGENT_SYSTEM_LAYOUT_TEMPLATE,
            &self.system_template,
            minijinja::Value::from_serialize(&self.system_vars),
        )?)
    }

    pub async fn render_turn_artifacts(
        &self,
        artifacts: Vec<TurnArtifact>,
    ) -> anyhow::Result<Vec<RenderedTurnArtifact>> {
        let mut rendered = Vec::new();
        for artifact in artifacts {
            rendered.push(RenderedTurnArtifact {
                kind: artifact.kind.clone(),
                rendered: artifact.render_full(&self.templates).await?,
            });
        }
        Ok(rendered)
    }

    pub async fn commit_default_turn_output<SO>(
        &self,
        ctx: &mut PromptContext<Turn, DefaultContextState>,
        sink_output: &mut SO,
    ) -> anyhow::Result<TurnFlow>
    where
        SO: DefaultTurnOutput + Send + Sync,
    {
        let update = sink_output.drain_default_turn_update();
        let artifacts = self.render_turn_artifacts(update.artifacts).await?;
        if !artifacts.is_empty() || update.task.is_some() {
            ctx.context_state_mut().feedback = DefaultAgentFeedback {
                artifacts,
                task: update.task,
            };
        }
        Ok(update.flow)
    }
}

impl<B, T> DefaultAgentViewModel<B, T>
where
    B: ContextViewBuilder + Clone,
    B::View: Clone,
    T: TurnTransform + Clone,
{
    pub fn with_context_builder(&self, context_builder: B) -> Self {
        Self {
            context_builder,
            system_vars: self.system_vars.clone(),
            system_template: self.system_template.clone(),
            user_template: self.user_template.clone(),
            templates: self.templates.clone(),
            transform: self.transform.clone(),
        }
    }
}

#[async_trait::async_trait]
impl<B, T> AgentViewModel<Turn> for DefaultAgentViewModel<B, T>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    B::Source: Sync,
    B::View: Clone + Send + 'static,
    T: TurnTransform + Clone + Send + Sync,
{
    type Source = B::Source;
    type View = B::View;
    type ContextState = DefaultContextState;

    async fn render_system(
        &self,
        ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<String> {
        match ctx.system() {
            Some(system) => Ok(system.to_owned()),
            None => self.render_system_prompt(),
        }
    }

    async fn render_user(
        &self,
        ctx: &PromptContext<Turn, Self::ContextState>,
        current_view: &Self::View,
        previous_view: Option<&Self::View>,
        call_id: &str,
        task: String,
    ) -> anyhow::Result<String> {
        let task = match ctx.context_state().feedback.task.as_deref() {
            Some(feedback_task) if task.is_empty() => feedback_task.to_owned(),
            Some(feedback_task) => format!("{feedback_task}\n\n{task}"),
            None => task,
        };
        let rendered_view = self
            .render_user_prompt(
                current_view,
                previous_view,
                call_id,
                task,
                ctx.context_state().feedback.artifacts.clone(),
            )
            .await?;
        Ok(rendered_view.user_prompt)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        self.context_builder.capture(source).await
    }

    async fn commit_turn<SO>(
        &self,
        ctx: &mut PromptContext<Turn, Self::ContextState>,
        request: &AgentTurnRequest<Turn>,
        executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut SO,
    ) -> anyhow::Result<TurnFlow>
    where
        SO: Send + Sync,
    {
        if !ctx.has_system() {
            ctx.set_system_once(request.system.clone());
        }

        if let Some(user) = self.transform.transform_user(&request.user) {
            ctx.push_history(Turn::user(user));
        }

        for item in executor_commit.append {
            match item.role {
                Role::User => ctx.push_history(item),
                Role::Assistant => match self.transform.transform_assistant(&item.text) {
                    Some(text) => {
                        ctx.push_history(Turn::assistant(text));
                    }
                    None => {}
                },
            }
        }

        ctx.context_state_mut().feedback = DefaultAgentFeedback::default();
        Ok(TurnFlow::Wait)
    }
}

// ── AgentConfig ────────────────────────────────────────────────────────────────

/// Default text agent built from a [`ContextViewBuilder`] and [`TurnTransform`].
pub type TextAgent<B, E, T, EV = TextTurnEvent> = Agent<DefaultAgentViewModel<B, T>, E, Turn, EV>;

/// Read-only session configuration shared across forks of an agent.
pub struct AgentConfig<VM, E, I = Turn, EV = TextTurnEvent>
where
    VM: AgentViewModel<I>,
    E: LLMExecutor<I, EV>,
{
    /// Captures and renders what the language agent sees.
    pub view: VM,

    /// LLM model identifier, e.g. `"deepseek/deepseek-v3.2"`.
    pub model: StorageString,

    /// Maximum output tokens per call.
    pub max_tokens: u64,

    pub executor: std::marker::PhantomData<(E, I, EV)>,
}

impl<VM, E, I, EV> fmt::Debug for AgentConfig<VM, E, I, EV>
where
    VM: AgentViewModel<I> + fmt::Debug,
    E: LLMExecutor<I, EV>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("view", &self.view)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

// ── Agent ──────────────────────────────────────────────────────────────────────

/// Stateful LLM agent parameterized over view model, executor, and transcript.
///
/// Read-only config lives in [`AgentConfig`] (behind `Arc`). Mutable session
/// fields (`ctx`, view cursor) are behind locks so callers can fork without
/// waiting for an in-flight LLM call to finish.
pub struct Agent<VM, E, I = Turn, EV = TextTurnEvent>
where
    VM: AgentViewModel<I>,
    E: LLMExecutor<I, EV>,
{
    /// Read-only configuration shared with forks.
    pub config: Arc<AgentConfig<VM, E, I, EV>>,

    /// Conversation history (system + committed turns).
    pub ctx: Arc<RwLock<PromptContext<I, VM::ContextState>>>,

    /// Previous successfully-rendered view frame used for delta rendering.
    pub view_cursor: Arc<RwLock<Option<VM::View>>>,

    observers: Vec<AgentTurnObserverHandle>,
}

impl<VM, E, I, EV> fmt::Debug for Agent<VM, E, I, EV>
where
    VM: AgentViewModel<I> + fmt::Debug,
    E: LLMExecutor<I, EV>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Agent")
            .field("config", &self.config)
            .field("ctx", &"<rwlock>")
            .field("view_cursor", &"<rwlock>")
            .field("observers", &self.observers.len())
            .finish()
    }
}

impl<B, E, T, EV> Agent<DefaultAgentViewModel<B, T>, E, Turn, EV>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    E: LLMExecutor<Turn, EV> + Clone,
    B::View: Clone + Send + 'static,
    B::Source: Sync,
    T: TurnTransform + Clone + Send + Sync,
    EV: Send + 'static,
{
    /// Construct a new agent with all persistent configuration.
    ///
    /// The app runtime/source is **not** a parameter — pass it to
    /// [`AgentTurnBuilder::execute`].
    pub fn new(
        context_builder: B,
        layout: impl PromptLayout,
        system_vars: PromptSystemVars,
        model: impl Into<String>,
        max_tokens: u64,
        transform: T,
    ) -> Self {
        let model = model.into();
        let view = DefaultAgentViewModel::new(layout, context_builder, system_vars, transform);
        Self {
            config: Arc::new(AgentConfig {
                view,
                model: model.into(),
                max_tokens,
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(PromptContext::without_system())),
            view_cursor: Arc::new(RwLock::new(None)),
            observers: Vec::new(),
        }
    }

    /// Fork this default text agent and replace the context builder for one runtime turn.
    pub async fn forked_with_context_builder(&self, context_builder: B) -> Self {
        let view = self.config.view.with_context_builder(context_builder);
        Self {
            config: Arc::new(AgentConfig {
                view,
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(self.ctx.read().await.clone())),
            view_cursor: Arc::new(RwLock::new(self.view_cursor.read().await.clone())),
            observers: self.observers.clone(),
        }
    }
}

impl<VM, E, I, EV> Agent<VM, E, I, EV>
where
    VM: AgentViewModel<I> + Clone,
    E: LLMExecutor<I, EV> + Clone,
    I: Clone + Send + 'static,
    EV: Send + 'static,
{
    /// Construct an agent from a custom [`AgentViewModel`].
    pub fn with_view(
        view: VM,
        model: impl Into<StorageString>,
        max_tokens: u64,
        ctx: PromptContext<I, VM::ContextState>,
    ) -> Self {
        Self {
            config: Arc::new(AgentConfig {
                view,
                model: model.into(),
                max_tokens,
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(ctx)),
            view_cursor: Arc::new(RwLock::new(None)),
            observers: Vec::new(),
        }
    }

    pub fn with_observer(mut self, observer: AgentTurnObserverHandle) -> Self {
        self.observers.push(observer);
        self
    }

    /// Fork this agent for a parallel sub-call.
    ///
    /// Clones prompt history and view-model snapshot state under brief locks;
    /// keeps view-model context state with the cloned prompt context.
    ///
    /// Safe to call while another call is in-flight — only needs short-lived
    /// read locks on the mutable fields.
    pub async fn forked(&self) -> Self {
        Self {
            config: Arc::new(AgentConfig {
                view: self.config.view.clone(),
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(self.ctx.read().await.clone())),
            view_cursor: Arc::new(RwLock::new(self.view_cursor.read().await.clone())),
            observers: self.observers.clone(),
        }
    }

    /// Begin building one model-backed turn for this agent.
    ///
    /// Returns an [`AgentTurnBuilder`]. Chain `.with_user`, then
    /// `.execute(&source, &executor).await`.
    ///
    /// ```rust,ignore
    /// agent
    ///     .call("ThinkAndSay")
    ///     .with_user("玩家说：「…」")
    ///     .execute(&source, &executor)
    ///     .await?;
    /// ```
    ///
    /// Takes `&self` (shared reference) — mutable fields are locked
    /// individually during [`execute_agent_turn`].
    pub fn call<'a>(&'a self, call_id: &'a str) -> AgentTurnBuilder<'a, VM, E, I, EV> {
        tracing::debug!(
            target: "agentview::agent",
            call_id,
            view_model = type_name::<VM>(),
            transcript = type_name::<I>(),
            "creating agent turn builder"
        );
        AgentTurnBuilder {
            agent: self,
            call_id,
            task: String::new(),
            side_sinks: Vec::new(),
            observers: self.observers.clone(),
            max_loops: 4,
        }
    }
}

// ── AgentTurnBuilder ──────────────────────────────────────────────────────────

/// Builder for one model-backed turn on an [`Agent`].
///
/// Created by [`Agent::call`]. Configure with `.with_user`, then
/// `.execute(&source, &executor).await`.
pub struct AgentTurnBuilder<'a, VM, E, I, EV = TextTurnEvent>
where
    VM: AgentViewModel<I>,
    E: LLMExecutor<I, EV>,
{
    agent: &'a Agent<VM, E, I, EV>,
    call_id: &'a str,
    task: String,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
    max_loops: usize,
}

impl<'a, VM, E, I, EV> AgentTurnBuilder<'a, VM, E, I, EV>
where
    VM: AgentViewModel<I>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    EV: Send + 'static,
{
    /// Set the call-specific user content appended after the context block.
    ///
    /// Examples:
    /// - NPC dialogue turn: `"玩家说：「{player_intent}」"`
    /// - Intent generation: `"NPC刚才说：{last}\n\n请生成对话选项："`
    pub fn with_user(mut self, content: impl Into<String>) -> Self {
        self.task = content.into();
        tracing::debug!(
            target: "agentview::agent",
            call_id = self.call_id,
            task_len = self.task.len(),
            "configured agent turn task"
        );
        self
    }

    pub fn with_max_loops(mut self, max_loops: usize) -> Self {
        self.max_loops = max_loops.max(1);
        tracing::debug!(
            target: "agentview::agent",
            call_id = self.call_id,
            max_loops = self.max_loops,
            "configured agent loop limit"
        );
        self
    }

    /// Add a per-call side-effect sink.
    pub fn with_side_sink(mut self, sink: impl TurnSink<EV, Output = ()> + 'static) -> Self {
        tracing::debug!(
            target: "agentview::agent",
            call_id = self.call_id,
            "registered agent turn side sink"
        );
        self.side_sinks.push(Box::new(sink));
        self
    }

    /// Execute the model-backed turn.
    ///
    /// `source` — the app runtime/source used to run the LLM call.
    ///
    /// Pipeline:
    /// 1. Capture current context, render context block (delta or full).
    /// 2. Build user message: `context_block + "\n\n" + user_content`.
    /// 3. Execute via the injected runtime with explicit per-call sinks.
    /// 4. On success: commit history, update the view model's previous snapshot.
    /// 5. On failure: return `Err` (agent state unchanged).
    pub async fn execute(self, source: &VM::Source, executor: &E) -> anyhow::Result<()> {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            side_sinks,
            observers,
            max_loops: _,
        } = self;

        execute_agent_turn_with_sink(
            agent,
            call_id,
            source,
            executor,
            task,
            NoopTurnSink,
            side_sinks,
            observers,
        )
        .await
        .map(|_: CommittedAgentTurn<()>| ())
    }

    pub async fn execute_with_sink<S>(
        self,
        source: &VM::Source,
        executor: &E,
        sink: S,
    ) -> anyhow::Result<S::Output>
    where
        S: TurnSink<EV> + Send + 'static,
        S::Output: Sync,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            side_sinks,
            observers,
            max_loops: _,
        } = self;

        execute_agent_turn_with_sink(
            agent, call_id, source, executor, task, sink, side_sinks, observers,
        )
        .await
        .map(|outcome| outcome.sink_output)
    }

    pub async fn execute_loop_with<S, BuildSink>(
        self,
        source: &VM::Source,
        executor: &E,
        mut build_sink: BuildSink,
    ) -> anyhow::Result<()>
    where
        S: TurnSink<EV> + Send + 'static,
        S::Output: Send + Sync + 'static,
        BuildSink: FnMut() -> S,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            mut task,
            mut side_sinks,
            observers,
            max_loops,
        } = self;

        tracing::debug!(
            target: "agentview::agent",
            call_id,
            max_loops,
            "starting generic agent control loop"
        );

        for loop_index in 0..max_loops {
            let loop_number = loop_index + 1;
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                loop_number,
                task_len = task.len(),
                "starting generic agent loop iteration"
            );

            let outcome = execute_agent_turn_with_sink(
                agent,
                call_id,
                source,
                executor,
                task,
                build_sink(),
                side_sinks,
                observers.clone(),
            )
            .await?;

            match outcome.flow {
                TurnFlow::Wait => {
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        loop_number,
                        "generic agent loop waiting"
                    );
                    return Ok(());
                }
                TurnFlow::Continue => {
                    task = String::new();
                    side_sinks = Vec::new();
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        loop_number,
                        "generic agent loop continuing"
                    );
                }
            }
        }

        Err(anyhow::anyhow!(
            "agent turn `{}` exceeded max loop count {}",
            call_id,
            max_loops
        ))
    }
}

impl<'a, B, T, E, EV> AgentTurnBuilder<'a, DefaultAgentViewModel<B, T>, E, Turn, EV>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    B::Source: Sync,
    B::View: Clone + Send + 'static,
    T: TurnTransform + Clone + Send + Sync,
    E: LLMExecutor<Turn, EV> + Clone + Send + Sync + 'static,
    EV: Send + 'static,
{
    pub async fn execute_loop<S, BuildSink>(
        self,
        source: &B::Source,
        executor: &E,
        mut build_sink: BuildSink,
    ) -> anyhow::Result<()>
    where
        S: TurnSink<EV> + Send + 'static,
        S::Output: DefaultTurnOutput + Send + Sync + 'static,
        BuildSink: FnMut() -> S,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            mut task,
            mut side_sinks,
            observers,
            max_loops,
        } = self;

        tracing::debug!(
            target: "agentview::agent",
            call_id,
            max_loops,
            "starting default streaming agent loop"
        );

        for loop_index in 0..max_loops {
            let loop_number = loop_index + 1;
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                loop_number,
                task_len = task.len(),
                "starting default streaming agent loop iteration"
            );

            let outcome = execute_agent_turn_with_sink(
                agent,
                call_id,
                source,
                executor,
                task,
                build_sink(),
                side_sinks,
                observers.clone(),
            )
            .await?;

            let mut sink_output = outcome.sink_output;
            let flow = {
                let mut ctx = agent.ctx.write().await;
                agent
                    .config
                    .view
                    .commit_default_turn_output(&mut ctx, &mut sink_output)
                    .await?
            };

            match flow {
                TurnFlow::Wait => {
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        loop_number,
                        "default streaming agent loop waiting"
                    );
                    return Ok(());
                }
                TurnFlow::Continue => {
                    task = String::new();
                    side_sinks = Vec::new();
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        loop_number,
                        "default streaming agent loop continuing"
                    );
                }
            }
        }

        Err(anyhow::anyhow!(
            "agent turn `{}` exceeded max loop count {}",
            call_id,
            max_loops
        ))
    }
}

async fn execute_agent_turn_with_sink<VM, E, I, S, EV>(
    agent: &Agent<VM, E, I, EV>,
    call_id: &str,
    source: &VM::Source,
    executor: &E,
    task: String,
    sink: S,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
) -> anyhow::Result<CommittedAgentTurn<S::Output>>
where
    VM: AgentViewModel<I>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    S: TurnSink<EV> + Send + 'static,
    S::Output: Sync,
    EV: Send + 'static,
{
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        incoming_task_len = task.len(),
        "preparing agent turn"
    );

    // ── Build request (brief read-lock on ctx) ─────────────────────────────
    let ctx_for_request = { agent.ctx.read().await.clone() };
    let had_system = ctx_for_request.has_system();
    let previous_view = { agent.view_cursor.read().await.clone() };
    let current_view = agent.config.view.capture_view(source).await;
    let system = agent
        .config
        .view
        .render_system(&ctx_for_request, source)
        .await?;
    let history = agent.config.view.history(&ctx_for_request);
    let user = agent
        .config
        .view
        .render_user(
            &ctx_for_request,
            &current_view,
            previous_view.as_ref(),
            call_id,
            task,
        )
        .await?;
    let request = AgentTurnRequest {
        call_id: call_id.into(),
        system,
        history,
        user,
        model: agent.config.model.clone(),
        max_tokens: agent.config.max_tokens,
    };
    if !had_system {
        notify_observers(
            &observers,
            AgentTurnEvent::SystemPromptRendered {
                call_id: call_id.into(),
                text: request.system.clone(),
            },
        )
        .await;
    }
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        history_len = request.history.len(),
        user_msg_len = request.user.len(),
        "built agent turn request"
    );

    // ── Execute agent turn (no locks held) ─────────────────────────────────
    let request_for_commit = request.clone();
    let mut call = AgentTurn::new(executor.clone(), request)
        .with_observers(observers.clone())
        .with_sink(sink);
    for sink in side_sinks {
        call = call.with_side_sink_boxed(sink);
    }
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        "starting agent turn execute"
    );
    let result = call.execute().await;
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        result_ok = result.is_ok(),
        "agent turn execute returned"
    );

    // ── Commit (brief write-locks) ─────────────────────────────────────────
    match result {
        Ok(outcome) => {
            let AgentTurnOutcome {
                executor_commit,
                mut sink_output,
            } = outcome;
            let flow;
            {
                let mut ctx = agent.ctx.write().await;
                flow = agent
                    .config
                    .view
                    .commit_turn(
                        &mut ctx,
                        &request_for_commit,
                        executor_commit,
                        &mut sink_output,
                    )
                    .await?
            };
            *agent.view_cursor.write().await = Some(current_view);
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                "agent turn committed"
            );
            Ok(CommittedAgentTurn { flow, sink_output })
        }
        Err(e) => {
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                error = %e,
                "agent turn failed"
            );
            Err(e)
        }
    }
}
