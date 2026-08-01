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
use crate::agent_view::AgentView as PomAgentView;
use crate::llm_call::{
    notify_observers, AgentTurn, AgentTurnEvent, AgentTurnFlow, AgentTurnObserverHandle,
    AgentTurnOutcome, AgentTurnRequest, ContextPreparation, ContextPreparationBudget,
    ExecutorCommit, LLMExecutor, NoopTurnSink, TextTurnEvent, TurnSink,
};
use crate::pom::{Document, XmlNode};
use crate::pom_renderer::render_pom_document;
use crate::pom_resolution::{resolve_system_document, resolve_user_document};
use crate::prompt_context::{IdentityTransform, PromptContext, Role, Turn, TurnTransform};
use crate::templates::{ContextViewBuilder, TurnArtifact};
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
    type View: PomAgentView<Root = XmlNode>
        + crate::agent_view::AgentViewValue
        + Clone
        + Send
        + Sync
        + 'static;
    type ContextState: Default + Clone + Send + 'static;

    /// Build the complete system-role POM document for this request.
    async fn build_system_document(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        source: &Self::Source,
    ) -> anyhow::Result<Document>;

    fn history(&self, ctx: &PromptContext<I, Self::ContextState>) -> Vec<I>
    where
        I: Clone,
    {
        let mut history = ctx.history().to_vec();
        history.extend(ctx.working_set().iter().cloned());
        history
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View;

    /// Build the complete user-role POM document.
    ///
    /// This method owns section order and explicitly decides where the current
    /// view participates as a [`crate::pom::DiffSlot`]. The runtime does not inject a
    /// context or turn-prompt envelope around the returned document.
    async fn build_user_document(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document>;

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

/// Compatibility result used by the legacy `Agent` execution bridge.
///
/// Mounted components deliberately do not use this type: a mounted epoch
/// attaches System separately and owns its per-turn bindings through the
/// mounted runtime. It remains public only because it occurs in the public
/// bounds of the legacy generic `Agent` API.
#[doc(hidden)]
#[derive(Debug)]
pub struct PreparedTurn<B> {
    system: Document,
    user: Document,
    binding: B,
}

impl<B> PreparedTurn<B> {
    pub fn new(system: Document, user: Document, binding: B) -> Self {
        Self {
            system,
            user,
            binding,
        }
    }

    pub fn system_document(&self) -> &Document {
        &self.system
    }

    pub fn user_document(&self) -> &Document {
        &self.user
    }

    pub fn binding(&self) -> &B {
        &self.binding
    }

    pub fn into_parts(self) -> (Document, Document, B) {
        (self.system, self.user, self.binding)
    }
}

/// Default authoring mode for legacy split-document view models.
///
/// The mode exists only to keep the compatibility blanket implementation
/// disjoint from native component authoring at Rust's trait-coherence boundary.
#[derive(Debug, Clone, Copy, Default)]
#[doc(hidden)]
pub struct LegacyAuthoring;

/// Legacy extension point for the combined per-turn `Agent` execution path.
///
/// Existing [`AgentViewModel`] implementations receive a blanket implementation
/// with `Binding = ()`. Raw compatibility component runtimes implement this
/// trait directly so a discarded context-preparation attempt also discards its
/// hook plan.
///
/// This is not a mounted-component authoring contract. It stays public solely
/// because the legacy generic `Agent` types name it in their public bounds;
/// ordinary authors should use the mounted component API instead.
#[doc(hidden)]
#[async_trait::async_trait]
pub trait AgentTurnAuthor<I = Turn, TurnOutput = (), Authoring = LegacyAuthoring>:
    Send + Sync
{
    type Source: Sync;
    type View: PomAgentView<Root = XmlNode>
        + crate::agent_view::AgentViewValue
        + Clone
        + Send
        + Sync
        + 'static;
    type ContextState: Default + Clone + Send + Sync + 'static;
    /// Ephemeral typed input owned by one [`AgentTurnBuilder`].
    ///
    /// Call props are borrowed by every preparation attempt for the logical
    /// call, including context replacement and loop continuation. They are not
    /// stored in [`PromptContext`] or the user-document cursor.
    type CallProps: Send + Sync + 'static;
    type Binding: Send + 'static;

    fn history(&self, ctx: &PromptContext<I, Self::ContextState>) -> Vec<I>
    where
        I: Clone;

    async fn capture_view(&self, source: &Self::Source) -> Self::View;

    async fn prepare_turn(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        source: &Self::Source,
        call_id: &str,
        task: StorageString,
        call_props: Option<&Self::CallProps>,
        current_view: &Self::View,
    ) -> anyhow::Result<PreparedTurn<Self::Binding>>;

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

#[async_trait::async_trait]
impl<I, TurnOutput, VM> AgentTurnAuthor<I, TurnOutput, LegacyAuthoring> for VM
where
    I: Clone + Send + Sync + 'static,
    TurnOutput: Send + Sync + 'static,
    VM: AgentViewModel<I, TurnOutput>,
    VM::ContextState: Sync,
{
    type Source = VM::Source;
    type View = VM::View;
    type ContextState = VM::ContextState;
    type CallProps = ();
    type Binding = ();

    fn history(&self, ctx: &PromptContext<I, Self::ContextState>) -> Vec<I>
    where
        I: Clone,
    {
        <VM as AgentViewModel<I, TurnOutput>>::history(self, ctx)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        <VM as AgentViewModel<I, TurnOutput>>::capture_view(self, source).await
    }

    async fn prepare_turn(
        &self,
        ctx: &PromptContext<I, Self::ContextState>,
        source: &Self::Source,
        call_id: &str,
        task: StorageString,
        _call_props: Option<&Self::CallProps>,
        current_view: &Self::View,
    ) -> anyhow::Result<PreparedTurn<Self::Binding>> {
        let system =
            <VM as AgentViewModel<I, TurnOutput>>::build_system_document(self, ctx, source).await?;
        let user = <VM as AgentViewModel<I, TurnOutput>>::build_user_document(
            self,
            ctx,
            call_id,
            task,
            current_view,
        )
        .await?;
        Ok(PreparedTurn::new(system, user, ()))
    }

    async fn commit_turn(
        &self,
        ctx: &mut PromptContext<I, Self::ContextState>,
        request: &AgentTurnRequest<I>,
        executor_commit: ExecutorCommit<I>,
        sink_output: &mut TurnOutput,
    ) -> anyhow::Result<TurnFlow>
    where
        TurnOutput: Send + Sync,
    {
        <VM as AgentViewModel<I, TurnOutput>>::commit_turn(
            self,
            ctx,
            request,
            executor_commit,
            sink_output,
        )
        .await
    }
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

#[derive(Debug, Clone, crate::AgentView)]
#[agent_view(markdown = "paragraph")]
struct DefaultParagraphView {
    #[view(text)]
    text: String,
}

#[derive(Debug, Clone, crate::AgentView)]
#[agent_view(document)]
struct DefaultUserDocumentView<V>
where
    V: PomAgentView<Root = XmlNode> + crate::agent_view::AgentViewValue,
{
    #[view(name = "agent_context", diff)]
    context: V,

    #[view(block)]
    artifacts: Vec<TurnArtifact>,

    #[view(block)]
    feedback: Option<DefaultParagraphView>,

    #[view(block)]
    task: Option<DefaultParagraphView>,
}

/// Captures and renders the app surface shown to a language agent.
pub struct DefaultAgentViewModel<B, S, T = IdentityTransform>
where
    B: ContextViewBuilder,
    S: PomAgentView<Root = Document>,
{
    /// Builder that knows how to capture the concrete root context view.
    pub context_builder: B,

    /// Stable typed system-document view.
    pub system_document: S,

    /// Text-only default commit policy.
    pub transform: T,
}

impl<B, S, T> fmt::Debug for DefaultAgentViewModel<B, S, T>
where
    B: ContextViewBuilder + fmt::Debug,
    B::View: fmt::Debug,
    S: PomAgentView<Root = Document> + fmt::Debug,
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultAgentViewModel")
            .field("context_builder", &self.context_builder)
            .field("system_document", &self.system_document)
            .field("transform", &self.transform)
            .finish()
    }
}

impl<B, S, T> Clone for DefaultAgentViewModel<B, S, T>
where
    B: ContextViewBuilder + Clone,
    S: PomAgentView<Root = Document> + Clone,
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            context_builder: self.context_builder.clone(),
            system_document: self.system_document.clone(),
            transform: self.transform.clone(),
        }
    }
}

