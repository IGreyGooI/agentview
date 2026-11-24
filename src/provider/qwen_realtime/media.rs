use std::{
    collections::{HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, watch, OwnedSemaphorePermit};
use tokio::time::Instant;

use crate::component::execution::{
    FrameRevision, ReactionPortFault as Fault, ReactionPortFaultCode as Code,
    ReactionPortFaultReason as Reason,
};

use super::{
    audio::{AudioDecoder, QwenRealtimeAudioEvent, QwenRealtimeResponseStatus},
    text::{ActiveTextTurn, TextCorrelations, TextRequest},
    transport::{self, Socket},
    Mode, QwenRealtimeTextReply,
};

const INPUT_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const MAX_PENDING_TRANSCRIPTIONS: usize = 128;
const TRANSCRIPTION_DRAIN: Duration = Duration::from_secs(15);
const NOTICE_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const ABANDONED_NOTICES: usize = 16;

struct Notice {
    item_id: String,
    create_event_id: String,
    response_event_id: String,
    deadline: Instant,
    created: bool,
    requested: bool,
}

#[derive(Clone)]
struct ContextResponseRequest {
    event_id: String,
    provider_session_id: String,
    revision: FrameRevision,
    cue_item: Option<String>,
}

pub(crate) enum Input {
    Append(String),
    SetMuted(bool),
    Speak(SpokenText),
    UserText(TextRequest),
    RefreshContext,
    RefreshContextAndRespond(TextRequest),
    Commit,
    Clear,
    Interrupt(Interrupt),
    Finish,
}

enum TextControl {
    User(TextRequest),
    Context(TextRequest),
}

pub(crate) struct SpokenText {
    pub(crate) text: String,
    _permit: OwnedSemaphorePermit,
}

impl SpokenText {
    pub(crate) fn new(text: String, permit: OwnedSemaphorePermit) -> Self {
        Self {
            text,
            _permit: permit,
        }
    }
}

pub(crate) struct Interrupt {
    pub(crate) response_id: String,
    pub(crate) result: oneshot::Sender<Result<bool, Fault>>,
}

/// Cloneable, bounded input to a mounted Qwen Realtime voice session.
#[derive(Clone)]
pub struct QwenRealtimeMediaInput {
    sender: mpsc::Sender<Input>,
    controls: Option<mpsc::Sender<TextControl>>,
    text_correlation: watch::Sender<Option<String>>,
    text_correlations: TextCorrelations,
}

impl QwenRealtimeMediaInput {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<Input>) {
        let (sender, receiver) = mpsc::channel(INPUT_CAPACITY);
        let (text_correlation, _) = watch::channel(None);
        (
            Self {
                sender,
                controls: None,
                text_correlation,
                text_correlations: TextCorrelations::default(),
            },
            receiver,
        )
    }

    pub(crate) fn text_correlations(&self) -> (watch::Receiver<Option<String>>, TextCorrelations) {
        (
            self.text_correlation.subscribe(),
            self.text_correlations.clone(),
        )
    }

    /// Enqueues 16 kHz mono signed little-endian PCM16.
    pub async fn append_pcm16(&self, pcm16: &[u8]) -> Result<(), Fault> {
        self.send(Input::Append(super::audio::encode_pcm16(pcm16)?))
            .await
    }

    /// Marks the end of one utterance. The provider commits it and starts generation.
    pub async fn commit_input(&self) -> Result<(), Fault> {
        self.send(Input::Commit).await
    }

    /// Discards audio buffered locally or on the server for the next utterance.
    pub async fn clear_input(&self) -> Result<(), Fault> {
        self.send(Input::Clear).await
    }

    /// Ends input and requests graceful session completion after outstanding work.
    pub async fn finish(&self) -> Result<(), Fault> {
        self.send(Input::Finish).await
    }

    pub(crate) async fn send(&self, input: Input) -> Result<(), Fault> {
        let input = match (input, &self.controls) {
            (Input::UserText(text), Some(controls)) => {
                return controls
                    .send(TextControl::User(text))
                    .await
                    .map_err(|_| closed());
            }
            (Input::RefreshContextAndRespond(text), Some(controls)) => {
                return controls
                    .send(TextControl::Context(text))
                    .await
                    .map_err(|_| closed());
            }
            (input, _) => input,
        };
        self.sender.send(input).await.map_err(|_| closed())
    }
}

/// Receives PCM16, transcripts and other server events for one mounted session.
pub struct QwenRealtimeMediaEvents {
    typed: broadcast::Receiver<QwenRealtimeAudioEvent>,
    raw: broadcast::Receiver<Arc<Value>>,
}

/// Provider-owned publishers for the shared voice transport contract.
pub(crate) struct MediaEventsSender {
    typed: broadcast::Sender<QwenRealtimeAudioEvent>,
    raw: broadcast::Sender<Arc<Value>>,
}

impl MediaEventsSender {
    pub(crate) fn new() -> (Self, QwenRealtimeMediaEvents) {
        let (typed, events) = broadcast::channel(EVENT_CAPACITY);
        let (raw, server_events) = broadcast::channel(EVENT_CAPACITY);
        (
            Self { typed, raw },
            QwenRealtimeMediaEvents {
                typed: events,
                raw: server_events,
            },
        )
    }

    pub(crate) fn publish(&self, event: QwenRealtimeAudioEvent) {
        let _ = self.typed.send(event);
    }

    pub(crate) fn publish_server_event(&self, event: Arc<Value>) {
        let _ = self.raw.send(event);
    }
}

/// An independent subscription to complete Qwen server events and local
/// `agentview.context_*` delivery observations. Local observations carry
/// only correlation/status metadata and are never sent to the model.
pub struct QwenRealtimeServerEvents(broadcast::Receiver<Arc<Value>>);

impl QwenRealtimeServerEvents {
    /// Receives the next complete JSON event. Reports lag if the reader falls behind.
    pub async fn next_event(&mut self) -> Result<Arc<Value>, Fault> {
        self.0.recv().await.map_err(event_fault)
    }
}

impl QwenRealtimeMediaEvents {
    /// Receives a typed media event. Slow consumers receive a limit fault on lag.
    pub async fn next_event(&mut self) -> Result<QwenRealtimeAudioEvent, Fault> {
        self.typed.recv().await.map_err(event_fault)
    }

