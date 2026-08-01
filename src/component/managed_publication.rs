//! Cancellation owner for one staged durable streaming publication.
//!
//! The actor is crate-private until `MountedAgent` owns the complete session
//! transaction. Once a command reaches its mailbox, cancelling the caller only
//! cancels that wait: the actor still publishes or resolves the same durable
//! request id. Losing the final handle aborts only a `Ready` candidate; a
//! `Resolving` candidate is resolved until the store gives an authoritative
//! answer.

use std::{convert::Infallible, error::Error, fmt, sync::Arc, time::Duration};

use tokio::sync::{mpsc, oneshot};

use super::{
    BindingAbortReason, DurablyPublishedProviderAttempt, DurablyPublishedStreamingAttempt,
    ProviderAttemptAbortReport, PublicationAttemptError, PublicationPhase, PublicationReceipt,
    PublicationResolution, PublicationResolveError, PublicationStore, StagedProviderPublication,
    StagedStreamingPublication, StreamingAbortReport, TurnChannels,
};

/// Phase owned by the managed publication actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedPublicationPhase {
    Ready,
    Resolving,
    Published,
    Completed,
    Aborted,
}

impl fmt::Display for ManagedPublicationPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ready => "ready",
            Self::Resolving => "resolving",
            Self::Published => "published",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        })
    }
}

impl From<PublicationPhase> for ManagedPublicationPhase {
    fn from(value: PublicationPhase) -> Self {
        match value {
            PublicationPhase::Ready => Self::Ready,
            PublicationPhase::Resolving => Self::Resolving,
            PublicationPhase::Published => Self::Published,
        }
    }
}

#[async_trait::async_trait]
trait ErasedManagedPublicationOperations<Version, StoreError>: Send + Sync
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    async fn phase(&self) -> Result<ManagedPublicationPhase, ManagedPublicationError<StoreError>>;

    async fn publish(
        &self,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>>;

    async fn resolve(
        &self,
    ) -> Result<PublicationResolution<Version>, ManagedPublicationError<StoreError>>;

    async fn complete(
        self: Box<Self>,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>>;

    async fn abort(
        self: Box<Self>,
        reason: BindingAbortReason,
    ) -> Result<StreamingAbortReport, ManagedPublicationError<StoreError>>;
}

/// Root-channel-erased durable owner returned to the future AgentLoop.
///
/// The host revision and store error remain typed because the owner needs them
/// for the next CAS and for publication recovery policy. Root channels,
/// mutation, payload, and concrete store implementation stay behind the
/// monomorphized adapter.
#[must_use = "a managed durable publication must be completed, aborted, or left to recovery"]
pub(crate) struct ManagedDurablePublication<Version = (), StoreError = Infallible>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    operations: Box<dyn ErasedManagedPublicationOperations<Version, StoreError>>,
}

impl<Version, StoreError> ManagedDurablePublication<Version, StoreError>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    pub(crate) async fn phase(
        &self,
    ) -> Result<ManagedPublicationPhase, ManagedPublicationError<StoreError>> {
        self.operations.phase().await
    }

    pub(crate) async fn publish(
        &self,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>> {
        self.operations.publish().await
    }

    pub(crate) async fn resolve(
        &self,
    ) -> Result<PublicationResolution<Version>, ManagedPublicationError<StoreError>> {
        self.operations.resolve().await
    }

    pub(crate) async fn complete(
        self,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>> {
        self.operations.complete().await
    }

    pub(crate) async fn abort(
        self,
        reason: BindingAbortReason,
    ) -> Result<StreamingAbortReport, ManagedPublicationError<StoreError>> {
        self.operations.abort(reason).await
    }
}

impl<Version, StoreError> fmt::Debug for ManagedDurablePublication<Version, StoreError>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedDurablePublication")
            .finish_non_exhaustive()
    }
}

/// A managed publication operation was requested in the wrong phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("managed publication is {phase}, but this operation requires {required}")]
pub(crate) struct ManagedPublicationPhaseError {
    phase: ManagedPublicationPhase,
    required: ManagedPublicationPhase,
}

impl ManagedPublicationPhaseError {
    pub(crate) fn phase(&self) -> ManagedPublicationPhase {
        self.phase
    }

    pub(crate) fn required(&self) -> ManagedPublicationPhase {
        self.required
    }
}

/// Start failure that returns the staged attempt to its caller.
pub(crate) struct ManagedPublicationStartError<P> {
    publication: Box<P>,
}

impl<P> ManagedPublicationStartError<P> {
    pub(crate) fn into_publication(self) -> P {
        *self.publication
    }
}

impl<P> fmt::Debug for ManagedPublicationStartError<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedPublicationStartError")
            .finish_non_exhaustive()
    }
}

impl<P> fmt::Display for ManagedPublicationStartError<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a managed durable publication requires a running Tokio runtime")
    }
}

impl<P> Error for ManagedPublicationStartError<P> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedPublicationOperation {
    Publish,
    Resolve,
}

impl fmt::Display for ManagedPublicationOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Publish => "publish",
            Self::Resolve => "resolve",
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ManagedPublicationPolicy {
    operation_timeout: Duration,
    initial_recovery_backoff: Duration,
    max_recovery_backoff: Duration,
}

impl Default for ManagedPublicationPolicy {
    fn default() -> Self {
        Self {
            operation_timeout: Duration::from_secs(30),
            initial_recovery_backoff: Duration::from_millis(100),
            max_recovery_backoff: Duration::from_secs(30),
        }
    }
}

