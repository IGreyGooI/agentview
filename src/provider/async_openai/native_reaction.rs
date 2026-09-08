//! Frame-native `ReactionPort` implementation for OpenAI Responses.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::Arc,
    task::Poll,
};

use ::async_openai::config::Config;
use async_trait::async_trait;
use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use serde_json::{Map, Value};

use crate::{
    component::execution::reaction::{
        Frame, ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall, ReactionPort,
        ReactionPortFault, ResettableReactionPort, SubmitFault, TargetContinuity,
        TargetDeclaration,
    },
    transcript::{AssistantPhase, CanonicalInputItem},
};

use super::{
    AsyncOpenAiResponsesProvider, NativeToolLedger, OpenAiWireEvent, ResponsesExecutionMode,
    ResponsesReactionTarget, SseWireLimiter, declaration_state_lost_fault, exceeds_limit,
    frame_request::{
        PreparedResponsesFrameRequest, PrivateOutputKind, ResponsesFrameRequestFault,
        ResponsesFrameRequestState,
    },
    has_native_function_call_completed, has_plaintext_reasoning_completed_content,
    has_plaintext_reasoning_lifecycle_content, has_unsupported_completed_item,
    has_unsupported_content_part, has_unsupported_lifecycle_item, is_native_tool_event,
    native_function_call,
    output::{OpenAiOutputLedger, SealedOpenAiPrivateOutput},
    reaction_fault::{OpenAiFailureClass, OpenAiReactionFailure, map_openai_fault},
    sse_event_size,
    usage::{ResponseUsageObserver, observe_response_usage, validated_response_usage},
};

#[async_trait]
impl ReactionPort for AsyncOpenAiResponsesProvider {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.frame_native_declaration()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let declaration = self
            .frame_native_declaration()
            .map_err(SubmitFault::Rejected)?;
        frame.check_handoff_precondition(&declaration)?;
        let prepared = ResponsesFrameRequestState::prepare(
            self.reaction_frame.as_ref(),
            &frame,
            &self.encoder,
            self.max_responses_serialized_request_body_bytes,
        )
        .map_err(frame_submit_fault)?;
        serde_json::from_slice::<Value>(&prepared.request_body).map_err(|_| {
            rejected(
                OpenAiFailureClass::RequestPreparation,
                "native request body is not one JSON value",
            )
        })?;
        let request = self
            .client
            .post(self.config.url("/responses"))
            .headers(self.config.headers())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(prepared.request_body.clone())
            .build()
            .map_err(|error| {
                rejected(
                    OpenAiFailureClass::RequestPreparation,
                    format!("request build failed: {error:?}"),
                )
            })?;

        let client = self.client.clone();
        let execution_mode = &mut self.execution_mode;
        let target = &mut self.reaction_target;
        let frame_slot = &mut self.reaction_frame;
        let revision = frame.revision();
        let read_timeout = self.read_timeout;
        let max_response_body_bytes = self.max_response_body_bytes;
        let max_sse_event_bytes = self.max_sse_event_bytes;
        let max_output_text_bytes = self.max_output_text_bytes;
        let response_usage_observer = self.response_usage_observer.clone();
        let mut transport = Box::pin(client.execute(request));
        let mut prepared = Some(prepared);

        let first_response = futures::future::poll_fn(|cx| {
            let declaration = target.declaration().map_err(SubmitFault::Rejected)?;
            if let Err(fault) = validate_native_frame_state(&declaration, frame_slot.as_ref()) {
                target.record_fault(fault);
                return Poll::Ready(Err(SubmitFault::Rejected(fault)));
            }
            frame.check_handoff_precondition(&declaration)?;
            let Some(PreparedResponsesFrameRequest { state, .. }) = prepared.take() else {
                return Poll::Ready(Err(rejected(
                    OpenAiFailureClass::Internal,
                    "native Frame candidate was consumed before handoff",
                )));
            };
            match execution_mode {
                ResponsesExecutionMode::Unclaimed => {
                    *execution_mode = ResponsesExecutionMode::FrameNative;
                }
                ResponsesExecutionMode::FrameNative => {}
                #[cfg(feature = "legacy-provider-port")]
                ResponsesExecutionMode::Legacy => {
                    return Poll::Ready(Err(SubmitFault::Rejected(declaration_state_lost_fault())));
                }
            }
            let response = transport.as_mut().poll(cx);
            *frame_slot = Some(state);
            target.accept(revision);
            Poll::Ready(Ok::<_, SubmitFault>(response))
        })
        .await?;

        let mut continuity = PostHandoffContinuity::new(target);
        let pending = async move {
            let response = match first_response {
                Poll::Ready(Ok(response)) => response,
                Poll::Ready(Err(error)) => {
                    return continuity.fail(port_fault(
                        OpenAiFailureClass::RequestTransport,
                        format!("request transport failed: {error:?}"),
                    ));
                }
                Poll::Pending => match transport.await {
                    Ok(response) => response,
                    Err(error) => {
                        return continuity.fail(port_fault(
                            OpenAiFailureClass::RequestTransport,
                            format!("request transport failed: {error:?}"),
                        ));
                    }
                },
            };

            let status = response.status();
            if !status.is_success() {
                return continuity.fail(map_openai_fault(&OpenAiReactionFailure::http_status(
                    status,
                )));
            }
            let is_event_stream = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|media_type| {
                    media_type.trim().eq_ignore_ascii_case("text/event-stream")
                });
            if !is_event_stream {
                return continuity.fail(port_fault(
                    OpenAiFailureClass::ResponseProtocolRetryable,
                    "successful response did not use text/event-stream",
                ));
            }
            if response.content_length().is_some_and(|content_length| {
                content_length > u64::try_from(max_response_body_bytes).unwrap_or(u64::MAX)
            }) {
                return continuity.fail(port_fault(
                    OpenAiFailureClass::ResponseBodyLimit,
                    "response Content-Length exceeded the configured limit",
                ));
            }

            let mut response_body_bytes = 0_usize;
            let mut sse_wire_limiter = SseWireLimiter::new(max_sse_event_bytes);
            let response_stream = response.bytes_stream().map(move |chunk| {
                let chunk = chunk
                    .map_err(|error| NativeBodyStreamFault::Transport(format!("{error:?}")))?;
                let next_size = response_body_bytes
                    .checked_add(chunk.len())
                    .ok_or(NativeBodyStreamFault::BodyLimit)?;
                if next_size > max_response_body_bytes {
                    return Err(NativeBodyStreamFault::BodyLimit);
                }
                sse_wire_limiter
                    .observe(&chunk)
                    .map_err(|_| NativeBodyStreamFault::EventLimit)?;
                response_body_bytes = next_size;
                Ok(chunk)
            });
            let Some(frame_state) = frame_slot.as_mut() else {
                return continuity.fail(port_fault(
                    OpenAiFailureClass::Internal,
                    "native Frame state was unavailable after handoff",
                ));
            };
            let Some(target) = continuity.transfer() else {
                return continuity.fail(port_fault(
                    OpenAiFailureClass::Internal,
                    "native target continuity was transferred more than once",
                ));
            };
            let state = NativeOpenAiStreamState {
                stream: response_stream.eventsource(),
                target,
                frame_state,
                response_completed: false,
                terminal_published: false,
                fault_recorded: false,
                finished: false,
                last_sequence: None,
                next_output_index: Some(0),
                output_ledger: OpenAiOutputLedger::default(),
                native_tools: NativeToolLedger::default(),
                ready_outputs: BTreeMap::new(),
                buffered_deltas: BTreeMap::new(),
                emissions: VecDeque::new(),
                primary_text: None,
                output_text_bytes: 0,
                read_timeout,
                max_sse_event_bytes,
                max_output_text_bytes,
                response_usage_observer,
            };
            Box::pin(futures::stream::unfold(state, next_native_fact)) as ProviderFactStream<'a>
        };

        Ok(Box::pin(futures::stream::once(pending).flatten()))
    }
}

