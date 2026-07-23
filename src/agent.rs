//! [`Agent<B, E, T>`] — stateful LLM conversation with typed context
//! view model, executor, and transform.
//!
//! ## Design
//!
//! Stable prompt/history parameters live on the agent. Per-call turn sinks are
//! passed explicitly when starting a call.
//!
//! `Agent` is the provider-backed turn runner. Its mutable prompt state lives
//! in an [`AgentSession`], while model config, transform, and the
//! [`AgentViewModel`] describe how turns are rendered and committed.
//! [`crate::llm_call::AgentTurn`] is the per-request transaction. The loop in
//! [`AgentTurnBuilder::execute_loop_with`] is still an implementation shape: it repeats
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
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::agent_session::AgentSession;
use crate::llm_call::{
    notify_observers, AgentTurn, AgentTurnEvent, AgentTurnFlow, AgentTurnObserverHandle,
    AgentTurnOutcome, AgentTurnRequest, ContextPreparation, ContextPreparationBudget,
    ExecutorCommit, LLMExecutor, NoopTurnSink, TextTurnEvent, TurnSink,
};
use crate::prompt_context::{IdentityTransform, PromptContext, Role, Turn, TurnTransform};
use crate::templates::{
    ContextBlockKind, ContextView, ContextViewBuilder, PromptFragment, PromptLayout,
    PromptRenderable, PromptSystemVars, PromptUserVars, RenderedTurnArtifact, TemplateEngine,
    TurnArtifact, AGENT_SYSTEM_LAYOUT_TEMPLATE, AGENT_USER_LAYOUT_TEMPLATE,
};
use crate::StorageString;

// ── AgentViewModel ────────────────────────────────────────────────────────────

/// Builds agent-facing requests and commits successful turns back into context.
///
/// `AgentViewModel` is the boundary where application state becomes an
/// agent-readable view and where executor/sink results become durable prompt
/// history. The transcript item type `I` stays application-defined.
#[async_trait::async_trait]
pub trait AgentViewModel<I = Turn, TurnOutput = ()>: Send + Sync {
    type Source: Sync;
    type View: ContextView + Clone + Send + 'static;
    type SystemPrompt: PromptRenderable + Send + Sync + 'static;
    type TurnPrompt: PromptRenderable + Send + Sync + 'static;
    type ContextState: Default + Clone + Send + 'static;

    async fn build_system_prompt(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        source: &Self::Source,
    ) -> anyhow::Result<Self::SystemPrompt>;

