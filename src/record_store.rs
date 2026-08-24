//! Authoritative append/CAS persistence for canonical and provider facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::transcript::{
    CanonicalInputItem, CanonicalTranscript, CanonicalTranscriptError, TranscriptRevision,
};

/// Stable session key owned by AgentView's record log.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Result<Self, RecordLogError> {
        validate_identifier("session id", value.into()).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Session-local authoritative log revision.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LogRevision(u64);

impl LogRevision {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Provider operation state retained as an append-only fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderOperationState {
    Attempted,
    Accepted,
    Completed,
    Failed,
}

/// Durable state of one provider-side System artifact attachment operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderArtifactAttachState {
    Prepared,
    Attempted,
    Accepted,
    Completed,
    Failed,
    Skipped,
}

/// Provider-side handling of the Epoch-scoped System artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderArtifactMode {
    Stateless,
    RemoteRetained,
}

/// Durable state of one provider-window compaction operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderCompactionState {
    Attempted,
    Completed { opaque_state: Option<Value> },
    Failed,
}

/// Acknowledged User-document baseline owned by durable publication authority.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct UserDiffCursor(u64);

impl UserDiffCursor {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Durable attempt state used to recover or fence unfinished execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptRecordState {
    Started,
    Prepared,
    Issued,
    Finished,
    Published,
    Aborted { reason: String },
}

/// Payload of one authoritative record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum SessionRecord {
    CanonicalItemAppended {
        transcript_revision: TranscriptRevision,
        item: CanonicalInputItem,
    },
    ProviderEpochOpened {
        provider_epoch_id: String,
        provider: String,
        profile: String,
        profile_version: u32,
        provider_binding: String,
        artifact_mode: ProviderArtifactMode,
        base_transcript_revision: TranscriptRevision,
    },
    ProviderEpochClosed {
        provider_epoch_id: String,
    },
    ProviderWindowCheckpointed {
        provider_epoch_id: String,
        window_revision: u64,
        transcript_revision: TranscriptRevision,
        history_policy_version: String,
        opaque_state: Option<Value>,
    },
    ProviderCompactionPrepared {
        compaction_id: String,
        provider_epoch_id: String,
        from_window_revision: u64,
        transcript_revision: TranscriptRevision,
        encoder_version: String,
        request_sha256: String,
        request_body: Vec<u8>,
    },
    ProviderCompactionAdvanced {
        compaction_id: String,
        compaction_revision: u64,
        state: ProviderCompactionState,
    },
    ProviderCompactionCheckpointed {
        compaction_id: String,
        provider_epoch_id: String,
        from_window_revision: u64,
        window_revision: u64,
        transcript_revision: TranscriptRevision,
        history_policy_version: String,
        opaque_state: Option<Value>,
    },
    ProviderOperationPrepared {
        operation_id: String,
        attempt_id: String,
        provider_epoch_id: String,
        window_revision: u64,
        transcript_revision: TranscriptRevision,
        user_diff_cursor: UserDiffCursor,
        encoder_version: String,
        request_sha256: String,
        request_body: Vec<u8>,
    },
    ProviderOperationAdvanced {
        operation_id: String,
        operation_revision: u64,
        state: ProviderOperationState,
    },
    EpochArtifactCommitted {
        epoch_id: String,
        artifact_fingerprint: String,
        contract_fingerprint: String,
        artifact: crate::pom::ResolvedDocument,
        contract_manifest: Vec<u8>,
    },
    EpochArtifactLogicallyAttached {
        epoch_id: String,
        attachment_id: String,
        artifact_fingerprint: String,
        contract_fingerprint: String,
    },
    ProviderArtifactAttachAdvanced {
        provider_epoch_id: String,
        attach_operation_id: String,
        attach_revision: u64,
        artifact_fingerprint: String,
        contract_fingerprint: String,
        encoder_version: String,
        request_sha256: Option<String>,
        request_body: Option<Vec<u8>>,
        skip_attempt_id: Option<String>,
        state: ProviderArtifactAttachState,
    },
    AttemptAdvanced {
        attempt_id: String,
        snapshot_id: String,
        attempt_revision: u64,
        state: AttemptRecordState,
    },
    TurnPublicationPrepared {
        publication_id: String,
        attempt_id: String,
        snapshot_id: String,
        base_transcript_revision: TranscriptRevision,
        previous_cursor: UserDiffCursor,
        next_cursor: UserDiffCursor,
        items: Vec<CanonicalInputItem>,
    },
    TurnPublicationCommitted {
        publication_id: String,
        attempt_id: String,
        base_transcript_revision: TranscriptRevision,
        next_transcript_revision: TranscriptRevision,
        next_cursor: UserDiffCursor,
    },
    UserDiffCursorAdvanced {
        publication_id: String,
        previous_cursor: UserDiffCursor,
        next_cursor: UserDiffCursor,
    },
}

