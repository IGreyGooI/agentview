use std::{
    collections::HashMap, convert::Infallible, error::Error, fmt, marker::PhantomData, sync::Arc,
};

use tokio::sync::Mutex as TokioMutex;

use crate::{
    llm_call::{TextTurnEvent, TurnSink},
    pom::{Document, XmlName, XmlNode},
    stream_parser::{HermesCallbackPhase, HermesParser, HermesParserError, XmlElement},
};

use super::experimental::{binding, keyed, view};
use super::{
    component, pom_view, BindingAbortAck, BindingAbortReason, BindingFactoryPlan, BindingFailure,
    BindingFault, BindingId, BindingInstance, BindingOrigin, BindingPhase, CommitStager, Component,
    ComponentError, DurableComponent, HookPlan, IntoComponentNode, Never, PreparedSessionMutation,
    PreparedUserTurn, ProviderAttemptIdentity, ProviderCapabilityPlan, ProviderDispatchFault,
    ProviderDispatcherAbort, ProviderDispatcherAbortOutcome, ProviderToolAttemptError,
    ProviderToolCall, ProviderToolCallOutcome, ProviderToolResult, PublicationFingerprintContext,
    PublicationFingerprintFactory, PublicationRequestId, PublicationStagingFailure,
    PublicationStagingPlan, RuntimeContract, RuntimeRoute, StagedPublication,
    StagedStreamingPublication, StreamingPublicationStagingFailure,
    StreamingPublicationStagingResult, TurnBindingCx, TurnChannels, TurnEmission, View, ViewExt,
};
use super::{
    provider::ProviderDispatchRuntime,
    provision::{AttemptBindingCx, FactoryDeclaration, RuntimeDeclaration},
};

type Reducer<S, E, D> =
    Box<dyn Fn(&mut S, &XmlElement) -> StreamUpdate<E, D> + Send + Sync + 'static>;
type FinishReducer<S, E, D> = Box<dyn FnOnce(S) -> StreamUpdate<E, D> + Send + 'static>;
type MountedUpdate<C> = StreamUpdate<TurnEmission<C>, <C as TurnChannels>::Diagnostic>;
type MountedInitializer<Props, S, C> = Arc<
    dyn for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<S, BindingFailure>
        + Send
        + Sync
        + 'static,
>;
type ReusableReducer<S, C> = Arc<
    dyn Fn(&mut S, &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> + Send + Sync + 'static,
>;
type ReusableFinishReducer<S, C> =
    Arc<dyn Fn(&mut S) -> Result<MountedUpdate<C>, BindingFailure> + Send + Sync + 'static>;
type ReusableAbortReducer<S> =
    Arc<dyn Fn(&mut S, BindingAbortReason) -> BindingAbortAck + Send + Sync + 'static>;

/// Pure values produced by one streaming reducer callback.
///
/// Emissions and diagnostics are independent collections: a callback may
/// produce either, both, or neither. `Result<Vec<E>, D>` remains accepted by
/// reducer builders as a compatibility shorthand; its `Err` is a non-terminal
/// diagnostic, not a request failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct StreamUpdate<E, D> {
    emissions: Vec<E>,
    diagnostics: Vec<D>,
}

impl<E, D> Default for StreamUpdate<E, D> {
    fn default() -> Self {
        Self {
            emissions: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

impl<E, D> StreamUpdate<E, D> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_emission(emission: E) -> Self {
        Self {
            emissions: vec![emission],
            diagnostics: Vec::new(),
        }
    }

    pub fn from_diagnostic(diagnostic: D) -> Self {
        Self {
            emissions: Vec::new(),
            diagnostics: vec![diagnostic],
        }
    }

    pub fn with_emission(mut self, emission: E) -> Self {
        self.emissions.push(emission);
        self
    }

    pub fn with_diagnostic(mut self, diagnostic: D) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    pub fn emissions(&self) -> &[E] {
        &self.emissions
    }

    pub fn diagnostics(&self) -> &[D] {
        &self.diagnostics
    }

    pub fn is_empty(&self) -> bool {
        self.emissions.is_empty() && self.diagnostics.is_empty()
    }

    pub(crate) fn map_emissions<C>(self, mut map: impl FnMut(E) -> C) -> StreamUpdate<C, D> {
        StreamUpdate {
            emissions: self.emissions.into_iter().map(&mut map).collect(),
            diagnostics: self.diagnostics,
        }
    }

    pub(crate) fn map_diagnostics<C>(self, mut map: impl FnMut(D) -> C) -> StreamUpdate<E, C> {
        StreamUpdate {
            emissions: self.emissions,
            diagnostics: self.diagnostics.into_iter().map(&mut map).collect(),
        }
    }

    /// Consume the update so hosts can move non-`Clone` lane values into their
    /// interpreters.
    pub fn into_parts(self) -> (Vec<E>, Vec<D>) {
        (self.emissions, self.diagnostics)
    }

    pub(crate) fn append(&mut self, other: Self) {
        self.emissions.extend(other.emissions);
        self.diagnostics.extend(other.diagnostics);
    }
}

/// Runtime context for one live effect emitted by a pure reducer callback.
///
/// The sequence is scoped to one provider attempt and follows parser wire
/// order. The binding origin identifies the component and route that emitted
/// the effect; the call label remains observational and is not identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEffectContext {
    origin: BindingOrigin,
    phase: BindingPhase,
    sequence: u64,
}

impl LiveEffectContext {
    pub fn origin(&self) -> &BindingOrigin {
        &self.origin
    }

    pub fn phase(&self) -> BindingPhase {
        self.phase
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Context supplied when an attempt's live scope must be cancelled or
/// compensated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAbortContext {
    identity: ProviderAttemptIdentity,
    reason: BindingAbortReason,
    applied_effects: u64,
}

impl LiveAbortContext {
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn reason(&self) -> BindingAbortReason {
        self.reason
    }

    pub fn applied_effects(&self) -> u64 {
        self.applied_effects
    }
}

/// Acknowledgement from the host after cancelling one live scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveEffectAbortAck {
    NoEffectsApplied,
    CompensationCompleted,
}

/// Host-selected interpreter for one harness's typed live-effect lane.
///
/// `apply` runs inside the parser callback and is awaited before parsing can
/// continue. Reducers remain synchronous and pure; application I/O belongs in
/// this runtime. `abort` must cancel or compensate every effect previously
/// acknowledged in the same live scope.
#[async_trait::async_trait]
pub trait LiveEffectRuntime<L>: Send + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn apply(&mut self, context: &LiveEffectContext, effect: L) -> Result<(), Self::Error>;

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error>;
}

/// Runtime for harnesses whose live lane is statically uninhabited.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLiveEffects;

#[async_trait::async_trait]
impl LiveEffectRuntime<Never> for NoLiveEffects {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        effect: Never,
    ) -> Result<(), Self::Error> {
        effect.absurd()
    }

    async fn abort(
        &mut self,
        _context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        Ok(LiveEffectAbortAck::NoEffectsApplied)
    }
}

/// Type-erased error returned by a host live-effect runtime.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct LiveEffectFailure {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

impl LiveEffectFailure {
    fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }

    pub fn source_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.as_ref()
    }
}

/// Terminal host failure while applying one live effect in parser order.
#[derive(Debug, thiserror::Error)]
#[error(
    "live effect {sequence} from `{binding}` on `{route}` failed during {phase}: {source}",
    sequence = .context.sequence(),
    binding = .context.origin().binding_id(),
    route = .context.origin().route(),
    phase = .context.phase(),
)]
pub struct LiveEffectFault {
    context: LiveEffectContext,
    #[source]
    source: LiveEffectFailure,
}

impl LiveEffectFault {
    pub fn context(&self) -> &LiveEffectContext {
        &self.context
    }

    pub fn source_failure(&self) -> &LiveEffectFailure {
        &self.source
    }
}

/// Host failure while compensating an aborted live scope.
#[derive(Debug, thiserror::Error)]
#[error(
    "live scope `{scope}` failed to abort after {applied} applied effect(s): {source}",
    scope = .context.identity().live_scope_id(),
    applied = .context.applied_effects(),
)]
pub struct LiveAbortFault {
    context: LiveAbortContext,
    #[source]
    source: LiveEffectFailure,
}

impl LiveAbortFault {
    pub fn context(&self) -> &LiveAbortContext {
        &self.context
    }

    pub fn source_failure(&self) -> &LiveEffectFailure {
        &self.source
    }
}

/// Result of invoking the host runtime while aborting an attempt.
#[derive(Debug)]
pub enum LiveAbortOutcome {
    Acknowledged(LiveEffectAbortAck),
    Failed(LiveAbortFault),
}

impl LiveAbortOutcome {
    pub fn acknowledgement(&self) -> Option<LiveEffectAbortAck> {
        match self {
            Self::Acknowledged(acknowledgement) => Some(*acknowledgement),
            Self::Failed(_) => None,
        }
    }

    pub fn failure(&self) -> Option<&LiveAbortFault> {
        match self {
            Self::Acknowledged(_) => None,
            Self::Failed(failure) => Some(failure),
        }
    }
}

#[async_trait::async_trait]
trait ErasedLiveEffectRuntime<L>: Send {
    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        effect: L,
    ) -> Result<(), LiveEffectFailure>;

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, LiveEffectFailure>;
}

struct TypedLiveEffectRuntime<R>(R);

#[async_trait::async_trait]
impl<L, R> ErasedLiveEffectRuntime<L> for TypedLiveEffectRuntime<R>
where
    L: Send + 'static,
    R: LiveEffectRuntime<L>,
{
    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        effect: L,
    ) -> Result<(), LiveEffectFailure> {
        self.0
            .apply(context, effect)
            .await
            .map_err(LiveEffectFailure::new)
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, LiveEffectFailure> {
        self.0.abort(context).await.map_err(LiveEffectFailure::new)
    }
}

pub(crate) struct LiveEffectDispatcher<L> {
    runtime: Box<dyn ErasedLiveEffectRuntime<L>>,
    next_sequence: u64,
    applied_effects: u64,
}

