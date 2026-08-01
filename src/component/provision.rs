//! Isolated P3 mount-plan types for heterogeneous provided declarations.
//!
//! This module deliberately does not adapt the plans into `AgentLoop`. It
//! proves the authoring, channel-normalization, identity, and mount-validation
//! boundary while the existing homogeneous `HookPlan<B>` remains compatible.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    marker::PhantomData,
    sync::Arc,
};

use serde_json::Value;

use crate::{
    pom::{Document, XmlName},
    stream_parser::XmlElement,
    StorageString,
};

use super::experimental::{keyed, mount, system};
use super::{
    binding, compile_component, BindingId, BindingKey, ChannelMap, ComponentChildren,
    ComponentError, ComponentKey, ComponentNode, HarnessEpochId, IntoComponentNode, LiveScopeId,
    NoBindings, PomChildren, PomView, PromptRole, ProviderAttemptId, StreamUpdate, TurnChannels,
    TurnEmission, TurnInstanceId, View, ViewExt,
};

type BoxBindingError = Box<dyn Error + Send + Sync + 'static>;
type BoxProviderDispatchError = Box<dyn Error + Send + Sync + 'static>;

/// Static route claimed by one event-driven binding factory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeRoute {
    namespace: StorageString,
    name: StorageString,
}

impl RuntimeRoute {
    pub fn new(
        namespace: impl Into<StorageString>,
        name: impl Into<StorageString>,
    ) -> Result<Self, ComponentError> {
        let namespace = namespace.into();
        let name = name.into();
        if namespace.is_empty() || name.is_empty() {
            return Err(ComponentError::InvalidBindingContract {
                message: "a runtime route requires non-empty namespace and name".to_owned(),
            });
        }
        Ok(Self { namespace, name })
    }

    pub fn xml(tag: impl Into<StorageString>) -> Result<Self, ComponentError> {
        let tag = tag.into();
        XmlName::new(tag.as_ref()).map_err(|error| ComponentError::InvalidBindingContract {
            message: format!("invalid XML runtime route `{tag}`: {error}"),
        })?;
        Self::new("xml", tag)
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for RuntimeRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.namespace, self.name)
    }
}

/// Provider-neutral static description of one native tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderToolSpec {
    name: StorageString,
    description: StorageString,
    input_schema: Value,
}

impl ProviderToolSpec {
    pub fn new(
        name: impl Into<StorageString>,
        description: impl Into<StorageString>,
        input_schema: Value,
    ) -> Result<Self, ComponentError> {
        let name = name.into();
        BindingKey::new(name.clone())?;
        if !input_schema.is_object() {
            return Err(ComponentError::InvalidBindingContract {
                message: format!("provider tool `{name}` requires an object input schema"),
            });
        }
        Ok(Self {
            name,
            description: description.into(),
            input_schema,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

/// Identity attached to an inbound call, its result, and provider-attributed
/// failures.
///
/// The invocation id is the replay/idempotency identity. Some providers also
/// require a distinct optional correlation id on the result transcript; that
/// value is preserved independently.
/// A missing or empty invocation id is represented as `None`; the runtime will
/// return `invalid_invocation_id` without invoking application I/O.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderCallIdentity {
    invocation_id: Option<StorageString>,
    result_correlation_id: Option<StorageString>,
}

impl ProviderCallIdentity {
    fn new(invocation_id: impl Into<StorageString>) -> Self {
        let invocation_id = invocation_id.into();
        Self {
            invocation_id: (!invocation_id.is_empty()).then_some(invocation_id),
            result_correlation_id: None,
        }
    }

    pub fn invocation_id(&self) -> Option<&str> {
        self.invocation_id.as_deref()
    }

    pub fn result_correlation_id(&self) -> Option<&str> {
        self.result_correlation_id.as_deref()
    }
}

/// Provider-neutral native tool invocation received during one model attempt.
///
/// Provider adapters translate their SDK-specific call type into this value.
/// Raw names and arguments are intentionally accepted so malformed model
/// output can become a model-visible tool result.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolCall {
    identity: ProviderCallIdentity,
    name: StorageString,
    arguments: Value,
}

impl ProviderToolCall {
    pub fn new(
        invocation_id: impl Into<StorageString>,
        name: impl Into<StorageString>,
        arguments: Value,
    ) -> Self {
        Self {
            identity: ProviderCallIdentity::new(invocation_id),
            name: name.into(),
            arguments,
        }
    }

    pub fn with_result_correlation_id(mut self, id: impl Into<StorageString>) -> Self {
        self.identity.result_correlation_id = Some(id.into());
        self
    }

    pub fn identity(&self) -> &ProviderCallIdentity {
        &self.identity
    }

    pub fn invocation_id(&self) -> Option<&str> {
        self.identity.invocation_id()
    }

    pub fn result_correlation_id(&self) -> Option<&str> {
        self.identity.result_correlation_id()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }
}

/// Provider-neutral response produced by an application dispatcher.
///
/// Expected validation/domain/tool errors use [`Self::Error`] and remain a
/// successful dispatch. The framework attaches the original call identity and
/// returns the resulting [`ProviderToolResult`] to the provider adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderToolResponse {
    Success {
        content: Value,
    },
    Error {
        code: StorageString,
        message: StorageString,
        details: Option<Value>,
    },
}

impl ProviderToolResponse {
    pub fn success(content: impl Into<Value>) -> Self {
        Self::Success {
            content: content.into(),
        }
    }

    pub fn error(code: impl Into<StorageString>, message: impl Into<StorageString>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn error_with_details(
        code: impl Into<StorageString>,
        message: impl Into<StorageString>,
        details: Value,
    ) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
            details: Some(details),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

/// Provider-neutral result returned to the active model loop.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolResult {
    identity: ProviderCallIdentity,
    name: StorageString,
    response: ProviderToolResponse,
}

impl ProviderToolResult {
    pub(crate) fn new(call: &ProviderToolCall, response: ProviderToolResponse) -> Self {
        Self {
            identity: call.identity.clone(),
            name: call.name.clone(),
            response,
        }
    }

    pub fn identity(&self) -> &ProviderCallIdentity {
        &self.identity
    }

    pub fn invocation_id(&self) -> Option<&str> {
        self.identity.invocation_id()
    }

    pub fn result_correlation_id(&self) -> Option<&str> {
        self.identity.result_correlation_id()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn response(&self) -> &ProviderToolResponse {
        &self.response
    }

    pub fn is_error(&self) -> bool {
        self.response.is_error()
    }
}

/// Immutable identity shared by every binding created for one provider attempt.
///
/// The user-facing call label is observational only. Resource ownership and
/// retry identity use the opaque epoch, turn, provider-attempt, and live-scope
/// tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAttemptIdentity {
    epoch_id: HarnessEpochId,
    turn_instance_id: TurnInstanceId,
    provider_attempt_id: ProviderAttemptId,
    live_scope_id: LiveScopeId,
    call_label: StorageString,
}

impl ProviderAttemptIdentity {
    pub(crate) fn fresh(
        epoch_id: HarnessEpochId,
        turn_instance_id: TurnInstanceId,
        call_label: StorageString,
    ) -> Self {
        Self {
            epoch_id,
            turn_instance_id,
            provider_attempt_id: ProviderAttemptId::fresh(),
            live_scope_id: LiveScopeId::fresh(),
            call_label,
        }
    }

    pub fn epoch_id(&self) -> HarnessEpochId {
        self.epoch_id
    }

    pub fn turn_instance_id(&self) -> TurnInstanceId {
        self.turn_instance_id
    }

    pub fn provider_attempt_id(&self) -> ProviderAttemptId {
        self.provider_attempt_id
    }

    pub fn live_scope_id(&self) -> LiveScopeId {
        self.live_scope_id
    }

    pub fn call_label(&self) -> &str {
        &self.call_label
    }
}

/// Read-only inputs supplied while one reusable binding factory is instantiated.
///
/// No service, executor, prompt history, or mutable session is exposed here.
/// State returned by a factory must be owned and `'static`, so it cannot retain
/// the borrowed turn props.
pub struct TurnBindingCx<'a, Props: ?Sized, C>
where
    C: TurnChannels,
{
    props: &'a Props,
    attempt: &'a ProviderAttemptIdentity,
    binding_id: &'a BindingId,
    route: &'a RuntimeRoute,
    _channels: PhantomData<fn() -> C>,
}

impl<'a, Props: ?Sized, C> TurnBindingCx<'a, Props, C>
where
    C: TurnChannels,
{
    fn new(
        props: &'a Props,
        attempt: &'a ProviderAttemptIdentity,
        binding_id: &'a BindingId,
        route: &'a RuntimeRoute,
    ) -> Self {
        Self {
            props,
            attempt,
            binding_id,
            route,
            _channels: PhantomData,
        }
    }

    pub fn props(&self) -> &'a Props {
        self.props
    }

    pub fn epoch_id(&self) -> HarnessEpochId {
        self.attempt.epoch_id()
    }

    pub fn turn_instance_id(&self) -> TurnInstanceId {
        self.attempt.turn_instance_id()
    }

    pub fn provider_attempt_id(&self) -> ProviderAttemptId {
        self.attempt.provider_attempt_id()
    }

    pub fn live_scope_id(&self) -> LiveScopeId {
        self.attempt.live_scope_id()
    }

    pub fn call_label(&self) -> &str {
        self.attempt.call_label()
    }

    pub fn binding_id(&self) -> &'a BindingId {
        self.binding_id
    }

    pub fn route(&self) -> &'a RuntimeRoute {
        self.route
    }
}

impl<Props: ?Sized, C> fmt::Debug for TurnBindingCx<'_, Props, C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnBindingCx")
            .field("attempt", self.attempt)
            .field("binding_id", self.binding_id)
            .field("route", self.route)
            .finish_non_exhaustive()
    }
}

pub(crate) struct AttemptBindingCx<'a, Props: ?Sized> {
    pub(crate) props: &'a Props,
    pub(crate) identity: &'a ProviderAttemptIdentity,
}

/// Read-only inputs supplied while one provider dispatcher group is created.
///
/// The factory is synchronous and must not perform application I/O or acquire
/// resources that require async cleanup. External tool I/O begins only in
/// [`ProviderDispatcher::dispatch`].
pub struct ProviderDispatcherCx<'a, Props: ?Sized, C>
where
    C: TurnChannels,
{
    props: &'a Props,
    attempt: &'a ProviderAttemptIdentity,
    capability_id: &'a BindingId,
    specs: &'a [ProviderToolSpec],
    _channels: PhantomData<fn() -> C>,
}

impl<'a, Props: ?Sized, C> ProviderDispatcherCx<'a, Props, C>
where
    C: TurnChannels,
{
    fn new(
        props: &'a Props,
        attempt: &'a ProviderAttemptIdentity,
        capability_id: &'a BindingId,
        specs: &'a [ProviderToolSpec],
    ) -> Self {
        Self {
            props,
            attempt,
            capability_id,
            specs,
            _channels: PhantomData,
        }
    }

    pub fn props(&self) -> &'a Props {
        self.props
    }

    pub fn identity(&self) -> &'a ProviderAttemptIdentity {
        self.attempt
    }

    pub fn capability_id(&self) -> &'a BindingId {
        self.capability_id
    }

    pub fn specs(&self) -> &'a [ProviderToolSpec] {
        self.specs
    }
}

impl<Props: ?Sized, C> fmt::Debug for ProviderDispatcherCx<'_, Props, C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDispatcherCx")
            .field("attempt", self.attempt)
            .field("capability_id", self.capability_id)
            .field("specs", &self.specs)
            .finish_non_exhaustive()
    }
}

pub(crate) struct AttemptProviderCx<'a, Props: ?Sized> {
    pub(crate) props: &'a Props,
    pub(crate) identity: &'a ProviderAttemptIdentity,
}

/// Stable key for one provider-native invocation inside an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderInvocationKey {
    epoch_id: HarnessEpochId,
    turn_instance_id: TurnInstanceId,
    capability_id: BindingId,
    invocation_id: StorageString,
}

impl ProviderInvocationKey {
    pub fn epoch_id(&self) -> HarnessEpochId {
        self.epoch_id
    }

    pub fn turn_instance_id(&self) -> TurnInstanceId {
        self.turn_instance_id
    }

    pub fn capability_id(&self) -> &BindingId {
        &self.capability_id
    }

    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }
}

/// Identity and ordering metadata for one awaited dispatcher invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDispatchContext {
    identity: ProviderAttemptIdentity,
    invocation: ProviderInvocationKey,
    sequence: u64,
}