impl SessionRecord {
    fn validate(&self) -> Result<(), RecordLogError> {
        match self {
            Self::CanonicalItemAppended {
                transcript_revision,
                item,
            } => {
                require_nonzero("transcript revision", transcript_revision.get())?;
                validate_canonical_item(item)
            }
            Self::ProviderEpochOpened {
                provider_epoch_id,
                provider,
                profile,
                profile_version,
                provider_binding,
                ..
            } => {
                validate_identifier_ref("provider epoch id", provider_epoch_id)?;
                validate_identifier_ref("provider", provider)?;
                validate_identifier_ref("provider profile", profile)?;
                validate_identifier_ref("provider binding", provider_binding)?;
                require_nonzero("provider profile version", u64::from(*profile_version))
            }
            Self::ProviderEpochClosed { provider_epoch_id } => {
                validate_identifier_ref("provider epoch id", provider_epoch_id)
            }
            Self::ProviderWindowCheckpointed {
                provider_epoch_id,
                window_revision,
                history_policy_version,
                ..
            } => {
                validate_identifier_ref("provider epoch id", provider_epoch_id)?;
                validate_identifier_ref("history policy version", history_policy_version)?;
                require_nonzero("provider window revision", *window_revision)
            }
            Self::ProviderCompactionPrepared {
                compaction_id,
                provider_epoch_id,
                from_window_revision,
                encoder_version,
                request_sha256,
                request_body,
                ..
            } => {
                validate_identifier_ref("provider compaction id", compaction_id)?;
                validate_identifier_ref("provider epoch id", provider_epoch_id)?;
                validate_identifier_ref("provider compaction encoder version", encoder_version)?;
                validate_sha256(request_sha256, request_body)?;
                require_nonzero("provider compaction source window", *from_window_revision)?;
                if request_body.is_empty() {
                    return Err(RecordLogError::EmptyRequestBody);
                }
                Ok(())
            }
            Self::ProviderCompactionAdvanced {
                compaction_id,
                compaction_revision,
                ..
            } => {
                validate_identifier_ref("provider compaction id", compaction_id)?;
                require_nonzero("provider compaction revision", *compaction_revision)
            }
            Self::ProviderCompactionCheckpointed {
                compaction_id,
                provider_epoch_id,
                from_window_revision,
                window_revision,
                history_policy_version,
                ..
            } => {
                validate_identifier_ref("provider compaction id", compaction_id)?;
                validate_identifier_ref("provider epoch id", provider_epoch_id)?;
                validate_identifier_ref("history policy version", history_policy_version)?;
                require_nonzero("provider compaction source window", *from_window_revision)?;
                require_nonzero("provider window revision", *window_revision)
            }
            Self::ProviderOperationPrepared {
                operation_id,
                attempt_id,
                provider_epoch_id,
                window_revision,
                encoder_version,
                request_sha256,
                request_body,
                ..
            } => {
                validate_identifier_ref("provider operation id", operation_id)?;
                validate_identifier_ref("attempt id", attempt_id)?;
                validate_identifier_ref("provider epoch id", provider_epoch_id)?;
                validate_identifier_ref("encoder version", encoder_version)?;
                validate_sha256(request_sha256, request_body)?;
                require_nonzero("provider window revision", *window_revision)?;
                if request_body.is_empty() {
                    return Err(RecordLogError::EmptyRequestBody);
                }
                Ok(())
            }
            Self::ProviderOperationAdvanced {
                operation_id,
                operation_revision,
                ..
            } => {
                validate_identifier_ref("provider operation id", operation_id)?;
                require_nonzero("provider operation revision", *operation_revision)
            }
            Self::EpochArtifactCommitted {
                epoch_id,
                artifact_fingerprint,
                contract_fingerprint,
                artifact,
                contract_manifest,
            } => {
                validate_identifier_ref("epoch id", epoch_id)?;
                validate_content_fingerprint(
                    "agentview.system-artifact/v1",
                    artifact_fingerprint,
                    &serde_json::to_vec(artifact)
                        .map_err(|error| RecordLogError::ArtifactEncoding(error.to_string()))?,
                )?;
                validate_content_fingerprint(
                    "agentview.contract-manifest/v1",
                    contract_fingerprint,
                    contract_manifest,
                )
            }
            Self::EpochArtifactLogicallyAttached {
                epoch_id,
                attachment_id,
                artifact_fingerprint,
                contract_fingerprint,
            } => {
                validate_identifier_ref("epoch id", epoch_id)?;
                validate_identifier_ref("artifact attachment id", attachment_id)?;
                validate_identifier_ref("artifact fingerprint", artifact_fingerprint)?;
                validate_identifier_ref("contract fingerprint", contract_fingerprint)
            }
            Self::ProviderArtifactAttachAdvanced {
                provider_epoch_id,
                attach_operation_id,
                attach_revision,
                artifact_fingerprint,
                contract_fingerprint,
                encoder_version,
                request_sha256,
                request_body,
                skip_attempt_id,
                state,
            } => {
                validate_identifier_ref("provider epoch id", provider_epoch_id)?;
                validate_identifier_ref(
                    "provider artifact attach operation id",
                    attach_operation_id,
                )?;
                validate_identifier_ref("artifact fingerprint", artifact_fingerprint)?;
                validate_identifier_ref("contract fingerprint", contract_fingerprint)?;
                validate_identifier_ref("encoder version", encoder_version)?;
                require_nonzero("provider artifact attach revision", *attach_revision)?;
                if let Some(attempt_id) = skip_attempt_id {
                    validate_identifier_ref("stateless artifact skip attempt id", attempt_id)?;
                }
                match (state, request_sha256, request_body) {
                    (
                        ProviderArtifactAttachState::Prepared,
                        Some(request_sha256),
                        Some(request_body),
                    ) => {
                        validate_sha256(request_sha256, request_body)?;
                        if request_body.is_empty() {
                            return Err(RecordLogError::EmptyRequestBody);
                        }
                        Ok(())
                    }
                    (ProviderArtifactAttachState::Prepared, _, _) => {
                        Err(RecordLogError::InvalidSequence(
                            "prepared provider artifact attach requires frozen request bytes",
                        ))
                    }
                    (_, None, None) => Ok(()),
                    _ => Err(RecordLogError::InvalidSequence(
                        "only prepared provider artifact attach carries request bytes",
                    )),
                }?;
                match (state, skip_attempt_id) {
                    (ProviderArtifactAttachState::Skipped, Some(_)) => Ok(()),
                    (ProviderArtifactAttachState::Skipped, None) => {
                        Err(RecordLogError::InvalidSequence(
                            "stateless provider artifact skip requires an attempt id",
                        ))
                    }
                    (_, None) => Ok(()),
                    (_, Some(_)) => Err(RecordLogError::InvalidSequence(
                        "only stateless provider artifact skip carries an attempt id",
                    )),
                }
            }
            Self::AttemptAdvanced {
                attempt_id,
                snapshot_id,
                attempt_revision,
                state,
            } => {
                validate_identifier_ref("attempt id", attempt_id)?;
                validate_identifier_ref("attempt snapshot id", snapshot_id)?;
                require_nonzero("attempt revision", *attempt_revision)?;
                if let AttemptRecordState::Aborted { reason } = state {
                    validate_identifier_ref("attempt abort reason", reason)?;
                }
                Ok(())
            }
            Self::TurnPublicationPrepared {
                publication_id,
                attempt_id,
                snapshot_id,
                previous_cursor,
                next_cursor,
                items,
                ..
            } => {
                validate_identifier_ref("publication id", publication_id)?;
                validate_identifier_ref("attempt id", attempt_id)?;
                validate_identifier_ref("attempt snapshot id", snapshot_id)?;
                if items.is_empty() {
                    return Err(RecordLogError::InvalidSequence(
                        "prepared publication must contain canonical items",
                    ));
                }
                for item in items {
                    validate_canonical_item(item)?;
                }
                if next_cursor.get() != next_revision(previous_cursor.get())? {
                    return Err(RecordLogError::InvalidSequence(
                        "prepared publication cursor gap",
                    ));
                }
                Ok(())
            }
            Self::TurnPublicationCommitted {
                publication_id,
                attempt_id,
                next_transcript_revision,
                next_cursor,
                ..
            } => {
                validate_identifier_ref("publication id", publication_id)?;
                validate_identifier_ref("attempt id", attempt_id)?;
                require_nonzero(
                    "publication transcript revision",
                    next_transcript_revision.get(),
                )?;
                require_nonzero("publication User diff cursor", next_cursor.get())
            }
            Self::UserDiffCursorAdvanced {
                publication_id,
                next_cursor,
                ..
            } => {
                validate_identifier_ref("publication id", publication_id)?;
                require_nonzero("User diff cursor", next_cursor.get())
            }
        }
    }
}

/// Record awaiting session-local revision assignment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewRecord {
    schema_version: u32,
    record: SessionRecord,
}

impl NewRecord {
    pub fn new(schema_version: u32, record: SessionRecord) -> Result<Self, RecordLogError> {
        if schema_version == 0 {
            return Err(RecordLogError::ZeroSchemaVersion);
        }
        record.validate()?;
        Ok(Self {
            schema_version,
            record,
        })
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn record(&self) -> &SessionRecord {
        &self.record
    }
}

impl<'de> Deserialize<'de> for NewRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireNewRecord {
            schema_version: u32,
            record: SessionRecord,
        }

        let wire = WireNewRecord::deserialize(deserializer)?;
        Self::new(wire.schema_version, wire.record).map_err(D::Error::custom)
    }
}

/// Non-empty append unit accepted or rejected atomically.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordBatch(Vec<NewRecord>);

impl RecordBatch {
    pub fn new(records: impl IntoIterator<Item = NewRecord>) -> Result<Self, RecordLogError> {
        let records = records.into_iter().collect::<Vec<_>>();
        if records.is_empty() {
            return Err(RecordLogError::EmptyBatch);
        }
        Ok(Self(records))
    }

    pub fn records(&self) -> &[NewRecord] {
        &self.0
    }

    pub fn into_records(self) -> Vec<NewRecord> {
        self.0
    }
}

/// Persisted authoritative envelope.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordEnvelope {
    session_id: SessionId,
    revision: LogRevision,
    schema_version: u32,
    record: SessionRecord,
}

impl RecordEnvelope {
    pub const INITIAL_TAIL: LogRevision = LogRevision::ZERO;

    pub fn new(
        session_id: SessionId,
        revision: LogRevision,
        record: NewRecord,
    ) -> Result<Self, RecordLogError> {
        require_nonzero("record log revision", revision.get())?;
        let session_id = SessionId::new(session_id.0)?;
        let record = NewRecord::new(record.schema_version, record.record)?;
        Ok(Self {
            session_id,
            revision,
            schema_version: record.schema_version,
            record: record.record,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn revision(&self) -> LogRevision {
        self.revision
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn record(&self) -> &SessionRecord {
        &self.record
    }
}

impl<'de> Deserialize<'de> for RecordEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRecordEnvelope {
            session_id: SessionId,
            revision: LogRevision,
            schema_version: u32,
            record: SessionRecord,
        }

        let wire = WireRecordEnvelope::deserialize(deserializer)?;
        let record = NewRecord::new(wire.schema_version, wire.record).map_err(D::Error::custom)?;
        Self::new(wire.session_id, wire.revision, record).map_err(D::Error::custom)
    }
}

/// Result of one atomic compare-and-append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Committed { tail: LogRevision },
    Conflict { actual_tail: LogRevision },
}

impl AppendOutcome {
    pub const fn committed(tail: u64) -> Self {
        Self::Committed {
            tail: LogRevision::new(tail),
        }
    }

    pub const fn conflict(actual_tail: u64) -> Self {
        Self::Conflict {
            actual_tail: LogRevision::new(actual_tail),
        }
    }

    pub const fn tail(self) -> LogRevision {
        match self {
            Self::Committed { tail } => tail,
            Self::Conflict { actual_tail } => actual_tail,
        }
    }
}

/// User-supplied CAS backend; append must validate inside its atomic commit fence.
#[async_trait]
pub trait RecordLog: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    async fn read(
        &self,
        session_id: &SessionId,
        after_revision: LogRevision,
    ) -> Result<Vec<RecordEnvelope>, Self::Error>;

    /// Atomically appends `batch` only when `expected_tail` is current.
    ///
    /// `Committed` means this invocation created the new records. A stale tail
    /// must return `Conflict` even when its candidate bytes equal records that
    /// are already present; exact replay is not new execution authority.
    async fn append(
        &self,
        session_id: &SessionId,
        expected_tail: LogRevision,
        batch: RecordBatch,
    ) -> Result<AppendOutcome, Self::Error>;
}

macro_rules! require_sequence {
    ($condition:expr, $reason:literal) => {
        if !$condition {
            return Err(RecordLogError::InvalidSequence($reason));
        }
    };
}