impl ResettableReactionPort for AsyncOpenAiResponsesProvider {
    fn reset_model_context(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        // Validate the current native state before changing it. In particular,
        // an Accepted declaration without its Frame state is already terminal.
        self.frame_native_declaration()?;

        self.reaction_target.lose_continuity();
        let declaration = self.reaction_target.declaration()?;
        if !matches!(
            declaration.continuity(),
            TargetContinuity::FullRequired { .. }
        ) {
            return Err(declaration_state_lost_fault());
        }

        // A later Full must be rebuilt from its Frame, not reconciled against
        // the prior wire prefix or its provider output.
        self.reaction_frame = None;
        Ok(declaration)
    }
}

impl AsyncOpenAiResponsesProvider {
    fn frame_native_declaration(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.ensure_frame_native_mode()?;
        let declaration = self.reaction_target.declaration()?;
        if let Err(fault) = validate_native_frame_state(&declaration, self.reaction_frame.as_ref())
        {
            self.reaction_target.record_fault(fault);
            return Err(fault);
        }
        Ok(declaration)
    }
}

fn validate_native_frame_state(
    declaration: &TargetDeclaration,
    state: Option<&ResponsesFrameRequestState>,
) -> Result<(), ReactionPortFault> {
    let TargetContinuity::Accepted { revision, .. } = declaration.continuity() else {
        return Ok(());
    };
    if state.is_some_and(|state| state.revision() == *revision) {
        Ok(())
    } else {
        Err(declaration_state_lost_fault())
    }
}

fn frame_submit_fault(error: ResponsesFrameRequestFault) -> SubmitFault {
    let class = error.failure_class();
    rejected(class, format!("{error:?}"))
}

fn rejected(class: OpenAiFailureClass, diagnostic: impl Into<String>) -> SubmitFault {
    SubmitFault::Rejected(port_fault(class, diagnostic))
}

fn port_fault(class: OpenAiFailureClass, diagnostic: impl Into<String>) -> ReactionPortFault {
    map_openai_fault(&OpenAiReactionFailure::message(class, diagnostic))
}

fn post_handoff_fault(fault: ReactionPortFault) -> ProviderFactStream<'static> {
    Box::pin(futures::stream::once(async move { Err(fault) }))
}

struct PostHandoffContinuity<'a> {
    target: Option<&'a mut ResponsesReactionTarget>,
}

impl<'a> PostHandoffContinuity<'a> {
    fn new(target: &'a mut ResponsesReactionTarget) -> Self {
        Self {
            target: Some(target),
        }
    }

    fn transfer(&mut self) -> Option<&'a mut ResponsesReactionTarget> {
        self.target.take()
    }

    fn fail(&mut self, fault: ReactionPortFault) -> ProviderFactStream<'static> {
        if let Some(target) = self.target.take() {
            target.record_fault(fault);
        }
        post_handoff_fault(fault)
    }
}

impl Drop for PostHandoffContinuity<'_> {
    fn drop(&mut self) {
        if let Some(target) = self.target.take() {
            target.lose_continuity();
        }
    }
}

#[derive(Debug)]
enum NativeBodyStreamFault {
    Transport(String),
    BodyLimit,
    EventLimit,
}

struct NativeOpenAiStreamState<'a, S> {
    stream: S,
    target: &'a mut ResponsesReactionTarget,
    frame_state: &'a mut ResponsesFrameRequestState,
    response_completed: bool,
    terminal_published: bool,
    fault_recorded: bool,
    finished: bool,
    last_sequence: Option<u64>,
    next_output_index: Option<u64>,
    output_ledger: OpenAiOutputLedger,
    native_tools: NativeToolLedger,
    ready_outputs: BTreeMap<u64, ReadyOutput>,
    buffered_deltas: BTreeMap<u64, VecDeque<ProviderFact>>,
    emissions: VecDeque<NativeEmission>,
    primary_text: Option<ProviderOutputKey>,
    output_text_bytes: usize,
    read_timeout: std::time::Duration,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    response_usage_observer: Option<Arc<ResponseUsageObserver>>,
}

enum ReadyOutput {
    Text {
        phase: Option<AssistantPhase>,
        text: String,
        wire_item: Value,
    },
    Tool {
        call: ProviderToolCall,
        wire_item: Value,
    },
    Private {
        kind: PrivateOutputKind,
        wire_item: Value,
    },
}

enum NativeEmission {
    Fact(ProviderFact),
    Public {
        output_index: u64,
        canonical_item: CanonicalInputItem,
        wire_item: Value,
        fact: ProviderFact,
    },
    Private {
        output_index: u64,
        kind: PrivateOutputKind,
        wire_item: Value,
    },
}

async fn next_native_fact<S>(
    mut state: NativeOpenAiStreamState<'_, S>,
) -> Option<(
    Result<ProviderFact, ReactionPortFault>,
    NativeOpenAiStreamState<'_, S>,
)>
where
    S: futures::Stream<
            Item = Result<eventsource_stream::Event, EventStreamError<NativeBodyStreamFault>>,
        > + Unpin,
{
    loop {
        match state.next_emission() {
            Ok(Some(fact)) => return Some((Ok(fact), state)),
            Ok(None) => {}
            Err(fault) => return failed_native_stream(state, fault),
        }
        if state.finished {
            return None;
        }

        let event = match tokio::time::timeout(state.read_timeout, state.stream.next()).await {
            Err(_) => {
                return failed_native_stream(
                    state,
                    port_fault(OpenAiFailureClass::StreamTimeout, "stream read timed out"),
                );
            }
            Ok(Some(Err(EventStreamError::Transport(NativeBodyStreamFault::Transport(
                diagnostic,
            ))))) => {
                return failed_native_stream(
                    state,
                    port_fault(OpenAiFailureClass::StreamTransport, diagnostic),
                );
            }
            Ok(Some(Err(EventStreamError::Transport(NativeBodyStreamFault::BodyLimit)))) => {
                return failed_native_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseBodyLimit,
                        "stream body limit exceeded",
                    ),
                );
            }
            Ok(Some(Err(EventStreamError::Transport(NativeBodyStreamFault::EventLimit)))) => {
                return failed_native_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::StreamEventLimit,
                        "SSE event wire limit exceeded",
                    ),
                );
            }
            Ok(Some(Err(error))) => {
                return failed_native_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        format!("SSE decode failed: {error:?}"),
                    ),
                );
            }
            Ok(Some(Ok(event))) => event,
            Ok(None) => {
                if state.terminal_published {
                    return None;
                }
                return failed_native_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        "stream ended before a validated terminal fact",
                    ),
                );
            }
        };

        if sse_event_size(&event).is_none_or(|event_size| event_size > state.max_sse_event_bytes) {
            return failed_native_stream(
                state,
                port_fault(
                    OpenAiFailureClass::StreamEventLimit,
                    "decoded SSE event exceeded configured limit",
                ),
            );
        }
        if event.data == "[DONE]" {
            if state.terminal_published {
                return None;
            }
            return failed_native_stream(
                state,
                port_fault(
                    OpenAiFailureClass::ResponseProtocolRetryable,
                    "DONE arrived before a validated terminal fact",
                ),
            );
        }
        let event_json = match serde_json::from_str::<Value>(&event.data) {
            Ok(value) => value,
            Err(error) => {
                return failed_native_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        format!("event JSON failed: {error:?}"),
                    ),
                );
            }
        };
        let frame = match serde_json::from_value::<OpenAiWireEvent>(event_json) {
            Ok(frame) => frame,
            Err(error) => {
                return failed_native_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        format!("event envelope failed: {error:?}"),
                    ),
                );
            }
        };
        if let Err(fault) = state.process_wire_event(frame) {
            return failed_native_stream(state, fault);
        }
    }
}

