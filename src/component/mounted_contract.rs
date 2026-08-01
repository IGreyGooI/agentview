//! Public value contracts for the future mounted call owner.
//!
//! These types keep typed attempt results and pure session reduction separate
//! from the private Active/Finished/publication actors.

use std::{
    error::Error,
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{agent::TurnFlow, agent_session::AgentSession, llm_call::ExecutorCommit};

use super::{
    attempt_driver::AttemptUpdateInterpreter, managed_attempt::FinishedAttemptRecord,
    DurableCallId, DurableCallInputId, DurableCallLeaseId, DurableSessionId, EpochContractId,
    MountedProviderRequest, ProviderAttemptIdentity, ProviderToolResult, ProviderTurnCursor,
    PublicationCandidateFingerprint, PublicationId, PublicationReceipt, PublicationRequestId,
    StreamUpdate, TurnChannels, TurnEmission,
};

/// Store-issued admission retained while one owner may execute a provider turn.
///
/// The deadline is diagnostic data expressed in Unix milliseconds. Only the
/// authoritative store decides whether it has elapsed; mounted owners must not
/// use their process clock to steal or extend a lease.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableCallLease {
    lease_id: DurableCallLeaseId,
    request_id: PublicationRequestId,
    expires_at_unix_ms: u64,
}

impl DurableCallLease {
    pub fn new(
        lease_id: DurableCallLeaseId,
        request_id: PublicationRequestId,
        expires_at_unix_ms: u64,
    ) -> Self {
        Self {
            lease_id,
            request_id,
            expires_at_unix_ms,
        }
    }

    pub fn lease_id(&self) -> &DurableCallLeaseId {
        &self.lease_id
    }

    pub fn request_id(&self) -> &PublicationRequestId {
        &self.request_id
    }

    pub fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

/// Store-issued fence held while a host reconciles one recovery-required
/// provider operation.
///
/// This is distinct from [`DurableCallLease`]: the latter identifies the
/// unresolved provider operation, while this fence serializes recovery
/// inspection across owner replacement. Only the authoritative store decides
/// whether the fence has expired.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MountedCallReconciliationFence {
    lease_id: DurableCallLeaseId,
    expires_at_unix_ms: u64,
}

impl MountedCallReconciliationFence {
    pub(crate) fn new(lease_id: DurableCallLeaseId, expires_at_unix_ms: u64) -> Self {
        Self {
            lease_id,
            expires_at_unix_ms,
        }
    }

    pub(crate) fn is_live_at(&self, unix_ms: u64) -> bool {
        unix_ms < self.expires_at_unix_ms
    }
}

/// Persisted identity of a provider-complete publication candidate.
///
/// This is an advanced persistence value, deliberately crate-private while
/// the mounted public facade is still being proven. It is written before the
/// first publication attempt, then removed atomically with the accepted
/// session mutation and outbox. A reopened owner can therefore resolve the
/// exact `(request_id, fingerprint)` without recreating provider work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClaimedCallPublication {
    session_id: DurableSessionId,
    epoch_contract_id: EpochContractId,
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    turn_index: u64,
    lease: DurableCallLease,
    fingerprint: PublicationCandidateFingerprint,
}

impl ClaimedCallPublication {
    pub(crate) fn new(
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        turn_index: u64,
        lease: DurableCallLease,
        fingerprint: PublicationCandidateFingerprint,
    ) -> Self {
        Self {
            session_id,
            epoch_contract_id,
            call_id,
            input_id,
            turn_index,
            lease,
            fingerprint,
        }
    }

    pub(crate) fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub(crate) fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub(crate) fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub(crate) fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    pub(crate) fn turn_index(&self) -> u64 {
        self.turn_index
    }

    pub(crate) fn lease(&self) -> &DurableCallLease {
        &self.lease
    }

    pub(crate) fn request_id(&self) -> &PublicationRequestId {
        self.lease.request_id()
    }

    pub(crate) fn fingerprint(&self) -> &PublicationCandidateFingerprint {
        &self.fingerprint
    }
}

/// Why a durable call cannot be replayed automatically after losing its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum DurableCallRecoveryReason {
    /// The admission lease elapsed after provider execution may have started.
    LeaseExpired,
    /// A provider-complete publication candidate was authoritatively absent.
    ///
    /// The provider, tool, or Live boundary may already have run, so the
    /// candidate cannot be recreated by rerunning the model.
    PublicationNotCommitted,
    /// The durable request id was found with a different candidate fingerprint.
    ///
    /// This is a data-integrity boundary, not a condition that can be retried
    /// by starting a second provider attempt.
    PublicationCandidateCollision,
    /// The provider joined cancellation without proving that its remote cursor
    /// is unchanged or supplying an authoritative cursor for a successor.
    CancellationIndeterminate,
    /// The provider or Live cleanup did not acknowledge before the mounted
    /// owner reached its cancellation boundary.
    CancellationCleanupUnacknowledged,
    /// The provider did not join before the host cancellation grace elapsed.
    CancellationProviderJoinTimedOut,
    /// A provider-complete publication candidate was present while a
    /// cancellation settlement attempted to retire the same Running call.
    CancellationPendingPublication,
    /// The active durable System epoch no longer matched the cancelling owner.
    CancellationEpochMismatch,
    /// The persisted provider cursor or cancellation-safe successor cursor did
    /// not validate against the active durable epoch.
    CancellationCursorInvalid,
    /// The cancelling owner no longer held the expected session revision even
    /// though its exact Running lease was still retained.
    CancellationSettlementConflict,
    /// A provider-complete result failed in the host session reducer without a
    /// deterministic semantic-rejection classification.
    SessionReductionIndeterminate,
}

