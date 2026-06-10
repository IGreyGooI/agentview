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

use tokio::sync::{Mutex as TokioMutex, RwLock};

use crate::StorageString;
use crate::llm_call::{
    AgentTurn, AgentTurnEvent, AgentTurnObserverHandle, AgentTurnOutcome, LLMExecutor,
    NoopTurnSink, TextTurnEvent, TurnSink, notify_observers,
};
use crate::prompt_context::{PromptContext, TurnTransform};
use crate::streaming_tool::{
    AgentFeedback, AgentTurnControl, ParseContext, ReturnPolicy, StreamingToolRunner,
};
use crate::templates::{
    AGENT_SYSTEM_LAYOUT_TEMPLATE, AGENT_USER_LAYOUT_TEMPLATE, ContextBlockKind, ContextView,
    ContextViewBuilder, PromptLayout, PromptRenderable, PromptSystemVars, PromptUserVars,
    RenderedTurnArtifact, TemplateEngine, TurnArtifact,
};

// ── AgentViewModel ────────────────────────────────────────────────────────────

/// Captures and renders the app surface shown to a language agent.
pub struct AgentViewModel<B>
where
    B: ContextViewBuilder,
{
    /// Builder that knows how to capture the concrete root context view.
    pub context_builder: B,

    /// Snapshot from the *previous* turn — used to compute delta context blocks.
    ///
    /// - `None` → cold path: render the full context view.
    /// - `Some(prev)` → warm path: render only the delta.
    pub prev_snapshot: Arc<RwLock<Option<B::View>>>,

    /// Stable variables used to lazily render the system prompt.
    pub system_vars: PromptSystemVars,

    /// Template source for the stable system prompt envelope.
    pub system_template: StorageString,

    /// Template source for each user prompt envelope.
    pub user_template: StorageString,

    /// Per-agent prompt template registry.
    pub templates: TemplateEngine,
}

impl<B> fmt::Debug for AgentViewModel<B>
where
    B: ContextViewBuilder + fmt::Debug,
    B::View: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentViewModel")
            .field("context_builder", &self.context_builder)
            .field("prev_snapshot", &"<rwlock>")
            .field("system_vars", &self.system_vars)
            .field("system_template", &"<template-source>")
            .field("user_template", &"<template-source>")
            .field("templates", &"<template-engine>")
            .finish()
    }
}

pub struct RenderedAgentView<V> {
    pub context_snapshot: V,
    pub context_kind: ContextBlockKind,
    pub context_block: String,
    pub rendered_artifacts: Vec<RenderedTurnArtifact>,
    pub user_prompt: String,
}

impl<B> AgentViewModel<B>
where
    B: ContextViewBuilder,
    B::View: Clone + Send + 'static,
{
    pub fn new(
        layout: impl PromptLayout,
        context_builder: B,
        system_vars: PromptSystemVars,
    ) -> Self {
        Self {
            context_builder,
            prev_snapshot: Arc::new(RwLock::new(None)),
            system_vars,
            system_template: layout.system_template().into(),
            user_template: layout.user_template().into(),
            templates: TemplateEngine::new(),
        }
    }

    pub async fn render_user_prompt(
        &self,
        source: &B::Source,
        call_id: &str,
        task: String,
        artifacts: Vec<TurnArtifact>,
    ) -> anyhow::Result<RenderedAgentView<B::View>> {
        let snap = self.context_builder.capture(source).await;

        let (context_kind, context_block) = {
            let mut prev_guard = self.prev_snapshot.write().await;
            match prev_guard.take() {
                None => {
                    drop(prev_guard);
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        "rendering full agent context"
                    );
                    (
                        ContextBlockKind::Full,
                        snap.render_full(&self.templates).await?,
                    )
                }
                Some(prev) => {
                    drop(prev_guard);
                    match snap.render_delta(&prev, &self.templates).await {
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
                    }
                }
            }
        };

        let mut rendered_artifacts = Vec::new();
        for artifact in artifacts {
            rendered_artifacts.push(RenderedTurnArtifact {
                kind: artifact.kind.clone(),
                rendered: artifact.render_full(&self.templates).await?,
            });
        }

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
            context_snapshot: snap,
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

    pub async fn commit_snapshot(&self, snapshot: B::View) {
        *self.prev_snapshot.write().await = Some(snapshot);
    }
}