/// Validates existing records plus a batch inside a backend's atomic commit boundary.
pub fn validate_record_append(
    session_id: &SessionId,
    existing: &[RecordEnvelope],
    batch: &RecordBatch,
) -> Result<(), RecordLogError> {
    let state = existing.iter().enumerate().try_fold(
        SequenceState::default(),
        |state, (index, envelope)| {
            require_sequence!(envelope.session_id() == session_id, "mixed session");
            let expected = revision_from_len(
                index
                    .checked_add(1)
                    .ok_or(RecordLogError::RevisionOverflow)?,
            )?;
            require_sequence!(envelope.revision() == expected, "log revision gap");
            NewRecord::new(envelope.schema_version, envelope.record.clone())?;
            state.applied(envelope.record())
        },
    )?;
    let state = state.ensure_no_incomplete_authority()?.begin_append();
    let state = batch.records().iter().try_fold(state, |state, record| {
        NewRecord::new(record.schema_version, record.record.clone())?;
        state.applied(record.record())
    })?;
    state.ensure_no_incomplete_authority()?;
    Ok(())
}

struct AttemptSequence {
    revision: u64,
    snapshot_id: String,
    provider_epoch_id: String,
    state: AttemptRecordState,
}

struct ArtifactSequence {
    artifact_fingerprint: String,
    contract_fingerprint: String,
}

struct EpochSequence {
    base_transcript_revision: u64,
    windows: Vec<u64>,
    profile: String,
    artifact_mode: ProviderArtifactMode,
}

struct ArtifactAttachSequence {
    provider_epoch_id: String,
    artifact_fingerprint: String,
    contract_fingerprint: String,
    encoder_version: String,
    skip_attempt_id: Option<String>,
    revision: u64,
    state: ProviderArtifactAttachState,
}

struct OperationSequence {
    attempt_id: String,
    revision: u64,
    state: Option<ProviderOperationState>,
}

struct CompactionSequence {
    provider_epoch_id: String,
    from_window_revision: u64,
    transcript_revision: TranscriptRevision,
    encoder_version: String,
    revision: u64,
    state: Option<ProviderCompactionState>,
    checkpointed: bool,
}

struct PreparedPublicationSequence {
    attempt_id: String,
    base_transcript_revision: TranscriptRevision,
    next_cursor: UserDiffCursor,
    items: Vec<CanonicalInputItem>,
    next_transcript_revision: TranscriptRevision,
}

enum PendingArtifact {
    LogicalAttach {
        epoch_id: String,
        artifact_fingerprint: String,
        contract_fingerprint: String,
        artifact: crate::pom::ResolvedDocument,
    },
    CanonicalSystem {
        artifact: crate::pom::ResolvedDocument,
    },
}

enum PendingPublication {
    Commit {
        publication_id: String,
        next_cursor: UserDiffCursor,
    },
    AttemptPublished {
        publication_id: String,
        attempt_id: String,
    },
}

struct PendingPublicationPreparation {
    attempt_id: String,
}

#[derive(Default)]
struct SequenceState {
    transcript: CanonicalTranscript,
    epochs: BTreeMap<String, EpochSequence>,
    active_epoch: Option<String>,
    operations: BTreeMap<String, OperationSequence>,
    compactions: BTreeMap<String, CompactionSequence>,
    artifacts: BTreeMap<String, ArtifactSequence>,
    logical_attachment_epochs: BTreeSet<String>,
    logical_attachment_ids: BTreeSet<String>,
    artifact_attach_operations: BTreeMap<String, ArtifactAttachSequence>,
    provider_attached_artifact_epochs: BTreeSet<String>,
    pending_stateless_artifact_skips: BTreeMap<String, String>,
    epochs_with_provider_operations: BTreeSet<String>,
    attempts: BTreeMap<String, AttemptSequence>,
    prepared_publications: BTreeMap<String, PreparedPublicationSequence>,
    publications: BTreeSet<String>,
    user_diff_cursor: UserDiffCursor,
    pending_publication: Option<PendingPublication>,
    pending_publication_preparation: Option<PendingPublicationPreparation>,
    pending_artifact: Option<PendingArtifact>,
    append_base_transcript_revision: Option<TranscriptRevision>,
    started_reservation_in_current_append: bool,
}

impl SequenceState {
    fn is_active(&self, epoch: &str) -> bool {
        self.active_epoch.as_deref() == Some(epoch)
    }

    fn begin_append(mut self) -> Self {
        self.append_base_transcript_revision = Some(self.transcript.revision());
        self.started_reservation_in_current_append = false;
        self
    }

    fn ensure_no_incomplete_authority(self) -> Result<Self, RecordLogError> {
        require_sequence!(
            self.pending_publication.is_none(),
            "incomplete publication batch"
        );
        require_sequence!(
            self.pending_publication_preparation.is_none(),
            "incomplete publication preparation batch"
        );
        require_sequence!(self.pending_artifact.is_none(), "incomplete artifact batch");
        require_sequence!(
            !self
                .prepared_publications
                .iter()
                .filter(|(publication_id, _)| !self.publications.contains(*publication_id))
                .any(|(_, prepared)| {
                    self.transcript.revision().get() > prepared.base_transcript_revision.get()
                }),
            "incomplete prepared publication commit batch"
        );
        Ok(self)
    }

    fn unresolved_compaction(&self) -> Option<&str> {
        self.compactions
            .iter()
            .find_map(|(operation_id, sequence)| {
                (!sequence.checkpointed
                    && !matches!(&sequence.state, Some(ProviderCompactionState::Failed)))
                .then_some(operation_id.as_str())
            })
    }

    fn unresolved_provider_operation(&self) -> Option<(&str, &OperationSequence)> {
        self.operations
            .iter()
            .find(|(_, sequence)| {
                let attempt_is_active =
                    self.attempts
                        .get(&sequence.attempt_id)
                        .is_some_and(|attempt| {
                            matches!(
                                attempt.state,
                                AttemptRecordState::Prepared | AttemptRecordState::Issued
                            )
                        });
                match sequence.state {
                    None => attempt_is_active,
                    Some(ProviderOperationState::Attempted | ProviderOperationState::Accepted) => {
                        true
                    }
                    Some(ProviderOperationState::Completed | ProviderOperationState::Failed) => {
                        false
                    }
                }
            })
            .map(|(operation_id, sequence)| (operation_id.as_str(), sequence))
    }

    fn validate_artifact_attach_exclusivity(
        &self,
        record: &SessionRecord,
    ) -> Result<(), RecordLogError> {
        let Some(epoch_id) = self.active_epoch.as_deref() else {
            return Ok(());
        };
        let epoch = self
            .epochs
            .get(epoch_id)
            .ok_or(RecordLogError::InvalidSequence("unknown active epoch"))?;
        if epoch.artifact_mode == ProviderArtifactMode::Stateless
            || !self.artifacts.contains_key(epoch_id)
            || self.provider_attached_artifact_epochs.contains(epoch_id)
            || self.pending_artifact.is_some()
        {
            return Ok(());
        }

        let remote_attach = self
            .artifact_attach_operations
            .iter()
            .find(|(_, sequence)| {
                sequence.provider_epoch_id == epoch_id
                    && !matches!(sequence.state, ProviderArtifactAttachState::Skipped)
            });
        let aborts_attempt = matches!(
            record,
            SessionRecord::AttemptAdvanced {
                state: AttemptRecordState::Aborted { .. },
                ..
            }
        );
        match remote_attach {
            Some((_, sequence))
                if matches!(sequence.state, ProviderArtifactAttachState::Failed) =>
            {
                require_sequence!(
                    aborts_attempt
                        || matches!(
                            record,
                            SessionRecord::ProviderEpochClosed {
                                provider_epoch_id
                            } if provider_epoch_id == epoch_id
                        ),
                    "failed provider artifact attach blocks work until Epoch rollover"
                );
            }
            Some((operation_id, _)) => {
                let continues_attach = matches!(
                    record,
                    SessionRecord::ProviderArtifactAttachAdvanced {
                        attach_operation_id,
                        ..
                    } if attach_operation_id == operation_id
                );
                require_sequence!(
                    continues_attach || aborts_attempt,
                    "unresolved provider artifact attach blocks other durable work"
                );
            }
            None => {
                let prepares_attach = matches!(
                    record,
                    SessionRecord::ProviderArtifactAttachAdvanced {
                        provider_epoch_id,
                        state: ProviderArtifactAttachState::Prepared,
                        ..
                    } if provider_epoch_id == epoch_id
                );
                require_sequence!(
                    prepares_attach || aborts_attempt,
                    "remote-retained artifact requires provider attach preparation"
                );
            }
        }
        Ok(())
    }

    fn validate_attempt_exclusivity(&self, record: &SessionRecord) -> Result<(), RecordLogError> {
        if let Some((started_id, _)) = self
            .attempts
            .iter()
            .find(|(_, attempt)| matches!(attempt.state, AttemptRecordState::Started))
        {
            let continues_started_attempt = matches!(
                record,
                SessionRecord::AttemptAdvanced {
                    attempt_id,
                    state: AttemptRecordState::Prepared | AttemptRecordState::Aborted { .. },
                    ..
                } if attempt_id == started_id
            );
            require_sequence!(
                continues_started_attempt,
                "started attempt blocks other durable work"
            );
        }

        if let SessionRecord::AttemptAdvanced {
            attempt_id,
            state: AttemptRecordState::Started,
            ..
        } = record
        {
            require_sequence!(
                !self.attempts.contains_key(attempt_id),
                "attempt identity reused"
            );
            require_sequence!(
                self.attempts.values().all(|attempt| matches!(
                    attempt.state,
                    AttemptRecordState::Published | AttemptRecordState::Aborted { .. }
                )),
                "unsettled attempt blocks a new model attempt"
            );
        }
        Ok(())
    }

