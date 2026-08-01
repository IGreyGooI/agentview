//! Durable publication contract for a future mounted agent owner.
//!
//! This module deliberately does not adapt the legacy [`super::TurnPublisher`].
//! That trait is an isolated phase-ordering proof: it cannot see the complete
//! session mutation or typed Commit values. Durable publication instead stages
//! every Commit while the root channel contract is still known, then asks one
//! store transaction to compare-and-swap the session and insert the matching
//! outbox rows.

use std::{error::Error, fmt};

use sha2::{Digest, Sha256};

use crate::StorageString;

use super::{
    BindingAbortReason, FinishedProviderAttempt, FinishedStreamingAttempt,
    ProviderAttemptAbortReport, ProviderAttemptIdentity, ProviderToolResult, StreamUpdate,
    StreamingAbortReport, TurnChannels, TurnEmission,
};
fn validate_durable_key(
    value: impl Into<StorageString>,
    kind: &'static str,
) -> Result<StorageString, DurableKeyError> {
    let value = value.into();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(DurableKeyError {
            kind,
            value: value.to_string(),
        });
    }
    Ok(value)
}

/// Invalid host-issued identity or stable contract key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} `{value}`; the value must be non-empty and contain no control characters")]
pub struct DurableKeyError {
    kind: &'static str,
    value: String,
}

/// Stable durable identity of one mounted agent session.
///
/// This value scopes every call, revision, epoch contract, and publication
/// handled by one mount-owned persistence binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DurableSessionId(StorageString);

impl DurableSessionId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "durable session id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DurableSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Store-issued durable identity of one immutable mounted System epoch.
///
/// This identity survives process restart and is the idempotency key for the
/// provider's initial System attachment. It is deliberately distinct from
/// [`super::HarnessEpochId`], which is allocated in process and is useful only
/// for diagnostics and type-safe attempt ownership.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DurableEpochId(StorageString);

impl DurableEpochId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "durable epoch id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DurableEpochId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-issued idempotency identity for one explicit durable System replacement.
///
/// A reconfiguration can render and attach a new epoch across process failure,
/// so retrying the same host operation must retain this id rather than deriving
/// it from a process-local timestamp or pointer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct EpochReconfigurationId(StorageString);

impl EpochReconfigurationId {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "epoch reconfiguration id").map(Self)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EpochReconfigurationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable identity of one logical mounted call, distinct from its diagnostic
/// label and from each per-turn publication request id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DurableCallId(StorageString);

impl DurableCallId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "durable call id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DurableCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable identity of the immutable inputs attached to one logical call.
///
/// A resumed continuation must present the same value before User rendering or
/// provider/tool execution. The host may use a content fingerprint or a stable
/// work-item version; process-local addresses are not valid identities.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DurableCallInputId(StorageString);

impl DurableCallInputId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "durable call input id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DurableCallInputId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Store-issued identity of one exclusive durable call admission.
///
/// A lease id is allocated by the authoritative persistence binding while it
/// atomically changes a call to `Running`. It is not a provider-attempt id and
/// must never be synthesized from process-local state. The mounted publication
/// transaction presents the same id so the store can reject a stale or expired
/// owner before accepting its session mutation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DurableCallLeaseId(StorageString);

impl DurableCallLeaseId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "durable call lease id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DurableCallLeaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-defined durable identity of a complete mounted System contract.
///
/// It must cover the System POM, binding factories, provider capabilities, and
/// every author-declared behavior/schema version. Process-local type ids,
/// closure addresses, and [`super::HarnessEpochId`] are not valid inputs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EpochContractId(StorageString);

impl EpochContractId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "epoch contract id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EpochContractId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Durable idempotency identity allocated before publication starts.
///
/// The host must allocate this from a namespace that survives process restart
/// and retain it while an indeterminate transaction is resolved. A
/// [`ProviderAttemptIdentity`] is process-local and must not be converted into
/// this value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PublicationRequestId(StorageString);

impl PublicationRequestId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "publication request id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PublicationRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable provider-facing identity for one durably admitted mounted turn.
///
/// A provider adapter receives this only through an ordinary
/// [`MountedProviderRequest`](super::MountedProviderRequest). It is distinct
/// from a display label and from the process-local
/// [`ProviderAttemptIdentity`](super::ProviderAttemptIdentity): a remote
/// session can use [`Self::remote_idempotency_key`] to recognize the same
/// logical execution after a local owner reloads or retries preparation.
///
/// The mounted owner constructs this after the durable call reservation has
/// selected the exact `(session, epoch, call, input, turn, request)` tuple.
/// It intentionally does not include a lease id, which may change while the
/// same logical provider operation is recovered.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProviderOperationIdentity {
    session_id: DurableSessionId,
    durable_epoch_id: DurableEpochId,
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    turn_index: u64,
    request_id: PublicationRequestId,
}

impl ProviderOperationIdentity {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: DurableSessionId,
        durable_epoch_id: DurableEpochId,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        turn_index: u64,
        request_id: PublicationRequestId,
    ) -> Self {
        Self {
            session_id,
            durable_epoch_id,
            call_id,
            input_id,
            turn_index,
            request_id,
        }
    }

    /// Canonical globally scoped idempotency key for this remote execution.
    ///
    /// The returned opaque value is a versioned SHA-256 digest of the exact
    /// durable `(session, epoch, call, input, turn, request)` identity. A
    /// stateful remote provider must use this value, rather than a display
    /// label or a bare publication request id, when deduplicating execution.
    /// It remains unchanged across a local lease replacement or User-document
    /// resync, but changes for every different durable epoch or logical turn.
    pub fn remote_idempotency_key(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"agentview.provider-operation.v1\0");
        update_provider_operation_key_part(&mut digest, self.session_id.as_str());
        update_provider_operation_key_part(&mut digest, self.durable_epoch_id.as_str());
        update_provider_operation_key_part(&mut digest, self.call_id.as_str());
        update_provider_operation_key_part(&mut digest, self.input_id.as_str());
        digest.update(self.turn_index.to_be_bytes());
        update_provider_operation_key_part(&mut digest, self.request_id.as_str());
        let digest = digest.finalize();
        format!("agentview.provider-operation.v1:{digest:x}")
    }

    /// Stable publication request id for this operation's local durable
    /// publication record.
    ///
    /// This value is not globally scoped and must not be used as a remote
    /// provider idempotency key by itself.
    pub fn publication_request_id(&self) -> &str {
        self.request_id.as_str()
    }

    /// Durable session that owns this operation.
    pub fn session_id(&self) -> &str {
        self.session_id.as_str()
    }

    /// Durable System epoch that owns the remote provider session.
    pub fn durable_epoch_id(&self) -> &str {
        self.durable_epoch_id.as_str()
    }

    /// Stable logical call identity.
    pub fn call_id(&self) -> &str {
        self.call_id.as_str()
    }

    /// Stable immutable input revision selected for the call.
    pub fn input_id(&self) -> &str {
        self.input_id.as_str()
    }

    /// Zero-based committed turn inside the logical call.
    pub fn turn_index(&self) -> u64 {
        self.turn_index
    }
}

fn update_provider_operation_key_part(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

/// Stable inputs supplied to a mount-owned per-turn request-id policy.
#[derive(Debug, Clone, Copy)]
pub struct PublicationRequestContext<'a> {
    session_id: &'a DurableSessionId,
    epoch_contract_id: &'a EpochContractId,
    call_id: &'a DurableCallId,
    turn_index: u64,
}

impl<'a> PublicationRequestContext<'a> {
    pub fn new(
        session_id: &'a DurableSessionId,
        epoch_contract_id: &'a EpochContractId,
        call_id: &'a DurableCallId,
        turn_index: u64,
    ) -> Self {
        Self {
            session_id,
            epoch_contract_id,
            call_id,
            turn_index,
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
}

/// Pure mount-owned policy for deriving each continuation turn's durable
/// publication request id.
///
/// The same context must always produce the same id. A call id therefore must
/// be allocated durably before execution and remain stable across recovery.
pub trait PublicationRequestIdFactory: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn request_id(
        &self,
        context: PublicationRequestContext<'_>,
    ) -> Result<PublicationRequestId, Self::Error>;
}

/// Host-produced stable digest of one complete publication candidate.
///
/// This is deliberately an opaque durable value rather than framework-derived
/// serialization. The host's [`PublicationFingerprintFactory`] receives the
/// actual staged outbox and must cover the expected revision, complete session
/// mutation (including any durable logical-call checkpoint), raw output,
/// provider results, and every ordered outbox contract and payload. It cannot
/// observe the process-local
/// [`ProviderAttemptIdentity`].
///
/// A store compares this value byte-for-byte with the value already recorded
/// for a [`PublicationRequestId`]. The same request id is idempotent only when
/// the fingerprint is identical.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PublicationCandidateFingerprint(StorageString);

impl PublicationCandidateFingerprint {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "publication candidate fingerprint").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl fmt::Display for PublicationCandidateFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-issued identity of one committed session/outbox transaction.
///
/// This identity is distinct from the request id so a store can expose its
/// native session revision, transaction, or event-log identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PublicationId(StorageString);

impl PublicationId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, DurableKeyError> {
        validate_durable_key(value, "publication id").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PublicationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable identity of one outbox row within a publication request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct OutboxItemId {
    request_id: PublicationRequestId,
    index: u64,
}

impl OutboxItemId {
    pub fn new(request_id: PublicationRequestId, index: u64) -> Self {
        Self { request_id, index }
    }