impl ProviderDispatchContext {
    pub(crate) fn new(
        identity: &ProviderAttemptIdentity,
        capability_id: &BindingId,
        invocation_id: impl Into<StorageString>,
        sequence: u64,
    ) -> Self {
        let invocation_id = invocation_id.into();
        Self {
            identity: identity.clone(),
            invocation: ProviderInvocationKey {
                epoch_id: identity.epoch_id(),
                turn_instance_id: identity.turn_instance_id(),
                capability_id: capability_id.clone(),
                invocation_id,
            },
            sequence,
        }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn invocation_key(&self) -> &ProviderInvocationKey {
        &self.invocation
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Context supplied while explicitly aborting one provider dispatcher group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDispatcherAbortContext {
    identity: ProviderAttemptIdentity,
    capability_id: BindingId,
    reason: BindingAbortReason,
    completed_calls: u64,
}

impl ProviderDispatcherAbortContext {
    pub(crate) fn new(
        identity: &ProviderAttemptIdentity,
        capability_id: &BindingId,
        reason: BindingAbortReason,
        completed_calls: u64,
    ) -> Self {
        Self {
            identity: identity.clone(),
            capability_id: capability_id.clone(),
            reason,
            completed_calls,
        }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn capability_id(&self) -> &BindingId {
        &self.capability_id
    }

    pub fn reason(&self) -> BindingAbortReason {
        self.reason
    }

    pub fn completed_calls(&self) -> u64 {
        self.completed_calls
    }
}

/// Pure binding callback or factory-initializer failure before host I/O.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct BindingFailure {
    #[source]
    source: BoxBindingError,
}

impl BindingFailure {
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }

    pub fn source_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.as_ref()
    }
}

/// Phase in which a pure binding operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindingPhase {
    Initialize,
    Open,
    Stream,
    Complete,
    Dispatch,
    Finish,
    Finalize,
}

impl fmt::Display for BindingPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Initialize => "initialize",
            Self::Open => "open",
            Self::Stream => "stream",
            Self::Complete => "complete",
            Self::Dispatch => "dispatch",
            Self::Finish => "finish",
            Self::Finalize => "finalize",
        })
    }
}

/// Stable identity of the binding that owns a terminal failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingOrigin {
    attempt: ProviderAttemptIdentity,
    binding_id: BindingId,
    route: RuntimeRoute,
}

impl BindingOrigin {
    pub(crate) fn new(
        attempt: &ProviderAttemptIdentity,
        binding_id: &BindingId,
        route: &RuntimeRoute,
    ) -> Self {
        Self {
            attempt: attempt.clone(),
            binding_id: binding_id.clone(),
            route: route.clone(),
        }
    }

    pub fn attempt(&self) -> &ProviderAttemptIdentity {
        &self.attempt
    }

    pub fn binding_id(&self) -> &BindingId {
        &self.binding_id
    }

    pub fn route(&self) -> &RuntimeRoute {
        &self.route
    }
}

/// Terminal failure attributed to one mounted binding and lifecycle phase.
#[derive(Debug, thiserror::Error)]
#[error("binding `{binding_id}` on `{route}` failed during {phase}: {source}", binding_id = .origin.binding_id(), route = .origin.route())]
pub struct BindingFault {
    origin: Box<BindingOrigin>,
    phase: BindingPhase,
    #[source]
    source: BindingFailure,
}

impl BindingFault {
    pub(crate) fn new(
        attempt: &ProviderAttemptIdentity,
        binding_id: &BindingId,
        route: &RuntimeRoute,
        phase: BindingPhase,
        source: BindingFailure,
    ) -> Self {
        Self {
            origin: Box::new(BindingOrigin::new(attempt, binding_id, route)),
            phase,
            source,
        }
    }

    pub fn origin(&self) -> &BindingOrigin {
        &self.origin
    }

    pub fn phase(&self) -> BindingPhase {
        self.phase
    }

    pub fn source_failure(&self) -> &BindingFailure {
        &self.source
    }
}

/// Why an in-progress binding instance is being discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindingAbortReason {
    InitializationFailure,
    ProviderFailure,
    ParserFailure,
    /// Host-owned Live interpretation or native capability runtime failed.
    HostRuntimeFailure,
    /// The host rejected a typed Output/Diagnostic update before publication.
    HostInterpretationFailure,
    Cancelled,
    PreparationReplaced,
    PublishFailure,
}

/// Synchronous acknowledgement returned while an attempt runtime is aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingAbortAck {
    NoCleanupNeeded,
    LocalCleanupCompleted,
}

/// Fresh, synchronous reducer state created from a mounted factory.
///
/// The callbacks are functional runtime declarations: they mutate only their
/// attempt-local state and return owned typed values. They must not perform
/// application I/O. A shared attempt parser invokes them in wire order; the
/// host interprets live and commit values outside these methods.
pub trait BindingInstance<C>: fmt::Debug + Send + 'static
where
    C: TurnChannels,
{
    fn on_open(
        &mut self,
        _element: &XmlElement,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, BindingFailure> {
        Ok(StreamUpdate::new())
    }

    fn on_stream(
        &mut self,
        _element: &XmlElement,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, BindingFailure> {
        Ok(StreamUpdate::new())
    }

    fn on_complete(
        &mut self,
        _element: &XmlElement,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, BindingFailure> {
        Ok(StreamUpdate::new())
    }

    fn on_finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, BindingFailure> {
        Ok(StreamUpdate::new())
    }

    fn abort(&mut self, _reason: BindingAbortReason) -> BindingAbortAck {
        BindingAbortAck::NoCleanupNeeded
    }

    fn kind(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// Type-erased terminal failure from an executable provider dispatcher.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct ProviderDispatchFailure {
    #[source]
    source: BoxProviderDispatchError,
}

impl ProviderDispatchFailure {
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }

    pub fn source_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.as_ref()
    }
}

/// Model-visible result plus typed component values from one tool invocation.
#[must_use]
pub struct ProviderDispatchUpdate<C>
where
    C: TurnChannels,
{
    response: ProviderToolResponse,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
}

impl<C> fmt::Debug for ProviderDispatchUpdate<C>
where
    C: TurnChannels,
    TurnEmission<C>: fmt::Debug,
    C::Diagnostic: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDispatchUpdate")
            .field("response", &self.response)
            .field("update", &self.update)
            .finish()
    }
}

impl<C> ProviderDispatchUpdate<C>
where
    C: TurnChannels,
{
    pub fn new(
        response: ProviderToolResponse,
        update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) -> Self {
        Self { response, update }
    }

    pub fn response(&self) -> &ProviderToolResponse {
        &self.response
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn into_parts(
        self,
    ) -> (
        ProviderToolResponse,
        StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) {
        (self.response, self.update)
    }

    fn map_channels<Root>(self, map: &dyn ChannelMap<C, Root>) -> ProviderDispatchUpdate<Root>
    where
        Root: TurnChannels,
    {
        ProviderDispatchUpdate {
            response: self.response,
            update: map_provider_update(self.update, map),
        }
    }
}

/// Acknowledgement after a dispatcher group releases attempt-local resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDispatcherAbortAck {
    NoCleanupNeeded,
    CleanupCompleted,
}

/// Fresh native-provider runtime state created for one provider attempt.
///
/// Unlike XML reducers, `dispatch` is an explicit host I/O boundary: it may
/// await a tool server and must return a provider-neutral result to the active
/// model loop. Application-level tool errors belong in
/// [`ProviderToolResponse::error`]; `Err` is reserved for terminal infrastructure
/// or lifecycle failure. Calls on one attempt runtime are serialized by the
/// framework.
#[async_trait::async_trait]
pub trait ProviderDispatcher<C>: fmt::Debug + Send + 'static
where
    C: TurnChannels,
{
    type Error: Error + Send + Sync + 'static;

    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<C>, Self::Error>;

    async fn finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, Self::Error> {
        Ok(StreamUpdate::new())
    }

    async fn abort(
        &mut self,
        _context: &ProviderDispatcherAbortContext,
    ) -> Result<ProviderDispatcherAbortAck, Self::Error> {
        Ok(ProviderDispatcherAbortAck::NoCleanupNeeded)
    }

    fn kind(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

#[async_trait::async_trait]
pub(crate) trait ErasedProviderDispatcher<C>: fmt::Debug + Send
where
    C: TurnChannels,
{
    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<C>, ProviderDispatchFailure>;

    async fn finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, ProviderDispatchFailure>;

    async fn abort(
        &mut self,
        context: &ProviderDispatcherAbortContext,
    ) -> Result<ProviderDispatcherAbortAck, ProviderDispatchFailure>;

    fn kind(&self) -> &'static str;
}

#[derive(Debug)]
struct TypedProviderDispatcher<D>(D);

#[async_trait::async_trait]
impl<C, D> ErasedProviderDispatcher<C> for TypedProviderDispatcher<D>
where
    C: TurnChannels,
    D: ProviderDispatcher<C>,
{
    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<C>, ProviderDispatchFailure> {
        self.0
            .dispatch(context, call)
            .await
            .map_err(ProviderDispatchFailure::new)
    }

    async fn finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, ProviderDispatchFailure> {
        self.0.finish().await.map_err(ProviderDispatchFailure::new)
    }

    async fn abort(
        &mut self,
        context: &ProviderDispatcherAbortContext,
    ) -> Result<ProviderDispatcherAbortAck, ProviderDispatchFailure> {
        self.0
            .abort(context)
            .await
            .map_err(ProviderDispatchFailure::new)
    }

    fn kind(&self) -> &'static str {
        self.0.kind()
    }
}

type BindingMaker<C, Props> = dyn for<'a> Fn(TurnBindingCx<'a, Props, C>) -> Result<Box<dyn BindingInstance<C>>, BindingFailure>
    + Send
    + Sync
    + 'static;
type DispatcherMaker<C, Props> = dyn for<'a> Fn(
        ProviderDispatcherCx<'a, Props, C>,
    ) -> Result<Box<dyn ErasedProviderDispatcher<C>>, ProviderDispatchFailure>
    + Send
    + Sync
    + 'static;
type SharedChannelMap<Local, Root> = Arc<dyn ChannelMap<Local, Root>>;
type SharedPropsProjection<Parent, Child> =
    Arc<dyn for<'a> Fn(&'a Parent) -> &'a Child + Send + Sync + 'static>;

fn validate_runtime_implementation_version(
    version: impl Into<StorageString>,
) -> Result<StorageString, ComponentError> {
    let version = version.into();
    if version.is_empty() || version.chars().any(char::is_control) {
        return Err(ComponentError::InvalidBindingContract {
            message: "a runtime implementation version must be non-empty and contain no control characters"
                .to_owned(),
        });
    }
    Ok(version)
}

/// Stable durable identity for exactly one provided runtime declaration.
///
/// The id identifies one binding factory or one provider capability group.
/// `implementation_version` is author-owned because Rust cannot fingerprint
/// reducer or dispatcher closure behavior. Change it whenever an implementation
/// change is incompatible with an existing durable epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeContract {
    id: StorageString,
    implementation_version: StorageString,
}

impl RuntimeContract {
    pub fn new(
        id: impl Into<StorageString>,
        implementation_version: impl Into<StorageString>,
    ) -> Result<Self, ComponentError> {
        Ok(Self {
            id: validate_runtime_declaration_id(id)?,
            implementation_version: validate_runtime_implementation_version(
                implementation_version,
            )?,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn implementation_version(&self) -> &str {
        &self.implementation_version
    }

    /// Stable component identity derived from this declaration id.
    ///
    /// Runtime declaration ids intentionally accept a wider character set than
    /// component keys. Encoding the UTF-8 bytes keeps the default structural
    /// key valid, deterministic, and collision-free without asking authors to
    /// repeat the same identity in two formats.
    fn structural_component_key(&self) -> StorageString {
        structural_component_key_for_declaration(&self.id)
    }
}

fn structural_component_key_for_declaration(declaration_id: &str) -> StorageString {
    let id = declaration_id.as_bytes();
    let mut key = String::with_capacity("contract-".len() + id.len() * 2);
    key.push_str("contract-");
    for byte in id {
        use std::fmt::Write as _;

        write!(&mut key, "{byte:02x}")
            .expect("writing a hexadecimal runtime declaration id cannot fail");
    }
    key.into()
}

/// Pure author-owned contract for one provider-native capability group.
///
/// This value contains only durable identity, an author contract version, and
/// ordered provider-facing schemas. It contains no dispatcher closure or host
/// implementation version. A mounted host must bind it through a
/// [`ProviderDispatcherRegistry`] before epoch admission can render or attach
/// the System POM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilityContract {
    declaration_id: StorageString,
    contract_version: StorageString,
    specs: Vec<ProviderToolSpec>,
}

impl ProviderCapabilityContract {
    pub fn new(
        declaration_id: impl Into<StorageString>,
        contract_version: impl Into<StorageString>,
        specs: impl IntoIterator<Item = ProviderToolSpec>,
    ) -> Result<Self, ComponentError> {
        Ok(Self {
            declaration_id: validate_runtime_declaration_id(declaration_id)?,
            contract_version: validate_runtime_implementation_version(contract_version)?,
            specs: validate_provider_specs(specs)?,
        })
    }

    pub fn declaration_id(&self) -> &str {
        &self.declaration_id
    }

    pub fn contract_version(&self) -> &str {
        &self.contract_version
    }

    pub fn specs(&self) -> &[ProviderToolSpec] {
        &self.specs
    }

    fn structural_component_key(&self) -> StorageString {
        structural_component_key_for_declaration(&self.declaration_id)
    }

    fn from_legacy(contract: RuntimeContract, specs: Vec<ProviderToolSpec>) -> Self {
        Self {
            declaration_id: contract.id,
            contract_version: contract.implementation_version,
            specs,
        }
    }
}

fn validate_runtime_declaration_id(
    declaration_id: impl Into<StorageString>,
) -> Result<StorageString, ComponentError> {
    let declaration_id = declaration_id.into();
    if declaration_id.is_empty() || declaration_id.chars().any(char::is_control) {
        return Err(ComponentError::InvalidBindingContract {
            message:
                "a durable runtime declaration id must be non-empty and contain no control characters"
                    .to_owned(),
        });
    }
    Ok(declaration_id)
}

/// One reusable, already channel-normalized binding factory declaration.
pub struct FactoryDeclaration<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    route: RuntimeRoute,
    runtime_contract: Option<RuntimeContract>,
    instantiate: Arc<BindingMaker<C, Props>>,
}

impl<C, Props> Clone for FactoryDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            route: self.route.clone(),
            runtime_contract: self.runtime_contract.clone(),
            instantiate: Arc::clone(&self.instantiate),
        }
    }
}

