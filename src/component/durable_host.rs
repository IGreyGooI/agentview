//! Narrow persistence port for an AgentView-owned durable mounted host.
//!
//! Backends store an opaque, versioned AgentView state blob. They do not
//! interpret mounted call leases, epoch fences, session revisions, provider
//! cursors, or reconfiguration state. AgentView evaluates those contracts and
//! uses this port only for an authoritative read and one atomic state/outbox
//! compare-and-exchange.

use std::{error::Error, fmt};

use crate::StorageString;

use super::{
    publication::{PublicationCandidateFingerprint, StagedOutbox},
    DurableSessionId,
};

/// Invalid durable backend generation token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid mounted state generation `{value}`; it must be non-empty and contain no control characters")]
pub struct MountedStateGenerationError {
    value: String,
}

/// Backend-issued compare-and-exchange token for one durable mounted state.
///
/// This token is deliberately separate from the mounted session revision.
/// Backends may use a row version, transaction id, or another opaque value,
/// but every successful write must return a different token.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct MountedStateGeneration(StorageString);

impl MountedStateGeneration {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, MountedStateGenerationError> {
        let value = value.into();
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(MountedStateGenerationError {
                value: value.to_string(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MountedStateGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Opaque AgentView-owned durable state bytes.
///
/// A backend must persist these bytes exactly. Schema validation and migration
/// belong to AgentView when the state is loaded; a backend must not deserialize
/// or partially update the blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedStateBlob(Vec<u8>);

impl MountedStateBlob {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// One authoritative backend observation for a durable mounted session.
///
/// `observed_at_unix_ms` must come from the same persistence authority that
/// performs compare-and-exchange. AgentView uses it to classify leases without
/// trusting a process-local clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedStateSnapshot {
    generation: Option<MountedStateGeneration>,
    state: Option<MountedStateBlob>,
    observed_at_unix_ms: u64,
}

impl MountedStateSnapshot {
    pub fn missing(observed_at_unix_ms: u64) -> Self {
        Self {
            generation: None,
            state: None,
            observed_at_unix_ms,
        }
    }

    pub fn present(
        generation: MountedStateGeneration,
        state: MountedStateBlob,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self {
            generation: Some(generation),
            state: Some(state),
            observed_at_unix_ms,
        }
    }

    pub fn generation(&self) -> Option<&MountedStateGeneration> {
        self.generation.as_ref()
    }

    pub fn state(&self) -> Option<&MountedStateBlob> {
        self.state.as_ref()
    }

    pub fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }

    pub fn into_parts(
        self,
    ) -> (
        Option<MountedStateGeneration>,
        Option<MountedStateBlob>,
        u64,
    ) {
        (self.generation, self.state, self.observed_at_unix_ms)
    }
}

/// Atomic opaque-state and typed-outbox write requested by AgentView.
///
/// The backend must compare `expected_generation`, check the optional deadline
/// using its authoritative clock, replace `state`, and insert every outbox item
/// in one transaction. A conflict or elapsed deadline must write neither the
/// state nor any outbox item.
#[derive(Debug, Clone, Copy)]
pub struct MountedStateWrite<'a, Payload> {
    session_id: &'a DurableSessionId,
    expected_generation: Option<&'a MountedStateGeneration>,
    state: &'a MountedStateBlob,
    outbox: Option<&'a StagedOutbox<Payload>>,
    must_commit_before_unix_ms: Option<u64>,
}

impl<'a, Payload> MountedStateWrite<'a, Payload> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        session_id: &'a DurableSessionId,
        expected_generation: Option<&'a MountedStateGeneration>,
        state: &'a MountedStateBlob,
        outbox: Option<&'a StagedOutbox<Payload>>,
        must_commit_before_unix_ms: Option<u64>,
    ) -> Self {
        Self {
            session_id,
            expected_generation,
            state,
            outbox,
            must_commit_before_unix_ms,
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

    pub fn outbox(&self) -> Option<&'a StagedOutbox<Payload>> {
        self.outbox
    }

    pub fn must_commit_before_unix_ms(&self) -> Option<u64> {
        self.must_commit_before_unix_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_write_keeps_cas_scope_and_backend_deadline() {
        let session_id = DurableSessionId::new("session-7").unwrap();
        let generation = MountedStateGeneration::new("row-version-3").unwrap();
        let state = MountedStateBlob::new(b"opaque-state".to_vec());
        let write =
            MountedStateWrite::<()>::new(&session_id, Some(&generation), &state, None, Some(5_000));

        assert_eq!(write.session_id(), &session_id);
        assert_eq!(write.expected_generation(), Some(&generation));
        assert_eq!(write.state().as_bytes(), b"opaque-state");
        assert!(write.outbox().is_none());
        assert_eq!(write.must_commit_before_unix_ms(), Some(5_000));
    }

    #[test]
    fn state_snapshot_cannot_mix_missing_generation_with_present_state() {
        let missing = MountedStateSnapshot::missing(41);
        assert!(missing.generation().is_none());
        assert!(missing.state().is_none());

        let present = MountedStateSnapshot::present(
            MountedStateGeneration::new("version-1").unwrap(),
            MountedStateBlob::new([1, 2, 3]),
            42,
        );
        assert_eq!(present.generation().unwrap().as_str(), "version-1");
        assert_eq!(present.state().unwrap().as_bytes(), &[1, 2, 3]);
        assert_eq!(present.observed_at_unix_ms(), 42);
    }
}

/// Result of one backend compare-and-exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountedStateWriteOutcome {
    /// State and all outbox rows committed atomically.
    Committed {
        generation: MountedStateGeneration,
        committed_at_unix_ms: u64,
    },
    /// The expected generation was stale. No write occurred.
    Conflict { current: MountedStateSnapshot },
    /// The write deadline elapsed according to the backend clock. No write occurred.
    DeadlineElapsed { current: MountedStateSnapshot },
}

impl MountedStateWriteOutcome {
    pub fn committed(generation: MountedStateGeneration, committed_at_unix_ms: u64) -> Self {
        Self::Committed {
            generation,
            committed_at_unix_ms,
        }
    }

