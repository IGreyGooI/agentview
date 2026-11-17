//! Frame-native `ReactionPort` implementation for OpenAI Responses.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    num::NonZeroU64,
    sync::{atomic::Ordering, Arc, Mutex},
    task::Poll,
};

use ::async_openai::config::Config;
use async_trait::async_trait;
use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use serde_json::{Map, Value};

use crate::{
    component::execution::reaction::{
        ForkableReactionPort, Frame, FrameBasis, ProviderFact, ProviderFactStream,
        ProviderOutputKey, ProviderToolCall, ReactionPort, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultReason, ResettableReactionPort, SubmitFault,
        TargetContinuity, TargetDeclaration,
    },
    component::execution::{DecisionBatchId, ProviderDecision},
    provider::codex_http_v1::CodexHttpV1Error,
    transcript::{AssistantPhase, CanonicalInputItem},
};

use super::{
    call_observation::{
        CallAccounting, CallOrigin, OpenAiResponsesCallKind, OpenAiResponsesCallOutcome,
    },
    declaration_state_lost_fault, exceeds_limit,
    frame_request::{
        PreparedResponsesFrameRequest, PrivateOutputKind, ResponsesFrameRequestFault,
        ResponsesFrameRequestState,
    },
    has_native_function_call_completed, has_unsupported_completed_item,
    has_unsupported_content_part, has_unsupported_lifecycle_item, is_native_tool_event,
    native_function_call,
    output::{OpenAiOutputLedger, SealedOpenAiPrivateOutput},
    parallel_compaction::{
        CompactionContext, CompactionMode, CompactionTransport, ContextEstimate,
    },
    reaction_fault::{map_openai_fault, OpenAiFailureClass, OpenAiReactionFailure},
    request_observation::{observe_request, OpenAiResponsesRequestSnapshot},
    responses_observation::{
        capture_error_response, observation_fault, observe_facts, CapturedResponse,
        OpenAiResponsesFrameBasis, OpenAiResponsesObservation,
        OpenAiResponsesRequestPreparationDiagnostic, OpenAiResponsesRequestPreparationReason,
        OpenAiResponsesTransportDiagnostic,
    },
    sse_event_size,
    usage::{observe_response_usage, validated_response_usage, ResponseUsageObserver},
    AsyncOpenAiResponsesProvider, NativeToolLedger, OpenAiWireEvent, ResponsesExecutionMode,
    ResponsesReactionTarget, SseWireLimiter,
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
        let decision_batch = frame
            .submission()
            .decision()
            .map(|decision| decision.batch_id().clone());
        let is_decision = decision_batch.is_some();
        let Some(context_window_tokens) =
            frame.prepared_profile().constraints.context_window.tokens()
        else {
            return Err(self
                .reject_frame_preparation(
                    &frame,
                    ResponsesFrameRequestFault::MissingContextWindow,
                    None,
                    false,
                )
                .await);
        };
        // Decision Frames never start, await, or install model summaries.
        // A previously running background attempt loses its installation
        // eligibility; its owned cleanup remains part of port shutdown.
        if is_decision {
            self.parallel_compaction.reset();
        }
        let mut compacted_base = if is_decision {
            None
        } else {
            match self.parallel_compaction.prepare_base(
                self.reaction_frame.as_ref(),
                &frame,
                &self.encoder,
            ) {
                Ok(base) => base,
                Err(error) => {
                    return Err(self
                        .reject_frame_preparation(&frame, error, None, false)
                        .await);
                }
            }
        };
        let base = compacted_base
            .as_ref()
            .map(|(_, state)| state)
            .or(self.reaction_frame.as_ref());
        let mut compacted_base_used = base.is_some_and(ResponsesFrameRequestState::is_compacted);
        let mut request_body_bytes = None;
        let mut prepared = ResponsesFrameRequestState::prepare_with_measurements(
            base,
            &frame,
            &self.encoder,
            self.max_responses_serialized_request_body_bytes,
            &mut request_body_bytes,
        );
        if matches!(
            &prepared,
            Err(ResponsesFrameRequestFault::Encoding(
                CodexHttpV1Error::SerializedRequestBodyLimit
            ))
        ) && compacted_base.is_some()
        {
            // Replacing a cached prefix can enlarge the upload. An optional
            // summary must not prevent a valid original request from proceeding.
            compacted_base = None;
            compacted_base_used = self
                .reaction_frame
                .as_ref()
                .is_some_and(ResponsesFrameRequestState::is_compacted);
            prepared = ResponsesFrameRequestState::prepare_with_measurements(
                self.reaction_frame.as_ref(),
                &frame,
                &self.encoder,
                self.max_responses_serialized_request_body_bytes,
                &mut request_body_bytes,
            );
        }
        let mut prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(self
                    .reject_frame_preparation(
                        &frame,
                        error,
                        request_body_bytes,
                        compacted_base_used,
                    )
                    .await);
            }
        };
        let estimate = ContextEstimate::from_bytes(prepared.context_bytes);
        let reserve = frame.prepared_profile().constraints.reserved_output_tokens.unwrap_or(0);
        if !is_decision
            && self.forced_compaction_enabled
            && compacted_base.is_none()
            && u128::from(estimate.tokens) + u128::from(reserve)
                > u128::from(context_window_tokens.get())
        {
            let source = self
                .reaction_frame
                .as_ref()
                .and_then(ResponsesFrameRequestState::compaction_source);
            let request = self.parallel_compaction.prepare_request(
                source,
                CompactionMode::Required {
                    wait_timeout: self.read_timeout,
                },
            );
            let transport = self.compaction_transport(
                context_window_tokens,
                &frame,
                OpenAiResponsesCallKind::ForcedCompaction,
            );
            if self.parallel_compaction.request(request, transport).await {
                compacted_base = match self.parallel_compaction.prepare_base(
                    self.reaction_frame.as_ref(),
                    &frame,
                    &self.encoder,
                ) {
                    Ok(base) => base,
                    Err(error) => {
                        return Err(self
                            .reject_frame_preparation(&frame, error, None, false)
                            .await);
                    }
                };
                if let Some((_, forced_base)) = compacted_base.as_ref() {
                    let mut compacted_bytes = None;
                    match ResponsesFrameRequestState::prepare_with_measurements(
                        Some(forced_base),
                        &frame,
                        &self.encoder,
                        self.max_responses_serialized_request_body_bytes,
                        &mut compacted_bytes,
                    ) {
                        Ok(compacted) => {
                            compacted_base_used = forced_base.is_compacted();
                            request_body_bytes = compacted_bytes;
                            prepared = compacted;
                        }
                        Err(ResponsesFrameRequestFault::Encoding(
                            CodexHttpV1Error::SerializedRequestBodyLimit,
                        )) => compacted_base = None,
                        Err(error) => {
                            return Err(self
                                .reject_frame_preparation(
                                    &frame,
                                    error,
                                    compacted_bytes,
                                    true,
                                )
                                .await);
                        }
                    }
                }
            }
            // The estimate is a compaction trigger, never an admission gate.
            // Missing/failed summaries and high post-compaction estimates leave
            // the original or compacted request eligible for actual handoff.
        }
        let compaction_request = if !is_decision
            && compacted_base.is_none()
            && self.parallel_compaction_enabled
            && !self.parallel_compaction.has_attempt()
        {
            let source = self
                .reaction_frame
                .as_ref()
                .and_then(ResponsesFrameRequestState::compaction_source);
            let context = CompactionContext {
                requested_context: frame
                    .requested_context_bytes()
                    .map(ContextEstimate::from_bytes),
                provider_context: ContextEstimate::from_bytes(prepared.context_bytes),
                compactable_context: ContextEstimate::from_bytes(
                    source.as_ref().map_or(0, |source| source.input_bytes),
                ),
                context_window_tokens,
                reserved_output_tokens: frame.prepared_profile().constraints.reserved_output_tokens,
            };
            Some(
                self.parallel_compaction
                    .prepare_request(source, CompactionMode::Background(&context)),
            )
        } else {
            None
        };
        let compaction_transport = compaction_request.as_ref().map(|_| {
            self.compaction_transport(
                context_window_tokens,
                &frame,
                OpenAiResponsesCallKind::BackgroundCompaction,
            )
        });
        if serde_json::from_slice::<Value>(&prepared.request_body).is_err() {
            let fault = port_fault(
                OpenAiFailureClass::RequestPreparation,
                "native request body is not one JSON value",
            );
            return Err(self
                .observe_preparation_rejection(
                    &frame,
                    fault,
                    OpenAiResponsesRequestPreparationReason::Encoding,
                    request_body_bytes,
                    compacted_base_used,
                )
                .await);
        }
        let request = match self
            .client
            .post(self.config.url("/responses"))
            .headers(self.config.headers())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(prepared.request_body.clone())
            .build()
        {
            Ok(request) => request,
            Err(_) => {
                let fault = port_fault(
                    OpenAiFailureClass::RequestPreparation,
                    "request build failed",
                );
                return Err(self
                    .observe_preparation_rejection(
                        &frame,
                        fault,
                        OpenAiResponsesRequestPreparationReason::RequestBuild,
                        request_body_bytes,
                        compacted_base_used,
                    )
                    .await);
            }
        };

        if let Some(observer) = &self.observer {
            observer
                .observe(OpenAiResponsesObservation::Request {
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
        let transport_observer = observer.clone();
        let observation_failed = self.observation_failed.clone();
        let capture = Arc::new(Mutex::new(CapturedResponse::default()));
        let stream_capture = capture.clone();
        let capture_enabled = observer.is_some();

        let request_observer = self.request_observer.clone();
        let request_snapshot = request_observer
            .as_ref()
            .map(|_| OpenAiResponsesRequestSnapshot::new(&frame, &prepared.request_body));
        let mut accounting = CallAccounting::new(
            &self.call_observer,
            CallOrigin::from_frame(&frame),
            OpenAiResponsesCallKind::Foreground,
            self.encoder.model().id(),
        );
        let client = self.client.clone();
        let execution_mode = &mut self.execution_mode;
        let target = &mut self.reaction_target;
        let frame_slot = &mut self.reaction_frame;
        let revision = frame.revision();
        let read_timeout = self.read_timeout;
        let sse_event_timeout = self.sse_event_timeout;
        let max_response_body_bytes = self.max_response_body_bytes;
        let max_sse_event_bytes = self.max_sse_event_bytes;
        let max_output_text_bytes = self.max_output_text_bytes;
        let cache_continuation = prepared.cache_continuation;
        let response_usage_observer = self.response_usage_observer.clone();
        let transport_started = std::time::Instant::now();
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
            accounting.start();
            let response = transport.as_mut().poll(cx);
            match &response {
                Poll::Ready(Ok(response)) => {
                    accounting.headers(response.status().as_u16(), response.headers());
                }
                Poll::Ready(Err(_)) => accounting.finish(OpenAiResponsesCallOutcome::Failed),
                Poll::Pending => {}
            }
            *frame_slot = Some(state);
            target.accept(revision);
            Poll::Ready(Ok::<_, SubmitFault>(response))
        })
        .await?;

        if let Some((id, _)) = compacted_base {
            self.parallel_compaction.installed(id);
        }
        if let Some((request, transport)) = compaction_request.zip(compaction_transport) {
            self.parallel_compaction.request(request, transport).await;
        }
        let mut continuity = PostHandoffContinuity::new(target);
        observe_request(&request_observer, request_snapshot);
        let pending = async move {
            let response = match first_response {
                Poll::Ready(Ok(response)) => response,
                Poll::Ready(Err(error)) => {
                    accounting.finish(OpenAiResponsesCallOutcome::Failed);
                    if observe_transport_failure(&transport_observer, &error, transport_started)
                        .await
                        .is_err()
                    {
                        observation_failed.store(true, Ordering::Release);
                        return continuity.fail(observation_fault());
                    }
                    return continuity.fail(port_fault(
                        OpenAiFailureClass::RequestTransport,
                        format!("request transport failed: {error:?}"),
                    ));
                }
                Poll::Pending => match transport.await {
                    Ok(response) => response,
                    Err(error) => {
                        accounting.finish(OpenAiResponsesCallOutcome::Failed);
                        if observe_transport_failure(&transport_observer, &error, transport_started)
                            .await
                            .is_err()
                        {
                            observation_failed.store(true, Ordering::Release);
                            return continuity.fail(observation_fault());
                        }
                        return continuity.fail(port_fault(
                            OpenAiFailureClass::RequestTransport,
                            format!("request transport failed: {error:?}"),
                        ));
                    }
                },
            };

            let status = response.status();
            accounting.headers(status.as_u16(), response.headers());
            stream_capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status = Some(status.as_u16());
            if !status.is_success() {
                let possible_cache_miss =
                    cache_continuation && status == reqwest::StatusCode::BAD_REQUEST;
                if capture_enabled || possible_cache_miss || accounting.is_enabled() {
                    capture_error_response(
                        response,
                        &stream_capture,
                        read_timeout,
                        max_response_body_bytes,
                    )
                    .await;
                    accounting.response_body(
                        &stream_capture
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .body,
                    );
                }
                accounting.finish(OpenAiResponsesCallOutcome::Failed);
                if possible_cache_miss {
                    let missing = {
                        let captured = stream_capture
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        serde_json::from_slice::<Value>(&captured.body).is_ok_and(|body| {
                            body.pointer("/error/code").and_then(Value::as_str)
                                == Some("InvalidParameter.PreviousResponseNotFound")
                        })
                    };
                    if missing {
                        // Ark's structured error means the remote prefix was
                        // lost. Full replay can recover the retained local
                        // context; all other admission errors keep their
                        // ordinary terminal classification.
                        return continuity.fail(port_fault(
                            OpenAiFailureClass::UpstreamRetryable,
                            "Ark previous response is no longer available",
                        ));
                    }
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
                if capture_enabled || accounting.is_enabled() {
                    capture_error_response(
                        response,
                        &stream_capture,
                        read_timeout,
                        max_response_body_bytes,
                    )
                    .await;
                    accounting.response_body(
                        &stream_capture
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .body,
                    );
                }
                accounting.finish(OpenAiResponsesCallOutcome::Failed);
                return continuity.fail(port_fault(
                    OpenAiFailureClass::ResponseProtocolRetryable,
                    "successful response did not use text/event-stream",
                ));
            }
            if response.content_length().is_some_and(|content_length| {
                content_length > u64::try_from(max_response_body_bytes).unwrap_or(u64::MAX)
            }) {
                accounting.finish(OpenAiResponsesCallOutcome::Failed);
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
                if capture_enabled {
                    let mut captured = stream_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let remaining = max_response_body_bytes.saturating_sub(captured.body.len());
                    captured
                        .body
                        .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
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
                accounting.finish(OpenAiResponsesCallOutcome::Failed);
                return continuity.fail(port_fault(
                    OpenAiFailureClass::Internal,
                    "native Frame state was unavailable after handoff",
                ));
            };
            let Some(target) = continuity.transfer() else {
                accounting.finish(OpenAiResponsesCallOutcome::Failed);
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
                decision_batch,
                decision_output: None,
                decision_refusals: BTreeSet::new(),
                decision_text_outputs: BTreeSet::new(),
                decision_wire_outputs: BTreeMap::new(),
                output_text_bytes: 0,
                sse_event_timeout,
                max_sse_event_bytes,
                max_output_text_bytes,
                response_usage_observer,
                accounting,
            };
            Box::pin(futures::stream::unfold(state, next_native_fact)) as ProviderFactStream<'a>
        };

        Ok(observe_facts(
            Box::pin(futures::stream::once(pending).flatten()),
            observer,
            capture,
            self.observation_failed.clone(),
        ))
    }

    fn fork_temporary(&mut self) -> Result<Box<dyn ReactionPort>, ReactionPortFault> {
        let mut child = self.fork_snapshot()?;
        match child.reaction_frame.as_mut() {
            Some(state) => state.make_temporary(&child.encoder).map_err(|_| {
                ReactionPortFault::retryable(
                    ReactionPortFaultCode::Rejected,
                    ReactionPortFaultReason::ForkNotReady,
                )
            })?,
            None if child.encoder.ark_caching_enabled() => {
                return Err(ReactionPortFault::retryable(
                    ReactionPortFaultCode::Rejected,
                    ReactionPortFaultReason::ForkNotReady,
                ));
            }
            None => {}
        }
        // Independent scratch must keep its frozen parent window intact.
        child.parallel_compaction_enabled = false;
        child.forced_compaction_enabled = false;
        Ok(Box::new(child))
    }

    async fn shutdown(&mut self) {
        self.parallel_compaction.shutdown().await;
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
        self.parallel_compaction.reset();
        Ok(declaration)
    }
}

#[async_trait]
impl ForkableReactionPort for AsyncOpenAiResponsesProvider {
    async fn fork(&mut self) -> Result<Self, ReactionPortFault> {
        self.fork_snapshot()
    }

    fn validate_fork_frame(&self, frame: &Frame) -> Result<(), ReactionPortFault> {
        let incompatible = || {
            ReactionPortFault::terminal(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::ForkSnapshotIncompatible,
            )
        };
        let declaration = self.frame_native_snapshot_declaration()?;
        if !matches!(frame.basis(), FrameBasis::Full) {
            return Err(incompatible());
        }
        frame
            .check_handoff_precondition(&declaration)
            .map_err(|_| incompatible())?;
        // Prepare only a local candidate: no observation, transport poll,
        // compaction policy evaluation, or worker launch occurs here.
        ResponsesFrameRequestState::prepare_with_measurements(
            self.reaction_frame.as_ref(),
            frame,
            &self.encoder,
            self.max_responses_serialized_request_body_bytes,
            &mut None,
        )
        .map(|_| ())
        .map_err(|error| {
            let fault = port_fault(error.failure_class(), format!("{error:?}"));
            ReactionPortFault::terminal(
                fault.code(),
                ReactionPortFaultReason::ForkSnapshotIncompatible,
            )
        })
    }
}

impl AsyncOpenAiResponsesProvider {
    fn fork_snapshot(&self) -> Result<Self, ReactionPortFault> {
        // Read-only validation keeps ordinary fork failures and cancellation
        // from changing the source declaration or its private continuation.
        self.ensure_frame_native_mode().map_err(|_| {
            ReactionPortFault::terminal(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::ForkSnapshotUnavailable,
            )
        })?;
        let declaration = self.frame_native_snapshot_declaration()?;
        if self.reaction_frame.as_ref().is_some_and(|state| {
            // An unsubmitted fork retains its source's revision under a new
            // target. A same-target FullRequired window instead needs recovery
            // from an interrupted or failed request before it can branch.
            let retained_target =
                TargetDeclaration::resume(state.revision(), declaration.profile().clone())
                    .identity();
            !state.is_forkable()
                || matches!(
                    declaration.continuity(),
                    TargetContinuity::FullRequired { .. }
                ) && retained_target == declaration.identity()
        }) {
            return Err(ReactionPortFault::retryable(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::ForkNotReady,
            ));
        }
        let reaction_target =
            ResponsesReactionTarget::new(declaration.profile().clone()).map_err(|_| {
                ReactionPortFault::terminal(
                    ReactionPortFaultCode::Internal,
                    ReactionPortFaultReason::ForkSnapshotUnavailable,
                )
            })?;
        Ok(Self {
            client: self.client.clone(),
            config: self.config.clone(),
            identity: self.identity.clone(),
            encoder: self.encoder.clone(),
            read_timeout: self.read_timeout,
            sse_event_timeout: self.sse_event_timeout,
            max_response_body_bytes: self.max_response_body_bytes,
            max_sse_event_bytes: self.max_sse_event_bytes,
            max_output_text_bytes: self.max_output_text_bytes,
            max_responses_serialized_request_body_bytes: self
                .max_responses_serialized_request_body_bytes,
            #[cfg(feature = "legacy-provider-port")]
            artifact_binding: None,
            #[cfg(feature = "legacy-provider-port")]
            continuation: None,
            #[cfg(feature = "legacy-provider-port")]
            tool_outputs: Arc::new(Mutex::new(super::ToolOutputStaging::default())),
            response_usage_observer: self.response_usage_observer.clone(),
            request_observer: self.request_observer.clone(),
            call_observer: self.call_observer.clone(),
            observer: self.observer.clone(),
            compaction_observer: self.compaction_observer.clone(),
            observation_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            execution_mode: ResponsesExecutionMode::FrameNative,
            reaction_target,
            reaction_frame: self.reaction_frame.clone(),
            parallel_compaction: self.parallel_compaction.fork(),
            parallel_compaction_enabled: self.parallel_compaction_enabled,
            forced_compaction_enabled: self.forced_compaction_enabled,
        })
    }

    async fn reject_frame_preparation(
        &self,
        frame: &Frame,
        error: ResponsesFrameRequestFault,
        request_body_bytes: Option<usize>,
        compacted_base_used: bool,
    ) -> SubmitFault {
        use OpenAiResponsesRequestPreparationReason as Reason;
        let reason = match &error {
            ResponsesFrameRequestFault::MissingContextWindow => Reason::MissingContextWindow,
            ResponsesFrameRequestFault::Encoding(
                crate::provider::codex_http_v1::CodexHttpV1Error::SerializedRequestBodyLimit,
            ) => Reason::SerializedRequestBodyLimit,
            ResponsesFrameRequestFault::Encoding(_) => Reason::Encoding,
            _ => Reason::InvalidFrameState,
        };
        let fault = port_fault(error.failure_class(), format!("{error:?}"));
        self.observe_preparation_rejection(
            frame,
            fault,
            reason,
            request_body_bytes,
            compacted_base_used,
        )
        .await
    }

    async fn observe_preparation_rejection(
        &self,
        frame: &Frame,
        fault: ReactionPortFault,
        reason: OpenAiResponsesRequestPreparationReason,
        request_body_bytes: Option<usize>,
        compacted_base_used: bool,
    ) -> SubmitFault {
        if let Some(observer) = &self.observer {
            let constraints = &frame.prepared_profile().constraints;
            let diagnostic = OpenAiResponsesRequestPreparationDiagnostic {
                reason,
                request_body_bytes,
                estimated_input_tokens: request_body_bytes
                    .map(|bytes| ContextEstimate::from_bytes(bytes).tokens),
                context_window_tokens: constraints.context_window.tokens().map(NonZeroU64::get),
                reserved_output_tokens: constraints.reserved_output_tokens.unwrap_or(0),
                serialized_request_body_limit_bytes: self
                    .max_responses_serialized_request_body_bytes,
                frame_basis: match frame.basis() {
                    crate::component::execution::reaction::FrameBasis::Full => {
                        OpenAiResponsesFrameBasis::Full
                    }
                    crate::component::execution::reaction::FrameBasis::DeltaFrom(_) => {
                        OpenAiResponsesFrameBasis::Delta
                    }
                },
                compacted_base_used,
                compaction: self.parallel_compaction.status(),
            };
            if observer
                .observe(OpenAiResponsesObservation::RequestPreparationFailed { fault, diagnostic })
                .await
                .is_err()
            {
                self.observation_failed.store(true, Ordering::Release);
            }
        }
        // An unavailable diagnostic must not replace the actual wire/limit
        // rejection that explains why no request was handed off.
        SubmitFault::Rejected(fault)
    }

    fn compaction_transport(
        &self,
        context_window_tokens: NonZeroU64,
        frame: &Frame,
        kind: OpenAiResponsesCallKind,
    ) -> CompactionTransport {
        CompactionTransport {
            client: self.client.clone(),
            config: self.config.clone(),
            encoder: self.encoder.clone(),
            request_limit: self.max_responses_serialized_request_body_bytes,
            response_limit: self.max_response_body_bytes,
            read_timeout: self.read_timeout,
            context_window_tokens,
            observer: self.compaction_observer.clone(),
            accounting: CallAccounting::new(
                &self.call_observer,
                CallOrigin::from_frame(frame),
                kind,
                self.encoder.model().id(),
            ),
        }
    }

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

    fn frame_native_snapshot_declaration(&self) -> Result<TargetDeclaration, ReactionPortFault> {
        if self.observation_failed.load(Ordering::Acquire) {
            return Err(observation_fault());
        }
        self.ensure_frame_native_mode()?;
        let declaration = self.reaction_target.declaration()?;
        validate_native_frame_state(&declaration, self.reaction_frame.as_ref())?;
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

async fn observe_transport_failure(
    observer: &Option<Arc<dyn super::OpenAiResponsesObserver>>,
    error: &reqwest::Error,
    started: std::time::Instant,
) -> Result<(), super::OpenAiResponsesObservationError> {
    if let Some(observer) = observer {
        observer
            .observe(OpenAiResponsesObservation::TransportDiagnostic {
                diagnostic: OpenAiResponsesTransportDiagnostic::request(error, started),
            })
            .await?;
    }
    Ok(())
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
    decision_batch: Option<DecisionBatchId>,
    decision_output: Option<ProviderOutputKey>,
    decision_refusals: BTreeSet<u64>,
    decision_text_outputs: BTreeSet<u64>,
    decision_wire_outputs: BTreeMap<u64, Value>,
    output_text_bytes: usize,
    sse_event_timeout: Option<std::time::Duration>,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    response_usage_observer: Option<Arc<ResponseUsageObserver>>,
    accounting: CallAccounting,
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

        let event =
            match super::transport::next_sse_event(state.stream.next(), state.sse_event_timeout)
                .await
            {
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
    state.accounting.finish(OpenAiResponsesCallOutcome::Failed);
    state.target.record_fault(fault);
    state.fault_recorded = true;
    Some((Err(fault), state))
}

impl<S> NativeOpenAiStreamState<'_, S> {
    fn bind_decision_content_kind(
        &mut self,
        output_index: u64,
        refusal: bool,
    ) -> Result<(), ReactionPortFault> {
        if refusal {
            if self.decision_text_outputs.contains(&output_index) {
                return Err(protocol(
                    "decision output mixed JSON text and model refusal",
                ));
            }
            self.decision_refusals.insert(output_index);
        } else {
            if self.decision_refusals.contains(&output_index) {
                return Err(protocol(
                    "decision output mixed model refusal and JSON text",
                ));
            }
            self.decision_text_outputs.insert(output_index);
        }
        Ok(())
    }

    /// The shared message ledger validates refusal lifecycles through the same
    /// single-text-part rules. Raw HTTP receipts and retained wire output keep
    /// the real refusal representation; only this local validation view changes.
    fn normalize_decision_refusal(
        &mut self,
        frame: &mut OpenAiWireEvent,
    ) -> Result<(), ReactionPortFault> {
        match frame.event_type.as_str() {
            "response.refusal.delta" | "response.refusal.done" => {
                let output_index = output_index(&frame.payload)?;
                self.bind_decision_content_kind(output_index, true)?;
                if frame.event_type == "response.refusal.delta" {
                    frame.event_type = "response.output_text.delta".to_owned();
                } else {
                    let text = frame
                        .payload
                        .remove("refusal")
                        .ok_or_else(|| protocol("refusal.done is missing refusal"))?;
                    frame.payload.insert("text".to_owned(), text);
                    frame.event_type = "response.output_text.done".to_owned();
                }
            }
            "response.output_text.delta" | "response.output_text.done" => {
                self.bind_decision_content_kind(output_index(&frame.payload)?, false)?;
            }
            "response.content_part.added" | "response.content_part.done" => {
                let output_index = output_index(&frame.payload)?;
                let kind = frame
                    .payload
                    .get("part")
                    .and_then(|part| part.get("type"))
                    .and_then(Value::as_str);
                if kind == Some("output_text") {
                    self.bind_decision_content_kind(output_index, false)?;
                }
                if frame
                    .payload
                    .get_mut("part")
                    .is_some_and(normalize_refusal_part)
                {
                    self.bind_decision_content_kind(output_index, true)?;
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let output_index = output_index(&frame.payload)?;
                if let Some(item) = frame.payload.get_mut("item") {
                    let original = item.clone();
                    let has_text =
                        item.get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|content| {
                                content.iter().any(|part| {
                                    part.get("type").and_then(Value::as_str) == Some("output_text")
                                })
                            });
                    if has_text {
                        self.bind_decision_content_kind(output_index, false)?;
                    }
                    if normalize_refusal_message(item) {
                        self.bind_decision_content_kind(output_index, true)?;
                        if frame.event_type == "response.output_item.done" {
                            self.decision_wire_outputs.insert(output_index, original);
                        }
                    }
                }
            }
            "response.completed" => {
                if let Some(output) = frame
                    .payload
                    .get_mut("response")
                    .and_then(|response| response.get_mut("output"))
                    .and_then(Value::as_array_mut)
                {
                    for (output_index, item) in output.iter_mut().enumerate() {
                        let has_text =
                            item.get("content")
                                .and_then(Value::as_array)
                                .is_some_and(|content| {
                                    content.iter().any(|part| {
                                        part.get("type").and_then(Value::as_str)
                                            == Some("output_text")
                                    })
                                });
                        if has_text {
                            self.bind_decision_content_kind(output_index as u64, false)?;
                        }
                        if normalize_refusal_message(item) {
                            self.bind_decision_content_kind(output_index as u64, true)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

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

    fn process_wire_event(&mut self, mut frame: OpenAiWireEvent) -> Result<(), ReactionPortFault> {
        if let Some(response) = frame.payload.get("response").and_then(Value::as_object) {
            self.accounting.response(response);
        }
        self.advance_sequence(&frame.payload)?;
        if self.response_completed {
            return Err(port_fault(
                OpenAiFailureClass::ResponseProtocolViolation,
                "event followed response.completed",
            ));
        }

        if self.decision_batch.is_some() {
            self.normalize_decision_refusal(&mut frame)?;
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
                self.output_ledger
                    .record_reasoning_text(
                        &frame.payload,
                        frame.event_type == "response.reasoning_text.done",
                    )
                    .map_err(protocol_fault)?;
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
                self.accounting
                    .finish(OpenAiResponsesCallOutcome::Incomplete);
                return Err(port_fault(
                    OpenAiFailureClass::ResponseIncomplete,
                    "upstream response reached an incomplete terminal state",
                ));
            }
            "response.failed" | "error" => {
                self.accounting.finish(OpenAiResponsesCallOutcome::Failed);
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
        // Some gateways omit event numbers; retain the last known number across gaps.
        let Some(sequence) = payload.get("sequence_number") else {
            return Ok(());
        };
        let sequence = sequence.as_u64().ok_or_else(|| {
            port_fault(
                OpenAiFailureClass::ResponseProtocolRetryable,
                "event has invalid sequence_number",
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
        if self.decision_batch.is_some() {
            // The complete JSON envelope is a Decision fact; fragments must
            // not acquire independent canonical TextDelta identities.
            return Ok(());
        }
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
        if has_unsupported_lifecycle_item(payload) && native_function_call(payload).is_none() {
            return Err(upstream_rejected("unsupported output item"));
        }
        if native_function_call(payload).is_some() {
            if self.decision_batch.is_some() {
                return Err(upstream_rejected(
                    "decision response requested an undeclared tool",
                ));
            }
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
        if has_unsupported_lifecycle_item(payload) && native_function_call(payload).is_none() {
            return Err(upstream_rejected("unsupported output item"));
        }
        let output_index = output_index(payload)?;
        let mut wire_item = payload
            .get("item")
            .cloned()
            .ok_or_else(|| protocol("output_item.done is missing item"))?;
        if let Some(original) = self.decision_wire_outputs.remove(&output_index) {
            wire_item = original;
        }
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
                    if phase != Some(AssistantPhase::Commentary) {
                        if let Some(batch_id) = &self.decision_batch {
                            if self.decision_output.replace(key).is_some() {
                                return Err(protocol(
                                    "response produced multiple decision outputs",
                                ));
                            }
                            let decision = if self.decision_refusals.contains(&next) {
                                ProviderDecision::refused(batch_id.clone(), text)
                            } else {
                                ProviderDecision::json_envelope(batch_id.clone(), text)
                            };
                            self.emissions.push_back(NativeEmission::Public {
                                output_index: next,
                                canonical_item: CanonicalInputItem::DecisionOutput {
                                    decision: decision.clone(),
                                },
                                wire_item,
                                fact: ProviderFact::Decision {
                                    output: key,
                                    decision,
                                },
                            });
                            self.next_output_index = next.checked_add(1);
                            continue;
                        }
                    }
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
        let response = &payload["response"];
        self.frame_state.record_cached_response(
            response["id"]
                .as_str()
                .expect("completed identity was validated"),
            response.get("expire_at").and_then(Value::as_u64),
        );
        self.response_completed = true;
        self.accounting
            .finish(OpenAiResponsesCallOutcome::Completed);
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

fn normalize_refusal_message(item: &mut Value) -> bool {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return false;
    }
    let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut refusal = false;
    for part in content {
        refusal |= normalize_refusal_part(part);
    }
    refusal
}

fn normalize_refusal_part(part: &mut Value) -> bool {
    if part.get("type").and_then(Value::as_str) != Some("refusal") {
        return false;
    }
    let Some(text) = part.get("refusal").and_then(Value::as_str) else {
        // Preserve invalid data for the ordinary protocol validator.
        return false;
    };
    *part = serde_json::json!({"type": "output_text", "text": text, "annotations": []});
    true
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
    mod call_accounting;
    mod main_ephemeral_cache;
    mod stream_timeouts;
    use std::{
        collections::{BTreeMap, VecDeque},
        num::{NonZeroU128, NonZeroU64},
        sync::{Arc, Mutex},
        task::Poll,
    };

    use crate::component::{
        execution::{Application, ReactionKind, ReactionOutcome},
        prelude::{component, view, Component, Signal},
    };
    use crate::provider::async_openai::{
        OpenAiResponsesObservation, OpenAiResponsesObservationError, OpenAiResponsesObserver,
    };
    use axum::{
        body::Body,
        extract::State,
        http::{header, Response, StatusCode},
        response::IntoResponse,
        routing::post,
        Router,
    };
    use eventsource_stream::EventStreamError;
    use futures::StreamExt;
    use serde_json::{json, Value};
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
            reaction::{
                ForkableReactionPort, Frame, FrameBasis, FrameRevision, FrameSubmission,
                ProjectionSubmission, ProviderFact, ProviderOutputKey, ProviderToolCall,
                ReactionPort, ReactionPortFaultCode, ReactionPortFaultKind,
                ReactionPortFaultReason, ResettableReactionPort, SubmitFault, TargetContinuity,
                TargetDeclaration, TargetEpoch, ToolCatalog,
            },
            ProviderIdentity,
        },
        provider::{
            async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
            codex_http_v1::{
                CodexHttpV1Encoder, CodexHttpV1Options, CodexReasoning, OpenRouterProviderRouting,
            },
        },
    };

    use super::{
        CallAccounting, NativeBodyStreamFault, NativeOpenAiStreamState, OpenAiOutputLedger,
        ReadyOutput, ResponsesFrameRequestState,
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
        let options = CodexHttpV1Options::new(
            crate::provider::ModelSpec::new("test-model", 272_000).unwrap(),
            None,
            None,
            None::<String>,
        )
        .unwrap();
        AsyncOpenAiResponsesProvider::try_new(config, identity, CodexHttpV1Encoder::new(options))
            .unwrap()
    }

    #[derive(Default)]
    struct Audit {
        events: Mutex<Vec<OpenAiResponsesObservation>>,
        reject_request: bool,
        reject_response: bool,
    }

    #[async_trait::async_trait]
    impl OpenAiResponsesObserver for Audit {
        async fn observe(
            &self,
            event: OpenAiResponsesObservation,
        ) -> Result<(), OpenAiResponsesObservationError> {
            let reject = matches!(event, OpenAiResponsesObservation::Request { .. })
                && self.reject_request
                || matches!(event, OpenAiResponsesObservation::Response { .. })
                    && self.reject_response;
            self.events.lock().unwrap().push(event);
            if reject {
                Err(OpenAiResponsesObservationError)
            } else {
                Ok(())
            }
        }
    }

    fn ark_provider(base: &str) -> AsyncOpenAiResponsesProvider {
        let mut provider = provider(base, None).without_parallel_compaction();
        provider.encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("test-model", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap()
            .with_ark_caching(true),
        );
        provider
    }

    fn ark_user_input(text: &str) -> crate::transcript::CanonicalInputItem {
        crate::transcript::CanonicalInputItem::message(
            crate::transcript::ConversationRole::User,
            crate::pom_resolution::resolve_system_document(crate::pom::Document::from_raw_text(
                text,
            )),
        )
    }

    fn ark_frame(
        declaration: &TargetDeclaration,
        sequence: u64,
        replay: Vec<crate::transcript::CanonicalInputItem>,
        staged_inputs: Vec<crate::transcript::CanonicalInputItem>,
        projection: Vec<crate::transcript::CanonicalInputItem>,
        tools: ToolCatalog,
    ) -> Frame {
        let epoch = declaration.continuity().epoch();
        let revision = FrameRevision::new(
            NonZeroU128::new(700).unwrap(),
            declaration.identity(),
            epoch,
            NonZeroU64::new(sequence).unwrap(),
        );
        let basis = match declaration.continuity() {
            TargetContinuity::FullRequired { .. } => FrameBasis::Full,
            TargetContinuity::Accepted { revision, .. } => FrameBasis::DeltaFrom(*revision),
        };
        Frame::from_compiled(
            revision,
            declaration.identity(),
            epoch,
            declaration.continuity().clone(),
            declaration.profile().clone(),
            basis,
            FrameSubmission::from_compiled(
                replay,
                staged_inputs,
                ProjectionSubmission::new(projection),
                tools,
                Vec::new(),
            ),
        )
        .unwrap()
    }

    fn ark_requests(audit: &Audit) -> Vec<Value> {
        audit
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                OpenAiResponsesObservation::Request { body, .. } => {
                    Some(serde_json::from_slice(body).unwrap())
                }
                _ => None,
            })
            .collect()
    }

    fn ark_next_tool_sse() -> String {
        let done = json!({
            "id": "call_item_2",
            "type": "function_call",
            "status": "completed",
            "call_id": "call_2",
            "name": "lookup",
            "arguments": "{\"page\":2}"
        });
        encode_sse([
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"call_item_2","type":"function_call","status":"in_progress","call_id":"call_2","name":"lookup","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"call_item_2","output_index":0,"delta":"{\"page\":2}"}),
            json!({"type":"response.function_call_arguments.done","item_id":"call_item_2","output_index":0,"arguments":"{\"page\":2}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":done.clone()}),
            json!({"type":"response.completed","response":{"id":"resp_second_tool","status":"completed","output":[done]}}),
        ])
    }

    async fn spawn_ark_response_sequence_server(
        replies: Vec<(StatusCode, &'static str, String)>,
    ) -> (
        String,
        Arc<Mutex<Vec<Value>>>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        struct Sequence {
            replies: Mutex<VecDeque<(StatusCode, &'static str, String)>>,
            requests: Arc<Mutex<Vec<Value>>>,
        }

        async fn respond(
            State(sequence): State<Arc<Sequence>>,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            sequence
                .requests
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            let (status, content_type, body) = sequence
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("next response");
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body))
                .unwrap()
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/responses", post(respond))
            .with_state(Arc::new(Sequence {
                replies: Mutex::new(VecDeque::from(replies)),
                requests: requests.clone(),
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://{address}"), requests, shutdown_tx, server)
    }

    #[tokio::test]
    async fn temporary_ark_siblings_and_tool_continuation_keep_the_stored_parent() {
        use crate::transcript::CanonicalInputItem;

        let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(vec![
            (
                StatusCode::OK,
                "text/event-stream",
                ark_compatible_text_sse(),
            ),
            (
                StatusCode::OK,
                "text/event-stream",
                encode_sse(plaintext_reasoning_and_tool_events()),
            ),
            (StatusCode::OK, "text/event-stream", completed_text_sse()),
            (StatusCode::OK, "text/event-stream", completed_text_sse()),
            (StatusCode::OK, "text/event-stream", completed_text_sse()),
        ])
        .await;
        let mut parent = ark_provider(&base);
        assert_eq!(
            parent.fork_temporary().err().unwrap().reason(),
            ReactionPortFaultReason::ForkNotReady
        );
        let tools = ToolCatalog::new(vec!["lookup".into()]).unwrap();
        let first_input = ark_user_input("main input");
        let declaration = parent.declare().unwrap();
        let initial = ark_frame(
            &declaration,
            1,
            Vec::new(),
            Vec::new(),
            vec![first_input.clone()],
            tools.clone(),
        );
        let facts: Vec<_> = parent
            .submit(initial)
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;
        let output = facts
            .iter()
            .find_map(|fact| match fact {
                ProviderFact::TextSealed { text, phase, .. } => {
                    Some(CanonicalInputItem::assistant_text(text.clone(), *phase))
                }
                _ => None,
            })
            .unwrap();
        let accepted = parent.declare().unwrap();
        let parent_revision = parent.reaction_frame.as_ref().unwrap().revision();
        let parent_wire = parent.reaction_frame.as_ref().unwrap().clone();
        let mut checkpoint = parent.fork_temporary().unwrap();
        let mut child = checkpoint.fork_temporary().unwrap();
        let mut sibling = checkpoint.fork_temporary().unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        let child_declaration = child.declare().unwrap();
        let sibling_declaration = sibling.declare().unwrap();
        assert_ne!(child_declaration.identity(), sibling_declaration.identity());
        assert_ne!(child_declaration.identity(), accepted.identity());
        let child_frame = ark_frame(
            &child_declaration,
            1,
            vec![first_input.clone(), output.clone()],
            Vec::new(),
            vec![ark_user_input("temporary input")],
            tools.clone(),
        );
        let child_facts: Vec<_> = child
            .submit(child_frame)
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;
        let call = child_facts
            .iter()
            .find_map(|fact| match fact {
                ProviderFact::ToolCall { call, .. } => Some(
                    CanonicalInputItem::tool_call(
                        call.call_id(),
                        call.name(),
                        call.raw_arguments(),
                    )
                    .unwrap(),
                ),
                _ => None,
            })
            .unwrap();
        let sibling_frame = ark_frame(
            &sibling_declaration,
            1,
            vec![first_input, output.clone()],
            Vec::new(),
            vec![ark_user_input("sibling input")],
            tools.clone(),
        );
        let sibling_facts: Vec<_> = sibling.submit(sibling_frame).await.unwrap().collect().await;
        assert!(sibling_facts.iter().all(Result::is_ok));
        let child_accepted = child.declare().unwrap();
        let child_next = ark_frame(
            &child_accepted,
            2,
            vec![call],
            vec![CanonicalInputItem::tool_result("call_1", "actual temporary result").unwrap()],
            vec![ark_user_input("continue temporary")],
            tools.clone(),
        );
        let continued: Vec<_> = child.submit(child_next).await.unwrap().collect().await;
        assert!(continued.iter().all(Result::is_ok));
        assert_eq!(parent.declare().unwrap(), accepted);
        assert_eq!(
            parent.reaction_frame.as_ref().unwrap().revision(),
            parent_revision
        );
        assert_eq!(
            parent.reaction_frame.as_ref().unwrap().wire_coverage(),
            parent_wire.wire_coverage()
        );
        assert_eq!(
            parent.reaction_frame.as_ref().unwrap().accepted_prefix(),
            parent_wire.accepted_prefix()
        );
        let main = ark_frame(
            &accepted,
            2,
            vec![output],
            Vec::new(),
            vec![ark_user_input("main continuation")],
            tools,
        );
        let continued: Vec<_> = parent.submit(main).await.unwrap().collect().await;
        assert!(continued.iter().all(Result::is_ok));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            for request in &requests[1..4] {
                assert_eq!(request["previous_response_id"], "resp_ark_1");
                assert_eq!(request["store"], false);
                assert!(request.get("caching").is_none());
                assert!(request.get("tools").is_none());
            }
            assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
            assert_eq!(requests[2]["input"].as_array().unwrap().len(), 1);
            let scratch = requests[3]["input"].as_array().unwrap();
            assert_eq!(scratch.len(), 5);
            assert_eq!(scratch[1]["type"], "reasoning");
            assert_eq!(scratch[2]["type"], "function_call");
            assert_eq!(scratch[3]["type"], "function_call_output");
            assert_eq!(scratch[3]["output"], "actual temporary result");
            assert_eq!(requests[4]["previous_response_id"], "resp_ark_1");
            assert_eq!(requests[4]["store"], true);
            assert_eq!(requests[4]["input"].as_array().unwrap().len(), 1);
        }
        child.shutdown().await;
        sibling.shutdown().await;
        checkpoint.shutdown().await;
        parent.shutdown().await;
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[derive(Clone)]
    struct ArkApplicationInput {
        main: String,
        observation: Option<String>,
    }

    #[component]
    fn ark_ephemeral_application(
        signal: Arc<Mutex<Option<Signal<ArkApplicationInput>>>>,
    ) -> Component {
        let input = use_signal(|| ArkApplicationInput {
            main: "main input".into(),
            observation: None,
        });
        *signal.lock().unwrap() = Some(input.clone());
        let input = input.with(Clone::clone).unwrap();
        let observation = input
            .observation
            .map(|text| {
                view! {
                    #[developer(ephemeral, repeat)]
                    { text }
                }
            })
            .unwrap_or_else(|| view! {});
        view! {
            #[system_once]
            { "retained application policy" }
            #[user]
            { input.main }
            { observation }
        }
    }

    fn ark_application_text_sse(response_id: &str, text: &str) -> String {
        let completed = json!({
            "id":"application_message", "type":"message", "status":"completed",
            "role":"assistant", "content":[{"type":"output_text", "text":text}]
        });
        encode_sse([
            json!({"type":"response.created", "response":{"id":response_id}}),
            json!({"type":"response.output_item.added", "output_index":0,
                "item":{"id":"application_message", "type":"message", "status":"in_progress", "role":"assistant", "content":[]}}),
            json!({"type":"response.output_text.delta", "output_index":0, "content_index":0,
                "item_id":"application_message", "delta":text}),
            json!({"type":"response.output_text.done", "output_index":0, "content_index":0,
                "item_id":"application_message", "text":text}),
            json!({"type":"response.output_item.done", "output_index":0, "item":completed.clone()}),
            json!({"type":"response.completed", "response":{"id":response_id, "status":"completed", "output":[completed]}}),
        ])
    }

    #[tokio::test]
    async fn application_local_ark_checkpoint_supports_siblings_continuation_and_reset() {
        let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(
            [
                ("main_1", "first main reply"),
                ("scratch_1", "first scratch reply"),
                ("scratch_2", "sibling scratch reply"),
                ("main_2", "second main reply"),
                ("scratch_continued", "continued scratch reply"),
                ("new_checkpoint", "new checkpoint scratch reply"),
                ("reset_main", "reset main reply"),
                ("reset_scratch", "reset scratch reply"),
            ]
            .map(|(id, text)| {
                (
                    StatusCode::OK,
                    "text/event-stream",
                    ark_application_text_sse(id, text),
                )
            })
            .into(),
        )
        .await;
        let signal = Arc::new(Mutex::new(None));
        let root_signal = signal.clone();
        let mut application = Application::mount(
            move || ark_ephemeral_application(root_signal.clone()),
            ark_provider(&base),
        )
        .unwrap();
        assert!(application.prepare().await.unwrap().is_continue());
        let input = signal.lock().unwrap().clone().unwrap();
        input
            .update(|input| {
                input.observation = Some("initial speech\ninitial judgment".into());
            })
            .unwrap();
        application.react(false).unwrap().await.unwrap();

        input
            .update(|input| input.observation = Some("current first judgment".into()))
            .unwrap();
        let first = application.react(true).unwrap();
        assert_eq!(first.kind(), ReactionKind::Temporary);
        assert_eq!(first.clone().await.unwrap(), ReactionOutcome::Completed);
        input
            .update(|input| input.observation = Some("current sibling judgment".into()))
            .unwrap();
        let sibling = application.react(true).unwrap();
        assert_eq!(sibling.kind(), ReactionKind::Temporary);
        sibling.await.unwrap();

        input
            .set(ArkApplicationInput {
                main: "advanced main input".into(),
                observation: Some("advanced main chronology".into()),
            })
            .unwrap();
        application.react(false).unwrap().await.unwrap();
        input.update(|input| input.observation = None).unwrap();
        first.react().unwrap().await.unwrap();
        application.react(true).unwrap().await.unwrap();
        drop(first);

        application.reset_model_context().unwrap();
        input
            .set(ArkApplicationInput {
                main: "reset main input".into(),
                observation: Some("reset speech one\nreset speech two\nreset judgment".into()),
            })
            .unwrap();
        application.react(false).unwrap().await.unwrap();
        input
            .update(|input| input.observation = Some("current reset judgment".into()))
            .unwrap();
        application.react(true).unwrap().await.unwrap();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 8, "every Temporary must reach transport");
            let wire_input = |index: usize| requests[index]["input"].as_array().unwrap();
            let assistant_texts = |index: usize| -> Vec<&str> {
                wire_input(index)
                    .iter()
                    .filter(|item| item["role"] == "assistant")
                    .flat_map(|item| item["content"].as_array().unwrap())
                    .map(|part| part["text"].as_str().unwrap())
                    .collect()
            };
            for request in requests.iter() {
                assert_eq!(request["store"], false);
                assert!(request.get("caching").is_none());
                assert!(request.get("previous_response_id").is_none());
            }
            assert_eq!(wire_input(0).len(), 2);
            for index in [1, 2] {
                assert_eq!(wire_input(index).len(), 3);
                assert_eq!(assistant_texts(index), ["first main reply"]);
                assert!(!wire_input(index).contains(&wire_input(0)[1]));
            }
            assert_eq!(assistant_texts(3), ["first main reply"]);
            assert_eq!(
                assistant_texts(4),
                ["first main reply", "first scratch reply"]
            );
            assert_eq!(
                assistant_texts(5),
                ["first main reply", "second main reply"]
            );
            for index in [4, 5] {
                assert!(wire_input(index)
                    .iter()
                    .all(|item| item["role"] != "developer"));
                for previous in [0, 1, 2, 3] {
                    assert!(!wire_input(index).contains(wire_input(previous).last().unwrap()));
                }
            }
            assert_eq!(wire_input(6).len(), 2);
            assert!(assistant_texts(6).is_empty());
            assert_eq!(assistant_texts(7), ["reset main reply"]);
            assert!(!wire_input(7).contains(wire_input(6).last().unwrap()));
        }
        application.shutdown().await.unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn application_temporary_over_estimates_preserve_the_main_stored_parent() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(
            [
                ("stored_main", "main reply"),
                ("scratch_1", "temporary output 1"),
                ("scratch_2", "temporary output 2"),
                ("scratch_3", "temporary output 3"),
                ("continued_main", "main continuation reply"),
            ]
            .map(|(id, text)| {
                (
                    StatusCode::OK,
                    "text/event-stream",
                    ark_application_text_sse(id, text),
                )
            })
            .into(),
        )
        .await;
        let policy_calls = Arc::new(AtomicUsize::new(0));
        let observed = policy_calls.clone();
        let mut provider = ark_provider(&base)
            .with_parallel_compaction(move |_: &super::CompactionContext| {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            })
            .with_forced_compaction();
        provider.reaction_target.profile.constraints.context_window =
            crate::component::execution::ContextWindow::Tokens(NonZeroU64::new(1024).unwrap());
        let signal = Arc::new(Mutex::new(None));
        let root_signal = signal.clone();
        let mut application = Application::mount(
            move || ark_ephemeral_application(root_signal.clone()),
            provider,
        )
        .unwrap();
        assert_eq!(
            application.react(false).unwrap().await.unwrap(),
            ReactionOutcome::Completed
        );
        let input = signal.lock().unwrap().clone().unwrap();
        for candidate in 1..=3 {
            input
                .update(|input| {
                    input.observation = Some(format!("ASR candidate {candidate}: {}", "历史记录中保留中文、引号\"和换行\n".repeat(1024)));
                })
                .unwrap();
            let reaction = application.react(true).unwrap();
            assert_eq!(reaction.kind(), ReactionKind::Temporary);
            assert_eq!(reaction.await.unwrap(), ReactionOutcome::Completed);
        }
        assert_eq!(policy_calls.load(Ordering::Relaxed), 0);
        input
            .set(ArkApplicationInput {
                main: "main continuation".into(),
                observation: None,
            })
            .unwrap();
        let main = application.react(false).unwrap();
        assert_eq!(main.kind(), ReactionKind::Main);
        assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            assert_eq!(requests[0]["store"], true);
            for request in &requests[1..4] {
                assert!(serde_json::to_vec(request).unwrap().len().div_ceil(4) > 1024);
                assert_eq!(request["store"], false);
                assert_eq!(request["previous_response_id"], "stored_main");
                assert!(request.get("caching").is_none());
                let input = request["input"].as_array().unwrap();
                assert_eq!(
                    input.len(),
                    1,
                    "siblings must not inherit prior scratch output"
                );
                assert_eq!(input[0]["role"], "developer");
            }
            let continued = &requests[4];
            assert_eq!(continued["store"], true);
            assert_eq!(continued["previous_response_id"], "stored_main");
            let input = continued["input"].as_array().unwrap();
            assert_eq!(
                input.len(),
                1,
                "temporary output must not enter main history"
            );
            assert_eq!(input[0]["role"], "user");
        }
        application.shutdown().await.unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn application_failed_idle_ephemeral_request_preserves_the_main_stored_parent() {
        let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(vec![
            (
                StatusCode::OK,
                "text/event-stream",
                ark_application_text_sse("stored_main", "main reply"),
            ),
            (
                StatusCode::BAD_REQUEST,
                "application/json",
                json!({"error":{"code":"InvalidParameter", "message":"fixture rejection"}})
                    .to_string(),
            ),
            (
                StatusCode::OK,
                "text/event-stream",
                ark_application_text_sse("continued_main", "main continuation reply"),
            ),
        ])
        .await;
        let signal = Arc::new(Mutex::new(None));
        let root_signal = signal.clone();
        let mut application = Application::mount(
            move || ark_ephemeral_application(root_signal.clone()),
            ark_provider(&base),
        )
        .unwrap();
        application.react(false).unwrap().await.unwrap();
        let input = signal.lock().unwrap().clone().unwrap();
        input
            .update(|input| input.observation = Some("ASR candidate".into()))
            .unwrap();
        let temporary = application.react(true).unwrap();
        assert_eq!(temporary.kind(), ReactionKind::Temporary);
        assert!(temporary.await.is_err());
        input
            .set(ArkApplicationInput {
                main: "main continuation".into(),
                observation: None,
            })
            .unwrap();
        let main = application.react(false).unwrap();
        assert_eq!(main.kind(), ReactionKind::Main);
        assert_eq!(main.await.unwrap(), ReactionOutcome::Completed);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1]["store"], false);
            assert_eq!(requests[1]["previous_response_id"], "stored_main");
            assert_eq!(requests[2]["store"], true);
            assert_eq!(requests[2]["previous_response_id"], "stored_main");
            assert_eq!(requests[2]["input"].as_array().unwrap().len(), 1);
            assert_eq!(requests[2]["input"][0]["role"], "user");
        }
        application.shutdown().await.unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ark_caching_missing_remote_response_recovers_with_full_replay_without_observer() {
        use crate::transcript::CanonicalInputItem;

        let missing = json!({
            "error": {
                "code": "InvalidParameter.PreviousResponseNotFound",
                "message": "The previous response is no longer available."
            }
        });
        let (base, requests, shutdown, server) = spawn_ark_response_sequence_server(vec![
            (
                StatusCode::OK,
                "text/event-stream",
                ark_compatible_text_sse(),
            ),
            (
                StatusCode::BAD_REQUEST,
                "application/json",
                missing.to_string(),
            ),
            (StatusCode::OK, "text/event-stream", completed_text_sse()),
        ])
        .await;
        let mut provider = ark_provider(&base);
        assert!(provider.observer.is_none());
        let initial = provider.declare().unwrap();
        let tools = ToolCatalog::new(vec!["lookup".into()]).unwrap();
        let first_input = ark_user_input("Inspect the retained context.");
        let first = ark_frame(
            &initial,
            1,
            Vec::new(),
            Vec::new(),
            vec![first_input.clone()],
            tools.clone(),
        );
        let first: Vec<_> = provider.submit(first).await.unwrap().collect().await;
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            first.last(),
            Some(ProviderFact::ReactionCompleted { .. })
        ));
        let previous_output = first
            .iter()
            .find_map(|fact| match fact {
                ProviderFact::TextSealed { text, phase, .. } => {
                    Some(CanonicalInputItem::assistant_text(text.clone(), *phase))
                }
                _ => None,
            })
            .unwrap();
        let next_input = ark_user_input("Inspect the next action.");
        let accepted = provider.declare().unwrap();
        let next = ark_frame(
            &accepted,
            2,
            vec![previous_output.clone()],
            Vec::new(),
            vec![next_input.clone()],
            tools.clone(),
        );
        let failed: Vec<_> = provider.submit(next).await.unwrap().collect().await;
        let fault = match failed.as_slice() {
            [Err(fault)] => *fault,
            _ => panic!("missing cached response did not yield one fault: {failed:?}"),
        };
        assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable);
        assert_eq!(fault.code(), ReactionPortFaultCode::Unavailable);
        assert_eq!(fault.reason(), ReactionPortFaultReason::UpstreamRejected);
        let recovery = provider.declare().unwrap();
        assert!(matches!(
            recovery.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > accepted.continuity().epoch()
        ));
        assert_eq!(requests.lock().unwrap().len(), 2);

        let next = ark_frame(
            &recovery,
            3,
            vec![first_input, previous_output, next_input],
            Vec::new(),
            Vec::new(),
            tools,
        );
        let recovered: Vec<_> = provider.submit(next).await.unwrap().collect().await;
        let recovered: Vec<_> = recovered.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            recovered.last(),
            Some(ProviderFact::ReactionCompleted {
                primary_text: Some(_)
            })
        ));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1]["previous_response_id"], "resp_ark_1");
            assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
            assert!(requests[2].get("previous_response_id").is_none());
            let replay = requests[2]["input"].as_array().unwrap();
            assert_eq!(replay.len(), 3);
            assert_eq!(replay[0], requests[0]["input"][0]);
            assert_eq!(replay[1]["role"], "assistant");
            assert_eq!(replay[2], requests[1]["input"][0]);
            assert_eq!(requests[2]["tools"][0]["name"], "lookup");
            assert_eq!(requests[2]["store"], true);
            assert_eq!(requests[2]["caching"]["type"], "enabled");
        }
        assert!(matches!(
            provider.declare().unwrap().continuity(),
            TargetContinuity::Accepted { .. }
        ));
        provider.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ark_caching_unrelated_bad_request_and_initial_missing_response_remain_terminal() {
        use crate::transcript::CanonicalInputItem;

        for continue_cached_response in [false, true] {
            let code = if continue_cached_response {
                "InvalidParameter.SomeOtherField"
            } else {
                "InvalidParameter.PreviousResponseNotFound"
            };
            let error = json!({"error":{"code":code}});
            let mut replies = Vec::new();
            if continue_cached_response {
                replies.push((StatusCode::OK, "text/event-stream", completed_text_sse()));
            }
            replies.push((
                StatusCode::BAD_REQUEST,
                "application/json",
                error.to_string(),
            ));
            let (base, requests, shutdown, server) =
                spawn_ark_response_sequence_server(replies).await;
            let mut provider = ark_provider(&base);
            assert!(provider.observer.is_none());
            let initial = provider.declare().unwrap();
            let failing_frame = if continue_cached_response {
                let first: Vec<_> = provider
                    .submit(full_frame(&initial))
                    .await
                    .unwrap()
                    .collect()
                    .await;
                let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
                assert!(matches!(
                    first.last(),
                    Some(ProviderFact::ReactionCompleted { .. })
                ));
                let accepted = provider.declare().unwrap();
                ark_frame(
                    &accepted,
                    2,
                    vec![CanonicalInputItem::assistant_text("hello", None)],
                    Vec::new(),
                    vec![ark_user_input("Continue using the current context.")],
                    ToolCatalog::new(Vec::new()).unwrap(),
                )
            } else {
                full_frame(&initial)
            };
            let failed: Vec<_> = provider
                .submit(failing_frame)
                .await
                .unwrap()
                .collect()
                .await;
            let fault = match failed.as_slice() {
                [Err(fault)] => *fault,
                _ => panic!("bad request did not yield one fault: {failed:?}"),
            };
            assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
            assert_eq!(fault.code(), ReactionPortFaultCode::Rejected);
            assert_eq!(fault.reason(), ReactionPortFaultReason::UpstreamRejected);
            assert_eq!(provider.declare().unwrap_err(), fault);
            {
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), if continue_cached_response { 2 } else { 1 });
                assert_eq!(
                    requests
                        .last()
                        .unwrap()
                        .get("previous_response_id")
                        .is_some(),
                    continue_cached_response
                );
            }
            provider.shutdown().await;
            let _ = shutdown.send(());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn ark_caching_uses_completed_response_id_for_the_next_delta_upload() {
        use crate::transcript::{CanonicalInputItem, InstructionAuthority};

        let (base, shutdown, server) =
            spawn_sse_sequence_server(vec![ark_compatible_text_sse(), completed_text_sse()]).await;
        let audit = Arc::new(Audit::default());
        let mut provider = ark_provider(&base).with_observer(audit.clone());
        let initial = provider.declare().unwrap();
        let system = CanonicalInputItem::instruction(
            InstructionAuthority::System,
            crate::pom_resolution::resolve_system_document(crate::pom::Document::from_raw_text(
                "Use the current state and available actions.",
            )),
        );
        let tools = ToolCatalog::new(vec!["lookup".into()]).unwrap();
        let first = ark_frame(
            &initial,
            1,
            Vec::new(),
            Vec::new(),
            vec![system, ark_user_input("Inspect the current state.")],
            tools.clone(),
        );
        let first: Vec<_> = provider.submit(first).await.unwrap().collect().await;
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            first.last(),
            Some(ProviderFact::ReactionCompleted {
                primary_text: Some(_)
            })
        ));
        let previous_output = first
            .iter()
            .find_map(|fact| match fact {
                ProviderFact::TextSealed { text, phase, .. } => {
                    Some(CanonicalInputItem::assistant_text(text.clone(), *phase))
                }
                _ => None,
            })
            .unwrap();

        let next_input = ark_user_input("Inspect the next available action.");
        let expected_input = provider
            .encoder
            .lower_canonical_item(&next_input)
            .unwrap()
            .into_parts()
            .1;
        let accepted = provider.declare().unwrap();
        let next = ark_frame(
            &accepted,
            2,
            vec![previous_output],
            Vec::new(),
            vec![next_input],
            tools,
        );
        let second: Vec<_> = provider.submit(next).await.unwrap().collect().await;
        let second: Vec<_> = second.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            second.last(),
            Some(ProviderFact::ReactionCompleted {
                primary_text: Some(_)
            })
        ));

        let requests = ark_requests(&audit);
        assert_eq!(requests.len(), 2);
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[0]["input"][0]["role"], "system");
        assert_eq!(requests[0]["input"][1]["role"], "user");
        assert_eq!(requests[0]["tools"][0]["name"], "lookup");
        assert_eq!(requests[1]["previous_response_id"], "resp_ark_1");
        assert_eq!(requests[1]["input"], Value::Array(expected_input));
        for request in &requests {
            assert_eq!(request["store"], true);
            assert_eq!(request["caching"]["type"], "enabled");
            assert!(request.get("instructions").is_none());
        }
        assert!(requests[1].get("tools").is_none());
        provider.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ark_caching_delivers_tool_feedback_then_accepts_the_next_action() {
        use crate::transcript::CanonicalInputItem;

        let (base, shutdown, server) = spawn_sse_sequence_server(vec![
            reasoning_and_tool_sse(),
            ark_next_tool_sse(),
            completed_text_sse(),
        ])
        .await;
        let audit = Arc::new(Audit::default());
        let mut provider = ark_provider(&base).with_observer(audit.clone());
        let tools = ToolCatalog::new(vec!["lookup".into()]).unwrap();
        let initial = provider.declare().unwrap();
        let first = ark_frame(
            &initial,
            1,
            Vec::new(),
            Vec::new(),
            vec![ark_user_input("Read both pages and report the result.")],
            tools.clone(),
        );
        let first: Vec<_> = provider.submit(first).await.unwrap().collect().await;
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        let first_call = match first.as_slice() {
            [ProviderFact::ToolCall { call, .. }, ProviderFact::ReactionCompleted { primary_text: None }] =>
            {
                assert_eq!(call.call_id(), "call_1");
                assert_eq!(call.name(), "lookup");
                call.clone()
            }
            _ => panic!("first response did not expose the first tool action: {first:?}"),
        };
        let first_output = json!({"page": 1, "has_more": true}).to_string();
        let accepted = provider.declare().unwrap();
        let feedback = ark_frame(
            &accepted,
            2,
            vec![CanonicalInputItem::tool_call(
                first_call.call_id(),
                first_call.name(),
                first_call.raw_arguments(),
            )
            .unwrap()],
            vec![CanonicalInputItem::tool_result(first_call.call_id(), &first_output).unwrap()],
            Vec::new(),
            tools.clone(),
        );
        let second: Vec<_> = provider.submit(feedback).await.unwrap().collect().await;
        let second: Vec<_> = second.into_iter().map(Result::unwrap).collect();
        let second_call = match second.as_slice() {
            [ProviderFact::ToolCall { call, .. }, ProviderFact::ReactionCompleted { primary_text: None }] =>
            {
                assert_eq!(call.call_id(), "call_2");
                assert_eq!(call.name(), "lookup");
                assert_eq!(
                    serde_json::from_str::<Value>(call.raw_arguments()).unwrap(),
                    json!({"page": 2})
                );
                call.clone()
            }
            _ => panic!("feedback did not lead to the next tool action: {second:?}"),
        };
        let second_output = json!({"page": 2, "has_more": false}).to_string();
        let accepted = provider.declare().unwrap();
        let feedback = ark_frame(
            &accepted,
            3,
            vec![CanonicalInputItem::tool_call(
                second_call.call_id(),
                second_call.name(),
                second_call.raw_arguments(),
            )
            .unwrap()],
            vec![CanonicalInputItem::tool_result(second_call.call_id(), &second_output).unwrap()],
            Vec::new(),
            tools,
        );
        let third: Vec<_> = provider.submit(feedback).await.unwrap().collect().await;
        let third: Vec<_> = third.into_iter().map(Result::unwrap).collect();
        assert!(third
            .iter()
            .any(|fact| matches!(fact, ProviderFact::TextSealed { .. })));
        assert!(matches!(
            third.last(),
            Some(ProviderFact::ReactionCompleted {
                primary_text: Some(_)
            })
        ));

        let requests = ark_requests(&audit);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["tools"][0]["name"], "lookup");
        for (index, response_id, call_id, output) in [
            (1, "resp_reasoning_tool", "call_1", first_output),
            (2, "resp_second_tool", "call_2", second_output),
        ] {
            assert_eq!(requests[index]["previous_response_id"], response_id);
            assert_eq!(
                requests[index]["input"],
                json!([{"type":"function_call_output","call_id":call_id,"output":output}])
            );
            assert_eq!(requests[index]["store"], true);
            assert_eq!(requests[index]["caching"]["type"], "enabled");
            assert!(requests[index].get("tools").is_none());
            assert!(requests[index].get("instructions").is_none());
        }
        provider.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ark_caching_context_reset_starts_a_new_remote_chain() {
        let (base, shutdown, server) =
            spawn_sse_sequence_server(vec![ark_compatible_text_sse(), completed_text_sse()]).await;
        let audit = Arc::new(Audit::default());
        let mut provider = ark_provider(&base).with_observer(audit.clone());
        let initial = provider.declare().unwrap();
        let tools = ToolCatalog::new(vec!["lookup".into()]).unwrap();
        let first = ark_frame(
            &initial,
            1,
            Vec::new(),
            Vec::new(),
            vec![ark_user_input("Inspect the prior context.")],
            tools.clone(),
        );
        let first: Vec<_> = provider.submit(first).await.unwrap().collect().await;
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            first.last(),
            Some(ProviderFact::ReactionCompleted { .. })
        ));
        let accepted = provider.declare().unwrap();
        let reset = provider.reset_model_context().unwrap();
        assert!(matches!(
            reset.continuity(),
            TargetContinuity::FullRequired { epoch }
                if *epoch > accepted.continuity().epoch()
        ));
        let replacement = ark_user_input("Inspect the replacement context.");
        let expected_input = provider
            .encoder
            .lower_canonical_item(&replacement)
            .unwrap()
            .into_parts()
            .1;
        let next = ark_frame(&reset, 2, Vec::new(), Vec::new(), vec![replacement], tools);
        let second: Vec<_> = provider.submit(next).await.unwrap().collect().await;
        let second: Vec<_> = second.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            second.last(),
            Some(ProviderFact::ReactionCompleted { .. })
        ));
        let requests = ark_requests(&audit);
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["input"], Value::Array(expected_input));
        assert_eq!(requests[1]["tools"][0]["name"], "lookup");
        assert_eq!(requests[1]["store"], true);
        assert_eq!(requests[1]["caching"]["type"], "enabled");
        provider.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "this test verifies Ark caching rejects legacy execution before handoff"
    )]
    #[tokio::test]
    async fn ark_caching_rejects_legacy_before_handoff_and_preserves_native_use() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let audit = Arc::new(Audit::default());
        let mut provider = ark_provider(&base).with_observer(audit.clone());
        let initial = provider.reaction_target.declaration().unwrap();
        let error = match ProviderPort::execute(&mut provider, empty_projection()).await {
            Err(error) => error,
            Ok(_) => panic!("Ark caching was accepted by the legacy provider port"),
        };
        assert_eq!(
            error.code(),
            crate::component::execution::ProviderFaultCode::RequestPreparation
        );
        assert!(audit.events.lock().unwrap().is_empty());
        assert_eq!(
            provider.execution_mode,
            super::ResponsesExecutionMode::Unclaimed
        );
        assert_eq!(provider.declare().unwrap(), initial);
        let facts: Vec<_> = provider
            .submit(full_frame(&initial))
            .await
            .unwrap()
            .collect()
            .await;
        let facts: Vec<_> = facts.into_iter().map(Result::unwrap).collect();
        assert!(matches!(
            facts.last(),
            Some(ProviderFact::ReactionCompleted { .. })
        ));
        assert_eq!(ark_requests(&audit).len(), 1);
        provider.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn raw_observation_preserves_bodies_and_precedes_reaction_completion() {
        let body = completed_text_sse();
        let (base, shutdown, server) = spawn_sse_server(body.clone()).await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        let frame = full_frame(&provider.declare().unwrap());
        let mut facts = provider.submit(frame).await.unwrap();
        while let Some(fact) = facts.next().await {
            if matches!(fact, Ok(ProviderFact::ReactionCompleted { .. })) {
                assert_eq!(audit.events.lock().unwrap().len(), 2);
            }
            assert!(fact.is_ok());
        }
        drop(facts);
        {
            let events = audit.events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[0], OpenAiResponsesObservation::Request { path, body }
                if path == "/responses" && serde_json::from_slice::<serde_json::Value>(body).unwrap()["model"] == "test-model")
            );
            assert!(
                matches!(&events[1], OpenAiResponsesObservation::Response { status: 200, body: captured, completed: true }
                if captured == body.as_bytes())
            );
        }
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn request_observation_failure_rejects_before_transport() {
        let mut provider = provider("http://127.0.0.1:1", None).with_observer(Arc::new(Audit {
            reject_request: true,
            ..Audit::default()
        }));
        let frame = full_frame(&provider.declare().unwrap());
        assert!(matches!(
            provider.submit(frame).await,
            Err(SubmitFault::Rejected(_))
        ));
        assert_eq!(
            provider.declare().unwrap_err().kind(),
            ReactionPortFaultKind::Terminal
        );
    }

    #[tokio::test]
    async fn comparison_hint_can_be_removed_without_changing_response_continuity() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None)
            .with_parallel_tool_calls(true)
            .with_observer(audit.clone());
        provider
            .set_prompt_cache_comparison_response_id(Some("prior_response".into()))
            .unwrap();
        let frame = full_frame(&provider.declare().unwrap());
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert!(facts.iter().all(Result::is_ok));
        let accepted = provider.declare().unwrap();
        provider
            .set_prompt_cache_comparison_response_id(None)
            .unwrap();
        assert_eq!(
            provider.declare().unwrap().continuity(),
            accepted.continuity()
        );
        let frame = full_frame_with_replay(
            &accepted,
            2,
            vec![crate::transcript::CanonicalInputItem::assistant_text(
                "hello", None,
            )],
        );
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert!(facts.iter().all(Result::is_ok));
        {
            let events = audit.events.lock().unwrap();
            let requests: Vec<Value> = events
                .iter()
                .filter_map(|event| match event {
                    OpenAiResponsesObservation::Request { body, .. } => {
                        Some(serde_json::from_slice(body).unwrap())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["parallel_tool_calls"], true);
            assert_eq!(
                requests[0]["prompt_cache_options"]["comparison_response_id"],
                "prior_response"
            );
            assert!(requests[1].get("prompt_cache_options").is_none());
        }
        provider.shutdown().await;
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_http_continuation_preserves_disabled_reasoning_and_endpoint_routing() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let audit = Arc::new(Audit::default());
        let options = CodexHttpV1Options::new(
            crate::provider::ModelSpec::new("test-model", 272_000).unwrap(),
            None,
            Some(CodexReasoning::disabled()),
            None::<String>,
        )
        .unwrap()
        .with_openrouter_provider_routing(
            OpenRouterProviderRouting::new(
                vec!["together".into(), "fireworks/us".into()],
                vec!["together".into(), "fireworks/us".into()],
                true,
            )
            .unwrap(),
        );
        let mut provider = AsyncOpenAiResponsesProvider::new(
            AsyncOpenAiTransportConfig::new(&base, "test-token").unwrap(),
            ProviderIdentity::new("openai", "native-responses", 1, "native-test").unwrap(),
            CodexHttpV1Encoder::new(options),
        )
        .with_observer(audit.clone());
        let frame = full_frame(&provider.declare().unwrap());
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert!(facts.iter().all(Result::is_ok));
        let accepted = provider.declare().unwrap();
        let frame = full_frame_with_replay(
            &accepted,
            2,
            vec![crate::transcript::CanonicalInputItem::assistant_text(
                "hello", None,
            )],
        );
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert!(facts.iter().all(Result::is_ok));
        {
            let events = audit.events.lock().unwrap();
            let requests: Vec<Value> = events
                .iter()
                .filter_map(|event| match event {
                    OpenAiResponsesObservation::Request { body, .. } => {
                        Some(serde_json::from_slice(body).unwrap())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(requests.len(), 2);
            assert!(requests[1]["input"].as_array().unwrap().iter().any(|item| {
                item["role"] == "assistant" && item["content"][0]["text"] == "hello"
            }));
            for request in requests {
                assert_eq!(request["reasoning"], json!({"enabled": false}));
                assert_eq!(
                    request["provider"],
                    json!({
                        "only": ["together", "fireworks/us"],
                        "order": ["together", "fireworks/us"],
                        "require_parameters": true,
                    })
                );
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        OpenAiResponsesObservation::Response {
                            status: 200,
                            completed: true,
                            ..
                        }
                    ))
                    .count(),
                2,
            );
        }
        provider.shutdown().await;
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn comparison_hint_is_included_in_the_serialized_request_gate() {
        let base = "http://127.0.0.1:1";
        let mut baseline = provider(base, None);
        let frame = full_frame(&baseline.declare().unwrap());
        let request =
            ResponsesFrameRequestState::prepare(None, &frame, &baseline.encoder, usize::MAX)
                .unwrap();
        let audit = Arc::new(Audit::default());
        let mut provider =
            provider(base, Some(request.request_body.len())).with_observer(audit.clone());
        provider
            .set_prompt_cache_comparison_response_id(Some("prior_response".into()))
            .unwrap();
        let frame = full_frame(&provider.declare().unwrap());
        assert!(matches!(provider.submit(frame).await,
            Err(SubmitFault::Rejected(fault)) if fault.code() == ReactionPortFaultCode::Limit));
        {
            let events = audit.events.lock().unwrap();
            assert!(
                matches!(events.as_slice(), [OpenAiResponsesObservation::RequestPreparationFailed { diagnostic, .. }]
                if diagnostic.reason == super::OpenAiResponsesRequestPreparationReason::SerializedRequestBodyLimit
                    && diagnostic.request_body_bytes.is_none())
            );
        }
        provider.shutdown().await;
    }

    #[tokio::test]
    async fn failed_connection_emits_safe_transport_diagnostic_after_handoff() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        let frame = full_frame(&provider.declare().unwrap());
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert!(matches!(facts.as_slice(), [Err(fault)]
            if fault.kind() == ReactionPortFaultKind::Retryable
                && fault.reason() == ReactionPortFaultReason::Transport));
        {
            let events = audit.events.lock().unwrap();
            assert!(matches!(events.as_slice(), [
                OpenAiResponsesObservation::Request { .. },
                OpenAiResponsesObservation::TransportDiagnostic { diagnostic },
                OpenAiResponsesObservation::Error { .. },
            ] if diagnostic.phase == super::super::OpenAiResponsesTransportPhase::Request
                && diagnostic.is_connect && !diagnostic.is_timeout));
        }
        provider.shutdown().await;
    }

    #[tokio::test]
    async fn response_observation_failure_keeps_native_output_order_but_blocks_completion() {
        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let mut provider = provider(&base, None).with_observer(Arc::new(Audit {
            reject_response: true,
            ..Audit::default()
        }));
        let frame = full_frame(&provider.declare().unwrap());
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert!(facts
            .iter()
            .any(|fact| matches!(fact, Ok(ProviderFact::TextSealed { .. }))));
        assert!(!facts
            .iter()
            .any(|fact| matches!(fact, Ok(ProviderFact::ReactionCompleted { .. }))));
        assert!(facts.iter().any(Result::is_err));
        assert_eq!(
            provider.declare().unwrap_err().kind(),
            ReactionPortFaultKind::Terminal
        );
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retryable_http_failure_records_raw_body_and_error() {
        let body = r#"{"error":{"message":"busy"}}"#;
        let (base, shutdown, server) = spawn_response_server(
            StatusCode::TOO_MANY_REQUESTS,
            "application/json",
            body.into(),
        )
        .await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        let first = provider.declare().unwrap();
        let facts: Vec<_> = provider
            .submit(full_frame(&first))
            .await
            .unwrap()
            .collect()
            .await;
        assert!(
            matches!(facts.as_slice(), [Err(fault)] if fault.kind() == ReactionPortFaultKind::Retryable)
        );
        assert!(
            provider.declare().unwrap().continuity().epoch().get()
                > first.continuity().epoch().get()
        );
        {
            let events = audit.events.lock().unwrap();
            assert_eq!(events.len(), 3);
            assert!(
                matches!(&events[1], OpenAiResponsesObservation::Response { status: 429, body: captured, completed: false }
                if captured == body.as_bytes())
            );
            assert!(matches!(
                &events[2],
                OpenAiResponsesObservation::Error { .. }
            ));
        }
        shutdown.send(()).unwrap();
        server.await.unwrap();
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

    async fn spawn_sse_sequence_server(
        bodies: Vec<String>,
    ) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        async fn respond(State(bodies): State<Arc<Mutex<VecDeque<String>>>>) -> impl IntoResponse {
            let body = bodies.lock().unwrap().pop_front().expect("next response");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }

        let app = Router::new()
            .route("/responses", post(respond))
            .with_state(Arc::new(Mutex::new(VecDeque::from(bodies))));
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

    fn unnumbered_tool_events() -> Vec<serde_json::Value> {
        let added = json!({
            "id": "call_item_1", "type": "function_call", "status": "in_progress",
            "call_id": "call_1", "name": "read_next", "arguments": ""
        });
        let done = json!({
            "id": "call_item_1", "type": "function_call", "status": "completed",
            "call_id": "call_1", "name": "read_next", "arguments": "{}"
        });
        vec![
            json!({"type":"response.created","response":{"id":"resp_tool","status":"in_progress","output":[]}}),
            json!({"type":"response.in_progress","response":{"id":"resp_tool","status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":added}),
            json!({"type":"response.function_call_arguments.delta","item_id":"call_item_1","output_index":0,"delta":"{}"}),
            json!({"type":"response.function_call_arguments.done","item_id":"call_item_1","output_index":0,"arguments":"{}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":done.clone()}),
            json!({"type":"response.completed","response":{"id":"resp_tool","status":"completed","output":[done]}}),
        ]
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

    fn plaintext_reasoning_and_tool_events() -> Vec<serde_json::Value> {
        let reasoning = json!({
            "id":"rs_plaintext",
            "type":"reasoning",
            "status":"completed",
            "summary":[],
            "format":"unknown",
            "content":[{"type":"reasoning_text","text":"private-reasoning-sentinel"}]
        });
        let tool_done = json!({
            "id":"call_item_1",
            "type":"function_call",
            "status":"completed",
            "call_id":"call_1",
            "name":"lookup",
            "arguments":"{}"
        });
        vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_plaintext","type":"reasoning","status":"in_progress","summary":[],"content":[]}}),
            json!({"type":"response.reasoning_text.delta","item_id":"rs_plaintext","output_index":0,"content_index":0,"delta":"private-reasoning-"}),
            json!({"type":"response.reasoning_text.delta","item_id":"rs_plaintext","output_index":0,"content_index":0,"delta":"sentinel"}),
            json!({"type":"response.reasoning_text.done","item_id":"rs_plaintext","output_index":0,"content_index":0,"text":"private-reasoning-sentinel"}),
            json!({"type":"response.output_item.done","output_index":0,"item":reasoning.clone()}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"call_item_1","type":"function_call","status":"in_progress","call_id":"call_1","name":"lookup","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"call_item_1","output_index":1,"delta":"{}"}),
            json!({"type":"response.function_call_arguments.done","item_id":"call_item_1","output_index":1,"arguments":"{}"}),
            json!({"type":"response.output_item.done","output_index":1,"item":tool_done.clone()}),
            json!({"type":"response.completed","response":{"id":"resp_plaintext_tool","status":"completed","output":[reasoning,tool_done]}}),
        ]
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
    async fn fork_retains_reasoning_and_closed_tools_with_independent_continuations() {
        use crate::transcript::CanonicalInputItem;

        let (base, shutdown, server) = spawn_sse_sequence_server(vec![
            encode_sse(plaintext_reasoning_and_tool_events()),
            completed_text_sse(),
            completed_text_sse(),
            completed_text_sse(),
        ])
        .await;
        let audit = Arc::new(Audit::default());
        let mut parent = provider(&base, None)
            .with_parallel_compaction(|_: &super::CompactionContext| false)
            .with_observer(audit.clone());
        let initial = parent.declare().unwrap();
        let facts = parent
            .submit(full_frame(&initial))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(facts.into_iter().all(|fact| fact.is_ok()));

        let pending = parent.declare().unwrap();
        assert_eq!(
            parent.fork().await.err().unwrap().reason(),
            ReactionPortFaultReason::ForkNotReady
        );
        assert_eq!(parent.declare().unwrap(), pending);

        let mut history = vec![
            CanonicalInputItem::tool_call("call_1", "lookup", "{}").unwrap(),
            CanonicalInputItem::tool_result("call_1", "lookup succeeded").unwrap(),
        ];
        let facts = parent
            .submit(full_frame_with_replay(&pending, 2, history.clone()))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(facts.into_iter().all(|fact| fact.is_ok()));
        history.push(CanonicalInputItem::assistant_text("hello", None));

        let accepted = parent.declare().unwrap();
        let source_revision = parent.reaction_frame.as_ref().unwrap().revision();
        let observation_count = audit.events.lock().unwrap().len();
        let mut child = parent.fork().await.unwrap();
        let forked = child.declare().unwrap();
        assert_ne!(forked.identity(), accepted.identity());
        assert_eq!(forked.profile(), accepted.profile());
        assert!(matches!(
            forked.continuity(),
            TargetContinuity::FullRequired { .. }
        ));
        assert_eq!(parent.declare().unwrap(), accepted);
        assert_eq!(
            child.reaction_frame.as_ref().unwrap().revision(),
            source_revision
        );
        assert!(!Arc::ptr_eq(
            &parent.observation_failed,
            &child.observation_failed
        ));

        let first_child = full_frame_with_replay(&forked, 1, history.clone());
        child.validate_fork_frame(&first_child).unwrap();
        let mut grandchild = child.fork().await.unwrap();
        let grandchild_declaration = grandchild.declare().unwrap();
        assert_ne!(grandchild_declaration.identity(), forked.identity());
        grandchild
            .validate_fork_frame(&full_frame_with_replay(
                &grandchild_declaration,
                1,
                history.clone(),
            ))
            .unwrap();
        grandchild.shutdown().await;
        let prepared = ResponsesFrameRequestState::prepare(
            child.reaction_frame.as_ref(),
            &first_child,
            &child.encoder,
            child.max_responses_serialized_request_body_bytes,
        )
        .unwrap();
        let request: Value = serde_json::from_slice(&prepared.request_body).unwrap();
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(
            input[0]["content"],
            json!([{
                "type":"reasoning_text", "text":"private-reasoning-sentinel"
            }])
        );
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "lookup succeeded");
        assert_eq!(input[3]["content"][0]["text"], "hello");

        let incompatible = full_frame_with_replay(
            &forked,
            1,
            vec![CanonicalInputItem::assistant_text("wrong prefix", None)],
        );
        assert_eq!(
            child
                .validate_fork_frame(&incompatible)
                .unwrap_err()
                .reason(),
            ReactionPortFaultReason::ForkSnapshotIncompatible
        );
        assert_eq!(audit.events.lock().unwrap().len(), observation_count);
        assert_eq!(child.declare().unwrap(), forked);
        assert!(!child.parallel_compaction.has_attempt());

        let mut parent_history = history.clone();
        parent_history.push(CanonicalInputItem::assistant_text(
            "parent continuation",
            None,
        ));
        history.push(CanonicalInputItem::assistant_text(
            "child continuation",
            None,
        ));
        let parent_frame = full_frame_with_replay(&accepted, 3, parent_history);
        let child_frame = full_frame_with_replay(&forked, 1, history);
        let parent_revision = parent_frame.revision();
        let child_revision = child_frame.revision();
        let (parent_facts, child_facts) = tokio::join!(
            async {
                parent
                    .submit(parent_frame)
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            },
            async {
                child
                    .submit(child_frame)
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            },
        );
        assert!(parent_facts.into_iter().all(|fact| fact.is_ok()));
        assert!(child_facts.into_iter().all(|fact| fact.is_ok()));
        let requests: Vec<Value> = audit
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                OpenAiResponsesObservation::Request { body, .. } => {
                    Some(serde_json::from_slice(body).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 4);
        let branch_inputs: Vec<_> = requests[2..]
            .iter()
            .map(|request| {
                request["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert!(branch_inputs.contains(&"parent continuation"));
        assert!(branch_inputs.contains(&"child continuation"));
        assert_eq!(
            parent.reaction_frame.as_ref().unwrap().revision(),
            parent_revision
        );
        assert_eq!(
            child.reaction_frame.as_ref().unwrap().revision(),
            child_revision
        );
        assert_ne!(parent_revision, child_revision);

        let parent_after = parent.declare().unwrap();
        child
            .observation_failed
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(child.declare().is_err());
        child.shutdown().await;
        assert_eq!(parent.declare().unwrap(), parent_after);
        parent.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fork_preflight_preserves_installed_compaction_and_rejects_system_changes() {
        use crate::{
            pom::Document,
            pom_resolution::resolve_system_document,
            transcript::{CanonicalInputItem, InstructionAuthority},
        };

        let (base, shutdown, server) = spawn_sse_server(compacted_text_sse()).await;
        let audit = Arc::new(Audit::default());
        let mut parent = provider(&base, None).with_observer(audit.clone());
        let initial = parent.declare().unwrap();
        let facts = parent
            .submit(full_frame(&initial))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(facts.into_iter().all(|fact| fact.is_ok()));
        let source = parent.declare().unwrap();
        let mut child = parent.fork().await.unwrap();
        let forked = child.declare().unwrap();
        let frame = full_frame_with_replay(
            &forked,
            1,
            vec![CanonicalInputItem::assistant_text("answer", None)],
        );
        let observation_count = audit.events.lock().unwrap().len();
        child.validate_fork_frame(&frame).unwrap();
        let prepared = ResponsesFrameRequestState::prepare(
            child.reaction_frame.as_ref(),
            &frame,
            &child.encoder,
            usize::MAX,
        )
        .unwrap();
        let request: Value = serde_json::from_slice(&prepared.request_body).unwrap();
        assert_eq!(request["input"][0]["type"], "compaction");
        assert_eq!(
            request["input"][0]["encrypted_content"],
            "opaque-compaction-secret"
        );

        let system = CanonicalInputItem::instruction(
            InstructionAuthority::System,
            resolve_system_document(Document::from_raw_text("changed policy")),
        );
        let changed = Frame::from_compiled(
            frame.revision(),
            frame.target(),
            frame.epoch(),
            frame.prepared_against().clone(),
            frame.prepared_profile().clone(),
            FrameBasis::Full,
            FrameSubmission::from_compiled(
                frame.submission().replay().to_vec(),
                Vec::new(),
                ProjectionSubmission::new(vec![system]),
                ToolCatalog::new(Vec::new()).unwrap(),
                Vec::new(),
            ),
        )
        .unwrap();
        assert_eq!(
            child.validate_fork_frame(&changed).unwrap_err().reason(),
            ReactionPortFaultReason::ForkSnapshotIncompatible
        );
        assert_eq!(audit.events.lock().unwrap().len(), observation_count);
        assert_eq!(parent.declare().unwrap(), source);
        assert_eq!(child.declare().unwrap(), forked);
        assert!(child.reaction_frame.as_ref().unwrap().is_compacted());
        parent.shutdown().await;
        child.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fork_rejects_unsettled_or_terminal_sessions_without_changing_the_source() {
        let mut parent = provider("http://127.0.0.1:1", None);
        let initial = parent.declare().unwrap();
        let stream = parent.submit(full_frame(&initial)).await.unwrap();
        drop(stream);
        let unsettled = parent.declare().unwrap();
        assert_eq!(
            parent.fork().await.err().unwrap().reason(),
            ReactionPortFaultReason::ForkNotReady
        );
        assert_eq!(parent.declare().unwrap(), unsettled);

        let terminal = super::port_fault(
            super::OpenAiFailureClass::Authentication,
            "private diagnostic",
        );
        parent.reaction_target.record_fault(terminal);
        assert_eq!(parent.fork().await.err().unwrap(), terminal);
        assert_eq!(parent.declare().unwrap_err(), terminal);
        parent.shutdown().await;
    }

    #[tokio::test]
    async fn fork_preflight_checks_wire_budgets_without_observation_or_compaction() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let evaluations = Arc::new(AtomicUsize::new(0));
        let observed = evaluations.clone();
        let audit = Arc::new(Audit::default());
        let mut parent = provider("http://127.0.0.1:1", Some(1))
            .with_parallel_compaction(move |_: &super::CompactionContext| {
                observed.fetch_add(1, Ordering::Relaxed);
                true
            })
            .with_forced_compaction()
            .with_observer(audit.clone());
        let source = parent.declare().unwrap();
        let mut child = parent.fork().await.unwrap();
        let forked = child.declare().unwrap();
        let fault = child.validate_fork_frame(&full_frame(&forked)).unwrap_err();
        assert_eq!(fault.code(), ReactionPortFaultCode::Limit);
        assert_eq!(
            fault.reason(),
            ReactionPortFaultReason::ForkSnapshotIncompatible
        );
        assert!(audit.events.lock().unwrap().is_empty());
        assert_eq!(evaluations.load(Ordering::Relaxed), 0);
        assert!(child.reaction_frame.is_none());
        assert!(!child.parallel_compaction.has_attempt());
        assert_eq!(child.declare().unwrap(), forked);
        assert_eq!(parent.declare().unwrap(), source);
        child.shutdown().await;
        parent.shutdown().await;
    }

    #[tokio::test]
    async fn fork_keeps_settled_interrupted_history_with_explicit_assistant_status() {
        use crate::transcript::CanonicalInputItem;

        let (base, shutdown, server) = spawn_sse_server(completed_text_sse()).await;
        let mut parent = provider(&base, None);
        parent.encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("test-model", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap()
            .with_explicit_assistant_status(),
        );
        let initial = parent.declare().unwrap();
        let interrupted = CanonicalInputItem::interrupted_assistant_text("prior partial", None);
        let facts = parent
            .submit(full_frame_with_replay(
                &initial,
                1,
                vec![interrupted.clone()],
            ))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(facts.into_iter().all(|fact| fact.is_ok()));
        let mut child = parent.fork().await.unwrap();
        let forked = child.declare().unwrap();
        let frame = full_frame_with_replay(
            &forked,
            1,
            vec![
                interrupted,
                CanonicalInputItem::assistant_text("hello", None),
            ],
        );
        child.validate_fork_frame(&frame).unwrap();
        let prepared = ResponsesFrameRequestState::prepare(
            child.reaction_frame.as_ref(),
            &frame,
            &child.encoder,
            usize::MAX,
        )
        .unwrap();
        let request: Value = serde_json::from_slice(&prepared.request_body).unwrap();
        assert_eq!(request["input"][0]["status"], "incomplete");
        assert_eq!(request["input"][0]["content"][0]["text"], "prior partial");
        assert_eq!(request["input"][1]["role"], "user");
        assert_eq!(request["input"][2]["status"], "completed");
        child.shutdown().await;
        parent.shutdown().await;
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[cfg(feature = "legacy-provider-port")]
    #[tokio::test]
    async fn fork_rejects_legacy_mode_without_changing_the_source() {
        let mut parent = provider("http://127.0.0.1:1", None);
        parent.execution_mode = super::ResponsesExecutionMode::Legacy;
        let source = parent.reaction_target.declaration().unwrap();
        assert_eq!(
            parent.fork().await.err().unwrap().reason(),
            ReactionPortFaultReason::ForkSnapshotUnavailable
        );
        assert_eq!(parent.execution_mode, super::ResponsesExecutionMode::Legacy);
        assert_eq!(parent.reaction_target.declaration().unwrap(), source);
        assert!(parent.reaction_frame.is_none());
        parent.shutdown().await;
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
        let request: Value = serde_json::from_slice(&prepared.request_body).unwrap();
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "replacement-context");

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
        let request: Value = serde_json::from_slice(&prepared.request_body).unwrap();
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "replacement-context");

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
        let request: Value = serde_json::from_slice(&prepared.request_body).unwrap();
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "replacement-context");

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
    async fn tool_stream_accepts_missing_sequence_numbers_and_preserves_continuity() {
        for sequences in [
            [None; 7],
            [None, None, None, None, None, None, Some(0)],
            [Some(0), None, Some(1), None, Some(10), None, Some(u64::MAX)],
        ] {
            let mut events = unnumbered_tool_events();
            for (event, sequence) in events.iter_mut().zip(sequences) {
                if let Some(sequence) = sequence {
                    event["sequence_number"] = json!(sequence);
                }
            }
            let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
            let mut provider = provider(&base, None);
            let declaration = ReactionPort::declare(&mut provider).unwrap();
            let frame = full_frame(&declaration);
            let revision = frame.revision();
            let mut stream = ReactionPort::submit(&mut provider, frame).await.unwrap();
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
                        call: ProviderToolCall::new("call_1", "read_next", "{}").unwrap(),
                    },
                    ProviderFact::ReactionCompleted { primary_text: None },
                ],
                "sequences: {sequences:?}"
            );
            assert!(matches!(
                ReactionPort::declare(&mut provider).unwrap().continuity(),
                TargetContinuity::Accepted { revision: accepted, .. } if *accepted == revision
            ));

            let _ = shutdown.send(());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tool_stream_rejects_invalid_present_sequence_numbers() {
        for sequence in [
            json!(null),
            json!("1"),
            json!(-1),
            json!(1.5),
            json!(true),
            json!({}),
            json!([]),
            serde_json::from_str("18446744073709551616").unwrap(),
        ] {
            let mut events = unnumbered_tool_events();
            events[0]["sequence_number"] = sequence.clone();
            let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
            let mut provider = provider(&base, None);
            let declaration = ReactionPort::declare(&mut provider).unwrap();
            let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
                .await
                .unwrap();
            let fault = stream.next().await.unwrap().unwrap_err();
            assert_eq!(fault.kind(), ReactionPortFaultKind::Retryable, "{sequence}");
            assert_eq!(fault.code(), ReactionPortFaultCode::Protocol, "{sequence}");
            assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
            assert!(stream.next().await.is_none());
            drop(stream);
            assert!(matches!(
                ReactionPort::declare(&mut provider).unwrap().continuity(),
                TargetContinuity::FullRequired { epoch }
                    if *epoch > declaration.continuity().epoch()
            ));

            let _ = shutdown.send(());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tool_stream_rejects_non_increasing_sequences_across_unnumbered_events() {
        for sequence in [9, 10] {
            let mut events = unnumbered_tool_events();
            events[0]["sequence_number"] = json!(10);
            events[2]["sequence_number"] = json!(sequence);
            let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
            let mut provider = provider(&base, None);
            let declaration = ReactionPort::declare(&mut provider).unwrap();
            let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
                .await
                .unwrap();
            let fault = stream.next().await.unwrap().unwrap_err();
            assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
            assert_eq!(fault.code(), ReactionPortFaultCode::Protocol);
            assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
            assert!(stream.next().await.is_none());
            drop(stream);

            let _ = shutdown.send(());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn unnumbered_tool_stream_still_requires_matching_lifecycle_and_completion() {
        let mut missing_arguments_done = unnumbered_tool_events();
        missing_arguments_done.remove(4);
        let mut mismatched_completion = unnumbered_tool_events();
        mismatched_completion[6]["response"]["output"][0]["arguments"] = json!("{\"key\":1}");

        for events in [missing_arguments_done, mismatched_completion] {
            let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
            let mut provider = provider(&base, None);
            let declaration = ReactionPort::declare(&mut provider).unwrap();
            let mut stream = ReactionPort::submit(&mut provider, full_frame(&declaration))
                .await
                .unwrap();
            let fault = loop {
                match stream
                    .next()
                    .await
                    .expect("invalid tool stream ended without a fault")
                {
                    Ok(fact) => assert!(matches!(fact, ProviderFact::ToolCall { .. })),
                    Err(fault) => break fault,
                }
            };
            assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
            assert_eq!(fault.code(), ReactionPortFaultCode::Protocol);
            assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
            assert!(stream.next().await.is_none());
            drop(stream);

            let _ = shutdown.send(());
            server.await.unwrap();
        }
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
    async fn plaintext_reasoning_remains_private_and_replays_before_tool_feedback() {
        let (base, shutdown, server) = spawn_sse_sequence_server(vec![
            encode_sse(plaintext_reasoning_and_tool_events()),
            completed_text_sse(),
        ])
        .await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let first: Vec<_> = ReactionPort::submit(&mut provider, full_frame(&initial))
            .await
            .unwrap()
            .collect()
            .await;
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            first,
            vec![
                ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(1),
                    ordinal: 1,
                    call: ProviderToolCall::new("call_1", "lookup", "{}").unwrap(),
                },
                ProviderFact::ReactionCompleted { primary_text: None },
            ]
        );

        let accepted = ReactionPort::declare(&mut provider).unwrap();
        let feedback = full_frame_with_replay(
            &accepted,
            2,
            vec![
                crate::transcript::CanonicalInputItem::tool_call("call_1", "lookup", "{}").unwrap(),
                crate::transcript::CanonicalInputItem::tool_result("call_1", "lookup succeeded")
                    .unwrap(),
            ],
        );
        let second: Vec<_> = ReactionPort::submit(&mut provider, feedback)
            .await
            .unwrap()
            .collect()
            .await;
        let second: Vec<_> = second.into_iter().map(Result::unwrap).collect();
        assert!(second.iter().any(|fact| {
            matches!(fact, ProviderFact::TextSealed { text, .. } if text == "hello")
        }));
        assert!(matches!(
            second.last(),
            Some(ProviderFact::ReactionCompleted {
                primary_text: Some(_)
            })
        ));

        let requests: Vec<serde_json::Value> = {
            let observations = audit.events.lock().unwrap();
            observations
                .iter()
                .filter_map(|event| match event {
                    OpenAiResponsesObservation::Request { body, .. } => {
                        Some(serde_json::from_slice(body).unwrap())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(requests.len(), 2);
        let input = requests[1]["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["format"], "unknown");
        assert_eq!(
            input[0]["content"],
            json!([{"type":"reasoning_text","text":"private-reasoning-sentinel"}])
        );
        assert!(input[0].get("encrypted_content").is_none());
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "lookup succeeded");
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    fn summary_only_reasoning_and_text_events() -> Vec<serde_json::Value> {
        let reasoning = json!({
            "id":"rs_summary", "type":"reasoning", "status":"completed",
            "summary":[{"type":"summary_text", "text":"private-summary-sentinel"}]
        });
        let message = json!({
            "id":"msg_after_summary", "type":"message", "status":"completed", "role":"assistant",
            "content":[{"type":"output_text", "text":"public answer", "annotations":[]}]
        });
        vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_summary","type":"reasoning","status":"in_progress","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_summary","output_index":0,"summary_index":0,"delta":"private-summary-sentinel"}),
            json!({"type":"response.output_item.done","output_index":0,"item":reasoning.clone()}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_after_summary","type":"message","status":"in_progress","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_after_summary","output_index":1,"content_index":0,"delta":"public answer"}),
            json!({"type":"response.output_text.done","item_id":"msg_after_summary","output_index":1,"content_index":0,"text":"public answer"}),
            json!({"type":"response.output_item.done","output_index":1,"item":message.clone()}),
            json!({"type":"response.completed","response":{"id":"resp_summary","status":"completed","output":[reasoning,message]}}),
        ]
    }

    #[tokio::test]
    async fn summary_only_reasoning_remains_private_and_replays_before_public_text() {
        let events = summary_only_reasoning_and_text_events();
        let expected_reasoning = events[2]["item"].clone();
        let (base, shutdown, server) =
            spawn_sse_sequence_server(vec![encode_sse(events), completed_text_sse()]).await;
        let audit = Arc::new(Audit::default());
        let mut provider = provider(&base, None).with_observer(audit.clone());
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let first: Vec<_> = ReactionPort::submit(&mut provider, full_frame(&initial))
            .await
            .unwrap()
            .collect()
            .await;
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            first,
            vec![
                ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(1),
                    phase: None,
                    delta: "public answer".to_owned(),
                },
                ProviderFact::TextSealed {
                    output: ProviderOutputKey::new(1),
                    phase: None,
                    text: "public answer".to_owned(),
                },
                ProviderFact::ReactionCompleted {
                    primary_text: Some(ProviderOutputKey::new(1))
                },
            ]
        );

        let accepted = ReactionPort::declare(&mut provider).unwrap();
        let next = full_frame_with_replay(
            &accepted,
            2,
            vec![crate::transcript::CanonicalInputItem::assistant_text(
                "public answer",
                None,
            )],
        );
        let second: Vec<_> = ReactionPort::submit(&mut provider, next)
            .await
            .unwrap()
            .collect()
            .await;
        assert!(second.iter().all(Result::is_ok));
        assert!(matches!(
            second.last(),
            Some(Ok(ProviderFact::ReactionCompleted { .. }))
        ));
        let requests: Vec<serde_json::Value> = audit
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                OpenAiResponsesObservation::Request { body, .. } => {
                    Some(serde_json::from_slice(body).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 2);
        let input = requests[1]["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0], expected_reasoning);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "public answer");
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn summary_only_reasoning_rejects_terminal_body_drift_without_leaking_summary() {
        let mut events = summary_only_reasoning_and_text_events();
        events[7]["response"]["output"][0]["summary"][0]["text"] =
            json!("changed-private-summary-sentinel");
        let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
        let mut provider = provider(&base, None);
        let initial = ReactionPort::declare(&mut provider).unwrap();
        let facts: Vec<_> = ReactionPort::submit(&mut provider, full_frame(&initial))
            .await
            .unwrap()
            .collect()
            .await;
        let fault = facts.iter().find_map(|fact| fact.as_ref().err()).unwrap();
        assert_eq!(fault.code(), ReactionPortFaultCode::Protocol);
        assert_eq!(fault.reason(), ReactionPortFaultReason::ResponseProtocol);
        assert!(!facts
            .iter()
            .any(|fact| matches!(fact, Ok(ProviderFact::ReactionCompleted { .. }))));
        assert!(!format!("{facts:?}").contains("summary-sentinel"));
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn plaintext_reasoning_content_parts_require_a_matching_completed_lifecycle() {
        let mut complete = plaintext_reasoning_and_tool_events();
        complete.insert(
            4,
            json!({
                "type":"response.content_part.done",
                "item_id":"rs_plaintext","output_index":0,"content_index":0,
                "part":{"type":"reasoning_text","text":"private-reasoning-sentinel"}
            }),
        );
        complete.insert(
            1,
            json!({
                "type":"response.content_part.added",
                "item_id":"rs_plaintext","output_index":0,"content_index":0,
                "part":{"type":"reasoning_text","text":""}
            }),
        );
        let mut mismatch = complete.clone();
        mismatch[5]["part"]["text"] = json!("different-private-reasoning");
        let mut unfinished = complete.clone();
        unfinished.remove(5);

        for (events, accepted) in [(complete, true), (mismatch, false), (unfinished, false)] {
            let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
            let mut provider = provider(&base, None);
            let declaration = ReactionPort::declare(&mut provider).unwrap();
            let facts: Vec<_> = ReactionPort::submit(&mut provider, full_frame(&declaration))
                .await
                .unwrap()
                .collect()
                .await;
            if accepted {
                assert!(matches!(
                    facts.as_slice(),
                    [
                        Ok(ProviderFact::ToolCall { .. }),
                        Ok(ProviderFact::ReactionCompleted { primary_text: None }),
                    ]
                ));
            } else {
                assert!(matches!(facts.as_slice(), [Err(fault)]
                    if fault.kind() == ReactionPortFaultKind::Terminal
                        && fault.code() == ReactionPortFaultCode::Protocol));
            }
            let _ = shutdown.send(());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn plaintext_reasoning_rejects_stream_identity_and_body_mismatches() {
        let mut wrong_item = plaintext_reasoning_and_tool_events();
        wrong_item[1]["item_id"] = json!("different-reasoning-item");
        let mut wrong_output = plaintext_reasoning_and_tool_events();
        wrong_output[1]["output_index"] = json!(1);
        let mut wrong_done = plaintext_reasoning_and_tool_events();
        wrong_done[3]["text"] = json!("different-private-reasoning");
        let mut wrong_seal = plaintext_reasoning_and_tool_events();
        wrong_seal[4]["item"]["content"][0]["text"] = json!("different-private-reasoning");
        let mut wrong_terminal = plaintext_reasoning_and_tool_events();
        wrong_terminal[9]["response"]["output"][0]["content"][0]["text"] =
            json!("different-private-reasoning");
        let mut after_done = plaintext_reasoning_and_tool_events();
        after_done.insert(4, after_done[1].clone());

        for (name, events) in [
            ("item identity", wrong_item),
            ("output identity", wrong_output),
            ("text done", wrong_done),
            ("item seal", wrong_seal),
            ("terminal body", wrong_terminal),
            ("delta after text done", after_done),
        ] {
            let (base, shutdown, server) = spawn_sse_server(encode_sse(events)).await;
            let mut provider = provider(&base, None);
            let declaration = ReactionPort::declare(&mut provider).unwrap();
            let facts: Vec<_> = ReactionPort::submit(&mut provider, full_frame(&declaration))
                .await
                .unwrap()
                .collect()
                .await;
            let fault = facts
                .iter()
                .find_map(|fact| fact.as_ref().err())
                .unwrap_or_else(|| panic!("{name} was accepted"));
            assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal, "{name}");
            assert_eq!(fault.code(), ReactionPortFaultCode::Protocol, "{name}");
            assert_eq!(
                fault.reason(),
                ReactionPortFaultReason::ResponseProtocol,
                "{name}"
            );
            assert!(facts
                .iter()
                .all(|fact| { matches!(fact, Ok(ProviderFact::ToolCall { .. }) | Err(_)) }));
            assert!(!format!("{fault:?}").contains("private-reasoning"));

            let _ = shutdown.send(());
            server.await.unwrap();
        }
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
            decision_batch: None,
            decision_output: None,
            decision_refusals: std::collections::BTreeSet::new(),
            decision_text_outputs: std::collections::BTreeSet::new(),
            decision_wire_outputs: BTreeMap::new(),
            output_text_bytes: 0,
            sse_event_timeout: Some(std::time::Duration::from_secs(1)),
            max_sse_event_bytes: 1024 * 1024,
            max_output_text_bytes: 1024 * 1024,
            response_usage_observer: None,
            accounting: CallAccounting::default(),
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
