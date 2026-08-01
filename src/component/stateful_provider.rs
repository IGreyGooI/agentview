//! Stateful remote-provider session adapter for a mounted durable epoch.
//!
//! This module deliberately separates the one System-bearing remote-session
//! attachment from ordinary User turns. It is an advanced host integration
//! boundary, not component authoring: ordinary POM authors do not construct
//! these values or receive a remote session handle.

use std::{error::Error, marker::PhantomData, sync::Arc};

use crate::llm_call::{ContextPreparation, ContextPreparationBudget};

use super::{
    durable_epoch::{
        AttachedProviderEpoch, EpochArtifactFingerprint, ProviderAdapterContract,
        ProviderEpochAttachRequest, ProviderEpochReceipt, ProviderEpochRehydrateRequest,
        ProviderToolDescriptor, ProviderTurnCursor,
    },
    provider_wire::{
        DurableMountedProviderExecutor, DurableMountedProviderOperationController,
        FallibleProviderWirePort, MountedProviderExecutor, MountedProviderExit,
        MountedProviderRequest, ProviderCancellationReason, ProviderCancellationToken,
        ProviderOperationStatus,
    },
    DurableEpochId, ProviderOperationIdentity,
};

/// System-bearing input to a stateful remote-session create-or-get operation.
///
/// The remote implementation must use `(durable_epoch_id,
/// artifact_fingerprint)` as an idempotency key. A retry for an existing pair
/// returns the original remote session/receipt and must not physically send
/// System or tool bytes a second time. A matching durable epoch with another
/// fingerprint is a contract mismatch, not a new epoch.
///
/// This type exists only at the factory attachment boundary. A session object
/// cannot receive it, so ordinary context preparation and execution cannot
/// accidentally resend System bytes.
pub struct ProviderSessionAttachRequest<'a> {
    durable_epoch_id: &'a DurableEpochId,
    artifact_fingerprint: &'a EpochArtifactFingerprint,
    system: &'a str,
    tools: &'a [ProviderToolDescriptor],
}

impl<'a> ProviderSessionAttachRequest<'a> {
    pub(crate) fn from_agentview(request: ProviderEpochAttachRequest<'a>) -> Self {
        Self {
            durable_epoch_id: request.durable_epoch_id(),
            artifact_fingerprint: request.fingerprint(),
            system: request.system(),
            tools: request.tools(),
        }
    }

    /// Durable remote-session identity selected by the durable epoch store.
    pub fn durable_epoch_id(&self) -> &'a DurableEpochId {
        self.durable_epoch_id
    }

    /// Canonical identity of the rendered System artifact.
    pub fn artifact_fingerprint(&self) -> &'a EpochArtifactFingerprint {
        self.artifact_fingerprint
    }

    /// The one System prompt payload for a newly created remote session.
    pub fn system(&self) -> &'a str {
        self.system
    }

    /// Native tool schema snapshot for the same immutable System epoch.
    pub fn tools(&self) -> &'a [ProviderToolDescriptor] {
        self.tools
    }
}

/// POM-free input for restoring a local binding to an existing remote session.
///
/// There deliberately is no System or tool accessor. Reopen and cursor reload
/// therefore cannot construct a second System transport through this API.
pub struct ProviderSessionRehydrateRequest<'a> {
    durable_epoch_id: &'a DurableEpochId,
    artifact_fingerprint: &'a EpochArtifactFingerprint,
    receipt: &'a ProviderEpochReceipt,
    cursor: Option<&'a ProviderTurnCursor>,
}

impl<'a> ProviderSessionRehydrateRequest<'a> {
    pub(crate) fn from_agentview(request: ProviderEpochRehydrateRequest<'a>) -> Self {
        Self {
            durable_epoch_id: request.durable_epoch_id(),
            artifact_fingerprint: request.fingerprint(),
            receipt: request.receipt(),
            cursor: request.cursor(),
        }
    }

    /// Durable remote-session identity selected by the durable epoch store.
    pub fn durable_epoch_id(&self) -> &'a DurableEpochId {
        self.durable_epoch_id
    }

    /// Canonical identity of the rendered System artifact.
    pub fn artifact_fingerprint(&self) -> &'a EpochArtifactFingerprint {
        self.artifact_fingerprint
    }

    /// Opaque receipt persisted after the initial remote attachment.
    pub fn receipt(&self) -> &'a ProviderEpochReceipt {
        self.receipt
    }

    /// Last cursor committed atomically with the durable mounted session.
    pub fn cursor(&self) -> Option<&'a ProviderTurnCursor> {
        self.cursor
    }
}

