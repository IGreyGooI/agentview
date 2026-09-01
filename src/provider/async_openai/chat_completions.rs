#[cfg(feature = "legacy-provider-port")]
use std::{future::Future, task::Poll};
use std::{
    num::{NonZeroU128, NonZeroU64},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[cfg(feature = "legacy-provider-port")]
use ::async_openai::config::Config;
use ::async_openai::config::OpenAIConfig;
#[cfg(feature = "legacy-provider-port")]
use async_trait::async_trait;
use eventsource_stream::Event;
#[cfg(feature = "legacy-provider-port")]
use eventsource_stream::{EventStreamError, Eventsource};
#[cfg(feature = "legacy-provider-port")]
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "the Chat adapter retains a feature-gated ProviderPort compatibility path"
)]
use crate::component::execution::{ProviderPort, RenderedProjection};
use crate::{
    component::execution::{
        reaction::{
            FrameProfile, FrameRevision, ReactionPortFault, ReactionPortFaultKind,
            TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        ProviderIdentity,
    },
    pom_renderer::PomRenderError,
    transcript::CanonicalTranscriptError,
};
#[cfg(feature = "legacy-provider-port")]
use crate::{
    component::execution::{ProviderEvent, ProviderEventStream, ProviderFault, ProviderFaultCode},
    llm_call::TextTurnEvent,
};

#[cfg(feature = "legacy-provider-port")]
use super::{
    faults::{
        output_limit_fault, redacted_status_fault, request_transport_fault,
        response_body_limit_fault, stream_error_code, stream_event_limit_fault,
        stream_transport_fault, OpenAiApi, OpenAiBodyStreamFault,
    },
    SseWireLimiter,
};
use super::{
    reaction_fault::{map_openai_fault, OpenAiFailureClass, OpenAiReactionFailure},
    AsyncOpenAiTransportConfig,
};

mod frame_request;
mod history;
mod native_reaction;

#[cfg(feature = "legacy-provider-port")]
use history::{ChatDiffMemo, ChatHistory, PreparedChatHistory};

pub const OPENAI_CHAT_COMPLETIONS_PROFILE: &str = "openai-chat-completions-v1";
const CHAT_TARGET_ID_DOMAIN: u128 = 1_u128 << 64;
static NEXT_CHAT_TARGET_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiChatCompletionsOptions {
    model: String,
}

impl OpenAiChatCompletionsOptions {
    pub fn new(model: impl Into<String>) -> Result<Self, OpenAiChatCompletionsError> {
        let model = model.into();
        if model.is_empty() {
            return Err(OpenAiChatCompletionsError::EmptyModel);
        }
        Ok(Self { model })
    }

    fn model(&self) -> &str {
        &self.model
    }
}

pub struct AsyncOpenAiChatCompletionsProvider {
    client: reqwest::Client,
    config: OpenAIConfig,
    identity: ProviderIdentity,
    options: OpenAiChatCompletionsOptions,
    read_timeout: Duration,
    max_response_body_bytes: usize,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    max_serialized_request_body_bytes: usize,
    #[cfg(feature = "legacy-provider-port")]
    history: Option<ChatHistory>,
    #[cfg(feature = "legacy-provider-port")]
    diff_memo: Option<ChatDiffMemo>,
    execution_mode: ChatExecutionMode,
    reaction_target: ChatReactionTarget,
    reaction_frame: Option<frame_request::ChatFrameRequestState>,
}

impl AsyncOpenAiChatCompletionsProvider {
    pub fn new(
        config: AsyncOpenAiTransportConfig,
        identity: ProviderIdentity,
        options: OpenAiChatCompletionsOptions,
    ) -> Self {
        Self::try_new(config, identity, options)
            .unwrap_or_else(|_| panic!("OpenAI transport initialization failed"))
    }

    /// Builds a Chat Completions provider without exposing HTTP-client source errors.
    pub fn try_new(
        config: AsyncOpenAiTransportConfig,
        identity: ProviderIdentity,
        options: OpenAiChatCompletionsOptions,
    ) -> Result<Self, super::AsyncOpenAiConfigError> {
        let initialized = super::transport::initialize(config)?;
        let reaction_target =
            ChatReactionTarget::new(initialized.chat_completions_frame_profile.clone())?;
        Ok(Self {
            client: initialized.client,
            config: initialized.config,
            identity,
            options,
            read_timeout: initialized.read_timeout,
            max_response_body_bytes: initialized.max_response_body_bytes,
            max_sse_event_bytes: initialized.max_sse_event_bytes,
            max_output_text_bytes: initialized.max_output_text_bytes,
            max_serialized_request_body_bytes: initialized
                .max_chat_completions_serialized_request_body_bytes,
            #[cfg(feature = "legacy-provider-port")]
            history: None,
            #[cfg(feature = "legacy-provider-port")]
            diff_memo: None,
            execution_mode: ChatExecutionMode::Unclaimed,
            reaction_target,
            reaction_frame: None,
        })
    }

    pub fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    fn ensure_frame_native_mode(&self) -> Result<(), ReactionPortFault> {
        match self.execution_mode {
            ChatExecutionMode::Unclaimed | ChatExecutionMode::FrameNative => Ok(()),
            #[cfg(feature = "legacy-provider-port")]
            ChatExecutionMode::Legacy => Err(chat_declaration_state_lost_fault()),
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    fn ensure_legacy_mode(&self) -> Result<(), ProviderFault> {
        match self.execution_mode {
            ChatExecutionMode::Unclaimed | ChatExecutionMode::Legacy => Ok(()),
            ChatExecutionMode::FrameNative => Err(ProviderFault::model_rejected(
                "OpenAI Chat Completions provider is already using the Frame-native protocol",
            )
            .with_code(ProviderFaultCode::RequestPreparation)),
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    async fn start_stream<'a>(
        &'a mut self,
        prepared: PreparedChatHistory,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        let PreparedChatHistory {
            request_body,
            candidate,
            memo,
        } = prepared;
        serde_json::from_slice::<Value>(&request_body).map_err(|_| {
            ProviderFault::model_rejected("Chat Completions request body is not one JSON value")
                .with_code(ProviderFaultCode::RequestPreparation)
        })?;
        let request = self
            .client
            .post(self.config.url("/chat/completions"))
            .headers(self.config.headers())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request_body)
            .build()
            .map_err(|_| {
                ProviderFault::retryable_transport("Chat Completions API transport failed")
                    .with_code(ProviderFaultCode::RequestPreparation)
            })?;
        let client = self.client.clone();
        let history_slot = &mut self.history;
        let diff_memo_slot = &mut self.diff_memo;
        let execution_mode = &mut self.execution_mode;
        let read_timeout = self.read_timeout;
        let max_response_body_bytes = self.max_response_body_bytes;
        let max_sse_event_bytes = self.max_sse_event_bytes;
        let max_output_text_bytes = self.max_output_text_bytes;
        let mut transport = Box::pin(client.execute(request));
        let mut handoff = Some((candidate, memo));
        let first_response = futures::future::poll_fn(|cx| {
            match execution_mode {
                ChatExecutionMode::Unclaimed => {
                    *execution_mode = ChatExecutionMode::Legacy;
                }
                ChatExecutionMode::Legacy => {}
                ChatExecutionMode::FrameNative => {
                    return Poll::Ready(Err(ProviderFault::model_rejected(
                        "OpenAI Chat Completions provider is already using the Frame-native protocol",
                    )
                    .with_code(ProviderFaultCode::RequestPreparation)));
                }
            }
            let response = transport
                .as_mut()
                .poll(cx)
                .map_err(|error| request_transport_fault(OpenAiApi::ChatCompletions, &error));
            let (candidate, memo) = handoff.take().expect("chat input gate handoff occurs once");
            // reqwest has been polled before history/memo advance, so
            // ApplicationHost observations after execute are causal.
            *history_slot = Some(candidate);
            *diff_memo_slot = Some(memo);
            Poll::Ready(Ok(response))
        })
        .await?;
        let pending = async move {
            let response = match first_response {
                Poll::Ready(Ok(response)) => response,
                Poll::Ready(Err(error)) => return super::post_handoff_fault(error),
                Poll::Pending => match transport.await {
                    Ok(response) => response,
                    Err(error) => {
                        return super::post_handoff_fault(request_transport_fault(
                            OpenAiApi::ChatCompletions,
                            &error,
                        ))
                    }
                },
            };
            let status = response.status();
            if !status.is_success() {
                return super::post_handoff_fault(redacted_status_fault(
                    OpenAiApi::ChatCompletions,
                    status,
                ));
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
                return super::post_handoff_fault(
                    ProviderFault::retryable_transport(
                        "Chat Completions API returned a successful non-SSE response",
                    )
                    .with_code(ProviderFaultCode::ResponseProtocol),
                );
            }
            if response.content_length().is_some_and(|content_length| {
                content_length > u64::try_from(max_response_body_bytes).unwrap_or(u64::MAX)
            }) {
                return super::post_handoff_fault(response_body_limit_fault(
                    OpenAiApi::ChatCompletions,
                ));
            }

            let mut response_body_bytes = 0_usize;
            let mut sse_wire_limiter = SseWireLimiter::new(max_sse_event_bytes);
            let response_stream = response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|error| {
                    OpenAiBodyStreamFault::ClassifiedTransport(stream_error_code(&error))
                })?;
                let next_size = response_body_bytes
                    .checked_add(chunk.len())
                    .ok_or(OpenAiBodyStreamFault::BodyLimit)?;
                if next_size > max_response_body_bytes {
                    return Err(OpenAiBodyStreamFault::BodyLimit);
                }
                sse_wire_limiter.observe(&chunk)?;
                response_body_bytes = next_size;
                Ok(chunk)
            });
            let state = ChatStreamState {
                stream: response_stream.eventsource(),
                history_slot,
                accumulated_text: String::new(),
                output_text_bytes: 0,
                response_id: None,
                response_model: None,
                finish_seen: false,
                finished: false,
                read_timeout,
                max_sse_event_bytes,
                max_output_text_bytes,
            };
            let mapped = futures::stream::unfold(state, |mut state| async move {
                if state.finished {
                    return None;
                }

                loop {
                    let event =
                        match tokio::time::timeout(state.read_timeout, state.stream.next()).await {
                            Err(_) => {
                                state.finished = true;
                                return Some((
                                    Err(ProviderFault::retryable_transport(
                                        "Chat Completions streaming response read timed out",
                                    )
                                    .with_code(ProviderFaultCode::StreamTimeout)),
                                    state,
                                ));
                            }
                            Ok(Some(Err(EventStreamError::Transport(
                                OpenAiBodyStreamFault::ClassifiedTransport(code),
                            )))) => {
                                state.finished = true;
                                return Some((
                                    Err(stream_transport_fault(OpenAiApi::ChatCompletions, code)),
                                    state,
                                ));
                            }
                            Ok(Some(Err(EventStreamError::Transport(
                                OpenAiBodyStreamFault::BodyLimit,
                            )))) => {
                                state.finished = true;
                                return Some((
                                    Err(response_body_limit_fault(OpenAiApi::ChatCompletions)),
                                    state,
                                ));
                            }
                            Ok(Some(Err(EventStreamError::Transport(
                                OpenAiBodyStreamFault::EventLimit,
                            )))) => {
                                state.finished = true;
                                return Some((
                                    Err(stream_event_limit_fault(OpenAiApi::ChatCompletions)),
                                    state,
                                ));
                            }
                            Ok(Some(Ok(event))) => event,
                            Ok(None) => {
                                state.finished = true;
                                return Some((
                                    Err(ProviderFault::retryable_transport(
                                        "Chat Completions stream ended before [DONE]",
                                    )
                                    .with_code(ProviderFaultCode::ResponseProtocol)),
                                    state,
                                ));
                            }
                            Ok(Some(Err(_))) => {
                                state.finished = true;
                                return Some((
                                Err(ProviderFault::retryable_transport(
                                    "Chat Completions API returned an invalid streaming response",
                                )
                                .with_code(ProviderFaultCode::ResponseProtocol)),
                                state,
                            ));
                            }
                        };
                    if sse_event_size(&event)
                        .is_none_or(|event_size| event_size > state.max_sse_event_bytes)
                    {
                        state.finished = true;
                        return Some((
                            Err(stream_event_limit_fault(OpenAiApi::ChatCompletions)),
                            state,
                        ));
                    }
                    if event.data == "[DONE]" {
                        if !state.finish_seen {
                            state.finished = true;
                            return Some((
                                Err(ProviderFault::retryable_transport(
                                    "Chat Completions stream ended before a stop finish reason",
                                )
                                .with_code(ProviderFaultCode::ResponseProtocol)),
                                state,
                            ));
                        }
                        let text = std::mem::take(&mut state.accumulated_text);
                        let candidate =
                            match state.history_slot.as_ref().cloned().ok_or_else(|| {
                                ProviderFault::model_rejected(
                                    "Chat Completions history candidate is unavailable",
                                )
                            }) {
                                Ok(candidate) => candidate,
                                Err(fault) => {
                                    state.finished = true;
                                    return Some((Err(fault), state));
                                }
                            };
                        let candidate = match candidate.with_output(text.clone()) {
                            Ok(candidate) => candidate,
                            Err(_) => {
                                state.finished = true;
                                return Some((
                                    Err(ProviderFault::model_rejected(
                                        "Chat Completions history update failed",
                                    )),
                                    state,
                                ));
                            }
                        };
                        *state.history_slot = Some(candidate);
                        state.finished = true;
                        return Some((
                            Ok(ProviderEvent::Text(TextTurnEvent::TextComplete(text))),
                            state,
                        ));
                    }
                    if state.finish_seen {
                        state.finished = true;
                        return Some((
                            Err(ProviderFault::model_rejected(
                                "Chat Completions emitted a frame after the terminal choice",
                            )
                            .with_code(ProviderFaultCode::ResponseProtocol)),
                            state,
                        ));
                    }
                    let frame = match serde_json::from_str::<ChatWireChunk>(&event.data) {
                        Ok(frame) => frame,
                        Err(_) => {
                            state.finished = true;
                            return Some((
                                Err(ProviderFault::retryable_transport(
                                    "Chat Completions API returned an invalid streaming response",
                                )
                                .with_code(ProviderFaultCode::ResponseProtocol)),
                                state,
                            ));
                        }
                    };
                    let delta = match validate_frame(&mut state, frame) {
                        Ok(delta) => delta,
                        Err(fault) => {
                            state.finished = true;
                            return Some((Err(fault), state));
                        }
                    };
                    let Some(delta) = delta else {
                        continue;
                    };
                    if exceeds_limit(
                        state.output_text_bytes,
                        delta.len(),
                        state.max_output_text_bytes,
                    ) {
                        state.finished = true;
                        return Some((Err(output_limit_fault(OpenAiApi::ChatCompletions)), state));
                    }
                    state.output_text_bytes += delta.len();
                    state.accumulated_text.push_str(&delta);
                    let Some(history) = state.history_slot.as_mut() else {
                        state.finished = true;
                        return Some((
                            Err(ProviderFault::model_rejected(
                                "Chat Completions local history is unavailable",
                            )),
                            state,
                        ));
                    };
                    if history.record_text_partial(delta.clone()).is_err() {
                        state.finished = true;
                        return Some((
                            Err(ProviderFault::model_rejected(
                                "Chat Completions partial output could not be retained",
                            )),
                            state,
                        ));
                    }
                    return Some((
                        Ok(ProviderEvent::Text(TextTurnEvent::TextDelta(delta))),
                        state,
                    ));
                }
            });
            Box::pin(mapped) as ProviderEventStream
        };
        Ok(Box::pin(futures::stream::once(pending).flatten()))
    }
}

