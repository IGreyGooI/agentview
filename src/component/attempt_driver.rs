//! Private object-safe driver proof for the mounted text/native attempt path.
//!
//! This module is intentionally crate-private until the final AgentLoop owner
//! controls cancellation and guarantees explicit abort. Root-specific values
//! never cross the object-safe boundary as `Any`: a monomorphized interpreter
//! receives each complete typed update before the non-generic owner proceeds.

use std::{
    convert::Infallible, error::Error, fmt, future::Future, marker::PhantomData, pin::Pin,
    sync::Arc,
};

use crate::llm_call::TextTurnEvent;

use super::{
    managed_publication::{
        erase_managed_streaming_publication, start_managed_streaming_publication_on,
        ManagedDurablePublication, ManagedStreamingPublication,
    },
    BindingAbortReason, ChannelTypeInfo, ChannelTypeMismatch, CommitStager,
    FinishedStreamingAttempt, LiveEffectRuntime, MountedStreamingAttempt, PreparedUserTurn,
    ProviderAttemptIdentity, ProviderToolAttemptError, ProviderToolCall, ProviderToolResult,
    PublicationFingerprintFactory, PublicationStagingPlan, PublicationStore,
    PublishedStreamingAttempt, PublishedTurnReceipt, StagedStreamingPublication, StreamUpdate,
    StreamingAbortReport, StreamingAttemptError, StreamingAttemptStartError,
    StreamingFinishFailure, TurnChannels, TurnEmission, TurnPublication, TurnPublisher,
};

/// Typed host boundary installed before a root contract is erased.
///
/// The interpreter owns application-specific handling of Output and Diagnostic
/// values. Live values have already been awaited by the mounted attempt, while
/// Commit values remain private until publication.
pub(crate) trait AttemptUpdateInterpreter<C>: Send + 'static
where
    C: TurnChannels,
{
    type Error: Error + Send + Sync + 'static;

    fn interpret(
        &mut self,
        identity: &ProviderAttemptIdentity,
        update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) -> Result<(), Self::Error>;
}

/// Typed post-publication delivery boundary installed before root erasure.
///
/// The receiver gets the framework-issued publication receipt plus the entire
/// still-retained Commit batch. A delivery error leaves the batch in the erased
/// published state so the owner can retry without running the model again.
///
/// This is an attempt-local, at-least-once proof. The receipt is not a durable
/// idempotency key: an interpreter that may partially apply a batch must either
/// make that application atomic or first persist a mapping to a host-issued
/// durable publication identity, then combine that identity with each stable
/// batch index.
#[async_trait::async_trait]
pub(crate) trait AttemptCommitInterpreter<C>: Send + 'static
where
    C: TurnChannels,
{
    type Error: Error + Send + Sync + 'static;

    async fn deliver(
        &mut self,
        receipt: &PublishedTurnReceipt,
        commits: &[C::Commit],
    ) -> Result<(), Self::Error>;
}

type BoxCommitDeliveryError = Box<dyn Error + Send + Sync>;

#[async_trait::async_trait]
trait ErasedCommitInterpreter<C>: Send
where
    C: TurnChannels,
{
    async fn deliver(
        &mut self,
        receipt: &PublishedTurnReceipt,
        commits: &[C::Commit],
    ) -> Result<(), BoxCommitDeliveryError>;
}

#[async_trait::async_trait]
impl<C, I> ErasedCommitInterpreter<C> for I
where
    C: TurnChannels,
    I: AttemptCommitInterpreter<C>,
{
    async fn deliver(
        &mut self,
        receipt: &PublishedTurnReceipt,
        commits: &[C::Commit],
    ) -> Result<(), BoxCommitDeliveryError> {
        AttemptCommitInterpreter::deliver(self, receipt, commits)
            .await
            .map_err(|source| Box::new(source) as BoxCommitDeliveryError)
    }
}

/// Finished model data available while the host constructs a durable mutation.
///
/// Process-local attempt identity is intentionally absent. A plan factory owns
/// any stable request identity and expected revision allocated by the host.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DurablePublicationPlanContext<'a> {
    raw_output: &'a str,
    provider_results: &'a [ProviderToolResult],
}

impl<'a> DurablePublicationPlanContext<'a> {
    pub(crate) fn raw_output(&self) -> &'a str {
        self.raw_output
    }

    pub(crate) fn provider_results(&self) -> &'a [ProviderToolResult] {
        self.provider_results
    }
}

/// Pure host boundary that builds the complete session mutation after finish.
pub(crate) trait DurablePublicationPlanFactory<Mutation, Version, Input = ()>:
    Send + 'static
{
    type Error: Error + Send + Sync + 'static;

    fn prepare(
        &mut self,
        input: Input,
        context: DurablePublicationPlanContext<'_>,
    ) -> Result<PublicationStagingPlan<Mutation, Version>, Self::Error>;
}

/// Passes an owner-built plan directly through the Finished handoff.
///
/// The plan remains a linear owned value. There is no shared slot between the
/// mounted owner and the monomorphized durable finalizer.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SuppliedPublicationPlanFactory;

impl<Mutation, Version>
    DurablePublicationPlanFactory<Mutation, Version, PublicationStagingPlan<Mutation, Version>>
    for SuppliedPublicationPlanFactory
where
    Mutation: Send + 'static,
    Version: Send + 'static,
{
    type Error = Infallible;

    fn prepare(
        &mut self,
        input: PublicationStagingPlan<Mutation, Version>,
        _context: DurablePublicationPlanContext<'_>,
    ) -> Result<PublicationStagingPlan<Mutation, Version>, Self::Error> {
        Ok(input)
    }
}

impl<Mutation, Version, E, F> DurablePublicationPlanFactory<Mutation, Version> for F
where
    E: Error + Send + Sync + 'static,
    F: for<'a> FnMut(
            DurablePublicationPlanContext<'a>,
        ) -> Result<PublicationStagingPlan<Mutation, Version>, E>
        + Send
        + 'static,
{
    type Error = E;

    fn prepare(
        &mut self,
        _input: (),
        context: DurablePublicationPlanContext<'_>,
    ) -> Result<PublicationStagingPlan<Mutation, Version>, Self::Error> {
        self(context)
    }
}

/// Typed staging failure returned to the still-monomorphized finalizer.
#[must_use = "a durable staging failure retains the finished attempt"]
pub(crate) struct DurablePublicationFinalizationFailure<C>
where
    C: TurnChannels,
{
    finished: Box<FinishedStreamingAttempt<C>>,
    source: Box<dyn Error + Send + Sync>,
}

impl<C> DurablePublicationFinalizationFailure<C>
where
    C: TurnChannels,
{
    fn new(
        finished: FinishedStreamingAttempt<C>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            finished: Box::new(finished),
            source: Box::new(source),
        }
    }

    fn into_parts(self) -> (FinishedStreamingAttempt<C>, Box<dyn Error + Send + Sync>) {
        (*self.finished, self.source)
    }
}

/// Monomorphized host adapter installed before the root channel is erased.
pub(crate) trait DurablePublicationFinalizer<C, Version, StoreError, Input = ()>:
    Send + 'static
where
    C: TurnChannels,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
{
    fn stage_and_start(
        &mut self,
        finished: FinishedStreamingAttempt<C>,
        input: Input,
        runtime: &tokio::runtime::Handle,
    ) -> Result<
        ManagedDurablePublication<Version, StoreError>,
        DurablePublicationFinalizationFailure<C>,
    >;
}

/// A launcher rejected ownership before starting a publication actor.
///
/// The untouched Ready publication is returned so the finalizer can restore
/// the exact Finished attempt and staging plan.
pub(crate) struct DurablePublicationLaunchFailure<Publication> {
    publication: Box<Publication>,
    source: Box<dyn Error + Send + Sync>,
}

impl<Publication> DurablePublicationLaunchFailure<Publication> {
    pub(crate) fn new(
        publication: Publication,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            publication: Box::new(publication),
            source: Box::new(source),
        }
    }

    fn into_parts(self) -> (Publication, Box<dyn Error + Send + Sync>) {
        (*self.publication, self.source)
    }
}

type DurableStreamingPublicationLaunchResult<C, Mutation, Payload, Version, Store> = Result<
    ManagedStreamingPublication<C, Version, <Store as PublicationStore<Mutation, Payload>>::Error>,
    DurablePublicationLaunchFailure<StagedStreamingPublication<C, Mutation, Payload, Version>>,
>;

/// Transfers a Ready publication candidate to a managed publication actor.
///
/// Returning `Err` means no actor or task was started and no ownership was
/// transferred. The failure must contain the same untouched Ready candidate
/// passed to `start`; the finalizer relies on that guarantee to restore the
/// exact Finished attempt and staging plan for retry.
pub(crate) trait DurableStreamingPublicationLauncher<C, Mutation, Payload, Version, Store>:
    Send + 'static
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    Store: PublicationStore<Mutation, Payload, Version = Version>,
{
    fn start(
        &mut self,
        runtime: &tokio::runtime::Handle,
        publication: StagedStreamingPublication<C, Mutation, Payload, Version>,
        store: Arc<Store>,
    ) -> DurableStreamingPublicationLaunchResult<C, Mutation, Payload, Version, Store>;
}

/// Tokio launcher used after the old actor has supplied its own live runtime.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TokioDurablePublicationLauncher;

impl<C, Mutation, Payload, Version, Store>
    DurableStreamingPublicationLauncher<C, Mutation, Payload, Version, Store>
    for TokioDurablePublicationLauncher
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Payload: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    Store: PublicationStore<Mutation, Payload, Version = Version>,
{
    fn start(
        &mut self,
        runtime: &tokio::runtime::Handle,
        publication: StagedStreamingPublication<C, Mutation, Payload, Version>,
        store: Arc<Store>,
    ) -> Result<
        ManagedStreamingPublication<C, Version, Store::Error>,
        DurablePublicationLaunchFailure<StagedStreamingPublication<C, Mutation, Payload, Version>>,
    > {
        Ok(start_managed_streaming_publication_on(
            runtime,
            publication,
            store,
        ))
    }
}

