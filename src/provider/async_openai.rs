//! OpenAI Responses adapter for the execution runtime's single provider port.
//!
//! AgentView owns canonical history, request encoding, typed events, terminal
//! validation, and retry policy. `async-openai` is intentionally limited to
//! validated OpenAI configuration: its typed SSE decoder logs malformed event
//! bodies before callers can redact them, so this adapter parses SSE itself.

use std::{
    collections::BTreeMap,
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
#[cfg(feature = "legacy-provider-port")]
use std::{future::Future, sync::Mutex, task::Poll};

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
use serde_json::{Map, Value};
use url::{Host, Url};

#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "the Responses adapter retains a feature-gated ProviderPort compatibility path"
)]
use crate::component::execution::{ProviderPort, RenderedProjection};
use crate::{
    component::execution::{
        reaction::{
            FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision, ReactionPortFault,
            ReactionPortFaultKind, TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        ProviderFault, ProviderIdentity, ToolCall,
    },
    provider::codex_http_v1::CodexHttpV1Encoder,
};
#[cfg(feature = "legacy-provider-port")]
use crate::{
    component::execution::{
        ProviderEvent, ProviderEventStream, ProviderFaultCode, ToolOutput, ToolOutputSink,
    },
    llm_call::TextTurnEvent,
};

#[cfg(feature = "legacy-provider-port")]
use self::continuation::{OpenAiContinuation, ResponsesInputGateReady};
use self::usage::ResponseUsageObserver;
#[cfg(feature = "legacy-provider-port")]
use self::{
    output::{CompletedOpenAiOutput, OpenAiOutputLedger},
    usage::{observe_response_usage, validated_response_usage},
};

#[cfg(feature = "legacy-provider-port")]
mod artifact_binding;
mod chat_completions;
#[cfg(feature = "legacy-provider-port")]
mod continuation;
mod faults;
mod frame_request;
mod native_reaction;
mod output;
mod reaction_fault;
mod transport;
mod usage;

#[cfg(feature = "legacy-provider-port")]
use self::artifact_binding::OpenAiInlineArtifactBinding;
pub use self::chat_completions::{
    AsyncOpenAiChatCompletionsProvider, OpenAiChatCompletionsError, OpenAiChatCompletionsOptions,
    OPENAI_CHAT_COMPLETIONS_PROFILE,
};
use self::faults::{
    has_plaintext_reasoning_completed_content, has_plaintext_reasoning_lifecycle_content,
    has_unsupported_completed_item, has_unsupported_content_part, has_unsupported_lifecycle_item,
    is_native_tool_event, OpenAiBodyStreamFault,
};
#[cfg(feature = "legacy-provider-port")]
use self::faults::{
    output_limit_fault, redacted_status_fault, request_transport_fault, required_string,
    response_body_limit_fault, response_completed_ledger_fault, response_event_envelope_fault,
    response_event_shape_fault, response_output_item_added_fault, response_stream_completion_fault,
    serialized_request_body_limit_fault, stream_error_code, stream_event_limit_fault,
    stream_transport_fault, unsupported_content_part_fault, unsupported_output_item_fault,
    unsupported_reasoning_content_fault, OpenAiApi, ResponseEventReason,
};
pub use self::usage::OpenAiResponsesUsage;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_OUTPUT_TEXT_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_RESPONSES_SERIALIZED_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CHAT_COMPLETIONS_SERIALIZED_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_RESPONSES_FRAME_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_RESPONSES_COMPONENT_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_CHAT_COMPLETIONS_FRAME_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CHAT_COMPLETIONS_COMPONENT_BYTES: usize = 4 * 1024 * 1024;
static NEXT_RESPONSES_TARGET_ID: AtomicU64 = AtomicU64::new(1);

/// Validated connection inputs shared by the OpenAI HTTP adapters.
pub struct AsyncOpenAiTransportConfig {
    api_base: String,
    api_key: String,
    bypass_environment_proxy: bool,
    connect_timeout: Duration,
    request_timeout: Duration,
    read_timeout: Duration,
    max_response_body_bytes: usize,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    max_responses_serialized_request_body_bytes: usize,
    max_chat_completions_serialized_request_body_bytes: usize,
    responses_frame_constraints: FrameConstraints,
    chat_completions_frame_constraints: FrameConstraints,
}

impl AsyncOpenAiTransportConfig {
    pub fn new(
        api_base: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        let api_base = api_base.into();
        let parsed =
            Url::parse(&api_base).map_err(|error| AsyncOpenAiConfigError::InvalidApiBase {
                message: error.to_string(),
            })?;
        let bypass_environment_proxy = has_loopback_host(&parsed);
        let scheme_is_allowed = match parsed.scheme() {
            "https" => true,
            "http" => bypass_environment_proxy,
            _ => false,
        };
        let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
        if !scheme_is_allowed
            || parsed.host_str().is_none()
            || has_userinfo
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(AsyncOpenAiConfigError::InvalidApiBase {
                message: "expected HTTPS, or HTTP with a loopback host, without userinfo, query, or fragment"
                    .to_owned(),
            });
        }

        let api_key = api_key.into();
        if api_key.is_empty() {
            return Err(AsyncOpenAiConfigError::EmptyApiKey);
        }
        if api_key
            .chars()
            .any(|character| !character.is_ascii() || character.is_ascii_control())
        {
            return Err(AsyncOpenAiConfigError::InvalidApiKey);
        }

        Ok(Self {
            api_base: api_base.trim_end_matches('/').to_owned(),
            api_key,
            bypass_environment_proxy,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_SSE_EVENT_BYTES,
            max_output_text_bytes: DEFAULT_MAX_OUTPUT_TEXT_BYTES,
            max_responses_serialized_request_body_bytes:
                DEFAULT_MAX_RESPONSES_SERIALIZED_REQUEST_BODY_BYTES,
            max_chat_completions_serialized_request_body_bytes:
                DEFAULT_MAX_CHAT_COMPLETIONS_SERIALIZED_REQUEST_BODY_BYTES,
            responses_frame_constraints: FrameConstraints {
                max_frame_bytes: DEFAULT_MAX_RESPONSES_FRAME_BYTES,
                max_component_bytes: DEFAULT_MAX_RESPONSES_COMPONENT_BYTES,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            chat_completions_frame_constraints: FrameConstraints {
                max_frame_bytes: DEFAULT_MAX_CHAT_COMPLETIONS_FRAME_BYTES,
                max_component_bytes: DEFAULT_MAX_CHAT_COMPLETIONS_COMPONENT_BYTES,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
        })
    }

    /// Replaces all transport deadlines with explicit non-zero values.
    pub fn with_timeouts(
        self,
        connect_timeout: Duration,
        request_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        validate_timeout("connect_timeout", connect_timeout)?;
        validate_timeout("request_timeout", request_timeout)?;
        validate_timeout("read_timeout", read_timeout)?;
        Ok(Self {
            connect_timeout,
            request_timeout,
            read_timeout,
            ..self
        })
    }

    /// Replaces the cumulative HTTP body, one SSE event, and output text limits.
    pub fn with_response_limits(
        self,
        max_response_body_bytes: usize,
        max_sse_event_bytes: usize,
        max_output_text_bytes: usize,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        validate_limit("max_response_body_bytes", max_response_body_bytes)?;
        validate_limit("max_sse_event_bytes", max_sse_event_bytes)?;
        validate_limit("max_output_text_bytes", max_output_text_bytes)?;
        Ok(Self {
            max_response_body_bytes,
            max_sse_event_bytes,
            max_output_text_bytes,
            ..self
        })
    }

    /// Replaces the OpenAI Responses serialized outbound request body byte limit.
    ///
    /// The limit is applied to the exact final JSON body after continuation
    /// reconciliation and compaction, before an HTTP request is constructed.
    pub fn with_responses_serialized_request_body_limit(
        self,
        max_serialized_request_body_bytes: usize,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        if max_serialized_request_body_bytes == 0 {
            return Err(AsyncOpenAiConfigError::InvalidSerializedRequestBodyLimit);
        }
        Ok(Self {
            max_responses_serialized_request_body_bytes: max_serialized_request_body_bytes,
            ..self
        })
    }

    /// Replaces the mount-stable canonical Frame constraints declared by the
    /// OpenAI Responses reaction target.
    ///
    /// These limits apply to AgentView's canonical Frame encoding. The exact
    /// serialized OpenAI request body retains its independent transport limit.
    pub fn with_responses_frame_constraints(
        self,
        constraints: FrameConstraints,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        responses_frame_profile(constraints.clone())?;
        Ok(Self {
            responses_frame_constraints: constraints,
            ..self
        })
    }

    /// Replaces the Chat Completions serialized outbound request body byte limit.
    ///
    /// The limit is applied to the exact final JSON body after history
    /// reconciliation, before transport handoff.
    pub fn with_chat_completions_serialized_request_body_limit(
        self,
        max_serialized_request_body_bytes: usize,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        if max_serialized_request_body_bytes == 0 {
            return Err(AsyncOpenAiConfigError::InvalidChatCompletionsSerializedRequestBodyLimit);
        }
        Ok(Self {
            max_chat_completions_serialized_request_body_bytes: max_serialized_request_body_bytes,
            ..self
        })
    }

    /// Replaces the mount-stable canonical Frame constraints declared by the
    /// OpenAI Chat Completions reaction target.
    ///
    /// These limits apply to AgentView's canonical Frame encoding. The exact
    /// serialized Chat request retains its independent transport limit.
    pub fn with_chat_completions_frame_constraints(
        self,
        constraints: FrameConstraints,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        chat_completions_frame_profile(constraints.clone())?;
        Ok(Self {
            chat_completions_frame_constraints: constraints,
            ..self
        })
    }
}

fn responses_frame_profile(
    constraints: FrameConstraints,
) -> Result<FrameProfile, AsyncOpenAiConfigError> {
    let profile = FrameProfile::new(constraints, FrameCapabilities::new(true));
    profile
        .validate()
        .map_err(|_| AsyncOpenAiConfigError::InvalidResponsesFrameProfile)?;
    Ok(profile)
}

fn chat_completions_frame_profile(
    constraints: FrameConstraints,
) -> Result<FrameProfile, AsyncOpenAiConfigError> {
    let profile = FrameProfile::new(constraints, FrameCapabilities::new(true));
    profile
        .validate()
        .map_err(|_| AsyncOpenAiConfigError::InvalidChatCompletionsFrameProfile)?;
    Ok(profile)
}

fn next_responses_target_identity() -> Result<TargetIdentity, AsyncOpenAiConfigError> {
    NEXT_RESPONSES_TARGET_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map(|value| {
            TargetIdentity::new(
                NonZeroU128::new(u128::from(value))
                    .expect("Responses target identity counter starts at one"),
            )
        })
        .map_err(|_| AsyncOpenAiConfigError::ResponsesTargetIdentityExhausted)
}

fn validate_timeout(name: &'static str, timeout: Duration) -> Result<(), AsyncOpenAiConfigError> {
    if timeout.is_zero() {
        return Err(AsyncOpenAiConfigError::InvalidTimeout { name });
    }
    Ok(())
}

fn validate_limit(name: &'static str, limit: usize) -> Result<(), AsyncOpenAiConfigError> {
    if limit == 0 {
        return Err(AsyncOpenAiConfigError::InvalidResponseLimit { name });
    }
    Ok(())
}

fn has_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// OpenAI Responses implementation of the execution runtime's provider port.
pub struct AsyncOpenAiResponsesProvider {
    client: reqwest::Client,
    config: OpenAIConfig,
    identity: ProviderIdentity,
    encoder: CodexHttpV1Encoder,
    read_timeout: Duration,
    max_response_body_bytes: usize,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    max_responses_serialized_request_body_bytes: usize,
    #[cfg(feature = "legacy-provider-port")]
    artifact_binding: Option<OpenAiInlineArtifactBinding>,
    #[cfg(feature = "legacy-provider-port")]
    continuation: Option<OpenAiContinuation>,
    #[cfg(feature = "legacy-provider-port")]
    tool_outputs: Arc<Mutex<ToolOutputStaging>>,
    response_usage_observer: Option<Arc<ResponseUsageObserver>>,
    execution_mode: ResponsesExecutionMode,
    reaction_target: ResponsesReactionTarget,
    reaction_frame: Option<frame_request::ResponsesFrameRequestState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesExecutionMode {
    Unclaimed,
    #[cfg(feature = "legacy-provider-port")]
    Legacy,
    FrameNative,
}

#[derive(Debug)]
struct ResponsesReactionTarget {
    identity: TargetIdentity,
    epoch: TargetEpoch,
    accepted_revision: Option<FrameRevision>,
    profile: FrameProfile,
    terminal_fault: Option<ReactionPortFault>,
}

impl ResponsesReactionTarget {
    fn new(profile: FrameProfile) -> Result<Self, AsyncOpenAiConfigError> {
        Ok(Self {
            identity: next_responses_target_identity()?,
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
            None => self.terminal_fault = Some(declaration_state_lost_fault()),
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

fn declaration_state_lost_fault() -> ReactionPortFault {
    reaction_fault::map_openai_fault(&reaction_fault::OpenAiReactionFailure::static_diagnostic(
        reaction_fault::OpenAiFailureClass::DeclarationStateLost,
        "Responses target declaration state is unavailable",
    ))
}

#[cfg(feature = "legacy-provider-port")]
#[derive(Debug, Default)]
struct ToolOutputStaging {
    calls: BTreeMap<u64, (String, Option<ToolOutput>)>,
}

#[cfg(feature = "legacy-provider-port")]
impl ToolOutputStaging {
    fn register(&mut self, ordinal: u64, call_id: String) -> Result<(), ()> {
        match self.calls.entry(ordinal) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((call_id, None));
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => Err(()),
        }
    }

    fn ready(&self) -> Result<Vec<(u64, ToolOutput)>, ()> {
        self.calls
            .iter()
            .map(|(ordinal, (_, output))| output.clone().map(|output| (*ordinal, output)).ok_or(()))
            .collect()
    }

    fn consume(&mut self, receipt: &[(u64, ToolOutput)]) -> Result<(), ()> {
        if !self.matches_receipt(receipt) {
            return Err(());
        }
        for (ordinal, _) in receipt {
            self.calls.remove(ordinal).ok_or(())?;
        }
        Ok(())
    }

    fn matches_receipt(&self, receipt: &[(u64, ToolOutput)]) -> bool {
        if self.calls.len() != receipt.len() {
            return false;
        }
        for ((ordinal, (call_id, accepted)), (receipt_ordinal, output)) in
            self.calls.iter().zip(receipt)
        {
            if ordinal != receipt_ordinal
                || call_id != output.call_id()
                || accepted.as_ref() != Some(output)
            {
                return false;
            }
        }
        true
    }
}

#[cfg(feature = "legacy-provider-port")]
impl ToolOutputSink for Mutex<ToolOutputStaging> {
    fn register(&self, ordinal: u64, call_id: &str) -> Result<(), ()> {
        self.lock()
            .map_err(|_| ())?
            .register(ordinal, call_id.to_owned())
    }

    fn accept(&self, ordinal: u64, output: ToolOutput) -> Result<(), ()> {
        let mut staged = self.lock().map_err(|_| ())?;
        let (call_id, existing) = staged.calls.get_mut(&ordinal).ok_or(())?;
        if *call_id != output.call_id() || existing.is_some() {
            return Err(());
        }
        *existing = Some(output);
        Ok(())
    }
}

impl AsyncOpenAiResponsesProvider {
    pub fn new(
        config: AsyncOpenAiTransportConfig,
        identity: ProviderIdentity,
        encoder: CodexHttpV1Encoder,
    ) -> Self {
        Self::try_new(config, identity, encoder)
            .unwrap_or_else(|_| panic!("OpenAI transport initialization failed"))
    }

    /// Builds a Responses provider without exposing HTTP-client source errors.
    pub fn try_new(
        config: AsyncOpenAiTransportConfig,
        identity: ProviderIdentity,
        encoder: CodexHttpV1Encoder,
    ) -> Result<Self, AsyncOpenAiConfigError> {
        let initialized = transport::initialize(config)?;
        let reaction_target = ResponsesReactionTarget::new(initialized.responses_frame_profile)?;
        Ok(Self {
            client: initialized.client,
            config: initialized.config,
            identity,
            encoder,
            read_timeout: initialized.read_timeout,
            max_response_body_bytes: initialized.max_response_body_bytes,
            max_sse_event_bytes: initialized.max_sse_event_bytes,
            max_output_text_bytes: initialized.max_output_text_bytes,
            max_responses_serialized_request_body_bytes: initialized
                .max_responses_serialized_request_body_bytes,
            #[cfg(feature = "legacy-provider-port")]
            artifact_binding: None,
            #[cfg(feature = "legacy-provider-port")]
            continuation: None,
            #[cfg(feature = "legacy-provider-port")]
            tool_outputs: Arc::new(Mutex::new(ToolOutputStaging::default())),
            response_usage_observer: None,
            execution_mode: ResponsesExecutionMode::Unclaimed,
            reaction_target,
            reaction_frame: None,
        })
    }

    pub fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    /// Observes validated terminal usage without adding provider accounting to ProviderPort events.
    ///
    /// The callback runs synchronously while the response stream is polled, so it must remain
    /// bounded and nonblocking. It receives no raw response data and cannot change request,
    /// history, or reconciliation state. A callback panic is contained.
    pub fn with_response_usage_observer(
        mut self,
        observer: impl Fn(OpenAiResponsesUsage) + Send + Sync + 'static,
    ) -> Self {
        self.response_usage_observer = Some(Arc::new(observer));
        self
    }

    fn ensure_frame_native_mode(&self) -> Result<(), ReactionPortFault> {
        match self.execution_mode {
            ResponsesExecutionMode::Unclaimed | ResponsesExecutionMode::FrameNative => Ok(()),
            #[cfg(feature = "legacy-provider-port")]
            ResponsesExecutionMode::Legacy => Err(declaration_state_lost_fault()),
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    fn ensure_legacy_mode(&self) -> Result<(), ProviderFault> {
        match self.execution_mode {
            ResponsesExecutionMode::Unclaimed | ResponsesExecutionMode::Legacy => Ok(()),
            ResponsesExecutionMode::FrameNative => Err(ProviderFault::model_rejected(
                "OpenAI Responses provider is already using the Frame-native protocol",
            )
            .with_code(ProviderFaultCode::RequestPreparation)),
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    fn staged_tool_output_receipt(&self) -> Result<Vec<(u64, ToolOutput)>, ProviderFault> {
        self.tool_outputs
            .lock()
            .map_err(|_| {
                ProviderFault::model_rejected("OpenAI tool result staging is unavailable")
            })?
            .ready()
            .map_err(|_| {
                ProviderFault::model_rejected("OpenAI pending tool call has no unique output")
                    .with_code(ProviderFaultCode::RequestPreparation)
            })
    }

    #[cfg(feature = "legacy-provider-port")]
    async fn start_stream<'a>(
        &'a mut self,
        gate: ResponsesInputGateReady,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        serde_json::from_slice::<Value>(&gate.request_body).map_err(|_| {
            ProviderFault::model_rejected("OpenAI request body is not one JSON value")
                .with_code(ProviderFaultCode::RequestPreparation)
        })?;
        let request = self
            .client
            .post(self.config.url("/responses"))
            .headers(self.config.headers())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(gate.request_body.clone())
            .build()
            .map_err(|_| {
                ProviderFault::retryable_transport("OpenAI API transport failed")
                    .with_code(ProviderFaultCode::RequestPreparation)
            })?;
        let client = self.client.clone();
        let execution_mode = &mut self.execution_mode;
        let continuation_slot = &mut self.continuation;
        let artifact_binding_slot = &mut self.artifact_binding;
        let encoder = self.encoder.clone();
        let read_timeout = self.read_timeout;
        let max_response_body_bytes = self.max_response_body_bytes;
        let max_sse_event_bytes = self.max_sse_event_bytes;
        let max_output_text_bytes = self.max_output_text_bytes;
        let max_serialized_request_body_bytes = self.max_responses_serialized_request_body_bytes;
        let handoff_staging = Arc::clone(&self.tool_outputs);
        let tool_output_sink: Arc<dyn ToolOutputSink> = self.tool_outputs.clone();
        let response_usage_observer = self.response_usage_observer.clone();
        let mut transport = Box::pin(client.execute(request));
        let mut gate = Some(gate);
        let first_response = futures::future::poll_fn(|cx| {
            // Gate validation is pre-handoff. The guard remains held through
            // this one non-blocking transport poll, so exact staging cannot
            // change between validation and consumption.
            let mut staged = handoff_staging.lock().map_err(|_| {
                ProviderFault::model_rejected("OpenAI tool result staging is unavailable")
            })?;
            let ready = gate.as_ref().expect("gate is present before handoff");
            if !staged.matches_receipt(&ready.staged_output_receipt) {
                return Poll::Ready(Err(ProviderFault::model_rejected(
                    "OpenAI tool result staging changed before handoff",
                )
                .with_code(ProviderFaultCode::RequestPreparation)));
            }
            match execution_mode {
                ResponsesExecutionMode::Unclaimed => {
                    *execution_mode = ResponsesExecutionMode::Legacy;
                }
                ResponsesExecutionMode::Legacy => {}
                ResponsesExecutionMode::FrameNative => {
                    return Poll::Ready(Err(ProviderFault::model_rejected(
                        "OpenAI Responses provider is already using the Frame-native protocol",
                    )
                    .with_code(ProviderFaultCode::RequestPreparation)));
                }
            }
            let response = transport
                .as_mut()
                .poll(cx)
                .map_err(|error| request_transport_fault(OpenAiApi::Responses, &error));
            let ready = gate.take().expect("gate is present before handoff");
            if staged.consume(&ready.staged_output_receipt).is_err() {
                return Poll::Ready(Err(ProviderFault::model_rejected(
                    "OpenAI tool result staging changed before handoff",
                )
                .with_code(ProviderFaultCode::RequestPreparation)));
            }
            // The immutable request was polled by reqwest before these facts
            // advance. ApplicationHost can now observe InputSubmitted after
            // execute returns without getting ahead of causal history.
            *continuation_slot = Some(ready.candidate);
            *artifact_binding_slot = Some(ready.artifact_binding);
            Poll::Ready(Ok(response))
        })
        .await?;
        let pending = async move {
            let response = match first_response {
                Poll::Ready(Ok(response)) => response,
                Poll::Ready(Err(error)) => return post_handoff_fault(error),
                Poll::Pending => match transport.await {
                    Ok(response) => response,
                    Err(error) => {
                        return post_handoff_fault(request_transport_fault(
                            OpenAiApi::Responses,
                            &error,
                        ));
                    }
                },
            };
            let status = response.status();
            if !status.is_success() {
                return post_handoff_fault(redacted_status_fault(OpenAiApi::Responses, status));
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
                return post_handoff_fault(
                    ProviderFault::retryable_transport(
                        "OpenAI API returned a successful non-SSE response",
                    )
                    .with_code(ProviderFaultCode::ResponseContentType),
                );
            }
            if response.content_length().is_some_and(|content_length| {
                content_length > u64::try_from(max_response_body_bytes).unwrap_or(u64::MAX)
            }) {
                return post_handoff_fault(response_body_limit_fault(OpenAiApi::Responses));
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
            let state = OpenAiStreamState {
                stream: response_stream.eventsource(),
                completed_output: None,
                continuation_slot,
                artifact_binding_slot,
                response_completed: false,
                terminal_published: false,
                finished: false,
                last_sequence: None,
                output_ledger: OpenAiOutputLedger::default(),
                native_tools: NativeToolLedger::default(),
                output_text_bytes: 0,
                encoder,
                read_timeout,
                max_sse_event_bytes,
                max_output_text_bytes,
                max_serialized_request_body_bytes,
                tool_output_sink,
                response_usage_observer,
            };
            let mapped = futures::stream::unfold(state, move |mut state| async move {
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
                                        "OpenAI streaming response read timed out",
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
                                    Err(stream_transport_fault(OpenAiApi::Responses, code)),
                                    state,
                                ));
                            }
                            Ok(Some(Err(EventStreamError::Transport(
                                OpenAiBodyStreamFault::BodyLimit,
                            )))) => {
                                state.finished = true;
                                return Some((
                                    Err(response_body_limit_fault(OpenAiApi::Responses)),
                                    state,
                                ));
                            }
                            Ok(Some(Err(EventStreamError::Transport(
                                OpenAiBodyStreamFault::EventLimit,
                            )))) => {
                                state.finished = true;
                                return Some((
                                    Err(stream_event_limit_fault(OpenAiApi::Responses)),
                                    state,
                                ));
                            }
                            Ok(Some(Ok(event))) => event,
                            Ok(None) => {
                                if state.terminal_published {
                                    return None;
                                }
                                let result = finish_openai_stream(&mut state);
                                state.finished = true;
                                return Some((result, state));
                            }
                            Ok(Some(Err(_))) => {
                                state.finished = true;
                                return Some((
                                    Err(ProviderFault::retryable_transport(
                                        "OpenAI API returned an invalid streaming response",
                                    )
                                    .with_code(ProviderFaultCode::StreamDecode)),
                                    state,
                                ));
                            }
                        };
                    if sse_event_size(&event)
                        .is_none_or(|event_size| event_size > state.max_sse_event_bytes)
                    {
                        state.finished = true;
                        return Some((Err(stream_event_limit_fault(OpenAiApi::Responses)), state));
                    }
                    if event.data == "[DONE]" {
                        if state.terminal_published {
                            return None;
                        }
                        let result = finish_openai_stream(&mut state);
                        state.finished = true;
                        return Some((result, state));
                    }
                    let event_json = match serde_json::from_str::<Value>(&event.data) {
                        Ok(event_json) => event_json,
                        Err(_) => {
                            state.finished = true;
                            return Some((
                                Err(ProviderFault::retryable_transport(
                                    "OpenAI API returned an invalid streaming response",
                                )
                                .with_code(ProviderFaultCode::ResponseEventJson)),
                                state,
                            ));
                        }
                    };
                    let frame = match serde_json::from_value::<OpenAiWireEvent>(event_json) {
                        Ok(frame) => frame,
                        Err(_) => {
                            state.finished = true;
                            return Some((
                                Err(response_event_envelope_fault(
                                    "OpenAI streaming event has an invalid shape",
                                )),
                                state,
                            ));
                        }
                    };
                    if let Err(error) = advance_sequence(
                        &mut state.last_sequence,
                        &frame.event_type,
                        &frame.payload,
                    ) {
                        state.finished = true;
                        return Some((Err(error), state));
                    }
                    if state.response_completed {
                        state.finished = true;
                        return Some((
                            Err(response_event_shape_fault(
                                &frame.event_type,
                                ResponseEventReason::TerminalOrder,
                                ProviderFault::model_rejected(
                                    "OpenAI stream emitted an event after response.completed",
                                ),
                            )),
                            state,
                        ));
                    }

                    let lifecycle_identity_fault = |fault| {
                        response_event_shape_fault(
                            &frame.event_type,
                            ResponseEventReason::LifecycleIdentity,
                            fault,
                        )
                    };
                    let ledger_mismatch_fault = |fault| {
                        response_event_shape_fault(
                            &frame.event_type,
                            ResponseEventReason::LedgerMismatch,
                            fault,
                        )
                    };
                    match frame.event_type.as_str() {
                        "response.created" => {
                            if let Err(error) =
                                state.output_ledger.record_response_created(&frame.payload)
                            {
                                state.finished = true;
                                return Some((Err(lifecycle_identity_fault(error)), state));
                            }
                        }
                        "response.in_progress" => {
                            if let Err(error) = state
                                .output_ledger
                                .record_response_in_progress(&frame.payload)
                            {
                                state.finished = true;
                                return Some((Err(lifecycle_identity_fault(error)), state));
                            }
                        }
                        "response.content_part.added" => {
                            if has_unsupported_content_part(&frame.payload) {
                                state.finished = true;
                                return Some((Err(unsupported_content_part_fault()), state));
                            }
                            if let Err(error) = state
                                .output_ledger
                                .record_content_part_added(&frame.payload)
                            {
                                state.finished = true;
                                return Some((Err(ledger_mismatch_fault(error)), state));
                            }
                        }
                        "response.content_part.done" => {
                            if has_unsupported_content_part(&frame.payload) {
                                state.finished = true;
                                return Some((Err(unsupported_content_part_fault()), state));
                            }
                            if let Err(error) =
                                state.output_ledger.record_content_part_done(&frame.payload)
                            {
                                state.finished = true;
                                return Some((Err(ledger_mismatch_fault(error)), state));
                            }
                        }
                        "response.output_text.delta" => {
                            let delta = match required_string(&frame.payload, "delta") {
                                Ok(delta) => delta,
                                Err(error) => {
                                    state.finished = true;
                                    return Some((Err(ledger_mismatch_fault(error)), state));
                                }
                            };
                            if exceeds_limit(
                                state.output_text_bytes,
                                delta.len(),
                                state.max_output_text_bytes,
                            ) {
                                state.finished = true;
                                return Some((
                                    Err(output_limit_fault(OpenAiApi::Responses)),
                                    state,
                                ));
                            }
                            let dispatch = match state
                                .output_ledger
                                .record_text_delta(&frame.payload, &delta)
                            {
                                Ok(dispatch) => dispatch,
                                Err(error) => {
                                    state.finished = true;
                                    return Some((Err(ledger_mismatch_fault(error)), state));
                                }
                            };
                            state.output_text_bytes += delta.len();
                            if dispatch {
                                let output_index = frame
                                    .payload
                                    .get("output_index")
                                    .and_then(Value::as_u64)
                                    .ok_or_else(|| {
                                        ledger_mismatch_fault(ProviderFault::model_rejected(
                                            "OpenAI text delta is missing output identity",
                                        ))
                                    });
                                let output_index = match output_index {
                                    Ok(output_index) => output_index,
                                    Err(error) => {
                                        state.finished = true;
                                        return Some((Err(error), state));
                                    }
                                };
                                let phase = state.output_ledger.message_phase(output_index);
                                let Some(continuation) = state.continuation_slot.as_mut() else {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI local history is unavailable",
                                        )),
                                        state,
                                    ));
                                };
                                let Some(artifact_binding) = state.artifact_binding_slot.as_ref()
                                else {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI continuation artifact binding is unavailable",
                                        )),
                                        state,
                                    ));
                                };
                                if let Err(error) = continuation.record_text_partial_bounded(
                                    output_index,
                                    &delta,
                                    phase,
                                    artifact_binding,
                                    &state.encoder,
                                    state.max_serialized_request_body_bytes,
                                ) {
                                    state.finished = true;
                                    let fault = if error.is_serialized_request_body_limit() {
                                        serialized_request_body_limit_fault()
                                    } else {
                                        ProviderFault::model_rejected(
                                            "OpenAI partial output could not be retained",
                                        )
                                    };
                                    return Some((Err(fault), state));
                                }
                                return Some((
                                    Ok(ProviderEvent::Text(TextTurnEvent::TextDelta(delta))),
                                    state,
                                ));
                            }
                        }
                        "response.output_text.annotation.added" => {
                            if let Err(error) = state
                                .output_ledger
                                .record_text_annotation_added(&frame.payload)
                            {
                                state.finished = true;
                                return Some((Err(ledger_mismatch_fault(error)), state));
                            }
                        }
                        "response.output_text.done" => {
                            let text = match required_string(&frame.payload, "text") {
                                Ok(text) => text,
                                Err(error) => {
                                    state.finished = true;
                                    return Some((Err(ledger_mismatch_fault(error)), state));
                                }
                            };
                            if text.len() > state.max_output_text_bytes {
                                state.finished = true;
                                return Some((
                                    Err(output_limit_fault(OpenAiApi::Responses)),
                                    state,
                                ));
                            }
                            if let Err(error) =
                                state.output_ledger.record_text_done(&frame.payload, &text)
                            {
                                state.finished = true;
                                return Some((Err(ledger_mismatch_fault(error)), state));
                            }
                        }
                        "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                            state.finished = true;
                            return Some((Err(unsupported_reasoning_content_fault()), state));
                        }
                        // Terminal capability rejection takes precedence over reconciliation faults;
                        // the fixed message matches the ledger contract without reading error text.
                        "response.completed" if has_unsupported_completed_item(&frame.payload) => {
                            if has_native_function_call_completed(&frame.payload) {
                                if let Err(error) = state.native_tools.completed(&frame.payload) {
                                    state.finished = true;
                                    return Some((Err(ledger_mismatch_fault(error)), state));
                                }
                                let mut reconciled = frame.payload.clone();
                                if let Some(output) = reconciled
                                    .get_mut("response")
                                    .and_then(|response| response.get_mut("output"))
                                    .and_then(Value::as_array_mut)
                                {
                                    output.retain(|item| {
                                        item.get("type").and_then(Value::as_str)
                                            != Some("function_call")
                                    });
                                }
                                let retained_output_is_empty = reconciled
                                    .get("response")
                                    .and_then(|response| response.get("output"))
                                    .and_then(Value::as_array)
                                    .is_some_and(Vec::is_empty);
                                if retained_output_is_empty {
                                    let completed_output = CompletedOpenAiOutput {
                                        output_items: Vec::new(),
                                        wire_items: Vec::new(),
                                        final_text: String::new(),
                                        output_text_bytes: 0,
                                    };
                                    match finish_accepted_response(
                                        &mut state,
                                        &frame.payload,
                                        completed_output,
                                    ) {
                                        Ok(event) => return Some((Ok(event), state)),
                                        Err(error) => {
                                            state.finished = true;
                                            return Some((Err(error), state));
                                        }
                                    }
                                }
                                match state.output_ledger.complete(&reconciled) {
                                    Ok(completed_output) => {
                                        match finish_accepted_response(
                                            &mut state,
                                            &frame.payload,
                                            completed_output,
                                        ) {
                                            Ok(event) => return Some((Ok(event), state)),
                                            Err(error) => {
                                                state.finished = true;
                                                return Some((Err(error), state));
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        state.finished = true;
                                        return Some((
                                            Err(ledger_mismatch_fault(error.into_provider_fault())),
                                            state,
                                        ));
                                    }
                                }
                            }
                            state.finished = true;
                            return Some((Err(unsupported_output_item_fault()), state));
                        }
                        "response.completed"
                            if has_plaintext_reasoning_completed_content(&frame.payload) =>
                        {
                            state.finished = true;
                            return Some((Err(unsupported_reasoning_content_fault()), state));
                        }
                        "response.completed" => {
                            match state.output_ledger.complete(&frame.payload) {
                                Ok(completed_output) => {
                                    match finish_accepted_response(
                                        &mut state,
                                        &frame.payload,
                                        completed_output,
                                    ) {
                                        Ok(event) => return Some((Ok(event), state)),
                                        Err(error) => {
                                            state.finished = true;
                                            return Some((Err(error), state));
                                        }
                                    }
                                }
                                Err(error) => {
                                    state.finished = true;
                                    let ledger_reason = error.response_ledger_reason();
                                    let message_text_reason = error.response_message_text_reason();
                                    let completed_reconciliation =
                                        error.response_completed_reconciliation();
                                    let output_identity_reason =
                                        error.response_output_identity_reason();
                                    let output_identity_detail =
                                        error.response_output_identity_detail();
                                    return Some((
                                        Err(response_completed_ledger_fault(
                                            ledger_reason,
                                            message_text_reason,
                                            completed_reconciliation,
                                            output_identity_reason,
                                            output_identity_detail,
                                            error.into_provider_fault(),
                                        )),
                                        state,
                                    ));
                                }
                            }
                        }
                        // Unsupported item types are capability rejections before lifecycle validation.
                        "response.output_item.added" => {
                            if has_plaintext_reasoning_lifecycle_content(&frame.payload) {
                                state.finished = true;
                                return Some((Err(unsupported_reasoning_content_fault()), state));
                            }
                            if has_unsupported_lifecycle_item(&frame.payload)
                                && native_function_call(&frame.payload).is_none()
                            {
                                state.finished = true;
                                return Some((Err(unsupported_output_item_fault()), state));
                            }
                            if native_function_call(&frame.payload).is_some() {
                                if let Err(error) = state.native_tools.added(&frame.payload) {
                                    state.finished = true;
                                    return Some((Err(ledger_mismatch_fault(error)), state));
                                }
                                continue;
                            }
                            if let Err(error) = state.output_ledger.record_added(&frame.payload) {
                                state.finished = true;
                                let reason = error.response_ledger_reason();
                                return Some((
                                    Err(response_output_item_added_fault(
                                        reason,
                                        error.into_provider_fault(),
                                    )),
                                    state,
                                ));
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            if let Err(error) = state.native_tools.delta(&frame.payload) {
                                state.finished = true;
                                return Some((Err(ledger_mismatch_fault(error)), state));
                            }
                        }
                        "response.function_call_arguments.done" => {
                            if let Err(error) = state.native_tools.arguments_done(&frame.payload) {
                                state.finished = true;
                                return Some((Err(ledger_mismatch_fault(error)), state));
                            }
                        }
                        "response.output_item.done" => {
                            if has_plaintext_reasoning_lifecycle_content(&frame.payload) {
                                state.finished = true;
                                return Some((Err(unsupported_reasoning_content_fault()), state));
                            }
                            if has_unsupported_lifecycle_item(&frame.payload)
                                && native_function_call(&frame.payload).is_none()
                            {
                                state.finished = true;
                                return Some((Err(unsupported_output_item_fault()), state));
                            }
                            if native_function_call(&frame.payload).is_some() {
                                let call = match state.native_tools.item_done(&frame.payload) {
                                    Ok(call) => call,
                                    Err(error) => {
                                        state.finished = true;
                                        return Some((Err(ledger_mismatch_fault(error)), state));
                                    }
                                };
                                let ordinal =
                                    match frame.payload.get("output_index").and_then(Value::as_u64)
                                    {
                                        Some(ordinal) => ordinal,
                                        None => {
                                            state.finished = true;
                                            return Some((
                                        Err(ledger_mismatch_fault(ProviderFault::model_rejected(
                                            "OpenAI function call is missing output identity",
                                        ))),
                                        state,
                                    ));
                                        }
                                    };
                                let Some(continuation) = state.continuation_slot.as_mut() else {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI local history is unavailable",
                                        )),
                                        state,
                                    ));
                                };
                                if continuation.append_tool_call(&call).is_err() {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI tool call ledger rejected an item",
                                        )),
                                        state,
                                    ));
                                }
                                let call = match call
                                    .with_output_sink(ordinal, state.tool_output_sink.clone())
                                {
                                    Ok(call) => call,
                                    Err(()) => {
                                        state.finished = true;
                                        return Some((
                                            Err(ProviderFault::model_rejected(
                                                "OpenAI tool result staging rejected a call",
                                            )),
                                            state,
                                        ));
                                    }
                                };
                                return Some((Ok(ProviderEvent::ToolCall(call)), state));
                            }
                            let sealed_private =
                                match state.output_ledger.record_done(&frame.payload) {
                                    Ok(sealed_private) => sealed_private,
                                    Err(error) => {
                                        state.finished = true;
                                        return Some((Err(lifecycle_identity_fault(error)), state));
                                    }
                                };
                            let output_index = frame
                                .payload
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .expect("validated output_item.done has output_index");
                            if let Some((text, phase)) =
                                state.output_ledger.message_text(output_index)
                            {
                                let Some(continuation) = state.continuation_slot.as_mut() else {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI local history is unavailable",
                                        )),
                                        state,
                                    ));
                                };
                                if continuation
                                    .seal_text_output(output_index, text, phase)
                                    .is_err()
                                {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI sealed output could not be retained",
                                        )),
                                        state,
                                    ));
                                }
                            }
                            if let Some(sealed_private) = sealed_private {
                                let Some(continuation) = state.continuation_slot.as_mut() else {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI local history is unavailable",
                                        )),
                                        state,
                                    ));
                                };
                                if continuation.seal_private_output(sealed_private).is_err() {
                                    state.finished = true;
                                    return Some((
                                        Err(ProviderFault::model_rejected(
                                            "OpenAI sealed private output could not be retained",
                                        )),
                                        state,
                                    ));
                                }
                            }
                        }
                        "response.failed" | "response.incomplete" | "error" => {
                            state.finished = true;
                            return Some((
                                Err(ProviderFault::model_rejected(format!(
                                    "OpenAI stream terminated with {}",
                                    frame.event_type
                                ))),
                                state,
                            ));
                        }
                        event_type if is_native_tool_event(event_type) => {
                            state.finished = true;
                            return Some((
                            Err(ProviderFault::model_rejected(
                                "native OpenAI tool events are not supported by this adapter version",
                            )),
                            state,
                        ));
                        }
                        event_type if event_type.starts_with("response.output_") => {
                            state.finished = true;
                            return Some((
                                Err(response_event_shape_fault(
                                    &frame.event_type,
                                    ResponseEventReason::UnsupportedOutput,
                                    ProviderFault::model_rejected(
                                        "OpenAI API returned an unsupported output event",
                                    ),
                                )),
                                state,
                            ));
                        }
                        _ => {}
                    }
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
    reason = "this impl preserves the legacy Responses ProviderPort contract"
)]
impl ProviderPort for AsyncOpenAiResponsesProvider {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.ensure_legacy_mode()?;
        let staged_tool_outputs = self.staged_tool_output_receipt()?;
        let prepared = OpenAiContinuation::prepare_bounded(
            self.continuation.as_ref(),
            self.artifact_binding.as_ref(),
            &staged_tool_outputs,
            projection,
            &self.encoder,
            self.max_responses_serialized_request_body_bytes,
        )
        .map_err(|error| {
            if error.is_serialized_request_body_limit() {
                serialized_request_body_limit_fault()
            } else {
                ProviderFault::model_rejected("OpenAI continuation preparation failed")
                    .with_code(ProviderFaultCode::RequestPreparation)
            }
        })?;
        if prepared.request_body.len() > self.max_responses_serialized_request_body_bytes {
            return Err(serialized_request_body_limit_fault());
        }
        self.start_stream(prepared).await
    }
}

