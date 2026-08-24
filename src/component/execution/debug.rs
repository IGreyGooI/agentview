use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    prompt_render::render_projection_prompt, ProviderEventStream, ProviderFault, ProviderPort,
    RenderedProjection,
};

/// Captured provider-neutral prompts from a [`DebugProviderPort`].
#[derive(Clone, Default)]
pub struct DebugPromptCapture {
    snapshots: Arc<Mutex<Vec<String>>>,
}

impl DebugPromptCapture {
    pub fn snapshots(&self) -> Vec<String> {
        self.snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn latest(&self) -> Option<String> {
        self.snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }
}

/// ProviderPort for inspecting the complete provider-neutral prompt projection.
///
/// It performs no model I/O and returns normal EOF after capturing one prompt.
pub struct DebugProviderPort {
    capture: DebugPromptCapture,
}

impl DebugProviderPort {
    pub fn new() -> (Self, DebugPromptCapture) {
        let capture = DebugPromptCapture::default();
        (
            Self {
                capture: capture.clone(),
            },
            capture,
        )
    }
}

impl Default for DebugProviderPort {
    fn default() -> Self {
        Self::new().0
    }
}

#[async_trait]
impl ProviderPort for DebugProviderPort {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        let prompt = render_projection_prompt(&projection)?;
        self.capture
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(prompt);
        Ok(Box::pin(futures::stream::empty()))
    }
}