/// Error returned to a caller while the actor retains authoritative state.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedPublicationError<E>
where
    E: Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Transition(#[from] PublicationAttemptError<E>),

    #[error(transparent)]
    Phase(#[from] ManagedPublicationPhaseError),

    #[error("managed publication {operation} did not complete within {timeout:?}")]
    OperationTimedOut {
        operation: ManagedPublicationOperation,
        timeout: Duration,
    },

    #[error("managed publication control task stopped unexpectedly")]
    ControlStopped,
}

enum PublicationCommand<Published, AbortReport, Version, StoreError>
where
    Published: Send + 'static,
    AbortReport: Send + 'static,
    Version: Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    Publish {
        reply: oneshot::Sender<
            Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>>,
        >,
    },
    Resolve {
        reply: oneshot::Sender<
            Result<PublicationResolution<Version>, ManagedPublicationError<StoreError>>,
        >,
    },
    Complete {
        reply: oneshot::Sender<Result<Published, ManagedPublicationPhaseError>>,
    },
    Abort {
        reason: BindingAbortReason,
        reply: oneshot::Sender<Result<AbortReport, ManagedPublicationPhaseError>>,
    },
    Phase {
        reply: oneshot::Sender<ManagedPublicationPhase>,
    },
}

type PublicationCommandReceiver<Published, AbortReport, Version, StoreError> =
    mpsc::UnboundedReceiver<PublicationCommand<Published, AbortReport, Version, StoreError>>;

struct PublicationControl<Published, AbortReport, Version, StoreError>
where
    Published: Send + 'static,
    AbortReport: Send + 'static,
    Version: Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    commands:
        mpsc::UnboundedSender<PublicationCommand<Published, AbortReport, Version, StoreError>>,
}

impl<Published, AbortReport, Version, StoreError>
    PublicationControl<Published, AbortReport, Version, StoreError>
where
    Published: Send + 'static,
    AbortReport: Send + 'static,
    Version: Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    async fn request<T>(
        &self,
        command: impl FnOnce(
            oneshot::Sender<T>,
        ) -> PublicationCommand<Published, AbortReport, Version, StoreError>,
    ) -> Result<T, ManagedPublicationError<StoreError>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| ManagedPublicationError::ControlStopped)?;
        receiver
            .await
            .map_err(|_| ManagedPublicationError::ControlStopped)
    }
}

/// Handle whose actor owns a staged publication across caller cancellation.
#[must_use = "a managed publication must be completed, aborted, or left to its recovery owner"]
pub(crate) struct ManagedPublication<Published, AbortReport, Version, StoreError>
where
    Published: Send + 'static,
    AbortReport: Send + 'static,
    Version: Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    control: PublicationControl<Published, AbortReport, Version, StoreError>,
}

pub(crate) type ManagedStreamingPublication<C, Version, StoreError> = ManagedPublication<
    DurablyPublishedStreamingAttempt<C, Version>,
    StreamingAbortReport,
    Version,
    StoreError,
>;

pub(crate) type ManagedProviderPublication<C, Version, StoreError> = ManagedPublication<
    DurablyPublishedProviderAttempt<C, Version>,
    ProviderAttemptAbortReport,
    Version,
    StoreError,
>;

type ManagedStreamingPublicationStart<C, Mutation, Payload, Version, StoreError> = Result<
    ManagedStreamingPublication<C, Version, StoreError>,
    ManagedPublicationStartError<StagedStreamingPublication<C, Mutation, Payload, Version>>,
>;

type ManagedProviderPublicationStart<C, Mutation, Payload, Version, StoreError> = Result<
    ManagedProviderPublication<C, Version, StoreError>,
    ManagedPublicationStartError<StagedProviderPublication<C, Mutation, Payload, Version>>,
>;

type ManagedPublicationStart<P, Published, AbortReport, Version, StoreError> = Result<
    ManagedPublication<Published, AbortReport, Version, StoreError>,
    ManagedPublicationStartError<P>,
>;

impl<Published, AbortReport, Version, StoreError>
    ManagedPublication<Published, AbortReport, Version, StoreError>
where
    Published: Send + 'static,
    AbortReport: Send + 'static,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    pub(crate) async fn phase(
        &self,
    ) -> Result<ManagedPublicationPhase, ManagedPublicationError<StoreError>> {
        self.control
            .request(|reply| PublicationCommand::Phase { reply })
            .await
    }

    pub(crate) async fn publish(
        &self,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>> {
        self.control
            .request(|reply| PublicationCommand::Publish { reply })
            .await?
    }

    pub(crate) async fn resolve(
        &self,
    ) -> Result<PublicationResolution<Version>, ManagedPublicationError<StoreError>> {
        self.control
            .request(|reply| PublicationCommand::Resolve { reply })
            .await?
    }

    pub(crate) async fn complete(self) -> Result<Published, ManagedPublicationError<StoreError>> {
        self.control
            .request(|reply| PublicationCommand::Complete { reply })
            .await?
            .map_err(Into::into)
    }

    pub(crate) async fn abort(
        self,
        reason: BindingAbortReason,
    ) -> Result<AbortReport, ManagedPublicationError<StoreError>> {
        self.control
            .request(|reply| PublicationCommand::Abort { reason, reply })
            .await?
            .map_err(Into::into)
    }
}

impl<Published, AbortReport, Version, StoreError> fmt::Debug
    for ManagedPublication<Published, AbortReport, Version, StoreError>
where
    Published: Send + 'static,
    AbortReport: Send + 'static,
    Version: Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedPublication")
            .finish_non_exhaustive()
    }
}

struct TypedManagedStreamingPublication<C, Version, StoreError>
where
    C: TurnChannels,
    Version: Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    managed: ManagedStreamingPublication<C, Version, StoreError>,
}

#[async_trait::async_trait]
impl<C, Version, StoreError> ErasedManagedPublicationOperations<Version, StoreError>
    for TypedManagedStreamingPublication<C, Version, StoreError>