/// Default host adapter for plan construction, Commit staging, fingerprinting,
/// and actor-to-actor transfer into the durable publication owner.
pub(crate) struct DurableStreamingPublicationFinalizer<
    C,
    Mutation,
    Version,
    PlanFactory,
    Stager,
    FingerprintFactory,
    Store,
    Launcher = TokioDurablePublicationLauncher,
> where
    C: TurnChannels,
{
    plan_factory: PlanFactory,
    retry_plan: Option<PublicationStagingPlan<Mutation, Version>>,
    stager: Stager,
    fingerprint_factory: FingerprintFactory,
    store: Arc<Store>,
    launcher: Launcher,
    channels: PhantomData<fn() -> C>,
}

impl<C, Mutation, Version, PlanFactory, Stager, FingerprintFactory, Store>
    DurableStreamingPublicationFinalizer<
        C,
        Mutation,
        Version,
        PlanFactory,
        Stager,
        FingerprintFactory,
        Store,
        TokioDurablePublicationLauncher,
    >
where
    C: TurnChannels,
{
    pub(crate) fn new(
        plan_factory: PlanFactory,
        stager: Stager,
        fingerprint_factory: FingerprintFactory,
        store: Arc<Store>,
    ) -> Self {
        Self {
            plan_factory,
            retry_plan: None,
            stager,
            fingerprint_factory,
            store,
            launcher: TokioDurablePublicationLauncher,
            channels: PhantomData,
        }
    }
}

impl<C, Mutation, Version, PlanFactory, Stager, FingerprintFactory, Store, Launcher>
    DurableStreamingPublicationFinalizer<
        C,
        Mutation,
        Version,
        PlanFactory,
        Stager,
        FingerprintFactory,
        Store,
        Launcher,
    >
where
    C: TurnChannels,
{
    pub(crate) fn with_launcher<NextLauncher>(
        self,
        launcher: NextLauncher,
    ) -> DurableStreamingPublicationFinalizer<
        C,
        Mutation,
        Version,
        PlanFactory,
        Stager,
        FingerprintFactory,
        Store,
        NextLauncher,
    > {
        DurableStreamingPublicationFinalizer {
            plan_factory: self.plan_factory,
            retry_plan: self.retry_plan,
            stager: self.stager,
            fingerprint_factory: self.fingerprint_factory,
            store: self.store,
            launcher,
            channels: PhantomData,
        }
    }
}

impl<C, Mutation, Version, PlanFactory, Stager, FingerprintFactory, Store, Launcher, Input>
    DurablePublicationFinalizer<C, Version, Store::Error, Input>
    for DurableStreamingPublicationFinalizer<
        C,
        Mutation,
        Version,
        PlanFactory,
        Stager,
        FingerprintFactory,
        Store,
        Launcher,
    >
where
    C: TurnChannels,
    Mutation: Send + Sync + 'static,
    Version: Clone + Send + Sync + 'static,
    PlanFactory: DurablePublicationPlanFactory<Mutation, Version, Input>,
    Stager: CommitStager<C>,
    FingerprintFactory: PublicationFingerprintFactory<Mutation, Stager::Payload, Version>,
    Store: PublicationStore<Mutation, Stager::Payload, Version = Version>,
    Launcher: DurableStreamingPublicationLauncher<C, Mutation, Stager::Payload, Version, Store>,
{
    fn stage_and_start(
        &mut self,
        finished: FinishedStreamingAttempt<C>,
        input: Input,
        runtime: &tokio::runtime::Handle,
    ) -> Result<
        ManagedDurablePublication<Version, Store::Error>,
        DurablePublicationFinalizationFailure<C>,
    > {
        let plan = match self.retry_plan.take() {
            Some(plan) => plan,
            None => {
                let prepared = self.plan_factory.prepare(
                    input,
                    DurablePublicationPlanContext {
                        raw_output: finished.raw_output(),
                        provider_results: finished.provider_results(),
                    },
                );
                match prepared {
                    Ok(plan) => plan,
                    Err(source) => {
                        return Err(DurablePublicationFinalizationFailure::new(finished, source));
                    }
                }
            }
        };
        let (request_id, expected_revision, mutation) = plan.into_parts();
        let staged = finished.stage_durable_publication(
            request_id,
            expected_revision,
            mutation,
            &self.stager,
            &self.fingerprint_factory,
        );
        let staged = match staged {
            Ok(staged) => staged,
            Err(failure) => {
                let (finished, plan, source) = failure.into_parts();
                self.retry_plan = Some(plan);
                return Err(DurablePublicationFinalizationFailure::new(finished, source));
            }
        };
        let managed = match self
            .launcher
            .start(runtime, staged, Arc::clone(&self.store))
        {
            Ok(managed) => managed,
            Err(failure) => {
                let (staged, source) = failure.into_parts();
                let (finished, plan) =
                    staged
                        .into_finished_and_plan_if_ready()
                        .unwrap_or_else(|_| {
                            unreachable!("a rejected launch retains a Ready candidate")
                        });
                self.retry_plan = Some(plan);
                return Err(DurablePublicationFinalizationFailure {
                    finished: Box::new(finished),
                    source,
                });
            }
        };
        Ok(erase_managed_streaming_publication(managed))
    }
}

/// Closed provider ingress understood by the current text AgentLoop path.
#[derive(Debug)]
pub(crate) enum ProviderWireInput {
    Text(TextTurnEvent),
    Tool(ProviderToolCall),
}

/// Provider-neutral acknowledgement returned after typed update delivery.
#[derive(Debug)]
pub(crate) enum AttemptDriverStep {
    TextAccepted,
    ToolResult {
        result: ProviderToolResult,
        replayed: bool,
    },
}

impl AttemptDriverStep {
    pub(crate) fn tool_result(&self) -> Option<&ProviderToolResult> {
        match self {
            Self::TextAccepted => None,
            Self::ToolResult { result, .. } => Some(result),
        }
    }

    pub(crate) fn replayed(&self) -> Option<bool> {
        match self {
            Self::TextAccepted => None,
            Self::ToolResult { replayed, .. } => Some(*replayed),
        }
    }
}

/// Type-erased failure from the typed update interpreter.
#[derive(Debug, thiserror::Error)]
#[error("typed attempt update interpretation failed: {source}")]
pub(crate) struct AttemptInterpretationFault {
    #[source]
    source: Box<dyn Error + Send + Sync>,
}

impl AttemptInterpretationFault {
    fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

/// Terminal failure while driving an active erased attempt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AttemptDriverFault {
    #[error(transparent)]
    Streaming(#[from] StreamingAttemptError),

    #[error(transparent)]
    Tool(#[from] ProviderToolAttemptError),

    #[error(transparent)]
    Interpretation(#[from] AttemptInterpretationFault),

    #[error("erased attempt is terminal after a prior failure")]
    Terminal,
}

/// Failure before any reducer or provider dispatcher is instantiated.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ErasedAttemptStartError {
    #[error(transparent)]
    Contract(#[from] ChannelTypeMismatch),

    #[error(transparent)]
    Runtime(#[from] StreamingAttemptStartError),
}

#[async_trait::async_trait]
trait ErasedAttemptOperations<Version, StoreError, PublicationInput>: fmt::Debug + Send
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn channel_types(&self) -> &ChannelTypeInfo;
    fn identity(&self) -> &ProviderAttemptIdentity;
    fn binding_count(&self) -> usize;
    fn dispatcher_count(&self) -> usize;
    fn provider_tool_count(&self) -> usize;
    fn raw_output(&self) -> &str;
    fn provider_results(&self) -> &[ProviderToolResult];

    async fn drive(
        &mut self,
        input: ProviderWireInput,
    ) -> Result<AttemptDriverStep, AttemptDriverFault>;

    async fn finish(
        self: Box<Self>,
    ) -> Result<ErasedFinishedAttempt<Version, StoreError, PublicationInput>, ErasedFinishFailure>;

    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport;
}

struct TypedAttemptOperations<C, I, Version, StoreError, PublicationInput>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    channel_types: ChannelTypeInfo,
    attempt: MountedStreamingAttempt<C>,
    interpreter: I,
    commit_interpreter: Option<Box<dyn ErasedCommitInterpreter<C>>>,
    durable_finalizer:
        Option<Box<dyn DurablePublicationFinalizer<C, Version, StoreError, PublicationInput>>>,
    terminal: bool,
}

impl<C, I, Version, StoreError, PublicationInput> fmt::Debug
    for TypedAttemptOperations<C, I, Version, StoreError, PublicationInput>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypedAttemptOperations")
            .field("channel_types", &self.channel_types)
            .field("attempt", &self.attempt)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<C, I, Version, StoreError, PublicationInput>
    ErasedAttemptOperations<Version, StoreError, PublicationInput>
    for TypedAttemptOperations<C, I, Version, StoreError, PublicationInput>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn channel_types(&self) -> &ChannelTypeInfo {
        &self.channel_types
    }

    fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    fn binding_count(&self) -> usize {
        self.attempt.binding_count()
    }

    fn dispatcher_count(&self) -> usize {
        self.attempt.dispatcher_count()
    }

    fn provider_tool_count(&self) -> usize {
        self.attempt.provider_tool_count()
    }

    fn raw_output(&self) -> &str {
        self.attempt.raw_output()
    }

    fn provider_results(&self) -> &[ProviderToolResult] {
        self.attempt.provider_results()
    }

    async fn drive(
        &mut self,
        input: ProviderWireInput,
    ) -> Result<AttemptDriverStep, AttemptDriverFault> {
        if self.terminal {
            return Err(AttemptDriverFault::Terminal);
        }

        let result = async {
            match input {
                ProviderWireInput::Text(event) => {
                    let update = self.attempt.on_event(event).await?;
                    self.interpreter
                        .interpret(self.attempt.identity(), update)
                        .map_err(AttemptInterpretationFault::new)?;
                    Ok(AttemptDriverStep::TextAccepted)
                }
                ProviderWireInput::Tool(call) => {
                    let outcome = self.attempt.call_tool(call).await?;
                    let replayed = outcome.replayed();
                    let (result, update) = outcome.into_parts();
                    self.interpreter
                        .interpret(self.attempt.identity(), update)
                        .map_err(AttemptInterpretationFault::new)?;
                    Ok(AttemptDriverStep::ToolResult { result, replayed })
                }
            }
        }
        .await;
        if result.is_err() {
            self.terminal = true;
        }
        result
    }

    async fn finish(
        self: Box<Self>,
    ) -> Result<ErasedFinishedAttempt<Version, StoreError, PublicationInput>, ErasedFinishFailure>
    {
        let Self {
            channel_types,
            attempt,
            mut interpreter,
            commit_interpreter,
            durable_finalizer,
            terminal,
        } = *self;
        let identity = attempt.identity().clone();

        if terminal {
            return Err(ErasedFinishFailure::active(
                channel_types,
                attempt,
                AttemptDriverFault::Terminal,
            ));
        }

        let mut finished = match attempt.finish_stream().await {
            Ok(finished) => finished,
            Err(failure) => {
                return Err(ErasedFinishFailure::runtime(channel_types, failure));
            }
        };
        let update = finished.take_update();
        if let Err(source) = interpreter.interpret(&identity, update) {
            return Err(ErasedFinishFailure::finished(
                channel_types,
                finished,
                AttemptDriverFault::Interpretation(AttemptInterpretationFault::new(source)),
            ));
        }

        Ok(ErasedFinishedAttempt {
            operations: Box::new(TypedFinishedOperations {
                channel_types,
                finished,
                interpreter,
                commit_interpreter,
                durable_finalizer,
            }),
        })
    }

    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }
}

