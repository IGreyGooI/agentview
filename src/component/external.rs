//! Durable host control for externally supplied mounted replies.
//!
//! This is deliberately separate from the provider-backed mounted owner. An
//! external caller supplies a reply after receiving a User document, while the
//! application host owns the domain mutation, persistence transaction, wake
//! publication, and recovery policy. The controller never receives a mutable
//! domain capability.

use std::{error::Error, fmt, sync::Arc};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    agent_view::AgentView,
    control::ControlReply,
    pom::Document,
    pom_cursor::UserDocumentCursor,
    pom_renderer::{render_pom_document, PomRenderError},
    pom_resolution::{resolve_user_document, PomResolutionError},
    StorageString,
};

use super::{
    durable_host::{MountedStateBlob, MountedStateGeneration},
    external_reply::{ExternalHarnessRuntime, MountedExternalHarnessDefinition},
    lifecycle::{compile_user_view, mount_system_component_with_contract, SystemMountError},
    ComponentError, DurableCallId, DurableCallInputId, DurableSessionId, EpochContractId,
    UserTurnContext,
};

// Schema v5 moves User publication behind the host port and advances the POM
// cursor only after an explicit delivery acknowledgement. Earlier states
// exposed raw User bytes without a durable delivery receipt, so they cannot be
// assigned a trustworthy delta baseline during reopen.
const EXTERNAL_STATE_SCHEMA_VERSION: u32 = 5;
const MAX_EXTERNAL_STATE_RETRIES: usize = 16;

/// Error returned when a durable external identity contains an invalid value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid external {kind} `{value}`; values must be non-empty and contain no control characters"
)]
pub struct ExternalIdentifierError {
    kind: &'static str,
    value: String,
}

fn checked_identifier(
    kind: &'static str,
    value: impl Into<StorageString>,
) -> Result<StorageString, ExternalIdentifierError> {
    let value = value.into();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ExternalIdentifierError {
            kind,
            value: value.to_string(),
        });
    }
    Ok(value)
}

/// Stable route selected by one external reply contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalActionRoute(StorageString);

impl ExternalActionRoute {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("action route", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalActionRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable semantic identity of an external reply decoder.
///
/// This is distinct from [`ExternalActionRoute`]: a route says where a typed
/// action is delivered, while this id covers the reply grammar, validation,
/// and action interpretation. Change it whenever the same wire reply could
/// decode differently. Hosts should include the same semantic version in the
/// mounted System epoch contract they choose for this controller.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalReplyContractId(StorageString);

impl ExternalReplyContractId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("reply contract id", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalReplyContractId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-issued revision for the exact domain snapshot used to render User.
///
/// The host validates this value again inside its domain/session transaction.
/// It must change whenever accepting the old action could mutate a different
/// domain state than the one the caller observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalSourceRevision(StorageString);

impl ExternalSourceRevision {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("source revision", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalSourceRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether one captured external frame accepts a reply.
///
/// This is runtime scheduling metadata, not a second kind of POM. The host must
/// derive it from the same authoritative domain phase captured with the props.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalFrameKind {
    Actionable,
    Passive,
}

/// One immutable domain snapshot selected by a host for a fresh external User
/// document or passive presentation.
///
/// The source revision and props intentionally travel together. A host must
/// capture both from the same authoritative snapshot, then pass this value to
/// [`MountedExternalController::observe`] or return it from
/// [`MountedExternalController::hook`]. The port still revalidates the
/// revision inside its commit transaction; this type prevents an accidental
/// API-level pairing of a new revision with stale prompt props.
#[derive(Clone)]
#[must_use]
pub struct ExternalObservation<Props: ?Sized + 'static> {
    source_revision: ExternalSourceRevision,
    props: Arc<Props>,
}

impl<Props: ?Sized + 'static> ExternalObservation<Props> {
    /// Construct an observation from one host-owned immutable snapshot.
    pub fn new(source_revision: ExternalSourceRevision, props: Arc<Props>) -> Self {
        Self {
            source_revision,
            props,
        }
    }

    /// Revision the host must revalidate if this observation becomes an
    /// external action.
    pub fn source_revision(&self) -> &ExternalSourceRevision {
        &self.source_revision
    }

    /// Exact immutable props used to render the User document.
    pub fn props(&self) -> &Props {
        self.props.as_ref()
    }

    /// Consume the observation while retaining both parts for host code that
    /// needs to hand the owned snapshot to another boundary.
    pub fn into_parts(self) -> (ExternalSourceRevision, Arc<Props>) {
        (self.source_revision, self.props)
    }
}

impl<Props: 'static> ExternalObservation<Props> {
    /// Construct an observation from owned props when the host does not already
    /// retain its snapshot in an [`Arc`].
    pub fn from_props(source_revision: ExternalSourceRevision, props: Props) -> Self {
        Self::new(source_revision, Arc::new(props))
    }
}

/// Caller-selected idempotency identity for one external reply.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalReplyId(StorageString);

impl ExternalReplyId {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("reply id", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalReplyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Opaque durable wake position issued by the host transaction authority.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalWakeCursor(StorageString);

impl ExternalWakeCursor {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("wake cursor", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalWakeCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Opaque durable proof that the host accepted one System delivery obligation.
///
/// A receipt represents either a provider/client acknowledgement or a durable,
/// ordered outbox record keyed to the rendered epoch artifact. It is not an
/// invitation for the controller to resend System bytes. A host may return
/// `Existing` only after this receipt is durable, and it must ensure every User
/// delivery is ordered after the recorded System obligation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalSystemDeliveryReceipt(StorageString);

impl ExternalSystemDeliveryReceipt {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("System delivery receipt", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalSystemDeliveryReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Canonical SHA-256 fingerprint retained in an action or reply ledger.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalFingerprint(StorageString);

impl ExternalFingerprint {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ExternalIdentifierError> {
        Ok(Self(checked_identifier("fingerprint", value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn fingerprint_bytes(prefix: &str, bytes: &[u8]) -> ExternalFingerprint {
    let mut digest = Sha256::new();
    digest.update(prefix.as_bytes());
    digest.update([0]);
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    ExternalFingerprint(format!("sha256:v1:{:x}", digest.finalize()).into())
}

fn fingerprint_rendered_user(rendered: &str) -> ExternalFingerprint {
    fingerprint_bytes("agentview.external.user", rendered.as_bytes())
}

/// Durable identity of exact User bytes accepted by the host publication
/// transaction.
///
/// This receipt proves that the host retained an immutable outbox row. It does
/// not by itself prove that a model/client consumed the bytes; that separate
/// fact is recorded through
/// [`MountedExternalController::acknowledge_user_delivery`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalUserDeliveryReceipt(StorageString);

impl ExternalUserDeliveryReceipt {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalUserDeliveryReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Delivery lane selected by the pure external frame declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalUserDeliveryLane {
    /// A real model/client User message. Its acknowledged cursor may become the
    /// base of a later delta.
    Prompt,
    /// An observer/UI update. It never reads or advances the prompt baseline.
    Presentation,
}

/// Exact User publication handed to the host inside the state CAS.
///
/// The host must insert an immutable outbox row for this candidate in the same
/// transaction as the supplied controller state. Retrying the same receipt
/// must retain the same bytes and dependency metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalUserDeliveryCandidate {
    receipt: ExternalUserDeliveryReceipt,
    lane: ExternalUserDeliveryLane,
    system_delivery_receipt: ExternalSystemDeliveryReceipt,
    base_receipt: Option<ExternalUserDeliveryReceipt>,
    user_fingerprint: ExternalFingerprint,
    rendered_user: StorageString,
}

impl ExternalUserDeliveryCandidate {
    fn new(
        receipt: ExternalUserDeliveryReceipt,
        lane: ExternalUserDeliveryLane,
        system_delivery_receipt: ExternalSystemDeliveryReceipt,
        base_receipt: Option<ExternalUserDeliveryReceipt>,
        user_fingerprint: ExternalFingerprint,
        rendered_user: impl Into<StorageString>,
    ) -> Self {
        Self {
            receipt,
            lane,
            system_delivery_receipt,
            base_receipt,
            user_fingerprint,
            rendered_user: rendered_user.into(),
        }
    }

    pub fn receipt(&self) -> &ExternalUserDeliveryReceipt {
        &self.receipt
    }

    pub fn lane(&self) -> ExternalUserDeliveryLane {
        self.lane
    }

    pub fn system_delivery_receipt(&self) -> &ExternalSystemDeliveryReceipt {
        &self.system_delivery_receipt
    }

    /// Acknowledged prompt delivery used to resolve this payload. `None` means
    /// the candidate is a self-contained full prompt/presentation.
    pub fn base_receipt(&self) -> Option<&ExternalUserDeliveryReceipt> {
        self.base_receipt.as_ref()
    }

    pub fn user_fingerprint(&self) -> &ExternalFingerprint {
        &self.user_fingerprint
    }

    /// Exact bytes the host must retain and publish for this receipt.
    pub fn rendered_user(&self) -> &str {
        &self.rendered_user
    }
}

fn user_delivery_receipt(
    session_id: &DurableSessionId,
    epoch_generation: u64,
    turn_index: u64,
    lane: ExternalUserDeliveryLane,
    system_delivery_receipt: &ExternalSystemDeliveryReceipt,
    base_receipt: Option<&ExternalUserDeliveryReceipt>,
    user_fingerprint: &ExternalFingerprint,
) -> ExternalUserDeliveryReceipt {
    let lane = match lane {
        ExternalUserDeliveryLane::Prompt => "prompt",
        ExternalUserDeliveryLane::Presentation => "presentation",
    };
    let base_receipt = base_receipt.map_or("", ExternalUserDeliveryReceipt::as_str);
    let identity = format!(
        "{}\0{epoch_generation}\0{turn_index}\0{lane}\0{}\0{base_receipt}\0{}",
        session_id.as_str(),
        system_delivery_receipt.as_str(),
        user_fingerprint.as_str()
    );
    let fingerprint = fingerprint_bytes("agentview.external.user.delivery", identity.as_bytes());
    ExternalUserDeliveryReceipt(fingerprint.0)
}

fn fingerprint_control_reply(
    reply: &ControlReply,
) -> Result<ExternalFingerprint, serde_json::Error> {
    let value = serde_json::to_value(reply)?;
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical);
    Ok(fingerprint_bytes(
        "agentview.external.reply",
        canonical.as_bytes(),
    ))
}

fn write_canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("strings always serialize"));
        }
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).expect("keys always serialize"));
                output.push(':');
                write_canonical_json(value, output);
            }
            output.push('}');
        }
    }
}

/// Opaque external-system generation selected by the authoritative host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalEpochLease {
    session_id: DurableSessionId,
    epoch_contract_id: EpochContractId,
    generation: u64,
    value: StorageString,
}

impl ExternalEpochLease {
    /// Construct a lease returned by a host implementation.
    ///
    /// Application code normally receives this only inside
    /// [`ExternalEpochAdmission::Create`].
    pub fn new(
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        generation: u64,
        value: impl Into<StorageString>,
    ) -> Result<Self, ExternalIdentifierError> {
        Ok(Self {
            session_id,
            epoch_contract_id,
            generation,
            value: checked_identifier("epoch lease", value)?,
        })
    }

    pub fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

/// Durable metadata for an activated external System epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalEpochArtifact {
    session_id: DurableSessionId,
    epoch_contract_id: EpochContractId,
    generation: u64,
    system_fingerprint: ExternalFingerprint,
}

impl ExternalEpochArtifact {
    pub fn from_rendered_system(lease: &ExternalEpochLease, rendered_system: &str) -> Self {
        Self {
            session_id: lease.session_id.clone(),
            epoch_contract_id: lease.epoch_contract_id.clone(),
            generation: lease.generation,
            system_fingerprint: fingerprint_bytes(
                "agentview.external.system",
                rendered_system.as_bytes(),
            ),
        }
    }