#[cfg(feature = "legacy-provider-port")]
struct OpenAiStreamState<'a, S> {
    stream: S,
    completed_output: Option<CompletedOpenAiOutput>,
    continuation_slot: &'a mut Option<OpenAiContinuation>,
    artifact_binding_slot: &'a mut Option<OpenAiInlineArtifactBinding>,
    response_completed: bool,
    terminal_published: bool,
    finished: bool,
    last_sequence: Option<u64>,
    output_ledger: OpenAiOutputLedger,
    native_tools: NativeToolLedger,
    output_text_bytes: usize,
    encoder: CodexHttpV1Encoder,
    read_timeout: Duration,
    max_sse_event_bytes: usize,
    max_output_text_bytes: usize,
    max_serialized_request_body_bytes: usize,
    tool_output_sink: Arc<dyn ToolOutputSink>,
    response_usage_observer: Option<Arc<ResponseUsageObserver>>,
}

struct SseWireLimiter {
    max_event_bytes: usize,
    event_bytes: usize,
    line_has_content: bool,
    pending_carriage_return: bool,
}

impl SseWireLimiter {
    const fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            event_bytes: 0,
            line_has_content: false,
            pending_carriage_return: false,
        }
    }

    fn observe(&mut self, bytes: &[u8]) -> Result<(), OpenAiBodyStreamFault> {
        for byte in bytes {
            if self.pending_carriage_return {
                if *byte == b'\n' {
                    self.add_wire_byte()?;
                    self.finish_line();
                    self.pending_carriage_return = false;
                    continue;
                }
                self.finish_line();
                self.pending_carriage_return = false;
            }

            self.add_wire_byte()?;
            match *byte {
                b'\r' => self.pending_carriage_return = true,
                b'\n' => self.finish_line(),
                _ => self.line_has_content = true,
            }
        }
        Ok(())
    }

    fn add_wire_byte(&mut self) -> Result<(), OpenAiBodyStreamFault> {
        self.event_bytes = self
            .event_bytes
            .checked_add(1)
            .ok_or(OpenAiBodyStreamFault::EventLimit)?;
        if self.event_bytes > self.max_event_bytes {
            return Err(OpenAiBodyStreamFault::EventLimit);
        }
        Ok(())
    }

    fn finish_line(&mut self) {
        if !self.line_has_content {
            self.event_bytes = 0;
        }
        self.line_has_content = false;
    }
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