#[async_trait]
#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "this impl preserves the legacy Chat Completions ProviderPort contract"
)]
impl ProviderPort for AsyncOpenAiChatCompletionsProvider {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.ensure_legacy_mode()?;
        let prepared = ChatHistory::prepare(
            self.history.as_ref(),
            self.diff_memo.as_ref(),
            projection,
            &self.options,
            self.max_serialized_request_body_bytes,
        )
        .map_err(|error| {
            let message = match error {
                OpenAiChatCompletionsError::SystemInstructionChanged => {
                    "Chat Completions System instruction changed after the first submission"
                }
                OpenAiChatCompletionsError::SerializedRequestBodyLimit => {
                    "OpenAI Chat Completions serialized outbound request body exceeded configured limit"
                }
                OpenAiChatCompletionsError::UnsupportedNativeToolDeclarations => {
                    "OpenAI Chat Completions does not support native tool declarations"
                }
                _ => "Chat Completions request preparation failed",
            };
            ProviderFault::model_rejected(message).with_code(ProviderFaultCode::RequestPreparation)
        })?;
        self.start_stream(prepared).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatExecutionMode {
    Unclaimed,
    #[cfg(feature = "legacy-provider-port")]
    Legacy,
    FrameNative,
}

#[derive(Debug)]
struct ChatReactionTarget {
    identity: TargetIdentity,
    epoch: TargetEpoch,
    accepted_revision: Option<FrameRevision>,
    profile: FrameProfile,
    terminal_fault: Option<ReactionPortFault>,
}

impl ChatReactionTarget {
    fn new(profile: FrameProfile) -> Result<Self, super::AsyncOpenAiConfigError> {
        Ok(Self {
            identity: next_chat_target_identity(&NEXT_CHAT_TARGET_ID)?,
            epoch: TargetEpoch::new(NonZeroU64::MIN),
            accepted_revision: None,
            profile,
            terminal_fault: None,
        })
    }