/// Stable boundary restored when a pre-provider reservation is abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum DurableCallReservationOrigin {
    /// No checkpoint existed before this call was admitted.
    New,
    /// The previous committed turn requested another provider turn.
    AwaitingContinuation,
}

/// Durable lifecycle of one logical call retained in the session call ledger.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum DurableCallStatus {
    /// One owner may perform pure User capture/render/context preparation.
    ///
    /// No provider, tool, or Live boundary has been crossed. Expiry or an
    /// explicit lease-fenced abandon restores `origin`; it must not require
    /// model recovery.
    Reserved {
        lease: DurableCallLease,
        origin: DurableCallReservationOrigin,
    },
    /// One store-claimed owner may execute `next_turn_index`.
    ///
    /// `origin` survives the provider boundary so a later remote
    /// `NeverAccepted` proof can restore the exact pre-provider checkpoint.
    /// A missing value represents a legacy persisted record whose origin was
    /// not retained; a recovery controller must leave that record fenced.
    Running {
        lease: DurableCallLease,
        #[serde(default)]
        origin: Option<DurableCallReservationOrigin>,
    },
    /// The previous committed turn requested another turn at `next_turn_index`.
    AwaitingContinuation,
    /// The logical call reached `TurnFlow::Wait`.
    Settled,
    /// The host durably paused the call and may explicitly resume it.
    Paused,
    /// A host explicitly stopped the call before its next turn.
    Stopped,
    /// Execution may have crossed the provider boundary and needs explicit
    /// host reconciliation before it can continue.
    RecoveryRequired {
        lease: DurableCallLease,
        /// The retained pre-provider checkpoint, when this record was written
        /// by an origin-aware mounted owner. Legacy records remain ambiguous.
        #[serde(default)]
        origin: Option<DurableCallReservationOrigin>,
        reason: DurableCallRecoveryReason,
    },
}

impl DurableCallStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Reserved { .. }
                | Self::Running { .. }
                | Self::AwaitingContinuation
                | Self::Paused
                | Self::RecoveryRequired { .. }
        )
    }

    pub fn lease(&self) -> Option<&DurableCallLease> {
        match self {
            Self::Reserved { lease, .. }
            | Self::Running { lease, .. }
            | Self::RecoveryRequired { lease, .. } => Some(lease),
            Self::AwaitingContinuation | Self::Settled | Self::Paused | Self::Stopped => None,
        }
    }
}

/// Restart-stable cursor for one logical mounted call.
///
/// Stable-boundary checkpoints are persisted with accepted session mutations.
/// `Reserved` is installed before User capture. The same lease is promoted to
/// `Running` only after final context preparation and attempt initialization,
/// immediately before provider spawn. Publication must present that unchanged
/// Running lease and atomically replace it with `AwaitingContinuation` or
/// `Settled`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableCallState {
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    epoch_contract_id: EpochContractId,
    next_turn_index: u64,
    last_request_id: Option<PublicationRequestId>,
    /// A temporary host-owned fence that serializes remote operation
    /// reconciliation. It is meaningful only for `RecoveryRequired` and is
    /// intentionally private to the persistence/controller boundary.
    #[serde(default)]
    reconciliation: Option<MountedCallReconciliationFence>,
    status: DurableCallStatus,
}

impl DurableCallState {
    fn from_parts(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: Option<PublicationRequestId>,
        status: DurableCallStatus,
    ) -> Self {
        Self {
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            last_request_id,
            reconciliation: None,
            status,
        }
    }

    pub fn reserved(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: Option<PublicationRequestId>,
        lease: DurableCallLease,
        origin: DurableCallReservationOrigin,
    ) -> Self {
        Self::from_parts(
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            last_request_id,
            DurableCallStatus::Reserved { lease, origin },
        )
    }

    pub fn running(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: Option<PublicationRequestId>,
        lease: DurableCallLease,
        origin: DurableCallReservationOrigin,
    ) -> Self {
        Self::from_parts(
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            last_request_id,
            DurableCallStatus::Running {
                lease,
                origin: Some(origin),
            },
        )
    }

    pub fn awaiting_continuation(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: PublicationRequestId,
    ) -> Self {
        Self::checkpoint(
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            last_request_id,
            DurableCallStatus::AwaitingContinuation,
        )
    }

    pub fn settled(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: PublicationRequestId,
    ) -> Self {
        Self::checkpoint(
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            last_request_id,
            DurableCallStatus::Settled,
        )
    }

    /// Retire a provider-started logical call after its cancellation has been
    /// durably settled. The prior publication checkpoint is intentionally
    /// retained: cancellation did not accept a new turn mutation.
    pub(crate) fn stopped(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: Option<PublicationRequestId>,
    ) -> Self {
        Self::from_parts(
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            last_request_id,
            DurableCallStatus::Stopped,
        )
    }

    fn checkpoint(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        epoch_contract_id: EpochContractId,
        next_turn_index: u64,
        last_request_id: PublicationRequestId,
        status: DurableCallStatus,
    ) -> Self {
        Self::from_parts(
            call_id,
            input_id,
            epoch_contract_id,
            next_turn_index,
            Some(last_request_id),
            status,
        )
    }

