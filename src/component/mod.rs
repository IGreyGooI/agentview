//! Functional component authoring over the Prompt Object Model.
//!
//! A component tree is a short-lived authoring value. Compilation projects its
//! POM fragments into exactly one system document and one user document while
//! collecting typed runtime bindings separately. It is not a virtual DOM and it
//! does not replace POM resolution, semantic diffing, or cursor ownership.

#[cfg_attr(not(feature = "raw-component-ir"), allow(dead_code))]
mod agent;
#[allow(dead_code)]
pub(crate) mod attempt_driver;
mod channels;
#[cfg_attr(not(feature = "raw-component-ir"), allow(dead_code))]
mod compile;
#[allow(dead_code)]
pub(crate) mod durable_epoch;
mod durable_host;
mod erasure;
#[cfg_attr(not(feature = "raw-component-ir"), allow(dead_code, unused_imports))]
mod experimental;
mod external;
mod external_reply;
mod host_binding;
mod identity;
mod lifecycle;
mod local_mounted;
#[allow(dead_code)]
mod managed_attempt;
#[allow(dead_code)]
mod managed_publication;
pub mod mounted;
#[allow(dead_code)]
mod mounted_agent;
mod mounted_contract;
mod node;
mod provider;
mod provider_wire;
#[cfg_attr(not(feature = "raw-component-ir"), allow(dead_code))]
mod provision;
mod publication;
mod stateful_provider;
#[cfg_attr(not(feature = "raw-component-ir"), allow(dead_code))]
mod streaming;

/// Explicit integration escape hatches.
///
/// These contracts are for provider, persistence, and compatibility-runtime
/// adapters. They are intentionally absent from the ordinary component
/// authoring surface and from [`crate::prelude`].
pub mod advanced {
    /// Host-owned durable external action/reply control contracts.
    ///
    /// This boundary is for chat, CLI, daemon, and skill loops that accept a
    /// typed reply outside a provider-backed mounted turn. The host owns the
    /// domain transaction and must atomically persist it with AgentView state.
    pub mod external {
        pub use super::super::external::*;
        pub use super::super::external_reply::{
            external_prompt_component, try_external_prompt_component, ExternalPromptComponent,
            ExternalReply, ExternalUserView, MountedExternalHarnessDefinition,
        };
    }

    /// Host-owned per-attempt runtime binding contracts.
    pub mod host {
        pub use super::super::host_binding::{
            LiveEffectRuntimeBindingContext, LiveEffectRuntimeFactory,
        };
        pub use super::super::local_mounted::MountedHostRuntime;
    }

    /// Raw compatibility IR used only by migration examples and framework
    /// tests.
    ///
    /// Enable the non-default `raw-component-ir` feature to access this
    /// module. Ordinary component authors should use the nominal POM and
    /// mounted APIs exported by this module instead.
    #[cfg(feature = "raw-component-ir")]
    pub mod experimental {
        pub use super::super::experimental::*;
    }

    /// Provider transport, durable attachment, receipt, and cursor contracts.
    pub mod provider {
        pub use super::super::durable_epoch::{
            AttachedProviderEpoch, EpochArtifactFingerprint, EpochContractManifestError,
            ProviderAdapterContract, ProviderAdapterError, ProviderEpochAttachRequest,
            ProviderEpochReceipt, ProviderEpochRehydrateRequest, ProviderToolDescriptor,
            ProviderTurnCursor,
        };
        pub use super::super::provider::{
            FinishedProviderAttempt, MountedProviderAttempt, ProviderAttemptAbortReport,
            ProviderAttemptFinishFailure, ProviderAttemptStartError, ProviderDispatchFault,
            ProviderDispatchOrigin, ProviderDispatchPhase, ProviderDispatcherAbort,
            ProviderDispatcherAbortOutcome, ProviderInvocationCollision, ProviderLiveEffectFault,
            ProviderPublishFailure, ProviderToolAttemptError, ProviderToolCallOutcome,
            PublishedProviderAttempt,
        };
        pub use super::super::provider_wire::{
            DurableMountedProviderExecutor, DurableMountedProviderOperationController,
            FallibleProviderWirePort, MountedProviderEpoch, MountedProviderEpochAttacher,
            MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
            ProviderCancellationCursorDisposition, ProviderCancellationReason,
            ProviderCancellationSource, ProviderCancellationToken, ProviderOperationStatus,
            ProviderToolCatalog, ProviderWireAck, ProviderWireEvent, ProviderWireFault,
        };
        // Provider capability construction binds an async dispatcher. Keep it
        // out of the functional component-authoring prelude: components may
        // declare prompt-facing contracts, while a host owns the I/O runtime.
        pub use super::super::provision::{
            durable_provider_tool, durable_provider_tool_with_context,
            durable_provider_tool_with_context_key, durable_provider_tool_with_key,
            durable_provider_tools, durable_provider_tools_with_context,
            durable_provider_tools_with_context_key, durable_provider_tools_with_key,
            provider_tool, provider_tool_with_context, provider_tools, provider_tools_with_context,
            ProviderDispatchContext, ProviderDispatchFailure, ProviderDispatchUpdate,
            ProviderDispatcher, ProviderDispatcherAbortAck, ProviderDispatcherAbortContext,
            ProviderDispatcherCx, ProviderDispatcherRegistry, ProviderDispatcherRegistryError,
            ProviderToolCall, ProviderToolResponse,
        };
        pub use super::super::publication::ProviderOperationIdentity;
        pub use super::super::stateful_provider::{
            ProviderSessionAttachRequest, ProviderSessionAttachment,
            ProviderSessionRehydrateRequest, StatefulMountedProviderAdapter,
            StatefulMountedProviderAdapterError, StatefulMountedProviderSession,
            StatefulMountedProviderSessionFactory,
        };
    }