    fn validate_provider_operation_exclusivity(
        &self,
        record: &SessionRecord,
    ) -> Result<(), RecordLogError> {
        let Some((operation_id, sequence)) = self.unresolved_provider_operation() else {
            return Ok(());
        };
        let continues_operation = matches!(
            record,
            SessionRecord::ProviderOperationAdvanced {
                operation_id: observed,
                ..
            } if observed == operation_id
        );
        let fences_pre_issue_attempt = sequence.state.is_none()
            && matches!(
                record,
                SessionRecord::AttemptAdvanced {
                    attempt_id,
                    state: AttemptRecordState::Issued | AttemptRecordState::Aborted { .. },
                    ..
                } if attempt_id == &sequence.attempt_id
            );
        require_sequence!(
            continues_operation || fences_pre_issue_attempt,
            "unresolved provider operation blocks other durable work"
        );
        Ok(())
    }

    fn validate_compaction_exclusivity(
        &self,
        record: &SessionRecord,
    ) -> Result<(), RecordLogError> {
        let Some(operation_id) = self.unresolved_compaction() else {
            return Ok(());
        };
        require_sequence!(
            matches!(
                record,
                SessionRecord::ProviderCompactionAdvanced {
                    compaction_id: observed,
                    ..
                } | SessionRecord::ProviderCompactionCheckpointed {
                    compaction_id: observed,
                    ..
                } if observed == operation_id
            ),
            "unresolved provider compaction blocks other durable work"
        );
        Ok(())
    }

    fn validate_artifact_continuation(&self, record: &SessionRecord) -> Result<(), RecordLogError> {
        match (&self.pending_artifact, record) {
            (
                Some(PendingArtifact::LogicalAttach {
                    epoch_id,
                    artifact_fingerprint,
                    contract_fingerprint,
                    ..
                }),
                SessionRecord::EpochArtifactLogicallyAttached {
                    epoch_id: attached_epoch,
                    artifact_fingerprint: attached_artifact,
                    contract_fingerprint: attached_contract,
                    ..
                },
            ) => {
                require_sequence!(epoch_id == attached_epoch, "logical attach epoch mismatch");
                require_sequence!(
                    artifact_fingerprint == attached_artifact,
                    "logical attach artifact mismatch"
                );
                require_sequence!(
                    contract_fingerprint == attached_contract,
                    "logical attach contract mismatch"
                );
            }
            (
                Some(PendingArtifact::CanonicalSystem { artifact, .. }),
                SessionRecord::CanonicalItemAppended {
                    item:
                        CanonicalInputItem::Instruction {
                            authority: crate::transcript::InstructionAuthority::System,
                            pom,
                        },
                    ..
                },
            ) => {
                require_sequence!(artifact == pom, "canonical System artifact mismatch");
            }
            (Some(PendingArtifact::LogicalAttach { .. }), _) => {
                return Err(RecordLogError::InvalidSequence(
                    "logical attach must follow artifact commit",
                ));
            }
            (Some(PendingArtifact::CanonicalSystem { .. }), _) => {
                return Err(RecordLogError::InvalidSequence(
                    "canonical System must follow logical attach",
                ));
            }
            (None, SessionRecord::EpochArtifactLogicallyAttached { .. }) => {
                return Err(RecordLogError::InvalidSequence(
                    "artifact records must be committed atomically",
                ));
            }
            (
                None,
                SessionRecord::CanonicalItemAppended {
                    item:
                        CanonicalInputItem::Instruction {
                            authority: crate::transcript::InstructionAuthority::System,
                            ..
                        },
                    ..
                },
            ) => {
                return Err(RecordLogError::InvalidSequence(
                    "canonical System requires atomic artifact admission",
                ));
            }
            (None, _) => {}
        }
        Ok(())
    }

    fn validate_publication_continuation(
        &self,
        record: &SessionRecord,
    ) -> Result<(), RecordLogError> {
        match (&self.pending_publication, record) {
            (
                Some(PendingPublication::Commit {
                    publication_id,
                    next_cursor,
                }),
                SessionRecord::TurnPublicationCommitted {
                    publication_id: committed_id,
                    next_cursor: committed_cursor,
                    ..
                },
            ) => {
                require_sequence!(publication_id == committed_id, "publication id mismatch");
                require_sequence!(
                    next_cursor == committed_cursor,
                    "publication cursor mismatch"
                );
            }
            (
                Some(PendingPublication::AttemptPublished {
                    publication_id,
                    attempt_id,
                }),
                SessionRecord::AttemptAdvanced {
                    attempt_id: published_attempt_id,
                    state: AttemptRecordState::Published,
                    ..
                },
            ) => {
                require_sequence!(
                    attempt_id == published_attempt_id,
                    "publication attempt mismatch"
                );
                require_sequence!(
                    self.publications.contains(publication_id),
                    "unknown publication"
                );
            }
            (Some(PendingPublication::Commit { .. }), _) => {
                return Err(RecordLogError::InvalidSequence(
                    "publication commit must follow cursor advance",
                ));
            }
            (Some(PendingPublication::AttemptPublished { .. }), _) => {
                return Err(RecordLogError::InvalidSequence(
                    "published attempt must follow publication commit",
                ));
            }
            (
                None,
                SessionRecord::TurnPublicationCommitted { .. }
                | SessionRecord::AttemptAdvanced {
                    state: AttemptRecordState::Published,
                    ..
                },
            ) => {
                return Err(RecordLogError::InvalidSequence(
                    "publication records must be committed atomically",
                ));
            }
            (None, _) => {}
        }
        Ok(())
    }

    fn validate_publication_preparation_continuation(
        &self,
        record: &SessionRecord,
    ) -> Result<(), RecordLogError> {
        match (&self.pending_publication_preparation, record) {
            (
                Some(pending),
                SessionRecord::AttemptAdvanced {
                    attempt_id,
                    state: AttemptRecordState::Finished,
                    ..
                },
            ) => {
                require_sequence!(
                    pending.attempt_id == *attempt_id,
                    "prepared publication attempt mismatch"
                );
            }
            (Some(_), _) => {
                return Err(RecordLogError::InvalidSequence(
                    "Finished attempt must follow publication preparation",
                ));
            }
            (None, _) => {}
        }
        Ok(())
    }

    fn validate_prepared_publication_exclusivity(
        &self,
        record: &SessionRecord,
    ) -> Result<(), RecordLogError> {
        if self.pending_publication_preparation.is_some() || self.pending_publication.is_some() {
            return Ok(());
        }
        let Some((publication_id, prepared)) = self
            .prepared_publications
            .iter()
            .find(|(publication_id, _)| !self.publications.contains(*publication_id))
        else {
            return Ok(());
        };
        let progress = self
            .transcript
            .revision()
            .get()
            .checked_sub(prepared.base_transcript_revision.get())
            .ok_or(RecordLogError::InvalidSequence(
                "prepared publication transcript authority regressed",
            ))?;
        let progress = usize::try_from(progress).map_err(|_| RecordLogError::RevisionOverflow)?;
        require_sequence!(
            progress <= prepared.items.len(),
            "prepared publication transcript authority drift"
        );

        if let Some(expected_item) = prepared.items.get(progress) {
            require_sequence!(
                matches!(
                    record,
                    SessionRecord::CanonicalItemAppended { item, .. }
                        if item == expected_item
                ),
                "unresolved prepared publication blocks unrelated durable work"
            );
        } else {
            require_sequence!(
                matches!(
                    record,
                    SessionRecord::UserDiffCursorAdvanced {
                        publication_id: observed,
                        ..
                    } if observed == publication_id
                ),
                "prepared publication cursor release must follow its canonical items"
            );
        }
        Ok(())
    }

