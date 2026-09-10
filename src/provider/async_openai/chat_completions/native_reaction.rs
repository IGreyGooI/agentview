//! Frame-native `ReactionPort` implementation for OpenAI Chat Completions.

use std::{
    future::Future,
    sync::{atomic::Ordering, Arc, Mutex},
    task::Poll,
};

use ::async_openai::config::Config;
use async_trait::async_trait;
use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use serde_json::Value;

use crate::component::execution::reaction::{
    Frame, ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault,
    ResettableReactionPort, SubmitFault, TargetContinuity, TargetDeclaration,
};

use super::observation::{
    observation_fault, observe_facts, CapturedResponse, OpenAiChatObservation, ResponseCapture,
};
use super::{
    exceeds_limit,
    frame_request::{ChatFrameRequestFault, ChatFrameRequestState, PreparedChatFrameRequest},
    sse_event_size, AsyncOpenAiChatCompletionsProvider, ChatExecutionMode, ChatReactionTarget,
    ChatWireChunk,
};
use crate::provider::async_openai::{
    reaction_fault::{map_openai_fault, OpenAiFailureClass, OpenAiReactionFailure},
    SseWireLimiter,
};

#[async_trait]
impl ReactionPort for AsyncOpenAiChatCompletionsProvider {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.frame_native_declaration()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let declaration = self
            .frame_native_declaration()
            .map_err(SubmitFault::Rejected)?;
        frame.check_handoff_precondition(&declaration)?;
        let prepared = ChatFrameRequestState::prepare(
            self.reaction_frame.as_ref(),
            &frame,
            &self.options,
            self.max_serialized_request_body_bytes,
        )
        .map_err(frame_submit_fault)?;
        serde_json::from_slice::<Value>(&prepared.request_body).map_err(|_| {
            rejected(
                OpenAiFailureClass::RequestPreparation,
                "native Chat request body is not one JSON value",
            )
        })?;
        let request = self
            .client
            .post(self.config.url("/chat/completions"))
            .headers(self.config.headers())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(prepared.request_body.clone())
            .build()
            .map_err(|error| {
                rejected(
                    OpenAiFailureClass::RequestPreparation,
                    format!("native Chat request build failed: {error:?}"),
                )
            })?;

        if let Some(observer) = &self.observer {
            observer
                .observe(OpenAiChatObservation::Request {
                    path: request.url().path().to_owned(),
                    body: prepared.request_body.clone(),
                })
                .await
                .map_err(|_| {
                    self.observation_failed.store(true, Ordering::Release);
                    SubmitFault::Rejected(observation_fault())
                })?;
        }
        let observer = self.observer.clone();
        let observation_failed = self.observation_failed.clone();
        let capture = Arc::new(Mutex::new(CapturedResponse::default()));
        let stream_capture = capture.clone();
        let capture_enabled = observer.is_some();

        let client = self.client.clone();
        let execution_mode = &mut self.execution_mode;
        let target = &mut self.reaction_target;
        let frame_slot = &mut self.reaction_frame;
        let revision = frame.revision();
        let read_timeout = self.read_timeout;
        let max_response_body_bytes = self.max_response_body_bytes;
        let max_sse_event_bytes = self.max_sse_event_bytes;
        let max_output_text_bytes = self.max_output_text_bytes;
        let mut transport = Box::pin(client.execute(request));
        let mut prepared = Some(prepared);

        let first_response = futures::future::poll_fn(|cx| {
            let declaration = target.declaration().map_err(SubmitFault::Rejected)?;
            if let Err(fault) = validate_native_frame_state(&declaration, frame_slot.as_ref()) {
                target.record_fault(fault);
                return Poll::Ready(Err(SubmitFault::Rejected(fault)));
            }
            frame.check_handoff_precondition(&declaration)?;
            let Some(PreparedChatFrameRequest { state, .. }) = prepared.take() else {
                return Poll::Ready(Err(rejected(
                    OpenAiFailureClass::Internal,
                    "native Chat Frame candidate was consumed before handoff",
                )));
            };
            match execution_mode {
                ChatExecutionMode::Unclaimed => {
                    *execution_mode = ChatExecutionMode::FrameNative;
                }
                ChatExecutionMode::FrameNative => {}
                #[cfg(feature = "legacy-provider-port")]
                ChatExecutionMode::Legacy => {
                    return Poll::Ready(Err(rejected(
                        OpenAiFailureClass::DeclarationStateLost,
                        "Chat provider is already using the legacy protocol",
                    )));
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
                        format!("native Chat request transport failed: {error:?}"),
                    ));
                }
                Poll::Pending => match transport.await {
                    Ok(response) => response,
                    Err(error) => {
                        return continuity.fail(port_fault(
                            OpenAiFailureClass::RequestTransport,
                            format!("native Chat request transport failed: {error:?}"),
                        ));
                    }
                },
            };

