use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::{oneshot, OwnedSemaphorePermit};

use crate::component::execution::ReactionPortFault;

use super::{media::closed, QwenRealtimeResponseStatus};

/// A remotely acknowledged text input and the response that first accepted it.
/// Dropping this receipt does not cancel an admitted input.
pub struct QwenRealtimeTextTurn {
    pub item_id: String,
    pub response_id: String,
    completed: oneshot::Receiver<Result<QwenRealtimeTextReply, ReactionPortFault>>,
}

impl QwenRealtimeTextTurn {
    /// Wait for the final response, including native tool continuations.
    pub async fn completed(self) -> Result<QwenRealtimeTextReply, ReactionPortFault> {
        self.completed.await.map_err(|_| closed())?
    }
}

/// The provider's actual terminal response for one text input or context reply.
#[derive(Debug)]
pub struct QwenRealtimeTextReply {
    pub response_id: String,
    pub text: String,
    pub status: QwenRealtimeResponseStatus,
    pub error: Option<ReactionPortFault>,
}

#[derive(Default)]
struct Correlations {
    calls: VecDeque<(String, String)>,
    responses: VecDeque<(String, String)>,
}

#[derive(Clone, Default)]
pub(crate) struct TextCorrelations(Arc<Mutex<Correlations>>);

impl TextCorrelations {
    pub(crate) fn get(&self, call_id: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .calls
            .iter()
            .find(|(id, _)| id == call_id)
            .map(|(_, correlation)| correlation.clone())
    }

    pub(crate) fn response(&self, response_id: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .responses
            .iter()
            .find(|(id, _)| id == response_id)
            .map(|(_, correlation)| correlation.clone())
    }

    pub(crate) fn remember(&self, call_id: &str, correlation: &str) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        Self::remember_in(&mut guard.calls, call_id, correlation);
    }

    pub(crate) fn remember_response(&self, response_id: &str, correlation: &str) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        Self::remember_in(&mut guard.responses, response_id, correlation);
    }

    fn remember_in(calls: &mut VecDeque<(String, String)>, call_id: &str, correlation: &str) {
        if let Some(index) = calls.iter().position(|(id, _)| id == call_id) {
            calls.remove(index);
        }
        if calls.len() == 128 {
            calls.pop_front();
        }
        calls.push_back((call_id.to_owned(), correlation.to_owned()));
    }
}

pub(crate) struct TextRequest {
    pub(crate) text: String,
    pub(crate) correlation: Option<String>,
    pub(crate) user: bool,
    pub(crate) item_id: Option<String>,
    admitted: Option<oneshot::Sender<Result<QwenRealtimeTextTurn, ReactionPortFault>>>,
    completed: Option<oneshot::Sender<Result<QwenRealtimeTextReply, ReactionPortFault>>>,
    completion: Option<oneshot::Receiver<Result<QwenRealtimeTextReply, ReactionPortFault>>>,
    _permit: OwnedSemaphorePermit,
}

impl TextRequest {
    pub(crate) fn queued(text: String, permit: OwnedSemaphorePermit) -> Self {
        Self {
            text,
            correlation: None,
            user: false,
            item_id: None,
            admitted: None,
            completed: None,
            completion: None,
            _permit: permit,
        }
    }

    pub(crate) fn acknowledged(
        text: String,
        correlation: Option<String>,
        user: bool,
        permit: OwnedSemaphorePermit,
    ) -> (
        Self,
        oneshot::Receiver<Result<QwenRealtimeTextTurn, ReactionPortFault>>,
    ) {
        let (admitted, admission) = oneshot::channel();
        let (completed, completion) = oneshot::channel();
        (
            Self {
                text,
                correlation,
                user,
                item_id: None,
                admitted: Some(admitted),
                completed: Some(completed),
                completion: Some(completion),
                _permit: permit,
            },
            admission,
        )
    }

    pub(crate) fn admit(&mut self, response_id: &str) {
        if let (Some(admitted), Some(completed)) = (self.admitted.take(), self.completion.take()) {
            let _ = admitted.send(Ok(QwenRealtimeTextTurn {
                item_id: self.item_id.clone().unwrap_or_default(),
                response_id: response_id.to_owned(),
                completed,
            }));
        }
    }

    pub(crate) fn reject(mut self, fault: ReactionPortFault) {
        if let Some(admitted) = self.admitted.take() {
            let _ = admitted.send(Err(fault));
        }
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(Err(fault));
        }
    }

    pub(crate) fn finish(mut self, reply: QwenRealtimeTextReply) {
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(Ok(reply));
        }
    }
}

pub(crate) struct ActiveTextTurn {
    pub(crate) request: TextRequest,
    pub(crate) response_id: Option<String>,
    pub(crate) last_response_id: String,
    pub(crate) response_event_id: String,
    pub(crate) speech_sequence: u64,
    pub(crate) continuation: bool,
    pub(crate) text: String,
    pub(crate) terminal: Option<QwenRealtimeTextReply>,
}