where
    C: TurnChannels,
    Version: Clone + Send + Sync + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    async fn phase(&self) -> Result<ManagedPublicationPhase, ManagedPublicationError<StoreError>> {
        self.managed.phase().await
    }

    async fn publish(
        &self,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>> {
        self.managed.publish().await
    }

    async fn resolve(
        &self,
    ) -> Result<PublicationResolution<Version>, ManagedPublicationError<StoreError>> {
        self.managed.resolve().await
    }

    async fn complete(
        self: Box<Self>,
    ) -> Result<PublicationReceipt<Version>, ManagedPublicationError<StoreError>> {
        self.managed
            .complete()
            .await
            .map(|published| published.receipt().clone())
    }

    async fn abort(
        self: Box<Self>,
        reason: BindingAbortReason,
    ) -> Result<StreamingAbortReport, ManagedPublicationError<StoreError>> {
        self.managed.abort(reason).await
    }
}

pub(crate) fn erase_managed_streaming_publication<C, Version, StoreError>(
    managed: ManagedStreamingPublication<C, Version, StoreError>,
) -> ManagedDurablePublication<Version, StoreError>
where
    C: TurnChannels,
    Version: Clone + Send + Sync + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    ManagedDurablePublication {
        operations: Box::new(TypedManagedStreamingPublication { managed }),
    }
}

enum ManagedPublicationState<P> {
    Staged(P),
    Completed,
    Aborted,
}

impl<P> ManagedPublicationState<P> {
    fn phase(&self) -> ManagedPublicationPhase
    where
        P: ManagedStagedPublicationPhase,
    {
        match self {
            Self::Staged(publication) => publication.phase().into(),
            Self::Completed => ManagedPublicationPhase::Completed,
            Self::Aborted => ManagedPublicationPhase::Aborted,
        }
    }
}

trait ManagedStagedPublicationPhase {
    fn phase(&self) -> PublicationPhase;
}

impl<C, Mutation, Payload, Version> ManagedStagedPublicationPhase
    for StagedStreamingPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
{
    fn phase(&self) -> PublicationPhase {
        StagedStreamingPublication::phase(self)
    }
}

impl<C, Mutation, Payload, Version> ManagedStagedPublicationPhase
    for StagedProviderPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
{
    fn phase(&self) -> PublicationPhase {
        StagedProviderPublication::phase(self)
    }
}

#[async_trait::async_trait]
trait ManagedStagedPublication<Mutation, Payload, Version, S>:
    ManagedStagedPublicationPhase + Send + Sized
where
    Mutation: Sync,
    Payload: Sync,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    type Published: Send + 'static;
    type AbortReport: Send + 'static;

    async fn publish(
        &mut self,
        store: &S,
    ) -> Result<PublicationReceipt<Version>, PublicationAttemptError<S::Error>>;

    async fn resolve(
        &mut self,
        store: &S,
    ) -> Result<PublicationResolution<Version>, PublicationAttemptError<S::Error>>;

    async fn complete(self) -> Result<Self::Published, Self>;

    async fn abort_ready(self, reason: BindingAbortReason) -> Self::AbortReport;
}

#[async_trait::async_trait]
impl<C, Mutation, Payload, Version, S> ManagedStagedPublication<Mutation, Payload, Version, S>
    for StagedStreamingPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    type Published = DurablyPublishedStreamingAttempt<C, Version>;
    type AbortReport = StreamingAbortReport;

    async fn publish(
        &mut self,
        store: &S,
    ) -> Result<PublicationReceipt<Version>, PublicationAttemptError<S::Error>> {
        StagedStreamingPublication::publish(self, store).await
    }

    async fn resolve(
        &mut self,
        store: &S,
    ) -> Result<PublicationResolution<Version>, PublicationAttemptError<S::Error>> {
        StagedStreamingPublication::resolve(self, store).await
    }

    async fn complete(self) -> Result<Self::Published, Self> {
        self.into_published().await
    }

    async fn abort_ready(self, reason: BindingAbortReason) -> Self::AbortReport {
        let finished = self.into_finished_if_ready().unwrap_or_else(|_| {
            unreachable!("ready publication must release its finished attempt")
        });
        finished.abort(reason).await
    }
}

#[async_trait::async_trait]
impl<C, Mutation, Payload, Version, S> ManagedStagedPublication<Mutation, Payload, Version, S>
    for StagedProviderPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    type Published = DurablyPublishedProviderAttempt<C, Version>;
    type AbortReport = ProviderAttemptAbortReport;

    async fn publish(
        &mut self,
        store: &S,
    ) -> Result<PublicationReceipt<Version>, PublicationAttemptError<S::Error>> {
        StagedProviderPublication::publish(self, store).await
    }

    async fn resolve(
        &mut self,
        store: &S,
    ) -> Result<PublicationResolution<Version>, PublicationAttemptError<S::Error>> {
        StagedProviderPublication::resolve(self, store).await
    }

    async fn complete(self) -> Result<Self::Published, Self> {
        self.into_published().await
    }

    async fn abort_ready(self, reason: BindingAbortReason) -> Self::AbortReport {
        let finished = self.into_finished_if_ready().unwrap_or_else(|_| {
            unreachable!("ready publication must release its finished attempt")
        });
        finished.abort(reason).await
    }
}

/// Start a cancellation owner after Commit staging has succeeded.
pub(crate) fn start_managed_streaming_publication<C, Mutation, Payload, Version, S>(
    publication: StagedStreamingPublication<C, Mutation, Payload, Version>,
    store: Arc<S>,
) -> ManagedStreamingPublicationStart<C, Mutation, Payload, Version, S::Error>
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    start_managed_publication(publication, store, ManagedPublicationPolicy::default())
}