impl<C, Props> FactoryDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn new<I>(route: RuntimeRoute, instantiate: impl Fn() -> I + Send + Sync + 'static) -> Self
    where
        I: BindingInstance<C>,
    {
        Self {
            route,
            runtime_contract: None,
            instantiate: Arc::new(move |_| Ok(Box::new(instantiate()))),
        }
    }

    pub fn with_context<I>(
        route: RuntimeRoute,
        instantiate: impl for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<I, BindingFailure>
            + Send
            + Sync
            + 'static,
    ) -> Self
    where
        I: BindingInstance<C>,
    {
        Self {
            route,
            runtime_contract: None,
            instantiate: Arc::new(move |context| {
                instantiate(&context)
                    .map(|instance| Box::new(instance) as Box<dyn BindingInstance<C>>)
            }),
        }
    }

    pub fn route(&self) -> &RuntimeRoute {
        &self.route
    }

    pub fn implementation_version(&self) -> Option<&str> {
        self.runtime_contract
            .as_ref()
            .map(RuntimeContract::implementation_version)
    }

    pub fn declaration_id(&self) -> Option<&str> {
        self.runtime_contract.as_ref().map(RuntimeContract::id)
    }

    pub(crate) fn with_runtime_contract(mut self, contract: RuntimeContract) -> Self {
        self.runtime_contract = Some(contract);
        self
    }

    fn instantiate(
        &self,
        context: TurnBindingCx<'_, Props, C>,
    ) -> Result<Box<dyn BindingInstance<C>>, BindingFailure> {
        (self.instantiate)(context)
    }

    fn map_channels<Root>(self, map: SharedChannelMap<C, Root>) -> FactoryDeclaration<Root, Props>
    where
        Root: TurnChannels,
    {
        let instantiate = self.instantiate;
        FactoryDeclaration {
            route: self.route,
            runtime_contract: self.runtime_contract,
            instantiate: Arc::new(move |context| {
                let local_context = TurnBindingCx::new(
                    context.props(),
                    context.attempt,
                    context.binding_id(),
                    context.route(),
                );
                instantiate(local_context).map(|inner| {
                    Box::new(MappedBindingInstance {
                        inner,
                        map: Arc::clone(&map),
                    }) as Box<dyn BindingInstance<Root>>
                })
            }),
        }
    }

    fn project_props<Parent>(
        self,
        project: SharedPropsProjection<Parent, Props>,
    ) -> FactoryDeclaration<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        let instantiate = self.instantiate;
        FactoryDeclaration {
            route: self.route,
            runtime_contract: self.runtime_contract,
            instantiate: Arc::new(move |context| {
                let local_context = TurnBindingCx::new(
                    project(context.props()),
                    context.attempt,
                    context.binding_id(),
                    context.route(),
                );
                instantiate(local_context)
            }),
        }
    }
}

impl<C, Props> fmt::Debug for FactoryDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FactoryDeclaration")
            .field("route", &self.route)
            .field("runtime_contract", &self.runtime_contract)
            .finish_non_exhaustive()
    }
}

struct MappedBindingInstance<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    inner: Box<dyn BindingInstance<Local>>,
    map: SharedChannelMap<Local, Root>,
}

impl<Local, Root> fmt::Debug for MappedBindingInstance<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MappedBindingInstance")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<Local, Root> BindingInstance<Root> for MappedBindingInstance<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    fn on_open(
        &mut self,
        element: &XmlElement,
    ) -> Result<StreamUpdate<TurnEmission<Root>, Root::Diagnostic>, BindingFailure> {
        self.inner
            .on_open(element)
            .map(|update| self.map_update(update))
    }

    fn on_stream(
        &mut self,
        element: &XmlElement,
    ) -> Result<StreamUpdate<TurnEmission<Root>, Root::Diagnostic>, BindingFailure> {
        self.inner
            .on_stream(element)
            .map(|update| self.map_update(update))
    }

    fn on_complete(
        &mut self,
        element: &XmlElement,
    ) -> Result<StreamUpdate<TurnEmission<Root>, Root::Diagnostic>, BindingFailure> {
        self.inner
            .on_complete(element)
            .map(|update| self.map_update(update))
    }

    fn on_finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<Root>, Root::Diagnostic>, BindingFailure> {
        self.inner.on_finish().map(|update| self.map_update(update))
    }

    fn abort(&mut self, reason: BindingAbortReason) -> BindingAbortAck {
        self.inner.abort(reason)
    }

    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
}

impl<Local, Root> MappedBindingInstance<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    fn map_update(
        &self,
        update: StreamUpdate<TurnEmission<Local>, Local::Diagnostic>,
    ) -> StreamUpdate<TurnEmission<Root>, Root::Diagnostic> {
        map_provider_update(update, self.map.as_ref())
    }
}

fn map_provider_update<Local, Root>(
    update: StreamUpdate<TurnEmission<Local>, Local::Diagnostic>,
    map: &dyn ChannelMap<Local, Root>,
) -> StreamUpdate<TurnEmission<Root>, Root::Diagnostic>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    update
        .map_emissions(|emission| match emission {
            TurnEmission::Output(output) => TurnEmission::Output(map.output(output)),
            TurnEmission::Live(live) => TurnEmission::Live(map.live(live)),
            TurnEmission::Commit(commit) => TurnEmission::Commit(map.commit(commit)),
        })
        .map_diagnostics(|diagnostic| map.diagnostic(diagnostic))
}

struct ProviderDispatcherBinding<C, Props: ?Sized + 'static>
where
    C: TurnChannels,
{
    host_implementation_version: Option<StorageString>,
    instantiate_dispatcher: Arc<DispatcherMaker<C, Props>>,
}

impl<C, Props> Clone for ProviderDispatcherBinding<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            host_implementation_version: self.host_implementation_version.clone(),
            instantiate_dispatcher: Arc::clone(&self.instantiate_dispatcher),
        }
    }
}

/// One provider-native tool group declaration.
///
/// Ordinary compatibility declarations may still carry an embedded dispatcher
/// binding. New durable declarations are pure: they retain only
/// [`ProviderCapabilityContract`] and receive their dispatcher from the
/// mount-owned [`ProviderDispatcherRegistry`].
pub struct ProviderCapabilityDeclaration<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    specs: Vec<ProviderToolSpec>,
    contract: Option<ProviderCapabilityContract>,
    dispatcher_binding: Option<ProviderDispatcherBinding<C, Props>>,
}

impl<C, Props> Clone for ProviderCapabilityDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            specs: self.specs.clone(),
            contract: self.contract.clone(),
            dispatcher_binding: self.dispatcher_binding.clone(),
        }
    }
}

impl<C, Props> ProviderCapabilityDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn new<D, Specs>(
        specs: Specs,
        instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
    ) -> Result<Self, ComponentError>
    where
        D: ProviderDispatcher<C>,
        Specs: IntoIterator<Item = ProviderToolSpec>,
    {
        Self::with_context(specs, move |_| Ok(instantiate_dispatcher()))
    }

    pub fn with_context<D, Specs>(
        specs: Specs,
        instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
            + Send
            + Sync
            + 'static,
    ) -> Result<Self, ComponentError>
    where
        D: ProviderDispatcher<C>,
        Specs: IntoIterator<Item = ProviderToolSpec>,
    {
        let specs = validate_provider_specs(specs)?;
        Ok(Self {
            specs,
            contract: None,
            dispatcher_binding: Some(ProviderDispatcherBinding {
                host_implementation_version: None,
                instantiate_dispatcher: Arc::new(move |context| {
                    instantiate_dispatcher(&context).map(|dispatcher| {
                        Box::new(TypedProviderDispatcher(dispatcher))
                            as Box<dyn ErasedProviderDispatcher<C>>
                    })
                }),
            }),
        })
    }

    fn from_contract(contract: ProviderCapabilityContract) -> Self {
        Self {
            specs: contract.specs.clone(),
            contract: Some(contract),
            dispatcher_binding: None,
        }
    }

    pub fn specs(&self) -> &[ProviderToolSpec] {
        &self.specs
    }

    pub fn contract_version(&self) -> Option<&str> {
        self.contract
            .as_ref()
            .map(ProviderCapabilityContract::contract_version)
    }

    pub fn host_implementation_version(&self) -> Option<&str> {
        self.dispatcher_binding
            .as_ref()
            .and_then(|binding| binding.host_implementation_version.as_deref())
    }

    /// Compatibility accessor retained for the legacy combined contract.
    pub fn implementation_version(&self) -> Option<&str> {
        self.contract_version()
    }

    pub fn declaration_id(&self) -> Option<&str> {
        self.contract
            .as_ref()
            .map(ProviderCapabilityContract::declaration_id)
    }

    fn contract(&self) -> Option<&ProviderCapabilityContract> {
        self.contract.as_ref()
    }

    pub(crate) fn with_runtime_contract(mut self, contract: RuntimeContract) -> Self {
        let host_implementation_version = contract.implementation_version.clone();
        self.contract = Some(ProviderCapabilityContract::from_legacy(
            contract,
            self.specs.clone(),
        ));
        if let Some(binding) = &mut self.dispatcher_binding {
            binding.host_implementation_version = Some(host_implementation_version);
        }
        self
    }

    fn with_dispatcher_binding(mut self, binding: ProviderDispatcherBinding<C, Props>) -> Self {
        self.dispatcher_binding = Some(binding);
        self
    }

    fn is_dispatcher_bound(&self) -> bool {
        self.dispatcher_binding.is_some()
    }

    fn instantiate_dispatcher(
        &self,
        context: ProviderDispatcherCx<'_, Props, C>,
    ) -> Result<Box<dyn ErasedProviderDispatcher<C>>, ProviderDispatchFailure> {
        let binding = self.dispatcher_binding.as_ref().ok_or_else(|| {
            ProviderDispatchFailure::new(ProviderDispatcherRegistryError::MissingBinding {
                declaration_id: self
                    .declaration_id()
                    .unwrap_or("<non-durable-provider-capability>")
                    .to_owned(),
            })
        })?;
        (binding.instantiate_dispatcher)(context)
    }

    fn map_channels<Root>(
        self,
        map: SharedChannelMap<C, Root>,
    ) -> ProviderCapabilityDeclaration<Root, Props>
    where
        Root: TurnChannels,
    {
        let dispatcher_binding = self.dispatcher_binding.map(|binding| {
            let instantiate_dispatcher = binding.instantiate_dispatcher;
            ProviderDispatcherBinding {
                host_implementation_version: binding.host_implementation_version,
                instantiate_dispatcher: Arc::new(move |context| {
                    let local_context = ProviderDispatcherCx::new(
                        context.props(),
                        context.attempt,
                        context.capability_id(),
                        context.specs(),
                    );
                    instantiate_dispatcher(local_context).map(|inner| {
                        Box::new(MappedProviderDispatcher {
                            inner,
                            map: Arc::clone(&map),
                        }) as Box<dyn ErasedProviderDispatcher<Root>>
                    })
                }),
            }
        });
        ProviderCapabilityDeclaration {
            specs: self.specs,
            contract: self.contract,
            dispatcher_binding,
        }
    }

    fn project_props<Parent>(
        self,
        project: SharedPropsProjection<Parent, Props>,
    ) -> ProviderCapabilityDeclaration<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        let dispatcher_binding = self.dispatcher_binding.map(|binding| {
            let instantiate_dispatcher = binding.instantiate_dispatcher;
            ProviderDispatcherBinding {
                host_implementation_version: binding.host_implementation_version,
                instantiate_dispatcher: Arc::new(move |context| {
                    let local_context = ProviderDispatcherCx::new(
                        project(context.props()),
                        context.attempt,
                        context.capability_id(),
                        context.specs(),
                    );
                    instantiate_dispatcher(local_context)
                }),
            }
        });
        ProviderCapabilityDeclaration {
            specs: self.specs,
            contract: self.contract,
            dispatcher_binding,
        }
    }
}

impl<C, Props> fmt::Debug for ProviderCapabilityDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCapabilityDeclaration")
            .field("specs", &self.specs)
            .field("contract", &self.contract)
            .field("dispatcher_bound", &self.is_dispatcher_bound())
            .finish_non_exhaustive()
    }
}