    fn history(&self, ctx: &PromptContext<I, Self::ContextState>) -> Vec<I>
    where
        I: Clone,
    {
        let mut history = ctx.history().to_vec();
        history.extend(ctx.working_set().iter().cloned());
        history
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View;

    async fn build_turn_prompt(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        call_id: &str,
        task: String,
    ) -> anyhow::Result<Self::TurnPrompt>;

    async fn commit_turn(
        &self,
        ctx: &mut PromptContext<I, Self::ContextState>,
        request: &AgentTurnRequest<I>,
        executor_commit: ExecutorCommit<I>,
        sink_output: &mut TurnOutput,
    ) -> anyhow::Result<TurnFlow>
    where
        TurnOutput: Send + Sync;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultContextState {
    pub feedback: DefaultAgentFeedback,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultAgentFeedback {
    #[serde(default, skip_deserializing)]
    pub artifacts: Vec<TurnArtifact>,
    pub task: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnFlow {
    #[default]
    Wait,
    Continue,
}

impl From<TurnFlow> for AgentTurnFlow {
    fn from(flow: TurnFlow) -> Self {
        match flow {
            TurnFlow::Wait => Self::Wait,
            TurnFlow::Continue => Self::Continue,
        }
    }
}

struct CommittedAgentTurn<O> {
    flow: TurnFlow,
    sink_output: O,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DefaultSystemPrompt {
    Rendered(StorageString),
    Template {
        vars: PromptSystemVars,
        template: StorageString,
    },
}

#[async_trait::async_trait]
impl PromptRenderable for DefaultSystemPrompt {
    async fn render_full<'a>(
        &'a self,
        templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        match self {
            Self::Rendered(text) => Ok(text.as_ref().into()),
            Self::Template { vars, template } => Ok(templates
                .render_template(
                    AGENT_SYSTEM_LAYOUT_TEMPLATE,
                    template,
                    minijinja::Value::from_serialize(vars),
                )?
                .into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultTurnPrompt {
    pub task: String,
    #[serde(default, skip_deserializing)]
    pub artifacts: Vec<TurnArtifact>,
    pub template: StorageString,
}

#[async_trait::async_trait]
impl PromptRenderable for DefaultTurnPrompt {
    async fn render_full<'a>(
        &'a self,
        templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        let mut rendered_artifacts = Vec::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            rendered_artifacts.push(RenderedTurnArtifact::new(
                artifact.kind(),
                artifact.render_full(templates).await?.into_string(),
            ));
        }

        Ok(templates
            .render_template(
                AGENT_USER_LAYOUT_TEMPLATE,
                &self.template,
                minijinja::Value::from_serialize(&PromptUserVars {
                    context_kind: ContextBlockKind::Empty,
                    context_block: String::new(),
                    artifacts: rendered_artifacts,
                    task: self.task.clone(),
                }),
            )?
            .into())
    }
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
    pub context_block: PromptFragment,
    pub artifacts: Vec<TurnArtifact>,
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

    pub async fn render_view_block(
        &self,
        current_view: &B::View,
        previous_view: Option<&B::View>,
        call_id: &str,
        artifacts: Vec<TurnArtifact>,
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
                    (ContextBlockKind::Empty, PromptFragment::new(String::new()))
                }
                Err(e) => return Err(e),
            },
        };

        Ok(RenderedAgentView {
            context_snapshot: current_view.clone(),
            context_kind,
            context_block,
            artifacts,
        })
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
impl<B, T, TurnOutput> AgentViewModel<Turn, TurnOutput> for DefaultAgentViewModel<B, T>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    B::Source: Sync,
    B::View: Clone + Send + 'static,
    T: TurnTransform + Clone + Send + Sync,
    TurnOutput: Send + Sync,
{
    type Source = B::Source;
    type View = B::View;
    type SystemPrompt = DefaultSystemPrompt;
    type TurnPrompt = DefaultTurnPrompt;
    type ContextState = DefaultContextState;

    async fn build_system_prompt(
        &self,
        ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Self::SystemPrompt> {
        match ctx.system() {
            Some(system) => Ok(DefaultSystemPrompt::Rendered(system.into())),
            None => Ok(DefaultSystemPrompt::Template {
                vars: self.system_vars.clone(),
                template: self.system_template.clone(),
            }),
        }
    }

    async fn build_turn_prompt(
        &self,
        ctx: &PromptContext<Turn, Self::ContextState>,
        _call_id: &str,
        task: String,
    ) -> anyhow::Result<Self::TurnPrompt> {
        let task = match ctx.context_state().feedback.task.as_deref() {
            Some(feedback_task) if task.is_empty() => feedback_task.to_owned(),
            Some(feedback_task) => format!("{feedback_task}\n\n{task}"),
            None => task,
        };
        Ok(DefaultTurnPrompt {
            task,
            artifacts: ctx.context_state().feedback.artifacts.clone(),
            template: self.user_template.clone(),
        })
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        self.context_builder.capture(source).await
    }

    async fn commit_turn(
        &self,
        ctx: &mut PromptContext<Turn, Self::ContextState>,
        request: &AgentTurnRequest<Turn>,
        executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut TurnOutput,
    ) -> anyhow::Result<TurnFlow> {
        if !ctx.has_system() {
            ctx.set_system_once(request.system.clone());
        }

        if let Some(user) = self.transform.transform_user(&request.user) {
            ctx.push_history(Turn::user(user));
        }

        for item in executor_commit.append {
            match item.role {
                Role::User => ctx.push_history(item),
                Role::Assistant => {
                    if let Some(text) = self.transform.transform_assistant(&item.text) {
                        ctx.push_history(Turn::assistant(text));
                    }
                }
            }
        }

        ctx.context_state_mut().feedback = DefaultAgentFeedback::default();
        Ok(TurnFlow::Wait)
    }
}

// ── AgentConfig ────────────────────────────────────────────────────────────────

/// Default text agent built from a [`ContextViewBuilder`] and [`TurnTransform`].
pub type TextAgent<B, E, T, EV = TextTurnEvent, TurnOutput = ()> =
    Agent<DefaultAgentViewModel<B, T>, E, Turn, EV, TurnOutput>;

/// Read-only session configuration shared across forks of an agent.
pub struct AgentConfig<VM, E, I = Turn, EV = TextTurnEvent, TurnOutput = ()>
where
    VM: AgentViewModel<I, TurnOutput>,
    E: LLMExecutor<I, EV>,
{
    /// Captures and renders what the language agent sees.
    pub view: VM,

    /// LLM model identifier, e.g. `"deepseek/deepseek-v3.2"`.
    pub model: StorageString,

    /// Maximum output tokens per call.
    pub max_tokens: u64,

    pub executor: std::marker::PhantomData<(E, I, EV, TurnOutput)>,
}

impl<VM, E, I, EV, TurnOutput> fmt::Debug for AgentConfig<VM, E, I, EV, TurnOutput>
where
    VM: AgentViewModel<I, TurnOutput> + fmt::Debug,
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

type SharedAgentSession<I, CS, V> = Arc<RwLock<AgentSession<I, CS, V>>>;

/// Stateful LLM agent parameterized over view model, executor, and transcript.
///
/// Read-only config lives in [`AgentConfig`] (behind `Arc`). Mutable prompt
/// state lives in one [`AgentSession`] so context and view cursor are always
/// snapshotted and committed together.
pub struct Agent<VM, E, I = Turn, EV = TextTurnEvent, TurnOutput = ()>
where
    VM: AgentViewModel<I, TurnOutput>,
    E: LLMExecutor<I, EV>,
{
    /// Read-only configuration shared with forks.
    pub config: Arc<AgentConfig<VM, E, I, EV, TurnOutput>>,

    session: SharedAgentSession<I, VM::ContextState, VM::View>,

    turn_lock: Arc<Mutex<()>>,
    observers: Vec<AgentTurnObserverHandle>,
}

impl<VM, E, I, EV, TurnOutput> fmt::Debug for Agent<VM, E, I, EV, TurnOutput>
where
    VM: AgentViewModel<I, TurnOutput> + fmt::Debug,
    E: LLMExecutor<I, EV>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Agent")
            .field("config", &self.config)
            .field("session", &"<rwlock>")
            .field("observers", &self.observers.len())
            .finish()
    }
}

/// Exclusive access to an agent session, serialized with model-backed turns.
pub struct AgentSessionWriteGuard<'a, I, CS, V> {
    session_guard: RwLockWriteGuard<'a, AgentSession<I, CS, V>>,
    _turn_guard: MutexGuard<'a, ()>,
}

impl<I, CS, V> Deref for AgentSessionWriteGuard<'_, I, CS, V> {
    type Target = AgentSession<I, CS, V>;

    fn deref(&self) -> &Self::Target {
        &self.session_guard
    }
}

impl<I, CS, V> DerefMut for AgentSessionWriteGuard<'_, I, CS, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session_guard
    }
}