/// Start a publication owner on the runtime already holding the old attempt.
///
/// Unlike the external proof constructor, this cannot fail after staging: the
/// actor-to-actor handoff supplies the runtime handle that owns both tasks.
pub(crate) fn start_managed_streaming_publication_on<C, Mutation, Payload, Version, S>(
    runtime: &tokio::runtime::Handle,
    publication: StagedStreamingPublication<C, Mutation, Payload, Version>,
    store: Arc<S>,
) -> ManagedStreamingPublication<C, Version, S::Error>
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    start_managed_publication_on(
        runtime,
        publication,
        store,
        ManagedPublicationPolicy::default(),
    )
}

/// Start the same cancellation/recovery owner for a native-only attempt.
pub(crate) fn start_managed_provider_publication<C, Mutation, Payload, Version, S>(
    publication: StagedProviderPublication<C, Mutation, Payload, Version>,
    store: Arc<S>,
) -> ManagedProviderPublicationStart<C, Mutation, Payload, Version, S::Error>
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    start_managed_publication(publication, store, ManagedPublicationPolicy::default())
}

fn start_managed_publication<P, Mutation, Payload, Version, S>(
    publication: P,
    store: Arc<S>,
    policy: ManagedPublicationPolicy,
) -> ManagedPublicationStart<P, P::Published, P::AbortReport, Version, S::Error>
where
    P: ManagedStagedPublication<Mutation, Payload, Version, S> + 'static,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    debug_assert!(!policy.operation_timeout.is_zero());
    debug_assert!(!policy.initial_recovery_backoff.is_zero());
    debug_assert!(policy.initial_recovery_backoff <= policy.max_recovery_backoff);
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(runtime) => runtime,
        Err(_) => {
            return Err(ManagedPublicationStartError {
                publication: Box::new(publication),
            })
        }
    };
    Ok(start_managed_publication_on(
        &runtime,
        publication,
        store,
        policy,
    ))
}

fn start_managed_publication_on<P, Mutation, Payload, Version, S>(
    runtime: &tokio::runtime::Handle,
    publication: P,
    store: Arc<S>,
    policy: ManagedPublicationPolicy,
) -> ManagedPublication<P::Published, P::AbortReport, Version, S::Error>
where
    P: ManagedStagedPublication<Mutation, Payload, Version, S> + 'static,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    let (commands, receiver) = mpsc::unbounded_channel();
    runtime.spawn(run_publication(
        ManagedPublicationState::Staged(publication),
        store,
        receiver,
        policy,
    ));
    ManagedPublication {
        control: PublicationControl { commands },
    }
}

async fn run_publication<P, Mutation, Payload, Version, S>(
    mut state: ManagedPublicationState<P>,
    store: Arc<S>,
    mut commands: PublicationCommandReceiver<P::Published, P::AbortReport, Version, S::Error>,
    policy: ManagedPublicationPolicy,
) where
    P: ManagedStagedPublication<Mutation, Payload, Version, S> + 'static,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    while let Some(command) = commands.recv().await {
        match command {
            PublicationCommand::Publish { reply } => {
                let result = publish(&mut state, store.as_ref(), policy).await;
                let _ = reply.send(result);
            }
            PublicationCommand::Resolve { reply } => {
                let result = resolve(&mut state, store.as_ref(), policy).await;
                let _ = reply.send(result);
            }
            PublicationCommand::Complete { reply } => {
                let result = complete(&mut state).await;
                let _ = reply.send(result);
            }
            PublicationCommand::Abort { reason, reply } => {
                let result = abort(&mut state, reason).await;
                let _ = reply.send(result);
            }
            PublicationCommand::Phase { reply } => {
                let _ = reply.send(state.phase());
            }
        }
    }

    settle_after_disconnect(state, store.as_ref(), policy).await;
}

async fn publish<P, Mutation, Payload, Version, S>(
    state: &mut ManagedPublicationState<P>,
    store: &S,
    policy: ManagedPublicationPolicy,
) -> Result<PublicationReceipt<Version>, ManagedPublicationError<S::Error>>
where
    P: ManagedStagedPublication<Mutation, Payload, Version, S>,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    let publication = match state {
        ManagedPublicationState::Staged(publication) => publication,
        ManagedPublicationState::Completed | ManagedPublicationState::Aborted => {
            unreachable!("typed handle only publishes a staged state")
        }
    };
    match tokio::time::timeout(policy.operation_timeout, publication.publish(store)).await {
        Ok(result) => result.map_err(Into::into),
        Err(_) => Err(ManagedPublicationError::OperationTimedOut {
            operation: ManagedPublicationOperation::Publish,
            timeout: policy.operation_timeout,
        }),
    }
}

async fn resolve<P, Mutation, Payload, Version, S>(
    state: &mut ManagedPublicationState<P>,
    store: &S,
    policy: ManagedPublicationPolicy,
) -> Result<PublicationResolution<Version>, ManagedPublicationError<S::Error>>
where
    P: ManagedStagedPublication<Mutation, Payload, Version, S>,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    let publication = match state {
        ManagedPublicationState::Staged(publication) => publication,
        ManagedPublicationState::Completed | ManagedPublicationState::Aborted => {
            unreachable!("typed handle only resolves a staged state")
        }
    };
    match tokio::time::timeout(policy.operation_timeout, publication.resolve(store)).await {
        Ok(result) => result.map_err(Into::into),
        Err(_) => Err(ManagedPublicationError::OperationTimedOut {
            operation: ManagedPublicationOperation::Resolve,
            timeout: policy.operation_timeout,
        }),
    }
}