    fn declaration(&self) -> Result<TargetDeclaration, ReactionPortFault> {
        if let Some(fault) = self.terminal_fault {
            return Err(fault);
        }
        Ok(match self.accepted_revision {
            Some(revision) => TargetDeclaration::resume(revision, self.profile.clone()),
            None => TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
        })
    }

    fn accept(&mut self, revision: FrameRevision) {
        let accepted = TargetDeclaration::resume(revision, self.profile.clone());
        debug_assert_eq!(accepted.identity(), self.identity);
        debug_assert_eq!(accepted.continuity().epoch(), self.epoch);
        self.accepted_revision = Some(revision);
    }

    fn lose_continuity(&mut self) {
        if self.terminal_fault.is_some() {
            return;
        }
        self.accepted_revision = None;
        let next_epoch = self
            .epoch
            .get()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(TargetEpoch::new);
        match next_epoch {
            Some(epoch) => self.epoch = epoch,
            None => self.terminal_fault = Some(chat_declaration_state_lost_fault()),
        }
    }

    fn record_fault(&mut self, fault: ReactionPortFault) {
        self.accepted_revision = None;
        match fault.kind() {
            ReactionPortFaultKind::Retryable => self.lose_continuity(),
            ReactionPortFaultKind::Terminal => {
                if self.terminal_fault.is_none() {
                    self.terminal_fault = Some(fault);
                }
            }
        }
    }
}

fn next_chat_target_identity(
    counter: &AtomicU64,
) -> Result<TargetIdentity, super::AsyncOpenAiConfigError> {
    let value = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| super::AsyncOpenAiConfigError::ChatCompletionsTargetIdentityExhausted)?;
    Ok(TargetIdentity::new(
        NonZeroU128::new(CHAT_TARGET_ID_DOMAIN | u128::from(value))
            .expect("Chat target domain is non-zero"),
    ))
}