    /// Receives the complete JSON event from the server, including vendor extensions.
    ///
    /// This bounded subscription reports lag. Content can include media and
    /// conversation data.
    pub async fn next_server_event(&mut self) -> Result<Arc<Value>, Fault> {
        self.raw.recv().await.map_err(event_fault)
    }

    /// Subscribes to future server events independently of typed media playback.
    pub fn subscribe_server_events(&self) -> QwenRealtimeServerEvents {
        QwenRealtimeServerEvents(self.raw.resubscribe())
    }
}

fn event_fault(error: broadcast::error::RecvError) -> Fault {
    match error {
        broadcast::error::RecvError::Closed => closed(),
        broadcast::error::RecvError::Lagged(_) => {
            Fault::retryable(Code::Limit, Reason::OutputLimit)
        }
    }
}

pub(crate) fn closed() -> Fault {
    Fault::retryable(Code::Unavailable, Reason::StreamTransport)
}

pub(super) struct Media {
    mode: Mode,
    input: mpsc::Receiver<Input>,
    controls: mpsc::Receiver<TextControl>,
    controls_closed: bool,
    events: MediaEventsSender,
    decoder: AudioDecoder,
    pending_asr: HashSet<String>,
    buffered_bytes: usize,
    commit_pending: bool,
    awaiting_commit: bool,
    committed_turn: bool,
    waiting_for_interruption: bool,
    interrupt: Option<Interrupt>,
    active_response: watch::Sender<Option<String>>,
    spoken: VecDeque<SpokenText>,
    spoken_items: Vec<String>,
    spoken_pending: bool,
    notice: Option<Notice>,
    abandoned_notices: VecDeque<Notice>,
    notice_cleanup: VecDeque<String>,
    notice_delete_events: VecDeque<(String, String)>,
    input_speech: bool,
    awaiting_speech_response: bool,
    context_refresh_pending: bool,
    context_response_pending: VecDeque<TextRequest>,
    user_texts: VecDeque<TextRequest>,
    prepared_text: Option<TextRequest>,
    text_turn: Option<ActiveTextTurn>,
    text_correlation: watch::Sender<Option<String>>,
    text_correlations: TextCorrelations,
    speech_sequence: u64,
    text_cancelled_response: Option<String>,
    interrupt_for_text: bool,
    retired_text_creates: VecDeque<String>,
    context_generation_waiting: Option<String>,
    context_generations: VecDeque<ContextResponseRequest>,
    finished: bool,
}

impl Media {
    pub(super) fn new(mode: Mode) -> (Self, QwenRealtimeMediaInput, QwenRealtimeMediaEvents) {
        let (mut sender, input) = QwenRealtimeMediaInput::channel();
        let (controls, control_input) = mpsc::channel(INPUT_CAPACITY);
        sender.controls = Some(controls);
        let (events, media_events) = MediaEventsSender::new();
        let (active_response, _) = watch::channel(None);
        (
            Self {
                mode,
                input,
                controls: control_input,
                controls_closed: false,
                events,
                decoder: AudioDecoder::default(),
                pending_asr: HashSet::new(),
                buffered_bytes: 0,
                commit_pending: false,
                awaiting_commit: false,
                committed_turn: false,
                waiting_for_interruption: false,
                interrupt: None,
                active_response,
                spoken: VecDeque::new(),
                spoken_items: Vec::new(),
                spoken_pending: false,
                notice: None,
                abandoned_notices: VecDeque::new(),
                notice_cleanup: VecDeque::new(),
                notice_delete_events: VecDeque::new(),
                input_speech: false,
                awaiting_speech_response: false,
                context_refresh_pending: false,
                context_response_pending: VecDeque::new(),
                user_texts: VecDeque::new(),
                prepared_text: None,
                text_turn: None,
                text_correlation: sender.text_correlation.clone(),
                text_correlations: sender.text_correlations.clone(),
                speech_sequence: 0,
                text_cancelled_response: None,
                interrupt_for_text: false,
                retired_text_creates: VecDeque::new(),
                context_generation_waiting: None,
                context_generations: VecDeque::new(),
                finished: false,
            },
            sender,
            media_events,
        )
    }

    pub(super) fn subscribe_active_response(&self) -> watch::Receiver<Option<String>> {
        self.active_response.subscribe()
    }

    pub(super) fn begin_context_frame(&mut self) -> (bool, Option<TextRequest>) {
        // Only refreshes already consumed by this socket owner are covered by
        // the Frame about to be installed. New requests remain in the input
        // queue, so an ACK cannot erase a later Signal update.
        let refresh = std::mem::take(&mut self.context_refresh_pending);
        let cue = if self.prepared_text.is_none()
            && self.user_texts.is_empty()
            && self.text_turn.is_none()
            && self.controls.is_empty()
            && self.context_response_available()
        {
            self.context_response_pending.pop_front()
        } else {
            None
        };
        self.context_refresh_pending =
            !self.context_response_pending.is_empty() || !self.user_texts.is_empty();
        (refresh, cue)
    }

    pub(super) fn prepare_text_frame(&mut self) {
        // The socket reader maintains voice activity. Select a queued user only
        // after the preceding response has ended, so its old callbacks cannot
        // acquire the new input's owner during the next Frame preparation.
        if self.prepared_text.is_none()
            && !self.user_texts.is_empty()
            && self.context_response_available()
        {
            if let Some(previous) = self.text_turn.take() {
                previous.request.finish(QwenRealtimeTextReply {
                    response_id: previous.last_response_id,
                    text: previous.text,
                    status: QwenRealtimeResponseStatus::Cancelled,
                    error: None,
                });
            }
            self.prepared_text = self.user_texts.pop_front();
            self.interrupt_for_text = false;
            self.text_correlation.send_replace(
                self.prepared_text
                    .as_ref()
                    .and_then(|text| text.correlation.clone()),
            );
        }
    }

    pub(super) fn take_prepared_text(&mut self) -> Option<TextRequest> {
        self.prepared_text.take()
    }

    pub(super) fn defer_text(&mut self, text: TextRequest) {
        if text.user {
            self.user_texts.push_front(text);
        } else {
            self.context_response_pending.push_front(text);
        }
        self.context_refresh_pending = true;
    }

