//! Typed Frame lowering for Qwen Realtime.

use serde_json::{json, Value};

use super::{Mode, QwenRealtimeVoiceprintAudioUrl};
use crate::{
    component::execution::{
        Frame, FrameBasis, ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason,
    },
    pom_renderer::render_pom_document,
    transcript::{
        AssistantTextStatus, CanonicalInputItem, ConversationRole, InstructionAuthority,
        ASSISTANT_OUTPUT_INTERRUPTED_MARKER,
    },
};

pub(super) struct Commands {
    pub(super) update: String,
    pub(super) items: Vec<String>,
    pub(super) ephemeral: Vec<String>,
}

pub(super) fn encode(
    frame: &Frame,
    max_request_bytes: usize,
    mode: Mode,
    voiceprint_audio_url: Option<&QwenRealtimeVoiceprintAudioUrl>,
) -> Result<Commands, ReactionPortFault> {
    let submission = frame.submission();
    if submission.decision().is_some() {
        return Err(preparation_fault());
    }

    let mut session = match (frame.basis(), mode) {
        (FrameBasis::Full, Mode::SmartTurnV2) => json!({
            "modalities": ["text", "audio"],
            "input_audio_format": "pcm",
            "output_audio_format": "pcm",
            "turn_detection": {"type": "smart_turn_v2"},
        }),
        (FrameBasis::Full, Mode::ManualAudio) => json!({
            "modalities": ["text", "audio"],
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "turn_detection": null,
        }),
        (FrameBasis::Full, Mode::Text) => json!({"modalities": ["text"], "turn_detection": null}),
        // The service freezes turn detection once the session processes input.
        (FrameBasis::DeltaFrom(_), _) => json!({}),
    };
    // Qwen consumes speaker URLs only on the first update. Subsequent Frames
    // retain the same media session, so repeating this field would misrepresent
    // a device setting edit as a live re-registration that the service ignores.
    if let (FrameBasis::Full, Mode::SmartTurnV2, Some(reference)) =
        (frame.basis(), mode, voiceprint_audio_url)
    {
        session["turn_detection"]["voiceprint_audio_urls"] = json!([reference.as_str()]);
    }
    let mut tools = Vec::with_capacity(submission.tools().definitions().len());
    for tool in submission.tools().definitions() {
        // Qwen's Realtime function declaration has no strict-schema option.
        if tool.strict() {
            return Err(preparation_fault());
        }
        tools.push(json!({
            "type": "function",
            "function": {
                "name": tool.name(),
                "description": tool.description(),
                "parameters": tool.input_schema(),
            },
        }));
    }
    session["tools"] = Value::Array(tools);

    for item in submission.projection().items() {
        if let CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            pom,
        } = item
        {
            if session.get("instructions").is_some() {
                return Err(preparation_fault());
            }
            session["instructions"] =
                Value::String(render_pom_document(pom).map_err(|_| preparation_fault())?);
        }
    }
    if submission
        .replay()
        .iter()
        .chain(submission.staged_inputs())
        .chain(submission.ephemeral())
        .any(is_system)
    {
        return Err(preparation_fault());
    }

    let replay = match frame.basis() {
        FrameBasis::Full => submission.replay(),
        // The port checks the replay suffix against output already on the socket.
        FrameBasis::DeltaFrom(_) => &[],
    };
    let mut request_bytes = 0_usize;
    let mut encode = |envelope: Value| {
        let envelope = serde_json::to_string(&envelope).map_err(|_| preparation_fault())?;
        request_bytes = request_bytes
            .checked_add(envelope.len())
            .filter(|bytes| *bytes <= max_request_bytes)
            .ok_or_else(request_limit_fault)?;
        Ok(envelope)
    };
    let update = encode(json!({"type": "session.update", "session": session}))?;
    let items = lower_items(
        replay
            .iter()
            .chain(submission.staged_inputs())
            .chain(submission.projection().items()),
        &mut encode,
    )?;
    let ephemeral = lower_items(submission.ephemeral().iter(), &mut encode)?;
    Ok(Commands {
        update,
        items,
        ephemeral,
    })
}

fn lower_items<'a>(
    items: impl Iterator<Item = &'a CanonicalInputItem>,
    mut encode: impl FnMut(Value) -> Result<String, ReactionPortFault>,
) -> Result<Vec<String>, ReactionPortFault> {
    let mut envelopes = Vec::new();
    for item in items {
        let envelope = match item {
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::System,
                ..
            } => continue,
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::Developer,
                pom,
            } => {
                let text = render_pom_document(pom).map_err(|_| preparation_fault())?;
                // This endpoint accepts system-role conversation items and
                // rejects developer-role items. Retain their authored order.
                message(
                    "system",
                    "input_text",
                    format!("[Developer instructions]\n\n{text}"),
                )
            }
            CanonicalInputItem::Message { role, pom } => {
                let text = render_pom_document(pom).map_err(|_| preparation_fault())?;
                let (role, content_type) = match role {
                    ConversationRole::User => ("user", "input_text"),
                    ConversationRole::Assistant => ("assistant", "output_text"),
                };
                message(role, content_type, text)
            }
            CanonicalInputItem::AssistantText { text, status, .. } => {
                let item = message("assistant", "output_text", text.clone());
                if *status == AssistantTextStatus::Interrupted {
                    envelopes.push(encode(item)?);
                    message(
                        "user",
                        "input_text",
                        ASSISTANT_OUTPUT_INTERRUPTED_MARKER.to_owned(),
                    )
                } else {
                    item
                }
            }
            CanonicalInputItem::ToolCall {
                call_id,
                name,
                raw_arguments,
            } => json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": raw_arguments,
                },
            }),
            CanonicalInputItem::ToolResult { call_id, content } => json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": content,
                },
            }),
            CanonicalInputItem::DecisionRequest { .. }
            | CanonicalInputItem::DecisionOutput { .. }
            | CanonicalInputItem::ProviderExtension(_) => return Err(preparation_fault()),
        };
        envelopes.push(encode(envelope)?);
    }
    Ok(envelopes)
}

fn message(role: &str, content_type: &str, text: String) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": role,
            "content": [{"type": content_type, "text": text}],
        },
    })
}

fn is_system(item: &CanonicalInputItem) -> bool {
    matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    )
}

fn preparation_fault() -> ReactionPortFault {
    ReactionPortFault::retryable(
        ReactionPortFaultCode::Rejected,
        ReactionPortFaultReason::RequestPreparation,
    )
}

fn request_limit_fault() -> ReactionPortFault {
    ReactionPortFault::retryable(
        ReactionPortFaultCode::Limit,
        ReactionPortFaultReason::RequestPreparation,
    )
}
