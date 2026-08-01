//! Cancellation-safe owner for one private erased streaming attempt.
//!
//! The actor, rather than a caller's future, owns the active/finished attempt.
//! Once a command reaches its mailbox it runs to a stable typestate even when
//! the caller stops awaiting its reply. When the last handle goes away, the
//! actor explicitly aborts any pre-publication state before it exits.
//!
//! This is intentionally crate-private. It is the cancellation primitive for a
//! future mounted AgentLoop, not another component authoring API. Published
//! commits need a distinct post-publication delivery owner and are therefore
//! outside this actor's state machine.

use std::{convert::Infallible, error::Error, fmt};

use tokio::sync::{mpsc, oneshot};

use super::{
    attempt_driver::{
        start_erased_streaming_attempt, start_erased_streaming_attempt_with_durable_finalizer,
        start_erased_streaming_attempt_with_durable_finalizer_and_identity, AttemptDriverFault,
        AttemptDriverStep, AttemptUpdateInterpreter, DurablePublicationFinalizer,
        ErasedActiveAttempt, ErasedAttemptStartError, ErasedDurableHandoffFault,
        ErasedFinishedAttempt, ProviderWireInput,
    },
    managed_publication::ManagedDurablePublication,
    BindingAbortReason, ChannelTypeInfo, FallibleProviderWirePort, LiveEffectRuntime,
    PreparedUserTurn, ProviderAttemptIdentity, ProviderToolAttemptError, ProviderWireAck,
    ProviderWireEvent, ProviderWireFault, StreamingAbortReport, StreamingAttemptError,
    TurnChannels,
};

/// Lifecycle phase currently owned by the managed attempt task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedAttemptPhase {
    Active,
    Finished,
    Transferred,
    Aborted,
}

impl fmt::Display for ManagedAttemptPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Active => "active",
            Self::Finished => "finished",
            Self::Transferred => "transferred",
            Self::Aborted => "aborted",
        })
    }
}

/// Start failure before a managed owner can take responsibility for an attempt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedAttemptStartError {
    #[error("a managed streaming attempt requires a running Tokio runtime")]
    NoRuntime,

    #[error(transparent)]
    Attempt(#[from] ErasedAttemptStartError),
}

/// A terminal driver error plus the aggregate abort report it triggered.
///
/// Local teardown always ran, but callers must inspect whether fallible host
/// compensation and dispatcher cleanup were acknowledged.
#[derive(Debug)]
pub(crate) struct ManagedAttemptFailure {
    source: Box<AttemptDriverFault>,
    abort_report: Box<StreamingAbortReport>,
}

impl ManagedAttemptFailure {
    pub(crate) fn source_fault(&self) -> &AttemptDriverFault {
        &self.source
    }

    pub(crate) fn abort_report(&self) -> &StreamingAbortReport {
        &self.abort_report
    }

    pub(crate) fn cleanup_acknowledged(&self) -> bool {
        self.abort_report.cleanup_acknowledged()
    }
}

impl fmt::Display for ManagedAttemptFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "managed attempt failed and was aborted: {}",
            self.source
        )
    }
}

impl std::error::Error for ManagedAttemptFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Failure returned by the managed owner rather than a bare typed attempt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedAttemptError {
    #[error(transparent)]
    Failed(#[from] ManagedAttemptFailure),

    #[error("managed attempt is {phase}, but this operation requires {required}")]
    WrongPhase {
        phase: ManagedAttemptPhase,
        required: &'static str,
    },

    #[error("managed attempt control task stopped unexpectedly")]
    ControlStopped,

    #[error(transparent)]
    DurableHandoff(#[from] ErasedDurableHandoffFault),
}

/// Result of an explicit abort request.
#[derive(Debug)]
pub(crate) enum ManagedAttemptAbort {
    Aborted(StreamingAbortReport),
    Transferred,
    AlreadyAborted,
}

impl ManagedAttemptAbort {
    pub(crate) fn report(&self) -> Option<&StreamingAbortReport> {
        match self {
            Self::Aborted(report) => Some(report),
            Self::Transferred | Self::AlreadyAborted => None,
        }
    }

    pub(crate) fn cleanup_acknowledged(&self) -> bool {
        match self {
            Self::Aborted(report) => report.cleanup_acknowledged(),
            Self::Transferred => true,
            Self::AlreadyAborted => false,
        }
    }
}

/// Failed handoff that still has a live handle to the old Finished actor.
pub(crate) enum BeginDurablePublicationError<
    Version = (),
    StoreError = Infallible,
    PublicationInput = (),
> where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    Retained {
        finished: ManagedFinishedAttempt<Version, StoreError, PublicationInput>,
        source: ManagedAttemptError,
    },
    ControlStopped,
}

impl<Version, StoreError, PublicationInput> fmt::Debug
    for BeginDurablePublicationError<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retained { source, .. } => formatter
                .debug_struct("BeginDurablePublicationError::Retained")
                .field("source", source)
                .finish_non_exhaustive(),
            Self::ControlStopped => {
                formatter.write_str("BeginDurablePublicationError::ControlStopped")
            }
        }
    }
}

impl<Version, StoreError, PublicationInput>
    BeginDurablePublicationError<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    pub(crate) fn source_error(&self) -> Option<&ManagedAttemptError> {
        match self {
            Self::Retained { source, .. } => Some(source),
            Self::ControlStopped => None,
        }
    }

    pub(crate) fn into_finished(
        self,
    ) -> Option<ManagedFinishedAttempt<Version, StoreError, PublicationInput>> {
        match self {
            Self::Retained { finished, .. } => Some(finished),
            Self::ControlStopped => None,
        }
    }
}

impl<Version, StoreError, PublicationInput> fmt::Display
    for BeginDurablePublicationError<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retained { source, .. } => {
                write!(formatter, "durable publication handoff failed: {source}")
            }
            Self::ControlStopped => formatter
                .write_str("managed attempt control stopped during durable publication handoff"),
        }
    }
}

impl<Version, StoreError, PublicationInput> std::error::Error
    for BeginDurablePublicationError<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source_error()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

enum AttemptCommand<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    Drive {
        input: ProviderWireInput,
        reply: oneshot::Sender<Result<AttemptDriverStep, ManagedAttemptError>>,
    },
    Finish {
        reply: oneshot::Sender<Result<FinishedAttemptRecord, ManagedAttemptError>>,
    },
    BeginDurablePublication {
        input: PublicationInput,
        reply: oneshot::Sender<
            Result<ManagedDurablePublication<Version, StoreError>, ManagedAttemptError>,
        >,
    },
    Abort {
        reason: BindingAbortReason,
        reply: oneshot::Sender<ManagedAttemptAbort>,
    },
    Phase {
        reply: oneshot::Sender<ManagedAttemptPhase>,
    },
}