#[cfg(feature = "legacy-provider-port")]
fn post_handoff_fault<'a>(fault: ProviderFault) -> ProviderEventStream<'a> {
    Box::pin(futures::stream::once(async move { Err(fault) }))
}

#[cfg(feature = "legacy-provider-port")]
fn finish_openai_stream<S>(
    state: &mut OpenAiStreamState<'_, S>,
) -> Result<ProviderEvent, ProviderFault> {
    if !state.response_completed {
        return Err(response_stream_completion_fault(
            ProviderFault::retryable_transport("OpenAI stream ended without response.completed"),
        ));
    }
    let completed = state.completed_output.as_ref().ok_or_else(|| {
        response_stream_completion_fault(ProviderFault::model_rejected(
            "OpenAI response.completed has no reconciled output",
        ))
    })?;
    let candidate = state.continuation_slot.as_ref().cloned().ok_or_else(|| {
        ProviderFault::model_rejected("OpenAI continuation candidate is unavailable")
    })?;
    let output_items = completed.output_items.clone();
    let wire_items = completed.wire_items.clone();
    let final_text = completed.final_text.clone();
    let mut candidate = candidate
        .with_outputs(output_items, wire_items)
        .map_err(|_| {
            ProviderFault::model_rejected("OpenAI output could not be retained for continuation")
        })?;
    candidate.complete_open_outputs().map_err(|_| {
        ProviderFault::model_rejected("OpenAI output lifecycle left open local history")
    })?;
    if candidate.has_pending_tool_calls() {
        *state.continuation_slot = Some(candidate);
        return Ok(ProviderEvent::Text(TextTurnEvent::TextComplete(final_text)));
    }
    let artifact_binding = state.artifact_binding_slot.as_ref().ok_or_else(|| {
        ProviderFault::model_rejected("OpenAI continuation artifact binding is unavailable")
            .with_code(ProviderFaultCode::RequestPreparation)
    })?;
    let retained_snapshot = candidate
        .encode_retained_snapshot_bounded(
            artifact_binding,
            &state.encoder,
            state.max_serialized_request_body_bytes,
        )
        .map_err(|error| {
            if error.is_serialized_request_body_limit() {
                serialized_request_body_limit_fault()
            } else {
                ProviderFault::model_rejected(
                    "OpenAI retained continuation snapshot preparation failed",
                )
                .with_code(ProviderFaultCode::RequestPreparation)
            }
        })?;
    if retained_snapshot.len() > state.max_serialized_request_body_bytes {
        return Err(serialized_request_body_limit_fault());
    }
    *state.continuation_slot = Some(candidate);
    Ok(ProviderEvent::Text(TextTurnEvent::TextComplete(final_text)))
}

