//! Frame-driven reaction protocol shared by Provider, Skill, and Plugin ports.
//!
//! The Component runtime and private `FrameSession` produce [`Frame`] values.
//! A [`ReactionPort`] can inspect and hand off a frame, but cannot mutate the
//! canonical history or the retained Component projection behind it.

use std::{
    num::{NonZeroU128, NonZeroU64},
    pin::Pin,
};

use async_trait::async_trait;
use futures::Stream;

use crate::transcript::{AssistantPhase, CanonicalInputItem, CanonicalTranscriptError};

/// Mount-stable identity of the logical target owned by one reaction port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetIdentity(NonZeroU128);

impl TargetIdentity {
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    pub const fn get(self) -> NonZeroU128 {
        self.0
    }
}

/// Monotonic continuity epoch within one [`TargetIdentity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetEpoch(NonZeroU64);

impl TargetEpoch {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

/// Opaque target-session delivery revision created by the private FrameSession.
///
/// A port may retain and repeat this token in a later declaration. Its fields
/// deliberately have no public accessors: it is not a transcript offset or a
/// dispatch capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameRevision {
    namespace: NonZeroU128,
    target: TargetIdentity,
    epoch: TargetEpoch,
    sequence: NonZeroU64,
}

impl FrameRevision {
    #[allow(dead_code)] // Allocated by the Phase 4 FrameSession compiler.
    pub(crate) const fn new(
        namespace: NonZeroU128,
        target: TargetIdentity,
        epoch: TargetEpoch,
        sequence: NonZeroU64,
    ) -> Self {
        Self {
            namespace,
            target,
            epoch,
            sequence,
        }
    }
}

/// Delivery continuity currently acknowledged by a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetContinuity {
    FullRequired {
        epoch: TargetEpoch,
    },
    Accepted {
        epoch: TargetEpoch,
        revision: FrameRevision,
    },
}

impl TargetContinuity {
    pub const fn epoch(&self) -> TargetEpoch {
        match self {
            Self::FullRequired { epoch } | Self::Accepted { epoch, .. } => *epoch,
        }
    }

    pub const fn accepted_revision(&self) -> Option<FrameRevision> {
        match self {
            Self::FullRequired { .. } => None,
            Self::Accepted { revision, .. } => Some(*revision),
        }
    }
}

/// Relationship between this submission and the target's retained baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBasis {
    Full,
    DeltaFrom(FrameRevision),
}

/// Stable canonical and target-estimated limits for one mounted application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameConstraints {
    /// Hard limit over the complete canonical `FrameSubmissionV1` encoding.
    pub max_frame_bytes: usize,
    /// Guaranteed envelope for the complete Component projection and tools.
    pub max_component_bytes: usize,
    /// Target-provided estimate; it is not part of the canonical byte meter.
    pub context_window_tokens: Option<u64>,
    /// Target-provided estimate; it is not part of the canonical byte meter.
    pub reserved_output_tokens: Option<u64>,
}

/// Optional structured submission features accepted by a target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameCapabilities {
    supports_semantic_delta: bool,
}

impl FrameCapabilities {
    pub const NONE: Self = Self::new(false);

    pub const fn new(supports_semantic_delta: bool) -> Self {
        Self {
            supports_semantic_delta,
        }
    }

    pub const fn supports_semantic_delta(self) -> bool {
        self.supports_semantic_delta
    }
}

/// Mount-stable target contract used to compile frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameProfile {
    pub constraints: FrameConstraints,
    pub capabilities: FrameCapabilities,
}

impl FrameProfile {
    pub const fn new(constraints: FrameConstraints, capabilities: FrameCapabilities) -> Self {
        Self {
            constraints,
            capabilities,
        }
    }
}

/// Idempotent snapshot of a reaction target's continuity and stable profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDeclaration {
    identity: TargetIdentity,
    continuity: TargetContinuity,
    profile: FrameProfile,
}

