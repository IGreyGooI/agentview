use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::component::execution::{
    ProviderFact, ProviderOutputKey, ProviderToolCall, ReactionPortFault, ReactionPortFaultCode,
    ReactionPortFaultReason,
};

/// One explicitly requested response. Session acknowledgements carry no facts.
#[derive(Default)]
pub(super) struct Decoder {
    response_id: Option<String>,
    outputs: BTreeMap<u64, Output>,
    next_output: u64,
    primary_text: Option<ProviderOutputKey>,
    audio: bool,
    complete: bool,
}

struct Output {
    item: WireItem,
    delta: String,
    saw_delta: bool,
    value_done_seen: bool,
    value_done: Option<String>,
    content_kind: Option<ContentKind>,
    part_added: bool,
    part_done: Option<WireContent>,
    audio_done: bool,
    sealed: bool,
    pending: VecDeque<ProviderFact>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum WireItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        status: String,
        role: String,
        content: Vec<WireContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        status: String,
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum ContentKind {
    #[serde(rename = "text", alias = "output_text")]
    Text,
    #[serde(rename = "audio")]
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WireContent {
    kind: ContentKind,
    text: Option<String>,
}

impl<'de> Deserialize<'de> for WireContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawContent {
            #[serde(rename = "type")]
            kind: ContentKind,
            text: Option<String>,
            transcript: Option<String>,
        }

        let raw = RawContent::deserialize(deserializer)?;
        if raw.kind == ContentKind::Text && raw.text.is_none() {
            return Err(serde::de::Error::custom("missing text content"));
        }
        let text = match (raw.text, raw.transcript) {
            (Some(text), Some(transcript)) if text != transcript => {
                return Err(serde::de::Error::custom("conflicting content text"));
            }
            (Some(text), _) | (_, Some(text)) => Some(text),
            (None, None) => None,
        };
        Ok(Self {
            kind: raw.kind,
            text,
        })
    }
}

impl WireItem {
    fn id(&self) -> &str {
        match self {
            Self::Message { id, .. } | Self::FunctionCall { id, .. } => id,
        }
    }

    fn is_message(&self) -> bool {
        matches!(self, Self::Message { .. })
    }
}

impl Decoder {
    pub(super) fn with_audio() -> Self {
        Self {
            audio: true,
            ..Self::default()
        }
    }

    pub(super) fn is_complete(&self) -> bool {
        self.complete
    }

    pub(super) fn on_event(
        &mut self,
        event: Value,
    ) -> Result<Vec<ProviderFact>, ReactionPortFault> {
        if self.complete {
            return Err(protocol());
        }
        match string(&event, "type")? {
            "error" => return Err(server_error(&event["error"])),
            "response.created" => self.response_created(&event)?,
            "response.output_item.added" => self.output_added(&event)?,
            "response.text.delta" => self.delta(&event, Some(ContentKind::Text))?,
            "response.audio_transcript.delta" if self.audio => {
                self.delta(&event, Some(ContentKind::Audio))?
            }
            "response.audio_transcript.done" if self.audio => {
                self.value_done(&event, Some(ContentKind::Audio), "transcript")?
            }
            "response.audio.delta" if self.audio => self.audio_event(&event, false)?,
            "response.audio.done" if self.audio => self.audio_event(&event, true)?,
            "response.function_call_arguments.delta" => self.delta(&event, None)?,
            "response.text.done" => self.value_done(&event, Some(ContentKind::Text), "text")?,
            "response.function_call_arguments.done" => {
                self.value_done(&event, None, "arguments")?
            }
            "response.content_part.added" => self.content_part(&event, false)?,
            "response.content_part.done" => self.content_part(&event, true)?,
            "response.output_item.done" => self.output_done(&event)?,
            "response.done" => return self.response_done(&event),
            "session.created"
            | "session.updated"
            | "conversation.item.created"
            | "rate_limits.updated" => {}
            _ => {}
        }
        self.release_ready()
    }

    fn response_created(&mut self, event: &Value) -> Result<(), ReactionPortFault> {
        let response = &event["response"];
        let id = identity(response, "id")?;
        if self.response_id.is_some() || string(response, "status")? != "in_progress" {
            return Err(protocol());
        }
        self.response_id = Some(id.to_owned());
        Ok(())
    }

