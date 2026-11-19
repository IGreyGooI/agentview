use std::{collections::HashSet, sync::Arc, time::Duration};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::component::execution::{
    ReactionPortFault as Fault, ReactionPortFaultCode as Code, ReactionPortFaultReason as Reason,
};

use super::{
    audio::{AudioDecoder, QwenRealtimeAudioEvent},
    transport::{self, Socket},
};

const INPUT_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const MAX_PENDING_TRANSCRIPTIONS: usize = 128;
const TRANSCRIPTION_DRAIN: Duration = Duration::from_secs(15);

enum Input {
    Append(String),
    Commit,
    Clear,
    Finish,
}

/// Cloneable, bounded input to a mounted Qwen Realtime voice session.
#[derive(Clone)]
pub struct QwenRealtimeMediaInput(mpsc::Sender<Input>);

impl QwenRealtimeMediaInput {
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

    async fn send(&self, input: Input) -> Result<(), Fault> {
        self.0.send(input).await.map_err(|_| closed())
    }
}

/// Receives PCM16, transcripts and other server events for one mounted session.
pub struct QwenRealtimeMediaEvents {
    typed: broadcast::Receiver<QwenRealtimeAudioEvent>,
    raw: broadcast::Receiver<Arc<Value>>,
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

fn closed() -> Fault {
    Fault::retryable(Code::Unavailable, Reason::StreamTransport)
}

pub(super) struct Media {
    input: mpsc::Receiver<Input>,
    events: broadcast::Sender<QwenRealtimeAudioEvent>,
    raw: broadcast::Sender<Arc<Value>>,
    decoder: AudioDecoder,
    pending_asr: HashSet<String>,
    buffered_bytes: usize,
    commit_pending: bool,
    awaiting_commit: bool,
    committed_turn: bool,
    finished: bool,
}

impl Media {
    pub(super) fn new() -> (Self, QwenRealtimeMediaInput, QwenRealtimeMediaEvents) {
        let (sender, input) = mpsc::channel(INPUT_CAPACITY);
        let (events, typed) = broadcast::channel(EVENT_CAPACITY);
        let (raw, server_events) = broadcast::channel(EVENT_CAPACITY);
        (
            Self {
                input,
                events,
                raw,
                decoder: AudioDecoder::default(),
                pending_asr: HashSet::new(),
                buffered_bytes: 0,
                commit_pending: false,
                awaiting_commit: false,
                committed_turn: false,
                finished: false,
            },
            QwenRealtimeMediaInput(sender),
            QwenRealtimeMediaEvents {
                typed,
                raw: server_events,
            },
        )
    }

    pub(super) fn observe(&mut self, event: &Value) -> Result<(), Fault> {
        if self.raw.receiver_count() > 0 {
            let _ = self.raw.send(Arc::new(event.clone()));
        }
        if let Some(event) = self.decoder.decode(event)? {
            match &event {
                QwenRealtimeAudioEvent::InputCommitted { item_id } => {
                    if self.pending_asr.len() >= MAX_PENDING_TRANSCRIPTIONS {
                        return Err(Fault::retryable(Code::Limit, Reason::OutputLimit));
                    }
                    if !self.awaiting_commit || !self.pending_asr.insert(item_id.clone()) {
                        return Err(transport::protocol_fault());
                    }
                    self.awaiting_commit = false;
                    self.committed_turn = true;
                }
                QwenRealtimeAudioEvent::InputTranscriptDone { item_id, .. }
                | QwenRealtimeAudioEvent::InputTranscriptFailed { item_id, .. } => {
                    self.pending_asr.remove(item_id);
                }
                _ => {}
            }
            let _ = self.events.send(event);
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
                let drained = tokio::time::timeout(TRANSCRIPTION_DRAIN, async {
                    while !self.pending_asr.is_empty() {
                        let (event, _) = transport::receive_unbounded(socket).await?;
                        self.observe(&event)?;
                    }
                    Ok::<(), Fault>(())
                })
                .await
                .map_err(|_| Fault::retryable(Code::Unavailable, Reason::StreamTimeout))?;
                drained?;
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

    /// Services microphone input alongside the provider's one socket reader.
    /// A later utterance's commit waits until current tool calls are closed.
    pub(super) async fn receive(
        &mut self,
        socket: &mut Socket,
        responding: bool,
    ) -> Result<(Value, usize), Fault> {
        loop {
            if self.finished || self.commit_pending || self.awaiting_commit {
                return self.read(socket, responding || self.awaiting_commit).await;
            }
            tokio::select! {
                result = receive(socket, responding) => {
                    let (event, bytes) = result?;
                    self.observe(&event)?;
                    return Ok((event, bytes));
                }
                input = self.input.recv() => match input {
                    Some(Input::Append(audio)) => {
                        // Bound the open utterance independently of the ingress queue.
                        self.buffered_bytes = self.buffered_bytes.saturating_add(audio.len());
                        if self.buffered_bytes > 4 * 1024 * 1024 {
                            return Err(Fault::retryable(Code::Limit, Reason::RequestPreparation));
                        }
                        transport::send(socket, json!({
                            "type": "input_audio_buffer.append", "audio": audio,
                        }).to_string()).await?;
                    }
                    Some(Input::Commit) if responding => self.commit_pending = true,
                    Some(Input::Commit) => self.commit(socket).await?,
                    Some(Input::Clear) => {
                        self.buffered_bytes = 0;
                        transport::send(socket, json!({"type": "input_audio_buffer.clear"}).to_string()).await?;
                    }
                    Some(Input::Finish) | None => {
                        self.finished = true;
                        self.input.close();
                        if !responding {
                            // Allow the idle driver to settle pending ASR with its own deadline.
                            return Ok((json!({"type": "agentview.input_finished"}), 0));
                        }
                    }
                }
            }
        }
    }

    async fn read(&mut self, socket: &mut Socket, bounded: bool) -> Result<(Value, usize), Fault> {
        let (event, bytes) = receive(socket, bounded).await?;
        self.observe(&event)?;
        Ok((event, bytes))
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