fn validate_provider_specs(
    specs: impl IntoIterator<Item = ProviderToolSpec>,
) -> Result<Vec<ProviderToolSpec>, ComponentError> {
    let specs = specs.into_iter().collect::<Vec<_>>();
    if specs.is_empty() {
        return Err(ComponentError::InvalidBindingContract {
            message: "a provider tool group requires at least one tool spec".to_owned(),
        });
    }
    let mut names = HashSet::with_capacity(specs.len());
    for spec in &specs {
        if !names.insert(spec.name().to_owned()) {
            return Err(ComponentError::InvalidBindingContract {
                message: format!(
                    "provider tool group contains duplicate tool name `{}`",
                    spec.name()
                ),
            });
        }
    }
    Ok(specs)
}

struct ProviderDispatcherRegistryEntry<C, Props: ?Sized + 'static>
where
    C: TurnChannels,
{
    contract: ProviderCapabilityContract,
    binding: ProviderDispatcherBinding<C, Props>,
}

impl<C, Props> Clone for ProviderDispatcherRegistryEntry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            contract: self.contract.clone(),
            binding: self.binding.clone(),
        }
    }
}

/// Host-owned dispatcher bindings keyed by pure provider capability identity.
///
/// The registry is typed to the final harness channels and turn snapshot. It is
/// installed on a mounted binding, not stored in the author-owned durable
/// System definition. Registration does not instantiate a dispatcher; a fresh
/// instance is created only for a final-ready provider attempt.
pub struct ProviderDispatcherRegistry<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    entries: HashMap<StorageString, ProviderDispatcherRegistryEntry<C, Props>>,
}

impl<C, Props> Clone for ProviderDispatcherRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<C, Props> fmt::Debug for ProviderDispatcherRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDispatcherRegistry")
            .field("declaration_ids", &self.entries.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<C, Props> ProviderDispatcherRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn register<D>(
        &mut self,
        contract: ProviderCapabilityContract,
        host_implementation_version: impl Into<StorageString>,
        instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
    ) -> Result<(), ProviderDispatcherRegistryError>
    where
        D: ProviderDispatcher<C>,
    {
        self.register_with_context(contract, host_implementation_version, move |_| {
            Ok(instantiate_dispatcher())
        })
    }

    pub fn register_with_context<D>(
        &mut self,
        contract: ProviderCapabilityContract,
        host_implementation_version: impl Into<StorageString>,
        instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
            + Send
            + Sync
            + 'static,
    ) -> Result<(), ProviderDispatcherRegistryError>
    where
        D: ProviderDispatcher<C>,
    {
        let declaration_id = StorageString::from(contract.declaration_id());
        if self.entries.contains_key(&declaration_id) {
            return Err(ProviderDispatcherRegistryError::DuplicateBinding {
                declaration_id: declaration_id.to_string(),
            });
        }
        let host_implementation_version =
            validate_runtime_implementation_version(host_implementation_version).map_err(
                |error| ProviderDispatcherRegistryError::InvalidHostImplementationVersion {
                    message: error.to_string(),
                },
            )?;
        let binding = ProviderDispatcherBinding {
            host_implementation_version: Some(host_implementation_version),
            instantiate_dispatcher: Arc::new(move |context| {
                instantiate_dispatcher(&context).map(|dispatcher| {
                    Box::new(TypedProviderDispatcher(dispatcher))
                        as Box<dyn ErasedProviderDispatcher<C>>
                })
            }),
        };
        self.entries.insert(
            declaration_id,
            ProviderDispatcherRegistryEntry { contract, binding },
        );
        Ok(())
    }

    pub fn with_dispatcher<D>(
        mut self,
        contract: ProviderCapabilityContract,
        host_implementation_version: impl Into<StorageString>,
        instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
    ) -> Result<Self, ProviderDispatcherRegistryError>
    where
        D: ProviderDispatcher<C>,
    {
        self.register(
            contract,
            host_implementation_version,
            instantiate_dispatcher,
        )?;
        Ok(self)
    }

    pub fn with_context<D>(
        mut self,
        contract: ProviderCapabilityContract,
        host_implementation_version: impl Into<StorageString>,
        instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
            + Send
            + Sync
            + 'static,
    ) -> Result<Self, ProviderDispatcherRegistryError>
    where
        D: ProviderDispatcher<C>,
    {
        self.register_with_context(
            contract,
            host_implementation_version,
            instantiate_dispatcher,
        )?;
        Ok(self)
    }

    fn entry(&self, declaration_id: &str) -> Option<&ProviderDispatcherRegistryEntry<C, Props>> {
        self.entries.get(declaration_id)
    }
}

impl<C, Props> Default for ProviderDispatcherRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderDispatcherRegistryError {
    #[error("invalid provider dispatcher host implementation version: {message}")]
    InvalidHostImplementationVersion { message: String },

    #[error("provider dispatcher `{declaration_id}` is registered more than once")]
    DuplicateBinding { declaration_id: String },

    #[error("provider capability `{declaration_id}` has no host dispatcher binding")]
    MissingBinding { declaration_id: String },

    #[error("provider capability `{declaration_id}` has both an embedded compatibility dispatcher and a host registry binding")]
    ConflictingEmbeddedBinding { declaration_id: String },

    #[error(
        "provider capability `{declaration_id}` author contract version mismatch: component `{component_version}`, host `{host_version}`"
    )]
    ContractVersionMismatch {
        declaration_id: String,
        component_version: String,
        host_version: String,
    },

    #[error(
        "provider capability `{declaration_id}` tool schema/order does not match the host binding"
    )]
    ToolSchemaMismatch { declaration_id: String },
}

struct MappedProviderDispatcher<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    inner: Box<dyn ErasedProviderDispatcher<Local>>,
    map: SharedChannelMap<Local, Root>,
}

impl<Local, Root> fmt::Debug for MappedProviderDispatcher<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MappedProviderDispatcher")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<Local, Root> ErasedProviderDispatcher<Root> for MappedProviderDispatcher<Local, Root>
where
    Local: TurnChannels,
    Root: TurnChannels,
{
    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<Root>, ProviderDispatchFailure> {
        self.inner
            .dispatch(context, call)
            .await
            .map(|update| update.map_channels(self.map.as_ref()))
    }

    async fn finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<Root>, Root::Diagnostic>, ProviderDispatchFailure> {
        self.inner
            .finish()
            .await
            .map(|update| map_provider_update(update, self.map.as_ref()))
    }

    async fn abort(
        &mut self,
        context: &ProviderDispatcherAbortContext,
    ) -> Result<ProviderDispatcherAbortAck, ProviderDispatchFailure> {
        self.inner.abort(context).await
    }

    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
}

/// One normalized runtime declaration carried by a provided component tree.
pub enum RuntimeDeclaration<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    BindingFactory(FactoryDeclaration<C, Props>),
    ProviderCapability(ProviderCapabilityDeclaration<C, Props>),
}

impl<C, Props> Clone for RuntimeDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        match self {
            Self::BindingFactory(factory) => Self::BindingFactory(factory.clone()),
            Self::ProviderCapability(capability) => Self::ProviderCapability(capability.clone()),
        }
    }
}

impl<C, Props> RuntimeDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn map_channels<Root>(self, map: SharedChannelMap<C, Root>) -> RuntimeDeclaration<Root, Props>
    where
        Root: TurnChannels,
    {
        match self {
            Self::BindingFactory(factory) => {
                RuntimeDeclaration::BindingFactory(factory.map_channels(Arc::clone(&map)))
            }
            Self::ProviderCapability(capability) => {
                RuntimeDeclaration::ProviderCapability(capability.map_channels(Arc::clone(&map)))
            }
        }
    }

    fn project_props<Parent>(
        self,
        project: SharedPropsProjection<Parent, Props>,
    ) -> RuntimeDeclaration<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        match self {
            Self::BindingFactory(factory) => {
                RuntimeDeclaration::BindingFactory(factory.project_props(project))
            }
            Self::ProviderCapability(capability) => {
                RuntimeDeclaration::ProviderCapability(capability.project_props(project))
            }
        }
    }
}

impl<C, Props> fmt::Debug for RuntimeDeclaration<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BindingFactory(factory) => formatter
                .debug_tuple("BindingFactory")
                .field(factory)
                .finish(),
            Self::ProviderCapability(capability) => formatter
                .debug_tuple("ProviderCapability")
                .field(capability)
                .finish(),
        }
    }
}

/// Internal exact-one durable leaf retained by mounted roots.
///
/// Its prompt projection is linear inside one process: durable admission may
/// consume it only when that process creates the epoch. Existing and recovery
/// opens use only the declaration and never inspect POM.
pub(crate) struct DurableRuntimeLeaf<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    component_name: &'static str,
    key: StorageString,
    prompt: std::sync::Mutex<Option<PomView>>,
    declaration: RuntimeDeclaration<C, Props>,
}

/// One durable provided component before projection into a System tree.
///
/// Unlike [`Component`], this carrier retains the exact-one runtime
/// declaration needed for process-local reopen. It composes only through
/// [`crate::component::durable_system`], unless the author explicitly erases
/// durability with [`DurableComponent::into_one_shot_component`]. Channel
/// mapping transforms the retained declaration, so first mount and reopen
/// cannot drift.
#[must_use]
pub struct DurableComponent<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    leaf: Result<DurableRuntimeLeaf<C, Props>, ComponentError>,
}

enum DurableSystemNode<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    Pom(std::sync::Mutex<Option<PomView>>),
    Runtime(DurableRuntimeLeaf<C, Props>),
    Error(ComponentError),
    Scope {
        scope: DurableComponentScope,
        children: Vec<DurableSystemNode<C, Props>>,
    },
}

/// Retained component boundary shared by durable Create and POM-free reopen.
///
/// The normal component compiler owns the canonical `ComponentId` algorithm.
/// A durable System retains this small structural projection so both paths can
/// feed an equivalent tree to that compiler without re-rendering System POM.
#[derive(Clone)]
pub(crate) struct DurableComponentScope {
    name: StorageString,
    key: Option<ComponentKey>,
}

impl DurableComponentScope {
    pub(crate) fn new(name: impl Into<StorageString>) -> Self {
        Self {
            name: name.into(),
            key: None,
        }
    }

    fn with_key(mut self, key: ComponentKey) -> Result<Self, ComponentError> {
        if self.key.is_some() {
            return Err(ComponentError::ComponentAlreadyKeyed);
        }
        self.key = Some(key);
        Ok(self)
    }

    fn wrap<C, Props>(self, child: Component<C, Props>) -> Component<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        let view = match self.key {
            Some(key) => keyed(self.name, key.as_str(), child),
            None => mount(self.name, child),
        };
        Component::from_view(view)
    }

    fn wrap_ref<C, Props>(&self, child: Component<C, Props>) -> Component<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        self.clone().wrap(child)
    }
}

/// Ordered, retained System component tree for one durable epoch contract.
///
/// POM-only nodes and runtime-bearing leaves keep their author order. A
/// durable `Create` consumes the POM projection once; reopen filters the same
/// tree to declarations without consuming or rendering any POM node.
#[must_use]
pub struct DurableSystem<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    nodes: Result<Vec<DurableSystemNode<C, Props>>, ComponentError>,
}