    fn response_matches(&self, event: &Value) -> Result<(), ReactionPortFault> {
        if self.response_id.as_deref() != Some(identity(event, "response_id")?) {
            return Err(protocol());
        }
        Ok(())
    }

    fn output_added(&mut self, event: &Value) -> Result<(), ReactionPortFault> {
        self.response_matches(event)?;
        let index = index(event, "output_index")?;
        let item: WireItem = decode(&event["item"])?;
        self.register_output(index, item, true)
    }

    fn register_output(
        &mut self,
        index: u64,
        item: WireItem,
        added: bool,
    ) -> Result<(), ReactionPortFault> {
        if item.id().is_empty()
            || self.outputs.contains_key(&index)
            || self
                .outputs
                .values()
                .any(|output| output.item.id() == item.id())
        {
            return Err(protocol());
        }
        match &item {
            WireItem::Message {
                status,
                role,
                content,
                ..
            } => {
                if status != expected_status(added)
                    || role != "assistant"
                    || (added && !content.is_empty())
                    || self.outputs.values().any(|output| output.item.is_message())
                {
                    return Err(protocol());
                }
            }
            WireItem::FunctionCall {
                status,
                call_id,
                name,
                arguments,
                ..
            } => {
                if status != expected_status(added)
                    || call_id.is_empty()
                    || name.is_empty()
                    || (added && !arguments.is_empty())
                    || self.outputs.values().any(|output| {
                        matches!(&output.item, WireItem::FunctionCall { call_id: existing, .. } if existing == call_id)
                    })
                {
                    return Err(protocol());
                }
            }
        }
        self.outputs.insert(
            index,
            Output {
                item,
                delta: String::new(),
                saw_delta: false,
                value_done_seen: false,
                value_done: None,
                content_kind: None,
                part_added: false,
                part_done: None,
                audio_done: false,
                sealed: false,
                pending: VecDeque::new(),
            },
        );
        Ok(())
    }

    fn output_mut(
        &mut self,
        event: &Value,
        content_kind: Option<ContentKind>,
    ) -> Result<(u64, &mut Output), ReactionPortFault> {
        self.response_matches(event)?;
        let output_index = index(event, "output_index")?;
        let output = self.outputs.get_mut(&output_index).ok_or_else(protocol)?;
        if output.sealed
            || output.item.id() != identity(event, "item_id")?
            || output.item.is_message() != content_kind.is_some()
        {
            return Err(protocol());
        }
        if let Some(kind) = content_kind {
            if index(event, "content_index")? != 0
                || output.content_kind.is_some_and(|previous| previous != kind)
            {
                return Err(protocol());
            }
            output.content_kind = Some(kind);
        }
        Ok((output_index, output))
    }

    fn delta(
        &mut self,
        event: &Value,
        content_kind: Option<ContentKind>,
    ) -> Result<(), ReactionPortFault> {
        let delta = string(event, "delta")?;
        let (index, output) = self.output_mut(event, content_kind)?;
        if output.value_done_seen || output.part_done.is_some() {
            return Err(protocol());
        }
        output.delta.push_str(delta);
        output.saw_delta = true;
        if content_kind.is_some() && !delta.is_empty() {
            output.pending.push_back(ProviderFact::TextDelta {
                output: ProviderOutputKey::new(index),
                phase: None,
                delta: delta.to_owned(),
            });
            self.primary_text = Some(ProviderOutputKey::new(index));
        }
        Ok(())
    }

    fn value_done(
        &mut self,
        event: &Value,
        content_kind: Option<ContentKind>,
        field: &str,
    ) -> Result<(), ReactionPortFault> {
        let value = match event.get(field) {
            Some(Value::String(value)) => Some(value.as_str()),
            None | Some(Value::Null) if content_kind == Some(ContentKind::Audio) => None,
            _ => return Err(protocol()),
        };
        let (_, output) = self.output_mut(event, content_kind)?;
        if output.value_done_seen {
            return Err(protocol());
        }
        validate_value(output, value)?;
        output.value_done_seen = true;
        output.value_done = value.map(str::to_owned);
        Ok(())
    }

    fn audio_event(&mut self, event: &Value, done: bool) -> Result<(), ReactionPortFault> {
        if !done {
            string(event, "delta")?;
        }
        let (_, output) = self.output_mut(event, Some(ContentKind::Audio))?;
        if output.audio_done || output.part_done.is_some() {
            return Err(protocol());
        }
        output.audio_done = done;
        Ok(())
    }

