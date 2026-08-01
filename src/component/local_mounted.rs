//! AgentView-owned process-local host for the mounted component runtime.

use std::{
    collections::hash_map::DefaultHasher,
    convert::Infallible,
    error::Error,
    fmt,
    future::Future,
    hash::{Hash, Hasher},
    marker::PhantomData,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    agent::TurnFlow, agent_session::AgentSession, llm_call::ExecutorCommit,
    pom_cursor::UserDocumentCursor, StorageString,
};

use super::{
    attempt_driver::{DurableStreamingPublicationFinalizer, SuppliedPublicationPlanFactory},
    durable_epoch::{
        ActiveEpochArtifact, DurableEpochStore, EpochContractManifest, EpochOpenAdmission,
        EpochOpenFence, EpochOpenLease, EpochOpenRecoveryPhase, EpochOpenRequest,
        ProviderTurnCursor, RenderedEpochArtifact,
    },
    durable_host::{
        DurableMountedStateBackend, DurableOutboxFingerprint, MountedStateBlob,
        MountedStateGeneration, MountedStateSnapshot, MountedStateWrite, MountedStateWriteOutcome,
    },
    host_binding::{LiveEffectRuntimeBindingContext, LiveEffectRuntimeFactory},
    managed_attempt::{
        start_managed_streaming_attempt_with_durable_finalizer_and_identity_on,
        ManagedActiveAttempt,
    },
    mounted::{
        advanced::{MountedAgentDriver, MountedAgentFactory},
        CapturedMountedHarnessDefinition, DurableEpochDefinition, MountedAgent,
        MountedHostBindings, MountedOpenError, MountedTurnCapture,
    },
    mounted_agent::{
        open_captured_bound_agent, MountedAgentBinding, MountedAttemptBindingContext,
        MountedCallAbandon, MountedCallCancellationDisposition, MountedCallCancellationSettlement,
        MountedCallCancellationSettlementRequest, MountedCallClaim, MountedCallClaimRequest,
        MountedCallLeaseRequest, MountedCallNeverAcceptedRecovery,
        MountedCallNeverAcceptedRecoveryRequest, MountedCallReconciliationAdmission,
        MountedCallReconciliationClaim, MountedCallReconciliationClaimRequest,
        MountedCallReconciliationRelease, MountedCallReconciliationReleaseRequest,
        MountedCallStart, MountedEpochReconfigurationRejectOutcome,
        MountedEpochReconfigurationRejectRequest, MountedEpochReconfigurationSnapshot,
        MountedEpochReconfigureAdmission, MountedEpochReconfigureRecoveryOutcome,
        MountedEpochReconfigureRecoveryRequest, MountedEpochReconfigureRequest,
        MountedMutationIdentity, MountedOwnerStore, MountedPendingPublicationReconciliation,
        MountedRuntime, MountedRuntimePolicy, MountedSessionMutation,
        MountedSessionPublicationError, MountedSessionPublicationStore,
        MountedSessionReductionFailure, MountedSessionStore,
    },
    mounted_contract::{
        ClaimedCallPublication, DurableCallLedger, DurableCallPreProviderCheckpoint,
        DurableCallRecoveryReason, DurableCallReservationOrigin, DurableCallState,
        DurableCallStatus, MountedCallReconciliationFence, MountedSessionSnapshot,
        SessionReduceContext, SessionReducer, StoredCallResult, TurnRecordInterpreter,
    },
    provision::ProviderDispatcherRegistry,
    publication::{
        CommitStager, CommitStagingContext, DurableCallLeaseId, PublicationCandidateFingerprint,
        PublicationFingerprintContext, PublicationFingerprintFactory, PublicationId,
        PublicationReceipt, PublicationRequest, PublicationRequestContext, PublicationRequestId,
        PublicationResolution, PublicationResolveError, PublicationStagingPlan, PublicationStore,
        PublicationWriteError, StagedCommit, StagedOutbox,
    },
    DurableSessionId, EpochContractId, Never, TurnChannels,
};
use super::{provider_wire::DurableMountedProviderExecutor, DurableKeyError};

const LOCAL_LEASE_DURATION: Duration = Duration::from_secs(60);
const DEFAULT_MOUNTED_CONTEXT_PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MOUNTED_PROVIDER_CANCELLATION_GRACE: Duration = Duration::from_secs(5);
// Schema v3 adds the committed User POM cursor to the durable envelope. This
// keeps a replacement owner on the same acknowledged delta lineage as the
// provider conversation. V1 remains rejected before candidate recomputation;
// silently dual-reading it could turn an unresolved write into a false replay.
const DURABLE_STATE_SCHEMA_VERSION: u32 = 3;
const DURABLE_PUBLICATION_FINGERPRINT_SCHEMA: &str = "agentview.durable-mounted-publication.v2";
const DURABLE_STATE_MAX_CAS_RETRIES: usize = 16;
// This host has one process-local store per factory, so these identify the
// local host implementation rather than an application-selected durable
// provider configuration. A production binding must derive its own values
// from its provider, persistence, reducer, and Live-runtime configuration.
const LOCAL_HOST_CONFIGURATION_FINGERPRINT: &str = "agentview.in-memory-mounted-host/v1";
const LOCAL_RUNTIME_IMPLEMENTATION_FINGERPRINT: &str = "agentview.in-memory-mounted-runtime/v1";

/// Failure while constructing the process-local mounted host.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InMemoryMountedFactoryError {
    #[error("failed to create the mounted owner runtime: {0}")]
    Runtime(#[source] std::io::Error),
}

/// A caller-owned Tokio runtime for one or more mounted host factories.
///
/// This is an advanced host-integration boundary, not a component-authoring
/// primitive. Construct it from the application's existing Tokio
/// [`tokio::runtime::Handle`] and pass clones to
/// [`InMemoryMountedAgentFactory::new_with_runtime`] or
/// [`DurableMountedAgentFactory::new_with_runtime`]. AgentView borrows the
/// runtime only: it never shuts it down. The application must keep the
/// corresponding [`tokio::runtime::Runtime`] alive until every factory, agent,
/// and detached mounted call using this value has finished. A current-thread
/// runtime must also remain driven for that whole interval.
#[derive(Clone)]
pub struct MountedHostRuntime {
    handle: tokio::runtime::Handle,
}

impl MountedHostRuntime {
    /// Borrow an already-running application Tokio runtime.
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self { handle }
    }

    fn into_mounted_runtime(self) -> MountedRuntime {
        MountedRuntime::from_handle(self.handle, default_mounted_runtime_policy())
    }
}

fn default_mounted_runtime_policy() -> MountedRuntimePolicy {
    MountedRuntimePolicy::new(
        DEFAULT_MOUNTED_CONTEXT_PREPARATION_TIMEOUT,
        DEFAULT_MOUNTED_PROVIDER_CANCELLATION_GRACE,
    )
}

/// AgentView-owned mounted host backed by one process-local transactional store.
///
/// Cloning or reusing one factory value preserves the same session, call
/// ledger, provider cursor, and active System epoch. That factory identity,
/// rather than [`DurableSessionId`] alone, scopes the System-once and replay
/// guarantee: each call to [`Self::new`] intentionally creates a fresh local
/// store, even if it receives an equal session id. Use [`Clone::clone`] to hand
/// the same local host to another owner or to model a drop/reopen.
///
/// This supports drop/reopen within one process but intentionally makes no
/// cross-process durability claim or session-id registry guarantee. The
/// provider, pure session reducer, and per-attempt Live runtime remain typed;
/// AgentView owns System admission, call fencing, publication, and replay.
pub struct InMemoryMountedAgentFactory<C, I, ContextState, Executor, Reducer, LiveFactory>
where
    C: TurnChannels<Commit = Never>,
{
    runtime: MountedRuntime,
    executor: Arc<Executor>,
    session_id: DurableSessionId,
    store: Arc<InMemoryMountedStore<I, ContextState>>,
    initial_session: AgentSession<I, ContextState>,
    reducer: Arc<Reducer>,
    live_factory: Arc<LiveFactory>,
    model: StorageString,
    max_tokens: u64,
    host_configuration_fingerprint: StorageString,
    runtime_implementation_fingerprint: StorageString,
    contract: PhantomData<fn() -> C>,
}

impl<C, I, ContextState, Executor, Reducer, LiveFactory> Clone
    for InMemoryMountedAgentFactory<C, I, ContextState, Executor, Reducer, LiveFactory>
where
    C: TurnChannels<Commit = Never>,
    I: Clone,
    ContextState: Clone,
{
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime.clone(),
            executor: Arc::clone(&self.executor),
            session_id: self.session_id.clone(),
            store: Arc::clone(&self.store),
            initial_session: self.initial_session.clone(),
            reducer: Arc::clone(&self.reducer),
            live_factory: Arc::clone(&self.live_factory),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            host_configuration_fingerprint: self.host_configuration_fingerprint.clone(),
            runtime_implementation_fingerprint: self.runtime_implementation_fingerprint.clone(),
            contract: PhantomData,
        }
    }
}

impl<C, I, ContextState, Executor, Reducer, LiveFactory>
    InMemoryMountedAgentFactory<C, I, ContextState, Executor, Reducer, LiveFactory>
where
    C: TurnChannels<Commit = Never>,
{
    /// Create one new, isolated process-local mounted host.
    ///
    /// Reuse or clone the returned value for every owner that represents this
    /// local durable epoch. Constructing a second factory with the same
    /// [`DurableSessionId`] starts a separate in-memory store; it is not a
    /// reopen operation and may create its own System epoch.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: DurableSessionId,
        initial_session: AgentSession<I, ContextState>,
        executor: Arc<Executor>,
        model: impl Into<StorageString>,
        max_tokens: u64,
        reducer: Reducer,
        live_factory: LiveFactory,
    ) -> Result<Self, InMemoryMountedFactoryError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(InMemoryMountedFactoryError::Runtime)?;
        let runtime = MountedRuntime::new(runtime, default_mounted_runtime_policy())
            .map_err(InMemoryMountedFactoryError::Runtime)?;
        Ok(Self::from_runtime(
            runtime,
            session_id,
            initial_session,
            executor,
            model.into(),
            max_tokens,
            reducer,
            live_factory,
        ))
    }

    /// Create one new process-local mounted host on an application-owned runtime.
    ///
    /// Unlike [`Self::new`], this constructor does not create a Tokio runtime
    /// or a keeper thread. Clones of `runtime` may be passed to independent
    /// factories when the application owns their scheduling and shutdown.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_runtime(
        runtime: MountedHostRuntime,
        session_id: DurableSessionId,
        initial_session: AgentSession<I, ContextState>,
        executor: Arc<Executor>,
        model: impl Into<StorageString>,
        max_tokens: u64,
        reducer: Reducer,
        live_factory: LiveFactory,
    ) -> Self {
        Self::from_runtime(
            runtime.into_mounted_runtime(),
            session_id,
            initial_session,
            executor,
            model.into(),
            max_tokens,
            reducer,
            live_factory,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_runtime(
        runtime: MountedRuntime,
        session_id: DurableSessionId,
        initial_session: AgentSession<I, ContextState>,
        executor: Arc<Executor>,
        model: StorageString,
        max_tokens: u64,
        reducer: Reducer,
        live_factory: LiveFactory,
    ) -> Self {
        Self {
            runtime,
            executor,
            session_id,
            store: Arc::new(InMemoryMountedStore::default()),
            initial_session,
            reducer: Arc::new(reducer),
            live_factory: Arc::new(live_factory),
            model,
            max_tokens,
            host_configuration_fingerprint: LOCAL_HOST_CONFIGURATION_FINGERPRINT.into(),
            runtime_implementation_fingerprint: LOCAL_RUNTIME_IMPLEMENTATION_FINGERPRINT.into(),
            contract: PhantomData,
        }
    }

    /// Open or reopen this process-local mounted owner.
    ///
    /// Reusing this factory or one of its clones preserves its active System
    /// epoch, session, call ledger, provider cursor, and replay results. The
    /// supplied definition is still checked on every open; a different epoch
    /// contract is rejected rather than rendering a second System prompt.
    pub async fn open<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
    ) -> Result<MountedAgent<C, Capture::CallProps, Capture::Source>, MountedOpenError>
    where
        I: Clone + Send + Sync + 'static,
        ContextState: Clone + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        self.open_with_bindings(definition, MountedHostBindings::new())
            .await
    }

    /// Open or reopen with explicit host-owned runtime bindings.
    ///
    /// This is the normal host integration entry point for any pure provided
    /// component contract. Bindings are validated before durable admission can
    /// render or attach System and are never serialized into the epoch.
    pub async fn open_with_bindings<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        bindings: MountedHostBindings<C, Capture::TurnProps>,
    ) -> Result<MountedAgent<C, Capture::CallProps, Capture::Source>, MountedOpenError>
    where
        I: Clone + Send + Sync + 'static,
        ContextState: Clone + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        MountedAgent::open_with_bindings(self, definition, bindings).await
    }

    /// Open or reopen with host-owned dispatcher bindings for pure provider contracts.
    ///
    /// The registry is validated before durable admission can render or attach
    /// System. Its contract and host implementation versions become part of
    /// the durable epoch manifest. Existing embedded compatibility dispatchers
    /// remain available through [`Self::open`].
    pub async fn open_with_provider_dispatchers<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        provider_dispatchers: ProviderDispatcherRegistry<C, Capture::TurnProps>,
    ) -> Result<MountedAgent<C, Capture::CallProps, Capture::Source>, MountedOpenError>
    where
        I: Clone + Send + Sync + 'static,
        ContextState: Clone + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        self.open_with_bindings(
            definition,
            MountedHostBindings::with_provider_dispatchers(provider_dispatchers),
        )
        .await
    }

    async fn open_driver_with_provider_dispatchers<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        provider_dispatchers: ProviderDispatcherRegistry<C, Capture::TurnProps>,
    ) -> Result<Arc<dyn MountedAgentDriver<C, Capture::CallProps, Capture::Source>>, MountedOpenError>
    where
        I: Clone + Send + Sync + 'static,
        ContextState: Clone + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        let epoch_definition = definition.epoch_arc();
        let binding = InMemoryMountedBinding {
            session_id: self.session_id.clone(),
            store: Arc::clone(&self.store),
            initial_session: self.initial_session.clone(),
            reducer: Arc::clone(&self.reducer),
            live_factory: Arc::clone(&self.live_factory),
            provider_dispatchers,
            epoch_definition,
            host_configuration_fingerprint: self.host_configuration_fingerprint.clone(),
            runtime_implementation_fingerprint: self.runtime_implementation_fingerprint.clone(),
        };
        let agent = open_captured_bound_agent::<C, Capture, Executor, I, u64, _>(
            self.runtime.clone(),
            definition,
            Arc::clone(&self.executor),
            self.model.clone(),
            self.max_tokens,
            binding,
        )
        .await?;
        Ok(agent.into_driver())
    }
}

impl<C, I, ContextState, Executor, Reducer, LiveFactory> fmt::Debug
    for InMemoryMountedAgentFactory<C, I, ContextState, Executor, Reducer, LiveFactory>
where
    C: TurnChannels<Commit = Never>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryMountedAgentFactory")
            .field("session_id", &self.session_id)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<C, I, ContextState, Executor, Reducer, LiveFactory, Capture> MountedAgentFactory<C, Capture, I>
    for InMemoryMountedAgentFactory<C, I, ContextState, Executor, Reducer, LiveFactory>
where
    C: TurnChannels<Commit = Never>,
    I: Clone + Send + Sync + 'static,
    ContextState: Clone + Send + Sync + 'static,
    Executor: DurableMountedProviderExecutor<I> + 'static,
    Reducer: SessionReducer<I, ContextState, C>,
    LiveFactory:
        LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
    Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
{
    async fn open(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        bindings: MountedHostBindings<C, Capture::TurnProps>,
    ) -> Result<Arc<dyn MountedAgentDriver<C, Capture::CallProps, Capture::Source>>, MountedOpenError>
    {
        self.open_driver_with_provider_dispatchers(definition, bindings.into_provider_dispatchers())
            .await
    }
}

struct InMemoryMountedBinding<C, I, ContextState, Props, Reducer, LiveFactory>
where
    C: TurnChannels<Commit = Never>,
    Props: ?Sized + 'static,
{
    session_id: DurableSessionId,
    store: Arc<InMemoryMountedStore<I, ContextState>>,
    initial_session: AgentSession<I, ContextState>,
    reducer: Arc<Reducer>,
    live_factory: Arc<LiveFactory>,
    provider_dispatchers: ProviderDispatcherRegistry<C, Props>,
    epoch_definition: Arc<DurableEpochDefinition<C, Props>>,
    host_configuration_fingerprint: StorageString,
    runtime_implementation_fingerprint: StorageString,
}

#[derive(Debug, thiserror::Error)]
enum InMemoryMountedBindingError {
    #[error("mounted host {phase} failed: {source}")]
    Phase {
        phase: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl InMemoryMountedBindingError {
    fn phase(phase: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Phase {
            phase,
            source: Box::new(source),
        }
    }
}

/// Immutable host policy that participates in a durable epoch contract.
///
/// These values are deliberately chosen by the application integration rather
/// than inferred from Rust type names. A change can alter reducer or Live
/// behavior even when the POM itself is unchanged, so reopening a session with
/// another value rejects the epoch instead of silently changing its runtime.
#[derive(Debug, Clone)]
pub struct DurableMountedHostConfig {
    host_configuration_fingerprint: StorageString,
    runtime_binder_fingerprint: StorageString,
    lease_duration: Duration,
}

impl DurableMountedHostConfig {
    /// Create durable host policy with the standard sixty-second call lease.
    pub fn new(
        host_configuration_fingerprint: impl Into<StorageString>,
        runtime_binder_fingerprint: impl Into<StorageString>,
    ) -> Self {
        Self {
            host_configuration_fingerprint: host_configuration_fingerprint.into(),
            runtime_binder_fingerprint: runtime_binder_fingerprint.into(),
            lease_duration: LOCAL_LEASE_DURATION,
        }
    }

    /// Replace the default call lease duration.
    pub fn with_lease_duration(
        mut self,
        lease_duration: Duration,
    ) -> Result<Self, DurableMountedHostConfigError> {
        if lease_duration.is_zero() {
            return Err(DurableMountedHostConfigError::ZeroLeaseDuration);
        }
        self.lease_duration = lease_duration;
        Ok(self)
    }
}

/// Invalid durable mounted-host policy.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DurableMountedHostConfigError {
    #[error("durable mounted host lease duration must be greater than zero")]
    ZeroLeaseDuration,
}

/// Failure while constructing the cross-process mounted host facade.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DurableMountedFactoryError {
    #[error("failed to create the mounted owner runtime: {0}")]
    Runtime(#[source] std::io::Error),
}

/// AgentView-owned mounted host backed by an application-provided CAS/outbox backend.
///
/// This is the advanced production counterpart to
/// [`InMemoryMountedAgentFactory`]. It keeps the durable owner, leases,
/// provider cursor, and state codec inside AgentView. Integrators provide only
/// the remote provider, opaque backend, pure session reducer, Live-runtime
/// factory, and pure Commit staging/fingerprinting policies.
///
/// Reconstructing this factory with the same `session_id` and `backend` is a
/// reopen operation: AgentView loads the durable state, rehydrates the provider
/// from its persisted receipt/cursor, and does not render or send System again.
pub struct DurableMountedAgentFactory<
    C,
    I,
    ContextState,
    Executor,
    Reducer,
    LiveFactory,
    Backend,
    Stager,
    Fingerprint,
> where
    C: TurnChannels,
    Stager: CommitStager<C>,
{
    runtime: MountedRuntime,
    executor: Arc<Executor>,
    backend: Arc<Backend>,
    session_id: DurableSessionId,
    initial_session: AgentSession<I, ContextState>,
    reducer: Arc<Reducer>,
    live_factory: Arc<LiveFactory>,
    commit_stager: Arc<Stager>,
    fingerprint_factory: Arc<Fingerprint>,
    model: StorageString,
    max_tokens: u64,
    host_config: DurableMountedHostConfig,
    contract: PhantomData<fn() -> C>,
}

impl<C, I, ContextState, Executor, Reducer, LiveFactory, Backend, Stager, Fingerprint> Clone
    for DurableMountedAgentFactory<
        C,
        I,
        ContextState,
        Executor,
        Reducer,
        LiveFactory,
        Backend,
        Stager,
        Fingerprint,
    >
where
    C: TurnChannels,
    I: Clone,
    ContextState: Clone,
    Stager: CommitStager<C>,
{
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime.clone(),
            executor: Arc::clone(&self.executor),
            backend: Arc::clone(&self.backend),
            session_id: self.session_id.clone(),
            initial_session: self.initial_session.clone(),
            reducer: Arc::clone(&self.reducer),
            live_factory: Arc::clone(&self.live_factory),
            commit_stager: Arc::clone(&self.commit_stager),
            fingerprint_factory: Arc::clone(&self.fingerprint_factory),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            host_config: self.host_config.clone(),
            contract: PhantomData,
        }
    }
}

