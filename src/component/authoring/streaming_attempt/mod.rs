//! Strict, transactional streaming XML tool declarations.

mod builder;
mod contract;
pub(crate) mod effects;
pub(crate) mod parser;
mod types;

pub use builder::{Missing, NoState, Ready, XmlStreamingAttemptBuilder};
pub use effects::{
    LiveApplyOutcome, LiveApplyResolution, LiveEffectRuntime, LiveSettleOutcome,
    LiveSettleResolution, NoLiveRuntime, NoPublisher, StreamingPublishOutcome,
    StreamingPublishResolution, StreamingToolAttemptPublisher,
};
pub use types::{
    AcceptedStreamingToolAttempt, LiveConfirmContext, LiveEffectContext, LiveRecoveryContext,
    LiveRollbackContext, LiveSettlement, LiveSettlementContext, MissingCompletion,
    NoStreamingValue, ReadyCompletion, RejectedStreamingToolAttempt, SelfClosing,
    StagedStreamingToolEmission, StagedStreamingToolValue, StreamingEffectId,
    StreamingEmissionOrigin, StreamingPublicationId, StreamingPublishContext,
    StreamingPublishRecoveryContext, StreamingToolAbortCause, StreamingToolAttemptContext,
    StreamingToolAttemptId, StreamingToolAttemptStart, StreamingToolChannels,
    StreamingToolDecision, StreamingToolDiagnostic, StreamingToolDiagnosticOrigin,
    StreamingToolDiagnosticRecord, StreamingToolEmission, StreamingToolRecoveryPhase,
    StreamingToolRejection, StreamingToolRejectionAction, StreamingToolUpdate, TextContent,
    XmlAttemptSummary, XmlAttributeAccessFault, XmlCardinality, XmlComplete, XmlContractViolation,
    XmlContractViolationKind, XmlDecodeViolation, XmlDecodedAttributes, XmlElementContract,
    XmlElementDraft, XmlElementForm, XmlElementHandlers, XmlElementSummary, XmlEnvelope,
    XmlOccurrenceId, XmlOccurrenceRejection, XmlOccurrenceValidity, XmlOpen, XmlSourceSpan,
    XmlTextDelta, XmlToolElement,
};

pub(crate) use contract::{ContractDeclaration, ErasedContract};
pub(crate) use types::{StreamingToolDeclarationFault, StreamingToolDriverFault};
