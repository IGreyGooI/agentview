use std::{collections::VecDeque, sync::Arc, time::Duration};

use serde_json::{json, Value};
use tokio::{sync::mpsc, time::Instant};

use crate::{
    component::execution::{
        FrameRevision, ProviderFact, ProviderOutputKey, ProviderToolCall, ToolOutput,
    },
    transcript::{AssistantTextStatus, CanonicalInputItem},
};

use super::{
    request::Request, transport, wire, Accepted, Code, DoubaoRealtimeProvider, Fault, Input, Reason,
};
use crate::provider::qwen_realtime::{
    media::Interrupt, QwenRealtimeAudioEvent as Audio, QwenRealtimeResponseStatus as Status,
    QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ,
};

const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

pub(super) struct Exchange<'a> {
    provider: &'a mut DoubaoRealtimeProvider,
    revision: FrameRevision,
    request: Request,
    facts: VecDeque<ProviderFact>,
    replay: Vec<CanonicalInputItem>,
    response: Option<Response>,
    last_response: Option<String>,
    batches: VecDeque<Vec<(String, Option<String>)>>,
    tool_calls: Vec<String>,
    results: mpsc::UnboundedReceiver<ToolOutput>,
    result_sender: mpsc::UnboundedSender<ToolOutput>,
    interrupt: Option<Interrupt>,
    deadline: Option<Instant>,
    done: bool,
    settled: bool,
}

struct Response {
    local_id: String,
    upstream_id: Option<String>,
    question_id: Option<String>,
    text: String,
    audio_open: bool,
    cancelled: bool,
}

impl<'a> Exchange<'a> {
    pub(super) fn new(
        provider: &'a mut DoubaoRealtimeProvider,
        revision: FrameRevision,
        request: Request,
    ) -> Self {
        let (result_sender, results) = mpsc::unbounded_channel();
        let last_response = provider
            .accepted
            .as_ref()
            .and_then(|state| state.last_response.clone());
        Self {
            provider,
            revision,
            request,
            facts: VecDeque::new(),
            replay: Vec::new(),
            response: None,
            last_response,
            batches: VecDeque::new(),
            tool_calls: Vec::new(),
            results,
            result_sender,
            interrupt: None,
            deadline: None,
            done: false,
            settled: false,
        }
    }

    pub(super) async fn next(&mut self) -> Option<Result<ProviderFact, Fault>> {
        if self.settled {
            return None;
        }
        match self.read_fact().await {
            Ok(Some(fact)) => Some(Ok(fact)),
            Ok(None) => {
                self.settled = true;
                self.provider.accepted = Some(Accepted {
                    revision: self.revision,
                    replay: std::mem::take(&mut self.replay),
                    configuration: std::mem::take(&mut self.request.configuration),
                    tool_calls: std::mem::take(&mut self.tool_calls),
                    last_response: self.last_response.take(),
                });
                None
            }
            Err(fault) => {
                self.lose_session(fault);
                self.settled = true;
                Some(Err(fault))
            }
        }
    }

    fn lose_session(&mut self, fault: Fault) {
        self.provider.connection = None;
        self.provider.accepted = None;
        self.provider.terminal = Some(Fault::terminal(fault.code(), fault.reason()));
        if let Some(media) = &self.provider.media {
            media.active_response.send_replace(None);
        }
    }

    async fn read_fact(&mut self) -> Result<Option<ProviderFact>, Fault> {
        for (command, ack) in std::mem::take(&mut self.request.commands) {
            transport::send(
                self.provider
                    .connection
                    .as_mut()
                    .expect("connected at handoff"),
                command,
            )
            .await?;
            if let Some(ack) = ack {
                self.wait_ack(ack).await?;
            }
        }
        loop {
            if let Some(fact) = self.facts.pop_front() {
                return Ok(Some(fact));
            }
            if self.done {
                return Ok(None);
            }
            let socket = self
                .provider
                .connection
                .as_mut()
                .expect("connected at handoff");
            let media = self.provider.media.as_mut().expect("mounted audio");
            tokio::select! {
                event = transport::receive_unbounded(socket) => {
                    let (event, _) = event?;
                    self.server_event(event).await?;
                }
                command = media.input.recv(), if !media.finished => self.input(command).await?,
                Some(output) = self.results.recv(), if !self.batches.is_empty() => self.tool_output(output).await?,
                () = wait_until(self.deadline) => {
                    return Err(Fault::retryable(Code::Unavailable, Reason::StreamTimeout));
                }
            }
        }
    }

