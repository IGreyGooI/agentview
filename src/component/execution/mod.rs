//! Retained Component execution and the public model-backend boundary.

mod application_host;
mod component_reaction;
mod debug;
mod external;
mod port;
mod prompt_render;

pub use application_host::{
    ApplicationHost, ApplicationHostFault, EngineObservation, EngineObserver,
};
pub use component_reaction::{
    ComponentReactionGeneration, ComponentReactionOutput, ComponentReactionOutputError,
    ComponentReactionProps, ComponentReactionRuntime, ComponentReactionRuntimeFault,
};
pub use debug::{DebugPromptCapture, DebugProviderPort};
pub use external::{
    ExternalAct, ExternalApplication, ExternalApplicationFault, ExternalObservation,
    ExternalObservationKind, ExternalProviderPort, ExternalRenderingGeneration,
};
pub(crate) use port::{
    ProjectionExecutionScope, ProviderResponseOutputIdentityObservedSpan,
    RenderedProjectionFragment, RenderedProjectionItemTemplate, ToolOutputSink,
};
pub use port::{
    ProviderEvent, ProviderEventStream, ProviderFault, ProviderFaultCode, ProviderFaultKind,
    ProviderIdentity, ProviderPort, ProviderResponseCompletedReconciliation,
    ProviderResponseEventDiagnostic, ProviderResponseEventReason, ProviderResponseEventType,
    ProviderResponseLedgerReason, ProviderResponseMessageTextReason,
    ProviderResponseOutputIdentityDetail, ProviderResponseOutputIdentityKindPair,
    ProviderResponseOutputIdentityLifecycleState, ProviderResponseOutputIdentityMappingBasis,
    ProviderResponseOutputIdentityObservedMessageDistance,
    ProviderResponseOutputIdentityObservedMessageRelation,
    ProviderResponseOutputIdentityObservedSpanEntry,
    ProviderResponseOutputIdentityObservedTextRelation,
    ProviderResponseOutputIdentityPhaseRelation, ProviderResponseOutputIdentityReason,
    ProviderResponseOutputIdentityStructure, ProviderResponseOutputItemShapeReason,
    ProviderResponseReconciliationContentPresence, ProviderResponseReconciliationIdPresence,
    ProviderResponseReconciliationIdRelation, ProviderResponseReconciliationItemKind,
    ProviderResponseReconciliationItemStatus, ProviderResponseReconciliationObservedTextState,
    ProviderResponseReconciliationPhase, ProviderResponseReconciliationResponseStatus,
    ProviderResponseReconciliationTextPresence, ProviderResponseReconciliationTextRelation,
    RenderedProjection, RenderedProjectionDiffMarker, RenderedProjectionError,
    RenderedProjectionNode, ToolCall, ToolOutput,
};
