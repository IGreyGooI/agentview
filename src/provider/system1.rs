//! Native System One judgments over a single JSON HTTP request.
//!
//! OpenRouter Decisions and TypeSafe-compatible System One use typed questions
//! and answers. They do not use the Responses SSE or tool-call protocols.

use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    task::Poll,
};

use ::async_openai::config::Config;
use async_trait::async_trait;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use url::Url;

use crate::{
    component::execution::{
        reaction::{FrameBasis, FrameCapabilities, FrameProfile, ProviderFact, ProviderOutputKey},
        DecisionBatchId, ForkableReactionPort, Frame, ProviderDecision, ProviderFactStream,
        ProviderIdentity, ReactionPort, ReactionPortFault, ReactionPortFaultCode,
        ReactionPortFaultReason, ResettableReactionPort, SubmitFault, TargetDeclaration,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};

use super::{
    async_openai::{
        call_observation::{CallAccounting, CallOrigin},
        reaction_fault::{map_openai_fault, OpenAiFailureClass, OpenAiReactionFailure},
        request_observation::{observe_request, OpenAiResponsesRequestSnapshot},
        usage::{observe_response_usage, validated_decision_usage},
        AsyncOpenAiConfigError, AsyncOpenAiTransportConfig, OpenAiResponsesCallEvent,
        OpenAiResponsesCallKind, OpenAiResponsesCallOutcome, OpenAiResponsesFrameBasis,
        OpenAiResponsesObservation, OpenAiResponsesObserver,
        OpenAiResponsesRequestPreparationDiagnostic, OpenAiResponsesRequestPreparationReason,
        OpenAiResponsesTransportDiagnostic, OpenAiResponsesTransportPhase, OpenAiResponsesUsage,
        ParallelCompactionPhase, ParallelCompactionStatus, ResponsesReactionTarget,
        SharedHttpTransport,
    },
    codex_http_v1::OpenRouterProviderRouting,
    ModelSpec,
};

/// Native HTTP protocol to use. Both endpoints have the same question/answer body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum System1Endpoint {
    /// OpenRouter's origin-relative `/api/alpha/decisions` endpoint.
    #[default]
    Decisions,
    /// TypeSafe-compatible `/v1/systemone`, relative to the configured API root.
    /// An API base already ending in `/v1` is used without adding another `/v1`.
    SystemOne,
}

/// Request metadata understood by OpenRouter's trace grouping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct System1Trace {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_name: Option<String>,
}

/// Native System One model and request options. Sampling and Responses cache
/// fields are intentionally absent because neither native endpoint defines them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct System1Options {
    model: ModelSpec,
    endpoint: System1Endpoint,
    ark_beta: bool,
    raw_authorization: bool,
    provider: Option<OpenRouterProviderRouting>,
    session_id: Option<String>,
    trace: Option<System1Trace>,
    user: Option<String>,
}

impl System1Options {
    pub fn new(model: ModelSpec) -> Self {
        Self {
            model,
            endpoint: System1Endpoint::Decisions,
            ark_beta: false,
            raw_authorization: false,
            provider: None,
            session_id: None,
            trace: None,
            user: None,
        }
    }

    pub const fn model(&self) -> &ModelSpec {
        &self.model
    }

    pub const fn endpoint(&self) -> System1Endpoint {
        self.endpoint
    }

    pub fn with_endpoint(mut self, endpoint: System1Endpoint) -> Self {
        self.endpoint = endpoint;
        self
    }

    /// Enables Ark's `ark-beta-systemone-api: true` request header.
    ///
    /// This does not change the endpoint. Select [`System1Endpoint::SystemOne`]
    /// and use Ark's `/api` or `/api/v1` API base when configuring its beta API.
    pub fn with_ark_beta(mut self) -> Self {
        self.ark_beta = true;
        self
    }

    /// Sends the API key directly in `Authorization`, without a Bearer prefix.
    ///
    /// This affects native System One requests only. Bearer authorization
    /// remains the default for both supported endpoint variants.
    pub fn with_raw_authorization(mut self) -> Self {
        self.raw_authorization = true;
        self
    }

    pub fn with_openrouter_provider_routing(mut self, routing: OpenRouterProviderRouting) -> Self {
        self.provider = Some(routing);
        self
    }

