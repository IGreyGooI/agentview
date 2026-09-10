//! Optional private diagnostics at the native Chat transport boundary.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use crate::component::execution::reaction::{
    ProviderFact, ProviderFactStream, ReactionPortFault, ReactionPortFaultCode,
    ReactionPortFaultReason,
};

/// Bodies are bounded by the provider's existing request and response limits.
/// Credentials and HTTP headers are deliberately excluded.
#[derive(Debug, Clone)]
pub enum OpenAiChatObservation {
    Request {
        path: String,
        body: Vec<u8>,
    },
    Response {
        status: u16,
        body: Vec<u8>,
        /// The Chat protocol reached its validated terminal marker. On failure,
        /// body contains only bytes received before that failure.
        completed: bool,
        usage: Option<Value>,
    },
    Error {
        fault: ReactionPortFault,
    },
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("Chat transport observation failed")]
pub struct OpenAiChatObservationError;

#[async_trait]
pub trait OpenAiChatObserver: Send + Sync {
    /// May durably persist diagnostics. Returning an error closes this provider
    /// session; no successful terminal fact is admitted after an audit failure.
    async fn observe(&self, event: OpenAiChatObservation)
        -> Result<(), OpenAiChatObservationError>;
}

#[derive(Default)]
pub(super) struct CapturedResponse {
    pub(super) status: Option<u16>,
    pub(super) body: Vec<u8>,
    pub(super) usage: Option<Value>,
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
    observer: Option<Arc<dyn OpenAiChatObserver>>,
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
            // A sealed Chat text is emitted only after [DONE]. Persist before
            // exposing it because Component handlers may finish an attempt here.
            let completed = matches!(
                fact,
                Ok(ProviderFact::TextSealed { .. } | ProviderFact::ReactionCompleted { .. })
            );
            if completed || fact.is_err() {
                let response = {
                    let mut capture = capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    capture
                        .status
                        .take()
                        .map(|status| OpenAiChatObservation::Response {
                            status,
                            body: std::mem::take(&mut capture.body),
                            completed,
                            usage: capture.usage.take(),
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
                        .observe(OpenAiChatObservation::Error { fault })
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
