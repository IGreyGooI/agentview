//! External reply types for chat, CLI, daemon, and skill control loops.
//!
//! This module is the sibling of the provider-backed [`llm_call`](crate::llm_call)
//! path. It describes how an external caller's reply can be represented and
//! parsed, without implying an [`LLMExecutor`](crate::llm_call::LLMExecutor).

use serde::{Deserialize, Serialize};

use crate::StorageString;

/// Reply supplied by an external caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlReply {
    Text(StorageString),
    Structured(serde_json::Value),
}

impl ControlReply {
    pub fn text(text: impl Into<StorageString>) -> Self {
        Self::Text(text.into())
    }

    pub fn structured(value: serde_json::Value) -> Self {
        Self::Structured(value)
    }

    pub fn as_structured(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Structured(value) => Some(value),
            Self::Text(_) => None,
        }
    }
}