            let status = response.status();
            stream_capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status = Some(status.as_u16());
            if !status.is_success() {
                if capture_enabled {
                    capture_error_response(
                        response,
                        &stream_capture,
                        read_timeout,
                        max_response_body_bytes,
                    )
                    .await;
                }
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
                if capture_enabled {
                    capture_error_response(
                        response,
                        &stream_capture,
                        read_timeout,
                        max_response_body_bytes,
                    )
                    .await;
                }
                return continuity.fail(port_fault(
                    OpenAiFailureClass::ResponseProtocolRetryable,
                    "successful Chat response did not use text/event-stream",
                ));
            }
            if response.content_length().is_some_and(|content_length| {
                content_length > u64::try_from(max_response_body_bytes).unwrap_or(u64::MAX)
            }) {
                return continuity.fail(port_fault(
                    OpenAiFailureClass::ResponseBodyLimit,
                    "Chat response Content-Length exceeded the configured limit",
                ));
            }

            let mut response_body_bytes = 0_usize;
            let mut sse_wire_limiter = SseWireLimiter::new(max_sse_event_bytes);
            let body_capture = stream_capture.clone();
            let response_stream = response.bytes_stream().map(move |chunk| {
                let chunk = chunk
                    .map_err(|error| NativeChatBodyStreamFault::Transport(format!("{error:?}")))?;
                if capture_enabled {
                    let mut captured = body_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let remaining = max_response_body_bytes.saturating_sub(captured.body.len());
                    captured
                        .body
                        .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                let next_size = response_body_bytes
                    .checked_add(chunk.len())
                    .ok_or(NativeChatBodyStreamFault::BodyLimit)?;
                if next_size > max_response_body_bytes {
                    return Err(NativeChatBodyStreamFault::BodyLimit);
                }
                sse_wire_limiter
                    .observe(&chunk)
                    .map_err(|_| NativeChatBodyStreamFault::EventLimit)?;
                response_body_bytes = next_size;
                Ok(chunk)
            });
            let Some(target) = continuity.transfer() else {
                return continuity.fail(port_fault(
                    OpenAiFailureClass::Internal,
                    "native Chat target continuity was transferred more than once",
                ));
            };
            let state = NativeChatStreamState {
                stream: response_stream.eventsource(),
                target,
                accumulated_text: String::new(),
                output_text_bytes: 0,
                response_id: None,
                response_model: None,
                finish_seen: false,
                completion_pending: false,
                terminal_published: false,
                fault_recorded: false,
                finished: false,
                read_timeout,
                max_sse_event_bytes,
                max_output_text_bytes,
                capture: stream_capture,
            };
            Box::pin(futures::stream::unfold(state, next_native_chat_fact))
                as ProviderFactStream<'a>
        };

        Ok(observe_facts(
            Box::pin(futures::stream::once(pending).flatten()),
            observer,
            capture,
            observation_failed,
        ))
    }
}

async fn capture_error_response(
    response: reqwest::Response,
    capture: &ResponseCapture,
    read_timeout: std::time::Duration,
    max_body_bytes: usize,
) {
    let mut body = response.bytes_stream();
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout(read_timeout, body.next()).await {
        let mut captured = capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = max_body_bytes.saturating_sub(captured.body.len());
        captured
            .body
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() >= remaining {
            break;
        }
    }
}