impl<L> LiveEffectDispatcher<L>
where
    L: Send + 'static,
{
    pub(crate) fn new(runtime: impl LiveEffectRuntime<L>) -> Self {
        Self {
            runtime: Box::new(TypedLiveEffectRuntime(runtime)),
            next_sequence: 0,
            applied_effects: 0,
        }
    }

    pub(crate) async fn apply(
        &mut self,
        origin: BindingOrigin,
        phase: BindingPhase,
        effect: L,
    ) -> Result<(), LiveEffectFault> {
        let context = LiveEffectContext {
            origin,
            phase,
            sequence: self.next_sequence,
        };
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("live effect sequence space exhausted");
        match self.runtime.apply(&context, effect).await {
            Ok(()) => {
                self.applied_effects = self
                    .applied_effects
                    .checked_add(1)
                    .expect("live effect count space exhausted");
                Ok(())
            }
            Err(source) => Err(LiveEffectFault { context, source }),
        }
    }

    pub(crate) async fn abort(
        &mut self,
        identity: ProviderAttemptIdentity,
        reason: BindingAbortReason,
    ) -> LiveAbortOutcome {
        let context = LiveAbortContext {
            identity,
            reason,
            applied_effects: self.applied_effects,
        };
        match self.runtime.abort(&context).await {
            Ok(acknowledgement) => LiveAbortOutcome::Acknowledged(acknowledgement),
            Err(source) => LiveAbortOutcome::Failed(LiveAbortFault { context, source }),
        }
    }
}

pub(crate) type SharedLiveEffectRuntime<L> = Arc<TokioMutex<LiveEffectDispatcher<L>>>;
pub(crate) type SharedCommitBuffer<K> = Arc<TokioMutex<Vec<K>>>;

/// Read-only provider result supplied to the host's publication transaction.
#[derive(Debug, Clone, Copy)]
pub struct TurnPublication<'a> {
    identity: &'a ProviderAttemptIdentity,
    raw_output: &'a str,
    provider_results: &'a [ProviderToolResult],
}

impl<'a> TurnPublication<'a> {
    pub(crate) fn provider(
        identity: &'a ProviderAttemptIdentity,
        provider_results: &'a [ProviderToolResult],
    ) -> Self {
        Self {
            identity,
            raw_output: "",
            provider_results,
        }
    }

    pub(crate) fn combined(
        identity: &'a ProviderAttemptIdentity,
        raw_output: &'a str,
        provider_results: &'a [ProviderToolResult],
    ) -> Self {
        Self {
            identity,
            raw_output,
            provider_results,
        }
    }

    pub fn identity(&self) -> &'a ProviderAttemptIdentity {
        self.identity
    }

    pub fn raw_output(&self) -> &'a str {
        self.raw_output
    }

    pub fn provider_results(&self) -> &'a [ProviderToolResult] {
        self.provider_results
    }
}

/// Host-owned publication boundary for a finished provider attempt.
///
/// The implementation must return `Ok` only after the session/draft mutation
/// is visible at its real atomic publication point. AgentView creates the
/// receipt and releases pending commit values only after this future succeeds.
#[async_trait::async_trait]
pub trait TurnPublisher: Send {
    type Error: Error + Send + Sync + 'static;

    async fn publish(&mut self, publication: TurnPublication<'_>) -> Result<(), Self::Error>;
}

/// Framework-issued proof that one provider attempt crossed publication.
///
/// There is deliberately no public constructor. Application code obtains this
/// value only inside [`PublishedStreamingAttempt`], after [`TurnPublisher`]
/// returned successfully. It proves phase ordering in the current runtime; it
/// is not a durable session revision or cross-process delivery identity.
#[derive(Debug)]
pub struct PublishedTurnReceipt {
    identity: ProviderAttemptIdentity,
}

impl PublishedTurnReceipt {
    pub(crate) fn new(identity: ProviderAttemptIdentity) -> Self {
        Self { identity }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }
}

/// Values accepted from a reducer callback.
pub trait IntoStreamUpdate<E, D> {
    fn into_stream_update(self) -> StreamUpdate<E, D>;
}

impl<E, D> IntoStreamUpdate<E, D> for StreamUpdate<E, D> {
    fn into_stream_update(self) -> StreamUpdate<E, D> {
        self
    }
}

impl<E, D> IntoStreamUpdate<E, D> for Vec<E> {
    fn into_stream_update(self) -> StreamUpdate<E, D> {
        StreamUpdate {
            emissions: self,
            diagnostics: Vec::new(),
        }
    }
}

impl<E, D> IntoStreamUpdate<E, D> for Result<Vec<E>, D> {
    fn into_stream_update(self) -> StreamUpdate<E, D> {
        match self {
            Ok(emissions) => StreamUpdate {
                emissions,
                diagnostics: Vec::new(),
            },
            Err(diagnostic) => StreamUpdate::from_diagnostic(diagnostic),
        }
    }
}

/// Begins authoring a streaming XML component with typed effects and diagnostics.
pub struct StreamingXml<E, D> {
    contract: XmlNode,
    _types: std::marker::PhantomData<fn() -> (E, D)>,
}

impl<E, D> StreamingXml<E, D> {
    pub fn new(contract: XmlNode) -> Self {
        Self {
            contract,
            _types: std::marker::PhantomData,
        }
    }