    pub fn with_session_id(mut self, id: impl Into<String>) -> Result<Self, System1ConfigError> {
        self.session_id = Some(bounded_identity("session_id", id.into())?);
        Ok(self)
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Result<Self, System1ConfigError> {
        self.user = Some(bounded_identity("user", user.into())?);
        Ok(self)
    }

    pub fn with_trace(mut self, trace: System1Trace) -> Self {
        self.trace = Some(trace);
        self
    }
}

fn bounded_identity(name: &'static str, value: String) -> Result<String, System1ConfigError> {
    if value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(System1ConfigError::InvalidIdentity { name });
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum System1ConfigError {
    #[error(transparent)]
    Transport(#[from] AsyncOpenAiConfigError),
    #[error("System One Frame profile is invalid")]
    InvalidFrameProfile,
    #[error("System One endpoint configuration is invalid")]
    InvalidEndpoint,
    #[error("System One {name} must be nonempty, at most 256 characters, and contain no control characters")]
    InvalidIdentity { name: &'static str },
}

/// A decision-only provider. Every request contains the complete Full Frame;
/// no remote response cursor, conversation cache or SSE decoder is involved.
pub struct System1Provider {
    http: SharedHttpTransport,
    identity: ProviderIdentity,
    options: System1Options,
    profile: FrameProfile,
    target: ResponsesReactionTarget,
}

impl System1Provider {
    pub fn try_new(
        transport: AsyncOpenAiTransportConfig,
        identity: ProviderIdentity,
        options: System1Options,
    ) -> Result<Self, System1ConfigError> {
        let http = SharedHttpTransport::initialize(transport, options.model.clone())?;
        Self::from_shared(http, identity, options)
    }

    fn from_shared(
        http: SharedHttpTransport,
        identity: ProviderIdentity,
        options: System1Options,
    ) -> Result<Self, System1ConfigError> {
        endpoint_url(&http, options.endpoint)?;
        let profile = FrameProfile::new(
            http.constraints.clone(),
            FrameCapabilities::new(false).with_decision(true),
        );
        profile
            .validate()
            .map_err(|_| System1ConfigError::InvalidFrameProfile)?;
        let target = ResponsesReactionTarget::new(profile.clone())?;
        Ok(Self {
            http,
            identity,
            options,
            profile,
            target,
        })
    }

    pub fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    pub fn options(&self) -> &System1Options {
        &self.options
    }

    pub fn fresh_session(&self) -> Result<Self, System1ConfigError> {
        Self::from_shared(
            self.http.clone(),
            self.identity.clone(),
            self.options.clone(),
        )
    }

    pub fn with_request_observer(
        mut self,
        observer: impl Fn(OpenAiResponsesRequestSnapshot) + Send + Sync + 'static,
    ) -> Self {
        self.http.request_observer = Some(Arc::new(observer));
        self
    }

    pub fn with_call_observer(
        mut self,
        observer: impl Fn(OpenAiResponsesCallEvent) + Send + Sync + 'static,
    ) -> Self {
        self.http.call_observer = Some(Arc::new(observer));
        self
    }

    pub fn with_response_usage_observer(
        mut self,
        observer: impl Fn(OpenAiResponsesUsage) + Send + Sync + 'static,
    ) -> Self {
        self.http.response_usage_observer = Some(Arc::new(observer));
        self
    }

    pub fn with_observer(mut self, observer: Arc<dyn OpenAiResponsesObserver>) -> Self {
        self.http.observer = Some(observer);
        self
    }

    fn request_headers(&self) -> Result<reqwest::header::HeaderMap, ()> {
        use reqwest::header::{HeaderValue, AUTHORIZATION};

        let mut headers = self.http.config.headers();
        if self.options.raw_authorization {
            let key = headers
                .get(AUTHORIZATION)
                .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
                .ok_or(())?;
            let value = HeaderValue::from_bytes(key).map_err(|_| ())?;
            headers.insert(AUTHORIZATION, value);
        }
        headers
            .get_mut(AUTHORIZATION)
            .ok_or(())?
            .set_sensitive(true);
        Ok(headers)
    }

    fn prepare(&self, frame: &Frame) -> Result<PreparedDecision, PreparationFailure> {
        let invalid = || {
            PreparationFailure::new(
                OpenAiFailureClass::RequestPreparation,
                OpenAiResponsesRequestPreparationReason::InvalidFrameState,
                None,
            )
        };
        if frame.basis() != FrameBasis::Full || !frame.submission().tools().definitions().is_empty()
        {
            return Err(invalid());
        }
        let declaration = frame.submission().decision().ok_or_else(invalid)?;
        declaration.validate().map_err(|_| invalid())?;
        let questions = declaration.questions();
        let state = json!({
            "replay": lower_items(frame.submission().replay(), declaration.batch_id()).map_err(|_| invalid())?,
            "staged_inputs": lower_items(frame.submission().staged_inputs(), declaration.batch_id()).map_err(|_| invalid())?,
            "projection": lower_items(frame.submission().projection().items(), declaration.batch_id()).map_err(|_| invalid())?,
            "ephemeral": lower_items(frame.submission().ephemeral(), declaration.batch_id()).map_err(|_| invalid())?,
        });
        let mut body =
            json!({"model":self.options.model.id(), "state":state, "questions":questions});
        let object = body.as_object_mut().expect("request is an object");
        if let Some(provider) = &self.options.provider {
            object.insert(
                "provider".into(),
                serde_json::to_value(provider).map_err(|_| invalid())?,
            );
        }
        if let Some(session_id) = &self.options.session_id {
            object.insert("session_id".into(), json!(session_id));
        }
        if let Some(trace) = &self.options.trace {
            object.insert(
                "trace".into(),
                serde_json::to_value(trace).map_err(|_| invalid())?,
            );
        }
        if let Some(user) = &self.options.user {
            object.insert("user".into(), json!(user));
        }
        let body = serde_json::to_vec(&body).map_err(|_| invalid())?;
        let bytes = body.len();
        if bytes > self.http.max_serialized_request_body_bytes {
            return Err(PreparationFailure::new(
                OpenAiFailureClass::RequestBodyLimit,
                OpenAiResponsesRequestPreparationReason::SerializedRequestBodyLimit,
                Some(bytes),
            ));
        }
        frame
            .prepared_profile()
            .constraints
            .context_window
            .tokens()
            .ok_or_else(|| {
                PreparationFailure::new(
                    OpenAiFailureClass::RequestPreparation,
                    OpenAiResponsesRequestPreparationReason::MissingContextWindow,
                    Some(bytes),
                )
            })?;
        Ok(PreparedDecision {
            body,
            batch_id: declaration.batch_id().clone(),
        })
    }

    async fn reject(&mut self, frame: &Frame, failure: PreparationFailure) -> SubmitFault {
        let fault = failure.fault;
        if let Some(observer) = &self.http.observer {
            let diagnostic = OpenAiResponsesRequestPreparationDiagnostic {
                reason: failure.reason,
                request_body_bytes: failure.bytes,
                estimated_input_tokens: failure
                    .bytes
                    .and_then(|bytes| u64::try_from(bytes.div_ceil(4)).ok()),
                context_window_tokens: self
                    .profile
                    .constraints
                    .context_window
                    .tokens()
                    .map(|tokens| tokens.get()),
                reserved_output_tokens: self
                    .profile
                    .constraints
                    .reserved_output_tokens
                    .unwrap_or(0),
                serialized_request_body_limit_bytes: self.http.max_serialized_request_body_bytes,
                frame_basis: if frame.basis() == FrameBasis::Full {
                    OpenAiResponsesFrameBasis::Full
                } else {
                    OpenAiResponsesFrameBasis::Delta
                },
                compacted_base_used: false,
                compaction: ParallelCompactionStatus {
                    attempt: 0,
                    phase: ParallelCompactionPhase::Idle,
                    fault: None,
                },
            };
            if observer
                .observe(OpenAiResponsesObservation::RequestPreparationFailed { fault, diagnostic })
                .await
                .is_err()
            {
                self.target.record_fault(observation_fault());
            }
        }
        SubmitFault::Rejected(fault)
    }
}

fn endpoint_url(
    http: &SharedHttpTransport,
    endpoint: System1Endpoint,
) -> Result<Url, System1ConfigError> {
    let mut url =
        Url::parse(&http.config.url("")).map_err(|_| System1ConfigError::InvalidEndpoint)?;
    url.set_query(None);
    url.set_fragment(None);
    match endpoint {
        System1Endpoint::Decisions => url.set_path("/api/alpha/decisions"),
        System1Endpoint::SystemOne => {
            let base = url.path().trim_end_matches('/');
            let path = if base.ends_with("/v1") {
                format!("{base}/systemone")
            } else {
                format!("{base}/v1/systemone")
            };
            url.set_path(&path);
        }
    }
    Ok(url)
}

fn lower_items(
    items: &[CanonicalInputItem],
    active_batch: &DecisionBatchId,
) -> Result<Vec<Value>, ()> {
    items
        .iter()
        // The active catalog travels once at the request root. Earlier catalogs
        // remain evidence for interpreting earlier answers after a question's
        // name, instructions or criteria are reused by the application.
        .filter(|item| !matches!(item, CanonicalInputItem::DecisionRequest { declaration } if declaration.batch_id() == active_batch))
        .map(|item| {
            if matches!(item, CanonicalInputItem::ProviderExtension(_)) {
                return Err(());
            }
            if let CanonicalInputItem::DecisionRequest { declaration } = item {
                return Ok(json!({
                    "kind": "decision_request",
                    "payload": {"questions": declaration.questions()},
                }));
            }
            let mut value = serde_json::to_value(item).map_err(|_| ())?;
            if let CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } = item
            {
                value["payload"]["pom"] = Value::String(render_pom_document(pom).map_err(|_| ())?);
            }
            Ok(value)
        })
        .collect()
}

struct PreparedDecision {
    body: Vec<u8>,
    batch_id: DecisionBatchId,
}

struct PreparationFailure {
    fault: ReactionPortFault,
    reason: OpenAiResponsesRequestPreparationReason,
    bytes: Option<usize>,
}

impl PreparationFailure {
    fn new(
        class: OpenAiFailureClass,
        reason: OpenAiResponsesRequestPreparationReason,
        bytes: Option<usize>,
    ) -> Self {
        let fault = if reason == OpenAiResponsesRequestPreparationReason::InvalidFrameState {
            ReactionPortFault::retryable(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::RequestPreparation,
            )
        } else {
            native_fault(class, "System One request preparation failed")
        };
        Self {
            fault,
            reason,
            bytes,
        }
    }
}

#[async_trait]
impl ReactionPort for System1Provider {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.target.declaration()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(
            &self.target.declaration().map_err(SubmitFault::Rejected)?,
        )?;
        let prepared = match self.prepare(&frame) {
            Ok(prepared) => prepared,
            Err(failure) => return Err(self.reject(&frame, failure).await),
        };
        let url = endpoint_url(&self.http, self.options.endpoint).map_err(|_| {
            SubmitFault::Rejected(native_fault(
                OpenAiFailureClass::RequestPreparation,
                "System One endpoint is invalid",
            ))
        })?;
        let request = self.request_headers().and_then(|headers| {
            let request = self
                .http
                .client
                .post(url)
                .headers(headers)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(prepared.body.clone());
            let request = if self.options.ark_beta {
                request.header("ark-beta-systemone-api", "true")
            } else {
                request
            };
            request.build().map_err(|_| ())
        });
        let request = match request {
            Ok(request) => request,
            Err(_) => {
                return Err(self
                    .reject(
                        &frame,
                        PreparationFailure::new(
                            OpenAiFailureClass::RequestPreparation,
                            OpenAiResponsesRequestPreparationReason::RequestBuild,
                            Some(prepared.body.len()),
                        ),
                    )
                    .await)
            }
        };
        if let Some(observer) = &self.http.observer {
            if observer
                .observe(OpenAiResponsesObservation::Request {
                    path: request.url().path().to_owned(),
                    body: prepared.body.clone(),
                })
                .await
                .is_err()
            {
                let fault = observation_fault();
                self.target.record_fault(fault);
                return Err(SubmitFault::Rejected(fault));
            }
        }
        let request_snapshot = self
            .http
            .request_observer
            .as_ref()
            .map(|_| OpenAiResponsesRequestSnapshot::new(&frame, &prepared.body));
        let mut accounting = CallAccounting::new(
            &self.http.call_observer,
            &None,
            CallOrigin::from_frame(&frame),
            OpenAiResponsesCallKind::Foreground,
            self.options.model.id(),
        );
        let client = self.http.client.clone();
        let mut transport = Box::pin(client.execute(request));
        let started = std::time::Instant::now();
        let first = futures::future::poll_fn(|cx| {
            frame.check_handoff_precondition(
                &self.target.declaration().map_err(SubmitFault::Rejected)?,
            )?;
            accounting.start();
            let first = transport.as_mut().poll(cx);
            if let Poll::Ready(Ok(response)) = &first {
                accounting.headers(response.status().as_u16(), response.headers());
            }
            self.target.accept(frame.revision());
            Poll::Ready(Ok::<_, SubmitFault>(first))
        })
        .await?;
        observe_request(&self.http.request_observer, request_snapshot);
        let http = self.http.clone();
        let batch_id = prepared.batch_id;
        let pending = async move {
            let response = match first {
                Poll::Ready(response) => response,
                Poll::Pending => transport.await,
            };
            let result = complete_request(response, &http, &mut accounting, started).await;
            match result {
                Ok(raw) => {
                    accounting.finish(OpenAiResponsesCallOutcome::Completed);
                    Ok(ProviderDecision::native_answers(batch_id, raw))
                }
                Err(fault) => {
                    accounting.finish(OpenAiResponsesCallOutcome::Failed);
                    if let Some(observer) = &http.observer {
                        if observer
                            .observe(OpenAiResponsesObservation::Error { fault })
                            .await
                            .is_err()
                        {
                            return Err(observation_fault());
                        }
                    }
                    Err(fault)
                }
            }
        }
        .boxed();
        let state = NativeFactState {
            pending: Some(pending),
            phase: 0,
            continuity: NativeContinuity {
                target: Some(&mut self.target),
            },
        };
        Ok(Box::pin(futures::stream::unfold(
            state,
            |mut state| async move {
                if let Some(pending) = state.pending.take() {
                    let fact = match pending.await {
                        Ok(decision) => {
                            state.phase = 1;
                            Ok(ProviderFact::Decision {
                                output: ProviderOutputKey::new(0),
                                decision,
                            })
                        }
                        Err(fault) => {
                            state.phase = 2;
                            state.continuity.fail(fault);
                            Err(fault)
                        }
                    };
                    return Some((fact, state));
                }
                if state.phase == 1 {
                    state.phase = 2;
                    return Some((
                        Ok(ProviderFact::ReactionCompleted { primary_text: None }),
                        state,
                    ));
                }
                state.continuity.complete();
                None
            },
        )))
    }

    fn fork_temporary(&mut self) -> Result<Box<dyn ReactionPort>, ReactionPortFault> {
        self.target.declaration()?;
        self.fresh_session()
            .map(|provider| Box::new(provider) as Box<dyn ReactionPort>)
            .map_err(|_| {
                native_fault(
                    OpenAiFailureClass::Internal,
                    "System One target allocation failed",
                )
            })
    }
}

#[async_trait]
impl ForkableReactionPort for System1Provider {
    async fn fork(&mut self) -> Result<Self, ReactionPortFault> {
        self.target.declaration()?;
        self.fresh_session().map_err(|_| {
            native_fault(
                OpenAiFailureClass::Internal,
                "System One target allocation failed",
            )
        })
    }

    fn validate_fork_frame(&self, frame: &Frame) -> Result<(), ReactionPortFault> {
        frame
            .check_handoff_precondition(&self.target.declaration()?)
            .map_err(|_| {
                ReactionPortFault::terminal(
                    ReactionPortFaultCode::Rejected,
                    ReactionPortFaultReason::ForkSnapshotIncompatible,
                )
            })?;
        self.prepare(frame)
            .map(|_| ())
            .map_err(|failure| failure.fault)
    }
}

impl ResettableReactionPort for System1Provider {
    fn reset_model_context(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.target.declaration()?;
        self.target.lose_continuity();
        self.target.declaration()
    }
}

struct NativeFactState<'a> {
    pending: Option<BoxFuture<'a, Result<ProviderDecision, ReactionPortFault>>>,
    phase: u8,
    continuity: NativeContinuity<'a>,
}

struct NativeContinuity<'a> {
    target: Option<&'a mut ResponsesReactionTarget>,
}