    pub fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn system_fingerprint(&self) -> &ExternalFingerprint {
        &self.system_fingerprint
    }
}

/// Request to reserve or reopen one durable external System epoch.
#[derive(Debug, Clone, Copy)]
pub struct ExternalEpochAcquireRequest<'a> {
    session_id: &'a DurableSessionId,
    epoch_contract_id: &'a EpochContractId,
}

impl<'a> ExternalEpochAcquireRequest<'a> {
    pub fn new(session_id: &'a DurableSessionId, epoch_contract_id: &'a EpochContractId) -> Self {
        Self {
            session_id,
            epoch_contract_id,
        }
    }

    pub fn session_id(&self) -> &'a DurableSessionId {
        self.session_id
    }

    pub fn epoch_contract_id(&self) -> &'a EpochContractId {
        self.epoch_contract_id
    }
}

/// Durable result of external System epoch admission.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExternalEpochAdmission {
    /// This owner alone may render and install the System POM.
    Create { lease: ExternalEpochLease },
    /// A prior owner activated the matching epoch. Reopen must not render or
    /// return a second System document.
    Existing {
        artifact: ExternalEpochArtifact,
        system_delivery_receipt: ExternalSystemDeliveryReceipt,
        state: ExternalStateSnapshot,
    },
    /// Another owner currently holds the Create lease.
    InFlight { generation: u64 },
    /// A previous owner stopped at an ambiguous epoch boundary.
    RecoveryRequired { generation: u64 },
    /// A different durable System contract is active for this session.
    ContractMismatch {
        active: EpochContractId,
        requested: EpochContractId,
    },
}

/// Request that atomically persists one System delivery obligation and the
/// initial opaque AgentView action state.
///
/// The port receives the only System bytes it may deliver for this epoch. It
/// must durably create an idempotent provider/client acknowledgement or an
/// ordered outbox record before reporting [`ExternalEpochActivation`].
pub struct ExternalEpochComplete<'a> {
    lease: &'a ExternalEpochLease,
    artifact: &'a ExternalEpochArtifact,
    rendered_system: &'a str,
    initial_state: &'a MountedStateBlob,
}

/// Result of durably activating one external epoch after its System delivery
/// obligation has been recorded.
#[derive(Debug, Clone)]
pub struct ExternalEpochActivation {
    state: ExternalStateSnapshot,
    system_delivery_receipt: ExternalSystemDeliveryReceipt,
}

impl ExternalEpochActivation {
    pub fn new(
        state: ExternalStateSnapshot,
        system_delivery_receipt: ExternalSystemDeliveryReceipt,
    ) -> Self {
        Self {
            state,
            system_delivery_receipt,
        }
    }

    pub fn state(&self) -> &ExternalStateSnapshot {
        &self.state
    }

    pub fn system_delivery_receipt(&self) -> &ExternalSystemDeliveryReceipt {
        &self.system_delivery_receipt
    }
}

impl<'a> ExternalEpochComplete<'a> {
    pub fn new(
        lease: &'a ExternalEpochLease,
        artifact: &'a ExternalEpochArtifact,
        rendered_system: &'a str,
        initial_state: &'a MountedStateBlob,
    ) -> Self {
        Self {
            lease,
            artifact,
            rendered_system,
            initial_state,
        }
    }

    pub fn lease(&self) -> &'a ExternalEpochLease {
        self.lease
    }

    pub fn artifact(&self) -> &'a ExternalEpochArtifact {
        self.artifact
    }

    pub fn rendered_system(&self) -> &'a str {
        self.rendered_system
    }

    pub fn initial_state(&self) -> &'a MountedStateBlob {
        self.initial_state
    }
}

/// One authoritative external-controller state observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalStateSnapshot {
    generation: Option<MountedStateGeneration>,
    state: Option<MountedStateBlob>,
    wake_cursor: ExternalWakeCursor,
}

impl ExternalStateSnapshot {
    pub fn missing(wake_cursor: ExternalWakeCursor) -> Self {
        Self {
            generation: None,
            state: None,
            wake_cursor,
        }
    }

    pub fn present(
        generation: MountedStateGeneration,
        state: MountedStateBlob,
        wake_cursor: ExternalWakeCursor,
    ) -> Self {
        Self {
            generation: Some(generation),
            state: Some(state),
            wake_cursor,
        }
    }

    pub fn generation(&self) -> Option<&MountedStateGeneration> {
        self.generation.as_ref()
    }

    pub fn state(&self) -> Option<&MountedStateBlob> {
        self.state.as_ref()
    }

    pub fn wake_cursor(&self) -> &ExternalWakeCursor {
        &self.wake_cursor
    }
}

/// Stable identity used to resolve an indeterminate external domain commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCommitIdentity {
    token: ExternalActionToken,
    reply_id: ExternalReplyId,
    reply_fingerprint: ExternalFingerprint,
}

impl ExternalCommitIdentity {
    pub fn new(
        token: ExternalActionToken,
        reply_id: ExternalReplyId,
        reply_fingerprint: ExternalFingerprint,
    ) -> Self {
        Self {
            token,
            reply_id,
            reply_fingerprint,
        }
    }

    pub fn token(&self) -> &ExternalActionToken {
        &self.token
    }

    pub fn reply_id(&self) -> &ExternalReplyId {
        &self.reply_id
    }

    pub fn reply_fingerprint(&self) -> &ExternalFingerprint {
        &self.reply_fingerprint
    }
}

/// The host operation attached to an AgentView external-state write.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExternalStateMutation<'a, Action> {
    /// Publish a rendered User document and reserve its typed reply token after
    /// validating the source revision. A prior passive presentation may be
    /// superseded by the same atomic write.
    PublishActionable {
        delivery: &'a ExternalUserDeliveryCandidate,
        ticket: &'a ExternalActionTicket,
        supersedes: Option<&'a ExternalPassivePresentation>,
    },
    /// Publish a rendered view that accepts no reply. A prior passive
    /// presentation may be superseded by the same atomic write.
    PublishPassive {
        delivery: &'a ExternalUserDeliveryCandidate,
        presentation: &'a ExternalPassivePresentation,
        supersedes: Option<&'a ExternalPassivePresentation>,
    },
    /// Confirm that the prompt consumer accepted one exact User delivery.
    /// The same transaction promotes its pending cursor to the next durable
    /// delta baseline.
    AcknowledgeUserDelivery {
        receipt: &'a ExternalUserDeliveryReceipt,
    },
    /// Persist a User-only full-resync fence for the next Actionable prompt.
    /// The active System epoch and its delivery receipt remain unchanged.
    RequestUserDocumentResync,
    /// Revalidate and persist a pure decoded reply before attempting its domain
    /// mutation. The host may reject the typed action here without changing
    /// state; it must perform no domain side effect in this phase.
    PrepareReply {
        identity: &'a ExternalCommitIdentity,
        action: &'a Action,
    },
    /// Atomically apply the typed domain action, update AgentView state, write
    /// the external reply ledger, publish any host outbox rows, and advance
    /// the wake cursor.
    Commit {
        identity: &'a ExternalCommitIdentity,
        action: &'a Action,
    },
    /// Cancel a reply that has not entered the domain transaction.
    Cancel {
        token: &'a ExternalActionToken,
        delivery_receipt: &'a ExternalUserDeliveryReceipt,
    },
    /// Fence a reply after the host cannot determine whether its domain commit
    /// happened. A port may accept this only after it has quiesced the commit
    /// identity, so no delayed commit can race the recovery record.
    MarkRecovery {
        identity: &'a ExternalCommitIdentity,
    },
}

impl<'a, Action> ExternalStateMutation<'a, Action> {
    pub fn token(&self) -> Option<&ExternalActionToken> {
        match self {
            Self::PublishActionable { ticket, .. } => Some(ticket.token()),
            Self::PublishPassive { .. } => None,
            Self::AcknowledgeUserDelivery { .. } => None,
            Self::RequestUserDocumentResync => None,
            Self::Cancel { token, .. } => Some(token),
            Self::PrepareReply { identity, .. }
            | Self::Commit { identity, .. }
            | Self::MarkRecovery { identity } => Some(identity.token()),
        }
    }

    /// Source revision that must still match for a fresh frame or action.
    pub fn source_revision(&self) -> Option<&ExternalSourceRevision> {
        match self {
            Self::PublishActionable { ticket, .. } => Some(ticket.token().source_revision()),
            Self::PublishPassive { presentation, .. } => Some(presentation.source_revision()),
            Self::PrepareReply { identity, .. } | Self::Commit { identity, .. } => {
                Some(identity.token().source_revision())
            }
            Self::AcknowledgeUserDelivery { .. }
            | Self::RequestUserDocumentResync
            | Self::Cancel { .. }
            | Self::MarkRecovery { .. } => None,
        }
    }

    /// Immutable User outbox candidate attached to a publication write.
    pub fn user_delivery(&self) -> Option<&ExternalUserDeliveryCandidate> {
        match self {
            Self::PublishActionable { delivery, .. } | Self::PublishPassive { delivery, .. } => {
                Some(delivery)
            }
            _ => None,
        }
    }
}

/// Compare-and-exchange request containing the next opaque AgentView state.
///
/// A [`MountedExternalPort`] must never split a `Commit`: the domain mutation,
/// reply ledger, state replacement, outbox insertion, and wake publication
/// are one transaction.
pub struct ExternalStateWrite<'a, Action> {
    session_id: &'a DurableSessionId,
    expected_generation: Option<&'a MountedStateGeneration>,
    state: &'a MountedStateBlob,
    mutation: ExternalStateMutation<'a, Action>,
}

impl<'a, Action> ExternalStateWrite<'a, Action> {
    pub fn new(
        session_id: &'a DurableSessionId,
        expected_generation: Option<&'a MountedStateGeneration>,
        state: &'a MountedStateBlob,
        mutation: ExternalStateMutation<'a, Action>,
    ) -> Self {
        Self {
            session_id,
            expected_generation,
            state,
            mutation,
        }
    }

    pub fn session_id(&self) -> &'a DurableSessionId {
        self.session_id
    }

    pub fn expected_generation(&self) -> Option<&'a MountedStateGeneration> {
        self.expected_generation
    }

    pub fn state(&self) -> &'a MountedStateBlob {
        self.state
    }

    pub fn mutation(&self) -> &ExternalStateMutation<'a, Action> {
        &self.mutation
    }
}

/// Result of one external-controller state transaction.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExternalStateWriteOutcome {
    Committed {
        generation: MountedStateGeneration,
        wake_cursor: ExternalWakeCursor,
    },
    Conflict {
        current: ExternalStateSnapshot,
    },
    SourceStale {
        current: ExternalStateSnapshot,
        actual: ExternalSourceRevision,
    },
    /// The host cannot say whether the domain transaction committed.
    ///
    /// This outcome is valid only for [`ExternalStateMutation::Commit`]. A
    /// state-only Publish, AcknowledgeUserDelivery, RequestUserDocumentResync,
    /// PrepareReply, Cancel, or MarkRecovery operation
    /// must resolve its own durability before returning (normally as
    /// `Committed` with a new generation or `Conflict`). The controller can
    /// recover a commit identity, but it has no sound generic recovery key for
    /// an ambiguous state-only write.
    Indeterminate,
}

/// Authoritative resolution for a previously indeterminate commit identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExternalCommitResolution {
    Committed,
    NotCommitted,
    Unknown,
}