#[cfg(feature = "legacy-provider-port")]
fn finish_accepted_response<S>(
    state: &mut OpenAiStreamState<'_, S>,
    payload: &Map<String, Value>,
    completed_output: CompletedOpenAiOutput,
) -> Result<ProviderEvent, ProviderFault> {
    if completed_output.output_text_bytes > state.max_output_text_bytes {
        return Err(output_limit_fault(OpenAiApi::Responses));
    }
    let response_usage = validated_response_usage(payload)?;
    state.completed_output = Some(completed_output);
    state.response_completed = true;
    let event = finish_openai_stream(state)?;
    observe_response_usage(&state.response_usage_observer, response_usage);
    state.terminal_published = true;
    Ok(event)
}

#[cfg(feature = "legacy-provider-port")]
impl<S> Drop for OpenAiStreamState<'_, S> {
    fn drop(&mut self) {
        if self.terminal_published {
            return;
        }
        if let Some(continuation) = self.continuation_slot.as_mut() {
            continuation.abort_open_outputs();
        }
    }
}

#[cfg(feature = "legacy-provider-port")]
fn advance_sequence(
    previous: &mut Option<u64>,
    event_type: &str,
    payload: &Map<String, Value>,
) -> Result<(), ProviderFault> {
    let sequence = payload
        .get("sequence_number")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            response_event_shape_fault(
                event_type,
                ResponseEventReason::Sequence,
                ProviderFault::retryable_transport(
                    "OpenAI streaming event is missing sequence_number",
                ),
            )
        })?;
    if previous.is_some_and(|previous| sequence <= previous) {
        return Err(response_event_shape_fault(
            event_type,
            ResponseEventReason::Sequence,
            ProviderFault::model_rejected(
                "OpenAI streaming sequence_number is not strictly increasing",
            ),
        ));
    }
    *previous = Some(sequence);
    Ok(())
}