    fn applied(mut self, record: &SessionRecord) -> Result<Self, RecordLogError> {
        self.validate_attempt_exclusivity(record)?;
        self.validate_artifact_attach_exclusivity(record)?;
        self.validate_provider_operation_exclusivity(record)?;
        self.validate_compaction_exclusivity(record)?;
        self.validate_artifact_continuation(record)?;
        self.validate_publication_continuation(record)?;
        self.validate_publication_preparation_continuation(record)?;
        self.validate_prepared_publication_exclusivity(record)?;
        match record {
            SessionRecord::CanonicalItemAppended {
                transcript_revision: revision,
                item,
            } => {
                let expected = next_revision(self.transcript.revision().get())?;
                require_sequence!(revision.get() == expected, "transcript gap");
                self.transcript = self.transcript.appended(item.clone())?;
                if matches!(
                    self.pending_artifact,
                    Some(PendingArtifact::CanonicalSystem { .. })
                ) {
                    self.pending_artifact = None;
                }
            }
            SessionRecord::ProviderEpochOpened {
                provider_epoch_id: epoch,
                profile,
                artifact_mode,
                base_transcript_revision: base,
                ..
            } => {
                require_sequence!(!self.epochs.contains_key(epoch), "epoch reused");
                require_sequence!(self.active_epoch.is_none(), "active epoch exists");
                let base = base.get();
                let tail = self.transcript.revision().get();
                require_sequence!(base <= tail, "epoch base exceeds transcript authority");
                self.epochs.insert(
                    epoch.clone(),
                    EpochSequence {
                        base_transcript_revision: base,
                        windows: Vec::new(),
                        profile: profile.clone(),
                        artifact_mode: *artifact_mode,
                    },
                );
                self.active_epoch = Some(epoch.clone());
            }
            SessionRecord::ProviderEpochClosed {
                provider_epoch_id: epoch,
            } => {
                require_sequence!(self.is_active(epoch), "inactive epoch");
                require_sequence!(
                    self.attempts.values().all(|attempt| {
                        attempt.provider_epoch_id != *epoch
                            || matches!(
                                attempt.state,
                                AttemptRecordState::Published | AttemptRecordState::Aborted { .. }
                            )
                    }),
                    "active epoch has an unsettled attempt"
                );
                self.active_epoch = None;
            }
            SessionRecord::ProviderWindowCheckpointed {
                provider_epoch_id: epoch,
                window_revision: window,
                transcript_revision: revision,
                ..
            } => {
                require_sequence!(self.is_active(epoch), "inactive epoch");
                let tail = self.transcript.revision().get();
                let epoch = self
                    .epochs
                    .get_mut(epoch)
                    .ok_or(RecordLogError::InvalidSequence("unknown epoch"))?;
                let expected = next_revision(revision_from_len(epoch.windows.len())?.get())?;
                require_sequence!(*window == expected, "window revision gap");
                let transcript = revision.get();
                let lower_bound = epoch
                    .windows
                    .last()
                    .copied()
                    .unwrap_or(epoch.base_transcript_revision);
                require_sequence!(
                    transcript >= lower_bound && transcript <= tail,
                    "window transcript range"
                );
                epoch.windows.push(transcript);
            }
            SessionRecord::ProviderCompactionPrepared {
                compaction_id,
                provider_epoch_id,
                from_window_revision,
                transcript_revision,
                encoder_version,
                ..
            } => {
                require_sequence!(
                    !self.compactions.contains_key(compaction_id),
                    "provider compaction id reused"
                );
                require_sequence!(
                    self.unresolved_compaction().is_none(),
                    "unresolved provider compaction exists"
                );
                require_sequence!(self.is_active(provider_epoch_id), "inactive epoch");
                require_sequence!(
                    self.attempts.values().all(|attempt| matches!(
                        attempt.state,
                        AttemptRecordState::Published | AttemptRecordState::Aborted { .. }
                    )),
                    "provider compaction requires settled attempts"
                );
                let epoch = self
                    .epochs
                    .get(provider_epoch_id)
                    .ok_or(RecordLogError::InvalidSequence("unknown epoch"))?;
                let current_window = revision_from_len(epoch.windows.len())?.get();
                require_sequence!(
                    *from_window_revision == current_window,
                    "provider compaction source window mismatch"
                );
                require_sequence!(
                    *transcript_revision == self.transcript.revision(),
                    "provider compaction transcript mismatch"
                );
                self.compactions.insert(
                    compaction_id.clone(),
                    CompactionSequence {
                        provider_epoch_id: provider_epoch_id.clone(),
                        from_window_revision: *from_window_revision,
                        transcript_revision: *transcript_revision,
                        encoder_version: encoder_version.clone(),
                        revision: 0,
                        state: None,
                        checkpointed: false,
                    },
                );
            }
            SessionRecord::ProviderCompactionAdvanced {
                compaction_id,
                compaction_revision,
                state,
            } => {
                let sequence = self.compactions.get_mut(compaction_id).ok_or(
                    RecordLogError::InvalidSequence("unknown provider compaction"),
                )?;
                require_sequence!(
                    *compaction_revision == next_revision(sequence.revision)?,
                    "provider compaction revision gap"
                );
                let valid_transition = matches!(
                    (&sequence.state, state),
                    (None, ProviderCompactionState::Attempted)
                        | (
                            Some(ProviderCompactionState::Attempted),
                            ProviderCompactionState::Completed { .. }
                                | ProviderCompactionState::Failed
                        )
                );
                require_sequence!(
                    valid_transition,
                    "invalid provider compaction state transition"
                );
                sequence.revision = *compaction_revision;
                sequence.state = Some(state.clone());
            }
            SessionRecord::ProviderCompactionCheckpointed {
                compaction_id,
                provider_epoch_id,
                from_window_revision,
                window_revision,
                transcript_revision,
                history_policy_version,
                opaque_state,
            } => {
                let sequence =
                    self.compactions
                        .get(compaction_id)
                        .ok_or(RecordLogError::InvalidSequence(
                            "unknown provider compaction",
                        ))?;
                require_sequence!(
                    !sequence.checkpointed,
                    "provider compaction checkpoint reused"
                );
                require_sequence!(
                    sequence.provider_epoch_id == *provider_epoch_id,
                    "provider compaction checkpoint epoch mismatch"
                );
                require_sequence!(
                    sequence.from_window_revision == *from_window_revision,
                    "provider compaction checkpoint source window mismatch"
                );
                require_sequence!(
                    sequence.transcript_revision == *transcript_revision,
                    "provider compaction checkpoint transcript mismatch"
                );
                require_sequence!(
                    sequence.encoder_version == *history_policy_version,
                    "provider compaction checkpoint policy mismatch"
                );
                require_sequence!(
                    matches!(
                        &sequence.state,
                        Some(ProviderCompactionState::Completed {
                            opaque_state: completed,
                        }) if completed == opaque_state
                    ),
                    "provider compaction checkpoint output mismatch"
                );
                let expected_window = next_revision(sequence.from_window_revision)?;
                require_sequence!(
                    *window_revision == expected_window,
                    "provider compaction checkpoint window mismatch"
                );
                require_sequence!(self.is_active(provider_epoch_id), "inactive epoch");
                let tail = self.transcript.revision().get();
                let epoch = self
                    .epochs
                    .get_mut(provider_epoch_id)
                    .ok_or(RecordLogError::InvalidSequence("unknown epoch"))?;
                let current_window = revision_from_len(epoch.windows.len())?.get();
                require_sequence!(
                    current_window == *from_window_revision,
                    "provider compaction checkpoint authority drift"
                );
                let transcript = transcript_revision.get();
                let lower_bound = epoch
                    .windows
                    .last()
                    .copied()
                    .unwrap_or(epoch.base_transcript_revision);
                require_sequence!(
                    transcript >= lower_bound && transcript <= tail,
                    "window transcript range"
                );
                epoch.windows.push(transcript);
                self.compactions
                    .get_mut(compaction_id)
                    .expect("compaction was validated above")
                    .checkpointed = true;
            }
            SessionRecord::ProviderOperationPrepared {
                operation_id: operation,
                attempt_id,
                provider_epoch_id: epoch,
                window_revision: window,
                transcript_revision: revision,
                user_diff_cursor,
                ..
            } => {
                require_sequence!(!self.operations.contains_key(operation), "operation reused");
                require_sequence!(
                    !self
                        .operations
                        .values()
                        .any(|sequence| sequence.attempt_id == *attempt_id),
                    "attempt already has a provider operation"
                );
                require_sequence!(self.is_active(epoch), "inactive epoch");
                require_sequence!(
                    self.attempts.get(attempt_id).is_some_and(|attempt| {
                        attempt.provider_epoch_id == *epoch
                            && matches!(attempt.state, AttemptRecordState::Prepared)
                    }),
                    "operation requires a prepared attempt"
                );
                require_sequence!(
                    *user_diff_cursor == self.user_diff_cursor,
                    "provider operation User diff cursor mismatch"
                );
                if self.artifacts.contains_key(epoch) {
                    let artifact_mode = self
                        .epochs
                        .get(epoch)
                        .ok_or(RecordLogError::InvalidSequence("unknown epoch"))?
                        .artifact_mode;
                    let provider_artifact_ready = match artifact_mode {
                        ProviderArtifactMode::RemoteRetained => {
                            self.provider_attached_artifact_epochs.contains(epoch)
                        }
                        ProviderArtifactMode::Stateless => self
                            .pending_stateless_artifact_skips
                            .remove(attempt_id)
                            .is_some_and(|skip_epoch| skip_epoch == *epoch),
                    };
                    require_sequence!(
                        provider_artifact_ready,
                        "provider artifact attach or skip required before operation"
                    );
                }
                let epoch_sequence = self
                    .epochs
                    .get(epoch)
                    .ok_or(RecordLogError::InvalidSequence("unknown epoch"))?;
                let index =
                    usize::try_from(window - 1).map_err(|_| RecordLogError::RevisionOverflow)?;
                let window_transcript = epoch_sequence
                    .windows
                    .get(index)
                    .ok_or(RecordLogError::InvalidSequence("unknown window"))?;
                require_sequence!(*window_transcript == revision.get(), "window mismatch");
                self.operations.insert(
                    operation.clone(),
                    OperationSequence {
                        attempt_id: attempt_id.clone(),
                        revision: 0,
                        state: None,
                    },
                );
                self.epochs_with_provider_operations.insert(epoch.clone());
            }
            SessionRecord::ProviderOperationAdvanced {
                operation_id: operation,
                operation_revision: revision,
                state,
            } => {
                let attempt_id = self
                    .operations
                    .get(operation)
                    .ok_or(RecordLogError::InvalidSequence("unknown operation"))?
                    .attempt_id
                    .clone();
                require_sequence!(
                    self.attempts.get(&attempt_id).is_some_and(|attempt| {
                        matches!(attempt.state, AttemptRecordState::Issued)
                    }),
                    "provider operation requires an issued attempt"
                );
                let sequence = self
                    .operations
                    .get_mut(operation)
                    .ok_or(RecordLogError::InvalidSequence("unknown operation"))?;
                let expected = next_revision(sequence.revision)?;
                require_sequence!(*revision == expected, "operation revision gap");
                let valid_transition = matches!(
                    (sequence.state, *state),
                    (None, ProviderOperationState::Attempted)
                        | (
                            Some(ProviderOperationState::Attempted),
                            ProviderOperationState::Accepted
                                | ProviderOperationState::Completed
                                | ProviderOperationState::Failed
                        )
                        | (
                            Some(ProviderOperationState::Accepted),
                            ProviderOperationState::Completed | ProviderOperationState::Failed
                        )
                );
                require_sequence!(valid_transition, "invalid operation state transition");
                sequence.revision = *revision;
                sequence.state = Some(*state);
            }
            SessionRecord::EpochArtifactCommitted {
                epoch_id,
                artifact_fingerprint,
                contract_fingerprint,
                artifact,
                ..
            } => {
                require_sequence!(
                    self.is_active(epoch_id),
                    "artifact committed outside active epoch"
                );
                require_sequence!(
                    !self.artifacts.contains_key(epoch_id),
                    "epoch artifact reused"
                );
                require_sequence!(
                    !self.epochs_with_provider_operations.contains(epoch_id),
                    "artifact committed after provider operation"
                );
                self.artifacts.insert(
                    epoch_id.clone(),
                    ArtifactSequence {
                        artifact_fingerprint: artifact_fingerprint.clone(),
                        contract_fingerprint: contract_fingerprint.clone(),
                    },
                );
                self.pending_artifact = Some(PendingArtifact::LogicalAttach {
                    epoch_id: epoch_id.clone(),
                    artifact_fingerprint: artifact_fingerprint.clone(),
                    contract_fingerprint: contract_fingerprint.clone(),
                    artifact: artifact.clone(),
                });
            }
            SessionRecord::EpochArtifactLogicallyAttached {
                epoch_id,
                attachment_id,
                artifact_fingerprint,
                contract_fingerprint,
            } => {
                require_sequence!(
                    self.is_active(epoch_id),
                    "logical artifact attach outside active epoch"
                );
                require_sequence!(
                    self.artifacts.get(epoch_id).is_some_and(|artifact| {
                        artifact.artifact_fingerprint == *artifact_fingerprint
                            && artifact.contract_fingerprint == *contract_fingerprint
                    }),
                    "logical attach artifact mismatch"
                );
                require_sequence!(
                    self.logical_attachment_epochs.insert(epoch_id.clone()),
                    "epoch logical artifact attachment reused"
                );
                require_sequence!(
                    self.logical_attachment_ids.insert(attachment_id.clone()),
                    "logical artifact attachment reused"
                );
                let pending = self
                    .pending_artifact
                    .take()
                    .ok_or(RecordLogError::InvalidSequence("missing artifact commit"))?;
                let PendingArtifact::LogicalAttach { artifact, .. } = pending else {
                    return Err(RecordLogError::InvalidSequence("invalid artifact phase"));
                };
                self.pending_artifact = Some(PendingArtifact::CanonicalSystem { artifact });
            }
            SessionRecord::ProviderArtifactAttachAdvanced {
                provider_epoch_id,
                attach_operation_id,
                attach_revision,
                artifact_fingerprint,
                contract_fingerprint,
                encoder_version,
                skip_attempt_id,
                state,
                ..
            } => {
                require_sequence!(
                    self.is_active(provider_epoch_id),
                    "provider artifact attach outside active epoch"
                );
                require_sequence!(
                    self.logical_attachment_epochs.contains(provider_epoch_id),
                    "provider artifact attach without logical artifact attachment"
                );
                require_sequence!(
                    self.artifacts
                        .get(provider_epoch_id)
                        .is_some_and(|artifact| {
                            artifact.artifact_fingerprint == *artifact_fingerprint
                                && artifact.contract_fingerprint == *contract_fingerprint
                        }),
                    "provider artifact context mismatch"
                );
                let artifact_mode = self
                    .epochs
                    .get(provider_epoch_id)
                    .ok_or(RecordLogError::InvalidSequence("unknown epoch"))?
                    .artifact_mode;
                require_sequence!(
                    self.epochs
                        .get(provider_epoch_id)
                        .is_some_and(|epoch| epoch.profile == *encoder_version),
                    "provider artifact attach encoder mismatch"
                );
                if let Some(sequence) = self.artifact_attach_operations.get_mut(attach_operation_id)
                {
                    require_sequence!(
                        sequence.provider_epoch_id == *provider_epoch_id
                            && sequence.artifact_fingerprint == *artifact_fingerprint
                            && sequence.contract_fingerprint == *contract_fingerprint
                            && sequence.encoder_version == *encoder_version
                            && sequence.skip_attempt_id == *skip_attempt_id,
                        "provider artifact attach context drift"
                    );
                    require_sequence!(
                        *attach_revision == next_revision(sequence.revision)?,
                        "provider artifact attach revision gap"
                    );
                    let valid_transition = matches!(
                        (sequence.state, *state),
                        (
                            ProviderArtifactAttachState::Prepared,
                            ProviderArtifactAttachState::Attempted
                        ) | (
                            ProviderArtifactAttachState::Attempted,
                            ProviderArtifactAttachState::Accepted
                                | ProviderArtifactAttachState::Completed
                                | ProviderArtifactAttachState::Failed
                        ) | (
                            ProviderArtifactAttachState::Accepted,
                            ProviderArtifactAttachState::Completed
                                | ProviderArtifactAttachState::Failed
                        )
                    );
                    require_sequence!(
                        valid_transition,
                        "invalid provider artifact attach transition"
                    );
                    require_sequence!(
                        artifact_mode == ProviderArtifactMode::RemoteRetained,
                        "provider artifact attachment mode mismatch"
                    );
                    sequence.revision = *attach_revision;
                    sequence.state = *state;
                    if matches!(state, ProviderArtifactAttachState::Completed) {
                        self.provider_attached_artifact_epochs
                            .insert(provider_epoch_id.clone());
                    }
                } else {
                    require_sequence!(
                        *attach_revision == 1,
                        "provider artifact attach initial revision gap"
                    );
                    match state {
                        ProviderArtifactAttachState::Prepared => {
                            require_sequence!(
                                artifact_mode == ProviderArtifactMode::RemoteRetained,
                                "provider artifact attachment mode mismatch"
                            );
                            require_sequence!(
                                !self.artifact_attach_operations.values().any(|sequence| {
                                    sequence.provider_epoch_id == *provider_epoch_id
                                        && !matches!(
                                            sequence.state,
                                            ProviderArtifactAttachState::Skipped
                                        )
                                }),
                                "provider artifact attach operation already exists for epoch"
                            );
                        }
                        ProviderArtifactAttachState::Skipped => {
                            require_sequence!(
                                artifact_mode == ProviderArtifactMode::Stateless,
                                "provider artifact attachment mode mismatch"
                            );
                            require_sequence!(
                                !self
                                    .provider_attached_artifact_epochs
                                    .contains(provider_epoch_id),
                                "provider artifact attachment mode changed"
                            );
                            let attempt_id =
                                skip_attempt_id
                                    .as_ref()
                                    .ok_or(RecordLogError::InvalidSequence(
                                        "stateless provider artifact skip requires an attempt id",
                                    ))?;
                            require_sequence!(
                                self.pending_stateless_artifact_skips
                                    .insert(attempt_id.clone(), provider_epoch_id.clone())
                                    .is_none(),
                                "attempt already has an unconsumed stateless artifact skip"
                            );
                            require_sequence!(
                                self.attempts
                                    .get(attempt_id)
                                    .is_some_and(|attempt| matches!(
                                        attempt.state,
                                        AttemptRecordState::Prepared
                                    )),
                                "stateless provider artifact skip requires a prepared attempt"
                            );
                        }
                        _ => {
                            return Err(RecordLogError::InvalidSequence(
                                "provider artifact attach must begin prepared or skipped",
                            ));
                        }
                    }
                    self.artifact_attach_operations.insert(
                        attach_operation_id.clone(),
                        ArtifactAttachSequence {
                            provider_epoch_id: provider_epoch_id.clone(),
                            artifact_fingerprint: artifact_fingerprint.clone(),
                            contract_fingerprint: contract_fingerprint.clone(),
                            encoder_version: encoder_version.clone(),
                            skip_attempt_id: skip_attempt_id.clone(),
                            revision: *attach_revision,
                            state: *state,
                        },
                    );
                }
            }
            SessionRecord::AttemptAdvanced {
                attempt_id,
                snapshot_id,
                attempt_revision,
                state,
            } => {
                let active_epoch = self.active_epoch.as_deref();
                let has_one_prepared_operation = self
                    .operations
                    .values()
                    .filter(|operation| operation.attempt_id == *attempt_id)
                    .count()
                    == 1;
                let has_publication_preparation = self.pending_publication_preparation.is_some();
                match self.attempts.get_mut(attempt_id) {
                    Some(attempt) => {
                        require_sequence!(
                            active_epoch == Some(attempt.provider_epoch_id.as_str()),
                            "attempt epoch drift"
                        );
                        require_sequence!(
                            attempt.snapshot_id == *snapshot_id,
                            "attempt snapshot identity drift"
                        );
                        require_sequence!(
                            *attempt_revision == next_revision(attempt.revision)?,
                            "attempt revision gap"
                        );
                        require_sequence!(
                            valid_attempt_transition(&attempt.state, state),
                            "invalid attempt state transition"
                        );
                        require_sequence!(
                            !self.started_reservation_in_current_append,
                            "Started reservation must end append"
                        );
                        if matches!(state, AttemptRecordState::Issued) {
                            require_sequence!(
                                has_one_prepared_operation,
                                "issued attempt requires one prepared provider operation"
                            );
                        }
                        if matches!(state, AttemptRecordState::Finished) {
                            require_sequence!(
                                has_publication_preparation,
                                "finished attempt requires publication preparation"
                            );
                        }
                        attempt.revision = *attempt_revision;
                        attempt.state = state.clone();
                    }
                    None => {
                        require_sequence!(*attempt_revision == 1, "attempt revision gap");
                        require_sequence!(
                            matches!(state, AttemptRecordState::Started),
                            "invalid initial attempt state"
                        );
                        let provider_epoch_id = active_epoch
                            .ok_or(RecordLogError::InvalidSequence(
                                "attempt requires an active epoch",
                            ))?
                            .to_owned();
                        self.attempts.insert(
                            attempt_id.clone(),
                            AttemptSequence {
                                revision: *attempt_revision,
                                snapshot_id: snapshot_id.clone(),
                                provider_epoch_id,
                                state: state.clone(),
                            },
                        );
                        if self.append_base_transcript_revision.is_some() {
                            self.started_reservation_in_current_append = true;
                        }
                    }
                }
                if matches!(state, AttemptRecordState::Published) {
                    self.pending_publication = None;
                }
                if matches!(state, AttemptRecordState::Finished) {
                    self.pending_publication_preparation = None;
                }
                if matches!(state, AttemptRecordState::Aborted { .. }) {
                    self.pending_stateless_artifact_skips.remove(attempt_id);
                }
            }
            SessionRecord::TurnPublicationPrepared {
                publication_id,
                attempt_id,
                snapshot_id,
                base_transcript_revision,
                previous_cursor,
                next_cursor,
                items,
            } => {
                require_sequence!(
                    !self.prepared_publications.contains_key(publication_id)
                        && !self.publications.contains(publication_id),
                    "publication preparation reused"
                );
                let attempt =
                    self.attempts
                        .get(attempt_id)
                        .ok_or(RecordLogError::InvalidSequence(
                            "unknown prepared publication attempt",
                        ))?;
                require_sequence!(
                    attempt.snapshot_id == *snapshot_id,
                    "prepared publication snapshot mismatch"
                );
                require_sequence!(
                    matches!(attempt.state, AttemptRecordState::Issued),
                    "prepared publication attempt is not issued"
                );
                require_sequence!(
                    self.operations.values().any(|operation| {
                        operation.attempt_id == *attempt_id
                            && matches!(operation.state, Some(ProviderOperationState::Completed))
                    }),
                    "publication preparation requires a completed provider operation"
                );
                require_sequence!(
                    *base_transcript_revision == self.transcript.revision(),
                    "prepared publication transcript base mismatch"
                );
                require_sequence!(
                    *previous_cursor == self.user_diff_cursor,
                    "prepared publication cursor base mismatch"
                );
                let mut staged = self.transcript.clone();
                for item in items {
                    staged = staged.appended(item.clone())?;
                }
                self.prepared_publications.insert(
                    publication_id.clone(),
                    PreparedPublicationSequence {
                        attempt_id: attempt_id.clone(),
                        base_transcript_revision: *base_transcript_revision,
                        next_cursor: *next_cursor,
                        items: items.clone(),
                        next_transcript_revision: staged.revision(),
                    },
                );
                self.pending_publication_preparation = Some(PendingPublicationPreparation {
                    attempt_id: attempt_id.clone(),
                });
            }
            SessionRecord::UserDiffCursorAdvanced {
                publication_id,
                previous_cursor,
                next_cursor,
            } => {
                require_sequence!(
                    *previous_cursor == self.user_diff_cursor,
                    "User diff cursor base mismatch"
                );
                require_sequence!(
                    next_cursor.get() == next_revision(previous_cursor.get())?,
                    "User diff cursor gap"
                );
                require_sequence!(
                    !self.publications.contains(publication_id),
                    "User diff cursor publication reused"
                );
                self.user_diff_cursor = *next_cursor;
                self.pending_publication = Some(PendingPublication::Commit {
                    publication_id: publication_id.clone(),
                    next_cursor: *next_cursor,
                });
            }
            SessionRecord::TurnPublicationCommitted {
                publication_id,
                attempt_id,
                base_transcript_revision,
                next_transcript_revision,
                next_cursor,
            } => {
                let prepared = self.prepared_publications.get(publication_id).ok_or(
                    RecordLogError::InvalidSequence("publication was not prepared"),
                )?;
                require_sequence!(
                    prepared.attempt_id == *attempt_id,
                    "prepared publication attempt mismatch"
                );
                require_sequence!(
                    prepared.base_transcript_revision == *base_transcript_revision,
                    "prepared publication transcript base mismatch"
                );
                require_sequence!(
                    prepared.next_transcript_revision == *next_transcript_revision,
                    "prepared publication transcript mismatch"
                );
                require_sequence!(
                    prepared.next_cursor == *next_cursor,
                    "prepared publication cursor mismatch"
                );
                let base = usize::try_from(prepared.base_transcript_revision.get())
                    .map_err(|_| RecordLogError::RevisionOverflow)?;
                require_sequence!(
                    self.transcript.items().get(base..) == Some(prepared.items.as_slice()),
                    "prepared publication canonical items mismatch"
                );
                let attempt =
                    self.attempts
                        .get(attempt_id)
                        .ok_or(RecordLogError::InvalidSequence(
                            "unknown publication attempt",
                        ))?;
                require_sequence!(
                    matches!(attempt.state, AttemptRecordState::Finished),
                    "publication attempt is not finished"
                );
                require_sequence!(
                    base_transcript_revision.get() <= next_transcript_revision.get(),
                    "publication transcript range"
                );
                if let Some(append_base) = self.append_base_transcript_revision {
                    require_sequence!(
                        append_base == *base_transcript_revision,
                        "publication base must equal append-start transcript authority"
                    );
                }
                require_sequence!(
                    *next_transcript_revision == self.transcript.revision(),
                    "publication transcript mismatch"
                );
                require_sequence!(
                    *next_cursor == self.user_diff_cursor,
                    "publication cursor mismatch"
                );
                require_sequence!(
                    self.publications.insert(publication_id.clone()),
                    "publication reused"
                );
                self.pending_publication = Some(PendingPublication::AttemptPublished {
                    publication_id: publication_id.clone(),
                    attempt_id: attempt_id.clone(),
                });
            }
        }
        Ok(self)
    }
}

