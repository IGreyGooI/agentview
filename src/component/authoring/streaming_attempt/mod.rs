//! Typed streaming XML tools with per-occurrence validation and managed effects.
//!
//! Ordinary model mistakes produce diagnostics while valid independent elements
//! continue to dispatch. The final reducer owns attempt acceptance; applications
//! render useful diagnostics back to the model, including after partial acceptance.

mod builder;
mod callbacks;
mod contract;
pub(crate) mod effects;
pub(crate) mod parser;
mod types;

pub use builder::{Missing, NoState, Ready, XmlStreamingAttemptBuilder};
pub use callbacks::{
    AsyncXmlCallback, IntoXmlCallbackElement, SyncXmlCallback, XmlCallbackDiagnostic,
    XmlCallbackElement, XmlCallbackReturn, XmlStreamingToolCallProps,
};
pub(crate) use callbacks::{XmlCallbackDeclaration, XmlCallbackFeedback, XmlCallbackRuntime};
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