/// Atomic external host port.
///
/// This is intentionally not an adapter around [`super::MountedAgent`]. The
/// host must implement it over the real transaction authority that owns both
/// application domain state and the opaque AgentView state blob. In particular
/// `Commit` must atomically perform all of the following:
///
/// 1. revalidate `token.source_revision()` against the domain row/snapshot;
/// 2. apply the typed `Action` exactly once for `ExternalCommitIdentity`;
/// 3. persist the supplied AgentView state bytes and reply ledger;
/// 4. insert host outbox rows and publish the returned wake cursor.
///
/// User publication mutations additionally require an immutable outbox row
/// ordered after their `system_delivery_receipt`. Acknowledgement mutations
/// must verify a real consumer acknowledgement before promoting the supplied
/// state. An unacknowledged cancellation may commit only if the host proves
/// that row was not consumed and durably prevents later delivery.
///
/// A process-local mutex plus a separate domain write is not a valid
/// implementation, because a crash between the two operations breaks replay
/// and recovery semantics.
#[async_trait::async_trait]
pub trait MountedExternalPort<Action>: Send + Sync + 'static
where
    Action: Send + Sync + 'static,
{
    type Error: Error + Send + Sync + 'static;

    /// Reserve the sole owner allowed to execute deferred System POM.
    async fn acquire_epoch(
        &self,
        request: ExternalEpochAcquireRequest<'_>,
    ) -> Result<ExternalEpochAdmission, Self::Error>;

    /// Atomically persist a System rendered by the owner of `lease`, its
    /// ordered delivery receipt/outbox identity, and the initial controller
    /// state.
    ///
    /// The port owns the actual transport or its durable outbox. It must not
    /// return [`ExternalEpochActivation`] until it has retained an idempotent
    /// System-delivery receipt. If a crash leaves delivery pending or
    /// ambiguous, a later [`Self::acquire_epoch`] must return `InFlight` or
    /// `RecoveryRequired`, never `Existing`.
    async fn complete_epoch(
        &self,
        request: ExternalEpochComplete<'_>,
    ) -> Result<ExternalEpochActivation, Self::Error>;

    /// Abandon a Create lease before System installation, for example when
    /// System POM compilation fails. This must not change an active epoch.
    async fn abandon_epoch(&self, lease: &ExternalEpochLease) -> Result<(), Self::Error>;

    /// Load the authoritative opaque state and current durable wake cursor.
    async fn load(
        &self,
        session_id: &DurableSessionId,
    ) -> Result<ExternalStateSnapshot, Self::Error>;

    /// Perform one atomic controller/domain mutation.
    async fn compare_exchange(
        &self,
        request: ExternalStateWrite<'_, Action>,
    ) -> Result<ExternalStateWriteOutcome, Self::Error>;

    /// Wait until a domain/session transaction advances the durable wake
    /// cursor beyond `after` and return the newest known cursor.
    async fn wait_for_wake(
        &self,
        session_id: &DurableSessionId,
        after: &ExternalWakeCursor,
    ) -> Result<ExternalWakeCursor, Self::Error>;

    /// Resolve a potentially committed domain transaction. `Unknown` is not
    /// permission to retry; the controller fences it as `RecoveryRequired`.
    async fn resolve_commit(
        &self,
        identity: &ExternalCommitIdentity,
    ) -> Result<ExternalCommitResolution, Self::Error>;
}

/// Stable ticket returned to an external caller after User rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActionTicket {
    token: ExternalActionToken,
    delivery_receipt: ExternalUserDeliveryReceipt,
}

impl ExternalActionTicket {
    fn new(token: ExternalActionToken, delivery_receipt: ExternalUserDeliveryReceipt) -> Self {
        Self {
            token,
            delivery_receipt,
        }
    }

    pub fn token(&self) -> &ExternalActionToken {
        &self.token
    }

    /// Host-owned outbox receipt for the exact User bytes bound to this token.
    pub fn delivery_receipt(&self) -> &ExternalUserDeliveryReceipt {
        &self.delivery_receipt
    }
}

/// Durable passive view publication that cannot be supplied to [`act`](
/// MountedExternalController::act).
///
/// A passive presentation may be replayed or superseded after a host wake, but
/// it never reserves an action route or reply decoder. The rendered bytes stay
/// self-contained until the User-delivery contract supplies an acknowledged
/// delta baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPassivePresentation {
    session_id: DurableSessionId,
    epoch_generation: u64,
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    turn_index: u64,
    source_revision: ExternalSourceRevision,
    user_fingerprint: ExternalFingerprint,
    delivery_receipt: ExternalUserDeliveryReceipt,
}

impl ExternalPassivePresentation {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: DurableSessionId,
        epoch_generation: u64,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        turn_index: u64,
        source_revision: ExternalSourceRevision,
        user_fingerprint: ExternalFingerprint,
        delivery_receipt: ExternalUserDeliveryReceipt,
    ) -> Self {
        Self {
            session_id,
            epoch_generation,
            call_id,
            input_id,
            turn_index,
            source_revision,
            user_fingerprint,
            delivery_receipt,
        }
    }

    pub fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub fn epoch_generation(&self) -> u64 {
        self.epoch_generation
    }

    pub fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    pub fn turn_index(&self) -> u64 {
        self.turn_index
    }

    pub fn source_revision(&self) -> &ExternalSourceRevision {
        &self.source_revision
    }

    pub fn user_fingerprint(&self) -> &ExternalFingerprint {
        &self.user_fingerprint
    }

    pub fn delivery_receipt(&self) -> &ExternalUserDeliveryReceipt {
        &self.delivery_receipt
    }
}

/// One rendered external frame with truthful reply semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExternalUserFrame {
    Actionable(ExternalActionTicket),
    Passive(ExternalPassivePresentation),
}

impl ExternalUserFrame {
    pub fn kind(&self) -> ExternalFrameKind {
        match self {
            Self::Actionable(_) => ExternalFrameKind::Actionable,
            Self::Passive(_) => ExternalFrameKind::Passive,
        }
    }

    pub fn delivery_receipt(&self) -> &ExternalUserDeliveryReceipt {
        match self {
            Self::Actionable(ticket) => ticket.delivery_receipt(),
            Self::Passive(presentation) => presentation.delivery_receipt(),
        }
    }

    pub fn action_ticket(&self) -> Option<&ExternalActionTicket> {
        match self {
            Self::Actionable(ticket) => Some(ticket),
            Self::Passive(_) => None,
        }
    }

    pub fn passive_presentation(&self) -> Option<&ExternalPassivePresentation> {
        match self {
            Self::Actionable(_) => None,
            Self::Passive(presentation) => Some(presentation),
        }
    }
}

/// Durable identity of one externally actionable User render.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalActionToken {
    session_id: DurableSessionId,
    epoch_generation: u64,
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    turn_index: u64,
    route: ExternalActionRoute,
    reply_contract_id: ExternalReplyContractId,
    source_revision: ExternalSourceRevision,
    user_fingerprint: ExternalFingerprint,
}

impl ExternalActionToken {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: DurableSessionId,
        epoch_generation: u64,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        turn_index: u64,
        route: ExternalActionRoute,
        reply_contract_id: ExternalReplyContractId,
        source_revision: ExternalSourceRevision,
        user_fingerprint: ExternalFingerprint,
    ) -> Self {
        Self {
            session_id,
            epoch_generation,
            call_id,
            input_id,
            turn_index,
            route,
            reply_contract_id,
            source_revision,
            user_fingerprint,
        }
    }

    pub fn session_id(&self) -> &DurableSessionId {
        &self.session_id
    }

    pub fn epoch_generation(&self) -> u64 {
        self.epoch_generation
    }

    pub fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    pub fn turn_index(&self) -> u64 {
        self.turn_index
    }

    pub fn route(&self) -> &ExternalActionRoute {
        &self.route
    }

    pub fn reply_contract_id(&self) -> &ExternalReplyContractId {
        &self.reply_contract_id
    }

    pub fn source_revision(&self) -> &ExternalSourceRevision {
        &self.source_revision
    }

    pub fn user_fingerprint(&self) -> &ExternalFingerprint {
        &self.user_fingerprint
    }
}

/// Pure POM grammar and typed decoder for replies supplied outside a provider
/// turn.
///
/// The System grammar and decoder are declared by the same component contract,
/// so [`super::ExternalReply`] cannot be constructed with one contract and an
/// unrelated reply POM. The decoder may validate syntax and contract shape,
/// but it may not mutate application state or perform I/O. Domain
/// authorization and stale-state validation happen inside
/// [`MountedExternalPort::compare_exchange`].
pub trait ExternalReplyContract: Send + Sync + 'static {
    type Action: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;
    type Diagnostic: fmt::Display + Send + Sync + 'static;
    type System: AgentView<Root = Document>;

    /// Exact System POM contribution that defines this reply wire grammar.
    ///
    /// It must remain epoch-static. Change [`Self::contract_id`] whenever a
    /// reply accepted by this grammar could map to a different action.
    fn system(&self) -> Self::System;

    /// Stable route included in every durable action token.
    fn route(&self) -> &ExternalActionRoute;

    /// Stable semantic identity of the reply grammar, decoder, and typed
    /// action mapping.
    ///
    /// Change this when a wire reply could decode differently. A controller
    /// will reject durable state created by another id instead of silently
    /// reinterpreting an in-flight reply under new parser semantics.
    fn contract_id(&self) -> &ExternalReplyContractId;

    /// Synchronously decode an external reply into a typed domain command.
    fn decode(&self, reply: &ControlReply) -> Result<Self::Action, Self::Diagnostic>;
}

/// Result of opening one mounted external controller.
///
/// The host-owned port receives System bytes during Create and returns a
/// durable delivery receipt. This type deliberately never exposes raw System
/// text, so a caller cannot accidentally make an untracked second attachment.
pub struct MountedExternalOpen<Props: ?Sized + 'static, Contract, Port>
where
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    controller: MountedExternalController<Props, Contract, Port>,
    system_delivery_receipt: ExternalSystemDeliveryReceipt,
}

impl<Props, Contract, Port> MountedExternalOpen<Props, Contract, Port>
where
    Props: ?Sized + Send + Sync + 'static,
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    /// Controller bound to the admitted durable epoch.
    pub fn controller(&self) -> &MountedExternalController<Props, Contract, Port> {
        &self.controller
    }

    /// Consume this open result and retain the controller.
    pub fn into_controller(self) -> MountedExternalController<Props, Contract, Port> {
        self.controller
    }

    /// Durable receipt for the sole System delivery obligation of this epoch.
    pub fn system_delivery_receipt(&self) -> &ExternalSystemDeliveryReceipt {
        &self.system_delivery_receipt
    }
}

impl<Props, Contract, Port> fmt::Debug for MountedExternalOpen<Props, Contract, Port>
where
    Props: ?Sized + Send + Sync + 'static,
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedExternalOpen")
            .field("controller", &self.controller)
            .field("system_delivery_receipt", &self.system_delivery_receipt)
            .finish()
    }
}

/// Result of capturing one fresh external User POM.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExternalObserveOutcome {
    /// The host atomically published this frame. Only the `Actionable` variant
    /// carries a token accepted by [`MountedExternalController::act`].
    Frame(ExternalUserFrame),
    /// The host detected that the requested source snapshot was already
    /// obsolete before it could reserve a ticket. Capture fresh props and a
    /// fresh revision before trying again.
    SourceStale {
        requested: ExternalSourceRevision,
        actual: ExternalSourceRevision,
    },
}

/// A typed external action that was atomically committed by the host.
///
/// For an immediate `Applied` result, `wake_cursor` is the exact cursor
/// returned by the commit transaction. For `Replayed` or `lookup`, it is the
/// authoritative cursor currently observed by the host, which is guaranteed
/// to be at or after that commit. The opaque AgentView state never guesses a
/// cursor before the host assigns it.
#[derive(Debug, Clone)]
pub struct ExternalActionCommit<Action> {
    identity: ExternalCommitIdentity,
    action: Action,
    wake_cursor: ExternalWakeCursor,
}

impl<Action> ExternalActionCommit<Action> {
    pub fn identity(&self) -> &ExternalCommitIdentity {
        &self.identity
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn into_action(self) -> Action {
        self.action
    }

    pub fn wake_cursor(&self) -> &ExternalWakeCursor {
        &self.wake_cursor
    }
}

/// Result of accepting one external reply.
///
/// Invalid replies are a normal typed result rather than a controller error:
/// no state or domain mutation has occurred in that case. An indeterminate
/// commit must be resolved with [`MountedExternalController::recover`] before
/// the caller tries again.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExternalActOutcome<Action, Diagnostic> {
    Applied(ExternalActionCommit<Action>),
    Replayed(ExternalActionCommit<Action>),
    InvalidReply {
        diagnostic: Diagnostic,
    },
    /// The source snapshot changed before the host accepted the action.
    ///
    /// If [`MountedExternalController::lookup`] reports `Prepared` for this
    /// token, cancel that prepared reply before observing a fresh User
    /// document. A second observation is intentionally rejected while any
    /// action remains active.
    SourceStale {
        token: ExternalActionToken,
        actual: ExternalSourceRevision,
    },
    Indeterminate {
        identity: ExternalCommitIdentity,
    },
}