fn chat_declaration_state_lost_fault() -> ReactionPortFault {
    map_openai_fault(&OpenAiReactionFailure::static_diagnostic(
        OpenAiFailureClass::DeclarationStateLost,
        "Chat Completions target declaration state is unavailable",
    ))
}

#[cfg(feature = "legacy-provider-port")]
struct ChatStreamState<'a, S> {
    stream: S,
    history_slot: &'a mut Option<ChatHistory>,
    accumulated_text: String,
    output_text_bytes: usize,
    response_id: Option<String>,
    response_model: Option<String>,
    finish_seen: bool,
    finished: bool,
    read_timeout: Duration,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
}

#[cfg(feature = "legacy-provider-port")]
impl<S> Drop for ChatStreamState<'_, S> {
    fn drop(&mut self) {
        if let Some(history) = self.history_slot.as_mut() {
            history.abort_open_output();
        }
    }
}

#[cfg(feature = "legacy-provider-port")]
fn validate_frame<S>(
    state: &mut ChatStreamState<'_, S>,
    frame: ChatWireChunk,
) -> Result<Option<String>, ProviderFault> {
    if frame.id.is_empty()
        || frame.model.is_empty()
        || frame.object != "chat.completion.chunk"
        || frame.choices.len() != 1
    {
        return Err(invalid_lifecycle());
    }
    match &state.response_id {
        Some(id) if id != &frame.id => return Err(invalid_lifecycle()),
        None => state.response_id = Some(frame.id),
        _ => {}
    }
    match &state.response_model {
        Some(model) if model != &frame.model => return Err(invalid_lifecycle()),
        None => state.response_model = Some(frame.model),
        _ => {}
    }

    let choice = frame
        .choices
        .into_iter()
        .next()
        .ok_or_else(invalid_lifecycle)?;
    if choice.delta.tool_calls.is_some()
        || choice.delta.function_call.is_some()
        || choice.delta.refusal.is_some()
    {
        return Err(ProviderFault::model_rejected(
            "Chat Completions returned unsupported non-text output",
        ));
    }
    if choice.index != 0
        || choice
            .delta
            .role
            .as_deref()
            .is_some_and(|role| role != "assistant")
    {
        return Err(invalid_lifecycle());
    }
    match choice.finish_reason.as_deref() {
        None => Ok(choice.delta.content.filter(|content| !content.is_empty())),
        Some("stop") if choice.delta.content.as_deref().is_none_or(str::is_empty) => {
            state.finish_seen = true;
            Ok(None)
        }
        Some(_) => Err(ProviderFault::model_rejected(
            "Chat Completions returned an unsupported finish reason",
        )),
    }
}