#[derive(Default)]
struct NativeToolLedger {
    calls: BTreeMap<u64, NativeToolCallLifecycle>,
}

struct NativeToolCallLifecycle {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    arguments_done: bool,
    sealed: bool,
}

impl NativeToolLedger {
    fn added(&mut self, payload: &Map<String, Value>) -> Result<(), ProviderFault> {
        let output_index = native_output_index(payload)?;
        let item = native_item(payload)?;
        if item.get("status").and_then(Value::as_str) != Some("in_progress")
            || item.get("arguments").and_then(Value::as_str) != Some("")
        {
            return Err(native_tool_fault());
        }
        let lifecycle = NativeToolCallLifecycle {
            item_id: native_string(item, "id")?,
            call_id: native_string(item, "call_id")?,
            name: native_string(item, "name")?,
            arguments: String::new(),
            arguments_done: false,
            sealed: false,
        };
        if self.calls.insert(output_index, lifecycle).is_some() {
            return Err(native_tool_fault());
        }
        Ok(())
    }

    fn delta(&mut self, payload: &Map<String, Value>) -> Result<(), ProviderFault> {
        let call = self.call_mut(payload)?;
        if call.arguments_done {
            return Err(native_tool_fault());
        }
        call.arguments
            .push_str(&native_payload_string(payload, "delta")?);
        Ok(())
    }