fn failed_native_stream<S>(
    mut state: NativeOpenAiStreamState<'_, S>,
    fault: ReactionPortFault,
) -> Option<(
    Result<ProviderFact, ReactionPortFault>,
    NativeOpenAiStreamState<'_, S>,
)> {
    state.finished = true;
    state.target.record_fault(fault);
    state.fault_recorded = true;
    Some((Err(fault), state))
}

impl<S> NativeOpenAiStreamState<'_, S> {
    fn next_emission(&mut self) -> Result<Option<ProviderFact>, ReactionPortFault> {
        while let Some(emission) = self.emissions.pop_front() {
            match emission {
                NativeEmission::Fact(fact) => {
                    if matches!(fact, ProviderFact::ReactionCompleted { .. }) {
                        self.terminal_published = true;
                    }
                    return Ok(Some(fact));
                }
                NativeEmission::Public {
                    output_index,
                    canonical_item,
                    wire_item,
                    fact,
                } => {
                    self.frame_state
                        .append_public_output(output_index, &canonical_item, wire_item)
                        .map_err(frame_stream_fault)?;
                    return Ok(Some(fact));
                }
                NativeEmission::Private {
                    output_index,
                    kind,
                    wire_item,
                } => self
                    .frame_state
                    .append_private_output(output_index, kind, wire_item)
                    .map_err(frame_stream_fault)?,
            }
        }
        Ok(None)
    }

    fn process_wire_event(&mut self, frame: OpenAiWireEvent) -> Result<(), ReactionPortFault> {
        self.advance_sequence(&frame.payload)?;
        if self.response_completed {
            return Err(port_fault(
                OpenAiFailureClass::ResponseProtocolViolation,
                "event followed response.completed",
            ));
        }

        match frame.event_type.as_str() {
            "response.created" => self
                .output_ledger
                .record_response_created(&frame.payload)
                .map_err(protocol_fault)?,
            "response.in_progress" => self
                .output_ledger
                .record_response_in_progress(&frame.payload)
                .map_err(protocol_fault)?,
            "response.content_part.added" => {
                if has_unsupported_content_part(&frame.payload) {
                    return Err(upstream_rejected("unsupported content part"));
                }
                self.output_ledger
                    .record_content_part_added(&frame.payload)
                    .map_err(protocol_fault)?;
            }
            "response.content_part.done" => {
                if has_unsupported_content_part(&frame.payload) {
                    return Err(upstream_rejected("unsupported content part"));
                }
                self.output_ledger
                    .record_content_part_done(&frame.payload)
                    .map_err(protocol_fault)?;
            }
            "response.output_text.delta" => self.record_text_delta(&frame.payload)?,
            "response.output_text.annotation.added" => self
                .output_ledger
                .record_text_annotation_added(&frame.payload)
                .map_err(protocol_fault)?,
            "response.output_text.done" => self.record_text_done(&frame.payload)?,
            "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                return Err(upstream_rejected("plaintext reasoning is unsupported"));
            }
            "response.output_item.added" => self.record_output_added(&frame.payload)?,
            "response.function_call_arguments.delta" => self
                .native_tools
                .delta(&frame.payload)
                .map_err(protocol_fault)?,
            "response.function_call_arguments.done" => self
                .native_tools
                .arguments_done(&frame.payload)
                .map_err(protocol_fault)?,
            "response.output_item.done" => self.record_output_done(&frame.payload)?,
            "response.completed" => self.record_completed(&frame.payload)?,
            "response.incomplete" => {
                return Err(port_fault(
                    OpenAiFailureClass::ResponseIncomplete,
                    "upstream response reached an incomplete terminal state",
                ));
            }
            "response.failed" | "error" => {
                return Err(upstream_rejected(
                    "upstream response terminated unsuccessfully",
                ));
            }
            event_type if is_native_tool_event(event_type) => {
                return Err(upstream_rejected("unsupported native tool event"));
            }
            event_type if event_type.starts_with("response.output_") => {
                return Err(upstream_rejected("unsupported output event"));
            }
            _ => {}
        }
        Ok(())
    }

    fn advance_sequence(&mut self, payload: &Map<String, Value>) -> Result<(), ReactionPortFault> {
        let sequence = payload
            .get("sequence_number")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                port_fault(
                    OpenAiFailureClass::ResponseProtocolRetryable,
                    "event is missing sequence_number",
                )
            })?;
        if self
            .last_sequence
            .is_some_and(|previous| sequence <= previous)
        {
            return Err(port_fault(
                OpenAiFailureClass::ResponseProtocolViolation,
                "event sequence did not increase",
            ));
        }
        self.last_sequence = Some(sequence);
        Ok(())
    }

    fn record_text_delta(&mut self, payload: &Map<String, Value>) -> Result<(), ReactionPortFault> {
        let delta = required_native_string(payload, "delta")?;
        if exceeds_limit(
            self.output_text_bytes,
            delta.len(),
            self.max_output_text_bytes,
        ) {
            return Err(port_fault(
                OpenAiFailureClass::OutputLimit,
                "cumulative output text limit exceeded",
            ));
        }
        self.output_ledger
            .record_text_delta(payload, &delta)
            .map_err(protocol_fault)?;
        self.output_text_bytes += delta.len();
        let output_index = output_index(payload)?;
        let next = self
            .next_output_index
            .ok_or_else(|| protocol("output index space exhausted"))?;
        if output_index < next {
            return Err(protocol("text delta followed its sealed output"));
        }
        let fact = ProviderFact::TextDelta {
            output: ProviderOutputKey::new(output_index),
            phase: self.output_ledger.message_phase(output_index),
            delta,
        };
        if output_index == next {
            self.emissions.push_back(NativeEmission::Fact(fact));
        } else {
            self.buffered_deltas
                .entry(output_index)
                .or_default()
                .push_back(fact);
        }
        Ok(())
    }

    fn record_text_done(&mut self, payload: &Map<String, Value>) -> Result<(), ReactionPortFault> {
        let text = required_native_string(payload, "text")?;
        if text.len() > self.max_output_text_bytes {
            return Err(port_fault(
                OpenAiFailureClass::OutputLimit,
                "sealed output text limit exceeded",
            ));
        }
        self.output_ledger
            .record_text_done(payload, &text)
            .map_err(protocol_fault)
    }

    fn record_output_added(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ReactionPortFault> {
        if has_plaintext_reasoning_lifecycle_content(payload) {
            return Err(upstream_rejected("plaintext reasoning is unsupported"));
        }
        if has_unsupported_lifecycle_item(payload) && native_function_call(payload).is_none() {
            return Err(upstream_rejected("unsupported output item"));
        }
        if native_function_call(payload).is_some() {
            self.native_tools.added(payload).map_err(protocol_fault)
        } else {
            self.output_ledger
                .record_added(payload)
                .map_err(|error| protocol_fault(error.into_provider_fault()))
        }
    }

    fn record_output_done(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ReactionPortFault> {
        if has_plaintext_reasoning_lifecycle_content(payload) {
            return Err(upstream_rejected("plaintext reasoning is unsupported"));
        }
        if has_unsupported_lifecycle_item(payload) && native_function_call(payload).is_none() {
            return Err(upstream_rejected("unsupported output item"));
        }
        let output_index = output_index(payload)?;
        let wire_item = payload
            .get("item")
            .cloned()
            .ok_or_else(|| protocol("output_item.done is missing item"))?;
        let ready = if native_function_call(payload).is_some() {
            let call = self
                .native_tools
                .item_done(payload)
                .map_err(protocol_fault)?;
            let call = ProviderToolCall::new(call.call_id(), call.name(), call.raw_arguments())
                .map_err(protocol_fault)?;
            ReadyOutput::Tool { call, wire_item }
        } else {
            let sealed_private = self
                .output_ledger
                .record_done(payload)
                .map_err(protocol_fault)?;
            match sealed_private {
                Some(private) => ready_private(private)?,
                None => {
                    let (text, phase) = self
                        .output_ledger
                        .message_text(output_index)
                        .ok_or_else(|| protocol("sealed message has no completed text"))?;
                    ReadyOutput::Text {
                        phase,
                        text: text.to_owned(),
                        wire_item,
                    }
                }
            }
        };
        if self.ready_outputs.insert(output_index, ready).is_some() {
            return Err(protocol("output index was sealed more than once"));
        }
        self.release_ready_outputs()
    }

    fn release_ready_outputs(&mut self) -> Result<(), ReactionPortFault> {
        loop {
            let next = self
                .next_output_index
                .ok_or_else(|| protocol("output index space exhausted"))?;
            let Some(ready) = self.ready_outputs.remove(&next) else {
                if let Some(deltas) = self.buffered_deltas.remove(&next) {
                    self.emissions
                        .extend(deltas.into_iter().map(NativeEmission::Fact));
                }
                return Ok(());
            };
            if let Some(deltas) = self.buffered_deltas.remove(&next) {
                self.emissions
                    .extend(deltas.into_iter().map(NativeEmission::Fact));
            }
            match ready {
                ReadyOutput::Text {
                    phase,
                    text,
                    wire_item,
                } => {
                    let key = ProviderOutputKey::new(next);
                    if phase != Some(AssistantPhase::Commentary)
                        && self.primary_text.replace(key).is_some()
                    {
                        return Err(protocol("response produced multiple primary text outputs"));
                    }
                    self.emissions.push_back(NativeEmission::Public {
                        output_index: next,
                        canonical_item: CanonicalInputItem::assistant_text(text.clone(), phase),
                        wire_item,
                        fact: ProviderFact::TextSealed {
                            output: key,
                            phase,
                            text,
                        },
                    });
                }
                ReadyOutput::Tool { call, wire_item } => {
                    let canonical_item = CanonicalInputItem::tool_call(
                        call.call_id(),
                        call.name(),
                        call.raw_arguments(),
                    )
                    .map_err(protocol_fault)?;
                    self.emissions.push_back(NativeEmission::Public {
                        output_index: next,
                        canonical_item,
                        wire_item,
                        fact: ProviderFact::ToolCall {
                            output: ProviderOutputKey::new(next),
                            ordinal: next,
                            call,
                        },
                    });
                }
                ReadyOutput::Private { kind, wire_item } => {
                    self.emissions.push_back(NativeEmission::Private {
                        output_index: next,
                        kind,
                        wire_item,
                    });
                }
            }
            self.next_output_index = next.checked_add(1);
        }
    }

    fn record_completed(&mut self, payload: &Map<String, Value>) -> Result<(), ReactionPortFault> {
        validate_completed_envelope(payload)?;
        if has_plaintext_reasoning_completed_content(payload) {
            return Err(upstream_rejected("plaintext reasoning is unsupported"));
        }
        let has_terminal_tool = has_native_function_call_completed(payload);
        if has_unsupported_completed_item(payload) && !has_terminal_tool {
            return Err(upstream_rejected("unsupported completed output item"));
        }
        if has_terminal_tool || !self.native_tools.calls.is_empty() {
            self.native_tools
                .completed(payload)
                .map_err(protocol_fault)?;
        }

        let mut public_payload = payload.clone();
        if let Some(output) = public_payload
            .get_mut("response")
            .and_then(|response| response.get_mut("output"))
            .and_then(Value::as_array_mut)
        {
            output.retain(|item| item.get("type").and_then(Value::as_str) != Some("function_call"));
        }
        self.output_ledger
            .validate_completed_allowing_no_primary(&public_payload)
            .map_err(|error| protocol_fault(error.into_provider_fault()))?;
        if !self.ready_outputs.is_empty() || !self.buffered_deltas.is_empty() {
            return Err(protocol("response.completed preceded lower output release"));
        }
        let usage = validated_response_usage(payload).map_err(protocol_fault)?;
        observe_response_usage(&self.response_usage_observer, usage);
        self.response_completed = true;
        self.emissions
            .push_back(NativeEmission::Fact(ProviderFact::ReactionCompleted {
                primary_text: self.primary_text,
            }));
        Ok(())
    }
}

impl<S> Drop for NativeOpenAiStreamState<'_, S> {
    fn drop(&mut self) {
        if !self.terminal_published && !self.fault_recorded {
            self.target.lose_continuity();
        }
    }
}