    pub fn conflict(current: MountedStateSnapshot) -> Self {
        Self::Conflict { current }
    }

    pub fn deadline_elapsed(current: MountedStateSnapshot) -> Self {
        Self::DeadlineElapsed { current }
    }
}

/// Cross-process persistence port for the AgentView-owned mounted state machine.
///
/// Implementations normally map one [`DurableSessionId`] to one database row
/// plus an outbox table. `compare_exchange` is the sole mutation operation so
/// call admission, session publication, epoch activation, and recovery cannot
/// be split across different transaction domains.
#[async_trait::async_trait]
pub trait DurableMountedStateBackend<Payload>: Send + Sync + 'static
where
    Payload: Send + Sync + 'static,
{
    type Error: Error + Send + Sync + 'static;

    async fn load(
        &self,
        session_id: &DurableSessionId,
    ) -> Result<MountedStateSnapshot, Self::Error>;

    async fn compare_exchange(
        &self,
        request: MountedStateWrite<'_, Payload>,
    ) -> Result<MountedStateWriteOutcome, Self::Error>;
}

/// Pure host policy for fingerprinting the complete typed Commit outbox.
///
/// AgentView combines this value with its private session mutation, provider
/// result, request identity, and raw output before it persists a publication.
/// The implementation must therefore cover every ordered outbox item,
/// including its id, contract, and payload, using a canonical encoding. It
/// never receives owner state, leases, cursors, or mutable runtime handles.
pub trait DurableOutboxFingerprint<Payload>: Send + Sync + 'static
where
    Payload: Send + Sync + 'static,
{
    type Error: Error + Send + Sync + 'static;

    fn fingerprint(
        &self,
        outbox: &StagedOutbox<Payload>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error>;
}