impl<C, Props> DurableSystem<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    /// Retain an ordered set of exact-one durable leaves.
    pub fn new(
        components: impl IntoIterator<Item = DurableComponent<C, Props>>,
    ) -> Result<Self, ComponentError> {
        let mut nodes = Vec::new();
        for component in components {
            nodes.push(DurableSystemNode::Runtime(component.leaf?));
        }
        Ok(Self { nodes: Ok(nodes) })
    }

    pub(crate) fn empty() -> Self {
        Self {
            nodes: Ok(Vec::new()),
        }
    }

    fn from_nodes(nodes: Vec<DurableSystemNode<C, Props>>) -> Self {
        Self { nodes: Ok(nodes) }
    }

    pub(crate) fn from_error(error: ComponentError) -> Self {
        Self { nodes: Err(error) }
    }

    /// Retain one named component boundary around this System contribution.
    ///
    /// The same scope is replayed by the POM-free runtime projection, so the
    /// normal component compiler assigns identical binding identities on
    /// Create and reopen. An empty contribution deliberately stays unscoped:
    /// a User-only feature must not perturb durable System identity.
    pub(crate) fn with_component_scope(self, name: impl Into<StorageString>) -> Self {
        let scope = DurableComponentScope::new(name);
        match self.nodes {
            Ok(nodes) if nodes.is_empty() => Self { nodes: Ok(nodes) },
            Ok(nodes) => Self {
                nodes: Ok(vec![DurableSystemNode::Scope {
                    scope,
                    children: nodes,
                }]),
            },
            // Keep the retained component boundary for a fallible macro body.
            // The compiler still returns the original error under that scope.
            Err(error) => Self {
                nodes: Ok(vec![DurableSystemNode::Scope {
                    scope,
                    children: vec![DurableSystemNode::Error(error)],
                }]),
            },
        }
    }

    /// Attach a key to the retained outer component scope.
    ///
    /// Empty Systems are the User-only case and intentionally have no durable
    /// scope to mutate. The corresponding User fragment carries the key.
    pub(crate) fn set_outer_component_key(
        &mut self,
        key: ComponentKey,
    ) -> Result<(), ComponentError> {
        match &mut self.nodes {
            Ok(nodes) if nodes.is_empty() => Ok(()),
            Ok(nodes) => match nodes.as_mut_slice() {
                [DurableSystemNode::Scope { scope, .. }] => {
                    let updated = scope.clone().with_key(key)?;
                    *scope = updated;
                    Ok(())
                }
                _ => Err(ComponentError::KeyRequiresComponent),
            },
            // A scoped fallible feature has already converted this into an
            // Error node. Preserve an unscoped error rather than masking it.
            Err(_) => Ok(()),
        }
    }

    pub(crate) fn prompt(prompt: PomView) -> Self {
        Self::from_nodes(vec![DurableSystemNode::Pom(std::sync::Mutex::new(Some(
            prompt,
        )))])
    }

    pub(crate) fn from_component(component: DurableComponent<C, Props>) -> Self {
        match component.leaf {
            Ok(leaf) => Self::from_nodes(vec![DurableSystemNode::Runtime(leaf)]),
            Err(error) => Self::from_error(error),
        }
    }

    pub(crate) fn append(mut self, other: Self) -> Self {
        self.nodes = match (self.nodes, other.nodes) {
            (Ok(mut nodes), Ok(other)) => {
                nodes.extend(other);
                Ok(nodes)
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
        };
        self
    }

    /// Explicitly erase durability for isolated one-shot compatibility mounts.
    pub fn into_one_shot_component(self) -> Component<C, Props> {
        match self.nodes {
            Ok(nodes) => component(
                nodes
                    .into_iter()
                    .map(DurableSystemNode::into_component)
                    .collect::<Vec<_>>(),
            ),
            Err(error) => Component::from_view(Err(error)),
        }
    }

    /// Consume the POM projection for the one admitted durable Create.
    pub(crate) fn take_create_component(&self) -> Component<C, Props> {
        match &self.nodes {
            Ok(nodes) => component(
                nodes
                    .iter()
                    .map(DurableSystemNode::component)
                    .collect::<Vec<_>>(),
            ),
            Err(error) => Component::from_view(Err(error.clone())),
        }
    }

    pub(crate) fn runtime_projection(
        &self,
    ) -> Result<RuntimeBindingRegistry<C, Props>, RuntimeBindingRegistryError> {
        let plan = compile_durable_mount_provided(system(self.runtime_component()))?;
        let (_, _, binding_factories, provider_capabilities) = plan.into_parts();
        Ok(RuntimeBindingRegistry::from_compiled_plans(
            binding_factories,
            provider_capabilities,
        ))
    }

    /// Build the structural runtime-only projection for reopen.
    ///
    /// POM-only System children become empty nodes because Create first
    /// flattens them to plain POM documents. Their internal authoring scopes
    /// therefore never participate in runtime binding identity on either
    /// path, while retained feature scopes and durable leaves do.
    fn runtime_component(&self) -> Component<C, Props> {
        match &self.nodes {
            Ok(nodes) => component(
                nodes
                    .iter()
                    .map(DurableSystemNode::runtime_component)
                    .collect::<Vec<_>>(),
            ),
            Err(error) => Component::from_view(Err(error.clone())),
        }
    }

    /// Lift this retained leaf graph into a parent harness contract.
    ///
    /// The POM projection is unchanged, while the same channel map is applied
    /// to every factory and provider dispatcher captured by the POM-free
    /// runtime projection. Keeping both projections together prevents a
    /// durable reopen from rebuilding a local-channel runtime after the System
    /// component was mounted under root channels.
    pub fn map_channels<Root>(self, map: impl ChannelMap<C, Root>) -> DurableSystem<Root, Props>
    where
        Root: TurnChannels,
    {
        let map: SharedChannelMap<C, Root> = Arc::new(map);
        DurableSystem {
            nodes: self.nodes.map(|nodes| {
                nodes
                    .into_iter()
                    .map(|node| node.map_channels(Arc::clone(&map)))
                    .collect()
            }),
        }
    }

    /// Project parent turn props into the props required by this reusable
    /// durable System feature.
    ///
    /// The projection is evaluated only while a fresh binding or provider
    /// dispatcher is instantiated for a prepared turn. It cannot affect the
    /// retained System POM, its one-time Create render, or the POM-free reopen
    /// projection. Channel mapping remains a separate operation.
    pub fn project_props<Parent>(
        self,
        project: impl for<'a> Fn(&'a Parent) -> &'a Props + Send + Sync + 'static,
    ) -> DurableSystem<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        let project: SharedPropsProjection<Parent, Props> = Arc::new(project);
        DurableSystem {
            nodes: self.nodes.map(|nodes| {
                nodes
                    .into_iter()
                    .map(|node| node.project_props(Arc::clone(&project)))
                    .collect()
            }),
        }
    }
}

/// Compile a POM-only durable System child once, then retain it as ordinary
/// POM for the outer mount compiler. This prevents POM-only component scopes
/// from shifting runtime binding positions on Create while reopen omits POM.
fn durable_system_pom_component<C, Props>(prompt: PomView) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    match compile_component::<NoBindings>(system(prompt)) {
        Ok(plan) => {
            let (system, _, _) = plan.into_parts();
            Component::from_view(Ok(ComponentNode::pom(system)))
        }
        Err(error) => Component::from_view(Err(error)),
    }
}

fn durable_system_pom_already_consumed<C, Props>() -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    Component::from_view(Err(ComponentError::InvalidBindingContract {
        message: "a durable System POM projection can only be mounted once per process".to_owned(),
    }))
}

impl<C, Props> DurableSystemNode<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component(self) -> Component<C, Props> {
        match self {
            Self::Pom(prompt) => prompt
                .into_inner()
                .expect("an unshared durable System POM mutex cannot be poisoned")
                .map_or_else(
                    durable_system_pom_already_consumed,
                    durable_system_pom_component,
                ),
            Self::Runtime(leaf) => leaf.into_component(),
            Self::Error(error) => Component::from_view(Err(error)),
            Self::Scope { scope, children } => scope.wrap(component(
                children
                    .into_iter()
                    .map(Self::into_component)
                    .collect::<Vec<_>>(),
            )),
        }
    }

    fn component(&self) -> Component<C, Props> {
        match self {
            Self::Pom(prompt) => prompt
                .lock()
                .expect("a durable System POM mutex cannot be poisoned")
                .take()
                .map_or_else(
                    durable_system_pom_already_consumed,
                    durable_system_pom_component,
                ),
            Self::Runtime(leaf) => leaf.component(),
            Self::Error(error) => Component::from_view(Err(error.clone())),
            Self::Scope { scope, children } => scope.wrap_ref(component(
                children.iter().map(Self::component).collect::<Vec<_>>(),
            )),
        }
    }

    fn runtime_component(&self) -> Component<C, Props> {
        match self {
            Self::Pom(_) => Component::from_view(Ok(ComponentNode::empty())),
            Self::Runtime(leaf) => leaf.runtime_component(),
            Self::Error(error) => Component::from_view(Err(error.clone())),
            Self::Scope { scope, children } => scope.wrap_ref(component(
                children
                    .iter()
                    .map(Self::runtime_component)
                    .collect::<Vec<_>>(),
            )),
        }
    }

    fn map_channels<Root>(self, map: SharedChannelMap<C, Root>) -> DurableSystemNode<Root, Props>
    where
        Root: TurnChannels,
    {
        match self {
            Self::Pom(prompt) => DurableSystemNode::Pom(prompt),
            Self::Runtime(leaf) => DurableSystemNode::Runtime(leaf.map_channels(map)),
            Self::Error(error) => DurableSystemNode::Error(error),
            Self::Scope { scope, children } => DurableSystemNode::Scope {
                scope,
                children: children
                    .into_iter()
                    .map(|child| child.map_channels(Arc::clone(&map)))
                    .collect(),
            },
        }
    }

    fn project_props<Parent>(
        self,
        project: SharedPropsProjection<Parent, Props>,
    ) -> DurableSystemNode<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        match self {
            Self::Pom(prompt) => DurableSystemNode::Pom(prompt),
            Self::Runtime(leaf) => DurableSystemNode::Runtime(leaf.project_props(project)),
            Self::Error(error) => DurableSystemNode::Error(error),
            Self::Scope { scope, children } => DurableSystemNode::Scope {
                scope,
                children: children
                    .into_iter()
                    .map(|child| child.project_props(Arc::clone(&project)))
                    .collect(),
            },
        }
    }
}

impl<C, Props> DurableRuntimeLeaf<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn binding(
        key: impl Into<StorageString>,
        contract: RuntimeContract,
        prompt: PomView,
        declaration: FactoryDeclaration<C, Props>,
    ) -> Self {
        Self {
            component_name: "agentview::component::DurableBinding",
            key: key.into(),
            prompt: std::sync::Mutex::new(Some(prompt)),
            declaration: RuntimeDeclaration::BindingFactory(
                declaration.with_runtime_contract(contract),
            ),
        }
    }

    fn provider(
        key: impl Into<StorageString>,
        contract: RuntimeContract,
        prompt: PomView,
        declaration: ProviderCapabilityDeclaration<C, Props>,
    ) -> Self {
        Self {
            component_name: "agentview::component::DurableProviderTools",
            key: key.into(),
            prompt: std::sync::Mutex::new(Some(prompt)),
            declaration: RuntimeDeclaration::ProviderCapability(
                declaration.with_runtime_contract(contract),
            ),
        }
    }

    fn provider_contract(
        key: impl Into<StorageString>,
        contract: ProviderCapabilityContract,
        prompt: PomView,
    ) -> Self {
        Self {
            component_name: "agentview::component::DurableProviderContract",
            key: key.into(),
            prompt: std::sync::Mutex::new(Some(prompt)),
            declaration: RuntimeDeclaration::ProviderCapability(
                ProviderCapabilityDeclaration::from_contract(contract),
            ),
        }
    }

    fn into_component(self) -> Component<C, Props> {
        let prompt = self
            .prompt
            .into_inner()
            .expect("an unshared durable prompt mutex cannot be poisoned")
            .expect("a durable prompt is consumed exactly once");
        durable_leaf_component(self.component_name, self.key, prompt, self.declaration)
    }

    fn component(&self) -> Component<C, Props> {
        let prompt = self
            .prompt
            .lock()
            .expect("a durable prompt mutex cannot be poisoned")
            .take();
        let Some(prompt) = prompt else {
            return Component::from_view(Err(ComponentError::InvalidBindingContract {
                message: "a durable component POM projection can only be mounted once per process"
                    .to_owned(),
            }));
        };
        durable_leaf_component(
            self.component_name,
            self.key.clone(),
            prompt,
            self.declaration.clone(),
        )
    }

    fn runtime_component(&self) -> Component<C, Props> {
        Component::from_view(keyed(
            self.component_name,
            self.key.clone(),
            component(binding("runtime", self.declaration.clone())),
        ))
    }

    fn map_channels<Root>(self, map: SharedChannelMap<C, Root>) -> DurableRuntimeLeaf<Root, Props>
    where
        Root: TurnChannels,
    {
        DurableRuntimeLeaf {
            component_name: self.component_name,
            key: self.key,
            prompt: self.prompt,
            declaration: self.declaration.map_channels(map),
        }
    }

    fn project_props<Parent>(
        self,
        project: SharedPropsProjection<Parent, Props>,
    ) -> DurableRuntimeLeaf<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        DurableRuntimeLeaf {
            component_name: self.component_name,
            key: self.key,
            prompt: self.prompt,
            declaration: self.declaration.project_props(project),
        }
    }
}

impl<C, Props> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub(crate) fn binding(
        contract: RuntimeContract,
        prompt: PomView,
        declaration: FactoryDeclaration<C, Props>,
    ) -> Self {
        let key = contract.structural_component_key();
        Self {
            leaf: Ok(DurableRuntimeLeaf::binding(
                key,
                contract,
                prompt,
                declaration,
            )),
        }
    }

    pub(crate) fn provider(
        contract: RuntimeContract,
        prompt: PomView,
        declaration: ProviderCapabilityDeclaration<C, Props>,
    ) -> Self {
        let key = contract.structural_component_key();
        Self {
            leaf: Ok(DurableRuntimeLeaf::provider(
                key,
                contract,
                prompt,
                declaration,
            )),
        }
    }

    pub(crate) fn provider_contract(contract: ProviderCapabilityContract, prompt: PomView) -> Self {
        let key = contract.structural_component_key();
        Self {
            leaf: Ok(DurableRuntimeLeaf::provider_contract(key, contract, prompt)),
        }
    }

    pub(crate) fn from_error(error: ComponentError) -> Self {
        Self { leaf: Err(error) }
    }

    /// Override the component-tree key derived from [`RuntimeContract`].
    ///
    /// This changes only the authoring-tree identity used for structural
    /// placement. It does not change the durable declaration id or runtime
    /// contract retained for reopen.
    pub fn key(mut self, key: impl Into<StorageString>) -> Self {
        let key = match ComponentKey::new(key) {
            Ok(key) => StorageString::from(key.as_str()),
            Err(error) => return Self::from_error(error),
        };
        if let Ok(leaf) = &mut self.leaf {
            leaf.key = key;
        }
        self
    }

    /// Map every runtime lane while retaining the same prompt projection and
    /// durable identity.
    pub fn map_channels<Root>(self, map: impl ChannelMap<C, Root>) -> DurableComponent<Root, Props>
    where
        Root: TurnChannels,
    {
        let map: SharedChannelMap<C, Root> = Arc::new(map);
        DurableComponent {
            leaf: self.leaf.map(|leaf| leaf.map_channels(map)),
        }
    }

    /// Project parent turn props into the props required by this durable leaf.
    ///
    /// The projection is pure, borrowed, and applies only to fresh runtime
    /// factory/dispatcher contexts. It leaves the prompt projection, runtime
    /// contract, structural key, and channel contract unchanged.
    pub fn project_props<Parent>(
        self,
        project: impl for<'a> Fn(&'a Parent) -> &'a Props + Send + Sync + 'static,
    ) -> DurableComponent<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        let project: SharedPropsProjection<Parent, Props> = Arc::new(project);
        DurableComponent {
            leaf: self.leaf.map(|leaf| leaf.project_props(project)),
        }
    }

    /// Explicitly erase durability for an isolated one-shot compatibility mount.
    pub fn into_one_shot_component(self) -> Component<C, Props> {
        match self.leaf {
            Ok(leaf) => leaf.into_component(),
            Err(error) => Component::from_view(Err(error)),
        }
    }
}