impl NativeContinuity<'_> {
    fn fail(&mut self, fault: ReactionPortFault) {
        if let Some(target) = self.target.take() {
            target.record_fault(fault);
        }
    }
    fn complete(&mut self) {
        self.target = None;
    }
}

impl Drop for NativeContinuity<'_> {
    fn drop(&mut self) {
        if let Some(target) = self.target.take() {
            target.lose_continuity();
        }
    }
}

async fn complete_request(
    response: Result<reqwest::Response, reqwest::Error>,
    http: &SharedHttpTransport,
    accounting: &mut CallAccounting,
    started: std::time::Instant,
) -> Result<String, ReactionPortFault> {
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            if let Some(observer) = &http.observer {
                let diagnostic = OpenAiResponsesTransportDiagnostic {
                    phase: OpenAiResponsesTransportPhase::Request,
                    is_connect: error.is_connect(),
                    is_timeout: error.is_timeout(),
                    elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                };
                observer
                    .observe(OpenAiResponsesObservation::TransportDiagnostic { diagnostic })
                    .await
                    .map_err(|_| observation_fault())?;
            }
            return Err(native_fault(
                OpenAiFailureClass::RequestTransport,
                "System One HTTP transport failed",
            ));
        }
    };
    let status = response.status();
    accounting.headers(status.as_u16(), response.headers());
    let json_content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    let (body, read_result) = read_body(response, http).await;
    accounting.response_body(&body);
    let result = match read_result {
        Err(fault) => Err(fault),
        Ok(()) if !status.is_success() => Err(map_openai_fault(
            &OpenAiReactionFailure::http_status(status),
        )),
        Ok(()) if !json_content_type => Err(native_fault(
            OpenAiFailureClass::ResponseProtocolRetryable,
            "System One expected application/json",
        )),
        Ok(()) => validate_response(&body, http),
    };
    if let Some(observer) = &http.observer {
        observer
            .observe(OpenAiResponsesObservation::Response {
                status: status.as_u16(),
                body,
                completed: result.is_ok(),
            })
            .await
            .map_err(|_| observation_fault())?;
    }
    result
}

