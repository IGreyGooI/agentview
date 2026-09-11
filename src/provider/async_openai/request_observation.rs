use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

use crate::component::execution::reaction::{Frame, FrameBasis, FrameRevision};

pub(super) type RequestObserver = dyn Fn(OpenAiResponsesRequestSnapshot) + Send + Sync;

/// The exact JSON body of one Frame-native Responses request handed to transport.
///
/// This includes the provider's accumulated wire history, instructions, tools,
/// and model settings. HTTP headers and credentials are not included. A handoff
/// does not establish that the server received or successfully processed it.
#[derive(Clone)]
pub struct OpenAiResponsesRequestSnapshot {
    frame_revision: FrameRevision,
    frame_basis: FrameBasis,
    body: Arc<[u8]>,
}

impl OpenAiResponsesRequestSnapshot {
    pub(super) fn new(frame: &Frame, body: &[u8]) -> Self {
        Self {
            frame_revision: frame.revision(),
            frame_basis: frame.basis(),
            body: Arc::from(body),
        }
    }

    pub const fn frame_revision(&self) -> FrameRevision {
        self.frame_revision
    }

    pub const fn frame_basis(&self) -> FrameBasis {
        self.frame_basis
    }

    /// Returns the serialized request body exactly as supplied to HTTP transport.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

pub(super) fn observe_request(
    observer: &Option<Arc<RequestObserver>>,
    snapshot: Option<OpenAiResponsesRequestSnapshot>,
) {
    if let (Some(observer), Some(snapshot)) = (observer, snapshot) {
        // Observation cannot unwind across a handoff already accepted by the port.
        let _ = catch_unwind(AssertUnwindSafe(|| observer(snapshot)));
    }
}