impl<B, S, T> DefaultAgentViewModel<B, S, T>
where
    B: ContextViewBuilder,
    B::View: PomAgentView<Root = XmlNode> + Clone + Send + 'static,
    S: PomAgentView<Root = Document>,
    T: TurnTransform,
{
    pub fn new(context_builder: B, system_document: S, transform: T) -> Self {
        Self {
            context_builder,
            system_document,
            transform,
        }
    }
}

impl<B, S, T> DefaultAgentViewModel<B, S, T>
where
    B: ContextViewBuilder + Clone,
    B::View: Clone,
    S: PomAgentView<Root = Document> + Clone,
    T: TurnTransform + Clone,
{
    pub fn with_context_builder(&self, context_builder: B) -> Self {
        Self {
            context_builder,
            system_document: self.system_document.clone(),
            transform: self.transform.clone(),
        }
    }
}

#[async_trait::async_trait]
impl<B, S, T, TurnOutput> AgentViewModel<Turn, TurnOutput> for DefaultAgentViewModel<B, S, T>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    B::Source: Sync,
    B::View: PomAgentView<Root = XmlNode> + Clone + Send + Sync + 'static,
    S: PomAgentView<Root = Document> + Clone + Send + Sync + 'static,
    T: TurnTransform + Clone + Send + Sync,
    TurnOutput: Send + Sync,
{
    type Source = B::Source;
    type View = B::View;
    type ContextState = DefaultContextState;

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        Ok(self.system_document.build_root()?)
    }

    async fn build_user_document(
        &self,
        ctx: &PromptContext<Turn, Self::ContextState>,
        _call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(DefaultUserDocumentView {
            context: current_view.clone(),
            artifacts: ctx.context_state().feedback.artifacts.clone(),
            feedback: ctx
                .context_state()
                .feedback
                .task
                .clone()
                .map(|text| DefaultParagraphView { text }),
            task: (!task.is_empty()).then(|| DefaultParagraphView {
                text: task.to_string(),
            }),
        }
        .build_root()?)
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
pub type TextAgent<B, S, E, T, EV = TextTurnEvent, TurnOutput = ()> =
    Agent<DefaultAgentViewModel<B, S, T>, E, Turn, EV, TurnOutput>;

/// Read-only session configuration shared across forks of an agent.
pub struct AgentConfig<
    VM,
    E,
    I = Turn,
    EV = TextTurnEvent,
    TurnOutput = (),
    Authoring = LegacyAuthoring,
> where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring>,
    E: LLMExecutor<I, EV>,
{
    /// Captures and renders what the language agent sees.
    pub view: VM,

    /// LLM model identifier, e.g. `"deepseek/deepseek-v3.2"`.
    pub model: StorageString,

    /// Maximum output tokens per call.
    pub max_tokens: u64,

    pub executor: std::marker::PhantomData<(E, I, EV, TurnOutput, Authoring)>,
}

impl<VM, E, I, EV, TurnOutput, Authoring> fmt::Debug
    for AgentConfig<VM, E, I, EV, TurnOutput, Authoring>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring> + fmt::Debug,
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

type SharedAgentSession<I, CS> = Arc<RwLock<AgentSession<I, CS>>>;

/// Stateful LLM agent parameterized over view model, executor, and transcript.
///
/// Read-only config lives in [`AgentConfig`] (behind `Arc`). Mutable prompt
/// state lives in one [`AgentSession`] so context and view cursor are always
/// snapshotted and committed together.
pub struct Agent<VM, E, I = Turn, EV = TextTurnEvent, TurnOutput = (), Authoring = LegacyAuthoring>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring>,
    E: LLMExecutor<I, EV>,
{
    /// Read-only configuration shared with forks.
    pub config: Arc<AgentConfig<VM, E, I, EV, TurnOutput, Authoring>>,

    session: SharedAgentSession<I, VM::ContextState>,

    turn_lock: Arc<Mutex<()>>,
    observers: Vec<AgentTurnObserverHandle>,
}

impl<VM, E, I, EV, TurnOutput, Authoring> fmt::Debug for Agent<VM, E, I, EV, TurnOutput, Authoring>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring> + fmt::Debug,
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
pub struct AgentSessionWriteGuard<'a, I, CS> {
    session_guard: RwLockWriteGuard<'a, AgentSession<I, CS>>,
    _turn_guard: MutexGuard<'a, ()>,
}