    /// Preserve this durable call's exact identity and pre-provider checkpoint
    /// while recording a recovery boundary.
    ///
    /// `last_request_id` is the last *committed* publication checkpoint. It
    /// must not be replaced by the unresolved provider operation: that
    /// operation is identified by `lease.request_id`. Keeping those identities
    /// separate lets a `NeverAccepted` reconciliation restore the exact
    /// continuation checkpoint instead of treating the unresolved operation as
    /// committed history.
    pub fn recovery_required(
        &self,
        lease: DurableCallLease,
        reason: DurableCallRecoveryReason,
    ) -> Self {
        Self::from_parts(
            self.call_id.clone(),
            self.input_id.clone(),
            self.epoch_contract_id.clone(),
            self.next_turn_index,
            self.last_request_id.clone(),
            DurableCallStatus::RecoveryRequired {
                lease,
                origin: self.reservation_origin(),
                reason,
            },
        )
    }

    pub fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    pub fn next_turn_index(&self) -> u64 {
        self.next_turn_index
    }

    pub fn last_request_id(&self) -> Option<&PublicationRequestId> {
        self.last_request_id.as_ref()
    }

    /// The exact checkpoint immediately preceding an active provider
    /// operation, when it was retained durably.
    ///
    /// Recovery code may use this only together with a strong remote
    /// `NeverAccepted` proof. `None` is intentionally ambiguous and must not
    /// be treated as [`DurableCallReservationOrigin::New`].
    pub fn reservation_origin(&self) -> Option<DurableCallReservationOrigin> {
        match &self.status {
            DurableCallStatus::Reserved { origin, .. } => Some(*origin),
            DurableCallStatus::Running { origin, .. }
            | DurableCallStatus::RecoveryRequired { origin, .. } => *origin,
            DurableCallStatus::AwaitingContinuation
            | DurableCallStatus::Settled
            | DurableCallStatus::Paused
            | DurableCallStatus::Stopped => None,
        }
    }

    /// Reconstruct the stable checkpoint immediately before this call crossed
    /// the provider boundary.
    ///
    /// The outer `None` deliberately covers both legacy records without a
    /// retained origin and malformed continuation records without their prior
    /// publication id. Callers must keep either case fenced rather than
    /// treating it as a new call.
    pub(crate) fn retained_pre_provider_checkpoint(
        &self,
    ) -> Option<DurableCallPreProviderCheckpoint> {
        match self.reservation_origin()? {
            DurableCallReservationOrigin::New => Some(DurableCallPreProviderCheckpoint::New),
            DurableCallReservationOrigin::AwaitingContinuation => {
                self.last_request_id().cloned().map(|last_request_id| {
                    DurableCallPreProviderCheckpoint::AwaitingContinuation(
                        Self::awaiting_continuation(
                            self.call_id.clone(),
                            self.input_id.clone(),
                            self.epoch_contract_id.clone(),
                            self.next_turn_index,
                            last_request_id,
                        ),
                    )
                })
            }
        }
    }

    /// Install a store-issued reconciliation fence without changing the
    /// unresolved operation or its pre-provider checkpoint.
    pub(crate) fn with_reconciliation_fence(
        &self,
        reconciliation: MountedCallReconciliationFence,
    ) -> Option<Self> {
        let DurableCallStatus::RecoveryRequired { .. } = &self.status else {
            return None;
        };
        let mut state = self.clone();
        state.reconciliation = Some(reconciliation);
        Some(state)
    }

    /// Clear exactly the reconciliation fence held by a completed controller.
    /// A mismatched or absent fence cannot alter the recovery state.
    pub(crate) fn without_reconciliation_fence(
        &self,
        expected: &MountedCallReconciliationFence,
    ) -> Option<Self> {
        let DurableCallStatus::RecoveryRequired { .. } = &self.status else {
            return None;
        };
        if self.reconciliation.as_ref() != Some(expected) {
            return None;
        }
        let mut state = self.clone();
        state.reconciliation = None;
        Some(state)
    }

    pub(crate) fn reconciliation_fence(&self) -> Option<&MountedCallReconciliationFence> {
        self.reconciliation.as_ref()
    }

    pub fn status(&self) -> &DurableCallStatus {
        &self.status
    }
}

/// Stable checkpoint retained by an origin-aware active call.
///
/// This is deliberately crate-private: it exists for persistence adapters and
/// recovery controllers, not as an application-facing call lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DurableCallPreProviderCheckpoint {
    New,
    AwaitingContinuation(DurableCallState),
}

/// Persistence-owned success proof for one settled logical call.
///
/// This value retains the store revision and publication fingerprint needed to
/// validate durable replay. It is deliberately private; the mounted facade
/// projects it into [`MountedCallResult`] before returning to an author.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredCallResult<Version> {
    session_id: DurableSessionId,
    epoch_contract_id: EpochContractId,
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    receipt: PublicationReceipt<Version>,
}

impl<Version> StoredCallResult<Version> {
    pub(crate) fn new(
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        receipt: PublicationReceipt<Version>,
    ) -> Self {
        Self {
            session_id,
            epoch_contract_id,
            call_id,
            input_id,
            receipt,
        }
    }