impl TargetDeclaration {
    pub fn full(identity: TargetIdentity, epoch: TargetEpoch, profile: FrameProfile) -> Self {
        Self {
            identity,
            continuity: TargetContinuity::FullRequired { epoch },
            profile,
        }
    }

    pub fn resume(revision: FrameRevision, profile: FrameProfile) -> Self {
        Self {
            identity: revision.target,
            continuity: TargetContinuity::Accepted {
                epoch: revision.epoch,
                revision,
            },
            profile,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), TargetDeclarationInvariantFault> {
        let TargetContinuity::Accepted { epoch, revision } = &self.continuity else {
            return Ok(());
        };
        if revision.target != self.identity {
            return Err(TargetDeclarationInvariantFault::AcceptedRevisionTargetMismatch);
        }
        if revision.epoch != *epoch {
            return Err(TargetDeclarationInvariantFault::AcceptedRevisionEpochMismatch);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn from_raw(
        identity: TargetIdentity,
        continuity: TargetContinuity,
        profile: FrameProfile,
    ) -> Self {
        Self {
            identity,
            continuity,
            profile,
        }
    }

    pub const fn identity(&self) -> TargetIdentity {
        self.identity
    }

    pub const fn continuity(&self) -> &TargetContinuity {
        &self.continuity
    }

    pub const fn profile(&self) -> &FrameProfile {
        &self.profile
    }
}

/// Internal fault for a declaration that bypassed its coherent constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TargetDeclarationInvariantFault {
    #[error("accepted Frame revision belongs to a different target")]
    AcceptedRevisionTargetMismatch,
    #[error("accepted Frame revision belongs to a different target epoch")]
    AcceptedRevisionEpochMismatch,
}

/// Already-lowered Component section of one Full or Delta submission.
///
/// A Full may begin with one normalized System snapshot. A Delta never carries
/// System input because an unchanged snapshot remains part of its accepted
/// baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionSubmission {
    items: Vec<CanonicalInputItem>,
}

impl ProjectionSubmission {
    #[allow(dead_code)] // Constructed only by the private Frame compiler.
    pub(crate) fn new(items: Vec<CanonicalInputItem>) -> Self {
        Self { items }
    }

    /// Ordered, already-lowered Component input for this submission.
    pub fn items(&self) -> &[CanonicalInputItem] {
        &self.items
    }
}

/// Component-declared tool identities in stable canonical order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCatalog {
    names: Vec<String>,
}

impl ToolCatalog {
    #[allow(dead_code)] // Constructed only by Component reconciliation.
    pub(crate) fn new(mut names: Vec<String>) -> Result<Self, ToolCatalogFault> {
        names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        for name in &names {
            if name.is_empty() {
                return Err(ToolCatalogFault::EmptyName);
            }
        }
        if let Some(name) = names
            .windows(2)
            .find_map(|pair| (pair[0] == pair[1]).then(|| pair[0].clone()))
        {
            return Err(ToolCatalogFault::DuplicateName { name });
        }
        Ok(Self { names })
    }

    /// Component-declared tool identities in canonical order.
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ToolCatalogFault {
    #[error("Component tool name must not be empty")]
    EmptyName,
    #[error("Component tool name `{name}` is declared more than once")]
    DuplicateName { name: String },
}

/// Exact target-visible semantic payload for one frame.
#[derive(Debug, PartialEq)]
pub struct FrameSubmission {
    replay: Vec<CanonicalInputItem>,
    staged_inputs: Vec<CanonicalInputItem>,
    projection: ProjectionSubmission,
    tools: ToolCatalog,
    canonical_bytes: Box<[u8]>,
}

impl FrameSubmission {
    /// Installs sections and their already-validated canonical JCS encoding.
    #[allow(dead_code)] // Called by the Phase 4 Frame compiler.
    pub(crate) fn from_compiled(
        replay: Vec<CanonicalInputItem>,
        staged_inputs: Vec<CanonicalInputItem>,
        projection: ProjectionSubmission,
        tools: ToolCatalog,
        canonical_bytes: Vec<u8>,
    ) -> Self {
        Self {
            replay,
            staged_inputs,
            projection,
            tools,
            canonical_bytes: canonical_bytes.into_boxed_slice(),
        }
    }