    /// Low-level one-shot mounting and runtime type-erasure contracts.
    pub mod lifecycle {
        pub use super::super::erasure::{
            ChannelTypeField, ChannelTypeInfo, ChannelTypeMismatch, TypeSlot,
        };
        pub use super::super::lifecycle::{
            mount_system_epoch, mount_system_epoch_with_contract, MountedEpoch, MountedTurn,
            PreparedUserTurn, SystemMountContext, SystemMountError, SystemView, UserTurnPlan,
        };
    }

    /// Persistence, lease, raw publication, and recovery contracts.
    pub mod persistence {
        pub use super::super::durable_host::{
            DurableMountedStateBackend, DurableOutboxFingerprint, MountedStateBlob,
            MountedStateGeneration, MountedStateGenerationError, MountedStateSnapshot,
            MountedStateWrite, MountedStateWriteOutcome,
        };
        pub use super::super::local_mounted::{
            DurableMountedAgentFactory, DurableMountedFactoryError, DurableMountedHostConfig,
            DurableMountedHostConfigError,
        };
        pub use super::super::mounted_contract::{
            DurableCallLease, DurableCallLedger, DurableCallLedgerError, DurableCallRecoveryReason,
            DurableCallReservationOrigin, DurableCallState, DurableCallStatus,
            MountedSessionSnapshot, SessionReduceContext, SessionReducer,
            SessionReducerFailureDisposition,
        };
        pub use super::super::publication::{
            stage_commit_outbox, CommitContract, CommitStager, CommitStagingContext,
            CommitStagingFailure, DurableCallLeaseId, DurablyPublishedProviderAttempt,
            DurablyPublishedStreamingAttempt, OutboxItemId, PreparedSessionMutation,
            ProviderPublicationStagingFailure, ProviderPublicationStagingResult,
            PublicationAttempt, PublicationAttemptError, PublicationCandidateError,
            PublicationCandidateFingerprint, PublicationFingerprintContext,
            PublicationFingerprintFactory, PublicationPhase, PublicationPhaseError,
            PublicationReceipt, PublicationReceiptMismatch, PublicationRequest,
            PublicationRequestContext, PublicationRequestId, PublicationRequestIdFactory,
            PublicationResolution, PublicationResolveError, PublicationStagingFailure,
            PublicationStagingPlan, PublicationStore, PublicationWriteError, StagedCommit,
            StagedOutbox, StagedOutboxItem, StagedProviderPublication, StagedPublication,
            StagedStreamingPublication, StreamingPublicationStagingFailure,
            StreamingPublicationStagingResult,
        };
    }
}

/// Curated imports for applications that own a mounted component runtime.
///
/// This layer consumes definitions produced through [`prelude`] and owns call
/// admission, capture, cancellation, Live delivery, and session reduction. It
/// deliberately excludes provider transports and persistence adapters; those
/// remain explicit integrations under [`advanced`].
pub mod host {
    pub mod prelude {
        pub use super::super::advanced::host::{
            LiveEffectRuntimeBindingContext, LiveEffectRuntimeFactory, MountedHostRuntime,
        };
        pub use super::super::{
            CapturedMountedHarnessDefinition, DurableCallId, DurableCallInputId,
            DurableEpochDefinition, DurableEpochId, DurableSessionId, EpochContractId,
            InMemoryMountedAgentFactory, InMemoryMountedFactoryError, LiveAbortContext,
            LiveAbortFault, LiveAbortOutcome, LiveEffectAbortAck, LiveEffectContext,
            LiveEffectFailure, LiveEffectFault, LiveEffectRuntime, MountedAgent, MountedCall,
            MountedCallCancellation, MountedCallInput, MountedCallInputError, MountedCallLifecycle,
            MountedCallLookupError, MountedCallOutcome, MountedCallReattachment,
            MountedCallRecoveryReason, MountedCallResult, MountedCallSnapshot,
            MountedHarnessDefinition, MountedHostBindings, MountedOpenError, MountedReloadError,
            MountedStartError, MountedTurnCapture, MountedTurnPublication, MountedWaitError, Never,
            NoLiveEffects, NoTurnChannels, ProviderAttemptIdentity, ProviderDispatcherRegistry,
            ProviderDispatcherRegistryError, SessionReduceContext, SessionReducer,
            SessionReducerFailureDisposition, TurnCaptureContext, TurnChannels, TurnRecord,
            TurnRecordParts,
        };
        pub use crate::agent::TurnFlow;
        pub use crate::agent_session::AgentSession;
        pub use crate::llm_call::ExecutorCommit;
    }
}

/// Curated imports for ordinary POM component authors.
///
/// This surface contains pure prompt authoring, typed provided-component
/// declarations, channel mapping, and mounted harness assembly. Provider
/// transports, turn capture, session reducers, persistence, leases, and
/// runtime actors are host-integration concerns and intentionally remain
/// outside this prelude.
pub mod prelude {
    pub use super::{
        binding_factory, binding_factory_with_context, component, durable_binding_factory,
        durable_binding_factory_with_context, durable_binding_factory_with_context_key,
        durable_binding_factory_with_key, durable_provider_contract, durable_system,
        external_prompt_component, pom_view, prompt_component, try_external_prompt_component,
        try_prompt_component, user_view, view, BindingKey, ChannelMap, Component, ComponentError,
        ComponentKey, DurableComponent, DurableSystem, EpochContractId, ExternalActionRoute,
        ExternalPromptComponent, ExternalReply, ExternalReplyContract, ExternalReplyContractId,
        ExternalUserView, MountedFeature, MountedHarnessDefinition, Never, NoTurnChannels, PomView,
        PromptComponent, ProviderCapabilityContract, ProviderToolSpec, RuntimeContract,
        StreamUpdate, StreamingXml, TurnChannelMap, TurnChannelMapBuilder, TurnChannels,
        TurnEmission, TurnLoopPolicy, UserTurnContext, UserView,
    };
    pub use crate::agent_view::AgentView;
    pub use crate::pom::{
        BlockBuilder, BlockChildren, BlockContent, CodeBlockNode, CodeSpanNode, ContentContext,
        ContentKind, ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, HeadingLevel,
        HeadingNode, InlineBuilder, InlineChildren, InlineContent, ListBuilder, ListItem, ListKind,
        ListNode, MarkdownKind, MarkdownNode, MixedBuilder, MixedChildren, MixedContent,
        ParagraphNode, PomError, ResolvedDocument, StrongNode, TextNode, XmlAttribute,
        XmlAttributes, XmlName, XmlNode,
    };
    pub use agentview_derive::{view, AgentView};
}

