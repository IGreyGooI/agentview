use std::num::NonZeroU64;

/// A model identifier bound to its effective context capacity on a deployment.
///
/// The caller supplies the capacity explicitly, including for well-known model
/// names. Request options retain this description, and the provider derives its
/// budget declaration from it. Only the identifier is sent as the
/// provider request's model field; the window is local budget metadata.
///
/// ```
/// use agentview::provider::ModelSpec;
///
/// let model = ModelSpec::new("my-model", 128_000)?;
/// assert_eq!(model.id(), "my-model");
/// assert_eq!(model.context_window_tokens().get(), 128_000);
/// # Ok::<(), agentview::provider::ModelSpecError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    id: String,
    context_window_tokens: NonZeroU64,
}

impl ModelSpec {
    /// Requires a non-empty identifier and a non-zero context capacity in tokens.
    pub fn new(id: impl Into<String>, context_window_tokens: u64) -> Result<Self, ModelSpecError> {
        let id = id.into();
        if id.is_empty() {
            return Err(ModelSpecError::EmptyId);
        }
        let context_window_tokens = NonZeroU64::new(context_window_tokens)
            .ok_or(ModelSpecError::ZeroContextWindowTokens)?;
        Ok(Self {
            id,
            context_window_tokens,
        })
    }

    /// Exact model identifier to send to the configured provider.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Effective token capacity of this model on the caller's deployment.
    pub const fn context_window_tokens(&self) -> NonZeroU64 {
        self.context_window_tokens
    }
}

/// Invalid model identity or context capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelSpecError {
    #[error("model identifier must be non-empty")]
    EmptyId,
    #[error("model context window must be greater than zero")]
    ZeroContextWindowTokens,
}