    pub(crate) fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub(crate) fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub(crate) fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub(crate) fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    pub(crate) fn receipt(&self) -> &PublicationReceipt<Version> {
        &self.receipt
    }
}

/// Persistence-bearing terminal result used by the private mounted owner.
///
/// The future public call handle converts this value exactly once into
/// [`MountedCallOutcome`]. Keeping the two enums distinct lets internal
/// recovery tests inspect revisions without making those revisions public API.
pub(crate) enum StoredMountedCallOutcome<C, Version>
where
    C: TurnChannels,
{
    Executed {
        records: Vec<TurnRecord<C>>,
        receipts: Vec<PublicationReceipt<Version>>,
        result: StoredCallResult<Version>,
    },
    Replayed {
        result: StoredCallResult<Version>,
    },
}

impl<C, Version> StoredMountedCallOutcome<C, Version>
where
    C: TurnChannels,
{
    #[cfg(test)]
    pub(crate) fn records(&self) -> &[TurnRecord<C>] {
        match self {
            Self::Executed { records, .. } => records,
            Self::Replayed { .. } => &[],
        }
    }

    #[cfg(test)]
    pub(crate) fn receipts(&self) -> &[PublicationReceipt<Version>] {
        match self {
            Self::Executed { receipts, .. } => receipts,
            Self::Replayed { result } => std::slice::from_ref(result.receipt()),
        }
    }

    #[cfg(test)]
    pub(crate) fn result(&self) -> &StoredCallResult<Version> {
        match self {
            Self::Executed { result, .. } | Self::Replayed { result } => result,
        }
    }
}

/// Public semantic fact for one durably accepted turn publication.
///
/// Store revisions, request ids, fingerprints, and leases are intentionally
/// absent. Those values belong to the persistence adapter and cannot become
/// part of an application-facing call contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedTurnPublication {
    publication_id: PublicationId,
    queued_outbox_items: u64,
}

impl MountedTurnPublication {
    pub fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }

    /// Number of effects durably queued by the publication transaction.
    pub fn queued_outbox_items(&self) -> u64 {
        self.queued_outbox_items
    }

    pub(crate) fn from_receipt<Version>(receipt: &PublicationReceipt<Version>) -> Self {
        Self {
            publication_id: receipt.publication_id().clone(),
            queued_outbox_items: receipt.outbox_count(),
        }
    }
}

/// Revision-free durable completion returned by the mounted call facade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedCallResult {
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    terminal_publication: MountedTurnPublication,
}

impl MountedCallResult {
    pub fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    pub fn terminal_publication(&self) -> &MountedTurnPublication {
        &self.terminal_publication
    }

    pub(crate) fn from_stored<Version>(result: &StoredCallResult<Version>) -> Self {
        Self {
            call_id: result.call_id().clone(),
            input_id: result.input_id().clone(),
            terminal_publication: MountedTurnPublication::from_receipt(result.receipt()),
        }
    }
}

/// Terminal result of one mounted logical call.
///
/// [`MountedCallOutcome::Executed`] is returned only to the owner that ran the
/// provider stream. Its records retain the typed Output and Diagnostic values
/// observed during that execution. A duplicate durable call never replays the
/// provider, tools, reducers, or Live effects; it returns
/// [`MountedCallOutcome::Replayed`] with the stable durable-result summary only.
///
/// Consumers that need typed records must handle them when the first execution
/// completes. The replay variant intentionally has no `records` field, so it
/// cannot be mistaken for an execution that produced no output.
#[non_exhaustive]
pub enum MountedCallOutcome<C>
where
    C: TurnChannels,
{
    /// This owner executed the call and durably published every committed turn.
    Executed {
        /// One typed provider observation for each committed turn, in call order.
        records: Vec<TurnRecord<C>>,
        /// Revision-free durable publications, in committed turn order.
        publications: Vec<MountedTurnPublication>,
        /// The replayable terminal proof for the settled logical call.
        result: MountedCallResult,
    },
    /// This call id and input id were already durably settled.
    ///
    /// The original owner may have observed typed outputs, diagnostics, and
    /// Live effects, but they are intentionally not reconstructed here.
    Replayed {
        /// The original terminal durable receipt.
        result: MountedCallResult,
    },
}

impl<C> fmt::Debug for MountedCallOutcome<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Executed {
                records,
                publications,
                ..
            } => formatter
                .debug_struct("MountedCallOutcome::Executed")
                .field("record_count", &records.len())
                .field("publication_count", &publications.len())
                .finish_non_exhaustive(),
            Self::Replayed { .. } => formatter
                .debug_struct("MountedCallOutcome::Replayed")
                .finish_non_exhaustive(),
        }
    }
}