impl<B, E, T, EV, TurnOutput> Agent<DefaultAgentViewModel<B, T>, E, Turn, EV, TurnOutput>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    E: LLMExecutor<Turn, EV> + Clone,
    B::View: Clone + Send + 'static,
    B::Source: Sync,
    T: TurnTransform + Clone + Send + Sync,
    EV: Send + 'static,
    TurnOutput: Send + Sync + 'static,
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
            session: Arc::new(RwLock::new(AgentSession::new(
                PromptContext::without_system(),
            ))),
            turn_lock: Arc::new(Mutex::new(())),
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
            session: Arc::new(RwLock::new(self.session.read().await.clone())),
            turn_lock: Arc::new(Mutex::new(())),
            observers: self.observers.clone(),
        }
    }
}

impl<VM, E, I, EV, TurnOutput> Agent<VM, E, I, EV, TurnOutput>
where
    VM: AgentViewModel<I, TurnOutput> + Clone,
    E: LLMExecutor<I, EV> + Clone,
    I: Clone + Send + 'static,
    EV: Send + 'static,
    TurnOutput: Send + Sync + 'static,
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
            session: Arc::new(RwLock::new(AgentSession::new(ctx))),
            turn_lock: Arc::new(Mutex::new(())),
            observers: Vec::new(),
        }
    }

    pub fn with_observer(mut self, observer: AgentTurnObserverHandle) -> Self {
        self.observers.push(observer);
        self
    }

    /// Read the last successfully committed session.
    pub async fn session(
        &self,
    ) -> RwLockReadGuard<'_, AgentSession<I, VM::ContextState, VM::View>> {
        self.session.read().await
    }

    /// Mutate committed session state after any in-flight turn completes.
    pub async fn session_mut(&self) -> AgentSessionWriteGuard<'_, I, VM::ContextState, VM::View> {
        let turn_guard = self.turn_lock.lock().await;
        let session_guard = self.session.write().await;
        AgentSessionWriteGuard {
            session_guard,
            _turn_guard: turn_guard,
        }
    }

    /// Fork this agent for a parallel sub-call.
    ///
    /// Clones the committed context and view cursor together under one brief
    /// read lock.
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
            session: Arc::new(RwLock::new(self.session.read().await.clone())),
            turn_lock: Arc::new(Mutex::new(())),
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
    /// Takes `&self` (shared reference); successful turns atomically replace
    /// the committed session.
    pub fn call<'a>(&'a self, call_id: &'a str) -> AgentTurnBuilder<'a, VM, E, I, EV, TurnOutput> {
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
            max_context_preparations: 3,
        }
    }
}