/// Root-channel-erased owner for one active combined text/native attempt.
#[must_use = "an active erased attempt must be finished or explicitly aborted"]
pub(crate) struct ErasedActiveAttempt<Version = (), StoreError = Infallible, PublicationInput = ()>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    operations: Box<dyn ErasedAttemptOperations<Version, StoreError, PublicationInput>>,
}

impl<Version, StoreError, PublicationInput> fmt::Debug
    for ErasedActiveAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedActiveAttempt")
            .field("identity", self.identity())
            .field("channel_types", self.channel_types())
            .finish_non_exhaustive()
    }
}

impl<Version, StoreError, PublicationInput>
    ErasedActiveAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    pub(crate) fn channel_types(&self) -> &ChannelTypeInfo {
        self.operations.channel_types()
    }

    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        self.operations.identity()
    }

    pub(crate) fn binding_count(&self) -> usize {
        self.operations.binding_count()
    }

    pub(crate) fn dispatcher_count(&self) -> usize {
        self.operations.dispatcher_count()
    }

    pub(crate) fn provider_tool_count(&self) -> usize {
        self.operations.provider_tool_count()
    }

    pub(crate) fn raw_output(&self) -> &str {
        self.operations.raw_output()
    }

    pub(crate) fn provider_results(&self) -> &[ProviderToolResult] {
        self.operations.provider_results()
    }

    pub(crate) async fn drive(
        &mut self,
        input: ProviderWireInput,
    ) -> Result<AttemptDriverStep, AttemptDriverFault> {
        self.operations.drive(input).await
    }

    pub(crate) async fn finish(
        self,
    ) -> Result<ErasedFinishedAttempt<Version, StoreError, PublicationInput>, ErasedFinishFailure>
    {
        self.operations.finish().await
    }

    pub(crate) async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.operations.abort(reason).await
    }
}

/// Start an object-safe driver only after the owner contract is verified.
///
/// The check happens before `start_streaming_attempt`, so a mismatch cannot
/// instantiate reducer state or provider dispatchers, let alone send a request.
pub(crate) fn start_erased_streaming_attempt<C, Props, R, I>(
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    live_runtime: R,
    interpreter: I,
) -> Result<ErasedActiveAttempt, ErasedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
{
    start_erased_streaming_attempt_with_optional_runtime_boundaries::<
        C,
        Props,
        R,
        I,
        (),
        Infallible,
        (),
    >(
        prepared,
        owner_contract,
        None,
        live_runtime,
        interpreter,
        None,
        None,
    )
}

/// Start an erased attempt with a typed post-publication Commit interpreter.
///
/// The interpreter is retained behind the same monomorphized shim as typed
/// Output/Diagnostic delivery. It is never invoked before the publisher has
/// succeeded, and delivery failure returns the exact published state for retry.
pub(crate) fn start_erased_streaming_attempt_with_commit_interpreter<C, Props, R, I, K>(
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    live_runtime: R,
    interpreter: I,
    commit_interpreter: K,
) -> Result<ErasedActiveAttempt, ErasedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
    K: AttemptCommitInterpreter<C>,
{
    start_erased_streaming_attempt_with_optional_runtime_boundaries::<
        C,
        Props,
        R,
        I,
        (),
        Infallible,
        (),
    >(
        prepared,
        owner_contract,
        None,
        live_runtime,
        interpreter,
        Some(Box::new(commit_interpreter)),
        None,
    )
}

/// Start an erased attempt with a durable finalizer already monomorphized for
/// the root channel contract.
pub(crate) fn start_erased_streaming_attempt_with_durable_finalizer<
    C,
    Props,
    R,
    I,
    F,
    Version,
    StoreError,
    PublicationInput,
>(
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    live_runtime: R,
    interpreter: I,
    durable_finalizer: F,
) -> Result<ErasedActiveAttempt<Version, StoreError, PublicationInput>, ErasedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
    F: DurablePublicationFinalizer<C, Version, StoreError, PublicationInput>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    start_erased_streaming_attempt_with_optional_runtime_boundaries(
        prepared,
        owner_contract,
        None,
        live_runtime,
        interpreter,
        None,
        Some(Box::new(durable_finalizer)),
    )
}

/// Start an erased durable attempt with an identity already allocated by its
/// mounted host binding.
pub(crate) fn start_erased_streaming_attempt_with_durable_finalizer_and_identity<
    C,
    Props,
    R,
    I,
    F,
    Version,
    StoreError,
    PublicationInput,
>(
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    identity: ProviderAttemptIdentity,
    live_runtime: R,
    interpreter: I,
    durable_finalizer: F,
) -> Result<ErasedActiveAttempt<Version, StoreError, PublicationInput>, ErasedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
    F: DurablePublicationFinalizer<C, Version, StoreError, PublicationInput>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    start_erased_streaming_attempt_with_optional_runtime_boundaries(
        prepared,
        owner_contract,
        Some(identity),
        live_runtime,
        interpreter,
        None,
        Some(Box::new(durable_finalizer)),
    )
}

fn start_erased_streaming_attempt_with_optional_runtime_boundaries<
    C,
    Props,
    R,
    I,
    Version,
    StoreError,
    PublicationInput,
>(
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    identity: Option<ProviderAttemptIdentity>,
    live_runtime: R,
    interpreter: I,
    commit_interpreter: Option<Box<dyn ErasedCommitInterpreter<C>>>,
    durable_finalizer: Option<
        Box<dyn DurablePublicationFinalizer<C, Version, StoreError, PublicationInput>>,
    >,
) -> Result<ErasedActiveAttempt<Version, StoreError, PublicationInput>, ErasedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    owner_contract.ensure::<C, Props>()?;
    let attempt = match identity {
        Some(identity) => prepared.start_streaming_attempt_with_identity(identity, live_runtime)?,
        None => prepared.start_streaming_attempt(live_runtime)?,
    };
    Ok(ErasedActiveAttempt {
        operations: Box::new(TypedAttemptOperations {
            channel_types: owner_contract.clone(),
            attempt,
            interpreter,
            commit_interpreter,
            durable_finalizer,
            terminal: false,
        }),
    })
}

#[async_trait::async_trait]
trait ErasedAbortableAttempt: fmt::Debug + Send {
    fn identity(&self) -> &ProviderAttemptIdentity;
    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport;
}

struct ActiveAbortState<C>
where
    C: TurnChannels,
{
    attempt: MountedStreamingAttempt<C>,
}

impl<C> fmt::Debug for ActiveAbortState<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveAbortState")
            .field("identity", self.attempt.identity())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<C> ErasedAbortableAttempt for ActiveAbortState<C>
where
    C: TurnChannels,
{
    fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }
}

struct FinishedAbortState<C>
where
    C: TurnChannels,
{
    finished: super::FinishedStreamingAttempt<C>,
}

impl<C> fmt::Debug for FinishedAbortState<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FinishedAbortState")
            .field("identity", self.finished.identity())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<C> ErasedAbortableAttempt for FinishedAbortState<C>
where
    C: TurnChannels,
{
    fn identity(&self) -> &ProviderAttemptIdentity {
        self.finished.identity()
    }

    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport {
        self.finished.abort(reason).await
    }
}

/// Finish failure retaining exactly one abortable pre-publication state.
#[must_use = "a failed erased finish must explicitly abort its retained attempt"]
pub(crate) struct ErasedFinishFailure {
    channel_types: ChannelTypeInfo,
    attempt: Box<dyn ErasedAbortableAttempt>,
    error: AttemptDriverFault,
}

impl ErasedFinishFailure {
    fn active<C>(
        channel_types: ChannelTypeInfo,
        attempt: MountedStreamingAttempt<C>,
        error: AttemptDriverFault,
    ) -> Self
    where
        C: TurnChannels,
    {
        Self {
            channel_types,
            attempt: Box::new(ActiveAbortState { attempt }),
            error,
        }
    }

