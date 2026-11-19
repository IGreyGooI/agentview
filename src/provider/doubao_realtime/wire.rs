//! JSON wire messages for the Seeduplex full-duplex Realtime API.
//!
//! Protocol reference:
//! <https://docs.volcengine.com/docs/doubao-voice/end-to-end-real-time-voice-full-duplex-version?lang=zh>.
//! The session requests native PCM16LE output rather than the Float32LE `pcm`
//! format used by the Node adapter. Both input and output stay mono PCM16LE.

use std::sync::atomic::{AtomicU64, Ordering};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

use crate::component::execution::ToolDefinition;

pub(super) fn session_create(
    model: &str,
    voice: &str,
    instructions: &str,
    tools: &[ToolDefinition],
) -> Value {
    session_event("session.create", model, voice, instructions, tools)
}

pub(super) fn session_update(
    model: &str,
    voice: &str,
    instructions: &str,
    tools: &[ToolDefinition],
) -> Value {
    // The documented create/update schema marks model and audio as required.
    // Keep the same audio configuration when updating the Component view, as
    // the existing Node adapter does; partial-update semantics are unspecified.
    session_event("session.update", model, voice, instructions, tools)
}

fn session_event(
    kind: &str,
    model: &str,
    voice: &str,
    instructions: &str,
    tools: &[ToolDefinition],
) -> Value {
    let tools: Vec<_> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name(),
                "description": tool.description(),
                "parameters": tool.input_schema(),
            })
        })
        .collect();
    json!({
        "event_id": id("event"),
        "type": kind,
        "session": {
            "model": model,
            "instructions": instructions,
            "audio": {
                "input": {"format": {"type": "pcm", "rate": 16_000}},
                "output": {
                    "format": {"type": "pcm_s16le", "rate": 24_000},
                    "voice": voice,
                    "speed": 0,
                    "loudness": 0,
                },
            },
            "tools": tools,
        },
    })
}

pub(super) fn append_audio(audio: &str) -> Value {
    json!({
        "event_id": id("event"),
        "type": "input_audio_buffer.append",
        "audio": audio,
    })
}

/// Seeduplex continues inference when these matching tool results arrive.
/// This message needs neither `response.create` nor a text-buffer commit.
pub(super) fn tool_results(results: &[(String, String)]) -> Value {
    let items: Vec<_> = results
        .iter()
        .map(|(call_id, output)| {
            json!({
                "call_id": call_id,
                "role": "tool",
                "content": [{"type": "input_text", "text": output}],
            })
        })
        .collect();
    json!({
        "event_id": id("event"),
        "type": "conversation.item.create",
        "items": items,
    })
}

/// Decodes one provider audio payload without changing response/session state.
pub(super) fn decode_audio(encoded: &str) -> Result<Vec<u8>, WireError> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| WireError::InvalidBase64)?;
    // The provider owns these bytes and can send a malformed chunk. A dangling
    // byte cannot represent the negotiated PCM16 sample; reject this payload
    // instead of shifting every following sample on the playback boundary.
    if bytes.len() % 2 != 0 {
        return Err(WireError::UnalignedPcm16);
    }
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum WireError {
    #[error("Seeduplex audio payload is not valid base64")]
    InvalidBase64,
    #[error("Seeduplex audio payload is not aligned to PCM16 samples")]
    UnalignedPcm16,
}

/// Correlation references are unique within this process and contain no secrets.
pub(super) fn id(prefix: &str) -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_negotiates_native_pcm16_and_flat_function_definitions() {
        let schema = json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        });
        let tool = ToolDefinition::new("weather", "City weather", schema.clone()).unwrap();
        let event = session_create("1.2.6.1", "test-voice", "Answer briefly", &[tool]);
        assert_eq!(event["type"], "session.create");
        assert_eq!(event["session"]["model"], "1.2.6.1");
        assert_eq!(event["session"]["instructions"], "Answer briefly");
        assert_eq!(
            event["session"]["audio"],
            json!({
                "input": {"format": {"type": "pcm", "rate": 16_000}},
                "output": {
                    "format": {"type": "pcm_s16le", "rate": 24_000},
                    "voice": "test-voice", "speed": 0, "loudness": 0,
                },
            })
        );
        assert_eq!(
            event["session"]["tools"],
            json!([{
                "type": "function", "name": "weather",
                "description": "City weather", "parameters": schema,
            }])
        );
        assert!(event["event_id"].as_str().unwrap().starts_with("event_"));
    }

    #[test]
    fn update_keeps_required_configuration_and_can_clear_instructions_and_tools() {
        let created = session_create("1.2.6.1", "test-voice", "Old view", &[]);
        let updated = session_update("1.2.6.1", "test-voice", "", &[]);
        assert_eq!(updated["type"], "session.update");
        assert_eq!(updated["session"]["model"], created["session"]["model"]);
        assert_eq!(updated["session"]["audio"], created["session"]["audio"]);
        assert_eq!(updated["session"]["instructions"], "");
        assert_eq!(updated["session"]["tools"], json!([]));
        assert_ne!(created["event_id"], updated["event_id"]);
    }

    #[test]
    fn tool_results_preserve_call_pairing_order_and_opaque_output_text() {
        let results = vec![
            ("call-2".to_owned(), "{ \"value\": 91 }\n".to_owned()),
            ("call-1".to_owned(), "工具失败：\"未找到\"".to_owned()),
        ];
        let event = tool_results(&results);
        assert_eq!(event["type"], "conversation.item.create");
        assert_eq!(
            event["items"],
            json!([
                {"call_id": "call-2", "role": "tool", "content": [{
                    "type": "input_text", "text": results[0].1,
                }]},
                {"call_id": "call-1", "role": "tool", "content": [{
                    "type": "input_text", "text": results[1].1,
                }]},
            ])
        );
    }

    #[test]
    fn audio_payload_round_trips_pcm16_bytes_without_float_conversion() {
        let samples = [i16::MIN, -1, 0, 1, i16::MAX];
        let bytes: Vec<_> = samples.into_iter().flat_map(i16::to_le_bytes).collect();
        let event = append_audio(&STANDARD.encode(&bytes));
        assert_eq!(event["type"], "input_audio_buffer.append");
        assert_eq!(
            decode_audio(event["audio"].as_str().unwrap()).unwrap(),
            bytes
        );
        assert_eq!(decode_audio("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn malformed_audio_reports_only_the_wire_error() {
        assert_eq!(decode_audio("not base64!"), Err(WireError::InvalidBase64));
        assert_eq!(
            decode_audio(&STANDARD.encode([1_u8, 2, 3])),
            Err(WireError::UnalignedPcm16)
        );
    }
}