/// Local remote-session binding and its durable remote-session receipt.
///
/// The receipt is persisted by AgentView after epoch admission. The session is
/// process-local and may contain a client connection, remote session handle,
/// or transport cancellation machinery.
pub struct ProviderSessionAttachment<Session> {
    session: Session,
    receipt: ProviderEpochReceipt,
}

impl<Session> ProviderSessionAttachment<Session> {
    pub fn new(session: Session, receipt: ProviderEpochReceipt) -> Self {
        Self { session, receipt }
    }

    fn into_parts(self) -> (Session, ProviderEpochReceipt) {
        (self.session, self.receipt)
    }
}

/// Host-bound remote session for one durable mounted epoch.
///
/// Inputs deliberately exclude System POM and tool schemas. Tools return via
/// the fallible wire port as AgentView-owned dispatches; the provider session
/// must await each acknowledgement before emitting the next event. On
/// cancellation it stops transport work and returns only after local transport
/// joins. Every request must carry a durable provider operation identity; the
/// remote transport uses its idempotency key for prepare/execute recovery.
#[async_trait::async_trait]
pub trait StatefulMountedProviderSession<I>: Send + Sync + 'static
where
    I: Send + 'static,
{
    type Error: Error + Send + Sync + 'static;

    /// Prepare one System-free turn request.
    ///
    /// A rehydrated session may return [`ContextPreparation::ResyncUserDocument`]
    /// when it cannot safely apply the next incremental User document. The
    /// mounted owner retains the same System epoch and repeats preparation with
    /// a full User document.
    async fn prepare_context(
        &self,
        request: &MountedProviderRequest<I>,
        budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<I>, Self::Error>;

    /// Execute one System-free mounted provider operation.
    async fn execute(
        &self,
        request: MountedProviderRequest<I>,
        wire: &mut dyn FallibleProviderWirePort,
        cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<I>, Self::Error>;

    /// Inspect one retained remote operation without resending prompt data.
    async fn inspect_operation(
        &self,
        operation: &ProviderOperationIdentity,
    ) -> Result<ProviderOperationStatus, Self::Error>;

    /// Request cancellation by durable operation identity and join the remote
    /// operation before returning an authoritative `Cancelled` status.
    async fn cancel_operation(
        &self,
        operation: &ProviderOperationIdentity,
        reason: ProviderCancellationReason,
    ) -> Result<ProviderOperationStatus, Self::Error>;
}

/// Factory for remote sessions owned by durable mounted epochs.
///
/// `attach_or_get_epoch` is the only method allowed to receive System/tool
/// data and must be idempotent over the durable epoch identity. In contrast,
/// `rehydrate_epoch` receives only an opaque receipt and optional continuation
/// cursor.
#[async_trait::async_trait]
pub trait StatefulMountedProviderSessionFactory<I>: Send + Sync + 'static
where
    I: Send + 'static,
{
    type Session: StatefulMountedProviderSession<I>;
    type Error: Error + Send + Sync + 'static;

    /// Adapter contract used to validate persisted receipts and cursors.
    fn adapter_contract(&self) -> ProviderAdapterContract;

    /// Idempotently create or retrieve the remote session for this epoch.
    async fn attach_or_get_epoch(
        &self,
        request: ProviderSessionAttachRequest<'_>,
    ) -> Result<ProviderSessionAttachment<Self::Session>, Self::Error>;

    /// Rebind a local client/session handle from durable provider state.
    async fn rehydrate_epoch(
        &self,
        request: ProviderSessionRehydrateRequest<'_>,
    ) -> Result<Self::Session, Self::Error>;
}

/// Error projection for the stateful mounted provider adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StatefulMountedProviderAdapterError {
    #[error("stateful mounted provider {phase} requires a durable provider operation identity")]
    MissingProviderOperation { phase: &'static str },

    #[error("stateful mounted provider {phase} failed: {source}")]
    Phase {
        phase: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl StatefulMountedProviderAdapterError {
    fn phase(phase: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Phase {
            phase,
            source: Box::new(source),
        }
    }

    fn require_provider_operation<I>(
        phase: &'static str,
        request: &MountedProviderRequest<I>,
    ) -> Result<(), Self> {
        if request.provider_operation().is_some() {
            Ok(())
        } else {
            Err(Self::MissingProviderOperation { phase })
        }
    }
}

/// Adapter from a host-owned stateful session factory to AgentView's durable
/// mounted provider traits.
///
/// This is a host integration object, not a POM component. It owns no reducer
/// state or application effects. It requires a provider operation identity on
/// every preparation and execution request before it invokes remote I/O.
pub struct StatefulMountedProviderAdapter<I, Factory>
where
    I: Send + 'static,
    Factory: StatefulMountedProviderSessionFactory<I>,
{
    factory: Arc<Factory>,
    transcript: PhantomData<fn() -> I>,
}

impl<I, Factory> StatefulMountedProviderAdapter<I, Factory>
where
    I: Send + 'static,
    Factory: StatefulMountedProviderSessionFactory<I>,
{
    pub fn new(factory: Arc<Factory>) -> Self {
        Self {
            factory,
            transcript: PhantomData,
        }
    }
}

#[async_trait::async_trait]
impl<I, Factory> MountedProviderExecutor<I> for StatefulMountedProviderAdapter<I, Factory>
where
    I: Send + Sync + 'static,
    Factory: StatefulMountedProviderSessionFactory<I>,
{
    type Error = StatefulMountedProviderAdapterError;
    type Epoch = Factory::Session;

    async fn prepare_context(
        &self,
        epoch: &Self::Epoch,
        request: &MountedProviderRequest<I>,
        budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<I>, Self::Error> {
        StatefulMountedProviderAdapterError::require_provider_operation(
            "context preparation",
            request,
        )?;
        epoch
            .prepare_context(request, budget)
            .await
            .map_err(|source| {
                StatefulMountedProviderAdapterError::phase("context preparation", source)
            })
    }

    async fn execute(
        &self,
        epoch: &Self::Epoch,
        request: MountedProviderRequest<I>,
        wire: &mut dyn FallibleProviderWirePort,
        cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<I>, Self::Error> {
        StatefulMountedProviderAdapterError::require_provider_operation("execution", &request)?;
        epoch
            .execute(request, wire, cancellation)
            .await
            .map_err(|source| StatefulMountedProviderAdapterError::phase("execution", source))
    }
}

#[async_trait::async_trait]
impl<I, Factory> DurableMountedProviderExecutor<I> for StatefulMountedProviderAdapter<I, Factory>
where
    I: Send + Sync + 'static,
    Factory: StatefulMountedProviderSessionFactory<I>,
{
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        self.factory.adapter_contract()
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        let attachment = self
            .factory
            .attach_or_get_epoch(ProviderSessionAttachRequest::from_agentview(request))
            .await
            .map_err(|source| StatefulMountedProviderAdapterError::phase("epoch attach", source))?;
        let (session, receipt) = attachment.into_parts();
        Ok(AttachedProviderEpoch::new(session, receipt))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        self.factory
            .rehydrate_epoch(ProviderSessionRehydrateRequest::from_agentview(request))
            .await
            .map_err(|source| StatefulMountedProviderAdapterError::phase("epoch rehydrate", source))
    }
}

#[async_trait::async_trait]
impl<I, Factory> DurableMountedProviderOperationController<I>
    for StatefulMountedProviderAdapter<I, Factory>
where
    I: Send + Sync + 'static,
    Factory: StatefulMountedProviderSessionFactory<I>,
{
    async fn inspect_operation(
        &self,
        epoch: &Self::Epoch,
        operation: &ProviderOperationIdentity,
    ) -> Result<ProviderOperationStatus, Self::Error> {
        epoch.inspect_operation(operation).await.map_err(|source| {
            StatefulMountedProviderAdapterError::phase("operation inspection", source)
        })
    }

    async fn cancel_operation(
        &self,
        epoch: &Self::Epoch,
        operation: &ProviderOperationIdentity,
        reason: ProviderCancellationReason,
    ) -> Result<ProviderOperationStatus, Self::Error> {
        epoch
            .cancel_operation(operation, reason)
            .await
            .map_err(|source| {
                StatefulMountedProviderAdapterError::phase("operation cancellation", source)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Arc};

    use super::*;
    use crate::component::{
        DurableCallId, DurableCallInputId, DurableSessionId, PublicationRequestId,
    };

    struct TestSession;

    #[async_trait::async_trait]
    impl StatefulMountedProviderSession<String> for TestSession {
        type Error = Infallible;

        async fn prepare_context(
            &self,
            _request: &MountedProviderRequest<String>,
            _budget: ContextPreparationBudget,
        ) -> Result<ContextPreparation<String>, Self::Error> {
            Ok(ContextPreparation::Ready)
        }

        async fn execute(
            &self,
            _request: MountedProviderRequest<String>,
            _wire: &mut dyn FallibleProviderWirePort,
            _cancellation: ProviderCancellationToken,
        ) -> Result<MountedProviderExit<String>, Self::Error> {
            unreachable!("a missing provider operation must be rejected before execution")
        }

        async fn inspect_operation(
            &self,
            _operation: &ProviderOperationIdentity,
        ) -> Result<ProviderOperationStatus, Self::Error> {
            Ok(ProviderOperationStatus::NeverAccepted)
        }

        async fn cancel_operation(
            &self,
            _operation: &ProviderOperationIdentity,
            _reason: ProviderCancellationReason,
        ) -> Result<ProviderOperationStatus, Self::Error> {
            Ok(ProviderOperationStatus::Cancelled {
                cursor:
                    super::super::provider_wire::ProviderCancellationCursorDisposition::Unchanged,
            })
        }
    }

    struct TestFactory;

    #[async_trait::async_trait]
    impl StatefulMountedProviderSessionFactory<String> for TestFactory {
        type Session = TestSession;
        type Error = Infallible;

        fn adapter_contract(&self) -> ProviderAdapterContract {
            ProviderAdapterContract::new("stateful-provider-test", 1)
                .expect("static adapter contract is valid")
        }

        async fn attach_or_get_epoch(
            &self,
            _request: ProviderSessionAttachRequest<'_>,
        ) -> Result<ProviderSessionAttachment<Self::Session>, Self::Error> {
            unreachable!("the request test does not attach an epoch")
        }

        async fn rehydrate_epoch(
            &self,
            _request: ProviderSessionRehydrateRequest<'_>,
        ) -> Result<Self::Session, Self::Error> {
            unreachable!("the request test does not rehydrate an epoch")
        }
    }

    #[tokio::test]
    async fn missing_provider_operation_is_rejected_before_context_preparation() {
        let adapter = StatefulMountedProviderAdapter::new(Arc::new(TestFactory));
        let request = MountedProviderRequest::new(
            "stateful-provider-test-call",
            Vec::<String>::new(),
            "User-only prompt".to_owned(),
            "test-model",
            1,
        );

        let result = adapter
            .prepare_context(
                &TestSession,
                &request,
                ContextPreparationBudget::new(0, 0, 0),
            )
            .await;

        assert!(matches!(
            result,
            Err(
                StatefulMountedProviderAdapterError::MissingProviderOperation {
                    phase: "context preparation"
                }
            )
        ));
    }

    fn operation_identity() -> ProviderOperationIdentity {
        ProviderOperationIdentity::new(
            DurableSessionId::new("stateful-provider-control-session").unwrap(),
            DurableEpochId::new("stateful-provider-control-epoch").unwrap(),
            DurableCallId::new("stateful-provider-control-call").unwrap(),
            DurableCallInputId::new("stateful-provider-control-input").unwrap(),
            3,
            PublicationRequestId::new("stateful-provider-control-request").unwrap(),
        )
    }

    #[tokio::test]
    async fn stateful_adapter_delegates_pom_free_operation_control() {
        let adapter = StatefulMountedProviderAdapter::new(Arc::new(TestFactory));
        let operation = operation_identity();

        assert_eq!(
            DurableMountedProviderOperationController::inspect_operation(
                &adapter,
                &TestSession,
                &operation,
            )
            .await
            .unwrap(),
            ProviderOperationStatus::NeverAccepted
        );
        assert_eq!(
            DurableMountedProviderOperationController::cancel_operation(
                &adapter,
                &TestSession,
                &operation,
                ProviderCancellationReason::OwnerCancelled,
            )
            .await
            .unwrap(),
            ProviderOperationStatus::Cancelled {
                cursor:
                    super::super::provider_wire::ProviderCancellationCursorDisposition::Unchanged,
            }
        );
    }
}