// ── AgentTurnBuilder ──────────────────────────────────────────────────────────

/// Builder for one model-backed turn on an [`Agent`].
///
/// Created by [`Agent::call`]. Configure with `.with_user`, then
/// `.execute(&source, &executor).await`.
pub struct AgentTurnBuilder<'a, VM, E, I, EV = TextTurnEvent, TurnOutput = ()>
where
    VM: AgentViewModel<I, TurnOutput>,
    E: LLMExecutor<I, EV>,
{
    agent: &'a Agent<VM, E, I, EV, TurnOutput>,
    call_id: &'a str,
    task: String,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
    max_loops: usize,
    max_context_preparations: usize,
}

impl<'a, VM, E, I, EV, TurnOutput> AgentTurnBuilder<'a, VM, E, I, EV, TurnOutput>
where
    VM: AgentViewModel<I, TurnOutput>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    EV: Send + 'static,
    TurnOutput: Send + Sync + 'static,
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

    /// Limit history replacements while preparing one logical model turn.
    pub fn with_max_context_preparations(mut self, max_attempts: usize) -> Self {
        self.max_context_preparations = max_attempts.clamp(1, 3);
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

    pub async fn execute_with_sink<S>(
        self,
        source: &VM::Source,
        executor: &E,
        sink: S,
    ) -> anyhow::Result<TurnOutput>
    where
        S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            side_sinks,
            observers,
            max_loops: _,
            max_context_preparations,
        } = self;

        execute_agent_turn_with_sink(
            agent,
            AgentTurnExecution {
                call_id,
                source,
                executor,
                task,
                sink,
                side_sinks,
                observers,
                max_context_preparations,
            },
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
        S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
        BuildSink: FnMut() -> S,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            mut task,
            mut side_sinks,
            observers,
            max_loops,
            max_context_preparations,
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
                AgentTurnExecution {
                    call_id,
                    source,
                    executor,
                    task,
                    sink: build_sink(),
                    side_sinks,
                    observers: observers.clone(),
                    max_context_preparations,
                },
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

impl<'a, VM, E, I, EV> AgentTurnBuilder<'a, VM, E, I, EV, ()>
where
    VM: AgentViewModel<I, ()>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    EV: Send + 'static,
{
    /// Execute the model-backed turn with the default no-op sink.
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
            max_context_preparations,
        } = self;

        execute_agent_turn_with_sink(
            agent,
            AgentTurnExecution {
                call_id,
                source,
                executor,
                task,
                sink: NoopTurnSink,
                side_sinks,
                observers,
                max_context_preparations,
            },
        )
        .await
        .map(|_: CommittedAgentTurn<()>| ())
    }
}

async fn render_view_block<V>(
    current_view: &V,
    previous_view: Option<&V>,
    templates: &TemplateEngine,
    call_id: &str,
) -> anyhow::Result<(ContextBlockKind, PromptFragment)>
where
    V: ContextView,
{
    match previous_view {
        None => {
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                "rendering full agent context"
            );
            Ok((
                ContextBlockKind::Full,
                current_view
                    .render_full(templates)
                    .await?
                    .with_memo("context:full"),
            ))
        }
        Some(prev) => match current_view.render_delta(prev, templates).await {
            Ok(Some(delta)) => {
                tracing::debug!(
                    target: "agentview::agent",
                    call_id,
                    "rendering delta agent context"
                );
                Ok((ContextBlockKind::Delta, delta.with_memo("context:delta")))
            }
            Ok(None) => {
                tracing::debug!(
                    target: "agentview::agent",
                    call_id,
                    "agent context unchanged"
                );
                Ok((
                    ContextBlockKind::Empty,
                    PromptFragment::new(String::new()).with_memo("context:empty"),
                ))
            }
            Err(e) => Err(e),
        },
    }
}

fn compose_user_message(context_block: PromptFragment, turn_prompt: PromptFragment) -> String {
    let context = context_block.as_str().trim();
    let turn = turn_prompt.as_str().trim();
    match (context.is_empty(), turn.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("## Turn Prompt\n\n{turn}"),
        (false, true) => format!("## View\n\n{context}"),
        (false, false) => format!("## View\n\n{context}\n\n## Turn Prompt\n\n{turn}"),
    }
}

