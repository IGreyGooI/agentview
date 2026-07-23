use super::{Document, PomError};

/// Builds a complete, unresolved POM document from prompt-facing Rust data.
///
/// A producer only owns the `Rust value -> POM AST` step. It must not resolve
/// diff slots, inspect session state, or render prompt text.
pub trait DocumentProducer {
    fn build_document(&self) -> Result<Document, PomError>;
}