fn ready_private(private: SealedOpenAiPrivateOutput) -> Result<ReadyOutput, ReactionPortFault> {
    let kind = match private.wire_item.get("type").and_then(Value::as_str) {
        Some("reasoning") => PrivateOutputKind::Reasoning,
        Some("compaction") => PrivateOutputKind::Compaction,
        _ => return Err(protocol("sealed private output has unsupported type")),
    };
    Ok(ReadyOutput::Private {
        kind,
        wire_item: private.wire_item,
    })
}

fn output_index(payload: &Map<String, Value>) -> Result<u64, ReactionPortFault> {
    payload
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol("output event is missing output_index"))
}

fn required_native_string(
    payload: &Map<String, Value>,
    field: &str,
) -> Result<String, ReactionPortFault> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| protocol(format!("event is missing string field {field}")))
}

fn validate_completed_envelope(payload: &Map<String, Value>) -> Result<(), ReactionPortFault> {
    let response = payload
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(|| protocol("response.completed is missing response"))?;
    if response.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(protocol(
            "response.completed did not carry completed response status",
        ));
    }
    if response
        .get("id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(protocol(
            "response.completed did not carry a response identity",
        ));
    }
    Ok(())
}

fn frame_stream_fault(error: ResponsesFrameRequestFault) -> ReactionPortFault {
    port_fault(error.failure_class(), format!("{error:?}"))
}

fn protocol_fault(error: impl std::fmt::Debug) -> ReactionPortFault {
    port_fault(
        OpenAiFailureClass::ResponseProtocolViolation,
        format!("{error:?}"),
    )
}

fn protocol(diagnostic: impl Into<String>) -> ReactionPortFault {
    port_fault(OpenAiFailureClass::ResponseProtocolViolation, diagnostic)
}

