//! Opt-in live transport diagnostics, separate from payload-free accounting.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use super::call_observation::{OpenAiResponsesCallKind, OpenAiResponsesCallOutcome};
use crate::component::execution::reaction::FrameRevision;

pub(super) type StreamObserver = dyn Fn(OpenAiResponsesStreamEvent) + Send + Sync;

/// One live transport observation. `elapsed_ms` uses the attempt's monotonic
/// clock; `timestamp_ms` is wall time for correlating independent observations.
/// `call_id` is identical to the payload-free call observer's attempt identity.
#[derive(Clone, Debug)]
pub struct OpenAiResponsesStreamEvent {
    pub call_id: String,
    pub frame_revision: FrameRevision,
    pub call_kind: OpenAiResponsesCallKind,
    pub timestamp_ms: u64,
    pub elapsed_ms: u64,
    pub event: OpenAiResponsesStreamEventKind,
}

/// Exact protocol data, without HTTP credentials, URL or arbitrary headers.
/// A network chunk is not a token; SSE observation precedes protocol admission.
/// Finished::Completed requires the provider's validated completion contract.
#[derive(Clone, Debug)]
pub enum OpenAiResponsesStreamEventKind {
    Request {
        body: Arc<[u8]>,
    },
    Headers {
        status: u16,
        request_id: Option<String>,
    },
    BodyChunk {
        bytes: usize,
    },
    Sse {
        event: String,
        id: String,
        data: String,
    },
    /// Non-SSE/error response body, subject to existing transport size limits.
    ResponseBody {
        body: Arc<[u8]>,
    },
    Finished {
        outcome: OpenAiResponsesCallOutcome,
    },
}

#[derive(Clone)]
pub(super) struct StreamDiagnostics {
    observer: Arc<StreamObserver>,
    call_id: String,
    frame_revision: FrameRevision,
    call_kind: OpenAiResponsesCallKind,
    started: Instant,
}

impl StreamDiagnostics {
    pub(super) fn new(
        observer: Arc<StreamObserver>,
        call_id: String,
        frame_revision: FrameRevision,
        call_kind: OpenAiResponsesCallKind,
        started: Instant,
    ) -> Self {
        Self {
            observer,
            call_id,
            frame_revision,
            call_kind,
            started,
        }
    }

    pub(super) fn emit(&self, event: OpenAiResponsesStreamEventKind) {
        let observed = OpenAiResponsesStreamEvent {
            call_id: self.call_id.clone(),
            frame_revision: self.frame_revision,
            call_kind: self.call_kind,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            elapsed_ms: self
                .started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            event,
        };
        let _ = catch_unwind(AssertUnwindSafe(|| (self.observer)(observed)));
    }
}