    pub fn request_id(&self) -> &PublicationRequestId {
        &self.request_id
    }

    pub fn index(&self) -> u64 {
        self.index
    }
}

impl fmt::Display for OutboxItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.request_id, self.index)
    }
}

/// Stable decoder/handler identity for one durable Commit payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct CommitContract {
    key: StorageString,
    schema_version: u32,
}

impl CommitContract {
    pub fn new(
        key: impl Into<StorageString>,
        schema_version: u32,
    ) -> Result<Self, DurableKeyError> {
        Ok(Self {
            key: validate_durable_key(key, "commit contract key")?,
            schema_version,
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

/// Application payload produced by a pure Commit staging function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedCommit<Payload> {
    contract: CommitContract,
    payload: Payload,
}

impl<Payload> StagedCommit<Payload> {
    pub fn new(contract: CommitContract, payload: Payload) -> Self {
        Self { contract, payload }
    }

    pub fn contract(&self) -> &CommitContract {
        &self.contract
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn into_parts(self) -> (CommitContract, Payload) {
        (self.contract, self.payload)
    }
}

/// Stable context supplied while encoding one Commit value.
#[derive(Debug, Clone, Copy)]
pub struct CommitStagingContext<'a> {
    item_id: &'a OutboxItemId,
}

impl<'a> CommitStagingContext<'a> {
    pub fn item_id(&self) -> &'a OutboxItemId {
        self.item_id
    }
}

/// Pure typed boundary from a root Commit value to one durable outbox payload.
///
/// `stage` is synchronous by design. Implementations must only encode and
/// validate data; application I/O and effect delivery belong to the durable
/// outbox worker. One Commit always maps to exactly one item so
/// `(PublicationRequestId, index)` remains stable across retries and restart.
pub trait CommitStager<C>: Send + Sync + 'static
where
    C: TurnChannels,
{
    type Payload: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn stage(
        &self,
        context: CommitStagingContext<'_>,
        commit: &C::Commit,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error>;
}

/// One fully owned outbox item ready for transactional persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedOutboxItem<Payload> {
    id: OutboxItemId,
    contract: CommitContract,
    payload: Payload,
}

impl<Payload> StagedOutboxItem<Payload> {
    pub fn id(&self) -> &OutboxItemId {
        &self.id
    }

    pub fn contract(&self) -> &CommitContract {
        &self.contract
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }
}

/// Ordered durable representation of one root Commit batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedOutbox<Payload> {
    request_id: PublicationRequestId,
    items: Vec<StagedOutboxItem<Payload>>,
}

impl<Payload> StagedOutbox<Payload> {
    pub fn request_id(&self) -> &PublicationRequestId {
        &self.request_id
    }

    pub fn items(&self) -> &[StagedOutboxItem<Payload>] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Commit encoding failure with its already-stable outbox item identity.
#[derive(Debug, thiserror::Error)]
#[error("failed to stage Commit as outbox item `{item_id}`: {source}")]
pub struct CommitStagingFailure<E>
where
    E: Error + Send + Sync + 'static,
{
    item_id: OutboxItemId,
    #[source]
    source: E,
}

impl<E> CommitStagingFailure<E>
where
    E: Error + Send + Sync + 'static,
{
    pub fn item_id(&self) -> &OutboxItemId {
        &self.item_id
    }

    pub fn source_error(&self) -> &E {
        &self.source
    }
}

/// Encode a complete Commit batch before root-channel type erasure.
pub fn stage_commit_outbox<C, S>(
    request_id: PublicationRequestId,
    commits: &[C::Commit],
    stager: &S,
) -> Result<StagedOutbox<S::Payload>, CommitStagingFailure<S::Error>>
where
    C: TurnChannels,
    S: CommitStager<C>,
{
    let mut items = Vec::with_capacity(commits.len());
    for (index, commit) in commits.iter().enumerate() {
        let index = u64::try_from(index).expect("usize always fits in u64 on supported targets");
        let id = OutboxItemId::new(request_id.clone(), index);
        let staged = stager
            .stage(CommitStagingContext { item_id: &id }, commit)
            .map_err(|source| CommitStagingFailure {
                item_id: id.clone(),
                source,
            })?;
        let (contract, payload) = staged.into_parts();
        items.push(StagedOutboxItem {
            id,
            contract,
            payload,
        });
    }
    Ok(StagedOutbox { request_id, items })
}

/// Pure, owned session mutation selected before the publication transaction.
///
/// For a mounted agent this value must describe the complete next durable
/// session state, including transcript, context state, User POM cursor, and
/// flow metadata. Constructing it must not perform I/O or mutate the live
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSessionMutation<Mutation> {
    value: Mutation,
}

impl<Mutation> PreparedSessionMutation<Mutation> {
    pub fn new(value: Mutation) -> Self {
        Self { value }
    }

    pub fn value(&self) -> &Mutation {
        &self.value
    }

    pub fn into_inner(self) -> Mutation {
        self.value
    }
}

/// Complete read-only candidate supplied to a host fingerprint factory.
///
/// This context is created only after every Commit has crossed the typed
/// [`CommitStager`] boundary. Consequently the outbox is the exact ordered
/// contract/payload sequence that the store will receive. Process-local
/// attempt identity is intentionally absent.
#[derive(Debug, Clone, Copy)]
pub struct PublicationFingerprintContext<'a, Mutation, Payload, Version> {
    request_id: &'a PublicationRequestId,
    expected_revision: &'a Version,
    mutation: &'a PreparedSessionMutation<Mutation>,
    raw_output: &'a str,
    provider_results: &'a [ProviderToolResult],
    outbox: &'a StagedOutbox<Payload>,
}

impl<'a, Mutation, Payload, Version> PublicationFingerprintContext<'a, Mutation, Payload, Version> {
    pub(crate) fn new(
        request_id: &'a PublicationRequestId,
        expected_revision: &'a Version,
        mutation: &'a PreparedSessionMutation<Mutation>,
        raw_output: &'a str,
        provider_results: &'a [ProviderToolResult],
        outbox: &'a StagedOutbox<Payload>,
    ) -> Self {
        Self {
            request_id,
            expected_revision,
            mutation,
            raw_output,
            provider_results,
            outbox,
        }
    }

    pub fn request_id(&self) -> &'a PublicationRequestId {
        self.request_id
    }

    pub fn expected_revision(&self) -> &'a Version {
        self.expected_revision
    }

    pub fn mutation(&self) -> &'a PreparedSessionMutation<Mutation> {
        self.mutation
    }

    pub fn raw_output(&self) -> &'a str {
        self.raw_output
    }

    pub fn provider_results(&self) -> &'a [ProviderToolResult] {
        self.provider_results
    }

    pub fn outbox(&self) -> &'a StagedOutbox<Payload> {
        self.outbox
    }
}

/// Pure host policy for canonicalizing one fully staged publication.
///
/// The factory is synchronous by design. It may encode, hash, and validate the
/// supplied values, but must not perform I/O or mutate application state. The
/// framework does not serialize generic mutation or payload values on the
/// host's behalf.
pub trait PublicationFingerprintFactory<Mutation, Payload, Version>: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<'_, Mutation, Payload, Version>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error>;
}

/// Failure while turning a finished attempt into a complete durable candidate.
#[derive(Debug, thiserror::Error)]
pub enum PublicationStagingFailure<CommitError, FingerprintError>
where
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Commit(#[from] CommitStagingFailure<CommitError>),

    #[error("failed to fingerprint the fully staged publication: {0}")]
    Fingerprint(FingerprintError),
}

/// Owned durable inputs that must remain identical across staging retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationStagingPlan<Mutation, Version> {
    request_id: PublicationRequestId,
    expected_revision: Version,
    mutation: PreparedSessionMutation<Mutation>,
}

impl<Mutation, Version> PublicationStagingPlan<Mutation, Version> {
    pub fn new(
        request_id: PublicationRequestId,
        expected_revision: Version,
        mutation: PreparedSessionMutation<Mutation>,
    ) -> Self {
        Self {
            request_id,
            expected_revision,
            mutation,
        }
    }

    pub fn request_id(&self) -> &PublicationRequestId {
        &self.request_id
    }

    pub fn expected_revision(&self) -> &Version {
        &self.expected_revision
    }

    pub fn mutation(&self) -> &PreparedSessionMutation<Mutation> {
        &self.mutation
    }