    fn runtime<C>(channel_types: ChannelTypeInfo, failure: StreamingFinishFailure<C>) -> Self
    where
        C: TurnChannels,
    {
        let (attempt, error) = failure.into_parts();
        Self {
            channel_types,
            attempt: Box::new(ActiveAbortState { attempt }),
            error: AttemptDriverFault::Streaming(error),
        }
    }

    fn finished<C>(
        channel_types: ChannelTypeInfo,
        finished: super::FinishedStreamingAttempt<C>,
        error: AttemptDriverFault,
    ) -> Self
    where
        C: TurnChannels,
    {
        Self {
            channel_types,
            attempt: Box::new(FinishedAbortState { finished }),
            error,
        }
    }

    pub(crate) fn channel_types(&self) -> &ChannelTypeInfo {
        &self.channel_types
    }

    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub(crate) fn error(&self) -> &AttemptDriverFault {
        &self.error
    }

    pub(crate) async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }

    /// Consume the retained abortable state while preserving the terminal
    /// driver error for an owner that must report both outcomes together.
    pub(crate) async fn into_error_and_abort(
        self,
        reason: BindingAbortReason,
    ) -> (AttemptDriverFault, StreamingAbortReport) {
        let Self { attempt, error, .. } = self;
        let report = attempt.abort(reason).await;
        (error, report)
    }
}

impl fmt::Debug for ErasedFinishFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedFinishFailure")
            .field("identity", self.identity())
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the finished attempt has no durable publication finalizer")]
struct MissingDurablePublicationFinalizer;

/// Type-erased staging fault while the old actor still owns Finished state.
#[derive(Debug, thiserror::Error)]
#[error("durable publication handoff preparation failed: {source}")]
pub(crate) struct ErasedDurableHandoffFault {
    #[source]
    source: Box<dyn Error + Send + Sync>,
}

/// Failed handoff retaining the exact object-safe finished attempt for retry.
#[must_use = "a failed durable handoff retains the finished attempt"]
pub(crate) struct ErasedDurableHandoffFailure<
    Version = (),
    StoreError = Infallible,
    PublicationInput = (),
> where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    attempt: ErasedFinishedAttempt<Version, StoreError, PublicationInput>,
    fault: ErasedDurableHandoffFault,
}

impl<Version, StoreError, PublicationInput>
    ErasedDurableHandoffFailure<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn new(
        attempt: ErasedFinishedAttempt<Version, StoreError, PublicationInput>,
        source: Box<dyn Error + Send + Sync>,
    ) -> Self {
        Self {
            attempt,
            fault: ErasedDurableHandoffFault { source },
        }
    }

    pub(crate) fn fault(&self) -> &ErasedDurableHandoffFault {
        &self.fault
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        ErasedFinishedAttempt<Version, StoreError, PublicationInput>,
        ErasedDurableHandoffFault,
    ) {
        (self.attempt, self.fault)
    }
}

impl<Version, StoreError, PublicationInput> fmt::Debug
    for ErasedDurableHandoffFailure<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedDurableHandoffFailure")
            .field("identity", self.attempt.identity())
            .field("fault", &self.fault)
            .finish()
    }
}

#[async_trait::async_trait]
trait ErasedFinishedOperations<Version, StoreError, PublicationInput>: fmt::Debug + Send
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn channel_types(&self) -> &ChannelTypeInfo;
    fn identity(&self) -> &ProviderAttemptIdentity;
    fn raw_output(&self) -> &str;
    fn provider_results(&self) -> &[ProviderToolResult];
    fn begin_durable_publication(
        self: Box<Self>,
        input: PublicationInput,
        runtime: &tokio::runtime::Handle,
    ) -> Result<
        ManagedDurablePublication<Version, StoreError>,
        ErasedDurableHandoffFailure<Version, StoreError, PublicationInput>,
    >;
    async fn after_publish(self: Box<Self>) -> ErasedPublishedAttempt;
    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport;
}

struct TypedFinishedOperations<C, I, Version, StoreError, PublicationInput>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    channel_types: ChannelTypeInfo,
    finished: super::FinishedStreamingAttempt<C>,
    interpreter: I,
    commit_interpreter: Option<Box<dyn ErasedCommitInterpreter<C>>>,
    durable_finalizer:
        Option<Box<dyn DurablePublicationFinalizer<C, Version, StoreError, PublicationInput>>>,
}

impl<C, I, Version, StoreError, PublicationInput> fmt::Debug
    for TypedFinishedOperations<C, I, Version, StoreError, PublicationInput>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypedFinishedOperations")
            .field("identity", self.finished.identity())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<C, I, Version, StoreError, PublicationInput>
    ErasedFinishedOperations<Version, StoreError, PublicationInput>
    for TypedFinishedOperations<C, I, Version, StoreError, PublicationInput>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn channel_types(&self) -> &ChannelTypeInfo {
        &self.channel_types
    }

    fn identity(&self) -> &ProviderAttemptIdentity {
        self.finished.identity()
    }

    fn raw_output(&self) -> &str {
        self.finished.raw_output()
    }

    fn provider_results(&self) -> &[ProviderToolResult] {
        self.finished.provider_results()
    }

    fn begin_durable_publication(
        self: Box<Self>,
        input: PublicationInput,
        runtime: &tokio::runtime::Handle,
    ) -> Result<
        ManagedDurablePublication<Version, StoreError>,
        ErasedDurableHandoffFailure<Version, StoreError, PublicationInput>,
    > {
        let Self {
            channel_types,
            finished,
            interpreter,
            commit_interpreter,
            mut durable_finalizer,
        } = *self;
        let Some(finalizer) = durable_finalizer.as_mut() else {
            let attempt = ErasedFinishedAttempt {
                operations: Box::new(TypedFinishedOperations {
                    channel_types,
                    finished,
                    interpreter,
                    commit_interpreter,
                    durable_finalizer,
                }),
            };
            return Err(ErasedDurableHandoffFailure::new(
                attempt,
                Box::new(MissingDurablePublicationFinalizer),
            ));
        };
        match finalizer.stage_and_start(finished, input, runtime) {
            Ok(publication) => Ok(publication),
            Err(failure) => {
                let (finished, source) = failure.into_parts();
                let attempt = ErasedFinishedAttempt {
                    operations: Box::new(TypedFinishedOperations {
                        channel_types,
                        finished,
                        interpreter,
                        commit_interpreter,
                        durable_finalizer,
                    }),
                };
                Err(ErasedDurableHandoffFailure::new(attempt, source))
            }
        }
    }

    async fn after_publish(self: Box<Self>) -> ErasedPublishedAttempt {
        let Self {
            channel_types,
            finished,
            interpreter,
            commit_interpreter,
            durable_finalizer: _,
        } = *self;
        ErasedPublishedAttempt {
            operations: Box::new(TypedPublishedOperations {
                channel_types,
                published: finished.after_publish().await,
                interpreter,
                commit_interpreter,
            }),
        }
    }

    async fn abort(self: Box<Self>, reason: BindingAbortReason) -> StreamingAbortReport {
        self.finished.abort(reason).await
    }
}

/// Root-channel-erased publication-pending typestate.
#[must_use = "a finished erased attempt must be published or explicitly aborted"]
pub(crate) struct ErasedFinishedAttempt<
    Version = (),
    StoreError = Infallible,
    PublicationInput = (),
> where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    operations: Box<dyn ErasedFinishedOperations<Version, StoreError, PublicationInput>>,
}

impl<Version, StoreError, PublicationInput> fmt::Debug
    for ErasedFinishedAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedFinishedAttempt")
            .field("identity", self.identity())
            .field("channel_types", self.channel_types())
            .finish_non_exhaustive()
    }
}

impl<Version, StoreError, PublicationInput>
    ErasedFinishedAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    pub(crate) fn channel_types(&self) -> &ChannelTypeInfo {
        self.operations.channel_types()
    }

    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        self.operations.identity()
    }

    pub(crate) fn raw_output(&self) -> &str {
        self.operations.raw_output()
    }

    pub(crate) fn provider_results(&self) -> &[ProviderToolResult] {
        self.operations.provider_results()
    }

    pub(crate) fn begin_durable_publication(
        self,
        input: PublicationInput,
        runtime: &tokio::runtime::Handle,
    ) -> Result<
        ManagedDurablePublication<Version, StoreError>,
        ErasedDurableHandoffFailure<Version, StoreError, PublicationInput>,
    > {
        self.operations.begin_durable_publication(input, runtime)
    }

    pub(crate) async fn publish_with<P>(
        self,
        publisher: &mut P,
    ) -> Result<
        ErasedPublishedAttempt,
        ErasedPublishFailure<P::Error, Version, StoreError, PublicationInput>,
    >
    where
        P: TurnPublisher,
    {
        let publication =
            TurnPublication::combined(self.identity(), self.raw_output(), self.provider_results());
        if let Err(source) = publisher.publish(publication).await {
            return Err(ErasedPublishFailure {
                attempt: self,
                source,
            });
        }
        Ok(self.operations.after_publish().await)
    }

    pub(crate) async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.operations.abort(reason).await
    }
}

/// Publication failure retaining the exact finished object-safe state.
#[must_use = "a failed erased publication must explicitly abort its retained attempt"]
pub(crate) struct ErasedPublishFailure<
    E,
    Version = (),
    StoreError = Infallible,
    PublicationInput = (),
> where
    E: Error + Send + Sync + 'static,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    attempt: ErasedFinishedAttempt<Version, StoreError, PublicationInput>,
    source: E,
}

impl<E, Version, StoreError, PublicationInput>
    ErasedPublishFailure<E, Version, StoreError, PublicationInput>
where
    E: Error + Send + Sync + 'static,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub(crate) fn provider_results(&self) -> &[ProviderToolResult] {
        self.attempt.provider_results()
    }

    pub(crate) fn source_error(&self) -> &E {
        &self.source
    }

    pub(crate) async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.attempt.abort(reason).await
    }
}

