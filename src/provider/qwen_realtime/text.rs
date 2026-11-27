use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::{oneshot, OwnedSemaphorePermit};

use crate::component::execution::ReactionPortFault;

use super::{media::closed, QwenRealtimeResponseStatus};

pub(super) const MAX_TEXT_BYTES: usize = 64 * 1024;

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
#[derive(Clone, Debug)]
pub struct QwenRealtimeTextReply {
    pub response_id: String,
    pub text: String,
    pub status: QwenRealtimeResponseStatus,
    pub error: Option<ReactionPortFault>,
}

#[derive(Default)]
struct Correlations {
    calls: VecDeque<(String, Vec<String>)>,
    responses: VecDeque<(String, Vec<String>)>,
    accepted_text_sequence: u64,
}

#[derive(Clone, Default)]
pub(crate) struct TextCorrelations(Arc<Mutex<Correlations>>);

impl TextCorrelations {
    pub(crate) fn accept_text(&self) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        guard.accepted_text_sequence = guard.accepted_text_sequence.wrapping_add(1);
    }

    pub(crate) fn accepted_text_sequence(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .accepted_text_sequence
    }

    pub(crate) fn get(&self, call_id: &str) -> Option<String> {
        let correlations = self.for_call(call_id);
        // A merged reply belongs to every original service event. Returning
        // its first origin would let a caller attribute another event's tool
        // action to that input, so the singular API omits combined ownership.
        (correlations.len() == 1).then(|| correlations[0].clone())
    }

    pub(crate) fn for_call(&self, call_id: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .calls
            .iter()
            .find(|(id, _)| id == call_id)
            .map(|(_, correlations)| correlations.clone())
            .unwrap_or_default()
    }

    pub(crate) fn response(&self, response_id: &str) -> Option<String> {
        let correlations = self.for_response(response_id);
        // A provider response may cover several merged service events. Expose
        // one owner only when its complete origin set contains exactly one.
        (correlations.len() == 1).then(|| correlations[0].clone())
    }

    pub(crate) fn for_response(&self, response_id: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .responses
            .iter()
            .find(|(id, _)| id == response_id)
            .map(|(_, correlations)| correlations.clone())
            .unwrap_or_default()
    }

    pub(crate) fn remember(&self, call_id: &str, correlations: &[String]) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        Self::remember_in(&mut guard.calls, call_id, correlations);
    }

    pub(crate) fn remember_response(&self, response_id: &str, correlations: &[String]) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        Self::remember_in(&mut guard.responses, response_id, correlations);
    }

    fn remember_in(
        calls: &mut VecDeque<(String, Vec<String>)>,
        call_id: &str,
        correlations: &[String],
    ) {
        // Uncorrelated provider turns have no caller provenance to retain.
        // Omit that observation so microphone-only calls cannot evict tracked
        // service origins from the existing bounded history.
        if correlations.is_empty() {
            return;
        }
        if let Some(index) = calls.iter().position(|(id, _)| id == call_id) {
            calls.remove(index);
        }
        if calls.len() == 128 {
            calls.pop_front();
        }
        calls.push_back((call_id.to_owned(), correlations.to_vec()));
    }
}

pub(crate) struct ContextReplacementOrigin {
    pub(crate) response_id: String,
    pub(crate) speech_sequence: u64,
    pub(crate) text_sequence: u64,
}

pub(crate) struct TextRequest {
    pub(crate) text: String,
    pub(crate) correlation: Option<String>,
    pub(crate) user: bool,
    pub(crate) item_id: Option<String>,
    pub(crate) replacement: Option<ContextReplacementOrigin>,
    pub(super) merge_renderer: Option<fn(&[&str]) -> String>,
    members: Vec<TextRequest>,
    admitted: Option<oneshot::Sender<Result<QwenRealtimeTextTurn, ReactionPortFault>>>,
    completed: Option<oneshot::Sender<Result<QwenRealtimeTextReply, ReactionPortFault>>>,
    completion: Option<oneshot::Receiver<Result<QwenRealtimeTextReply, ReactionPortFault>>>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl TextRequest {
    pub(crate) fn queued(text: String, permit: OwnedSemaphorePermit) -> Self {
        Self {
            text,
            correlation: None,
            user: false,
            item_id: None,
            replacement: None,
            merge_renderer: None,
            members: Vec::new(),
            admitted: None,
            completed: None,
            completion: None,
            _permit: Some(permit),
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
                replacement: None,
                merge_renderer: None,
                members: Vec::new(),
                admitted: Some(admitted),
                completed: Some(completed),
                completion: Some(completion),
                _permit: Some(permit),
            },
            admission,
        )
    }

    pub(super) fn merged(text: String, members: Vec<TextRequest>) -> Self {
        Self {
            text,
            correlation: None,
            user: false,
            item_id: None,
            replacement: None,
            merge_renderer: None,
            members,
            admitted: None,
            completed: None,
            completion: None,
            _permit: None,
        }
    }

    pub(super) fn sources(&self) -> impl Iterator<Item = &TextRequest> {
        self.members
            .is_empty()
            .then_some(self)
            .into_iter()
            .chain(self.members.iter())
    }

    pub(super) fn correlations(&self) -> Vec<String> {
        self.sources()
            .filter_map(|source| source.correlation.clone())
            .collect()
    }

    pub(crate) fn admit(&mut self, response_id: &str) {
        if let (Some(admitted), Some(completed)) = (self.admitted.take(), self.completion.take()) {
            let _ = admitted.send(Ok(QwenRealtimeTextTurn {
                item_id: self.item_id.clone().unwrap_or_default(),
                response_id: response_id.to_owned(),
                completed,
            }));
        }
        for member in &mut self.members {
            member.item_id = self.item_id.clone();
            member.admit(response_id);
        }
    }

    pub(crate) fn reject(mut self, fault: ReactionPortFault) {
        for member in self.members.drain(..) {
            member.reject(fault);
        }
        if let Some(admitted) = self.admitted.take() {
            let _ = admitted.send(Err(fault));
        }
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(Err(fault));
        }
    }

    pub(crate) fn finish(mut self, reply: QwenRealtimeTextReply) {
        for member in self.members.drain(..) {
            member.finish(reply.clone());
        }
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