struct AttemptControl<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    commands: mpsc::UnboundedSender<AttemptCommand<Version, StoreError, PublicationInput>>,
}

impl<Version, StoreError, PublicationInput> AttemptControl<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    async fn request<T>(
        &self,
        command: impl FnOnce(
            oneshot::Sender<T>,
        ) -> AttemptCommand<Version, StoreError, PublicationInput>,
    ) -> Result<T, ManagedAttemptError> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| ManagedAttemptError::ControlStopped)?;
        receiver
            .await
            .map_err(|_| ManagedAttemptError::ControlStopped)
    }

    async fn phase(&self) -> Result<ManagedAttemptPhase, ManagedAttemptError> {
        self.request(|reply| AttemptCommand::Phase { reply }).await
    }
}

/// A managed active attempt. Dropping its final handle asks the actor to abort.
#[must_use = "a managed active attempt must be finished or explicitly aborted"]
pub(crate) struct ManagedActiveAttempt<Version = (), StoreError = Infallible, PublicationInput = ()>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    control: Option<AttemptControl<Version, StoreError, PublicationInput>>,
}

impl<Version, StoreError, PublicationInput>
    ManagedActiveAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn control(&self) -> &AttemptControl<Version, StoreError, PublicationInput> {
        self.control
            .as_ref()
            .expect("managed active attempt control is present until ownership transfers")
    }

    pub(crate) async fn phase(&self) -> Result<ManagedAttemptPhase, ManagedAttemptError> {
        self.control().phase().await
    }

    pub(crate) async fn drive(
        &mut self,
        input: ProviderWireInput,
    ) -> Result<AttemptDriverStep, ManagedAttemptError> {
        self.control()
            .request(|reply| AttemptCommand::Drive { input, reply })
            .await?
    }

    /// Transition to publication-pending state without transferring the state
    /// into this caller's future.
    pub(crate) async fn finish(
        mut self,
    ) -> Result<ManagedFinishedAttempt<Version, StoreError, PublicationInput>, ManagedAttemptError>
    {
        let record = self
            .control()
            .request(|reply| AttemptCommand::Finish { reply })
            .await??;
        Ok(ManagedFinishedAttempt {
            control: self
                .control
                .take()
                .expect("successful finish transfers the managed owner"),
            record,
        })
    }

    pub(crate) async fn abort(
        mut self,
        reason: BindingAbortReason,
    ) -> Result<ManagedAttemptAbort, ManagedAttemptError> {
        let result = self
            .control()
            .request(|reply| AttemptCommand::Abort { reason, reply })
            .await;
        self.control.take();
        result
    }
}

impl<Version, StoreError, PublicationInput> fmt::Debug
    for ManagedActiveAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedActiveAttempt")
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<Version, StoreError, PublicationInput> FallibleProviderWirePort
    for ManagedActiveAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    async fn submit(
        &mut self,
        event: ProviderWireEvent,
    ) -> Result<ProviderWireAck, ProviderWireFault> {
        let input = match event {
            ProviderWireEvent::Text(event) => ProviderWireInput::Text(event),
            ProviderWireEvent::Tool(call) => ProviderWireInput::Tool(call),
        };
        match self.drive(input).await.map_err(ProviderWireFault::new)? {
            AttemptDriverStep::TextAccepted => Ok(ProviderWireAck::TextAccepted),
            AttemptDriverStep::ToolResult { result, replayed } => {
                Ok(ProviderWireAck::ToolResult { result, replayed })
            }
        }
    }
}

/// A managed finished attempt. It remains abortable until a later publication
/// owner atomically publishes the session and takes responsibility for Commit.
#[must_use = "a managed finished attempt must be published by its owner or explicitly aborted"]
pub(crate) struct ManagedFinishedAttempt<
    Version = (),
    StoreError = Infallible,
    PublicationInput = (),
> where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    control: AttemptControl<Version, StoreError, PublicationInput>,
    record: FinishedAttemptRecord,
}

/// Immutable provider data captured at the Active -> Finished transition.
///
/// The managed actor keeps the actual Finished attempt. This cloneable snapshot
/// lets the mounted owner build one pure turn record without reaching through a
/// shared callback side channel.
#[derive(Debug, Clone)]
pub(crate) struct FinishedAttemptRecord {
    raw_output: String,
    provider_results: Vec<super::ProviderToolResult>,
}

impl FinishedAttemptRecord {
    pub(crate) fn raw_output(&self) -> &str {
        &self.raw_output
    }

    pub(crate) fn provider_results(&self) -> &[super::ProviderToolResult] {
        &self.provider_results
    }
}

impl<Version, StoreError, PublicationInput>
    ManagedFinishedAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    pub(crate) async fn phase(&self) -> Result<ManagedAttemptPhase, ManagedAttemptError> {
        self.control.phase().await
    }

    pub(crate) fn record(&self) -> &FinishedAttemptRecord {
        &self.record
    }

    pub(crate) async fn abort(
        self,
        reason: BindingAbortReason,
    ) -> Result<ManagedAttemptAbort, ManagedAttemptError> {
        self.control
            .request(|reply| AttemptCommand::Abort { reason, reply })
            .await
    }

    pub(crate) async fn begin_durable_publication(
        self,
        input: PublicationInput,
    ) -> Result<
        ManagedDurablePublication<Version, StoreError>,
        BeginDurablePublicationError<Version, StoreError, PublicationInput>,
    > {
        match self
            .control
            .request(|reply| AttemptCommand::BeginDurablePublication { input, reply })
            .await
        {
            Ok(Ok(publication)) => Ok(publication),
            Ok(Err(source)) => Err(BeginDurablePublicationError::Retained {
                finished: self,
                source,
            }),
            Err(ManagedAttemptError::ControlStopped) => {
                Err(BeginDurablePublicationError::ControlStopped)
            }
            Err(source) => Err(BeginDurablePublicationError::Retained {
                finished: self,
                source,
            }),
        }
    }
}

impl<Version, StoreError, PublicationInput> fmt::Debug
    for ManagedFinishedAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedFinishedAttempt")
            .finish_non_exhaustive()
    }
}

enum AttemptState<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    Active(ErasedActiveAttempt<Version, StoreError, PublicationInput>),
    Finished(ErasedFinishedAttempt<Version, StoreError, PublicationInput>),
    Transferred,
    Aborted,
}