fn validate_response(body: &[u8], http: &SharedHttpTransport) -> Result<String, ReactionPortFault> {
    let invalid = || {
        native_fault(
            OpenAiFailureClass::ResponseProtocolViolation,
            "System One response envelope is invalid",
        )
    };
    let response: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let object = response.as_object().ok_or_else(invalid)?;
    object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let usage = validated_decision_usage(object).map_err(|_| invalid())?;
    let _ = catch_unwind(AssertUnwindSafe(|| {
        observe_response_usage(&http.response_usage_observer, usage)
    }));
    // Answer schema and question correspondence belong to shared admission.
    String::from_utf8(body.to_vec()).map_err(|_| invalid())
}

async fn read_body(
    response: reqwest::Response,
    http: &SharedHttpTransport,
) -> (Vec<u8>, Result<(), ReactionPortFault>) {
    let limit = http.max_response_body_bytes;
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX))
    {
        return (
            Vec::new(),
            Err(native_fault(
                OpenAiFailureClass::ResponseBodyLimit,
                "System One response exceeds its byte limit",
            )),
        );
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(http.read_timeout, stream.next()).await {
            Err(_) => {
                return (
                    body,
                    Err(native_fault(
                        OpenAiFailureClass::StreamTimeout,
                        "System One response read timed out",
                    )),
                )
            }
            Ok(None) => return (body, Ok(())),
            Ok(Some(Err(_))) => {
                return (
                    body,
                    Err(native_fault(
                        OpenAiFailureClass::StreamTransport,
                        "System One response read failed",
                    )),
                )
            }
            Ok(Some(Ok(chunk))) => {
                let remaining = limit.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if chunk.len() > remaining {
                    return (
                        body,
                        Err(native_fault(
                            OpenAiFailureClass::ResponseBodyLimit,
                            "System One response exceeds its byte limit",
                        )),
                    );
                }
            }
        }
    }
}