pub use channels::{Never, NoTurnChannels, TurnChannels, TurnEmission};
#[allow(unused_imports)]
pub(crate) use durable_epoch::{
    AttachedProviderEpoch, EpochArtifactFingerprint, EpochContractManifestError,
    ProviderAdapterContract, ProviderAdapterError, ProviderEpochAttachRequest,
    ProviderEpochReceipt, ProviderEpochRehydrateRequest, ProviderToolDescriptor,
    ProviderTurnCursor,
};
#[allow(unused_imports)]
pub(crate) use erasure::{ChannelTypeField, ChannelTypeInfo, ChannelTypeMismatch, TypeSlot};
pub use external::{ExternalActionRoute, ExternalReplyContract, ExternalReplyContractId};
pub use external_reply::{
    external_prompt_component, try_external_prompt_component, ExternalPromptComponent,
    ExternalReply, ExternalUserView, MountedExternalHarnessDefinition,
};
pub use identity::{
    BindingId, BindingKey, ComponentId, ComponentKey, HarnessEpochId, LiveScopeId,
    ProviderAttemptId, TurnInstanceId,
};
#[allow(unused_imports)]
pub(crate) use lifecycle::{
    mount_system_epoch, mount_system_epoch_with_contract, MountedEpoch, MountedTurn,
    PreparedUserTurn, SystemMountContext, SystemMountError, SystemView, UserTurnPlan,
};
pub use lifecycle::{system_view, user_view, UserTurnContext, UserView};
pub use local_mounted::{InMemoryMountedAgentFactory, InMemoryMountedFactoryError};
pub use mounted::{
    prompt_component, try_prompt_component, CapturedMountedHarnessDefinition,
    DurableEpochDefinition, MountedAgent, MountedCall, MountedCallCancellation, MountedCallInput,
    MountedCallInputError, MountedCallLifecycle, MountedCallLookupError, MountedCallReattachment,
    MountedCallRecoveryReason, MountedCallSnapshot, MountedFeature, MountedHarnessDefinition,
    MountedHostBindings, MountedOpenError, MountedReloadError, MountedStartError,
    MountedTurnCapture, MountedWaitError, PromptComponent, TurnCaptureContext, TurnLoopPolicy,
    UserTurnRenderer,
};
#[allow(unused_imports)]
pub(crate) use mounted_contract::{
    DurableCallLease, DurableCallLedger, DurableCallLedgerError, DurableCallRecoveryReason,
    DurableCallReservationOrigin, DurableCallState, DurableCallStatus, MountedSessionSnapshot,
};
pub use mounted_contract::{
    MountedCallOutcome, MountedCallResult, MountedTurnPublication, SessionReduceContext,
    SessionReducer, SessionReducerFailureDisposition, TurnRecord, TurnRecordParts,
};
pub use node::PromptRole;
#[allow(unused_imports)]
pub(crate) use provider::{
    FinishedProviderAttempt, MountedProviderAttempt, ProviderAttemptAbortReport,
    ProviderAttemptFinishFailure, ProviderAttemptStartError, ProviderDispatchFault,
    ProviderDispatchOrigin, ProviderDispatchPhase, ProviderDispatcherAbort,
    ProviderDispatcherAbortOutcome, ProviderInvocationCollision, ProviderLiveEffectFault,
    ProviderPublishFailure, ProviderToolAttemptError, ProviderToolCallOutcome,
    PublishedProviderAttempt,
};
#[allow(unused_imports)]
pub(crate) use provider_wire::{
    DurableMountedProviderExecutor, DurableMountedProviderOperationController,
    FallibleProviderWirePort, MountedProviderEpoch, MountedProviderEpochAttacher,
    MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
    ProviderCancellationCursorDisposition, ProviderCancellationReason, ProviderCancellationSource,
    ProviderCancellationToken, ProviderOperationStatus, ProviderToolCatalog, ProviderWireAck,
    ProviderWireEvent, ProviderWireFault,
};
pub use provision::{
    binding_factory, binding_factory_with_context, component, durable_binding_factory,
    durable_binding_factory_with_context, durable_binding_factory_with_context_key,
    durable_binding_factory_with_key, durable_provider_contract, durable_provider_tool,
    durable_provider_tool_with_context, durable_provider_tool_with_context_key,
    durable_provider_tool_with_key, durable_provider_tools, durable_provider_tools_with_context,
    durable_provider_tools_with_context_key, durable_provider_tools_with_key, provider_tool,
    provider_tool_with_context, provider_tools, provider_tools_with_context, BindingAbortAck,
    BindingAbortReason, BindingFailure, BindingFault, BindingInstance, BindingOrigin, BindingPhase,
    Component, DurableComponent, DurableSystem, ProviderAttemptIdentity, ProviderCallIdentity,
    ProviderCapabilityContract, ProviderDispatchContext, ProviderDispatchFailure,
    ProviderDispatchUpdate, ProviderDispatcher, ProviderDispatcherAbortAck,
    ProviderDispatcherAbortContext, ProviderDispatcherCx, ProviderDispatcherRegistry,
    ProviderDispatcherRegistryError, ProviderInvocationKey, ProviderToolCall, ProviderToolResponse,
    ProviderToolResult, ProviderToolSpec, RuntimeContract, RuntimeRoute, TurnBindingCx,
};
#[allow(unused_imports)]
pub(crate) use publication::{
    stage_commit_outbox, CommitContract, CommitStager, CommitStagingContext, CommitStagingFailure,
    DurableCallLeaseId, DurablyPublishedProviderAttempt, DurablyPublishedStreamingAttempt,
    OutboxItemId, PreparedSessionMutation, ProviderOperationIdentity,
    ProviderPublicationStagingFailure, ProviderPublicationStagingResult, PublicationAttempt,
    PublicationAttemptError, PublicationCandidateError, PublicationCandidateFingerprint,
    PublicationFingerprintContext, PublicationFingerprintFactory, PublicationPhase,
    PublicationPhaseError, PublicationReceipt, PublicationReceiptMismatch, PublicationRequest,
    PublicationRequestContext, PublicationRequestId, PublicationRequestIdFactory,
    PublicationResolution, PublicationResolveError, PublicationStagingFailure,
    PublicationStagingPlan, PublicationStore, PublicationWriteError, StagedCommit, StagedOutbox,
    StagedOutboxItem, StagedProviderPublication, StagedPublication, StagedStreamingPublication,
    StreamingPublicationStagingFailure, StreamingPublicationStagingResult,
};
pub use publication::{
    DurableCallId, DurableCallInputId, DurableEpochId, DurableKeyError, DurableSessionId,
    EpochContractId, PublicationId,
};
pub use streaming::{
    ChannelMap, IntoStreamUpdate, LiveAbortContext, LiveAbortFault, LiveAbortOutcome,
    LiveEffectAbortAck, LiveEffectContext, LiveEffectFailure, LiveEffectFault, LiveEffectRuntime,
    MissingChannelMapLane, NoLiveEffects, StreamUpdate, StreamingXml, StreamingXmlFactoryReducer,
    TurnChannelMap, TurnChannelMapBuilder,
};