async fn complete<P, Mutation, Payload, Version, S>(
    state: &mut ManagedPublicationState<P>,
) -> Result<P::Published, ManagedPublicationPhaseError>
where
    P: ManagedStagedPublication<Mutation, Payload, Version, S>,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    let phase = state.phase();
    if phase != ManagedPublicationPhase::Published {
        return Err(ManagedPublicationPhaseError {
            phase,
            required: ManagedPublicationPhase::Published,
        });
    }
    let current = std::mem::replace(state, ManagedPublicationState::Completed);
    let ManagedPublicationState::Staged(publication) = current else {
        unreachable!("published phase belongs to a staged publication")
    };
    publication.complete().await.map_err(|publication| {
        *state = ManagedPublicationState::Staged(publication);
        ManagedPublicationPhaseError {
            phase: state.phase(),
            required: ManagedPublicationPhase::Published,
        }
    })
}

async fn abort<P, Mutation, Payload, Version, S>(
    state: &mut ManagedPublicationState<P>,
    reason: BindingAbortReason,
) -> Result<P::AbortReport, ManagedPublicationPhaseError>
where
    P: ManagedStagedPublication<Mutation, Payload, Version, S>,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    let phase = state.phase();
    if phase != ManagedPublicationPhase::Ready {
        return Err(ManagedPublicationPhaseError {
            phase,
            required: ManagedPublicationPhase::Ready,
        });
    }
    let current = std::mem::replace(state, ManagedPublicationState::Aborted);
    let ManagedPublicationState::Staged(publication) = current else {
        unreachable!("ready phase belongs to a staged publication")
    };
    Ok(publication.abort_ready(reason).await)
}

