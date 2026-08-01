//! Compatibility adapter from a root component to the current agent transaction.
//!
//! This path recompiles one combined System/User tree during every context
//! preparation. It does not provide the mounted epoch's System-once lifecycle;
//! new runtime work belongs to [`super::MountedEpoch`] and its future AgentLoop
//! owner.

use crate::{
    agent::{Agent, AgentTurnAuthor, AgentTurnBuilder, PreparedTurn, TurnFlow},
    agent_view::{AgentView as PomAgentView, AgentViewValue},
    llm_call::{
        AgentTurnRequest, ExecutorCommit, LLMExecutor, NoopTurnSink, TextTurnEvent, TurnSink,
    },
    pom::XmlNode,
    prompt_context::{PromptContext, Turn},
    StorageString,
};

use super::{
    compile_component, ComponentError, HookPlan, StreamingBinding, StreamingComponentError,
    StreamingComponentSink, StreamingOutcome, View,
};

/// Authoring mode used by native component harnesses.
///
/// This is a type-level dispatch marker, not runtime state or another prompt
/// representation.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ComponentAuthoring;

/// Converts a compiled component hook plan into one existing [`TurnSink`].
///
/// The implementation is selected by the root component's binding type. This
/// keeps concrete sink and output types at the application boundary while the
/// heterogeneous reducer state inside a streaming plan remains private.
pub trait ComponentBinding<E>: Send + Sized + 'static
where
    E: Send + 'static,
{
    type Output: Send + Sync + 'static;
    type Sink: TurnSink<E, Output = Self::Output> + Send + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    fn bind(plan: HookPlan<Self>) -> Result<Self::Sink, Self::Error>;
}

impl<E> ComponentBinding<E> for ()
where
    E: Send + 'static,
{
    type Output = ();
    type Sink = NoopTurnSink;
    type Error = ComponentError;

    fn bind(plan: HookPlan<Self>) -> Result<Self::Sink, Self::Error> {
        if plan.is_empty() {
            Ok(NoopTurnSink)
        } else {
            Err(ComponentError::UnexpectedRuntimeBindings { count: plan.len() })
        }
    }
}

impl<Effect, Diagnostic> ComponentBinding<TextTurnEvent> for StreamingBinding<Effect, Diagnostic>
where
    Effect: Send + Sync + 'static,
    Diagnostic: Send + Sync + 'static,
{
    type Output = StreamingOutcome<Effect, Diagnostic>;
    type Sink = StreamingComponentSink<Effect, Diagnostic>;
    type Error = StreamingComponentError;

    fn bind(plan: HookPlan<Self>) -> Result<Self::Sink, Self::Error> {
        StreamingComponentSink::try_new(plan)
    }
}

/// Read-only inputs available during one pure root-component render.
pub struct ComponentTurnContext<'a, I, V, ContextState, Props> {
    context: &'a PromptContext<I, ContextState>,
    call_id: &'a str,
    task: &'a StorageString,
    props: &'a Props,
    current_view: &'a V,
}

impl<'a, I, V, ContextState, Props> ComponentTurnContext<'a, I, V, ContextState, Props> {
    pub fn context(&self) -> &'a PromptContext<I, ContextState> {
        self.context
    }

    pub fn call_id(&self) -> &'a str {
        self.call_id
    }

    pub fn task(&self) -> &'a str {
        self.task
    }

    /// Typed, call-local input supplied through
    /// [`crate::agent::AgentTurnBuilder::with_props`].
    pub fn props(&self) -> &'a Props {
        self.props
    }

    pub fn current_view(&self) -> &'a V {
        self.current_view
    }
}

