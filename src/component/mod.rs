//! Functional Component authoring and application execution.
//!
//! `authoring` lowers `#[component]` and `view!` into a render-local
//! declaration tree. `execution` is the only Host lifecycle for that tree.

pub mod authoring;
pub mod execution;
mod host;

mod identity;
mod signal;
mod task;

pub use host::{ComponentHost, ComponentHostFault, ComponentHostId, PreparedRender};
pub(crate) use identity::ComponentId;
pub use signal::SignalAccessError;

/// Curated imports for Component authors.
pub mod prelude {
    pub use super::authoring::streaming_attempt::*;
    pub use super::authoring::{
        spawn, use_coroutine, use_future, use_preparation, use_provider_event_handler,
        use_reaction_completion, use_reaction_request, use_signal, Component, Coroutine,
        CoroutineInbox, CoroutineSendError, NativeToolCall, ProviderEventSelector, ReactionRequest,
        ReactionRequestError, Signal, SpawnError, StreamingXml, StreamingXmlTag,
        XmlContractDiagnostic, XmlStreamingToolCall,
    };
    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "the compatibility prelude intentionally re-exports legacy event authoring aliases"
    )]
    #[deprecated(note = "use `use_provider_event_handler` for provider event routing")]
    pub use super::authoring::{EventInput, EventListener};
    pub use super::execution::ProviderEvent;
    pub use super::SignalAccessError;
    pub use crate::llm_call::TextTurnEvent;
    pub use crate::pom::{
        BlockBuilder, BlockChildren, BlockContent, CodeBlockNode, CodeSpanNode, ContentContext,
        ContentKind, ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, HeadingLevel,
        HeadingNode, InlineBuilder, InlineChildren, InlineContent, ListBuilder, ListItem, ListKind,
        ListNode, MarkdownKind, MarkdownNode, MixedBuilder, MixedChildren, MixedContent,
        ParagraphNode, PomError, ResolvedDocument, StrongNode, TextNode, XmlAttribute,
        XmlAttributes, XmlName, XmlNode,
    };
    pub use crate::stream_parser::XmlElement;
    #[cfg(feature = "legacy-provider-port")]
    #[deprecated(note = "use `use_provider_event_handler` for provider event routing")]
    pub use agentview_derive::ComponentEvents;
    pub use agentview_derive::{component, view, AgentView};
}
