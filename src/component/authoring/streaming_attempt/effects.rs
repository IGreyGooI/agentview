//! Owned managed-lane ledger for one streaming-tool contract attempt.
//!
//! Parser/reducer code feeds one [`StreamingToolUpdate`] at a time into this
//! module.  The ledger owns every effect operation before it starts external
//! I/O, which makes an outer Application-owned worker able to survive loss of
//! its original waiter without guessing about an external result.

use std::{
    error::Error,
    panic::{catch_unwind, AssertUnwindSafe},
};

use async_trait::async_trait;
use futures::FutureExt;

use super::types::*;

/// Result of applying one Live value.
///
/// `Indeterminate` means the adapter cannot prove whether the external effect
/// happened. The caller must retain the prepared operation and resolve it
/// later; it must not retry the apply call directly.
pub enum LiveApplyOutcome<Receipt, AdapterError> {
    Applied(Receipt),
    NotApplied(AdapterError),
    Indeterminate,
}

/// Result of confirming or rolling back one applied Live receipt.
pub enum LiveSettleOutcome<AdapterError> {
    Settled,
    NotSettled(AdapterError),
    Indeterminate,
}

/// Resolution result for a previously indeterminate apply operation.
pub enum LiveApplyResolution<Receipt> {
    Applied(Receipt),
    NotApplied,
    StillIndeterminate,
}

/// Resolution result for a previously indeterminate settlement operation.
pub enum LiveSettleResolution {
    Settled,
    NotSettled,
    StillIndeterminate,
}

/// Adapter for a contract's immediately visible but compensatable values.
///
/// Operation and receipt values are deliberately borrowed for every later
/// call. They stay in the ledger until settlement is definitive, so adapter
/// implementations may use immutable operation IDs across fresh runtime
/// instances after a caught panic.
#[async_trait]
pub trait LiveEffectRuntime<Live>: Send + 'static {
    type Receipt: Send + Sync + 'static;
    type ApplyOperation: Send + Sync + 'static;
    type SettlementOperation: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    /// Pure, pre-I/O construction of stable apply evidence.
    fn prepare_apply(
        &mut self,
        context: &LiveEffectContext,
        effect: Live,
    ) -> Result<Self::ApplyOperation, Self::Error>;

    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        operation: &Self::ApplyOperation,
    ) -> LiveApplyOutcome<Self::Receipt, Self::Error>;

    async fn resolve_apply(
        &mut self,
        context: &LiveRecoveryContext,
        operation: &Self::ApplyOperation,
    ) -> Result<LiveApplyResolution<Self::Receipt>, Self::Error>;

    /// Pure, pre-I/O construction of one settlement operation per receipt.
    fn prepare_settlement(
        &mut self,
        context: &LiveSettlementContext,
        receipt: &Self::Receipt,
    ) -> Result<Self::SettlementOperation, Self::Error>;

    async fn confirm(
        &mut self,
        context: &LiveConfirmContext,
        receipt: &Self::Receipt,
        operation: &Self::SettlementOperation,
    ) -> LiveSettleOutcome<Self::Error>;

    async fn rollback(
        &mut self,
        context: &LiveRollbackContext,
        receipt: &Self::Receipt,
        operation: &Self::SettlementOperation,
    ) -> LiveSettleOutcome<Self::Error>;

    async fn resolve_settlement(
        &mut self,
        context: &LiveRecoveryContext,
        receipt: &Self::Receipt,
        operation: &Self::SettlementOperation,
    ) -> Result<LiveSettleResolution, Self::Error>;
}

/// Result of publishing a contract's accepted Output/Commit batch.
pub enum StreamingPublishOutcome<Published, AdapterError> {
    Published(Published),
    NotPublished(AdapterError),
    Indeterminate,
}

/// Resolution result for an indeterminate publication operation.
pub enum StreamingPublishResolution<Published> {
    Published(Published),
    NotPublished,
    StillIndeterminate,
}

/// Application-provided atomic publication boundary for one contract.
#[async_trait]
pub trait StreamingToolAttemptPublisher<C: StreamingToolChannels>: Send + 'static {
    type Published: Send + 'static;
    type PublicationOperation: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    /// Pure, pre-I/O construction. The operation owns the accepted attempt.
    fn prepare(
        &mut self,
        context: &StreamingPublishContext,
        attempt: AcceptedStreamingToolAttempt<C>,
    ) -> Result<Self::PublicationOperation, Self::Error>;

    async fn publish(
        &mut self,
        context: &StreamingPublishContext,
        operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<Self::Published, Self::Error>;

    async fn resolve(
        &mut self,
        context: &StreamingPublishRecoveryContext,
        operation: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<Self::Published>, Self::Error>;
}

/// Internal no-op runtime used by the builder when a contract's `Live` lane is
/// uninhabited. It is never constructed because the retained factory is `None`.
#[doc(hidden)]
pub struct NoLiveRuntime;

/// Internal no-op publisher used by the builder when Output and Commit are
/// both uninhabited. It is never constructed because the retained factory is
/// `None`.
#[doc(hidden)]
pub struct NoPublisher;

#[async_trait]
impl LiveEffectRuntime<NoStreamingValue> for NoLiveRuntime {
    type Receipt = NoStreamingValue;
    type ApplyOperation = NoStreamingValue;
    type SettlementOperation = NoStreamingValue;
    type Error = std::convert::Infallible;

    fn prepare_apply(
        &mut self,
        _context: &LiveEffectContext,
        effect: NoStreamingValue,
    ) -> Result<Self::ApplyOperation, Self::Error> {
        match effect {}
    }

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        _operation: &Self::ApplyOperation,
    ) -> LiveApplyOutcome<Self::Receipt, Self::Error> {
        unreachable!("NoLiveRuntime cannot receive a Live emission")
    }

    async fn resolve_apply(
        &mut self,
        _context: &LiveRecoveryContext,
        _operation: &Self::ApplyOperation,
    ) -> Result<LiveApplyResolution<Self::Receipt>, Self::Error> {
        unreachable!("NoLiveRuntime cannot resolve a Live emission")
    }

    fn prepare_settlement(
        &mut self,
        _context: &LiveSettlementContext,
        _receipt: &Self::Receipt,
    ) -> Result<Self::SettlementOperation, Self::Error> {
        unreachable!("NoLiveRuntime cannot settle a Live receipt")
    }

    async fn confirm(
        &mut self,
        _context: &LiveConfirmContext,
        _receipt: &Self::Receipt,
        _operation: &Self::SettlementOperation,
    ) -> LiveSettleOutcome<Self::Error> {
        unreachable!("NoLiveRuntime cannot confirm a Live receipt")
    }

    async fn rollback(
        &mut self,
        _context: &LiveRollbackContext,
        _receipt: &Self::Receipt,
        _operation: &Self::SettlementOperation,
    ) -> LiveSettleOutcome<Self::Error> {
        unreachable!("NoLiveRuntime cannot roll back a Live receipt")
    }

    async fn resolve_settlement(
        &mut self,
        _context: &LiveRecoveryContext,
        _receipt: &Self::Receipt,
        _operation: &Self::SettlementOperation,
    ) -> Result<LiveSettleResolution, Self::Error> {
        unreachable!("NoLiveRuntime cannot resolve a Live receipt")
    }
}