    pub fn into_parts(
        self,
    ) -> (
        PublicationRequestId,
        Version,
        PreparedSessionMutation<Mutation>,
    ) {
        (self.request_id, self.expected_revision, self.mutation)
    }
}

/// Fully owned candidate retained across publication retry and resolution.
///
/// `C::Commit` no longer appears in this type. Its values have already crossed
/// the monomorphized [`CommitStager`] boundary into durable payloads.
#[derive(Debug)]
pub struct StagedPublication<Mutation, Payload, Version> {
    request_id: PublicationRequestId,
    fingerprint: PublicationCandidateFingerprint,
    expected_revision: Version,
    mutation: PreparedSessionMutation<Mutation>,
    attempt: ProviderAttemptIdentity,
    raw_output: String,
    provider_results: Vec<ProviderToolResult>,
    outbox: StagedOutbox<Payload>,
}

impl<Mutation, Payload, Version> StagedPublication<Mutation, Payload, Version> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: PublicationRequestId,
        fingerprint: PublicationCandidateFingerprint,
        expected_revision: Version,
        mutation: PreparedSessionMutation<Mutation>,
        attempt: ProviderAttemptIdentity,
        raw_output: impl Into<String>,
        provider_results: Vec<ProviderToolResult>,
        outbox: StagedOutbox<Payload>,
    ) -> Result<Self, PublicationCandidateError> {
        if request_id != *outbox.request_id() {
            return Err(PublicationCandidateError::OutboxRequestMismatch {
                publication: request_id,
                outbox: outbox.request_id().clone(),
            });
        }
        Ok(Self {
            request_id,
            fingerprint,
            expected_revision,
            mutation,
            attempt,
            raw_output: raw_output.into(),
            provider_results,
            outbox,
        })
    }

    pub fn request_id(&self) -> &PublicationRequestId {
        &self.request_id
    }

    pub fn fingerprint(&self) -> &PublicationCandidateFingerprint {
        &self.fingerprint
    }

    pub fn expected_revision(&self) -> &Version {
        &self.expected_revision
    }

    pub fn mutation(&self) -> &PreparedSessionMutation<Mutation> {
        &self.mutation
    }

    /// Process-local correlation only; never a durable write key.
    pub fn attempt(&self) -> &ProviderAttemptIdentity {
        &self.attempt
    }

    pub fn raw_output(&self) -> &str {
        &self.raw_output
    }

    pub fn provider_results(&self) -> &[ProviderToolResult] {
        &self.provider_results
    }

    pub fn outbox(&self) -> &StagedOutbox<Payload> {
        &self.outbox
    }

    pub fn as_request(&self) -> PublicationRequest<'_, Mutation, Payload, Version> {
        PublicationRequest { candidate: self }
    }

    pub(crate) fn into_staging_plan(self) -> PublicationStagingPlan<Mutation, Version> {
        PublicationStagingPlan::new(self.request_id, self.expected_revision, self.mutation)
    }

    fn validate_receipt(
        &self,
        receipt: &PublicationReceipt<Version>,
    ) -> Result<(), PublicationReceiptMismatch> {
        if receipt.request_id() != self.request_id() {
            return Err(PublicationReceiptMismatch::RequestId {
                expected: self.request_id.clone(),
                actual: receipt.request_id.clone(),
            });
        }
        if receipt.fingerprint() != self.fingerprint() {
            return Err(PublicationReceiptMismatch::Fingerprint {
                expected: self.fingerprint.clone(),
                actual: receipt.fingerprint.clone(),
            });
        }
        let expected = u64::try_from(self.outbox.len())
            .expect("usize always fits in u64 on supported targets");
        if receipt.outbox_count() != expected {
            return Err(PublicationReceiptMismatch::OutboxCount {
                expected,
                actual: receipt.outbox_count(),
            });
        }
        Ok(())
    }
}

/// Invalid combination of a publication request and staged outbox.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicationCandidateError {
    #[error(
        "publication request id `{publication}` does not match staged outbox request id `{outbox}`"
    )]
    OutboxRequestMismatch {
        publication: PublicationRequestId,
        outbox: PublicationRequestId,
    },
}

/// Read-only input to one atomic session/outbox transaction.
#[derive(Debug)]
pub struct PublicationRequest<'a, Mutation, Payload, Version> {
    candidate: &'a StagedPublication<Mutation, Payload, Version>,
}

impl<Mutation, Payload, Version> Copy for PublicationRequest<'_, Mutation, Payload, Version> {}

impl<Mutation, Payload, Version> Clone for PublicationRequest<'_, Mutation, Payload, Version> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, Mutation, Payload, Version> PublicationRequest<'a, Mutation, Payload, Version> {
    pub fn request_id(&self) -> &'a PublicationRequestId {
        self.candidate.request_id()
    }

    pub fn fingerprint(&self) -> &'a PublicationCandidateFingerprint {
        self.candidate.fingerprint()
    }

    pub fn expected_revision(&self) -> &'a Version {
        self.candidate.expected_revision()
    }

    pub fn mutation(&self) -> &'a PreparedSessionMutation<Mutation> {
        self.candidate.mutation()
    }

    /// Process-local diagnostic correlation only.
    pub fn attempt(&self) -> &'a ProviderAttemptIdentity {
        self.candidate.attempt()
    }

    pub fn raw_output(&self) -> &'a str {
        self.candidate.raw_output()
    }

    pub fn provider_results(&self) -> &'a [ProviderToolResult] {
        self.candidate.provider_results()
    }

    pub fn outbox(&self) -> &'a StagedOutbox<Payload> {
        self.candidate.outbox()
    }
}

/// Durable proof that the session mutation and its outbox were committed.
///
/// Success means effects are durably queued, not that an external receiver has
/// applied them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicationReceipt<Version> {
    request_id: PublicationRequestId,
    fingerprint: PublicationCandidateFingerprint,
    publication_id: PublicationId,
    session_revision: Version,
    outbox_count: u64,
}

impl<Version> PublicationReceipt<Version> {
    pub fn new(
        request_id: PublicationRequestId,
        fingerprint: PublicationCandidateFingerprint,
        publication_id: PublicationId,
        session_revision: Version,
        outbox_count: u64,
    ) -> Self {
        Self {
            request_id,
            fingerprint,
            publication_id,
            session_revision,
            outbox_count,
        }
    }

    pub fn request_id(&self) -> &PublicationRequestId {
        &self.request_id
    }

    pub fn fingerprint(&self) -> &PublicationCandidateFingerprint {
        &self.fingerprint
    }

    pub fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }

    pub fn session_revision(&self) -> &Version {
        &self.session_revision
    }

    pub fn outbox_count(&self) -> u64 {
        self.outbox_count
    }
}

/// Store response when the publish call itself did not return a receipt.
#[derive(Debug, thiserror::Error)]
pub enum PublicationWriteError<E>
where
    E: Error + Send + Sync + 'static,
{
    /// The candidate was definitely not committed because the expected
    /// session revision is stale. A mounted owner must reload the authoritative
    /// session and revision before it accepts another call.
    #[error("publication revision conflicted with the authoritative session: {0}")]
    Conflict(E),

    /// The store guarantees that the transaction did not commit.
    #[error("publication was definitely not committed: {0}")]
    Rejected(E),

    /// The store cannot say whether the transaction committed.
    #[error("publication outcome is indeterminate and must be resolved: {0}")]
    Indeterminate(E),
}

/// Failure while resolving an indeterminate publication.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicationResolveError<E>
where
    E: Error + Send + Sync + 'static,
{
    /// The store could not produce an authoritative answer yet.
    #[error("publication resolution is temporarily unavailable: {0}")]
    Unavailable(E),

    /// The request id belongs to a different candidate fingerprint.
    ///
    /// This proves that the candidate being resolved did not commit under this
    /// request id. Retrying the same id cannot succeed.
    #[error("publication request id collided with a different candidate: {0}")]
    CandidateCollision(E),
}

/// Authoritative resolution of a previously issued publication request id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationResolution<Version> {
    Published(PublicationReceipt<Version>),
    NotCommitted,
}

/// Atomic durable store for one prepared session mutation and staged outbox.
///
/// `publish` must compare `expected_revision`, write the complete mutation,
/// insert every item with a unique `(request_id, item index)`, and commit those
/// writes in one transaction. Retrying the same request id and candidate
/// fingerprint must be idempotent. A record with the same request id and a
/// different fingerprint is a definite collision: `publish` must return
/// [`PublicationWriteError::Rejected`] and preserve the existing mutation and
/// outbox. It must never deliver an external effect.
#[async_trait::async_trait]
pub trait PublicationStore<Mutation, Payload>: Send + Sync + 'static
where
    Mutation: Sync,
    Payload: Sync,
{
    type Version: Clone + Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    async fn publish(
        &self,
        request: PublicationRequest<'_, Mutation, Payload, Self::Version>,
    ) -> Result<PublicationReceipt<Self::Version>, PublicationWriteError<Self::Error>>;

    /// Resolve an indeterminate publish without rerunning the model or changing
    /// its request id or fingerprint.
    ///
    /// If `request_id` exists with a different fingerprint, this must return
    /// [`PublicationResolveError::CandidateCollision`]. It must never return that receipt or
    /// [`PublicationResolution::NotCommitted`] for the mismatched candidate.
    async fn resolve(
        &self,
        request_id: &PublicationRequestId,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<PublicationResolution<Self::Version>, PublicationResolveError<Self::Error>>;
}

/// Durable publication lifecycle retained by its eventual managed owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationPhase {
    Ready,
    Resolving,
    Published,
}