use crate::StorageString;

pub(crate) use compile::{compile_component, HookPlan};
pub(crate) use experimental::{binding, system, user, IntoComponentNode, View, ViewExt};
pub(crate) use node::ComponentNode;
#[cfg(test)]
pub(crate) use provision::RuntimeBindingRegistryError;
pub(crate) use provision::{
    compile_durable_mount_provided, compile_mount_provided, BindingFactoryPlan, MountPlanError,
    ProviderCapabilityPlan, RuntimeBindingRegistry,
};
pub(crate) use streaming::{
    FinishedStreamingAttempt, MountedStreamingAttempt, PublishedStreamingAttempt,
    PublishedTurnReceipt, StreamingAbortReport, StreamingAttemptError, StreamingAttemptStartError,
    StreamingBinding, StreamingChannelsView, StreamingComponentError, StreamingComponentSink,
    StreamingFinishFailure, StreamingOutcome, StreamingValueView, TurnPublication, TurnPublisher,
};

/// Error produced while authoring or compiling a component tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComponentError {
    #[error(transparent)]
    Pom(#[from] crate::pom::PomError),

    #[error("invalid {kind} key `{value}`; keys must use ASCII letters, digits, `_`, `-`, or `.`")]
    InvalidKey { kind: &'static str, value: String },

    #[error("a key can only be attached to a component boundary")]
    KeyRequiresComponent,

    #[error("the component invocation already has a key")]
    ComponentAlreadyKeyed,

    #[error("component `{component}` emitted POM outside a system or user placement")]
    UnplacedPom { component: ComponentId },

    #[error(
        "component `{component}` nests a {inner} placement inside an existing {outer} placement"
    )]
    NestedRole {
        component: ComponentId,
        outer: PromptRole,
        inner: PromptRole,
    },

    #[error("component `{parent}` has duplicate child key `{key}`")]
    DuplicateComponentKey {
        parent: ComponentId,
        key: ComponentKey,
    },

    #[error("component `{component}` has duplicate binding key `{key}`")]
    DuplicateBindingKey {
        component: ComponentId,
        key: BindingKey,
    },

    #[error("invalid runtime binding contract: {message}")]
    InvalidBindingContract { message: String },

    #[error("POM-only component plan contains {count} runtime binding(s)")]
    UnexpectedRuntimeBindings { count: usize },

    #[error(
        "component call requires props `{expected}`; configure the turn with `.with_props(...)`"
    )]
    MissingCallProps { expected: &'static str },
}

/// Uninhabited marker proving that a component tree has no runtime bindings.
#[derive(Debug)]
pub enum NoBindings {}

// These traits deliberately describe the authoring vocabulary rather than the
// raw component IR. Their implementations stay closed so a normal component
// body cannot accidentally become a compatibility escape hatch.
mod authoring_sealed {
    pub trait PomChildren {}

    pub trait ComponentChildren<C, Props: ?Sized + 'static> {}

    pub trait DurableSystemChildren<C, Props: ?Sized + 'static> {}
}

/// Values that can be composed into a POM-only prompt fragment.
///
/// This is a sealed structural input accepted by [`view`], [`pom_view`], and
/// [`user_view`]. Component authors normally use `Document`, [`PomView`],
/// `Option`, `Vec`, arrays, and tuples without naming this trait directly.
/// Raw compatibility views are available only with the non-default
/// `raw-component-ir` feature.
pub trait PomChildren: authoring_sealed::PomChildren {
    #[doc(hidden)]
    fn into_pom_view(self) -> PomView;
}

/// Values that can be composed into one mountable [`Component`].
///
/// This is the System-side counterpart to [`PomChildren`]. It accepts POM
/// fragments and already-declared components, keeping runtime declarations
/// inside their matching provided component rather than exposing the raw IR in
/// the ordinary authoring API.
pub trait ComponentChildren<C, Props: ?Sized + 'static>:
    authoring_sealed::ComponentChildren<C, Props>
