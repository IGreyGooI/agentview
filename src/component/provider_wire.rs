//! Fallible provider-wire contract for a mounted component epoch.
//!
//! This is deliberately separate from [`crate::llm_call::LLMExecutor`]. A
//! mounted provider needs the epoch's immutable tool schemas before it starts
//! a request, then needs a fallible, ordered ingress while the request is
//! active. The eventual mounted turn owner supplies the port; provider
//! adapters never receive reducer state, dispatcher factories, or lifecycle
//! typestates.
//!
//! The public contract does not expose its cancellation-token implementation.
//! On a terminal [`ProviderWireFault`], an adapter must stop its provider stream
//! and return promptly; on external cancellation, the mounted turn owner must
//! cancel and join the provider operation before completing attempt cleanup.

use std::{error::Error, sync::Arc};

use tokio::sync::watch;

use crate::{
    llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit, TextTurnEvent},
    StorageString,
};

use super::{
    durable_epoch::{
        AttachedProviderEpoch, DurableProviderEpoch, ProviderAdapterContract,
        ProviderEpochAttachRequest, ProviderEpochRehydrateRequest, ProviderTurnCursor,
    },
    HarnessEpochId, ProviderCapabilityPlan, ProviderOperationIdentity, ProviderToolCall,
    ProviderToolResult, ProviderToolSpec, TurnChannels,
};

/// Immutable provider-tool schema snapshot for one mounted System epoch.
///
/// The catalog contains only request-facing schema data. It intentionally does
/// not expose provider capability declarations or dispatcher factories, which
/// remain owned by the mounted epoch and are instantiated only for a final
/// prepared turn.
#[derive(Debug, Clone)]
pub struct ProviderToolCatalog {
    epoch_id: HarnessEpochId,
    specs: Arc<[ProviderToolSpec]>,
}

impl ProviderToolCatalog {
    /// Create an ordered tool snapshot for `epoch_id`.
    ///
    /// Callers supply the already flattened provider-request order. The mounted
    /// component compiler is responsible for validating globally unique tool
    /// names before it constructs this catalog.
    pub fn from_specs(
        epoch_id: HarnessEpochId,
        specs: impl IntoIterator<Item = ProviderToolSpec>,
    ) -> Self {
        Self {
            epoch_id,
            specs: Arc::from(specs.into_iter().collect::<Vec<_>>()),
        }
    }

    /// Snapshot every mounted provider capability without exposing its
    /// dispatcher factory to the provider adapter.
    pub fn from_capabilities<C, Props>(
        epoch_id: HarnessEpochId,
        capabilities: &ProviderCapabilityPlan<C, Props>,
    ) -> Self
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        Self::from_specs(
            epoch_id,
            capabilities
                .capabilities()
                .iter()
                .flat_map(|capability| capability.specs().iter().cloned()),
        )
    }

    /// The System epoch that owns these immutable schemas.
    pub fn epoch_id(&self) -> HarnessEpochId {
        self.epoch_id
    }

    /// Tool schemas in the exact provider-request order.
    pub fn specs(&self) -> &[ProviderToolSpec] {
        &self.specs
    }

    /// Iterate over tool schemas in provider-request order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ProviderToolSpec> + '_ {
        self.specs.iter()
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

/// Immutable System attachment for one ordinary provider epoch.
///
/// The mounted owner creates this value exactly once for a successful harness
/// epoch and gives it to [`MountedProviderEpochAttacher::attach_epoch`].
/// Ordinary provider requests intentionally cannot contain System bytes or
/// tool schemas: an adapter that needs a stateful provider conversation must
/// establish or select it from this attachment and reuse it for subsequent
/// turns.
#[derive(Debug, Clone)]
pub struct MountedProviderEpoch {
    system: Arc<str>,
    tools: ProviderToolCatalog,
}

impl MountedProviderEpoch {
    /// Create the immutable provider attachment for one mounted System epoch.
    ///
    /// The epoch identity is sourced from `tools`, which prevents a caller from
    /// accidentally pairing System bytes with another epoch's capability set.
    pub(crate) fn new(system: Arc<str>, tools: ProviderToolCatalog) -> Self {
        Self { system, tools }
    }

    /// The mounted harness epoch that owns this System attachment.
    pub fn epoch_id(&self) -> HarnessEpochId {
        self.tools.epoch_id()
    }