impl fmt::Display for PublicationPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ready => "ready",
            Self::Resolving => "resolving",
            Self::Published => "published",
        })
    }
}

enum PublicationAttemptState<Mutation, Payload, Version> {
    Ready(StagedPublication<Mutation, Payload, Version>),
    Resolving(StagedPublication<Mutation, Payload, Version>),
    Published(PublicationReceipt<Version>),
    Transitioning,
}

/// Cancellation-aware state machine for one durable publication candidate.
///
/// `publish` changes the phase to [`PublicationPhase::Resolving`] before its
/// first await. Cancelling that future therefore never makes the candidate look
/// safe to abort or replay. A real `MountedAgent` must keep this value in its
/// managed owner; this type alone does not make dropping the final owner safe.
#[must_use = "a durable publication attempt must reach Published or remain owned for resolution"]
pub struct PublicationAttempt<Mutation, Payload, Version> {
    state: PublicationAttemptState<Mutation, Payload, Version>,
}

impl<Mutation, Payload, Version> fmt::Debug for PublicationAttempt<Mutation, Payload, Version>
where
    Version: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("PublicationAttempt");
        debug.field("phase", &self.phase());
        if let Some(candidate) = self.candidate() {
            debug.field("request_id", candidate.request_id());
        }
        if let Some(receipt) = self.receipt() {
            debug.field("receipt", receipt);
        }
        debug.finish_non_exhaustive()
    }
}

impl<Mutation, Payload, Version> PublicationAttempt<Mutation, Payload, Version> {
    pub fn new(candidate: StagedPublication<Mutation, Payload, Version>) -> Self {
        Self {
            state: PublicationAttemptState::Ready(candidate),
        }
    }

    pub fn phase(&self) -> PublicationPhase {
        match self.state {
            PublicationAttemptState::Ready(_) => PublicationPhase::Ready,
            PublicationAttemptState::Resolving(_) | PublicationAttemptState::Transitioning => {
                PublicationPhase::Resolving
            }
            PublicationAttemptState::Published(_) => PublicationPhase::Published,
        }
    }

    pub fn candidate(&self) -> Option<&StagedPublication<Mutation, Payload, Version>> {
        match &self.state {
            PublicationAttemptState::Ready(candidate)
            | PublicationAttemptState::Resolving(candidate) => Some(candidate),
            PublicationAttemptState::Published(_) | PublicationAttemptState::Transitioning => None,
        }
    }

    pub fn receipt(&self) -> Option<&PublicationReceipt<Version>> {
        match &self.state {
            PublicationAttemptState::Published(receipt) => Some(receipt),
            _ => None,
        }
    }

    fn move_candidate(
        &mut self,
        required: PublicationPhase,
        next: impl FnOnce(
            StagedPublication<Mutation, Payload, Version>,
        ) -> PublicationAttemptState<Mutation, Payload, Version>,
    ) -> Result<(), PublicationPhaseError> {
        if self.phase() != required {
            return Err(PublicationPhaseError {
                phase: self.phase(),
                required,
            });
        }
        let current = std::mem::replace(&mut self.state, PublicationAttemptState::Transitioning);
        let candidate = match current {
            PublicationAttemptState::Ready(candidate)
            | PublicationAttemptState::Resolving(candidate) => candidate,
            PublicationAttemptState::Published(receipt) => {
                self.state = PublicationAttemptState::Published(receipt);
                unreachable!("phase preflight accepted a published attempt")
            }
            PublicationAttemptState::Transitioning => {
                unreachable!("publication transitions are synchronous")
            }
        };
        self.state = next(candidate);
        Ok(())
    }

    fn accept_receipt(
        &mut self,
        receipt: PublicationReceipt<Version>,
    ) -> Result<PublicationReceipt<Version>, PublicationReceiptMismatch>
    where
        Version: Clone,
    {
        let candidate = match &self.state {
            PublicationAttemptState::Resolving(candidate) => candidate,
            _ => unreachable!("only a resolving attempt can accept a receipt"),
        };
        candidate.validate_receipt(&receipt)?;
        self.state = PublicationAttemptState::Published(receipt.clone());
        Ok(receipt)
    }

    /// Start or retry the same durable transaction.
    ///
    /// The candidate enters `Resolving` before the store future is polled, so
    /// cancellation requires `resolve` rather than abort or provider replay.
    pub async fn publish<S>(
        &mut self,
        store: &S,
    ) -> Result<PublicationReceipt<Version>, PublicationAttemptError<S::Error>>
    where
        Mutation: Sync,
        Payload: Sync,
        Version: Clone + Send + Sync + 'static,
        S: PublicationStore<Mutation, Payload, Version = Version>,
    {
        self.move_candidate(PublicationPhase::Ready, PublicationAttemptState::Resolving)?;
        let request = match &self.state {
            PublicationAttemptState::Resolving(candidate) => candidate.as_request(),
            _ => unreachable!("ready attempt just entered resolving"),
        };
        match store.publish(request).await {
            Ok(receipt) => self
                .accept_receipt(receipt)
                .map_err(PublicationAttemptError::Receipt),
            Err(PublicationWriteError::Conflict(source)) => {
                self.move_candidate(PublicationPhase::Resolving, PublicationAttemptState::Ready)?;
                Err(PublicationAttemptError::Conflict(source))
            }
            Err(PublicationWriteError::Rejected(source)) => {
                self.move_candidate(PublicationPhase::Resolving, PublicationAttemptState::Ready)?;
                Err(PublicationAttemptError::Rejected(source))
            }
            Err(PublicationWriteError::Indeterminate(source)) => {
                Err(PublicationAttemptError::Indeterminate(source))
            }
        }
    }

    /// Resolve a cancelled or indeterminate publish using its original request
    /// id and candidate fingerprint. `NotCommitted` is the only resolution
    /// that returns to `Ready`.
    pub async fn resolve<S>(
        &mut self,
        store: &S,
    ) -> Result<PublicationResolution<Version>, PublicationAttemptError<S::Error>>
    where
        Mutation: Sync,
        Payload: Sync,
        Version: Clone + Send + Sync + 'static,
        S: PublicationStore<Mutation, Payload, Version = Version>,
    {
        if self.phase() != PublicationPhase::Resolving {
            return Err(PublicationPhaseError {
                phase: self.phase(),
                required: PublicationPhase::Resolving,
            }
            .into());
        }
        let (request_id, fingerprint) = {
            let candidate = self
                .candidate()
                .expect("resolving attempt retains its candidate");
            (
                candidate.request_id().clone(),
                candidate.fingerprint().clone(),
            )
        };
        match store.resolve(&request_id, &fingerprint).await {
            Ok(PublicationResolution::Published(receipt)) => {
                let receipt = self
                    .accept_receipt(receipt)
                    .map_err(PublicationAttemptError::Receipt)?;
                Ok(PublicationResolution::Published(receipt))
            }
            Ok(PublicationResolution::NotCommitted) => {
                self.move_candidate(PublicationPhase::Resolving, PublicationAttemptState::Ready)?;
                Ok(PublicationResolution::NotCommitted)
            }
            Err(source @ PublicationResolveError::CandidateCollision(_)) => {
                self.move_candidate(PublicationPhase::Resolving, PublicationAttemptState::Ready)?;
                Err(PublicationAttemptError::Resolve(source))
            }
            Err(source @ PublicationResolveError::Unavailable(_)) => {
                Err(PublicationAttemptError::Resolve(source))
            }
        }
    }

    /// Recover a candidate only after the store authoritatively says it was not
    /// committed. Resolving and Published states deliberately cannot be taken.
    pub fn into_ready_candidate(
        mut self,
    ) -> Result<StagedPublication<Mutation, Payload, Version>, Box<Self>> {
        if !matches!(self.state, PublicationAttemptState::Ready(_)) {
            return Err(Box::new(self));
        }
        let state = std::mem::replace(&mut self.state, PublicationAttemptState::Transitioning);
        match state {
            PublicationAttemptState::Ready(candidate) => Ok(candidate),
            _ => unreachable!("ready phase preflight matched the state"),
        }
    }
}

/// Invalid operation for the current durable publication phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("publication is {phase}, but this operation requires {required}")]
pub struct PublicationPhaseError {
    phase: PublicationPhase,
    required: PublicationPhase,
}