    /// Exact append-only replay section for this Full or Delta submission.
    /// Replaceable Component System state is never stored here.
    pub fn replay(&self) -> &[CanonicalInputItem] {
        &self.replay
    }

    /// Ordered mandatory inputs handed off by this submission.
    pub fn staged_inputs(&self) -> &[CanonicalInputItem] {
        &self.staged_inputs
    }

    pub fn projection(&self) -> &ProjectionSubmission {
        &self.projection
    }

    pub fn tools(&self) -> &ToolCatalog {
        &self.tools
    }

    /// Exact stable framework encoding used by `max_frame_bytes` accounting.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
}

/// One move-only, exact submission prepared for a declared target snapshot.
#[derive(Debug, PartialEq)]
pub struct Frame {
    revision: FrameRevision,
    target: TargetIdentity,
    epoch: TargetEpoch,
    prepared_against: TargetContinuity,
    prepared_profile: FrameProfile,
    basis: FrameBasis,
    submission: FrameSubmission,
}

impl Frame {
    /// Builds a Frame only after checking every duplicated continuity field.
    #[allow(dead_code)] // Called by the Phase 4 Frame compiler.
    pub(crate) fn from_compiled(
        revision: FrameRevision,
        target: TargetIdentity,
        epoch: TargetEpoch,
        prepared_against: TargetContinuity,
        prepared_profile: FrameProfile,
        basis: FrameBasis,
        submission: FrameSubmission,
    ) -> Result<Self, FrameInvariantFault> {
        if revision.target != target || revision.epoch != epoch {
            return Err(FrameInvariantFault::RevisionScopeMismatch);
        }
        if prepared_against.epoch() != epoch {
            return Err(FrameInvariantFault::PreparedEpochMismatch);
        }

        if let Some(accepted) = prepared_against.accepted_revision() {
            if accepted.target != target || accepted.epoch != epoch {
                return Err(FrameInvariantFault::PreparedRevisionScopeMismatch);
            }
        }

        if let FrameBasis::DeltaFrom(base) = basis {
            if prepared_against.accepted_revision() != Some(base) {
                return Err(FrameInvariantFault::DeltaPreconditionMismatch);
            }
            if base.target != target || base.epoch != epoch || base.namespace != revision.namespace
            {
                return Err(FrameInvariantFault::DeltaBaseScopeMismatch);
            }
            if base.sequence >= revision.sequence {
                return Err(FrameInvariantFault::NonMonotonicRevision);
            }
        }

        Ok(Self {
            revision,
            target,
            epoch,
            prepared_against,
            prepared_profile,
            basis,
            submission,
        })
    }

    pub const fn revision(&self) -> FrameRevision {
        self.revision
    }

    pub const fn target(&self) -> TargetIdentity {
        self.target
    }

    pub const fn epoch(&self) -> TargetEpoch {
        self.epoch
    }

    pub const fn prepared_against(&self) -> &TargetContinuity {
        &self.prepared_against
    }

    /// Exact mount-stable profile used to compile this Frame.
    pub const fn prepared_profile(&self) -> &FrameProfile {
        &self.prepared_profile
    }

    /// Checks the target snapshot immediately before crossing handoff.
    ///
    /// Ports should call this against their current snapshot in the same poll
    /// that can first cause real or ambiguous delivery.
    pub fn check_handoff_precondition(
        &self,
        current: &TargetDeclaration,
    ) -> Result<(), SubmitFault> {
        if current.validate().is_err()
            || current.identity() != self.target
            || current.continuity() != &self.prepared_against
        {
            return Err(SubmitFault::ContinuityChanged);
        }
        if current.profile() != &self.prepared_profile {
            return Err(SubmitFault::ProfileChanged);
        }
        Ok(())
    }

    pub const fn basis(&self) -> FrameBasis {
        self.basis
    }