    /// Rendered System prompt bytes, available only during epoch attachment.
    pub fn system(&self) -> &str {
        &self.system
    }

    /// Immutable native-tool schemas for the same mounted epoch.
    pub fn tools(&self) -> &ProviderToolCatalog {
        &self.tools
    }

    /// Split the attachment when an adapter needs to retain its owned values.
    pub fn into_parts(self) -> (Arc<str>, ProviderToolCatalog) {
        (self.system, self.tools)
    }
}

/// One ordinary mounted provider request.
///
/// This type deliberately has no System field and no tool catalog. Both are
/// attached once through [`MountedProviderEpoch`] before any preparation or
/// execution begins. Keeping that absence in the data model prevents context
/// replacement, retry, and continuation code from silently sending a second
/// System prompt.
#[derive(Debug, Clone)]
pub struct MountedProviderRequest<I> {
    call_label: StorageString,
    history: Vec<I>,
    user: String,
    model: StorageString,
    max_tokens: u64,
    provider_cursor: Option<ProviderTurnCursor>,
    provider_operation: Option<ProviderOperationIdentity>,
}

impl<I> MountedProviderRequest<I> {
    pub(crate) fn new(
        call_label: impl Into<StorageString>,
        history: Vec<I>,
        user: String,
        model: impl Into<StorageString>,
        max_tokens: u64,
    ) -> Self {
        Self {
            call_label: call_label.into(),
            history,
            user,
            model: model.into(),
            max_tokens,
            provider_cursor: None,
            provider_operation: None,
        }
    }

    pub(crate) fn with_provider_cursor(mut self, cursor: Option<ProviderTurnCursor>) -> Self {
        self.provider_cursor = cursor;
        self
    }

    /// Bind the durable operation selected by the mounted call owner.
    ///
    /// This remains crate-private because normal component authors must not
    /// synthesize remote idempotency identities. Stateful provider adapters can
    /// only observe the identity through [`Self::provider_operation`].
    pub(crate) fn with_provider_operation(mut self, operation: ProviderOperationIdentity) -> Self {
        self.provider_operation = Some(operation);
        self
    }

    /// Diagnostic call label selected by the mounted owner.
    ///
    /// This is deliberately separate from [`Self::provider_operation`]: labels
    /// may be reused and must never be sent as a remote idempotency key.
    pub fn call_label(&self) -> &str {
        &self.call_label
    }

    /// Committed transcript selected for this ordinary provider turn.
    pub fn history(&self) -> &[I] {
        &self.history
    }

    /// Fully resolved User prompt for this ordinary provider turn.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Model identifier selected when the mounted owner opened.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Maximum output tokens selected by the mounted owner.
    pub fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    /// Last provider continuation cursor accepted with the durable session.
    pub fn provider_cursor(&self) -> Option<&ProviderTurnCursor> {
        self.provider_cursor.as_ref()
    }

    /// Durable idempotency identity for this provider execution, when the
    /// request came from a mounted durable owner.
    ///
    /// Compatibility and isolated lifecycle tests may intentionally have no
    /// such identity. A stateful production adapter should require it before
    /// sending the request to a remote session.
    pub fn provider_operation(&self) -> Option<&ProviderOperationIdentity> {
        self.provider_operation.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        StorageString,
        Vec<I>,
        String,
        StorageString,
        u64,
        Option<ProviderTurnCursor>,
        Option<ProviderOperationIdentity>,
    ) {
        (
            self.call_label,
            self.history,
            self.user,
            self.model,
            self.max_tokens,
            self.provider_cursor,
            self.provider_operation,
        )
    }
}

/// Provider-neutral inbound item for one active mounted attempt.
///
/// Text and native tool calls share this one sequence. A provider adapter must
/// submit each item only after the preceding acknowledgement has returned.
#[derive(Debug, Clone)]
pub enum ProviderWireEvent {
    Text(TextTurnEvent),
    Tool(ProviderToolCall),
}

/// Acknowledgement for one accepted [`ProviderWireEvent`].
#[derive(Debug, Clone)]
pub enum ProviderWireAck {
    TextAccepted,
    ToolResult {
        result: ProviderToolResult,
        replayed: bool,
    },
}

