// Portions adapted from OpenAI Codex, Copyright 2025 OpenAI.
// SPDX-License-Identifier: Apache-2.0
// Source and modifications: ../../../../third_party/codex/README.md

//! Codex local compaction: append the checkpoint prompt to the current history,
//! drain ordinary inference, retain recent user text, and append the summary.
//! The caller owns the immutable source and installs the resulting candidate.

use serde_json::{json, Map, Value};

use super::{
    output::OpenAiOutputLedger, parallel_compaction::ParallelCompactionFault, OpenAiWireEvent,
};

pub(super) const SUMMARIZATION_PROMPT: &str = include_str!("local_compaction/prompt.md");
pub(super) const SUMMARY_PREFIX: &str = include_str!("local_compaction/summary_prefix.md");
const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;

pub(super) fn summary_input(history: &[Value]) -> Vec<Value> {
    let mut input = history.to_vec();
    input.push(user_message(SUMMARIZATION_PROMPT));
    input
}

fn user_message(text: &str) -> Value {
    json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]})
}

fn content_items_to_text(item: &Value) -> Option<String> {
    let content = item.get("content")?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let pieces = content
        .as_array()?
        .iter()
        .filter(|part| matches!(part["type"].as_str(), Some("input_text" | "output_text")))
        .filter_map(|part| part["text"].as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    (!pieces.is_empty()).then(|| pieces.join("\n"))
}

/// Match Codex's recent-user-message selection, excluding previous summaries.
/// The small-window cap leaves room for both the summary and live foreground
/// tail. This is a wire projection; it never alters canonical user messages.
pub(super) fn build_compacted_history(
    source: &[Value],
    summary: &str,
    context_window_tokens: u64,
) -> Result<Vec<Value>, ParallelCompactionFault> {
    if summary.trim().is_empty() {
        return Err(ParallelCompactionFault::Protocol);
    }
    let mut remaining = COMPACT_USER_MESSAGE_MAX_TOKENS.min(context_window_tokens / 10);
    let summary_prefix = format!("{}\n", SUMMARY_PREFIX.trim_end());
    let mut retained = Vec::new();
    for item in source.iter().rev() {
        if remaining == 0 {
            break;
        }
        if item["role"] != "user" {
            continue;
        }
        let Some(text) = content_items_to_text(item) else {
            continue;
        };
        if text.starts_with(&summary_prefix) {
            continue;
        }
        let tokens = text.len().div_ceil(4) as u64;
        if tokens <= remaining {
            retained.push(user_message(&text));
            remaining -= tokens;
        } else {
            retained.push(user_message(&truncate_user_text(
                &text,
                remaining as usize * 4,
            )));
            break;
        }
    }
    retained.reverse();
    retained.push(user_message(&format!("{summary_prefix}{summary}")));
    let old_bytes = serde_json::to_vec(source)
        .map_err(|_| ParallelCompactionFault::Protocol)?
        .len();
    let new_bytes = serde_json::to_vec(&retained)
        .map_err(|_| ParallelCompactionFault::Protocol)?
        .len();
    if new_bytes >= old_bytes {
        return Err(ParallelCompactionFault::NoReduction);
    }
    Ok(retained)
}

fn truncate_user_text(text: &str, max_bytes: usize) -> String {
    const MARKER: &str = "\n[...truncated...]\n";
    if max_bytes < MARKER.len() {
        return ".".repeat(max_bytes.min(3));
    }
    let available = max_bytes - MARKER.len();
    let mut head = available.div_ceil(2);
    let mut tail = text.len().saturating_sub(available / 2);
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{MARKER}{}", &text[..head], &text[tail..])
}

/// Observe the ordinary Responses stream without any application capabilities.
/// Like Codex, use the last sealed assistant message only after completion.
#[derive(Default)]
pub(super) struct SummaryStream {
    ledger: OpenAiOutputLedger,
    last_sequence: Option<u64>,
    last_summary: Option<(u64, String)>,
    output_bytes: usize,
}

impl SummaryStream {
    pub(super) fn event(
        &mut self,
        frame: OpenAiWireEvent,
        max_text_bytes: usize,
    ) -> Result<Option<String>, ParallelCompactionFault> {
        let protocol = |_| ParallelCompactionFault::Protocol;
        let payload = &frame.payload;
        if let Some(sequence) = payload.get("sequence_number") {
            let sequence = sequence.as_u64().ok_or(ParallelCompactionFault::Protocol)?;
            if self.last_sequence.is_some_and(|last| sequence <= last) {
                return Err(ParallelCompactionFault::Protocol);
            }
            self.last_sequence = Some(sequence);
        }
        match frame.event_type.as_str() {
            "response.created" => self
                .ledger
                .record_response_created(payload)
                .map_err(protocol)?,
            "response.in_progress" => self
                .ledger
                .record_response_in_progress(payload)
                .map_err(protocol)?,
            "response.content_part.added" => self
                .ledger
                .record_content_part_added(payload)
                .map_err(protocol)?,
            "response.content_part.done" => self
                .ledger
                .record_content_part_done(payload)
                .map_err(protocol)?,
            "response.output_text.annotation.added" => self
                .ledger
                .record_text_annotation_added(payload)
                .map_err(protocol)?,
            "response.output_text.delta" => {
                let text = string(payload, "delta")?;
                self.output_bytes = self
                    .output_bytes
                    .checked_add(text.len())
                    .ok_or(ParallelCompactionFault::Limit)?;
                if self.output_bytes > max_text_bytes {
                    return Err(ParallelCompactionFault::Limit);
                }
                self.ledger
                    .record_text_delta(payload, text)
                    .map_err(protocol)?;
            }
            "response.output_text.done" => {
                let text = string(payload, "text")?;
                if text.len() > max_text_bytes {
                    return Err(ParallelCompactionFault::Limit);
                }
                self.ledger
                    .record_text_done(payload, text)
                    .map_err(protocol)?;
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                self.ledger
                    .record_reasoning_text(payload, frame.event_type.ends_with(".done"))
                    .map_err(protocol)?;
            }
            "response.output_item.added" | "response.output_item.done" => {
                let kind = payload.get("item").and_then(|item| item["type"].as_str());
                if !matches!(kind, Some("message" | "reasoning")) {
                    return Err(ParallelCompactionFault::Protocol);
                }
                if frame.event_type.ends_with(".added") {
                    self.ledger
                        .record_added(payload)
                        .map_err(|_| ParallelCompactionFault::Protocol)?;
                } else {
                    self.ledger.record_done(payload).map_err(protocol)?;
                    if kind == Some("message") {
                        let index = payload
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .ok_or(ParallelCompactionFault::Protocol)?;
                        let (text, _) = self
                            .ledger
                            .message_text(index)
                            .ok_or(ParallelCompactionFault::Protocol)?;
                        if text.len() > max_text_bytes {
                            return Err(ParallelCompactionFault::Limit);
                        }
                        if self
                            .last_summary
                            .as_ref()
                            .is_none_or(|(previous, _)| index > *previous)
                        {
                            self.last_summary = Some((index, text.to_owned()));
                        }
                    }
                }
            }
            "response.completed" => {
                let response = payload
                    .get("response")
                    .ok_or(ParallelCompactionFault::Protocol)?;
                if response["status"] != "completed"
                    || response.get("error").is_some_and(|error| !error.is_null())
                    || response
                        .get("incomplete_details")
                        .is_some_and(|details| !details.is_null())
                {
                    return Err(ParallelCompactionFault::Protocol);
                }
                self.ledger
                    .validate_completed_allowing_no_primary(payload)
                    .map_err(|_| ParallelCompactionFault::Protocol)?;
                let (_, summary) = self
                    .last_summary
                    .take()
                    .ok_or(ParallelCompactionFault::Protocol)?;
                if summary.trim().is_empty() {
                    return Err(ParallelCompactionFault::Protocol);
                }
                return Ok(Some(summary));
            }
            "response.failed" | "response.incomplete" | "error" => {
                return Err(ParallelCompactionFault::Protocol)
            }
            event
                if event.starts_with("response.output_")
                    || event.starts_with("response.function_call") =>
            {
                return Err(ParallelCompactionFault::Protocol)
            }
            _ => {}
        }
        Ok(None)
    }
}

fn string<'a>(
    payload: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, ParallelCompactionFault> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or(ParallelCompactionFault::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: Value) -> OpenAiWireEvent {
        serde_json::from_value(value).unwrap()
    }

    fn message_events(index: u64, text: &str) -> Vec<Value> {
        let id = format!("message-{index}");
        let phase = if index == 0 {
            "commentary"
        } else {
            "final_answer"
        };
        vec![
            json!({"type":"response.output_item.added","output_index":index,"item":{"type":"message","id":id,"role":"assistant","status":"in_progress","phase":phase,"content":[]}}),
            json!({"type":"response.output_text.delta","output_index":index,"content_index":0,"item_id":id,"delta":text}),
            json!({"type":"response.output_text.done","output_index":index,"content_index":0,"item_id":id,"text":text}),
            json!({"type":"response.output_item.done","output_index":index,"item":{"type":"message","id":id,"role":"assistant","status":"completed","phase":phase,"content":[{"type":"output_text","text":text,"annotations":[]}]}}),
        ]
    }

    #[test]
    fn checkpoint_prompt_is_last_and_history_is_unchanged() {
        let history = vec![
            user_message("task"),
            json!({"type":"function_call_output","call_id":"call","output":"result"}),
        ];
        let input = summary_input(&history);
        assert_eq!(&input[..history.len()], &history);
        assert_eq!(input.last().unwrap(), &user_message(SUMMARIZATION_PROMPT));
    }

    #[test]
    fn rebuild_retains_recent_user_text_in_order_and_excludes_previous_summaries() {
        let source = vec![
            user_message("first request"),
            user_message(&format!("{}\nprevious summary", SUMMARY_PREFIX.trim_end())),
            json!({"type":"message","role":"assistant","content":"long old history ".repeat(200)}),
            user_message("second request"),
        ];
        let compacted = build_compacted_history(&source, "Current progress", 272_000).unwrap();
        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[0], user_message("first request"));
        assert_eq!(compacted[1], user_message("second request"));
        assert_eq!(
            compacted[2],
            user_message(&format!("{}\nCurrent progress", SUMMARY_PREFIX.trim_end()))
        );
        let second_source = [
            compacted,
            vec![json!({"type":"message","role":"assistant","content":"new history ".repeat(200)})],
        ]
        .concat();
        let again = build_compacted_history(&second_source, "Updated progress", 272_000).unwrap();
        assert_eq!(again.len(), 3);
        assert!(!serde_json::to_string(&again)
            .unwrap()
            .contains("Current progress"));
    }

    #[test]
    fn retained_user_budget_keeps_newest_requests_and_truncates_unicode_safely() {
        let old = format!("start {} end", "状态🙂".repeat(40_000));
        let source = vec![
            user_message("oldest"),
            user_message(&old),
            user_message("latest request"),
        ];
        for window in [2000, 272_000] {
            let compacted = build_compacted_history(&source, "Progress", window).unwrap();
            assert_eq!(compacted.len(), 3);
            let text = compacted[0]["content"][0]["text"].as_str().unwrap();
            assert!(text.starts_with("start ") && text.ends_with(" end"));
            assert!(text.contains("[...truncated...]"));
            assert_eq!(compacted[1], user_message("latest request"));
            assert!(
                text.len().div_ceil(4) + "latest request".len().div_ceil(4)
                    <= 20_000.min(window as usize / 10)
            );
        }
        for bytes in 0..20 {
            assert!(truncate_user_text("🙂状态🙂状态🙂状态", bytes).len() <= bytes);
        }
    }

    #[test]
    fn empty_or_nonreducing_summaries_preserve_the_old_window() {
        assert_eq!(
            build_compacted_history(&[user_message("small")], " ", 1000),
            Err(ParallelCompactionFault::Protocol)
        );
        assert_eq!(
            build_compacted_history(&[user_message("small")], "larger", 1000),
            Err(ParallelCompactionFault::NoReduction)
        );
    }

    #[test]
    fn stream_uses_last_assistant_text_only_after_valid_completion() {
        let mut stream = SummaryStream::default();
        for event in message_events(0, "Preparing summary")
            .into_iter()
            .chain(message_events(1, "Final summary"))
        {
            assert_eq!(stream.event(frame(event), 4096).unwrap(), None);
        }
        // Compatible endpoints may omit the terminal output array; sealed
        // lifecycle output is still required and fully validated.
        assert_eq!(stream.event(frame(json!({"type":"response.completed","response":{"id":"response","status":"completed"}})), 4096).unwrap(), Some("Final summary".into()));
    }

    #[test]
    fn invalid_streams_cannot_publish_a_summary() {
        for event in [
            json!({"type":"response.incomplete"}),
            json!({"type":"response.failed"}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","encrypted_content":"opaque"}}),
            json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[]}}),
        ] {
            assert_eq!(
                SummaryStream::default().event(frame(event), 4096),
                Err(ParallelCompactionFault::Protocol)
            );
        }
        let mut stream = SummaryStream::default();
        for event in message_events(0, "sealed text") {
            stream.event(frame(event), 4096).unwrap();
        }
        let mut changed = message_events(0, "different text").pop().unwrap()["item"].clone();
        changed["status"] = json!("completed");
        assert_eq!(stream.event(frame(json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[changed]}})), 4096), Err(ParallelCompactionFault::Protocol));
    }

    #[test]
    fn streamed_text_and_sequence_limits_are_enforced() {
        let events = message_events(0, "too much output");
        let mut stream = SummaryStream::default();
        stream.event(frame(events[0].clone()), 3).unwrap();
        assert_eq!(
            stream.event(frame(events[1].clone()), 3),
            Err(ParallelCompactionFault::Limit)
        );
        let mut stream = SummaryStream::default();
        let mut added = events[0].clone();
        added["sequence_number"] = json!(2);
        stream.event(frame(added), 100).unwrap();
        let mut delta = events[1].clone();
        delta["sequence_number"] = json!(1);
        assert_eq!(
            stream.event(frame(delta), 100),
            Err(ParallelCompactionFault::Protocol)
        );
    }
}