    pub(super) fn begin_text_response(&mut self, request: TextRequest, event_id: String) {
        self.text_turn = Some(ActiveTextTurn {
            request,
            response_id: None,
            last_response_id: String::new(),
            response_event_id: event_id,
            speech_sequence: self.speech_sequence,
            continuation: false,
            text: String::new(),
            terminal: None,
        });
    }

    pub(super) fn fail_text(&self, request: TextRequest, reason: &'static str, fault: Fault) {
        if !request.user {
            self.publish_context_refresh_event(json!({"type":"agentview.context_response_failed",
                "correlation": request.correlation, "reason":reason}));
        }
        self.text_correlation.send_replace(None);
        request.reject(fault);
    }

    pub(super) fn retain_context_response(&mut self, request: TextRequest) {
        // Correlated completion commands represent one queued delivery. Voice
        // may win this Frame, but must not consume that unstarted delivery.
        // Legacy uncorrelated optional cues retain their documented skip policy.
        if request.correlation.is_some() {
            self.defer_text(request);
        }
    }

    pub(super) fn settle_text_turn(&mut self) {
        if self
            .text_turn
            .as_ref()
            .is_some_and(|turn| turn.terminal.is_some())
        {
            let mut turn = self.text_turn.take().expect("settled text response");
            let reply = turn.terminal.take().expect("terminal text reply");
            self.publish_context_refresh_event(json!({
                "type":if turn.request.user { "agentview.text_response_completed" } else { "agentview.context_response_completed" },
                "correlation":turn.request.correlation, "item_id":turn.request.item_id,
                "response_id":reply.response_id, "status":response_status(reply.status),
            }));
            turn.request.finish(reply);
            self.text_correlation.send_replace(None);
        }
    }

    fn observe_text_turn(&mut self, event: &Value) {
        let Some(mut turn) = self.text_turn.take() else {
            return;
        };
        match event["type"].as_str() {
            Some("response.created") if turn.response_id.is_none() => {
                // Qwen's response.created does not echo the client's create ID.
                // If VAD advanced after our create, it may describe automatic
                // speech rather than this text. Preserve the original request
                // and its ACKed item until that voice response ends; assigning
                // this ambiguous response would send a voice answer to text.
                if turn.speech_sequence != self.speech_sequence {
                    self.defer_unbound_text(turn);
                    return;
                }
                if let Some(response_id) = event["response"]["id"].as_str() {
                    turn.response_id = Some(response_id.to_owned());
                    turn.last_response_id = response_id.to_owned();
                    turn.continuation = false;
                    turn.text.clear();
                    if let Some(correlation) = &turn.request.correlation {
                        self.text_correlations
                            .remember_response(response_id, correlation);
                    }
                    turn.request.admit(response_id);
                    self.publish_context_refresh_event(json!({
                        "type":if turn.request.user { "agentview.text_response_started" } else { "agentview.context_response_started" },
                        "correlation":turn.request.correlation, "item_id":turn.request.item_id,
                        "response_id":response_id,
                    }));
                }
            }
            Some("response.audio_transcript.done" | "response.text.done")
                if event["response_id"].as_str() == turn.response_id.as_deref() =>
            {
                if let Some(text) = event["transcript"]
                    .as_str()
                    .or_else(|| event["text"].as_str())
                {
                    turn.text = text.to_owned();
                }
            }
            Some("response.done")
                if event["response"]["id"].as_str() == turn.response_id.as_deref() =>
            {
                let response = &event["response"];
                // The terminal envelope also carries the provider's final
                // content. Some sessions omit separate transcript/text-done
                // events; keep their actual text in the acknowledged receipt.
                if turn.text.is_empty() {
                    turn.text = response["output"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|item| item["type"] == "message" && item["role"] == "assistant")
                        .flat_map(|item| item["content"].as_array().into_iter().flatten())
                        .filter_map(|part| {
                            part["transcript"]
                                .as_str()
                                .or_else(|| part["text"].as_str())
                        })
                        .collect();
                }
                let output = response["output"].as_array();
                let tools = output
                    .into_iter()
                    .flatten()
                    .filter(|item| item["type"] == "function_call");
                let mut has_tools = false;
                for tool in tools {
                    has_tools = true;
                    if let (Some(call_id), Some(correlation)) = (
                        tool["call_id"].as_str(),
                        turn.request.correlation.as_deref(),
                    ) {
                        self.text_correlations.remember(call_id, correlation);
                    }
                }
                let status = match response["status"].as_str() {
                    Some("completed") => QwenRealtimeResponseStatus::Completed,
                    Some("cancelled") => QwenRealtimeResponseStatus::Cancelled,
                    Some("incomplete") => QwenRealtimeResponseStatus::Incomplete,
                    _ => QwenRealtimeResponseStatus::Failed,
                };
                if status == QwenRealtimeResponseStatus::Completed && has_tools {
                    turn.continuation = true;
                    turn.response_id = None;
                } else {
                    turn.terminal = Some(QwenRealtimeTextReply {
                        response_id: turn.response_id.clone().unwrap_or_default(),
                        text: turn.text.clone(),
                        status,
                        error: (status == QwenRealtimeResponseStatus::Failed).then(|| {
                            super::output::server_error(&response["status_details"]["error"])
                        }),
                    });
                }
            }
            _ => {}
        }
        self.text_turn = Some(turn);
    }

    // Voice VAD and an explicit create can advance before the create has a
    // provable response identity. Preserve the original ACKed text, clearing
    // only this create's ownership; the voice terminal opens the next Frame.
    fn defer_unbound_text(&mut self, turn: ActiveTextTurn) {
        self.publish_context_refresh_event(json!({
            "type":"agentview.text_response_deferred",
            "correlation":turn.request.correlation,
            "item_id":turn.request.item_id, "reason":"user_turn",
        }));
        self.text_correlation.send_replace(None);
        self.context_generation_waiting = None;
        if turn.request.user {
            if self.retired_text_creates.len() == ABANDONED_NOTICES {
                self.retired_text_creates.pop_front();
            }
            self.retired_text_creates.push_back(turn.response_event_id);
            self.defer_text(turn.request);
        } else {
            self.retain_context_response(turn.request);
        }
    }