/// Terminal host failure while driving a mounted provider attempt.
///
/// Expected model/tool errors are represented by a successful
/// [`ProviderWireAck::ToolResult`] whose response is an error. Every value of
/// this type instead means that no further wire input may be submitted.
#[derive(Debug, thiserror::Error)]
#[error("mounted provider wire terminated: {source}")]
pub struct ProviderWireFault {
    #[source]
    source: Box<dyn Error + Send + Sync + 'static>,
}

/// Owner-selected reason for stopping one mounted provider operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCancellationReason {
    OwnerCancelled,
    DeadlineExceeded,
    RuntimeShutdown,
}

/// Provider-owned proof of how a cancelled operation may be resumed.
///
/// Joining the provider future does not prove that a stateful remote session
/// stayed at the cursor supplied in the request. The adapter must make that
/// disposition explicit. The mounted owner may terminally stop the durable
/// call only for `Unchanged` or `ResumeFrom`; `Indeterminate` requires durable
/// recovery and keeps successor calls fenced.
///
/// `ResumeFrom` is stronger than returning a cursor from a normal completion
/// that raced with cancellation. It promises that the next request can safely
/// use this cursor while the application session, User cursor, and effect
/// publication remain unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderCancellationCursorDisposition {
    /// The provider session did not advance beyond the request's input cursor.
    Unchanged,
    /// An authoritative cancellation-safe cursor for the next request.
    ResumeFrom(ProviderTurnCursor),
    /// The adapter cannot prove the provider session's post-cancel state.
    Indeterminate,
}

/// Remote provider's retained fact for one durable operation identity.
///
/// `NeverAccepted` is a strong negative proof, not a synonym for a missing
/// cache entry. A provider may return it only while its operation-retention
/// contract can prove that this identity was never accepted. Retention expiry,
/// transport ambiguity, and an unavailable operation ledger are `Unknown`.
///
/// `Completed` does not contain model output or replayable wire events. A
/// mounted recovery owner may use it only to resolve an already-persisted
/// publication candidate; without that local proof the call remains recovery
/// required.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderOperationStatus {
    NeverAccepted,
    Running,
    Completed,
    Cancelled {
        cursor: ProviderCancellationCursorDisposition,
    },
    Unknown,
}

/// Internal proof that a remote provider retained a strong negative result for
/// one durable operation identity.
///
/// A recovery controller can obtain this only from
/// [`ProviderOperationStatus::NeverAccepted`]. It is then consumed by the
/// store-backed checkpoint transition; an absent operation, retention expiry,
/// or any other remote status cannot be treated as permission to replay a
/// provider turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderOperationNeverAcceptedProof(());

impl ProviderOperationNeverAcceptedProof {
    pub(crate) fn from_status(status: &ProviderOperationStatus) -> Option<Self> {
        matches!(status, ProviderOperationStatus::NeverAccepted).then_some(Self(()))
    }
}

#[derive(Debug)]
struct ProviderCancellationState {
    reason: watch::Sender<Option<ProviderCancellationReason>>,
}

/// Owner half of one provider-operation cancellation scope.
///
/// Cancelling is one-shot: the first reason wins. The owner must continue
/// polling or join the matching executor future after calling [`cancel`](Self::cancel).
#[derive(Debug, Clone)]
pub struct ProviderCancellationSource {
    state: Arc<ProviderCancellationState>,
}

impl ProviderCancellationSource {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ProviderCancellationState {
                reason: watch::channel(None).0,
            }),
        }
    }

    pub fn token(&self) -> ProviderCancellationToken {
        ProviderCancellationToken {
            state: Arc::clone(&self.state),
        }
    }

    /// Signal cancellation. Returns `true` only for the first accepted reason.
    pub fn cancel(&self, reason: ProviderCancellationReason) -> bool {
        self.state.reason.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        })
    }

    pub fn reason(&self) -> Option<ProviderCancellationReason> {
        *self.state.reason.borrow()
    }
}

impl Default for ProviderCancellationSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapter half of one provider-operation cancellation scope.
#[derive(Debug, Clone)]
pub struct ProviderCancellationToken {
    state: Arc<ProviderCancellationState>,
}

impl ProviderCancellationToken {
    pub fn reason(&self) -> Option<ProviderCancellationReason> {
        *self.state.reason.borrow()
    }

    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }

    /// Wait until the mounted owner requests cancellation.
    pub async fn cancelled(&self) -> ProviderCancellationReason {
        let mut reason = self.state.reason.subscribe();
        loop {
            if let Some(reason) = *reason.borrow_and_update() {
                return reason;
            }
            reason
                .changed()
                .await
                .expect("the cancellation source is retained by the shared token state");
        }
    }
}