    /// Supply state owned only by this streamed response.
    pub fn init_state<S>(self, state: S) -> StreamingXmlReducer<S, E, D> {
        StreamingXmlReducer {
            contract: self.contract,
            state,
            on_open: None,
            on_stream: None,
            on_complete: None,
            finish: None,
        }
    }
}

/// Pure reducer declarations attached to one streaming XML contract.
pub struct StreamingXmlReducer<S, E, D> {
    contract: XmlNode,
    state: S,
    on_open: Option<Reducer<S, E, D>>,
    on_stream: Option<Reducer<S, E, D>>,
    on_complete: Option<Reducer<S, E, D>>,
    finish: Option<FinishReducer<S, E, D>>,
}

impl<S, E, D> StreamingXmlReducer<S, E, D>
where
    S: Send + 'static,
    E: Send + 'static,
    D: Send + 'static,
{
    pub fn on_open<R>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> R + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<E, D>,
    {
        self.on_open = Some(Box::new(move |state, element| {
            reducer(state, element).into_stream_update()
        }));
        self
    }

    pub fn on_stream<R>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> R + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<E, D>,
    {
        self.on_stream = Some(Box::new(move |state, element| {
            reducer(state, element).into_stream_update()
        }));
        self
    }

    pub fn on_complete<R>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> R + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<E, D>,
    {
        self.on_complete = Some(Box::new(move |state, element| {
            reducer(state, element).into_stream_update()
        }));
        self
    }

    pub fn finish<R>(mut self, reducer: impl FnOnce(S) -> R + Send + 'static) -> Self
    where
        R: IntoStreamUpdate<E, D>,
    {
        self.finish = Some(Box::new(move |state| reducer(state).into_stream_update()));
        self
    }

    /// Project the contract as POM and declare its matching stream binding.
    pub fn into_view(self) -> View<StreamingBinding<E, D>> {
        let tag = streaming_contract_tag(&self.contract)?;
        let stream_binding = StreamingBinding {
            tag: tag.clone(),
            handler: Box::new(TypedStreamingHandler {
                state: Some(self.state),
                on_open: self.on_open,
                on_stream: self.on_stream,
                on_complete: self.on_complete,
                finish: self.finish,
            }),
        };
        keyed(
            "agentview::component::StreamingXml",
            tag,
            view((
                Document::from_xml(self.contract),
                binding("stream", stream_binding),
            )),
        )
    }
}

impl<S, C> StreamingXmlReducer<S, TurnEmission<C>, C::Diagnostic>
where
    S: Send + 'static,
    C: TurnChannels,
{
    /// Project a reducer that emits a complete local channel bundle.
    ///
    /// The nominal return type intentionally exposes only whole-bundle channel
    /// mapping. It cannot use the single-lane `map_output`, `map_live`, or
    /// `map_commit` shortcuts.
    pub fn into_channels_view(self) -> StreamingChannelsView<C> {
        StreamingChannelsView::from_view(self.into_view())
    }
}

impl<C> StreamingXml<TurnEmission<C>, C::Diagnostic>
where
    C: TurnChannels,
{
    /// Declare an infallible reusable initializer for mounted attempts.
    pub fn state_with<Props, S>(
        self,
        initialize: impl Fn() -> S + Send + Sync + 'static,
    ) -> StreamingXmlFactoryReducer<S, C, Props>
    where
        Props: ?Sized + 'static,
        S: Send + 'static,
    {
        StreamingXmlFactoryReducer::new(self.contract, move |_| Ok(initialize()))
    }

    /// Declare a reusable initializer that reads typed per-turn props.
    pub fn try_state_with<Props, S, InitError>(
        self,
        initialize: impl for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<S, InitError>
            + Send
            + Sync
            + 'static,
    ) -> StreamingXmlFactoryReducer<S, C, Props>
    where
        Props: ?Sized + 'static,
        S: Send + 'static,
        InitError: Error + Send + Sync + 'static,
    {
        StreamingXmlFactoryReducer::new(self.contract, move |context| {
            initialize(context).map_err(BindingFailure::new)
        })
    }
}

/// Reusable reducer declaration retained by a mounted System epoch.
///
/// This declaration owns only reusable closures. Every provider attempt calls
/// the initializer with typed turn props and receives fresh owned state.
pub struct StreamingXmlFactoryReducer<S, C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    contract: XmlNode,
    initialize: MountedInitializer<Props, S, C>,
    on_open: Option<ReusableReducer<S, C>>,
    on_stream: Option<ReusableReducer<S, C>>,
    on_complete: Option<ReusableReducer<S, C>>,
    on_finish: Option<ReusableFinishReducer<S, C>>,
    on_abort: Option<ReusableAbortReducer<S>>,
}

impl<S, C, Props> StreamingXmlFactoryReducer<S, C, Props>
where
    S: Send + 'static,
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn new(
        contract: XmlNode,
        initialize: impl for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<S, BindingFailure>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            contract,
            initialize: Arc::new(initialize),
            on_open: None,
            on_stream: None,
            on_complete: None,
            on_finish: None,
            on_abort: None,
        }
    }

    pub fn on_open<R>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> R + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
    {
        self.on_open = Some(Arc::new(move |state, element| {
            Ok(reducer(state, element).into_stream_update())
        }));
        self
    }

    pub fn try_on_open<R, ReducerError>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> Result<R, ReducerError> + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
        ReducerError: Error + Send + Sync + 'static,
    {
        self.on_open = Some(Arc::new(move |state, element| {
            reducer(state, element)
                .map(IntoStreamUpdate::into_stream_update)
                .map_err(BindingFailure::new)
        }));
        self
    }

    pub fn on_stream<R>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> R + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
    {
        self.on_stream = Some(Arc::new(move |state, element| {
            Ok(reducer(state, element).into_stream_update())
        }));
        self
    }

    pub fn try_on_stream<R, ReducerError>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> Result<R, ReducerError> + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
        ReducerError: Error + Send + Sync + 'static,
    {
        self.on_stream = Some(Arc::new(move |state, element| {
            reducer(state, element)
                .map(IntoStreamUpdate::into_stream_update)
                .map_err(BindingFailure::new)
        }));
        self
    }

    pub fn on_complete<R>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> R + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
    {
        self.on_complete = Some(Arc::new(move |state, element| {
            Ok(reducer(state, element).into_stream_update())
        }));
        self
    }

    pub fn try_on_complete<R, ReducerError>(
        mut self,
        reducer: impl Fn(&mut S, &XmlElement) -> Result<R, ReducerError> + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
        ReducerError: Error + Send + Sync + 'static,
    {
        self.on_complete = Some(Arc::new(move |state, element| {
            reducer(state, element)
                .map(IntoStreamUpdate::into_stream_update)
                .map_err(BindingFailure::new)
        }));
        self
    }

    /// Run after strict parser finalization while keeping state abortable.
    pub fn on_finish<R>(mut self, reducer: impl Fn(&mut S) -> R + Send + Sync + 'static) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
    {
        self.on_finish = Some(Arc::new(move |state| {
            Ok(reducer(state).into_stream_update())
        }));
        self
    }

    pub fn try_on_finish<R, ReducerError>(
        mut self,
        reducer: impl Fn(&mut S) -> Result<R, ReducerError> + Send + Sync + 'static,
    ) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
        ReducerError: Error + Send + Sync + 'static,
    {
        self.on_finish = Some(Arc::new(move |state| {
            reducer(state)
                .map(IntoStreamUpdate::into_stream_update)
                .map_err(BindingFailure::new)
        }));
        self
    }

    pub fn finish<R>(self, reducer: impl Fn(&mut S) -> R + Send + Sync + 'static) -> Self
    where
        R: IntoStreamUpdate<TurnEmission<C>, C::Diagnostic>,
    {
        self.on_finish(reducer)
    }

    /// Register pure local teardown. External compensation belongs to P4.
    pub fn on_abort(
        mut self,
        reducer: impl Fn(&mut S, BindingAbortReason) -> BindingAbortAck + Send + Sync + 'static,
    ) -> Self {
        self.on_abort = Some(Arc::new(reducer));
        self
    }

    /// Project one System POM contract and its matching reusable XML factory.
    ///
    /// This is the mounted component entry point. It preserves the contract,
    /// reducer factory, and typed channel declaration as one value; no parser
    /// state is created until a final prepared provider attempt starts.
    pub fn into_component(self) -> Component<C, Props> {
        let contract = self.contract.clone();
        component((|| {
            let (tag, declaration) = self.into_factory_parts()?;

            keyed(
                "agentview::component::StreamingXml",
                tag,
                component((
                    Document::from_xml(contract),
                    binding("stream", RuntimeDeclaration::BindingFactory(declaration)),
                )),
            )
        })())
    }

    /// Project one durable System POM contract and exactly one reusable XML
    /// factory.
    ///
    /// The runtime identity is consumed by this streaming leaf. It cannot be
    /// applied later to an aggregate component subtree. The durable component
    /// tree key defaults to the contract identity.
    pub fn into_durable_component(
        self,
        runtime_contract: RuntimeContract,
    ) -> DurableComponent<C, Props> {
        let contract = self.contract.clone();
        match self.into_factory_parts() {
            Ok((_, declaration)) => DurableComponent::binding(
                runtime_contract,
                pom_view(Document::from_xml(contract)),
                declaration,
            ),
            Err(error) => DurableComponent::from_error(error),
        }
    }

    /// Durable streaming leaf with an explicit component-tree key.
    ///
    /// New leaves derive their structural key from [`RuntimeContract`] by
    /// default. Use this only when a parent needs a distinct stable placement
    /// identity without changing the durable runtime declaration id.
    pub fn into_durable_component_with_key(
        self,
        key: impl Into<crate::StorageString>,
        runtime_contract: RuntimeContract,
    ) -> DurableComponent<C, Props> {
        self.into_durable_component(runtime_contract).key(key)
    }

    fn into_factory_parts(self) -> Result<(String, FactoryDeclaration<C, Props>), ComponentError> {
        let tag = streaming_contract_tag(&self.contract)?;
        let route = RuntimeRoute::xml(tag.clone())?;
        let initialize = self.initialize;
        let on_open = self.on_open;
        let on_stream = self.on_stream;
        let on_complete = self.on_complete;
        let on_finish = self.on_finish;
        let on_abort = self.on_abort;
        let binding_tag = tag.clone();
        let declaration = FactoryDeclaration::with_context(route, move |context| {
            initialize(context).map(|state| MountedStreamingBinding {
                tag: binding_tag.clone(),
                state,
                on_open: on_open.clone(),
                on_stream: on_stream.clone(),
                on_complete: on_complete.clone(),
                on_finish: on_finish.clone(),
                on_abort: on_abort.clone(),
            })
        });
        Ok((tag, declaration))
    }
}

struct MountedStreamingBinding<S, C>
where
    C: TurnChannels,
{
    tag: String,
    state: S,
    on_open: Option<ReusableReducer<S, C>>,
    on_stream: Option<ReusableReducer<S, C>>,
    on_complete: Option<ReusableReducer<S, C>>,
    on_finish: Option<ReusableFinishReducer<S, C>>,
    on_abort: Option<ReusableAbortReducer<S>>,
}

impl<S, C> fmt::Debug for MountedStreamingBinding<S, C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedStreamingBinding")
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

impl<S, C> BindingInstance<C> for MountedStreamingBinding<S, C>
where
    S: Send + 'static,
    C: TurnChannels,
{
    fn on_open(&mut self, element: &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> {
        match &self.on_open {
            Some(reducer) => reducer(&mut self.state, element),
            None => Ok(StreamUpdate::new()),
        }
    }

    fn on_stream(&mut self, element: &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> {
        match &self.on_stream {
            Some(reducer) => reducer(&mut self.state, element),
            None => Ok(StreamUpdate::new()),
        }
    }

    fn on_complete(&mut self, element: &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> {
        match &self.on_complete {
            Some(reducer) => reducer(&mut self.state, element),
            None => Ok(StreamUpdate::new()),
        }
    }

    fn on_finish(&mut self) -> Result<MountedUpdate<C>, BindingFailure> {
        match &self.on_finish {
            Some(reducer) => reducer(&mut self.state),
            None => Ok(StreamUpdate::new()),
        }
    }

    fn abort(&mut self, reason: BindingAbortReason) -> BindingAbortAck {
        match &self.on_abort {
            Some(reducer) => reducer(&mut self.state, reason),
            None => BindingAbortAck::NoCleanupNeeded,
        }
    }
}

fn streaming_contract_tag(contract: &XmlNode) -> Result<String, ComponentError> {
    if contract.name().as_str() != "tool" {
        return Ok(contract.name().to_string());
    }

    let name = XmlName::try_from("name").expect("name is a valid static XML name");
    let value =
        contract
            .attributes()
            .get(&name)
            .ok_or_else(|| ComponentError::InvalidBindingContract {
                message: "a <tool> streaming contract requires a name attribute".to_owned(),
            })?;
    XmlName::new(value.value())
        .map(|name| name.to_string())
        .map_err(|error| ComponentError::InvalidBindingContract {
            message: error.to_string(),
        })
}

trait ErasedStreamingHandler<E, D>: Send {
    fn on_open(&mut self, element: &XmlElement) -> StreamUpdate<E, D>;
    fn on_stream(&mut self, element: &XmlElement) -> StreamUpdate<E, D>;
    fn on_complete(&mut self, element: &XmlElement) -> StreamUpdate<E, D>;
    fn finish(&mut self) -> StreamUpdate<E, D>;
}

struct TypedStreamingHandler<S, E, D> {
    state: Option<S>,
    on_open: Option<Reducer<S, E, D>>,
    on_stream: Option<Reducer<S, E, D>>,
    on_complete: Option<Reducer<S, E, D>>,
    finish: Option<FinishReducer<S, E, D>>,
}

impl<S, E, D> TypedStreamingHandler<S, E, D> {
    fn reduce(
        state: &mut Option<S>,
        reducer: &Option<Reducer<S, E, D>>,
        element: &XmlElement,
    ) -> StreamUpdate<E, D> {
        match reducer {
            Some(reducer) => reducer(
                state
                    .as_mut()
                    .expect("streaming state is present until finish"),
                element,
            ),
            None => StreamUpdate::new(),
        }
    }
}

impl<S, E, D> ErasedStreamingHandler<E, D> for TypedStreamingHandler<S, E, D>
where
    S: Send + 'static,
    E: Send + 'static,
    D: Send + 'static,
{
    fn on_open(&mut self, element: &XmlElement) -> StreamUpdate<E, D> {
        Self::reduce(&mut self.state, &self.on_open, element)
    }

    fn on_stream(&mut self, element: &XmlElement) -> StreamUpdate<E, D> {
        Self::reduce(&mut self.state, &self.on_stream, element)
    }

    fn on_complete(&mut self, element: &XmlElement) -> StreamUpdate<E, D> {
        Self::reduce(&mut self.state, &self.on_complete, element)
    }

    fn finish(&mut self) -> StreamUpdate<E, D> {
        let state = self
            .state
            .take()
            .expect("a streaming binding is finished exactly once");
        match self.finish.take() {
            Some(finish) => finish(state),
            None => StreamUpdate::new(),
        }
    }
}

/// Type-erased stream state with typed effects and diagnostics.
pub struct StreamingBinding<E, D> {
    tag: String,
    handler: Box<dyn ErasedStreamingHandler<E, D>>,
}

impl<E, D> StreamingBinding<E, D> {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Lift a reusable component's local effect into its parent's effect type.
    pub(crate) fn map_effect<C, F>(self, map: F) -> StreamingBinding<C, D>
    where
        E: Send + 'static,
        C: Send + 'static,
        D: Send + 'static,
        F: Fn(E) -> C + Send + Sync + 'static,
    {
        self.map_effect_shared(Arc::new(map))
    }

    fn map_effect_shared<C, F>(self, map: Arc<F>) -> StreamingBinding<C, D>
    where
        E: Send + 'static,
        C: Send + 'static,
        D: Send + 'static,
        F: Fn(E) -> C + Send + Sync + 'static,
    {
        StreamingBinding {
            tag: self.tag,
            handler: Box::new(MappedEffectHandler {
                inner: self.handler,
                map,
                _output: std::marker::PhantomData,
            }),
        }
    }

    /// Lift this binding's local diagnostic into a parent diagnostic type.
    pub(crate) fn map_diagnostic<C, F>(self, map: F) -> StreamingBinding<E, C>
    where
        E: Send + 'static,
        C: Send + 'static,
        D: Send + 'static,
        F: Fn(D) -> C + Send + Sync + 'static,
    {
        self.map_diagnostic_shared(Arc::new(map))
    }

    fn map_diagnostic_shared<C, F>(self, map: Arc<F>) -> StreamingBinding<E, C>
    where
        E: Send + 'static,
        C: Send + 'static,
        D: Send + 'static,
        F: Fn(D) -> C + Send + Sync + 'static,
    {
        StreamingBinding {
            tag: self.tag,
            handler: Box::new(MappedDiagnosticHandler {
                inner: self.handler,
                map,
                _output: std::marker::PhantomData,
            }),
        }
    }
}

impl<E, D> fmt::Debug for StreamingBinding<E, D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingBinding")
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

impl<C> BindingInstance<C> for StreamingBinding<TurnEmission<C>, C::Diagnostic>
where
    C: TurnChannels,
{
    fn on_open(&mut self, element: &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> {
        Ok(self.handler.on_open(element))
    }

    fn on_stream(&mut self, element: &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> {
        Ok(self.handler.on_stream(element))
    }

    fn on_complete(&mut self, element: &XmlElement) -> Result<MountedUpdate<C>, BindingFailure> {
        Ok(self.handler.on_complete(element))
    }

    fn on_finish(&mut self) -> Result<MountedUpdate<C>, BindingFailure> {
        Ok(self.handler.finish())
    }
}

struct MappedEffectHandler<E, C, D, F> {
    inner: Box<dyn ErasedStreamingHandler<E, D>>,
    map: Arc<F>,
    _output: std::marker::PhantomData<fn() -> C>,
}

impl<E, C, D, F> MappedEffectHandler<E, C, D, F>
where
    F: Fn(E) -> C,
{
    fn map_update(&self, update: StreamUpdate<E, D>) -> StreamUpdate<C, D> {
        update.map_emissions(|effect| (self.map)(effect))
    }
}

impl<E, C, D, F> ErasedStreamingHandler<C, D> for MappedEffectHandler<E, C, D, F>
where
    E: Send + 'static,
    C: Send + 'static,
    D: Send + 'static,
    F: Fn(E) -> C + Send + Sync + 'static,
{
    fn on_open(&mut self, element: &XmlElement) -> StreamUpdate<C, D> {
        let update = self.inner.on_open(element);
        self.map_update(update)
    }

    fn on_stream(&mut self, element: &XmlElement) -> StreamUpdate<C, D> {
        let update = self.inner.on_stream(element);
        self.map_update(update)
    }

    fn on_complete(&mut self, element: &XmlElement) -> StreamUpdate<C, D> {
        let update = self.inner.on_complete(element);
        self.map_update(update)
    }

    fn finish(&mut self) -> StreamUpdate<C, D> {
        let update = self.inner.finish();
        self.map_update(update)
    }
}

struct MappedDiagnosticHandler<E, D, C, F> {
    inner: Box<dyn ErasedStreamingHandler<E, D>>,
    map: Arc<F>,
    _output: std::marker::PhantomData<fn() -> C>,
}

impl<E, D, C, F> MappedDiagnosticHandler<E, D, C, F>
where
    F: Fn(D) -> C,
{
    fn map_update(&self, update: StreamUpdate<E, D>) -> StreamUpdate<E, C> {
        update.map_diagnostics(|diagnostic| (self.map)(diagnostic))
    }
}

impl<E, D, C, F> ErasedStreamingHandler<E, C> for MappedDiagnosticHandler<E, D, C, F>
where
    E: Send + 'static,
    D: Send + 'static,
    C: Send + 'static,
    F: Fn(D) -> C + Send + Sync + 'static,
{
    fn on_open(&mut self, element: &XmlElement) -> StreamUpdate<E, C> {
        let update = self.inner.on_open(element);
        self.map_update(update)
    }

    fn on_stream(&mut self, element: &XmlElement) -> StreamUpdate<E, C> {
        let update = self.inner.on_stream(element);
        self.map_update(update)
    }

    fn on_complete(&mut self, element: &XmlElement) -> StreamUpdate<E, C> {
        let update = self.inner.on_complete(element);
        self.map_update(update)
    }

    fn finish(&mut self) -> StreamUpdate<E, C> {
        let update = self.inner.finish();
        self.map_update(update)
    }
}

/// Complete mapping from one component-local channel contract to its parent.
///
/// Unlike the single-lane helpers, this mapping preserves whether each local
/// value is output, live, or commit. It also normalizes diagnostics in the same
/// operation, so a multi-lane component cannot be partially lifted.
pub trait ChannelMap<Local, Root>: Send + Sync + 'static
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    fn output(&self, output: Local::Output) -> Root::Output;

    fn live(&self, live: Local::Live) -> Root::Live;

    fn commit(&self, commit: Local::Commit) -> Root::Commit;

    fn diagnostic(&self, diagnostic: Local::Diagnostic) -> Root::Diagnostic;
}

fn map_channel_emission<Local, Root, M>(
    map: &M,
    emission: TurnEmission<Local>,
) -> TurnEmission<Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
    M: ChannelMap<Local, Root> + ?Sized,
{
    match emission {
        TurnEmission::Output(output) => TurnEmission::Output(map.output(output)),
        TurnEmission::Live(live) => TurnEmission::Live(map.live(live)),
        TurnEmission::Commit(commit) => TurnEmission::Commit(map.commit(commit)),
    }
}

/// Closure-backed [`ChannelMap`] for ordinary component authoring.
pub struct TurnChannelMap<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    output: Arc<dyn Fn(Local::Output) -> Root::Output + Send + Sync>,
    live: Arc<dyn Fn(Local::Live) -> Root::Live + Send + Sync>,
    commit: Arc<dyn Fn(Local::Commit) -> Root::Commit + Send + Sync>,
    diagnostic: Arc<dyn Fn(Local::Diagnostic) -> Root::Diagnostic + Send + Sync>,
}

/// Type-state marker for an unconfigured [`TurnChannelMap`] lane.
///
/// It is public only because it appears in [`TurnChannelMapBuilder`]'s return
/// type. Authors never need to name it.
#[doc(hidden)]
pub struct MissingChannelMapLane;

/// Named, type-state builder for a complete [`TurnChannelMap`].
///
/// A local component maps all four lanes together. Naming every lane avoids
/// accidentally swapping same-shaped closures such as `live` and `commit`.
/// [`TurnChannelMapBuilder::build`] exists only after all four lanes are set.
#[must_use]
pub struct TurnChannelMapBuilder<
    Local,
    Root,
    Output = MissingChannelMapLane,
    Live = MissingChannelMapLane,
    Commit = MissingChannelMapLane,
    Diagnostic = MissingChannelMapLane,
> where
    Local: TurnChannels,
    Root: TurnChannels,
{
    output: Output,
    live: Live,
    commit: Commit,
    diagnostic: Diagnostic,
    _channels: PhantomData<fn() -> (Local, Root)>,
}

impl<Local, Root> TurnChannelMap<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    /// Start a named mapping for every local channel lane.
    pub fn builder() -> TurnChannelMapBuilder<Local, Root> {
        TurnChannelMapBuilder {
            output: MissingChannelMapLane,
            live: MissingChannelMapLane,
            commit: MissingChannelMapLane,
            diagnostic: MissingChannelMapLane,
            _channels: PhantomData,
        }
    }

    /// Construct a complete map from positional lanes.
    ///
    /// Prefer [`Self::builder`] in new authoring code so the lane identities
    /// remain visible at the call site.
    pub fn new(
        output: impl Fn(Local::Output) -> Root::Output + Send + Sync + 'static,
        live: impl Fn(Local::Live) -> Root::Live + Send + Sync + 'static,
        commit: impl Fn(Local::Commit) -> Root::Commit + Send + Sync + 'static,
        diagnostic: impl Fn(Local::Diagnostic) -> Root::Diagnostic + Send + Sync + 'static,
    ) -> Self {
        Self {
            output: Arc::new(output),
            live: Arc::new(live),
            commit: Arc::new(commit),
            diagnostic: Arc::new(diagnostic),
        }
    }
}

impl<Local, Root, Live, Commit, Diagnostic>
    TurnChannelMapBuilder<Local, Root, MissingChannelMapLane, Live, Commit, Diagnostic>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    /// Map the component's final output lane.
    pub fn output<Output>(
        self,
        output: Output,
    ) -> TurnChannelMapBuilder<Local, Root, Output, Live, Commit, Diagnostic>
    where
        Output: Fn(Local::Output) -> Root::Output + Send + Sync + 'static,
    {
        TurnChannelMapBuilder {
            output,
            live: self.live,
            commit: self.commit,
            diagnostic: self.diagnostic,
            _channels: PhantomData,
        }
    }
}

impl<Local, Root, Output, Commit, Diagnostic>
    TurnChannelMapBuilder<Local, Root, Output, MissingChannelMapLane, Commit, Diagnostic>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    /// Map the component's real-time effect lane.
    pub fn live<Live>(
        self,
        live: Live,
    ) -> TurnChannelMapBuilder<Local, Root, Output, Live, Commit, Diagnostic>
    where
        Live: Fn(Local::Live) -> Root::Live + Send + Sync + 'static,
    {
        TurnChannelMapBuilder {
            output: self.output,
            live,
            commit: self.commit,
            diagnostic: self.diagnostic,
            _channels: PhantomData,
        }
    }
}

impl<Local, Root, Output, Live, Diagnostic>
    TurnChannelMapBuilder<Local, Root, Output, Live, MissingChannelMapLane, Diagnostic>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    /// Map the component's durable commit lane.
    pub fn commit<Commit>(
        self,
        commit: Commit,
    ) -> TurnChannelMapBuilder<Local, Root, Output, Live, Commit, Diagnostic>
    where
        Commit: Fn(Local::Commit) -> Root::Commit + Send + Sync + 'static,
    {
        TurnChannelMapBuilder {
            output: self.output,
            live: self.live,
            commit,
            diagnostic: self.diagnostic,
            _channels: PhantomData,
        }
    }
}

impl<Local, Root, Output, Live, Commit>
    TurnChannelMapBuilder<Local, Root, Output, Live, Commit, MissingChannelMapLane>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    /// Map the component's non-terminal diagnostic lane.
    pub fn diagnostic<Diagnostic>(
        self,
        diagnostic: Diagnostic,
    ) -> TurnChannelMapBuilder<Local, Root, Output, Live, Commit, Diagnostic>
    where
        Diagnostic: Fn(Local::Diagnostic) -> Root::Diagnostic + Send + Sync + 'static,
    {
        TurnChannelMapBuilder {
            output: self.output,
            live: self.live,
            commit: self.commit,
            diagnostic,
            _channels: PhantomData,
        }
    }
}