    pub(super) fn isolated_text_failure(&self, event: &Value) -> bool {
        event["type"] == "response.done"
            && self.text_turn.as_ref().is_some_and(|turn| {
                turn.response_id.as_deref() == event["response"]["id"].as_str()
                    && turn.terminal.as_ref().is_some_and(|reply| {
                        matches!(
                            reply.status,
                            QwenRealtimeResponseStatus::Failed
                                | QwenRealtimeResponseStatus::Incomplete
                        )
                    })
            })
    }

    pub(super) fn context_response_available(&self) -> bool {
        !self.decoder.is_responding()
            && !self.input_speech
            && !self.awaiting_speech_response
            && !self.awaiting_commit
            && !self.spoken_pending
            && !self.waiting_for_interruption
            && self.context_generation_waiting.is_none()
    }

    pub(super) fn expect_context_response(
        &mut self,
        event_id: String,
        provider_session_id: String,
        revision: FrameRevision,
        cue_item: Option<String>,
    ) {
        self.context_generation_waiting = Some(event_id.clone());
        if self.context_generations.len() == ABANDONED_NOTICES {
            self.context_generations.pop_front();
        }
        self.context_generations.push_back(ContextResponseRequest {
            event_id,
            provider_session_id,
            revision,
            cue_item,
        });
    }

    pub(super) fn owns_context_cue(&self, item_id: &str) -> bool {
        self.context_generations
            .iter()
            .any(|request| request.cue_item.as_deref() == Some(item_id))
    }

    fn context_response_rejection(&self, event: &Value) -> Option<(ContextResponseRequest, bool)> {
        if event["type"] != "error" || event["error"]["type"] != "invalid_request_error" {
            return None;
        }
        let explicit = event["error"]["event_id"].as_str();
        let request = self.context_generations.iter().find(|request| {
            explicit == Some(request.event_id.as_str())
                || (explicit.is_none()
                    && event["error"]["param"] == "response.create"
                    && self.context_generation_waiting.as_deref()
                        == Some(request.event_id.as_str()))
        })?;
        Some((
            request.clone(),
            self.context_generation_waiting.as_deref() == Some(request.event_id.as_str()),
        ))
    }

    pub(super) fn observe_context_rejection(&self, event: Value) {
        // The context writer classified this optional request refusal before
        // the audio decoder. Publish its diagnostic without failing playback.
        self.events.publish_server_event(Arc::new(event));
    }

    pub(super) fn publish_context_refresh_event(&self, event: Value) {
        // Local delivery observations share the raw media stream so the owning
        // application can record ACK outcomes even when SDK tracing is filtered.
        // The context writer supplies only IDs, counters and fixed status text.
        self.events.publish_server_event(Arc::new(event));
    }

    pub(super) fn owns_control_error(&self, event: &Value) -> bool {
        self.rejected_notice(event).is_some()
            || self.is_cancel_rejection(event)
            || self.context_response_rejection(event).is_some()
            || self.text_response_rejection(event)
            || self.retired_text_rejection(event)
    }

    fn retired_text_rejection(&self, event: &Value) -> bool {
        // A deferred create and the next create advance independently of late
        // provider error delivery. Ignore only a refusal with an exact retired
        // client event ID, so it cannot reject the newer request or voice.
        event["type"] == "error"
            && event["error"]["type"] == "invalid_request_error"
            && event["error"]["event_id"].as_str().is_some_and(|id| {
                self.retired_text_creates
                    .iter()
                    .any(|retired| retired == id)
            })
    }

    fn text_response_rejection(&self, event: &Value) -> bool {
        // The local explicit create owns one pending response. A rejection of
        // this event must fail only that input, while unrelated transport or
        // provider errors keep their existing failure behavior.
        event["type"] == "error"
            && event["error"]["type"] == "invalid_request_error"
            && self.text_turn.as_ref().is_some_and(|turn| {
                turn.request.user
                    && turn.response_id.is_none()
                    && !turn.continuation
                    && (event["error"]["event_id"].as_str()
                        == Some(turn.response_event_id.as_str())
                        || (event["error"]["event_id"].is_null()
                            && event["error"]["param"] == "response.create"))
            })
    }