/// Compatibility harness around one pure combined System/User component tree.
///
/// [`render`](ComponentHarness::render) is the pure, synchronous boundary. The
/// async capture and commit methods belong to the host transaction; they are
/// not component hooks and reducers must not call them.
///
/// This trait is kept for the existing [`Agent`] adapter. It renders both roles
/// on every preparation and therefore must not be used to claim mounted
/// System-once semantics. The target component runtime mounts a
/// [`super::SystemView`] into [`super::MountedEpoch`] and renders only
/// [`super::UserView`] per preparation.
#[async_trait::async_trait]
pub trait ComponentHarness<I = Turn>: Clone + Send + Sync
where
    I: Clone + Send + Sync + 'static,
{
    type Source: Sync;
    type View: PomAgentView<Root = XmlNode> + AgentViewValue + Clone + Send + Sync + 'static;
    type ContextState: Default + Clone + Send + Sync + 'static;
    type CallProps: Send + Sync + 'static;
    type Event: Send + 'static;
    type Output: Send + Sync + 'static;
    type Binding: ComponentBinding<Self::Event, Output = Self::Output>;

    fn history(&self, context: &PromptContext<I, Self::ContextState>) -> Vec<I> {
        let mut history = context.history().to_vec();
        history.extend(context.working_set().iter().cloned());
        history
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View;

    fn render(
        &self,
        input: ComponentTurnContext<'_, I, Self::View, Self::ContextState, Self::CallProps>,
    ) -> View<Self::Binding>;

    async fn commit_turn(
        &self,
        context: &mut PromptContext<I, Self::ContextState>,
        request: &AgentTurnRequest<I>,
        executor_commit: ExecutorCommit<I>,
        output: &mut Self::Output,
    ) -> anyhow::Result<TurnFlow>;
}

/// Adapts one compatibility [`ComponentHarness`] to [`crate::agent::Agent`].
///
/// This implements [`AgentTurnAuthor`] rather than the older split-document
/// [`crate::agent::AgentViewModel`] contract so system POM, user POM, and the
/// matching binding plan always come from the same render attempt. It is not
/// the mounted System/User owner and does not preserve a System epoch across
/// preparations.
#[derive(Debug, Clone)]
pub struct ComponentAgentViewModel<C> {
    component: C,
}

/// Provider-backed compatibility agent driven by one combined component harness.
pub type ComponentAgent<C, E, I = Turn> = Agent<
    ComponentAgentViewModel<C>,
    E,
    I,
    <C as ComponentHarness<I>>::Event,
    <C as ComponentHarness<I>>::Output,
    ComponentAuthoring,
>;

impl<C> ComponentAgentViewModel<C> {
    pub fn new(component: C) -> Self {
        Self { component }
    }

    pub fn component(&self) -> &C {
        &self.component
    }

    pub fn into_component(self) -> C {
        self.component
    }
}

#[async_trait::async_trait]
impl<I, C> AgentTurnAuthor<I, C::Output, ComponentAuthoring> for ComponentAgentViewModel<C>
where
    I: Clone + Send + Sync + 'static,
    C: ComponentHarness<I>,
{
    type Source = C::Source;
    type View = C::View;
    type ContextState = C::ContextState;
    type CallProps = C::CallProps;
    type Binding = HookPlan<C::Binding>;

    fn history(&self, context: &PromptContext<I, Self::ContextState>) -> Vec<I> {
        self.component.history(context)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        self.component.capture_view(source).await
    }

    async fn prepare_turn(
        &self,
        context: &PromptContext<I, Self::ContextState>,
        _source: &Self::Source,
        call_id: &str,
        task: StorageString,
        call_props: Option<&Self::CallProps>,
        current_view: &Self::View,
    ) -> anyhow::Result<PreparedTurn<Self::Binding>> {
        let props = call_props.ok_or(ComponentError::MissingCallProps {
            expected: std::any::type_name::<C::CallProps>(),
        })?;
        let tree = self.component.render(ComponentTurnContext {
            context,
            call_id,
            task: &task,
            props,
            current_view,
        })?;
        let plan = compile_component(tree)?;
        let (system, user, hooks) = plan.into_parts();
        Ok(PreparedTurn::new(system, user, hooks))
    }

    async fn commit_turn(
        &self,
        context: &mut PromptContext<I, Self::ContextState>,
        request: &AgentTurnRequest<I>,
        executor_commit: ExecutorCommit<I>,
        output: &mut C::Output,
    ) -> anyhow::Result<TurnFlow>
    where
        C::Output: Send + Sync,
    {
        self.component
            .commit_turn(context, request, executor_commit, output)
            .await
    }
}

impl<C, Executor, I>
    Agent<ComponentAgentViewModel<C>, Executor, I, C::Event, C::Output, ComponentAuthoring>
where
    I: Clone + Send + Sync + 'static,
    C: ComponentHarness<I>,
    Executor: LLMExecutor<I, C::Event> + Clone,
{
    /// Construct a provider-backed agent from a host harness whose `render`
    /// method returns the pure root component tree.
    pub fn with_component(
        component: C,
        model: impl Into<StorageString>,
        max_tokens: u64,
        context: PromptContext<I, C::ContextState>,
    ) -> Self {
        Self::with_view(
            ComponentAgentViewModel::new(component),
            model,
            max_tokens,
            context,
        )
    }
}

impl<'a, C, Executor, I>
    AgentTurnBuilder<
        'a,
        ComponentAgentViewModel<C>,
        Executor,
        I,
        C::Event,
        C::Output,
        ComponentAuthoring,
    >
where
    I: Clone + Send + Sync + 'static,
    C: ComponentHarness<I>,
    Executor: LLMExecutor<I, C::Event> + Clone + Send + Sync + 'static,
{
    /// Execute a root component with its final prepared hook plan.
    ///
    /// Binding occurs after `ContextPreparation::Ready`; plans from discarded
    /// history-replacement or User-resync attempts are dropped without
    /// registering handlers.
    pub async fn execute_component(
        self,
        source: &C::Source,
        executor: &Executor,
    ) -> anyhow::Result<C::Output> {
        self.execute_with_sink_factory(source, executor, |plan| {
            <C::Binding as ComponentBinding<C::Event>>::bind(plan).map_err(anyhow::Error::new)
        })
        .await
    }

    /// Execute repeated component turns, rebinding the hook plan produced by
    /// each final preparation attempt.
    pub async fn execute_component_loop(
        self,
        source: &C::Source,
        executor: &Executor,
    ) -> anyhow::Result<()> {
        self.execute_loop_with_sink_factory(source, executor, |plan| {
            <C::Binding as ComponentBinding<C::Event>>::bind(plan).map_err(anyhow::Error::new)
        })
        .await
    }
}