    fn arguments_done(&mut self, payload: &Map<String, Value>) -> Result<(), ProviderFault> {
        let arguments = native_payload_string(payload, "arguments")?;
        let call = self.call_mut(payload)?;
        if call.arguments_done || call.arguments != arguments {
            return Err(native_tool_fault());
        }
        call.arguments_done = true;
        Ok(())
    }

    fn item_done(&mut self, payload: &Map<String, Value>) -> Result<ToolCall, ProviderFault> {
        let output_index = native_output_index(payload)?;
        let item = native_item(payload)?;
        let call = self
            .calls
            .get_mut(&output_index)
            .ok_or_else(native_tool_fault)?;
        if call.sealed
            || !call.arguments_done
            || item.get("status").and_then(Value::as_str) != Some("completed")
            || native_string(item, "id")? != call.item_id
            || native_string(item, "call_id")? != call.call_id
            || native_string(item, "name")? != call.name
            || native_string(item, "arguments")? != call.arguments
        {
            return Err(native_tool_fault());
        }
        call.sealed = true;
        ToolCall::new(&call.call_id, &call.name, &call.arguments).map_err(|_| native_tool_fault())
    }

    fn completed(&self, payload: &Map<String, Value>) -> Result<(), ProviderFault> {
        let output = payload
            .get("response")
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array)
            .ok_or_else(native_tool_fault)?;
        let mut terminal_calls = 0_usize;
        let mut terminal_ids = std::collections::HashSet::new();
        let mut terminal_call_ids = std::collections::HashSet::new();
        for (ordinal, item) in output.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                continue;
            }
            let ordinal = u64::try_from(ordinal).map_err(|_| native_tool_fault())?;
            let call = self.calls.get(&ordinal).ok_or_else(native_tool_fault)?;
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(native_tool_fault)?;
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(native_tool_fault)?;
            if !terminal_ids.insert(item_id) || !terminal_call_ids.insert(call_id) {
                return Err(native_tool_fault());
            }
            if !call.sealed
                || item.get("status").and_then(Value::as_str) != Some("completed")
                || item_id != call.item_id
                || call_id != call.call_id
                || item.get("name").and_then(Value::as_str) != Some(&call.name)
                || item.get("arguments").and_then(Value::as_str) != Some(&call.arguments)
            {
                return Err(native_tool_fault());
            }
            terminal_calls += 1;
        }
        if terminal_calls != self.calls.len() || self.calls.values().any(|call| !call.sealed) {
            return Err(native_tool_fault());
        }
        Ok(())
    }

    fn call_mut(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<&mut NativeToolCallLifecycle, ProviderFault> {
        let output_index = native_output_index(payload)?;
        let item_id = native_payload_string(payload, "item_id")?;
        let call = self
            .calls
            .get_mut(&output_index)
            .ok_or_else(native_tool_fault)?;
        if call.item_id != item_id {
            return Err(native_tool_fault());
        }
        Ok(call)
    }
}