#[async_trait]
impl<C> StreamingToolAttemptPublisher<C> for NoPublisher
where
    C: StreamingToolChannels<Output = NoStreamingValue, Commit = NoStreamingValue>,
{
    type Published = NoStreamingValue;
    type PublicationOperation = NoStreamingValue;
    type Error = std::convert::Infallible;

    fn prepare(
        &mut self,
        _context: &StreamingPublishContext,
        _attempt: AcceptedStreamingToolAttempt<C>,
    ) -> Result<Self::PublicationOperation, Self::Error> {
        unreachable!("NoPublisher cannot prepare a disabled terminal lane")
    }

    async fn publish(
        &mut self,
        _context: &StreamingPublishContext,
        _operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<Self::Published, Self::Error> {
        unreachable!("NoPublisher cannot publish a disabled terminal lane")
    }

    async fn resolve(
        &mut self,
        _context: &StreamingPublishRecoveryContext,
        _operation: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<Self::Published>, Self::Error> {
        unreachable!("NoPublisher cannot resolve a disabled terminal lane")
    }
}

/// Sanitized internal failure. Adapter error payloads never cross this layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedEffectsFault {
    AttemptClosed,
    LiveDisabled,
    LiveFactory,
    LivePrepare,
    LiveNotApplied,
    PublisherDisabled,
    PublisherFactory,
    PublisherPrepare,
    PublishNotPublished,
    PublicationDisabledWithEntries,
    Invariant,
}

/// High-level progress visible to the contract machine and Application actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedEffectsStatus {
    Active,
    Accepted,
    Aborted,
    RecoveryRequired,
}

/// One diagnostic with the contract-attempt sequence allocated at its authored
/// position in a reducer update.
pub(crate) struct EffectDiagnostic<D> {
    pub(crate) sequence: u64,
    pub(crate) diagnostic: D,
}

/// Diagnostics removed from a reducer update without requiring a `Clone` bound.
pub(crate) struct EffectsUpdate<D> {
    diagnostics: Vec<EffectDiagnostic<D>>,
    status: ManagedEffectsStatus,
}

impl<D> EffectsUpdate<D> {
    pub(crate) fn into_diagnostics(self) -> Vec<EffectDiagnostic<D>> {
        self.diagnostics
    }

    pub(crate) const fn status(&self) -> ManagedEffectsStatus {
        self.status
    }
}

/// Factory retained across adapter instance recreation.
pub(crate) type LiveRuntimeFactory<R> =
    Box<dyn Fn(&StreamingToolAttemptStart) -> Result<R, ManagedEffectsFault> + Send + Sync>;

/// Factory retained across publisher instance recreation.
pub(crate) type PublisherFactory<P> =
    Box<dyn Fn(&StreamingToolAttemptStart) -> Result<P, ManagedEffectsFault> + Send + Sync>;

#[derive(Clone)]
struct LiveRecordIdentity {
    effect: StreamingEffectId,
    occurrence: Option<XmlOccurrenceId>,
}

struct PendingApply<R, Live>
where
    R: LiveEffectRuntime<Live>,
{
    identity: LiveRecordIdentity,
    operation: R::ApplyOperation,
}

enum ReceiptSettlement<R, Live>
where
    R: LiveEffectRuntime<Live>,
{
    Applied,
    PreparationNeeded,
    /// A stable operation was retained before an adapter future was polled.
    /// Recovery must resolve it rather than invoke the adapter operation again.
    Pending {
        settlement: LiveSettlement,
        cause: Option<StreamingToolAbortCause>,
        operation: R::SettlementOperation,
    },
    Retryable {
        settlement: LiveSettlement,
        cause: Option<StreamingToolAbortCause>,
        operation: R::SettlementOperation,
    },
    Settled,
}

struct ReceiptRecord<R, Live>
where
    R: LiveEffectRuntime<Live>,
{
    identity: LiveRecordIdentity,
    receipt: R::Receipt,
    settlement: ReceiptSettlement<R, Live>,
}

enum PublicationState<P, C>
where
    C: StreamingToolChannels,
    P: StreamingToolAttemptPublisher<C>,
{
    Open,
    Disabled,
    /// Publication evidence was retained before `publish` was polled.  This
    /// also represents a caught future panic or a dropped caller.
    Pending {
        publication: StreamingPublicationId,
        operation: P::PublicationOperation,
    },
    Published,
    NotPublished,
}

#[derive(Clone, Copy)]
enum CleanupIntent {
    None,
    Confirm,
    Rollback(StreamingToolAbortCause),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptDisposition {
    Open,
    Accepted,
    Aborted(StreamingToolAbortCause),
}

/// Generic owned interpreter for one contract's Output/Commit staging and Live
/// receipt table. It never spawns or borrows an Application; an outer actor may
/// move the complete value into a worker after handoff.
pub(crate) struct ManagedEffects<C, R, P>
where
    C: StreamingToolChannels,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    start: StreamingToolAttemptStart,
    live_factory: Option<LiveRuntimeFactory<R>>,
    publisher_factory: Option<PublisherFactory<P>>,
    live_runtime: Option<R>,
    publisher: Option<P>,
    next_sequence: u64,
    next_effect: u64,
    next_publication: u64,
    staged: Vec<StagedStreamingToolEmission<C>>,
    pending_apply: Option<PendingApply<R, C::Live>>,
    receipts: Vec<ReceiptRecord<R, C::Live>>,
    publication: PublicationState<P, C>,
    intent: CleanupIntent,
    disposition: AttemptDisposition,
}