impl<C, I, ContextState, Executor, Reducer, LiveFactory, Backend, Stager, Fingerprint>
    DurableMountedAgentFactory<
        C,
        I,
        ContextState,
        Executor,
        Reducer,
        LiveFactory,
        Backend,
        Stager,
        Fingerprint,
    >
where
    C: TurnChannels,
    Stager: CommitStager<C>,
{
    /// Create a factory for one durable session identity.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: DurableSessionId,
        backend: Arc<Backend>,
        initial_session: AgentSession<I, ContextState>,
        executor: Arc<Executor>,
        model: impl Into<StorageString>,
        max_tokens: u64,
        host_config: DurableMountedHostConfig,
        reducer: Reducer,
        live_factory: LiveFactory,
        commit_stager: Stager,
        fingerprint_factory: Fingerprint,
    ) -> Result<Self, DurableMountedFactoryError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(DurableMountedFactoryError::Runtime)?;
        let runtime = MountedRuntime::new(runtime, default_mounted_runtime_policy())
            .map_err(DurableMountedFactoryError::Runtime)?;
        Ok(Self::from_runtime(
            runtime,
            session_id,
            backend,
            initial_session,
            executor,
            model.into(),
            max_tokens,
            host_config,
            reducer,
            live_factory,
            commit_stager,
            fingerprint_factory,
        ))
    }

    /// Create a durable factory on an application-owned Tokio runtime.
    ///
    /// The caller may clone one [`MountedHostRuntime`] into many factories.
    /// AgentView schedules mounted owners and detached calls on that runtime,
    /// but never owns or shuts it down. This is the caller-owned runtime path;
    /// the compatibility [`Self::new`] constructor remains available for local
    /// examples and tests that want an AgentView-owned runtime.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_runtime(
        runtime: MountedHostRuntime,
        session_id: DurableSessionId,
        backend: Arc<Backend>,
        initial_session: AgentSession<I, ContextState>,
        executor: Arc<Executor>,
        model: impl Into<StorageString>,
        max_tokens: u64,
        host_config: DurableMountedHostConfig,
        reducer: Reducer,
        live_factory: LiveFactory,
        commit_stager: Stager,
        fingerprint_factory: Fingerprint,
    ) -> Self {
        Self::from_runtime(
            runtime.into_mounted_runtime(),
            session_id,
            backend,
            initial_session,
            executor,
            model.into(),
            max_tokens,
            host_config,
            reducer,
            live_factory,
            commit_stager,
            fingerprint_factory,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_runtime(
        runtime: MountedRuntime,
        session_id: DurableSessionId,
        backend: Arc<Backend>,
        initial_session: AgentSession<I, ContextState>,
        executor: Arc<Executor>,
        model: StorageString,
        max_tokens: u64,
        host_config: DurableMountedHostConfig,
        reducer: Reducer,
        live_factory: LiveFactory,
        commit_stager: Stager,
        fingerprint_factory: Fingerprint,
    ) -> Self {
        Self {
            runtime,
            executor,
            backend,
            session_id,
            initial_session,
            reducer: Arc::new(reducer),
            live_factory: Arc::new(live_factory),
            commit_stager: Arc::new(commit_stager),
            fingerprint_factory: Arc::new(fingerprint_factory),
            model,
            max_tokens,
            host_config,
            contract: PhantomData,
        }
    }

    /// Open or reopen the durable mounted owner.
    pub async fn open<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
    ) -> Result<MountedAgent<C, Capture::CallProps, Capture::Source>, MountedOpenError>
    where
        I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Backend: DurableMountedStateBackend<Stager::Payload>,
        Fingerprint: DurableOutboxFingerprint<Stager::Payload>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        self.open_with_bindings(definition, MountedHostBindings::new())
            .await
    }

    /// Open or reopen with explicit host-owned runtime bindings.
    ///
    /// This is the normal host integration entry point for any pure provided
    /// component contract. Bindings are validated before durable admission can
    /// render or attach System and are never serialized into the epoch.
    pub async fn open_with_bindings<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        bindings: MountedHostBindings<C, Capture::TurnProps>,
    ) -> Result<MountedAgent<C, Capture::CallProps, Capture::Source>, MountedOpenError>
    where
        I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Backend: DurableMountedStateBackend<Stager::Payload>,
        Fingerprint: DurableOutboxFingerprint<Stager::Payload>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        MountedAgent::open_with_bindings(self, definition, bindings).await
    }

    /// Open or reopen with host-owned dispatcher bindings for pure provider contracts.
    ///
    /// Contract, schema, and host implementation drift are rejected before
    /// System projection or provider attachment. The validated host
    /// implementation version is persisted in the durable epoch manifest.
    pub async fn open_with_provider_dispatchers<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        provider_dispatchers: ProviderDispatcherRegistry<C, Capture::TurnProps>,
    ) -> Result<MountedAgent<C, Capture::CallProps, Capture::Source>, MountedOpenError>
    where
        I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Backend: DurableMountedStateBackend<Stager::Payload>,
        Fingerprint: DurableOutboxFingerprint<Stager::Payload>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        self.open_with_bindings(
            definition,
            MountedHostBindings::with_provider_dispatchers(provider_dispatchers),
        )
        .await
    }

    async fn open_driver_with_provider_dispatchers<Capture>(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        provider_dispatchers: ProviderDispatcherRegistry<C, Capture::TurnProps>,
    ) -> Result<Arc<dyn MountedAgentDriver<C, Capture::CallProps, Capture::Source>>, MountedOpenError>
    where
        I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
        Executor: DurableMountedProviderExecutor<I> + 'static,
        Reducer: SessionReducer<I, ContextState, C>,
        LiveFactory:
            LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
        Backend: DurableMountedStateBackend<Stager::Payload>,
        Fingerprint: DurableOutboxFingerprint<Stager::Payload>,
        Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
    {
        let epoch_definition = definition.epoch_arc();
        let store = Arc::new(DurableBackendMountedStore::new(
            Arc::clone(&self.backend),
            self.session_id.clone(),
            self.host_config.lease_duration,
        ));
        let binding = DurableBackendMountedBinding {
            session_id: self.session_id.clone(),
            store,
            initial_session: self.initial_session.clone(),
            reducer: Arc::clone(&self.reducer),
            live_factory: Arc::clone(&self.live_factory),
            commit_stager: Arc::clone(&self.commit_stager),
            fingerprint_factory: Arc::clone(&self.fingerprint_factory),
            provider_dispatchers,
            epoch_definition,
            host_config: self.host_config.clone(),
        };
        let agent = open_captured_bound_agent::<C, Capture, Executor, I, u64, _>(
            self.runtime.clone(),
            definition,
            Arc::clone(&self.executor),
            self.model.clone(),
            self.max_tokens,
            binding,
        )
        .await?;
        Ok(agent.into_driver())
    }
}

impl<C, I, ContextState, Executor, Reducer, LiveFactory, Backend, Stager, Fingerprint> fmt::Debug
    for DurableMountedAgentFactory<
        C,
        I,
        ContextState,
        Executor,
        Reducer,
        LiveFactory,
        Backend,
        Stager,
        Fingerprint,
    >
where
    C: TurnChannels,
    Stager: CommitStager<C>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableMountedAgentFactory")
            .field("session_id", &self.session_id)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

struct SharedCommitStager<C, Stager>
where
    C: TurnChannels,
    Stager: CommitStager<C>,
{
    inner: Arc<Stager>,
    channels: PhantomData<fn() -> C>,
}

impl<C, Stager> SharedCommitStager<C, Stager>
where
    C: TurnChannels,
    Stager: CommitStager<C>,
{
    fn new(inner: Arc<Stager>) -> Self {
        Self {
            inner,
            channels: PhantomData,
        }
    }
}

impl<C, Stager> CommitStager<C> for SharedCommitStager<C, Stager>
where
    C: TurnChannels,
    Stager: CommitStager<C>,
{
    type Payload = Stager::Payload;
    type Error = Stager::Error;

    fn stage(
        &self,
        context: CommitStagingContext<'_>,
        commit: &C::Commit,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        self.inner.stage(context, commit)
    }
}

type DurableMountedContract<I, ContextState, Payload> =
    PhantomData<fn() -> (I, ContextState, Payload)>;

struct DurableMountedFingerprintFactory<I, ContextState, Payload, Fingerprint> {
    inner: Arc<Fingerprint>,
    contract: DurableMountedContract<I, ContextState, Payload>,
}