fn native_tool_fault() -> ProviderFault {
    ProviderFault::model_rejected("OpenAI function call lifecycle is invalid")
}

fn native_output_index(payload: &Map<String, Value>) -> Result<u64, ProviderFault> {
    payload
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or_else(native_tool_fault)
}

fn native_payload_string(
    payload: &Map<String, Value>,
    field: &str,
) -> Result<String, ProviderFault> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(native_tool_fault)
}

fn native_item(payload: &Map<String, Value>) -> Result<&Map<String, Value>, ProviderFault> {
    let item = payload
        .get("item")
        .and_then(Value::as_object)
        .ok_or_else(native_tool_fault)?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return Err(native_tool_fault());
    }
    Ok(item)
}

fn native_string(item: &Map<String, Value>, field: &str) -> Result<String, ProviderFault> {
    item.get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(native_tool_fault)
}

fn native_function_call(payload: &Map<String, Value>) -> Option<Result<ToolCall, ProviderFault>> {
    let item = payload.get("item")?.as_object()?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    Some(
        ToolCall::new(
            item.get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            item.get("name").and_then(Value::as_str).unwrap_or_default(),
            item.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .map_err(|_| ProviderFault::model_rejected("OpenAI function call has an invalid shape")),
    )
}

fn has_native_function_call_completed(payload: &Map<String, Value>) -> bool {
    payload
        .get("response")
        .and_then(|response| response.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        })
}

#[cfg(all(test, feature = "legacy-provider-port"))]
mod terminal_precedence_tests {
    use super::*;
    use crate::provider::codex_http_v1::CodexHttpV1Options;