impl<C> MountedCallOutcome<C>
where
    C: TurnChannels,
{
    #[allow(dead_code)]
    pub(crate) fn from_stored<Version>(outcome: StoredMountedCallOutcome<C, Version>) -> Self {
        match outcome {
            StoredMountedCallOutcome::Executed {
                records,
                receipts,
                result,
            } => Self::Executed {
                records,
                publications: receipts
                    .iter()
                    .map(MountedTurnPublication::from_receipt)
                    .collect(),
                result: MountedCallResult::from_stored(&result),
            },
            StoredMountedCallOutcome::Replayed { result } => Self::Replayed {
                result: MountedCallResult::from_stored(&result),
            },
        }
    }

    /// Whether this result came from durable replay rather than a new provider
    /// execution.
    pub fn is_replayed(&self) -> bool {
        matches!(self, Self::Replayed { .. })
    }

    /// The terminal durable-result summary for either execution path.
    pub fn result(&self) -> &MountedCallResult {
        match self {
            Self::Executed { result, .. } | Self::Replayed { result } => result,
        }
    }

    /// Typed records observed by this process while it executed the call.
    ///
    /// A replay always returns an empty slice. Match on the enum variant when
    /// the distinction itself is application-significant.
    #[cfg(test)]
    pub(crate) fn records(&self) -> &[TurnRecord<C>] {
        match self {
            Self::Executed { records, .. } => records,
            Self::Replayed { .. } => &[],
        }
    }

    /// Durable semantic publications for committed turns.
    #[cfg(test)]
    pub(crate) fn publications(&self) -> &[MountedTurnPublication] {
        match self {
            Self::Executed { publications, .. } => publications,
            Self::Replayed { result } => std::slice::from_ref(result.terminal_publication()),
        }
    }
}

/// Restart-stable call history for one serial durable session.
///
/// Terminal entries remain in this ledger after later calls begin so a reused
/// call id can be rejected or replayed idempotently without executing a model.
/// At most one entry may be active because the mounted session itself is
/// serial.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableCallLedger {
    calls: Vec<DurableCallState>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurableCallLedgerError {
    #[error("durable call ledger contains duplicate call id `{call_id}`")]
    DuplicateCall { call_id: DurableCallId },

    #[error(
        "serial durable call ledger contains multiple active calls `{first_call_id}` and `{second_call_id}`"
    )]
    MultipleActiveCalls {
        first_call_id: DurableCallId,
        second_call_id: DurableCallId,
    },
}

impl DurableCallLedger {
    pub fn from_calls(calls: Vec<DurableCallState>) -> Result<Self, DurableCallLedgerError> {
        let ledger = Self { calls };
        ledger.validate()?;
        Ok(ledger)
    }

    pub fn calls(&self) -> &[DurableCallState] {
        &self.calls
    }

    pub fn get(&self, call_id: &DurableCallId) -> Option<&DurableCallState> {
        self.calls.iter().find(|call| call.call_id() == call_id)
    }

    pub fn active(&self) -> Option<&DurableCallState> {
        self.calls.iter().find(|call| call.status().is_active())
    }

    pub fn validate(&self) -> Result<(), DurableCallLedgerError> {
        let mut active = None;
        for (index, call) in self.calls.iter().enumerate() {
            if self.calls[..index]
                .iter()
                .any(|existing| existing.call_id() == call.call_id())
            {
                return Err(DurableCallLedgerError::DuplicateCall {
                    call_id: call.call_id().clone(),
                });
            }
            if call.status().is_active() {
                if let Some(first) = active {
                    return Err(DurableCallLedgerError::MultipleActiveCalls {
                        first_call_id: first,
                        second_call_id: call.call_id().clone(),
                    });
                }
                active = Some(call.call_id().clone());
            }
        }
        Ok(())
    }

    pub(crate) fn upsert(&mut self, state: DurableCallState) {
        if let Some(existing) = self
            .calls
            .iter_mut()
            .find(|existing| existing.call_id() == state.call_id())
        {
            *existing = state;
        } else {
            self.calls.push(state);
        }
    }

    pub(crate) fn remove(&mut self, call_id: &DurableCallId) -> Option<DurableCallState> {
        let index = self
            .calls
            .iter()
            .position(|call| call.call_id() == call_id)?;
        Some(self.calls.remove(index))
    }
}

/// Authoritative durable state loaded before a mounted owner is opened or
/// after it enters `ReloadRequired`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountedSessionSnapshot<I, ContextState, Version> {
    session_id: DurableSessionId,
    epoch_contract_id: EpochContractId,
    session: AgentSession<I, ContextState>,
    revision: Version,
    call_ledger: DurableCallLedger,
    provider_cursor: Option<ProviderTurnCursor>,
}

impl<I, ContextState, Version> MountedSessionSnapshot<I, ContextState, Version> {
    pub fn new(
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        session: AgentSession<I, ContextState>,
        revision: Version,
    ) -> Self {
        Self {
            session_id,
            epoch_contract_id,
            session,
            revision,
            call_ledger: DurableCallLedger::default(),
            provider_cursor: None,
        }
    }

    pub fn with_call_ledger(mut self, call_ledger: DurableCallLedger) -> Self {
        self.call_ledger = call_ledger;
        self
    }

    pub fn with_provider_cursor(mut self, provider_cursor: Option<ProviderTurnCursor>) -> Self {
        self.provider_cursor = provider_cursor;
        self
    }

    pub fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub fn session(&self) -> &AgentSession<I, ContextState> {
        &self.session
    }

    pub fn revision(&self) -> &Version {
        &self.revision
    }

    pub fn call_ledger(&self) -> &DurableCallLedger {
        &self.call_ledger
    }

    pub fn provider_cursor(&self) -> Option<&ProviderTurnCursor> {
        self.provider_cursor.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        DurableSessionId,
        EpochContractId,
        AgentSession<I, ContextState>,
        Version,
        DurableCallLedger,
        Option<ProviderTurnCursor>,
    ) {
        (
            self.session_id,
            self.epoch_contract_id,
            self.session,
            self.revision,
            self.call_ledger,
            self.provider_cursor,
        )
    }
}

