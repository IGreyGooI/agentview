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
//! - [`prelude`] is the recommended import path for building ordinary
//!   agent-facing runtimes.
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
//! use agentview::prelude::*;
//! ```
//!
#![deny(clippy::disallowed_types)]

pub mod agent;
pub mod llm_call;
pub mod prompt_context;
pub mod stream_parser;
pub mod streaming_tool;
pub mod templates;

pub type StorageString = ecow::EcoString;

/// Common imports for building an agent-facing ViewModel runtime.
pub mod prelude {
    pub use crate::agent::{
        Agent, AgentTurnBuilder, AgentViewModel, DefaultAgentViewModel, TextAgent, TurnFlow,
    };
    pub use crate::llm_call::{
        AgentTurnEvent, AgentTurnObserver, AgentTurnObserverHandle, AgentTurnOutcome,
        AgentTurnRequest, ExecutorCommit, LLMExecutor, NoopTurnSink, TextTurnEvent, TurnSink,
    };
    pub use crate::prompt_context::{
        AgentTurnError, IdentityTransform, PromptContext, Role, Turn, TurnTransform,
    };
    pub use crate::stream_parser::{HermesParser, XmlElement};
    pub use crate::streaming_tool::{
        ParseContext, StreamingTool, StreamingToolError, StreamingToolRunner,
    };
    pub use crate::templates::{
        ContextBlockKind, ContextView, ContextViewBuilder, PromptFragment, PromptLayout,
        PromptRenderable, PromptSystemVars, TemplateEngine, TurnArtifact,
    };
    pub use crate::StorageString;
}