impl<C, R, P> ManagedEffects<C, R, P>
where
    C: StreamingToolChannels,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    pub(crate) fn new(
        start: StreamingToolAttemptStart,
        live_factory: Option<LiveRuntimeFactory<R>>,
        publisher_factory: Option<PublisherFactory<P>>,
    ) -> Self {
        let publication = if publisher_factory.is_some() {
            PublicationState::Open
        } else {
            PublicationState::Disabled
        };
        Self {
            start,
            live_factory,
            publisher_factory,
            live_runtime: None,
            publisher: None,
            next_sequence: 0,
            next_effect: 0,
            next_publication: 0,
            staged: Vec::new(),
            pending_apply: None,
            receipts: Vec::new(),
            publication,
            intent: CleanupIntent::None,
            disposition: AttemptDisposition::Open,
        }
    }

    /// Consume one synchronous reducer update. Live apply calls are awaited in
    /// authored entry order; no later entry is interpreted after uncertainty.
    pub(crate) async fn handle_update(
        &mut self,
        origin: StreamingEmissionOrigin,
        update: StreamingToolUpdate<C>,
    ) -> Result<EffectsUpdate<C::Diagnostic>, ManagedEffectsFault> {
        self.ensure_open()?;
        if self.requires_recovery() {
            return Ok(EffectsUpdate {
                diagnostics: Vec::new(),
                status: ManagedEffectsStatus::RecoveryRequired,
            });
        }

        let mut diagnostics = Vec::new();
        for emission in update.into_entries() {
            let sequence = self.allocate_sequence()?;
            match emission {
                StreamingToolEmission::Output(value) => {
                    self.staged.push(StagedStreamingToolEmission::new(
                        sequence,
                        origin.clone(),
                        StagedStreamingToolValue::Output(value),
                    ))
                }
                StreamingToolEmission::Commit(value) => {
                    self.staged.push(StagedStreamingToolEmission::new(
                        sequence,
                        origin.clone(),
                        StagedStreamingToolValue::Commit(value),
                    ))
                }
                StreamingToolEmission::Diagnostic(diagnostic) => {
                    diagnostics.push(EffectDiagnostic {
                        sequence,
                        diagnostic,
                    });
                }
                StreamingToolEmission::Live(value) => {
                    match self.apply_live(sequence, origin.clone(), value).await? {
                        ManagedEffectsStatus::Active => {}
                        ManagedEffectsStatus::RecoveryRequired => {
                            return Ok(EffectsUpdate {
                                diagnostics,
                                status: ManagedEffectsStatus::RecoveryRequired,
                            });
                        }
                        ManagedEffectsStatus::Accepted | ManagedEffectsStatus::Aborted => {
                            return Err(ManagedEffectsFault::Invariant);
                        }
                    }
                }
            }
        }
        Ok(EffectsUpdate {
            diagnostics,
            status: self.status(),
        })
    }

    /// Remove one invalid occurrence's staged values and locally compensate
    /// its receipts. Any uncertainty escalates to a whole-attempt rollback.
    pub(crate) async fn invalidate_occurrence(
        &mut self,
        occurrence: XmlOccurrenceId,
    ) -> Result<ManagedEffectsStatus, ManagedEffectsFault> {
        self.ensure_open()?;
        if self.requires_recovery() {
            return Ok(ManagedEffectsStatus::RecoveryRequired);
        }

        self.staged
            .retain(|entry| entry.origin().occurrence() != Some(occurrence));
        let pending_apply = self
            .pending_apply
            .as_ref()
            .is_some_and(|pending| pending.identity.occurrence == Some(occurrence));
        if pending_apply {
            self.begin_abort(StreamingToolAbortCause::OccurrenceInvalidated);
            return self.drive_cleanup(false).await;
        }

        let local_clear = self
            .settle_matching(
                LiveSettlement::Rollback,
                Some(StreamingToolAbortCause::OccurrenceInvalidated),
                false,
                |record| record.identity.occurrence == Some(occurrence),
            )
            .await;
        if local_clear {
            return Ok(self.status());
        }

        self.begin_abort(StreamingToolAbortCause::OccurrenceInvalidated);
        self.drive_cleanup(false).await
    }

    /// Cross the accepted publication barrier and then confirm all Live
    /// receipts in apply order. A disabled publisher is a no-op barrier only
    /// when there are no staged Output or Commit values.
    pub(crate) async fn accept(
        &mut self,
        raw_output: String,
        diagnostics: Vec<StreamingToolDiagnosticRecord<C::Diagnostic>>,
    ) -> Result<ManagedEffectsStatus, ManagedEffectsFault> {
        self.ensure_open()?;
        if self.requires_recovery() {
            return Ok(ManagedEffectsStatus::RecoveryRequired);
        }
        self.disposition = AttemptDisposition::Accepted;

        if matches!(&self.publication, PublicationState::Disabled) {
            if !self.staged.is_empty() {
                self.begin_abort(StreamingToolAbortCause::RuntimeFault);
                let _ = self.drive_cleanup(false).await;
                return Err(ManagedEffectsFault::PublicationDisabledWithEntries);
            }
            self.intent = CleanupIntent::Confirm;
            return self.drive_cleanup(false).await;
        }
        if !matches!(&self.publication, PublicationState::Open) {
            return Ok(self.status());
        }

        let publication = self.allocate_publication_id()?;
        let context = StreamingPublishContext::new(&self.start, publication);
        let attempt = AcceptedStreamingToolAttempt::new(
            StreamingToolAttemptContext::from_start(&self.start),
            raw_output,
            std::mem::take(&mut self.staged),
            diagnostics,
        );
        let operation = match self.prepare_publication(&context, attempt) {
            Ok(operation) => operation,
            Err(fault) => {
                self.publication = PublicationState::NotPublished;
                self.begin_abort(StreamingToolAbortCause::PublicationNotPublished);
                let _ = self.drive_cleanup(false).await;
                return Err(fault);
            }
        };

        // The operation is deliberately stored before `publish` is polled.
        self.publication = PublicationState::Pending {
            publication,
            operation,
        };
        match self.publish_pending(&context).await {
            PublishCall::Published => {
                self.intent = CleanupIntent::Confirm;
                self.drive_cleanup(false).await
            }
            PublishCall::NotPublished => {
                self.begin_abort(StreamingToolAbortCause::PublicationNotPublished);
                let status = self.drive_cleanup(false).await?;
                if status == ManagedEffectsStatus::RecoveryRequired {
                    Ok(status)
                } else {
                    Err(ManagedEffectsFault::PublishNotPublished)
                }
            }
            PublishCall::Indeterminate => Ok(ManagedEffectsStatus::RecoveryRequired),
        }
    }

    /// Abort a not-published contract. A publication operation already in the
    /// ledger blocks rollback until recovery proves the publication outcome.
    pub(crate) async fn abort(
        &mut self,
        cause: StreamingToolAbortCause,
    ) -> Result<ManagedEffectsStatus, ManagedEffectsFault> {
        self.begin_abort(cause);
        self.drive_cleanup(false).await
    }

    /// Make one bounded recovery pass. Recovering an interrupted Live apply or
    /// settlement never resumes parsing; it selects whole-attempt rollback.
    pub(crate) async fn recover(&mut self) -> ManagedEffectsStatus {
        if self.pending_apply.is_some()
            || (self.has_unsettled_settlement() && matches!(self.intent, CleanupIntent::None))
        {
            self.ensure_abort(StreamingToolAbortCause::RuntimeFault);
        }
        if self.pending_apply.is_some() && !self.resolve_pending_apply().await {
            return ManagedEffectsStatus::RecoveryRequired;
        }
        if self.publication_pending() && !self.resolve_publication().await {
            return ManagedEffectsStatus::RecoveryRequired;
        }
        match self.drive_cleanup(true).await {
            Ok(status) => status,
            Err(_) => ManagedEffectsStatus::RecoveryRequired,
        }
    }

    pub(crate) fn is_clear(&self) -> bool {
        !self.requires_recovery()
    }

    pub(crate) fn status(&self) -> ManagedEffectsStatus {
        if self.requires_recovery() {
            return ManagedEffectsStatus::RecoveryRequired;
        }
        match self.disposition {
            AttemptDisposition::Open => ManagedEffectsStatus::Active,
            AttemptDisposition::Accepted => ManagedEffectsStatus::Accepted,
            AttemptDisposition::Aborted(_) => ManagedEffectsStatus::Aborted,
        }
    }

    /// The saved reason for an aborted disposition survives recovery until the
    /// outer supervisor has reported the terminal attempt result.
    pub(crate) fn abort_cause(&self) -> Option<StreamingToolAbortCause> {
        match self.disposition {
            AttemptDisposition::Aborted(cause) => Some(cause),
            AttemptDisposition::Open | AttemptDisposition::Accepted => None,
        }
    }

    fn ensure_open(&self) -> Result<(), ManagedEffectsFault> {
        match self.disposition {
            AttemptDisposition::Open => Ok(()),
            AttemptDisposition::Accepted | AttemptDisposition::Aborted(_) => {
                Err(ManagedEffectsFault::AttemptClosed)
            }
        }
    }

    /// Allocate from the contract-wide event order. The contract machine uses
    /// this same allocator for parser events and diagnostics.
    pub(crate) fn allocate_sequence(&mut self) -> Result<u64, ManagedEffectsFault> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ManagedEffectsFault::Invariant)?;
        Ok(sequence)
    }

    fn allocate_effect_id(&mut self) -> Result<StreamingEffectId, ManagedEffectsFault> {
        let value = self.next_effect;
        self.next_effect = self
            .next_effect
            .checked_add(1)
            .ok_or(ManagedEffectsFault::Invariant)?;
        Ok(StreamingEffectId::new(value))
    }

    fn allocate_publication_id(&mut self) -> Result<StreamingPublicationId, ManagedEffectsFault> {
        let value = self.next_publication;
        self.next_publication = self
            .next_publication
            .checked_add(1)
            .ok_or(ManagedEffectsFault::Invariant)?;
        Ok(StreamingPublicationId::new(value))
    }

    fn take_live_runtime(&mut self) -> Result<R, ManagedEffectsFault> {
        if let Some(runtime) = self.live_runtime.take() {
            return Ok(runtime);
        }
        let factory = self
            .live_factory
            .as_ref()
            .ok_or(ManagedEffectsFault::LiveDisabled)?;
        match catch_unwind(AssertUnwindSafe(|| factory(&self.start))) {
            Ok(Ok(runtime)) => Ok(runtime),
            Ok(Err(_)) | Err(_) => Err(ManagedEffectsFault::LiveFactory),
        }
    }

    fn restore_live_runtime(&mut self, runtime: R) {
        debug_assert!(self.live_runtime.is_none());
        self.live_runtime = Some(runtime);
    }

    fn take_publisher(&mut self) -> Result<P, ManagedEffectsFault> {
        if let Some(publisher) = self.publisher.take() {
            return Ok(publisher);
        }
        let factory = self
            .publisher_factory
            .as_ref()
            .ok_or(ManagedEffectsFault::PublisherDisabled)?;
        match catch_unwind(AssertUnwindSafe(|| factory(&self.start))) {
            Ok(Ok(publisher)) => Ok(publisher),
            Ok(Err(_)) | Err(_) => Err(ManagedEffectsFault::PublisherFactory),
        }
    }

    fn restore_publisher(&mut self, publisher: P) {
        debug_assert!(self.publisher.is_none());
        self.publisher = Some(publisher);
    }

    async fn apply_live(
        &mut self,
        sequence: u64,
        origin: StreamingEmissionOrigin,
        effect: C::Live,
    ) -> Result<ManagedEffectsStatus, ManagedEffectsFault> {
        let effect_id = self.allocate_effect_id()?;
        let identity = LiveRecordIdentity {
            effect: effect_id,
            occurrence: origin.occurrence(),
        };
        let context = LiveEffectContext::new(&self.start, effect_id, sequence, origin);
        let mut runtime = match self.take_live_runtime() {
            Ok(runtime) => runtime,
            Err(fault) => {
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                let _ = self.drive_cleanup(false).await;
                return Err(fault);
            }
        };
        let prepared = catch_unwind(AssertUnwindSafe(|| runtime.prepare_apply(&context, effect)));
        let operation = match prepared {
            Ok(Ok(operation)) => {
                self.restore_live_runtime(runtime);
                operation
            }
            Ok(Err(_)) => {
                self.restore_live_runtime(runtime);
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                let _ = self.drive_cleanup(false).await;
                return Err(ManagedEffectsFault::LivePrepare);
            }
            Err(_) => {
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                let _ = self.drive_cleanup(false).await;
                return Err(ManagedEffectsFault::LivePrepare);
            }
        };

        // Retain the operation before starting side-effecting work.
        self.pending_apply = Some(PendingApply {
            identity,
            operation,
        });
        let mut runtime = match self.take_live_runtime() {
            Ok(runtime) => runtime,
            Err(_) => {
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                return Ok(ManagedEffectsStatus::RecoveryRequired);
            }
        };
        let result = {
            let pending = self
                .pending_apply
                .as_ref()
                .expect("live apply operation must be retained");
            AssertUnwindSafe(runtime.apply(&context, &pending.operation))
                .catch_unwind()
                .await
        };
        match result {
            Ok(LiveApplyOutcome::Applied(receipt)) => {
                self.restore_live_runtime(runtime);
                let pending = self
                    .pending_apply
                    .take()
                    .expect("live apply operation must be retained");
                self.receipts.push(ReceiptRecord {
                    identity: pending.identity,
                    receipt,
                    settlement: ReceiptSettlement::Applied,
                });
                Ok(ManagedEffectsStatus::Active)
            }
            Ok(LiveApplyOutcome::NotApplied(_)) => {
                self.restore_live_runtime(runtime);
                self.pending_apply.take();
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                let _ = self.drive_cleanup(false).await;
                Err(ManagedEffectsFault::LiveNotApplied)
            }
            Ok(LiveApplyOutcome::Indeterminate) => {
                self.restore_live_runtime(runtime);
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                Ok(ManagedEffectsStatus::RecoveryRequired)
            }
            Err(_) => {
                self.begin_abort(StreamingToolAbortCause::LiveFault);
                Ok(ManagedEffectsStatus::RecoveryRequired)
            }
        }
    }

    fn prepare_publication(
        &mut self,
        context: &StreamingPublishContext,
        attempt: AcceptedStreamingToolAttempt<C>,
    ) -> Result<P::PublicationOperation, ManagedEffectsFault> {
        let mut publisher = self.take_publisher()?;
        match catch_unwind(AssertUnwindSafe(|| publisher.prepare(context, attempt))) {
            Ok(Ok(operation)) => {
                self.restore_publisher(publisher);
                Ok(operation)
            }
            Ok(Err(_)) => {
                self.restore_publisher(publisher);
                Err(ManagedEffectsFault::PublisherPrepare)
            }
            Err(_) => Err(ManagedEffectsFault::PublisherPrepare),
        }
    }

    async fn publish_pending(&mut self, context: &StreamingPublishContext) -> PublishCall {
        let mut publisher = match self.take_publisher() {
            Ok(publisher) => publisher,
            Err(_) => return PublishCall::Indeterminate,
        };
        let result = {
            let PublicationState::Pending { operation, .. } = &self.publication else {
                return PublishCall::Indeterminate;
            };
            AssertUnwindSafe(publisher.publish(context, operation))
                .catch_unwind()
                .await
        };
        match result {
            Ok(StreamingPublishOutcome::Published(_)) => {
                self.restore_publisher(publisher);
                self.publication = PublicationState::Published;
                PublishCall::Published
            }
            Ok(StreamingPublishOutcome::NotPublished(_)) => {
                self.restore_publisher(publisher);
                self.publication = PublicationState::NotPublished;
                PublishCall::NotPublished
            }
            Ok(StreamingPublishOutcome::Indeterminate) => {
                self.restore_publisher(publisher);
                PublishCall::Indeterminate
            }
            Err(_) => PublishCall::Indeterminate,
        }
    }

    fn begin_abort(&mut self, cause: StreamingToolAbortCause) {
        self.staged.clear();
        // A model rejection is a normal business disposition until an outer
        // cancellation or fault supersedes it. Once a terminal fault is
        // selected, later cleanup calls must not erase its original cause.
        if !matches!(
            self.disposition,
            AttemptDisposition::Aborted(existing)
                if existing != StreamingToolAbortCause::Rejected
        ) {
            self.disposition = AttemptDisposition::Aborted(cause);
        }
        let rollback_cause = self.abort_cause().unwrap_or(cause);
        self.intent = if matches!(&self.publication, PublicationState::Published) {
            CleanupIntent::Confirm
        } else {
            CleanupIntent::Rollback(rollback_cause)
        };
    }

    fn ensure_abort(&mut self, cause: StreamingToolAbortCause) {
        self.begin_abort(cause);
    }

    async fn resolve_pending_apply(&mut self) -> bool {
        let Some(pending) = self.pending_apply.as_ref() else {
            return true;
        };
        let context = LiveRecoveryContext::new(
            &self.start,
            pending.identity.effect,
            StreamingToolRecoveryPhase::ResolveLiveApply,
            Some(
                self.abort_cause()
                    .unwrap_or(StreamingToolAbortCause::RuntimeFault),
            ),
        );
        let mut runtime = match self.take_live_runtime() {
            Ok(runtime) => runtime,
            Err(_) => return false,
        };
        let result = {
            let pending = self
                .pending_apply
                .as_ref()
                .expect("pending apply must remain retained during recovery");
            AssertUnwindSafe(runtime.resolve_apply(&context, &pending.operation))
                .catch_unwind()
                .await
        };
        match result {
            Ok(Ok(LiveApplyResolution::Applied(receipt))) => {
                self.restore_live_runtime(runtime);
                let pending = self
                    .pending_apply
                    .take()
                    .expect("pending apply must remain retained during recovery");
                self.receipts.push(ReceiptRecord {
                    identity: pending.identity,
                    receipt,
                    settlement: ReceiptSettlement::Applied,
                });
                self.ensure_abort(StreamingToolAbortCause::RuntimeFault);
                true
            }
            Ok(Ok(LiveApplyResolution::NotApplied)) => {
                self.restore_live_runtime(runtime);
                self.pending_apply.take();
                self.ensure_abort(StreamingToolAbortCause::RuntimeFault);
                true
            }
            Ok(Ok(LiveApplyResolution::StillIndeterminate)) | Ok(Err(_)) => {
                self.restore_live_runtime(runtime);
                false
            }
            Err(_) => false,
        }
    }

    async fn resolve_publication(&mut self) -> bool {
        let publication = match &self.publication {
            PublicationState::Pending { publication, .. } => *publication,
            _ => return true,
        };
        let context = StreamingPublishRecoveryContext::new(&self.start, publication);
        let mut publisher = match self.take_publisher() {
            Ok(publisher) => publisher,
            Err(_) => return false,
        };
        let result = {
            let PublicationState::Pending { operation, .. } = &self.publication else {
                return false;
            };
            AssertUnwindSafe(publisher.resolve(&context, operation))
                .catch_unwind()
                .await
        };
        match result {
            Ok(Ok(StreamingPublishResolution::Published(_))) => {
                self.restore_publisher(publisher);
                self.publication = PublicationState::Published;
                self.intent = CleanupIntent::Confirm;
                true
            }
            Ok(Ok(StreamingPublishResolution::NotPublished)) => {
                self.restore_publisher(publisher);
                self.publication = PublicationState::NotPublished;
                self.begin_abort(StreamingToolAbortCause::PublicationNotPublished);
                true
            }
            Ok(Ok(StreamingPublishResolution::StillIndeterminate)) | Ok(Err(_)) => {
                self.restore_publisher(publisher);
                false
            }
            Err(_) => false,
        }
    }

    async fn drive_cleanup(
        &mut self,
        recovery: bool,
    ) -> Result<ManagedEffectsStatus, ManagedEffectsFault> {
        if self.pending_apply.is_some() || self.publication_pending() {
            return Ok(ManagedEffectsStatus::RecoveryRequired);
        }
        let (settlement, cause) = match self.intent {
            CleanupIntent::None => return Ok(self.status()),
            CleanupIntent::Confirm => (LiveSettlement::Confirm, None),
            CleanupIntent::Rollback(cause) => (LiveSettlement::Rollback, Some(cause)),
        };
        if recovery {
            self.resolve_pending_settlements(settlement).await;
        }
        let clear = self
            .settle_matching(settlement, cause, recovery, |_| true)
            .await;
        if clear {
            self.intent = CleanupIntent::None;
            Ok(self.status())
        } else {
            Ok(ManagedEffectsStatus::RecoveryRequired)
        }
    }

    async fn resolve_pending_settlements(&mut self, settlement: LiveSettlement) {
        let mut indexes = self
            .receipts
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                matches!(
                    &record.settlement,
                    ReceiptSettlement::Pending {
                        settlement: stored,
                        ..
                    } if *stored == settlement
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if settlement == LiveSettlement::Rollback {
            indexes.reverse();
        }
        for index in indexes {
            let _ = self.resolve_pending_settlement(index).await;
        }
    }

    async fn resolve_pending_settlement(&mut self, index: usize) -> bool {
        let Some(record) = self.receipts.get(index) else {
            return false;
        };
        let (effect, settlement, cause) = match &record.settlement {
            ReceiptSettlement::Pending {
                settlement, cause, ..
            } => (record.identity.effect, *settlement, *cause),
            _ => return true,
        };
        let phase = match settlement {
            LiveSettlement::Confirm => StreamingToolRecoveryPhase::ConfirmLive,
            LiveSettlement::Rollback => StreamingToolRecoveryPhase::RollbackLive,
        };
        let context = LiveRecoveryContext::new(&self.start, effect, phase, cause);
        let mut runtime = match self.take_live_runtime() {
            Ok(runtime) => runtime,
            Err(_) => return false,
        };
        let result = {
            let record = &self.receipts[index];
            let ReceiptSettlement::Pending { operation, .. } = &record.settlement else {
                return false;
            };
            AssertUnwindSafe(runtime.resolve_settlement(&context, &record.receipt, operation))
                .catch_unwind()
                .await
        };
        match result {
            Ok(Ok(LiveSettleResolution::Settled)) => {
                self.restore_live_runtime(runtime);
                self.receipts[index].settlement = ReceiptSettlement::Settled;
                true
            }
            Ok(Ok(LiveSettleResolution::NotSettled)) => {
                self.restore_live_runtime(runtime);
                self.pending_to_retryable(index)
            }
            Ok(Ok(LiveSettleResolution::StillIndeterminate)) | Ok(Err(_)) => {
                self.restore_live_runtime(runtime);
                false
            }
            Err(_) => false,
        }
    }

    async fn settle_matching(
        &mut self,
        settlement: LiveSettlement,
        cause: Option<StreamingToolAbortCause>,
        recovery: bool,
        mut matches: impl FnMut(&ReceiptRecord<R, C::Live>) -> bool,
    ) -> bool {
        let mut indexes = self
            .receipts
            .iter()
            .enumerate()
            .filter_map(|(index, record)| matches(record).then_some(index))
            .collect::<Vec<_>>();
        if settlement == LiveSettlement::Rollback {
            indexes.reverse();
        }
        let mut clear = true;
        for index in indexes {
            if !self.settle_one(index, settlement, cause, recovery).await {
                clear = false;
            }
        }
        clear
    }

    async fn settle_one(
        &mut self,
        index: usize,
        settlement: LiveSettlement,
        cause: Option<StreamingToolAbortCause>,
        recovery: bool,
    ) -> bool {
        let Some(record) = self.receipts.get(index) else {
            return false;
        };
        match &record.settlement {
            ReceiptSettlement::Settled => return true,
            ReceiptSettlement::Pending { .. } => return false,
            ReceiptSettlement::Retryable {
                settlement: stored, ..
            } if *stored != settlement || !recovery => return false,
            ReceiptSettlement::Applied => {}
            ReceiptSettlement::PreparationNeeded if !recovery => return false,
            ReceiptSettlement::PreparationNeeded | ReceiptSettlement::Retryable { .. } => {}
        }

        let needs_prepare = matches!(
            &self.receipts[index].settlement,
            ReceiptSettlement::Applied | ReceiptSettlement::PreparationNeeded
        );
        if needs_prepare {
            let effect = self.receipts[index].identity.effect;
            let context = LiveSettlementContext::new(&self.start, effect, settlement, cause);
            let mut runtime = match self.take_live_runtime() {
                Ok(runtime) => runtime,
                Err(_) => {
                    self.receipts[index].settlement = ReceiptSettlement::PreparationNeeded;
                    return false;
                }
            };
            let prepared = {
                let receipt = &self.receipts[index].receipt;
                catch_unwind(AssertUnwindSafe(|| {
                    runtime.prepare_settlement(&context, receipt)
                }))
            };
            match prepared {
                Ok(Ok(operation)) => {
                    self.restore_live_runtime(runtime);
                    self.receipts[index].settlement = ReceiptSettlement::Pending {
                        settlement,
                        cause,
                        operation,
                    };
                }
                Ok(Err(_)) => {
                    self.restore_live_runtime(runtime);
                    self.receipts[index].settlement = ReceiptSettlement::PreparationNeeded;
                    return false;
                }
                Err(_) => {
                    self.receipts[index].settlement = ReceiptSettlement::PreparationNeeded;
                    return false;
                }
            }
        } else {
            let previous = std::mem::replace(
                &mut self.receipts[index].settlement,
                ReceiptSettlement::PreparationNeeded,
            );
            match previous {
                ReceiptSettlement::Retryable {
                    settlement: stored,
                    cause,
                    operation,
                } if stored == settlement => {
                    self.receipts[index].settlement = ReceiptSettlement::Pending {
                        settlement: stored,
                        cause,
                        operation,
                    };
                }
                other => {
                    self.receipts[index].settlement = other;
                    return false;
                }
            }
        }
        self.call_pending_settlement(index).await
    }

    async fn call_pending_settlement(&mut self, index: usize) -> bool {
        let Some(record) = self.receipts.get(index) else {
            return false;
        };
        let (effect, settlement, cause) = match &record.settlement {
            ReceiptSettlement::Pending {
                settlement, cause, ..
            } => (record.identity.effect, *settlement, *cause),
            _ => return false,
        };
        let mut runtime = match self.take_live_runtime() {
            Ok(runtime) => runtime,
            Err(_) => return false,
        };
        let result = {
            let record = &self.receipts[index];
            let ReceiptSettlement::Pending { operation, .. } = &record.settlement else {
                return false;
            };
            match settlement {
                LiveSettlement::Confirm => {
                    let context = LiveConfirmContext::new(&self.start, effect);
                    AssertUnwindSafe(runtime.confirm(&context, &record.receipt, operation))
                        .catch_unwind()
                        .await
                }
                LiveSettlement::Rollback => {
                    let context = LiveRollbackContext::new(
                        &self.start,
                        effect,
                        cause.unwrap_or(StreamingToolAbortCause::RuntimeFault),
                    );
                    AssertUnwindSafe(runtime.rollback(&context, &record.receipt, operation))
                        .catch_unwind()
                        .await
                }
            }
        };
        match result {
            Ok(LiveSettleOutcome::Settled) => {
                self.restore_live_runtime(runtime);
                self.receipts[index].settlement = ReceiptSettlement::Settled;
                true
            }
            Ok(LiveSettleOutcome::NotSettled(_)) => {
                self.restore_live_runtime(runtime);
                self.pending_to_retryable(index)
            }
            Ok(LiveSettleOutcome::Indeterminate) => {
                self.restore_live_runtime(runtime);
                false
            }
            Err(_) => false,
        }
    }

    fn pending_to_retryable(&mut self, index: usize) -> bool {
        let Some(record) = self.receipts.get_mut(index) else {
            return false;
        };
        let previous =
            std::mem::replace(&mut record.settlement, ReceiptSettlement::PreparationNeeded);
        match previous {
            ReceiptSettlement::Pending {
                settlement,
                cause,
                operation,
            } => {
                record.settlement = ReceiptSettlement::Retryable {
                    settlement,
                    cause,
                    operation,
                };
                true
            }
            other => {
                record.settlement = other;
                false
            }
        }
    }

    fn publication_pending(&self) -> bool {
        matches!(&self.publication, PublicationState::Pending { .. })
    }

    fn has_unsettled_settlement(&self) -> bool {
        self.receipts.iter().any(|record| {
            matches!(
                &record.settlement,
                ReceiptSettlement::PreparationNeeded
                    | ReceiptSettlement::Pending { .. }
                    | ReceiptSettlement::Retryable { .. }
            )
        })
    }

    fn requires_recovery(&self) -> bool {
        self.pending_apply.is_some()
            || self.publication_pending()
            || self.has_unsettled_settlement()
    }
}