impl<I, CS> Deref for AgentSessionWriteGuard<'_, I, CS> {
    type Target = AgentSession<I, CS>;

    fn deref(&self) -> &Self::Target {
        &self.session_guard
    }
}

impl<I, CS> DerefMut for AgentSessionWriteGuard<'_, I, CS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session_guard
    }
}

impl<B, S, E, T, EV, TurnOutput> Agent<DefaultAgentViewModel<B, S, T>, E, Turn, EV, TurnOutput>
where
    B: ContextViewBuilder + Clone + Send + Sync,
    E: LLMExecutor<Turn, EV> + Clone,
    B::View: PomAgentView<Root = XmlNode> + Clone + Send + Sync + 'static,
    B::Source: Sync,
    S: PomAgentView<Root = Document> + Clone + Send + Sync + 'static,
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
        system_document: S,
        model: impl Into<String>,
        max_tokens: u64,
        transform: T,
    ) -> Self {
        let model = model.into();
        let view = DefaultAgentViewModel::new(context_builder, system_document, transform);
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

impl<VM, E, I, EV, TurnOutput, Authoring> Agent<VM, E, I, EV, TurnOutput, Authoring>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring> + Clone,
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
    pub async fn session(&self) -> RwLockReadGuard<'_, AgentSession<I, VM::ContextState>> {
        self.session.read().await
    }

    /// Mutate committed session state after any in-flight turn completes.
    pub async fn session_mut(&self) -> AgentSessionWriteGuard<'_, I, VM::ContextState> {
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
    pub fn call<'a>(
        &'a self,
        call_id: &'a str,
    ) -> AgentTurnBuilder<'a, VM, E, I, EV, TurnOutput, Authoring> {
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
            call_props: None,
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
pub struct AgentTurnBuilder<
    'a,
    VM,
    E,
    I,
    EV = TextTurnEvent,
    TurnOutput = (),
    Authoring = LegacyAuthoring,