impl PublicationPhaseError {
    pub fn phase(&self) -> PublicationPhase {
        self.phase
    }

    pub fn required(&self) -> PublicationPhase {
        self.required
    }
}

/// A store returned a receipt for a different durable request, fingerprint, or
/// item count.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicationReceiptMismatch {
    #[error("publication receipt request id mismatch: expected `{expected}`, got `{actual}`")]
    RequestId {
        expected: PublicationRequestId,
        actual: PublicationRequestId,
    },

    #[error("publication receipt fingerprint mismatch: expected `{expected}`, got `{actual}`")]
    Fingerprint {
        expected: PublicationCandidateFingerprint,
        actual: PublicationCandidateFingerprint,
    },

    #[error("publication receipt outbox count mismatch: expected {expected}, got {actual}")]
    OutboxCount { expected: u64, actual: u64 },
}

/// Recoverable durable publication transition failure.
#[derive(Debug, thiserror::Error)]
pub enum PublicationAttemptError<E>
where
    E: Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Phase(#[from] PublicationPhaseError),

    #[error("publication revision conflicted with the authoritative session: {0}")]
    Conflict(E),

    #[error("publication was definitely rejected: {0}")]
    Rejected(E),

    #[error("publication outcome is indeterminate and requires resolution: {0}")]
    Indeterminate(E),

    #[error(transparent)]
    Resolve(#[from] PublicationResolveError<E>),

    #[error(transparent)]
    Receipt(#[from] PublicationReceiptMismatch),
}

/// Durable staging failure that retains the finished, still-abortable attempt.
#[must_use = "a staging failure retains a finished attempt that must be retried or aborted"]
pub struct StreamingPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    finished: Box<FinishedStreamingAttempt<C>>,
    plan: PublicationStagingPlan<Mutation, Version>,
    source: PublicationStagingFailure<CommitError, FingerprintError>,
}

/// Typed result of staging a streaming attempt for durable publication.
pub type StreamingPublicationStagingResult<
    C,
    Mutation,
    Payload,
    Version,
    CommitError,
    FingerprintError,
> = Result<
    StagedStreamingPublication<C, Mutation, Payload, Version>,
    StreamingPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>,
>;

impl<C, Mutation, Version, CommitError, FingerprintError>
    StreamingPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    pub(crate) fn new(
        finished: FinishedStreamingAttempt<C>,
        plan: PublicationStagingPlan<Mutation, Version>,
        source: PublicationStagingFailure<CommitError, FingerprintError>,
    ) -> Self {
        Self {
            finished: Box::new(finished),
            plan,
            source,
        }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.finished.identity()
    }

    pub fn source_error(&self) -> &PublicationStagingFailure<CommitError, FingerprintError> {
        &self.source
    }

    pub fn plan(&self) -> &PublicationStagingPlan<Mutation, Version> {
        &self.plan
    }

    pub fn into_parts(
        self,
    ) -> (
        FinishedStreamingAttempt<C>,
        PublicationStagingPlan<Mutation, Version>,
        PublicationStagingFailure<CommitError, FingerprintError>,
    ) {
        (*self.finished, self.plan, self.source)
    }

    pub async fn abort(self, reason: BindingAbortReason) -> StreamingAbortReport {
        self.finished.abort(reason).await
    }
}

impl<C, Mutation, Version, CommitError, FingerprintError> fmt::Debug
    for StreamingPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingPublicationStagingFailure")
            .field("identity", self.identity())
            .field("source", &self.source)
            .finish()
    }
}

impl<C, Mutation, Version, CommitError, FingerprintError> fmt::Display
    for StreamingPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "durable publication staging failed for provider attempt `{}`: {}",
            self.identity().provider_attempt_id(),
            self.source
        )
    }
}

impl<C, Mutation, Version, CommitError, FingerprintError> Error
    for StreamingPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Finished streaming attempt plus its fully staged durable publication.
///
/// Store operations borrow this value rather than consuming it. If a publish
/// waiter is cancelled, [`PublicationPhase::Resolving`] remains observable and
/// the same request id must be resolved before this attempt can be aborted or
/// retried.
#[must_use = "a staged streaming publication must be published, resolved, or recovered while Ready"]
pub struct StagedStreamingPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
{
    finished: FinishedStreamingAttempt<C>,
    publication: PublicationAttempt<Mutation, Payload, Version>,
}

type StreamingFinishedAndPlan<C, Mutation, Version> = (
    FinishedStreamingAttempt<C>,
    PublicationStagingPlan<Mutation, Version>,
);

type StreamingReadyRecovery<C, Mutation, Payload, Version> = Result<
    StreamingFinishedAndPlan<C, Mutation, Version>,
    Box<StagedStreamingPublication<C, Mutation, Payload, Version>>,
>;

impl<C, Mutation, Payload, Version> StagedStreamingPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
{
    pub(crate) fn new(
        finished: FinishedStreamingAttempt<C>,
        candidate: StagedPublication<Mutation, Payload, Version>,
    ) -> Self {
        Self {
            finished,
            publication: PublicationAttempt::new(candidate),
        }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.finished.identity()
    }

    pub fn phase(&self) -> PublicationPhase {
        self.publication.phase()
    }

    pub fn candidate(&self) -> Option<&StagedPublication<Mutation, Payload, Version>> {
        self.publication.candidate()
    }

    pub fn receipt(&self) -> Option<&PublicationReceipt<Version>> {
        self.publication.receipt()
    }

    pub async fn publish<S>(
        &mut self,
        store: &S,
    ) -> Result<PublicationReceipt<Version>, PublicationAttemptError<S::Error>>
    where
        Mutation: Sync,
        Payload: Sync,
        Version: Clone + Send + Sync + 'static,
        S: PublicationStore<Mutation, Payload, Version = Version>,
    {
        self.publication.publish(store).await
    }

    pub async fn resolve<S>(
        &mut self,
        store: &S,
    ) -> Result<PublicationResolution<Version>, PublicationAttemptError<S::Error>>
    where
        Mutation: Sync,
        Payload: Sync,
        Version: Clone + Send + Sync + 'static,
        S: PublicationStore<Mutation, Payload, Version = Version>,
    {
        self.publication.resolve(store).await
    }

    /// Recover the original finished attempt only when publication is known not
    /// to have committed. A Resolving or Published value is returned unchanged.
    pub fn into_finished_if_ready(self) -> Result<FinishedStreamingAttempt<C>, Box<Self>> {
        self.into_finished_and_plan_if_ready()
            .map(|(finished, _plan)| finished)
    }

    pub(crate) fn into_finished_and_plan_if_ready(
        self,
    ) -> StreamingReadyRecovery<C, Mutation, Payload, Version> {
        let Self {
            finished,
            publication,
        } = self;
        match publication.into_ready_candidate() {
            Ok(candidate) => Ok((finished, candidate.into_staging_plan())),
            Err(publication) => Err(Box::new(Self {
                finished,
                publication: *publication,
            })),
        }
    }

    /// Finish the in-memory lifecycle after the durable receipt exists.
    ///
    /// Commit values are discarded here because the store transaction already
    /// owns their staged payloads. They are never exposed to the caller.
    pub async fn into_published(self) -> Result<DurablyPublishedStreamingAttempt<C, Version>, Self>
    where
        Version: Clone,
    {
        let Some(receipt) = self.publication.receipt().cloned() else {
            return Err(self);
        };
        let Self {
            finished,
            publication: _,
        } = self;
        let (raw_output, provider_results, update) = finished.into_durable_parts().await;
        Ok(DurablyPublishedStreamingAttempt {
            receipt,
            raw_output,
            provider_results,
            update,
        })
    }
}

impl<C, Mutation, Payload, Version> fmt::Debug
    for StagedStreamingPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
    Version: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedStreamingPublication")
            .field("identity", self.identity())
            .field("publication", &self.publication)
            .finish_non_exhaustive()
    }
}

/// Streaming outcome whose session mutation and Commit outbox are durable.
///
/// This type intentionally has no `pending_commits` or delivery method. The
/// outbox worker, not the request future, owns external effect delivery.
#[must_use = "a durable publication receipt should be observed by the mounted owner"]
pub struct DurablyPublishedStreamingAttempt<C, Version>
where
    C: TurnChannels,
{
    receipt: PublicationReceipt<Version>,
    raw_output: String,
    provider_results: Vec<ProviderToolResult>,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
}

impl<C, Version> DurablyPublishedStreamingAttempt<C, Version>
where
    C: TurnChannels,
{
    pub fn receipt(&self) -> &PublicationReceipt<Version> {
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
}

impl<C, Version> fmt::Debug for DurablyPublishedStreamingAttempt<C, Version>
where
    C: TurnChannels,
    Version: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurablyPublishedStreamingAttempt")
            .field("receipt", &self.receipt)
            .field("raw_output_len", &self.raw_output.len())
            .field("provider_results", &self.provider_results.len())
            .finish_non_exhaustive()
    }
}