enum PublishCall {
    Published,
    NotPublished,
    Indeterminate,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use super::*;

    struct LiveOnlyChannels;

    impl StreamingToolChannels for LiveOnlyChannels {
        type Output = NoStreamingValue;
        type Live = u8;
        type Commit = NoStreamingValue;
        type Diagnostic = ();
    }

    struct PublishedChannels;

    impl StreamingToolChannels for PublishedChannels {
        type Output = String;
        type Live = u8;
        type Commit = NoStreamingValue;
        type Diagnostic = ();
    }

    fn start() -> StreamingToolAttemptStart {
        StreamingToolAttemptStart::new(StreamingToolAttemptId::new(1), "effects-test", "v1")
    }

    #[derive(Clone, Copy)]
    enum ApplyStep {
        Applied,
        Indeterminate,
        Panic,
    }

    #[derive(Clone, Copy)]
    enum ResolveApplyStep {
        Applied,
    }

    #[derive(Clone, Copy)]
    enum SettlementStep {
        Settled,
        NotSettled,
        Indeterminate,
    }

    #[derive(Clone, Copy)]
    enum ResolveSettlementStep {
        Settled,
    }

    #[derive(Default)]
    struct RuntimeState {
        apply: VecDeque<ApplyStep>,
        resolve_apply: VecDeque<ResolveApplyStep>,
        settlement: HashMap<(bool, u8), VecDeque<SettlementStep>>,
        resolve_settlement: HashMap<u8, VecDeque<ResolveSettlementStep>>,
        applied: Vec<u8>,
        resolved_apply: Vec<u8>,
        confirmed: Vec<u8>,
        rolled_back: Vec<u8>,
        resolved_settlement: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct RuntimeProbe(Arc<Mutex<RuntimeState>>);

