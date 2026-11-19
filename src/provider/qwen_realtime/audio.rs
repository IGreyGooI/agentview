//! Typed media events and PCM16 encoding for Qwen Realtime.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;

use crate::component::execution::{
    ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason,
};

use super::{output::server_error, transport};

/// Sample rate for signed little-endian mono PCM16 input.
pub const QWEN_REALTIME_INPUT_SAMPLE_RATE_HZ: u32 = 16_000;
/// Sample rate for signed little-endian mono PCM16 output.
pub const QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ: u32 = 24_000;
/// Maximum accepted PCM16 bytes in one append operation.
pub const QWEN_REALTIME_MAX_INPUT_CHUNK_BYTES: usize = 32 * 1024;

#[derive(Default)]
pub(super) struct AudioDecoder {
    active_response: Option<String>,
}

/// Terminal state reported by the server for one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QwenRealtimeResponseStatus {
    Completed,
    Cancelled,
    Incomplete,
    Failed,
}

/// Typed media and turn events from a persistent Qwen Realtime session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QwenRealtimeAudioEvent {
    InputCommitted {
        item_id: String,
    },
    InputTranscriptDelta {
        item_id: String,
        content_index: u64,
        /// The transcript prefix the recognizer has confirmed.
        text: String,
        /// The current provisional suffix, which later events can revise.
        stash: String,
    },
    InputTranscriptDone {
        item_id: String,
        content_index: u64,
        transcript: String,
    },
    InputTranscriptFailed {
        item_id: String,
        error: ReactionPortFault,
    },
    ResponseStarted {
        response_id: String,
    },
    AudioDelta {
        response_id: String,
        item_id: String,
        content_index: u64,
        sample_rate_hz: u32,
        pcm16: Vec<u8>,
    },
    AudioDone {
        response_id: String,
        item_id: String,
        content_index: u64,
    },
    OutputTranscriptDelta {
        response_id: String,
        item_id: String,
        content_index: u64,
        delta: String,
    },
    OutputTranscriptDone {
        response_id: String,
        item_id: String,
        content_index: u64,
        transcript: String,
    },
    OutputTextDelta {
        response_id: String,
        item_id: String,
        content_index: u64,
        delta: String,
    },
    OutputTextDone {
        response_id: String,
        item_id: String,
        content_index: u64,
        text: String,
    },
    ResponseEnded {
        response_id: String,
        status: QwenRealtimeResponseStatus,
        error: Option<ReactionPortFault>,
    },
}