/// Durable staging failure that retains a native-only finished attempt.
#[must_use = "a staging failure retains a finished provider attempt that must be retried or aborted"]
pub struct ProviderPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    finished: Box<FinishedProviderAttempt<C>>,
    plan: PublicationStagingPlan<Mutation, Version>,
    source: PublicationStagingFailure<CommitError, FingerprintError>,
}

/// Typed result of staging a native provider attempt for durable publication.
pub type ProviderPublicationStagingResult<
    C,
    Mutation,
    Payload,
    Version,
    CommitError,
    FingerprintError,
> = Result<
    StagedProviderPublication<C, Mutation, Payload, Version>,
    ProviderPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>,
>;

impl<C, Mutation, Version, CommitError, FingerprintError>
    ProviderPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    pub(crate) fn new(
        finished: FinishedProviderAttempt<C>,
        plan: PublicationStagingPlan<Mutation, Version>,
        source: PublicationStagingFailure<CommitError, FingerprintError>,
    ) -> Self {
        Self {
            finished: Box::new(finished),
            plan,
            source,
        }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.finished.identity()
    }

    pub fn source_error(&self) -> &PublicationStagingFailure<CommitError, FingerprintError> {
        &self.source
    }

    pub fn plan(&self) -> &PublicationStagingPlan<Mutation, Version> {
        &self.plan
    }

    pub fn into_parts(
        self,
    ) -> (
        FinishedProviderAttempt<C>,
        PublicationStagingPlan<Mutation, Version>,
        PublicationStagingFailure<CommitError, FingerprintError>,
    ) {
        (*self.finished, self.plan, self.source)
    }

    pub async fn abort(self, reason: BindingAbortReason) -> ProviderAttemptAbortReport {
        self.finished.abort(reason).await
    }
}

impl<C, Mutation, Version, CommitError, FingerprintError> fmt::Debug
    for ProviderPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderPublicationStagingFailure")
            .field("identity", self.identity())
            .field("source", &self.source)
            .finish()
    }
}

impl<C, Mutation, Version, CommitError, FingerprintError> fmt::Display
    for ProviderPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "durable publication staging failed for native provider attempt `{}`: {}",
            self.identity().provider_attempt_id(),
            self.source
        )
    }
}

impl<C, Mutation, Version, CommitError, FingerprintError> Error
    for ProviderPublicationStagingFailure<C, Mutation, Version, CommitError, FingerprintError>
where
    C: TurnChannels,
    CommitError: Error + Send + Sync + 'static,
    FingerprintError: Error + Send + Sync + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Native-only finished attempt plus its staged durable publication.
#[must_use = "a staged provider publication must be published, resolved, or recovered while Ready"]
pub struct StagedProviderPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
{
    finished: FinishedProviderAttempt<C>,
    publication: PublicationAttempt<Mutation, Payload, Version>,
}

type ProviderFinishedAndPlan<C, Mutation, Version> = (
    FinishedProviderAttempt<C>,
    PublicationStagingPlan<Mutation, Version>,
);

type ProviderReadyRecovery<C, Mutation, Payload, Version> = Result<
    ProviderFinishedAndPlan<C, Mutation, Version>,
    Box<StagedProviderPublication<C, Mutation, Payload, Version>>,
>;

impl<C, Mutation, Payload, Version> StagedProviderPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
{
    pub(crate) fn new(
        finished: FinishedProviderAttempt<C>,
        candidate: StagedPublication<Mutation, Payload, Version>,
    ) -> Self {
        Self {
            finished,
            publication: PublicationAttempt::new(candidate),
        }
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.finished.identity()
    }

    pub fn phase(&self) -> PublicationPhase {
        self.publication.phase()
    }

    pub fn candidate(&self) -> Option<&StagedPublication<Mutation, Payload, Version>> {
        self.publication.candidate()
    }

    pub fn receipt(&self) -> Option<&PublicationReceipt<Version>> {
        self.publication.receipt()
    }

    pub async fn publish<S>(
        &mut self,
        store: &S,
    ) -> Result<PublicationReceipt<Version>, PublicationAttemptError<S::Error>>
    where
        Mutation: Sync,
        Payload: Sync,
        Version: Clone + Send + Sync + 'static,
        S: PublicationStore<Mutation, Payload, Version = Version>,
    {
        self.publication.publish(store).await
    }

    pub async fn resolve<S>(
        &mut self,
        store: &S,
    ) -> Result<PublicationResolution<Version>, PublicationAttemptError<S::Error>>
    where
        Mutation: Sync,
        Payload: Sync,
        Version: Clone + Send + Sync + 'static,
        S: PublicationStore<Mutation, Payload, Version = Version>,
    {
        self.publication.resolve(store).await
    }

    pub fn into_finished_if_ready(self) -> Result<FinishedProviderAttempt<C>, Box<Self>> {
        self.into_finished_and_plan_if_ready()
            .map(|(finished, _plan)| finished)
    }

    pub(crate) fn into_finished_and_plan_if_ready(
        self,
    ) -> ProviderReadyRecovery<C, Mutation, Payload, Version> {
        let Self {
            finished,
            publication,
        } = self;
        match publication.into_ready_candidate() {
            Ok(candidate) => Ok((finished, candidate.into_staging_plan())),
            Err(publication) => Err(Box::new(Self {
                finished,
                publication: *publication,
            })),
        }
    }

    pub async fn into_published(self) -> Result<DurablyPublishedProviderAttempt<C, Version>, Self>
    where
        Version: Clone,
    {
        let Some(receipt) = self.publication.receipt().cloned() else {
            return Err(self);
        };
        let Self {
            finished,
            publication: _,
        } = self;
        let (results, update) = finished.into_durable_parts().await;
        Ok(DurablyPublishedProviderAttempt {
            receipt,
            results,
            update,
        })
    }
}

impl<C, Mutation, Payload, Version> fmt::Debug
    for StagedProviderPublication<C, Mutation, Payload, Version>
where
    C: TurnChannels,
    Version: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedProviderPublication")
            .field("identity", self.identity())
            .field("publication", &self.publication)
            .finish_non_exhaustive()
    }
}

/// Native-only outcome whose session mutation and Commit outbox are durable.
#[must_use = "a durable publication receipt should be observed by the mounted owner"]
pub struct DurablyPublishedProviderAttempt<C, Version>
where
    C: TurnChannels,
{
    receipt: PublicationReceipt<Version>,
    results: Vec<ProviderToolResult>,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
}

impl<C, Version> DurablyPublishedProviderAttempt<C, Version>
where
    C: TurnChannels,
{
    pub fn receipt(&self) -> &PublicationReceipt<Version> {
        &self.receipt
    }

    pub fn results(&self) -> &[ProviderToolResult] {
        &self.results
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        std::mem::take(&mut self.update)
    }
}