impl AsyncOpenAiChatCompletionsProvider {
    fn frame_native_declaration(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        if self.observation_failed.load(Ordering::Acquire) {
            return Err(observation_fault());
        }
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

impl ResettableReactionPort for AsyncOpenAiChatCompletionsProvider {
    fn reset_model_context(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.frame_native_declaration()?;
        self.reaction_target.lose_continuity();
        self.reaction_frame = None;
        #[cfg(feature = "legacy-provider-port")]
        {
            self.history = None;
            self.diff_memo = None;
        }
        self.frame_native_declaration()
    }
}

fn validate_native_frame_state(
    declaration: &TargetDeclaration,
    state: Option<&ChatFrameRequestState>,
) -> Result<(), ReactionPortFault> {
    let TargetContinuity::Accepted { revision, .. } = declaration.continuity() else {
        return Ok(());
    };
    if state.is_some_and(|state| state.revision() == *revision) {
        Ok(())
    } else {
        Err(port_fault(
            OpenAiFailureClass::DeclarationStateLost,
            "accepted Chat declaration has no matching wire baseline",
        ))
    }
}

fn frame_submit_fault(error: ChatFrameRequestFault) -> SubmitFault {
    rejected(error.failure_class(), format!("{error:?}"))
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
    target: Option<&'a mut ChatReactionTarget>,
}

impl<'a> PostHandoffContinuity<'a> {
    fn new(target: &'a mut ChatReactionTarget) -> Self {
        Self {
            target: Some(target),
        }
    }

    fn transfer(&mut self) -> Option<&'a mut ChatReactionTarget> {
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
enum NativeChatBodyStreamFault {
    Transport(String),
    BodyLimit,
    EventLimit,
}

struct NativeChatStreamState<'a, S> {
    stream: S,
    target: &'a mut ChatReactionTarget,
    accumulated_text: String,
    output_text_bytes: usize,
    response_id: Option<String>,
    response_model: Option<String>,
    finish_seen: bool,
    completion_pending: bool,
    terminal_published: bool,
    fault_recorded: bool,
    finished: bool,
    read_timeout: std::time::Duration,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    capture: ResponseCapture,
}

async fn next_native_chat_fact<S>(
    mut state: NativeChatStreamState<'_, S>,
) -> Option<(
    Result<ProviderFact, ReactionPortFault>,
    NativeChatStreamState<'_, S>,
)>
where
    S: futures::Stream<
            Item = Result<eventsource_stream::Event, EventStreamError<NativeChatBodyStreamFault>>,
        > + Unpin,
{
    if state.completion_pending {
        state.completion_pending = false;
        state.terminal_published = true;
        state.finished = true;
        return Some((
            Ok(ProviderFact::ReactionCompleted {
                primary_text: Some(ProviderOutputKey::new(0)),
            }),
            state,
        ));
    }
    if state.finished {
        return None;
    }

    loop {
        let event = match tokio::time::timeout(state.read_timeout, state.stream.next()).await {
            Err(_) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::StreamTimeout,
                        "Chat stream read timed out",
                    ),
                );
            }
            Ok(Some(Err(EventStreamError::Transport(NativeChatBodyStreamFault::Transport(
                diagnostic,
            ))))) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(OpenAiFailureClass::StreamTransport, diagnostic),
                );
            }
            Ok(Some(Err(EventStreamError::Transport(NativeChatBodyStreamFault::BodyLimit)))) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseBodyLimit,
                        "Chat stream body limit exceeded",
                    ),
                );
            }
            Ok(Some(Err(EventStreamError::Transport(NativeChatBodyStreamFault::EventLimit)))) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::StreamEventLimit,
                        "Chat SSE event wire limit exceeded",
                    ),
                );
            }
            Ok(Some(Err(error))) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        format!("Chat SSE decode failed: {error:?}"),
                    ),
                );
            }
            Ok(Some(Ok(event))) => event,
            Ok(None) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        "Chat stream ended before a validated terminal fact",
                    ),
                );
            }
        };

        if sse_event_size(&event).is_none_or(|size| size > state.max_sse_event_bytes) {
            return failed_native_chat_stream(
                state,
                port_fault(
                    OpenAiFailureClass::StreamEventLimit,
                    "decoded Chat SSE event exceeded configured limit",
                ),
            );
        }
        if event.data == "[DONE]" {
            if !state.finish_seen {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        "Chat DONE arrived before stop finish reason",
                    ),
                );
            }
            let text = std::mem::take(&mut state.accumulated_text);
            if text.is_empty() {
                state.terminal_published = true;
                state.finished = true;
                return Some((
                    Ok(ProviderFact::ReactionCompleted { primary_text: None }),
                    state,
                ));
            }
            state.completion_pending = true;
            return Some((
                Ok(ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    text,
                }),
                state,
            ));
        }
        let frame = match serde_json::from_str::<ChatWireChunk>(&event.data) {
            Ok(frame) => frame,
            Err(error) => {
                return failed_native_chat_stream(
                    state,
                    port_fault(
                        OpenAiFailureClass::ResponseProtocolRetryable,
                        format!("Chat event JSON failed: {error:?}"),
                    ),
                );
            }
        };
        let delta = match validate_native_chunk(&mut state, frame) {
            Ok(delta) => delta,
            Err(fault) => return failed_native_chat_stream(state, fault),
        };
        let Some(delta) = delta else {
            continue;
        };
        if exceeds_limit(
            state.output_text_bytes,
            delta.len(),
            state.max_output_text_bytes,
        ) {
            return failed_native_chat_stream(
                state,
                port_fault(
                    OpenAiFailureClass::OutputLimit,
                    "Chat output text exceeded configured limit",
                ),
            );
        }
        state.output_text_bytes += delta.len();
        state.accumulated_text.push_str(&delta);
        return Some((
            Ok(ProviderFact::TextDelta {
                output: ProviderOutputKey::new(0),
                phase: None,
                delta,
            }),
            state,
        ));
    }
}

fn failed_native_chat_stream<S>(
    mut state: NativeChatStreamState<'_, S>,
    fault: ReactionPortFault,
) -> Option<(
    Result<ProviderFact, ReactionPortFault>,
    NativeChatStreamState<'_, S>,
)> {
    state.finished = true;
    state.target.record_fault(fault);
    state.fault_recorded = true;
    Some((Err(fault), state))
}