impl<Local, Root, Output, Live, Commit, Diagnostic>
    TurnChannelMapBuilder<Local, Root, Output, Live, Commit, Diagnostic>
where
    Local: TurnChannels,
    Root: TurnChannels,
    Output: Fn(Local::Output) -> Root::Output + Send + Sync + 'static,
    Live: Fn(Local::Live) -> Root::Live + Send + Sync + 'static,
    Commit: Fn(Local::Commit) -> Root::Commit + Send + Sync + 'static,
    Diagnostic: Fn(Local::Diagnostic) -> Root::Diagnostic + Send + Sync + 'static,
{
    /// Finish a complete named lane mapping.
    pub fn build(self) -> TurnChannelMap<Local, Root> {
        TurnChannelMap::new(self.output, self.live, self.commit, self.diagnostic)
    }
}

impl<Local, Root> ChannelMap<Local, Root> for TurnChannelMap<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    fn output(&self, output: Local::Output) -> Root::Output {
        (self.output)(output)
    }

    fn live(&self, live: Local::Live) -> Root::Live {
        (self.live)(live)
    }

    fn commit(&self, commit: Local::Commit) -> Root::Commit {
        (self.commit)(commit)
    }

    fn diagnostic(&self, diagnostic: Local::Diagnostic) -> Root::Diagnostic {
        (self.diagnostic)(diagnostic)
    }
}

