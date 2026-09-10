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
        /// True only after a validated response.completed event. Otherwise
        /// body contains the bytes captured before the provider failure.
        completed: bool,
    },
    Error {
        fault: ReactionPortFault,
    },
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("Responses transport observation failed")]
pub struct OpenAiResponsesObservationError;

#[async_trait]
pub trait OpenAiResponsesObserver: Send + Sync {
    /// A failed observation closes this provider session. Final response
    /// observation precedes ReactionCompleted; earlier output facts keep their
    /// ordinary Responses ordering and are not gated by this callback.
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