    pub const fn submission(&self) -> &FrameSubmission {
        &self.submission
    }
}

/// Internal compiler fault for a self-contradictory Frame candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FrameInvariantFault {
    #[error("Frame revision does not belong to the Frame target and epoch")]
    RevisionScopeMismatch,
    #[error("prepared continuity epoch does not match the Frame epoch")]
    PreparedEpochMismatch,
    #[error("accepted revision does not belong to the Frame target and epoch")]
    PreparedRevisionScopeMismatch,
    #[error("DeltaFrom revision does not advance its base revision")]
    NonMonotonicRevision,
    #[error("DeltaFrom base does not equal the accepted prepared revision")]
    DeltaPreconditionMismatch,
    #[error("DeltaFrom base does not belong to the next Frame revision scope")]
    DeltaBaseScopeMismatch,
}

/// Reaction-local identity of one provider-neutral output lifecycle.
///
/// The first fact carrying a previously unseen key establishes that output's
/// position in canonical reaction order. Later facts for the key retain that
/// position even when other output lifecycles are interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderOutputKey(u64);

impl ProviderOutputKey {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One complete, provider-neutral function call emitted by a reaction target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderToolCall {
    call_id: String,
    name: String,
    raw_arguments: String,
}

impl ProviderToolCall {
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Result<Self, ProviderToolCallError> {
        let item = CanonicalInputItem::tool_call(call_id, name, raw_arguments)?;
        let CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } = item
        else {
            unreachable!("canonical tool call constructor returns a ToolCall")
        };
        Ok(Self {
            call_id,
            name,
            raw_arguments,
        })
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn raw_arguments(&self) -> &str {
        &self.raw_arguments
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderToolCallError {
    #[error(transparent)]
    Canonical(#[from] CanonicalTranscriptError),
}

/// One ordered, target-neutral fact that shared history can admit directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFact {
    TextDelta {
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        delta: String,
    },
    TextSealed {
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        text: String,
    },
    ToolCall {
        output: ProviderOutputKey,
        ordinal: u64,
        call: ProviderToolCall,
    },
    ReactionCompleted {
        primary_text: Option<ProviderOutputKey>,
    },
}

/// Retry policy for a target failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReactionPortFaultKind {
    /// A later explicit reaction may retry the operation.
    Retryable,
    /// The current logical target session cannot continue.
    Terminal,
}

/// Payload-free structural category for a target failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReactionPortFaultCode {
    Unavailable,
    Rejected,
    Protocol,
    Limit,
    Internal,
}

/// Payload-free cause reported by a reaction port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReactionPortFaultReason {
    Declaration,
    RequestPreparation,
    Transport,
    Authentication,
    Authorization,
    RateLimited,
    UpstreamRejected,
    ResponseProtocol,
    StreamTransport,
    StreamTimeout,
    OutputLimit,
    Other,
}

/// A sanitized target failure reported before handoff or while consuming facts.
///
/// The fault deliberately accepts and retains only closed classifications.
/// Provider-authored text, credentials, wire payloads, and arbitrary error
/// sources must remain in the port's private diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("reaction port {kind:?}: {code:?}/{reason:?}")]
pub struct ReactionPortFault {
    kind: ReactionPortFaultKind,
    code: ReactionPortFaultCode,
    reason: ReactionPortFaultReason,
}

impl ReactionPortFault {
    pub const fn retryable(code: ReactionPortFaultCode, reason: ReactionPortFaultReason) -> Self {
        Self {
            kind: ReactionPortFaultKind::Retryable,
            code,
            reason,
        }
    }

    pub const fn terminal(code: ReactionPortFaultCode, reason: ReactionPortFaultReason) -> Self {
        Self {
            kind: ReactionPortFaultKind::Terminal,
            code,
            reason,
        }
    }

    pub const fn kind(&self) -> ReactionPortFaultKind {
        self.kind
    }

    pub const fn code(&self) -> ReactionPortFaultCode {
        self.code
    }

    pub const fn reason(&self) -> ReactionPortFaultReason {
        self.reason
    }
}