impl<E, Version, StoreError, PublicationInput> fmt::Debug
    for ErasedPublishFailure<E, Version, StoreError, PublicationInput>
where
    E: Error + Send + Sync + 'static,
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedPublishFailure")
            .field("identity", self.identity())
            .field("source", &self.source)
            .finish()
    }
}

trait ErasedPublishedOperations: fmt::Debug + Send {
    fn channel_types(&self) -> &ChannelTypeInfo;
    fn identity(&self) -> &ProviderAttemptIdentity;
    fn raw_output(&self) -> &str;
    fn provider_results(&self) -> &[ProviderToolResult];
    fn pending_commit_count(&self) -> usize;
    fn deliver(self: Box<Self>) -> ErasedCommitDeliveryFuture;
    fn discard(self: Box<Self>) -> PublishedDiscardReport;
}

type ErasedCommitDeliveryFuture = Pin<
    Box<
        dyn Future<Output = Result<DeliveredCommitReport, ErasedCommitDeliveryFailure>>
            + Send
            + 'static,
    >,
>;

struct TypedPublishedOperations<C, I>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
{
    channel_types: ChannelTypeInfo,
    published: PublishedStreamingAttempt<C>,
    interpreter: I,
    commit_interpreter: Option<Box<dyn ErasedCommitInterpreter<C>>>,
}

impl<C, I> fmt::Debug for TypedPublishedOperations<C, I>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypedPublishedOperations")
            .field("identity", self.published.receipt().identity())
            .field("pending_commits", &self.published.pending_commits().len())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<C, I> ErasedPublishedOperations for TypedPublishedOperations<C, I>
where
    C: TurnChannels,
    I: AttemptUpdateInterpreter<C>,
{
    fn channel_types(&self) -> &ChannelTypeInfo {
        &self.channel_types
    }

    fn identity(&self) -> &ProviderAttemptIdentity {
        self.published.receipt().identity()
    }

    fn raw_output(&self) -> &str {
        self.published.raw_output()
    }

    fn provider_results(&self) -> &[ProviderToolResult] {
        self.published.provider_results()
    }

    fn pending_commit_count(&self) -> usize {
        self.published.pending_commits().len()
    }

    fn deliver(self: Box<Self>) -> ErasedCommitDeliveryFuture {
        Box::pin(async move {
            let Self {
                channel_types,
                mut published,
                interpreter,
                commit_interpreter,
            } = *self;
            let identity = published.receipt().identity().clone();
            let delivered_commits = published.pending_commits().len();
            if delivered_commits == 0 {
                drop(interpreter);
                return Ok(DeliveredCommitReport {
                    identity,
                    delivered_commits,
                });
            }
            let Some(mut commit_interpreter) = commit_interpreter else {
                return Err(ErasedCommitDeliveryFailure::new(
                    ErasedPublishedAttempt {
                        operations: Box::new(Self {
                            channel_types,
                            published,
                            interpreter,
                            commit_interpreter: None,
                        }),
                    },
                    CommitDeliveryNotConfigured,
                ));
            };
            if let Err(source) = commit_interpreter
                .deliver(published.receipt(), published.pending_commits())
                .await
            {
                return Err(ErasedCommitDeliveryFailure::boxed(
                    ErasedPublishedAttempt {
                        operations: Box::new(Self {
                            channel_types,
                            published,
                            interpreter,
                            commit_interpreter: Some(commit_interpreter),
                        }),
                    },
                    source,
                ));
            }

            let discarded = published.take_pending_commits().len();
            debug_assert_eq!(discarded, delivered_commits);
            drop(interpreter);
            Ok(DeliveredCommitReport {
                identity,
                delivered_commits,
            })
        })
    }

    fn discard(self: Box<Self>) -> PublishedDiscardReport {
        let Self {
            mut published,
            interpreter,
            ..
        } = *self;
        let identity = published.receipt().identity().clone();
        let discarded_commits = published.take_pending_commits().len();
        drop(interpreter);
        PublishedDiscardReport {
            identity,
            discarded_commits,
        }
    }
}

/// Published state retaining typed Commit values behind its monomorphized shim.
#[must_use = "published commits must remain retained or be deliberately discarded"]
pub(crate) struct ErasedPublishedAttempt {
    operations: Box<dyn ErasedPublishedOperations>,
}

impl fmt::Debug for ErasedPublishedAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedPublishedAttempt")
            .field("identity", self.identity())
            .field("channel_types", self.channel_types())
            .field("pending_commits", &self.pending_commit_count())
            .finish_non_exhaustive()
    }
}

impl ErasedPublishedAttempt {
    pub(crate) fn channel_types(&self) -> &ChannelTypeInfo {
        self.operations.channel_types()
    }

    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        self.operations.identity()
    }

    pub(crate) fn raw_output(&self) -> &str {
        self.operations.raw_output()
    }

    pub(crate) fn provider_results(&self) -> &[ProviderToolResult] {
        self.operations.provider_results()
    }

    pub(crate) fn pending_commit_count(&self) -> usize {
        self.operations.pending_commit_count()
    }

    /// Deliver the still-retained Commit batch after session publication.
    ///
    /// A delivery failure does not roll back publication and does not abort the
    /// attempt. The returned failure owns this exact published state so the
    /// caller can retry delivery without replaying model/provider work.
    pub(crate) async fn deliver_commits(
        self,
    ) -> Result<DeliveredCommitReport, ErasedCommitDeliveryFailure> {
        self.operations.deliver().await
    }

    pub(crate) fn discard_pending_commits(self) -> PublishedDiscardReport {
        self.operations.discard()
    }
}

/// No typed Commit interpreter was supplied when the erased attempt started.
#[derive(Debug, thiserror::Error)]
#[error("published Commit values have no configured delivery interpreter")]
struct CommitDeliveryNotConfigured;

/// A post-publication delivery failure retaining the exact published state.
#[must_use = "a failed Commit delivery must remain owned or be explicitly recovered for retry"]
pub(crate) struct ErasedCommitDeliveryFailure {
    attempt: ErasedPublishedAttempt,
    source: BoxCommitDeliveryError,
}

impl ErasedCommitDeliveryFailure {
    fn new(attempt: ErasedPublishedAttempt, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            attempt,
            source: Box::new(source),
        }
    }

    fn boxed(attempt: ErasedPublishedAttempt, source: BoxCommitDeliveryError) -> Self {
        Self { attempt, source }
    }

    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub(crate) fn pending_commit_count(&self) -> usize {
        self.attempt.pending_commit_count()
    }

    pub(crate) fn source_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.as_ref()
    }

    /// Recover the exact post-publication state to retry its original
    /// interpreter or deliberately discard the retained batch.
    pub(crate) fn into_published(self) -> ErasedPublishedAttempt {
        self.attempt
    }

    pub(crate) async fn retry(self) -> Result<DeliveredCommitReport, ErasedCommitDeliveryFailure> {
        self.attempt.deliver_commits().await
    }
}

impl fmt::Debug for ErasedCommitDeliveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedCommitDeliveryFailure")
            .field("identity", self.identity())
            .field("pending_commits", &self.pending_commit_count())
            .field("source", &self.source)
            .finish()
    }
}

/// Explicit receipt that a published Commit batch was delivered successfully.
#[derive(Debug)]
pub(crate) struct DeliveredCommitReport {
    identity: ProviderAttemptIdentity,
    delivered_commits: usize,
}

impl DeliveredCommitReport {
    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub(crate) fn delivered_commits(&self) -> usize {
        self.delivered_commits
    }
}

/// Explicit acknowledgement that a host chose not to interpret published Commit values.
#[derive(Debug)]
pub(crate) struct PublishedDiscardReport {
    identity: ProviderAttemptIdentity,
    discarded_commits: usize,
}