where
    C: TurnChannels,
{
    #[doc(hidden)]
    fn into_component(self) -> Component<C, Props>;
}

/// Ordered children accepted by [`durable_system`].
///
/// Only POM-only values, durable leaves, and existing durable System trees are
/// accepted. Ordinary runtime-bearing [`Component`] values are intentionally
/// excluded so a reopen contract cannot be erased or duplicated implicitly.
pub trait DurableSystemChildren<C, Props: ?Sized + 'static>:
    authoring_sealed::DurableSystemChildren<C, Props>
where
    C: TurnChannels,
{
    #[doc(hidden)]
    fn into_durable_system(self) -> DurableSystem<C, Props>;
}

/// A component view that can only contribute POM.
///
/// `PomView` is role agnostic. Its parent decides whether the POM is placed in
/// the system or user document, and it can be safely composed with any root
/// binding type because it cannot contain a runtime binding.
#[derive(Debug)]
pub struct PomView(experimental::View<NoBindings>);

impl PomView {
    pub(crate) fn from_error(error: ComponentError) -> Self {
        Self(Err(error))
    }

    /// Attach a stable parent-provided key to this component invocation.
    pub fn key(self, key: impl Into<StorageString>) -> Self {
        Self(self.0.and_then(|node| node.with_key(key)))
    }

    /// Wrap this fragment in a retained feature component boundary.
    ///
    /// This is crate-private because only the mounted feature authoring path
    /// has a matching durable System projection for the same scope.
    pub(crate) fn with_component_scope(
        self,
        name: impl Into<StorageString>,
        key: Option<ComponentKey>,
    ) -> Self {
        Self(
            self.0
                .map(|child| ComponentNode::component(name, key, child)),
        )
    }

    fn into_view<B>(self) -> experimental::View<B>
    where
        B: 'static,
    {
        self.0
            .map(|node| node.map_binding(|binding| match binding {}))
    }
}

fn empty_pom_view() -> PomView {
    PomView(Ok(ComponentNode::empty()))
}

fn fragment_pom_views(children: impl IntoIterator<Item = PomView>) -> PomView {
    PomView(
        children
            .into_iter()
            .map(PomView::into_view::<NoBindings>)
            .collect::<Result<Vec<_>, _>>()
            .map(ComponentNode::fragment),
    )
}

impl authoring_sealed::PomChildren for PomView {}

impl PomChildren for PomView {
    fn into_pom_view(self) -> PomView {
        self
    }
}

impl authoring_sealed::PomChildren for crate::pom::Document {}

impl PomChildren for crate::pom::Document {
    fn into_pom_view(self) -> PomView {
        PomView(Ok(ComponentNode::pom(self)))
    }
}

impl authoring_sealed::PomChildren for () {}

impl PomChildren for () {
    fn into_pom_view(self) -> PomView {
        empty_pom_view()
    }
}

impl authoring_sealed::PomChildren for experimental::View<NoBindings> {}

impl PomChildren for experimental::View<NoBindings> {
    fn into_pom_view(self) -> PomView {
        PomView(self)
    }
}

impl<T> authoring_sealed::PomChildren for Option<T> where T: PomChildren {}

impl<T> PomChildren for Option<T>
where
    T: PomChildren,
{
    fn into_pom_view(self) -> PomView {
        self.map(PomChildren::into_pom_view)
            .unwrap_or_else(empty_pom_view)
    }
}

impl<T> authoring_sealed::PomChildren for Vec<T> where T: PomChildren {}

impl<T> PomChildren for Vec<T>
where
    T: PomChildren,
{
    fn into_pom_view(self) -> PomView {
        fragment_pom_views(self.into_iter().map(PomChildren::into_pom_view))
    }
}

impl<T, const N: usize> authoring_sealed::PomChildren for [T; N] where T: PomChildren {}

impl<T, const N: usize> PomChildren for [T; N]
where
    T: PomChildren,
{
    fn into_pom_view(self) -> PomView {
        fragment_pom_views(self.into_iter().map(PomChildren::into_pom_view))
    }
}

macro_rules! impl_pom_children_tuple {
    ($($name:ident),+ $(,)?) => {
        impl<$($name),+> authoring_sealed::PomChildren for ($($name,)+)
        where
            $($name: PomChildren,)+
        {}

        impl<$($name),+> PomChildren for ($($name,)+)
        where
            $($name: PomChildren,)+
        {
            #[allow(non_snake_case)]
            fn into_pom_view(self) -> PomView {
                let ($($name,)+) = self;
                fragment_pom_views([$($name.into_pom_view(),)+])
            }
        }
    };
}

impl_pom_children_tuple!(A);
impl_pom_children_tuple!(A, B);
impl_pom_children_tuple!(A, B, C);
impl_pom_children_tuple!(A, B, C, D);
impl_pom_children_tuple!(A, B, C, D, E);
impl_pom_children_tuple!(A, B, C, D, E, F);
impl_pom_children_tuple!(A, B, C, D, E, F, G);
impl_pom_children_tuple!(A, B, C, D, E, F, G, H);
impl_pom_children_tuple!(A, B, C, D, E, F, G, H, I);
impl_pom_children_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_pom_children_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_pom_children_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

fn empty_component<C, Props>() -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    Component::from_view(Ok(ComponentNode::empty()))
}

fn fragment_components<C, Props>(
    children: impl IntoIterator<Item = Component<C, Props>>,
) -> Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    Component::from_view(
        children
            .into_iter()
            .map(Component::into_mount_provided_view)
            .collect::<Result<Vec<_>, _>>()
            .map(ComponentNode::fragment),
    )
}

impl<C, Props> authoring_sealed::ComponentChildren<C, Props> for Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> ComponentChildren<C, Props> for Component<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component(self) -> Component<C, Props> {
        self
    }
}