fn valid_attempt_transition(previous: &AttemptRecordState, next: &AttemptRecordState) -> bool {
    matches!(
        (previous, next),
        (
            AttemptRecordState::Started,
            AttemptRecordState::Prepared | AttemptRecordState::Aborted { .. }
        ) | (
            AttemptRecordState::Prepared,
            AttemptRecordState::Issued | AttemptRecordState::Aborted { .. }
        ) | (
            AttemptRecordState::Issued,
            AttemptRecordState::Finished | AttemptRecordState::Aborted { .. }
        ) | (AttemptRecordState::Finished, AttemptRecordState::Published)
    )
}

#[derive(Default)]
struct MemoryState {
    sessions: BTreeMap<SessionId, Vec<RecordEnvelope>>,
    fail_next_append: bool,
}

/// In-memory conformance implementation, not a production durability claim.
#[derive(Default)]
pub struct MemoryRecordStore {
    state: Mutex<MemoryState>,
}

impl MemoryRecordStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms one deterministic backend failure for conformance tests.
    pub fn fail_next_append(&self) -> Result<(), RecordLogError> {
        let mut state = self.state()?;
        state.fail_next_append = true;
        Ok(())
    }

    fn state(&self) -> Result<MutexGuard<'_, MemoryState>, RecordLogError> {
        self.state.lock().map_err(|_| RecordLogError::Poisoned)
    }
}