impl<Version, StoreError, PublicationInput> AttemptState<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    fn phase(&self) -> ManagedAttemptPhase {
        match self {
            Self::Active(_) => ManagedAttemptPhase::Active,
            Self::Finished(_) => ManagedAttemptPhase::Finished,
            Self::Transferred => ManagedAttemptPhase::Transferred,
            Self::Aborted => ManagedAttemptPhase::Aborted,
        }
    }
}

/// Start a managed attempt after verifying that a task can own its async abort.
pub(crate) fn start_managed_streaming_attempt<C, Props, R, I>(
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    live_runtime: R,
    interpreter: I,
) -> Result<ManagedActiveAttempt, ManagedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
{
    // Check before instantiating a live scope so a no-runtime failure cannot
    // create an attempt that has no managed cleanup owner.
    let runtime =
        tokio::runtime::Handle::try_current().map_err(|_| ManagedAttemptStartError::NoRuntime)?;
    start_managed_streaming_attempt_on(
        &runtime,
        prepared,
        owner_contract,
        live_runtime,
        interpreter,
    )
}

/// Start a managed streaming attempt on the runtime selected by its owner.
pub(crate) fn start_managed_streaming_attempt_on<C, Props, R, I>(
    runtime: &tokio::runtime::Handle,
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    live_runtime: R,
    interpreter: I,
) -> Result<ManagedActiveAttempt, ManagedAttemptStartError>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    R: LiveEffectRuntime<C::Live>,
    I: AttemptUpdateInterpreter<C>,
{
    let active =
        start_erased_streaming_attempt(prepared, owner_contract, live_runtime, interpreter)?;
    Ok(spawn_managed_attempt(runtime, active))
}

/// Start the managed owner with its durable staging adapter installed before
/// root-channel erasure.
pub(crate) fn start_managed_streaming_attempt_with_durable_finalizer<
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
    finalizer: F,
) -> Result<ManagedActiveAttempt<Version, StoreError, PublicationInput>, ManagedAttemptStartError>
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
    let runtime =
        tokio::runtime::Handle::try_current().map_err(|_| ManagedAttemptStartError::NoRuntime)?;
    start_managed_streaming_attempt_with_durable_finalizer_on(
        &runtime,
        prepared,
        owner_contract,
        live_runtime,
        interpreter,
        finalizer,
    )
}

/// Start a durable managed streaming attempt on the owner's selected runtime.
pub(crate) fn start_managed_streaming_attempt_with_durable_finalizer_on<
    C,
    Props,
    R,
    I,
    F,
    Version,
    StoreError,
    PublicationInput,
>(
    runtime: &tokio::runtime::Handle,
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    live_runtime: R,
    interpreter: I,
    finalizer: F,
) -> Result<ManagedActiveAttempt<Version, StoreError, PublicationInput>, ManagedAttemptStartError>
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
    let active = start_erased_streaming_attempt_with_durable_finalizer(
        prepared,
        owner_contract,
        live_runtime,
        interpreter,
        finalizer,
    )?;
    Ok(spawn_managed_attempt(runtime, active))
}

/// Start a durable managed attempt using the identity already exposed to the
/// host's Live runtime factory.
pub(crate) fn start_managed_streaming_attempt_with_durable_finalizer_and_identity_on<
    C,
    Props,
    R,
    I,
    F,
    Version,
    StoreError,
    PublicationInput,
>(
    runtime: &tokio::runtime::Handle,
    prepared: &PreparedUserTurn<'_, '_, C, Props>,
    owner_contract: &ChannelTypeInfo,
    identity: ProviderAttemptIdentity,
    live_runtime: R,
    interpreter: I,
    finalizer: F,
) -> Result<ManagedActiveAttempt<Version, StoreError, PublicationInput>, ManagedAttemptStartError>
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
    let active = start_erased_streaming_attempt_with_durable_finalizer_and_identity(
        prepared,
        owner_contract,
        identity,
        live_runtime,
        interpreter,
        finalizer,
    )?;
    Ok(spawn_managed_attempt(runtime, active))
}

fn spawn_managed_attempt<Version, StoreError, PublicationInput>(
    runtime: &tokio::runtime::Handle,
    active: ErasedActiveAttempt<Version, StoreError, PublicationInput>,
) -> ManagedActiveAttempt<Version, StoreError, PublicationInput>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    let (commands, receiver) = mpsc::unbounded_channel();
    runtime.spawn(run_attempt(
        AttemptState::Active(active),
        receiver,
        runtime.clone(),
    ));
    ManagedActiveAttempt {
        control: Some(AttemptControl { commands }),
    }
}

async fn run_attempt<Version, StoreError, PublicationInput>(
    mut state: AttemptState<Version, StoreError, PublicationInput>,
    mut commands: mpsc::UnboundedReceiver<AttemptCommand<Version, StoreError, PublicationInput>>,
    runtime: tokio::runtime::Handle,
) where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    while let Some(command) = commands.recv().await {
        match command {
            AttemptCommand::Drive { input, reply } => {
                let result = drive(&mut state, input).await;
                let _ = reply.send(result);
            }
            AttemptCommand::Finish { reply } => {
                let result = finish(&mut state).await;
                let _ = reply.send(result);
            }
            AttemptCommand::BeginDurablePublication { input, reply } => {
                let result = begin_durable_publication(&mut state, input, &runtime);
                let _ = reply.send(result);
            }
            AttemptCommand::Abort { reason, reply } => {
                let result = abort(&mut state, reason).await;
                let _ = reply.send(result);
            }
            AttemptCommand::Phase { reply } => {
                let _ = reply.send(state.phase());
            }
        }
    }

    // Losing every client is a cancellation request, not permission to drop a
    // live scope. The actor owns the state until this acknowledgement finishes.
    let _ = abort(&mut state, BindingAbortReason::Cancelled).await;
}

async fn drive<Version, StoreError, PublicationInput>(
    state: &mut AttemptState<Version, StoreError, PublicationInput>,
    input: ProviderWireInput,
) -> Result<AttemptDriverStep, ManagedAttemptError>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    let current = std::mem::replace(state, AttemptState::Aborted);
    match current {
        AttemptState::Active(mut attempt) => match attempt.drive(input).await {
            Ok(step) => {
                *state = AttemptState::Active(attempt);
                Ok(step)
            }
            Err(source) => {
                let reason = abort_reason_for_fault(&source);
                let abort_report = attempt.abort(reason).await;
                Err(ManagedAttemptFailure {
                    source: Box::new(source),
                    abort_report: Box::new(abort_report),
                }
                .into())
            }
        },
        other => {
            let phase = other.phase();
            *state = other;
            Err(ManagedAttemptError::WrongPhase {
                phase,
                required: "an active attempt",
            })
        }
    }
}