fn durable_leaf_component<C, Props>(
    component_name: &'static str,
    key: StorageString,
    prompt: PomView,
    declaration: RuntimeDeclaration<C, Props>,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    Component::from_view(keyed(
        component_name,
        key,
        component((prompt, binding("runtime", declaration))),
    ))
}

/// POM plus heterogeneous runtime declarations normalized to `C`.
pub type MountProvidedView<C, Props = ()> = View<RuntimeDeclaration<C, Props>>;

/// One mountable component contribution.
///
/// A `Component` owns prompt-facing POM together with the reusable runtime
/// declarations that make that POM executable. The declarations are compiled
/// only from a System root; a [`crate::component::PomView`] remains the
/// deliberately smaller carrier for POM-only children that may also appear in
/// a User root.
///
/// This is the author-facing carrier for the mounted runtime. The older
/// `MountProvidedView` spelling remains available while the compatibility
/// component compiler is migrated, but new mounted components should return
/// `Component<C, Props>` and use [`component`] at legacy declaration
/// boundaries.
pub struct Component<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    view: MountProvidedView<C, Props>,
}

impl<C, Props> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub(crate) fn from_view(view: MountProvidedView<C, Props>) -> Self {
        Self { view }
    }

    /// Attach a stable parent-provided key to this component invocation.
    pub fn key(self, key: impl Into<StorageString>) -> Self {
        Self::from_view(self.view.key(key))
    }

    /// Map all runtime lanes from a local component contract to its parent
    /// harness contract in one operation.
    pub fn map_channels<Root>(self, map: impl ChannelMap<C, Root>) -> Component<Root, Props>
    where
        Root: TurnChannels,
    {
        Component::from_view(MountProvidedViewExt::map_channels(self.view, map))
    }

    /// Project parent turn props into the props required by this reusable
    /// provided component.
    ///
    /// The projection is evaluated only while the mounted runtime creates a
    /// fresh binding or provider dispatcher. It is deliberately independent of
    /// channel mapping and does not alter this component's POM or identity.
    pub fn project_props<Parent>(
        self,
        project: impl for<'a> Fn(&'a Parent) -> &'a Props + Send + Sync + 'static,
    ) -> Component<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        let project: SharedPropsProjection<Parent, Props> = Arc::new(project);
        Component::from_view(
            self.view
                .map_binding(move |declaration| declaration.project_props(Arc::clone(&project))),
        )
    }

    /// Return this value unchanged when generic component helpers need one
    /// common conversion point.
    pub fn into_component(self) -> Self {
        self
    }

    pub(crate) fn into_mount_provided_view(self) -> MountProvidedView<C, Props> {
        self.view
    }
}

impl<C, Props> IntoComponentNode<RuntimeDeclaration<C, Props>> for Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component_node(self) -> View<RuntimeDeclaration<C, Props>> {
        self.view
    }
}

/// Construct one mounted component from POM children and runtime declarations.
///
/// `PomView` and ordinary POM values compose into this carrier without gaining
/// runtime capabilities. Runtime declarations must still be placed under a
/// System root before a mount plan can be compiled.
pub fn component<C, Props>(children: impl ComponentChildren<C, Props>) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    children.into_component()
}

/// Declare one prompt contract and its reusable event-binding factory.
///
/// The contract and declaration form one view, so role placement cannot expose
/// a runtime route without also exposing its prompt-facing POM.
pub fn binding_factory<C, Props, I>(
    key: impl Into<StorageString>,
    prompt_contract: Document,
    route: RuntimeRoute,
    instantiate: impl Fn() -> I + Send + Sync + 'static,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    I: BindingInstance<C>,
{
    component((
        prompt_contract,
        binding(
            key,
            RuntimeDeclaration::BindingFactory(FactoryDeclaration::new(route, instantiate)),
        ),
    ))
}

/// Declare one prompt contract and a factory that reads turn props at start.
pub fn binding_factory_with_context<C, Props, I>(
    key: impl Into<StorageString>,
    prompt_contract: Document,
    route: RuntimeRoute,
    instantiate: impl for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<I, BindingFailure>
        + Send
        + Sync
        + 'static,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    I: BindingInstance<C>,
{
    component((
        prompt_contract,
        binding(
            key,
            RuntimeDeclaration::BindingFactory(FactoryDeclaration::with_context(
                route,
                instantiate,
            )),
        ),
    ))
}

/// Declare one durable prompt contract and exactly one reusable binding
/// factory.
///
/// Unlike the compatibility [`binding_factory`], the durable identity is
/// consumed at this leaf boundary. An aggregate [`Component`] cannot stamp one
/// identity over zero or multiple declarations.
pub fn durable_binding_factory<C, Props, I>(
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    route: RuntimeRoute,
    instantiate: impl Fn() -> I + Send + Sync + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    I: BindingInstance<C>,
{
    DurableComponent::binding(
        contract,
        prompt_contract.into_pom_view(),
        FactoryDeclaration::new(route, instantiate),
    )
}

/// Durable binding factory with an explicit component-tree key.
///
/// New durable leaves derive their key from [`RuntimeContract`] by default.
/// Use this only when a parent needs a different stable structural identity.
pub fn durable_binding_factory_with_key<C, Props, I>(
    key: impl Into<StorageString>,
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    route: RuntimeRoute,
    instantiate: impl Fn() -> I + Send + Sync + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    I: BindingInstance<C>,
{
    durable_binding_factory(contract, prompt_contract, route, instantiate).key(key)
}

/// Durable binding leaf whose fresh instance reads exact prepared-turn props.
pub fn durable_binding_factory_with_context<C, Props, I>(
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    route: RuntimeRoute,
    instantiate: impl for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<I, BindingFailure>
        + Send
        + Sync
        + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    I: BindingInstance<C>,
{
    DurableComponent::binding(
        contract,
        prompt_contract.into_pom_view(),
        FactoryDeclaration::with_context(route, instantiate),
    )
}

/// Context-aware durable binding factory with an explicit component-tree key.
pub fn durable_binding_factory_with_context_key<C, Props, I>(
    key: impl Into<StorageString>,
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    route: RuntimeRoute,
    instantiate: impl for<'a> Fn(&TurnBindingCx<'a, Props, C>) -> Result<I, BindingFailure>
        + Send
        + Sync
        + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    I: BindingInstance<C>,
{
    durable_binding_factory_with_context(contract, prompt_contract, route, instantiate).key(key)
}

/// Declare one provider-native tool group and its per-attempt dispatcher.
pub fn provider_tools<C, Props, D, Specs>(
    key: impl Into<StorageString>,
    specs: Specs,
    instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
    Specs: IntoIterator<Item = ProviderToolSpec>,
{
    Component::from_view(
        ProviderCapabilityDeclaration::new(specs, instantiate_dispatcher).and_then(|declaration| {
            binding(key, RuntimeDeclaration::ProviderCapability(declaration))
        }),
    )
}

/// Declare a pure durable provider-native capability.
///
/// The component owns prompt-facing rules, ordered schemas, stable identity,
/// and the author contract version. It does not capture provider services or a
/// dispatcher closure. The final mounted host must supply a matching
/// [`ProviderDispatcherRegistry`] entry before the System POM can be rendered
/// or attached.
pub fn durable_provider_contract<C, Props>(
    contract: ProviderCapabilityContract,
    prompt_contract: impl PomChildren,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    DurableComponent::provider_contract(contract, prompt_contract.into_pom_view())
}

/// Declare a tool group whose fresh dispatcher reads exact prepared-turn props.
pub fn provider_tools_with_context<C, Props, D, Specs>(
    key: impl Into<StorageString>,
    specs: Specs,
    instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
        + Send
        + Sync
        + 'static,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
    Specs: IntoIterator<Item = ProviderToolSpec>,
{
    Component::from_view(
        ProviderCapabilityDeclaration::with_context(specs, instantiate_dispatcher).and_then(
            |declaration| binding(key, RuntimeDeclaration::ProviderCapability(declaration)),
        ),
    )
}

/// Declare one durable provider capability group.
///
/// All `specs` share one fresh per-attempt dispatcher and therefore one
/// [`RuntimeContract`]. The optional prompt-facing rules remain in the same
/// leaf while the provider schemas are preserved as an ordered group.
pub fn durable_provider_tools<C, Props, D, Specs>(
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    specs: Specs,
    instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
    Specs: IntoIterator<Item = ProviderToolSpec>,
{
    match ProviderCapabilityDeclaration::new(specs, instantiate_dispatcher) {
        Ok(declaration) => {
            DurableComponent::provider(contract, prompt_contract.into_pom_view(), declaration)
        }
        Err(error) => DurableComponent::from_error(error),
    }
}

/// Durable provider capability group with an explicit component-tree key.
pub fn durable_provider_tools_with_key<C, Props, D, Specs>(
    key: impl Into<StorageString>,
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    specs: Specs,
    instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
    Specs: IntoIterator<Item = ProviderToolSpec>,
{
    durable_provider_tools(contract, prompt_contract, specs, instantiate_dispatcher).key(key)
}

/// Durable provider capability leaf whose dispatcher reads prepared-turn props.
pub fn durable_provider_tools_with_context<C, Props, D, Specs>(
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    specs: Specs,
    instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
        + Send
        + Sync
        + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
    Specs: IntoIterator<Item = ProviderToolSpec>,
{
    match ProviderCapabilityDeclaration::with_context(specs, instantiate_dispatcher) {
        Ok(declaration) => {
            DurableComponent::provider(contract, prompt_contract.into_pom_view(), declaration)
        }
        Err(error) => DurableComponent::from_error(error),
    }
}

/// Context-aware durable provider group with an explicit component-tree key.
pub fn durable_provider_tools_with_context_key<C, Props, D, Specs>(
    key: impl Into<StorageString>,
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    specs: Specs,
    instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
        + Send
        + Sync
        + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
    Specs: IntoIterator<Item = ProviderToolSpec>,
{
    durable_provider_tools_with_context(contract, prompt_contract, specs, instantiate_dispatcher)
        .key(key)
}

/// Single-tool convenience wrapper around [`durable_provider_tools`].
pub fn durable_provider_tool<C, Props, D>(
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    spec: ProviderToolSpec,
    instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
{
    durable_provider_tools(contract, prompt_contract, [spec], instantiate_dispatcher)
}

/// Single durable provider tool with an explicit component-tree key.
pub fn durable_provider_tool_with_key<C, Props, D>(
    key: impl Into<StorageString>,
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    spec: ProviderToolSpec,
    instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
{
    durable_provider_tool(contract, prompt_contract, spec, instantiate_dispatcher).key(key)
}

/// Context-aware single-tool convenience wrapper.
pub fn durable_provider_tool_with_context<C, Props, D>(
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    spec: ProviderToolSpec,
    instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
        + Send
        + Sync
        + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
{
    durable_provider_tools_with_context(contract, prompt_contract, [spec], instantiate_dispatcher)
}

/// Context-aware single durable provider tool with an explicit component-tree
/// key.
pub fn durable_provider_tool_with_context_key<C, Props, D>(
    key: impl Into<StorageString>,
    contract: RuntimeContract,
    prompt_contract: impl PomChildren,
    spec: ProviderToolSpec,
    instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
        + Send
        + Sync
        + 'static,
) -> DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
{
    durable_provider_tool_with_context(contract, prompt_contract, spec, instantiate_dispatcher)
        .key(key)
}

/// Single-tool convenience wrapper around [`provider_tools`].
pub fn provider_tool<C, Props, D>(
    key: impl Into<StorageString>,
    spec: ProviderToolSpec,
    instantiate_dispatcher: impl Fn() -> D + Send + Sync + 'static,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
{
    provider_tools(key, [spec], instantiate_dispatcher)
}

/// Single-tool convenience wrapper with exact prepared-turn context.
pub fn provider_tool_with_context<C, Props, D>(
    key: impl Into<StorageString>,
    spec: ProviderToolSpec,
    instantiate_dispatcher: impl for<'a> Fn(&ProviderDispatcherCx<'a, Props, C>) -> Result<D, ProviderDispatchFailure>
        + Send
        + Sync
        + 'static,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    D: ProviderDispatcher<C>,
{
    provider_tools_with_context(key, [spec], instantiate_dispatcher)
}

/// `View` extension for mapping every declaration in a hybrid component.
pub trait MountProvidedViewExt<Local, Props: ?Sized + 'static = ()>
where
    Local: TurnChannels,
{
    fn map_channels<Root>(
        self,
        map: impl ChannelMap<Local, Root>,
    ) -> MountProvidedView<Root, Props>
    where
        Root: TurnChannels;
}

impl<Local, Props> MountProvidedViewExt<Local, Props> for MountProvidedView<Local, Props>
where
    Local: TurnChannels,
    Props: ?Sized + 'static,
{
    fn map_channels<Root>(self, map: impl ChannelMap<Local, Root>) -> MountProvidedView<Root, Props>
    where
        Root: TurnChannels,
    {
        let map: SharedChannelMap<Local, Root> = Arc::new(map);
        self.map_binding(move |declaration| declaration.map_channels(Arc::clone(&map)))
    }
}

/// One mounted binding factory with stable component-tree identity.
pub struct MountedBindingFactory<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    id: BindingId,
    declaration: FactoryDeclaration<C, Props>,
}

impl<C, Props> Clone for MountedBindingFactory<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            declaration: self.declaration.clone(),
        }
    }
}

impl<C, Props> MountedBindingFactory<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn id(&self) -> &BindingId {
        &self.id
    }

    pub fn route(&self) -> &RuntimeRoute {
        self.declaration.route()
    }

    pub fn implementation_version(&self) -> Option<&str> {
        self.declaration.implementation_version()
    }

    pub(crate) fn declaration_id(&self) -> Option<&str> {
        self.declaration.declaration_id()
    }

    pub(crate) fn instantiate(
        &self,
        attempt: AttemptBindingCx<'_, Props>,
    ) -> Result<Box<dyn BindingInstance<C>>, BindingFault> {
        let context = TurnBindingCx::new(
            attempt.props,
            attempt.identity,
            &self.id,
            self.declaration.route(),
        );
        self.declaration.instantiate(context).map_err(|source| {
            BindingFault::new(
                attempt.identity,
                &self.id,
                self.declaration.route(),
                BindingPhase::Initialize,
                source,
            )
        })
    }
}

impl<C, Props> fmt::Debug for MountedBindingFactory<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedBindingFactory")
            .field("id", &self.id)
            .field("route", self.route())
            .finish_non_exhaustive()
    }
}

/// Ordered, heterogeneous factories collected from the System subtree.
#[derive(Debug)]
pub struct BindingFactoryPlan<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    factories: Vec<MountedBindingFactory<C, Props>>,
}

impl<C, Props> Clone for BindingFactoryPlan<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            factories: self.factories.clone(),
        }
    }
}

impl<C, Props> BindingFactoryPlan<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub(crate) fn empty() -> Self {
        Self {
            factories: Vec::new(),
        }
    }

    pub fn factories(&self) -> &[MountedBindingFactory<C, Props>] {
        &self.factories
    }

    pub fn len(&self) -> usize {
        self.factories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

/// One mounted provider capability with stable component-tree identity.
pub struct MountedProviderCapability<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    id: BindingId,
    declaration: ProviderCapabilityDeclaration<C, Props>,
}

impl<C, Props> Clone for MountedProviderCapability<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            declaration: self.declaration.clone(),
        }
    }
}

impl<C, Props> MountedProviderCapability<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn id(&self) -> &BindingId {
        &self.id
    }

    pub fn specs(&self) -> &[ProviderToolSpec] {
        self.declaration.specs()
    }

    pub fn implementation_version(&self) -> Option<&str> {
        self.declaration.implementation_version()
    }

    pub(crate) fn contract_version(&self) -> Option<&str> {
        self.declaration.contract_version()
    }

    pub(crate) fn host_implementation_version(&self) -> Option<&str> {
        self.declaration.host_implementation_version()
    }

    pub(crate) fn declaration_id(&self) -> Option<&str> {
        self.declaration.declaration_id()
    }

    pub(crate) fn instantiate_dispatcher(
        &self,
        attempt: AttemptProviderCx<'_, Props>,
    ) -> Result<Box<dyn ErasedProviderDispatcher<C>>, ProviderDispatchFailure> {
        self.declaration
            .instantiate_dispatcher(ProviderDispatcherCx::new(
                attempt.props,
                attempt.identity,
                &self.id,
                self.declaration.specs(),
            ))
    }
}