/// Complete typed observation of one finished provider attempt.
///
/// Output and Diagnostic values preserve callback wire order within their own
/// lanes. Live values have already been awaited by the live runtime. Commit
/// values remain private until they are staged into the durable outbox.
#[derive(Debug)]
pub struct TurnRecord<C>
where
    C: TurnChannels,
{
    outputs: Vec<C::Output>,
    diagnostics: Vec<C::Diagnostic>,
    raw_output: String,
    provider_results: Vec<ProviderToolResult>,
}

/// Owned lanes and provider transcript extracted from a [`TurnRecord`].
#[derive(Debug)]
pub struct TurnRecordParts<C>
where
    C: TurnChannels,
{
    pub outputs: Vec<C::Output>,
    pub diagnostics: Vec<C::Diagnostic>,
    pub raw_output: String,
    pub provider_results: Vec<ProviderToolResult>,
}

impl<C> TurnRecord<C>
where
    C: TurnChannels,
{
    pub fn outputs(&self) -> &[C::Output] {
        &self.outputs
    }

    pub fn diagnostics(&self) -> &[C::Diagnostic] {
        &self.diagnostics
    }

    pub fn raw_output(&self) -> &str {
        &self.raw_output
    }

    pub fn provider_results(&self) -> &[ProviderToolResult] {
        &self.provider_results
    }

    pub fn into_parts(self) -> TurnRecordParts<C> {
        TurnRecordParts {
            outputs: self.outputs,
            diagnostics: self.diagnostics,
            raw_output: self.raw_output,
            provider_results: self.provider_results,
        }
    }
}

/// Immutable identity and provider inputs supplied to one pure session
/// reduction.
#[derive(Clone, Copy)]
pub struct SessionReduceContext<'a, I, C>
where
    C: TurnChannels,
{
    session_id: &'a DurableSessionId,
    epoch_contract_id: &'a EpochContractId,
    call_id: &'a DurableCallId,
    turn_index: u64,
    request: &'a MountedProviderRequest<I>,
    record: &'a TurnRecord<C>,
}

impl<I, C> std::fmt::Debug for SessionReduceContext<'_, I, C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionReduceContext")
            .field("session_id", self.session_id)
            .field("epoch_contract_id", self.epoch_contract_id)
            .field("call_id", self.call_id)
            .field("turn_index", &self.turn_index)
            .finish_non_exhaustive()
    }
}

impl<'a, I, C> SessionReduceContext<'a, I, C>
where
    C: TurnChannels,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: &'a DurableSessionId,
        epoch_contract_id: &'a EpochContractId,
        call_id: &'a DurableCallId,
        turn_index: u64,
        request: &'a MountedProviderRequest<I>,
        record: &'a TurnRecord<C>,
    ) -> Self {
        Self {
            session_id,
            epoch_contract_id,
            call_id,
            turn_index,
            request,
            record,
        }
    }

    pub fn session_id(&self) -> &'a DurableSessionId {
        self.session_id
    }

    pub fn epoch_contract_id(&self) -> &'a EpochContractId {
        self.epoch_contract_id
    }

    pub fn call_id(&self) -> &'a DurableCallId {
        self.call_id
    }

    pub fn turn_index(&self) -> u64 {
        self.turn_index
    }

    pub fn request(&self) -> &'a MountedProviderRequest<I> {
        self.request
    }

    pub fn record(&self) -> &'a TurnRecord<C> {
        self.record
    }
}

/// Host action after a pure session reducer rejects one provider-complete turn.
///
/// Reducer errors default to [`Self::Indeterminate`]. A reducer may return
/// [`Self::Reject`] only for a deterministic contract or semantic rejection:
/// the mounted owner will compensate Live effects and durably retire the call
/// without publishing the candidate session, User cursor, or Commit outbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionReducerFailureDisposition {
    /// The failure may not be safe to retire automatically. Successor calls
    /// remain fenced behind explicit durable recovery.
    #[default]
    Indeterminate,
    /// The finished provider reply is deterministically invalid and may be
    /// rejected after acknowledged Live cleanup.
    Reject,
}

/// Synchronous pure boundary from one finished turn to a complete next
/// session draft and flow decision.
///
/// Implementations may mutate only the supplied owned draft. They must not
/// perform I/O or read mutable state through a side channel. The mounted owner
/// applies the framework-owned System snapshot and User cursor only after this
/// function succeeds.
pub trait SessionReducer<I, ContextState, C>: Send + Sync + 'static
where
    C: TurnChannels,
{
    type Error: Error + Send + Sync + 'static;

    fn reduce(
        &self,
        session: &mut AgentSession<I, ContextState>,
        context: SessionReduceContext<'_, I, C>,
        executor_commit: ExecutorCommit<I>,
    ) -> Result<TurnFlow, Self::Error>;

    /// Classify a reducer error for durable settlement.
    ///
    /// The conservative default requires recovery. Implementations should
    /// return [`SessionReducerFailureDisposition::Reject`] only when the error
    /// depends solely on the immutable provider result and captured turn data.
    fn failure_disposition(&self, _error: &Self::Error) -> SessionReducerFailureDisposition {
        SessionReducerFailureDisposition::Indeterminate
    }
}