fn native_fault(class: OpenAiFailureClass, diagnostic: &'static str) -> ReactionPortFault {
    map_openai_fault(&OpenAiReactionFailure::static_diagnostic(class, diagnostic))
}

fn observation_fault() -> ReactionPortFault {
    ReactionPortFault::terminal(
        ReactionPortFaultCode::Internal,
        ReactionPortFaultReason::Other,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        num::{NonZeroU128, NonZeroU64},
        sync::Mutex,
    };

    use axum::{routing::post, Json, Router};
    use tokio::net::TcpListener;

    use crate::{
        component::execution::{
            reaction::{
                FrameRevision, FrameSubmission, ProjectionSubmission, TargetContinuity, ToolCatalog,
            },
            DecisionDeclaration, DecisionQuestion,
        },
        transcript::ProviderExtension,
    };

    use super::*;

    fn config(base: &str) -> AsyncOpenAiTransportConfig {
        AsyncOpenAiTransportConfig::new(base, "test-key").unwrap()
    }

    fn identity() -> ProviderIdentity {
        ProviderIdentity::new("test", "system1", 1, "native").unwrap()
    }

    fn provider(base: &str) -> System1Provider {
        System1Provider::try_new(
            config(base),
            identity(),
            System1Options::new(ModelSpec::new("typesafe/jev-test", 32_768).unwrap()),
        )
        .unwrap()
    }

    #[test]
    fn native_authorization_headers_redact_keys_in_debug_for_both_styles() {
        for raw in [false, true] {
            let mut port = provider("http://127.0.0.1:1/api/v1");
            if raw {
                port.options = port.options.with_raw_authorization();
            }
            let headers = port.request_headers().unwrap();
            let authorization = headers.get(reqwest::header::AUTHORIZATION).unwrap();
            assert_eq!(
                authorization.to_str().unwrap(),
                if raw { "test-key" } else { "Bearer test-key" }
            );
            assert!(authorization.is_sensitive());
            assert!(!format!("{headers:?}").contains("test-key"));
        }
    }

    fn frame(port: &mut System1Provider) -> Frame {
        full_frame(
            port,
            1,
            DecisionDeclaration::new(BTreeMap::from([(
                "gate".into(),
                DecisionQuestion::noul("Is the gate ready?"),
            )]))
            .unwrap(),
            Vec::new(),
        )
    }

    fn full_frame(
        port: &mut System1Provider,
        sequence: u64,
        decision: DecisionDeclaration,
        replay: Vec<CanonicalInputItem>,
    ) -> Frame {
        let target = port.declare().unwrap();
        let epoch = target.continuity().epoch();
        let revision = FrameRevision::new(
            NonZeroU128::new(71).unwrap(),
            target.identity(),
            epoch,
            NonZeroU64::new(sequence).unwrap(),
        );
        let submission = FrameSubmission::from_compiled(
            replay,
            Vec::new(),
            ProjectionSubmission::new(vec![CanonicalInputItem::DecisionRequest {
                declaration: decision.clone(),
            }]),
            ToolCatalog::new(Vec::new()).unwrap(),
            Vec::new(),
        )
        .with_decision(Some(decision));
        Frame::from_compiled(
            revision,
            target.identity(),
            epoch,
            target.continuity().clone(),
            target.profile().clone(),
            FrameBasis::Full,
            submission,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn consecutive_full_frames_keep_previous_question_meaning_outside_the_active_catalog() {
        let observed = Arc::new(Mutex::new(Vec::<Value>::new()));
        let capture = observed.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let app = Router::new().route("/api/alpha/decisions", post(move |Json(body): Json<Value>| {
            let choice = body["questions"]["team"]["criteria"].as_object().unwrap().keys().next().unwrap().clone();
            capture.lock().unwrap().push(body);
            async move {
                Json(json!({
                    "model":"typesafe/jev-test", "answers":{"team":{"type":"choice","choice":choice}},
                    "usage":{"input_tokens":10,"output_tokens":2}
                }))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut port = provider(&base);
        let old = DecisionDeclaration::new(BTreeMap::from([(
            "team".to_owned(),
            DecisionQuestion::choice(
                "Who owns this ticket?",
                BTreeMap::from([("payments".to_owned(), json!("Payments and billing"))]),
            ),
        )]))
        .unwrap();
        let first = full_frame(&mut port, 1, old.clone(), Vec::new());
        let mut facts = port.submit(first).await.unwrap();
        let ProviderFact::Decision { decision, .. } = facts.next().await.unwrap().unwrap() else {
            panic!("expected decision")
        };
        assert!(matches!(
            facts.next().await,
            Some(Ok(ProviderFact::ReactionCompleted { .. }))
        ));
        assert!(facts.next().await.is_none());
        drop(facts);
        let current = DecisionDeclaration::new(BTreeMap::from([(
            "team".to_owned(),
            DecisionQuestion::choice(
                "Which region owns this ticket?",
                BTreeMap::from([("west".to_owned(), json!("Western region"))]),
            ),
        )]))
        .unwrap();
        let replay = vec![
            CanonicalInputItem::DecisionRequest {
                declaration: old.clone(),
            },
            CanonicalInputItem::DecisionOutput { decision },
        ];
        let second = full_frame(&mut port, 2, current.clone(), replay);
        let facts: Vec<_> = port.submit(second).await.unwrap().collect().await;
        assert!(facts.iter().all(Result::is_ok));
        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0]["questions"],
            serde_json::to_value(old.questions()).unwrap()
        );
        assert!(requests[0]["state"]["projection"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            requests[1]["questions"],
            serde_json::to_value(current.questions()).unwrap()
        );
        assert_eq!(
            requests[1]["state"]["replay"][0],
            json!({
                "kind":"decision_request", "payload":{"questions":old.questions()},
            })
        );
        assert_eq!(requests[1]["state"]["replay"][1]["kind"], "decision_output");
        assert!(requests[1]["state"]["projection"]
            .as_array()
            .unwrap()
            .is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_native_streams_preserves_accounting_and_requires_full_recovery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let app = Router::new().route("/api/alpha/decisions", post(|| async {
            Json(json!({
                "model":"typesafe/jev-test", "id":"decision-response",
                "answers":{"gate":{"type":"noul","noul":0.8}},
                "usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":6}}
            }))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let usages = Arc::new(Mutex::new(Vec::new()));
        let observed_usage = usages.clone();
        let mut port = provider(&base)
            .with_call_observer(move |event| observed.lock().unwrap().push(event))
            .with_response_usage_observer(move |usage| observed_usage.lock().unwrap().push(usage));
        let initial = port.declare().unwrap();
        let first = frame(&mut port);
        let stream = port.submit(first).await.unwrap();
        drop(stream);
        let cancelled = port.declare().unwrap();
        assert!(matches!(
            cancelled.continuity(),
            TargetContinuity::FullRequired { .. }
        ));
        assert!(cancelled.continuity().epoch() > initial.continuity().epoch());

        let second = frame(&mut port);
        let mut stream = port.submit(second).await.unwrap();
        let decision = stream.next().await.unwrap().unwrap();
        assert!(
            matches!(decision, ProviderFact::Decision { decision, .. } if decision.is_native_answers())
        );
        drop(stream);
        let before_terminal = port.declare().unwrap();
        assert!(matches!(
            before_terminal.continuity(),
            TargetContinuity::FullRequired { .. }
        ));
        assert!(before_terminal.continuity().epoch() > cancelled.continuity().epoch());

        let third = frame(&mut port);
        let revision = third.revision();
        let facts: Vec<_> = port.submit(third).await.unwrap().collect().await;
        assert_eq!(facts.len(), 2);
        assert!(matches!(&facts[0], Ok(ProviderFact::Decision { .. })));
        assert!(matches!(
            &facts[1],
            Ok(ProviderFact::ReactionCompleted { primary_text: None })
        ));
        assert_eq!(
            port.declare().unwrap().continuity().accepted_revision(),
            Some(revision)
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        let outcomes: Vec<_> = calls
            .iter()
            .filter_map(|event| match event {
                OpenAiResponsesCallEvent::Finished(snapshot) => snapshot.outcome,
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                OpenAiResponsesCallOutcome::Cancelled,
                OpenAiResponsesCallOutcome::Completed,
                OpenAiResponsesCallOutcome::Completed
            ]
        );
        let usages = usages.lock().unwrap();
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].input_tokens(), Some(10));
        assert_eq!(usages[0].cached_tokens(), Some(6));
        assert_eq!(usages[0].output_tokens(), Some(2));
        assert_eq!(usages[0].total_tokens(), Some(12));
        server.abort();
    }

    #[tokio::test]
    async fn request_byte_rejections_do_not_handoff_or_change_continuity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let mut port = provider("http://127.0.0.1:1/api/v1")
            .with_call_observer(move |event| observed.lock().unwrap().push(event));
        let candidate = frame(&mut port);
        port.http.max_serialized_request_body_bytes = 1;
        let initial = port.declare().unwrap();
        let fault = port.submit(candidate).await.err().unwrap();
        let SubmitFault::Rejected(fault) = fault else {
            panic!("expected local limit rejection")
        };
        assert_eq!(fault.code(), ReactionPortFaultCode::Limit);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(port.declare().unwrap(), initial);
    }

    #[test]
    fn native_options_and_fresh_sessions_keep_http_policy_with_native_capabilities() {
        let routing =
            OpenRouterProviderRouting::new(vec!["typesafe".into()], vec!["typesafe".into()], true)
                .unwrap();
        let mut native = System1Provider::try_new(
            config("http://127.0.0.1:1/api/v1")
                .with_responses_serialized_request_body_limit(20_000)
                .unwrap(),
            identity(),
            System1Options::new(ModelSpec::new("typesafe/jev-test", 32_768).unwrap())
                .with_openrouter_provider_routing(routing.clone()),
        )
        .unwrap();
        let native_target = native.declare().unwrap();
        assert_eq!(native.options.model().id(), "typesafe/jev-test");
        assert_eq!(native.options.provider, Some(routing));
        assert_eq!(native.http.max_serialized_request_body_bytes, 20_000);
        let capabilities = native_target.profile().capabilities;
        assert!(capabilities.supports_decision());
        assert!(!capabilities.supports_semantic_delta());
        assert!(!capabilities.supports_native_tool_calls());
        assert!(!capabilities.supports_text_delta_output());
        let mut fresh = native.fresh_session().unwrap();
        let fresh_target = fresh.declare().unwrap();
        assert_ne!(fresh_target.identity(), native_target.identity());
        assert_eq!(fresh_target.profile(), native_target.profile());
        assert!(matches!(
            fresh_target.continuity(),
            TargetContinuity::FullRequired { .. }
        ));
        assert_eq!(fresh.options, native.options);
        let candidate = frame(&mut native);
        let control = CanonicalInputItem::DecisionRequest {
            declaration: candidate.submission().decision().unwrap().clone(),
        };
        assert!(lower_items(
            &[control],
            candidate.submission().decision().unwrap().batch_id()
        )
        .unwrap()
        .is_empty());
        let extension = CanonicalInputItem::provider_extension(
            ProviderExtension::new(
                "openai",
                "encrypted_reasoning",
                1,
                json!({"private":"opaque"}),
            )
            .unwrap(),
        );
        assert!(lower_items(
            &[extension],
            candidate.submission().decision().unwrap().batch_id()
        )
        .is_err());
    }
}