/// A streaming component whose emissions and diagnostics form one complete
/// channel contract.
///
/// This nominal wrapper prevents a multi-lane component from accidentally
/// using a single-lane mapping shortcut. It composes like any other component
/// view and can be compiled directly after it is normalized to root channels.
pub struct StreamingChannelsView<C>
where
    C: TurnChannels,
{
    view: View<StreamingBinding<TurnEmission<C>, C::Diagnostic>>,
}

impl<C> StreamingChannelsView<C>
where
    C: TurnChannels,
{
    pub(crate) fn from_view(view: View<StreamingBinding<TurnEmission<C>, C::Diagnostic>>) -> Self {
        Self { view }
    }

    /// Attach a stable parent-provided key to this component invocation.
    pub fn key(self, key: impl Into<crate::StorageString>) -> Self {
        Self::from_view(self.view.key(key))
    }

    pub(crate) fn deferred(
        name: &'static str,
        render: impl FnOnce() -> Self + Send + 'static,
    ) -> Self {
        Self::from_view(Ok(super::ComponentNode::deferred_component(
            name,
            move || render().view,
        )))
    }
}

impl<C> IntoComponentNode<StreamingBinding<TurnEmission<C>, C::Diagnostic>>
    for StreamingChannelsView<C>
where
    C: TurnChannels,
{
    fn into_component_node(self) -> View<StreamingBinding<TurnEmission<C>, C::Diagnostic>> {
        self.view
    }
}

/// Mapping operation for atomically lifting every lane of a local stream.
pub trait StreamingChannelsViewExt<Local>
where
    Local: TurnChannels,
{
    fn map_channels<Root>(self, map: impl ChannelMap<Local, Root>) -> StreamingChannelsView<Root>
    where
        Root: TurnChannels;
}

impl<Local> StreamingChannelsViewExt<Local> for StreamingChannelsView<Local>
where
    Local: TurnChannels,
{
    fn map_channels<Root>(self, map: impl ChannelMap<Local, Root>) -> StreamingChannelsView<Root>
    where
        Root: TurnChannels,
    {
        let map = Arc::new(map);
        let view = self.view.map_binding(move |binding| {
            let emission_map = Arc::clone(&map);
            let binding = binding
                .map_effect(move |emission| map_channel_emission(emission_map.as_ref(), emission));
            let diagnostic_map = Arc::clone(&map);
            binding.map_diagnostic(move |diagnostic| diagnostic_map.diagnostic(diagnostic))
        });
        StreamingChannelsView::from_view(view)
    }
}

/// Binding-bearing streaming view normalized to one harness channel contract.
pub type StreamingProvidedView<C> = StreamingChannelsView<C>;

/// Intermediate streaming view whose value lane is mapped but whose local
/// diagnostic still needs to be lifted to the harness diagnostic type.
pub struct StreamingChannelView<C, D>
where
    C: TurnChannels,
{
    view: View<StreamingBinding<TurnEmission<C>, D>>,
}

impl<C, D> StreamingChannelView<C, D>
where
    C: TurnChannels,
    TurnEmission<C>: Send + 'static,
    C::Diagnostic: Send + 'static,
    D: Send + 'static,
{
    pub fn map_diagnostic(
        self,
        map: impl Fn(D) -> C::Diagnostic + Send + Sync + 'static,
    ) -> StreamingProvidedView<C> {
        let map = Arc::new(map);
        StreamingChannelsView::from_view(
            self.view
                .map_binding(move |binding| binding.map_diagnostic_shared(Arc::clone(&map))),
        )
    }
}

impl<C> StreamingChannelView<C, <C as TurnChannels>::Diagnostic>
where
    C: TurnChannels,
{
    pub fn into_view(self) -> StreamingProvidedView<C> {
        StreamingChannelsView::from_view(self.view)
    }
}

/// A streaming component with one unclassified value lane and one diagnostic
/// lane.
///
/// Lane assignment is available only on this nominal type. Converting it into
/// a general component tree consumes that capability, so wrapping a complete
/// multi-lane view in [`super::view`] cannot recover the single-lane helpers.
pub struct StreamingValueView<E, D> {
    view: View<StreamingBinding<E, D>>,
}

impl<E, D> StreamingValueView<E, D>
where
    E: 'static,
    D: 'static,
{
    pub(crate) fn from_view(view: View<StreamingBinding<E, D>>) -> Self {
        Self { view }
    }

    pub(crate) fn deferred(
        name: &'static str,
        render: impl FnOnce() -> Self + Send + 'static,
    ) -> Self {
        Self::from_view(Ok(super::ComponentNode::deferred_component(
            name,
            move || render().view,
        )))
    }

    /// Attach a stable parent-provided key to this component invocation.
    pub fn key(self, key: impl Into<crate::StorageString>) -> Self {
        Self::from_view(self.view.key(key))
    }

    /// Lift a reusable component's local value without assigning a phase.
    pub fn map_effect<C>(
        self,
        map: impl Fn(E) -> C + Send + Sync + 'static,
    ) -> StreamingValueView<C, D>
    where
        E: Send + 'static,
        C: Send + 'static,
        D: Send + 'static,
    {
        let map = Arc::new(map);
        StreamingValueView::from_view(
            self.view
                .map_binding(move |binding| binding.map_effect_shared(Arc::clone(&map))),
        )
    }

    pub fn map_output<C>(
        self,
        map: impl Fn(E) -> C::Output + Send + Sync + 'static,
    ) -> StreamingChannelView<C, D>
    where
        E: Send + 'static,
        D: Send + 'static,
        C: TurnChannels,
        TurnEmission<C>: Send + 'static,
    {
        map_streaming_channel(self, move |effect| TurnEmission::Output(map(effect)))
    }

    pub fn map_live<C>(
        self,
        map: impl Fn(E) -> C::Live + Send + Sync + 'static,
    ) -> StreamingChannelView<C, D>
    where
        E: Send + 'static,
        D: Send + 'static,
        C: TurnChannels,
        TurnEmission<C>: Send + 'static,
    {
        map_streaming_channel(self, move |effect| TurnEmission::Live(map(effect)))
    }

    pub fn map_commit<C>(
        self,
        map: impl Fn(E) -> C::Commit + Send + Sync + 'static,
    ) -> StreamingChannelView<C, D>
    where
        E: Send + 'static,
        D: Send + 'static,
        C: TurnChannels,
    {
        map_streaming_channel(self, move |effect| TurnEmission::Commit(map(effect)))
    }
}

impl<E, D> IntoComponentNode<StreamingBinding<E, D>> for StreamingValueView<E, D> {
    fn into_component_node(self) -> View<StreamingBinding<E, D>> {
        self.view
    }
}

fn map_streaming_channel<E, D, C>(
    view: StreamingValueView<E, D>,
    map: impl Fn(E) -> TurnEmission<C> + Send + Sync + 'static,
) -> StreamingChannelView<C, D>
where
    E: Send + 'static,
    D: Send + 'static,
    C: TurnChannels,
    TurnEmission<C>: Send + 'static,
{
    let map = Arc::new(map);
    StreamingChannelView {
        view: view
            .view
            .map_binding(move |binding| binding.map_effect_shared(Arc::clone(&map))),
    }
}

type SharedMountedBinding<C> = Arc<TokioMutex<Box<dyn BindingInstance<C>>>>;

struct MountedAttemptBinding<C>
where
    C: TurnChannels,
{
    id: BindingId,
    route: RuntimeRoute,
    binding: SharedMountedBinding<C>,
}

