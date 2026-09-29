//! Model descriptions, provider-owned history projection, and request encoding.
//!
//! Transport clients and third-party SDK types belong in adapter modules built
//! on top of these AgentView-owned contracts.

use std::error::Error;

use crate::transcript::CanonicalTranscript;

pub mod async_openai;
pub mod codex_http_v1;
mod model;

pub use model::{ModelSpec, ModelSpecError};

/// Projects canonical history into one provider profile's accepted window.
pub trait HistoryPolicy {
    type History;
    type Error: Error + Send + Sync + 'static;

    fn project_history(
        &self,
        transcript: &CanonicalTranscript,
    ) -> Result<Self::History, Self::Error>;
}

/// Deterministically serializes one provider request body.
pub trait ProviderRequestEncoder {
    type Error: Error + Send + Sync + 'static;

    fn encode_request(&self, transcript: &CanonicalTranscript) -> Result<Vec<u8>, Self::Error>;
}