    impl RuntimeProbe {
        fn state(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
            self.0.lock().unwrap()
        }
    }

    struct Runtime {
        probe: RuntimeProbe,
    }

    impl Runtime {
        fn settle(&self, receipt: u8, rollback: bool) -> LiveSettleOutcome<io::Error> {
            let step = {
                let mut state = self.probe.state();
                if rollback {
                    state.rolled_back.push(receipt);
                } else {
                    state.confirmed.push(receipt);
                }
                state
                    .settlement
                    .get_mut(&(rollback, receipt))
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(SettlementStep::Settled)
            };
            match step {
                SettlementStep::Settled => LiveSettleOutcome::Settled,
                SettlementStep::NotSettled => {
                    LiveSettleOutcome::NotSettled(io::Error::other("not settled"))
                }
                SettlementStep::Indeterminate => LiveSettleOutcome::Indeterminate,
            }
        }
    }

    #[async_trait]
    impl LiveEffectRuntime<u8> for Runtime {
        type Receipt = u8;
        type ApplyOperation = u8;
        type SettlementOperation = u8;
        type Error = io::Error;

        fn prepare_apply(
            &mut self,
            _context: &LiveEffectContext,
            effect: u8,
        ) -> Result<Self::ApplyOperation, Self::Error> {
            Ok(effect)
        }

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            operation: &Self::ApplyOperation,
        ) -> LiveApplyOutcome<Self::Receipt, Self::Error> {
            let step = {
                let mut state = self.probe.state();
                state.applied.push(*operation);
                state.apply.pop_front().unwrap_or(ApplyStep::Applied)
            };
            match step {
                ApplyStep::Applied => LiveApplyOutcome::Applied(*operation),
                ApplyStep::Indeterminate => LiveApplyOutcome::Indeterminate,
                ApplyStep::Panic => panic!("test apply panic"),
            }
        }