/// Terminal failure while reducing or finalizing one mounted attempt.
#[derive(Debug, thiserror::Error)]
pub enum StreamingAttemptError {
    #[error(transparent)]
    Binding(#[from] BindingFault),

    #[error(transparent)]
    Live(#[from] LiveEffectFault),

    #[error(transparent)]
    Provider(#[from] ProviderToolAttemptError),

    #[error(
        "provider TextComplete diverged from the {delta_bytes} bytes already received as deltas \
         (completion has {completion_bytes} bytes)"
    )]
    TextCompletionMismatch {
        delta_bytes: usize,
        completion_bytes: usize,
    },

    #[error("provider emitted text after TextComplete")]
    TextAfterCompletion,

    #[error("mounted streaming attempt is terminal after a prior failure")]
    Terminal,
}

/// Failure while atomically creating XML reducers and native dispatchers.
#[derive(Debug, thiserror::Error)]
pub enum StreamingAttemptStartError {
    #[error(transparent)]
    Binding(#[from] BindingFault),

    #[error(transparent)]
    Provider(#[from] ProviderDispatchFault),
}

impl StreamingAttemptStartError {
    pub fn binding_fault(&self) -> Option<&BindingFault> {
        match self {
            Self::Binding(fault) => Some(fault),
            Self::Provider(_) => None,
        }
    }

    pub fn provider_fault(&self) -> Option<&ProviderDispatchFault> {
        match self {
            Self::Binding(_) => None,
            Self::Provider(fault) => Some(fault),
        }
    }
}

#[derive(Debug)]
enum MountedCallbackFailure {
    Binding(BindingFailure),
    Live(LiveEffectFault),
}

impl From<BindingFailure> for MountedCallbackFailure {
    fn from(failure: BindingFailure) -> Self {
        Self::Binding(failure)
    }
}

impl From<LiveEffectFault> for MountedCallbackFailure {
    fn from(fault: LiveEffectFault) -> Self {
        Self::Live(fault)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("stream ended with incomplete registered XML tag <{tag}>")]
struct IncompleteRegisteredXml {
    tag: String,
}

/// One binding's acknowledgement during an explicit attempt abort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingBindingAbort {
    id: BindingId,
    route: RuntimeRoute,
    acknowledgement: BindingAbortAck,
}

impl StreamingBindingAbort {
    pub fn id(&self) -> &BindingId {
        &self.id
    }

    pub fn route(&self) -> &RuntimeRoute {
        &self.route
    }

    pub fn acknowledgement(&self) -> BindingAbortAck {
        self.acknowledgement
    }
}

/// Combined host compensation and pure reducer teardown report for an aborted
/// provider attempt. Local teardown always runs even if live compensation
/// fails; callers must inspect [`StreamingAbortReport::live`].
#[derive(Debug)]
pub struct StreamingAbortReport {
    identity: ProviderAttemptIdentity,
    reason: BindingAbortReason,
    bindings: Vec<StreamingBindingAbort>,
    dispatchers: Vec<ProviderDispatcherAbort>,
    live: LiveAbortOutcome,
}

impl StreamingAbortReport {
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn reason(&self) -> BindingAbortReason {
        self.reason
    }

    pub fn bindings(&self) -> &[StreamingBindingAbort] {
        &self.bindings
    }

    pub fn dispatchers(&self) -> &[ProviderDispatcherAbort] {
        &self.dispatchers
    }

    pub fn live(&self) -> &LiveAbortOutcome {
        &self.live
    }

    /// Whether every fallible host cleanup boundary acknowledged the abort.
    pub fn cleanup_acknowledged(&self) -> bool {
        self.live.acknowledgement().is_some()
            && self.dispatchers.iter().all(|dispatcher| {
                matches!(
                    dispatcher.outcome(),
                    ProviderDispatcherAbortOutcome::Acknowledged(_)
                )
            })
    }
}

/// Fresh parser and reducer state for exactly one provider attempt.
///
/// All XML routes share this parser. Therefore callbacks run in the order tags
/// appear on the wire, even when factory declaration order differs. Every Live
/// value is interpreted and awaited inside its callback before parsing
/// continues. Commit values stay private until host publication succeeds;
/// event updates contain only Output values and diagnostics.
#[must_use = "an active attempt must be finished or explicitly aborted"]
pub struct MountedStreamingAttempt<C>
where
    C: TurnChannels,
{
    identity: ProviderAttemptIdentity,
    parser: HermesParser<MountedCallbackFailure>,
    bindings: Vec<MountedAttemptBinding<C>>,
    provider_runtime: ProviderDispatchRuntime<C>,
    live_runtime: SharedLiveEffectRuntime<C::Live>,
    pending_commits: SharedCommitBuffer<C::Commit>,
    accumulator: Arc<TokioMutex<StreamAccumulator<TurnEmission<C>, C::Diagnostic>>>,
    raw_output: String,
    provider_results: Vec<ProviderToolResult>,
    text_complete: bool,
    terminal: bool,
}

impl<C> fmt::Debug for MountedStreamingAttempt<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedStreamingAttempt")
            .field("identity", &self.identity)
            .field("bindings", &self.bindings.len())
            .field("dispatchers", &self.provider_runtime.dispatcher_count())
            .field("raw_output", &self.raw_output)
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl<C> MountedStreamingAttempt<C>
where
    C: TurnChannels,
{
    fn new<Props, R>(
        plan: &BindingFactoryPlan<C, Props>,
        provider_plan: &ProviderCapabilityPlan<C, Props>,
        props: &Props,
        identity: ProviderAttemptIdentity,
        live_runtime: R,
    ) -> Result<Self, StreamingAttemptStartError>
    where
        Props: ?Sized + 'static,
        R: LiveEffectRuntime<C::Live>,
    {
        let mut created = Vec::new();
        for factory in plan
            .factories()
            .iter()
            .filter(|factory| factory.route().namespace() == "xml")
        {
            match factory.instantiate(AttemptBindingCx {
                props,
                identity: &identity,
            }) {
                Ok(binding) => {
                    created.push((factory.id().clone(), factory.route().clone(), binding))
                }
                Err(error) => {
                    for (_, _, binding) in &mut created {
                        binding.abort(BindingAbortReason::InitializationFailure);
                    }
                    return Err(error.into());
                }
            }
        }

        let provider_runtime =
            match ProviderDispatchRuntime::new(provider_plan, props, identity.clone()) {
                Ok(runtime) => runtime,
                Err(error) => {
                    for (_, _, binding) in &mut created {
                        binding.abort(BindingAbortReason::InitializationFailure);
                    }
                    return Err(error.into());
                }
            };

        let mut parser = HermesParser::<MountedCallbackFailure>::new();
        let accumulator = Arc::new(TokioMutex::new(StreamAccumulator::default()));
        let live_runtime = Arc::new(TokioMutex::new(LiveEffectDispatcher::new(live_runtime)));
        let pending_commits = Arc::new(TokioMutex::new(Vec::new()));
        let mut bindings = Vec::with_capacity(created.len());
        for (id, route, binding) in created {
            let binding: SharedMountedBinding<C> = Arc::new(TokioMutex::new(binding));
            register_mounted_callbacks(
                &mut parser,
                route.name().to_owned(),
                Arc::clone(&binding),
                Arc::clone(&accumulator),
                Arc::clone(&live_runtime),
                Arc::clone(&pending_commits),
                BindingOrigin::new(&identity, &id, &route),
            );
            bindings.push(MountedAttemptBinding { id, route, binding });
        }

        Ok(Self {
            identity,
            parser,
            bindings,
            provider_runtime,
            live_runtime,
            pending_commits,
            accumulator,
            raw_output: String::new(),
            provider_results: Vec::new(),
            text_complete: false,
            terminal: false,
        })
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }

    pub fn dispatcher_count(&self) -> usize {
        self.provider_runtime.dispatcher_count()
    }

    pub fn provider_tool_count(&self) -> usize {
        self.provider_runtime.tool_count()
    }

    pub fn raw_output(&self) -> &str {
        &self.raw_output
    }

    pub fn provider_results(&self) -> &[ProviderToolResult] {
        &self.provider_results
    }

    /// Await one provider-native tool under the same identity and effect scope
    /// as the XML stream. Expected tool failures are model-visible responses;
    /// terminal runtime failures stop both dispatch and parsing.
    pub async fn call_tool(
        &mut self,
        call: ProviderToolCall,
    ) -> Result<ProviderToolCallOutcome<C>, ProviderToolAttemptError> {
        if self.terminal {
            return Err(ProviderToolAttemptError::Terminal);
        }
        match self
            .provider_runtime
            .call_tool(
                call,
                Arc::clone(&self.live_runtime),
                Arc::clone(&self.pending_commits),
            )
            .await
        {
            Ok(outcome) => {
                if !outcome.replayed() {
                    self.provider_results.push(outcome.result().clone());
                }
                Ok(outcome)
            }
            Err(error) => {
                self.terminal = true;
                Err(error)
            }
        }
    }

    /// Parse one provider event after awaiting its Live effects.
    ///
    /// Returned emissions contain only the Output lane. Live values have
    /// already been interpreted, while Commit values remain publication-gated.
    pub async fn on_event(
        &mut self,
        event: TextTurnEvent,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, StreamingAttemptError> {
        if self.terminal {
            return Err(StreamingAttemptError::Terminal);
        }
        let parser_result = match event {
            TextTurnEvent::TextDelta(chunk) => {
                if self.text_complete {
                    self.terminal = true;
                    return Err(StreamingAttemptError::TextAfterCompletion);
                }
                self.raw_output.push_str(&chunk);
                self.parser.try_feed(&chunk).await
            }
            TextTurnEvent::TextComplete(output) => {
                if self.text_complete {
                    self.terminal = true;
                    return Err(StreamingAttemptError::TextAfterCompletion);
                }
                self.text_complete = true;

                let delta_bytes = self.raw_output.len();
                let completion_bytes = output.len();
                if !output.starts_with(&self.raw_output) {
                    self.raw_output = output;
                    self.terminal = true;
                    return Err(StreamingAttemptError::TextCompletionMismatch {
                        delta_bytes,
                        completion_bytes,
                    });
                }

                let suffix = output[delta_bytes..].to_owned();
                self.raw_output = output;
                if suffix.is_empty() {
                    Ok(())
                } else {
                    self.parser.try_feed(&suffix).await
                }
            }
        };
        if let Err(error) = parser_result {
            self.terminal = true;
            return Err(self.map_parser_error(error));
        }
        Ok(self.take_update().await)
    }

    /// Strictly finalize parser input and transition to pending publication.
    pub async fn finish_stream(
        mut self,
    ) -> Result<FinishedStreamingAttempt<C>, StreamingFinishFailure<C>> {
        if self.terminal {
            return Err(StreamingFinishFailure {
                attempt: self,
                error: StreamingAttemptError::Terminal,
            });
        }

        if let Err(error) = self.parser.try_finalize_strict().await {
            self.terminal = true;
            let error = self.map_parser_error(error);
            return Err(StreamingFinishFailure {
                attempt: self,
                error,
            });
        }

        for index in 0..self.bindings.len() {
            let result = {
                let mut binding = self.bindings[index].binding.lock().await;
                binding.on_finish()
            };
            let update = match result {
                Ok(update) => update,
                Err(source) => {
                    self.terminal = true;
                    let id = self.bindings[index].id.clone();
                    let route = self.bindings[index].route.clone();
                    let error = BindingFault::new(
                        &self.identity,
                        &id,
                        &route,
                        super::BindingPhase::Finish,
                        source,
                    );
                    return Err(StreamingFinishFailure {
                        attempt: self,
                        error: error.into(),
                    });
                }
            };
            let origin = BindingOrigin::new(
                &self.identity,
                &self.bindings[index].id,
                &self.bindings[index].route,
            );
            let update = match interpret_live_update(
                update,
                Arc::clone(&self.live_runtime),
                Arc::clone(&self.pending_commits),
                origin,
                BindingPhase::Finish,
            )
            .await
            {
                Ok(update) => update,
                Err(error) => {
                    self.terminal = true;
                    return Err(StreamingFinishFailure {
                        attempt: self,
                        error: error.into(),
                    });
                }
            };
            self.accumulator.lock().await.record(update);
        }

        let provider_update = match self
            .provider_runtime
            .finish(
                Arc::clone(&self.live_runtime),
                Arc::clone(&self.pending_commits),
            )
            .await
        {
            Ok(update) => update,
            Err(error) => {
                self.terminal = true;
                return Err(StreamingFinishFailure {
                    attempt: self,
                    error: StreamingAttemptError::Provider(error),
                });
            }
        };
        self.accumulator.lock().await.record(provider_update);

        let update = self.take_update().await;
        Ok(FinishedStreamingAttempt {
            attempt: self,
            update,
        })
    }

    /// Drop parser input without finalization and acknowledge local reducer
    /// teardown for every installed binding.
    pub async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        abort_mounted_bindings(
            self.identity,
            self.bindings,
            self.provider_runtime,
            self.live_runtime,
            reason,
        )
        .await
    }

    async fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        let mut accumulator = self.accumulator.lock().await;
        StreamUpdate {
            emissions: std::mem::take(&mut accumulator.effects),
            diagnostics: std::mem::take(&mut accumulator.diagnostics),
        }
    }

    fn map_parser_error(
        &self,
        error: HermesParserError<MountedCallbackFailure>,
    ) -> StreamingAttemptError {
        match error {
            HermesParserError::Callback { tag, phase, source } => {
                if let MountedCallbackFailure::Live(fault) = source {
                    return fault.into();
                }
                let MountedCallbackFailure::Binding(source) = source else {
                    unreachable!("live callback failures return above")
                };
                let mounted = self
                    .binding_for_tag(&tag)
                    .expect("parser callbacks are registered from mounted XML bindings");
                BindingFault::new(
                    &self.identity,
                    &mounted.id,
                    &mounted.route,
                    match phase {
                        HermesCallbackPhase::Open => super::BindingPhase::Open,
                        HermesCallbackPhase::Stream => super::BindingPhase::Stream,
                        HermesCallbackPhase::Complete => super::BindingPhase::Complete,
                    },
                    source,
                )
                .into()
            }
            HermesParserError::IncompleteRegisteredTag { tag } => {
                let mounted = self
                    .binding_for_tag(&tag)
                    .expect("strict registered tags come from mounted XML bindings");
                BindingFault::new(
                    &self.identity,
                    &mounted.id,
                    &mounted.route,
                    super::BindingPhase::Finalize,
                    BindingFailure::new(IncompleteRegisteredXml { tag }),
                )
                .into()
            }
            HermesParserError::Terminal => StreamingAttemptError::Terminal,
        }
    }

    fn binding_for_tag(&self, tag: &str) -> Option<&MountedAttemptBinding<C>> {
        self.bindings
            .iter()
            .find(|mounted| mounted.route.namespace() == "xml" && mounted.route.name() == tag)
    }
}

/// Attempt whose parser and finish reducers succeeded but whose session is not
/// yet known to be published.
#[must_use = "a finished attempt must be published or explicitly aborted"]
pub struct FinishedStreamingAttempt<C>
where
    C: TurnChannels,
{
    attempt: MountedStreamingAttempt<C>,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
}

impl<C> fmt::Debug for FinishedStreamingAttempt<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FinishedStreamingAttempt")
            .field("identity", &self.attempt.identity)
            .field("bindings", &self.attempt.bindings.len())
            .finish_non_exhaustive()
    }
}