/// A guaranteed pre-handoff failure from [`ReactionPort::submit`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitFault {
    #[error("target continuity changed before frame handoff")]
    ContinuityChanged,
    #[error("target Frame profile changed before frame handoff")]
    ProfileChanged,
    #[error(transparent)]
    Rejected(#[from] ReactionPortFault),
}

/// Ordered target facts borrowed from the port that produced them.
pub type ProviderFactStream<'a> =
    Pin<Box<dyn Stream<Item = Result<ProviderFact, ReactionPortFault>> + Send + 'a>>;

/// Structured integration boundary shared by Provider, Skill, and Plugin ports.
#[async_trait]
pub trait ReactionPort: Send {
    /// Returns an idempotent snapshot without advancing delivery state.
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault>;

    /// Hands one exact Frame to this target and returns its ordered fact stream.
    ///
    /// The port must check [`Frame::check_handoff_precondition`] against its
    /// current target snapshot in the crossing poll. Every `Pending` poll and
    /// every `Ready(Err(_))` proves the Frame has not crossed the handoff
    /// boundary. The first poll that causes real or ambiguous delivery must
    /// return `Ready(Ok(stream))` in that same poll; all failures after that
    /// boundary are yielded by the returned stream.
    ///
    /// Dropping an unfinished returned fact stream must synchronously leave a
    /// later `declare()` truthful: compatible `Accepted` only when exact
    /// continuation remains usable, higher-epoch `FullRequired` when recovery
    /// invalidates continuity, or a terminal declaration fault. Stale
    /// `Accepted` continuity is forbidden.
    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault>;
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU128, NonZeroU64};

    use async_trait::async_trait;
    use futures::stream;

    use super::{
        Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameInvariantFault, FrameProfile,
        FrameRevision, FrameSubmission, ProjectionSubmission, ProviderFact, ProviderFactStream,
        ProviderOutputKey, ProviderToolCall, ReactionPort, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultKind, ReactionPortFaultReason, SubmitFault,
        TargetContinuity, TargetDeclaration, TargetEpoch, TargetIdentity, ToolCatalog,
        ToolCatalogFault,
    };

    struct BorrowingPort {
        declaration: TargetDeclaration,
        facts: Vec<Result<ProviderFact, ReactionPortFault>>,
    }

    #[async_trait]
    impl ReactionPort for BorrowingPort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            Ok(self.declaration.clone())
        }

        async fn submit<'a>(
            &'a mut self,
            _frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            Ok(Box::pin(stream::iter(self.facts.iter().cloned())))
        }
    }

    fn profile() -> FrameProfile {
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 4_096,
                max_component_bytes: 1_024,
                context_window_tokens: Some(8_192),
                reserved_output_tokens: Some(1_024),
            },
            FrameCapabilities::new(true),
        )
    }

    fn revision(
        namespace: u128,
        target: TargetIdentity,
        epoch: TargetEpoch,
        sequence: u64,
    ) -> FrameRevision {
        FrameRevision::new(
            NonZeroU128::new(namespace).unwrap(),
            target,
            epoch,
            NonZeroU64::new(sequence).unwrap(),
        )
    }

    fn submission() -> FrameSubmission {
        FrameSubmission {
            replay: Vec::new(),
            staged_inputs: Vec::new(),
            projection: ProjectionSubmission::new(Vec::new()),
            tools: ToolCatalog::new(Vec::new()).unwrap(),
            canonical_bytes: b"test-only-canonical-frame".to_vec().into_boxed_slice(),
        }
    }

    #[test]
    fn third_party_values_have_public_constructors_and_accessors() {
        let identity = TargetIdentity::new(NonZeroU128::new(7).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(3).unwrap());
        let declaration = TargetDeclaration::full(identity, epoch, profile());

        assert_eq!(declaration.identity(), identity);
        assert_eq!(declaration.continuity().epoch(), epoch);
        assert_eq!(identity.get().get(), 7);
        assert_eq!(epoch.get().get(), 3);
        assert!(declaration.profile().capabilities.supports_semantic_delta());
        assert_eq!(ProviderOutputKey::new(0).get(), 0);
    }

    #[test]
    fn resume_derives_target_scope_from_the_opaque_revision() {
        let identity = TargetIdentity::new(NonZeroU128::new(9).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(4).unwrap());
        let accepted = revision(71, identity, epoch, 12);
        let declaration = TargetDeclaration::resume(accepted, profile());

        assert_eq!(declaration.identity(), identity);
        assert_eq!(declaration.continuity().epoch(), epoch);
        assert_eq!(declaration.continuity().accepted_revision(), Some(accepted));
        assert_eq!(declaration.validate(), Ok(()));
    }

    #[test]
    fn fact_stream_may_borrow_the_port() {
        let identity = TargetIdentity::new(NonZeroU128::new(11).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(1).unwrap());
        let mut port = BorrowingPort {
            declaration: TargetDeclaration::full(identity, epoch, profile()),
            facts: vec![Ok(ProviderFact::ReactionCompleted { primary_text: None })],
        };

        assert_eq!(port.declare().unwrap().identity(), identity);
    }

    #[test]
    fn provider_tool_call_is_ready_for_direct_canonical_admission() {
        let call = ProviderToolCall::new("call-1", "lookup", r#"{"query":"value"}"#).unwrap();
        assert_eq!(call.call_id(), "call-1");
        assert_eq!(call.name(), "lookup");
        assert_eq!(call.raw_arguments(), r#"{"query":"value"}"#);
        assert!(ProviderToolCall::new("", "lookup", "{}").is_err());
        assert!(ProviderToolCall::new("call-1", "", "{}").is_err());
        assert!(ProviderToolCall::new("call-1", "lookup", "not-json").is_err());
        assert!(ProviderToolCall::new("call-1", "lookup", "[]").is_err());
    }

    #[test]
    fn tool_catalog_rejects_empty_and_duplicate_names() {
        assert_eq!(
            ToolCatalog::new(vec![String::new()]),
            Err(ToolCatalogFault::EmptyName)
        );
        assert_eq!(
            ToolCatalog::new(vec!["same".to_owned(), "same".to_owned()]),
            Err(ToolCatalogFault::DuplicateName {
                name: "same".to_owned(),
            })
        );
    }

    #[test]
    fn reaction_port_fault_is_structured_and_does_not_retain_provider_payload() {
        const PROVIDER_SENTINEL: &str = "provider-authored-secret-sentinel";

        fn sanitize_provider_failure(_provider_payload: &str) -> ReactionPortFault {
            ReactionPortFault::terminal(
                ReactionPortFaultCode::Protocol,
                ReactionPortFaultReason::ResponseProtocol,
            )
        }

        let fault = sanitize_provider_failure(PROVIDER_SENTINEL);
        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.code(), ReactionPortFaultCode::Protocol);
        assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
        assert!(!fault.to_string().contains(PROVIDER_SENTINEL));
        assert!(!format!("{fault:?}").contains(PROVIDER_SENTINEL));
        assert!(std::error::Error::source(&fault).is_none());

        let submit = SubmitFault::Rejected(fault);
        assert!(!submit.to_string().contains(PROVIDER_SENTINEL));
        assert!(!format!("{submit:?}").contains(PROVIDER_SENTINEL));

        let retryable = ReactionPortFault::retryable(
            ReactionPortFaultCode::Unavailable,
            ReactionPortFaultReason::Transport,
        );
        assert_eq!(retryable.kind(), ReactionPortFaultKind::Retryable);
        assert_eq!(retryable.code(), ReactionPortFaultCode::Unavailable);
        assert_eq!(retryable.reason(), ReactionPortFaultReason::Transport);
    }

    #[test]
    fn compiled_delta_requires_one_coherent_revision_scope() {
        let target = TargetIdentity::new(NonZeroU128::new(17).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(3).unwrap());
        let base = revision(9, target, epoch, 4);
        let next = revision(9, target, epoch, 5);

        let frame = Frame::from_compiled(
            next,
            target,
            epoch,
            TargetContinuity::Accepted {
                epoch,
                revision: base,
            },
            profile(),
            FrameBasis::DeltaFrom(base),
            submission(),
        )
        .unwrap();

        assert_eq!(frame.revision(), next);
        assert_eq!(frame.prepared_against().accepted_revision(), Some(base));
        assert_eq!(frame.prepared_profile(), &profile());
    }

    #[test]
    fn compiled_full_can_rebase_a_foreign_accepted_namespace() {
        let target = TargetIdentity::new(NonZeroU128::new(19).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(3).unwrap());
        let foreign = revision(91, target, epoch, 99);
        let rebased = revision(9, target, epoch, 1);

        let frame = Frame::from_compiled(
            rebased,
            target,
            epoch,
            TargetContinuity::Accepted {
                epoch,
                revision: foreign,
            },
            profile(),
            FrameBasis::Full,
            submission(),
        )
        .unwrap();

        assert_eq!(frame.revision(), rebased);
        assert_eq!(frame.prepared_against().accepted_revision(), Some(foreign));
        assert_eq!(frame.basis(), FrameBasis::Full);
    }

    #[test]
    fn compiled_delta_rejects_foreign_or_non_advancing_bases() {
        let target = TargetIdentity::new(NonZeroU128::new(20).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(3).unwrap());
        let foreign = revision(91, target, epoch, 4);
        let next = revision(9, target, epoch, 5);

        assert_eq!(
            Frame::from_compiled(
                next,
                target,
                epoch,
                TargetContinuity::Accepted {
                    epoch,
                    revision: foreign,
                },
                profile(),
                FrameBasis::DeltaFrom(foreign),
                submission(),
            ),
            Err(FrameInvariantFault::DeltaBaseScopeMismatch)
        );

        let base = revision(9, target, epoch, 5);
        assert_eq!(
            Frame::from_compiled(
                next,
                target,
                epoch,
                TargetContinuity::Accepted {
                    epoch,
                    revision: base,
                },
                profile(),
                FrameBasis::DeltaFrom(base),
                submission(),
            ),
            Err(FrameInvariantFault::NonMonotonicRevision)
        );
    }

    #[test]
    fn compiled_frame_rejects_contradictory_control_fields() {
        let target = TargetIdentity::new(NonZeroU128::new(21).unwrap());
        let other_target = TargetIdentity::new(NonZeroU128::new(22).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(1).unwrap());
        let other_epoch = TargetEpoch::new(NonZeroU64::new(2).unwrap());
        let base = revision(7, target, epoch, 1);
        let next = revision(7, target, epoch, 2);

        assert_eq!(
            Frame::from_compiled(
                revision(7, other_target, epoch, 2),
                target,
                epoch,
                TargetContinuity::FullRequired { epoch },
                profile(),
                FrameBasis::Full,
                submission(),
            ),
            Err(FrameInvariantFault::RevisionScopeMismatch)
        );
        assert_eq!(
            Frame::from_compiled(
                next,
                target,
                epoch,
                TargetContinuity::FullRequired { epoch: other_epoch },
                profile(),
                FrameBasis::Full,
                submission(),
            ),
            Err(FrameInvariantFault::PreparedEpochMismatch)
        );
        assert_eq!(
            Frame::from_compiled(
                next,
                target,
                epoch,
                TargetContinuity::Accepted {
                    epoch,
                    revision: base,
                },
                profile(),
                FrameBasis::DeltaFrom(revision(7, target, epoch, 2)),
                submission(),
            ),
            Err(FrameInvariantFault::DeltaPreconditionMismatch)
        );
        assert_eq!(
            Frame::from_compiled(
                next,
                target,
                epoch,
                TargetContinuity::Accepted {
                    epoch,
                    revision: revision(7, other_target, epoch, 1),
                },
                profile(),
                FrameBasis::Full,
                submission(),
            ),
            Err(FrameInvariantFault::PreparedRevisionScopeMismatch)
        );
    }
}