    async fn wait_ack(&mut self, expected: &str) -> Result<(), Fault> {
        tokio::time::timeout(ACK_TIMEOUT, async {
            loop {
                let (event, _) = transport::receive_unbounded(
                    self.provider
                        .connection
                        .as_mut()
                        .expect("connected at handoff"),
                )
                .await?;
                if event["type"] == expected {
                    self.publish_raw(event);
                    return Ok(());
                }
                self.server_event(event).await?;
            }
        })
        .await
        .map_err(|_| Fault::retryable(Code::Unavailable, Reason::StreamTimeout))?
    }

    async fn input(&mut self, command: Option<Input>) -> Result<(), Fault> {
        match command {
            Some(Input::Append(audio)) => self.send(wire::append_audio(&audio)).await?,
            Some(Input::SetMuted(muted)) => {
                let kind = if muted {
                    "input_audio_mute.commit"
                } else {
                    "input_audio_unmute.commit"
                };
                self.send(json!({"type": kind, "event_id": wire::id("event")}))
                    .await?;
            }
            Some(Input::Interrupt(request)) => self.cancel(request).await?,
            Some(Input::Finish) | None => {
                self.provider
                    .media
                    .as_mut()
                    .expect("mounted audio")
                    .finished = true;
                if self.response.is_none() {
                    self.complete(None);
                }
            }
            Some(Input::Commit | Input::Clear) => {}
        }
        Ok(())
    }

    async fn send(&mut self, event: Value) -> Result<(), Fault> {
        transport::send(
            self.provider
                .connection
                .as_mut()
                .expect("connected at handoff"),
            event.to_string(),
        )
        .await
    }

    async fn cancel(&mut self, request: Interrupt) -> Result<(), Fault> {
        // The input component may have captured its response ID before the
        // server ended that reply. Seeduplex cancels the current reply without
        // any wire ID, so ignore stale requests instead of cancelling its successor.
        let media = self.provider.media.as_ref().expect("mounted audio");
        let active = self
            .response
            .as_ref()
            .filter(|response| !response.cancelled)
            .map(|response| response.local_id.as_str());
        let valid = active.is_some_and(|active| {
            request.response_id == active
                && media
                    .cancel
                    .as_ref()
                    .is_none_or(|cancel| cancel.response_id == active)
        });
        if !valid || self.interrupt.is_some() {
            let _ = request.result.send(Ok(false));
            return Ok(());
        }
        self.interrupt = Some(request);
        if media.cancel.is_none() {
            let response_id = active.expect("validated response").to_owned();
            let event_id = wire::id("event");
            self.send(json!({"type": "response.cancel", "event_id": event_id}))
                .await?;
            self.provider.media.as_mut().expect("mounted audio").cancel =
                Some(super::PendingCancel {
                    event_id,
                    response_id,
                });
        }
        Ok(())
    }

    fn publish_raw(&self, event: Value) {
        self.provider
            .media
            .as_ref()
            .expect("mounted audio")
            .events
            .publish_server_event(Arc::new(event));
    }

    fn publish(&self, event: Audio) {
        self.provider
            .media
            .as_ref()
            .expect("mounted audio")
            .events
            .publish(event);
    }