impl PublishedDiscardReport {
    pub(crate) fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub(crate) fn discarded_commits(&self) -> usize {
        self.discarded_commits
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use serde_json::json;

    use crate::pom::{XmlName, XmlNode};

    use super::*;
    use crate::component::{
        mount_system_epoch, provider_tool_with_context, system_view, user_view, BindingAbortAck,
        LiveAbortContext, LiveEffectAbortAck, LiveEffectContext, ProviderDispatchContext,
        ProviderDispatchFailure, ProviderDispatchUpdate, ProviderDispatcher,
        ProviderDispatcherAbortAck, ProviderDispatcherAbortContext, ProviderDispatcherCx,
        ProviderToolResponse, ProviderToolSpec, StreamingXml, SystemMountContext, SystemView,
    };

    #[derive(Debug, PartialEq, Eq)]
    struct XmlOutput(&'static str);

    #[derive(Debug, PartialEq, Eq)]
    struct XmlLive(&'static str);

    #[derive(Debug, PartialEq, Eq)]
    struct XmlCommit(&'static str);

    #[derive(Debug, PartialEq, Eq)]
    struct XmlDiagnostic(&'static str);

    struct XmlChannels;

    impl TurnChannels for XmlChannels {
        type Output = XmlOutput;
        type Live = XmlLive;
        type Commit = XmlCommit;
        type Diagnostic = XmlDiagnostic;
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ToolOutput(u32);

    #[derive(Debug, PartialEq, Eq)]
    struct ToolLive(u32);

    #[derive(Debug, PartialEq, Eq)]
    struct ToolCommit(u32);

    #[derive(Debug, PartialEq, Eq)]
    struct ToolDiagnostic(u32);

    struct ToolChannels;

    impl TurnChannels for ToolChannels {
        type Output = ToolOutput;
        type Live = ToolLive;
        type Commit = ToolCommit;
        type Diagnostic = ToolDiagnostic;
    }

    #[derive(Default)]
    struct XmlInterpretation {
        outputs: Vec<XmlOutput>,
        diagnostics: Vec<XmlDiagnostic>,
    }

    struct XmlInterpreter {
        state: Arc<Mutex<XmlInterpretation>>,
    }

    impl AttemptUpdateInterpreter<XmlChannels> for XmlInterpreter {
        type Error = UnexpectedLifecycleLane;

        fn interpret(
            &mut self,
            _identity: &ProviderAttemptIdentity,
            update: StreamUpdate<TurnEmission<XmlChannels>, XmlDiagnostic>,
        ) -> Result<(), Self::Error> {
            let (emissions, diagnostics) = update.into_parts();
            let mut state = self.state.lock().unwrap();
            for emission in emissions {
                match emission {
                    TurnEmission::Output(output) => state.outputs.push(output),
                    TurnEmission::Live(_) => return Err(UnexpectedLifecycleLane("live")),
                    TurnEmission::Commit(_) => return Err(UnexpectedLifecycleLane("commit")),
                }
            }
            state.diagnostics.extend(diagnostics);
            Ok(())
        }
    }

    #[derive(Default)]
    struct ToolInterpretation {
        outputs: Vec<ToolOutput>,
        diagnostics: Vec<ToolDiagnostic>,
    }

    struct ToolInterpreter {
        state: Arc<Mutex<ToolInterpretation>>,
    }

    impl AttemptUpdateInterpreter<ToolChannels> for ToolInterpreter {
        type Error = UnexpectedLifecycleLane;

        fn interpret(
            &mut self,
            _identity: &ProviderAttemptIdentity,
            update: StreamUpdate<TurnEmission<ToolChannels>, ToolDiagnostic>,
        ) -> Result<(), Self::Error> {
            let (emissions, diagnostics) = update.into_parts();
            let mut state = self.state.lock().unwrap();
            for emission in emissions {
                match emission {
                    TurnEmission::Output(output) => state.outputs.push(output),
                    TurnEmission::Live(_) => return Err(UnexpectedLifecycleLane("live")),
                    TurnEmission::Commit(_) => return Err(UnexpectedLifecycleLane("commit")),
                }
            }
            state.diagnostics.extend(diagnostics);
            Ok(())
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("unexpected {0} value escaped lifecycle interpretation")]
    struct UnexpectedLifecycleLane(&'static str);

    #[derive(Debug, thiserror::Error)]
    #[error("typed update receiver rejected delivery")]
    struct InterpretationRejected;

    struct RejectingToolInterpreter;

    impl AttemptUpdateInterpreter<ToolChannels> for RejectingToolInterpreter {
        type Error = InterpretationRejected;

        fn interpret(
            &mut self,
            _identity: &ProviderAttemptIdentity,
            _update: StreamUpdate<TurnEmission<ToolChannels>, ToolDiagnostic>,
        ) -> Result<(), Self::Error> {
            Err(InterpretationRejected)
        }
    }

    struct LiveState<L> {
        effects: Vec<L>,
        aborts: usize,
    }

    impl<L> Default for LiveState<L> {
        fn default() -> Self {
            Self {
                effects: Vec::new(),
                aborts: 0,
            }
        }
    }

    struct RecordingLive<L> {
        state: Arc<Mutex<LiveState<L>>>,
    }

    #[async_trait::async_trait]
    impl<L> LiveEffectRuntime<L> for RecordingLive<L>
    where
        L: Send + 'static,
    {
        type Error = Infallible;

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            effect: L,
        ) -> Result<(), Self::Error> {
            self.state.lock().unwrap().effects.push(effect);
            Ok(())
        }

        async fn abort(
            &mut self,
            context: &LiveAbortContext,
        ) -> Result<LiveEffectAbortAck, Self::Error> {
            self.state.lock().unwrap().aborts += 1;
            Ok(if context.applied_effects() == 0 {
                LiveEffectAbortAck::NoEffectsApplied
            } else {
                LiveEffectAbortAck::CompensationCompleted
            })
        }
    }

    fn recording_live<L>() -> (RecordingLive<L>, Arc<Mutex<LiveState<L>>>) {
        let state = Arc::new(Mutex::new(LiveState::default()));
        (
            RecordingLive {
                state: Arc::clone(&state),
            },
            state,
        )
    }

    struct XmlMountProps {
        aborts: Arc<AtomicUsize>,
    }

    fn contract(name: &str) -> XmlNode {
        XmlNode::new(XmlName::try_from(name).unwrap())
    }

    fn xml_system(cx: SystemMountContext<'_, XmlMountProps>) -> SystemView<XmlChannels> {
        let aborts = Arc::clone(&cx.props().aborts);
        system_view(
            StreamingXml::<TurnEmission<XmlChannels>, XmlDiagnostic>::new(contract("selection"))
                .state_with(|| ())
                .on_complete(|_, _| {
                    StreamUpdate::from_emission(TurnEmission::Output(XmlOutput("selected")))
                        .with_emission(TurnEmission::Live(XmlLive("open-ui")))
                        .with_emission(TurnEmission::Commit(XmlCommit("save-selection")))
                        .with_diagnostic(XmlDiagnostic("selection-observed"))
                })
                .on_finish(|_| {
                    StreamUpdate::from_emission(TurnEmission::Output(XmlOutput("xml-finished")))
                        .with_diagnostic(XmlDiagnostic("finish-observed"))
                })
                .on_abort(move |_, _| {
                    aborts.fetch_add(1, Ordering::SeqCst);
                    BindingAbortAck::LocalCleanupCompleted
                })
                .into_component(),
        )
    }

    fn two_commit_xml_system(cx: SystemMountContext<'_, XmlMountProps>) -> SystemView<XmlChannels> {
        let aborts = Arc::clone(&cx.props().aborts);
        system_view(
            StreamingXml::<TurnEmission<XmlChannels>, XmlDiagnostic>::new(contract("selection"))
                .state_with(|| ())
                .on_complete(|_, _| {
                    StreamUpdate::from_emission(TurnEmission::Commit(XmlCommit("first")))
                        .with_emission(TurnEmission::Commit(XmlCommit("second")))
                })
                .on_abort(move |_, _| {
                    aborts.fetch_add(1, Ordering::SeqCst);
                    BindingAbortAck::LocalCleanupCompleted
                })
                .into_component(),
        )
    }

    #[derive(Debug, Default)]
    struct ToolTrace {
        initializations: usize,
        calls: usize,
        finishes: usize,
        aborts: usize,
    }

    struct ToolMountProps {
        trace: Arc<Mutex<ToolTrace>>,
    }

    struct ToolProps {
        marker: u32,
    }

    #[derive(Debug)]
    struct ToolDispatcher {
        marker: u32,
        trace: Arc<Mutex<ToolTrace>>,
    }

    #[async_trait::async_trait]
    impl ProviderDispatcher<ToolChannels> for ToolDispatcher {
        type Error = Infallible;

        async fn dispatch(
            &mut self,
            _context: &ProviderDispatchContext,
            _call: ProviderToolCall,
        ) -> Result<ProviderDispatchUpdate<ToolChannels>, Self::Error> {
            self.trace.lock().unwrap().calls += 1;
            Ok(ProviderDispatchUpdate::new(
                ProviderToolResponse::success(json!({ "marker": self.marker })),
                StreamUpdate::from_emission(TurnEmission::Output(ToolOutput(self.marker)))
                    .with_emission(TurnEmission::Live(ToolLive(self.marker + 1)))
                    .with_emission(TurnEmission::Commit(ToolCommit(self.marker + 2)))
                    .with_diagnostic(ToolDiagnostic(self.marker + 3)),
            ))
        }

        async fn finish(
            &mut self,
        ) -> Result<StreamUpdate<TurnEmission<ToolChannels>, ToolDiagnostic>, Self::Error> {
            self.trace.lock().unwrap().finishes += 1;
            Ok(
                StreamUpdate::from_emission(TurnEmission::Output(ToolOutput(self.marker + 10)))
                    .with_diagnostic(ToolDiagnostic(self.marker + 11)),
            )
        }

        async fn abort(
            &mut self,
            _context: &ProviderDispatcherAbortContext,
        ) -> Result<ProviderDispatcherAbortAck, Self::Error> {
            self.trace.lock().unwrap().aborts += 1;
            Ok(ProviderDispatcherAbortAck::CleanupCompleted)
        }
    }

    fn tool_spec() -> ProviderToolSpec {
        ProviderToolSpec::new(
            "lookup",
            "Look up one value",
            json!({ "type": "object", "properties": {} }),
        )
        .unwrap()
    }

    fn tool_system(
        cx: SystemMountContext<'_, ToolMountProps>,
    ) -> SystemView<ToolChannels, ToolProps> {
        let trace = Arc::clone(&cx.props().trace);
        system_view(provider_tool_with_context(
            "lookup",
            tool_spec(),
            move |cx: &ProviderDispatcherCx<'_, ToolProps, ToolChannels>| {
                trace.lock().unwrap().initializations += 1;
                Ok::<_, ProviderDispatchFailure>(ToolDispatcher {
                    marker: cx.props().marker,
                    trace: Arc::clone(&trace),
                })
            },
        ))
    }

    #[derive(Default)]
    struct RecordingPublisher {
        calls: usize,
        results: Vec<ProviderToolResult>,
    }

    #[async_trait::async_trait]
    impl TurnPublisher for RecordingPublisher {
        type Error = Infallible;

        async fn publish(&mut self, publication: TurnPublication<'_>) -> Result<(), Self::Error> {
            self.calls += 1;
            self.results
                .extend_from_slice(publication.provider_results());
            Ok(())
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("publisher rejected the turn")]
    struct PublishRejected;

    struct RejectingPublisher;

    #[async_trait::async_trait]
    impl TurnPublisher for RejectingPublisher {
        type Error = PublishRejected;

        async fn publish(&mut self, _publication: TurnPublication<'_>) -> Result<(), Self::Error> {
            Err(PublishRejected)
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("commit delivery temporarily rejected")]
    struct CommitDeliveryRejected;

    type XmlCommitDeliveries = Arc<Mutex<Vec<(ProviderAttemptIdentity, Vec<&'static str>)>>>;

    struct RetryingXmlCommitInterpreter {
        calls: Arc<AtomicUsize>,
        delivered: XmlCommitDeliveries,
    }

    #[async_trait::async_trait]
    impl AttemptCommitInterpreter<XmlChannels> for RetryingXmlCommitInterpreter {
        type Error = CommitDeliveryRejected;

        async fn deliver(
            &mut self,
            receipt: &PublishedTurnReceipt,
            commits: &[XmlCommit],
        ) -> Result<(), Self::Error> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(CommitDeliveryRejected);
            }
            self.delivered.lock().unwrap().push((
                receipt.identity().clone(),
                commits.iter().map(|commit| commit.0).collect(),
            ));
            Ok(())
        }
    }

    struct RecordingXmlCommitInterpreter {
        calls: Arc<AtomicUsize>,
        delivered: XmlCommitDeliveries,
    }

    #[async_trait::async_trait]
    impl AttemptCommitInterpreter<XmlChannels> for RecordingXmlCommitInterpreter {
        type Error = Infallible;

        async fn deliver(
            &mut self,
            receipt: &PublishedTurnReceipt,
            commits: &[XmlCommit],
        ) -> Result<(), Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.delivered.lock().unwrap().push((
                receipt.identity().clone(),
                commits.iter().map(|commit| commit.0).collect(),
            ));
            Ok(())
        }
    }

    struct PartiallyApplyingXmlCommitInterpreter {
        first_call: bool,
        applied: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl AttemptCommitInterpreter<XmlChannels> for PartiallyApplyingXmlCommitInterpreter {
        type Error = CommitDeliveryRejected;

        async fn deliver(
            &mut self,
            _receipt: &PublishedTurnReceipt,
            commits: &[XmlCommit],
        ) -> Result<(), Self::Error> {
            if self.first_call {
                self.first_call = false;
                self.applied.lock().unwrap().push(commits[0].0);
                return Err(CommitDeliveryRejected);
            }
            self.applied
                .lock()
                .unwrap()
                .extend(commits.iter().map(|commit| commit.0));
            Ok(())
        }
    }

    #[tokio::test]
    async fn two_root_contracts_share_non_generic_storage_and_keep_typed_delivery() {
        fn assert_send<T: Send>() {}
        assert_send::<ErasedActiveAttempt>();

        let xml_aborts = Arc::new(AtomicUsize::new(0));
        let xml_epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&xml_aborts),
            },
            xml_system,
        )
        .unwrap();
        let xml_record = Arc::new(Mutex::new(XmlInterpretation::default()));
        let (xml_live, xml_live_state) = recording_live();
        let xml_turn = xml_epoch.begin_turn("xml");
        let xml_prepared = xml_turn.prepare_user(&(), |_| user_view(())).unwrap();
        let xml_attempt = start_erased_streaming_attempt(
            &xml_prepared,
            xml_epoch.channel_type_info(),
            xml_live,
            XmlInterpreter {
                state: Arc::clone(&xml_record),
            },
        )
        .unwrap();

        let tool_trace = Arc::new(Mutex::new(ToolTrace::default()));
        let tool_epoch = mount_system_epoch(
            &ToolMountProps {
                trace: Arc::clone(&tool_trace),
            },
            tool_system,
        )
        .unwrap();
        let tool_props = ToolProps { marker: 7 };
        let tool_record = Arc::new(Mutex::new(ToolInterpretation::default()));
        let (tool_live, tool_live_state) = recording_live();
        let tool_turn = tool_epoch.begin_turn("tool");
        let tool_prepared = tool_turn
            .prepare_user(&tool_props, |_| user_view(()))
            .unwrap();
        let tool_attempt = start_erased_streaming_attempt(
            &tool_prepared,
            tool_epoch.channel_type_info(),
            tool_live,
            ToolInterpreter {
                state: Arc::clone(&tool_record),
            },
        )
        .unwrap();

        let mut active = vec![xml_attempt, tool_attempt];
        assert_ne!(
            active[0].channel_types().root_channels(),
            active[1].channel_types().root_channels()
        );
        assert_eq!(active[0].binding_count(), 1);
        assert_eq!(active[1].dispatcher_count(), 1);
        assert_eq!(active[1].provider_tool_count(), 1);

        let text = active[0]
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();
        assert!(matches!(text, AttemptDriverStep::TextAccepted));

        let tool = active[1]
            .drive(ProviderWireInput::Tool(
                ProviderToolCall::new("invoke-1", "lookup", json!({}))
                    .with_result_correlation_id("provider-1"),
            ))
            .await
            .unwrap();
        assert_eq!(tool.replayed(), Some(false));
        assert_eq!(
            tool.tool_result().unwrap().invocation_id(),
            Some("invoke-1")
        );
        assert_eq!(
            tool.tool_result().unwrap().result_correlation_id(),
            Some("provider-1")
        );

        let mut finished = Vec::new();
        for attempt in active {
            finished.push(attempt.finish().await.unwrap());
        }
        assert_eq!(finished[0].raw_output(), "<selection />");
        assert_eq!(finished[1].provider_results().len(), 1);

        let mut publisher = RecordingPublisher::default();
        let mut published = Vec::new();
        for attempt in finished {
            published.push(attempt.publish_with(&mut publisher).await.unwrap());
        }
        assert_eq!(publisher.calls, 2);
        assert_eq!(publisher.results.len(), 1);
        assert_eq!(published[0].pending_commit_count(), 1);
        assert_eq!(published[1].pending_commit_count(), 1);

        let xml_discard = published.remove(0).discard_pending_commits();
        let tool_discard = published.remove(0).discard_pending_commits();
        assert_eq!(xml_discard.discarded_commits(), 1);
        assert_eq!(tool_discard.discarded_commits(), 1);

        let xml_record = xml_record.lock().unwrap();
        assert_eq!(
            xml_record.outputs,
            [XmlOutput("selected"), XmlOutput("xml-finished")]
        );
        assert_eq!(
            xml_record.diagnostics,
            [
                XmlDiagnostic("selection-observed"),
                XmlDiagnostic("finish-observed")
            ]
        );
        assert_eq!(xml_live_state.lock().unwrap().effects, [XmlLive("open-ui")]);

        let tool_record = tool_record.lock().unwrap();
        assert_eq!(tool_record.outputs, [ToolOutput(7), ToolOutput(17)]);
        assert_eq!(
            tool_record.diagnostics,
            [ToolDiagnostic(10), ToolDiagnostic(18)]
        );
        assert_eq!(tool_live_state.lock().unwrap().effects, [ToolLive(8)]);
        assert_eq!(tool_trace.lock().unwrap().calls, 1);
        assert_eq!(xml_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn contract_mismatch_fails_before_dispatcher_initialization() {
        let trace = Arc::new(Mutex::new(ToolTrace::default()));
        let epoch = mount_system_epoch(
            &ToolMountProps {
                trace: Arc::clone(&trace),
            },
            tool_system,
        )
        .unwrap();
        let props = ToolProps { marker: 3 };
        let turn = epoch.begin_turn("mismatch");
        let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();
        let wrong_contract = ChannelTypeInfo::of::<XmlChannels, ()>();
        let (live, _) = recording_live();
        let interpreter = ToolInterpreter {
            state: Arc::new(Mutex::new(ToolInterpretation::default())),
        };

        let error = start_erased_streaming_attempt(&prepared, &wrong_contract, live, interpreter)
            .unwrap_err();
        assert!(matches!(error, ErasedAttemptStartError::Contract(_)));
        let trace = trace.lock().unwrap();
        assert_eq!(trace.initializations, 0);
        assert_eq!(trace.calls, 0);
    }

    #[tokio::test]
    async fn tool_result_is_withheld_when_typed_update_delivery_fails() {
        let trace = Arc::new(Mutex::new(ToolTrace::default()));
        let epoch = mount_system_epoch(
            &ToolMountProps {
                trace: Arc::clone(&trace),
            },
            tool_system,
        )
        .unwrap();
        let props = ToolProps { marker: 13 };
        let turn = epoch.begin_turn("delivery-failure");
        let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let mut attempt = start_erased_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            live,
            RejectingToolInterpreter,
        )
        .unwrap();

        let error = attempt
            .drive(ProviderWireInput::Tool(ProviderToolCall::new(
                "invoke-rejected",
                "lookup",
                json!({}),
            )))
            .await
            .unwrap_err();
        assert!(matches!(error, AttemptDriverFault::Interpretation(_)));
        assert_eq!(attempt.provider_results().len(), 1);
        assert!(matches!(
            attempt
                .drive(ProviderWireInput::Tool(ProviderToolCall::new(
                    "invoke-after-terminal",
                    "lookup",
                    json!({}),
                )))
                .await,
            Err(AttemptDriverFault::Terminal)
        ));

        let report = attempt.abort(BindingAbortReason::ProviderFailure).await;
        assert_eq!(report.dispatchers().len(), 1);
        let trace = trace.lock().unwrap();
        assert_eq!(trace.calls, 1);
        assert_eq!(trace.aborts, 1);
    }

    #[derive(Debug, thiserror::Error)]
    #[error("live application failed")]
    struct LiveRejected;

    struct RejectingLive {
        aborts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LiveEffectRuntime<XmlLive> for RejectingLive {
        type Error = LiveRejected;

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            _effect: XmlLive,
        ) -> Result<(), Self::Error> {
            Err(LiveRejected)
        }

        async fn abort(
            &mut self,
            _context: &LiveAbortContext,
        ) -> Result<LiveEffectAbortAck, Self::Error> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(LiveEffectAbortAck::NoEffectsApplied)
        }
    }

    #[tokio::test]
    async fn live_failure_is_terminal_but_active_state_remains_abortable() {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let live_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&local_aborts),
            },
            xml_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("live-failure");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let mut attempt = start_erased_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            RejectingLive {
                aborts: Arc::clone(&live_aborts),
            },
            XmlInterpreter {
                state: Arc::new(Mutex::new(XmlInterpretation::default())),
            },
        )
        .unwrap();

        let error = attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AttemptDriverFault::Streaming(StreamingAttemptError::Live(_))
        ));
        assert!(matches!(
            attempt
                .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                    "ignored".to_owned()
                )))
                .await,
            Err(AttemptDriverFault::Terminal)
        ));

        let report = attempt.abort(BindingAbortReason::ProviderFailure).await;
        assert_eq!(report.bindings().len(), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
        assert_eq!(live_aborts.load(Ordering::SeqCst), 1);
    }

    #[derive(Debug, thiserror::Error)]
    #[error("finish reducer failed")]
    struct FinishRejected;

    fn finish_failure_system(cx: SystemMountContext<'_, XmlMountProps>) -> SystemView<XmlChannels> {
        let aborts = Arc::clone(&cx.props().aborts);
        system_view(
            StreamingXml::<TurnEmission<XmlChannels>, XmlDiagnostic>::new(contract("finish"))
                .state_with(|| ())
                .try_on_finish(|_| {
                    Err::<StreamUpdate<TurnEmission<XmlChannels>, XmlDiagnostic>, _>(FinishRejected)
                })
                .on_abort(move |_, _| {
                    aborts.fetch_add(1, Ordering::SeqCst);
                    BindingAbortAck::LocalCleanupCompleted
                })
                .into_component(),
        )
    }

    #[tokio::test]
    async fn finish_failure_owns_the_abortable_attempt() {
        let aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&aborts),
            },
            finish_failure_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("finish-failure");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let attempt = start_erased_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            live,
            XmlInterpreter {
                state: Arc::new(Mutex::new(XmlInterpretation::default())),
            },
        )
        .unwrap();
        let identity = attempt.identity().clone();

        let failure = attempt.finish().await.unwrap_err();
        assert_eq!(failure.identity(), &identity);
        assert!(matches!(
            failure.error(),
            AttemptDriverFault::Streaming(StreamingAttemptError::Binding(_))
        ));
        let report = failure.abort(BindingAbortReason::ProviderFailure).await;
        assert_eq!(report.identity(), &identity);
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn publication_failure_retains_results_and_finished_abort_state() {
        let trace = Arc::new(Mutex::new(ToolTrace::default()));
        let epoch = mount_system_epoch(
            &ToolMountProps {
                trace: Arc::clone(&trace),
            },
            tool_system,
        )
        .unwrap();
        let props = ToolProps { marker: 5 };
        let turn = epoch.begin_turn("publish-failure");
        let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let mut attempt = start_erased_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            live,
            ToolInterpreter {
                state: Arc::new(Mutex::new(ToolInterpretation::default())),
            },
        )
        .unwrap();
        attempt
            .drive(ProviderWireInput::Tool(
                ProviderToolCall::new("invoke-publish", "lookup", json!({}))
                    .with_result_correlation_id("provider-publish"),
            ))
            .await
            .unwrap();
        let finished = attempt.finish().await.unwrap();
        let identity = finished.identity().clone();

        let failure = finished
            .publish_with(&mut RejectingPublisher)
            .await
            .unwrap_err();
        assert_eq!(failure.identity(), &identity);
        assert_eq!(failure.provider_results().len(), 1);
        assert_eq!(
            failure.provider_results()[0].result_correlation_id(),
            Some("provider-publish")
        );
        let report = failure.abort(BindingAbortReason::PublishFailure).await;
        assert_eq!(report.identity(), &identity);
        assert_eq!(trace.lock().unwrap().aborts, 1);
    }

    #[tokio::test]
    async fn commit_delivery_failure_retains_published_state_for_retry() {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&local_aborts),
            },
            xml_system,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let turn = epoch.begin_turn("commit-delivery");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let (live, live_state) = recording_live();
        let interpretation = Arc::new(Mutex::new(XmlInterpretation::default()));
        let mut attempt = start_erased_streaming_attempt_with_commit_interpreter(
            &prepared,
            epoch.channel_type_info(),
            live,
            XmlInterpreter {
                state: Arc::clone(&interpretation),
            },
            RetryingXmlCommitInterpreter {
                calls: Arc::clone(&calls),
                delivered: Arc::clone(&delivered),
            },
        )
        .unwrap();

        attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();
        let finished = attempt.finish().await.unwrap();
        let identity = finished.identity().clone();
        let mut publisher = RecordingPublisher::default();
        let published = finished.publish_with(&mut publisher).await.unwrap();
        assert_eq!(publisher.calls, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let failure = published.deliver_commits().await.unwrap_err();
        assert_eq!(failure.identity(), &identity);
        assert_eq!(failure.pending_commit_count(), 1);
        assert_eq!(
            failure.source_error().to_string(),
            "commit delivery temporarily rejected"
        );
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);

        let report = failure.retry().await.unwrap();
        assert_eq!(report.identity(), &identity);
        assert_eq!(report.delivered_commits(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(publisher.calls, 1);
        assert_eq!(live_state.lock().unwrap().effects.len(), 1);
        assert_eq!(interpretation.lock().unwrap().outputs.len(), 2);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
        assert_eq!(
            delivered.lock().unwrap().as_slice(),
            &[(identity, vec!["save-selection"])]
        );
    }

    #[tokio::test]
    async fn partial_delivery_failure_replays_the_complete_batch() {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&local_aborts),
            },
            two_commit_xml_system,
        )
        .unwrap();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let turn = epoch.begin_turn("partial-commit-delivery");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let mut attempt = start_erased_streaming_attempt_with_commit_interpreter(
            &prepared,
            epoch.channel_type_info(),
            live,
            XmlInterpreter {
                state: Arc::new(Mutex::new(XmlInterpretation::default())),
            },
            PartiallyApplyingXmlCommitInterpreter {
                first_call: true,
                applied: Arc::clone(&applied),
            },
        )
        .unwrap();

        attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();
        let finished = attempt.finish().await.unwrap();
        let mut publisher = RecordingPublisher::default();
        let published = finished.publish_with(&mut publisher).await.unwrap();

        let failure = published.deliver_commits().await.unwrap_err();
        assert_eq!(failure.pending_commit_count(), 2);
        assert_eq!(applied.lock().unwrap().as_slice(), &["first"]);

        let report = failure.retry().await.unwrap();
        assert_eq!(report.delivered_commits(), 2);
        assert_eq!(
            applied.lock().unwrap().as_slice(),
            &["first", "first", "second"]
        );
        assert_eq!(publisher.calls, 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn commit_interpreter_runs_only_after_successful_publication() {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&local_aborts),
            },
            xml_system,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let turn = epoch.begin_turn("commit-publication-gate");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let mut attempt = start_erased_streaming_attempt_with_commit_interpreter(
            &prepared,
            epoch.channel_type_info(),
            live,
            XmlInterpreter {
                state: Arc::new(Mutex::new(XmlInterpretation::default())),
            },
            RecordingXmlCommitInterpreter {
                calls: Arc::clone(&calls),
                delivered: Arc::clone(&delivered),
            },
        )
        .unwrap();

        attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let finished = attempt.finish().await.unwrap();
        let identity = finished.identity().clone();
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let mut publisher = RecordingPublisher::default();
        let published = finished.publish_with(&mut publisher).await.unwrap();
        assert_eq!(publisher.calls, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let report = published.deliver_commits().await.unwrap();
        assert_eq!(report.identity(), &identity);
        assert_eq!(report.delivered_commits(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            delivered.lock().unwrap().as_slice(),
            &[(identity, vec!["save-selection"])]
        );
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_commit_interpreter_retains_recoverable_published_state() {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&local_aborts),
            },
            xml_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("commit-not-configured");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let mut attempt = start_erased_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            live,
            XmlInterpreter {
                state: Arc::new(Mutex::new(XmlInterpretation::default())),
            },
        )
        .unwrap();

        attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();
        let finished = attempt.finish().await.unwrap();
        let identity = finished.identity().clone();
        let mut publisher = RecordingPublisher::default();
        let published = finished.publish_with(&mut publisher).await.unwrap();

        let failure = published.deliver_commits().await.unwrap_err();
        assert_eq!(failure.identity(), &identity);
        assert_eq!(failure.pending_commit_count(), 1);
        assert_eq!(
            failure.source_error().to_string(),
            "published Commit values have no configured delivery interpreter"
        );
        assert_eq!(publisher.calls, 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);

        let published = failure.into_published();
        assert_eq!(published.pending_commit_count(), 1);
        let report = published.discard_pending_commits();
        assert_eq!(report.identity(), &identity);
        assert_eq!(report.discarded_commits(), 1);
        assert_eq!(publisher.calls, 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_commit_batch_needs_no_interpreter() {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &XmlMountProps {
                aborts: Arc::clone(&local_aborts),
            },
            xml_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("empty-commit-delivery");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let (live, _) = recording_live();
        let attempt = start_erased_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            live,
            XmlInterpreter {
                state: Arc::new(Mutex::new(XmlInterpretation::default())),
            },
        )
        .unwrap();

        let finished = attempt.finish().await.unwrap();
        let identity = finished.identity().clone();
        let mut publisher = RecordingPublisher::default();
        let published = finished.publish_with(&mut publisher).await.unwrap();
        assert_eq!(published.pending_commit_count(), 0);

        let report = published.deliver_commits().await.unwrap();
        assert_eq!(report.identity(), &identity);
        assert_eq!(report.delivered_commits(), 0);
        assert_eq!(publisher.calls, 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
    }
}