    fn content_part(&mut self, event: &Value, done: bool) -> Result<(), ReactionPortFault> {
        let part: WireContent = decode(&event["part"])?;
        validate_content(&part, self.audio)?;
        let (_, output) = self.output_mut(event, Some(part.kind))?;
        if done {
            if output.part_done.is_some() {
                return Err(protocol());
            }
            validate_value(output, part.text.as_deref())?;
            output.part_done = Some(part);
        } else {
            if output.part_added
                || part.text.as_deref().is_some_and(|text| !text.is_empty())
                || output.value_done_seen
                || output.part_done.is_some()
                || output.saw_delta
            {
                return Err(protocol());
            }
            output.part_added = true;
        }
        Ok(())
    }

    fn output_done(&mut self, event: &Value) -> Result<(), ReactionPortFault> {
        if self.response_id.is_none() && self.outputs.is_empty() {
            self.response_id = Some(identity(event, "response_id")?.to_owned());
        }
        self.response_matches(event)?;
        let index = index(event, "output_index")?;
        let item: WireItem = decode(&event["item"])?;
        if !self.outputs.contains_key(&index) {
            self.register_output(index, item.clone(), false)?;
        }
        self.seal_output(index, item)
    }

    fn seal_output(&mut self, index: u64, item: WireItem) -> Result<(), ReactionPortFault> {
        let output = self.outputs.get_mut(&index).ok_or_else(protocol)?;
        if output.sealed || output.item.id() != item.id() {
            return Err(protocol());
        }
        let fact = match (&output.item, &item) {
            (
                WireItem::Message { .. },
                WireItem::Message {
                    status,
                    role,
                    content,
                    ..
                },
            ) => {
                if status != "completed" || role != "assistant" || content.len() != 1 {
                    return Err(protocol());
                }
                let content = &content[0];
                validate_content(content, self.audio)?;
                if output.content_kind.is_some_and(|kind| kind != content.kind) {
                    return Err(protocol());
                }
                validate_value(output, content.text.as_deref())?;
                content.text.clone().map(|text| ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(index),
                    phase: None,
                    text,
                })
            }
            (
                WireItem::FunctionCall {
                    call_id: original_id,
                    name: original_name,
                    ..
                },
                WireItem::FunctionCall {
                    status,
                    call_id,
                    name,
                    arguments,
                    ..
                },
            ) => {
                if status != "completed" || call_id != original_id || name != original_name {
                    return Err(protocol());
                }
                validate_value(output, Some(arguments))?;
                Some(ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(index),
                    ordinal: index,
                    call: ProviderToolCall::new(call_id, name, arguments)
                        .map_err(|_| protocol())?,
                })
            }
            _ => return Err(protocol()),
        };
        if matches!(fact, Some(ProviderFact::TextSealed { .. })) {
            self.primary_text = Some(ProviderOutputKey::new(index));
        }
        output.item = item;
        output.sealed = true;
        output.pending.extend(fact);
        Ok(())
    }

    fn release_ready(&mut self) -> Result<Vec<ProviderFact>, ReactionPortFault> {
        let mut facts = Vec::new();
        while let Some(output) = self.outputs.get_mut(&self.next_output) {
            facts.extend(output.pending.drain(..));
            if !output.sealed {
                break;
            }
            self.next_output = self.next_output.checked_add(1).ok_or_else(protocol)?;
        }
        Ok(facts)
    }

    fn response_done(&mut self, event: &Value) -> Result<Vec<ProviderFact>, ReactionPortFault> {
        let response = &event["response"];
        let response_id = identity(response, "id")?;
        if self.response_id.is_none() && self.outputs.is_empty() {
            self.response_id = Some(response_id.to_owned());
        }
        if self.response_id.as_deref() != Some(response_id) {
            return Err(protocol());
        }
        if string(response, "status")? != "completed" {
            return Err(server_error(&response["status_details"]["error"]));
        }
        let items: Vec<WireItem> = decode(&response["output"])?;
        if self
            .outputs
            .last_key_value()
            .is_some_and(|(index, _)| *index >= items.len() as u64)
        {
            return Err(protocol());
        }
        for (index, item) in items.iter().enumerate() {
            let index = index as u64;
            if !self.outputs.contains_key(&index) {
                self.register_output(index, item.clone(), false)?;
                self.seal_output(index, item.clone())?;
            }
            let output = self.outputs.get(&index).ok_or_else(protocol)?;
            if !output.sealed || output.item != *item {
                return Err(protocol());
            }
        }
        let mut facts = self.release_ready()?;
        if self.next_output != items.len() as u64 {
            return Err(protocol());
        }
        self.complete = true;
        facts.push(ProviderFact::ReactionCompleted {
            primary_text: self.primary_text,
        });
        Ok(facts)
    }
}