/// Joined terminal outcome of one mounted provider execution.
pub enum MountedProviderExit<I> {
    Completed {
        commit: ExecutorCommit<I>,
        cursor: Option<ProviderTurnCursor>,
    },
    Cancelled {
        reason: ProviderCancellationReason,
        cursor: ProviderCancellationCursorDisposition,
    },
}

impl<I> MountedProviderExit<I> {
    pub fn completed(commit: ExecutorCommit<I>) -> Self {
        Self::Completed {
            commit,
            cursor: None,
        }
    }

    /// Complete a stateful provider turn with its next durable continuation.
    pub fn completed_with_cursor(commit: ExecutorCommit<I>, cursor: ProviderTurnCursor) -> Self {
        Self::Completed {
            commit,
            cursor: Some(cursor),
        }
    }

    /// Report joined cancellation when the adapter cannot authoritatively say
    /// whether its durable provider cursor advanced.
    ///
    /// This is deliberately conservative: a mounted durable owner treats this
    /// as `Indeterminate` and enters recovery rather than releasing a
    /// successor call. Adapters that can prove the normal cancellation state
    /// must use [`Self::cancelled_without_cursor_change`] or
    /// [`Self::cancelled_with_resume_cursor`] instead.
    pub fn cancelled(reason: ProviderCancellationReason) -> Self {
        Self::Cancelled {
            reason,
            cursor: ProviderCancellationCursorDisposition::Indeterminate,
        }
    }

    /// Report joined cancellation and prove that the provider cursor did not
    /// advance. Stateless adapters use this after stopping all transport work.
    pub fn cancelled_without_cursor_change(reason: ProviderCancellationReason) -> Self {
        Self::Cancelled {
            reason,
            cursor: ProviderCancellationCursorDisposition::Unchanged,
        }
    }

    /// Report joined cancellation with an authoritative cursor from which a
    /// later request can safely resume without accepting this turn locally.
    pub fn cancelled_with_resume_cursor(
        reason: ProviderCancellationReason,
        cursor: ProviderTurnCursor,
    ) -> Self {
        Self::Cancelled {
            reason,
            cursor: ProviderCancellationCursorDisposition::ResumeFrom(cursor),
        }
    }

    pub fn cancellation_reason(&self) -> Option<ProviderCancellationReason> {
        match self {
            Self::Completed { .. } => None,
            Self::Cancelled { reason, .. } => Some(*reason),
        }
    }

    pub fn cancellation_cursor_disposition(
        &self,
    ) -> Option<&ProviderCancellationCursorDisposition> {
        match self {
            Self::Completed { .. } => None,
            Self::Cancelled { cursor, .. } => Some(cursor),
        }
    }

    pub fn into_commit(self) -> Result<ExecutorCommit<I>, ProviderCancellationReason> {
        match self {
            Self::Completed { commit, .. } => Ok(commit),
            Self::Cancelled { reason, .. } => Err(reason),
        }
    }

    pub fn into_completion(
        self,
    ) -> Result<(ExecutorCommit<I>, Option<ProviderTurnCursor>), ProviderCancellationReason> {
        match self {
            Self::Completed { commit, cursor } => Ok((commit, cursor)),
            Self::Cancelled { reason, .. } => Err(reason),
        }
    }
}

impl ProviderWireFault {
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }

    pub fn source_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.as_ref()
    }
}

/// Object-safe, fallible ingress for one active mounted provider attempt.
///
/// The mounted owner serializes this port with its attempt actor. A successful
/// tool acknowledgement is returned only after the component runtime accepted
/// the associated typed update; a terminal fault requires the adapter to stop
/// its provider operation immediately. The port has no `finish`, `publish`, or
/// `abort` method because those transitions remain owner responsibilities.
#[async_trait::async_trait]
pub trait FallibleProviderWirePort: Send {
    async fn submit(
        &mut self,
        event: ProviderWireEvent,
    ) -> Result<ProviderWireAck, ProviderWireFault>;
}