    pub(super) fn observe(&mut self, event: &Value) -> Result<(), Fault> {
        if self.events.raw.receiver_count() > 0 {
            self.events.publish_server_event(Arc::new(event.clone()));
        }
        if self.is_cancel_rejection(event) {
            // Qwen may reject cancel while the old response is completing or
            // when automatic generation cannot be cancelled by a client. Only
            // this control request failed. Keep processing audio and the reply.
            if let Some(request) = self.interrupt.take() {
                let _ = request
                    .result
                    .send(Err(super::output::server_error(&event["error"])));
            }
            return Ok(());
        }
        match event["type"].as_str() {
            Some("input_audio_buffer.speech_started") => {
                self.speech_sequence = self.speech_sequence.wrapping_add(1);
                self.text_correlation.send_replace(None);
                self.input_speech = true;
                self.awaiting_speech_response = true;
                // User speech can win the race with optional text insertion.
                // Do not request a second response inside that user's turn.
                if self.notice.as_ref().is_some_and(|notice| !notice.requested) {
                    self.abandon_notice("user_speech", true);
                }
            }
            Some("input_audio_buffer.speech_stopped") => {
                self.input_speech = false;
                if event["reason"] == "turn_invalid" {
                    self.awaiting_speech_response = false;
                }
            }
            Some("conversation.item.created") => {
                let item_id = event["item"]["id"].as_str();
                if let Some(notice) = self
                    .notice
                    .as_mut()
                    .filter(|notice| Some(notice.item_id.as_str()) == item_id && !notice.created)
                {
                    notice.created = true;
                    self.spoken_items.push(notice.item_id.clone());
                    tracing::info!(event = "qwen_spoken_notice_ack", item_id = notice.item_id);
                } else if let Some(index) = self
                    .abandoned_notices
                    .iter()
                    .position(|notice| Some(notice.item_id.as_str()) == item_id)
                {
                    // A late ACK proves the abandoned optional item exists.
                    // Retire it through optional cleanup, without generating
                    // or retrying the discarded notification.
                    let notice = self
                        .abandoned_notices
                        .remove(index)
                        .expect("matched notice");
                    self.queue_notice_cleanup(notice.item_id);
                }
            }
            Some("conversation.item.deleted") => {
                self.notice_delete_events
                    .retain(|(_, item_id)| Some(item_id.as_str()) != event["item_id"].as_str());
            }
            Some("response.created") => {
                if let Some(notice) = self.notice.as_ref().filter(|notice| notice.requested) {
                    tracing::info!(
                        event = "qwen_spoken_notice_response",
                        item_id = notice.item_id,
                        response_id = event["response"]["id"].as_str()
                    );
                }
            }
            Some("response.done") => {
                self.text_cancelled_response = None;
                self.context_generation_waiting = None;
                self.spoken_pending = false;
                self.notice = None;
            }
            _ => {}
        }
        self.observe_text_turn(event);
        if self.mode == Mode::SmartTurnV2 {
            match event["type"].as_str() {
                Some("input_audio_buffer.speech_started") if self.decoder.is_responding() => {
                    // Qwen owns interruption. Once it detects the new speech,
                    // wait for the former response's done before forwarding
                    // more audio that could trigger a reply on the old Frame.
                    self.waiting_for_interruption = true;
                }
                Some("response.done" | "input_audio_buffer.speech_stopped") => {
                    self.waiting_for_interruption = false;
                }
                _ => {}
            }
        }
        if let Some(event) = self.decoder.decode(event)? {
            match &event {
                QwenRealtimeAudioEvent::InputCommitted { item_id } => {
                    if self.mode == Mode::SmartTurnV2 {
                        // The server owns utterance commitment. Bound optional
                        // transcript drain state without rejecting a server turn.
                        if self.pending_asr.len() < MAX_PENDING_TRANSCRIPTIONS {
                            self.pending_asr.insert(item_id.clone());
                        }
                        self.awaiting_commit = true;
                        self.events.publish(event);
                        return Ok(());
                    }
                    if self.pending_asr.len() >= MAX_PENDING_TRANSCRIPTIONS {
                        return Err(Fault::retryable(Code::Limit, Reason::OutputLimit));
                    }
                    if !self.awaiting_commit || !self.pending_asr.insert(item_id.clone()) {
                        return Err(transport::protocol_fault());
                    }
                    self.awaiting_commit = false;
                    self.awaiting_speech_response = false;
                    self.committed_turn = true;
                }
                QwenRealtimeAudioEvent::ResponseStarted { response_id }
                    if self.mode == Mode::SmartTurnV2 =>
                {
                    self.awaiting_commit = false;
                    self.awaiting_speech_response = false;
                    self.active_response.send_replace(Some(response_id.clone()));
                }
                QwenRealtimeAudioEvent::ResponseEnded {
                    response_id,
                    status,
                    ..
                } => {
                    self.active_response.send_replace(None);
                    if self
                        .interrupt
                        .as_ref()
                        .is_some_and(|request| request.response_id == *response_id)
                    {
                        if let Some(request) = self.interrupt.take() {
                            let _ = request
                                .result
                                .send(Ok(*status == QwenRealtimeResponseStatus::Cancelled));
                        }
                    }
                }
                QwenRealtimeAudioEvent::InputTranscriptDone { item_id, .. }
                | QwenRealtimeAudioEvent::InputTranscriptFailed { item_id, .. } => {
                    self.pending_asr.remove(item_id);
                }
                _ => {}
            }
            self.events.publish(event);
        }
        Ok(())
    }

    /// Waits for the next committed user utterance or the end of input.
    pub(super) async fn wait_for_turn(&mut self, socket: &mut Socket) -> Result<bool, Fault> {
        if self.commit_pending {
            self.commit(socket).await?;
        }
        loop {
            if self.committed_turn {
                return Ok(true);
            }
            if self.finished {
                self.drain_transcripts(socket).await?;
                return Ok(false);
            }
            let (event, _) = self.receive(socket, false).await?;
            let kind = event["type"]
                .as_str()
                .ok_or_else(transport::protocol_fault)?;
            if kind.starts_with("response.") {
                return Err(transport::protocol_fault());
            }
        }
    }

    pub(super) fn begin_turn(&mut self) {
        self.committed_turn = false;
    }

    pub(super) fn is_finished(&self) -> bool {
        // Finish closes the input producer, while the socket still owns every
        // accepted text and completion cue. Close only after those queued
        // deliveries settle; otherwise Finish would drop their receipts.
        self.finished
            && self.spoken.is_empty()
            && self.user_texts.is_empty()
            && self.context_response_pending.is_empty()
            && self.prepared_text.is_none()
            && self.text_turn.is_none()
            && self.controls.is_empty()
    }

    pub(super) fn take_spoken_items(&mut self) -> Vec<String> {
        std::mem::take(&mut self.spoken_items)
    }