async fn finish<Version, StoreError, PublicationInput>(
    state: &mut AttemptState<Version, StoreError, PublicationInput>,
) -> Result<FinishedAttemptRecord, ManagedAttemptError>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    let current = std::mem::replace(state, AttemptState::Aborted);
    match current {
        AttemptState::Active(attempt) => match attempt.finish().await {
            Ok(finished) => {
                let record = FinishedAttemptRecord {
                    raw_output: finished.raw_output().to_owned(),
                    provider_results: finished.provider_results().to_vec(),
                };
                *state = AttemptState::Finished(finished);
                Ok(record)
            }
            Err(failure) => {
                let reason = abort_reason_for_fault(failure.error());
                let (source, abort_report) = failure.into_error_and_abort(reason).await;
                Err(ManagedAttemptFailure {
                    source: Box::new(source),
                    abort_report: Box::new(abort_report),
                }
                .into())
            }
        },
        other => {
            let phase = other.phase();
            *state = other;
            Err(ManagedAttemptError::WrongPhase {
                phase,
                required: "an active attempt",
            })
        }
    }
}

fn begin_durable_publication<Version, StoreError, PublicationInput>(
    state: &mut AttemptState<Version, StoreError, PublicationInput>,
    input: PublicationInput,
    runtime: &tokio::runtime::Handle,
) -> Result<ManagedDurablePublication<Version, StoreError>, ManagedAttemptError>
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    let current = std::mem::replace(state, AttemptState::Aborted);
    match current {
        AttemptState::Finished(attempt) => {
            match attempt.begin_durable_publication(input, runtime) {
                Ok(publication) => {
                    *state = AttemptState::Transferred;
                    Ok(publication)
                }
                Err(failure) => {
                    let (attempt, fault) = failure.into_parts();
                    *state = AttemptState::Finished(attempt);
                    Err(fault.into())
                }
            }
        }
        other => {
            let phase = other.phase();
            *state = other;
            Err(ManagedAttemptError::WrongPhase {
                phase,
                required: "a finished attempt with a durable finalizer",
            })
        }
    }
}

fn abort_reason_for_fault(fault: &AttemptDriverFault) -> BindingAbortReason {
    match fault {
        AttemptDriverFault::Interpretation(_) => BindingAbortReason::HostInterpretationFailure,
        AttemptDriverFault::Streaming(fault) => match fault {
            StreamingAttemptError::Binding(_) => BindingAbortReason::ParserFailure,
            StreamingAttemptError::Live(_) => BindingAbortReason::HostRuntimeFailure,
            StreamingAttemptError::Provider(fault) => abort_reason_for_tool_fault(fault),
            StreamingAttemptError::TextCompletionMismatch { .. }
            | StreamingAttemptError::TextAfterCompletion
            | StreamingAttemptError::Terminal => BindingAbortReason::ProviderFailure,
        },
        AttemptDriverFault::Tool(fault) => abort_reason_for_tool_fault(fault),
        AttemptDriverFault::Terminal => BindingAbortReason::ProviderFailure,
    }
}

fn abort_reason_for_tool_fault(fault: &ProviderToolAttemptError) -> BindingAbortReason {
    match fault {
        ProviderToolAttemptError::Dispatcher(_) | ProviderToolAttemptError::Live(_) => {
            BindingAbortReason::HostRuntimeFailure
        }
        ProviderToolAttemptError::InvocationIdCollision(_) | ProviderToolAttemptError::Terminal => {
            BindingAbortReason::ProviderFailure
        }
    }
}