fn upstream_rejected(diagnostic: impl Into<String>) -> ReactionPortFault {
    port_fault(OpenAiFailureClass::UpstreamRejected, diagnostic)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        num::{NonZeroU64, NonZeroU128},
        sync::Arc,
        task::Poll,
    };

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{Response, StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use eventsource_stream::EventStreamError;
    use futures::StreamExt;
    use serde_json::json;
    use tokio::sync::oneshot;

    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "these tests intentionally exercise the retained ProviderPort mode fence"
    )]
    use crate::component::execution::ProviderPort;
    #[cfg(feature = "legacy-provider-port")]
    use crate::component::execution::{RenderedProjection, RenderedProjectionNode};
    use crate::{
        component::execution::{
            ProviderIdentity,
            reaction::{
                Frame, FrameBasis, FrameRevision, FrameSubmission, ProjectionSubmission,
                ProviderFact, ProviderOutputKey, ProviderToolCall, ReactionPort,
                ReactionPortFaultCode, ReactionPortFaultKind, ReactionPortFaultReason,
                ResettableReactionPort, SubmitFault, TargetContinuity, TargetDeclaration,
                TargetEpoch, ToolCatalog,
            },
        },
        provider::{
            async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
            codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options},
        },
    };

    use super::{
        NativeBodyStreamFault, NativeOpenAiStreamState, OpenAiOutputLedger, ReadyOutput,
        ResponsesFrameRequestState,
    };
    use crate::provider::async_openai::{NativeToolLedger, ResponsesReactionTarget};

    fn provider(base: &str, body_limit: Option<usize>) -> AsyncOpenAiResponsesProvider {
        let config = AsyncOpenAiTransportConfig::new(base, "test-token").unwrap();
        let config = match body_limit {
            Some(limit) => config
                .with_responses_serialized_request_body_limit(limit)
                .unwrap(),
            None => config,
        };
        let identity =
            ProviderIdentity::new("openai", "native-responses", 1, "native-test").unwrap();
        let options = CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap();
        AsyncOpenAiResponsesProvider::try_new(config, identity, CodexHttpV1Encoder::new(options))
            .unwrap()
    }

    fn full_frame(declaration: &TargetDeclaration) -> Frame {
        full_frame_with_replay(declaration, 1, Vec::new())
    }

    fn full_frame_with_replay(
        declaration: &TargetDeclaration,
        sequence: u64,
        replay: Vec<crate::transcript::CanonicalInputItem>,
    ) -> Frame {
        let epoch = declaration.continuity().epoch();
        let revision = FrameRevision::new(
            NonZeroU128::new(700).unwrap(),
            declaration.identity(),
            epoch,
            NonZeroU64::new(sequence).unwrap(),
        );
        let submission = FrameSubmission::from_compiled(
            replay,
            Vec::new(),
            ProjectionSubmission::new(Vec::new()),
            ToolCatalog::new(Vec::new()).unwrap(),
            Vec::new(),
        );
        Frame::from_compiled(
            revision,
            declaration.identity(),
            epoch,
            declaration.continuity().clone(),
            declaration.profile().clone(),
            FrameBasis::Full,
            submission,
        )
        .unwrap()
    }

    async fn spawn_sse_server(
        body: String,
    ) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        spawn_response_server(StatusCode::OK, "text/event-stream", body).await
    }

    async fn spawn_response_server(
        status: StatusCode,
        content_type: &'static str,
        body: String,
    ) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        #[derive(Clone)]
        struct Reply {
            status: StatusCode,
            content_type: &'static str,
            body: Arc<String>,
        }

        async fn respond(State(reply): State<Reply>) -> impl IntoResponse {
            Response::builder()
                .status(reply.status)
                .header(header::CONTENT_TYPE, reply.content_type)
                .body(Body::from(reply.body.as_str().to_owned()))
                .unwrap()
        }

        let app = Router::new()
            .route("/responses", post(respond))
            .with_state(Reply {
                status,
                content_type,
                body: Arc::new(body),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://{address}"), shutdown_tx, task)
    }

    fn completed_text_sse() -> String {
        encode_sse([
            json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": {"id":"msg_1","type":"message","status":"in_progress","role":"assistant","content":[]}
            }),
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 2,
                "delta": "hel",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0
            }),
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 3,
                "delta": "lo",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0
            }),
            json!({
                "type": "response.output_text.done",
                "sequence_number": 4,
                "text": "hello",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0
            }),
            json!({
                "type": "response.output_item.done",
                "sequence_number": 5,
                "output_index": 0,
                "item": {"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[]}]}
            }),
            json!({
                "type": "response.completed",
                "sequence_number": 6,
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "output": [{
                        "id": "msg_1",
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "hello",
                            "annotations": []
                        }]
                    }]
                }
            }),
        ])
    }

    fn incomplete_text_sse() -> String {
        encode_sse([
            json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": {"id":"msg_incomplete","type":"message","status":"in_progress","role":"assistant","content":[]}
            }),
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 2,
                "delta": "partial",
                "item_id": "msg_incomplete",
                "output_index": 0,
                "content_index": 0
            }),
            json!({
                "type": "response.incomplete",
                "sequence_number": 3,
                "response": {
                    "id": "resp_incomplete",
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"}
                }
            }),
        ])
    }

    fn ark_compatible_text_sse() -> String {
        let text = "<say>ready</say>";
        let message_id = "msg_ark_1";
        let completed_item = json!({
            "id": message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        });
        encode_sse([
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {"id": "resp_ark_1"}
            }),
            json!({
                "type": "response.in_progress",
                "sequence_number": 1,
                "response": {"id": "resp_ark_1"}
            }),
            json!({
                "type": "response.output_item.added",
                "sequence_number": 2,
                "output_index": 0,
                "item": {
                    "id": message_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant"
                }
            }),
            json!({
                "type": "response.content_part.added",
                "sequence_number": 3,
                "output_index": 0,
                "content_index": 0,
                "item_id": message_id,
                "part": {"type": "output_text", "text": ""}
            }),
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 4,
                "output_index": 0,
                "content_index": 0,
                "item_id": message_id,
                "delta": "<say>"
            }),
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 5,
                "output_index": 0,
                "content_index": 0,
                "item_id": message_id,
                "delta": "ready</say>"
            }),
            json!({
                "type": "response.output_text.done",
                "sequence_number": 6,
                "output_index": 0,
                "content_index": 0,
                "item_id": message_id,
                "text": text
            }),
            json!({
                "type": "response.content_part.done",
                "sequence_number": 7,
                "output_index": 0,
                "content_index": 0,
                "item_id": message_id,
                "part": {"type": "output_text", "text": text}
            }),
            json!({
                "type": "response.output_item.done",
                "sequence_number": 8,
                "output_index": 0,
                "item": completed_item.clone()
            }),
            json!({
                "type": "response.completed",
                "sequence_number": 9,
                "response": {
                    "id": "resp_ark_1",
                    "status": "completed",
                    "output": [completed_item]
                }
            }),
        ])
    }

    fn phased_text_sse() -> String {
        let commentary = "Thinking\ncarefully.";
        let final_text = "done";
        let commentary_added = json!({
            "id": "msg_commentary",
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "phase": "commentary",
            "content": []
        });
        let commentary_done = json!({
            "id": "msg_commentary",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "phase": "commentary",
            "content": [{"type": "output_text", "text": commentary, "annotations": []}]
        });
        let final_added = json!({
            "id": "msg_final",
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "phase": "final_answer",
            "content": []
        });
        let final_done = json!({
            "id": "msg_final",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "phase": "final_answer",
            "content": [{"type": "output_text", "text": final_text, "annotations": []}]
        });
        encode_sse([
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":commentary_added}),
            json!({"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_commentary","output_index":0,"content_index":0,"delta":commentary}),
            json!({"type":"response.output_text.done","sequence_number":3,"item_id":"msg_commentary","output_index":0,"content_index":0,"text":commentary}),
            json!({"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":commentary_done.clone()}),
            json!({"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":final_added}),
            json!({"type":"response.output_text.delta","sequence_number":6,"item_id":"msg_final","output_index":1,"content_index":0,"delta":final_text}),
            json!({"type":"response.output_text.done","sequence_number":7,"item_id":"msg_final","output_index":1,"content_index":0,"text":final_text}),
            json!({"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":final_done.clone()}),
            json!({"type":"response.completed","sequence_number":9,"response":{"id":"resp_phased","status":"completed","output":[commentary_done,final_done]}}),
        ])
    }

    fn tool_only_sse() -> String {
        let added = json!({
            "id": "call_item_1",
            "type": "function_call",
            "status": "in_progress",
            "call_id": "call_1",
            "name": "lookup",
            "arguments": ""
        });
        let done = json!({
            "id": "call_item_1",
            "type": "function_call",
            "status": "completed",
            "call_id": "call_1",
            "name": "lookup",
            "arguments": "{\"key\":1}"
        });
        encode_sse([
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":added}),
            json!({"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"call_item_1","output_index":0,"delta":"{\"key\":"}),
            json!({"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"call_item_1","output_index":0,"delta":"1}"}),
            json!({"type":"response.function_call_arguments.done","sequence_number":4,"item_id":"call_item_1","output_index":0,"arguments":"{\"key\":1}"}),
            json!({"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":done.clone()}),
            json!({"type":"response.completed","sequence_number":6,"response":{"id":"resp_tool","status":"completed","output":[done]}}),
        ])
    }

    fn compacted_text_sse() -> String {
        let compaction = json!({
            "id": "cmp_1",
            "type": "compaction",
            "encrypted_content": "opaque-compaction-secret"
        });
        let message_added = json!({
            "id": "msg_after_compaction",
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": []
        });
        let message_done = json!({
            "id": "msg_after_compaction",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type":"output_text","text":"answer","annotations":[]}]
        });
        encode_sse([
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":compaction.clone()}),
            json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":compaction.clone()}),
            json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":message_added}),
            json!({"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_after_compaction","output_index":1,"content_index":0,"delta":"answer"}),
            json!({"type":"response.output_text.done","sequence_number":5,"item_id":"msg_after_compaction","output_index":1,"content_index":0,"text":"answer"}),
            json!({"type":"response.output_item.done","sequence_number":6,"output_index":1,"item":message_done.clone()}),
            json!({"type":"response.completed","sequence_number":7,"response":{"id":"resp_compacted","status":"completed","output":[compaction,message_done]}}),
        ])
    }

    fn commentary_only_sse() -> String {
        let added = json!({
            "id":"msg_commentary",
            "type":"message",
            "status":"in_progress",
            "role":"assistant",
            "phase":"commentary",
            "content":[]
        });
        let done = json!({
            "id":"msg_commentary",
            "type":"message",
            "status":"completed",
            "role":"assistant",
            "phase":"commentary",
            "content":[{"type":"output_text","text":"working","annotations":[]}]
        });
        encode_sse([
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":added}),
            json!({"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_commentary","output_index":0,"content_index":0,"delta":"working"}),
            json!({"type":"response.output_text.done","sequence_number":3,"item_id":"msg_commentary","output_index":0,"content_index":0,"text":"working"}),
            json!({"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":done.clone()}),
            json!({"type":"response.completed","sequence_number":5,"response":{"id":"resp_commentary","status":"completed","output":[done]}}),
        ])
    }

    fn reasoning_and_tool_sse() -> String {
        let reasoning = json!({
            "id":"rs_1",
            "type":"reasoning",
            "status":"completed",
            "summary":[],
            "encrypted_content":"opaque"
        });
        let tool_added = json!({
            "id":"call_item_1",
            "type":"function_call",
            "status":"in_progress",
            "call_id":"call_1",
            "name":"lookup",
            "arguments":""
        });
        let tool_done = json!({
            "id":"call_item_1",
            "type":"function_call",
            "status":"completed",
            "call_id":"call_1",
            "name":"lookup",
            "arguments":"{}"
        });
        encode_sse([
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"in_progress","summary":[]}}),
            json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":reasoning.clone()}),
            json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":tool_added}),
            json!({"type":"response.function_call_arguments.delta","sequence_number":4,"item_id":"call_item_1","output_index":1,"delta":"{}"}),
            json!({"type":"response.function_call_arguments.done","sequence_number":5,"item_id":"call_item_1","output_index":1,"arguments":"{}"}),
            json!({"type":"response.output_item.done","sequence_number":6,"output_index":1,"item":tool_done.clone()}),
            json!({"type":"response.completed","sequence_number":7,"response":{"id":"resp_reasoning_tool","status":"completed","output":[reasoning,tool_done]}}),
        ])
    }

    #[cfg(feature = "legacy-provider-port")]
    fn empty_projection() -> RenderedProjection {
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", Vec::new())])
            .unwrap()
    }

    fn encode_sse(events: impl IntoIterator<Item = serde_json::Value>) -> String {
        events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[tokio::test]
    async fn submit_crosses_handoff_in_the_first_transport_poll() {
        let mut provider = provider("http://127.0.0.1:1", None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let frame = full_frame(&initial);
        let mut submit = Box::pin(ReactionPort::submit(&mut provider, frame));

        let stream = match futures::poll!(submit.as_mut()) {
            Poll::Ready(Ok(stream)) => stream,
            Poll::Ready(Err(error)) => panic!("submit rejected before handoff: {error:?}"),
            Poll::Pending => panic!("crossing transport poll escaped as Pending"),
        };
        drop(stream);
        drop(submit);

        let after_drop = ReactionPort::declare(&mut provider).unwrap();
        assert!(matches!(
            after_drop.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > initial.continuity().epoch()
        ));
    }

    #[tokio::test]
    async fn request_limit_rejection_is_pre_handoff_and_keeps_declaration() {
        let mut provider = provider("http://127.0.0.1:1", Some(1));
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let error = match ReactionPort::submit(&mut provider, full_frame(&initial)).await {
            Err(error) => error,
            Ok(_) => panic!("request exceeding the configured limit was accepted"),
        };

        let SubmitFault::Rejected(fault) = error else {
            panic!("request limit used the wrong pre-handoff fault: {error:?}");
        };
        assert_eq!(fault.code(), ReactionPortFaultCode::Limit);
        assert_eq!(fault.reason(), ReactionPortFaultReason::RequestPreparation);
        assert_eq!(ReactionPort::declare(&mut provider).unwrap(), initial);
    }

    #[tokio::test]
    async fn stale_continuity_and_profile_are_rejected_before_handoff() {
        let mut provider = provider("http://127.0.0.1:1", None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let stale_continuity = full_frame(&declaration);
        provider.reaction_target.lose_continuity();
        let continuity_error = match ReactionPort::submit(&mut provider, stale_continuity).await {
            Err(error) => error,
            Ok(_) => panic!("stale target continuity was accepted"),
        };
        assert_eq!(continuity_error, SubmitFault::ContinuityChanged);

        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let stale_profile = full_frame(&declaration);
        provider.reaction_target.profile.constraints.max_frame_bytes += 1;
        let profile_error = match ReactionPort::submit(&mut provider, stale_profile).await {
            Err(error) => error,
            Ok(_) => panic!("stale target profile was accepted"),
        };
        assert_eq!(profile_error, SubmitFault::ProfileChanged);
    }

    #[tokio::test]
    async fn post_handoff_http_fault_advances_the_continuity_epoch() {
        let (base, shutdown, server) = spawn_response_server(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            "{}".to_owned(),
        )
        .await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&initial))
            .await
            .unwrap();
        let fault = stream.next().await.unwrap().unwrap_err();
        assert_eq!(
            fault.code(),
            crate::component::execution::reaction::ReactionPortFaultCode::Unavailable
        );
        assert_eq!(fault.reason(), ReactionPortFaultReason::UpstreamRejected);
        drop(stream);

        let after_fault = ReactionPort::declare(&mut provider).unwrap();
        assert!(matches!(
            after_fault.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > initial.continuity().epoch()
        ));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn eof_before_terminal_fact_advances_the_continuity_epoch() {
        let (base, shutdown, server) = spawn_sse_server(String::new()).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&initial))
            .await
            .unwrap();
        let fault = stream.next().await.unwrap().unwrap_err();
        assert_eq!(fault.code(), ReactionPortFaultCode::Protocol);
        assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
        drop(stream);

        let after_fault = ReactionPort::declare(&mut provider).unwrap();
        assert!(matches!(
            after_fault.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > initial.continuity().epoch()
        ));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn terminal_http_fault_is_sticky_across_declare() {
        let (base, shutdown, server) = spawn_response_server(
            StatusCode::UNAUTHORIZED,
            "application/json",
            "{}".to_owned(),
        )
        .await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let fault = stream.next().await.unwrap().unwrap_err();
        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.reason(), ReactionPortFaultReason::Authentication);
        drop(stream);

        assert_eq!(ReactionPort::declare(&mut provider).unwrap_err(), fault);

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn terminal_stream_protocol_fault_is_sticky_across_declare() {
        let malformed = encode_sse([json!({
            "type":"response.completed",
            "sequence_number":1,
            "response":{"id":"resp_bad","status":"failed","output":[]}
        })]);
        let (base, shutdown, server) = spawn_sse_server(malformed).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let fault = stream.next().await.unwrap().unwrap_err();
        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.code(), ReactionPortFaultCode::Protocol);
        drop(stream);

        assert_eq!(ReactionPort::declare(&mut provider).unwrap_err(), fault);

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[test]
    fn accepted_declaration_without_matching_native_state_fails_closed() {
        let mut provider = provider("http://127.0.0.1:1", None);
        let initial = provider.reaction_target.declaration().unwrap();
        let revision = full_frame(&initial).revision();
        provider.reaction_target.accept(revision);

        let fault = ReactionPort::declare(&mut provider).unwrap_err();
        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
        assert_eq!(ReactionPort::declare(&mut provider).unwrap_err(), fault);
    }

    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "this test verifies a legacy setup failure does not claim native mode"
    )]
    #[tokio::test]
    async fn pre_handoff_legacy_failure_does_not_claim_the_provider_mode() {
        let mut provider = provider("http://127.0.0.1:1", Some(1));
        let error = match ProviderPort::execute(&mut provider, empty_projection()).await {
            Err(error) => error,
            Ok(_) => panic!("oversized legacy request was accepted"),
        };
        assert_eq!(
            error.code(),
            crate::component::execution::ProviderFaultCode::RequestPreparation
        );
        assert!(ReactionPort::declare(&mut provider).is_ok());
    }

    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "this test intentionally crosses legacy ProviderPort and native ReactionPort"
    )]
    #[tokio::test]
    async fn legacy_and_frame_native_modes_cannot_be_alternated() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let mut legacy = provider(&base, None);
        let legacy_stream = ProviderPort::execute(&mut legacy, empty_projection())
            .await
            .unwrap();
        drop(legacy_stream);
        let legacy_fault = ReactionPort::declare(&mut legacy).unwrap_err();
        assert_eq!(legacy_fault.kind(), ReactionPortFaultKind::Terminal);

        let mut native = provider("http://127.0.0.1:1", None);
        let declaration = ReactionPort::declare(&mut native).unwrap();
        let native_stream = ReactionPort::submit(&mut native, full_frame(&declaration))
            .await
            .unwrap();
        drop(native_stream);
        let legacy_error = match ProviderPort::execute(&mut native, empty_projection()).await {
            Err(error) => error,
            Ok(_) => panic!("Frame-native provider switched to legacy mode"),
        };
        assert_eq!(
            legacy_error.code(),
            crate::component::execution::ProviderFaultCode::RequestPreparation
        );

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_stream_emits_delta_seal_and_identity_bearing_completion() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let revision = full_frame(&declaration).revision();
        let frame = full_frame(&declaration);
        let mut stream = ReactionPort::submit(&mut provider, frame).await.unwrap();
        let mut facts = Vec::new();
        while let Some(fact) = stream.next().await {
            facts.push(fact.unwrap());
        }
        drop(stream);

        assert_eq!(
            facts,
            vec![
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: "hel".to_owned(),
                },
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: "lo".to_owned(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    text: "hello".to_owned(),
                },
                ProviderFact::ReactionCompleted {
                    primary_text: Some(ProviderOutputKey::new(0)),
                },
            ]
        );
        assert!(matches!(
            ReactionPort::declare(&mut provider).unwrap().continuity(),
            TargetContinuity::Accepted {
                revision: accepted,
                ..
            } if *accepted == revision
        ));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reset_model_context_discards_the_prior_responses_wire_prefix() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let old = crate::transcript::CanonicalInputItem::assistant_text("old-context", None);
        let first = full_frame_with_replay(&initial, 1, vec![old]);
        let mut stream = ReactionPort::submit(&mut provider, first).await.unwrap();
        while let Some(fact) = stream.next().await {
            fact.unwrap();
        }
        drop(stream);

        let accepted = ReactionPort::declare(&mut provider).unwrap();
        let reset = ResettableReactionPort::reset_model_context(&mut provider).unwrap();
        assert_eq!(reset.identity(), accepted.identity());
        assert_eq!(reset.profile(), accepted.profile());
        assert!(matches!(
            reset.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > accepted.continuity().epoch()
        ));
        assert!(provider.reaction_frame.is_none());

        let replacement =
            crate::transcript::CanonicalInputItem::assistant_text("replacement-context", None);
        let next = full_frame_with_replay(&reset, 2, vec![replacement]);
        let prepared = ResponsesFrameRequestState::prepare(
            provider.reaction_frame.as_ref(),
            &next,
            &provider.encoder,
            provider.max_responses_serialized_request_body_bytes,
        )
        .unwrap();
        let request = String::from_utf8(prepared.request_body).unwrap();
        assert!(request.contains("replacement-context"));
        assert!(!request.contains("old-context"));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reset_model_context_after_interrupted_stream_discards_the_prior_wire_prefix() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let old = crate::transcript::CanonicalInputItem::assistant_text("old-context", None);
        let first = full_frame_with_replay(&initial, 1, vec![old]);
        let mut stream = ReactionPort::submit(&mut provider, first).await.unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            ProviderFact::TextDelta { delta, .. } if delta == "hel"
        ));
        drop(stream);

        let after_drop = ReactionPort::declare(&mut provider).unwrap();
        assert!(matches!(
            after_drop.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > initial.continuity().epoch()
        ));
        assert!(provider.reaction_frame.is_some());

        let reset = ResettableReactionPort::reset_model_context(&mut provider).unwrap();
        assert!(matches!(
            reset.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > after_drop.continuity().epoch()
        ));
        assert!(provider.reaction_frame.is_none());

        let replacement =
            crate::transcript::CanonicalInputItem::assistant_text("replacement-context", None);
        let next = full_frame_with_replay(&reset, 2, vec![replacement]);
        let prepared = ResponsesFrameRequestState::prepare(
            provider.reaction_frame.as_ref(),
            &next,
            &provider.encoder,
            provider.max_responses_serialized_request_body_bytes,
        )
        .unwrap();
        let request = String::from_utf8(prepared.request_body).unwrap();
        assert!(request.contains("replacement-context"));
        assert!(!request.contains("old-context"));
        assert!(!request.contains("hel"));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn incomplete_response_after_partial_text_is_retryable_and_requires_a_fresh_full() {
        let (base, shutdown, server) = spawn_sse_server(incomplete_text_sse()).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let old = crate::transcript::CanonicalInputItem::assistant_text("old-context", None);
        let first = full_frame_with_replay(&initial, 1, vec![old]);
        let mut stream = ReactionPort::submit(&mut provider, first).await.unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            ProviderFact::TextDelta { delta, .. } if delta == "partial"
        ));
        let fault = stream.next().await.unwrap().unwrap_err();
        assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
        assert_eq!(fault.code(), ReactionPortFaultCode::Rejected);
        assert_eq!(fault.reason(), ReactionPortFaultReason::UpstreamRejected);
        assert!(stream.next().await.is_none());
        drop(stream);

        let after_incomplete = ReactionPort::declare(&mut provider).unwrap();
        assert!(matches!(
            after_incomplete.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > initial.continuity().epoch()
        ));
        assert!(provider.reaction_frame.is_some());

        let reset = ResettableReactionPort::reset_model_context(&mut provider).unwrap();
        assert!(matches!(
            reset.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > after_incomplete.continuity().epoch()
        ));
        assert!(provider.reaction_frame.is_none());

        let replacement =
            crate::transcript::CanonicalInputItem::assistant_text("replacement-context", None);
        let next = full_frame_with_replay(&reset, 2, vec![replacement]);
        let prepared = ResponsesFrameRequestState::prepare(
            provider.reaction_frame.as_ref(),
            &next,
            &provider.encoder,
            provider.max_responses_serialized_request_body_bytes,
        )
        .unwrap();
        let request = String::from_utf8(prepared.request_body).unwrap();
        assert!(request.contains("replacement-context"));
        assert!(!request.contains("old-context"));
        assert!(!request.contains("partial"));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[test]
    fn reset_model_context_epoch_exhaustion_retires_the_provider() {
        let mut provider = provider("http://127.0.0.1:1", None);
        provider.reaction_target.epoch = TargetEpoch::new(NonZeroU64::new(u64::MAX).unwrap());

        let fault = ResettableReactionPort::reset_model_context(&mut provider).unwrap_err();
        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.code(), ReactionPortFaultCode::Internal);
        assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
        assert_eq!(ReactionPort::declare(&mut provider).unwrap_err(), fault);
        assert!(provider.reaction_frame.is_none());
    }

    #[tokio::test]
    async fn native_stream_accepts_ark_compatible_optional_fields() {
        let (base, shutdown, server) = spawn_sse_server(ark_compatible_text_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let mut facts = Vec::new();
        while let Some(fact) = stream.next().await {
            facts.push(fact.unwrap());
        }
        drop(stream);

        assert_eq!(
            facts,
            vec![
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: "<say>".to_owned(),
                },
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: "ready</say>".to_owned(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    text: "<say>ready</say>".to_owned(),
                },
                ProviderFact::ReactionCompleted {
                    primary_text: Some(ProviderOutputKey::new(0)),
                },
            ]
        );

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn commentary_delta_is_public_but_does_not_become_primary_text() {
        let (base, shutdown, server) = spawn_sse_server(phased_text_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let mut facts = Vec::new();
        while let Some(fact) = stream.next().await {
            facts.push(fact.unwrap());
        }
        drop(stream);

        assert_eq!(
            facts,
            vec![
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: Some(crate::transcript::AssistantPhase::Commentary),
                    delta: "Thinking\ncarefully.".to_owned(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: Some(crate::transcript::AssistantPhase::Commentary),
                    text: "Thinking\ncarefully.".to_owned(),
                },
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(1),
                    phase: Some(crate::transcript::AssistantPhase::FinalAnswer),
                    delta: "done".to_owned(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(1),
                    phase: Some(crate::transcript::AssistantPhase::FinalAnswer),
                    text: "done".to_owned(),
                },
                ProviderFact::ReactionCompleted {
                    primary_text: Some(ProviderOutputKey::new(1)),
                },
            ]
        );

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn commentary_only_completion_is_valid_without_primary_text() {
        let (base, shutdown, server) = spawn_sse_server(commentary_only_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let mut facts = Vec::new();
        while let Some(fact) = stream.next().await {
            facts.push(fact.unwrap());
        }
        drop(stream);

        assert!(matches!(
            facts.last(),
            Some(ProviderFact::ReactionCompleted { primary_text: None })
        ));
        assert!(facts.iter().any(|fact| matches!(
            fact,
            ProviderFact::TextDelta {
                phase: Some(crate::transcript::AssistantPhase::Commentary),
                delta,
                ..
            } if delta == "working"
        )));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tool_only_completion_has_no_primary_text() {
        let (base, shutdown, server) = spawn_sse_server(tool_only_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let mut facts = Vec::new();
        while let Some(fact) = stream.next().await {
            facts.push(fact.unwrap());
        }
        drop(stream);

        assert_eq!(
            facts,
            vec![
                ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(0),
                    ordinal: 0,
                    call: ProviderToolCall::new("call_1", "lookup", "{\"key\":1}").unwrap(),
                },
                ProviderFact::ReactionCompleted { primary_text: None },
            ]
        );

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reasoning_and_tool_completion_is_valid_without_primary_text() {
        let (base, shutdown, server) = spawn_sse_server(reasoning_and_tool_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        let mut facts = Vec::new();
        while let Some(fact) = stream.next().await {
            facts.push(fact.unwrap());
        }
        drop(stream);

        assert_eq!(
            facts,
            vec![
                ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(1),
                    ordinal: 1,
                    call: ProviderToolCall::new("call_1", "lookup", "{}").unwrap(),
                },
                ProviderFact::ReactionCompleted { primary_text: None },
            ]
        );

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn private_compaction_recovers_only_with_matching_canonical_coverage() {
        let (base, shutdown, server) = spawn_sse_server(compacted_text_sse()).await;
        let mut provider = provider(&base, None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
            .await
            .unwrap();
        while let Some(fact) = stream.next().await {
            fact.unwrap();
        }
        drop(stream);

        let accepted = ReactionPort::declare(&mut provider).unwrap();
        let replay = vec![crate::transcript::CanonicalInputItem::assistant_text(
            "answer", None,
        )];
        let recovery = full_frame_with_replay(&accepted, 2, replay);
        let prepared = ResponsesFrameRequestState::prepare(
            provider.reaction_frame.as_ref(),
            &recovery,
            &provider.encoder,
            provider.max_responses_serialized_request_body_bytes,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&prepared.request_body).unwrap();

        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["input"][0]["type"], "compaction");
        assert_eq!(
            body["input"][0]["encrypted_content"],
            "opaque-compaction-secret"
        );
        assert_eq!(body["input"][1]["content"][0]["text"], "answer");

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[test]
    fn later_text_waits_for_earlier_private_output_seal() {
        let provider = provider("http://127.0.0.1:1", None);
        let declaration = provider.reaction_target.declaration().unwrap();
        let frame = full_frame(&declaration);
        let mut frame_state = ResponsesFrameRequestState::prepare(
            None,
            &frame,
            &provider.encoder,
            provider.max_responses_serialized_request_body_bytes,
        )
        .unwrap()
        .state;
        let mut target = ResponsesReactionTarget::new(declaration.profile().clone()).unwrap();
        let stream = futures::stream::empty::<
            Result<eventsource_stream::Event, EventStreamError<NativeBodyStreamFault>>,
        >();
        let mut state = NativeOpenAiStreamState {
            stream,
            target: &mut target,
            frame_state: &mut frame_state,
            response_completed: false,
            terminal_published: false,
            fault_recorded: false,
            finished: false,
            last_sequence: None,
            next_output_index: Some(0),
            output_ledger: OpenAiOutputLedger::default(),
            native_tools: NativeToolLedger::default(),
            ready_outputs: BTreeMap::new(),
            buffered_deltas: BTreeMap::new(),
            emissions: VecDeque::new(),
            primary_text: None,
            output_text_bytes: 0,
            read_timeout: std::time::Duration::from_secs(1),
            max_sse_event_bytes: 1024 * 1024,
            max_output_text_bytes: 1024 * 1024,
            response_usage_observer: None,
        };

        for value in [
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"in_progress","summary":[]}}),
            json!({"type":"response.output_item.added","sequence_number":2,"output_index":1,"item":{"id":"msg_1","type":"message","status":"in_progress","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","sequence_number":3,"delta":"later","item_id":"msg_1","output_index":1,"content_index":0}),
            json!({"type":"response.output_text.done","sequence_number":4,"text":"later","item_id":"msg_1","output_index":1,"content_index":0}),
            json!({"type":"response.output_item.done","sequence_number":5,"output_index":1,"item":{"id":"msg_1","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"later","annotations":[]}]}}),
        ] {
            state
                .process_wire_event(serde_json::from_value(value).unwrap())
                .unwrap();
        }
        assert!(state.next_emission().unwrap().is_none());

        let reasoning_done = json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"private"}],"encrypted_content":"opaque"}});
        state
            .process_wire_event(serde_json::from_value(reasoning_done).unwrap())
            .unwrap();

        assert!(matches!(
            state.next_emission().unwrap(),
            Some(ProviderFact::TextDelta { output, delta, .. })
                if output == ProviderOutputKey::new(1) && delta == "later"
        ));
        assert!(matches!(
            state.next_emission().unwrap(),
            Some(ProviderFact::TextSealed { output, text, .. })
                if output == ProviderOutputKey::new(1) && text == "later"
        ));
        assert_eq!(state.frame_state.wire_coverage().item_count(), 1);
        assert!(state.ready_outputs.is_empty());
        assert!(!matches!(
            state.ready_outputs.get(&0),
            Some(ReadyOutput::Private { .. })
        ));
    }
}
