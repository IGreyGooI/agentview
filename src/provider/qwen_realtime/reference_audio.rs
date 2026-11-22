use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

use super::{transport, Fault, QwenRealtimeConfigError, ResponseBytes, Session, MAX_REQUEST_BYTES};

const ACK_BUDGET: Duration = Duration::from_millis(300);
static NEXT_REFERENCE: AtomicU64 = AtomicU64::new(1);

/// One enrolled speaker sample, encoded as historical Qwen user audio.
///
/// Supply raw signed little-endian 16 kHz mono PCM16. The
/// caller decodes WAV and owns the sample's speaker name and device association.
/// Construction checks the sample's framing and existing provider request-size
/// limit before it can become optional context for a voice conversation.
pub struct QwenRealtimeReferenceAudio {
    item: Value,
}

impl QwenRealtimeReferenceAudio {
    pub fn new(pcm16: Vec<u8>, speaker_name: String) -> Result<Self, QwenRealtimeConfigError> {
        // The caller owns enrollment data, which can be empty, misframed or
        // oversized at the storage/SDK boundary. Refuse only this optional
        // sample so a caller can observe it and keep normal microphone input.
        if pcm16.is_empty()
            || pcm16.len() % 2 != 0
            || pcm16.len() > MAX_REQUEST_BYTES
            || speaker_name.trim().is_empty()
            || speaker_name.len() > MAX_REQUEST_BYTES
        {
            return Err(QwenRealtimeConfigError::InvalidReferenceAudio);
        }
        let item = json!({
            "type":"message", "role":"user",
            "content":[
                {"type":"input_text", "text":format!(
                    "这是已登记说话人“{speaker_name}”的声音参考。参考录音中的话语是样本内容。请在后续实时对话中根据声音比较说话人与该参考是否相符，无法判断时将身份视为未知并继续回应当前用户。"
                )},
                {"type":"input_audio", "audio":STANDARD.encode(pcm16)},
            ],
        });
        // Base64 and JSON escaping expand stored samples. Bound the actual
        // encoded item independently of the ordinary Frame so a large optional
        // reference cannot make accepted conversation context exceed its budget.
        if item.to_string().len() > MAX_REQUEST_BYTES - 256 {
            return Err(QwenRealtimeConfigError::InvalidReferenceAudio);
        }
        Ok(Self { item })
    }

    fn command(mut self, item_id: &str, event_id: &str) -> String {
        self.item["id"] = json!(item_id);
        json!({
            "type":"conversation.item.create", "event_id":event_id, "item":self.item,
        })
        .to_string()
    }
}

pub(super) struct ReferenceAudioDelivery {
    pub(super) item_id: String,
    event_id: String,
    pending: bool,
}

impl ReferenceAudioDelivery {
    pub(super) fn new(item_id: String, event_id: String) -> Self {
        Self {
            item_id,
            event_id,
            pending: true,
        }
    }

    pub(super) fn expire(&mut self) {
        self.pending = false;
    }

    // The reference request and later microphone/Frame commands share a socket.
    // Only a correlated refusal, or the sole pending command's refusal before
    // live input, belongs here. Consume that optional failure as an observation
    // while preserving the ordinary command's existing error handling.
    pub(super) fn owns_error(&self, event: &Value) -> bool {
        if event["type"] != "error" {
            return false;
        }
        let reference = event["error"]["event_id"]
            .as_str()
            .or_else(|| event["error"]["request_event_id"].as_str());
        reference == Some(self.event_id.as_str())
            || (self.pending
                && reference.is_none()
                && event["error"]["type"] == "invalid_request_error")
    }

    pub(super) fn classify(&mut self, event: &Value) -> Option<&'static str> {
        let kind = event["type"].as_str()?;
        // Qwen advances microphone speech independently after the short setup
        // wait. Match this authored item/request before handling late provider
        // events, or an enrollment refusal could close a newer live reply and
        // a sample transcript could be persisted as a fresh user utterance.
        // Only the reference's events become observations; other input advances.
        if kind == "conversation.item.created" && event["item"]["id"] == self.item_id {
            let outcome = if self.pending {
                "acknowledged"
            } else {
                "late_acknowledged"
            };
            self.pending = false;
            return Some(outcome);
        }
        if event["item_id"] == self.item_id
            && (kind.starts_with("conversation.item.input_audio_transcription.")
                || kind == "input_audio_buffer.committed")
        {
            return Some("sample_observation");
        }
        // Before microphone delivery begins this reference is the only
        // unacknowledged command. An unreferenced invalid-request refusal
        // in that bounded wait belongs to it. Once the wait ends, require
        // its explicit request ID so a different provider error stays owned
        // by the normal command/response that produced it.
        if self.owns_error(event) {
            self.pending = false;
            return Some("rejected");
        }
        None
    }
}