impl<B> AgentViewModel<B>
where
    B: ContextViewBuilder + Clone,
    B::View: Clone,
{
    pub async fn forked(&self) -> Self {
        Self {
            context_builder: self.context_builder.clone(),
            prev_snapshot: Arc::new(RwLock::new(self.prev_snapshot.read().await.clone())),
            system_vars: self.system_vars.clone(),
            system_template: self.system_template.clone(),
            user_template: self.user_template.clone(),
            templates: TemplateEngine::new(),
        }
    }

    pub async fn forked_with_context_builder(&self, context_builder: B) -> Self {
        Self {
            context_builder,
            prev_snapshot: Arc::new(RwLock::new(self.prev_snapshot.read().await.clone())),
            system_vars: self.system_vars.clone(),
            system_template: self.system_template.clone(),
            user_template: self.user_template.clone(),
            templates: TemplateEngine::new(),
        }
    }
}

// ── AgentConfig ────────────────────────────────────────────────────────────────

/// Read-only session configuration shared across forks of an agent.
pub struct AgentConfig<B, E, T, EV = TextTurnEvent>
where
    B: ContextViewBuilder,
    E: LLMExecutor<EV>,
    T: TurnTransform,
{
    /// Captures and renders what the language agent sees.
    pub view: AgentViewModel<B>,

    /// LLM model identifier, e.g. `"deepseek/deepseek-v3.2"`.
    pub model: StorageString,

    /// Maximum output tokens per call.
    pub max_tokens: u64,

    /// How exchanges are committed to `ctx` after a successful call.
    pub transform: T,

    pub executor: std::marker::PhantomData<(E, EV)>,
}

impl<B, E, T, EV> fmt::Debug for AgentConfig<B, E, T, EV>
where
    B: ContextViewBuilder + fmt::Debug,
    E: LLMExecutor<EV>,
    B::View: fmt::Debug,
    T: TurnTransform + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("view", &self.view)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("transform", &self.transform)
            .finish()
    }
}

// ── Agent ──────────────────────────────────────────────────────────────────────

/// Stateful LLM agent parameterized over context builder and turn transform.
///
/// Read-only config lives in [`AgentConfig`] (behind `Arc`). Mutable session
/// fields (`ctx`, `pending_feedback`) and view-model snapshot state are behind
/// locks so callers can fork without waiting for an in-flight LLM call to finish.
pub struct Agent<B, E, T, EV = TextTurnEvent>
where
    B: ContextViewBuilder,
    E: LLMExecutor<EV>,
    T: TurnTransform,
{
    /// Read-only configuration shared with forks.
    pub config: Arc<AgentConfig<B, E, T, EV>>,

    /// Conversation history (system + committed turns).
    pub ctx: Arc<RwLock<PromptContext>>,

    /// Runtime feedback saved by the last sleeping behavior for the next awake.
    pub pending_feedback: Arc<TokioMutex<AgentFeedback>>,

    observers: Vec<AgentTurnObserverHandle>,
}

impl<B, E, T, EV> fmt::Debug for Agent<B, E, T, EV>
where
    B: ContextViewBuilder + fmt::Debug,
    E: LLMExecutor<EV>,
    B::View: fmt::Debug,
    T: TurnTransform + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Agent")
            .field("config", &self.config)
            .field("ctx", &"<rwlock>")
            .field("pending_feedback", &"<mutex>")
            .field("observers", &self.observers.len())
            .finish()
    }
}