async fn settle_after_disconnect<P, Mutation, Payload, Version, S>(
    mut state: ManagedPublicationState<P>,
    store: &S,
    policy: ManagedPublicationPolicy,
) where
    P: ManagedStagedPublication<Mutation, Payload, Version, S>,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    S: PublicationStore<Mutation, Payload, Version = Version>,
{
    let mut retry_count = 0_u64;
    let mut backoff = policy.initial_recovery_backoff;
    loop {
        match state.phase() {
            ManagedPublicationPhase::Ready => {
                let _ = abort(&mut state, BindingAbortReason::Cancelled).await;
                return;
            }
            ManagedPublicationPhase::Resolving => match resolve(&mut state, store, policy).await {
                Ok(PublicationResolution::Published(_))
                | Ok(PublicationResolution::NotCommitted) => continue,
                Err(ManagedPublicationError::Transition(PublicationAttemptError::Resolve(
                    PublicationResolveError::CandidateCollision(error),
                ))) => {
                    tracing::error!(
                        error = %error,
                        "durable publication request collided with another candidate; aborting the uncommitted attempt"
                    );
                    let _ = abort(&mut state, BindingAbortReason::PublishFailure).await;
                    return;
                }
                Err(error) => {
                    retry_count = retry_count.saturating_add(1);
                    tracing::warn!(
                        error = %error,
                        retry_count,
                        retry_in_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
                        "durable publication resolution failed; retaining recovery obligation"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = next_recovery_backoff(backoff, policy.max_recovery_backoff);
                }
            },
            ManagedPublicationPhase::Published => {
                let _ = complete(&mut state).await;
                return;
            }
            ManagedPublicationPhase::Completed | ManagedPublicationPhase::Aborted => return,
        }
    }
}

fn next_recovery_backoff(current: Duration, maximum: Duration) -> Duration {
    current.checked_mul(2).unwrap_or(maximum).min(maximum)
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Instant,
    };

    use serde_json::json;
    use tokio::{sync::Notify, task::yield_now, time::timeout};

    use super::*;
    use crate::{
        component::{
            mount_system_epoch, provider_tool, system_view, user_view, CommitContract,
            CommitStager, CommitStagingContext, LiveAbortContext, LiveEffectAbortAck,
            LiveEffectContext, LiveEffectRuntime, PreparedSessionMutation, ProviderDispatchContext,
            ProviderDispatchUpdate, ProviderDispatcher, ProviderDispatcherAbortAck,
            ProviderDispatcherAbortContext, ProviderToolCall, ProviderToolResponse,
            ProviderToolSpec, PublicationCandidateFingerprint, PublicationFingerprintContext,
            PublicationFingerprintFactory, PublicationId, PublicationRequest, PublicationRequestId,
            PublicationWriteError, StagedCommit, StreamUpdate, StreamingXml, SystemMountContext,
            SystemView, TurnEmission,
        },
        llm_call::TextTurnEvent,
        pom::{XmlName, XmlNode},
    };

    struct TestChannels;

    impl TurnChannels for TestChannels {
        type Output = ();
        type Live = ();
        type Commit = u32;
        type Diagnostic = ();
    }

    struct TestState {
        drops: Arc<AtomicUsize>,
        aborts: Arc<AtomicUsize>,
    }

    impl Drop for TestState {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct MountProps {
        drops: Arc<AtomicUsize>,
        aborts: Arc<AtomicUsize>,
    }

    fn test_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<TestChannels> {
        let drops = Arc::clone(&cx.props().drops);
        let aborts = Arc::clone(&cx.props().aborts);
        system_view(
            StreamingXml::<TurnEmission<TestChannels>, ()>::new(XmlNode::new(
                XmlName::try_from("effect").unwrap(),
            ))
            .state_with(move || TestState {
                drops: Arc::clone(&drops),
                aborts: Arc::clone(&aborts),
            })
            .on_complete(|_, _| StreamUpdate::from_emission(TurnEmission::Commit(7)))
            .on_abort(|state, _| {
                state.aborts.fetch_add(1, Ordering::SeqCst);
                super::super::BindingAbortAck::LocalCleanupCompleted
            })
            .into_component(),
        )
    }

    struct LiveRuntime;

    #[async_trait::async_trait]
    impl LiveEffectRuntime<()> for LiveRuntime {
        type Error = Infallible;

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            _effect: (),
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn abort(
            &mut self,
            context: &LiveAbortContext,
        ) -> Result<LiveEffectAbortAck, Self::Error> {
            Ok(if context.applied_effects() == 0 {
                LiveEffectAbortAck::NoEffectsApplied
            } else {
                LiveEffectAbortAck::CompensationCompleted
            })
        }
    }

    struct Stager;

    impl CommitStager<TestChannels> for Stager {
        type Payload = u32;
        type Error = Infallible;

        fn stage(
            &self,
            _context: CommitStagingContext<'_>,
            commit: &u32,
        ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
            Ok(StagedCommit::new(
                CommitContract::new("test.effect", 1).unwrap(),
                *commit,
            ))
        }
    }

    struct FingerprintFactory;

    impl PublicationFingerprintFactory<(), u32, u64> for FingerprintFactory {
        type Error = crate::component::DurableKeyError;

        fn fingerprint(
            &self,
            context: PublicationFingerprintContext<'_, (), u32, u64>,
        ) -> Result<PublicationCandidateFingerprint, Self::Error> {
            PublicationCandidateFingerprint::new(format!(
                "managed-v1|request={}|revision={}|mutation=()|raw={:?}|results={:?}|outbox={:?}",
                context.request_id(),
                context.expected_revision(),
                context.raw_output(),
                context.provider_results(),
                context.outbox(),
            ))
        }
    }

    #[derive(Debug)]
    struct NativeDispatcher {
        drops: Arc<AtomicUsize>,
        aborts: Arc<AtomicUsize>,
    }

    impl Drop for NativeDispatcher {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ProviderDispatcher<TestChannels> for NativeDispatcher {
        type Error = Infallible;

        async fn dispatch(
            &mut self,
            _context: &ProviderDispatchContext,
            _call: ProviderToolCall,
        ) -> Result<ProviderDispatchUpdate<TestChannels>, Self::Error> {
            Ok(ProviderDispatchUpdate::new(
                ProviderToolResponse::success("ok"),
                StreamUpdate::from_emission(TurnEmission::Commit(7)),
            ))
        }

        async fn abort(
            &mut self,
            _context: &ProviderDispatcherAbortContext,
        ) -> Result<ProviderDispatcherAbortAck, Self::Error> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderDispatcherAbortAck::CleanupCompleted)
        }
    }

    fn native_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<TestChannels> {
        let drops = Arc::clone(&cx.props().drops);
        let aborts = Arc::clone(&cx.props().aborts);
        system_view(provider_tool(
            "native-effect",
            ProviderToolSpec::new(
                "native-effect",
                "Produce one native Commit value",
                json!({ "type": "object", "properties": {} }),
            )
            .unwrap(),
            move || NativeDispatcher {
                drops: Arc::clone(&drops),
                aborts: Arc::clone(&aborts),
            },
        ))
    }

    #[derive(Debug, Clone, thiserror::Error)]
    #[error("scripted publication failure")]
    struct StoreFailure;

    #[derive(Clone, Copy)]
    enum Behavior {
        Commit,
        Reject,
        Indeterminate,
        ResolveCollision,
    }

    struct GatedStore {
        behavior: Behavior,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        committed: Arc<AtomicBool>,
        receipt: Arc<Mutex<Option<PublicationReceipt<u64>>>>,
        publish_calls: Arc<AtomicUsize>,
        resolve_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PublicationStore<(), u32> for GatedStore {
        type Version = u64;
        type Error = StoreFailure;

        async fn publish(
            &self,
            request: PublicationRequest<'_, (), u32, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            self.publish_calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            if matches!(self.behavior, Behavior::Reject) {
                return Err(PublicationWriteError::Rejected(StoreFailure));
            }
            if matches!(self.behavior, Behavior::ResolveCollision) {
                return Err(PublicationWriteError::Indeterminate(StoreFailure));
            }
            let mut stored_receipt = self.receipt.lock().unwrap();
            if let Some(receipt) = stored_receipt.as_ref() {
                return if receipt.request_id() != request.request_id()
                    || receipt.fingerprint().as_bytes() != request.fingerprint().as_bytes()
                {
                    Err(PublicationWriteError::Rejected(StoreFailure))
                } else {
                    Ok(receipt.clone())
                };
            }
            let receipt = PublicationReceipt::new(
                request.request_id().clone(),
                request.fingerprint().clone(),
                PublicationId::new("managed-publication-1").unwrap(),
                1,
                u64::try_from(request.outbox().len()).unwrap(),
            );
            *stored_receipt = Some(receipt.clone());
            drop(stored_receipt);
            self.committed.store(true, Ordering::SeqCst);
            if matches!(self.behavior, Behavior::Indeterminate) {
                Err(PublicationWriteError::Indeterminate(StoreFailure))
            } else {
                Ok(receipt)
            }
        }

        async fn resolve(
            &self,
            request_id: &PublicationRequestId,
            fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            if matches!(self.behavior, Behavior::ResolveCollision) {
                return Err(PublicationResolveError::CandidateCollision(StoreFailure));
            }
            match self.receipt.lock().unwrap().as_ref() {
                Some(receipt)
                    if receipt.request_id() == request_id
                        && receipt.fingerprint().as_bytes() == fingerprint.as_bytes() =>
                {
                    Ok(PublicationResolution::Published(receipt.clone()))
                }
                Some(receipt) if receipt.request_id() == request_id => {
                    Err(PublicationResolveError::CandidateCollision(StoreFailure))
                }
                Some(_) | None => Ok(PublicationResolution::NotCommitted),
            }
        }
    }

    fn gated_store(behavior: Behavior) -> Arc<GatedStore> {
        Arc::new(GatedStore {
            behavior,
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            committed: Arc::new(AtomicBool::new(false)),
            receipt: Arc::new(Mutex::new(None)),
            publish_calls: Arc::new(AtomicUsize::new(0)),
            resolve_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    struct BackoffStore {
        failures_before_not_committed: usize,
        resolve_calls: AtomicUsize,
        resolve_times: Mutex<Vec<Instant>>,
    }

    #[async_trait::async_trait]
    impl PublicationStore<(), u32> for BackoffStore {
        type Version = u64;
        type Error = StoreFailure;

        async fn publish(
            &self,
            _request: PublicationRequest<'_, (), u32, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            Err(PublicationWriteError::Indeterminate(StoreFailure))
        }

        async fn resolve(
            &self,
            _request_id: &PublicationRequestId,
            _fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            self.resolve_times.lock().unwrap().push(Instant::now());
            let call = self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures_before_not_committed {
                Err(PublicationResolveError::Unavailable(StoreFailure))
            } else {
                Ok(PublicationResolution::NotCommitted)
            }
        }
    }

    struct ResolveTimeoutStore {
        resolve_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl PublicationStore<(), u32> for ResolveTimeoutStore {
        type Version = u64;
        type Error = StoreFailure;

        async fn publish(
            &self,
            _request: PublicationRequest<'_, (), u32, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            Err(PublicationWriteError::Indeterminate(StoreFailure))
        }

        async fn resolve(
            &self,
            _request_id: &PublicationRequestId,
            _fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            if self.resolve_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return std::future::pending().await;
            }
            Ok(PublicationResolution::NotCommitted)
        }
    }

    async fn staged_streaming_fixture() -> (
        StagedStreamingPublication<TestChannels, (), u32, u64>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let drops = Arc::new(AtomicUsize::new(0));
        let aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &MountProps {
                drops: Arc::clone(&drops),
                aborts: Arc::clone(&aborts),
            },
            test_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("managed-publication");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let mut attempt = prepared.start_streaming_attempt(LiveRuntime).unwrap();
        let _ = attempt
            .on_event(TextTurnEvent::TextDelta("<effect />".to_owned()))
            .await
            .unwrap();
        let finished = attempt.finish_stream().await.unwrap();
        let publication = finished
            .stage_durable_publication(
                PublicationRequestId::new("managed/session-1").unwrap(),
                0_u64,
                PreparedSessionMutation::new(()),
                &Stager,
                &FingerprintFactory,
            )
            .unwrap();
        (publication, drops, aborts)
    }

    async fn managed_fixture(
        behavior: Behavior,
    ) -> (
        ManagedStreamingPublication<TestChannels, u64, StoreFailure>,
        Arc<GatedStore>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let (publication, drops, aborts) = staged_streaming_fixture().await;
        let store = gated_store(behavior);
        let managed = start_managed_streaming_publication(publication, Arc::clone(&store)).unwrap();
        (managed, store, drops, aborts)
    }

    async fn managed_provider_fixture(
        behavior: Behavior,
    ) -> (
        ManagedProviderPublication<TestChannels, u64, StoreFailure>,
        Arc<GatedStore>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let drops = Arc::new(AtomicUsize::new(0));
        let aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &MountProps {
                drops: Arc::clone(&drops),
                aborts: Arc::clone(&aborts),
            },
            native_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("managed-native-publication");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let mut attempt = prepared.start_provider_attempt(LiveRuntime).unwrap();
        let _ = attempt
            .call_tool(ProviderToolCall::new(
                "native-call-1",
                "native-effect",
                json!({}),
            ))
            .await
            .unwrap();
        let finished = attempt.finish().await.unwrap();
        let publication = finished
            .stage_durable_publication(
                PublicationRequestId::new("managed/native-session-1").unwrap(),
                0_u64,
                PreparedSessionMutation::new(()),
                &Stager,
                &FingerprintFactory,
            )
            .unwrap();
        let store = gated_store(behavior);
        let managed = start_managed_provider_publication(publication, Arc::clone(&store)).unwrap();
        (managed, store, drops, aborts)
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::SeqCst) < expected {
                yield_now().await;
            }
        })
        .await
        .expect("managed publication should settle");
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_cancel_committed_publication() {
        let (managed, store, drops, aborts) = managed_fixture(Behavior::Commit).await;
        let waiter = tokio::spawn(async move { managed.publish().await });
        store.entered.notified().await;
        waiter.abort();
        let _ = waiter.await;
        store.release.notify_one();

        wait_for_count(&drops, 1).await;
        assert!(store.committed.load(Ordering::SeqCst));
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 0);
        assert_eq!(aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn indeterminate_publication_is_resolved_after_final_handle_loss() {
        let (managed, store, drops, aborts) = managed_fixture(Behavior::Indeterminate).await;
        let waiter = tokio::spawn(async move { managed.publish().await });
        store.entered.notified().await;
        waiter.abort();
        let _ = waiter.await;
        store.release.notify_one();

        wait_for_count(&drops, 1).await;
        assert!(store.committed.load(Ordering::SeqCst));
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 1);
        assert_eq!(aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn known_rejection_aborts_after_final_handle_loss() {
        let (managed, store, drops, aborts) = managed_fixture(Behavior::Reject).await;
        let waiter = tokio::spawn(async move { managed.publish().await });
        store.entered.notified().await;
        waiter.abort();
        let _ = waiter.await;
        store.release.notify_one();

        wait_for_count(&drops, 1).await;
        assert!(!store.committed.load(Ordering::SeqCst));
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 0);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_publication_is_recovered_after_cancelled_waiter() {
        let (managed, store, drops, aborts) =
            managed_provider_fixture(Behavior::Indeterminate).await;
        let waiter = tokio::spawn(async move { managed.publish().await });
        store.entered.notified().await;
        waiter.abort();
        let _ = waiter.await;
        store.release.notify_one();

        wait_for_count(&drops, 1).await;
        assert!(store.committed.load(Ordering::SeqCst));
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 1);
        assert_eq!(aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejected_native_publication_aborts_dispatcher_after_cancelled_waiter() {
        let (managed, store, drops, aborts) = managed_provider_fixture(Behavior::Reject).await;
        let waiter = tokio::spawn(async move { managed.publish().await });
        store.entered.notified().await;
        waiter.abort();
        let _ = waiter.await;
        store.release.notify_one();

        wait_for_count(&drops, 1).await;
        assert!(!store.committed.load(Ordering::SeqCst));
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 0);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn publish_timeout_requires_authoritative_resolution_before_abort() {
        let (publication, drops, aborts) = staged_streaming_fixture().await;
        let store = gated_store(Behavior::Commit);
        let policy = ManagedPublicationPolicy {
            operation_timeout: Duration::from_millis(10),
            initial_recovery_backoff: Duration::from_millis(5),
            max_recovery_backoff: Duration::from_millis(20),
        };
        let managed = start_managed_publication(publication, Arc::clone(&store), policy).unwrap();

        let error = managed.publish().await.unwrap_err();
        assert!(matches!(
            error,
            ManagedPublicationError::OperationTimedOut {
                operation: ManagedPublicationOperation::Publish,
                ..
            }
        ));
        assert_eq!(
            managed.phase().await.unwrap(),
            ManagedPublicationPhase::Resolving
        );
        assert!(!store.committed.load(Ordering::SeqCst));
        assert_eq!(aborts.load(Ordering::SeqCst), 0);

        assert_eq!(
            managed.resolve().await.unwrap(),
            PublicationResolution::NotCommitted
        );
        assert_eq!(
            managed.phase().await.unwrap(),
            ManagedPublicationPhase::Ready
        );
        let _ = managed.abort(BindingAbortReason::Cancelled).await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disconnected_recovery_uses_exponential_backoff() {
        let (publication, drops, aborts) = staged_streaming_fixture().await;
        let store = Arc::new(BackoffStore {
            failures_before_not_committed: 2,
            resolve_calls: AtomicUsize::new(0),
            resolve_times: Mutex::new(Vec::new()),
        });
        let policy = ManagedPublicationPolicy {
            operation_timeout: Duration::from_secs(1),
            initial_recovery_backoff: Duration::from_millis(10),
            max_recovery_backoff: Duration::from_millis(40),
        };
        let managed = start_managed_publication(publication, Arc::clone(&store), policy).unwrap();

        assert!(matches!(
            managed.publish().await,
            Err(ManagedPublicationError::Transition(
                PublicationAttemptError::Indeterminate(_)
            ))
        ));
        drop(managed);
        wait_for_count(&drops, 1).await;

        let resolve_times = store.resolve_times.lock().unwrap();
        assert_eq!(resolve_times.len(), 3);
        assert!(
            resolve_times[1].duration_since(resolve_times[0]) >= policy.initial_recovery_backoff
        );
        assert!(
            resolve_times[2].duration_since(resolve_times[1])
                >= next_recovery_backoff(
                    policy.initial_recovery_backoff,
                    policy.max_recovery_backoff,
                )
        );
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resolve_timeout_retries_before_authoritative_abort() {
        let (publication, drops, aborts) = staged_streaming_fixture().await;
        let store = Arc::new(ResolveTimeoutStore {
            resolve_calls: AtomicUsize::new(0),
        });
        let policy = ManagedPublicationPolicy {
            operation_timeout: Duration::from_millis(10),
            initial_recovery_backoff: Duration::from_millis(5),
            max_recovery_backoff: Duration::from_millis(20),
        };
        let managed = start_managed_publication(publication, Arc::clone(&store), policy).unwrap();

        assert!(matches!(
            managed.publish().await,
            Err(ManagedPublicationError::Transition(
                PublicationAttemptError::Indeterminate(_)
            ))
        ));
        drop(managed);
        wait_for_count(&drops, 1).await;

        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 2);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resolve_collision_is_terminal_and_aborts_without_retry() {
        let (managed, store, drops, aborts) = managed_fixture(Behavior::ResolveCollision).await;

        let publish = managed.publish();
        let release_store = async {
            store.entered.notified().await;
            store.release.notify_one();
        };
        let (result, ()) = tokio::join!(publish, release_store);
        assert!(matches!(
            result,
            Err(ManagedPublicationError::Transition(
                PublicationAttemptError::Indeterminate(_)
            ))
        ));
        drop(managed);
        wait_for_count(&drops, 1).await;

        assert!(!store.committed.load(Ordering::SeqCst));
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.resolve_calls.load(Ordering::SeqCst), 1);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn no_runtime_start_error_returns_staged_publication_for_abort() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (publication, drops, aborts) = runtime.block_on(staged_streaming_fixture());
        drop(runtime);

        let error = start_managed_streaming_publication(publication, gated_store(Behavior::Commit))
            .unwrap_err();
        let publication = error.into_publication();
        assert_eq!(publication.phase(), PublicationPhase::Ready);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let finished = match publication.into_finished_if_ready() {
                Ok(finished) => finished,
                Err(_) => panic!("start failure must retain a ready publication"),
            };
            let _ = finished.abort(BindingAbortReason::Cancelled).await;
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }
}