impl<C, Version> fmt::Debug for DurablyPublishedProviderAttempt<C, Version>
where
    C: TurnChannels,
    Version: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurablyPublishedProviderAttempt")
            .field("receipt", &self.receipt)
            .field("provider_results", &self.results.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use tokio::sync::Notify;

    use super::*;
    use crate::component::{HarnessEpochId, TurnInstanceId};

    struct TestChannels;

    impl TurnChannels for TestChannels {
        type Output = ();
        type Live = ();
        type Commit = u32;
        type Diagnostic = ();
    }

    struct NumberStager;

    impl CommitStager<TestChannels> for NumberStager {
        type Payload = String;
        type Error = Infallible;

        fn stage(
            &self,
            context: CommitStagingContext<'_>,
            commit: &u32,
        ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
            Ok(StagedCommit::new(
                CommitContract::new("test.number", 1).unwrap(),
                format!("{}={commit}", context.item_id()),
            ))
        }
    }

    fn attempt_identity() -> ProviderAttemptIdentity {
        ProviderAttemptIdentity::fresh(
            HarnessEpochId::fresh(),
            TurnInstanceId::fresh(),
            "publication-test".into(),
        )
    }

    // This is deliberately test-only host policy. Production hosts own the
    // canonical encoding for their mutation and outbox payload types.
    fn test_canonical_fingerprint(
        expected_revision: u64,
        mutation: &PreparedSessionMutation<String>,
        raw_output: &str,
        provider_results: &[ProviderToolResult],
        outbox: &StagedOutbox<String>,
    ) -> PublicationCandidateFingerprint {
        let provider_results = provider_results
            .iter()
            .map(|result| {
                format!(
                    "{{invocation_id={:?},correlation_id={:?},name={:?},response={:?}}}",
                    result.invocation_id(),
                    result.result_correlation_id(),
                    result.name(),
                    result.response(),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let outbox = outbox
            .items()
            .iter()
            .map(|item| {
                format!(
                    "{{index={},contract_key={:?},contract_schema_version={},payload={:?}}}",
                    item.id().index(),
                    item.contract().key(),
                    item.contract().schema_version(),
                    item.payload(),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        PublicationCandidateFingerprint::new(format!(
            "v1|expected_revision={expected_revision}|mutation={:?}|raw_output={raw_output:?}|provider_results=[{provider_results}]|outbox=[{outbox}]",
            mutation.value(),
        ))
        .unwrap()
    }

    fn candidate_with(
        request: &str,
        revision: u64,
        mutation: &str,
        raw_output: &str,
        commits: &[u32],
    ) -> StagedPublication<String, String, u64> {
        let request_id = PublicationRequestId::new(request).unwrap();
        let outbox =
            stage_commit_outbox::<TestChannels, _>(request_id.clone(), commits, &NumberStager)
                .unwrap();
        let mutation = PreparedSessionMutation::new(mutation.to_owned());
        let provider_results = Vec::new();
        let fingerprint =
            test_canonical_fingerprint(revision, &mutation, raw_output, &provider_results, &outbox);
        StagedPublication::new(
            request_id,
            fingerprint,
            revision,
            mutation,
            attempt_identity(),
            raw_output,
            provider_results,
            outbox,
        )
        .unwrap()
    }

    fn candidate(
        request: &str,
        revision: u64,
        commits: &[u32],
    ) -> StagedPublication<String, String, u64> {
        candidate_with(
            request,
            revision,
            "next-session",
            "assistant output",
            commits,
        )
    }

    #[test]
    fn staging_preserves_order_contract_and_stable_item_ids() {
        let candidate = candidate("request-a", 4, &[7, 9]);
        let items = candidate.outbox().items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id().request_id().as_str(), "request-a");
        assert_eq!(items[0].id().index(), 0);
        assert_eq!(items[1].id().index(), 1);
        assert_eq!(items[0].contract().key(), "test.number");
        assert_eq!(items[0].contract().schema_version(), 1);
        assert_eq!(items[0].payload(), "request-a:0=7");
        assert_eq!(items[1].payload(), "request-a:1=9");
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    enum StoreFailure {
        #[error("scripted store failure")]
        Scripted,

        #[error("publication request id was reused with a different candidate fingerprint")]
        FingerprintCollision,
    }

    #[derive(Clone, Copy)]
    enum PublishBehavior {
        Commit,
        Reject,
        CommitThenIndeterminate,
    }

    struct StoredRecord {
        receipt: PublicationReceipt<u64>,
        mutation: String,
        payloads: Vec<String>,
    }

    struct MemoryStore {
        behavior: Mutex<PublishBehavior>,
        records: Mutex<HashMap<PublicationRequestId, StoredRecord>>,
        publish_calls: Mutex<usize>,
    }

    impl MemoryStore {
        fn new(behavior: PublishBehavior) -> Self {
            Self {
                behavior: Mutex::new(behavior),
                records: Mutex::new(HashMap::new()),
                publish_calls: Mutex::new(0),
            }
        }

        fn set_behavior(&self, behavior: PublishBehavior) {
            *self.behavior.lock().unwrap() = behavior;
        }

        fn publish_calls(&self) -> usize {
            *self.publish_calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl PublicationStore<String, String> for MemoryStore {
        type Version = u64;
        type Error = StoreFailure;

        async fn publish(
            &self,
            request: PublicationRequest<'_, String, String, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            *self.publish_calls.lock().unwrap() += 1;
            let behavior = *self.behavior.lock().unwrap();
            if matches!(behavior, PublishBehavior::Reject) {
                return Err(PublicationWriteError::Rejected(StoreFailure::Scripted));
            }

            let mut records = self.records.lock().unwrap();
            let receipt = if let Some(record) = records.get(request.request_id()) {
                if record.receipt.fingerprint() != request.fingerprint() {
                    return Err(PublicationWriteError::Rejected(
                        StoreFailure::FingerprintCollision,
                    ));
                }
                record.receipt.clone()
            } else {
                let next_revision = request.expected_revision() + 1;
                let receipt = PublicationReceipt::new(
                    request.request_id().clone(),
                    request.fingerprint().clone(),
                    PublicationId::new(format!("publication-{next_revision}")).unwrap(),
                    next_revision,
                    u64::try_from(request.outbox().len()).unwrap(),
                );
                records.insert(
                    request.request_id().clone(),
                    StoredRecord {
                        receipt: receipt.clone(),
                        mutation: request.mutation().value().clone(),
                        payloads: request
                            .outbox()
                            .items()
                            .iter()
                            .map(|item| item.payload().clone())
                            .collect(),
                    },
                );
                receipt
            };
            drop(records);

            if matches!(behavior, PublishBehavior::CommitThenIndeterminate) {
                Err(PublicationWriteError::Indeterminate(StoreFailure::Scripted))
            } else {
                Ok(receipt)
            }
        }

        async fn resolve(
            &self,
            request_id: &PublicationRequestId,
            fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            match self.records.lock().unwrap().get(request_id) {
                Some(record) if record.receipt.fingerprint() == fingerprint => {
                    Ok(PublicationResolution::Published(record.receipt.clone()))
                }
                Some(_) => Err(PublicationResolveError::CandidateCollision(
                    StoreFailure::FingerprintCollision,
                )),
                None => Ok(PublicationResolution::NotCommitted),
            }
        }
    }

    #[tokio::test]
    async fn indeterminate_commit_resolves_without_republishing_session_or_outbox() {
        let store = MemoryStore::new(PublishBehavior::CommitThenIndeterminate);
        let mut publication = PublicationAttempt::new(candidate("request-b", 10, &[3, 5]));

        let error = publication.publish(&store).await.unwrap_err();
        assert!(matches!(error, PublicationAttemptError::Indeterminate(_)));
        assert_eq!(publication.phase(), PublicationPhase::Resolving);
        assert_eq!(store.publish_calls(), 1);

        let resolution = publication.resolve(&store).await.unwrap();
        let PublicationResolution::Published(receipt) = resolution else {
            panic!("committed request must resolve as published");
        };
        assert_eq!(receipt.session_revision(), &11);
        assert_eq!(receipt.outbox_count(), 2);
        assert_eq!(publication.phase(), PublicationPhase::Published);
        assert_eq!(store.publish_calls(), 1);

        let records = store.records.lock().unwrap();
        let record = records
            .get(&PublicationRequestId::new("request-b").unwrap())
            .unwrap();
        assert_eq!(record.mutation, "next-session");
        assert_eq!(record.payloads, ["request-b:0=3", "request-b:1=5"]);
    }

    #[tokio::test]
    async fn same_request_and_fingerprint_reuses_one_session_and_outbox_record() {
        let store = MemoryStore::new(PublishBehavior::Commit);
        let mut first = PublicationAttempt::new(candidate("request-idempotent", 4, &[7, 9]));
        let first_attempt = first.candidate().unwrap().attempt().clone();
        let first_fingerprint = first.candidate().unwrap().fingerprint().clone();
        let first_receipt = first.publish(&store).await.unwrap();

        let mut retry = PublicationAttempt::new(candidate("request-idempotent", 4, &[7, 9]));
        assert_ne!(retry.candidate().unwrap().attempt(), &first_attempt);
        assert_eq!(retry.candidate().unwrap().fingerprint(), &first_fingerprint);
        let retry_receipt = retry.publish(&store).await.unwrap();

        assert_eq!(retry_receipt, first_receipt);
        assert_eq!(store.publish_calls(), 2);
        let records = store.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        let record = records
            .get(&PublicationRequestId::new("request-idempotent").unwrap())
            .unwrap();
        assert_eq!(record.mutation, "next-session");
        assert_eq!(
            record.payloads,
            ["request-idempotent:0=7", "request-idempotent:1=9"]
        );
    }

    #[tokio::test]
    async fn same_request_with_different_fingerprint_is_rejected_without_overwrite() {
        let store = MemoryStore::new(PublishBehavior::Commit);
        let mut first = PublicationAttempt::new(candidate_with(
            "request-collision",
            4,
            "first-session",
            "first output",
            &[7],
        ));
        first.publish(&store).await.unwrap();

        let mut collision = PublicationAttempt::new(candidate_with(
            "request-collision",
            4,
            "second-session",
            "second output",
            &[9],
        ));
        let error = collision.publish(&store).await.unwrap_err();
        assert!(matches!(
            error,
            PublicationAttemptError::Rejected(StoreFailure::FingerprintCollision)
        ));
        assert_eq!(collision.phase(), PublicationPhase::Ready);

        let records = store.records.lock().unwrap();
        let record = records
            .get(&PublicationRequestId::new("request-collision").unwrap())
            .unwrap();
        assert_eq!(record.mutation, "first-session");
        assert_eq!(record.payloads, ["request-collision:0=7"]);
    }

    #[tokio::test]
    async fn resolve_rejects_a_request_id_collision_instead_of_reusing_a_receipt() {
        let store = MemoryStore::new(PublishBehavior::Commit);
        let mut first = PublicationAttempt::new(candidate("request-resolve-collision", 4, &[7]));
        first.publish(&store).await.unwrap();

        let different = candidate("request-resolve-collision", 4, &[9]);
        let error = store
            .resolve(different.request_id(), different.fingerprint())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            PublicationResolveError::CandidateCollision(StoreFailure::FingerprintCollision)
        );
    }

    struct ResolveCollisionStore;

    #[async_trait::async_trait]
    impl PublicationStore<String, String> for ResolveCollisionStore {
        type Version = u64;
        type Error = StoreFailure;

        async fn publish(
            &self,
            _request: PublicationRequest<'_, String, String, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            Err(PublicationWriteError::Indeterminate(StoreFailure::Scripted))
        }

        async fn resolve(
            &self,
            _request_id: &PublicationRequestId,
            _fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            Err(PublicationResolveError::CandidateCollision(
                StoreFailure::FingerprintCollision,
            ))
        }
    }

    #[tokio::test]
    async fn resolve_collision_returns_candidate_to_ready_for_terminal_abort() {
        let mut publication =
            PublicationAttempt::new(candidate("request-collision-ready", 4, &[7]));
        assert!(matches!(
            publication.publish(&ResolveCollisionStore).await,
            Err(PublicationAttemptError::Indeterminate(_))
        ));

        let error = publication
            .resolve(&ResolveCollisionStore)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PublicationAttemptError::Resolve(PublicationResolveError::CandidateCollision(
                StoreFailure::FingerprintCollision
            ))
        ));
        assert_eq!(publication.phase(), PublicationPhase::Ready);
    }

    #[tokio::test]
    async fn known_rejection_returns_to_ready_and_retries_same_candidate() {
        let store = MemoryStore::new(PublishBehavior::Reject);
        let mut publication = PublicationAttempt::new(candidate("request-c", 2, &[8]));

        let error = publication.publish(&store).await.unwrap_err();
        assert!(matches!(error, PublicationAttemptError::Rejected(_)));
        assert_eq!(publication.phase(), PublicationPhase::Ready);
        assert_eq!(
            publication.candidate().unwrap().request_id().as_str(),
            "request-c"
        );

        store.set_behavior(PublishBehavior::Commit);
        let receipt = publication.publish(&store).await.unwrap();
        assert_eq!(receipt.request_id().as_str(), "request-c");
        assert_eq!(publication.phase(), PublicationPhase::Published);
        assert_eq!(store.publish_calls(), 2);
    }

    struct WrongFingerprintReceiptStore;

    #[async_trait::async_trait]
    impl PublicationStore<String, String> for WrongFingerprintReceiptStore {
        type Version = u64;
        type Error = Infallible;

        async fn publish(
            &self,
            request: PublicationRequest<'_, String, String, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            Ok(PublicationReceipt::new(
                request.request_id().clone(),
                PublicationCandidateFingerprint::new("wrong-fingerprint").unwrap(),
                PublicationId::new("wrong-fingerprint-publication").unwrap(),
                request.expected_revision() + 1,
                u64::try_from(request.outbox().len()).unwrap(),
            ))
        }

        async fn resolve(
            &self,
            _request_id: &PublicationRequestId,
            _fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            Ok(PublicationResolution::NotCommitted)
        }
    }

    #[tokio::test]
    async fn receipt_with_a_different_fingerprint_is_rejected() {
        let mut publication = PublicationAttempt::new(candidate("request-bad-receipt", 1, &[3]));

        let error = publication
            .publish(&WrongFingerprintReceiptStore)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PublicationAttemptError::Receipt(PublicationReceiptMismatch::Fingerprint { .. })
        ));
        assert_eq!(publication.phase(), PublicationPhase::Resolving);
    }

    struct CancelledStore {
        entered: Arc<Notify>,
        block: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl PublicationStore<String, String> for CancelledStore {
        type Version = u64;
        type Error = StoreFailure;

        async fn publish(
            &self,
            _request: PublicationRequest<'_, String, String, u64>,
        ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
            self.entered.notify_one();
            self.block.notified().await;
            unreachable!("the test cancels this store future")
        }

        async fn resolve(
            &self,
            _request_id: &PublicationRequestId,
            _fingerprint: &PublicationCandidateFingerprint,
        ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
            Ok(PublicationResolution::NotCommitted)
        }
    }

    #[tokio::test]
    async fn cancelling_publish_waiter_leaves_attempt_in_resolving_phase() {
        let entered = Arc::new(Notify::new());
        let store = CancelledStore {
            entered: Arc::clone(&entered),
            block: Arc::new(Notify::new()),
        };
        let mut publication = PublicationAttempt::new(candidate("request-d", 1, &[]));

        let mut publish = Box::pin(publication.publish(&store));
        tokio::select! {
            _ = entered.notified() => {}
            result = &mut publish => panic!("publish unexpectedly completed: {result:?}"),
        }
        drop(publish);

        assert_eq!(publication.phase(), PublicationPhase::Resolving);
        assert!(matches!(
            publication.resolve(&store).await.unwrap(),
            PublicationResolution::NotCommitted
        ));
        assert_eq!(publication.phase(), PublicationPhase::Ready);
    }

    #[test]
    fn durable_identity_contracts_reject_empty_and_control_character_keys() {
        for invalid in ["", "line\nbreak", "carriage\rreturn"] {
            assert!(DurableSessionId::new(invalid).is_err());
            assert!(DurableEpochId::new(invalid).is_err());
            assert!(DurableCallId::new(invalid).is_err());
            assert!(DurableCallInputId::new(invalid).is_err());
            assert!(EpochContractId::new(invalid).is_err());
            assert!(PublicationRequestId::new(invalid).is_err());
        }

        let session_id = DurableSessionId::new("forgotten-city/session-42").unwrap();
        let epoch_id = DurableEpochId::new("forgotten-city/session-42/epoch-1").unwrap();
        let epoch_contract_id = EpochContractId::new("player/v3").unwrap();
        let call_id = DurableCallId::new("request-9").unwrap();
        let context = PublicationRequestContext::new(&session_id, &epoch_contract_id, &call_id, 7);

        assert_eq!(context.session_id(), &session_id);
        assert_eq!(epoch_id.as_str(), "forgotten-city/session-42/epoch-1");
        assert_eq!(context.epoch_contract_id(), &epoch_contract_id);
        assert_eq!(context.call_id(), &call_id);
        assert_eq!(context.turn_index(), 7);
    }

    #[test]
    fn remote_provider_idempotency_key_scopes_the_complete_durable_operation() {
        let operation =
            |session: &str, epoch: &str, call: &str, input: &str, turn: u64, request: &str| {
                ProviderOperationIdentity::new(
                    DurableSessionId::new(session).unwrap(),
                    DurableEpochId::new(epoch).unwrap(),
                    DurableCallId::new(call).unwrap(),
                    DurableCallInputId::new(input).unwrap(),
                    turn,
                    PublicationRequestId::new(request).unwrap(),
                )
            };

        let base = operation("session-a", "epoch-a", "call-a", "input-a", 3, "request-a");
        let same_operation = operation("session-a", "epoch-a", "call-a", "input-a", 3, "request-a");

        let key = base.remote_idempotency_key();
        assert!(key.starts_with("agentview.provider-operation.v1:"));
        assert_eq!(key, same_operation.remote_idempotency_key());
        assert_eq!(base.publication_request_id(), "request-a");
        assert_ne!(
            key,
            operation("session-b", "epoch-a", "call-a", "input-a", 3, "request-a")
                .remote_idempotency_key()
        );
        assert_ne!(
            key,
            operation("session-a", "epoch-b", "call-a", "input-a", 3, "request-a")
                .remote_idempotency_key()
        );
        assert_ne!(
            key,
            operation("session-a", "epoch-a", "call-b", "input-a", 3, "request-a")
                .remote_idempotency_key()
        );
        assert_ne!(
            key,
            operation("session-a", "epoch-a", "call-a", "input-b", 3, "request-a")
                .remote_idempotency_key()
        );
        assert_ne!(
            key,
            operation("session-a", "epoch-a", "call-a", "input-a", 4, "request-a")
                .remote_idempotency_key()
        );
        assert_ne!(
            key,
            operation("session-a", "epoch-a", "call-a", "input-a", 3, "request-b")
                .remote_idempotency_key()
        );
    }

    #[test]
    fn invalid_candidate_rejects_mixed_request_ids() {
        let outbox = stage_commit_outbox::<TestChannels, _>(
            PublicationRequestId::new("outbox-id").unwrap(),
            &[1],
            &NumberStager,
        )
        .unwrap();
        let error = StagedPublication::new(
            PublicationRequestId::new("publication-id").unwrap(),
            PublicationCandidateFingerprint::new("test-invalid-candidate").unwrap(),
            0_u64,
            PreparedSessionMutation::new(()),
            attempt_identity(),
            "",
            Vec::new(),
            outbox,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PublicationCandidateError::OutboxRequestMismatch { .. }
        ));
    }
}
