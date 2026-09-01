//! Retained Component execution and the public model-backend boundary.
//!
//! [`Application`] is the stable owner boundary for one mounted Component
//! runtime and fixed reaction port. Its port, session, history, and driver
//! demand plumbing remain private; callers mount, observe a projection, drive
//! explicit reactions, and consume the owner with shutdown.

mod admission;
mod application;
#[cfg(feature = "legacy-provider-port")]
mod application_host;
#[cfg(feature = "legacy-provider-port")]
mod component_reaction;
mod debug;
mod driver_demand;
mod external;
mod frame;
mod integration;
mod port;
mod projection_diff;
#[cfg(feature = "legacy-provider-port")]
mod prompt_render;

pub(crate) use driver_demand::{DriverDemandFault, DriverDemandHandle};

/// Frame protocol for Provider, Skill, and Plugin adapters.
///
/// This module path remains available for compatibility. New integrations can
/// import the curated protocol directly from `component::execution`.
pub mod reaction;

pub use admission::{ReactionAdmissionReason, ToolOutputStagingReason};
pub use application::{
    Application, ApplicationFault, ApplicationFaultCode, ApplicationFaultKind,
    ApplicationFaultReason, ApplicationFaultStage, ProjectionSnapshot,
};
#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "this feature-gated export retains the deprecated ApplicationHost compatibility API"
)]
#[deprecated(note = "use `Application<P>` as the mounted runtime owner")]
pub use application_host::{
    ApplicationHost, ApplicationHostFault, EngineObservation, EngineObserver,
};
#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "this feature-gated export retains the deprecated ComponentReactionRuntime API"
)]
#[deprecated(note = "use `Application<P>` as the mounted runtime owner")]
pub use component_reaction::{
    ComponentReactionGeneration, ComponentReactionOutput, ComponentReactionOutputError,
    ComponentReactionProps, ComponentReactionRuntime, ComponentReactionRuntimeFault,
};
pub use debug::{DebugFrameSnapshot, DebugPromptCapture, DebugProviderPort};
pub use external::{
    ExternalAct, ExternalApplication, ExternalApplicationFault, ExternalControl,
    ExternalControlFault, ExternalIngressGeneration, ExternalObservation, ExternalObservationKind,
    ExternalProviderPort, ExternalRenderingGeneration,
};
#[cfg(feature = "legacy-provider-port")]
#[deprecated(note = "use `ProviderFactStream` with `ReactionPort`")]
pub use port::ProviderEventStream;
#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "this feature-gated export retains the deprecated ProviderPort compatibility trait"
)]
#[deprecated(note = "use `ReactionPort`")]
pub use port::ProviderPort;
#[cfg(feature = "legacy-provider-port")]
pub(crate) use port::ToolOutputSink;
pub(crate) use port::{
    ProjectionExecutionScope, ProviderResponseOutputIdentityObservedSpan,
    RenderedProjectionFragment, RenderedProjectionItemTemplate,
};
pub use port::{
    ProviderEvent, ProviderFault, ProviderFaultCode, ProviderFaultKind, ProviderIdentity,
    ProviderResponseCompletedReconciliation, ProviderResponseEventDiagnostic,
    ProviderResponseEventReason, ProviderResponseEventType, ProviderResponseLedgerReason,
    ProviderResponseMessageTextReason, ProviderResponseOutputIdentityDetail,
    ProviderResponseOutputIdentityKindPair, ProviderResponseOutputIdentityLifecycleState,
    ProviderResponseOutputIdentityMappingBasis,
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
#[cfg(feature = "legacy-provider-port")]
pub(crate) use projection_diff::ProjectionAppendPolicy;
pub(crate) use projection_diff::{
    ProjectionDiffState, ProjectionReconciliationState, ReconciledProjectionPlan,
};
pub use reaction::{
    Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
    FrameSubmission, ProjectionSubmission, ProviderFact, ProviderFactStream, ProviderOutputKey,
    ProviderToolCall, ProviderToolCallError, ReactionPort, ReactionPortFault,
    ReactionPortFaultCode, ReactionPortFaultKind, ReactionPortFaultReason, SubmitFault,
    TargetContinuity, TargetDeclaration, TargetEpoch, TargetIdentity, ToolCatalog,
};