#[cfg(feature = "legacy-provider-port")]
fn invalid_lifecycle() -> ProviderFault {
    ProviderFault::model_rejected("Chat Completions returned an invalid text lifecycle")
        .with_code(ProviderFaultCode::ResponseProtocol)
}

fn sse_event_size(event: &Event) -> Option<usize> {
    event
        .data
        .len()
        .checked_add(event.event.len())?
        .checked_add(event.id.len())
}

fn exceeds_limit(current: usize, additional: usize, limit: usize) -> bool {
    current
        .checked_add(additional)
        .is_none_or(|total| total > limit)
}

#[derive(Deserialize)]
struct ChatWireChunk {
    id: String,
    object: String,
    model: String,
    choices: Vec<ChatWireChoice>,
}

#[derive(Deserialize)]
struct ChatWireChoice {
    index: u32,
    delta: ChatWireDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatWireDelta {
    content: Option<String>,
    role: Option<String>,
    tool_calls: Option<Value>,
    function_call: Option<Value>,
    refusal: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenAiChatCompletionsError {
    #[error("Chat Completions model must be non-empty")]
    EmptyModel,
    #[error(
        "Chat Completions does not accept provider extension {provider}/{capability}@{schema_version}"
    )]
    UnsupportedProviderExtension {
        provider: String,
        capability: String,
        schema_version: u32,
    },
    #[error("Chat Completions projection could not be reconciled")]
    InvalidProjectionReconciliation,
    #[error("Chat Completions System instruction changed after the first submission")]
    SystemInstructionChanged,
    #[error("Chat Completions serialized outbound request body exceeded configured limit")]
    SerializedRequestBodyLimit,
    #[error("Chat Completions does not support native tool declarations")]
    UnsupportedNativeToolDeclarations,
    #[error(transparent)]
    PomRender(#[from] PomRenderError),
    #[error(transparent)]
    Projection(#[from] crate::component::execution::RenderedProjectionError),
    #[error(transparent)]
    Canonical(CanonicalTranscriptError),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

#[cfg(test)]
mod frame_profile_tests {
    use std::sync::atomic::AtomicU64;

    use crate::component::execution::reaction::{
        FrameConstraints, FrameRevision, ReactionPortFaultKind, ReactionPortFaultReason,
        TargetContinuity,
    };

    use super::*;

    fn config() -> AsyncOpenAiTransportConfig {
        AsyncOpenAiTransportConfig::new("http://127.0.0.1:1/v1", "test-token").unwrap()
    }

    fn provider(config: AsyncOpenAiTransportConfig) -> AsyncOpenAiChatCompletionsProvider {
        let identity = ProviderIdentity::new("openai", "chat-frame-profile", 1, "test").unwrap();
        let options = OpenAiChatCompletionsOptions::new("test-model").unwrap();
        AsyncOpenAiChatCompletionsProvider::try_new(config, identity, options).unwrap()
    }

    fn assert_invalid(constraints: FrameConstraints) {
        let error = match config().with_chat_completions_frame_constraints(constraints) {
            Ok(_) => panic!("invalid Chat Frame constraints were accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            super::super::AsyncOpenAiConfigError::InvalidChatCompletionsFrameProfile
        );
    }

    #[test]
    fn chat_target_declares_exact_production_defaults() {
        let provider = provider(config());
        let declaration = provider.reaction_target.declaration().unwrap();

        assert_eq!(
            declaration.profile().constraints,
            FrameConstraints {
                max_frame_bytes: 16 * 1024 * 1024,
                max_component_bytes: 4 * 1024 * 1024,
                context_window_tokens: None,
                reserved_output_tokens: None,
            }
        );
        assert!(declaration.profile().capabilities.supports_semantic_delta());
        assert!(matches!(
            declaration.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch == TargetEpoch::new(NonZeroU64::MIN)
        ));
    }

    #[test]
    fn chat_target_retains_custom_frame_constraints() {
        let constraints = FrameConstraints {
            max_frame_bytes: 32 * 1024,
            max_component_bytes: 8 * 1024,
            context_window_tokens: Some(128_000),
            reserved_output_tokens: Some(8_192),
        };
        let provider = provider(
            config()
                .with_chat_completions_frame_constraints(constraints.clone())
                .unwrap(),
        );

        assert_eq!(
            provider
                .reaction_target
                .declaration()
                .unwrap()
                .profile()
                .constraints,
            constraints
        );
    }

    #[test]
    fn chat_frame_constraints_reject_invalid_profile_shapes() {
        let valid = FrameConstraints {
            max_frame_bytes: 1_024,
            max_component_bytes: 256,
            context_window_tokens: Some(4_096),
            reserved_output_tokens: Some(512),
        };
        assert_invalid(FrameConstraints {
            max_frame_bytes: 0,
            ..valid.clone()
        });
        assert_invalid(FrameConstraints {
            max_component_bytes: 0,
            ..valid.clone()
        });
        assert_invalid(FrameConstraints {
            context_window_tokens: Some(0),
            ..valid.clone()
        });
        assert_invalid(FrameConstraints {
            reserved_output_tokens: Some(0),
            ..valid.clone()
        });
        assert_invalid(FrameConstraints {
            max_frame_bytes: 255,
            ..valid
        });
    }

    #[test]
    fn chat_target_identity_is_unique_and_domain_separated() {
        let first = provider(config());
        let second = provider(config());
        let first = first.reaction_target.declaration().unwrap().identity();
        let second = second.reaction_target.declaration().unwrap().identity();

        assert_ne!(first, second);
        assert_ne!(first.get().get() >> 64, 0);
        assert_eq!(first.get().get() >> 64, CHAT_TARGET_ID_DOMAIN >> 64);
    }

    #[test]
    fn accepted_continuity_preserves_the_chat_profile() {
        let mut provider = provider(config());
        let initial = provider.reaction_target.declaration().unwrap();
        let revision = FrameRevision::new(
            NonZeroU128::new(9).unwrap(),
            initial.identity(),
            initial.continuity().epoch(),
            NonZeroU64::MIN,
        );

        provider.reaction_target.accept(revision);
        let accepted = provider.reaction_target.declaration().unwrap();

        assert_eq!(accepted.profile(), initial.profile());
        assert_eq!(accepted.continuity().accepted_revision(), Some(revision));
    }

    #[test]
    fn chat_identity_exhaustion_is_typed_and_does_not_panic() {
        let exhausted = AtomicU64::new(u64::MAX);

        assert_eq!(
            next_chat_target_identity(&exhausted),
            Err(super::super::AsyncOpenAiConfigError::ChatCompletionsTargetIdentityExhausted)
        );
    }

    #[test]
    fn exhausted_chat_epoch_becomes_a_stable_terminal_fault() {
        let mut provider = provider(config());
        provider.reaction_target.epoch = TargetEpoch::new(NonZeroU64::new(u64::MAX).unwrap());
        provider.reaction_target.lose_continuity();

        for _ in 0..2 {
            let fault = provider.reaction_target.declaration().unwrap_err();
            assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
            assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
        }
    }
}