impl<C, Props> authoring_sealed::ComponentChildren<C, Props> for PomView
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> ComponentChildren<C, Props> for PomView
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component(self) -> Component<C, Props> {
        Component::from_view(self.into_view())
    }
}

impl<C, Props> authoring_sealed::ComponentChildren<C, Props> for crate::pom::Document
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> ComponentChildren<C, Props> for crate::pom::Document
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component(self) -> Component<C, Props> {
        Component::from_view(Ok(ComponentNode::pom(self)))
    }
}

impl<C, Props> authoring_sealed::ComponentChildren<C, Props> for ()
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> ComponentChildren<C, Props> for ()
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component(self) -> Component<C, Props> {
        empty_component()
    }
}

impl<C, Props> authoring_sealed::ComponentChildren<C, Props>
    for experimental::View<provision::RuntimeDeclaration<C, Props>>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> ComponentChildren<C, Props>
    for experimental::View<provision::RuntimeDeclaration<C, Props>>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_component(self) -> Component<C, Props> {
        Component::from_view(self)
    }
}

impl<C, Props, T> authoring_sealed::ComponentChildren<C, Props> for Option<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: ComponentChildren<C, Props>,
{
}

impl<C, Props, T> ComponentChildren<C, Props> for Option<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: ComponentChildren<C, Props>,
{
    fn into_component(self) -> Component<C, Props> {
        self.map(ComponentChildren::into_component)
            .unwrap_or_else(empty_component)
    }
}

impl<C, Props, T> authoring_sealed::ComponentChildren<C, Props> for Vec<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: ComponentChildren<C, Props>,
{
}

impl<C, Props, T> ComponentChildren<C, Props> for Vec<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: ComponentChildren<C, Props>,
{
    fn into_component(self) -> Component<C, Props> {
        fragment_components(self.into_iter().map(ComponentChildren::into_component))
    }
}

impl<C, Props, T, const N: usize> authoring_sealed::ComponentChildren<C, Props> for [T; N]
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: ComponentChildren<C, Props>,
{
}

impl<C, Props, T, const N: usize> ComponentChildren<C, Props> for [T; N]
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: ComponentChildren<C, Props>,
{
    fn into_component(self) -> Component<C, Props> {
        fragment_components(self.into_iter().map(ComponentChildren::into_component))
    }
}

macro_rules! impl_component_children_tuple {
    ($($name:ident),+ $(,)?) => {
        impl<Root, TurnProps, $($name),+> authoring_sealed::ComponentChildren<Root, TurnProps> for ($($name,)+)
        where
            Root: TurnChannels,
            TurnProps: ?Sized + 'static,
            $($name: ComponentChildren<Root, TurnProps>,)+
        {}

        impl<Root, TurnProps, $($name),+> ComponentChildren<Root, TurnProps> for ($($name,)+)
        where
            Root: TurnChannels,
            TurnProps: ?Sized + 'static,
            $($name: ComponentChildren<Root, TurnProps>,)+
        {
            #[allow(non_snake_case)]
            fn into_component(self) -> Component<Root, TurnProps> {
                let ($($name,)+) = self;
                fragment_components([$($name.into_component(),)+])
            }
        }
    };
}

impl_component_children_tuple!(A);
impl_component_children_tuple!(A, B);
impl_component_children_tuple!(A, B, C);
impl_component_children_tuple!(A, B, C, D);
impl_component_children_tuple!(A, B, C, D, E);
impl_component_children_tuple!(A, B, C, D, E, F);
impl_component_children_tuple!(A, B, C, D, E, F, G);
impl_component_children_tuple!(A, B, C, D, E, F, G, H);
impl_component_children_tuple!(A, B, C, D, E, F, G, H, I);
impl_component_children_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_component_children_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_component_children_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

fn empty_durable_system<C, Props>() -> DurableSystem<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    DurableSystem::empty()
}

fn fragment_durable_systems<C, Props>(
    children: impl IntoIterator<Item = DurableSystem<C, Props>>,
) -> DurableSystem<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    children
        .into_iter()
        .fold(empty_durable_system(), DurableSystem::append)
}

impl<C, Props> authoring_sealed::DurableSystemChildren<C, Props> for DurableSystem<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> DurableSystemChildren<C, Props> for DurableSystem<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        self
    }
}

impl<C, Props> authoring_sealed::DurableSystemChildren<C, Props> for DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> DurableSystemChildren<C, Props> for DurableComponent<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        DurableSystem::from_component(self)
    }
}

impl<C, Props> authoring_sealed::DurableSystemChildren<C, Props> for PomView
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> DurableSystemChildren<C, Props> for PomView
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        DurableSystem::prompt(self)
    }
}

impl<C, Props> authoring_sealed::DurableSystemChildren<C, Props> for crate::pom::Document
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> DurableSystemChildren<C, Props> for crate::pom::Document
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        DurableSystem::prompt(self.into_pom_view())
    }
}

impl<C, Props> authoring_sealed::DurableSystemChildren<C, Props> for ()
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
}

impl<C, Props> DurableSystemChildren<C, Props> for ()
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        empty_durable_system()
    }
}

impl<C, Props, T> authoring_sealed::DurableSystemChildren<C, Props> for Option<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: DurableSystemChildren<C, Props>,
{
}

impl<C, Props, T> DurableSystemChildren<C, Props> for Option<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: DurableSystemChildren<C, Props>,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        self.map(DurableSystemChildren::into_durable_system)
            .unwrap_or_else(empty_durable_system)
    }
}

impl<C, Props, T> authoring_sealed::DurableSystemChildren<C, Props> for Vec<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: DurableSystemChildren<C, Props>,
{
}