fn validate_value(output: &Output, value: Option<&str>) -> Result<(), ReactionPortFault> {
    if (output.value_done_seen && output.value_done.as_deref() != value)
        || (output.saw_delta && Some(output.delta.as_str()) != value)
        || output
            .part_done
            .as_ref()
            .is_some_and(|part| part.text.as_deref() != value)
    {
        return Err(protocol());
    }
    Ok(())
}

fn expected_status(added: bool) -> &'static str {
    if added {
        "in_progress"
    } else {
        "completed"
    }
}

fn validate_content(content: &WireContent, audio: bool) -> Result<(), ReactionPortFault> {
    if content.kind == ContentKind::Audio && !audio {
        return Err(protocol());
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, ReactionPortFault> {
    serde_json::from_value(value.clone()).map_err(|_| protocol())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ReactionPortFault> {
    value.get(key).and_then(Value::as_str).ok_or_else(protocol)
}

fn identity<'a>(value: &'a Value, key: &str) -> Result<&'a str, ReactionPortFault> {
    let value = string(value, key)?;
    if value.is_empty() {
        return Err(protocol());
    }
    Ok(value)
}

fn index(value: &Value, key: &str) -> Result<u64, ReactionPortFault> {
    value.get(key).and_then(Value::as_u64).ok_or_else(protocol)
}

fn protocol() -> ReactionPortFault {
    ReactionPortFault::terminal(
        ReactionPortFaultCode::Protocol,
        ReactionPortFaultReason::ResponseProtocol,
    )
}

pub(super) fn server_error(error: &Value) -> ReactionPortFault {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match code {
        "invalid_api_key" | "InvalidApiKey" | "Unauthorized" | "unauthorized" => {
            ReactionPortFault::terminal(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::Authentication,
            )
        }
        "Forbidden" | "forbidden" | "PermissionDenied" => ReactionPortFault::terminal(
            ReactionPortFaultCode::Rejected,
            ReactionPortFaultReason::Authorization,
        ),
        "rate_limit_exceeded" | "Throttling" | "TooManyRequests" => ReactionPortFault::retryable(
            ReactionPortFaultCode::Unavailable,
            ReactionPortFaultReason::RateLimited,
        ),
        "server_error" | "internal_error" | "ServiceUnavailable" | "InternalError" => {
            ReactionPortFault::retryable(
                ReactionPortFaultCode::Unavailable,
                ReactionPortFaultReason::Transport,
            )
        }
        _ => ReactionPortFault::terminal(
            ReactionPortFaultCode::Rejected,
            ReactionPortFaultReason::UpstreamRejected,
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message(text: &str) -> Value {
        json!({
            "type": "message", "id": "message-1", "status": "completed",
            "role": "assistant", "content": [{"type": "text", "text": text}]
        })
    }

    fn completed(output: Vec<Value>) -> Value {
        json!({
            "type": "response.done",
            "response": {"id": "response-1", "status": "completed", "output": output}
        })
    }

    #[test]
    fn completion_only_response_admits_tools_then_text() {
        let mut decoder = Decoder::default();
        let facts = decoder
            .on_event(completed(vec![
                json!({
                    "type": "function_call", "id": "tool-1", "status": "completed",
                    "call_id": "call-1", "name": "lookup", "arguments": "{\"id\":3}"
                }),
                message("Lookup requested."),
            ]))
            .unwrap();
        assert_eq!(facts.len(), 3);
        assert!(
            matches!(&facts[0], ProviderFact::ToolCall { ordinal: 0, call, .. }
            if call.name() == "lookup" && call.raw_arguments() == "{\"id\":3}")
        );
        assert!(matches!(&facts[1], ProviderFact::TextSealed { text, .. }
            if text == "Lookup requested."));
        assert_eq!(
            facts[2],
            ProviderFact::ReactionCompleted {
                primary_text: Some(ProviderOutputKey::new(1))
            }
        );
        assert!(decoder.is_complete());
        assert!(decoder.on_event(completed(vec![])).is_err());
    }

    #[test]
    fn completion_only_item_does_not_require_text_done() {
        let mut decoder = Decoder::default();
        let item = message("Ready.");
        let facts = decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 0, "item": item
            }))
            .unwrap();
        assert!(matches!(&facts[..], [ProviderFact::TextSealed { text, .. }] if text == "Ready."));
        let facts = decoder.on_event(completed(vec![item])).unwrap();
        assert!(matches!(
            &facts[..],
            [ProviderFact::ReactionCompleted {
                primary_text: Some(_)
            }]
        ));
    }

    #[test]
    fn response_completion_cannot_replace_an_open_output() {
        let mut decoder = Decoder::default();
        decoder
            .on_event(json!({
                "type": "response.created",
                "response": {"id": "response-1", "status": "in_progress"}
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.output_item.added", "response_id": "response-1",
                "output_index": 0,
                "item": {"type": "message", "id": "message-1", "status": "in_progress",
                    "role": "assistant", "content": []}
            }))
            .unwrap();
        let facts = decoder
            .on_event(json!({
                "type": "response.text.delta", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "delta": "Ready"
            }))
            .unwrap();
        assert!(matches!(&facts[..], [ProviderFact::TextDelta { delta, .. }] if delta == "Ready"));
        assert!(decoder
            .on_event(completed(vec![message("Ready.")]))
            .is_err());
        assert!(!decoder.is_complete());
    }

    #[test]
    fn completed_item_must_match_already_published_text() {
        let mut decoder = Decoder::default();
        decoder
            .on_event(json!({
                "type": "response.created",
                "response": {"id": "response-1", "status": "in_progress"}
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.output_item.added", "response_id": "response-1",
                "output_index": 0,
                "item": {"type": "message", "id": "message-1", "status": "in_progress",
                    "role": "assistant", "content": []}
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.text.delta", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "delta": "Ready"
            }))
            .unwrap();
        assert!(decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 0, "item": message("Changed")
            }))
            .is_err());
    }

    #[test]
    fn later_output_waits_for_earlier_output_before_publication() {
        let mut decoder = Decoder::default();
        let facts = decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 1, "item": message("Ready.")
            }))
            .unwrap();
        assert!(facts.is_empty());
        let facts = decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 0,
                "item": {"type": "function_call", "id": "tool-1", "status": "completed",
                    "call_id": "call-1", "name": "lookup", "arguments": "{}"}
            }))
            .unwrap();
        assert!(matches!(
            &facts[..],
            [
                ProviderFact::ToolCall { ordinal: 0, .. },
                ProviderFact::TextSealed { .. }
            ]
        ));
    }

    #[test]
    fn terminal_output_cannot_omit_a_previously_observed_later_item() {
        let mut decoder = Decoder::default();
        decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 4,
                "item": {"type": "function_call", "id": "tool-1", "status": "completed",
                    "call_id": "call-1", "name": "lookup", "arguments": "{}"}
            }))
            .unwrap();
        assert!(decoder
            .on_event(completed(vec![message("Ready.")]))
            .is_err());
        assert!(!decoder.is_complete());
    }

    #[test]
    fn malformed_function_arguments_never_publish_a_tool_call() {
        for arguments in ["{", "[]", "null", "true"] {
            let mut decoder = Decoder::default();
            assert!(decoder
                .on_event(completed(vec![json!({
                    "type": "function_call", "id": "tool-1", "status": "completed",
                    "call_id": "call-1", "name": "lookup", "arguments": arguments
                })]))
                .is_err());
            assert!(!decoder.is_complete());
        }
    }

    #[test]
    fn output_cannot_change_the_active_response_identity() {
        let mut decoder = Decoder::default();
        decoder
            .on_event(json!({
                "type": "response.created",
                "response": {"id": "response-1", "status": "in_progress"}
            }))
            .unwrap();
        assert!(decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "different-response",
                "output_index": 0, "item": message("Ready.")
            }))
            .is_err());
        assert!(!decoder.is_complete());
    }

    fn open_audio_output() -> Decoder {
        let mut decoder = Decoder::with_audio();
        decoder
            .on_event(json!({
                "type": "response.created",
                "response": {"id": "response-1", "status": "in_progress"}
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.output_item.added", "response_id": "response-1",
                "output_index": 0,
                "item": {"type": "message", "id": "message-1", "status": "in_progress",
                    "role": "assistant", "content": []}
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.content_part.added", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1",
                "part": {"type": "audio"}
            }))
            .unwrap();
        decoder
    }

    fn audio_item(content: Value) -> Value {
        json!({
            "type": "message", "id": "message-1", "status": "completed",
            "role": "assistant", "content": [content]
        })
    }

    #[test]
    fn audio_transcripts_admit_text_and_normalize_terminal_content_fields() {
        let mut decoder = open_audio_output();
        assert!(decoder
            .on_event(json!({
                "type": "response.audio.delta", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "delta": "AA=="
            }))
            .unwrap()
            .is_empty());
        let facts = decoder
            .on_event(json!({
                "type": "response.audio_transcript.delta", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "delta": "Ready."
            }))
            .unwrap();
        assert!(matches!(&facts[..], [ProviderFact::TextDelta { delta, .. }] if delta == "Ready."));
        decoder
            .on_event(json!({
                "type": "response.audio_transcript.done", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "transcript": "Ready."
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.audio.done", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1"
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.content_part.done", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1",
                "part": {"type": "audio", "transcript": "Ready."}
            }))
            .unwrap();
        let facts = decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 0, "item": audio_item(json!({"type": "audio", "text": "Ready."}))
            }))
            .unwrap();
        assert!(matches!(&facts[..], [ProviderFact::TextSealed { text, .. }] if text == "Ready."));
        let facts = decoder
            .on_event(completed(vec![audio_item(json!({
                "type": "audio", "transcript": "Ready.", "text": null
            }))]))
            .unwrap();
        assert_eq!(
            facts,
            vec![ProviderFact::ReactionCompleted {
                primary_text: Some(ProviderOutputKey::new(0))
            }]
        );
    }

    #[test]
    fn audio_only_completion_has_no_fabricated_text() {
        for content in [
            json!({"type": "audio"}),
            json!({"type": "audio", "transcript": null}),
            json!({"type": "audio", "text": null}),
        ] {
            let mut decoder = Decoder::with_audio();
            assert_eq!(
                decoder
                    .on_event(completed(vec![audio_item(content)]))
                    .unwrap(),
                vec![ProviderFact::ReactionCompleted { primary_text: None }]
            );
        }
    }

    #[test]
    fn audio_only_output_validates_stream_identity_and_completion() {
        let mut decoder = open_audio_output();
        decoder
            .on_event(json!({
                "type": "response.audio.delta", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "delta": "AA=="
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.audio_transcript.done", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1", "transcript": null
            }))
            .unwrap();
        decoder
            .on_event(json!({
                "type": "response.audio.done", "response_id": "response-1",
                "output_index": 0, "content_index": 0, "item_id": "message-1"
            }))
            .unwrap();
        assert!(decoder
            .on_event(json!({
                "type": "response.output_item.done", "response_id": "response-1",
                "output_index": 0, "item": audio_item(json!({"type": "audio"}))
            }))
            .unwrap()
            .is_empty());
        assert_eq!(
            decoder
                .on_event(completed(vec![audio_item(
                    json!({"type": "audio", "transcript": null})
                )]))
                .unwrap(),
            vec![ProviderFact::ReactionCompleted { primary_text: None }]
        );

        let mut invalid = open_audio_output();
        assert!(invalid
            .on_event(json!({
                "type": "response.audio.delta", "response_id": "response-1",
                "output_index": 0, "content_index": 1, "item_id": "message-1", "delta": "AA=="
            }))
            .is_err());
    }

    #[test]
    fn audio_mode_preserves_text_requirements_and_text_only_rejection() {
        let mut text_only = Decoder::default();
        assert!(text_only
            .on_event(completed(vec![audio_item(
                json!({"type": "audio", "text": "Ready."})
            )]))
            .is_err());
        let mut audio = Decoder::with_audio();
        assert!(audio
            .on_event(completed(vec![audio_item(
                json!({"type": "text", "transcript": "Ready."})
            )]))
            .is_err());
        let mut conflicting = Decoder::with_audio();
        assert!(conflicting
            .on_event(completed(vec![audio_item(json!({
                "type": "audio", "transcript": "Ready.", "text": "Different."
            }))]))
            .is_err());
    }
}