    async fn drain_transcripts(&mut self, socket: &mut Socket) -> Result<(), Fault> {
        tokio::time::timeout(TRANSCRIPTION_DRAIN, async {
            while !self.pending_asr.is_empty() {
                let (event, _) = transport::receive_unbounded(socket).await?;
                self.observe(&event)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| Fault::retryable(Code::Unavailable, Reason::StreamTimeout))?
    }

    /// Services microphone input alongside the provider's one socket reader.
    /// A later utterance's commit waits until current tool calls are closed.
    pub(super) async fn receive(
        &mut self,
        socket: &mut Socket,
        responding: bool,
    ) -> Result<(Value, usize), Fault> {
        loop {
            // New text can reach the queue after an earlier response.create
            // but before Qwen assigns its response ID. The socket reader keeps
            // this intent until that response becomes cancellable. Deferring
            // an ACKed item after a VAD race does not set this new-input intent.
            if self.interrupt_for_text {
                if let Some(response_id) = self.decoder.active_response().map(str::to_owned) {
                    if self.text_cancelled_response.as_deref() != Some(response_id.as_str()) {
                        transport::send(socket, json!({"type":"response.cancel"}).to_string())
                            .await?;
                        self.publish_context_refresh_event(json!({
                            "type":"agentview.text_response_cancel_sent", "response_id":response_id,
                        }));
                        self.text_cancelled_response = Some(response_id);
                    }
                    self.interrupt_for_text = false;
                }
            }
            if let Some(item_id) = self.notice_cleanup.pop_front() {
                // An acknowledged optional item lost the race to user speech.
                // Remove it before forwarding more input where possible; its
                // own ACK/error is observed independently and never retried.
                let event_id = format!("discard_{item_id}");
                transport::send(
                    socket,
                    json!({"type":"conversation.item.delete",
                    "event_id":event_id, "item_id":item_id})
                    .to_string(),
                )
                .await?;
                if self.notice_delete_events.len() == ABANDONED_NOTICES {
                    self.notice_delete_events.pop_front();
                }
                self.notice_delete_events.push_back((event_id, item_id));
                continue;
            }
            // The socket reader owns the response and VAD state. A notification
            // received during a reply stays queued until the following normal
            // Frame; a user utterance already in progress takes precedence.
            let idle = !responding
                && !self.decoder.is_responding()
                && !self.input_speech
                && !self.awaiting_speech_response
                && !self.awaiting_commit;
            let can_speak = idle && !self.spoken_pending;
            if can_speak && !self.user_texts.is_empty() {
                self.prepare_text_frame();
            }
            if self.mode == Mode::SmartTurnV2
                && can_speak
                && (self.context_refresh_pending || self.prepared_text.is_some())
            {
                return Ok((json!({"type":"agentview.context_refresh"}), 0));
            }
            let notice_ready = idle
                && self
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.created && !notice.requested);
            let notice_deadline = self
                .notice
                .as_ref()
                .filter(|notice| !notice.created)
                .map_or_else(
                    || Instant::now() + NOTICE_ACK_TIMEOUT,
                    |notice| notice.deadline,
                );
            if self.is_finished() && !responding && !self.spoken_pending {
                return Ok((json!({"type":"agentview.input_finished"}), 0));
            }
            let hold_microphone =
                self.commit_pending || self.awaiting_commit || self.waiting_for_interruption;
            tokio::select! {
                biased;
                // Microphone forwarding may be paused behind an old response,
                // and the server may continuously have audio ready. This
                // separate bounded control lane remains readable in both cases;
                // accepting text can cancel the response that owns that wait.
                control = self.controls.recv(), if !self.controls_closed => match control {
                    Some(TextControl::User(text)) => self.queue_user_text(text),
                    Some(TextControl::Context(text)) => {
                        self.context_refresh_pending = true;
                        self.context_response_pending.push_back(text);
                    }
                    None => self.controls_closed = true,
                },
                result = receive(socket, responding || self.awaiting_commit || self.spoken_pending) => {
                    let (event, bytes) = result?;
                    return self.admit(event, bytes);
                }
                () = tokio::time::sleep_until(notice_deadline), if self.notice.as_ref().is_some_and(|notice| !notice.created) => {
                    // The provider did not accept this optional text in time.
                    // Drop only the notification and keep microphone/reply
                    // processing alive; a later matching ACK is cleanup evidence.
                    self.abandon_notice("create_timeout", true);
                }
                () = std::future::ready(()), if notice_ready => {
                    let notice = self.notice.as_mut().expect("acknowledged notification");
                    notice.requested = true;
                    let event_id = notice.response_event_id.clone();
                    transport::send(socket, json!({
                        "type": "response.create", "event_id":event_id,
                    }).to_string()).await?;
                }
                () = std::future::ready(()), if can_speak && !self.spoken.is_empty() => {
                    let spoken = self.spoken.pop_front().expect("queued notification");
                    static NEXT_SPOKEN_ITEM: AtomicU64 = AtomicU64::new(1);
                    let item_id = format!("msg_agentview_notice_{}", NEXT_SPOKEN_ITEM.fetch_add(1, Ordering::Relaxed));
                    let create_event_id = format!("create_{item_id}");
                    let response_event_id = format!("respond_{item_id}");
                    transport::send(socket, json!({
                        "type": "conversation.item.create", "event_id":create_event_id, "item": {
                            "id": item_id, "type": "message", "role": "user",
                            "content": [{"type":"input_text", "text": format!(
                                "这是服务端已生成的通知。现在只逐字朗读下面的通知正文，不回答先前的问题，不调用工具，也不要添加前后说明：\n\n{}", spoken.text,
                            )}],
                        },
                    }).to_string()).await?;
                    self.notice = Some(Notice { item_id, create_event_id, response_event_id,
                        deadline:Instant::now() + NOTICE_ACK_TIMEOUT, created:false, requested:false });
                    self.spoken_pending = true;
                }
                input = self.input.recv(), if !self.finished && !hold_microphone => match input {
                    Some(Input::Append(audio)) => {
                        // Bound the open utterance independently of the ingress queue.
                        if self.mode == Mode::ManualAudio {
                            self.buffered_bytes = self.buffered_bytes.saturating_add(audio.len());
                            if self.buffered_bytes > 4 * 1024 * 1024 {
                                return Err(Fault::retryable(Code::Limit, Reason::RequestPreparation));
                            }
                        }
                        transport::send(socket, json!({
                            "type": "input_audio_buffer.append", "audio": audio,
                        }).to_string()).await?;
                    }
                    Some(Input::SetMuted(_)) => {},
                    Some(Input::Speak(spoken)) => self.spoken.push_back(spoken),
                    Some(Input::UserText(text)) => {
                        self.queue_user_text(text);
                    },
                    Some(Input::RefreshContext) => self.context_refresh_pending = true,
                    Some(Input::RefreshContextAndRespond(instruction)) => {
                        self.context_refresh_pending = true;
                        self.context_response_pending.push_back(instruction);
                    }
                    Some(Input::Commit) if responding => self.commit_pending = true,
                    Some(Input::Commit) => self.commit(socket).await?,
                    Some(Input::Clear) => {
                        self.buffered_bytes = 0;
                        transport::send(socket, json!({"type": "input_audio_buffer.clear"}).to_string()).await?;
                    }
                    Some(Input::Interrupt(request)) => {
                        // The single socket reader owns the active response. An
                        // old request can arrive just as a newer reply starts.
                        // Drop only that stale control so it never cancels the new reply.
                        if self.decoder.active_response() != Some(request.response_id.as_str()) {
                            let _ = request.result.send(Ok(false));
                        } else if self.interrupt.is_some() {
                            let _ = request.result.send(Err(Fault::retryable(Code::Unavailable, Reason::UpstreamRejected)));
                        } else {
                            transport::send(socket, json!({"type": "response.cancel"}).to_string()).await?;
                            self.interrupt = Some(request);
                        }
                    }
                    Some(Input::Finish) | None => {
                        self.finished = true;
                        self.input.close();
                        if !responding && self.spoken.is_empty() && !self.spoken_pending {
                            // Allow the idle driver to settle pending ASR with its own deadline.
                            if self.mode == Mode::SmartTurnV2 {
                                // Late transcripts are observations and do not prevent shutdown.
                                let _ = self.drain_transcripts(socket).await;
                            }
                            return Ok((json!({"type": "agentview.input_finished"}), 0));
                        }
                    }
                }
            }
        }
    }

    fn queue_user_text(&mut self, text: TextRequest) {
        self.publish_context_refresh_event(json!({
            "type":"agentview.text_input_queued", "correlation":text.correlation,
        }));
        self.user_texts.push_back(text);
        self.context_refresh_pending = true;
        self.interrupt_for_text = true;
    }

    pub(super) fn admit(&mut self, event: Value, bytes: usize) -> Result<(Value, usize), Fault> {
        if self.retired_text_rejection(&event) {
            self.events.publish_server_event(Arc::new(event));
            return Ok((json!({"type":"agentview.text_create_retired"}), 0));
        }
        if self.text_response_rejection(&event) {
            self.events.publish_server_event(Arc::new(event.clone()));
            if let Some(turn) = self.text_turn.take() {
                // A VAD event after this create makes its refusal a speech
                // race, rather than a refusal of the original text input.
                // Keep that ACKed input queued, without inserting it again.
                if turn.speech_sequence != self.speech_sequence {
                    self.defer_unbound_text(turn);
                } else {
                    self.fail_text(
                        turn.request,
                        "provider_rejected",
                        super::output::server_error(&event["error"]),
                    );
                }
            }
            self.context_generation_waiting = None;
            return Ok((json!({"type":"agentview.text_response_rejected"}), 0));
        }
        // This optional response can lose a race after all context ACKs but
        // before the server handles response.create. Only its owned rejection
        // retires the cue and wait; late errors cannot cancel a newer response.
        if let Some((request, current)) = self.context_response_rejection(&event) {
            self.events.publish_server_event(Arc::new(event));
            self.publish_context_refresh_event(json!({
                "type":"agentview.context_response_skipped",
                "provider_session_id":request.provider_session_id,
                "frame_revision":format!("{:?}", request.revision),
                "reason":"provider_rejected",
            }));
            if current {
                self.context_generation_waiting = None;
                if let Some(turn) = self.text_turn.take() {
                    self.fail_text(
                        turn.request,
                        "provider_rejected",
                        Fault::retryable(Code::Rejected, Reason::RequestPreparation),
                    );
                }
            }
            return Ok((
                json!({"type":"agentview.context_response_rejected", "cancel_wait":current,
                "cue_item_id":if current { request.cue_item } else { None }}),
                0,
            ));
        }
        // Optional notification requests are the only client item/create-response
        // commands outstanding in this media phase. Match an explicit request
        // reference, or its create parameter while waiting for this exact ACK;
        // unrelated provider errors retain their existing failure semantics.
        if let Some(item_id) = self.rejected_notice(&event) {
            self.events.publish_server_event(Arc::new(event));
            if self
                .notice
                .as_ref()
                .is_some_and(|notice| notice.item_id == item_id)
            {
                self.abandon_notice("provider_rejected", false);
            } else {
                tracing::warn!(event = "qwen_spoken_notice_late_rejection", item_id);
            }
            return Ok((json!({"type":"agentview.notice_rejected"}), 0));
        }
        // Optional notice ACKs can arrive while the ordinary Frame writer waits
        // for a Persona/context create or delete ACK. Classify our owned IDs
        // before observation removes their records, then consume only those
        // ACKs so they cannot satisfy a different Frame command's waiter.
        let notice_ack = match event["type"].as_str() {
            Some("conversation.item.created") => event["item"]["id"].as_str().is_some_and(|id| {
                self.notice
                    .as_ref()
                    .is_some_and(|notice| notice.item_id == id)
                    || self
                        .abandoned_notices
                        .iter()
                        .any(|notice| notice.item_id == id)
                    || self.notice_cleanup.iter().any(|item_id| item_id == id)
                    || self
                        .notice_delete_events
                        .iter()
                        .any(|(_, item_id)| item_id == id)
            }),
            Some("conversation.item.deleted") => self
                .notice_delete_events
                .iter()
                .any(|(_, item_id)| Some(item_id.as_str()) == event["item_id"].as_str()),
            _ => false,
        };
        self.observe(&event)?;
        if notice_ack {
            Ok((json!({"type":"agentview.notice_ack"}), 0))
        } else if self.is_cancel_rejection(&event) {
            Ok((json!({"type": "agentview.cancel_rejected"}), 0))
        } else {
            Ok((event, bytes))
        }
    }

    fn rejected_notice(&self, event: &Value) -> Option<String> {
        if event["type"] != "error" {
            return None;
        }
        let reference = event["error"]["event_id"]
            .as_str()
            .or_else(|| event["error"]["request_event_id"].as_str());
        let matched = |notice: &Notice| {
            reference
                .is_some_and(|id| id == notice.create_event_id || id == notice.response_event_id)
        };
        let current = self.notice.as_ref().filter(|notice| {
            matched(notice)
                || (reference.is_none()
                    && !notice.created
                    && event["error"]["param"] == "conversation.item.create")
                || (reference.is_none()
                    && notice.requested
                    && !self.decoder.is_responding()
                    && event["error"]["param"] == "response.create")
        });
        current
            .or_else(|| self.abandoned_notices.iter().find(|notice| matched(notice)))
            .map(|notice| notice.item_id.clone())
            .or_else(|| {
                self.notice_delete_events
                    .iter()
                    .find(|(id, _)| Some(id.as_str()) == reference)
                    .map(|(_, item_id)| item_id.clone())
            })
    }

    fn abandon_notice(&mut self, reason: &'static str, retain_late_ack: bool) {
        if let Some(notice) = self.notice.take() {
            tracing::warn!(
                event = "qwen_spoken_notice_skipped",
                item_id = notice.item_id,
                reason
            );
            if notice.created {
                self.spoken_items
                    .retain(|item_id| *item_id != notice.item_id);
                self.queue_notice_cleanup(notice.item_id);
            } else if retain_late_ack && !notice.created {
                if self.abandoned_notices.len() == ABANDONED_NOTICES {
                    self.abandoned_notices.pop_front();
                }
                self.abandoned_notices.push_back(notice);
            }
        }
        self.spoken_pending = false;
    }

    fn queue_notice_cleanup(&mut self, item_id: String) {
        if self.notice_cleanup.len() < ABANDONED_NOTICES {
            self.notice_cleanup.push_back(item_id);
        } else {
            // Keep confirmed cleanup evidence on the existing response-boundary
            // cleanup path if several late ACKs arrive together.
            self.spoken_items.push(item_id);
        }
    }

    fn is_cancel_rejection(&self, event: &Value) -> bool {
        self.mode == Mode::SmartTurnV2
            && event["type"] == "error"
            && event["error"]["param"] == "response.cancel"
    }

    async fn commit(&mut self, socket: &mut Socket) -> Result<(), Fault> {
        transport::send(
            socket,
            json!({"type": "input_audio_buffer.commit"}).to_string(),
        )
        .await?;
        self.buffered_bytes = 0;
        self.commit_pending = false;
        self.awaiting_commit = true;
        Ok(())
    }
}

fn response_status(status: QwenRealtimeResponseStatus) -> &'static str {
    match status {
        QwenRealtimeResponseStatus::Completed => "completed",
        QwenRealtimeResponseStatus::Cancelled => "cancelled",
        QwenRealtimeResponseStatus::Incomplete => "incomplete",
        QwenRealtimeResponseStatus::Failed => "failed",
    }
}