impl<C, Props, T> DurableSystemChildren<C, Props> for Vec<T>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: DurableSystemChildren<C, Props>,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        fragment_durable_systems(
            self.into_iter()
                .map(DurableSystemChildren::into_durable_system),
        )
    }
}

impl<C, Props, T, const N: usize> authoring_sealed::DurableSystemChildren<C, Props> for [T; N]
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: DurableSystemChildren<C, Props>,
{
}

impl<C, Props, T, const N: usize> DurableSystemChildren<C, Props> for [T; N]
where
    C: TurnChannels,
    Props: ?Sized + 'static,
    T: DurableSystemChildren<C, Props>,
{
    fn into_durable_system(self) -> DurableSystem<C, Props> {
        fragment_durable_systems(
            self.into_iter()
                .map(DurableSystemChildren::into_durable_system),
        )
    }
}

macro_rules! impl_durable_system_children_tuple {
    ($($name:ident),+ $(,)?) => {
        impl<Root, TurnProps, $($name),+>
            authoring_sealed::DurableSystemChildren<Root, TurnProps> for ($($name,)+)
        where
            Root: TurnChannels,
            TurnProps: ?Sized + 'static,
            $($name: DurableSystemChildren<Root, TurnProps>,)+
        {}

        impl<Root, TurnProps, $($name),+> DurableSystemChildren<Root, TurnProps>
            for ($($name,)+)
        where
            Root: TurnChannels,
            TurnProps: ?Sized + 'static,
            $($name: DurableSystemChildren<Root, TurnProps>,)+
        {
            #[allow(non_snake_case)]
            fn into_durable_system(self) -> DurableSystem<Root, TurnProps> {
                let ($($name,)+) = self;
                fragment_durable_systems([$($name.into_durable_system(),)+])
            }
        }
    };
}

impl_durable_system_children_tuple!(A);
impl_durable_system_children_tuple!(A, B);
impl_durable_system_children_tuple!(A, B, C);
impl_durable_system_children_tuple!(A, B, C, D);
impl_durable_system_children_tuple!(A, B, C, D, E);
impl_durable_system_children_tuple!(A, B, C, D, E, F);
impl_durable_system_children_tuple!(A, B, C, D, E, F, G);
impl_durable_system_children_tuple!(A, B, C, D, E, F, G, H);
impl_durable_system_children_tuple!(A, B, C, D, E, F, G, H, I);
impl_durable_system_children_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_durable_system_children_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_durable_system_children_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

/// Compose one ordered durable System definition.
///
/// Calling this function constructs a pure retained definition. POM
/// compilation, resolution, rendering, persistence, and provider attachment
/// still occur only if durable admission selects `Create`.
pub fn durable_system<C, Props>(
    children: impl DurableSystemChildren<C, Props>,
) -> DurableSystem<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    children.into_durable_system()
}

/// Compose POM-only children into one role-agnostic prompt fragment.
///
/// Runtime-bearing children belong under [`component`], while the lifecycle
/// root chooses whether this POM appears in the System or User document.
pub fn view(children: impl PomChildren) -> PomView {
    children.into_pom_view()
}

/// Explicit POM-only spelling for ordinary helpers outside a component body.
pub fn pom_view(children: impl PomChildren) -> PomView {
    children.into_pom_view()
}