        async fn resolve_apply(
            &mut self,
            _context: &LiveRecoveryContext,
            operation: &Self::ApplyOperation,
        ) -> Result<LiveApplyResolution<Self::Receipt>, Self::Error> {
            let step = {
                let mut state = self.probe.state();
                state.resolved_apply.push(*operation);
                state
                    .resolve_apply
                    .pop_front()
                    .unwrap_or(ResolveApplyStep::Applied)
            };
            Ok(match step {
                ResolveApplyStep::Applied => LiveApplyResolution::Applied(*operation),
            })
        }

        fn prepare_settlement(
            &mut self,
            _context: &LiveSettlementContext,
            receipt: &Self::Receipt,
        ) -> Result<Self::SettlementOperation, Self::Error> {
            Ok(*receipt)
        }

        async fn confirm(
            &mut self,
            _context: &LiveConfirmContext,
            receipt: &Self::Receipt,
            _operation: &Self::SettlementOperation,
        ) -> LiveSettleOutcome<Self::Error> {
            self.settle(*receipt, false)
        }

        async fn rollback(
            &mut self,
            _context: &LiveRollbackContext,
            receipt: &Self::Receipt,
            _operation: &Self::SettlementOperation,
        ) -> LiveSettleOutcome<Self::Error> {
            self.settle(*receipt, true)
        }