/// Result of promoting one delivered prompt to the acknowledged delta
/// baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExternalUserDeliveryAckOutcome {
    Acknowledged,
    AlreadyAcknowledged,
}

/// Result of durably fencing the next Actionable User delivery to full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExternalUserResyncOutcome {
    Requested,
    AlreadyRequested,
}

/// Result of waiting for a host wake and capturing a new User document.
#[derive(Debug, Clone)]
pub struct ExternalHookOutcome {
    wake_cursor: ExternalWakeCursor,
    observation: ExternalObserveOutcome,
}

impl ExternalHookOutcome {
    pub fn wake_cursor(&self) -> &ExternalWakeCursor {
        &self.wake_cursor
    }

    pub fn observation(&self) -> &ExternalObserveOutcome {
        &self.observation
    }

    pub fn into_observation(self) -> ExternalObserveOutcome {
        self.observation
    }
}

/// Result of cancelling a ticket before its domain transaction began.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExternalCancelOutcome {
    Cancelled,
    AlreadyCancelled,
    AlreadyCommitted { wake_cursor: ExternalWakeCursor },
}

/// Lease-free durable lifecycle of one external action token.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExternalActionLifecycle {
    AwaitingDelivery {
        ticket: ExternalActionTicket,
    },
    AwaitingReply {
        ticket: ExternalActionTicket,
    },
    Prepared {
        identity: ExternalCommitIdentity,
    },
    RecoveryRequired {
        identity: ExternalCommitIdentity,
    },
    Committed {
        identity: ExternalCommitIdentity,
        wake_cursor: ExternalWakeCursor,
    },
    Cancelled,
    Unknown,
}

/// Result of resolving an indeterminate external domain commit.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExternalRecoveryOutcome<Action> {
    Replayed(ExternalActionCommit<Action>),
    /// The port proved that the domain transaction did not commit. The caller
    /// may explicitly call `act` again with the same reply id and reply.
    Retry,
    /// The port could not determine the outcome and the token is now fenced.
    /// It must not be retried automatically.
    RecoveryRequired,
}