#[doc(hidden)]
pub mod __private {
    use super::{
        Component, ComponentError, ComponentNode, DurableComponent, DurableSystem,
        ExternalPromptComponent, MountedFeature, NoBindings, PomView, StreamingChannelsView,
        StreamingValueView, TurnChannels, View,
    };
    pub trait DeferComponentBody<Body>: Sized {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> Body + Send + 'static,
        ) -> Self;
    }

    impl<B> DeferComponentBody<View<B>> for View<B>
    where
        B: 'static,
    {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> View<B> + Send + 'static,
        ) -> Self {
            Ok(ComponentNode::deferred_component(name, render))
        }
    }

    impl DeferComponentBody<View<NoBindings>> for PomView {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> View<NoBindings> + Send + 'static,
        ) -> Self {
            PomView(Ok(ComponentNode::deferred_component(name, render)))
        }
    }

    impl DeferComponentBody<PomView> for PomView {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> PomView + Send + 'static,
        ) -> Self {
            PomView(Ok(ComponentNode::deferred_component(name, move || {
                render().into_view()
            })))
        }
    }

    impl<C, Props> DeferComponentBody<Component<C, Props>> for Component<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> Component<C, Props> + Send + 'static,
        ) -> Self {
            Component::from_view(Ok(ComponentNode::deferred_component(name, move || {
                render().into_mount_provided_view()
            })))
        }
    }

    impl<C> DeferComponentBody<StreamingChannelsView<C>> for StreamingChannelsView<C>
    where
        C: TurnChannels,
    {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> StreamingChannelsView<C> + Send + 'static,
        ) -> Self {
            StreamingChannelsView::deferred(name, render)
        }
    }

    impl<E, D> DeferComponentBody<StreamingValueView<E, D>> for StreamingValueView<E, D>
    where
        E: 'static,
        D: 'static,
    {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> StreamingValueView<E, D> + Send + 'static,
        ) -> Self {
            StreamingValueView::deferred(name, render)
        }
    }

    impl<E, D> DeferComponentBody<View<super::StreamingBinding<E, D>>> for StreamingValueView<E, D>
    where
        E: 'static,
        D: 'static,
    {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> View<super::StreamingBinding<E, D>> + Send + 'static,
        ) -> Self {
            StreamingValueView::deferred(name, move || StreamingValueView::from_view(render()))
        }
    }

    impl<C> DeferComponentBody<View<super::StreamingBinding<super::TurnEmission<C>, C::Diagnostic>>>
        for StreamingChannelsView<C>
    where
        C: TurnChannels,
    {
        fn defer_component(
            name: &'static str,
            render: impl FnOnce() -> View<super::StreamingBinding<super::TurnEmission<C>, C::Diagnostic>>
                + Send
                + 'static,
        ) -> Self {
            StreamingChannelsView::deferred(
                name,
                move || StreamingChannelsView::from_view(render()),
            )
        }
    }

    pub fn defer_component<Output, Body>(
        name: &'static str,
        render: impl FnOnce() -> Body + Send + 'static,
    ) -> Output
    where
        Output: DeferComponentBody<Body>,
    {
        Output::defer_component(name, render)
    }

    pub trait DeferFallibleComponentBody<Body>: Sized {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<Body, ComponentError> + Send + 'static,
        ) -> Self;
    }

    impl<B> DeferFallibleComponentBody<View<B>> for View<B>
    where
        B: 'static,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<View<B>, ComponentError> + Send + 'static,
        ) -> Self {
            Ok(ComponentNode::deferred_component(name, move || {
                render().and_then(|view| view)
            }))
        }
    }

    impl DeferFallibleComponentBody<View<NoBindings>> for PomView {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<View<NoBindings>, ComponentError> + Send + 'static,
        ) -> Self {
            PomView(Ok(ComponentNode::deferred_component(name, move || {
                render().and_then(|view| view)
            })))
        }
    }

    impl DeferFallibleComponentBody<PomView> for PomView {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<PomView, ComponentError> + Send + 'static,
        ) -> Self {
            PomView(Ok(ComponentNode::deferred_component(name, move || {
                render().and_then(PomView::into_view)
            })))
        }
    }

    impl<C, Props> DeferFallibleComponentBody<Component<C, Props>> for Component<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<Component<C, Props>, ComponentError> + Send + 'static,
        ) -> Self {
            Component::from_view(Ok(ComponentNode::deferred_component(name, move || {
                render().and_then(Component::into_mount_provided_view)
            })))
        }
    }

    impl<C, Props> DeferFallibleComponentBody<DurableComponent<C, Props>> for DurableComponent<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        fn defer_fallible_component(
            _name: &'static str,
            render: impl FnOnce() -> Result<DurableComponent<C, Props>, ComponentError> + Send + 'static,
        ) -> Self {
            render().unwrap_or_else(DurableComponent::from_error)
        }
    }

    impl<C, Props> DeferFallibleComponentBody<DurableSystem<C, Props>> for DurableSystem<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        fn defer_fallible_component(
            _name: &'static str,
            render: impl FnOnce() -> Result<DurableSystem<C, Props>, ComponentError> + Send + 'static,
        ) -> Self {
            render().unwrap_or_else(DurableSystem::from_error)
        }
    }

    impl<C, Props> DeferFallibleComponentBody<MountedFeature<C, Props>> for MountedFeature<C, Props>
    where
        C: TurnChannels,
        Props: ?Sized + 'static,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<MountedFeature<C, Props>, ComponentError> + Send + 'static,
        ) -> Self {
            render()
                .map(|feature| feature.with_component_scope(name))
                .unwrap_or_else(|error| {
                    MountedFeature::system_only(DurableSystem::from_error(error))
                        .with_component_scope(name)
                })
        }
    }

    impl<Props> DeferFallibleComponentBody<ExternalPromptComponent<Props>>
        for ExternalPromptComponent<Props>
    where
        Props: ?Sized + 'static,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<ExternalPromptComponent<Props>, ComponentError>
                + Send
                + 'static,
        ) -> Self {
            render()
                .map(|component| component.with_component_scope(name))
                .unwrap_or_else(ExternalPromptComponent::from_error)
        }
    }

    impl<C> DeferFallibleComponentBody<StreamingChannelsView<C>> for StreamingChannelsView<C>
    where
        C: TurnChannels,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<StreamingChannelsView<C>, ComponentError> + Send + 'static,
        ) -> Self {
            StreamingChannelsView::deferred(name, move || match render() {
                Ok(view) => view,
                Err(error) => StreamingChannelsView::from_view(Err(error)),
            })
        }
    }

    impl<E, D> DeferFallibleComponentBody<StreamingValueView<E, D>> for StreamingValueView<E, D>
    where
        E: 'static,
        D: 'static,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<StreamingValueView<E, D>, ComponentError> + Send + 'static,
        ) -> Self {
            StreamingValueView::deferred(name, move || match render() {
                Ok(view) => view,
                Err(error) => StreamingValueView::from_view(Err(error)),
            })
        }
    }

    impl<E, D> DeferFallibleComponentBody<View<super::StreamingBinding<E, D>>>
        for StreamingValueView<E, D>
    where
        E: 'static,
        D: 'static,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<View<super::StreamingBinding<E, D>>, ComponentError>
                + Send
                + 'static,
        ) -> Self {
            StreamingValueView::deferred(name, move || {
                StreamingValueView::from_view(render().and_then(|view| view))
            })
        }
    }

    impl<C>
        DeferFallibleComponentBody<
            View<super::StreamingBinding<super::TurnEmission<C>, C::Diagnostic>>,
        > for StreamingChannelsView<C>
    where
        C: TurnChannels,
    {
        fn defer_fallible_component(
            name: &'static str,
            render: impl FnOnce() -> Result<
                    View<super::StreamingBinding<super::TurnEmission<C>, C::Diagnostic>>,
                    ComponentError,
                > + Send
                + 'static,
        ) -> Self {
            StreamingChannelsView::deferred(name, move || {
                StreamingChannelsView::from_view(render().and_then(|view| view))
            })
        }
    }

    pub fn defer_fallible_component<Output, Body>(
        name: &'static str,
        render: impl FnOnce() -> Result<Body, ComponentError> + Send + 'static,
    ) -> Output
    where
        Output: DeferFallibleComponentBody<Body>,
    {
        Output::defer_fallible_component(name, render)
    }

    pub fn into_component_prop<T>(value: T) -> T
    where
        T: Send + 'static,
    {
        value
    }
}