fn validate_native_chunk<S>(
    state: &mut NativeChatStreamState<'_, S>,
    frame: ChatWireChunk,
) -> Result<Option<String>, ReactionPortFault> {
    if frame.id.is_empty()
        || frame.model.is_empty()
        || frame.object != "chat.completion.chunk"
        || (frame.choices.len() != 1 && !(frame.choices.is_empty() && frame.usage.is_some()))
    {
        return Err(protocol("invalid Chat chunk envelope"));
    }
    match &state.response_id {
        Some(id) if id != &frame.id => return Err(protocol("Chat response identity changed")),
        None => state.response_id = Some(frame.id),
        _ => {}
    }
    match &state.response_model {
        Some(model) if model != &frame.model => {
            return Err(protocol("Chat response model identity changed"));
        }
        None => state.response_model = Some(frame.model),
        _ => {}
    }

    if let Some(usage) = frame.usage {
        state
            .capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .usage = Some(usage);
    }
    if frame.choices.is_empty() {
        if !state.finish_seen {
            return Err(protocol("Chat usage arrived before terminal choice"));
        }
        return Ok(None);
    }
    if state.finish_seen {
        return Err(protocol("Chat event followed the terminal choice"));
    }

    let choice = frame
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| protocol("Chat chunk omitted its choice"))?;
    if choice.delta.tool_calls.is_some()
        || choice.delta.function_call.is_some()
        || choice.delta.refusal.is_some()
    {
        return Err(upstream_rejected(
            "Chat returned unsupported non-text output",
        ));
    }
    if choice.index != 0
        || choice
            .delta
            .role
            .as_deref()
            .is_some_and(|role| role != "assistant")
    {
        return Err(protocol("invalid Chat text lifecycle"));
    }
    match choice.finish_reason.as_deref() {
        None => Ok(choice.delta.content.filter(|content| !content.is_empty())),
        Some("stop") if choice.delta.content.as_deref().is_none_or(str::is_empty) => {
            state.finish_seen = true;
            Ok(None)
        }
        Some(_) => Err(upstream_rejected(
            "Chat returned an unsupported finish reason",
        )),
    }
}