> where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring>,
    E: LLMExecutor<I, EV>,
{
    agent: &'a Agent<VM, E, I, EV, TurnOutput, Authoring>,
    call_id: &'a str,
    task: String,
    call_props: Option<VM::CallProps>,
    side_sinks: Vec<Box<dyn TurnSink<EV, Output = ()>>>,
    observers: Vec<AgentTurnObserverHandle>,
    max_loops: usize,
    max_context_preparations: usize,
}

impl<'a, VM, E, I, EV, TurnOutput, Authoring>
    AgentTurnBuilder<'a, VM, E, I, EV, TurnOutput, Authoring>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    EV: Send + 'static,
    TurnOutput: Send + Sync + 'static,
{
    /// Set the call-specific task passed to `AgentViewModel::build_user_document`.
    ///
    /// The view model owns where and how this value appears in its typed POM
    /// document. Examples:
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

    /// Set typed input for this logical call.
    ///
    /// The value remains private to this builder. Component renders borrow it
    /// again when context preparation retries or an agent loop continues, but
    /// it is never copied into persistent prompt context or diff state.
    pub fn with_props(mut self, props: VM::CallProps) -> Self {
        self.call_props = Some(props);
        tracing::debug!(
            target: "agentview::agent",
            call_id = self.call_id,
            props_type = type_name::<VM::CallProps>(),
            "configured typed agent call props"
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

    /// Execute with a sink created only after final context preparation.
    ///
    /// Component runtimes use this boundary to keep a prompt contract and its
    /// runtime binding on the same final preparation attempt. The factory is
    /// not called for discarded history replacements or preparation failures.
    pub async fn execute_with_sink_factory<S, BuildSink>(
        self,
        source: &VM::Source,
        executor: &E,
        build_sink: BuildSink,
    ) -> anyhow::Result<TurnOutput>
    where
        S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
        BuildSink: FnOnce(VM::Binding) -> anyhow::Result<S> + Send,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            call_props,
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
                call_props: call_props.as_ref(),
                build_sink,
                side_sinks,
                observers,
                max_context_preparations,
            },
        )
        .await
        .map(|outcome| outcome.sink_output)
    }

    pub async fn execute_loop_with_sink_factory<S, BuildSink>(
        self,
        source: &VM::Source,
        executor: &E,
        mut build_sink: BuildSink,
    ) -> anyhow::Result<()>
    where
        S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
        BuildSink: FnMut(VM::Binding) -> anyhow::Result<S> + Send,
    {
        let AgentTurnBuilder {
            agent,
            call_id,
            mut task,
            call_props,
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
                    call_props: call_props.as_ref(),
                    build_sink: &mut build_sink,
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

impl<'a, VM, E, I, EV, TurnOutput, Authoring>
    AgentTurnBuilder<'a, VM, E, I, EV, TurnOutput, Authoring>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring, Binding = ()>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    EV: Send + 'static,
    TurnOutput: Send + Sync + 'static,
{
    /// Execute a legacy empty-binding turn with an explicitly supplied sink.
    pub async fn execute_with_sink<S>(
        self,
        source: &VM::Source,
        executor: &E,
        sink: S,
    ) -> anyhow::Result<TurnOutput>
    where
        S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
    {
        self.execute_with_sink_factory(source, executor, move |()| Ok(sink))
            .await
    }

    /// Execute a legacy empty-binding control loop with a fresh sink per turn.
    pub async fn execute_loop_with<S, BuildSink>(
        self,
        source: &VM::Source,
        executor: &E,
        mut build_sink: BuildSink,
    ) -> anyhow::Result<()>
    where
        S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
        BuildSink: FnMut() -> S + Send,
    {
        self.execute_loop_with_sink_factory(source, executor, move |()| Ok(build_sink()))
            .await
    }
}

impl<'a, VM, E, I, EV, Authoring> AgentTurnBuilder<'a, VM, E, I, EV, (), Authoring>
where
    VM: AgentTurnAuthor<I, (), Authoring, Binding = ()>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    EV: Send + 'static,
{
    /// Execute the model-backed turn with the default no-op sink.
    ///
    /// `source` — the app runtime/source used to run the LLM call.
    ///
    /// Pipeline:
    /// 1. Capture the current typed view.
    /// 2. Build and resolve the complete system/user POM documents.
    /// 3. Render the resolved documents and execute with the per-call sinks.
    /// 4. On success, commit history and the candidate user-document cursor.
    /// 5. On failure, return `Err` with the session state unchanged.
    pub async fn execute(self, source: &VM::Source, executor: &E) -> anyhow::Result<()> {
        let AgentTurnBuilder {
            agent,
            call_id,
            task,
            call_props,
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
                call_props: call_props.as_ref(),
                build_sink: |_| Ok(NoopTurnSink),
                side_sinks,
                observers,
                max_context_preparations,
            },
        )
        .await
        .map(|_: CommittedAgentTurn<()>| ())
    }
}

struct AgentTurnExecution<'a, Source, E, BuildSink, EV, CallProps> {
    call_id: &'a str,
    source: &'a Source,
    executor: &'a E,
    task: String,
    call_props: Option<&'a CallProps>,
    build_sink: BuildSink,
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

async fn execute_agent_turn_with_sink<VM, E, I, S, BuildSink, EV, TurnOutput, Authoring>(
    agent: &Agent<VM, E, I, EV, TurnOutput, Authoring>,
    execution: AgentTurnExecution<'_, VM::Source, E, BuildSink, EV, VM::CallProps>,
) -> anyhow::Result<CommittedAgentTurn<TurnOutput>>
where
    VM: AgentTurnAuthor<I, TurnOutput, Authoring>,
    E: LLMExecutor<I, EV> + Clone + Send + Sync + 'static,
    I: Clone + Send + 'static,
    S: TurnSink<EV, Output = TurnOutput> + Send + 'static,
    BuildSink: FnOnce(VM::Binding) -> anyhow::Result<S> + Send,
    TurnOutput: Send + Sync + 'static,
    EV: Send + 'static,
{
    let AgentTurnExecution {
        call_id,
        source,
        executor,
        task,
        call_props,
        build_sink,
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
    let (request, next_user_document_cursor, binding) = loop {
        // ── Build request against the private turn draft ──────────────────
        let ctx_for_request = draft_session.context();
        let committed_history_len = ctx_for_request.history().len();
        let current_view = agent.config.view.capture_view(source).await;
        let prepared = agent
            .config
            .view
            .prepare_turn(
                ctx_for_request,
                source,
                call_id,
                task.clone().into(),
                call_props,
                &current_view,
            )
            .await?;
        let (system_document, user_document, binding) = prepared.into_parts();
        let system = render_pom_document(&resolve_system_document(system_document))?;
        let history = agent.config.view.history(ctx_for_request);
        let (resolved_user_document, next_user_document_cursor) =
            resolve_user_document(user_document, draft_session.user_document_cursor())?;
        let user = render_pom_document(&resolved_user_document)?;
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
            .prepare_context(&request, preparation_budget)
            .await?
        {
            ContextPreparation::Ready => {
                break (request, next_user_document_cursor, binding);
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
            ContextPreparation::ResyncUserDocument => {
                if preparation_replacements >= max_context_preparations {
                    anyhow::bail!(
                        "agent turn `{call_id}` exceeded context preparation replacement limit {max_context_preparations}"
                    );
                }
                preparation_replacements += 1;

                draft_session.reset_user_document_cursor();
                tracing::debug!(
                    target: "agentview::agent",
                    call_id,
                    preparation_replacements,
                    "reset User document baseline during context preparation"
                );
            }
        }
    };
    let sink = build_sink(binding)?;

    notify_observers(
        &observers,
        AgentTurnEvent::SystemPromptRendered {
            call_id: call_id.into(),
            text: request.system.clone(),
        },
    )
    .await;
    tracing::debug!(
        target: "agentview::agent",
        call_id,
        history_len = request.history.len(),
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
            draft_session.set_system_snapshot(request_for_commit.system.clone());
            draft_session.set_user_document_cursor(next_user_document_cursor);
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
