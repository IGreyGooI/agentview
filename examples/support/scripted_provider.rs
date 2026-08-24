use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use agentview::component::execution::{
    ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort, RenderedProjection,
};
use async_trait::async_trait;

#[derive(Clone, Default)]
pub struct ProjectionCapture {
    projections: Arc<Mutex<Vec<RenderedProjection>>>,
}

impl ProjectionCapture {
    pub fn snapshots(&self) -> Vec<RenderedProjection> {
        self.projections
            .lock()
            .expect("projection capture lock")
            .clone()
    }
}

pub struct ScriptedProvider {
    scripts: VecDeque<Vec<ProviderEvent>>,
    capture: ProjectionCapture,
}

impl ScriptedProvider {
    pub fn new(scripts: impl IntoIterator<Item = Vec<ProviderEvent>>) -> (Self, ProjectionCapture) {
        let capture = ProjectionCapture::default();
        (
            Self {
                scripts: scripts.into_iter().collect(),
                capture: capture.clone(),
            },
            capture,
        )
    }
}

#[async_trait]
impl ProviderPort for ScriptedProvider {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.capture
            .projections
            .lock()
            .expect("projection capture lock")
            .push(projection);
        let events = self.scripts.pop_front().ok_or_else(|| {
            ProviderFault::retryable_transport("scripted provider has no reaction script")
        })?;
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}