impl<B, E, T, EV> Agent<B, E, T, EV>
where
    B: ContextViewBuilder + Clone,
    E: LLMExecutor<EV> + Clone,
    B::View: Clone + Send + 'static,
    T: TurnTransform + Clone,
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
        let view = AgentViewModel::new(layout, context_builder, system_vars);
        Self {
            config: Arc::new(AgentConfig {
                view,
                model: model.into(),
                max_tokens,
                transform,
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(PromptContext::without_system())),
            pending_feedback: Arc::new(TokioMutex::new(AgentFeedback::empty())),
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
    /// resets `pending_feedback`.
    ///
    /// Safe to call while another call is in-flight — only needs short-lived
    /// read locks on the mutable fields.
    pub async fn forked(&self) -> Self {
        let view = self.config.view.forked().await;
        Self {
            config: Arc::new(AgentConfig {
                view,
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                transform: self.config.transform.clone(),
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(self.ctx.read().await.clone())),
            pending_feedback: Arc::new(TokioMutex::new(AgentFeedback::empty())),
            observers: self.observers.clone(),
        }
    }

    /// Fork this agent and replace the context builder for one runtime turn.
    pub async fn forked_with_context_builder(&self, context_builder: B) -> Self {
        let view = self
            .config
            .view
            .forked_with_context_builder(context_builder)
            .await;
        Self {
            config: Arc::new(AgentConfig {
                view,
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                transform: self.config.transform.clone(),
                executor: std::marker::PhantomData,
            }),
            ctx: Arc::new(RwLock::new(self.ctx.read().await.clone())),
            pending_feedback: Arc::new(TokioMutex::new(AgentFeedback::empty())),
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
    pub fn call<'a>(&'a self, call_id: &'a str) -> AgentTurnBuilder<'a, B, E, T, EV> {
        tracing::debug!(
            target: "agentview::agent",
            call_id,
            builder = type_name::<B>(),
            transform = type_name::<T>(),
            "creating agent turn builder"
        );
        AgentTurnBuilder {
            agent: self,
            call_id,
            task: String::new(),
            artifacts: Vec::new(),
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
pub struct AgentTurnBuilder<'a, B, E, T, EV = TextTurnEvent>
where
    B: ContextViewBuilder,
    E: LLMExecutor<EV>,
    T: TurnTransform,
{
    agent: &'a Agent<B, E, T, EV>,
    call_id: &'a str,
    task: String,
    artifacts: Vec<TurnArtifact>,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
    max_loops: usize,
}

impl<'a, B, E, T, EV> AgentTurnBuilder<'a, B, E, T, EV>
where
    B: ContextViewBuilder,
    E: LLMExecutor<EV> + Clone + Send + Sync + 'static,
    B::View: Clone + Send + 'static,
    T: TurnTransform + Clone,
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

    pub fn with_artifact(mut self, artifact: TurnArtifact) -> Self {
        tracing::debug!(
            target: "agentview::agent",
            call_id = self.call_id,
            artifact_kind = %artifact.kind,
            "queued agent turn artifact"
        );
        self.artifacts.push(artifact);
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
    pub async fn execute(self, source: &B::Source, executor: &E) -> anyhow::Result<()> {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            artifacts,
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
            artifacts,
            NoopTurnSink,
            side_sinks,
            observers,
        )
        .await
        .map(|_: AgentTurnOutcome<()>| ())
    }

    pub async fn execute_with_sink<S>(
        self,
        source: &B::Source,
        executor: &E,
        sink: S,
    ) -> anyhow::Result<S::Output>
    where
        S: TurnSink<EV> + Send + 'static,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            artifacts,
            side_sinks,
            observers,
            max_loops: _,
        } = self;

        execute_agent_turn_with_sink(
            agent, call_id, source, executor, task, artifacts, sink, side_sinks, observers,
        )
        .await
        .map(|outcome| outcome.sink_output)
    }

    pub async fn execute_loop<C, BuildRunner, Decide>(
        self,
        source: &B::Source,
        executor: &E,
        mut build_runner: BuildRunner,
        decide: Decide,
    ) -> anyhow::Result<()>
    where
        C: ParseContext + Send + 'static,
        BuildRunner: FnMut() -> StreamingToolRunner<C>,
        Decide: Fn(C) -> AgentTurnControl,
        StreamingToolRunner<C>: TurnSink<EV, Output = C>,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            mut task,
            mut artifacts,
            mut side_sinks,
            observers,
            max_loops,
        } = self;

        tracing::debug!(
            target: "agentview::agent",
            call_id,
            max_loops,
            "starting agent execute_loop"
        );

        for loop_index in 0..max_loops {
            let loop_number = loop_index + 1;
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                loop_number,
                task_len = task.len(),
                artifact_count = artifacts.len(),
                "starting agent loop iteration"
            );
            let runner = build_runner();

            let outcome = execute_agent_turn_with_sink(
                agent,
                call_id,
                source,
                executor,
                task,
                artifacts,
                runner,
                side_sinks,
                observers.clone(),
            )
            .await?;

            let control = decide(outcome.sink_output);

            tracing::debug!(
                target: "agentview::agent",
                call_id,
                loop_number,
                return_policy = ?control.return_policy,
                feedback_artifact_count = control.feedback.artifacts.len(),
                feedback_has_task = control.feedback.task.is_some(),
                "agent loop decision produced control"
            );

            match control.return_policy {
                ReturnPolicy::Sleep => {
                    *agent.pending_feedback.lock().await = control.feedback;
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        loop_number,
                        "agent loop sleeping"
                    );
                    return Ok(());
                }
                ReturnPolicy::Continue => {
                    task = control.feedback.task.unwrap_or_default();
                    artifacts = control.feedback.artifacts;
                    side_sinks = Vec::new();
                    tracing::debug!(
                        target: "agentview::agent",
                        call_id,
                        loop_number,
                        next_task_len = task.len(),
                        next_artifact_count = artifacts.len(),
                        "agent loop continuing with feedback"
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

async fn execute_agent_turn_with_sink<B, E, T, S, EV>(
    agent: &Agent<B, E, T, EV>,
    call_id: &str,
    source: &B::Source,
    executor: &E,
    task: String,
    mut artifacts: Vec<TurnArtifact>,
    sink: S,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
) -> anyhow::Result<AgentTurnOutcome<S::Output>>
where
    B: ContextViewBuilder,
    E: LLMExecutor<EV> + Clone + Send + Sync + 'static,
    B::View: Clone + Send + 'static,
    T: TurnTransform + Clone,
    S: TurnSink<EV> + Send + 'static,
    EV: Send + 'static,
{
    // ── Drain pending feedback (brief lock) ────────────────────────────────
    let deferred_feedback = {
        let mut fb = agent.pending_feedback.lock().await;
        std::mem::take(&mut *fb)
    };
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        incoming_task_len = task.len(),
        incoming_artifact_count = artifacts.len(),
        pending_artifact_count = deferred_feedback.artifacts.len(),
        pending_has_task = deferred_feedback.task.is_some(),
        "preparing agent turn"
    );
    artifacts.splice(0..0, deferred_feedback.artifacts);
    let task = match deferred_feedback.task {
        Some(feedback_task) if task.is_empty() => feedback_task,
        Some(feedback_task) => format!("{feedback_task}\n\n{task}"),
        None => task,
    };

    let rendered_view = agent
        .config
        .view
        .render_user_prompt(source, call_id, task, artifacts)
        .await?;
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        context_kind = ?rendered_view.context_kind,
        context_block_len = rendered_view.context_block.len(),
        "rendered agent context block"
    );
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        rendered_artifact_count = rendered_view.rendered_artifacts.len(),
        user_msg_len = rendered_view.user_prompt.len(),
        "rendered agent user prompt"
    );

    // ── Build prompt context (brief read-lock on ctx) ──────────────────────
    let mut ctx_for_call = { agent.ctx.read().await.clone() };
    if !ctx_for_call.has_system() {
        let system = agent.config.view.render_system_prompt()?;
        notify_observers(
            &observers,
            AgentTurnEvent::SystemPromptRendered {
                call_id: call_id.into(),
                text: system.clone(),
            },
        )
        .await;
        ctx_for_call.set_system_once(system);
    }
    ctx_for_call.push_user(rendered_view.user_prompt);

    // ── Execute agent turn (no locks held) ─────────────────────────────────
    let transform = agent.config.transform.clone();
    let mut call = AgentTurn::new(
        executor.clone(),
        call_id,
        ctx_for_call,
        transform,
        agent.config.model.clone(),
        agent.config.max_tokens,
    )
    .with_observers(observers)
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
        Ok(committed) => {
            *agent.ctx.write().await = committed.context.clone();
            agent
                .config
                .view
                .commit_snapshot(rendered_view.context_snapshot)
                .await;
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                "agent turn committed"
            );
            Ok(committed)
        }
        Err(e) => {
            let _ = e.ctx;
            tracing::debug!(
                target: "agentview::agent",
                call_id,
                error = %e.source,
                "agent turn failed"
            );
            Err(e.source)
        }
    }
}