struct PendingTurnLanes<C>
where
    C: TurnChannels,
{
    outputs: Vec<C::Output>,
    diagnostics: Vec<C::Diagnostic>,
}

impl<C> Default for PendingTurnLanes<C>
where
    C: TurnChannels,
{
    fn default() -> Self {
        Self {
            outputs: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

type SharedTurnLanes<C> = Arc<Mutex<Option<PendingTurnLanes<C>>>>;

pub(crate) struct TurnRecordInterpreter<C>
where
    C: TurnChannels,
{
    lanes: SharedTurnLanes<C>,
    _channels: PhantomData<fn() -> C>,
}

pub(crate) struct PendingTurnRecord<C>
where
    C: TurnChannels,
{
    lanes: SharedTurnLanes<C>,
}

pub(crate) fn turn_record_accumulator<C>() -> (TurnRecordInterpreter<C>, PendingTurnRecord<C>)
where
    C: TurnChannels,
{
    let lanes = Arc::new(Mutex::new(Some(PendingTurnLanes::default())));
    (
        TurnRecordInterpreter {
            lanes: Arc::clone(&lanes),
            _channels: PhantomData,
        },
        PendingTurnRecord { lanes },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TurnRecordInterpretationError {
    #[error("Live values must be consumed before the typed turn-record interpreter")]
    UnexpectedLive,

    #[error("Commit values must be retained before the typed turn-record interpreter")]
    UnexpectedCommit,
}

impl<C> AttemptUpdateInterpreter<C> for TurnRecordInterpreter<C>
where
    C: TurnChannels,
{
    type Error = TurnRecordInterpretationError;

    fn interpret(
        &mut self,
        _identity: &ProviderAttemptIdentity,
        update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) -> Result<(), Self::Error> {
        let (emissions, diagnostics) = update.into_parts();
        let mut lanes = self.lanes.lock().expect("turn record mutex was poisoned");
        let lanes = lanes
            .as_mut()
            .expect("turn record remains pending until the attempt finishes");
        for emission in emissions {
            match emission {
                TurnEmission::Output(output) => lanes.outputs.push(output),
                TurnEmission::Live(_) => return Err(TurnRecordInterpretationError::UnexpectedLive),
                TurnEmission::Commit(_) => {
                    return Err(TurnRecordInterpretationError::UnexpectedCommit);
                }
            }
        }
        lanes.diagnostics.extend(diagnostics);
        Ok(())
    }
}

impl<C> PendingTurnRecord<C>
where
    C: TurnChannels,
{
    pub(crate) fn finish(self, finished: &FinishedAttemptRecord) -> TurnRecord<C> {
        let lanes = self
            .lanes
            .lock()
            .expect("turn record mutex was poisoned")
            .take()
            .expect("a pending turn record can only finish once");
        TurnRecord {
            outputs: lanes.outputs,
            diagnostics: lanes.diagnostics,
            raw_output: finished.raw_output().to_owned(),
            provider_results: finished.provider_results().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OpaqueOutput;
    struct OpaqueDiagnostic;

    struct OpaqueChannels;

    impl TurnChannels for OpaqueChannels {
        type Output = OpaqueOutput;
        type Live = super::super::Never;
        type Commit = super::super::Never;
        type Diagnostic = OpaqueDiagnostic;
    }

    fn call_id(value: &str) -> DurableCallId {
        DurableCallId::new(value).unwrap()
    }

    fn input_id(value: &str) -> DurableCallInputId {
        DurableCallInputId::new(value).unwrap()
    }

    fn epoch_id() -> EpochContractId {
        EpochContractId::new("test/epoch/v1").unwrap()
    }

    fn request_id(value: &str) -> PublicationRequestId {
        PublicationRequestId::new(value).unwrap()
    }

    fn lease(value: &str) -> DurableCallLease {
        DurableCallLease::new(
            DurableCallLeaseId::new(format!("{value}/lease")).unwrap(),
            request_id(&format!("{value}/request")),
            1_000,
        )
    }

    fn remove_status_origin(value: &mut serde_json::Value, variant: &str) {
        value
            .get_mut("status")
            .and_then(|status| status.get_mut(variant))
            .and_then(serde_json::Value::as_object_mut)
            .expect("the serialized durable call status has named fields")
            .remove("origin");
    }

    #[test]
    fn public_call_result_projects_only_semantic_publication_facts() {
        let stored = StoredCallResult::new(
            DurableSessionId::new("test/session").unwrap(),
            epoch_id(),
            call_id("test-call"),
            input_id("test-call/input"),
            PublicationReceipt::new(
                request_id("test-call/turn-0"),
                PublicationCandidateFingerprint::new("test/fingerprint").unwrap(),
                PublicationId::new("test/publication").unwrap(),
                41_u64,
                3,
            ),
        );

        let result = MountedCallResult::from_stored(&stored);
        assert_eq!(result.call_id().as_str(), "test-call");
        assert_eq!(result.input_id().as_str(), "test-call/input");
        assert_eq!(
            result.terminal_publication().publication_id().as_str(),
            "test/publication"
        );
        assert_eq!(result.terminal_publication().queued_outbox_items(), 3);

        let replay: MountedCallOutcome<OpaqueChannels> = MountedCallOutcome::Replayed { result };
        assert!(replay.is_replayed());
        assert!(replay.records().is_empty());
        assert_eq!(replay.publications().len(), 1);
        assert!(format!("{replay:?}").contains("Replayed"));
    }

    #[test]
    fn durable_call_ledger_rejects_duplicate_and_multiple_active_calls() {
        let duplicate = DurableCallLedger::from_calls(vec![
            DurableCallState::settled(
                call_id("same-call"),
                input_id("same-call/input"),
                epoch_id(),
                1,
                request_id("same-call/turn-0"),
            ),
            DurableCallState::settled(
                call_id("same-call"),
                input_id("same-call/input"),
                epoch_id(),
                1,
                request_id("same-call/turn-0"),
            ),
        ])
        .unwrap_err();
        assert!(matches!(
            duplicate,
            DurableCallLedgerError::DuplicateCall { call_id }
                if call_id.as_str() == "same-call"
        ));

        let multiple_active = DurableCallLedger::from_calls(vec![
            DurableCallState::awaiting_continuation(
                call_id("first-call"),
                input_id("first-call/input"),
                epoch_id(),
                1,
                request_id("first-call/turn-0"),
            ),
            DurableCallState::awaiting_continuation(
                call_id("second-call"),
                input_id("second-call/input"),
                epoch_id(),
                1,
                request_id("second-call/turn-0"),
            ),
        ])
        .unwrap_err();
        assert!(matches!(
            multiple_active,
            DurableCallLedgerError::MultipleActiveCalls {
                first_call_id,
                second_call_id,
            } if first_call_id.as_str() == "first-call"
                && second_call_id.as_str() == "second-call"
        ));
    }

    #[test]
    fn provider_started_states_retain_the_exact_reservation_origin() {
        for origin in [
            DurableCallReservationOrigin::New,
            DurableCallReservationOrigin::AwaitingContinuation,
        ] {
            let running = DurableCallState::running(
                call_id("origin-call"),
                input_id("origin-call/input"),
                epoch_id(),
                2,
                Some(request_id("origin-call/turn-1")),
                lease("origin-call/turn-2"),
                origin,
            );
            assert_eq!(running.reservation_origin(), Some(origin));

            let recovered: DurableCallState =
                serde_json::from_value(serde_json::to_value(&running).unwrap()).unwrap();
            assert_eq!(recovered.reservation_origin(), Some(origin));

            let recovery = running.recovery_required(
                lease("origin-call/turn-2"),
                DurableCallRecoveryReason::LeaseExpired,
            );
            assert_eq!(recovery.reservation_origin(), Some(origin));

            let recovered: DurableCallState =
                serde_json::from_value(serde_json::to_value(&recovery).unwrap()).unwrap();
            assert_eq!(recovered.reservation_origin(), Some(origin));
        }
    }

    #[test]
    fn legacy_provider_started_states_without_origin_remain_ambiguous() {
        let running = DurableCallState::running(
            call_id("legacy-running"),
            input_id("legacy-running/input"),
            epoch_id(),
            0,
            None,
            lease("legacy-running/turn-0"),
            DurableCallReservationOrigin::New,
        );
        let mut running_json = serde_json::to_value(running).unwrap();
        remove_status_origin(&mut running_json, "Running");
        let running: DurableCallState = serde_json::from_value(running_json).unwrap();
        assert_eq!(running.reservation_origin(), None);

        let recovery_seed = DurableCallState::running(
            call_id("legacy-recovery"),
            input_id("legacy-recovery/input"),
            epoch_id(),
            1,
            None,
            lease("legacy-recovery/turn-1"),
            DurableCallReservationOrigin::AwaitingContinuation,
        );
        let recovery = recovery_seed.recovery_required(
            lease("legacy-recovery/turn-1"),
            DurableCallRecoveryReason::LeaseExpired,
        );
        let mut recovery_json = serde_json::to_value(recovery).unwrap();
        remove_status_origin(&mut recovery_json, "RecoveryRequired");
        let recovery: DurableCallState = serde_json::from_value(recovery_json).unwrap();
        assert_eq!(recovery.reservation_origin(), None);
    }

    #[test]
    fn recovery_retains_the_pre_provider_continuation_checkpoint() {
        let prior_request_id = request_id("continuation/turn-0");
        let unresolved_request_id = request_id("continuation/turn-1");
        let unresolved_lease = DurableCallLease::new(
            DurableCallLeaseId::new("continuation/lease-1").unwrap(),
            unresolved_request_id.clone(),
            1_000,
        );
        let running = DurableCallState::running(
            call_id("continuation"),
            input_id("continuation/input"),
            epoch_id(),
            1,
            Some(prior_request_id.clone()),
            unresolved_lease.clone(),
            DurableCallReservationOrigin::AwaitingContinuation,
        );

        let recovery =
            running.recovery_required(unresolved_lease, DurableCallRecoveryReason::LeaseExpired);

        assert_eq!(recovery.last_request_id(), Some(&prior_request_id));
        assert_eq!(
            recovery.status().lease().unwrap().request_id(),
            &unresolved_request_id
        );
        match recovery.retained_pre_provider_checkpoint() {
            Some(DurableCallPreProviderCheckpoint::AwaitingContinuation(restored)) => {
                assert_eq!(restored.last_request_id(), Some(&prior_request_id));
                assert_eq!(restored.next_turn_index(), 1);
            }
            checkpoint => panic!("unexpected checkpoint: {checkpoint:?}"),
        }
    }
}