async fn receive(socket: &mut Socket, bounded: bool) -> Result<(Value, usize), Fault> {
    if bounded {
        transport::receive(socket).await
    } else {
        transport::receive_unbounded(socket).await
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    fn notice(id: &str) -> Notice {
        Notice {
            item_id: id.to_owned(),
            create_event_id: format!("create_{id}"),
            response_event_id: format!("respond_{id}"),
            deadline: Instant::now() + NOTICE_ACK_TIMEOUT,
            created: false,
            requested: false,
        }
    }

    fn rejected(event_id: &str) -> Value {
        json!({"type":"error", "error": {
            "type":"invalid_request_error", "code":"invalid_parameter",
            "param":"conversation.item.create", "event_id":event_id,
            "message":"Optional text rejected",
        }})
    }

    #[test]
    fn late_rejection_does_not_discard_a_newer_notice() {
        let (mut media, _input, _events) = Media::new(Mode::SmartTurnV2);
        media.notice = Some(notice("old"));
        media.abandon_notice("create_timeout", true);
        media.notice = Some(notice("current"));
        media.spoken_pending = true;

        let (event, _) = media.admit(rejected("create_old"), 10).unwrap();
        assert_eq!(event["type"], "agentview.notice_rejected");
        assert_eq!(media.notice.as_ref().unwrap().item_id, "current");
        assert!(media.spoken_pending);
    }

    #[test]
    fn explicit_other_request_never_matches_optional_parameter_fallback() {
        let (mut media, _input, _events) = Media::new(Mode::SmartTurnV2);
        media.notice = Some(notice("current"));
        media.spoken_pending = true;

        assert!(media.admit(rejected("frame_context_create"), 10).is_err());
        assert_eq!(media.notice.as_ref().unwrap().item_id, "current");
        assert!(media.spoken_pending);
    }

    #[test]
    fn rejected_optional_create_keeps_user_input_usable() {
        let (mut media, _input, _events) = Media::new(Mode::SmartTurnV2);
        media.notice = Some(notice("optional"));
        media.spoken_pending = true;
        media.admit(rejected("create_optional"), 10).unwrap();
        assert!(media.notice.is_none());
        assert!(!media.spoken_pending);
        media
            .admit(json!({"type":"input_audio_buffer.speech_started"}), 10)
            .unwrap();
        assert!(media.input_speech);
        assert!(media.awaiting_speech_response);
        media
            .admit(
                json!({"type":"input_audio_buffer.speech_stopped", "reason":"turn_invalid"}),
                10,
            )
            .unwrap();
        assert!(!media.input_speech);
        assert!(!media.awaiting_speech_response);
    }

    #[test]
    fn speech_after_ack_cleans_the_item_before_optional_generation() {
        let (mut media, _input, _events) = Media::new(Mode::SmartTurnV2);
        media.notice = Some(notice("optional"));
        media.spoken_pending = true;
        media
            .admit(
                json!({"type":"conversation.item.created", "item":{"id":"optional"}}),
                10,
            )
            .unwrap();
        assert!(media.notice.as_ref().unwrap().created);
        media
            .admit(json!({"type":"input_audio_buffer.speech_started"}), 10)
            .unwrap();
        assert!(media.notice.is_none());
        assert!(!media.spoken_pending);
        assert!(media.spoken_items.is_empty());
        assert_eq!(media.notice_cleanup.front().unwrap(), "optional");
    }

    #[test]
    fn late_optional_acks_cannot_satisfy_ordinary_frame_waiters() {
        let (mut media, _input, _events) = Media::new(Mode::SmartTurnV2);
        media.notice = Some(notice("abandoned"));
        media.abandon_notice("create_timeout", true);
        media.notice = Some(notice("current"));
        media.spoken_pending = true;
        let (late_create, _) = media
            .admit(
                json!({"type":"conversation.item.created", "item":{"id":"abandoned"}}),
                10,
            )
            .unwrap();
        assert_eq!(late_create["type"], "agentview.notice_ack");
        assert_eq!(media.notice_cleanup.pop_front().unwrap(), "abandoned");
        media
            .notice_delete_events
            .push_back(("discard_abandoned".into(), "abandoned".into()));
        let (late_delete, _) = media
            .admit(
                json!({"type":"conversation.item.deleted", "item_id":"abandoned"}),
                10,
            )
            .unwrap();
        assert_eq!(late_delete["type"], "agentview.notice_ack");
        assert!(media.notice_delete_events.is_empty());
        assert_eq!(media.notice.as_ref().unwrap().item_id, "current");
        assert!(media.spoken_pending);

        for event in [
            json!({"type":"conversation.item.created", "item":{"id":"frame_context"}}),
            json!({"type":"conversation.item.deleted", "item_id":"ordinary_transient"}),
        ] {
            assert_eq!(media.admit(event.clone(), 10).unwrap().0, event);
        }
    }
}
