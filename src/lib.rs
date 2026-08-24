//! `agentview` is an AI/AX runtime: Agent Interface plus Agent Experience for
//! language agents.
//!
//! At its core, `agentview` treats agent interaction as a ViewModel problem. It
//! renders structured views from application state, computes diffs between
//! turns, routes stream and tool events into stateful sinks, validates typed
//! actions, and commits successful turns into history.
//!
//! ## API Layers
//!
//! - [`component::prelude`] is the recommended import path for authoring POM
//!   components.
//! - [`component::execution`] contains the Host-owned application lifecycle.
//! - [`prelude`] contains the original ViewModel runtime.
//! - Module paths such as [`agent`], [`llm_call`], [`templates`], and
//!   [`streaming_tool`] remain public as advanced APIs and escape hatches while
//!   the crate is still evolving.
//! - [`llm_call::AgentTurn`] is the lower-level transaction primitive used by
//!   [`agent::Agent`]. Most applications should start from [`agent::Agent`].
//! - [`stream_parser`] is a reusable Hermes-style XML streaming parser utility.
//!
//! The current policy is to keep modules public during iteration. The stable
//! mental model is the `prelude`; visibility hardening can happen after more
//! real adapter usage.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use agentview::component::prelude::*;
//! ```
//!
#![deny(clippy::disallowed_types)]

extern crate self as agentview;

pub mod agent;
pub mod agent_session;
pub mod agent_view;
pub mod component;
pub mod control;
pub mod llm_call;
pub mod pom;
pub mod pom_cursor;
mod pom_diff;
pub mod pom_renderer;
pub mod pom_resolution;
pub mod prompt_context;
pub mod provider;
pub mod record_store;
mod semantic_diff;
pub mod semantic_view;
pub mod stream_parser;
pub mod streaming_tool;
pub mod templates;
pub mod transcript;
pub mod view_app;
pub mod view_awake;
pub mod view_state;

pub type StorageString = ecow::EcoString;

pub use agent_view::AgentView;
pub use agentview_derive::AgentView;

/// Common imports for building an agent-facing ViewModel runtime.
pub mod prelude {
    pub use crate::agent::{
        Agent, AgentTurnBuilder, AgentViewModel, DefaultAgentViewModel, TextAgent, TurnFlow,
    };
    pub use crate::agent_session::AgentSession;
    pub use crate::agent_view::AgentView;
    pub use crate::control::ControlReply;
    pub use crate::llm_call::{
        AgentTurnEvent, AgentTurnObserver, AgentTurnObserverHandle, AgentTurnOutcome,
        AgentTurnRequest, ContextPreparation, ContextPreparationBudget, ExecutorCommit,
        LLMExecutor, NoopTurnSink, TextTurnEvent, TurnSink,
    };
    pub use crate::pom::{
        BlockBuilder, BlockChildren, BlockContent, CodeBlockNode, CodeSpanNode, ContentContext,
        ContentKind, ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, HeadingLevel,
        HeadingNode, InlineBuilder, InlineChildren, InlineContent, ListBuilder, ListItem, ListKind,
        ListNode, MarkdownKind, MarkdownNode, MixedBuilder, MixedChildren, MixedContent,
        ParagraphNode, PomError, ResolvedDocument, StrongNode, TextNode, XmlAttribute,
        XmlAttributes, XmlName, XmlNode,
    };
    pub use crate::pom_cursor::UserDocumentCursor;
    pub use crate::pom_renderer::{render_pom_document, PomRenderError};
    pub use crate::pom_resolution::{
        resolve_artifact_document, resolve_system_document, resolve_user_document,
        PomResolutionError,
    };
    pub use crate::prompt_context::{
        AgentTurnError, IdentityTransform, PromptContext, Role, Turn, TurnTransform,
    };
    pub use crate::semantic_view::{
        render_agent_view_diff_xml, render_agent_view_xml, render_semantic_fragment_xml,
        render_semantic_node_xml, AgentView as LegacyAgentView, AgentViewCollect, AgentViewRoot,
        SemanticField, SemanticFragment, SemanticNode,
    };
    pub use crate::stream_parser::{HermesParser, XmlElement};
    pub use crate::streaming_tool::{
        ParseContext, StreamingTool, StreamingToolError, StreamingToolRegistrationError,
        StreamingToolRunner,
    };
    pub use crate::templates::{ContextViewBuilder, TurnArtifact, TurnArtifactError};
    pub use crate::view_app::{AgentViewApp, AgentViewAppError};
    pub use crate::view_awake::{ViewAwake, ViewAwakeHandle, ViewAwakeSubscription, ViewEpoch};
    pub use crate::view_state::{ViewPatch, ViewSnapshot, ViewTurnId, ViewUpdate, ViewUpdateBody};
    pub use crate::StorageString;
    pub use agentview_derive::AgentView;
}