impl<C, Props> fmt::Debug for MountedProviderCapability<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedProviderCapability")
            .field("id", &self.id)
            .field("specs", &self.specs())
            .finish_non_exhaustive()
    }
}

/// Ordered provider capabilities collected from the System subtree.
#[derive(Debug)]
pub struct ProviderCapabilityPlan<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    capabilities: Vec<MountedProviderCapability<C, Props>>,
}

impl<C, Props> Clone for ProviderCapabilityPlan<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            capabilities: self.capabilities.clone(),
        }
    }
}

impl<C, Props> ProviderCapabilityPlan<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub(crate) fn empty() -> Self {
        Self {
            capabilities: Vec::new(),
        }
    }

    pub fn capabilities(&self) -> &[MountedProviderCapability<C, Props>] {
        &self.capabilities
    }

    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    fn bind_dispatchers(
        self,
        registry: &ProviderDispatcherRegistry<C, Props>,
    ) -> Result<Self, ProviderDispatcherRegistryError> {
        let mut capabilities = Vec::with_capacity(self.capabilities.len());
        for mut capability in self.capabilities {
            let declaration_id = capability.declaration_id();
            let registry_entry = declaration_id.and_then(|id| registry.entry(id));
            if capability.declaration.is_dispatcher_bound() {
                if let (Some(declaration_id), Some(_)) = (declaration_id, registry_entry) {
                    return Err(
                        ProviderDispatcherRegistryError::ConflictingEmbeddedBinding {
                            declaration_id: declaration_id.to_owned(),
                        },
                    );
                }
                capabilities.push(capability);
                continue;
            }

            let contract = capability.declaration.contract().ok_or_else(|| {
                ProviderDispatcherRegistryError::MissingBinding {
                    declaration_id: declaration_id
                        .unwrap_or("<non-durable-provider-capability>")
                        .to_owned(),
                }
            })?;
            let entry =
                registry_entry.ok_or_else(|| ProviderDispatcherRegistryError::MissingBinding {
                    declaration_id: contract.declaration_id().to_owned(),
                })?;
            if entry.contract.contract_version() != contract.contract_version() {
                return Err(ProviderDispatcherRegistryError::ContractVersionMismatch {
                    declaration_id: contract.declaration_id().to_owned(),
                    component_version: contract.contract_version().to_owned(),
                    host_version: entry.contract.contract_version().to_owned(),
                });
            }
            if entry.contract.specs() != contract.specs() {
                return Err(ProviderDispatcherRegistryError::ToolSchemaMismatch {
                    declaration_id: contract.declaration_id().to_owned(),
                });
            }
            capability.declaration = capability
                .declaration
                .with_dispatcher_binding(entry.binding.clone());
            capabilities.push(capability);
        }
        Ok(Self { capabilities })
    }
}

/// POM-free process-local registry used to rebuild one durable epoch runtime.
///
/// It contains only reducer factories, provider dispatch factories, routes,
/// schemas, and explicit durable identities. Its construction API cannot
/// accept a [`Component`], [`Document`], or lifecycle root.
pub(crate) struct RuntimeBindingRegistry<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    binding_factories: BindingFactoryPlan<C, Props>,
    provider_capabilities: ProviderCapabilityPlan<C, Props>,
}

impl<C, Props> Clone for RuntimeBindingRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            binding_factories: self.binding_factories.clone(),
            provider_capabilities: self.provider_capabilities.clone(),
        }
    }
}

impl<C, Props> RuntimeBindingRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub(crate) fn new() -> Self {
        Self {
            binding_factories: BindingFactoryPlan::empty(),
            provider_capabilities: ProviderCapabilityPlan::empty(),
        }
    }

    pub(crate) fn from_compiled_plans(
        binding_factories: BindingFactoryPlan<C, Props>,
        provider_capabilities: ProviderCapabilityPlan<C, Props>,
    ) -> Self {
        Self {
            binding_factories,
            provider_capabilities,
        }
    }

    pub(crate) fn binding_factories(&self) -> &BindingFactoryPlan<C, Props> {
        &self.binding_factories
    }

    pub(crate) fn provider_capabilities(&self) -> &ProviderCapabilityPlan<C, Props> {
        &self.provider_capabilities
    }

    pub(crate) fn into_plans(
        self,
    ) -> (
        BindingFactoryPlan<C, Props>,
        ProviderCapabilityPlan<C, Props>,
    ) {
        (self.binding_factories, self.provider_capabilities)
    }

    pub(crate) fn bind_provider_dispatchers(
        self,
        registry: &ProviderDispatcherRegistry<C, Props>,
    ) -> Result<Self, ProviderDispatcherRegistryError> {
        Ok(Self {
            binding_factories: self.binding_factories,
            provider_capabilities: self.provider_capabilities.bind_dispatchers(registry)?,
        })
    }
}

impl<C, Props> Default for RuntimeBindingRegistry<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RuntimeBindingRegistryError {
    #[error(transparent)]
    MountPlan(#[from] MountPlanError),
}

/// Parallel compiler output used to prove the future mounted epoch boundary.
#[derive(Debug)]
pub struct MountPlan<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    system: Document,
    user: Document,
    binding_factories: BindingFactoryPlan<C, Props>,
    provider_capabilities: ProviderCapabilityPlan<C, Props>,
}

impl<C, Props> MountPlan<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn system_document(&self) -> &Document {
        &self.system
    }

    pub fn user_document(&self) -> &Document {
        &self.user
    }

    pub fn binding_factories(&self) -> &BindingFactoryPlan<C, Props> {
        &self.binding_factories
    }

    pub fn provider_capabilities(&self) -> &ProviderCapabilityPlan<C, Props> {
        &self.provider_capabilities
    }

    pub fn into_parts(
        self,
    ) -> (
        Document,
        Document,
        BindingFactoryPlan<C, Props>,
        ProviderCapabilityPlan<C, Props>,
    ) {
        (
            self.system,
            self.user,
            self.binding_factories,
            self.provider_capabilities,
        )
    }
}

/// Mount-time validation failure. No factory or dispatcher has been created
/// when this error is returned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MountPlanError {
    #[error(transparent)]
    Component(#[from] ComponentError),

    #[error(
        "runtime declaration `{id}` must be inside the System subtree; placement was {role:?}"
    )]
    RuntimeDeclarationOutsideSystem {
        id: BindingId,
        role: Option<PromptRole>,
    },

    #[error("runtime declaration `{id}` in a durable System root must be authored by a durable leaf constructor")]
    NonDurableRuntimeDeclaration { id: BindingId },

    #[error(
        "durable runtime declaration `{declaration_id}` is declared by both `{first}` and `{second}`"
    )]
    DuplicateRuntimeDeclaration {
        declaration_id: StorageString,
        first: BindingId,
        second: BindingId,
    },

    #[error("runtime route `{route}` is declared by both `{first}` and `{second}`")]
    DuplicateBindingRoute {
        route: RuntimeRoute,
        first: BindingId,
        second: BindingId,
    },

    #[error("provider tool `{name}` is declared by both `{first}` and `{second}`")]
    DuplicateProviderTool {
        name: StorageString,
        first: BindingId,
        second: BindingId,
    },
}

/// Compile and validate a heterogeneous provided-component tree without
/// installing it into an agent runtime.
pub fn compile_mount_provided<C, Props>(
    view: impl super::experimental::IntoComponentNode<RuntimeDeclaration<C, Props>>,
) -> Result<MountPlan<C, Props>, MountPlanError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    compile_mount_provided_inner(view, false)
}

pub(crate) fn compile_durable_mount_provided<C, Props>(
    view: impl super::experimental::IntoComponentNode<RuntimeDeclaration<C, Props>>,
) -> Result<MountPlan<C, Props>, MountPlanError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    compile_mount_provided_inner(view, true)
}