impl<C> FinishedStreamingAttempt<C>
where
    C: TurnChannels,
{
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub fn raw_output(&self) -> &str {
        self.attempt.raw_output()
    }

    pub fn provider_results(&self) -> &[ProviderToolResult] {
        self.attempt.provider_results()
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        std::mem::take(&mut self.update)
    }

    /// Encode every private Commit into a stable durable outbox candidate before
    /// session publication.
    ///
    /// Staging is synchronous and performs no I/O. The fingerprint factory is
    /// called only after Commit staging and sees the exact ordered outbox that
    /// will be published. A failure in either step retains this exact finished
    /// attempt for retry or acknowledged abort.
    pub fn stage_durable_publication<S, F, Mutation, Version>(
        self,
        request_id: PublicationRequestId,
        expected_revision: Version,
        mutation: PreparedSessionMutation<Mutation>,
        stager: &S,
        fingerprint_factory: &F,
    ) -> StreamingPublicationStagingResult<C, Mutation, S::Payload, Version, S::Error, F::Error>
    where
        S: CommitStager<C>,
        F: PublicationFingerprintFactory<Mutation, S::Payload, Version>,
    {
        let plan = PublicationStagingPlan::new(request_id, expected_revision, mutation);
        let outbox = {
            let commits = self
                .attempt
                .pending_commits
                .try_lock()
                .expect("a finished attempt has no active Commit producer");
            super::stage_commit_outbox::<C, S>(plan.request_id().clone(), &commits, stager)
        };
        let outbox = match outbox {
            Ok(outbox) => outbox,
            Err(source) => {
                return Err(StreamingPublicationStagingFailure::new(
                    self,
                    plan,
                    PublicationStagingFailure::Commit(source),
                ));
            }
        };
        let fingerprint = fingerprint_factory.fingerprint(PublicationFingerprintContext::new(
            plan.request_id(),
            plan.expected_revision(),
            plan.mutation(),
            self.raw_output(),
            self.provider_results(),
            &outbox,
        ));
        let fingerprint = match fingerprint {
            Ok(fingerprint) => fingerprint,
            Err(source) => {
                return Err(StreamingPublicationStagingFailure::new(
                    self,
                    plan,
                    PublicationStagingFailure::Fingerprint(source),
                ));
            }
        };
        let (request_id, expected_revision, mutation) = plan.into_parts();
        let candidate = StagedPublication::new(
            request_id,
            fingerprint,
            expected_revision,
            mutation,
            self.identity().clone(),
            self.raw_output(),
            self.provider_results().to_vec(),
            outbox,
        )
        .expect("outbox was staged with the same publication request id");
        Ok(StagedStreamingPublication::new(self, candidate))
    }

    /// Await the host's real publication point before releasing pending commit
    /// values into a published typestate.
    pub async fn publish_with<P>(
        self,
        publisher: &mut P,
    ) -> Result<PublishedStreamingAttempt<C>, StreamingPublishFailure<C, P::Error>>
    where
        P: TurnPublisher,
    {
        let publication =
            TurnPublication::combined(self.identity(), self.raw_output(), self.provider_results());
        if let Err(source) = publisher.publish(publication).await {
            return Err(StreamingPublishFailure {
                attempt: self,
                source,
            });
        }
        Ok(self.after_publish().await)
    }

    pub(crate) async fn after_publish(self) -> PublishedStreamingAttempt<C> {
        let Self { attempt, update } = self;
        let commits = {
            let mut pending = attempt.pending_commits.lock().await;
            std::mem::take(&mut *pending)
        };
        let receipt = PublishedTurnReceipt {
            identity: attempt.identity.clone(),
        };
        PublishedStreamingAttempt {
            receipt,
            raw_output: attempt.raw_output,
            provider_results: attempt.provider_results,
            update,
            commits,
        }
    }

    pub(crate) async fn into_durable_parts(
        self,
    ) -> (
        String,
        Vec<ProviderToolResult>,
        StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) {
        let Self {
            mut attempt,
            update,
        } = self;
        let commits = {
            let mut pending = attempt.pending_commits.lock().await;
            std::mem::take(&mut *pending)
        };
        drop(commits);
        let raw_output = std::mem::take(&mut attempt.raw_output);
        let provider_results = std::mem::take(&mut attempt.provider_results);
        (raw_output, provider_results, update)
    }

    /// Abort after parser finish, including when session publication failed.
    pub async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }
}

/// Successfully published provider attempt whose commit values are now safe
/// for a post-publication interpreter to consume.
#[must_use = "published commit values must be delivered or deliberately discarded"]
pub struct PublishedStreamingAttempt<C>
where
    C: TurnChannels,
{
    receipt: PublishedTurnReceipt,
    raw_output: String,
    provider_results: Vec<ProviderToolResult>,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    commits: Vec<C::Commit>,
}

impl<C> fmt::Debug for PublishedStreamingAttempt<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedStreamingAttempt")
            .field("identity", self.receipt.identity())
            .field("pending_commits", &self.commits.len())
            .finish_non_exhaustive()
    }
}

impl<C> PublishedStreamingAttempt<C>
where
    C: TurnChannels,
{
    pub fn receipt(&self) -> &PublishedTurnReceipt {
        &self.receipt
    }

    pub fn raw_output(&self) -> &str {
        &self.raw_output
    }

    pub fn provider_results(&self) -> &[ProviderToolResult] {
        &self.provider_results
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        std::mem::take(&mut self.update)
    }

    pub fn pending_commits(&self) -> &[C::Commit] {
        &self.commits
    }

    pub fn take_pending_commits(&mut self) -> Vec<C::Commit> {
        std::mem::take(&mut self.commits)
    }
}

/// Publication failure retaining the finished attempt for explicit abort.
#[must_use = "a failed publication must explicitly abort the retained attempt"]
pub struct StreamingPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    attempt: FinishedStreamingAttempt<C>,
    source: E,
}

impl<C, E> fmt::Debug for StreamingPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingPublishFailure")
            .field("identity", self.attempt.identity())
            .field("source", &self.source)
            .finish()
    }
}

impl<C, E> fmt::Display for StreamingPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "publication failed for provider attempt `{}`: {}",
            self.attempt.identity().provider_attempt_id(),
            self.source
        )
    }
}

impl<C, E> Error for StreamingPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl<C, E> StreamingPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub fn provider_results(&self) -> &[ProviderToolResult] {
        self.attempt.provider_results()
    }

    pub fn source_error(&self) -> &E {
        &self.source
    }

    pub async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }
}

/// Recoverable finish error; the retained attempt must still be aborted.
#[must_use = "a finish failure must explicitly abort the retained attempt"]
pub struct StreamingFinishFailure<C>
where
    C: TurnChannels,
{
    attempt: MountedStreamingAttempt<C>,
    error: StreamingAttemptError,
}

impl<C> fmt::Debug for StreamingFinishFailure<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingFinishFailure")
            .field("identity", self.attempt.identity())
            .field("error", &self.error)
            .finish()
    }
}

impl<C> StreamingFinishFailure<C>
where
    C: TurnChannels,
{
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub fn error(&self) -> &StreamingAttemptError {
        &self.error
    }

    pub(crate) fn into_parts(self) -> (MountedStreamingAttempt<C>, StreamingAttemptError) {
        (self.attempt, self.error)
    }

    pub async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }
}

async fn abort_mounted_bindings<C>(
    identity: ProviderAttemptIdentity,
    mounted_bindings: Vec<MountedAttemptBinding<C>>,
    provider_runtime: ProviderDispatchRuntime<C>,
    live_runtime: SharedLiveEffectRuntime<C::Live>,
    reason: BindingAbortReason,
) -> StreamingAbortReport
where
    C: TurnChannels,
{
    let live = live_runtime
        .lock()
        .await
        .abort(identity.clone(), reason)
        .await;
    let mut bindings = Vec::with_capacity(mounted_bindings.len());
    for mounted in mounted_bindings {
        let acknowledgement = mounted.binding.lock().await.abort(reason);
        bindings.push(StreamingBindingAbort {
            id: mounted.id,
            route: mounted.route,
            acknowledgement,
        });
    }
    let dispatchers = provider_runtime.abort(reason).await;
    StreamingAbortReport {
        identity,
        reason,
        bindings,
        dispatchers,
        live,
    }
}