        async fn resolve_settlement(
            &mut self,
            _context: &LiveRecoveryContext,
            receipt: &Self::Receipt,
            _operation: &Self::SettlementOperation,
        ) -> Result<LiveSettleResolution, Self::Error> {
            let step = {
                let mut state = self.probe.state();
                state.resolved_settlement.push(*receipt);
                state
                    .resolve_settlement
                    .get_mut(receipt)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(ResolveSettlementStep::Settled)
            };
            Ok(match step {
                ResolveSettlementStep::Settled => LiveSettleResolution::Settled,
            })
        }
    }

    fn live_only_effects(
        probe: RuntimeProbe,
    ) -> ManagedEffects<LiveOnlyChannels, Runtime, NoPublisher> {
        ManagedEffects::new(
            start(),
            Some(Box::new(move |_| {
                Ok(Runtime {
                    probe: probe.clone(),
                })
            })),
            None,
        )
    }

    #[derive(Clone, Copy)]
    enum PublishStep {
        Published,
        NotPublished,
        Indeterminate,
    }

    #[derive(Default)]
    struct PublisherState {
        publish: VecDeque<PublishStep>,
        resolve: VecDeque<PublishStep>,
        published: usize,
        resolved: usize,
    }

    #[derive(Clone, Default)]
    struct PublisherProbe(Arc<Mutex<PublisherState>>);

    struct Publisher {
        probe: PublisherProbe,
    }

    #[async_trait]
    impl StreamingToolAttemptPublisher<PublishedChannels> for Publisher {
        type Published = ();
        type PublicationOperation = AcceptedStreamingToolAttempt<PublishedChannels>;
        type Error = io::Error;

        fn prepare(
            &mut self,
            _context: &StreamingPublishContext,
            attempt: AcceptedStreamingToolAttempt<PublishedChannels>,
        ) -> Result<Self::PublicationOperation, Self::Error> {
            Ok(attempt)
        }

        async fn publish(
            &mut self,
            _context: &StreamingPublishContext,
            _operation: &Self::PublicationOperation,
        ) -> StreamingPublishOutcome<Self::Published, Self::Error> {
            let step = {
                let mut state = self.probe.0.lock().unwrap();
                state.published += 1;
                state.publish.pop_front().unwrap_or(PublishStep::Published)
            };
            match step {
                PublishStep::Published => StreamingPublishOutcome::Published(()),
                PublishStep::NotPublished => {
                    StreamingPublishOutcome::NotPublished(io::Error::other("not published"))
                }
                PublishStep::Indeterminate => StreamingPublishOutcome::Indeterminate,
            }
        }

        async fn resolve(
            &mut self,
            _context: &StreamingPublishRecoveryContext,
            _operation: &Self::PublicationOperation,
        ) -> Result<StreamingPublishResolution<Self::Published>, Self::Error> {
            let step = {
                let mut state = self.probe.0.lock().unwrap();
                state.resolved += 1;
                state.resolve.pop_front().unwrap_or(PublishStep::Published)
            };
            Ok(match step {
                PublishStep::Published => StreamingPublishResolution::Published(()),
                PublishStep::NotPublished => StreamingPublishResolution::NotPublished,
                PublishStep::Indeterminate => StreamingPublishResolution::StillIndeterminate,
            })
        }
    }