impl<I, ContextState, Payload, Fingerprint>
    DurableMountedFingerprintFactory<I, ContextState, Payload, Fingerprint>
{
    fn new(inner: Arc<Fingerprint>) -> Self {
        Self {
            inner,
            contract: PhantomData,
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum DurableMountedFingerprintError<E>
where
    E: Error + Send + Sync + 'static,
{
    #[error("failed to canonicalize the durable mounted session mutation: {0}")]
    Mutation(#[source] serde_json::Error),

    #[error("failed to canonicalize durable mounted provider results: {0}")]
    ProviderResults(#[source] serde_json::Error),

    #[error("failed to fingerprint the durable mounted Commit outbox: {0}")]
    Outbox(#[source] E),
}

fn update_durable_fingerprint_part(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn update_optional_durable_fingerprint_part(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            update_durable_fingerprint_part(digest, value.as_bytes());
        }
        None => digest.update([0]),
    }
}

/// Encode a value for a durable publication fingerprint.
///
/// Serde's map iteration order is not a durable contract for arbitrary host
/// transcript or context types. Normalize every JSON object recursively before
/// encoding so a `HashMap` rebuilt in another process cannot turn the same
/// mounted mutation into a different publication candidate. Arrays deliberately
/// retain order because they are semantic ordered sequences in POM/session data.
/// Callers must serialize unordered collections such as `HashSet` into a stable
/// sorted sequence; JSON cannot distinguish those from intentionally ordered
/// arrays after serialization.
fn canonical_durable_json_bytes<T>(value: &T) -> Result<Vec<u8>, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let mut value = serde_json::to_value(value)?;
    canonicalize_durable_json_value(&mut value);
    serde_json::to_vec(&value)
}

fn canonicalize_durable_json_value(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                canonicalize_durable_json_value(value);
            }
        }
        Value::Object(object) => {
            let mut fields = std::mem::take(object).into_iter().collect::<Vec<_>>();
            for (_, value) in &mut fields {
                canonicalize_durable_json_value(value);
            }
            fields.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            object.extend(fields);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn fingerprint_durable_mounted_publication<Mutation, Payload, Fingerprint>(
    fingerprint: &Fingerprint,
    context: PublicationFingerprintContext<'_, Mutation, Payload, u64>,
) -> Result<PublicationCandidateFingerprint, DurableMountedFingerprintError<Fingerprint::Error>>
where
    Mutation: Serialize,
    Payload: Send + Sync + 'static,
    Fingerprint: DurableOutboxFingerprint<Payload>,
{
    let mutation = canonical_durable_json_bytes(context.mutation().value())
        .map_err(DurableMountedFingerprintError::Mutation)?;
    let outbox = fingerprint
        .fingerprint(context.outbox())
        .map_err(DurableMountedFingerprintError::Outbox)?;

    let mut digest = Sha256::new();
    digest.update(DURABLE_PUBLICATION_FINGERPRINT_SCHEMA.as_bytes());
    digest.update([0]);
    update_durable_fingerprint_part(&mut digest, context.request_id().as_str().as_bytes());
    digest.update(context.expected_revision().to_be_bytes());
    update_durable_fingerprint_part(&mut digest, &mutation);
    update_durable_fingerprint_part(&mut digest, context.raw_output().as_bytes());
    digest.update((context.provider_results().len() as u64).to_be_bytes());
    for result in context.provider_results() {
        update_optional_durable_fingerprint_part(&mut digest, result.invocation_id());
        update_optional_durable_fingerprint_part(&mut digest, result.result_correlation_id());
        update_durable_fingerprint_part(&mut digest, result.name().as_bytes());
        match result.response() {
            super::ProviderToolResponse::Success { content } => {
                digest.update([1]);
                let content = canonical_durable_json_bytes(content)
                    .map_err(DurableMountedFingerprintError::ProviderResults)?;
                update_durable_fingerprint_part(&mut digest, &content);
            }
            super::ProviderToolResponse::Error {
                code,
                message,
                details,
            } => {
                digest.update([2]);
                update_durable_fingerprint_part(&mut digest, code.as_bytes());
                update_durable_fingerprint_part(&mut digest, message.as_bytes());
                match details {
                    Some(details) => {
                        digest.update([1]);
                        let details = canonical_durable_json_bytes(details)
                            .map_err(DurableMountedFingerprintError::ProviderResults)?;
                        update_durable_fingerprint_part(&mut digest, &details);
                    }
                    None => digest.update([0]),
                }
            }
        }
    }
    update_durable_fingerprint_part(&mut digest, outbox.as_bytes());
    Ok(
        PublicationCandidateFingerprint::new(format!("sha256:v2:{:x}", digest.finalize()))
            .expect("a versioned SHA-256 fingerprint is a valid durable key"),
    )
}

impl<I, ContextState, Payload, Fingerprint>
    PublicationFingerprintFactory<MountedSessionMutation<I, ContextState>, Payload, u64>
    for DurableMountedFingerprintFactory<I, ContextState, Payload, Fingerprint>
where
    I: Serialize + 'static,
    ContextState: Serialize + 'static,
    Payload: Send + Sync + 'static,
    Fingerprint: DurableOutboxFingerprint<Payload>,
{
    type Error = DurableMountedFingerprintError<Fingerprint::Error>;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<
            '_,
            MountedSessionMutation<I, ContextState>,
            Payload,
            u64,
        >,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        fingerprint_durable_mounted_publication(self.inner.as_ref(), context)
    }
}

struct DurableBackendMountedBinding<
    C,
    I,
    ContextState,
    Props,
    Reducer,
    LiveFactory,
    Backend,
    Stager,
    Fingerprint,
> where
    C: TurnChannels,
    Props: ?Sized + 'static,
    Stager: CommitStager<C>,
{
    session_id: DurableSessionId,
    store: Arc<DurableBackendMountedStore<I, ContextState, Stager::Payload, Backend>>,
    initial_session: AgentSession<I, ContextState>,
    reducer: Arc<Reducer>,
    live_factory: Arc<LiveFactory>,
    commit_stager: Arc<Stager>,
    fingerprint_factory: Arc<Fingerprint>,
    provider_dispatchers: ProviderDispatcherRegistry<C, Props>,
    epoch_definition: Arc<DurableEpochDefinition<C, Props>>,
    host_config: DurableMountedHostConfig,
}

#[derive(Debug, thiserror::Error)]
enum DurableBackendMountedBindingError {
    #[error("durable mounted host {phase} failed: {source}")]
    Phase {
        phase: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl DurableBackendMountedBindingError {
    fn phase(phase: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Phase {
            phase,
            source: Box::new(source),
        }
    }
}

#[async_trait::async_trait]
impl<C, I, ContextState, Executor, Reducer, LiveFactory, Backend, Stager, Fingerprint, Capture>
    MountedAgentFactory<C, Capture, I>
    for DurableMountedAgentFactory<
        C,
        I,
        ContextState,
        Executor,
        Reducer,
        LiveFactory,
        Backend,
        Stager,
        Fingerprint,
    >
where
    C: TurnChannels,
    I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    Executor: DurableMountedProviderExecutor<I> + 'static,
    Reducer: SessionReducer<I, ContextState, C>,
    LiveFactory:
        LiveEffectRuntimeFactory<C, Capture::CallProps, Capture::Source, Capture::TurnProps>,
    Backend: DurableMountedStateBackend<Stager::Payload>,
    Stager: CommitStager<C>,
    Fingerprint: DurableOutboxFingerprint<Stager::Payload>,
    Capture: MountedTurnCapture<Transcript = I, ContextState = ContextState>,
{
    async fn open(
        &self,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        bindings: MountedHostBindings<C, Capture::TurnProps>,
    ) -> Result<Arc<dyn MountedAgentDriver<C, Capture::CallProps, Capture::Source>>, MountedOpenError>
    {
        self.open_driver_with_provider_dispatchers(definition, bindings.into_provider_dispatchers())
            .await
    }
}

#[async_trait::async_trait]
impl<H, C, I, ContextState, Props, Reducer, LiveFactory, Backend, Stager, Fingerprint>
    MountedAgentBinding<H, I, u64>
    for DurableBackendMountedBinding<
        C,
        I,
        ContextState,
        Props,
        Reducer,
        LiveFactory,
        Backend,
        Stager,
        Fingerprint,
    >
where
    H: super::mounted_agent::MountedTurnHarness<
        I,
        Channels = C,
        ContextState = ContextState,
        TurnProps = Props,
    >,
    C: TurnChannels,
    I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    Props: Send + Sync + 'static,
    Reducer: SessionReducer<I, ContextState, C>,
    LiveFactory: LiveEffectRuntimeFactory<C, H::CallProps, H::Source, Props>,
    Backend: DurableMountedStateBackend<Stager::Payload>,
    Stager: CommitStager<C>,
    Fingerprint: DurableOutboxFingerprint<Stager::Payload>,
{
    type Payload = Stager::Payload;
    type Store = DurableBackendMountedStore<I, ContextState, Stager::Payload, Backend>;
    type EpochDefinition = DurableEpochDefinition<C, Props>;
    type Error = DurableBackendMountedBindingError;

    fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    fn initial_epoch_definition(&self) -> Arc<Self::EpochDefinition> {
        Arc::clone(&self.epoch_definition)
    }

    fn host_configuration_fingerprint(&self) -> StorageString {
        self.host_config.host_configuration_fingerprint.clone()
    }

    fn runtime_implementation_fingerprint(&self) -> StorageString {
        self.host_config.runtime_binder_fingerprint.clone()
    }

    fn provider_dispatcher_registry(&self) -> ProviderDispatcherRegistry<C, Props> {
        self.provider_dispatchers.clone()
    }

    fn store(&self) -> Arc<Self::Store> {
        Arc::clone(&self.store)
    }

    fn initial_session(&self) -> AgentSession<I, ContextState> {
        self.initial_session.clone()
    }

    fn start_attempt<'epoch, 'turn_props>(
        &self,
        runtime: &tokio::runtime::Handle,
        prepared: &super::PreparedUserTurn<'epoch, 'turn_props, C, Props>,
        context: MountedAttemptBindingContext<'_, H::CallProps, H::Source>,
        plan_factory: SuppliedPublicationPlanFactory,
        interpreter: TurnRecordInterpreter<C>,
        store: Arc<
            MountedSessionPublicationStore<Self::Store, I, ContextState, Self::Payload, u64>,
        >,
    ) -> Result<
        ManagedActiveAttempt<
            u64,
            MountedSessionPublicationError<DurableBackendMountedStoreError<Backend::Error>>,
            PublicationStagingPlan<MountedSessionMutation<I, ContextState>, u64>,
        >,
        Self::Error,
    > {
        let attempt = prepared.next_attempt_identity();
        let live_runtime = self
            .live_factory
            .bind(LiveEffectRuntimeBindingContext::new(
                &self.session_id,
                self.epoch_definition.epoch_contract_id(),
                context.call_id(),
                context.input_id(),
                context.turn_index(),
                context.call_props(),
                context.source(),
                prepared.props(),
                &attempt,
            ))
            .map_err(|source| {
                DurableBackendMountedBindingError::phase("live runtime binding", source)
            })?;
        let finalizer = DurableStreamingPublicationFinalizer::new(
            plan_factory,
            SharedCommitStager::new(Arc::clone(&self.commit_stager)),
            DurableMountedFingerprintFactory::new(Arc::clone(&self.fingerprint_factory)),
            store,
        );
        start_managed_streaming_attempt_with_durable_finalizer_and_identity_on(
            runtime,
            prepared,
            prepared.epoch().channel_type_info(),
            attempt,
            live_runtime,
            interpreter,
            finalizer,
        )
        .map_err(|source| DurableBackendMountedBindingError::phase("attempt start", source))
    }

    fn request_id(
        &self,
        context: PublicationRequestContext<'_>,
    ) -> Result<PublicationRequestId, Self::Error> {
        PublicationRequestId::new(format!(
            "{}/{}/turn-{}",
            context.session_id(),
            context.call_id(),
            context.turn_index()
        ))
        .map_err(|source| DurableBackendMountedBindingError::phase("request identity", source))
    }

    fn reduce(
        &self,
        session: &mut AgentSession<I, ContextState>,
        context: SessionReduceContext<'_, I, C>,
        executor_commit: ExecutorCommit<I>,
    ) -> Result<TurnFlow, MountedSessionReductionFailure<Self::Error>> {
        self.reducer
            .reduce(session, context, executor_commit)
            .map_err(|source| {
                let disposition = self.reducer.failure_disposition(&source);
                MountedSessionReductionFailure::new(
                    disposition,
                    DurableBackendMountedBindingError::phase("session reducer", source),
                )
            })
    }
}

#[async_trait::async_trait]
impl<H, C, I, ContextState, Props, Reducer, LiveFactory> MountedAgentBinding<H, I, u64>
    for InMemoryMountedBinding<C, I, ContextState, Props, Reducer, LiveFactory>
where
    H: super::mounted_agent::MountedTurnHarness<
        I,
        Channels = C,
        ContextState = ContextState,
        TurnProps = Props,
    >,
    C: TurnChannels<Commit = Never>,
    I: Clone + Send + Sync + 'static,
    ContextState: Clone + Send + Sync + 'static,
    Props: Send + Sync + 'static,
    Reducer: SessionReducer<I, ContextState, C>,
    LiveFactory: LiveEffectRuntimeFactory<C, H::CallProps, H::Source, Props>,
{
    type Payload = Never;
    type Store = InMemoryMountedStore<I, ContextState>;
    type EpochDefinition = DurableEpochDefinition<C, Props>;
    type Error = InMemoryMountedBindingError;

    fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    fn initial_epoch_definition(&self) -> Arc<Self::EpochDefinition> {
        Arc::clone(&self.epoch_definition)
    }

    fn host_configuration_fingerprint(&self) -> StorageString {
        self.host_configuration_fingerprint.clone()
    }

    fn runtime_implementation_fingerprint(&self) -> StorageString {
        self.runtime_implementation_fingerprint.clone()
    }

    fn provider_dispatcher_registry(&self) -> ProviderDispatcherRegistry<C, Props> {
        self.provider_dispatchers.clone()
    }

    fn store(&self) -> Arc<Self::Store> {
        Arc::clone(&self.store)
    }

    fn initial_session(&self) -> AgentSession<I, ContextState> {
        self.initial_session.clone()
    }

    fn start_attempt<'epoch, 'turn_props>(
        &self,
        runtime: &tokio::runtime::Handle,
        prepared: &super::PreparedUserTurn<'epoch, 'turn_props, C, Props>,
        context: MountedAttemptBindingContext<'_, H::CallProps, H::Source>,
        plan_factory: SuppliedPublicationPlanFactory,
        interpreter: TurnRecordInterpreter<C>,
        store: Arc<
            MountedSessionPublicationStore<Self::Store, I, ContextState, Self::Payload, u64>,
        >,
    ) -> Result<
        ManagedActiveAttempt<
            u64,
            MountedSessionPublicationError<InMemoryMountedStoreError>,
            PublicationStagingPlan<MountedSessionMutation<I, ContextState>, u64>,
        >,
        Self::Error,
    > {
        let attempt = prepared.next_attempt_identity();
        let live_runtime = self
            .live_factory
            .bind(LiveEffectRuntimeBindingContext::new(
                &self.session_id,
                self.epoch_definition.epoch_contract_id(),
                context.call_id(),
                context.input_id(),
                context.turn_index(),
                context.call_props(),
                context.source(),
                prepared.props(),
                &attempt,
            ))
            .map_err(|source| InMemoryMountedBindingError::phase("live runtime binding", source))?;
        let finalizer = DurableStreamingPublicationFinalizer::new(
            plan_factory,
            NeverCommitStager,
            InMemoryFingerprintFactory,
            store,
        );
        start_managed_streaming_attempt_with_durable_finalizer_and_identity_on(
            runtime,
            prepared,
            prepared.epoch().channel_type_info(),
            attempt,
            live_runtime,
            interpreter,
            finalizer,
        )
        .map_err(|source| InMemoryMountedBindingError::phase("attempt start", source))
    }

    fn request_id(
        &self,
        context: PublicationRequestContext<'_>,
    ) -> Result<PublicationRequestId, Self::Error> {
        PublicationRequestId::new(format!(
            "{}/{}/turn-{}",
            context.session_id(),
            context.call_id(),
            context.turn_index()
        ))
        .map_err(|source| InMemoryMountedBindingError::phase("request identity", source))
    }

    fn reduce(
        &self,
        session: &mut AgentSession<I, ContextState>,
        context: SessionReduceContext<'_, I, C>,
        executor_commit: ExecutorCommit<I>,
    ) -> Result<TurnFlow, MountedSessionReductionFailure<Self::Error>> {
        self.reducer
            .reduce(session, context, executor_commit)
            .map_err(|source| {
                let disposition = self.reducer.failure_disposition(&source);
                MountedSessionReductionFailure::new(
                    disposition,
                    InMemoryMountedBindingError::phase("session reducer", source),
                )
            })
    }
}

struct NeverCommitStager;

impl<C> CommitStager<C> for NeverCommitStager
where
    C: TurnChannels<Commit = Never>,
{
    type Payload = Never;
    type Error = Infallible;

    fn stage(
        &self,
        _context: CommitStagingContext<'_>,
        commit: &Never,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        commit.absurd()
    }
}

struct InMemoryFingerprintFactory;

impl<I, ContextState>
    PublicationFingerprintFactory<MountedSessionMutation<I, ContextState>, Never, u64>
    for InMemoryFingerprintFactory
{
    type Error = Infallible;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<
            '_,
            MountedSessionMutation<I, ContextState>,
            Never,
            u64,
        >,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        let mut hash = DefaultHasher::new();
        context.request_id().as_str().hash(&mut hash);
        context.expected_revision().hash(&mut hash);
        context.raw_output().hash(&mut hash);
        format!("{:?}", context.provider_results()).hash(&mut hash);
        context.outbox().len().hash(&mut hash);
        if let Some(identity) = context.mutation().value().identity() {
            identity.session_id().as_str().hash(&mut hash);
            identity.epoch_contract_id().as_str().hash(&mut hash);
            identity.call_id().as_str().hash(&mut hash);
            identity.input_id().as_str().hash(&mut hash);
            identity.turn_index().hash(&mut hash);
        }
        if let Some(cursor) = context.mutation().value().provider_cursor() {
            format!("{cursor:?}").hash(&mut hash);
        }
        Ok(
            PublicationCandidateFingerprint::new(format!("in-memory-v1/{:016x}", hash.finish()))
                .expect("the fixed hexadecimal in-memory fingerprint is valid"),
        )
    }
}

#[derive(Debug, Clone, thiserror::Error)]
enum InMemoryMountedStoreError {
    #[error(transparent)]
    DurableKey(#[from] DurableKeyError),
    #[error("mounted state revision expected {expected}, found {actual}")]
    Revision { expected: u64, actual: u64 },
    #[error("mounted state revision is exhausted")]
    RevisionExhausted,
    #[error("mounted publication identity collided")]
    Collision,
    #[error("mounted state transition was rejected")]
    Rejected,
    #[error("mounted call lease expired")]
    LeaseExpired,
    #[error("mounted store is scoped to session `{expected}`, not `{actual}`")]
    SessionScope {
        expected: DurableSessionId,
        actual: DurableSessionId,
    },
    #[error("mounted epoch fence is stale")]
    EpochFence,
    #[error("mounted epoch transition is invalid")]
    EpochTransition,
    #[error("mounted store has no active epoch")]
    ActiveEpochMissing,
    #[error("mounted store active epoch does not match the owner")]
    ActiveEpochMismatch,
    #[error("mounted store has no initialized owner session")]
    OwnerSessionMissing,
    #[error("mounted owner System does not match the active epoch")]
    OwnerSystemMismatch,
    #[error("explicit System reconfiguration is not supported by the in-memory host yet")]
    ReconfigurationUnsupported,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
enum InMemoryEpochState {
    #[default]
    Empty,
    RenderStarted {
        manifest: EpochContractManifest,
        fence: EpochOpenFence,
    },
    Rendered {
        artifact: RenderedEpochArtifact,
        /// `None` means the last attachment attempt returned an error after
        /// persisting this artifact. Only a fresh ResumeAttachment fence may
        /// use it; Create remains impossible.
        fence: Option<EpochOpenFence>,
    },
    Active(ActiveEpochArtifact),
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct InMemoryMountedState<I, ContextState> {
    session_id: Option<DurableSessionId>,
    session: Option<AgentSession<I, ContextState>>,
    revision: u64,
    call_ledger: DurableCallLedger,
    provider_cursor: Option<ProviderTurnCursor>,
    pending_publication: Option<ClaimedCallPublication>,
    receipts: Vec<PublicationReceipt<u64>>,
    settled_results: Vec<StoredCallResult<u64>>,
    epoch: InMemoryEpochState,
    lease_sequence: u64,
    epoch_sequence: u64,
}

impl<I, ContextState> Default for InMemoryMountedState<I, ContextState> {
    fn default() -> Self {
        Self {
            session_id: None,
            session: None,
            revision: 0,
            call_ledger: DurableCallLedger::default(),
            provider_cursor: None,
            pending_publication: None,
            receipts: Vec::new(),
            settled_results: Vec::new(),
            epoch: InMemoryEpochState::Empty,
            lease_sequence: 0,
            epoch_sequence: 0,
        }
    }
}

struct InMemoryMountedStore<I, ContextState> {
    state: Mutex<InMemoryMountedState<I, ContextState>>,
    authoritative_now_unix_ms: Option<u64>,
    lease_duration: Duration,
}

impl<I, ContextState> Default for InMemoryMountedStore<I, ContextState> {
    fn default() -> Self {
        Self {
            state: Mutex::new(InMemoryMountedState::default()),
            authoritative_now_unix_ms: None,
            lease_duration: LOCAL_LEASE_DURATION,
        }
    }
}

impl<I, ContextState> InMemoryMountedStore<I, ContextState> {
    fn from_state_at(
        state: InMemoryMountedState<I, ContextState>,
        authoritative_now_unix_ms: u64,
        lease_duration: Duration,
    ) -> Self {
        Self {
            state: Mutex::new(state),
            authoritative_now_unix_ms: Some(authoritative_now_unix_ms),
            lease_duration,
        }
    }

    fn into_state(self) -> InMemoryMountedState<I, ContextState> {
        self.state
            .into_inner()
            .expect("transaction-local mounted store poisoned")
    }

    fn now_unix_ms(&self) -> u64 {
        self.authoritative_now_unix_ms.unwrap_or_else(|| {
            u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        })
    }

    fn next_revision(
        state: &mut InMemoryMountedState<I, ContextState>,
    ) -> Result<u64, InMemoryMountedStoreError> {
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or(InMemoryMountedStoreError::RevisionExhausted)?;
        Ok(state.revision)
    }

    fn issue_epoch_fence(
        &self,
        state: &mut InMemoryMountedState<I, ContextState>,
        durable_epoch_id: super::DurableEpochId,
    ) -> Result<EpochOpenFence, InMemoryMountedStoreError> {
        state.epoch_sequence = state
            .epoch_sequence
            .checked_add(1)
            .ok_or(InMemoryMountedStoreError::RevisionExhausted)?;
        let issued_at = self.now_unix_ms();
        let expires_at = issued_at
            .saturating_add(u64::try_from(self.lease_duration.as_millis()).unwrap_or(u64::MAX));
        let lease = EpochOpenLease::new(issued_at, expires_at)
            .map_err(|_| InMemoryMountedStoreError::EpochFence)?;
        EpochOpenFence::new(
            durable_epoch_id,
            format!("in-memory-epoch-fence-{}", state.epoch_sequence),
            lease,
        )
        .map_err(|_| InMemoryMountedStoreError::EpochFence)
    }

    fn fence_is_live(&self, fence: &EpochOpenFence) -> bool {
        fence.lease().is_live_at(self.now_unix_ms())
    }

    fn ensure_scope(
        state: &mut InMemoryMountedState<I, ContextState>,
        session_id: &DurableSessionId,
    ) -> Result<(), InMemoryMountedStoreError> {
        match state.session_id.as_ref() {
            Some(existing) if existing != session_id => {
                Err(InMemoryMountedStoreError::SessionScope {
                    expected: existing.clone(),
                    actual: session_id.clone(),
                })
            }
            Some(_) => Ok(()),
            None => {
                state.session_id = Some(session_id.clone());
                Ok(())
            }
        }
    }

    fn new_call_lease(
        &self,
        state: &mut InMemoryMountedState<I, ContextState>,
        request: &MountedCallClaimRequest<'_, u64>,
    ) -> Result<super::DurableCallLease, InMemoryMountedStoreError> {
        state.lease_sequence = state
            .lease_sequence
            .checked_add(1)
            .ok_or(InMemoryMountedStoreError::RevisionExhausted)?;
        let expires_at = self
            .now_unix_ms()
            .saturating_add(u64::try_from(self.lease_duration.as_millis()).unwrap_or(u64::MAX));
        Ok(super::DurableCallLease::new(
            DurableCallLeaseId::new(format!(
                "{}/{}/lease-{}",
                request.session_id(),
                request.call_id(),
                state.lease_sequence
            ))?,
            request.request_id().clone(),
            expires_at,
        ))
    }

    fn new_reconciliation_fence(
        &self,
        state: &mut InMemoryMountedState<I, ContextState>,
        session_id: &DurableSessionId,
        call_id: &super::DurableCallId,
    ) -> Result<MountedCallReconciliationFence, InMemoryMountedStoreError> {
        state.lease_sequence = state
            .lease_sequence
            .checked_add(1)
            .ok_or(InMemoryMountedStoreError::RevisionExhausted)?;
        let expires_at = self
            .now_unix_ms()
            .saturating_add(u64::try_from(self.lease_duration.as_millis()).unwrap_or(u64::MAX));
        Ok(MountedCallReconciliationFence::new(
            DurableCallLeaseId::new(format!(
                "{session_id}/{call_id}/reconciliation-{}",
                state.lease_sequence
            ))?,
            expires_at,
        ))
    }

    fn snapshot(
        state: &InMemoryMountedState<I, ContextState>,
        epoch_contract_id: &EpochContractId,
    ) -> Result<MountedSessionSnapshot<I, ContextState, u64>, InMemoryMountedStoreError>
    where
        I: Clone,
        ContextState: Clone,
    {
        let session_id = state
            .session_id
            .clone()
            .ok_or(InMemoryMountedStoreError::OwnerSessionMissing)?;
        let session = state
            .session
            .clone()
            .ok_or(InMemoryMountedStoreError::OwnerSessionMissing)?;
        Ok(MountedSessionSnapshot::new(
            session_id,
            epoch_contract_id.clone(),
            session,
            state.revision,
        )
        .with_call_ledger(state.call_ledger.clone())
        .with_provider_cursor(state.provider_cursor.clone()))
    }

    fn checkpoint_recovery(
        state: &mut InMemoryMountedState<I, ContextState>,
        publication: &ClaimedCallPublication,
        reason: DurableCallRecoveryReason,
    ) -> Result<(), InMemoryMountedStoreError> {
        let Some(call) = state.call_ledger.get(publication.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if matches!(call.status(), DurableCallStatus::RecoveryRequired { lease, .. }
            if lease == publication.lease())
        {
            state.pending_publication = None;
            return Ok(());
        }
        let DurableCallStatus::Running { lease, .. } = call.status() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if call.input_id() != publication.input_id()
            || call.epoch_contract_id() != publication.epoch_contract_id()
            || call.next_turn_index() != publication.turn_index()
            || lease != publication.lease()
        {
            return Err(InMemoryMountedStoreError::Rejected);
        }
        state
            .call_ledger
            .upsert(call.recovery_required(lease.clone(), reason));
        Self::next_revision(state)?;
        state.pending_publication = None;
        Ok(())
    }

    fn checkpoint_cancellation_recovery(
        state: &mut InMemoryMountedState<I, ContextState>,
        call: &DurableCallState,
        lease: &super::DurableCallLease,
        _request: &MountedCallCancellationSettlementRequest<'_, u64>,
        reason: DurableCallRecoveryReason,
    ) -> Result<MountedCallCancellationSettlement<u64>, InMemoryMountedStoreError> {
        let recovery = call.recovery_required(lease.clone(), reason);
        let session_revision = Self::next_revision(state)?;
        state.call_ledger.upsert(recovery.clone());
        Ok(MountedCallCancellationSettlement::RecoveryRequired {
            state: recovery,
            session_revision,
        })
    }

    fn commit<Payload>(
        &self,
        expected_active_epoch: &ActiveEpochArtifact,
        request: PublicationRequest<'_, MountedSessionMutation<I, ContextState>, Payload, u64>,
    ) -> Result<InMemoryPublicationCommit, PublicationWriteError<InMemoryMountedStoreError>>
    where
        I: Clone,
        ContextState: Clone,
    {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        if let Some(receipt) = state
            .receipts
            .iter()
            .find(|receipt| receipt.request_id() == request.request_id())
        {
            return if receipt.fingerprint() == request.fingerprint() {
                Ok(InMemoryPublicationCommit::Existing(receipt.clone()))
            } else {
                Err(PublicationWriteError::Rejected(
                    InMemoryMountedStoreError::Collision,
                ))
            };
        }
        if state.revision != *request.expected_revision() {
            return Err(PublicationWriteError::Conflict(
                InMemoryMountedStoreError::Revision {
                    expected: *request.expected_revision(),
                    actual: state.revision,
                },
            ));
        }
        let Some(identity) = request.mutation().value().identity() else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        let Some(session_id) = state.session_id.clone() else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        let Some(running) = state.call_ledger.get(identity.call_id()).cloned() else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        let DurableCallStatus::Running { lease, .. } = running.status() else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        if identity.session_id() != &session_id
            || running.input_id() != identity.input_id()
            || running.epoch_contract_id() != identity.epoch_contract_id()
            || running.next_turn_index() != identity.turn_index()
            || lease.lease_id() != identity.lease_id()
            || lease.request_id() != identity.request_id()
            || request.request_id() != identity.request_id()
        {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        }
        if self.now_unix_ms() >= lease.expires_at_unix_ms() {
            state.call_ledger.upsert(
                running.recovery_required(lease.clone(), DurableCallRecoveryReason::LeaseExpired),
            );
            state.revision = state.revision.saturating_add(1);
            return Err(PublicationWriteError::Conflict(
                InMemoryMountedStoreError::LeaseExpired,
            ));
        }
        let Some(pending) = state.pending_publication.as_ref() else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        if pending.session_id() != identity.session_id()
            || pending.epoch_contract_id() != identity.epoch_contract_id()
            || pending.call_id() != identity.call_id()
            || pending.input_id() != identity.input_id()
            || pending.turn_index() != identity.turn_index()
            || pending.request_id() != request.request_id()
            || pending.fingerprint() != request.fingerprint()
            || pending.lease() != lease
        {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        }
        let InMemoryEpochState::Active(active_epoch) = &state.epoch else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::ActiveEpochMissing,
            ));
        };
        if active_epoch != expected_active_epoch {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::ActiveEpochMismatch,
            ));
        }
        let next_cursor = request.mutation().value().provider_cursor();
        if super::mounted_agent::validate_provider_cursor_transition(
            state.provider_cursor.as_ref(),
            next_cursor,
        )
        .is_err()
            || next_cursor
                .is_some_and(|cursor| active_epoch.validate_provider_cursor(cursor).is_err())
        {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        }
        let Some(next_call) = request.mutation().value().durable_call_state().cloned() else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        let Some(expected_next_turn) = identity.turn_index().checked_add(1) else {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        };
        let valid_flow = matches!(
            (request.mutation().value().flow(), next_call.status()),
            (TurnFlow::Continue, DurableCallStatus::AwaitingContinuation)
                | (TurnFlow::Wait, DurableCallStatus::Settled)
        );
        if next_call.call_id() != identity.call_id()
            || next_call.input_id() != identity.input_id()
            || next_call.epoch_contract_id() != identity.epoch_contract_id()
            || next_call.next_turn_index() != expected_next_turn
            || next_call.last_request_id() != Some(identity.request_id())
            || !valid_flow
        {
            return Err(PublicationWriteError::Rejected(
                InMemoryMountedStoreError::Rejected,
            ));
        }

        let next_revision = state.revision.checked_add(1).ok_or_else(|| {
            PublicationWriteError::Rejected(InMemoryMountedStoreError::RevisionExhausted)
        })?;
        let receipt = PublicationReceipt::new(
            request.request_id().clone(),
            request.fingerprint().clone(),
            PublicationId::new(format!("in-memory/revision-{next_revision}"))
                .expect("the in-memory publication id is valid"),
            next_revision,
            u64::try_from(request.outbox().len()).unwrap_or(u64::MAX),
        );
        state.session = Some(request.mutation().value().session().clone());
        state.provider_cursor = next_cursor.cloned();
        state.call_ledger.upsert(next_call.clone());
        if matches!(next_call.status(), DurableCallStatus::Settled) {
            state
                .settled_results
                .retain(|result| result.call_id() != next_call.call_id());
            state.settled_results.push(StoredCallResult::new(
                session_id.clone(),
                identity.epoch_contract_id().clone(),
                identity.call_id().clone(),
                identity.input_id().clone(),
                receipt.clone(),
            ));
        }
        state.pending_publication = None;
        state.revision = next_revision;
        state.receipts.push(receipt.clone());
        Ok(InMemoryPublicationCommit::Published(receipt))
    }

    fn active_call_lease_deadline(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("transaction-local mounted store poisoned")
            .call_ledger
            .active()
            .and_then(|call| call.status().lease())
            .map(|lease| lease.expires_at_unix_ms())
    }

    fn epoch_fence_deadline(&self) -> Option<u64> {
        let state = self
            .state
            .lock()
            .expect("transaction-local mounted store poisoned");
        match &state.epoch {
            InMemoryEpochState::RenderStarted { fence, .. } => {
                Some(fence.lease().expires_at_unix_ms())
            }
            InMemoryEpochState::Rendered {
                fence: Some(fence), ..
            } => Some(fence.lease().expires_at_unix_ms()),
            InMemoryEpochState::Empty
            | InMemoryEpochState::Rendered { fence: None, .. }
            | InMemoryEpochState::Active(_) => None,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DurableMountedStateEnvelope<I, ContextState> {
    schema_version: u32,
    state: InMemoryMountedState<I, ContextState>,
    /// Unlike a general-purpose `AgentSession` serialization, a durable
    /// mounted owner must restore the exact acknowledged User POM baseline.
    /// It is persisted atomically with the session mutation and outbox.
    #[serde(default)]
    user_document_cursor: UserDocumentCursor,
}

struct LocalStateTransaction<R, E, I, ContextState> {
    result: Result<R, E>,
    state: InMemoryMountedState<I, ContextState>,
    must_commit_before_unix_ms: Option<u64>,
    write_outbox: bool,
}

/// Internal distinction required to make state/outbox publication atomic.
///
/// A matching prior receipt is a replay: it returns the same public receipt
/// but must not enqueue the Commit outbox again. A newly published receipt
/// must always reach the backend compare-and-exchange, even if a future state
/// codec happens to produce identical bytes.
enum InMemoryPublicationCommit {
    Published(PublicationReceipt<u64>),
    Existing(PublicationReceipt<u64>),
}

impl InMemoryPublicationCommit {
    fn into_receipt(self) -> PublicationReceipt<u64> {
        match self {
            Self::Published(receipt) | Self::Existing(receipt) => receipt,
        }
    }
}

impl<R, E, I, ContextState> LocalStateTransaction<R, E, I, ContextState> {
    fn new(
        result: Result<R, E>,
        state: InMemoryMountedState<I, ContextState>,
        must_commit_before_unix_ms: Option<u64>,
        write_outbox: bool,
    ) -> Self {
        Self {
            result,
            state,
            must_commit_before_unix_ms,
            write_outbox,
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum DurableBackendMountedStoreError<E>
where
    E: Error + Send + Sync + 'static,
{
    #[error("durable mounted backend failed: {0}")]
    Backend(#[source] E),

    #[error("failed to decode durable mounted state: {0}")]
    Decode(#[source] serde_json::Error),

    #[error("failed to encode durable mounted state: {0}")]
    Encode(#[source] serde_json::Error),

    #[error("durable mounted state schema {actual} is not supported; expected {expected}")]
    Schema { expected: u32, actual: u32 },

    #[error("durable mounted state remained contended after {attempts} CAS attempts")]
    Contended { attempts: usize },

    #[error(
        "durable mounted backend committed state without advancing its generation from {previous} to {actual}"
    )]
    GenerationNotAdvanced {
        previous: MountedStateGeneration,
        actual: MountedStateGeneration,
    },

    #[error(transparent)]
    Transition(#[from] InMemoryMountedStoreError),
}

impl<E> DurableBackendMountedStoreError<E>
where
    E: Error + Send + Sync + 'static,
{
    fn publication_write(
        error: PublicationWriteError<InMemoryMountedStoreError>,
    ) -> PublicationWriteError<Self> {
        match error {
            PublicationWriteError::Conflict(error) => {
                PublicationWriteError::Conflict(Self::Transition(error))
            }
            PublicationWriteError::Rejected(error) => {
                PublicationWriteError::Rejected(Self::Transition(error))
            }
            PublicationWriteError::Indeterminate(error) => {
                PublicationWriteError::Indeterminate(Self::Transition(error))
            }
        }
    }

    fn publication_resolve(
        error: PublicationResolveError<InMemoryMountedStoreError>,
    ) -> PublicationResolveError<Self> {
        match error {
            PublicationResolveError::Unavailable(error) => {
                PublicationResolveError::Unavailable(Self::Transition(error))
            }
            PublicationResolveError::CandidateCollision(error) => {
                PublicationResolveError::CandidateCollision(Self::Transition(error))
            }
        }
    }
}

struct DurableBackendMountedStore<I, ContextState, Payload, Backend> {
    backend: Arc<Backend>,
    session_id: DurableSessionId,
    lease_duration: Duration,
    contract: DurableMountedContract<I, ContextState, Payload>,
}

impl<I, ContextState, Payload, Backend>
    DurableBackendMountedStore<I, ContextState, Payload, Backend>
where
    I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Backend: DurableMountedStateBackend<Payload>,
{
    fn new(backend: Arc<Backend>, session_id: DurableSessionId, lease_duration: Duration) -> Self {
        Self {
            backend,
            session_id,
            lease_duration,
            contract: PhantomData,
        }
    }

    fn decode_state(
        &self,
        snapshot: &MountedStateSnapshot,
    ) -> Result<
        InMemoryMountedState<I, ContextState>,
        DurableBackendMountedStoreError<Backend::Error>,
    > {
        let Some(blob) = snapshot.state() else {
            return Ok(InMemoryMountedState::default());
        };
        let DurableMountedStateEnvelope {
            schema_version,
            mut state,
            user_document_cursor,
        } = serde_json::from_slice(blob.as_bytes())
            .map_err(DurableBackendMountedStoreError::Decode)?;
        match schema_version {
            DURABLE_STATE_SCHEMA_VERSION => {
                if let Some(session) = state.session.as_mut() {
                    session.set_user_document_cursor(user_document_cursor);
                }
            }
            // Version 2 did not durably encode the POM baseline. Its first
            // successor remains a full User resync, then writes schema v3.
            2 => {}
            actual => {
                return Err(DurableBackendMountedStoreError::Schema {
                    expected: DURABLE_STATE_SCHEMA_VERSION,
                    actual,
                });
            }
        }
        Ok(state)
    }

    fn encode_state(
        state: InMemoryMountedState<I, ContextState>,
    ) -> Result<MountedStateBlob, DurableBackendMountedStoreError<Backend::Error>> {
        let user_document_cursor = state
            .session
            .as_ref()
            .map(|session| session.user_document_cursor().clone())
            .unwrap_or_default();
        serde_json::to_vec(&DurableMountedStateEnvelope {
            schema_version: DURABLE_STATE_SCHEMA_VERSION,
            state,
            user_document_cursor,
        })
        .map(MountedStateBlob::new)
        .map_err(DurableBackendMountedStoreError::Encode)
    }

    async fn transact<R, E, Operation, OperationFuture>(
        &self,
        operation: Operation,
        outbox: Option<&StagedOutbox<Payload>>,
    ) -> Result<Result<R, E>, DurableBackendMountedStoreError<Backend::Error>>
    where
        R: Send,
        E: Send,
        Operation: Fn(InMemoryMountedStore<I, ContextState>) -> OperationFuture,
        OperationFuture: Future<Output = LocalStateTransaction<R, E, I, ContextState>> + Send,
    {
        let mut snapshot = self
            .backend
            .load(&self.session_id)
            .await
            .map_err(DurableBackendMountedStoreError::Backend)?;
        for _ in 0..DURABLE_STATE_MAX_CAS_RETRIES {
            let previous = snapshot.state().map(|state| state.as_bytes().to_vec());
            let state = self.decode_state(&snapshot)?;
            let expected_generation = snapshot.generation().cloned();
            let store = InMemoryMountedStore::from_state_at(
                state,
                snapshot.observed_at_unix_ms(),
                self.lease_duration,
            );
            let transaction = operation(store).await;
            let next = Self::encode_state(transaction.state)?;

            if !transaction.write_outbox && previous.as_deref() == Some(next.as_bytes()) {
                return Ok(transaction.result);
            }

            let write = MountedStateWrite::new(
                &self.session_id,
                expected_generation.as_ref(),
                &next,
                transaction.write_outbox.then_some(outbox).flatten(),
                transaction.must_commit_before_unix_ms,
            );
            match self
                .backend
                .compare_exchange(write)
                .await
                .map_err(DurableBackendMountedStoreError::Backend)?
            {
                MountedStateWriteOutcome::Committed { generation, .. } => {
                    if expected_generation.as_ref() == Some(&generation) {
                        return Err(DurableBackendMountedStoreError::GenerationNotAdvanced {
                            previous: expected_generation
                                .expect("an equal expected generation is present"),
                            actual: generation,
                        });
                    }
                    return Ok(transaction.result);
                }
                MountedStateWriteOutcome::Conflict { current }
                | MountedStateWriteOutcome::DeadlineElapsed { current } => {
                    snapshot = current;
                }
            }
        }
        Err(DurableBackendMountedStoreError::Contended {
            attempts: DURABLE_STATE_MAX_CAS_RETRIES,
        })
    }
}

#[async_trait::async_trait]
impl<I, ContextState> DurableEpochStore for InMemoryMountedStore<I, ContextState>
where
    I: Clone + Send + Sync + 'static,
    ContextState: Clone + Send + Sync + 'static,
{
    type Error = InMemoryMountedStoreError;

    async fn acquire_epoch(
        &self,
        request: EpochOpenRequest<'_>,
    ) -> Result<EpochOpenAdmission, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, request.session_id())?;
        match state.epoch.clone() {
            InMemoryEpochState::Empty => {
                let durable_epoch_id =
                    super::DurableEpochId::new(format!("{}/epoch-1", request.session_id()))?;
                let fence = self.issue_epoch_fence(&mut state, durable_epoch_id)?;
                state.epoch = InMemoryEpochState::RenderStarted {
                    manifest: request.manifest().clone(),
                    fence: fence.clone(),
                };
                Ok(EpochOpenAdmission::Create { fence })
            }
            InMemoryEpochState::RenderStarted { manifest, fence } => {
                if manifest != *request.manifest() {
                    Ok(EpochOpenAdmission::Conflict {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                        existing: manifest,
                    })
                } else if self.fence_is_live(&fence) {
                    Ok(EpochOpenAdmission::InFlight {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                        lease_expires_at_unix_ms: fence.lease().expires_at_unix_ms(),
                    })
                } else {
                    Ok(EpochOpenAdmission::RecoveryRequired {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                        phase: EpochOpenRecoveryPhase::RenderStarted,
                    })
                }
            }
            InMemoryEpochState::Rendered { artifact, fence } => {
                if artifact.manifest() != request.manifest() {
                    return Ok(EpochOpenAdmission::Conflict {
                        durable_epoch_id: artifact.durable_epoch_id().clone(),
                        existing: artifact.manifest().clone(),
                    });
                }
                if fence
                    .as_ref()
                    .is_some_and(|fence| self.fence_is_live(fence))
                {
                    return Ok(EpochOpenAdmission::InFlight {
                        durable_epoch_id: artifact.durable_epoch_id().clone(),
                        lease_expires_at_unix_ms: fence
                            .as_ref()
                            .expect("a live rendered fence is present")
                            .lease()
                            .expires_at_unix_ms(),
                    });
                }
                let next =
                    self.issue_epoch_fence(&mut state, artifact.durable_epoch_id().clone())?;
                state.epoch = InMemoryEpochState::Rendered {
                    artifact: artifact.clone(),
                    fence: Some(next.clone()),
                };
                Ok(EpochOpenAdmission::ResumeAttachment {
                    fence: next,
                    artifact,
                })
            }
            InMemoryEpochState::Active(artifact) => {
                if artifact.rendered().manifest() == request.manifest() {
                    Ok(EpochOpenAdmission::Existing { artifact })
                } else {
                    Ok(EpochOpenAdmission::Conflict {
                        durable_epoch_id: artifact.rendered().durable_epoch_id().clone(),
                        existing: artifact.rendered().manifest().clone(),
                    })
                }
            }
        }
    }

    async fn store_rendered_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let InMemoryEpochState::RenderStarted {
            manifest,
            fence: current,
        } = state.epoch.clone()
        else {
            return Err(InMemoryMountedStoreError::EpochTransition);
        };
        if current != *fence
            || !self.fence_is_live(fence)
            || artifact.durable_epoch_id() != fence.durable_epoch_id()
            || artifact.manifest() != &manifest
            || artifact.validate().is_err()
        {
            return Err(InMemoryMountedStoreError::EpochFence);
        }
        state.epoch = InMemoryEpochState::Rendered {
            artifact: artifact.clone(),
            fence: Some(fence.clone()),
        };
        Ok(())
    }

    async fn relinquish_epoch_attachment(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let InMemoryEpochState::Rendered {
            artifact: rendered,
            fence: current,
        } = state.epoch.clone()
        else {
            return Err(InMemoryMountedStoreError::EpochTransition);
        };
        if current.as_ref() != Some(fence) || &rendered != artifact || artifact.validate().is_err()
        {
            return Err(InMemoryMountedStoreError::EpochFence);
        }
        state.epoch = InMemoryEpochState::Rendered {
            artifact: rendered,
            fence: None,
        };
        Ok(())
    }

    async fn activate_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &ActiveEpochArtifact,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let InMemoryEpochState::Rendered {
            artifact: rendered,
            fence: current,
        } = state.epoch.clone()
        else {
            return Err(InMemoryMountedStoreError::EpochTransition);
        };
        if current.as_ref() != Some(fence)
            || !self.fence_is_live(fence)
            || artifact.rendered() != &rendered
            || artifact.validate().is_err()
        {
            return Err(InMemoryMountedStoreError::EpochFence);
        }
        let session = state
            .session
            .as_mut()
            .ok_or(InMemoryMountedStoreError::OwnerSessionMissing)?;
        match session.context().system() {
            None => session.set_system_once(artifact.rendered().rendered_system().to_owned()),
            Some(existing) if existing == artifact.rendered().rendered_system() => {}
            Some(_) => return Err(InMemoryMountedStoreError::OwnerSystemMismatch),
        }
        state.epoch = InMemoryEpochState::Active(artifact.clone());
        Ok(())
    }
}

#[async_trait::async_trait]
impl<I, ContextState, Payload, Backend> DurableEpochStore
    for DurableBackendMountedStore<I, ContextState, Payload, Backend>
where
    I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Backend: DurableMountedStateBackend<Payload>,
{
    type Error = DurableBackendMountedStoreError<Backend::Error>;

    async fn acquire_epoch(
        &self,
        request: EpochOpenRequest<'_>,
    ) -> Result<EpochOpenAdmission, Self::Error> {
        let session_id = request.session_id();
        let manifest = request.manifest();
        self.transact(
            |store| async move {
                let result = store
                    .acquire_epoch(EpochOpenRequest::new(session_id, manifest))
                    .await;
                let deadline = match result.as_ref() {
                    Ok(EpochOpenAdmission::Create { fence })
                    | Ok(EpochOpenAdmission::ResumeAttachment { fence, .. }) => {
                        Some(fence.lease().expires_at_unix_ms())
                    }
                    _ => None,
                };
                let state = store.into_state();
                LocalStateTransaction::new(result, state, deadline, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn store_rendered_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error> {
        self.transact(
            |store| async move {
                let deadline = store.epoch_fence_deadline();
                let result = store.store_rendered_epoch(fence, artifact).await;
                let deadline = result.as_ref().ok().and(deadline);
                let state = store.into_state();
                LocalStateTransaction::new(result, state, deadline, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn relinquish_epoch_attachment(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error> {
        self.transact(
            |store| async move {
                let result = store.relinquish_epoch_attachment(fence, artifact).await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn activate_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &ActiveEpochArtifact,
    ) -> Result<(), Self::Error> {
        self.transact(
            |store| async move {
                let deadline = store.epoch_fence_deadline();
                let result = store.activate_epoch(fence, artifact).await;
                let deadline = result.as_ref().ok().and(deadline);
                let state = store.into_state();
                LocalStateTransaction::new(result, state, deadline, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }
}

#[async_trait::async_trait]
impl<I, ContextState, Payload> PublicationStore<MountedSessionMutation<I, ContextState>, Payload>
    for InMemoryMountedStore<I, ContextState>
where
    I: Clone + Send + Sync + 'static,
    ContextState: Clone + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
{
    type Version = u64;
    type Error = InMemoryMountedStoreError;

    async fn publish(
        &self,
        _request: PublicationRequest<'_, MountedSessionMutation<I, ContextState>, Payload, u64>,
    ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
        Err(PublicationWriteError::Rejected(
            InMemoryMountedStoreError::Rejected,
        ))
    }

    async fn resolve(
        &self,
        request_id: &PublicationRequestId,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
        let state = self.state.lock().expect("in-memory mounted store poisoned");
        match state
            .receipts
            .iter()
            .find(|receipt| receipt.request_id() == request_id)
        {
            Some(receipt) if receipt.fingerprint() == fingerprint => {
                Ok(PublicationResolution::Published(receipt.clone()))
            }
            Some(_) => Err(PublicationResolveError::CandidateCollision(
                InMemoryMountedStoreError::Collision,
            )),
            None => Ok(PublicationResolution::NotCommitted),
        }
    }
}

#[async_trait::async_trait]
impl<I, ContextState, Payload> MountedSessionStore<I, ContextState, Payload, u64>
    for InMemoryMountedStore<I, ContextState>
where
    I: Clone + Send + Sync + 'static,
    ContextState: Clone + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
{
    type Error = InMemoryMountedStoreError;

    async fn claim_call(
        &self,
        request: MountedCallClaimRequest<'_, u64>,
    ) -> Result<MountedCallClaim<u64>, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, request.session_id())?;
        if let Some(existing) = state.call_ledger.active().cloned() {
            match existing.status() {
                DurableCallStatus::Reserved { lease, origin }
                    if self.now_unix_ms() >= lease.expires_at_unix_ms() =>
                {
                    match origin {
                        DurableCallReservationOrigin::New => {
                            state.call_ledger.remove(existing.call_id());
                        }
                        DurableCallReservationOrigin::AwaitingContinuation => {
                            state
                                .call_ledger
                                .upsert(DurableCallState::awaiting_continuation(
                                    existing.call_id().clone(),
                                    existing.input_id().clone(),
                                    existing.epoch_contract_id().clone(),
                                    existing.next_turn_index(),
                                    existing
                                        .last_request_id()
                                        .expect("continuation reservation has a request id")
                                        .clone(),
                                ));
                        }
                    }
                    Self::next_revision(&mut state)?;
                    return Ok(MountedCallClaim::RevisionConflict);
                }
                DurableCallStatus::Running { lease, .. }
                    if self.now_unix_ms() >= lease.expires_at_unix_ms() =>
                {
                    let recovery = existing
                        .recovery_required(lease.clone(), DurableCallRecoveryReason::LeaseExpired);
                    state.call_ledger.upsert(recovery.clone());
                    Self::next_revision(&mut state)?;
                    return Ok(MountedCallClaim::Existing(recovery));
                }
                _ => {}
            }
        }
        if let Some(existing) = state.call_ledger.get(request.call_id()).cloned() {
            if existing.input_id() != request.input_id()
                || existing.epoch_contract_id() != request.epoch_contract_id()
            {
                return Ok(MountedCallClaim::Existing(existing));
            }
            if !matches!(existing.status(), DurableCallStatus::AwaitingContinuation) {
                return Ok(MountedCallClaim::Existing(existing));
            }
            if existing.next_turn_index() != request.turn_index()
                || state.revision != *request.expected_revision()
            {
                return Ok(MountedCallClaim::RevisionConflict);
            }
            let lease = self.new_call_lease(&mut state, &request)?;
            let reserved = DurableCallState::reserved(
                existing.call_id().clone(),
                existing.input_id().clone(),
                existing.epoch_contract_id().clone(),
                existing.next_turn_index(),
                existing.last_request_id().cloned(),
                lease,
                DurableCallReservationOrigin::AwaitingContinuation,
            );
            state.call_ledger.upsert(reserved.clone());
            let revision = Self::next_revision(&mut state)?;
            return Ok(MountedCallClaim::Acquired {
                state: reserved,
                session_revision: revision,
            });
        }
        if let Some(active) = state.call_ledger.active().cloned() {
            return Ok(MountedCallClaim::Existing(active));
        }
        if state.revision != *request.expected_revision() || request.turn_index() != 0 {
            return Ok(MountedCallClaim::RevisionConflict);
        }
        let lease = self.new_call_lease(&mut state, &request)?;
        let reserved = DurableCallState::reserved(
            request.call_id().clone(),
            request.input_id().clone(),
            request.epoch_contract_id().clone(),
            0,
            None,
            lease,
            DurableCallReservationOrigin::New,
        );
        state.call_ledger.upsert(reserved.clone());
        let revision = Self::next_revision(&mut state)?;
        Ok(MountedCallClaim::Acquired {
            state: reserved,
            session_revision: revision,
        })
    }

    async fn mark_provider_started(
        &self,
        request: MountedCallLeaseRequest<'_, u64>,
    ) -> Result<MountedCallStart<u64>, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, request.session_id())?;
        if state.revision != *request.expected_revision() {
            return Ok(MountedCallStart::RevisionConflict);
        }
        let Some(reserved) = state.call_ledger.get(request.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        let DurableCallStatus::Reserved { lease, origin } = reserved.status() else {
            return Ok(MountedCallStart::Existing(reserved));
        };
        if reserved.input_id() != request.input_id()
            || reserved.epoch_contract_id() != request.epoch_contract_id()
            || reserved.next_turn_index() != request.turn_index()
            || lease.lease_id() != request.lease_id()
            || lease.request_id() != request.request_id()
            || self.now_unix_ms() >= lease.expires_at_unix_ms()
        {
            return Ok(MountedCallStart::RevisionConflict);
        }
        let running = DurableCallState::running(
            reserved.call_id().clone(),
            reserved.input_id().clone(),
            reserved.epoch_contract_id().clone(),
            reserved.next_turn_index(),
            reserved.last_request_id().cloned(),
            lease.clone(),
            *origin,
        );
        state.call_ledger.upsert(running.clone());
        let revision = Self::next_revision(&mut state)?;
        Ok(MountedCallStart::Started {
            state: running,
            session_revision: revision,
        })
    }

    async fn abandon_unstarted(
        &self,
        request: MountedCallLeaseRequest<'_, u64>,
    ) -> Result<MountedCallAbandon<u64>, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, request.session_id())?;
        if state.revision != *request.expected_revision() {
            return Ok(MountedCallAbandon::RevisionConflict);
        }
        let Some(reserved) = state.call_ledger.get(request.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        let DurableCallStatus::Reserved { lease, origin: _ } = reserved.status() else {
            return Ok(MountedCallAbandon::Existing(reserved));
        };
        if reserved.input_id() != request.input_id()
            || reserved.epoch_contract_id() != request.epoch_contract_id()
            || reserved.next_turn_index() != request.turn_index()
            || lease.lease_id() != request.lease_id()
            || lease.request_id() != request.request_id()
        {
            return Ok(MountedCallAbandon::Existing(reserved));
        }
        let Some(checkpoint) = reserved.retained_pre_provider_checkpoint() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        let restored = match checkpoint {
            DurableCallPreProviderCheckpoint::New => {
                state.call_ledger.remove(reserved.call_id());
                None
            }
            DurableCallPreProviderCheckpoint::AwaitingContinuation(restored) => {
                state.call_ledger.upsert(restored.clone());
                Some(restored)
            }
        };
        let revision = Self::next_revision(&mut state)?;
        Ok(MountedCallAbandon::Abandoned {
            state: restored,
            session_revision: revision,
        })
    }

    async fn claim_reconciliation(
        &self,
        request: MountedCallReconciliationClaimRequest<'_>,
    ) -> Result<MountedCallReconciliationAdmission<u64>, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let Some(session_id) = state.session_id.as_ref() else {
            return Err(InMemoryMountedStoreError::OwnerSessionMissing);
        };
        if session_id != request.session_id() {
            return Err(InMemoryMountedStoreError::SessionScope {
                expected: session_id.clone(),
                actual: request.session_id().clone(),
            });
        }
        let Some(call) = state.call_ledger.get(request.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        let InMemoryEpochState::Active(active_epoch) = &state.epoch else {
            return Ok(MountedCallReconciliationAdmission::Existing(call));
        };
        if active_epoch != request.expected_active_epoch()
            || active_epoch.rendered().manifest().epoch_contract_id() != call.epoch_contract_id()
            || state.pending_publication.is_some()
            || !matches!(call.status(), DurableCallStatus::RecoveryRequired { .. })
        {
            return Ok(MountedCallReconciliationAdmission::Existing(call));
        }
        if call
            .reconciliation_fence()
            .is_some_and(|fence| fence.is_live_at(self.now_unix_ms()))
        {
            return Ok(MountedCallReconciliationAdmission::Existing(call));
        }
        let fence =
            self.new_reconciliation_fence(&mut state, request.session_id(), call.call_id())?;
        let claimed = call
            .with_reconciliation_fence(fence.clone())
            .expect("a recovery-required call accepts a reconciliation fence");
        state.call_ledger.upsert(claimed.clone());
        let session_revision = Self::next_revision(&mut state)?;
        Ok(MountedCallReconciliationAdmission::Acquired(
            MountedCallReconciliationClaim::new(claimed, fence, session_revision),
        ))
    }

    async fn release_reconciliation(
        &self,
        request: MountedCallReconciliationReleaseRequest<'_, u64>,
    ) -> Result<MountedCallReconciliationRelease<u64>, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let Some(session_id) = state.session_id.as_ref() else {
            return Err(InMemoryMountedStoreError::OwnerSessionMissing);
        };
        if session_id != request.session_id() {
            return Err(InMemoryMountedStoreError::SessionScope {
                expected: session_id.clone(),
                actual: request.session_id().clone(),
            });
        }
        if state.revision != *request.expected_revision() {
            return Ok(MountedCallReconciliationRelease::RevisionConflict);
        }
        let Some(call) = state.call_ledger.get(request.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        let InMemoryEpochState::Active(active_epoch) = &state.epoch else {
            return Ok(MountedCallReconciliationRelease::Existing(call));
        };
        let Some(lease) = call.status().lease() else {
            return Ok(MountedCallReconciliationRelease::Existing(call));
        };
        if active_epoch != request.expected_active_epoch()
            || active_epoch.rendered().manifest().epoch_contract_id() != call.epoch_contract_id()
            || call.input_id() != request.input_id()
            || call.next_turn_index() != request.turn_index()
            || lease.request_id() != request.request_id()
            || lease.lease_id() != request.lease_id()
        {
            return Ok(MountedCallReconciliationRelease::Existing(call));
        }
        if !request.fence().is_live_at(self.now_unix_ms()) {
            return Ok(MountedCallReconciliationRelease::Existing(call));
        }
        let Some(released) = call.without_reconciliation_fence(request.fence()) else {
            return Ok(MountedCallReconciliationRelease::Existing(call));
        };
        state.call_ledger.upsert(released.clone());
        let session_revision = Self::next_revision(&mut state)?;
        Ok(MountedCallReconciliationRelease::Released {
            state: released,
            session_revision,
        })
    }

    async fn reconcile_never_accepted_operation(
        &self,
        request: MountedCallNeverAcceptedRecoveryRequest<'_, u64>,
    ) -> Result<MountedCallNeverAcceptedRecovery<u64>, Self::Error> {
        // The opaque proof is capability-based: only the provider operation
        // controller can create it from an authoritative NeverAccepted status.
        let _proof = request.proof();
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let Some(session_id) = state.session_id.as_ref() else {
            return Err(InMemoryMountedStoreError::OwnerSessionMissing);
        };
        if session_id != request.session_id() {
            return Err(InMemoryMountedStoreError::SessionScope {
                expected: session_id.clone(),
                actual: request.session_id().clone(),
            });
        }
        if state.revision != *request.expected_revision() {
            return Ok(MountedCallNeverAcceptedRecovery::RevisionConflict);
        }
        let Some(call) = state.call_ledger.get(request.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if call.input_id() != request.input_id()
            || call.epoch_contract_id() != request.epoch_contract_id()
            || call.next_turn_index() != request.turn_index()
        {
            return Ok(MountedCallNeverAcceptedRecovery::Existing(call));
        }
        let DurableCallStatus::RecoveryRequired { lease, .. } = call.status() else {
            return Ok(MountedCallNeverAcceptedRecovery::Existing(call));
        };
        if lease.lease_id() != request.lease_id() || lease.request_id() != request.request_id() {
            return Ok(MountedCallNeverAcceptedRecovery::Existing(call));
        }
        if call.reconciliation_fence() != Some(request.reconciliation_fence())
            || !request
                .reconciliation_fence()
                .is_live_at(self.now_unix_ms())
        {
            return Ok(MountedCallNeverAcceptedRecovery::Existing(call));
        }
        let InMemoryEpochState::Active(active_epoch) = &state.epoch else {
            return Ok(MountedCallNeverAcceptedRecovery::Existing(call));
        };
        if active_epoch != request.expected_active_epoch()
            || active_epoch.rendered().manifest().epoch_contract_id() != request.epoch_contract_id()
            || state.pending_publication.is_some()
        {
            return Ok(MountedCallNeverAcceptedRecovery::Existing(call));
        }
        let Some(checkpoint) = call.retained_pre_provider_checkpoint() else {
            return Ok(MountedCallNeverAcceptedRecovery::AmbiguousOrigin(call));
        };
        let restored = match checkpoint {
            DurableCallPreProviderCheckpoint::New => {
                state.call_ledger.remove(call.call_id());
                None
            }
            DurableCallPreProviderCheckpoint::AwaitingContinuation(restored) => {
                state.call_ledger.upsert(restored.clone());
                Some(restored)
            }
        };
        let session_revision = Self::next_revision(&mut state)?;
        Ok(MountedCallNeverAcceptedRecovery::Restored {
            state: restored,
            session_revision,
        })
    }

    async fn settle_cancelled_call(
        &self,
        request: MountedCallCancellationSettlementRequest<'_, u64>,
    ) -> Result<MountedCallCancellationSettlement<u64>, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, request.session_id())?;
        let Some(call) = state.call_ledger.get(request.call_id()).cloned() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if call.input_id() != request.input_id()
            || call.epoch_contract_id() != request.epoch_contract_id()
            || call.next_turn_index() != request.turn_index()
        {
            return Ok(MountedCallCancellationSettlement::Existing(call));
        }
        match call.status() {
            DurableCallStatus::RecoveryRequired { lease, .. }
                if lease.lease_id() == request.lease_id()
                    && lease.request_id() == request.request_id() =>
            {
                return Ok(MountedCallCancellationSettlement::RecoveryRequired {
                    state: call,
                    session_revision: state.revision,
                });
            }
            DurableCallStatus::Running { lease, .. }
                if lease.lease_id() == request.lease_id()
                    && lease.request_id() == request.request_id() => {}
            _ => return Ok(MountedCallCancellationSettlement::Existing(call)),
        }

        let lease = match call.status() {
            DurableCallStatus::Running { lease, .. } => lease.clone(),
            _ => unreachable!("the Running branch above retains the exact call lease"),
        };
        if state.revision != *request.expected_revision() {
            return Self::checkpoint_cancellation_recovery(
                &mut state,
                &call,
                &lease,
                &request,
                DurableCallRecoveryReason::CancellationSettlementConflict,
            );
        }
        if state.pending_publication.is_some() {
            return Self::checkpoint_cancellation_recovery(
                &mut state,
                &call,
                &lease,
                &request,
                DurableCallRecoveryReason::CancellationPendingPublication,
            );
        }
        if self.now_unix_ms() >= lease.expires_at_unix_ms() {
            return Self::checkpoint_cancellation_recovery(
                &mut state,
                &call,
                &lease,
                &request,
                DurableCallRecoveryReason::LeaseExpired,
            );
        }
        if let MountedCallCancellationDisposition::Recover(reason) = request.disposition() {
            return Self::checkpoint_cancellation_recovery(
                &mut state, &call, &lease, &request, *reason,
            );
        }

        let Some(active_epoch) = (match &state.epoch {
            InMemoryEpochState::Active(active_epoch) => Some(active_epoch.clone()),
            InMemoryEpochState::Empty
            | InMemoryEpochState::RenderStarted { .. }
            | InMemoryEpochState::Rendered { .. } => None,
        }) else {
            return Self::checkpoint_cancellation_recovery(
                &mut state,
                &call,
                &lease,
                &request,
                DurableCallRecoveryReason::CancellationEpochMismatch,
            );
        };
        if &active_epoch != request.expected_active_epoch()
            || state
                .provider_cursor
                .as_ref()
                .is_some_and(|cursor| active_epoch.validate_provider_cursor(cursor).is_err())
        {
            return Self::checkpoint_cancellation_recovery(
                &mut state,
                &call,
                &lease,
                &request,
                if &active_epoch != request.expected_active_epoch() {
                    DurableCallRecoveryReason::CancellationEpochMismatch
                } else {
                    DurableCallRecoveryReason::CancellationCursorInvalid
                },
            );
        }

        let next_cursor = match request.disposition() {
            MountedCallCancellationDisposition::StopUnchanged => state.provider_cursor.clone(),
            MountedCallCancellationDisposition::StopWithCursor(cursor) => {
                if super::mounted_agent::validate_provider_cursor_transition(
                    state.provider_cursor.as_ref(),
                    Some(cursor),
                )
                .is_err()
                    || active_epoch.validate_provider_cursor(cursor).is_err()
                {
                    return Self::checkpoint_cancellation_recovery(
                        &mut state,
                        &call,
                        &lease,
                        &request,
                        DurableCallRecoveryReason::CancellationCursorInvalid,
                    );
                }
                Some(cursor.clone())
            }
            MountedCallCancellationDisposition::Recover(_) => {
                unreachable!("recovery dispositions return before normal cancellation settlement")
            }
        };
        let stopped = DurableCallState::stopped(
            call.call_id().clone(),
            call.input_id().clone(),
            call.epoch_contract_id().clone(),
            call.next_turn_index(),
            call.last_request_id().cloned(),
        );
        let session_revision = Self::next_revision(&mut state)?;
        state.provider_cursor = next_cursor;
        let provider_cursor = state.provider_cursor.clone();
        state.call_ledger.upsert(stopped.clone());
        Ok(MountedCallCancellationSettlement::Stopped {
            state: stopped,
            provider_cursor,
            session_revision,
        })
    }

    async fn publish_claimed_turn(
        &self,
        expected_active_epoch: &ActiveEpochArtifact,
        request: PublicationRequest<'_, MountedSessionMutation<I, ContextState>, Payload, u64>,
    ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
        self.commit(expected_active_epoch, request)
            .map(InMemoryPublicationCommit::into_receipt)
    }

    async fn record_pending_publication(
        &self,
        identity: &MountedMutationIdentity,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<ClaimedCallPublication, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        let Some(running) = state.call_ledger.get(identity.call_id()) else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        let DurableCallStatus::Running { lease, .. } = running.status() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if state.session_id.as_ref() != Some(identity.session_id())
            || running.input_id() != identity.input_id()
            || running.epoch_contract_id() != identity.epoch_contract_id()
            || running.next_turn_index() != identity.turn_index()
            || lease.lease_id() != identity.lease_id()
            || lease.request_id() != identity.request_id()
        {
            return Err(InMemoryMountedStoreError::Rejected);
        }
        let pending = ClaimedCallPublication::new(
            identity.session_id().clone(),
            identity.epoch_contract_id().clone(),
            identity.call_id().clone(),
            identity.input_id().clone(),
            identity.turn_index(),
            lease.clone(),
            fingerprint.clone(),
        );
        match state.pending_publication.as_ref() {
            Some(existing) if existing == &pending => Ok(existing.clone()),
            Some(_) => Err(InMemoryMountedStoreError::Collision),
            None => {
                state.pending_publication = Some(pending.clone());
                Ok(pending)
            }
        }
    }

    async fn pending_publication(
        &self,
        session_id: &DurableSessionId,
    ) -> Result<Option<ClaimedCallPublication>, Self::Error> {
        let state = self.state.lock().expect("in-memory mounted store poisoned");
        if let Some(existing) = state.session_id.as_ref() {
            if existing != session_id {
                return Err(InMemoryMountedStoreError::SessionScope {
                    expected: existing.clone(),
                    actual: session_id.clone(),
                });
            }
        }
        Ok(state.pending_publication.clone())
    }

    async fn finalize_pending_publication(
        &self,
        publication: &ClaimedCallPublication,
        receipt: &PublicationReceipt<u64>,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        if receipt.request_id() != publication.request_id()
            || receipt.fingerprint() != publication.fingerprint()
            || !state.receipts.iter().any(|stored| stored == receipt)
        {
            return Err(InMemoryMountedStoreError::Rejected);
        }
        match state.pending_publication.as_ref() {
            None => Ok(()),
            Some(existing) if existing == publication => {
                state.pending_publication = None;
                Ok(())
            }
            Some(_) => Err(InMemoryMountedStoreError::Collision),
        }
    }

    async fn mark_pending_recovery(
        &self,
        publication: &ClaimedCallPublication,
        reason: DurableCallRecoveryReason,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::checkpoint_recovery(&mut state, publication, reason)
    }

    async fn reconcile_unowned_pending_publication(
        &self,
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
    ) -> Result<MountedPendingPublicationReconciliation, Self::Error> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, session_id)?;
        let Some(pending) = state.pending_publication.clone() else {
            return Ok(MountedPendingPublicationReconciliation::None);
        };
        if pending.epoch_contract_id() != epoch_contract_id {
            return Err(InMemoryMountedStoreError::Rejected);
        }
        if let Some(receipt) = state
            .receipts
            .iter()
            .find(|receipt| receipt.request_id() == pending.request_id())
        {
            if receipt.fingerprint() != pending.fingerprint() {
                Self::checkpoint_recovery(
                    &mut state,
                    &pending,
                    DurableCallRecoveryReason::PublicationCandidateCollision,
                )?;
                return Ok(
                    MountedPendingPublicationReconciliation::CandidateCollision {
                        request_id: pending.request_id().clone(),
                    },
                );
            }
            state.pending_publication = None;
            return Ok(MountedPendingPublicationReconciliation::Finalized);
        }
        let Some(call) = state.call_ledger.get(pending.call_id()) else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if let DurableCallStatus::RecoveryRequired { reason, .. } = call.status() {
            let reason = *reason;
            state.pending_publication = None;
            return Ok(MountedPendingPublicationReconciliation::RecoveryRequired { reason });
        }
        let DurableCallStatus::Running { lease, .. } = call.status() else {
            return Err(InMemoryMountedStoreError::Rejected);
        };
        if self.now_unix_ms() < lease.expires_at_unix_ms() {
            return Ok(MountedPendingPublicationReconciliation::InFlight {
                call_id: call.call_id().clone(),
                request_id: pending.request_id().clone(),
                expires_at_unix_ms: lease.expires_at_unix_ms(),
            });
        }
        Self::checkpoint_recovery(
            &mut state,
            &pending,
            DurableCallRecoveryReason::PublicationNotCommitted,
        )?;
        Ok(MountedPendingPublicationReconciliation::RecoveryRequired {
            reason: DurableCallRecoveryReason::PublicationNotCommitted,
        })
    }

    async fn resolve_publication(
        &self,
        request_id: &PublicationRequestId,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
        <Self as PublicationStore<MountedSessionMutation<I, ContextState>, Payload>>::resolve(
            self,
            request_id,
            fingerprint,
        )
        .await
    }

    async fn load_settled_call_result(
        &self,
        session_id: &DurableSessionId,
        call: &DurableCallState,
    ) -> Result<Option<StoredCallResult<u64>>, Self::Error> {
        let state = self.state.lock().expect("in-memory mounted store poisoned");
        if state.session_id.as_ref() != Some(session_id) {
            return Err(InMemoryMountedStoreError::Rejected);
        }
        Ok(state
            .settled_results
            .iter()
            .find(|result| {
                result.call_id() == call.call_id()
                    && result.input_id() == call.input_id()
                    && result.epoch_contract_id() == call.epoch_contract_id()
            })
            .cloned())
    }
}

#[async_trait::async_trait]
impl<I, ContextState, Payload, Backend> MountedSessionStore<I, ContextState, Payload, u64>
    for DurableBackendMountedStore<I, ContextState, Payload, Backend>
where
    I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Backend: DurableMountedStateBackend<Payload>,
{
    type Error = DurableBackendMountedStoreError<Backend::Error>;

    async fn claim_call(
        &self,
        request: MountedCallClaimRequest<'_, u64>,
    ) -> Result<MountedCallClaim<u64>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::claim_call(&store, request)
                .await;
                let deadline = match result.as_ref() {
                    Ok(MountedCallClaim::Acquired { .. }) => store.active_call_lease_deadline(),
                    _ => None,
                };
                let state = store.into_state();
                LocalStateTransaction::new(result, state, deadline, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn mark_provider_started(
        &self,
        request: MountedCallLeaseRequest<'_, u64>,
    ) -> Result<MountedCallStart<u64>, Self::Error> {
        self.transact(
            |store| async move {
                let deadline = store.active_call_lease_deadline();
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::mark_provider_started(&store, request)
                .await;
                let deadline = match result.as_ref() {
                    Ok(MountedCallStart::Started { .. }) => deadline,
                    _ => None,
                };
                let state = store.into_state();
                LocalStateTransaction::new(result, state, deadline, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn abandon_unstarted(
        &self,
        request: MountedCallLeaseRequest<'_, u64>,
    ) -> Result<MountedCallAbandon<u64>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::abandon_unstarted(&store, request)
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn claim_reconciliation(
        &self,
        request: MountedCallReconciliationClaimRequest<'_>,
    ) -> Result<MountedCallReconciliationAdmission<u64>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::claim_reconciliation(&store, request)
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn release_reconciliation(
        &self,
        request: MountedCallReconciliationReleaseRequest<'_, u64>,
    ) -> Result<MountedCallReconciliationRelease<u64>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::release_reconciliation(&store, request)
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn reconcile_never_accepted_operation(
        &self,
        request: MountedCallNeverAcceptedRecoveryRequest<'_, u64>,
    ) -> Result<MountedCallNeverAcceptedRecovery<u64>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::reconcile_never_accepted_operation(&store, request)
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn settle_cancelled_call(
        &self,
        request: MountedCallCancellationSettlementRequest<'_, u64>,
    ) -> Result<MountedCallCancellationSettlement<u64>, Self::Error> {
        self.transact(
            |store| {
                let request = request.clone();
                async move {
                    let deadline = store.active_call_lease_deadline();
                    let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                        I,
                        ContextState,
                        Payload,
                        u64,
                    >>::settle_cancelled_call(&store, request)
                    .await;
                    let deadline = match result.as_ref() {
                        Ok(MountedCallCancellationSettlement::Stopped { .. }) => deadline,
                        _ => None,
                    };
                    let state = store.into_state();
                    LocalStateTransaction::new(result, state, deadline, false)
                }
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn publish_claimed_turn(
        &self,
        expected_active_epoch: &ActiveEpochArtifact,
        request: PublicationRequest<'_, MountedSessionMutation<I, ContextState>, Payload, u64>,
    ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
        let result = self
            .transact(
                |store| async move {
                    let deadline = store.active_call_lease_deadline();
                    let result = store.commit(expected_active_epoch, request);
                    let write_outbox =
                        matches!(result.as_ref(), Ok(InMemoryPublicationCommit::Published(_)));
                    let deadline = write_outbox.then_some(deadline).flatten();
                    let state = store.into_state();
                    LocalStateTransaction::new(result, state, deadline, write_outbox)
                },
                Some(request.outbox()),
            )
            .await
            .map_err(PublicationWriteError::Indeterminate)?;
        result
            .map(InMemoryPublicationCommit::into_receipt)
            .map_err(DurableBackendMountedStoreError::publication_write)
    }

    async fn record_pending_publication(
        &self,
        identity: &MountedMutationIdentity,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<ClaimedCallPublication, Self::Error> {
        self.transact(
            |store| async move {
                let deadline = store.active_call_lease_deadline();
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::record_pending_publication(
                    &store, identity, fingerprint
                )
                .await;
                let deadline = result.as_ref().ok().and(deadline);
                let state = store.into_state();
                LocalStateTransaction::new(result, state, deadline, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn pending_publication(
        &self,
        session_id: &DurableSessionId,
    ) -> Result<Option<ClaimedCallPublication>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::pending_publication(&store, session_id)
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn finalize_pending_publication(
        &self,
        publication: &ClaimedCallPublication,
        receipt: &PublicationReceipt<u64>,
    ) -> Result<(), Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::finalize_pending_publication(
                    &store, publication, receipt
                )
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn mark_pending_recovery(
        &self,
        publication: &ClaimedCallPublication,
        reason: DurableCallRecoveryReason,
    ) -> Result<(), Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::mark_pending_recovery(&store, publication, reason)
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn reconcile_unowned_pending_publication(
        &self,
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
    ) -> Result<MountedPendingPublicationReconciliation, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::reconcile_unowned_pending_publication(
                    &store, session_id, epoch_contract_id
                )
                .await;
                let state = store.into_state();
                LocalStateTransaction::new(result, state, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn resolve_publication(
        &self,
        request_id: &PublicationRequestId,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
        let result = self
            .transact(
                |store| async move {
                    let result = <InMemoryMountedStore<I, ContextState> as PublicationStore<
                        MountedSessionMutation<I, ContextState>,
                        Payload,
                    >>::resolve(&store, request_id, fingerprint)
                    .await;
                    let state = store.into_state();
                    LocalStateTransaction::new(result, state, None, false)
                },
                None,
            )
            .await
            .map_err(PublicationResolveError::Unavailable)?;
        result.map_err(DurableBackendMountedStoreError::publication_resolve)
    }

    async fn load_settled_call_result(
        &self,
        session_id: &DurableSessionId,
        state: &DurableCallState,
    ) -> Result<Option<StoredCallResult<u64>>, Self::Error> {
        self.transact(
            |store| async move {
                let result = <InMemoryMountedStore<I, ContextState> as MountedSessionStore<
                    I,
                    ContextState,
                    Payload,
                    u64,
                >>::load_settled_call_result(&store, session_id, state)
                .await;
                let next = store.into_state();
                LocalStateTransaction::new(result, next, None, false)
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }
}

#[async_trait::async_trait]
impl<I, ContextState, Payload> MountedOwnerStore<I, ContextState, Payload, u64>
    for InMemoryMountedStore<I, ContextState>
where
    I: Clone + Send + Sync + 'static,
    ContextState: Clone + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
{
    type SnapshotError = InMemoryMountedStoreError;
    type ReconfigurationError = InMemoryMountedStoreError;

    async fn load_owner_snapshot(
        &self,
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
        initial_session: AgentSession<I, ContextState>,
    ) -> Result<MountedSessionSnapshot<I, ContextState, u64>, Self::SnapshotError> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, session_id)?;
        if state.session.is_none() {
            state.session = Some(initial_session);
        }
        Self::snapshot(&state, epoch_contract_id)
    }

    async fn load_active_owner_snapshot(
        &self,
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
        expected_active_epoch: &ActiveEpochArtifact,
    ) -> Result<MountedSessionSnapshot<I, ContextState, u64>, Self::SnapshotError> {
        let mut state = self.state.lock().expect("in-memory mounted store poisoned");
        Self::ensure_scope(&mut state, session_id)?;
        let InMemoryEpochState::Active(active) = &state.epoch else {
            return Err(InMemoryMountedStoreError::ActiveEpochMissing);
        };
        if active != expected_active_epoch
            || active.rendered().manifest().epoch_contract_id() != epoch_contract_id
        {
            return Err(InMemoryMountedStoreError::ActiveEpochMismatch);
        }
        if state
            .session
            .as_ref()
            .and_then(|session| session.context().system())
            != Some(active.rendered().rendered_system())
            || state
                .provider_cursor
                .as_ref()
                .is_some_and(|cursor| active.validate_provider_cursor(cursor).is_err())
        {
            return Err(InMemoryMountedStoreError::OwnerSystemMismatch);
        }
        Self::snapshot(&state, epoch_contract_id)
    }

    async fn load_epoch_reconfiguration_status(
        &self,
        _session_id: &DurableSessionId,
        _reconfiguration_id: &super::publication::EpochReconfigurationId,
    ) -> Result<MountedEpochReconfigurationSnapshot, Self::ReconfigurationError> {
        Err(InMemoryMountedStoreError::ReconfigurationUnsupported)
    }

    async fn reject_epoch_reconfiguration(
        &self,
        _request: MountedEpochReconfigurationRejectRequest<'_>,
    ) -> Result<MountedEpochReconfigurationRejectOutcome, Self::ReconfigurationError> {
        Err(InMemoryMountedStoreError::ReconfigurationUnsupported)
    }

    async fn acquire_epoch_reconfiguration(
        &self,
        _request: MountedEpochReconfigureRequest<'_, I, ContextState, u64>,
    ) -> Result<MountedEpochReconfigureAdmission, Self::ReconfigurationError> {
        Err(InMemoryMountedStoreError::ReconfigurationUnsupported)
    }

    async fn store_reconfigured_rendered_epoch(
        &self,
        _reconfiguration_id: &super::publication::EpochReconfigurationId,
        _fence: &EpochOpenFence,
        _artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::ReconfigurationError> {
        Err(InMemoryMountedStoreError::ReconfigurationUnsupported)
    }

    async fn activate_epoch_reconfiguration(
        &self,
        _reconfiguration_id: &super::publication::EpochReconfigurationId,
        _fence: &EpochOpenFence,
        _artifact: &ActiveEpochArtifact,
    ) -> Result<(), Self::ReconfigurationError> {
        Err(InMemoryMountedStoreError::ReconfigurationUnsupported)
    }

    async fn recover_epoch_reconfiguration(
        &self,
        _request: MountedEpochReconfigureRecoveryRequest<'_>,
    ) -> Result<
        MountedEpochReconfigureRecoveryOutcome<I, ContextState, u64>,
        Self::ReconfigurationError,
    > {
        Err(InMemoryMountedStoreError::ReconfigurationUnsupported)
    }
}

#[async_trait::async_trait]
impl<I, ContextState, Payload, Backend> MountedOwnerStore<I, ContextState, Payload, u64>
    for DurableBackendMountedStore<I, ContextState, Payload, Backend>
where
    I: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    ContextState: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Backend: DurableMountedStateBackend<Payload>,
{
    type SnapshotError = DurableBackendMountedStoreError<Backend::Error>;
    type ReconfigurationError = DurableBackendMountedStoreError<Backend::Error>;

    async fn load_owner_snapshot(
        &self,
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
        initial_session: AgentSession<I, ContextState>,
    ) -> Result<MountedSessionSnapshot<I, ContextState, u64>, Self::SnapshotError>
    where
        I: Clone,
        ContextState: Clone,
    {
        let session_id = session_id.clone();
        let epoch_contract_id = epoch_contract_id.clone();
        self.transact(
            move |store| {
                let session_id = session_id.clone();
                let epoch_contract_id = epoch_contract_id.clone();
                let initial_session = initial_session.clone();
                async move {
                    let result = <InMemoryMountedStore<I, ContextState> as MountedOwnerStore<
                        I,
                        ContextState,
                        Payload,
                        u64,
                    >>::load_owner_snapshot(
                        &store,
                        &session_id,
                        &epoch_contract_id,
                        initial_session,
                    )
                    .await;
                    let state = store.into_state();
                    LocalStateTransaction::new(result, state, None, false)
                }
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn load_active_owner_snapshot(
        &self,
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
        expected_active_epoch: &ActiveEpochArtifact,
    ) -> Result<MountedSessionSnapshot<I, ContextState, u64>, Self::SnapshotError>
    where
        I: Clone,
        ContextState: Clone,
    {
        let session_id = session_id.clone();
        let epoch_contract_id = epoch_contract_id.clone();
        let expected_active_epoch = expected_active_epoch.clone();
        self.transact(
            move |store| {
                let session_id = session_id.clone();
                let epoch_contract_id = epoch_contract_id.clone();
                let expected_active_epoch = expected_active_epoch.clone();
                async move {
                    let result = <InMemoryMountedStore<I, ContextState> as MountedOwnerStore<
                        I,
                        ContextState,
                        Payload,
                        u64,
                    >>::load_active_owner_snapshot(
                        &store,
                        &session_id,
                        &epoch_contract_id,
                        &expected_active_epoch,
                    )
                    .await;
                    let state = store.into_state();
                    LocalStateTransaction::new(result, state, None, false)
                }
            },
            None,
        )
        .await?
        .map_err(DurableBackendMountedStoreError::Transition)
    }

    async fn load_epoch_reconfiguration_status(
        &self,
        _session_id: &DurableSessionId,
        _reconfiguration_id: &super::publication::EpochReconfigurationId,
    ) -> Result<MountedEpochReconfigurationSnapshot, Self::ReconfigurationError> {
        Err(DurableBackendMountedStoreError::Transition(
            InMemoryMountedStoreError::ReconfigurationUnsupported,
        ))
    }

    async fn reject_epoch_reconfiguration(
        &self,
        _request: MountedEpochReconfigurationRejectRequest<'_>,
    ) -> Result<MountedEpochReconfigurationRejectOutcome, Self::ReconfigurationError> {
        Err(DurableBackendMountedStoreError::Transition(
            InMemoryMountedStoreError::ReconfigurationUnsupported,
        ))
    }

    async fn acquire_epoch_reconfiguration(
        &self,
        _request: MountedEpochReconfigureRequest<'_, I, ContextState, u64>,
    ) -> Result<MountedEpochReconfigureAdmission, Self::ReconfigurationError>
    where
        I: Clone,
        ContextState: Clone,
    {
        Err(DurableBackendMountedStoreError::Transition(
            InMemoryMountedStoreError::ReconfigurationUnsupported,
        ))
    }

    async fn store_reconfigured_rendered_epoch(
        &self,
        _reconfiguration_id: &super::publication::EpochReconfigurationId,
        _fence: &EpochOpenFence,
        _artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::ReconfigurationError> {
        Err(DurableBackendMountedStoreError::Transition(
            InMemoryMountedStoreError::ReconfigurationUnsupported,
        ))
    }

    async fn activate_epoch_reconfiguration(
        &self,
        _reconfiguration_id: &super::publication::EpochReconfigurationId,
        _fence: &EpochOpenFence,
        _artifact: &ActiveEpochArtifact,
    ) -> Result<(), Self::ReconfigurationError> {
        Err(DurableBackendMountedStoreError::Transition(
            InMemoryMountedStoreError::ReconfigurationUnsupported,
        ))
    }

    async fn recover_epoch_reconfiguration(
        &self,
        _request: MountedEpochReconfigureRecoveryRequest<'_>,
    ) -> Result<
        MountedEpochReconfigureRecoveryOutcome<I, ContextState, u64>,
        Self::ReconfigurationError,
    >
    where
        I: Clone,
        ContextState: Clone,
    {
        Err(DurableBackendMountedStoreError::Transition(
            InMemoryMountedStoreError::ReconfigurationUnsupported,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, convert::Infallible};

    use serde_json::{json, Map};

    use super::super::publication::{stage_commit_outbox, PreparedSessionMutation};
    use super::super::{
        provider_wire::ProviderOperationStatus, DurableCallId, DurableCallInputId,
        DurableCallLease, DurableEpochId, NoTurnChannels, ProviderAdapterContract,
        ProviderEpochReceipt, ProviderToolCall, ProviderToolResponse, ProviderToolResult,
    };
    use super::*;
    use crate::{
        pom::{DiffSlot, DiffStrategy, Document, XmlNode},
        pom_resolution::resolve_user_document,
        prompt_context::PromptContext,
    };

    #[test]
    fn durable_fingerprint_json_is_stable_across_hashmap_reconstruction() {
        let mut first_context = HashMap::new();
        first_context.insert("zebra", "last");
        first_context.insert("ant", "first");
        let mut first = HashMap::new();
        first.insert("context", first_context);

        let mut second_context = HashMap::new();
        second_context.insert("ant", "first");
        second_context.insert("zebra", "last");
        let mut second = HashMap::new();
        second.insert("context", second_context);

        let first = canonical_durable_json_bytes(&first).unwrap();
        let second = canonical_durable_json_bytes(&second).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            std::str::from_utf8(&first).unwrap(),
            r#"{"context":{"ant":"first","zebra":"last"}}"#
        );
    }

    struct EmptyCommitStager;

    impl CommitStager<NoTurnChannels> for EmptyCommitStager {
        type Payload = Never;
        type Error = Infallible;

        fn stage(
            &self,
            _context: CommitStagingContext<'_>,
            commit: &Never,
        ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
            match *commit {}
        }
    }

    struct EmptyOutboxFingerprint;

    impl DurableOutboxFingerprint<Never> for EmptyOutboxFingerprint {
        type Error = Infallible;

        fn fingerprint(
            &self,
            outbox: &StagedOutbox<Never>,
        ) -> Result<PublicationCandidateFingerprint, Self::Error> {
            assert!(outbox.is_empty());
            Ok(PublicationCandidateFingerprint::new("empty-outbox/v1").unwrap())
        }
    }

    fn nested_map(reverse: bool) -> HashMap<String, HashMap<String, String>> {
        let mut inner = HashMap::new();
        let mut outer = HashMap::new();
        if reverse {
            inner.insert("zebra".to_owned(), "last".to_owned());
            inner.insert("ant".to_owned(), "first".to_owned());
            outer.insert("context".to_owned(), inner);
        } else {
            inner.insert("ant".to_owned(), "first".to_owned());
            inner.insert("zebra".to_owned(), "last".to_owned());
            outer.insert("context".to_owned(), inner);
        }
        outer
    }

    fn nested_json_object(reverse: bool) -> Value {
        let mut inner = Map::new();
        let mut outer = Map::new();
        if reverse {
            inner.insert("zebra".to_owned(), json!("last"));
            inner.insert("ant".to_owned(), json!("first"));
        } else {
            inner.insert("ant".to_owned(), json!("first"));
            inner.insert("zebra".to_owned(), json!("last"));
        }
        outer.insert("context".to_owned(), Value::Object(inner));
        Value::Object(outer)
    }

    fn provider_results(reverse: bool) -> Vec<ProviderToolResult> {
        let success_call = ProviderToolCall::new("success-call", "lookup", json!({}));
        let error_call = ProviderToolCall::new("error-call", "validate", json!({}));
        vec![
            ProviderToolResult::new(
                &success_call,
                ProviderToolResponse::success(nested_json_object(reverse)),
            ),
            ProviderToolResult::new(
                &error_call,
                ProviderToolResponse::error_with_details(
                    "invalid",
                    "validation failed",
                    nested_json_object(reverse),
                ),
            ),
        ]
    }

    #[test]
    fn final_durable_fingerprint_is_stable_for_unordered_mutation_and_provider_json() {
        let request_id = PublicationRequestId::new("canonical-fingerprint-request").unwrap();
        let commits: [Never; 0] = [];
        let outbox = stage_commit_outbox::<NoTurnChannels, _>(
            request_id.clone(),
            &commits,
            &EmptyCommitStager,
        )
        .unwrap();
        let revision = 7;
        let first_mutation = PreparedSessionMutation::new(nested_map(false));
        let second_mutation = PreparedSessionMutation::new(nested_map(true));
        let first_results = provider_results(false);
        let second_results = provider_results(true);

        let first = fingerprint_durable_mounted_publication(
            &EmptyOutboxFingerprint,
            PublicationFingerprintContext::new(
                &request_id,
                &revision,
                &first_mutation,
                "raw-output",
                &first_results,
                &outbox,
            ),
        )
        .unwrap();
        let second = fingerprint_durable_mounted_publication(
            &EmptyOutboxFingerprint,
            PublicationFingerprintContext::new(
                &request_id,
                &revision,
                &second_mutation,
                "raw-output",
                &second_results,
                &outbox,
            ),
        )
        .unwrap();

        assert_eq!(first, second);
        assert!(first.as_str().starts_with("sha256:v2:"));
    }

    #[derive(Default)]
    struct CursorBackend {
        row: Mutex<CursorBackendRow>,
    }

    #[derive(Default)]
    struct CursorBackendRow {
        generation: u64,
        state: Option<MountedStateBlob>,
        return_stale_generation_once: bool,
    }

    impl CursorBackend {
        fn snapshot(row: &CursorBackendRow) -> MountedStateSnapshot {
            match &row.state {
                Some(state) => MountedStateSnapshot::present(
                    MountedStateGeneration::new(format!("cursor-generation-{}", row.generation))
                        .unwrap(),
                    state.clone(),
                    10_000,
                ),
                None => MountedStateSnapshot::missing(10_000),
            }
        }
    }

    #[async_trait::async_trait]
    impl DurableMountedStateBackend<Never> for CursorBackend {
        type Error = Infallible;

        async fn load(
            &self,
            _session_id: &DurableSessionId,
        ) -> Result<MountedStateSnapshot, Self::Error> {
            Ok(Self::snapshot(&self.row.lock().unwrap()))
        }

        async fn compare_exchange(
            &self,
            request: MountedStateWrite<'_, Never>,
        ) -> Result<MountedStateWriteOutcome, Self::Error> {
            assert!(request.outbox().is_none());
            let mut row = self.row.lock().unwrap();
            let current = Self::snapshot(&row);
            if request.expected_generation() != current.generation() {
                return Ok(MountedStateWriteOutcome::conflict(current));
            }
            let stale_generation = row.return_stale_generation_once.then(|| {
                current
                    .generation()
                    .expect("a stale-generation response requires an existing row")
                    .clone()
            });
            row.return_stale_generation_once = false;
            row.generation += 1;
            row.state = Some(request.state().clone());
            Ok(MountedStateWriteOutcome::committed(
                stale_generation.unwrap_or_else(|| {
                    MountedStateGeneration::new(format!("cursor-generation-{}", row.generation))
                        .unwrap()
                }),
                10_000,
            ))
        }
    }

    #[test]
    fn durable_backend_rejects_v1_state_before_candidate_recomputation() {
        let backend = Arc::new(CursorBackend::default());
        let session_id = DurableSessionId::new("legacy-schema/session").unwrap();
        let store = DurableBackendMountedStore::<String, usize, Never, _>::new(
            backend,
            session_id,
            Duration::from_secs(60),
        );
        let legacy = serde_json::to_vec(&DurableMountedStateEnvelope {
            schema_version: 1,
            state: InMemoryMountedState::<String, usize>::default(),
            user_document_cursor: UserDocumentCursor::default(),
        })
        .unwrap();
        let snapshot = MountedStateSnapshot::present(
            MountedStateGeneration::new("legacy-schema-generation").unwrap(),
            MountedStateBlob::new(legacy),
            10_000,
        );

        let error = match store.decode_state(&snapshot) {
            Ok(_) => panic!("v1 durable state must require an explicit migration"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            DurableBackendMountedStoreError::Schema {
                expected: 3,
                actual: 1,
            }
        ));
    }

    #[tokio::test]
    async fn durable_backend_migrates_v2_with_a_full_user_resync_then_writes_v3() {
        let backend = Arc::new(CursorBackend::default());
        let session_id = DurableSessionId::new("v2-schema/session").unwrap();
        let store = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            session_id.clone(),
            Duration::from_secs(60),
        );
        let legacy_state = InMemoryMountedState {
            session_id: Some(session_id.clone()),
            session: Some(session_with_user_cursor()),
            ..Default::default()
        };
        let legacy = serde_json::to_vec(&json!({
            "schema_version": 2,
            "state": legacy_state,
        }))
        .unwrap();
        let snapshot = MountedStateSnapshot::present(
            MountedStateGeneration::new("v2-schema-generation").unwrap(),
            MountedStateBlob::new(legacy),
            10_000,
        );

        let migrated = store.decode_state(&snapshot).unwrap();
        assert!(migrated
            .session
            .as_ref()
            .expect("v2 fixture stores a session")
            .user_document_cursor()
            .is_empty());

        store
            .transact(
                move |_| {
                    let migrated = migrated.clone();
                    async move {
                        LocalStateTransaction::new(Ok::<_, Infallible>(()), migrated, None, false)
                    }
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();

        let rewritten = backend.load(&session_id).await.unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(rewritten.state().unwrap().as_bytes()).unwrap();
        assert_eq!(rewritten["schema_version"], 3);
        assert!(rewritten["user_document_cursor"]["slots"].is_object());
    }

    fn session_with_user_cursor() -> AgentSession<String, usize> {
        let mut session = AgentSession::new(PromptContext::without_system());
        let document = Document::build(|blocks| {
            blocks.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                XmlNode::try_build("agent_context", |children| {
                    children.text(crate::pom::TextNode::new("cursor-baseline-sentinel"));
                    Ok(())
                })
                .unwrap(),
            ));
        });
        let (_, cursor) = resolve_user_document(document, &UserDocumentCursor::default()).unwrap();
        session.set_user_document_cursor(cursor);
        session
    }

    struct CancellationSettlementFixture {
        store: InMemoryMountedStore<String, usize>,
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        active_epoch: ActiveEpochArtifact,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        request_id: PublicationRequestId,
        lease_id: DurableCallLeaseId,
        lease: DurableCallLease,
        expected_revision: u64,
    }

    impl CancellationSettlementFixture {
        fn new(label: &str) -> Self {
            Self::with_lease_expiry(label, u64::MAX)
        }

        fn with_lease_expiry(label: &str, expires_at_unix_ms: u64) -> Self {
            let session_id = DurableSessionId::new(format!("local/{label}")).unwrap();
            let epoch_contract_id = EpochContractId::new(format!("local/{label}/v1")).unwrap();
            let active_epoch =
                active_epoch(epoch_contract_id.clone(), &format!("local/{label}/epoch"));
            let call_id = DurableCallId::new(format!("{label}-call")).unwrap();
            let input_id = DurableCallInputId::new(format!("{label}-call/input-v1")).unwrap();
            let request_id = PublicationRequestId::new(format!("{label}-request")).unwrap();
            let lease_id = DurableCallLeaseId::new(format!("{label}-lease")).unwrap();
            let lease =
                DurableCallLease::new(lease_id.clone(), request_id.clone(), expires_at_unix_ms);
            let expected_revision = 41;
            let running = DurableCallState::running(
                call_id.clone(),
                input_id.clone(),
                epoch_contract_id.clone(),
                0,
                None,
                lease.clone(),
                DurableCallReservationOrigin::New,
            );
            let store = InMemoryMountedStore::default();
            {
                let mut state = store.state.lock().unwrap();
                state.session_id = Some(session_id.clone());
                state.revision = expected_revision;
                state.epoch = InMemoryEpochState::Active(active_epoch.clone());
                state.call_ledger.upsert(running);
            }
            Self {
                store,
                session_id,
                epoch_contract_id,
                active_epoch,
                call_id,
                input_id,
                request_id,
                lease_id,
                lease,
                expected_revision,
            }
        }

        fn request(
            &self,
            disposition: MountedCallCancellationDisposition,
        ) -> MountedCallCancellationSettlementRequest<'_, u64> {
            MountedCallCancellationSettlementRequest::new(
                &self.session_id,
                &self.active_epoch,
                &self.epoch_contract_id,
                &self.call_id,
                &self.input_id,
                0,
                &self.request_id,
                &self.lease_id,
                &self.expected_revision,
                disposition,
            )
        }
    }

    struct NeverAcceptedRecoveryFixture {
        store: InMemoryMountedStore<String, usize>,
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        active_epoch: ActiveEpochArtifact,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        turn_index: u64,
        request_id: PublicationRequestId,
        lease_id: DurableCallLeaseId,
        lease: DurableCallLease,
        reconciliation_fence: MountedCallReconciliationFence,
        expected_revision: u64,
        prior_request_id: Option<PublicationRequestId>,
    }

    impl NeverAcceptedRecoveryFixture {
        fn new(label: &str, origin: DurableCallReservationOrigin) -> Self {
            Self::with_reconciliation_lease_duration(label, origin, LOCAL_LEASE_DURATION)
        }

        fn with_reconciliation_lease_duration(
            label: &str,
            origin: DurableCallReservationOrigin,
            reconciliation_lease_duration: Duration,
        ) -> Self {
            let session_id = DurableSessionId::new(format!("never-accepted/{label}")).unwrap();
            let epoch_contract_id =
                EpochContractId::new(format!("never-accepted/{label}/v1")).unwrap();
            let active_epoch = active_epoch(
                epoch_contract_id.clone(),
                &format!("never-accepted/{label}/epoch"),
            );
            let call_id = DurableCallId::new(format!("never-accepted/{label}/call")).unwrap();
            let input_id =
                DurableCallInputId::new(format!("never-accepted/{label}/call/input-v1")).unwrap();
            let request_id =
                PublicationRequestId::new(format!("never-accepted/{label}/provider-request"))
                    .unwrap();
            let lease_id =
                DurableCallLeaseId::new(format!("never-accepted/{label}/lease")).unwrap();
            let lease = DurableCallLease::new(lease_id.clone(), request_id.clone(), u64::MAX);
            let turn_index = matches!(origin, DurableCallReservationOrigin::AwaitingContinuation)
                .then_some(1)
                .unwrap_or(0);
            let prior_request_id =
                matches!(origin, DurableCallReservationOrigin::AwaitingContinuation).then(|| {
                    PublicationRequestId::new(format!("never-accepted/{label}/prior-request"))
                        .unwrap()
                });
            let running = DurableCallState::running(
                call_id.clone(),
                input_id.clone(),
                epoch_contract_id.clone(),
                turn_index,
                prior_request_id.clone(),
                lease.clone(),
                origin,
            );
            let recovery =
                running.recovery_required(lease.clone(), DurableCallRecoveryReason::LeaseExpired);
            let reconciliation_fence = MountedCallReconciliationFence::new(
                DurableCallLeaseId::new(format!("never-accepted/{label}/reconciliation-fence"))
                    .unwrap(),
                u64::MAX,
            );
            let recovery = recovery
                .with_reconciliation_fence(reconciliation_fence.clone())
                .expect("a recovery call accepts its fixture reconciliation fence");
            let expected_revision = 41;
            let store = InMemoryMountedStore::from_state_at(
                InMemoryMountedState::default(),
                1_000,
                reconciliation_lease_duration,
            );
            {
                let mut state = store.state.lock().unwrap();
                state.session_id = Some(session_id.clone());
                state.revision = expected_revision;
                state.epoch = InMemoryEpochState::Active(active_epoch.clone());
                state.call_ledger.upsert(recovery);
            }
            Self {
                store,
                session_id,
                epoch_contract_id,
                active_epoch,
                call_id,
                input_id,
                turn_index,
                request_id,
                lease_id,
                lease,
                reconciliation_fence,
                expected_revision,
                prior_request_id,
            }
        }

        fn request(&self) -> MountedCallNeverAcceptedRecoveryRequest<'_, u64> {
            MountedCallNeverAcceptedRecoveryRequest::new(
                &self.session_id,
                &self.active_epoch,
                &self.epoch_contract_id,
                &self.call_id,
                &self.input_id,
                self.turn_index,
                &self.request_id,
                &self.lease_id,
                &self.expected_revision,
                &self.reconciliation_fence,
                &ProviderOperationStatus::NeverAccepted,
            )
            .expect("NeverAccepted status must mint the opaque recovery proof")
        }
    }

    fn legacy_recovery_without_origin(recovery: &DurableCallState) -> DurableCallState {
        let mut encoded = serde_json::to_value(recovery).unwrap();
        encoded["status"]["RecoveryRequired"]
            .as_object_mut()
            .expect("recovery state serializes its tagged status as an object")
            .remove("origin");
        serde_json::from_value(encoded).unwrap()
    }

    fn active_epoch(
        epoch_contract_id: EpochContractId,
        durable_epoch_id: &str,
    ) -> ActiveEpochArtifact {
        let provider_adapter = ProviderAdapterContract::new("test-provider", 1).unwrap();
        let manifest = EpochContractManifest::new(
            epoch_contract_id,
            "test-host/v1",
            "test-runtime/v1",
            std::num::NonZeroUsize::MIN,
            provider_adapter.clone(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let durable_epoch_id = DurableEpochId::new(durable_epoch_id).unwrap();
        let rendered = RenderedEpochArtifact::new(
            durable_epoch_id.clone(),
            manifest,
            "<policy>local cancellation parity</policy>",
        )
        .unwrap();
        let receipt = ProviderEpochReceipt::new(
            provider_adapter.adapter(),
            provider_adapter.receipt_schema_version(),
            durable_epoch_id,
            rendered.fingerprint().clone(),
            json!({ "test": "provider-receipt" }),
        )
        .unwrap();
        ActiveEpochArtifact::new(rendered, receipt).unwrap()
    }

    fn invalid_cursor(fixture: &CancellationSettlementFixture) -> ProviderTurnCursor {
        ProviderTurnCursor::new(
            "another-provider",
            1,
            fixture.active_epoch.rendered().durable_epoch_id().clone(),
            fixture.active_epoch.rendered().fingerprint().clone(),
            json!({ "provider_turn": 7 }),
        )
        .unwrap()
    }

    fn assert_recovery(
        outcome: MountedCallCancellationSettlement<u64>,
        expected_reason: DurableCallRecoveryReason,
    ) -> u64 {
        let MountedCallCancellationSettlement::RecoveryRequired {
            state,
            session_revision,
        } = outcome
        else {
            panic!("cancellation fault must fence the call with durable recovery")
        };
        assert!(matches!(
            state.status(),
            DurableCallStatus::RecoveryRequired { reason, .. } if *reason == expected_reason
        ));
        session_revision
    }

    async fn settle_test_cancellation(
        store: &InMemoryMountedStore<String, usize>,
        request: MountedCallCancellationSettlementRequest<'_, u64>,
    ) -> Result<MountedCallCancellationSettlement<u64>, InMemoryMountedStoreError> {
        <InMemoryMountedStore<String, usize> as MountedSessionStore<String, usize, String, u64>>::settle_cancelled_call(
            store, request,
        )
        .await
    }

    async fn reconcile_test_never_accepted(
        store: &InMemoryMountedStore<String, usize>,
        request: MountedCallNeverAcceptedRecoveryRequest<'_, u64>,
    ) -> Result<MountedCallNeverAcceptedRecovery<u64>, InMemoryMountedStoreError> {
        <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::reconcile_never_accepted_operation(store, request)
        .await
    }

    #[tokio::test]
    async fn reconciliation_claim_serializes_inspection_and_release_preserves_recovery() {
        let fixture =
            NeverAcceptedRecoveryFixture::new("claim-release", DurableCallReservationOrigin::New);
        {
            let mut state = fixture.store.state.lock().unwrap();
            let recovery = state
                .call_ledger
                .get(&fixture.call_id)
                .expect("fixture installs a recovery call")
                .clone();
            state.call_ledger.upsert(
                recovery
                    .without_reconciliation_fence(&fixture.reconciliation_fence)
                    .expect("fixture recovery has its bootstrap fence"),
            );
        }

        let claim = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::claim_reconciliation(
            &fixture.store,
            MountedCallReconciliationClaimRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.call_id,
            ),
        )
        .await
        .unwrap();
        let MountedCallReconciliationAdmission::Acquired(claim) = claim else {
            panic!("an unfenced recovery call must grant one reconciliation claim")
        };
        assert_eq!(claim.session_revision(), &(fixture.expected_revision + 1));
        assert_eq!(claim.state().reconciliation_fence(), Some(claim.fence()));

        let duplicate = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::claim_reconciliation(
            &fixture.store,
            MountedCallReconciliationClaimRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.call_id,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            duplicate,
            MountedCallReconciliationAdmission::Existing(ref state)
                if state.reconciliation_fence() == Some(claim.fence())
        ));

        let release = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::release_reconciliation(
            &fixture.store,
            MountedCallReconciliationReleaseRequest::from_claim(
                &fixture.session_id,
                &fixture.active_epoch,
                &claim,
            )
            .expect("a reconciliation claim carries its provider operation lease"),
        )
        .await
        .unwrap();
        let MountedCallReconciliationRelease::Released {
            state,
            session_revision,
        } = release
        else {
            panic!("the owning reconciliation claim must be releasable")
        };
        assert_eq!(session_revision, fixture.expected_revision + 2);
        assert!(matches!(
            state.status(),
            DurableCallStatus::RecoveryRequired { .. }
        ));
        assert!(state.reconciliation_fence().is_none());
    }

    #[tokio::test]
    async fn expired_reconciliation_claim_cannot_restore_or_release_recovery() {
        let fixture = NeverAcceptedRecoveryFixture::with_reconciliation_lease_duration(
            "expired-claim",
            DurableCallReservationOrigin::New,
            Duration::from_millis(0),
        );
        {
            let mut state = fixture.store.state.lock().unwrap();
            let recovery = state
                .call_ledger
                .get(&fixture.call_id)
                .expect("fixture installs a recovery call")
                .clone();
            state.call_ledger.upsert(
                recovery
                    .without_reconciliation_fence(&fixture.reconciliation_fence)
                    .expect("fixture recovery has its bootstrap fence"),
            );
        }

        let claim = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::claim_reconciliation(
            &fixture.store,
            MountedCallReconciliationClaimRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.call_id,
            ),
        )
        .await
        .unwrap();
        let MountedCallReconciliationAdmission::Acquired(claim) = claim else {
            panic!("an unfenced recovery call must grant one reconciliation claim")
        };
        assert!(!claim.fence().is_live_at(1_000));

        let release = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::release_reconciliation(
            &fixture.store,
            MountedCallReconciliationReleaseRequest::from_claim(
                &fixture.session_id,
                &fixture.active_epoch,
                &claim,
            )
            .expect("a reconciliation claim carries its provider operation lease"),
        )
        .await
        .unwrap();
        assert!(matches!(
            release,
            MountedCallReconciliationRelease::Existing(ref state)
                if state.reconciliation_fence() == Some(claim.fence())
        ));

        let recovery = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::reconcile_never_accepted_operation(
            &fixture.store,
            MountedCallNeverAcceptedRecoveryRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.epoch_contract_id,
                &fixture.call_id,
                &fixture.input_id,
                fixture.turn_index,
                &fixture.request_id,
                &fixture.lease_id,
                claim.session_revision(),
                claim.fence(),
                &ProviderOperationStatus::NeverAccepted,
            )
            .expect("NeverAccepted status must mint the opaque recovery proof"),
        )
        .await
        .unwrap();
        assert!(matches!(
            recovery,
            MountedCallNeverAcceptedRecovery::Existing(ref state)
                if state.reconciliation_fence() == Some(claim.fence())
        ));

        let state = fixture.store.state.lock().unwrap();
        assert_eq!(state.revision, fixture.expected_revision + 1);
        let retained = state
            .call_ledger
            .get(&fixture.call_id)
            .expect("a stale controller must leave the recovery call fenced");
        assert!(matches!(
            retained.status(),
            DurableCallStatus::RecoveryRequired { .. }
        ));
        assert_eq!(retained.reconciliation_fence(), Some(claim.fence()));
    }

    #[tokio::test]
    async fn never_accepted_recovery_removes_a_new_call_and_allows_a_later_claim() {
        let fixture =
            NeverAcceptedRecoveryFixture::new("new-call", DurableCallReservationOrigin::New);

        let outcome = reconcile_test_never_accepted(&fixture.store, fixture.request())
            .await
            .unwrap();
        let restored_revision = match outcome {
            MountedCallNeverAcceptedRecovery::Restored {
                state: None,
                session_revision,
            } => session_revision,
            _ => panic!("a proven unaccepted new call must be removed"),
        };
        {
            let state = fixture.store.state.lock().unwrap();
            assert_eq!(state.revision, restored_revision);
            assert!(state.call_ledger.get(&fixture.call_id).is_none());
        }

        let retry_request =
            PublicationRequestId::new("never-accepted/new-call/retry-request").unwrap();
        let claim = <InMemoryMountedStore<String, usize> as MountedSessionStore<
            String,
            usize,
            String,
            u64,
        >>::claim_call(
            &fixture.store,
            MountedCallClaimRequest::new(
                &fixture.session_id,
                &fixture.epoch_contract_id,
                &fixture.call_id,
                &fixture.input_id,
                0,
                &retry_request,
                &restored_revision,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            claim,
            MountedCallClaim::Acquired { state, .. }
                if matches!(state.status(), DurableCallStatus::Reserved {
                    origin: DurableCallReservationOrigin::New,
                    ..
                })
        ));
    }

    #[tokio::test]
    async fn never_accepted_recovery_restores_the_continuation_checkpoint() {
        let fixture = NeverAcceptedRecoveryFixture::new(
            "continuation",
            DurableCallReservationOrigin::AwaitingContinuation,
        );
        let prior_request_id = fixture
            .prior_request_id
            .as_ref()
            .expect("continuation fixture retains the prior publication id")
            .clone();

        let outcome = reconcile_test_never_accepted(&fixture.store, fixture.request())
            .await
            .unwrap();
        let restored_revision = match outcome {
            MountedCallNeverAcceptedRecovery::Restored {
                state: Some(restored),
                session_revision,
            } => {
                assert!(matches!(
                    restored.status(),
                    DurableCallStatus::AwaitingContinuation
                ));
                assert_eq!(restored.next_turn_index(), fixture.turn_index);
                assert_eq!(restored.last_request_id(), Some(&prior_request_id));
                session_revision
            }
            _ => panic!("a proven unaccepted continuation must restore its prior checkpoint"),
        };
        let state = fixture.store.state.lock().unwrap();
        let restored = state
            .call_ledger
            .get(&fixture.call_id)
            .expect("continuation checkpoint remains in the durable ledger");
        assert_eq!(state.revision, restored_revision);
        assert!(matches!(
            restored.status(),
            DurableCallStatus::AwaitingContinuation
        ));
        assert_eq!(restored.last_request_id(), Some(&prior_request_id));
    }

    #[tokio::test]
    async fn never_accepted_recovery_keeps_legacy_origin_records_fenced() {
        let fixture =
            NeverAcceptedRecoveryFixture::new("legacy-origin", DurableCallReservationOrigin::New);
        {
            let mut state = fixture.store.state.lock().unwrap();
            let recovery = state
                .call_ledger
                .get(&fixture.call_id)
                .expect("fixture installs the recovery state")
                .clone();
            state
                .call_ledger
                .upsert(legacy_recovery_without_origin(&recovery));
        }

        let outcome = reconcile_test_never_accepted(&fixture.store, fixture.request())
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            MountedCallNeverAcceptedRecovery::AmbiguousOrigin(ref state)
                if matches!(state.status(), DurableCallStatus::RecoveryRequired {
                    origin: None,
                    ..
                })
        ));
        let state = fixture.store.state.lock().unwrap();
        assert_eq!(state.revision, fixture.expected_revision);
        assert!(matches!(
            state
                .call_ledger
                .get(&fixture.call_id)
                .map(DurableCallState::status),
            Some(DurableCallStatus::RecoveryRequired { origin: None, .. })
        ));
    }

    #[tokio::test]
    async fn never_accepted_recovery_rejects_stale_or_foreign_fences_without_mutation() {
        let stale =
            NeverAcceptedRecoveryFixture::new("stale-revision", DurableCallReservationOrigin::New);
        let stale_revision = stale.expected_revision + 1;
        let stale_outcome = reconcile_test_never_accepted(
            &stale.store,
            MountedCallNeverAcceptedRecoveryRequest::new(
                &stale.session_id,
                &stale.active_epoch,
                &stale.epoch_contract_id,
                &stale.call_id,
                &stale.input_id,
                stale.turn_index,
                &stale.request_id,
                &stale.lease_id,
                &stale_revision,
                &stale.reconciliation_fence,
                &ProviderOperationStatus::NeverAccepted,
            )
            .expect("NeverAccepted status must mint the opaque recovery proof"),
        )
        .await
        .unwrap();
        assert!(matches!(
            stale_outcome,
            MountedCallNeverAcceptedRecovery::RevisionConflict
        ));
        assert_eq!(
            stale.store.state.lock().unwrap().revision,
            stale.expected_revision
        );

        let foreign_lease =
            NeverAcceptedRecoveryFixture::new("foreign-lease", DurableCallReservationOrigin::New);
        let other_lease_id =
            DurableCallLeaseId::new("never-accepted/foreign-lease/other-lease").unwrap();
        let foreign_lease_outcome = reconcile_test_never_accepted(
            &foreign_lease.store,
            MountedCallNeverAcceptedRecoveryRequest::new(
                &foreign_lease.session_id,
                &foreign_lease.active_epoch,
                &foreign_lease.epoch_contract_id,
                &foreign_lease.call_id,
                &foreign_lease.input_id,
                foreign_lease.turn_index,
                &foreign_lease.request_id,
                &other_lease_id,
                &foreign_lease.expected_revision,
                &foreign_lease.reconciliation_fence,
                &ProviderOperationStatus::NeverAccepted,
            )
            .expect("NeverAccepted status must mint the opaque recovery proof"),
        )
        .await
        .unwrap();
        assert!(matches!(
            foreign_lease_outcome,
            MountedCallNeverAcceptedRecovery::Existing(ref state)
                if matches!(state.status(), DurableCallStatus::RecoveryRequired { .. })
        ));
        assert_eq!(
            foreign_lease.store.state.lock().unwrap().revision,
            foreign_lease.expected_revision
        );

        let foreign_epoch =
            NeverAcceptedRecoveryFixture::new("foreign-epoch", DurableCallReservationOrigin::New);
        let replacement_epoch = active_epoch(
            foreign_epoch.epoch_contract_id.clone(),
            "never-accepted/foreign-epoch/replacement",
        );
        let foreign_epoch_outcome = reconcile_test_never_accepted(
            &foreign_epoch.store,
            MountedCallNeverAcceptedRecoveryRequest::new(
                &foreign_epoch.session_id,
                &replacement_epoch,
                &foreign_epoch.epoch_contract_id,
                &foreign_epoch.call_id,
                &foreign_epoch.input_id,
                foreign_epoch.turn_index,
                &foreign_epoch.request_id,
                &foreign_epoch.lease_id,
                &foreign_epoch.expected_revision,
                &foreign_epoch.reconciliation_fence,
                &ProviderOperationStatus::NeverAccepted,
            )
            .expect("NeverAccepted status must mint the opaque recovery proof"),
        )
        .await
        .unwrap();
        assert!(matches!(
            foreign_epoch_outcome,
            MountedCallNeverAcceptedRecovery::Existing(ref state)
                if matches!(state.status(), DurableCallStatus::RecoveryRequired { .. })
        ));
        assert_eq!(
            foreign_epoch.store.state.lock().unwrap().revision,
            foreign_epoch.expected_revision
        );

        let pending = NeverAcceptedRecoveryFixture::new(
            "pending-publication",
            DurableCallReservationOrigin::New,
        );
        pending.store.state.lock().unwrap().pending_publication =
            Some(ClaimedCallPublication::new(
                pending.session_id.clone(),
                pending.epoch_contract_id.clone(),
                pending.call_id.clone(),
                pending.input_id.clone(),
                pending.turn_index,
                pending.lease.clone(),
                PublicationCandidateFingerprint::new(
                    "never-accepted/pending-publication/fingerprint",
                )
                .unwrap(),
            ));
        let pending_outcome = reconcile_test_never_accepted(&pending.store, pending.request())
            .await
            .unwrap();
        assert!(matches!(
            pending_outcome,
            MountedCallNeverAcceptedRecovery::Existing(ref state)
                if matches!(state.status(), DurableCallStatus::RecoveryRequired { .. })
        ));
        assert_eq!(
            pending.store.state.lock().unwrap().revision,
            pending.expected_revision
        );
    }

    #[tokio::test]
    async fn durable_backend_persists_a_never_accepted_checkpoint_restore() {
        let fixture =
            NeverAcceptedRecoveryFixture::new("durable-backend", DurableCallReservationOrigin::New);
        let backend = Arc::new(CursorBackend::default());
        {
            let mut row = backend.row.lock().unwrap();
            row.generation = 1;
            row.state = Some(
                DurableBackendMountedStore::<String, usize, Never, CursorBackend>::encode_state(
                    fixture.store.state.lock().unwrap().clone(),
                )
                .unwrap(),
            );
        }
        let store = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            fixture.session_id.clone(),
            Duration::from_secs(60),
        );

        let outcome = <DurableBackendMountedStore<String, usize, Never, CursorBackend> as MountedSessionStore<
            String,
            usize,
            Never,
            u64,
        >>::reconcile_never_accepted_operation(&store, fixture.request())
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            MountedCallNeverAcceptedRecovery::Restored { state: None, .. }
        ));

        let replacement = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            fixture.session_id.clone(),
            Duration::from_secs(60),
        );
        let snapshot = backend.load(&fixture.session_id).await.unwrap();
        let persisted = replacement.decode_state(&snapshot).unwrap();
        assert_eq!(persisted.revision, fixture.expected_revision + 1);
        assert!(persisted.call_ledger.get(&fixture.call_id).is_none());
    }

    #[tokio::test]
    async fn durable_backend_persists_reconciliation_claim_and_release_across_reload() {
        let fixture = NeverAcceptedRecoveryFixture::new(
            "durable-claim-release",
            DurableCallReservationOrigin::New,
        );
        {
            let mut state = fixture.store.state.lock().unwrap();
            let recovery = state
                .call_ledger
                .get(&fixture.call_id)
                .expect("fixture installs a recovery call")
                .clone();
            state.call_ledger.upsert(
                recovery
                    .without_reconciliation_fence(&fixture.reconciliation_fence)
                    .expect("fixture recovery has its bootstrap fence"),
            );
        }
        let backend = Arc::new(CursorBackend::default());
        {
            let mut row = backend.row.lock().unwrap();
            row.generation = 1;
            row.state = Some(
                DurableBackendMountedStore::<String, usize, Never, CursorBackend>::encode_state(
                    fixture.store.state.lock().unwrap().clone(),
                )
                .unwrap(),
            );
        }
        let store = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            fixture.session_id.clone(),
            Duration::from_secs(60),
        );

        let claim = <DurableBackendMountedStore<String, usize, Never, CursorBackend> as MountedSessionStore<
            String,
            usize,
            Never,
            u64,
        >>::claim_reconciliation(
            &store,
            MountedCallReconciliationClaimRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.call_id,
            ),
        )
        .await
        .unwrap();
        let MountedCallReconciliationAdmission::Acquired(claim) = claim else {
            panic!("a durable recovery call must grant one reconciliation claim")
        };
        assert_eq!(claim.session_revision(), &(fixture.expected_revision + 1));

        let replacement = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            fixture.session_id.clone(),
            Duration::from_secs(60),
        );
        let claim_snapshot = backend.load(&fixture.session_id).await.unwrap();
        let claimed_state = replacement.decode_state(&claim_snapshot).unwrap();
        assert_eq!(claimed_state.revision, fixture.expected_revision + 1);
        assert_eq!(
            claimed_state
                .call_ledger
                .get(&fixture.call_id)
                .and_then(DurableCallState::reconciliation_fence),
            Some(claim.fence())
        );

        let release = <DurableBackendMountedStore<String, usize, Never, CursorBackend> as MountedSessionStore<
            String,
            usize,
            Never,
            u64,
        >>::release_reconciliation(
            &replacement,
            MountedCallReconciliationReleaseRequest::from_claim(
                &fixture.session_id,
                &fixture.active_epoch,
                &claim,
            )
            .expect("a reconciliation claim carries its provider operation lease"),
        )
        .await
        .unwrap();
        assert!(matches!(
            release,
            MountedCallReconciliationRelease::Released { .. }
        ));

        let reloaded = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            fixture.session_id.clone(),
            Duration::from_secs(60),
        );
        let release_snapshot = backend.load(&fixture.session_id).await.unwrap();
        let released_state = reloaded.decode_state(&release_snapshot).unwrap();
        assert_eq!(released_state.revision, fixture.expected_revision + 2);
        let recovery = released_state
            .call_ledger
            .get(&fixture.call_id)
            .expect("releasing an inspection must retain recovery state");
        assert!(matches!(
            recovery.status(),
            DurableCallStatus::RecoveryRequired { .. }
        ));
        assert!(recovery.reconciliation_fence().is_none());
    }

    #[tokio::test]
    async fn cancellation_revision_conflict_requires_recovery_in_the_real_local_store() {
        let fixture = CancellationSettlementFixture::new("cancel-revision-conflict");
        fixture.store.state.lock().unwrap().revision += 1;

        let outcome = settle_test_cancellation(
            &fixture.store,
            fixture.request(MountedCallCancellationDisposition::StopUnchanged),
        )
        .await
        .unwrap();

        assert_recovery(
            outcome,
            DurableCallRecoveryReason::CancellationSettlementConflict,
        );
    }

    #[tokio::test]
    async fn cancellation_pending_publication_requires_recovery_in_the_real_local_store() {
        let fixture = CancellationSettlementFixture::new("cancel-pending-publication");
        fixture.store.state.lock().unwrap().pending_publication =
            Some(ClaimedCallPublication::new(
                fixture.session_id.clone(),
                fixture.epoch_contract_id.clone(),
                fixture.call_id.clone(),
                fixture.input_id.clone(),
                0,
                fixture.lease.clone(),
                PublicationCandidateFingerprint::new("cancel-pending-publication/fingerprint")
                    .unwrap(),
            ));

        let outcome = settle_test_cancellation(
            &fixture.store,
            fixture.request(MountedCallCancellationDisposition::StopUnchanged),
        )
        .await
        .unwrap();

        assert_recovery(
            outcome,
            DurableCallRecoveryReason::CancellationPendingPublication,
        );
    }

    #[tokio::test]
    async fn cancellation_epoch_mismatch_requires_recovery_in_the_real_local_store() {
        let fixture = CancellationSettlementFixture::new("cancel-epoch-mismatch");
        fixture.store.state.lock().unwrap().epoch = InMemoryEpochState::Active(active_epoch(
            EpochContractId::new("local/replacement/v1").unwrap(),
            "local/cancel-epoch-mismatch/replacement",
        ));

        let outcome = settle_test_cancellation(
            &fixture.store,
            fixture.request(MountedCallCancellationDisposition::StopUnchanged),
        )
        .await
        .unwrap();

        assert_recovery(
            outcome,
            DurableCallRecoveryReason::CancellationEpochMismatch,
        );
    }

    #[tokio::test]
    async fn cancellation_invalid_cursors_require_recovery_in_the_real_local_store() {
        let stored_cursor = CancellationSettlementFixture::new("cancel-invalid-stored-cursor");
        stored_cursor.store.state.lock().unwrap().provider_cursor =
            Some(invalid_cursor(&stored_cursor));
        let stored_outcome = settle_test_cancellation(
            &stored_cursor.store,
            stored_cursor.request(MountedCallCancellationDisposition::StopUnchanged),
        )
        .await
        .unwrap();
        assert_recovery(
            stored_outcome,
            DurableCallRecoveryReason::CancellationCursorInvalid,
        );

        let resume_cursor = CancellationSettlementFixture::new("cancel-invalid-resume-cursor");
        let resume_outcome = settle_test_cancellation(
            &resume_cursor.store,
            resume_cursor.request(MountedCallCancellationDisposition::StopWithCursor(
                invalid_cursor(&resume_cursor),
            )),
        )
        .await
        .unwrap();
        assert_recovery(
            resume_outcome,
            DurableCallRecoveryReason::CancellationCursorInvalid,
        );
    }

    #[tokio::test]
    async fn cancellation_expired_lease_requires_recovery_in_the_real_local_store() {
        let fixture = CancellationSettlementFixture::with_lease_expiry("cancel-expired-lease", 0);

        let outcome = settle_test_cancellation(
            &fixture.store,
            fixture.request(MountedCallCancellationDisposition::StopUnchanged),
        )
        .await
        .unwrap();

        assert_recovery(outcome, DurableCallRecoveryReason::LeaseExpired);
    }

    #[tokio::test]
    async fn cancellation_foreign_fence_does_not_mutate_the_real_local_store() {
        let fixture = CancellationSettlementFixture::new("cancel-foreign-fence");
        let foreign_lease = DurableCallLeaseId::new("cancel-foreign-fence/other-lease").unwrap();
        let foreign_lease_outcome = settle_test_cancellation(
            &fixture.store,
            MountedCallCancellationSettlementRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.epoch_contract_id,
                &fixture.call_id,
                &fixture.input_id,
                0,
                &fixture.request_id,
                &foreign_lease,
                &fixture.expected_revision,
                MountedCallCancellationDisposition::StopUnchanged,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            foreign_lease_outcome,
            MountedCallCancellationSettlement::Existing(ref state)
                if matches!(state.status(), DurableCallStatus::Running { .. })
        ));

        let foreign_request =
            PublicationRequestId::new("cancel-foreign-fence/other-request").unwrap();
        let foreign_request_outcome = settle_test_cancellation(
            &fixture.store,
            MountedCallCancellationSettlementRequest::new(
                &fixture.session_id,
                &fixture.active_epoch,
                &fixture.epoch_contract_id,
                &fixture.call_id,
                &fixture.input_id,
                0,
                &foreign_request,
                &fixture.lease_id,
                &fixture.expected_revision,
                MountedCallCancellationDisposition::StopUnchanged,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            foreign_request_outcome,
            MountedCallCancellationSettlement::Existing(ref state)
                if matches!(state.status(), DurableCallStatus::Running { .. })
        ));

        let state = fixture.store.state.lock().unwrap();
        assert_eq!(state.revision, fixture.expected_revision);
        assert!(matches!(
            state
                .call_ledger
                .get(&fixture.call_id)
                .map(DurableCallState::status),
            Some(DurableCallStatus::Running { .. })
        ));
    }

    #[tokio::test]
    async fn cancellation_recovery_retry_is_idempotent_in_the_real_local_store() {
        let fixture = CancellationSettlementFixture::new("cancel-recovery-idempotency");
        let first = settle_test_cancellation(
            &fixture.store,
            fixture.request(MountedCallCancellationDisposition::Recover(
                DurableCallRecoveryReason::CancellationIndeterminate,
            )),
        )
        .await
        .unwrap();
        let revision_after_first =
            assert_recovery(first, DurableCallRecoveryReason::CancellationIndeterminate);

        let retry = settle_test_cancellation(
            &fixture.store,
            fixture.request(MountedCallCancellationDisposition::StopUnchanged),
        )
        .await
        .unwrap();
        let revision_after_retry =
            assert_recovery(retry, DurableCallRecoveryReason::CancellationIndeterminate);

        assert_eq!(revision_after_retry, revision_after_first);
        assert_eq!(
            fixture.store.state.lock().unwrap().revision,
            revision_after_first
        );
    }

    #[tokio::test]
    async fn durable_blob_persists_pom_cursor_and_replacement_owner_keeps_delta_lineage() {
        let backend = Arc::new(CursorBackend::default());
        let session_id = DurableSessionId::new("cursor-persistence/session").unwrap();
        let store = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            session_id.clone(),
            Duration::from_secs(60),
        );
        let state = InMemoryMountedState {
            session_id: Some(session_id.clone()),
            session: Some(session_with_user_cursor()),
            ..Default::default()
        };
        let expected_cursor = state
            .session
            .as_ref()
            .expect("fixture stores a session")
            .user_document_cursor()
            .clone();

        store
            .transact(
                move |_| {
                    let state = state.clone();
                    async move {
                        LocalStateTransaction::new(Ok::<_, Infallible>(()), state, None, false)
                    }
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();

        let snapshot = backend.load(&session_id).await.unwrap();
        let encoded = std::str::from_utf8(snapshot.state().unwrap().as_bytes()).unwrap();
        assert!(encoded.contains("user_document_cursor"));
        assert!(encoded.contains("cursor-baseline-sentinel"));

        let same_owner = store.decode_state(&snapshot).unwrap();
        assert_eq!(
            same_owner
                .session
                .as_ref()
                .unwrap()
                .user_document_cursor()
                .len(),
            1
        );

        let replacement = DurableBackendMountedStore::<String, usize, Never, _>::new(
            backend,
            session_id,
            Duration::from_secs(60),
        );
        let reopened = replacement.decode_state(&snapshot).unwrap();
        assert_eq!(
            reopened.session.as_ref().unwrap().user_document_cursor(),
            &expected_cursor
        );
    }

    #[tokio::test]
    async fn durable_backend_rejects_a_committed_write_with_an_unchanged_generation() {
        let backend = Arc::new(CursorBackend::default());
        let session_id = DurableSessionId::new("generation-persistence/session").unwrap();
        let store = DurableBackendMountedStore::<String, usize, Never, _>::new(
            Arc::clone(&backend),
            session_id.clone(),
            Duration::from_secs(60),
        );
        let initial = InMemoryMountedState {
            session_id: Some(session_id),
            ..Default::default()
        };

        store
            .transact(
                move |_| {
                    let initial = initial.clone();
                    async move {
                        LocalStateTransaction::new(Ok::<_, Infallible>(()), initial, None, false)
                    }
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();

        backend.row.lock().unwrap().return_stale_generation_once = true;
        let changed = InMemoryMountedState {
            session_id: Some(DurableSessionId::new("generation-persistence/session").unwrap()),
            revision: 1,
            ..Default::default()
        };
        let error = store
            .transact(
                move |_| {
                    let changed = changed.clone();
                    async move {
                        LocalStateTransaction::new(Ok::<_, Infallible>(()), changed, None, false)
                    }
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            DurableBackendMountedStoreError::GenerationNotAdvanced { .. }
        ));
    }
}