struct AgentTurnExecution<'a, Source, E, S, EV> {
    call_id: &'a str,
    source: &'a Source,
    executor: &'a E,
    task: String,
    sink: S,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
    max_context_preparations: usize,
}

fn notify_committed_turn_observers(
    observers: Vec<AgentTurnObserverHandle>,
    call_id: StorageString,
    flow: AgentTurnFlow,
) {
    if observers.is_empty() {
        return;
    }

    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            call_id = %call_id,
            "dropping post-commit observer events without a Tokio runtime"
        );
        return;
    };

    std::mem::drop(runtime.spawn(async move {
        notify_observers(
            &observers,
            AgentTurnEvent::TurnCommitted {
                call_id: call_id.clone(),
            },
        )
        .await;
        notify_observers(
            &observers,
            AgentTurnEvent::TurnFlowDecided { call_id, flow },
        )
        .await;
    }));
}

async fn execute_agent_turn_with_sink<VM, E, I, S, EV, TurnOutput>(
    agent: &Agent<VM, E, I, EV, TurnOutput>,
    execution: AgentTurnExecution<'_, VM::Source, E, S, EV>,
) -> anyhow::Result<CommittedAgentTurn<TurnOutput>>
where
    VM: AgentViewModel<I, TurnOutput>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
    TurnOutput: Send + Sync + 'static,
    EV: Send + 'static,
{
    let AgentTurnExecution {
        call_id,
        source,
        executor,
        task,
        sink,
        side_sinks,
        observers,
        max_context_preparations,
    } = execution;
    let _turn_guard = agent.turn_lock.lock().await;
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        incoming_task_len = task.len(),
        "preparing agent turn"
    );

    let mut draft_session = { agent.session.read().await.clone() };
    let mut preparation_replacements = 0;
    let (request, current_view, context_kind, had_system) = loop {
        // ── Build request against the private turn draft ──────────────────
        let ctx_for_request = draft_session.context();
        let committed_history_len = ctx_for_request.history().len();
        let had_system = ctx_for_request.has_system();
        let previous_view = draft_session.view_cursor();
        let current_view = agent.config.view.capture_view(source).await;
        let templates = TemplateEngine::new();
        let system_prompt = agent
            .config
            .view
            .build_system_prompt(ctx_for_request, source)
            .await?;
        let system = system_prompt.render_full(&templates).await?.into_string();
        let history = agent.config.view.history(ctx_for_request);
        let (context_kind, context_block) =
            render_view_block(&current_view, previous_view, &templates, call_id).await?;
        let turn_prompt = agent
            .config
            .view
            .build_turn_prompt(ctx_for_request, call_id, task.clone())
            .await?;
        let turn_prompt = turn_prompt.render_full(&templates).await?;
        let user = compose_user_message(context_block, turn_prompt);
        let request = AgentTurnRequest {
            call_id: call_id.into(),
            system,
            history,
            user,
            model: agent.config.model.clone(),
            max_tokens: agent.config.max_tokens,
        };

        let preparation_budget = ContextPreparationBudget::new(
            preparation_replacements,
            max_context_preparations,
            committed_history_len,
        );
        match executor
            .prepare_context(request, preparation_budget)
            .await?
        {
            ContextPreparation::Ready(request) => {
                break (request, current_view, context_kind, had_system);
            }
            ContextPreparation::ReplaceHistory { history } => {
                if preparation_replacements >= max_context_preparations {
                    anyhow::bail!(
                        "agent turn `{call_id}` exceeded context preparation replacement limit {max_context_preparations}"
                    );
                }
                preparation_replacements += 1;

                draft_session.replace_history(history);
                tracing::debug!(
                    target: "agentview::agent",
                    call_id,
                    preparation_replacements,
                    "replaced history during context preparation"
                );
            }
        }
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
        context_kind = ?context_kind,
        user_msg_len = request.user.len(),
        "built agent turn request"
    );

    // ── Execute agent turn (no session lock held) ─────────────────────────
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

    // ── Commit the completed session draft atomically ─────────────────────
    match result {
        Ok(outcome) => {
            let AgentTurnOutcome {
                executor_commit,
                mut sink_output,
            } = outcome;
            let flow = agent
                .config
                .view
                .commit_turn(
                    draft_session.context_mut(),
                    &request_for_commit,
                    executor_commit,
                    &mut sink_output,
                )
                .await?;
            draft_session.set_view_cursor(current_view);
            *agent.session.write().await = draft_session;
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                "agent turn committed"
            );
            notify_committed_turn_observers(observers, call_id.into(), flow.into());
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