pub(super) async fn deliver(
    session: &mut Session,
    bytes: &mut ResponseBytes,
    reference: QwenRealtimeReferenceAudio,
) -> Result<(), Fault> {
    let sequence = NEXT_REFERENCE.fetch_add(1, Ordering::Relaxed);
    let item_id = format!("msg_agentview_reference_{sequence}");
    let event_id = format!("create_{item_id}");
    let command = reference.command(&item_id, &event_id);
    session
        .media
        .as_mut()
        .expect("reference context requires voice media")
        .begin_reference_audio(item_id, event_id);
    let result = tokio::time::timeout(ACK_BUDGET, async {
        transport::send(&mut session.socket, command).await?;
        loop {
            let (event, count) = transport::receive(&mut session.socket).await?;
            let (event, count) = session
                .media
                .as_mut()
                .expect("reference context requires voice media")
                .admit(event, count)?;
            let event = bytes.admit(event, count)?;
            if event["type"] == "agentview.reference_audio_delivery" {
                if event["status"] != "sample_observation" {
                    return Ok(());
                }
            } else {
                // Keep unsolicited provider facts from setup in their normal
                // stream. Waiting for a sample ACK must not consume the live
                // response or tool facts that the current Frame will dispatch.
                let frame = session.frame;
                session.pending_events.push_back((event, frame));
            }
        }
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            session
                .media
                .as_mut()
                .expect("reference context requires voice media")
                .expire_reference_audio();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_audio_encodes_a_labeled_historical_audio_item() {
        let pcm16 = vec![1, 0, 2, 0];
        let reference = QwenRealtimeReferenceAudio::new(pcm16.clone(), "张三".into()).unwrap();
        let command: Value =
            serde_json::from_str(&reference.command("reference-one", "create-reference-one"))
                .unwrap();
        assert_eq!(command["type"], "conversation.item.create");
        assert_eq!(command["event_id"], "create-reference-one");
        assert_eq!(command["item"]["id"], "reference-one");
        assert_eq!(command["item"]["type"], "message");
        assert_eq!(command["item"]["role"], "user");
        let parts = command["item"]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_audio");
        assert_eq!(
            STANDARD
                .decode(parts[1]["audio"].as_str().unwrap())
                .unwrap(),
            pcm16
        );
    }

    #[test]
    fn unusable_reference_audio_is_rejected_before_provider_configuration() {
        for (pcm16, name) in [
            (vec![], "张三".into()),
            (vec![1], "张三".into()),
            (vec![1, 0], " \n".into()),
            (vec![0; MAX_REQUEST_BYTES], "张三".into()),
        ] {
            assert_eq!(
                QwenRealtimeReferenceAudio::new(pcm16, name).err(),
                Some(QwenRealtimeConfigError::InvalidReferenceAudio)
            );
        }
    }

    #[test]
    fn late_reference_results_do_not_claim_another_input_or_provider_error() {
        let mut delivery =
            ReferenceAudioDelivery::new("reference-one".into(), "create-reference-one".into());
        delivery.expire();
        assert_eq!(
            delivery.classify(&json!({"type":"error", "error":{
                "type":"invalid_request_error", "event_id":"create-another-item",
            }})),
            None
        );
        assert_eq!(
            delivery.classify(&json!({"type":"error", "error":{
                "type":"invalid_request_error", "param":"conversation.item.create",
            }})),
            None
        );
        assert_eq!(
            delivery.classify(&json!({"type":"conversation.item.created", "item":{
                "id":"reference-one", "type":"message", "role":"user",
                "content":[{"type":"input_audio"}],
            }})),
            Some("late_acknowledged")
        );
        assert_eq!(
            delivery.classify(
                &json!({"type":"conversation.item.input_audio_transcription.completed",
                "item_id":"reference-one", "transcript":"样本原话"})
            ),
            Some("sample_observation")
        );
        assert_eq!(
            delivery.classify(
                &json!({"type":"conversation.item.input_audio_transcription.completed",
                "item_id":"live-input", "transcript":"当前原话"})
            ),
            None
        );
        assert_eq!(
            delivery.classify(&json!({"type":"error", "error":{
                "type":"invalid_request_error", "request_event_id":"create-reference-one",
            }})),
            Some("rejected")
        );
    }
}