#[async_trait]
impl RecordLog for MemoryRecordStore {
    type Error = RecordLogError;

    async fn read(
        &self,
        session_id: &SessionId,
        after_revision: LogRevision,
    ) -> Result<Vec<RecordEnvelope>, Self::Error> {
        let state = self.state()?;
        let records = state
            .sessions
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let tail = revision_from_len(records.len())?;
        if after_revision > tail {
            return Err(RecordLogError::RevisionBeyondTail {
                requested: after_revision,
                tail,
            });
        }
        Ok(records
            .iter()
            .filter(|record| record.revision > after_revision)
            .cloned()
            .collect())
    }

    async fn append(
        &self,
        session_id: &SessionId,
        expected_tail: LogRevision,
        batch: RecordBatch,
    ) -> Result<AppendOutcome, Self::Error> {
        let mut state = self.state()?;
        if state.fail_next_append {
            state.fail_next_append = false;
            return Err(RecordLogError::InjectedFailure);
        }

        let existing = state
            .sessions
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let current = existing.len();
        let actual_tail = revision_from_len(current)?;
        if actual_tail != expected_tail {
            return Ok(AppendOutcome::Conflict { actual_tail });
        }
        validate_record_append(session_id, existing, &batch)?;

        let mut next_revision = actual_tail.get();
        let mut envelopes = Vec::with_capacity(batch.records().len());
        for record in batch.into_records() {
            next_revision = next_revision
                .checked_add(1)
                .ok_or(RecordLogError::RevisionOverflow)?;
            envelopes.push(RecordEnvelope::new(
                session_id.clone(),
                LogRevision::new(next_revision),
                record,
            )?);
        }

        state
            .sessions
            .entry(session_id.clone())
            .or_default()
            .extend(envelopes);
        Ok(AppendOutcome::Committed {
            tail: LogRevision::new(next_revision),
        })
    }
}

