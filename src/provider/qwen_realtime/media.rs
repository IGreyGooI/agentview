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

use crate::component::execution::{
    ReactionPortFault as Fault, ReactionPortFaultCode as Code, ReactionPortFaultReason as Reason,
};

use super::{
    audio::{AudioDecoder, QwenRealtimeAudioEvent, QwenRealtimeResponseStatus},
    transport::{self, Socket},
    Mode,
};

const INPUT_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const MAX_PENDING_TRANSCRIPTIONS: usize = 128;
const TRANSCRIPTION_DRAIN: Duration = Duration::from_secs(15);

pub(crate) enum Input {
    Append(String),
    SetMuted(bool),
    Speak(SpokenText),
    Commit,
    Clear,
    Interrupt(Interrupt),
    Finish,
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
pub struct QwenRealtimeMediaInput(mpsc::Sender<Input>);

impl QwenRealtimeMediaInput {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<Input>) {
        let (sender, receiver) = mpsc::channel(INPUT_CAPACITY);
        (Self(sender), receiver)
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
        self.0.send(input).await.map_err(|_| closed())
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

/// An independent subscription to complete Qwen server events.
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
    input_speech: bool,
    awaiting_speech_response: bool,
    finished: bool,
}

impl Media {
    pub(super) fn new(mode: Mode) -> (Self, QwenRealtimeMediaInput, QwenRealtimeMediaEvents) {
        let (sender, input) = QwenRealtimeMediaInput::channel();
        let (events, media_events) = MediaEventsSender::new();
        let (active_response, _) = watch::channel(None);
        (
            Self {
                mode,
                input,
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
                input_speech: false,
                awaiting_speech_response: false,
                finished: false,
            },
            sender,
            media_events,
        )
    }

    pub(super) fn subscribe_active_response(&self) -> watch::Receiver<Option<String>> {
        self.active_response.subscribe()
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
                self.input_speech = true;
                self.awaiting_speech_response = true;
            }
            Some("input_audio_buffer.speech_stopped") => self.input_speech = false,
            Some("response.done") => self.spoken_pending = false,
            _ => {}
        }
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
        self.finished && self.spoken.is_empty()
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
            // The socket reader owns the response and VAD state. A notification
            // received during a reply stays queued until the following normal
            // Frame; a user utterance already in progress takes precedence.
            let can_speak = !responding
                && !self.decoder.is_responding()
                && !self.spoken_pending
                && !self.input_speech
                && !self.awaiting_speech_response
                && !self.awaiting_commit;
            if self.finished && !responding && !self.spoken_pending && self.spoken.is_empty() {
                return Ok((json!({"type":"agentview.input_finished"}), 0));
            }
            if self.finished && (!can_speak || self.spoken.is_empty())
                || self.commit_pending
                || self.awaiting_commit
                || self.waiting_for_interruption
            {
                return self
                    .read(
                        socket,
                        responding || self.awaiting_commit || self.spoken_pending,
                    )
                    .await;
            }
            tokio::select! {
                biased;
                result = receive(socket, responding || self.spoken_pending) => {
                    let (event, bytes) = result?;
                    return self.admit(event, bytes);
                }
                () = std::future::ready(()), if can_speak && !self.spoken.is_empty() => {
                    let spoken = self.spoken.pop_front().expect("queued notification");
                    static NEXT_SPOKEN_ITEM: AtomicU64 = AtomicU64::new(1);
                    let item_id = format!("msg_agentview_notice_{}", NEXT_SPOKEN_ITEM.fetch_add(1, Ordering::Relaxed));
                    transport::send(socket, json!({
                        "type": "conversation.item.create", "item": {
                            "id": item_id, "type": "message", "role": "system",
                            "content": [{"type":"input_text", "text": format!(
                                "请朗读以下服务端通知，不添加前后说明：\n\n{}", spoken.text,
                            )}],
                        },
                    }).to_string()).await?;
                    self.spoken_items.push(item_id);
                    self.spoken_pending = true;
                    transport::send(socket, json!({"type": "response.create"}).to_string()).await?;
                }
                input = self.input.recv(), if !self.finished => match input {
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

    async fn read(&mut self, socket: &mut Socket, bounded: bool) -> Result<(Value, usize), Fault> {
        let (event, bytes) = receive(socket, bounded).await?;
        self.admit(event, bytes)
    }

    pub(super) fn admit(&mut self, event: Value, bytes: usize) -> Result<(Value, usize), Fault> {
        self.observe(&event)?;
        if self.is_cancel_rejection(&event) {
            Ok((json!({"type": "agentview.cancel_rejected"}), 0))
        } else {
            Ok((event, bytes))
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

async fn receive(socket: &mut Socket, bounded: bool) -> Result<(Value, usize), Fault> {
    if bounded {
        transport::receive(socket).await
    } else {
        transport::receive_unbounded(socket).await
    }
}