fn compile_mount_provided_inner<C, Props>(
    view: impl super::experimental::IntoComponentNode<RuntimeDeclaration<C, Props>>,
    require_durable: bool,
) -> Result<MountPlan<C, Props>, MountPlanError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    let turn = compile_component(view)?;
    let (system, user, hooks) = turn.into_parts();
    let mut factories = Vec::new();
    let mut capabilities = Vec::new();
    let mut routes = HashMap::<RuntimeRoute, BindingId>::new();
    let mut tool_names = HashMap::<StorageString, BindingId>::new();
    let mut durable_declarations = HashMap::<StorageString, BindingId>::new();

    for mounted in hooks.into_bindings() {
        let (id, role, declaration) = mounted.into_placed_parts();
        if role != Some(PromptRole::System) {
            return Err(MountPlanError::RuntimeDeclarationOutsideSystem { id, role });
        }

        let durable_declaration_id = match &declaration {
            RuntimeDeclaration::BindingFactory(declaration) => declaration.declaration_id(),
            RuntimeDeclaration::ProviderCapability(declaration) => declaration.declaration_id(),
        };
        if require_durable && durable_declaration_id.is_none() {
            return Err(MountPlanError::NonDurableRuntimeDeclaration { id });
        }
        if let Some(declaration_id) = durable_declaration_id {
            let declaration_id = StorageString::from(declaration_id);
            if let Some(first) = durable_declarations.insert(declaration_id.clone(), id.clone()) {
                return Err(MountPlanError::DuplicateRuntimeDeclaration {
                    declaration_id,
                    first,
                    second: id,
                });
            }
        }

        match declaration {
            RuntimeDeclaration::BindingFactory(declaration) => {
                let route = declaration.route().clone();
                if let Some(first) = routes.insert(route.clone(), id.clone()) {
                    return Err(MountPlanError::DuplicateBindingRoute {
                        route,
                        first,
                        second: id,
                    });
                }
                factories.push(MountedBindingFactory { id, declaration });
            }
            RuntimeDeclaration::ProviderCapability(declaration) => {
                for spec in declaration.specs() {
                    let name = StorageString::from(spec.name());
                    if let Some(first) = tool_names.insert(name.clone(), id.clone()) {
                        return Err(MountPlanError::DuplicateProviderTool {
                            name,
                            first,
                            second: id,
                        });
                    }
                }
                capabilities.push(MountedProviderCapability { id, declaration });
            }
        }
    }

    Ok(MountPlan {
        system,
        user,
        binding_factories: BindingFactoryPlan { factories },
        provider_capabilities: ProviderCapabilityPlan { capabilities },
    })
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use serde_json::json;

    use super::*;
    use crate::{
        component::{durable_system, experimental::system, TurnChannelMap},
        pom::XmlNode,
        pom_renderer::render_pom_document,
        pom_resolution::resolve_system_document,
    };

    struct LocalChannels;

    impl TurnChannels for LocalChannels {
        type Output = u8;
        type Live = u8;
        type Commit = u8;
        type Diagnostic = u8;
    }

    struct RootChannels;

    impl TurnChannels for RootChannels {
        type Output = u16;
        type Live = u16;
        type Commit = u16;
        type Diagnostic = u16;
    }

    #[derive(Debug)]
    struct LocalBinding;

    impl BindingInstance<LocalChannels> for LocalBinding {}

    #[derive(Debug)]
    struct RootBinding;

    impl BindingInstance<RootChannels> for RootBinding {}

    #[derive(Debug)]
    struct MappingBinding;

    impl BindingInstance<LocalChannels> for MappingBinding {
        fn on_finish(
            &mut self,
        ) -> Result<StreamUpdate<TurnEmission<LocalChannels>, u8>, BindingFailure> {
            Ok(StreamUpdate::new()
                .with_emission(TurnEmission::Output(7))
                .with_emission(TurnEmission::Live(8))
                .with_emission(TurnEmission::Commit(9))
                .with_diagnostic(10))
        }
    }

    #[derive(Debug)]
    struct MappingDispatcher;

    #[async_trait::async_trait]
    impl ProviderDispatcher<LocalChannels> for MappingDispatcher {
        type Error = Infallible;

        async fn dispatch(
            &mut self,
            _context: &ProviderDispatchContext,
            _call: ProviderToolCall,
        ) -> Result<ProviderDispatchUpdate<LocalChannels>, Self::Error> {
            Ok(ProviderDispatchUpdate::new(
                ProviderToolResponse::success("mapped"),
                StreamUpdate::new()
                    .with_emission(TurnEmission::Output(1))
                    .with_emission(TurnEmission::Live(2))
                    .with_emission(TurnEmission::Commit(3))
                    .with_diagnostic(4),
            ))
        }

        async fn finish(
            &mut self,
        ) -> Result<StreamUpdate<TurnEmission<LocalChannels>, u8>, Self::Error> {
            Ok(StreamUpdate::new()
                .with_emission(TurnEmission::Output(5))
                .with_diagnostic(6))
        }
    }

    fn mapped_channels() -> TurnChannelMap<LocalChannels, RootChannels> {
        TurnChannelMap::new(
            |value| u16::from(value) + 100,
            |value| u16::from(value) + 200,
            |value| u16::from(value) + 300,
            |value| u16::from(value) + 400,
        )
    }

    fn assert_binding_update(update: StreamUpdate<TurnEmission<RootChannels>, u16>) {
        let (emissions, diagnostics) = update.into_parts();
        assert!(matches!(
            emissions.as_slice(),
            [
                TurnEmission::Output(107),
                TurnEmission::Live(208),
                TurnEmission::Commit(309),
            ]
        ));
        assert_eq!(diagnostics, [410]);
    }

    fn assert_dispatch_update(update: StreamUpdate<TurnEmission<RootChannels>, u16>) {
        let (emissions, diagnostics) = update.into_parts();
        assert!(matches!(
            emissions.as_slice(),
            [
                TurnEmission::Output(101),
                TurnEmission::Live(202),
                TurnEmission::Commit(303),
            ]
        ));
        assert_eq!(diagnostics, [404]);
    }

    fn assert_finish_update(update: StreamUpdate<TurnEmission<RootChannels>, u16>) {
        let (emissions, diagnostics) = update.into_parts();
        assert!(matches!(emissions.as_slice(), [TurnEmission::Output(105)]));
        assert_eq!(diagnostics, [406]);
    }

    async fn assert_mapped_runtime(
        factories: &BindingFactoryPlan<RootChannels>,
        capabilities: &ProviderCapabilityPlan<RootChannels>,
        attempt: &ProviderAttemptIdentity,
    ) {
        let props = ();
        let mut binding = factories.factories()[0]
            .instantiate(AttemptBindingCx {
                props: &props,
                identity: attempt,
            })
            .unwrap();
        assert_binding_update(binding.on_finish().unwrap());

        let capability = &capabilities.capabilities()[0];
        let mut dispatcher = capability
            .instantiate_dispatcher(AttemptProviderCx {
                props: &props,
                identity: attempt,
            })
            .unwrap();
        let context = ProviderDispatchContext::new(attempt, capability.id(), "mapped-call", 0);
        let dispatch = dispatcher
            .dispatch(
                &context,
                ProviderToolCall::new("mapped-call", "mapped_tool", json!({})),
            )
            .await
            .unwrap();
        assert_dispatch_update(dispatch.into_parts().1);
        assert_finish_update(dispatcher.finish().await.unwrap());
    }

    #[test]
    fn retained_durable_catalog_maps_component_and_rebind_projection_together() {
        let component = durable_binding_factory(
            RuntimeContract::new("test.local-binding", "v1").unwrap(),
            Document::from_xml(XmlNode::new(XmlName::try_from("local_binding").unwrap())),
            RuntimeRoute::xml("local_binding").unwrap(),
            || LocalBinding,
        );
        let catalog = DurableSystem::new([component])
            .unwrap()
            .map_channels(TurnChannelMap::new(
                u16::from,
                u16::from,
                u16::from,
                u16::from,
            ));

        let plan: MountPlan<RootChannels> =
            compile_durable_mount_provided(system(catalog.take_create_component())).unwrap();
        let rebound: RuntimeBindingRegistry<RootChannels> = catalog.runtime_projection().unwrap();

        assert_eq!(plan.binding_factories().len(), 1);
        assert_eq!(rebound.binding_factories().len(), 1);
        assert_eq!(
            plan.binding_factories().factories()[0].route(),
            rebound.binding_factories().factories()[0].route(),
        );
        assert_eq!(
            plan.binding_factories().factories()[0].declaration_id(),
            rebound.binding_factories().factories()[0].declaration_id(),
        );
        assert_eq!(
            plan.binding_factories().factories()[0].implementation_version(),
            rebound.binding_factories().factories()[0].implementation_version(),
        );
        assert_eq!(
            plan.binding_factories().factories()[0].id(),
            rebound.binding_factories().factories()[0].id(),
        );
    }

    #[test]
    fn durable_system_structural_children_preserve_author_order() {
        let first = durable_binding_factory(
            RuntimeContract::new("test.ordered-first", "v1").unwrap(),
            Document::from_xml(XmlNode::new(XmlName::try_from("first_leaf").unwrap())),
            RuntimeRoute::xml("first_leaf").unwrap(),
            || RootBinding,
        );
        let second = durable_binding_factory(
            RuntimeContract::new("test.ordered-second", "v1").unwrap(),
            Document::from_xml(XmlNode::new(XmlName::try_from("second_leaf").unwrap())),
            RuntimeRoute::xml("second_leaf").unwrap(),
            || RootBinding,
        );
        let catalog: DurableSystem<RootChannels> = durable_system((
            Document::from_xml(XmlNode::new(XmlName::try_from("root_rules").unwrap())),
            Some(Document::from_xml(XmlNode::new(
                XmlName::try_from("optional_rules").unwrap(),
            ))),
            vec![
                Document::from_xml(XmlNode::new(XmlName::try_from("vec_first").unwrap())),
                Document::from_xml(XmlNode::new(XmlName::try_from("vec_second").unwrap())),
            ],
            [
                Document::from_xml(XmlNode::new(XmlName::try_from("array_first").unwrap())),
                Document::from_xml(XmlNode::new(XmlName::try_from("array_second").unwrap())),
            ],
            durable_system((
                first,
                Document::from_xml(XmlNode::new(XmlName::try_from("nested_rules").unwrap())),
                second,
            )),
        ));

        let projection = catalog.runtime_projection().unwrap();
        let plan = compile_durable_mount_provided(system(catalog.take_create_component())).unwrap();

        assert_eq!(
            render_pom_document(&resolve_system_document(plan.system_document().clone())).unwrap(),
            concat!(
                "<root_rules />\n\n",
                "<optional_rules />\n\n",
                "<vec_first />\n\n",
                "<vec_second />\n\n",
                "<array_first />\n\n",
                "<array_second />\n\n",
                "<first_leaf />\n\n",
                "<nested_rules />\n\n",
                "<second_leaf />",
            ),
        );
        assert_eq!(
            projection
                .binding_factories()
                .factories()
                .iter()
                .map(|factory| factory.declaration_id())
                .collect::<Vec<_>>(),
            [Some("test.ordered-first"), Some("test.ordered-second")],
        );
        assert_eq!(
            plan.binding_factories()
                .factories()
                .iter()
                .map(|factory| factory.id())
                .collect::<Vec<_>>(),
            projection
                .binding_factories()
                .factories()
                .iter()
                .map(|factory| factory.id())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn retained_feature_scope_and_pom_component_scopes_match_after_reopen() {
        let durable_leaf = durable_binding_factory(
            RuntimeContract::new("test.scoped-runtime", "v1").unwrap(),
            Document::from_xml(XmlNode::new(XmlName::try_from("scoped_runtime").unwrap())),
            RuntimeRoute::xml("scoped_runtime").unwrap(),
            || RootBinding,
        );
        let preamble = PomView::with_component_scope(
            Document::from_xml(XmlNode::new(XmlName::try_from("preamble").unwrap()))
                .into_pom_view(),
            "test::preamble",
            None,
        );
        let mut feature_system = DurableSystem::new([durable_leaf])
            .unwrap()
            .with_component_scope("test::scoped-feature");
        feature_system
            .set_outer_component_key(ComponentKey::new("stable-feature").unwrap())
            .unwrap();
        let catalog: DurableSystem<RootChannels> = durable_system((preamble, feature_system));

        let created =
            compile_durable_mount_provided(system(catalog.take_create_component())).unwrap();
        let reopened = catalog.runtime_projection().unwrap();
        let created_id = created.binding_factories().factories()[0].id();
        let reopened_id = reopened.binding_factories().factories()[0].id();

        assert_eq!(created_id, reopened_id);
        assert!(created_id
            .component()
            .as_str()
            .contains("test::scoped-feature[stable-feature]"));
    }

    #[tokio::test]
    async fn retained_durable_catalog_maps_runtime_behavior_for_create_and_reopen() {
        let binding = durable_binding_factory(
            RuntimeContract::new("test.mapped-binding", "v1").unwrap(),
            Document::from_xml(XmlNode::new(XmlName::try_from("mapped_binding").unwrap())),
            RuntimeRoute::xml("mapped_binding").unwrap(),
            || MappingBinding,
        );
        let provider = durable_provider_tool(
            RuntimeContract::new("test.mapped-provider", "v1").unwrap(),
            (),
            ProviderToolSpec::new(
                "mapped_tool",
                "Emit all mapped channels",
                json!({ "type": "object", "properties": {} }),
            )
            .unwrap(),
            || MappingDispatcher,
        );
        let catalog = DurableSystem::new([binding, provider])
            .unwrap()
            .map_channels(mapped_channels());

        let created =
            compile_durable_mount_provided(system(catalog.take_create_component())).unwrap();
        let reopened = catalog.runtime_projection().unwrap();
        let attempt = ProviderAttemptIdentity::fresh(
            HarnessEpochId::fresh(),
            TurnInstanceId::fresh(),
            "mapped-runtime".into(),
        );

        assert_mapped_runtime(
            created.binding_factories(),
            created.provider_capabilities(),
            &attempt,
        )
        .await;
        assert_mapped_runtime(
            reopened.binding_factories(),
            reopened.provider_capabilities(),
            &attempt,
        )
        .await;
    }
}
