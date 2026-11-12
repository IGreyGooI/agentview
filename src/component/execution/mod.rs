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
pub(crate) mod command;
mod command_parser;
#[cfg(feature = "legacy-provider-port")]
mod component_reaction;
mod debug;
mod decision;
mod driver_demand;
mod external;
mod frame;
mod frame_observation;
mod integration;
mod port;
mod projection_diff;
mod prompt_export;
mod prompt_profile;
#[cfg(feature = "legacy-provider-port")]
mod prompt_render;
mod stdin;
mod streaming;
mod tool_definition;

pub(crate) use driver_demand::{DriverDemandFault, DriverDemandHandle};

/// Frame protocol for Provider, Skill, and Plugin adapters.
///
/// This module path remains available for compatibility. New integrations can
/// import the curated protocol directly from `component::execution`.
pub mod reaction;

pub use crate::component::authoring::ExitReason;
pub use crate::component::ReactionKind;
pub use admission::{ReactionAdmissionReason, ToolOutputStagingReason};
pub use application::{
    ApplicationFault, ApplicationFaultCode, ApplicationFaultKind, ApplicationFaultReason,
    ApplicationFaultStage, ParallelApplication as Application, ProjectionSnapshot, Reaction,
    ReactionOutcome, ReactionWait, DefinitionUpdate, DefinitionUpdateError,
    DefinitionUpdateErrorKind, DefinitionVersion,
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
pub use command::{
    CommandCall, CommandInput, CommandInputError, CommandOutcome, CommandResponse, CommandSender,
};
pub use command_parser::{CommandParseError, CommandParser};
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
pub use frame_observation::FrameObservation;
pub use decision::{
    ChoiceAnswer, DecisionAnswer, DecisionAnswers, DecisionBatchId, DecisionBox,
    DecisionDeclaration, DecisionDeclarationError, DecisionFailure, DecisionNoulCriteria,
    DecisionOutput, DecisionQuestion, DecisionSource, NoulAnswer, ProviderDecision,
    QuestionOutcome, ScoreAnswer,
};
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
    ProjectionExecutionScope, ProjectionProvenance, ProviderResponseOutputIdentityObservedSpan,
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
pub use prompt_export::PromptExportError;
pub use prompt_profile::{
    ComponentByteUsage, PromptByteProfile, PromptComponent, PromptSource, PromptSourceKind,
    PromptSourceSpan, RenderedPrompt,
};
pub use reaction::{
    ContextWindow, ForkableReactionPort, Frame, FrameBasis, FrameCapabilities, FrameCapability,
    FrameConstraints, FrameProfile, FrameRequirements, FrameRevision, FrameSubmission,
    ProjectionSubmission, ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall,
    ProviderToolCallError, ReactionPort, ReactionPortFault, ReactionPortFaultCode,
    ReactionPortFaultKind, ReactionPortFaultReason, ResettableReactionPort, SubmitFault,
    TargetContinuity, TargetDeclaration, TargetEpoch, TargetIdentity, ToolCatalog,
};
pub use stdin::{StdinApplication, StdinApplicationError, StdinApplicationOptions, StdinResponse};
pub use streaming::{StreamingToolRecoveryReport, StreamingToolRecoveryStatus};
pub use tool_definition::{ToolDefinition, ToolDefinitionError};