impl AudioDecoder {
    pub(super) fn decode(
        &mut self,
        event: &Value,
    ) -> Result<Option<QwenRealtimeAudioEvent>, ReactionPortFault> {
        let kind = string(event, "type")?;
        let decoded = match kind.as_str() {
            "input_audio_buffer.committed" => QwenRealtimeAudioEvent::InputCommitted {
                item_id: identity(event, "item_id")?,
            },
            "conversation.item.input_audio_transcription.delta" => {
                QwenRealtimeAudioEvent::InputTranscriptDelta {
                    item_id: identity(event, "item_id")?,
                    content_index: event
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .ok_or_else(transport::protocol_fault)?,
                    text: string(event, "text")?,
                    stash: string(event, "stash")?,
                }
            }
            "conversation.item.input_audio_transcription.completed" => {
                QwenRealtimeAudioEvent::InputTranscriptDone {
                    item_id: identity(event, "item_id")?,
                    content_index: event
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .ok_or_else(transport::protocol_fault)?,
                    transcript: string(event, "transcript")?,
                }
            }
            "conversation.item.input_audio_transcription.failed" => {
                QwenRealtimeAudioEvent::InputTranscriptFailed {
                    item_id: identity(event, "item_id")?,
                    error: server_error(&event["error"]),
                }
            }
            "error" => return Err(server_error(&event["error"])),
            "response.created" => {
                let response_id = identity(&event["response"], "id")?;
                if self.active_response.is_some()
                    || string(&event["response"], "status")? != "in_progress"
                {
                    return Err(transport::protocol_fault());
                }
                self.active_response = Some(response_id.clone());
                QwenRealtimeAudioEvent::ResponseStarted { response_id }
            }
            "response.done" => {
                let response = &event["response"];
                let response_id = identity(response, "id")?;
                if self.active_response.as_deref() != Some(response_id.as_str()) {
                    return Err(transport::protocol_fault());
                }
                let status = match string(response, "status")?.as_str() {
                    "completed" => QwenRealtimeResponseStatus::Completed,
                    "cancelled" => QwenRealtimeResponseStatus::Cancelled,
                    "incomplete" => QwenRealtimeResponseStatus::Incomplete,
                    "failed" => QwenRealtimeResponseStatus::Failed,
                    _ => return Err(transport::protocol_fault()),
                };
                let error = (status == QwenRealtimeResponseStatus::Failed)
                    .then(|| server_error(&response["status_details"]["error"]));
                self.active_response = None;
                QwenRealtimeAudioEvent::ResponseEnded {
                    response_id,
                    status,
                    error,
                }
            }
            "response.audio.delta"
            | "response.audio.done"
            | "response.audio_transcript.delta"
            | "response.audio_transcript.done"
            | "response.text.delta"
            | "response.text.done" => {
                let response_id = identity(event, "response_id")?;
                if self.active_response.as_deref() != Some(response_id.as_str()) {
                    return Err(transport::protocol_fault());
                }
                let item_id = identity(event, "item_id")?;
                let content_index = event
                    .get("content_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(transport::protocol_fault)?;
                match kind.as_str() {
                    "response.audio.delta" => {
                        let pcm16 = STANDARD
                            .decode(string(event, "delta")?)
                            .map_err(|_| transport::protocol_fault())?;
                        if pcm16.is_empty() || pcm16.len() % 2 != 0 {
                            return Err(transport::protocol_fault());
                        }
                        QwenRealtimeAudioEvent::AudioDelta {
                            response_id,
                            item_id,
                            content_index,
                            sample_rate_hz: QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ,
                            pcm16,
                        }
                    }
                    "response.audio.done" => QwenRealtimeAudioEvent::AudioDone {
                        response_id,
                        item_id,
                        content_index,
                    },
                    "response.audio_transcript.delta" => {
                        QwenRealtimeAudioEvent::OutputTranscriptDelta {
                            response_id,
                            item_id,
                            content_index,
                            delta: string(event, "delta")?,
                        }
                    }
                    "response.audio_transcript.done" => {
                        QwenRealtimeAudioEvent::OutputTranscriptDone {
                            response_id,
                            item_id,
                            content_index,
                            transcript: string(event, "transcript")?,
                        }
                    }
                    "response.text.delta" => QwenRealtimeAudioEvent::OutputTextDelta {
                        response_id,
                        item_id,
                        content_index,
                        delta: string(event, "delta")?,
                    },
                    "response.text.done" => QwenRealtimeAudioEvent::OutputTextDone {
                        response_id,
                        item_id,
                        content_index,
                        text: string(event, "text")?,
                    },
                    _ => return Err(transport::protocol_fault()),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(decoded))
    }
}

pub(super) fn encode_pcm16(pcm16: &[u8]) -> Result<String, ReactionPortFault> {
    if pcm16.is_empty() || pcm16.len() % 2 != 0 || pcm16.len() > QWEN_REALTIME_MAX_INPUT_CHUNK_BYTES
    {
        return Err(ReactionPortFault::retryable(
            ReactionPortFaultCode::Rejected,
            ReactionPortFaultReason::RequestPreparation,
        ));
    }
    Ok(STANDARD.encode(pcm16))
}

fn string(value: &Value, key: &str) -> Result<String, ReactionPortFault> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(transport::protocol_fault)
}

fn identity(value: &Value, key: &str) -> Result<String, ReactionPortFault> {
    let value = string(value, key)?;
    if value.is_empty() {
        return Err(transport::protocol_fault());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_transcript_delta_preserves_confirmed_and_provisional_text() {
        let mut decoder = AudioDecoder::default();
        let event = decoder
            .decode(&json!({
                "type": "conversation.item.input_audio_transcription.delta",
                "item_id": "input-1",
                "content_index": 0,
                "text": "你好",
                "stash": "世界",
            }))
            .unwrap();
        assert_eq!(
            event,
            Some(QwenRealtimeAudioEvent::InputTranscriptDelta {
                item_id: "input-1".to_owned(),
                content_index: 0,
                text: "你好".to_owned(),
                stash: "世界".to_owned(),
            })
        );
    }

    #[test]
    fn input_transcription_can_finish_after_response_ends() {
        let mut decoder = AudioDecoder::default();
        decoder
            .decode(&json!({
                "type": "response.created",
                "response": {"id": "response-1", "status": "in_progress"},
            }))
            .unwrap();
        decoder
            .decode(&json!({
                "type": "response.done",
                "response": {"id": "response-1", "status": "completed"},
            }))
            .unwrap();
        let event = decoder
            .decode(&json!({
                "type": "conversation.item.input_audio_transcription.completed",
                "item_id": "input-1",
                "content_index": 0,
                "transcript": "你好世界",
            }))
            .unwrap();
        assert_eq!(
            event,
            Some(QwenRealtimeAudioEvent::InputTranscriptDone {
                item_id: "input-1".to_owned(),
                content_index: 0,
                transcript: "你好世界".to_owned(),
            })
        );
        assert!(matches!(
            decoder
                .decode(&json!({
                    "type": "response.created",
                    "response": {"id": "response-2", "status": "in_progress"},
                }))
                .unwrap(),
            Some(QwenRealtimeAudioEvent::ResponseStarted { response_id })
                if response_id == "response-2"
        ));
    }
}