    #[test]
    fn terminal_output_limit_precedes_malformed_usage() {
        let mut continuation = None;
        let mut artifact_binding = None;
        let options = CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap();
        let tool_output_sink: Arc<dyn ToolOutputSink> =
            Arc::new(Mutex::new(ToolOutputStaging::default()));
        let mut state = OpenAiStreamState {
            stream: (),
            completed_output: None,
            continuation_slot: &mut continuation,
            artifact_binding_slot: &mut artifact_binding,
            response_completed: false,
            terminal_published: false,
            finished: false,
            last_sequence: None,
            output_ledger: OpenAiOutputLedger::default(),
            native_tools: NativeToolLedger::default(),
            output_text_bytes: 0,
            encoder: CodexHttpV1Encoder::new(options),
            read_timeout: Duration::from_secs(1),
            max_sse_event_bytes: 1024,
            max_output_text_bytes: 8,
            max_serialized_request_body_bytes: 1024,
            tool_output_sink,
            response_usage_observer: None,
        };
        let payload = serde_json::json!({
            "response": {
                "usage": {
                    "input_tokens": -1,
                    "private_usage_note": "must-not-be-exposed"
                }
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        let completed_output = CompletedOpenAiOutput {
            output_items: Vec::new(),
            wire_items: Vec::new(),
            final_text: "123456789".to_owned(),
            output_text_bytes: 9,
        };

        let fault = finish_accepted_response(&mut state, &payload, completed_output).unwrap_err();

        assert_eq!(fault.code(), ProviderFaultCode::OutputLimit);
        assert!(!format!("{fault:?}\n{fault}").contains("must-not-be-exposed"));
    }
}

#[cfg(test)]
mod native_tool_tests {
    use super::*;

    fn payload(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn added() -> Map<String, Value> {
        payload(serde_json::json!({
            "output_index": 0,
            "item": {"id": "item-1", "type": "function_call", "status": "in_progress", "call_id": "call-1", "name": "lookup", "arguments": ""}
        }))
    }

    #[test]
    fn native_tool_lifecycle_seals_only_after_matching_arguments_done() {
        let mut ledger = NativeToolLedger::default();
        ledger.added(&added()).unwrap();
        ledger
            .delta(&payload(serde_json::json!({
                "output_index": 0, "item_id": "item-1", "delta": "{\"q\":"
            })))
            .unwrap();
        ledger
            .delta(&payload(serde_json::json!({
                "output_index": 0, "item_id": "item-1", "delta": "\"x\"}"
            })))
            .unwrap();
        ledger
            .arguments_done(&payload(serde_json::json!({
                "output_index": 0, "item_id": "item-1", "arguments": "{\"q\":\"x\"}"
            })))
            .unwrap();
        let call = ledger.item_done(&payload(serde_json::json!({
            "output_index": 0,
            "item": {"id": "item-1", "type": "function_call", "status": "completed", "call_id": "call-1", "name": "lookup", "arguments": "{\"q\":\"x\"}"}
        }))).unwrap();
        assert_eq!(call.call_id(), "call-1");
    }

    #[test]
    fn native_tool_done_without_lifecycle_is_rejected() {
        let mut ledger = NativeToolLedger::default();
        assert!(ledger.item_done(&payload(serde_json::json!({
            "output_index": 0,
            "item": {"id": "item-1", "type": "function_call", "status": "completed", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
        }))).is_err());
    }

    #[test]
    fn native_tool_terminal_output_must_match_the_lifecycle_ordinal() {
        let mut ledger = NativeToolLedger::default();
        ledger.added(&added()).unwrap();
        ledger
            .delta(&payload(serde_json::json!({
                "output_index": 0, "item_id": "item-1", "delta": "{}"
            })))
            .unwrap();
        ledger
            .arguments_done(&payload(serde_json::json!({
                "output_index": 0, "item_id": "item-1", "arguments": "{}"
            })))
            .unwrap();
        ledger
            .item_done(&payload(serde_json::json!({
                "output_index": 0,
                "item": {"id": "item-1", "type": "function_call", "status": "completed", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
            })))
            .unwrap();
        assert!(ledger
            .completed(&payload(serde_json::json!({
                "response": {"output": [
                    {"id": "message", "type": "message", "status": "completed", "role": "assistant", "content": []},
                    {"id": "item-1", "type": "function_call", "status": "completed", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
                ]}
            })))
            .is_err());
    }

    #[test]
    #[cfg(feature = "legacy-provider-port")]
    fn provider_owned_tool_result_staging_rejects_duplicate_and_orphan_output() {
        let staging = Mutex::new(ToolOutputStaging::default());
        staging.register(3, "call-1").unwrap();
        let first = ToolCall::new("call-1", "lookup", "{}").unwrap();
        let orphan = ToolCall::new("call-2", "lookup", "{}").unwrap();
        assert!(staging.register(3, "replacement-call").is_err());
        assert!(staging.accept(3, first.output("ok")).is_ok());
        assert!(staging.accept(3, first.output("again")).is_err());
        assert!(staging.accept(4, orphan.output("orphan")).is_err());
    }

    #[test]
    #[cfg(feature = "legacy-provider-port")]
    fn tool_result_staging_consumes_only_an_exact_full_table_receipt() {
        let staging = Mutex::new(ToolOutputStaging::default());
        let first = ToolCall::new("call-1", "first", "{}").unwrap();
        let second = ToolCall::new("call-2", "second", "{}").unwrap();
        staging.register(3, "call-1").unwrap();
        staging.register(7, "call-2").unwrap();
        let first_output = first.output("first-result");
        let second_output = second.output("second-result");
        staging.accept(3, first_output.clone()).unwrap();
        staging.accept(7, second_output.clone()).unwrap();

        assert!(staging
            .lock()
            .unwrap()
            .consume(&[(3, first_output.clone())])
            .is_err());
        assert_eq!(
            staging.lock().unwrap().ready().unwrap(),
            vec![(3, first_output.clone()), (7, second_output.clone())],
            "a subset receipt must not partially consume the staging table"
        );

        let extra = ToolCall::new("call-extra", "extra", "{}").unwrap();
        assert!(staging
            .lock()
            .unwrap()
            .consume(&[
                (3, first_output.clone()),
                (7, second_output.clone()),
                (9, extra.output("extra-result")),
            ])
            .is_err());
        assert_eq!(
            staging.lock().unwrap().ready().unwrap(),
            vec![(3, first_output), (7, second_output)],
            "an extra receipt entry must not consume the valid staged outputs"
        );
    }
}

#[derive(Deserialize)]
struct OpenAiWireEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(flatten)]
    payload: Map<String, Value>,
}

/// Sanitized OpenAI transport configuration or initialization failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncOpenAiConfigError {
    #[error("invalid OpenAI API base: {message}")]
    InvalidApiBase { message: String },
    #[error("OpenAI API key must be non-empty")]
    EmptyApiKey,
    #[error("OpenAI API key must contain only printable ASCII characters")]
    InvalidApiKey,
    #[error("OpenAI transport timeout {name} must be non-zero")]
    InvalidTimeout { name: &'static str },
    #[error("OpenAI response limit {name} must be non-zero")]
    InvalidResponseLimit { name: &'static str },
    #[error("OpenAI Responses serialized outbound request body limit must be non-zero")]
    InvalidSerializedRequestBodyLimit,
    #[error("OpenAI Chat Completions serialized outbound request body limit must be non-zero")]
    InvalidChatCompletionsSerializedRequestBodyLimit,
    #[error("OpenAI Responses Frame profile is invalid")]
    InvalidResponsesFrameProfile,
    #[error("OpenAI Chat Completions Frame profile is invalid")]
    InvalidChatCompletionsFrameProfile,
    #[error("OpenAI Responses target identity space is exhausted")]
    ResponsesTargetIdentityExhausted,
    #[error("OpenAI Chat Completions target identity space is exhausted")]
    ChatCompletionsTargetIdentityExhausted,
    #[error("OpenAI transport initialization failed")]
    TransportInitialization,
}

#[cfg(test)]
mod responses_frame_profile_tests {
    use super::*;
    use crate::component::execution::reaction::{
        ReactionPortFaultCode, ReactionPortFaultKind, ReactionPortFaultReason, TargetContinuity,
    };
    use crate::provider::codex_http_v1::CodexHttpV1Options;

    fn config() -> AsyncOpenAiTransportConfig {
        AsyncOpenAiTransportConfig::new("http://127.0.0.1:1/v1", "test-token").unwrap()
    }

    fn provider(config: AsyncOpenAiTransportConfig) -> AsyncOpenAiResponsesProvider {
        let identity =
            ProviderIdentity::new("openai", "codex-http-v1", 1, "frame-profile-test").unwrap();
        let options = CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap();
        AsyncOpenAiResponsesProvider::try_new(config, identity, CodexHttpV1Encoder::new(options))
            .unwrap()
    }

    fn assert_invalid(constraints: FrameConstraints) {
        let error = match config().with_responses_frame_constraints(constraints) {
            Ok(_) => panic!("invalid Responses Frame constraints were accepted"),
            Err(error) => error,
        };
        assert_eq!(error, AsyncOpenAiConfigError::InvalidResponsesFrameProfile);
    }

    #[test]
    fn responses_target_declares_exact_production_defaults() {
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
    fn responses_target_retains_custom_frame_constraints() {
        let constraints = FrameConstraints {
            max_frame_bytes: 32 * 1024,
            max_component_bytes: 8 * 1024,
            context_window_tokens: Some(128_000),
            reserved_output_tokens: Some(8_192),
        };
        let provider = provider(
            config()
                .with_responses_frame_constraints(constraints.clone())
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
    fn responses_frame_constraints_reject_every_invalid_profile_shape() {
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
            ..valid.clone()
        });
        assert_invalid(FrameConstraints {
            max_frame_bytes: 64,
            max_component_bytes: 64,
            ..valid
        });
    }

    #[test]
    fn responses_target_declaration_is_idempotent() {
        let provider = provider(config());

        assert_eq!(
            provider.reaction_target.declaration().unwrap(),
            provider.reaction_target.declaration().unwrap()
        );
    }

    #[test]
    fn responses_provider_instances_have_distinct_target_identities() {
        let first = provider(config());
        let second = provider(config());

        assert_ne!(
            first.reaction_target.declaration().unwrap().identity(),
            second.reaction_target.declaration().unwrap().identity()
        );
    }

    #[test]
    fn accepted_continuity_preserves_the_mount_stable_profile() {
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
        assert!(matches!(
            accepted.continuity(),
            TargetContinuity::Accepted {
                revision: accepted_revision,
                ..
            } if *accepted_revision == revision
        ));
    }

    #[test]
    fn exhausted_epoch_becomes_a_stable_terminal_declaration_fault() {
        let mut provider = provider(config());
        provider.reaction_target.epoch = TargetEpoch::new(NonZeroU64::new(u64::MAX).unwrap());

        provider.reaction_target.lose_continuity();

        for _ in 0..2 {
            let fault = provider.reaction_target.declaration().unwrap_err();
            assert_eq!(fault.kind(), ReactionPortFaultKind::Terminal);
            assert_eq!(fault.code(), ReactionPortFaultCode::Internal);
            assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
        }
    }
}