    async fn server_event(&mut self, event: Value) -> Result<(), Fault> {
        self.publish_raw(event.clone());
        let kind = event["type"].as_str().unwrap_or_default();
        match kind {
            "error" => {
                let detail = event.get("error").unwrap_or(&event);
                let code = detail.get("code").or_else(|| detail.get("status_code"));
                let provider_code = code.and_then(|code| {
                    code.as_i64()
                        .or_else(|| code.as_str().and_then(|code| code.parse().ok()))
                });
                tracing::warn!(event = "seeduplex_upstream_error", provider_code);
                // A cancel may lose a race with the old response ending. Only
                // a rejection correlated to this control should be handled
                // here. Other provider errors keep their regular fault path.
                let media = self.provider.media.as_mut().expect("mounted audio");
                if media.cancel.as_ref().is_some_and(|cancel| {
                    event["error"]["event_id"] == cancel.event_id
                        || event["event_id"] == cancel.event_id
                }) {
                    media.cancel = None;
                    if let Some(request) = self.interrupt.take() {
                        let _ = request.result.send(Err(upstream_fault()));
                    }
                    return Ok(());
                }
                return Err(upstream_fault());
            }
            "session.closed" => {
                return Err(Fault::terminal(Code::Unavailable, Reason::StreamTransport))
            }
            "conversation.item.input_audio_transcription.started" => {
                self.input_started(&event);
                // Seeduplex's automatic interruption starts a new ASR item
                // before it sends the old reply's bare response.done. Sending
                // an extra response.cancel here races that provider boundary.
                // Stop local playback immediately and drain the old reply.
                let item = string(&event, "item_id");
                if self.response.as_ref().is_some_and(|response| {
                    item.is_none() || response.question_id.as_deref() != item
                }) {
                    self.mark_cancelled();
                }
            }
            "conversation.item.input_audio_transcription.delta"
            | "conversation.item.input_audio_transcription.completed"
            | "conversation.item.input_audio_transcription.failed" => {
                self.transcription(kind, &event)
            }
            "response.canceled" => {
                // Older exchanges can complete before the cancel ACK arrives.
                // The ACK has no response ID; consume it only for the reply
                // captured when we sent the one outstanding cancel.
                let media = self.provider.media.as_mut().expect("mounted audio");
                if let Some(cancel) = media.cancel.take() {
                    if self
                        .response
                        .as_ref()
                        .is_some_and(|response| response.local_id == cancel.response_id)
                    {
                        // The service can send ID-less tail audio after this
                        // ACK. Stop playback immediately and keep its Frame
                        // open until the actual response.done drains that tail.
                        self.mark_cancelled();
                    }
                }
            }
            "response.done" => {
                if !self.is_stale(&event) {
                    let status = match event["response"]["status"]
                        .as_str()
                        .or(event["status"].as_str())
                    {
                        Some("canceled" | "cancelled") => Status::Cancelled,
                        Some("incomplete") => Status::Incomplete,
                        Some("failed") => Status::Failed,
                        // A natural interruption can deliver done before the
                        // next ASR starts. If the provider never finished its
                        // audio, discard queued playback for the cut-off reply.
                        None if self
                            .response
                            .as_ref()
                            .is_some_and(|response| response.audio_open) =>
                        {
                            Status::Cancelled
                        }
                        _ => Status::Completed,
                    };
                    if self.response.is_some() {
                        self.complete(Some(status));
                    } else if event_id(&event).is_some() {
                        self.begin_response(&event);
                        self.complete(Some(status));
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if self.accepts_output(&event) {
                    self.begin_response(&event);
                    self.tool_requests(&event)?;
                }
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.output_audio.started"
            | "response.output_audio.delta"
            | "response.output_audio.done" => {
                if self.accepts_output(&event) {
                    self.begin_response(&event);
                    self.response_event(kind, &event)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn input_started(&mut self, event: &Value) {
        let media = self.provider.media.as_mut().expect("mounted audio");
        let item = string(event, "item_id")
            .map(str::to_owned)
            .unwrap_or_else(|| wire::id("input"));
        media.input_item = Some(item.clone());
        self.publish_raw(json!({"type": "input_audio_buffer.speech_started", "item_id": item}));
    }

    fn transcription(&mut self, kind: &str, event: &Value) {
        let media = self.provider.media.as_mut().expect("mounted audio");
        let item_id = string(event, "item_id")
            .map(str::to_owned)
            .or_else(|| media.input_item.clone())
            .unwrap_or_else(|| wire::id("input"));
        let content_index = event["content_index"].as_u64().unwrap_or_default();
        match kind {
            "conversation.item.input_audio_transcription.delta" => {
                self.publish(Audio::InputTranscriptDelta {
                    item_id,
                    content_index,
                    text: String::new(),
                    stash: event["delta"].as_str().unwrap_or_default().to_owned(),
                });
            }
            "conversation.item.input_audio_transcription.completed" => {
                media.input_item = None;
                self.publish_raw(
                    json!({"type": "input_audio_buffer.committed", "item_id": item_id}),
                );
                self.publish(Audio::InputCommitted {
                    item_id: item_id.clone(),
                });
                self.publish(Audio::InputTranscriptDone {
                    item_id,
                    content_index,
                    transcript: event["transcript"]
                        .as_str()
                        .or(event["text"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            _ => {
                media.input_item = None;
                self.publish(Audio::InputTranscriptFailed {
                    item_id,
                    error: upstream_fault(),
                });
            }
        }
    }

    fn is_stale(&self, event: &Value) -> bool {
        // The upstream can deliver old chunks after cancel. Discard those
        // carrying the sealed response ID and preserve the next exchange.
        event_id(event).is_some_and(|id| self.last_response.as_deref() == Some(id))
    }

    fn accepts_output(&self, event: &Value) -> bool {
        // The provider can keep sending chunks belonging to the cancelled
        // exchange. They must not restart playback or call another tool.
        !self
            .response
            .as_ref()
            .is_some_and(|response| response.cancelled)
            && !self.is_stale(event)
    }

    fn begin_response(&mut self, event: &Value) {
        self.deadline = Some(Instant::now() + RESPONSE_TIMEOUT);
        if let Some(response) = &mut self.response {
            if response.upstream_id.is_none() {
                response.upstream_id = event_id(event).map(str::to_owned);
            }
            if response.question_id.is_none() {
                response.question_id = string(event, "question_id").map(str::to_owned);
            }
            return;
        }
        let upstream_id = event_id(event).map(str::to_owned);
        let local_id = upstream_id.clone().unwrap_or_else(|| wire::id("response"));
        self.response = Some(Response {
            local_id: local_id.clone(),
            upstream_id,
            question_id: string(event, "question_id").map(str::to_owned),
            text: String::new(),
            audio_open: false,
            cancelled: false,
        });
        self.provider
            .media
            .as_ref()
            .expect("mounted audio")
            .active_response
            .send_replace(Some(local_id.clone()));
        self.publish(Audio::ResponseStarted {
            response_id: local_id,
        });
    }

    fn response_event(&mut self, kind: &str, event: &Value) -> Result<(), Fault> {
        let response = self.response.as_mut().expect("response started");
        let response_id = response.local_id.clone();
        let item_id = string(event, "item_id")
            .map(str::to_owned)
            .unwrap_or_else(|| response_id.clone());
        let content_index = event["content_index"].as_u64().unwrap_or_default();
        let audio = match kind {
            "response.output_audio.started" => {
                response.audio_open = true;
                return Ok(());
            }
            "response.output_text.delta" => {
                let delta = event["delta"]
                    .as_str()
                    .or(event["text"].as_str())
                    .unwrap_or_default();
                if response.text.len().saturating_add(delta.len()) > MAX_TEXT_BYTES {
                    return Err(Fault::retryable(Code::Limit, Reason::OutputLimit));
                }
                response.text.push_str(delta);
                Audio::OutputTranscriptDelta {
                    response_id,
                    item_id,
                    content_index,
                    delta: delta.to_owned(),
                }
            }
            "response.output_text.done" => {
                if let Some(text) = event["text"].as_str().or(event["transcript"].as_str()) {
                    response.text = text.to_owned();
                }
                Audio::OutputTranscriptDone {
                    response_id,
                    item_id,
                    content_index,
                    transcript: response.text.clone(),
                }
            }
            "response.output_audio.delta" => {
                response.audio_open = true;
                let pcm16 = event["delta"]
                    .as_str()
                    .or(event["audio"].as_str())
                    .and_then(|audio| wire::decode_audio(audio).ok());
                // PCM framing is supplied by the remote synthesizer. Dropping
                // a malformed chunk prevents it from shifting decoder alignment
                // without affecting later audio, tools or transcript delivery.
                let Some(pcm16) = pcm16 else {
                    tracing::warn!(event = "seeduplex_invalid_audio");
                    return Ok(());
                };
                Audio::AudioDelta {
                    response_id,
                    item_id,
                    content_index,
                    sample_rate_hz: QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ,
                    pcm16,
                }
            }
            "response.output_audio.done" => {
                response.audio_open = false;
                Audio::AudioDone {
                    response_id,
                    item_id,
                    content_index,
                }
            }
            _ => return Ok(()),
        };
        self.publish(audio);
        Ok(())
    }

    fn tool_requests(&mut self, event: &Value) -> Result<(), Fault> {
        let requests = event["items"]
            .as_array()
            .ok_or_else(transport::protocol_fault)?;
        let mut batch = Vec::new();
        for item in requests {
            let call_id = string(item, "call_id").ok_or_else(transport::protocol_fault)?;
            // The server owns call IDs and may repeat an event. Re-dispatching
            // this ID could repeat an external side effect, so skip the repeat.
            if self.tool_calls.iter().any(|id| id == call_id) {
                tracing::debug!(event = "seeduplex_duplicate_tool_call");
                continue;
            }
            let call = ProviderToolCall::new(
                call_id,
                string(item, "name").ok_or_else(transport::protocol_fault)?,
                item["arguments"]
                    .as_str()
                    .ok_or_else(transport::protocol_fault)?,
            )
            .map_err(|_| transport::protocol_fault())?
            .with_output_sender(self.result_sender.clone());
            let ordinal = self.tool_calls.len() as u64;
            self.tool_calls.push(call_id.to_owned());
            self.replay.push(CanonicalInputItem::ToolCall {
                call_id: call_id.to_owned(),
                name: call.name().to_owned(),
                raw_arguments: call.raw_arguments().to_owned(),
            });
            batch.push((call_id.to_owned(), None));
            self.facts.push_back(ProviderFact::ToolCall {
                output: ProviderOutputKey::new(ordinal),
                ordinal,
                call,
            });
        }
        if !batch.is_empty() {
            self.batches.push_back(batch);
        }
        Ok(())
    }

    async fn tool_output(&mut self, output: ToolOutput) -> Result<(), Fault> {
        if let Some((_, content)) = self
            .batches
            .iter_mut()
            .flatten()
            .find(|(id, _)| id == output.call_id())
        {
            *content = Some(output.content().to_owned());
        }
        while self
            .batches
            .front()
            .is_some_and(|batch| batch.iter().all(|(_, content)| content.is_some()))
        {
            let results: Vec<_> = self
                .batches
                .pop_front()
                .expect("complete batch")
                .into_iter()
                .map(|(id, content)| (id, content.expect("all outputs arrived")))
                .collect();
            self.send(wire::tool_results(&results)).await?;
            self.deadline = Some(Instant::now() + RESPONSE_TIMEOUT);
        }
        Ok(())
    }

    fn mark_cancelled(&mut self) {
        // The automatic VAD signal and a response.cancel acknowledgement can
        // both refer to the same reply. Publish one terminal playback event
        // and leave protocol completion to response.done.
        if let Some(response) = self
            .response
            .as_mut()
            .filter(|response| !response.cancelled)
        {
            response.cancelled = true;
            let response_id = response.local_id.clone();
            self.batches.clear();
            self.end_playback(response_id, Status::Cancelled);
        }
    }

    fn end_playback(&mut self, response_id: String, status: Status) {
        self.provider
            .media
            .as_ref()
            .expect("mounted audio")
            .active_response
            .send_replace(None);
        self.publish(Audio::ResponseEnded {
            response_id,
            status,
            error: None,
        });
        if let Some(request) = self.interrupt.take() {
            let _ = request.result.send(Ok(status == Status::Cancelled));
        }
    }

    fn complete(&mut self, status: Option<Status>) {
        let mut primary_text = None;
        if let (Some(response), Some(status)) = (self.response.take(), status) {
            self.last_response = response.upstream_id;
            if !response.cancelled {
                self.end_playback(response.local_id, status);
            }
            if !response.cancelled && status == Status::Completed && !response.text.is_empty() {
                let output = ProviderOutputKey::new(self.tool_calls.len() as u64);
                primary_text = Some(output);
                self.replay.push(CanonicalInputItem::AssistantText {
                    text: response.text.clone(),
                    phase: None,
                    status: AssistantTextStatus::Sealed,
                });
                self.facts.push_back(ProviderFact::TextSealed {
                    output,
                    phase: None,
                    text: response.text,
                });
            }
        }
        self.facts
            .push_back(ProviderFact::ReactionCompleted { primary_text });
        self.done = true;
    }
}

impl Drop for Exchange<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.lose_session(Fault::terminal(Code::Unavailable, Reason::StreamTransport));
        }
    }
}

fn string<'a>(event: &'a Value, field: &str) -> Option<&'a str> {
    event[field].as_str().filter(|value| !value.is_empty())
}

fn event_id(event: &Value) -> Option<&str> {
    string(event, "response_id").or_else(|| string(&event["response"], "id"))
}

fn upstream_fault() -> Fault {
    Fault::retryable(Code::Rejected, Reason::UpstreamRejected)
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
