//! Typed contracts for native function tools.

use std::{fmt, future::Future};

use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Serialize};

/// A typed native tool that can be mounted with [`NativeToolCall`](super::NativeToolCall).
///
/// `#[tool]` generates this implementation for an ordinary free function. The
/// arguments are decoded from the provider payload into `Args`; successful
/// values are serialized by the mounted native-tool component.
pub trait NativeTool: Send + Sync + 'static {
    /// The owned JSON object decoded from one provider function call.
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    /// The value serialized into the matching function-call output item.
    type Output: Serialize + Send + 'static;

    /// A handler error reported as a native-tool reaction fault.
    type Error: fmt::Display + Send + 'static;

    /// Provider-visible tool name.
    const NAME: &'static str;

    /// Provider-visible tool description.
    const DESCRIPTION: &'static str;

    /// Invoke the tool with one fully decoded argument object.
    fn call(
        &self,
        args: Self::Args,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
}

/// A lightweight error for tools that only need a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}