/// Provider execution boundary for a mounted System/User epoch.
///
/// This is intentionally not a compatibility extension of
/// [`crate::llm_call::LLMExecutor`]: the legacy executor has no mounted tool
/// catalog and its sink callbacks cannot report a terminal wire failure or
/// return a native tool result to the provider.
#[async_trait::async_trait]
pub trait MountedProviderExecutor<I>: Send + Sync
where
    I: Send + 'static,
{
    type Error: Error + Send + Sync + 'static;

    /// Provider-owned binding for one immutable mounted System epoch.
    ///
    /// An epoch binding must be safe to share with all ordinary turns from
    /// that epoch and must not expose the attached System bytes back through
    /// the per-turn request API.
    type Epoch: Send + Sync + 'static;

    /// Inspect the ordinary User/history request before the owner decides
    /// whether context is ready or history must be replaced.
    ///
    /// This pre-attempt operation must be read-only, side-effect free, and safe
    /// to cancel by dropping its future. The mounted owner applies a deadline by
    /// dropping the future; there is no cooperative cancellation token or join
    /// acknowledgement at this phase. Durable writes and externally visible
    /// work therefore belong in `execute` or later owner-controlled phases.
    async fn prepare_context(
        &self,
        epoch: &Self::Epoch,
        request: &MountedProviderRequest<I>,
        budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<I>, Self::Error>;

    /// Run one provider operation against the owner's active wire port.
    ///
    /// On [`ProviderWireFault`], implementations must stop their provider
    /// transport and return an error. For owner cancellation, implementations
    /// must race transport work with `cancellation.cancelled()`, stop transport,
    /// and return [`MountedProviderExit::Cancelled`] with the same reason. The
    /// owner must join this future before it performs acknowledged attempt abort.
    ///
    /// A cancellation exit is cursor-authoritative only when the adapter uses
    /// [`MountedProviderExit::cancelled_without_cursor_change`] or
    /// [`MountedProviderExit::cancelled_with_resume_cursor`]. The generic
    /// [`MountedProviderExit::cancelled`] constructor reports an indeterminate
    /// cursor and deliberately fences durable successors behind recovery.
    async fn execute(
        &self,
        epoch: &Self::Epoch,
        request: MountedProviderRequest<I>,
        wire: &mut dyn FallibleProviderWirePort,
        cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<I>, Self::Error>;
}

/// Ordinary, non-durable System attachment capability.
///
/// This is deliberately separate from [`DurableMountedProviderExecutor`]. A
/// durable provider must attach through a durable epoch id and artifact
/// fingerprint, so requiring it to expose this weaker one-shot path would
/// create an API that can only fail at runtime. One-shot compatibility owners
/// and explicit in-process reconfiguration use this trait; a stateful durable
/// adapter need not implement it at all.
pub trait MountedProviderEpochAttacher<I>: MountedProviderExecutor<I>
where
    I: Send + 'static,
{
    /// Attach the one logical System prompt and its native-tool schemas.
    ///
    /// This binding/factory boundary must not start a provider turn or emit
    /// model-visible output. Per-turn `prepare_context` and `execute` never
    /// receive System bytes directly.
    fn attach_epoch(&self, epoch: MountedProviderEpoch) -> Result<Self::Epoch, Self::Error>;
}

/// Durable System-epoch extension for a mounted provider adapter.
///
/// The ordinary [`MountedProviderExecutor`] boundary remains intentionally
/// unaware of persisted artifacts. An executor opts into this contract only
/// when it can attach an initial System with a durable idempotency key and
/// later rebuild its local binding from an opaque receipt. The rehydration
/// request has no System or tool accessor.
///
/// This is an advanced provider integration boundary. It does not expose the
/// mounted owner, persistence store, leases, revisions, or attempt actors.
#[async_trait::async_trait]
pub trait DurableMountedProviderExecutor<I>: MountedProviderExecutor<I>
where
    I: Send + 'static,
{
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract;

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error>;

    /// Rebind an existing remote epoch from its durable receipt and provider
    /// cursor, without receiving System or tool bytes.
    ///
    /// After process replacement, the first ordinary request may contain a
    /// full User document even when the previous request used POM deltas.
    /// Adapters must treat that full snapshot as synchronization within this
    /// same provider epoch/cursor lineage, not as another System attachment or
    /// an unrelated remote conversation. This proactive resync does not call
    /// `prepare_context` twice and does not consume the caller's preparation
    /// rewrite budget.
    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error>;
}

/// POM-free control plane for restart-stable remote provider operations.
///
/// This is an advanced host capability. The mounted owner supplies the exact
/// [`ProviderOperationIdentity`] reconstructed from authoritative durable
/// state; callers never provide System/User text, tools, a lease, or a store
/// revision. Returning from `cancel_operation` with `Cancelled` means the
/// remote operation has joined and its cursor disposition is authoritative.
/// Any ambiguous or expired remote state must remain `Unknown`.
#[async_trait::async_trait]
pub trait DurableMountedProviderOperationController<I>: DurableMountedProviderExecutor<I>
where
    I: Send + 'static,
{
    async fn inspect_operation(
        &self,
        epoch: &Self::Epoch,
        operation: &ProviderOperationIdentity,
    ) -> Result<ProviderOperationStatus, Self::Error>;

    async fn cancel_operation(
        &self,
        epoch: &Self::Epoch,
        operation: &ProviderOperationIdentity,
        reason: ProviderCancellationReason,
    ) -> Result<ProviderOperationStatus, Self::Error>;
}

/// Adapter from the mounted provider executor to the generic durable-epoch
/// coordinator. It is deliberately not exposed through ordinary turn APIs.
pub(crate) struct MountedDurableProvider<E, I>
where
    E: MountedProviderExecutor<I>,
    I: Send + 'static,
{
    executor: Arc<E>,
    _transcript: std::marker::PhantomData<fn(I)>,
}

impl<E, I> MountedDurableProvider<E, I>
where
    E: MountedProviderExecutor<I>,
    I: Send + 'static,
{
    pub(crate) fn new(executor: Arc<E>) -> Self {
        Self {
            executor,
            _transcript: std::marker::PhantomData,
        }
    }

    pub(crate) fn adapter_contract(&self) -> ProviderAdapterContract
    where
        E: DurableMountedProviderExecutor<I>,
    {
        self.executor.durable_provider_adapter_contract()
    }
}

#[async_trait::async_trait]
impl<E, I> DurableProviderEpoch for MountedDurableProvider<E, I>
where
    E: DurableMountedProviderExecutor<I> + 'static,
    I: Send + Sync + 'static,
{
    type Binding = E::Epoch;
    type Error = E::Error;

    async fn attach_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Binding>, Self::Error> {
        self.executor.attach_durable_epoch(request).await
    }

    async fn rehydrate_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Binding, Self::Error> {
        self.executor.rehydrate_durable_epoch(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::component::ProviderToolResponse;

    fn tool_spec(name: &str) -> ProviderToolSpec {
        ProviderToolSpec::new(
            name,
            format!("Run {name}"),
            json!({ "type": "object", "properties": {} }),
        )
        .unwrap()
    }

    #[test]
    fn catalog_is_an_immutable_ordered_snapshot() {
        let epoch_id = HarnessEpochId::fresh();
        let mut source = vec![tool_spec("inspect"), tool_spec("move")];
        let catalog = ProviderToolCatalog::from_specs(epoch_id, source.iter().cloned());
        let clone = catalog.clone();
        source.clear();

        assert_eq!(catalog.epoch_id(), epoch_id);
        assert_eq!(catalog.len(), 2);
        assert!(!catalog.is_empty());
        assert!(Arc::ptr_eq(&catalog.specs, &clone.specs));
        assert_eq!(
            catalog
                .iter()
                .map(ProviderToolSpec::name)
                .collect::<Vec<_>>(),
            ["inspect", "move"]
        );
        assert_eq!(
            clone
                .specs()
                .iter()
                .map(ProviderToolSpec::name)
                .collect::<Vec<_>>(),
            ["inspect", "move"]
        );
    }

    #[test]
    fn epoch_attachment_keeps_system_and_tools_out_of_an_ordinary_turn_request() {
        let epoch_id = HarnessEpochId::fresh();
        let attachment = MountedProviderEpoch::new(
            Arc::from("<policy>mounted once</policy>"),
            ProviderToolCatalog::from_specs(epoch_id, [tool_spec("inspect")]),
        );
        let request = MountedProviderRequest::new(
            "call-42",
            vec!["history".to_owned()],
            "<task>inspect</task>".to_owned(),
            "test-model",
            256,
        );

        assert_eq!(attachment.epoch_id(), epoch_id);
        assert_eq!(attachment.system(), "<policy>mounted once</policy>");
        assert_eq!(attachment.tools().specs()[0].name(), "inspect");
        assert_eq!(request.call_label(), "call-42");
        assert_eq!(request.history(), ["history"]);
        assert_eq!(request.user(), "<task>inspect</task>");
        assert_eq!(request.model(), "test-model");
        assert_eq!(request.max_tokens(), 256);
    }

    #[test]
    fn tool_ack_preserves_result_and_replay_marker() {
        let call = ProviderToolCall::new("call-7", "inspect", json!({}));
        let result = ProviderToolResult::new(
            &call,
            ProviderToolResponse::success(json!({ "status": "ready" })),
        );
        let acknowledgement = ProviderWireAck::ToolResult {
            result,
            replayed: true,
        };

        let ProviderWireAck::ToolResult { result, replayed } = acknowledgement else {
            panic!("tool acknowledgement must retain the tool result");
        };
        assert!(replayed);
        assert_eq!(result.invocation_id(), Some("call-7"));
        assert_eq!(result.name(), "inspect");
        assert!(!result.is_error());
    }

    #[tokio::test]
    async fn provider_cancellation_is_one_shot_and_awaitable() {
        let source = ProviderCancellationSource::new();
        let token = source.token();
        let waiter = tokio::spawn(async move { token.cancelled().await });

        assert!(source.cancel(ProviderCancellationReason::DeadlineExceeded));
        assert!(!source.cancel(ProviderCancellationReason::RuntimeShutdown));
        assert_eq!(
            waiter.await.unwrap(),
            ProviderCancellationReason::DeadlineExceeded
        );
        assert_eq!(
            source.reason(),
            Some(ProviderCancellationReason::DeadlineExceeded)
        );
    }

    #[tokio::test]
    async fn cancellation_before_the_waiter_is_polled_is_retained() {
        let source = ProviderCancellationSource::new();
        let token = source.token();
        let waiter = token.cancelled();

        assert!(source.cancel(ProviderCancellationReason::OwnerCancelled));
        assert_eq!(waiter.await, ProviderCancellationReason::OwnerCancelled);
    }

    #[tokio::test]
    async fn cancellation_wakes_every_token_waiter() {
        let source = ProviderCancellationSource::new();
        let waiters = (0..8)
            .map(|_| {
                let token = source.token();
                tokio::spawn(async move { token.cancelled().await })
            })
            .collect::<Vec<_>>();

        tokio::task::yield_now().await;
        assert!(source.cancel(ProviderCancellationReason::RuntimeShutdown));

        for waiter in waiters {
            assert_eq!(
                waiter.await.unwrap(),
                ProviderCancellationReason::RuntimeShutdown
            );
        }
    }

    #[test]
    fn cancelled_exit_is_conservative_unless_the_adapter_proves_cursor_unchanged() {
        let indeterminate =
            MountedProviderExit::<String>::cancelled(ProviderCancellationReason::OwnerCancelled);
        assert_eq!(
            indeterminate.cancellation_cursor_disposition(),
            Some(&ProviderCancellationCursorDisposition::Indeterminate)
        );
        assert!(matches!(
            indeterminate.into_completion(),
            Err(ProviderCancellationReason::OwnerCancelled)
        ));

        let unchanged = MountedProviderExit::<String>::cancelled_without_cursor_change(
            ProviderCancellationReason::DeadlineExceeded,
        );
        assert_eq!(
            unchanged.cancellation_cursor_disposition(),
            Some(&ProviderCancellationCursorDisposition::Unchanged)
        );
        assert!(matches!(
            unchanged.into_completion(),
            Err(ProviderCancellationReason::DeadlineExceeded)
        ));

        let resume_cursor: ProviderTurnCursor = serde_json::from_value(json!({
            "adapter": "test-provider",
            "schema_version": 1,
            "durable_epoch_id": "test/session/epoch-1",
            "artifact_fingerprint": format!("sha256:{}", "0".repeat(64)),
            "value": { "remote_turn": 7 },
        }))
        .expect("the persisted cursor fixture matches the public cursor shape");
        let resumable = MountedProviderExit::<String>::cancelled_with_resume_cursor(
            ProviderCancellationReason::RuntimeShutdown,
            resume_cursor.clone(),
        );
        assert_eq!(
            resumable.cancellation_cursor_disposition(),
            Some(&ProviderCancellationCursorDisposition::ResumeFrom(
                resume_cursor
            ))
        );
        assert!(matches!(
            resumable.into_completion(),
            Err(ProviderCancellationReason::RuntimeShutdown)
        ));
    }
}