/// Rebuilds canonical history from a complete session record sequence.
pub fn reconstruct_transcript(
    records: &[RecordEnvelope],
) -> Result<CanonicalTranscript, TranscriptReconstructionError> {
    let mut transcript = CanonicalTranscript::new();
    let mut session_id: Option<&SessionId> = None;
    let mut expected_log_revision = 1_u64;

    for envelope in records {
        if session_id.is_some_and(|session| session != envelope.session_id()) {
            return Err(TranscriptReconstructionError::MixedSessions);
        }
        session_id = Some(envelope.session_id());
        if envelope.revision().get() != expected_log_revision {
            return Err(TranscriptReconstructionError::NonContiguousLog {
                expected: LogRevision::new(expected_log_revision),
                actual: envelope.revision(),
            });
        }
        expected_log_revision += 1;

        if let SessionRecord::CanonicalItemAppended {
            transcript_revision,
            item,
        } = envelope.record()
        {
            let expected = transcript
                .revision()
                .get()
                .checked_add(1)
                .ok_or(TranscriptReconstructionError::RevisionOverflow)?;
            if transcript_revision.get() != expected {
                return Err(TranscriptReconstructionError::NonContiguousTranscript {
                    expected: TranscriptRevision::new(expected),
                    actual: *transcript_revision,
                });
            }
            transcript = transcript.appended(item.clone())?;
        }
    }
    Ok(transcript)
}

/// Invalid record input or in-memory conformance failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecordLogError {
    #[error("invalid {kind} `{value}`; identifiers must be non-empty ASCII tokens")]
    InvalidIdentifier { kind: &'static str, value: String },
    #[error("record schema version must be greater than zero")]
    ZeroSchemaVersion,
    #[error("record batch must contain at least one record")]
    EmptyBatch,
    #[error("{kind} must be greater than zero")]
    ZeroRevision { kind: &'static str },
    #[error("prepared provider operation request body must be non-empty")]
    EmptyRequestBody,
    #[error("failed to encode durable artifact: {0}")]
    ArtifactEncoding(String),
    #[error("provider request digest must use lowercase sha256:<64 hex> form")]
    InvalidSha256,
    #[error("provider request digest mismatch: declared {declared}, computed {computed}")]
    RequestDigestMismatch { declared: String, computed: String },
    #[error("invalid provider artifact attach disposition `{0}`")]
    InvalidProviderArtifactAttachDisposition(String),
    #[error("invalid authoritative record sequence: {0}")]
    InvalidSequence(&'static str),
    #[error(transparent)]
    Canonical(#[from] CanonicalTranscriptError),
    #[error("record revision overflow")]
    RevisionOverflow,
    #[error("requested revision {requested:?} is beyond current tail {tail:?}")]
    RevisionBeyondTail {
        requested: LogRevision,
        tail: LogRevision,
    },
    #[error("in-memory record store mutex is poisoned")]
    Poisoned,
    #[error("injected append failure")]
    InjectedFailure,
}

/// Invalid authoritative record sequence during transcript reconstruction.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum TranscriptReconstructionError {
    #[error("record sequence mixes multiple session ids")]
    MixedSessions,
    #[error("record log is not contiguous: expected {expected:?}, got {actual:?}")]
    NonContiguousLog {
        expected: LogRevision,
        actual: LogRevision,
    },
    #[error("transcript is not contiguous: expected {expected:?}, got {actual:?}")]
    NonContiguousTranscript {
        expected: TranscriptRevision,
        actual: TranscriptRevision,
    },
    #[error("revision overflow while reconstructing transcript")]
    RevisionOverflow,
    #[error(transparent)]
    Canonical(#[from] CanonicalTranscriptError),
}

fn validate_identifier(kind: &'static str, value: String) -> Result<String, RecordLogError> {
    validate_identifier_ref(kind, &value)?;
    Ok(value)
}

fn validate_identifier_ref(kind: &'static str, value: &str) -> Result<(), RecordLogError> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        });
    if valid {
        Ok(())
    } else {
        Err(RecordLogError::InvalidIdentifier {
            kind,
            value: value.to_owned(),
        })
    }
}

fn require_nonzero(kind: &'static str, revision: u64) -> Result<(), RecordLogError> {
    if revision == 0 {
        Err(RecordLogError::ZeroRevision { kind })
    } else {
        Ok(())
    }
}

fn validate_canonical_item(item: &CanonicalInputItem) -> Result<(), RecordLogError> {
    match item {
        CanonicalInputItem::Instruction { .. }
        | CanonicalInputItem::Message { .. }
        | CanonicalInputItem::AssistantText { .. } => Ok(()),
        CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } => {
            CanonicalInputItem::tool_call(call_id, name, raw_arguments)?;
            Ok(())
        }
        CanonicalInputItem::ToolResult { call_id, content } => {
            CanonicalInputItem::tool_result(call_id, content)?;
            Ok(())
        }
        CanonicalInputItem::ProviderExtension(extension) => {
            crate::transcript::ProviderExtension::new(
                extension.provider(),
                extension.capability(),
                extension.schema_version(),
                extension.payload().clone(),
            )?;
            Ok(())
        }
    }
}

fn validate_sha256(value: &str, request_body: &[u8]) -> Result<(), RecordLogError> {
    let digest = value
        .strip_prefix("sha256:")
        .ok_or(RecordLogError::InvalidSha256)?;
    let is_lower_hex = digest
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (digest.len() == 64 && is_lower_hex)
        .then_some(())
        .ok_or(RecordLogError::InvalidSha256)?;
    let computed = format!("sha256:{:x}", Sha256::digest(request_body));
    if value == computed {
        Ok(())
    } else {
        Err(RecordLogError::RequestDigestMismatch {
            declared: value.to_owned(),
            computed,
        })
    }
}

fn validate_content_fingerprint(
    schema: &str,
    value: &str,
    content: &[u8],
) -> Result<(), RecordLogError> {
    let digest = value
        .strip_prefix(schema)
        .and_then(|value| value.strip_prefix(":sha256:"))
        .ok_or(RecordLogError::InvalidSha256)?;
    let is_lower_hex = digest
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if digest.len() != 64 || !is_lower_hex {
        return Err(RecordLogError::InvalidSha256);
    }
    let computed = format!("{schema}:sha256:{:x}", Sha256::digest(content));
    if value == computed {
        Ok(())
    } else {
        Err(RecordLogError::RequestDigestMismatch {
            declared: value.to_owned(),
            computed,
        })
    }
}

fn revision_from_len(length: usize) -> Result<LogRevision, RecordLogError> {
    u64::try_from(length)
        .map(LogRevision::new)
        .map_err(|_| RecordLogError::RevisionOverflow)
}
fn next_revision(revision: u64) -> Result<u64, RecordLogError> {
    revision
        .checked_add(1)
        .ok_or(RecordLogError::RevisionOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_next_append_propagates_a_poisoned_mutex() {
        let store = MemoryRecordStore::new();
        let panic = std::panic::catch_unwind(|| {
            let _state = store.state.lock().unwrap();
            panic!("poison the record-store mutex");
        });
        assert!(panic.is_err());

        assert_eq!(store.fail_next_append(), Err(RecordLogError::Poisoned));
    }
}
