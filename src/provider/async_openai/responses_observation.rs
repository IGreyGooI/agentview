//! Optional raw transport diagnostics for the native Responses port.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use futures::StreamExt;

use crate::component::execution::reaction::{
    ProviderFact, ProviderFactStream, ReactionPortFault, ReactionPortFaultCode,
    ReactionPortFaultReason,
};

use super::parallel_compaction::ParallelCompactionStatus;

/// Payload-free preparation classification. Optional measurements are absent
/// when encoding stopped before an exact complete wire body existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiResponsesRequestPreparationReason {
    ContextWindowLimit,
    SerializedRequestBodyLimit,
    MissingContextWindow,
    InvalidFrameState,
    Encoding,
    RequestBuild,
}

impl OpenAiResponsesRequestPreparationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextWindowLimit => "context_window_limit",
            Self::SerializedRequestBodyLimit => "serialized_request_body_limit",
            Self::MissingContextWindow => "missing_context_window",
            Self::InvalidFrameState => "invalid_frame_state",
            Self::Encoding => "encoding",
            Self::RequestBuild => "request_build",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiResponsesFrameBasis {
    Full,
    Delta,
}

impl OpenAiResponsesFrameBasis {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Delta => "delta",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiResponsesRequestPreparationDiagnostic {
    pub reason: OpenAiResponsesRequestPreparationReason,
    pub request_body_bytes: Option<usize>,
    pub estimated_input_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    pub reserved_output_tokens: u64,
    pub serialized_request_body_limit_bytes: usize,
    pub frame_basis: OpenAiResponsesFrameBasis,
    pub compacted_base_used: bool,
    pub compaction: ParallelCompactionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiResponsesTransportPhase {
    Request,
}

/// Structured reqwest facts. Never includes URLs, headers, source errors, or
/// provider/body text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiResponsesTransportDiagnostic {
    pub phase: OpenAiResponsesTransportPhase,
    pub is_connect: bool,
    pub is_timeout: bool,
    pub elapsed_ms: u64,
}

impl OpenAiResponsesTransportDiagnostic {
    pub(super) fn request(error: &reqwest::Error, started: std::time::Instant) -> Self {
        Self {
            phase: OpenAiResponsesTransportPhase::Request,
            is_connect: error.is_connect(),
            is_timeout: error.is_timeout(),
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        }
    }
}

/// Bounded HTTP bodies without credentials or response headers.
#[derive(Debug, Clone)]
pub enum OpenAiResponsesObservation {
    Request {
        path: String,
        body: Vec<u8>,
    },
    Response {
        status: u16,
        body: Vec<u8>,
        /// True only after a validated completed response (the native
        /// foreground path requires response.completed). Otherwise body
        /// contains the bounded bytes captured before the provider failure.
        completed: bool,
    },
    Error {
        fault: ReactionPortFault,
    },
    RequestPreparationFailed {
        fault: ReactionPortFault,
        diagnostic: OpenAiResponsesRequestPreparationDiagnostic,
    },
    TransportDiagnostic {
        diagnostic: OpenAiResponsesTransportDiagnostic,
    },
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("Responses transport observation failed")]
pub struct OpenAiResponsesObservationError;

#[async_trait]
pub trait OpenAiResponsesObserver: Send + Sync {
    /// A failed observation closes this provider session. Final response
    /// observation precedes ReactionCompleted; earlier output facts keep their
    /// ordinary Responses ordering and are not gated by this callback. A failed
    /// preparation receipt retains its primary local rejection. Summary worker
    /// receipt failure invalidates only that speculative candidate.
    async fn observe(
        &self,
        event: OpenAiResponsesObservation,
    ) -> Result<(), OpenAiResponsesObservationError>;
}

#[derive(Default)]
pub(super) struct CapturedResponse {
    pub(super) status: Option<u16>,
    pub(super) body: Vec<u8>,
}

pub(super) type ResponseCapture = Arc<Mutex<CapturedResponse>>;

pub(super) fn observation_fault() -> ReactionPortFault {
    ReactionPortFault::terminal(
        ReactionPortFaultCode::Internal,
        ReactionPortFaultReason::Other,
    )
}

pub(super) fn observe_facts<'a>(
    stream: ProviderFactStream<'a>,
    observer: Option<Arc<dyn OpenAiResponsesObserver>>,
    capture: ResponseCapture,
    failed: Arc<AtomicBool>,
) -> ProviderFactStream<'a> {
    let Some(observer) = observer else {
        return stream;
    };
    Box::pin(stream.then(move |fact| {
        let observer = observer.clone();
        let capture = capture.clone();
        let failed = failed.clone();
        async move {
            if failed.load(Ordering::Acquire) {
                return Err(observation_fault());
            }
            let completed = matches!(fact, Ok(ProviderFact::ReactionCompleted { .. }));
            if completed || fact.is_err() {
                let response = {
                    let mut capture = capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    capture
                        .status
                        .take()
                        .map(|status| OpenAiResponsesObservation::Response {
                            status,
                            body: std::mem::take(&mut capture.body),
                            completed,
                        })
                };
                if let Some(response) = response {
                    if observer.observe(response).await.is_err() {
                        failed.store(true, Ordering::Release);
                        return Err(observation_fault());
                    }
                }
                if let Err(fault) = fact {
                    if observer
                        .observe(OpenAiResponsesObservation::Error { fault })
                        .await
                        .is_err()
                    {
                        failed.store(true, Ordering::Release);
                        return Err(observation_fault());
                    }
                }
            }
            fact
        }
    }))
}

pub(super) async fn capture_error_response(
    response: reqwest::Response,
    capture: &ResponseCapture,
    read_timeout: std::time::Duration,
    max_body_bytes: usize,
) {
    let mut body = response.bytes_stream();
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout(read_timeout, body.next()).await {
        let mut captured = capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = max_body_bytes.saturating_sub(captured.body.len());
        captured
            .body
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() >= remaining {
            break;
        }
    }
}