impl<C, Props> PreparedUserTurn<'_, '_, C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    /// Instantiate fresh XML reducer state after the final User plan is ready.
    pub fn start_streaming_attempt<R>(
        &self,
        live_runtime: R,
    ) -> Result<MountedStreamingAttempt<C>, StreamingAttemptStartError>
    where
        R: LiveEffectRuntime<C::Live>,
    {
        self.start_streaming_attempt_with_identity(self.next_attempt_identity(), live_runtime)
    }

    pub(crate) fn start_streaming_attempt_with_identity<R>(
        &self,
        identity: ProviderAttemptIdentity,
        live_runtime: R,
    ) -> Result<MountedStreamingAttempt<C>, StreamingAttemptStartError>
    where
        R: LiveEffectRuntime<C::Live>,
    {
        MountedStreamingAttempt::new(
            self.epoch().binding_factories(),
            self.epoch().provider_capabilities(),
            self.props(),
            identity,
            live_runtime,
        )
    }
}

fn register_mounted_callbacks<C>(
    parser: &mut HermesParser<MountedCallbackFailure>,
    tag: String,
    binding: SharedMountedBinding<C>,
    accumulator: Arc<TokioMutex<StreamAccumulator<TurnEmission<C>, C::Diagnostic>>>,
    live_runtime: SharedLiveEffectRuntime<C::Live>,
    pending_commits: SharedCommitBuffer<C::Commit>,
    origin: BindingOrigin,
) where
    C: TurnChannels,
{
    let open_binding = Arc::clone(&binding);
    let open_accumulator = Arc::clone(&accumulator);
    let open_live_runtime = Arc::clone(&live_runtime);
    let open_pending_commits = Arc::clone(&pending_commits);
    let open_origin = origin.clone();
    parser.try_on_open(tag.clone(), move |element| {
        let binding = Arc::clone(&open_binding);
        let accumulator = Arc::clone(&open_accumulator);
        let live_runtime = Arc::clone(&open_live_runtime);
        let pending_commits = Arc::clone(&open_pending_commits);
        let origin = open_origin.clone();
        async move {
            let update = binding
                .lock()
                .await
                .on_open(&element)
                .map_err(MountedCallbackFailure::from)?;
            let update = interpret_live_update(
                update,
                live_runtime,
                pending_commits,
                origin,
                BindingPhase::Open,
            )
            .await
            .map_err(MountedCallbackFailure::from)?;
            accumulator.lock().await.record(update);
            Ok(())
        }
    });

    let stream_binding = Arc::clone(&binding);
    let stream_accumulator = Arc::clone(&accumulator);
    let stream_live_runtime = Arc::clone(&live_runtime);
    let stream_pending_commits = Arc::clone(&pending_commits);
    let stream_origin = origin.clone();
    parser.try_on_stream(tag.clone(), move |element| {
        let binding = Arc::clone(&stream_binding);
        let accumulator = Arc::clone(&stream_accumulator);
        let live_runtime = Arc::clone(&stream_live_runtime);
        let pending_commits = Arc::clone(&stream_pending_commits);
        let origin = stream_origin.clone();
        async move {
            let update = binding
                .lock()
                .await
                .on_stream(&element)
                .map_err(MountedCallbackFailure::from)?;
            let update = interpret_live_update(
                update,
                live_runtime,
                pending_commits,
                origin,
                BindingPhase::Stream,
            )
            .await
            .map_err(MountedCallbackFailure::from)?;
            accumulator.lock().await.record(update);
            Ok(())
        }
    });

    parser.try_on_complete(tag, move |element| {
        let binding = Arc::clone(&binding);
        let accumulator = Arc::clone(&accumulator);
        let live_runtime = Arc::clone(&live_runtime);
        let pending_commits = Arc::clone(&pending_commits);
        let origin = origin.clone();
        async move {
            let update = binding
                .lock()
                .await
                .on_complete(&element)
                .map_err(MountedCallbackFailure::from)?;
            let update = interpret_live_update(
                update,
                live_runtime,
                pending_commits,
                origin,
                BindingPhase::Complete,
            )
            .await
            .map_err(MountedCallbackFailure::from)?;
            accumulator.lock().await.record(update);
            Ok(())
        }
    });
}

pub(crate) async fn interpret_live_update<C>(
    update: MountedUpdate<C>,
    live_runtime: SharedLiveEffectRuntime<C::Live>,
    pending_commits: SharedCommitBuffer<C::Commit>,
    origin: BindingOrigin,
    phase: BindingPhase,
) -> Result<MountedUpdate<C>, LiveEffectFault>
where
    C: TurnChannels,
{
    let (emissions, diagnostics) = update.into_parts();
    let mut retained = Vec::with_capacity(emissions.len());
    for emission in emissions {
        match emission {
            TurnEmission::Live(effect) => {
                live_runtime
                    .lock()
                    .await
                    .apply(origin.clone(), phase, effect)
                    .await?;
            }
            TurnEmission::Output(output) => retained.push(TurnEmission::Output(output)),
            TurnEmission::Commit(commit) => pending_commits.lock().await.push(commit),
        }
    }
    Ok(StreamUpdate {
        emissions: retained,
        diagnostics,
    })
}

type SharedHandler<E, D> = Arc<TokioMutex<Box<dyn ErasedStreamingHandler<E, D>>>>;

struct StreamAccumulator<E, D> {
    effects: Vec<E>,
    diagnostics: Vec<D>,
}

impl<E, D> Default for StreamAccumulator<E, D> {
    fn default() -> Self {
        Self {
            effects: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

impl<E, D> StreamAccumulator<E, D> {
    fn record(&mut self, update: StreamUpdate<E, D>) {
        self.effects.extend(update.emissions);
        self.diagnostics.extend(update.diagnostics);
    }
}

/// Typed result of parsing one streamed response through component reducers.
#[derive(Debug)]
pub struct StreamingOutcome<E, D> {
    raw_output: String,
    effects: Vec<E>,
    diagnostics: Vec<D>,
}

impl<E, D> StreamingOutcome<E, D> {
    pub fn raw_output(&self) -> &str {
        &self.raw_output
    }

    pub fn effects(&self) -> &[E] {
        &self.effects
    }

    pub fn diagnostics(&self) -> &[D] {
        &self.diagnostics
    }

    pub fn into_parts(self) -> (String, Vec<E>, Vec<D>) {
        (self.raw_output, self.effects, self.diagnostics)
    }
}

/// Error while binding a compiled streaming hook plan to the parser.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamingComponentError {
    #[error("streaming tag `{tag}` is registered by both `{first}` and `{second}`")]
    DuplicateTag {
        tag: String,
        first: BindingId,
        second: BindingId,
    },
}

/// Existing `TurnSink` adapter for a compiled pure streaming hook plan.
pub struct StreamingComponentSink<E, D> {
    parser: HermesParser,
    handlers: Vec<SharedHandler<E, D>>,
    accumulator: Arc<TokioMutex<StreamAccumulator<E, D>>>,
    raw_output: String,
}

impl<E, D> StreamingComponentSink<E, D>
where
    E: Send + 'static,
    D: Send + 'static,
{
    pub fn try_new(
        plan: HookPlan<StreamingBinding<E, D>>,
    ) -> Result<Self, StreamingComponentError> {
        let mut parser = HermesParser::new();
        let accumulator = Arc::new(TokioMutex::new(StreamAccumulator::default()));
        let mut handlers = Vec::new();
        let mut tags = HashMap::<String, BindingId>::new();

        for mounted in plan.into_bindings() {
            let (id, binding) = mounted.into_parts();
            if let Some(first) = tags.insert(binding.tag.clone(), id.clone()) {
                return Err(StreamingComponentError::DuplicateTag {
                    tag: binding.tag,
                    first,
                    second: id,
                });
            }

            let tag = binding.tag;
            let handler: SharedHandler<E, D> = Arc::new(TokioMutex::new(binding.handler));
            register_callbacks(
                &mut parser,
                tag,
                Arc::clone(&handler),
                Arc::clone(&accumulator),
            );
            handlers.push(handler);
        }

        Ok(Self {
            parser,
            handlers,
            accumulator,
            raw_output: String::new(),
        })
    }
}

fn register_callbacks<E, D>(
    parser: &mut HermesParser,
    tag: String,
    handler: SharedHandler<E, D>,
    accumulator: Arc<TokioMutex<StreamAccumulator<E, D>>>,
) where
    E: Send + 'static,
    D: Send + 'static,
{
    let open_handler = Arc::clone(&handler);
    let open_accumulator = Arc::clone(&accumulator);
    parser.on_open(tag.clone(), move |element| {
        let handler = Arc::clone(&open_handler);
        let accumulator = Arc::clone(&open_accumulator);
        async move {
            let result = handler.lock().await.on_open(&element);
            accumulator.lock().await.record(result);
        }
    });

    let stream_handler = Arc::clone(&handler);
    let stream_accumulator = Arc::clone(&accumulator);
    parser.on_stream(tag.clone(), move |element| {
        let handler = Arc::clone(&stream_handler);
        let accumulator = Arc::clone(&stream_accumulator);
        async move {
            let result = handler.lock().await.on_stream(&element);
            accumulator.lock().await.record(result);
        }
    });

    parser.on_complete(tag, move |element| {
        let handler = Arc::clone(&handler);
        let accumulator = Arc::clone(&accumulator);
        async move {
            let result = handler.lock().await.on_complete(&element);
            accumulator.lock().await.record(result);
        }
    });
}

#[async_trait::async_trait]
impl<E, D> TurnSink<TextTurnEvent> for StreamingComponentSink<E, D>
where
    E: Send + 'static,
    D: Send + 'static,
{
    type Output = StreamingOutcome<E, D>;

    async fn on_event(&mut self, event: TextTurnEvent) {
        match event {
            TextTurnEvent::TextDelta(chunk) => self.parser.feed(&chunk).await,
            TextTurnEvent::TextComplete(output) => self.raw_output = output,
        }
    }

    async fn finish(mut self: Box<Self>) -> Self::Output {
        self.parser.finalize().await;
        for handler in &self.handlers {
            let result = handler.lock().await.finish();
            self.accumulator.lock().await.record(result);
        }
        let mut accumulator = self.accumulator.lock().await;
        StreamingOutcome {
            raw_output: self.raw_output,
            effects: std::mem::take(&mut accumulator.effects),
            diagnostics: std::mem::take(&mut accumulator.diagnostics),
        }
    }
}