    fn published_effects(
        runtime: RuntimeProbe,
        publisher: PublisherProbe,
    ) -> ManagedEffects<PublishedChannels, Runtime, Publisher> {
        ManagedEffects::new(
            start(),
            Some(Box::new(move |_| {
                Ok(Runtime {
                    probe: runtime.clone(),
                })
            })),
            Some(Box::new(move |_| {
                Ok(Publisher {
                    probe: publisher.clone(),
                })
            })),
        )
    }

    #[tokio::test]
    async fn rollback_continues_in_reverse_order_after_multiple_uncertain_receipts() {
        let probe = RuntimeProbe::default();
        {
            let mut state = probe.state();
            state
                .settlement
                .insert((true, 4), VecDeque::from([SettlementStep::Indeterminate]));
            state.settlement.insert(
                (true, 3),
                VecDeque::from([SettlementStep::NotSettled, SettlementStep::Settled]),
            );
            state
                .settlement
                .insert((true, 2), VecDeque::from([SettlementStep::Indeterminate]));
        }
        let mut effects = live_only_effects(probe.clone());
        let update = StreamingToolUpdate::live(1)
            .with_live(2)
            .with_live(3)
            .with_live(4);
        effects
            .handle_update(StreamingEmissionOrigin::Finish, update)
            .await
            .unwrap();

        assert_eq!(
            effects
                .abort(StreamingToolAbortCause::Rejected)
                .await
                .unwrap(),
            ManagedEffectsStatus::RecoveryRequired
        );
        assert_eq!(probe.state().rolled_back, vec![4, 3, 2, 1]);

        assert_eq!(effects.recover().await, ManagedEffectsStatus::Aborted);
        assert_eq!(
            effects.abort_cause(),
            Some(StreamingToolAbortCause::Rejected)
        );
        let state = probe.state();
        assert_eq!(state.resolved_settlement, vec![4, 2]);
        assert_eq!(state.rolled_back, vec![4, 3, 2, 1, 3]);
    }

    #[tokio::test]
    async fn recovered_indeterminate_apply_is_rolled_back_without_resuming() {
        let probe = RuntimeProbe::default();
        {
            let mut state = probe.state();
            state.apply.push_back(ApplyStep::Indeterminate);
            state.resolve_apply.push_back(ResolveApplyStep::Applied);
        }
        let mut effects = live_only_effects(probe.clone());
        let update = effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::live(7),
            )
            .await
            .unwrap();
        assert_eq!(update.status(), ManagedEffectsStatus::RecoveryRequired);

        assert_eq!(effects.recover().await, ManagedEffectsStatus::Aborted);
        assert_eq!(
            effects.abort_cause(),
            Some(StreamingToolAbortCause::LiveFault)
        );
        let state = probe.state();
        assert_eq!(state.resolved_apply, vec![7]);
        assert_eq!(state.rolled_back, vec![7]);
    }

    #[tokio::test]
    async fn indeterminate_publication_blocks_rollback_until_it_resolves_published() {
        let runtime = RuntimeProbe::default();
        let publisher = PublisherProbe::default();
        {
            let mut state = publisher.0.lock().unwrap();
            state.publish.push_back(PublishStep::Indeterminate);
            state.resolve.push_back(PublishStep::Published);
        }
        let mut effects = published_effects(runtime.clone(), publisher.clone());
        effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::live(1),
            )
            .await
            .unwrap();
        effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::output("accepted".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(
            effects
                .accept("<tool/>".to_owned(), Vec::new())
                .await
                .unwrap(),
            ManagedEffectsStatus::RecoveryRequired
        );
        assert!(runtime.state().rolled_back.is_empty());
        assert!(runtime.state().confirmed.is_empty());

        assert_eq!(effects.recover().await, ManagedEffectsStatus::Accepted);
        assert_eq!(runtime.state().confirmed, vec![1]);
        assert!(runtime.state().rolled_back.is_empty());
        assert_eq!(publisher.0.lock().unwrap().resolved, 1);
    }

    #[tokio::test]
    async fn publication_resolving_not_published_rolls_back_live_receipts() {
        let runtime = RuntimeProbe::default();
        let publisher = PublisherProbe::default();
        {
            let mut state = publisher.0.lock().unwrap();
            state.publish.push_back(PublishStep::Indeterminate);
            state.resolve.push_back(PublishStep::NotPublished);
        }
        let mut effects = published_effects(runtime.clone(), publisher);
        effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::live(1),
            )
            .await
            .unwrap();
        effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::output("accepted".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(
            effects
                .accept("<tool/>".to_owned(), Vec::new())
                .await
                .unwrap(),
            ManagedEffectsStatus::RecoveryRequired
        );
        assert_eq!(effects.recover().await, ManagedEffectsStatus::Aborted);
        assert_eq!(
            effects.abort_cause(),
            Some(StreamingToolAbortCause::PublicationNotPublished)
        );
        assert_eq!(runtime.state().rolled_back, vec![1]);
        assert!(runtime.state().confirmed.is_empty());
    }

    #[tokio::test]
    async fn no_publication_values_cross_the_noop_barrier_and_confirm_live_receipts() {
        let probe = RuntimeProbe::default();
        let mut effects = live_only_effects(probe.clone());
        effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::live(1),
            )
            .await
            .unwrap();

        assert_eq!(
            effects.accept("".to_owned(), Vec::new()).await.unwrap(),
            ManagedEffectsStatus::Accepted
        );
        assert_eq!(probe.state().confirmed, vec![1]);
    }

    #[tokio::test]
    async fn factory_panic_after_apply_evidence_keeps_that_evidence_for_later_recovery() {
        let probe = RuntimeProbe::default();
        probe.state().apply.push_back(ApplyStep::Panic);
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let factory_calls_for_factory = factory_calls.clone();
        let factory_probe = probe.clone();
        let mut effects = ManagedEffects::<LiveOnlyChannels, Runtime, NoPublisher>::new(
            start(),
            Some(Box::new(move |_| {
                let call = factory_calls_for_factory.fetch_add(1, Ordering::SeqCst);
                if call == 1 {
                    panic!("test runtime factory panic");
                }
                Ok(Runtime {
                    probe: factory_probe.clone(),
                })
            })),
            None,
        );

        let update = effects
            .handle_update(
                StreamingEmissionOrigin::Finish,
                StreamingToolUpdate::live(6),
            )
            .await
            .unwrap();
        assert_eq!(update.status(), ManagedEffectsStatus::RecoveryRequired);
        assert!(effects.pending_apply.is_some());

        assert_eq!(
            effects.recover().await,
            ManagedEffectsStatus::RecoveryRequired
        );
        assert!(effects.pending_apply.is_some());
        assert_eq!(factory_calls.load(Ordering::SeqCst), 2);

        assert_eq!(effects.recover().await, ManagedEffectsStatus::Aborted);
        let state = probe.state();
        assert_eq!(state.resolved_apply, vec![6]);
        assert_eq!(state.rolled_back, vec![6]);
    }
}