/// Serialization failure for AgentView's opaque external controller state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExternalControllerStateError {
    #[error("could not encode durable external controller state")]
    Encode(#[source] serde_json::Error),
    #[error("could not decode durable external controller state")]
    Decode(#[source] serde_json::Error),
}

/// Failure while opening or driving a mounted external controller.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MountedExternalControllerError<E>
where
    E: Error + Send + Sync + 'static,
{
    #[error("external host operation failed")]
    Port(#[source] E),

    #[error(transparent)]
    SystemMount(#[from] SystemMountError),

    #[error(transparent)]
    Component(#[from] ComponentError),

    #[error(transparent)]
    UserResolution(#[from] PomResolutionError),

    #[error(transparent)]
    UserRender(#[from] PomRenderError),

    #[error(transparent)]
    State(#[from] ExternalControllerStateError),

    #[error("another owner is creating external System epoch generation {generation}")]
    EpochInFlight { generation: u64 },

    #[error("external System epoch generation {generation} requires recovery")]
    EpochRecoveryRequired { generation: u64 },

    #[error("external System epoch contract mismatch: active `{active}`, requested `{requested}`")]
    EpochContractMismatch {
        active: EpochContractId,
        requested: EpochContractId,
    },

    #[error("external epoch artifact belongs to session `{actual}`, not `{expected}`")]
    EpochSessionMismatch {
        expected: DurableSessionId,
        actual: DurableSessionId,
    },

    #[error("external epoch artifact contract `{actual}` does not match `{expected}`")]
    EpochArtifactContractMismatch {
        expected: EpochContractId,
        actual: EpochContractId,
    },

    #[error("external controller state is missing after the epoch was admitted")]
    MissingState,

    #[error("unsupported external controller state schema {actual}; expected {expected}")]
    StateSchemaMismatch { expected: u32, actual: u32 },

    #[error("external controller state belongs to session `{actual}`, not `{expected}`")]
    StateSessionMismatch {
        expected: DurableSessionId,
        actual: DurableSessionId,
    },

    #[error("external controller state contract `{actual}` does not match `{expected}`")]
    StateContractMismatch {
        expected: EpochContractId,
        actual: EpochContractId,
    },

    #[error("external reply contract `{actual}` does not match `{expected}`")]
    StateReplyContractMismatch {
        expected: ExternalReplyContractId,
        actual: ExternalReplyContractId,
    },

    #[error("external controller state epoch generation {actual} does not match {expected}")]
    StateEpochMismatch { expected: u64, actual: u64 },

    #[error("external action token belongs to session `{actual}`, not `{expected}`")]
    TokenSessionMismatch {
        expected: DurableSessionId,
        actual: DurableSessionId,
    },

    #[error("external action token epoch generation {actual} does not match {expected}")]
    TokenEpochMismatch { expected: u64, actual: u64 },

    #[error("external action token route `{actual}` does not match `{expected}`")]
    TokenRouteMismatch {
        expected: ExternalActionRoute,
        actual: ExternalActionRoute,
    },

    #[error("external action token reply contract `{actual}` does not match `{expected}`")]
    TokenReplyContractMismatch {
        expected: ExternalReplyContractId,
        actual: ExternalReplyContractId,
    },

    #[error("external action `{active:?}` is still active")]
    ActionAlreadyActive { active: ExternalActionToken },

    #[error("User delivery `{receipt}` must be acknowledged before its action can run")]
    UserDeliveryNotAcknowledged {
        receipt: ExternalUserDeliveryReceipt,
    },

    #[error("User delivery `{receipt}` is not the current prompt delivery")]
    UnknownUserDelivery {
        receipt: ExternalUserDeliveryReceipt,
    },

    #[error("User delivery acknowledgement was rejected by the host transaction")]
    UserDeliveryAcknowledgementRejected {
        receipt: ExternalUserDeliveryReceipt,
    },

    #[error("User-document full resync was rejected by the host transaction")]
    UserResyncRejected,

    #[error("external reply id `{reply_id}` is already bound to another reply")]
    ReplyIdCollision { reply_id: ExternalReplyId },

    #[error("external action token `{token:?}` is already bound to another reply")]
    ReplyTokenCollision { token: ExternalActionToken },

    #[error("external action token `{token:?}` was cancelled")]
    TokenCancelled { token: ExternalActionToken },

    #[error("external action requires recovery before it can proceed")]
    RecoveryRequired { identity: ExternalCommitIdentity },

    #[error("external controller state conflicted {attempts} consecutive times")]
    StateConflict { attempts: usize },

    #[error(
        "external host reported a successful state write without advancing generation `{previous}` (returned `{actual}`)"
    )]
    /// The port violated its CAS contract. Do not assume the requested
    /// state-only operation happened; inspect the token with `lookup` before
    /// deciding whether it is safe to retry.
    GenerationNotAdvanced {
        previous: MountedStateGeneration,
        actual: MountedStateGeneration,
    },

    #[error("external prepare-reply write returned an indeterminate outcome")]
    /// The port violated the state-only-write rule. Use `lookup` with the
    /// identity token; if it is still awaiting a reply, retry `act` with the
    /// same reply id and bytes. If it is prepared, that same `act` call safely
    /// resumes the commit.
    PrepareIndeterminate { identity: ExternalCommitIdentity },

    #[error("external observation write returned an indeterminate outcome")]
    /// The port violated the state-only-write rule. Use `lookup`: an
    /// `AwaitingReply` result contains the durable ticket; `Unknown` permits a
    /// fresh observation with the same host snapshot.
    ObserveIndeterminate { token: ExternalActionToken },

    #[error("external passive presentation write returned an indeterminate outcome")]
    PresentationIndeterminate {
        call_id: DurableCallId,
        input_id: DurableCallInputId,
    },

    #[error("User delivery acknowledgement returned an indeterminate outcome")]
    UserDeliveryAcknowledgementIndeterminate {
        receipt: ExternalUserDeliveryReceipt,
    },

    #[error("User-document full resync returned an indeterminate outcome")]
    UserResyncIndeterminate,

    #[error("external cancellation write returned an indeterminate outcome")]
    /// The port violated the state-only-write rule. Use `lookup` before
    /// retrying cancellation or creating another observation.
    CancelIndeterminate { token: ExternalActionToken },

    #[error("external cancellation was rejected by the host transaction")]
    CancellationRejected { token: ExternalActionToken },

    #[error("external recovery fence write was rejected")]
    /// Inspect the identity with `lookup` before retrying `recover`; the fence
    /// may already have been recorded by another owner.
    RecoveryFenceRejected { identity: ExternalCommitIdentity },

    #[error("the host resolved a commit as committed but did not retain its replay ledger entry")]
    RecoveryInvariant { identity: ExternalCommitIdentity },

    #[error("external action turn index overflowed")]
    TurnIndexOverflow,

    #[error("external action token `{token:?}` is not known to this controller")]
    UnknownToken { token: ExternalActionToken },

    #[error("System compilation failed and the host could not abandon the Create lease")]
    SystemMountAbandonFailed {
        system: SystemMountError,
        #[source]
        abandon: E,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalControllerState<Action> {
    schema_version: u32,
    session_id: DurableSessionId,
    epoch_contract_id: EpochContractId,
    epoch_generation: u64,
    reply_contract_id: ExternalReplyContractId,
    next_turn_index: u64,
    acknowledged_user: Option<ExternalAcknowledgedUser>,
    force_full_next_prompt: bool,
    current: Option<ExternalCurrentFrame<Action>>,
    terminal: Vec<ExternalTerminalAction<Action>>,
}

impl<Action> ExternalControllerState<Action> {
    fn fresh(
        session_id: DurableSessionId,
        epoch_contract_id: EpochContractId,
        epoch_generation: u64,
        reply_contract_id: ExternalReplyContractId,
    ) -> Self {
        Self {
            schema_version: EXTERNAL_STATE_SCHEMA_VERSION,
            session_id,
            epoch_contract_id,
            epoch_generation,
            reply_contract_id,
            next_turn_index: 1,
            acknowledged_user: None,
            force_full_next_prompt: false,
            current: None,
            terminal: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalAcknowledgedUser {
    receipt: ExternalUserDeliveryReceipt,
    cursor: UserDocumentCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ExternalCurrentFrame<Action> {
    Passive(ExternalPassivePresentation),
    Actionable(ExternalActiveAction<Action>),
}

impl<Action> ExternalCurrentFrame<Action> {
    fn active_action(&self) -> Option<&ExternalActiveAction<Action>> {
        match self {
            Self::Passive(_) => None,
            Self::Actionable(active) => Some(active),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ExternalActiveAction<Action> {
    AwaitingDelivery {
        ticket: ExternalActionTicket,
        next_cursor: UserDocumentCursor,
    },
    AwaitingReply {
        ticket: ExternalActionTicket,
    },
    Prepared {
        ticket: ExternalActionTicket,
        identity: ExternalCommitIdentity,
        action: Action,
    },
    RecoveryRequired {
        ticket: ExternalActionTicket,
        identity: ExternalCommitIdentity,
    },
}

impl<Action> ExternalActiveAction<Action> {
    fn ticket(&self) -> &ExternalActionTicket {
        match self {
            Self::AwaitingDelivery { ticket, .. }
            | Self::AwaitingReply { ticket }
            | Self::Prepared { ticket, .. }
            | Self::RecoveryRequired { ticket, .. } => ticket,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ExternalTerminalAction<Action> {
    Committed {
        identity: ExternalCommitIdentity,
        action: Action,
    },
    Cancelled {
        token: ExternalActionToken,
    },
}

impl<Action> ExternalTerminalAction<Action> {
    fn token(&self) -> &ExternalActionToken {
        match self {
            Self::Committed { identity, .. } => identity.token(),
            Self::Cancelled { token } => token,
        }
    }
}

struct LoadedExternalState<Action> {
    generation: MountedStateGeneration,
    wake_cursor: ExternalWakeCursor,
    state: ExternalControllerState<Action>,
}

/// Durable controller for externally supplied typed replies.
///
/// The controller owns no application-domain capability. It renders a pure
/// mounted User view, records a durable ticket, decodes a reply synchronously,
/// and delegates every mutable action to [`MountedExternalPort`]. The port is
/// the transaction authority for the domain row, AgentView state blob, reply
/// ledger, outbox, and wake cursor.
///
/// One durable session is bound to one logical prompt consumer and its retained
/// System context. Replacing the process owner may reuse the durable receipts;
/// replacing the consumer requires a fresh epoch/session attachment rather
/// than borrowing this cursor or sending System a second time inside the same
/// epoch.
pub struct MountedExternalController<Props: ?Sized + 'static, Contract, Port>
where
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    definition: Arc<ExternalHarnessRuntime<Props>>,
    contract: Arc<Contract>,
    port: Arc<Port>,
    session_id: DurableSessionId,
    epoch: ExternalEpochArtifact,
    system_delivery_receipt: ExternalSystemDeliveryReceipt,
    // This only avoids redundant local CAS races. The port's CAS remains the
    // authority across owners and processes.
    operation_lock: Arc<Mutex<()>>,
}

impl<Props, Contract, Port> Clone for MountedExternalController<Props, Contract, Port>
where
    Props: ?Sized + 'static,
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    fn clone(&self) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            contract: Arc::clone(&self.contract),
            port: Arc::clone(&self.port),
            session_id: self.session_id.clone(),
            epoch: self.epoch.clone(),
            system_delivery_receipt: self.system_delivery_receipt.clone(),
            operation_lock: Arc::clone(&self.operation_lock),
        }
    }
}

impl<Props, Contract, Port> fmt::Debug for MountedExternalController<Props, Contract, Port>
where
    Props: ?Sized + Send + Sync + 'static,
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedExternalController")
            .field("session_id", &self.session_id)
            .field("epoch_contract_id", self.epoch.epoch_contract_id())
            .field("epoch_generation", &self.epoch.generation())
            .field("route", self.contract.route())
            .finish_non_exhaustive()
    }
}

// Recovery errors intentionally retain the complete opaque token/commit
// identity. These paths are control-plane failures, so preserving actionable
// diagnostics is more important than shrinking the uncommon Result payload.
#[allow(clippy::result_large_err)]
impl<Props, Contract, Port> MountedExternalController<Props, Contract, Port>
where
    Props: ?Sized + Send + Sync + 'static,
    Contract: ExternalReplyContract,
    Port: MountedExternalPort<Contract::Action>,
{
    /// Open one durable external System epoch.
    ///
    /// The Create winner renders System exactly once and gives its bytes only
    /// to the host port, which must persist a delivery receipt or ordered
    /// outbox obligation before activation. Reopen never touches the retained
    /// System POM or returns raw System bytes. The reply grammar and decoder
    /// arrive together through [`MountedExternalHarnessDefinition`], so a host
    /// cannot pair this controller with a decoder detached from the authored
    /// System contract. The epoch contract id must also change whenever the
    /// User POM or diff-slot semantics change, because acknowledged cursors are
    /// retained under that same epoch.
    pub async fn open(
        harness: MountedExternalHarnessDefinition<Props, Contract>,
        port: Arc<Port>,
        session_id: DurableSessionId,
    ) -> Result<
        MountedExternalOpen<Props, Contract, Port>,
        MountedExternalControllerError<Port::Error>,
    > {
        let (definition, contract) = harness.into_parts();
        let contract = Arc::new(contract);
        let epoch_contract_id = definition.epoch().epoch_contract_id().clone();
        let admission = port
            .acquire_epoch(ExternalEpochAcquireRequest::new(
                &session_id,
                &epoch_contract_id,
            ))
            .await
            .map_err(MountedExternalControllerError::Port)?;

        match admission {
            ExternalEpochAdmission::Create { lease } => {
                Self::validate_lease(&session_id, &epoch_contract_id, &lease)?;
                let mounted = match mount_system_component_with_contract(
                    epoch_contract_id.clone(),
                    definition.epoch().durable_system().take_create_component(),
                ) {
                    Ok(mounted) => mounted,
                    Err(system) => match port.abandon_epoch(&lease).await {
                        Ok(()) => return Err(MountedExternalControllerError::SystemMount(system)),
                        Err(abandon) => {
                            return Err(MountedExternalControllerError::SystemMountAbandonFailed {
                                system,
                                abandon,
                            });
                        }
                    },
                };
                let artifact =
                    ExternalEpochArtifact::from_rendered_system(&lease, mounted.rendered_system());
                let state = ExternalControllerState::<Contract::Action>::fresh(
                    session_id.clone(),
                    epoch_contract_id.clone(),
                    lease.generation(),
                    contract.contract_id().clone(),
                );
                let initial_state = Self::encode_state(&state)?;
                let activation = port
                    .complete_epoch(ExternalEpochComplete::new(
                        &lease,
                        &artifact,
                        mounted.rendered_system(),
                        &initial_state,
                    ))
                    .await
                    .map_err(MountedExternalControllerError::Port)?;
                let system_delivery_receipt = activation.system_delivery_receipt().clone();
                let controller = Self::new(
                    definition,
                    contract,
                    port,
                    session_id,
                    artifact,
                    system_delivery_receipt.clone(),
                );
                controller.decode_loaded_state(activation.state())?;
                Ok(MountedExternalOpen {
                    controller,
                    system_delivery_receipt,
                })
            }
            ExternalEpochAdmission::Existing {
                artifact,
                system_delivery_receipt,
                state,
            } => {
                Self::validate_artifact(&session_id, &epoch_contract_id, &artifact)?;
                let controller = Self::new(
                    definition,
                    contract,
                    port,
                    session_id,
                    artifact,
                    system_delivery_receipt.clone(),
                );
                controller.decode_loaded_state(&state)?;
                Ok(MountedExternalOpen {
                    controller,
                    system_delivery_receipt,
                })
            }
            ExternalEpochAdmission::InFlight { generation } => {
                Err(MountedExternalControllerError::EpochInFlight { generation })
            }
            ExternalEpochAdmission::RecoveryRequired { generation } => {
                Err(MountedExternalControllerError::EpochRecoveryRequired { generation })
            }
            ExternalEpochAdmission::ContractMismatch { active, requested } => {
                Err(MountedExternalControllerError::EpochContractMismatch { active, requested })
            }
        }
    }

    fn new(
        definition: Arc<ExternalHarnessRuntime<Props>>,
        contract: Arc<Contract>,
        port: Arc<Port>,
        session_id: DurableSessionId,
        epoch: ExternalEpochArtifact,
        system_delivery_receipt: ExternalSystemDeliveryReceipt,
    ) -> Self {
        Self {
            definition,
            contract,
            port,
            session_id,
            epoch,
            system_delivery_receipt,
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    fn validate_lease(
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
        lease: &ExternalEpochLease,
    ) -> Result<(), MountedExternalControllerError<Port::Error>> {
        if lease.session_id() != session_id {
            return Err(MountedExternalControllerError::EpochSessionMismatch {
                expected: session_id.clone(),
                actual: lease.session_id().clone(),
            });
        }
        if lease.epoch_contract_id() != epoch_contract_id {
            return Err(
                MountedExternalControllerError::EpochArtifactContractMismatch {
                    expected: epoch_contract_id.clone(),
                    actual: lease.epoch_contract_id().clone(),
                },
            );
        }
        Ok(())
    }

    fn validate_artifact(
        session_id: &DurableSessionId,
        epoch_contract_id: &EpochContractId,
        artifact: &ExternalEpochArtifact,
    ) -> Result<(), MountedExternalControllerError<Port::Error>> {
        if artifact.session_id() != session_id {
            return Err(MountedExternalControllerError::EpochSessionMismatch {
                expected: session_id.clone(),
                actual: artifact.session_id().clone(),
            });
        }
        if artifact.epoch_contract_id() != epoch_contract_id {
            return Err(
                MountedExternalControllerError::EpochArtifactContractMismatch {
                    expected: epoch_contract_id.clone(),
                    actual: artifact.epoch_contract_id().clone(),
                },
            );
        }
        Ok(())
    }

    fn encode_state(
        state: &ExternalControllerState<Contract::Action>,
    ) -> Result<MountedStateBlob, MountedExternalControllerError<Port::Error>> {
        serde_json::to_vec(state)
            .map(MountedStateBlob::new)
            .map_err(|source| {
                MountedExternalControllerError::State(ExternalControllerStateError::Encode(source))
            })
    }

    fn decode_loaded_state(
        &self,
        snapshot: &ExternalStateSnapshot,
    ) -> Result<LoadedExternalState<Contract::Action>, MountedExternalControllerError<Port::Error>>
    {
        let generation = snapshot
            .generation()
            .cloned()
            .ok_or(MountedExternalControllerError::MissingState)?;
        let state = snapshot
            .state()
            .ok_or(MountedExternalControllerError::MissingState)?;
        let state =
            serde_json::from_slice::<ExternalControllerState<Contract::Action>>(state.as_bytes())
                .map_err(|source| {
                MountedExternalControllerError::State(ExternalControllerStateError::Decode(source))
            })?;
        self.validate_state(&state)?;
        Ok(LoadedExternalState {
            generation,
            wake_cursor: snapshot.wake_cursor().clone(),
            state,
        })
    }

    fn validate_state(
        &self,
        state: &ExternalControllerState<Contract::Action>,
    ) -> Result<(), MountedExternalControllerError<Port::Error>> {
        if state.schema_version != EXTERNAL_STATE_SCHEMA_VERSION {
            return Err(MountedExternalControllerError::StateSchemaMismatch {
                expected: EXTERNAL_STATE_SCHEMA_VERSION,
                actual: state.schema_version,
            });
        }
        if state.session_id != self.session_id {
            return Err(MountedExternalControllerError::StateSessionMismatch {
                expected: self.session_id.clone(),
                actual: state.session_id.clone(),
            });
        }
        if state.epoch_contract_id != *self.epoch.epoch_contract_id() {
            return Err(MountedExternalControllerError::StateContractMismatch {
                expected: self.epoch.epoch_contract_id().clone(),
                actual: state.epoch_contract_id.clone(),
            });
        }
        if state.epoch_generation != self.epoch.generation() {
            return Err(MountedExternalControllerError::StateEpochMismatch {
                expected: self.epoch.generation(),
                actual: state.epoch_generation,
            });
        }
        if state.reply_contract_id != *self.contract.contract_id() {
            return Err(MountedExternalControllerError::StateReplyContractMismatch {
                expected: self.contract.contract_id().clone(),
                actual: state.reply_contract_id.clone(),
            });
        }
        if let Some(current) = &state.current {
            match current {
                ExternalCurrentFrame::Actionable(active) => {
                    self.validate_token(active.ticket().token())?;
                }
                ExternalCurrentFrame::Passive(presentation) => {
                    self.validate_presentation(presentation)?;
                }
            }
        }
        for terminal in &state.terminal {
            self.validate_token(terminal.token())?;
        }
        Ok(())
    }

    async fn load_state(
        &self,
    ) -> Result<LoadedExternalState<Contract::Action>, MountedExternalControllerError<Port::Error>>
    {
        let snapshot = self
            .port
            .load(&self.session_id)
            .await
            .map_err(MountedExternalControllerError::Port)?;
        self.decode_loaded_state(&snapshot)
    }

    fn validate_token(
        &self,
        token: &ExternalActionToken,
    ) -> Result<(), MountedExternalControllerError<Port::Error>> {
        if token.session_id() != &self.session_id {
            return Err(MountedExternalControllerError::TokenSessionMismatch {
                expected: self.session_id.clone(),
                actual: token.session_id().clone(),
            });
        }
        if token.epoch_generation() != self.epoch.generation() {
            return Err(MountedExternalControllerError::TokenEpochMismatch {
                expected: self.epoch.generation(),
                actual: token.epoch_generation(),
            });
        }
        if token.route() != self.contract.route() {
            return Err(MountedExternalControllerError::TokenRouteMismatch {
                expected: self.contract.route().clone(),
                actual: token.route().clone(),
            });
        }
        if token.reply_contract_id() != self.contract.contract_id() {
            return Err(MountedExternalControllerError::TokenReplyContractMismatch {
                expected: self.contract.contract_id().clone(),
                actual: token.reply_contract_id().clone(),
            });
        }
        Ok(())
    }

    fn validate_presentation(
        &self,
        presentation: &ExternalPassivePresentation,
    ) -> Result<(), MountedExternalControllerError<Port::Error>> {
        if presentation.session_id() != &self.session_id {
            return Err(MountedExternalControllerError::StateSessionMismatch {
                expected: self.session_id.clone(),
                actual: presentation.session_id().clone(),
            });
        }
        if presentation.epoch_generation() != self.epoch.generation() {
            return Err(MountedExternalControllerError::StateEpochMismatch {
                expected: self.epoch.generation(),
                actual: presentation.epoch_generation(),
            });
        }
        Ok(())
    }

    /// A successful CAS must make the next write fence distinct. Without this
    /// check a faulty port could report success while leaving the old state in
    /// place, causing the controller to expose a phantom ticket or action.
    fn ensure_generation_advanced(
        previous: &MountedStateGeneration,
        actual: &MountedStateGeneration,
    ) -> Result<(), MountedExternalControllerError<Port::Error>> {
        if previous == actual {
            return Err(MountedExternalControllerError::GenerationNotAdvanced {
                previous: previous.clone(),
                actual: actual.clone(),
            });
        }
        Ok(())
    }

    fn ticket_matches_observation(
        &self,
        ticket: &ExternalActionTicket,
        call_id: &DurableCallId,
        input_id: &DurableCallInputId,
        source_revision: &ExternalSourceRevision,
    ) -> bool {
        let token = ticket.token();
        token.call_id() == call_id
            && token.input_id() == input_id
            && token.source_revision() == source_revision
            && token.route() == self.contract.route()
            && token.reply_contract_id() == self.contract.contract_id()
    }

    fn presentation_matches_observation(
        &self,
        presentation: &ExternalPassivePresentation,
        call_id: &DurableCallId,
        input_id: &DurableCallInputId,
        source_revision: &ExternalSourceRevision,
    ) -> bool {
        presentation.call_id() == call_id
            && presentation.input_id() == input_id
            && presentation.source_revision() == source_revision
    }

    fn replay_for(
        &self,
        state: &ExternalControllerState<Contract::Action>,
        wake_cursor: &ExternalWakeCursor,
        identity: &ExternalCommitIdentity,
    ) -> Result<
        Option<ExternalActionCommit<Contract::Action>>,
        MountedExternalControllerError<Port::Error>,
    > {
        for terminal in &state.terminal {
            match terminal {
                ExternalTerminalAction::Committed {
                    identity: stored,
                    action,
                } => {
                    if stored.reply_id() == identity.reply_id() {
                        if stored == identity {
                            return Ok(Some(ExternalActionCommit {
                                identity: stored.clone(),
                                action: action.clone(),
                                wake_cursor: wake_cursor.clone(),
                            }));
                        }
                        return Err(MountedExternalControllerError::ReplyIdCollision {
                            reply_id: identity.reply_id().clone(),
                        });
                    }
                    if stored.token() == identity.token() {
                        return Err(MountedExternalControllerError::ReplyTokenCollision {
                            token: identity.token().clone(),
                        });
                    }
                }
                ExternalTerminalAction::Cancelled { token } if token == identity.token() => {
                    return Err(MountedExternalControllerError::TokenCancelled {
                        token: token.clone(),
                    });
                }
                ExternalTerminalAction::Cancelled { .. } => {}
            }
        }
        Ok(None)
    }

    fn lifecycle_from_state(
        &self,
        state: &ExternalControllerState<Contract::Action>,
        wake_cursor: &ExternalWakeCursor,
        token: &ExternalActionToken,
    ) -> ExternalActionLifecycle {
        if let Some(active) = state
            .current
            .as_ref()
            .and_then(ExternalCurrentFrame::active_action)
        {
            match active {
                ExternalActiveAction::AwaitingDelivery { ticket, .. }
                    if ticket.token() == token =>
                {
                    return ExternalActionLifecycle::AwaitingDelivery {
                        ticket: ticket.clone(),
                    };
                }
                ExternalActiveAction::AwaitingReply { ticket } if ticket.token() == token => {
                    return ExternalActionLifecycle::AwaitingReply {
                        ticket: ticket.clone(),
                    };
                }
                ExternalActiveAction::Prepared { identity, .. } if identity.token() == token => {
                    return ExternalActionLifecycle::Prepared {
                        identity: identity.clone(),
                    };
                }
                ExternalActiveAction::RecoveryRequired { identity, .. }
                    if identity.token() == token =>
                {
                    return ExternalActionLifecycle::RecoveryRequired {
                        identity: identity.clone(),
                    };
                }
                _ => {}
            }
        }
        for terminal in &state.terminal {
            match terminal {
                ExternalTerminalAction::Committed { identity, .. } if identity.token() == token => {
                    return ExternalActionLifecycle::Committed {
                        identity: identity.clone(),
                        wake_cursor: wake_cursor.clone(),
                    };
                }
                ExternalTerminalAction::Cancelled { token: cancelled } if cancelled == token => {
                    return ExternalActionLifecycle::Cancelled;
                }
                _ => {}
            }
        }
        ExternalActionLifecycle::Unknown
    }

    /// Render and atomically publish one fresh external frame.
    ///
    /// `observation` owns the exact immutable snapshot selected by the host for
    /// this turn together with its source revision. If any prompt-relevant
    /// domain input changes, the host must capture a new observation; retrying
    /// the same revision and frame kind returns the already reserved frame
    /// rather than silently replacing its rendered text. An actionable frame
    /// blocks replacement until it is committed or cancelled. A passive
    /// presentation may be superseded by a later authoritative observation.
    pub async fn observe(
        &self,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        observation: ExternalObservation<Props>,
    ) -> Result<ExternalObserveOutcome, MountedExternalControllerError<Port::Error>> {
        let (source_revision, props) = observation.into_parts();
        let _local_operation = self.operation_lock.lock().await;
        let mut authored_user = None;
        for _attempt in 1..=MAX_EXTERNAL_STATE_RETRIES {
            let loaded = self.load_state().await?;

            // Reopen/retry discovers the already-published immutable outbox
            // identity before executing the pure renderer again.
            let supersedes = match &loaded.state.current {
                Some(ExternalCurrentFrame::Actionable(active)) => match active {
                    ExternalActiveAction::AwaitingDelivery { ticket, .. }
                    | ExternalActiveAction::AwaitingReply { ticket }
                        if self.ticket_matches_observation(
                            ticket,
                            &call_id,
                            &input_id,
                            &source_revision,
                        ) =>
                    {
                        return Ok(ExternalObserveOutcome::Frame(
                            ExternalUserFrame::Actionable(ticket.clone()),
                        ));
                    }
                    ExternalActiveAction::RecoveryRequired { identity, .. } => {
                        return Err(MountedExternalControllerError::RecoveryRequired {
                            identity: identity.clone(),
                        });
                    }
                    _ => {
                        return Err(MountedExternalControllerError::ActionAlreadyActive {
                            active: active.ticket().token().clone(),
                        });
                    }
                },
                Some(ExternalCurrentFrame::Passive(presentation))
                    if self.presentation_matches_observation(
                        presentation,
                        &call_id,
                        &input_id,
                        &source_revision,
                    ) =>
                {
                    return Ok(ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(
                        presentation.clone(),
                    )));
                }
                Some(ExternalCurrentFrame::Passive(presentation)) => Some(presentation.clone()),
                None => None,
            };

            let (kind, user) = match authored_user.as_ref() {
                Some(authored) => authored,
                None => {
                    let external_user = self
                        .definition
                        .render_user(UserTurnContext::new(props.as_ref()))?;
                    let (kind, user) = external_user.into_parts();
                    let user = compile_user_view(user)?.into_document();
                    authored_user.insert((kind, user))
                }
            };

            let may_use_acknowledged_baseline =
                *kind == ExternalFrameKind::Actionable && !loaded.state.force_full_next_prompt;
            let previous_cursor = if may_use_acknowledged_baseline {
                loaded
                    .state
                    .acknowledged_user
                    .as_ref()
                    .map(|acknowledged| acknowledged.cursor.clone())
                    .unwrap_or_default()
            } else {
                // Presentation is an independent observer lane. It is always
                // self-contained and cannot consume the prompt baseline.
                UserDocumentCursor::default()
            };
            let base_receipt = if may_use_acknowledged_baseline {
                loaded
                    .state
                    .acknowledged_user
                    .as_ref()
                    .map(|acknowledged| acknowledged.receipt.clone())
            } else {
                None
            };
            let (resolved_user, next_cursor) =
                resolve_user_document(user.clone(), &previous_cursor)?;
            let rendered_user = render_pom_document(&resolved_user)?;
            let user_fingerprint = fingerprint_rendered_user(&rendered_user);

            let next_turn_index = loaded
                .state
                .next_turn_index
                .checked_add(1)
                .ok_or(MountedExternalControllerError::TurnIndexOverflow)?;
            let lane = match kind {
                ExternalFrameKind::Actionable => ExternalUserDeliveryLane::Prompt,
                ExternalFrameKind::Passive => ExternalUserDeliveryLane::Presentation,
            };
            let delivery_receipt = user_delivery_receipt(
                &self.session_id,
                self.epoch.generation(),
                loaded.state.next_turn_index,
                lane,
                &self.system_delivery_receipt,
                base_receipt.as_ref(),
                &user_fingerprint,
            );
            let delivery = ExternalUserDeliveryCandidate::new(
                delivery_receipt.clone(),
                lane,
                self.system_delivery_receipt.clone(),
                base_receipt,
                user_fingerprint.clone(),
                rendered_user,
            );
            let frame = match kind {
                ExternalFrameKind::Actionable => {
                    let token = ExternalActionToken::new(
                        self.session_id.clone(),
                        self.epoch.generation(),
                        call_id.clone(),
                        input_id.clone(),
                        loaded.state.next_turn_index,
                        self.contract.route().clone(),
                        self.contract.contract_id().clone(),
                        source_revision.clone(),
                        user_fingerprint.clone(),
                    );
                    ExternalUserFrame::Actionable(ExternalActionTicket::new(
                        token,
                        delivery_receipt,
                    ))
                }
                ExternalFrameKind::Passive => {
                    ExternalUserFrame::Passive(ExternalPassivePresentation::new(
                        self.session_id.clone(),
                        self.epoch.generation(),
                        call_id.clone(),
                        input_id.clone(),
                        loaded.state.next_turn_index,
                        source_revision.clone(),
                        user_fingerprint.clone(),
                        delivery_receipt,
                    ))
                }
            };
            let mut next_state = loaded.state;
            next_state.next_turn_index = next_turn_index;
            next_state.current = Some(match &frame {
                ExternalUserFrame::Actionable(ticket) => {
                    ExternalCurrentFrame::Actionable(ExternalActiveAction::AwaitingDelivery {
                        ticket: ticket.clone(),
                        next_cursor,
                    })
                }
                ExternalUserFrame::Passive(presentation) => {
                    ExternalCurrentFrame::Passive(presentation.clone())
                }
            });
            let state = Self::encode_state(&next_state)?;
            let mutation = match &frame {
                ExternalUserFrame::Actionable(ticket) => ExternalStateMutation::PublishActionable {
                    delivery: &delivery,
                    ticket,
                    supersedes: supersedes.as_ref(),
                },
                ExternalUserFrame::Passive(presentation) => ExternalStateMutation::PublishPassive {
                    delivery: &delivery,
                    presentation,
                    supersedes: supersedes.as_ref(),
                },
            };
            let outcome = self
                .port
                .compare_exchange(ExternalStateWrite::new(
                    &self.session_id,
                    Some(&loaded.generation),
                    &state,
                    mutation,
                ))
                .await
                .map_err(MountedExternalControllerError::Port)?;
            match outcome {
                ExternalStateWriteOutcome::Committed { generation, .. } => {
                    Self::ensure_generation_advanced(&loaded.generation, &generation)?;
                    return Ok(ExternalObserveOutcome::Frame(frame));
                }
                ExternalStateWriteOutcome::Conflict { .. } => continue,
                ExternalStateWriteOutcome::SourceStale { actual, .. } => {
                    return Ok(ExternalObserveOutcome::SourceStale {
                        requested: source_revision,
                        actual,
                    });
                }
                ExternalStateWriteOutcome::Indeterminate => {
                    return Err(match frame {
                        ExternalUserFrame::Actionable(ticket) => {
                            MountedExternalControllerError::ObserveIndeterminate {
                                token: ticket.token,
                            }
                        }
                        ExternalUserFrame::Passive(_) => {
                            MountedExternalControllerError::PresentationIndeterminate {
                                call_id,
                                input_id,
                            }
                        }
                    });
                }
            }
        }
        Err(MountedExternalControllerError::StateConflict {
            attempts: MAX_EXTERNAL_STATE_RETRIES,
        })
    }

    /// Atomically record that the prompt consumer accepted one exact User
    /// delivery and promote its candidate cursor to the durable delta base.
    ///
    /// This is a host callback, not a rendering shortcut. The port must verify
    /// the transport acknowledgement against its immutable outbox row in the
    /// same transaction. Merely publishing or reading the candidate is not an
    /// acknowledgement.
    pub async fn acknowledge_user_delivery(
        &self,
        receipt: &ExternalUserDeliveryReceipt,
    ) -> Result<ExternalUserDeliveryAckOutcome, MountedExternalControllerError<Port::Error>> {
        let _local_operation = self.operation_lock.lock().await;
        for _attempt in 1..=MAX_EXTERNAL_STATE_RETRIES {
            let loaded = self.load_state().await?;
            if loaded
                .state
                .acknowledged_user
                .as_ref()
                .is_some_and(|acknowledged| &acknowledged.receipt == receipt)
            {
                return Ok(ExternalUserDeliveryAckOutcome::AlreadyAcknowledged);
            }

            let (ticket, next_cursor) = match loaded
                .state
                .current
                .as_ref()
                .and_then(ExternalCurrentFrame::active_action)
            {
                Some(ExternalActiveAction::AwaitingDelivery {
                    ticket,
                    next_cursor,
                }) if ticket.delivery_receipt() == receipt => (ticket.clone(), next_cursor.clone()),
                _ => {
                    return Err(MountedExternalControllerError::UnknownUserDelivery {
                        receipt: receipt.clone(),
                    });
                }
            };

            let mut acknowledged = loaded.state;
            acknowledged.acknowledged_user = Some(ExternalAcknowledgedUser {
                receipt: receipt.clone(),
                cursor: next_cursor,
            });
            acknowledged.force_full_next_prompt = false;
            acknowledged.current = Some(ExternalCurrentFrame::Actionable(
                ExternalActiveAction::AwaitingReply { ticket },
            ));
            let state = Self::encode_state(&acknowledged)?;
            let outcome = self
                .port
                .compare_exchange(ExternalStateWrite::new(
                    &self.session_id,
                    Some(&loaded.generation),
                    &state,
                    ExternalStateMutation::AcknowledgeUserDelivery { receipt },
                ))
                .await
                .map_err(MountedExternalControllerError::Port)?;
            match outcome {
                ExternalStateWriteOutcome::Committed { generation, .. } => {
                    Self::ensure_generation_advanced(&loaded.generation, &generation)?;
                    return Ok(ExternalUserDeliveryAckOutcome::Acknowledged);
                }
                ExternalStateWriteOutcome::Conflict { .. } => continue,
                ExternalStateWriteOutcome::SourceStale { .. } => {
                    return Err(
                        MountedExternalControllerError::UserDeliveryAcknowledgementRejected {
                            receipt: receipt.clone(),
                        },
                    );
                }
                ExternalStateWriteOutcome::Indeterminate => {
                    return Err(
                        MountedExternalControllerError::UserDeliveryAcknowledgementIndeterminate {
                            receipt: receipt.clone(),
                        },
                    );
                }
            }
        }
        Err(MountedExternalControllerError::StateConflict {
            attempts: MAX_EXTERNAL_STATE_RETRIES,
        })
    }

    /// Force the next Actionable User document to be self-contained without
    /// reinstalling or redelivering System.
    ///
    /// Use this only when the same logical prompt consumer has lost its
    /// acknowledged User baseline. If an unacknowledged Actionable delta is
    /// current, this operation durably cancels/tombstones that exact delivery
    /// before admitting its full replacement. An acknowledged or prepared
    /// action must settle first. Passive presentations may remain current
    /// because they never participate in the prompt cursor.
    pub async fn request_user_document_resync(
        &self,
    ) -> Result<ExternalUserResyncOutcome, MountedExternalControllerError<Port::Error>> {
        let _local_operation = self.operation_lock.lock().await;
        for _attempt in 1..=MAX_EXTERNAL_STATE_RETRIES {
            let loaded = self.load_state().await?;
            if loaded.state.force_full_next_prompt {
                return Ok(ExternalUserResyncOutcome::AlreadyRequested);
            }
            let active = loaded
                .state
                .current
                .as_ref()
                .and_then(ExternalCurrentFrame::active_action)
                .cloned();
            if let Some(active) = active {
                let ExternalActiveAction::AwaitingDelivery { ticket, .. } = active else {
                    return Err(MountedExternalControllerError::ActionAlreadyActive {
                        active: active.ticket().token().clone(),
                    });
                };
                let token = ticket.token().clone();
                let delivery_receipt = ticket.delivery_receipt().clone();
                let mut fenced = loaded.state;
                fenced.current = None;
                fenced.force_full_next_prompt = true;
                fenced.terminal.push(ExternalTerminalAction::Cancelled {
                    token: token.clone(),
                });
                let state = Self::encode_state(&fenced)?;
                let outcome = self
                    .port
                    .compare_exchange(ExternalStateWrite::new(
                        &self.session_id,
                        Some(&loaded.generation),
                        &state,
                        ExternalStateMutation::Cancel {
                            token: &token,
                            delivery_receipt: &delivery_receipt,
                        },
                    ))
                    .await
                    .map_err(MountedExternalControllerError::Port)?;
                match outcome {
                    ExternalStateWriteOutcome::Committed { generation, .. } => {
                        Self::ensure_generation_advanced(&loaded.generation, &generation)?;
                        return Ok(ExternalUserResyncOutcome::Requested);
                    }
                    ExternalStateWriteOutcome::Conflict { .. } => continue,
                    ExternalStateWriteOutcome::SourceStale { .. } => {
                        return Err(MountedExternalControllerError::UserResyncRejected);
                    }
                    ExternalStateWriteOutcome::Indeterminate => {
                        return Err(MountedExternalControllerError::UserResyncIndeterminate);
                    }
                }
            }

            let mut fenced = loaded.state;
            fenced.force_full_next_prompt = true;
            let state = Self::encode_state(&fenced)?;
            let outcome = self
                .port
                .compare_exchange(ExternalStateWrite::new(
                    &self.session_id,
                    Some(&loaded.generation),
                    &state,
                    ExternalStateMutation::RequestUserDocumentResync,
                ))
                .await
                .map_err(MountedExternalControllerError::Port)?;
            match outcome {
                ExternalStateWriteOutcome::Committed { generation, .. } => {
                    Self::ensure_generation_advanced(&loaded.generation, &generation)?;
                    return Ok(ExternalUserResyncOutcome::Requested);
                }
                ExternalStateWriteOutcome::Conflict { .. } => continue,
                ExternalStateWriteOutcome::SourceStale { .. } => {
                    return Err(MountedExternalControllerError::UserResyncRejected);
                }
                ExternalStateWriteOutcome::Indeterminate => {
                    return Err(MountedExternalControllerError::UserResyncIndeterminate);
                }
            }
        }
        Err(MountedExternalControllerError::StateConflict {
            attempts: MAX_EXTERNAL_STATE_RETRIES,
        })
    }

    /// Decode a reply and atomically commit its typed action through the host.
    ///
    /// Decoding is synchronous and side-effect free. A successful `Commit`
    /// is the sole point where a host may mutate its domain, state blob,
    /// idempotency ledger, outbox, and wake cursor.
    pub async fn act(
        &self,
        token: &ExternalActionToken,
        reply_id: ExternalReplyId,
        reply: ControlReply,
    ) -> Result<
        ExternalActOutcome<Contract::Action, Contract::Diagnostic>,
        MountedExternalControllerError<Port::Error>,
    > {
        self.validate_token(token)?;
        let reply_fingerprint = fingerprint_control_reply(&reply).map_err(|source| {
            MountedExternalControllerError::State(ExternalControllerStateError::Encode(source))
        })?;
        let identity = ExternalCommitIdentity::new(token.clone(), reply_id, reply_fingerprint);
        let _local_operation = self.operation_lock.lock().await;

        for _attempt in 1..=MAX_EXTERNAL_STATE_RETRIES {
            let loaded = self.load_state().await?;
            if let Some(replay) = self.replay_for(&loaded.state, &loaded.wake_cursor, &identity)? {
                return Ok(ExternalActOutcome::Replayed(replay));
            }

            let active = loaded
                .state
                .current
                .as_ref()
                .and_then(ExternalCurrentFrame::active_action)
                .cloned();
            let action = match active.as_ref() {
                Some(ExternalActiveAction::AwaitingDelivery { ticket, .. }) => {
                    if ticket.token() != token {
                        return Err(MountedExternalControllerError::ActionAlreadyActive {
                            active: ticket.token().clone(),
                        });
                    }
                    return Err(
                        MountedExternalControllerError::UserDeliveryNotAcknowledged {
                            receipt: ticket.delivery_receipt().clone(),
                        },
                    );
                }
                Some(ExternalActiveAction::AwaitingReply { ticket }) => {
                    if ticket.token() != token {
                        return Err(MountedExternalControllerError::ActionAlreadyActive {
                            active: ticket.token().clone(),
                        });
                    }
                    match self.contract.decode(&reply) {
                        Ok(action) => {
                            let mut prepared = loaded.state;
                            prepared.current = Some(ExternalCurrentFrame::Actionable(
                                ExternalActiveAction::Prepared {
                                    ticket: ticket.clone(),
                                    identity: identity.clone(),
                                    action: action.clone(),
                                },
                            ));
                            let state = Self::encode_state(&prepared)?;
                            let outcome = self
                                .port
                                .compare_exchange(ExternalStateWrite::new(
                                    &self.session_id,
                                    Some(&loaded.generation),
                                    &state,
                                    ExternalStateMutation::PrepareReply {
                                        identity: &identity,
                                        action: &action,
                                    },
                                ))
                                .await
                                .map_err(MountedExternalControllerError::Port)?;
                            match outcome {
                                ExternalStateWriteOutcome::Committed { generation, .. } => {
                                    Self::ensure_generation_advanced(
                                        &loaded.generation,
                                        &generation,
                                    )?;
                                    continue;
                                }
                                ExternalStateWriteOutcome::Conflict { .. } => continue,
                                ExternalStateWriteOutcome::SourceStale { actual, .. } => {
                                    return Ok(ExternalActOutcome::SourceStale {
                                        token: token.clone(),
                                        actual,
                                    });
                                }
                                ExternalStateWriteOutcome::Indeterminate => {
                                    return Err(
                                        MountedExternalControllerError::PrepareIndeterminate {
                                            identity,
                                        },
                                    );
                                }
                            }
                        }
                        Err(diagnostic) => {
                            return Ok(ExternalActOutcome::InvalidReply { diagnostic });
                        }
                    }
                }
                Some(ExternalActiveAction::Prepared {
                    ticket,
                    identity: stored_identity,
                    action,
                }) => {
                    if ticket.token() != token {
                        return Err(MountedExternalControllerError::ActionAlreadyActive {
                            active: ticket.token().clone(),
                        });
                    }
                    if stored_identity != &identity {
                        return Err(MountedExternalControllerError::ReplyTokenCollision {
                            token: token.clone(),
                        });
                    }
                    action.clone()
                }
                Some(ExternalActiveAction::RecoveryRequired {
                    ticket,
                    identity: stored_identity,
                }) => {
                    if ticket.token() != token {
                        return Err(MountedExternalControllerError::ActionAlreadyActive {
                            active: ticket.token().clone(),
                        });
                    }
                    return Err(MountedExternalControllerError::RecoveryRequired {
                        identity: stored_identity.clone(),
                    });
                }
                None => {
                    return Err(MountedExternalControllerError::UnknownToken {
                        token: token.clone(),
                    });
                }
            };

            let mut committed = loaded.state;
            committed.current = None;
            committed.terminal.push(ExternalTerminalAction::Committed {
                identity: identity.clone(),
                action: action.clone(),
            });
            let state = Self::encode_state(&committed)?;
            let outcome = self
                .port
                .compare_exchange(ExternalStateWrite::new(
                    &self.session_id,
                    Some(&loaded.generation),
                    &state,
                    ExternalStateMutation::Commit {
                        identity: &identity,
                        action: &action,
                    },
                ))
                .await
                .map_err(MountedExternalControllerError::Port)?;
            match outcome {
                ExternalStateWriteOutcome::Committed {
                    generation,
                    wake_cursor,
                } => {
                    if Self::ensure_generation_advanced(&loaded.generation, &generation).is_err() {
                        // The domain transaction may have committed even when
                        // the port returned an invalid write fence. Preserve
                        // the identity so the caller must reconcile it.
                        return Ok(ExternalActOutcome::Indeterminate { identity });
                    }
                    return Ok(ExternalActOutcome::Applied(ExternalActionCommit {
                        identity,
                        action,
                        wake_cursor,
                    }));
                }
                ExternalStateWriteOutcome::Conflict { .. } => continue,
                ExternalStateWriteOutcome::SourceStale { actual, .. } => {
                    return Ok(ExternalActOutcome::SourceStale {
                        token: token.clone(),
                        actual,
                    });
                }
                ExternalStateWriteOutcome::Indeterminate => {
                    return Ok(ExternalActOutcome::Indeterminate { identity });
                }
            }
        }
        Err(MountedExternalControllerError::StateConflict {
            attempts: MAX_EXTERNAL_STATE_RETRIES,
        })
    }

    /// Wait until the host publishes a durable cursor newer than `after`.
    ///
    /// Hosts with an asynchronous capture path can await this first, capture
    /// their current domain snapshot, then call [`Self::observe`].
    pub async fn wait_for_wake(
        &self,
        after: &ExternalWakeCursor,
    ) -> Result<ExternalWakeCursor, MountedExternalControllerError<Port::Error>> {
        self.port
            .wait_for_wake(&self.session_id, after)
            .await
            .map_err(MountedExternalControllerError::Port)
    }

    /// Wait for a host wake, then synchronously capture a fresh User ticket.
    ///
    /// `capture` runs only after the returned wake has been observed. It must
    /// return one [`ExternalObservation`] from the exact snapshot used for this
    /// User render. Hosts whose capture needs I/O should use
    /// [`Self::wait_for_wake`] followed by their async capture and `observe`.
    pub async fn hook(
        &self,
        after: &ExternalWakeCursor,
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        capture: impl FnOnce() -> ExternalObservation<Props>,
    ) -> Result<ExternalHookOutcome, MountedExternalControllerError<Port::Error>> {
        let wake_cursor = self.wait_for_wake(after).await?;
        let observation = self.observe(call_id, input_id, capture()).await?;
        Ok(ExternalHookOutcome {
            wake_cursor,
            observation,
        })
    }

    /// Cancel a ticket before it enters a domain commit.
    ///
    /// A port must reject cancellation once it has begun the matching domain
    /// transaction. That condition is checked by the same authoritative
    /// transaction boundary that accepts `Commit`; a local lock is not enough.
    /// For an unacknowledged delivery, the same transaction must also prove
    /// that the exact outbox row was not consumed and durably tombstone it.
    /// Ambiguous delivery is a rejection, not permission to send a successor.
    pub async fn cancel(
        &self,
        token: &ExternalActionToken,
    ) -> Result<ExternalCancelOutcome, MountedExternalControllerError<Port::Error>> {
        self.validate_token(token)?;
        let _local_operation = self.operation_lock.lock().await;
        for _attempt in 1..=MAX_EXTERNAL_STATE_RETRIES {
            let loaded = self.load_state().await?;
            match self.lifecycle_from_state(&loaded.state, &loaded.wake_cursor, token) {
                ExternalActionLifecycle::Cancelled => {
                    return Ok(ExternalCancelOutcome::AlreadyCancelled);
                }
                ExternalActionLifecycle::Committed { wake_cursor, .. } => {
                    return Ok(ExternalCancelOutcome::AlreadyCommitted { wake_cursor });
                }
                ExternalActionLifecycle::RecoveryRequired { identity } => {
                    return Err(MountedExternalControllerError::RecoveryRequired { identity });
                }
                ExternalActionLifecycle::Unknown => {
                    return Err(MountedExternalControllerError::UnknownToken {
                        token: token.clone(),
                    });
                }
                ExternalActionLifecycle::AwaitingDelivery { .. }
                | ExternalActionLifecycle::AwaitingReply { .. }
                | ExternalActionLifecycle::Prepared { .. } => {}
            }

            let active = loaded
                .state
                .current
                .as_ref()
                .and_then(ExternalCurrentFrame::active_action)
                .expect("a non-terminal known token has an active action");
            let delivery_receipt = active.ticket().delivery_receipt().clone();
            let cancelled_before_ack =
                matches!(active, ExternalActiveAction::AwaitingDelivery { .. });

            let mut cancelled = loaded.state;
            cancelled.current = None;
            if cancelled_before_ack {
                cancelled.force_full_next_prompt = true;
            }
            cancelled.terminal.push(ExternalTerminalAction::Cancelled {
                token: token.clone(),
            });
            let state = Self::encode_state(&cancelled)?;
            let outcome = self
                .port
                .compare_exchange(ExternalStateWrite::new(
                    &self.session_id,
                    Some(&loaded.generation),
                    &state,
                    ExternalStateMutation::Cancel {
                        token,
                        delivery_receipt: &delivery_receipt,
                    },
                ))
                .await
                .map_err(MountedExternalControllerError::Port)?;
            match outcome {
                ExternalStateWriteOutcome::Committed { generation, .. } => {
                    Self::ensure_generation_advanced(&loaded.generation, &generation)?;
                    return Ok(ExternalCancelOutcome::Cancelled);
                }
                ExternalStateWriteOutcome::Conflict { .. } => continue,
                ExternalStateWriteOutcome::SourceStale { .. } => {
                    return Err(MountedExternalControllerError::CancellationRejected {
                        token: token.clone(),
                    });
                }
                ExternalStateWriteOutcome::Indeterminate => {
                    return Err(MountedExternalControllerError::CancelIndeterminate {
                        token: token.clone(),
                    });
                }
            }
        }
        Err(MountedExternalControllerError::StateConflict {
            attempts: MAX_EXTERNAL_STATE_RETRIES,
        })
    }

    /// Inspect an action without acquiring an owner lease or sending a prompt.
    pub async fn lookup(
        &self,
        token: &ExternalActionToken,
    ) -> Result<ExternalActionLifecycle, MountedExternalControllerError<Port::Error>> {
        self.validate_token(token)?;
        let loaded = self.load_state().await?;
        Ok(self.lifecycle_from_state(&loaded.state, &loaded.wake_cursor, token))
    }

    /// Resolve an indeterminate `Commit` through the host's durable ledger.
    ///
    /// `NotCommitted` deliberately returns `Retry` instead of silently
    /// calling `act`: the caller remains responsible for deciding whether the
    /// original reply is still appropriate. `Unknown` is fenced durably before
    /// this method returns and cannot be retried automatically.
    pub async fn recover(
        &self,
        identity: &ExternalCommitIdentity,
    ) -> Result<
        ExternalRecoveryOutcome<Contract::Action>,
        MountedExternalControllerError<Port::Error>,
    > {
        self.validate_token(identity.token())?;
        let _local_operation = self.operation_lock.lock().await;
        match self
            .port
            .resolve_commit(identity)
            .await
            .map_err(MountedExternalControllerError::Port)?
        {
            ExternalCommitResolution::Committed => {
                let loaded = self.load_state().await?;
                if let Some(replay) =
                    self.replay_for(&loaded.state, &loaded.wake_cursor, identity)?
                {
                    return Ok(ExternalRecoveryOutcome::Replayed(replay));
                }
                Err(MountedExternalControllerError::RecoveryInvariant {
                    identity: identity.clone(),
                })
            }
            ExternalCommitResolution::NotCommitted => {
                let loaded = self.load_state().await?;
                if let Some(replay) =
                    self.replay_for(&loaded.state, &loaded.wake_cursor, identity)?
                {
                    return Ok(ExternalRecoveryOutcome::Replayed(replay));
                }
                match loaded.state.current {
                    Some(ExternalCurrentFrame::Actionable(ExternalActiveAction::Prepared {
                        identity: stored,
                        ..
                    })) if stored == *identity => Ok(ExternalRecoveryOutcome::Retry),
                    Some(ExternalCurrentFrame::Actionable(
                        ExternalActiveAction::RecoveryRequired {
                            identity: stored, ..
                        },
                    )) if stored == *identity => Ok(ExternalRecoveryOutcome::RecoveryRequired),
                    _ => Err(MountedExternalControllerError::RecoveryInvariant {
                        identity: identity.clone(),
                    }),
                }
            }
            ExternalCommitResolution::Unknown => self.fence_recovery(identity).await,
        }
    }

    async fn fence_recovery(
        &self,
        identity: &ExternalCommitIdentity,
    ) -> Result<
        ExternalRecoveryOutcome<Contract::Action>,
        MountedExternalControllerError<Port::Error>,
    > {
        for _attempt in 1..=MAX_EXTERNAL_STATE_RETRIES {
            let loaded = self.load_state().await?;
            if let Some(replay) = self.replay_for(&loaded.state, &loaded.wake_cursor, identity)? {
                return Ok(ExternalRecoveryOutcome::Replayed(replay));
            }
            let active = loaded
                .state
                .current
                .as_ref()
                .and_then(ExternalCurrentFrame::active_action)
                .cloned();
            let Some(active) = active.as_ref() else {
                return Err(MountedExternalControllerError::RecoveryInvariant {
                    identity: identity.clone(),
                });
            };
            match active {
                ExternalActiveAction::RecoveryRequired {
                    identity: stored, ..
                } if stored == identity => return Ok(ExternalRecoveryOutcome::RecoveryRequired),
                ExternalActiveAction::Prepared {
                    ticket,
                    identity: stored,
                    ..
                } if stored == identity => {
                    let mut fenced = loaded.state;
                    fenced.current = Some(ExternalCurrentFrame::Actionable(
                        ExternalActiveAction::RecoveryRequired {
                            ticket: ticket.clone(),
                            identity: identity.clone(),
                        },
                    ));
                    let state = Self::encode_state(&fenced)?;
                    let outcome = self
                        .port
                        .compare_exchange(ExternalStateWrite::new(
                            &self.session_id,
                            Some(&loaded.generation),
                            &state,
                            ExternalStateMutation::MarkRecovery { identity },
                        ))
                        .await
                        .map_err(MountedExternalControllerError::Port)?;
                    match outcome {
                        ExternalStateWriteOutcome::Committed { generation, .. } => {
                            Self::ensure_generation_advanced(&loaded.generation, &generation)?;
                            return Ok(ExternalRecoveryOutcome::RecoveryRequired);
                        }
                        ExternalStateWriteOutcome::Conflict { .. } => continue,
                        ExternalStateWriteOutcome::SourceStale { .. }
                        | ExternalStateWriteOutcome::Indeterminate => {
                            return Err(MountedExternalControllerError::RecoveryFenceRejected {
                                identity: identity.clone(),
                            });
                        }
                    }
                }
                _ => {
                    return Err(MountedExternalControllerError::RecoveryInvariant {
                        identity: identity.clone(),
                    });
                }
            }
        }
        Err(MountedExternalControllerError::StateConflict {
            attempts: MAX_EXTERNAL_STATE_RETRIES,
        })
    }
}