impl<S> Drop for NativeChatStreamState<'_, S> {
    fn drop(&mut self) {
        if !self.terminal_published && !self.fault_recorded {
            self.target.lose_continuity();
        }
    }
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
        num::{NonZeroU128, NonZeroU64},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        task::Poll,
    };

    use crate::component::{
        execution::{Application, ApplicationFaultKind, ApplicationFaultReason},
        prelude::*,
    };
    use axum::{
        body::{Body, Bytes},
        extract::State,
        http::{header, Response, StatusCode},
        response::IntoResponse,
        routing::post,
        Router,
    };
    use futures::StreamExt;
    use serde_json::{json, Value};
    use tokio::sync::{mpsc, oneshot};

    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "these tests intentionally exercise the retained Chat ProviderPort mode fence"
    )]
    use crate::component::execution::ProviderPort;
    #[cfg(feature = "legacy-provider-port")]
    use crate::component::execution::{RenderedProjection, RenderedProjectionNode};
    use crate::{
        component::execution::{
            reaction::{
                Frame, FrameBasis, FrameRevision, FrameSubmission, ProjectionSubmission,
                ProviderFact, ProviderOutputKey, ReactionPort, ReactionPortFaultCode,
                ReactionPortFaultKind, ReactionPortFaultReason, ResettableReactionPort,
                SubmitFault, TargetContinuity, TargetDeclaration, ToolCatalog,
            },
            ProviderIdentity,
        },
        provider::async_openai::{
            AsyncOpenAiChatCompletionsProvider, AsyncOpenAiTransportConfig,
            OpenAiChatCompletionsOptions, OpenAiChatObservation, OpenAiChatObservationError,
            OpenAiChatObserver,
        },
        transcript::CanonicalInputItem,
    };

    #[derive(Clone)]
    struct Reply {
        status: StatusCode,
        content_type: &'static str,
        body: Arc<String>,
        requests: mpsc::UnboundedSender<Vec<u8>>,
    }

    async fn respond(State(reply): State<Reply>, body: Bytes) -> impl IntoResponse {
        reply.requests.send(body.to_vec()).unwrap();
        Response::builder()
            .status(reply.status)
            .header(header::CONTENT_TYPE, reply.content_type)
            .body(Body::from(reply.body.as_str().to_owned()))
            .unwrap()
    }

    async fn spawn_server(
        status: StatusCode,
        content_type: &'static str,
        body: String,
    ) -> (
        String,
        mpsc::UnboundedReceiver<Vec<u8>>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let app = Router::new()
            .route("/chat/completions", post(respond))
            .with_state(Reply {
                status,
                content_type,
                body: Arc::new(body),
                requests: request_tx,
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
        (format!("http://{address}"), request_rx, shutdown_tx, task)
    }

    fn completed_sse(text: &str) -> String {
        let delta = json!({
            "id": "chatcmpl_native",
            "object": "chat.completion.chunk",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": text},
                "finish_reason": null
            }]
        });
        let stop = json!({
            "id": "chatcmpl_native",
            "object": "chat.completion.chunk",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        });
        format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
    }

    fn invalid_finish_sse() -> String {
        let invalid = json!({
            "id": "chatcmpl_native",
            "object": "chat.completion.chunk",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "length"
            }]
        });
        format!("data: {invalid}\n\n")
    }

    fn provider(base: &str, body_limit: Option<usize>) -> AsyncOpenAiChatCompletionsProvider {
        let config = AsyncOpenAiTransportConfig::new(base, "test-token").unwrap();
        let config = match body_limit {
            Some(limit) => config
                .with_chat_completions_serialized_request_body_limit(limit)
                .unwrap(),
            None => config,
        };
        let identity = ProviderIdentity::new("openai", "native-chat", 1, "native-test").unwrap();
        let options = OpenAiChatCompletionsOptions::new("test-model").unwrap();
        AsyncOpenAiChatCompletionsProvider::try_new(config, identity, options).unwrap()
    }

    fn frame(
        declaration: &TargetDeclaration,
        sequence: u64,
        basis: FrameBasis,
        replay: Vec<CanonicalInputItem>,
        projection: Vec<CanonicalInputItem>,
        tools: Vec<&str>,
    ) -> Frame {
        let epoch = declaration.continuity().epoch();
        let revision = FrameRevision::new(
            NonZeroU128::new(701).unwrap(),
            declaration.identity(),
            epoch,
            NonZeroU64::new(sequence).unwrap(),
        );
        let submission = FrameSubmission::from_compiled(
            replay,
            Vec::new(),
            ProjectionSubmission::new(projection),
            ToolCatalog::new(tools.into_iter().map(str::to_owned).collect()).unwrap(),
            Vec::new(),
        );
        Frame::from_compiled(
            revision,
            declaration.identity(),
            epoch,
            declaration.continuity().clone(),
            declaration.profile().clone(),
            basis,
            submission,
        )
        .unwrap()
    }

    fn full_frame(declaration: &TargetDeclaration) -> Frame {
        frame(
            declaration,
            1,
            FrameBasis::Full,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[cfg(feature = "legacy-provider-port")]
    fn empty_projection() -> RenderedProjection {
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", Vec::new())])
            .unwrap()
    }

    async fn collect_facts(
        provider: &mut AsyncOpenAiChatCompletionsProvider,
        frame: Frame,
    ) -> Vec<Result<ProviderFact, crate::component::execution::reaction::ReactionPortFault>> {
        ReactionPort::submit(provider, frame)
            .await
            .unwrap()
            .collect()
            .await
    }

    #[derive(Default)]
    struct Audit {
        events: Mutex<Vec<OpenAiChatObservation>>,
        reject_request: bool,
        reject_response: bool,
    }

    #[component]
    fn retry_test_application(completions: Arc<AtomicUsize>) -> Component {
        let exit = use_application_exit();
        use_provider_event_handler(ProviderEvent::TEXT, move |event| {
            let completions = completions.clone();
            async move {
                if let TextTurnEvent::TextComplete(text) = event {
                    assert_eq!(text, "accepted");
                    completions.fetch_add(1, Ordering::SeqCst);
                }
                Ok::<(), String>(())
            }
        });
        use_reaction_completion(move || async move {
            exit.request(ExitReason::Completed)?;
            Ok::<(), anyhow::Error>(())
        });
        view! { retry_probe { "Complete this single request." } }
    }

    #[tokio::test]
    async fn application_run_resumes_after_transient_http_error_without_duplicate_completion() {
        let requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let captured = requests.clone();
        let server_app = Router::new().route(
            "/chat/completions",
            post(move |body: Bytes| {
                let captured = captured.clone();
                async move {
                    let first = {
                        let mut requests = captured.lock().unwrap();
                        requests.push(body.to_vec());
                        requests.len() == 1
                    };
                    if first {
                        Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(r#"{"error":{"message":"busy"}}"#))
                            .unwrap()
                    } else {
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from(completed_sse("accepted")))
                            .unwrap()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, server_app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let completions = Arc::new(AtomicUsize::new(0));
        let accepted = completions.clone();
        let mut application = Application::mount(
            move || retry_test_application(accepted.clone()),
            provider(&base, None),
        )
        .unwrap();
        let failure = application.run().await.unwrap_err();
        assert_eq!(failure.kind(), ApplicationFaultKind::Retryable);
        assert_eq!(
            failure.reason(),
            ApplicationFaultReason::Port(ReactionPortFaultReason::RateLimited)
        );
        assert_eq!(completions.load(Ordering::SeqCst), 0);
        assert_eq!(application.run().await.unwrap(), ExitReason::Completed);
        application.shutdown().await.unwrap();
        assert_eq!(completions.load(Ordering::SeqCst), 1);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let first: Value = serde_json::from_slice(&requests[0]).unwrap();
        let second: Value = serde_json::from_slice(&requests[1]).unwrap();
        assert_eq!(first["messages"], second["messages"]);
        drop(requests);
        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[async_trait::async_trait]
    impl OpenAiChatObserver for Audit {
        async fn observe(
            &self,
            event: OpenAiChatObservation,
        ) -> Result<(), OpenAiChatObservationError> {
            let reject = matches!(event, OpenAiChatObservation::Request { .. })
                && self.reject_request
                || matches!(event, OpenAiChatObservation::Response { .. }) && self.reject_response;
            self.events.lock().unwrap().push(event);
            if reject {
                Err(OpenAiChatObservationError)
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn observes_exact_bodies_and_usage_before_sealed_text() {
        let usage = json!({"prompt_tokens": 41, "completion_tokens": 7, "total_tokens": 48});
        let usage_chunk = json!({
            "id": "chatcmpl_native", "object": "chat.completion.chunk", "model": "test-model",
            "choices": [], "usage": usage,
        });
        let response = completed_sse("<answer />").replace(
            "data: [DONE]",
            &format!("data: {usage_chunk}\n\ndata: [DONE]"),
        );
        let (base, mut requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", response.clone()).await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        provider.options = OpenAiChatCompletionsOptions::new("test-model")
            .unwrap()
            .with_max_tokens(321)
            .unwrap()
            .with_temperature(0.2)
            .unwrap()
            .with_usage(true);
        let frame = full_frame(&provider.declare().unwrap());
        let mut facts = provider.submit(frame).await.unwrap();
        while let Some(fact) = facts.next().await {
            if matches!(fact, Ok(ProviderFact::TextSealed { .. })) {
                assert_eq!(audit.events.lock().unwrap().len(), 2);
            }
            assert!(fact.is_ok());
        }
        drop(facts);
        let request = requests.recv().await.unwrap();
        let wire: Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(wire["max_tokens"], 321);
        assert_eq!(wire["temperature"], 0.2);
        assert_eq!(wire["stream_options"]["include_usage"], true);
        let events = audit.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], OpenAiChatObservation::Request { body, path } if body == &request && path == "/chat/completions")
        );
        assert!(
            matches!(&events[1], OpenAiChatObservation::Response { body, status: 200, completed: true, usage: Some(observed) } if body == response.as_bytes() && observed == &usage)
        );
        drop(events);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_request_audit_blocks_transport_and_is_terminal() {
        let (base, mut requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", completed_sse("ok")).await;
        let mut provider = provider(&base, None).with_observer(Arc::new(Audit {
            reject_request: true,
            ..Audit::default()
        }));
        let frame = full_frame(&provider.declare().unwrap());
        assert!(matches!(
            provider.submit(frame).await,
            Err(SubmitFault::Rejected(_))
        ));
        assert!(requests.try_recv().is_err());
        assert_eq!(
            provider.declare().unwrap_err().kind(),
            ReactionPortFaultKind::Terminal
        );
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retryable_provider_failure_records_raw_body_and_rebuilds_continuity() {
        let response = r#"{"error":{"message":"busy"}}"#;
        let (base, _requests, shutdown, server) = spawn_server(
            StatusCode::TOO_MANY_REQUESTS,
            "application/json",
            response.into(),
        )
        .await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        let first = provider.declare().unwrap();
        let facts = collect_facts(&mut provider, full_frame(&first)).await;
        assert!(
            matches!(facts.as_slice(), [Err(fault)] if fault.kind() == ReactionPortFaultKind::Retryable)
        );
        let next = provider.declare().unwrap();
        assert!(next.continuity().epoch().get() > first.continuity().epoch().get());
        let events = audit.events.lock().unwrap();
        assert!(
            matches!(&events[1], OpenAiChatObservation::Response { status: 429, body, completed: false, .. } if body == response.as_bytes())
        );
        assert!(matches!(&events[2], OpenAiChatObservation::Error { .. }));
        drop(events);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_response_audit_never_exposes_sealed_text_or_completion() {
        let (base, _requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", completed_sse("ok")).await;
        let mut provider = provider(&base, None).with_observer(Arc::new(Audit {
            reject_response: true,
            ..Audit::default()
        }));
        let frame = full_frame(&provider.declare().unwrap());
        let facts = collect_facts(&mut provider, frame).await;
        assert!(facts.iter().any(Result::is_err));
        assert!(!facts.iter().any(|fact| matches!(
            fact,
            Ok(ProviderFact::TextSealed { .. } | ProviderFact::ReactionCompleted { .. })
        )));
        assert_eq!(
            provider.declare().unwrap_err().kind(),
            ReactionPortFaultKind::Terminal
        );
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn context_reset_discards_accepted_wire_history() {
        let (base, mut requests, shutdown, server) = spawn_server(
            StatusCode::OK,
            "text/event-stream",
            completed_sse("old output"),
        )
        .await;
        let mut provider = provider(&base, None);
        let first = provider.declare().unwrap();
        let input = frame(
            &first,
            1,
            FrameBasis::Full,
            vec![CanonicalInputItem::assistant_text("old baseline", None)],
            vec![],
            vec![],
        );
        let _ = collect_facts(&mut provider, input).await;
        let _ = requests.recv().await.unwrap();
        let reset = provider.reset_model_context().unwrap();
        assert_eq!(reset.identity(), first.identity());
        assert!(reset.continuity().epoch().get() > first.continuity().epoch().get());
        let input = full_frame(&reset);
        let _ = collect_facts(&mut provider, input).await;
        let request: Value = serde_json::from_slice(&requests.recv().await.unwrap()).unwrap();
        assert_eq!(request["messages"], json!([]));
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn submit_crosses_handoff_in_the_first_transport_poll() {
        let mut provider = provider("http://127.0.0.1:1", None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let mut submit = Box::pin(ReactionPort::submit(&mut provider, full_frame(&initial)));

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
    async fn complete_text_uses_one_output_identity_and_terminal_marker() {
        let (base, mut requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", completed_sse("hello")).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let facts = collect_facts(&mut provider, full_frame(&initial)).await;

        assert_eq!(
            facts,
            [
                Ok(ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    delta: "hello".to_owned(),
                }),
                Ok(ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(0),
                    phase: None,
                    text: "hello".to_owned(),
                }),
                Ok(ProviderFact::ReactionCompleted {
                    primary_text: Some(ProviderOutputKey::new(0)),
                }),
            ]
        );
        assert!(requests.recv().await.is_some());
        assert!(matches!(
            ReactionPort::declare(&mut provider).unwrap().continuity(),
            TargetContinuity::Accepted { .. }
        ));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_completion_does_not_fabricate_a_text_output() {
        let (base, _requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", completed_sse("")).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();

        let facts = collect_facts(&mut provider, full_frame(&initial)).await;

        assert_eq!(
            facts,
            [Ok(ProviderFact::ReactionCompleted { primary_text: None })]
        );
        assert!(matches!(
            ReactionPort::declare(&mut provider).unwrap().continuity(),
            TargetContinuity::Accepted { .. }
        ));

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn next_delta_adds_the_prior_output_and_new_input_once() {
        let (base, mut requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", completed_sse("answer")).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let first = frame(
            &initial,
            1,
            FrameBasis::Full,
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("authored", None)],
            Vec::new(),
        );
        assert!(collect_facts(&mut provider, first)
            .await
            .into_iter()
            .all(|fact| fact.is_ok()));
        let first_body: Value = serde_json::from_slice(&requests.recv().await.unwrap()).unwrap();
        assert_eq!(first_body["messages"].as_array().unwrap().len(), 1);

        let accepted = ReactionPort::declare(&mut provider).unwrap();
        let base_revision = accepted.continuity().accepted_revision().unwrap();
        let second = frame(
            &accepted,
            2,
            FrameBasis::DeltaFrom(base_revision),
            vec![CanonicalInputItem::assistant_text("answer", None)],
            vec![CanonicalInputItem::assistant_text("next", None)],
            Vec::new(),
        );
        assert!(collect_facts(&mut provider, second)
            .await
            .into_iter()
            .all(|fact| fact.is_ok()));
        let second_body: Value = serde_json::from_slice(&requests.recv().await.unwrap()).unwrap();
        let contents = second_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(contents, ["authored", "answer", "next"]);

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retryable_http_fault_advances_epoch() {
        let (base, _requests, shutdown, server) = spawn_server(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            "upstream-private".to_owned(),
        )
        .await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let facts = collect_facts(&mut provider, full_frame(&initial)).await;
        let fault = facts.into_iter().next().unwrap().unwrap_err();

        assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
        assert_eq!(fault.reason(), ReactionPortFaultReason::UpstreamRejected);
        let after = ReactionPort::declare(&mut provider).unwrap();
        assert!(after.continuity().epoch() > initial.continuity().epoch());

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn terminal_protocol_fault_is_sticky() {
        let (base, _requests, shutdown, server) =
            spawn_server(StatusCode::OK, "text/event-stream", invalid_finish_sse()).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let facts = collect_facts(&mut provider, full_frame(&initial)).await;
        let fault = facts.into_iter().next().unwrap().unwrap_err();

        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.code(), ReactionPortFaultCode::Rejected);
        for _ in 0..2 {
            assert_eq!(ReactionPort::declare(&mut provider).unwrap_err(), fault);
        }

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn done_before_stop_is_retryable_and_requires_a_new_epoch() {
        let (base, _requests, shutdown, server) = spawn_server(
            StatusCode::OK,
            "text/event-stream",
            "data: [DONE]\n\n".to_owned(),
        )
        .await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let fault = collect_facts(&mut provider, full_frame(&initial))
            .await
            .into_iter()
            .next()
            .unwrap()
            .unwrap_err();

        assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
        assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
        assert!(
            ReactionPort::declare(&mut provider)
                .unwrap()
                .continuity()
                .epoch()
                > initial.continuity().epoch()
        );

        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn local_rejections_do_not_cross_handoff_or_claim_mode() {
        let mut limited = provider("http://127.0.0.1:1", Some(1));
        let initial = ReactionPort::declare(&mut limited).unwrap();
        let error = match ReactionPort::submit(&mut limited, full_frame(&initial)).await {
            Err(error) => error,
            Ok(_) => panic!("oversized request was accepted"),
        };
        let SubmitFault::Rejected(fault) = error else {
            panic!("wrong request-limit fault: {error:?}");
        };
        assert_eq!(fault.code(), ReactionPortFaultCode::Limit);
        assert_eq!(ReactionPort::declare(&mut limited).unwrap(), initial);
        assert_eq!(limited.execution_mode, super::ChatExecutionMode::Unclaimed);

        let mut tool_rejected = provider("http://127.0.0.1:1", None);
        let initial = ReactionPort::declare(&mut tool_rejected).unwrap();
        let tools = frame(
            &initial,
            1,
            FrameBasis::Full,
            Vec::new(),
            Vec::new(),
            vec!["lookup"],
        );
        assert!(matches!(
            ReactionPort::submit(&mut tool_rejected, tools).await,
            Err(SubmitFault::Rejected(_))
        ));
        assert_eq!(ReactionPort::declare(&mut tool_rejected).unwrap(), initial);
        assert_eq!(
            tool_rejected.execution_mode,
            super::ChatExecutionMode::Unclaimed
        );
    }

    #[tokio::test]
    async fn stale_continuity_and_profile_are_rejected_before_handoff() {
        let mut provider = provider("http://127.0.0.1:1", None);
        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let stale_continuity = full_frame(&declaration);
        provider.reaction_target.lose_continuity();
        assert!(matches!(
            ReactionPort::submit(&mut provider, stale_continuity).await,
            Err(SubmitFault::ContinuityChanged)
        ));

        let declaration = ReactionPort::declare(&mut provider).unwrap();
        let stale_profile = full_frame(&declaration);
        provider.reaction_target.profile.constraints.max_frame_bytes += 1;
        assert!(matches!(
            ReactionPort::submit(&mut provider, stale_profile).await,
            Err(SubmitFault::ProfileChanged)
        ));
    }

    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "this test intentionally crosses legacy ProviderPort and native ReactionPort"
    )]
    #[tokio::test]
    async fn legacy_and_native_handoffs_are_mutually_exclusive() {
        let mut legacy_first = provider("http://127.0.0.1:1", None);
        let legacy_stream = ProviderPort::execute(&mut legacy_first, empty_projection())
            .await
            .unwrap();
        drop(legacy_stream);
        assert_eq!(
            legacy_first.execution_mode,
            super::ChatExecutionMode::Legacy
        );
        let fault = ReactionPort::declare(&mut legacy_first).unwrap_err();
        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);

        let mut native_first = provider("http://127.0.0.1:1", None);
        let declaration = ReactionPort::declare(&mut native_first).unwrap();
        let native_stream = ReactionPort::submit(&mut native_first, full_frame(&declaration))
            .await
            .unwrap();
        drop(native_stream);
        assert_eq!(
            native_first.execution_mode,
            super::ChatExecutionMode::FrameNative
        );
        let legacy_fault = match ProviderPort::execute(&mut native_first, empty_projection()).await
        {
            Err(fault) => fault,
            Ok(_) => panic!("legacy mode was accepted after native handoff"),
        };
        assert_eq!(
            legacy_fault.code(),
            crate::component::execution::ProviderFaultCode::RequestPreparation
        );
    }

    #[test]
    fn accepted_declaration_requires_matching_private_wire_state() {
        let mut provider = provider("http://127.0.0.1:1", None);
        let initial = provider.reaction_target.declaration().unwrap();
        let revision = FrameRevision::new(
            NonZeroU128::new(701).unwrap(),
            initial.identity(),
            initial.continuity().epoch(),
            NonZeroU64::MIN,
        );
        provider.reaction_target.accept(revision);

        let fault = ReactionPort::declare(&mut provider).unwrap_err();

        assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
        assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
        assert_eq!(ReactionPort::declare(&mut provider).unwrap_err(), fault);
    }
}
