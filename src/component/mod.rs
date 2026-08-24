//! Functional Component authoring and application execution.
//!
//! `authoring` lowers `#[component]` and `view!` into a render-local
//! declaration tree. `execution` is the only Host lifecycle for that tree.

pub mod authoring;
pub mod execution;
mod host;

mod identity;
mod signal;

pub use host::{ComponentHost, ComponentHostFault, ComponentHostId, PreparedRender};
pub(crate) use identity::ComponentId;
pub use signal::SignalAccessError;

/// Curated imports for Component authors.
pub mod prelude {
    pub use super::authoring::{
        use_signal, Component, EventInput, EventListener, NativeToolCall, Signal,
        XmlContractDiagnostic, XmlStreamingToolCall,
    };
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
    pub use agentview_derive::{component, view, AgentView, ComponentEvents};
}