async fn abort<Version, StoreError, PublicationInput>(
    state: &mut AttemptState<Version, StoreError, PublicationInput>,
    reason: BindingAbortReason,
) -> ManagedAttemptAbort
where
    Version: Clone + Send + 'static,
    StoreError: Error + Send + Sync + 'static,
    PublicationInput: Send + 'static,
{
    let current = std::mem::replace(state, AttemptState::Aborted);
    match current {
        AttemptState::Active(attempt) => ManagedAttemptAbort::Aborted(attempt.abort(reason).await),
        AttemptState::Finished(attempt) => {
            ManagedAttemptAbort::Aborted(attempt.abort(reason).await)
        }
        AttemptState::Transferred => ManagedAttemptAbort::Transferred,
        AttemptState::Aborted => ManagedAttemptAbort::AlreadyAborted,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Condvar, Mutex,
        },
    };

    use serde_json::json;
    use tokio::{sync::Notify, task::yield_now, time::timeout};

    use crate::{
        component::{
            mount_system_epoch, provider_tool, system_view, user_view, BindingAbortAck,
            CommitContract, CommitStager, CommitStagingContext, DurableKeyError, LiveAbortContext,
            LiveEffectAbortAck, LiveEffectContext, PreparedSessionMutation,
            ProviderAttemptIdentity, ProviderDispatchContext, ProviderDispatchUpdate,
            ProviderDispatcher, ProviderDispatcherAbortAck, ProviderDispatcherAbortContext,
            ProviderToolCall, ProviderToolResponse, ProviderToolSpec,
            PublicationCandidateFingerprint, PublicationFingerprintContext,
            PublicationFingerprintFactory, PublicationId, PublicationReceipt, PublicationRequest,
            PublicationRequestId, PublicationResolution, PublicationResolveError,
            PublicationStagingPlan, PublicationStore, PublicationWriteError, StagedCommit,
            StagedStreamingPublication, StreamUpdate, StreamingXml, SystemMountContext, SystemView,
            TurnEmission,
        },
        llm_call::TextTurnEvent,
        pom::{XmlName, XmlNode},
    };

    use super::super::attempt_driver::{
        DurablePublicationLaunchFailure, DurablePublicationPlanContext,
        DurablePublicationPlanFactory, DurableStreamingPublicationFinalizer,
        DurableStreamingPublicationLauncher,
    };
    use super::super::managed_publication::{
        start_managed_streaming_publication_on, ManagedStreamingPublication,
    };

    use super::*;

    struct TestChannels;

    impl TurnChannels for TestChannels {
        type Output = ();
        type Live = ();
        type Commit = ();
        type Diagnostic = ();
    }

    #[derive(Clone)]
    struct Gate {
        entered: Arc<AtomicBool>,
        entered_notify: Arc<Notify>,
        release: Arc<Notify>,
        aborts: Arc<AtomicUsize>,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                entered: Arc::new(AtomicBool::new(false)),
                entered_notify: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
                aborts: Arc::new(AtomicUsize::new(0)),
            }
        }

        async fn wait_until_entered(&self) {
            while !self.entered.load(Ordering::SeqCst) {
                self.entered_notify.notified().await;
            }
        }
    }

    #[derive(Clone)]
    struct BlockingBoundary {
        entered: Arc<AtomicBool>,
        entered_notify: Arc<Notify>,
        released: Arc<(Mutex<bool>, Condvar)>,
    }

    impl BlockingBoundary {
        fn new() -> Self {
            Self {
                entered: Arc::new(AtomicBool::new(false)),
                entered_notify: Arc::new(Notify::new()),
                released: Arc::new((Mutex::new(false), Condvar::new())),
            }
        }

        fn block(&self) {
            self.entered.store(true, Ordering::SeqCst);
            self.entered_notify.notify_waiters();
            let (released, wake) = &*self.released;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }

        async fn wait_until_entered(&self) {
            while !self.entered.load(Ordering::SeqCst) {
                self.entered_notify.notified().await;
            }
        }

        fn release(&self) {
            let (released, wake) = &*self.released;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
    }

    #[async_trait::async_trait]
    impl LiveEffectRuntime<()> for Gate {
        type Error = Infallible;

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            _effect: (),
        ) -> Result<(), Self::Error> {
            self.entered.store(true, Ordering::SeqCst);
            self.entered_notify.notify_waiters();
            self.release.notified().await;
            Ok(())
        }

        async fn abort(
            &mut self,
            _context: &LiveAbortContext,
        ) -> Result<LiveEffectAbortAck, Self::Error> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(LiveEffectAbortAck::CompensationCompleted)
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("live runtime rejected the effect")]
    struct LiveRejected;

    struct RejectingLive {
        aborts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LiveEffectRuntime<()> for RejectingLive {
        type Error = LiveRejected;

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            _effect: (),
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

    struct IgnoreUpdates;

    impl AttemptUpdateInterpreter<TestChannels> for IgnoreUpdates {
        type Error = Infallible;

        fn interpret(
            &mut self,
            _identity: &ProviderAttemptIdentity,
            _update: super::super::StreamUpdate<TurnEmission<TestChannels>, ()>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    struct TestCommitStager;

    impl CommitStager<TestChannels> for TestCommitStager {
        type Payload = String;
        type Error = Infallible;

        fn stage(
            &self,
            context: CommitStagingContext<'_>,
            _commit: &(),
        ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
            Ok(StagedCommit::new(
                CommitContract::new("test.managed-handoff", 1).unwrap(),
                context.item_id().to_string(),
            ))
        }
    }

    struct BlockingCommitStager {
        boundary: BlockingBoundary,
    }

    impl CommitStager<TestChannels> for BlockingCommitStager {
        type Payload = String;
        type Error = Infallible;

        fn stage(
            &self,
            context: CommitStagingContext<'_>,
            _commit: &(),
        ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
            self.boundary.block();
            Ok(StagedCommit::new(
                CommitContract::new("test.managed-handoff", 1).unwrap(),
                context.item_id().to_string(),
            ))
        }
    }

    struct TestPlanFactory {
        calls: Arc<AtomicUsize>,
        staged: Arc<Notify>,
    }

    impl DurablePublicationPlanFactory<String, u64> for TestPlanFactory {
        type Error = Infallible;

        fn prepare(
            &mut self,
            _input: (),
            context: DurablePublicationPlanContext<'_>,
        ) -> Result<PublicationStagingPlan<String, u64>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.staged.notify_one();
            assert!(context.provider_results().is_empty());
            Ok(PublicationStagingPlan::new(
                PublicationRequestId::new("managed-handoff/session-1").unwrap(),
                4,
                PreparedSessionMutation::new(context.raw_output().to_owned()),
            ))
        }
    }

    struct NativePlanFactory {
        calls: Arc<AtomicUsize>,
    }

    impl DurablePublicationPlanFactory<String, u64> for NativePlanFactory {
        type Error = Infallible;

        fn prepare(
            &mut self,
            _input: (),
            context: DurablePublicationPlanContext<'_>,
        ) -> Result<PublicationStagingPlan<String, u64>, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(context.raw_output(), "");
            assert_eq!(context.provider_results().len(), 1);
            Ok(PublicationStagingPlan::new(
                PublicationRequestId::new("managed-handoff/native-session-1").unwrap(),
                9,
                PreparedSessionMutation::new(format!(
                    "native-results={}",
                    context.provider_results().len()
                )),
            ))
        }
    }

    #[derive(Debug, thiserror::Error)]
    enum TestFingerprintError {
        #[error("scripted fingerprint failure")]
        Scripted,

        #[error(transparent)]
        Invalid(#[from] DurableKeyError),
    }

    struct TestFingerprintFactory {
        fail_once: Arc<AtomicBool>,
    }

    impl PublicationFingerprintFactory<String, String, u64> for TestFingerprintFactory {
        type Error = TestFingerprintError;

        fn fingerprint(
            &self,
            context: PublicationFingerprintContext<'_, String, String, u64>,
        ) -> Result<PublicationCandidateFingerprint, Self::Error> {
            assert_eq!(context.outbox().len(), 1);
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(TestFingerprintError::Scripted);
            }
            Ok(PublicationCandidateFingerprint::new(format!(
                "handoff-v1|request={}|revision={}|mutation={:?}|raw={:?}|results={:?}|outbox={:?}",
                context.request_id(),
                context.expected_revision(),
                context.mutation().value(),
                context.raw_output(),
                context.provider_results(),
                context.outbox(),
            ))?)
        }
    }

    #[derive(Debug, Clone, thiserror::Error)]
    #[error("managed handoff store rejected the publication")]
    struct HandoffStoreError;

    #[derive(Default)]
    struct HandoffStore {
        receipt: Mutex<Option<PublicationReceipt<u64>>>,
        mutation: Mutex<Option<String>>,
        publish_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl PublicationStore<String, String> for HandoffStore {
        type Version = u64;
        type Error = HandoffStoreError;

        async fn publish(
            &self,
            request: PublicationRequest<'_, String, String, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            self.publish_calls.fetch_add(1, Ordering::SeqCst);
            let mut receipt = self.receipt.lock().unwrap();
            if let Some(receipt) = receipt.as_ref() {
                return Ok(receipt.clone());
            }
            let committed = PublicationReceipt::new(
                request.request_id().clone(),
                request.fingerprint().clone(),
                PublicationId::new("managed-handoff/publication-1").unwrap(),
                request.expected_revision() + 1,
                u64::try_from(request.outbox().len()).unwrap(),
            );
            *self.mutation.lock().unwrap() = Some(request.mutation().value().clone());
            *receipt = Some(committed.clone());
            Ok(committed)
        }

        async fn resolve(
            &self,
            _request_id: &PublicationRequestId,
            _fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            Ok(match self.receipt.lock().unwrap().as_ref() {
                Some(receipt) => PublicationResolution::Published(receipt.clone()),
                None => PublicationResolution::NotCommitted,
            })
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("scripted publication launch rejection")]
    struct LaunchRejected;

    struct RejectOnceLauncher {
        reject_once: Arc<AtomicBool>,
    }

    impl DurableStreamingPublicationLauncher<TestChannels, String, String, u64, HandoffStore>
        for RejectOnceLauncher
    {
        fn start(
            &mut self,
            runtime: &tokio::runtime::Handle,
            publication: StagedStreamingPublication<TestChannels, String, String, u64>,
            store: Arc<HandoffStore>,
        ) -> Result<
            ManagedStreamingPublication<TestChannels, u64, HandoffStoreError>,
            DurablePublicationLaunchFailure<
                StagedStreamingPublication<TestChannels, String, String, u64>,
            >,
        > {
            if self.reject_once.swap(false, Ordering::SeqCst) {
                return Err(DurablePublicationLaunchFailure::new(
                    publication,
                    LaunchRejected,
                ));
            }
            Ok(start_managed_streaming_publication_on(
                runtime,
                publication,
                store,
            ))
        }
    }

    struct BlockingLauncher {
        boundary: BlockingBoundary,
    }

    impl DurableStreamingPublicationLauncher<TestChannels, String, String, u64, HandoffStore>
        for BlockingLauncher
    {
        fn start(
            &mut self,
            runtime: &tokio::runtime::Handle,
            publication: StagedStreamingPublication<TestChannels, String, String, u64>,
            store: Arc<HandoffStore>,
        ) -> Result<
            ManagedStreamingPublication<TestChannels, u64, HandoffStoreError>,
            DurablePublicationLaunchFailure<
                StagedStreamingPublication<TestChannels, String, String, u64>,
            >,
        > {
            self.boundary.block();
            Ok(start_managed_streaming_publication_on(
                runtime,
                publication,
                store,
            ))
        }
    }

    struct RejectUpdates;

    #[derive(Debug, thiserror::Error)]
    #[error("update rejected")]
    struct UpdateRejected;

    impl AttemptUpdateInterpreter<TestChannels> for RejectUpdates {
        type Error = UpdateRejected;

        fn interpret(
            &mut self,
            _identity: &ProviderAttemptIdentity,
            _update: super::super::StreamUpdate<TurnEmission<TestChannels>, ()>,
        ) -> Result<(), Self::Error> {
            Err(UpdateRejected)
        }
    }

    struct MountProps {
        local_aborts: Arc<AtomicUsize>,
        finish_live: bool,
    }

    fn contract() -> XmlNode {
        XmlNode::new(XmlName::try_from("selection").unwrap())
    }

    fn stream_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<TestChannels> {
        let local_aborts = Arc::clone(&cx.props().local_aborts);
        let mut stream = StreamingXml::<TurnEmission<TestChannels>, ()>::new(contract())
            .state_with(|| ())
            .on_abort(move |_, _| {
                local_aborts.fetch_add(1, Ordering::SeqCst);
                BindingAbortAck::LocalCleanupCompleted
            });
        if cx.props().finish_live {
            stream = stream.on_finish(|_| vec![TurnEmission::Live(())]);
        } else {
            stream = stream.on_complete(|_, _| vec![TurnEmission::Live(())]);
        }
        system_view(stream.into_component())
    }

    fn durable_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<TestChannels> {
        let local_aborts = Arc::clone(&cx.props().local_aborts);
        system_view(
            StreamingXml::<TurnEmission<TestChannels>, ()>::new(contract())
                .state_with(|| ())
                .on_finish(|_| vec![TurnEmission::Commit(())])
                .on_abort(move |_, _| {
                    local_aborts.fetch_add(1, Ordering::SeqCst);
                    BindingAbortAck::LocalCleanupCompleted
                })
                .into_component(),
        )
    }

    #[derive(Debug)]
    struct NativeCommitDispatcher {
        aborts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProviderDispatcher<TestChannels> for NativeCommitDispatcher {
        type Error = Infallible;

        async fn dispatch(
            &mut self,
            _context: &ProviderDispatchContext,
            _call: ProviderToolCall,
        ) -> Result<ProviderDispatchUpdate<TestChannels>, Self::Error> {
            Ok(ProviderDispatchUpdate::new(
                ProviderToolResponse::success("native commit staged"),
                StreamUpdate::from_emission(TurnEmission::Commit(())),
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

    fn native_durable_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<TestChannels> {
        let aborts = Arc::clone(&cx.props().local_aborts);
        system_view(provider_tool(
            "native-commit",
            ProviderToolSpec::new(
                "native-commit",
                "Produce one native Commit value",
                json!({ "type": "object", "properties": {} }),
            )
            .unwrap(),
            move || NativeCommitDispatcher {
                aborts: Arc::clone(&aborts),
            },
        ))
    }

    fn start<I>(
        gate: Gate,
        finish_live: bool,
        interpreter: I,
    ) -> (ManagedActiveAttempt, Arc<AtomicUsize>)
    where
        I: AttemptUpdateInterpreter<TestChannels>,
    {
        start_with_runtime(gate, finish_live, interpreter)
    }

    fn start_with_runtime<R, I>(
        live_runtime: R,
        finish_live: bool,
        interpreter: I,
    ) -> (ManagedActiveAttempt, Arc<AtomicUsize>)
    where
        R: LiveEffectRuntime<()>,
        I: AttemptUpdateInterpreter<TestChannels>,
    {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &MountProps {
                local_aborts: Arc::clone(&local_aborts),
                finish_live,
            },
            stream_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("managed-test");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let attempt = start_managed_streaming_attempt(
            &prepared,
            epoch.channel_type_info(),
            live_runtime,
            interpreter,
        )
        .unwrap();
        (attempt, local_aborts)
    }

    type DurableFixture = (
        ManagedActiveAttempt<u64, HandoffStoreError>,
        Gate,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<Notify>,
        Arc<HandoffStore>,
    );

    type NativeDurableFixture = (
        ManagedActiveAttempt<u64, HandoffStoreError>,
        Gate,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<HandoffStore>,
    );

    fn start_durable(fail_fingerprint_once: bool, fail_launch_once: bool) -> DurableFixture {
        start_durable_with(
            fail_fingerprint_once,
            TestCommitStager,
            RejectOnceLauncher {
                reject_once: Arc::new(AtomicBool::new(fail_launch_once)),
            },
        )
    }

    fn start_durable_with<Stager, Launcher>(
        fail_fingerprint_once: bool,
        stager: Stager,
        launcher: Launcher,
    ) -> DurableFixture
    where
        Stager: CommitStager<TestChannels, Payload = String>,
        Launcher:
            DurableStreamingPublicationLauncher<TestChannels, String, String, u64, HandoffStore>,
    {
        let local_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &MountProps {
                local_aborts: Arc::clone(&local_aborts),
                finish_live: false,
            },
            durable_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("managed-durable-test");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let gate = Gate::new();
        let plan_calls = Arc::new(AtomicUsize::new(0));
        let staged = Arc::new(Notify::new());
        let store = Arc::new(HandoffStore::default());
        let finalizer = DurableStreamingPublicationFinalizer::new(
            TestPlanFactory {
                calls: Arc::clone(&plan_calls),
                staged: Arc::clone(&staged),
            },
            stager,
            TestFingerprintFactory {
                fail_once: Arc::new(AtomicBool::new(fail_fingerprint_once)),
            },
            Arc::clone(&store),
        )
        .with_launcher(launcher);
        let attempt = start_managed_streaming_attempt_with_durable_finalizer(
            &prepared,
            epoch.channel_type_info(),
            gate.clone(),
            IgnoreUpdates,
            finalizer,
        )
        .unwrap();
        (attempt, gate, local_aborts, plan_calls, staged, store)
    }

    fn start_native_durable() -> NativeDurableFixture {
        let dispatcher_aborts = Arc::new(AtomicUsize::new(0));
        let epoch = mount_system_epoch(
            &MountProps {
                local_aborts: Arc::clone(&dispatcher_aborts),
                finish_live: false,
            },
            native_durable_system,
        )
        .unwrap();
        let turn = epoch.begin_turn("managed-native-durable-test");
        let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
        let gate = Gate::new();
        let plan_calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(HandoffStore::default());
        let finalizer = DurableStreamingPublicationFinalizer::new(
            NativePlanFactory {
                calls: Arc::clone(&plan_calls),
            },
            TestCommitStager,
            TestFingerprintFactory {
                fail_once: Arc::new(AtomicBool::new(false)),
            },
            Arc::clone(&store),
        );
        let attempt = start_managed_streaming_attempt_with_durable_finalizer(
            &prepared,
            epoch.channel_type_info(),
            gate.clone(),
            IgnoreUpdates,
            finalizer,
        )
        .unwrap();
        (attempt, gate, dispatcher_aborts, plan_calls, store)
    }

    async fn wait_for_abort(gate: &Gate, local_aborts: &AtomicUsize) {
        timeout(std::time::Duration::from_secs(1), async {
            while gate.aborts.load(Ordering::SeqCst) == 0
                || local_aborts.load(Ordering::SeqCst) == 0
            {
                yield_now().await;
            }
        })
        .await
        .expect("managed owner should acknowledge cancellation");
    }

    #[tokio::test]
    async fn cancellation_while_drive_is_in_flight_still_aborts_the_live_scope() {
        let gate = Gate::new();
        let (attempt, local_aborts) = start(gate.clone(), false, IgnoreUpdates);
        let drive = tokio::spawn(async move {
            let mut attempt = attempt;
            attempt
                .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                    "<selection />".to_owned(),
                )))
                .await
        });

        gate.wait_until_entered().await;
        drive.abort();
        let _ = drive.await;
        gate.release.notify_waiters();
        wait_for_abort(&gate, &local_aborts).await;
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_while_finish_is_in_flight_aborts_the_finished_state() {
        let gate = Gate::new();
        let (mut attempt, local_aborts) = start(gate.clone(), true, IgnoreUpdates);
        attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();

        let finish = tokio::spawn(async move { attempt.finish().await });
        gate.wait_until_entered().await;
        finish.abort();
        let _ = finish.await;
        gate.release.notify_waiters();
        wait_for_abort(&gate, &local_aborts).await;
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_finish_transfers_a_finished_owner_with_explicit_abort() {
        let gate = Gate::new();
        let (attempt, local_aborts) = start(gate.clone(), false, IgnoreUpdates);

        let finished = attempt.finish().await.unwrap();
        assert_eq!(
            finished.phase().await.unwrap(),
            ManagedAttemptPhase::Finished
        );

        let aborted = finished.abort(BindingAbortReason::Cancelled).await.unwrap();
        assert_eq!(
            aborted.report().unwrap().reason(),
            BindingAbortReason::Cancelled
        );
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_durable_finalizer_retains_finished_for_explicit_abort() {
        let gate = Gate::new();
        let (attempt, local_aborts) = start(gate.clone(), false, IgnoreUpdates);
        let finished = attempt.finish().await.unwrap();

        let failure = finished.begin_durable_publication(()).await.unwrap_err();
        assert!(failure
            .source_error()
            .unwrap()
            .to_string()
            .contains("no durable publication finalizer"));
        let finished = failure.into_finished().unwrap();
        assert_eq!(
            finished.phase().await.unwrap(),
            ManagedAttemptPhase::Finished
        );
        finished.abort(BindingAbortReason::Cancelled).await.unwrap();
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn durable_staging_failure_restores_finished_actor_and_exact_plan_for_retry() {
        let (mut attempt, gate, local_aborts, plan_calls, _staged, store) =
            start_durable(true, false);
        attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap();
        let finished = attempt.finish().await.unwrap();

        let failure = finished.begin_durable_publication(()).await.unwrap_err();
        assert!(failure
            .source_error()
            .unwrap()
            .to_string()
            .contains("scripted fingerprint failure"));
        let finished = failure
            .into_finished()
            .expect("staging failure keeps the old actor handle");
        assert_eq!(
            finished.phase().await.unwrap(),
            ManagedAttemptPhase::Finished
        );

        let publication = finished.begin_durable_publication(()).await.unwrap();
        assert_eq!(plan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            publication.phase().await.unwrap(),
            super::super::managed_publication::ManagedPublicationPhase::Ready
        );
        let published = publication.publish().await.unwrap();
        assert_eq!(published.request_id().as_str(), "managed-handoff/session-1");
        assert_eq!(*published.session_revision(), 5);
        assert_eq!(published.outbox_count(), 1);
        let completed = publication.complete().await.unwrap();
        assert_eq!(completed, published);
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.mutation.lock().unwrap().as_deref(),
            Some("<selection />")
        );
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 0);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn synchronous_launch_failure_restores_finished_and_exact_plan() {
        let (attempt, gate, local_aborts, plan_calls, _staged, store) = start_durable(false, true);
        let finished = attempt.finish().await.unwrap();

        let failure = finished.begin_durable_publication(()).await.unwrap_err();
        assert!(failure
            .source_error()
            .unwrap()
            .to_string()
            .contains("scripted publication launch rejection"));
        let finished = failure
            .into_finished()
            .expect("launch failure keeps the old actor handle");
        assert_eq!(
            finished.phase().await.unwrap(),
            ManagedAttemptPhase::Finished
        );

        let publication = finished.begin_durable_publication(()).await.unwrap();
        assert_eq!(plan_calls.load(Ordering::SeqCst), 1);
        let receipt = publication.publish().await.unwrap();
        assert_eq!(receipt.request_id().as_str(), "managed-handoff/session-1");
        assert_eq!(*receipt.session_revision(), 5);
        let completed = publication.complete().await.unwrap();
        assert_eq!(completed, receipt);
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 0);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn native_tool_commit_uses_the_same_actor_to_actor_durable_handoff() {
        let (mut attempt, gate, dispatcher_aborts, plan_calls, store) = start_native_durable();
        let step = attempt
            .drive(ProviderWireInput::Tool(ProviderToolCall::new(
                "native-call-1",
                "native-commit",
                json!({}),
            )))
            .await
            .unwrap();
        assert_eq!(step.replayed(), Some(false));
        assert_eq!(step.tool_result().unwrap().name(), "native-commit");

        let finished = attempt.finish().await.unwrap();
        let publication = finished.begin_durable_publication(()).await.unwrap();
        let receipt = publication.publish().await.unwrap();
        assert_eq!(
            receipt.request_id().as_str(),
            "managed-handoff/native-session-1"
        );
        assert_eq!(*receipt.session_revision(), 10);
        assert_eq!(receipt.outbox_count(), 1);
        assert_eq!(publication.complete().await.unwrap(), receipt);

        assert_eq!(plan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.mutation.lock().unwrap().as_deref(),
            Some("native-results=1")
        );
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 0);
        assert_eq!(dispatcher_aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_staging_still_transfers_to_one_cleanup_owner() {
        let boundary = BlockingBoundary::new();
        let (attempt, gate, local_aborts, plan_calls, _staged, store) = start_durable_with(
            false,
            BlockingCommitStager {
                boundary: boundary.clone(),
            },
            RejectOnceLauncher {
                reject_once: Arc::new(AtomicBool::new(false)),
            },
        );
        let finished = attempt.finish().await.unwrap();
        let waiter = tokio::spawn(async move { finished.begin_durable_publication(()).await });

        boundary.wait_until_entered().await;
        waiter.abort();
        let _ = waiter.await;
        boundary.release();
        wait_for_abort(&gate, &local_aborts).await;

        assert_eq!(plan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 0);
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_launch_drops_reply_into_publication_recovery() {
        let boundary = BlockingBoundary::new();
        let (attempt, gate, local_aborts, plan_calls, _staged, store) = start_durable_with(
            false,
            TestCommitStager,
            BlockingLauncher {
                boundary: boundary.clone(),
            },
        );
        let finished = attempt.finish().await.unwrap();
        let waiter = tokio::spawn(async move { finished.begin_durable_publication(()).await });

        boundary.wait_until_entered().await;
        waiter.abort();
        let _ = waiter.await;
        boundary.release();
        wait_for_abort(&gate, &local_aborts).await;

        assert_eq!(plan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.publish_calls.load(Ordering::SeqCst), 0);
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn terminal_driver_failure_is_aborted_before_the_error_returns() {
        let gate = Gate::new();
        let (mut attempt, local_aborts) = start(gate.clone(), false, RejectUpdates);
        let drive = tokio::spawn(async move {
            attempt
                .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                    "<selection />".to_owned(),
                )))
                .await
        });
        gate.wait_until_entered().await;
        gate.release.notify_one();
        let error = drive.await.unwrap().unwrap_err();

        let ManagedAttemptError::Failed(failure) = error else {
            panic!("driver interpretation failure should retain an abort report");
        };
        assert!(matches!(
            failure.source_fault(),
            AttemptDriverFault::Interpretation(_)
        ));
        assert_eq!(
            failure.abort_report().reason(),
            BindingAbortReason::HostInterpretationFailure
        );
        assert_eq!(gate.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn live_runtime_failure_uses_host_runtime_abort_telemetry() {
        let runtime_aborts = Arc::new(AtomicUsize::new(0));
        let (mut attempt, local_aborts) = start_with_runtime(
            RejectingLive {
                aborts: Arc::clone(&runtime_aborts),
            },
            false,
            IgnoreUpdates,
        );

        let error = attempt
            .drive(ProviderWireInput::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap_err();
        let ManagedAttemptError::Failed(failure) = error else {
            panic!("live runtime failure should retain an abort report");
        };
        assert!(matches!(
            failure.source_fault(),
            AttemptDriverFault::Streaming(StreamingAttemptError::Live(_))
        ));
        assert_eq!(
            failure.abort_report().reason(),
            BindingAbortReason::HostRuntimeFailure
        );
        assert_eq!(runtime_aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fallible_provider_port_acknowledges_text_after_typed_delivery() {
        let gate = Gate::new();
        let (mut attempt, _) = start(gate, false, IgnoreUpdates);

        let acknowledgement = attempt
            .submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(
                "plain text".to_owned(),
            )))
            .await
            .unwrap();
        assert!(matches!(acknowledgement, ProviderWireAck::TextAccepted));

        let finished = attempt.finish().await.unwrap();
        finished.abort(BindingAbortReason::Cancelled).await.unwrap();
    }

    #[tokio::test]
    async fn fallible_provider_port_surfaces_terminal_host_failure_immediately() {
        let runtime_aborts = Arc::new(AtomicUsize::new(0));
        let (mut attempt, local_aborts) = start_with_runtime(
            RejectingLive {
                aborts: Arc::clone(&runtime_aborts),
            },
            false,
            IgnoreUpdates,
        );

        let fault = attempt
            .submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(
                "<selection />".to_owned(),
            )))
            .await
            .unwrap_err();

        assert!(fault
            .source_error()
            .to_string()
            .contains("managed attempt failed and was aborted"));
        assert_eq!(attempt.phase().await.unwrap(), ManagedAttemptPhase::Aborted);
        assert_eq!(runtime_aborts.load(Ordering::SeqCst), 1);
        assert_eq!(local_aborts.load(Ordering::SeqCst), 1);
    }
}
