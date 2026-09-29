//! Speculative provider-private compaction. Workers produce candidates; only
//! foreground handoff can install them. No worker owns application capabilities.

use std::{num::NonZeroU64, panic::resume_unwind, time::Duration};

use async_openai::config::{Config, OpenAIConfig};
use eventsource_stream::{EventStreamError, Eventsource};
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{sync::watch, task::JoinHandle};

use super::frame_request::{ResponsesFrameRequestFault, ResponsesFrameRequestState};
use super::{
    local_compaction::{build_compacted_history, summary_input, SummaryStream},
    sse_event_size, OpenAiWireEvent, SseWireLimiter,
};
use crate::{component::execution::reaction::Frame, provider::codex_http_v1::CodexHttpV1Encoder};

/// A byte-based heuristic, never an exact tokenizer count. Round up so a
/// nonempty context cannot have an estimate of zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextEstimate {
    pub bytes: usize,
    pub tokens: u64,
}

impl ContextEstimate {
    pub fn from_bytes(bytes: usize) -> Self {
        Self {
            bytes,
            tokens: bytes.div_ceil(4) as u64,
        }
    }
}

/// Inputs to a compaction policy. Requested context is the COMPLETE Component
/// envelope before diffing. Provider context is the complete prospective wire
/// request, including retained history, private artifacts, instructions and
/// tools. Their JSON encodings differ; both use the bytes/4 estimate.
#[derive(Debug, Clone, Copy)]
pub struct CompactionContext {
    pub requested_context: Option<ContextEstimate>,
    pub provider_context: ContextEstimate,
    pub compactable_context: ContextEstimate,
    /// Resolved model capacity, declared by the provider before mounting.
    pub context_window_tokens: NonZeroU64,
    pub reserved_output_tokens: Option<u64>,
}

/// Synchronous, bounded admission policy, evaluated before transport handoff.
/// Returning true permits a background attempt; closure, size and lifecycle
/// checks still apply. User panics propagate on the caller's stack.
pub trait CompactionPolicy: Send + Sync {
    fn should_compact(&self, context: &CompactionContext) -> bool;
}

impl<F: Fn(&CompactionContext) -> bool + Send + Sync> CompactionPolicy for F {
    fn should_compact(&self, context: &CompactionContext) -> bool {
        self(context)
    }
}

/// Compact when requested context is strictly below provider context / N.
/// Default N=10; N=11 gives a slightly smaller trigger ratio. This controls
/// history growth relative to current needs, not an absolute context limit.
#[derive(Debug, Clone, Copy)]
pub struct ContextRatioPolicy {
    pub provider_context_multiple: NonZeroU64,
}

impl Default for ContextRatioPolicy {
    fn default() -> Self {
        Self {
            provider_context_multiple: NonZeroU64::new(10).unwrap(),
        }
    }
}

impl CompactionPolicy for ContextRatioPolicy {
    fn should_compact(&self, context: &CompactionContext) -> bool {
        context.requested_context.is_some_and(|requested| {
            u128::from(requested.tokens) * u128::from(self.provider_context_multiple.get())
                < u128::from(context.provider_context.tokens)
        })
    }
}

/// Compact at an absolute estimate, independently of the requested/provider
/// ratio. Default to 90% of (window - output reserve), and cap an explicit
/// threshold at that value. The provider always declares a resolved window.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenThresholdPolicy {
    pub token_limit: Option<NonZeroU64>,
}

impl TokenThresholdPolicy {
    pub fn threshold_tokens(&self, context: &CompactionContext) -> u64 {
        let input_budget = context
            .context_window_tokens
            .get()
            .saturating_sub(context.reserved_output_tokens.unwrap_or(0));
        let window_threshold = (u128::from(input_budget) * 9 / 10) as u64;
        self.token_limit
            .map_or(window_threshold, |limit| limit.get().min(window_threshold))
    }
}

impl CompactionPolicy for TokenThresholdPolicy {
    fn should_compact(&self, context: &CompactionContext) -> bool {
        context.provider_context.tokens >= self.threshold_tokens(context)
    }
}

/// The native Responses provider's default policy: compact at the token
/// threshold OR when requested context is below one tenth of provider context.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCompactionPolicy {
    pub token_threshold: TokenThresholdPolicy,
    pub context_ratio: ContextRatioPolicy,
}

impl CompactionPolicy for DefaultCompactionPolicy {
    fn should_compact(&self, context: &CompactionContext) -> bool {
        self.token_threshold.should_compact(context) || self.context_ratio.should_compact(context)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParallelCompactionFault {
    Transport,
    HttpStatus(u16),
    Protocol,
    Limit,
    Timeout,
    NoReduction,
    RuntimeUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelCompactionPhase {
    Idle,
    Running,
    Ready,
    Installed,
    Discarded,
    Failed,
    Cancelled,
}

/// Ready is a computed candidate; Installed means a foreground Frame has
/// handed it off. Status contains no provider payload or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParallelCompactionStatus {
    pub attempt: u64,
    pub phase: ParallelCompactionPhase,
    pub fault: Option<ParallelCompactionFault>,
}

/// Obtain before moving the provider into Application. Observing never starts
/// reactions. Updates may coalesce; the latest status remains available.
#[derive(Clone)]
pub struct ParallelCompactionMonitor {
    receiver: watch::Receiver<ParallelCompactionStatus>,
}

impl ParallelCompactionMonitor {
    pub fn status(&self) -> ParallelCompactionStatus {
        *self.receiver.borrow()
    }

    /// None after the provider and its worker have both gone away.
    pub async fn changed(&mut self) -> Option<ParallelCompactionStatus> {
        self.receiver.changed().await.ok()?;
        Some(*self.receiver.borrow_and_update())
    }
}

#[derive(Clone)]
pub(super) struct CompactionSource {
    pub(super) input: Vec<Value>,
    pub(super) instructions: String,
    pub(super) input_bytes: usize,
    fingerprint: [u8; 32],
}

impl CompactionSource {
    pub(super) fn new(input: Vec<Value>, instructions: String) -> Option<Self> {
        let encoded = serde_json::to_vec(&input).ok()?;
        let mut digest = Sha256::new();
        digest.update(b"agentview:parallel-compaction-source:v1");
        digest.update((instructions.len() as u64).to_be_bytes());
        digest.update(instructions.as_bytes());
        digest.update(&encoded);
        Some(Self {
            input,
            instructions,
            input_bytes: encoded.len(),
            fingerprint: digest.finalize().into(),
        })
    }
}

struct Attempt {
    id: u64,
    source: CompactionSource,
    task: Option<JoinHandle<Result<Vec<Value>, ParallelCompactionFault>>>,
    output: Option<Vec<Value>>,
    cancelled: bool,
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub(super) struct ParallelCompaction {
    policy: Box<dyn CompactionPolicy>,
    status: watch::Sender<ParallelCompactionStatus>,
    attempt: Option<Attempt>,
    last_source: Option<[u8; 32]>,
}

pub(super) struct CompactionTransport {
    pub(super) client: reqwest::Client,
    pub(super) config: OpenAIConfig,
    pub(super) encoder: CodexHttpV1Encoder,
    pub(super) request_limit: usize,
    pub(super) response_limit: usize,
    pub(super) event_limit: usize,
    pub(super) text_limit: usize,
    pub(super) read_timeout: Duration,
    pub(super) context_window_tokens: NonZeroU64,
    pub(super) reserved_output_tokens: u64,
}

impl CompactionTransport {
    fn summary_request(
        &self,
        source: &CompactionSource,
    ) -> Result<reqwest::Request, ParallelCompactionFault> {
        let input = summary_input(&source.input);
        let body = self
            .encoder
            .encode_frame_request_bounded(&input, &source.instructions, &[], self.request_limit)
            .map_err(|_| ParallelCompactionFault::Limit)?;
        // Keep the ordinary request options, including omission of an
        // unspecified output limit. Do not ask the server for a model-specific
        // output capacity just because the context window has space for it.
        let input_tokens = ContextEstimate::from_bytes(body.len()).tokens;
        let output_reserve = self
            .reserved_output_tokens
            .max(u64::from(self.encoder.max_output_tokens().unwrap_or(0)));
        if input_tokens >= self.context_window_tokens.get()
            || u128::from(input_tokens) + u128::from(output_reserve)
                > u128::from(self.context_window_tokens.get())
        {
            return Err(ParallelCompactionFault::Limit);
        }
        self.client
            .post(self.config.url("/responses"))
            .headers(self.config.headers())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .build()
            .map_err(|_| ParallelCompactionFault::Transport)
    }
}

impl ParallelCompaction {
    pub(super) fn new(policy: impl CompactionPolicy + 'static) -> Self {
        let (status, _) = watch::channel(ParallelCompactionStatus {
            attempt: 0,
            phase: ParallelCompactionPhase::Idle,
            fault: None,
        });
        Self {
            policy: Box::new(policy),
            status,
            attempt: None,
            last_source: None,
        }
    }

    pub(super) fn monitor(&self) -> ParallelCompactionMonitor {
        ParallelCompactionMonitor {
            receiver: self.status.subscribe(),
        }
    }

    pub(super) fn replace_policy(&mut self, policy: impl CompactionPolicy + 'static) {
        self.policy = Box::new(policy);
    }

    pub(super) fn has_attempt(&self) -> bool {
        self.attempt.is_some()
    }

    fn phase(&self, phase: ParallelCompactionPhase) {
        self.status.send_modify(|status| {
            status.phase = phase;
            status.fault = None;
        });
    }

    fn refresh(&mut self) {
        let Some(attempt) = &mut self.attempt else {
            return;
        };
        let Some(task) = &mut attempt.task else {
            return;
        };
        if !task.is_finished() {
            return;
        }
        let Some(result) = (&mut *task).now_or_never() else {
            return;
        };
        let was_cancelled = attempt.cancelled;
        attempt.task = None;
        match result {
            Err(error) if error.is_panic() => {
                self.attempt = None;
                resume_unwind(error.into_panic());
            }
            Err(error) if error.is_cancelled() => {
                self.attempt = None;
                if !was_cancelled {
                    self.fail(ParallelCompactionFault::RuntimeUnavailable);
                }
            }
            Err(_) => {
                self.attempt = None;
                self.fail(ParallelCompactionFault::RuntimeUnavailable);
            }
            Ok(Ok(output)) if !was_cancelled => attempt.output = Some(output),
            _ => {
                self.attempt = None;
            }
        }
    }

    /// Disposable state only. Preparing it never consumes the Ready candidate.
    pub(super) fn prepare_base(
        &mut self,
        previous: Option<&ResponsesFrameRequestState>,
        frame: &Frame,
        encoder: &CodexHttpV1Encoder,
    ) -> Result<Option<(u64, ResponsesFrameRequestState)>, ResponsesFrameRequestFault> {
        self.refresh();
        let Some(attempt) = &self.attempt else {
            return Ok(None);
        };
        if attempt.cancelled {
            return Ok(None);
        }
        let Some(output) = &attempt.output else {
            return Ok(None);
        };
        if let Some(previous) = previous {
            if let Some(state) =
                previous.with_parallel_compaction(&attempt.source, output, frame, encoder)?
            {
                return Ok(Some((attempt.id, state)));
            }
        }
        self.attempt = None;
        self.phase(ParallelCompactionPhase::Discarded);
        Ok(None)
    }

    pub(super) fn select_source(
        &self,
        source: Option<CompactionSource>,
        context: &CompactionContext,
    ) -> Option<CompactionSource> {
        if self.attempt.is_some() {
            return None;
        }
        let source = source?;
        if self.last_source == Some(source.fingerprint) {
            return None;
        }
        self.policy.should_compact(context).then_some(source)
    }

    /// Forced admission bypasses only the background policy. It still shares
    /// the one-worker and one-attempt-per-source guards.
    pub(super) fn select_forced_source(
        &self,
        source: Option<CompactionSource>,
    ) -> Option<CompactionSource> {
        if self.attempt.is_some() {
            return None;
        }
        let source = source?;
        (self.last_source != Some(source.fingerprint)).then_some(source)
    }

    /// Waits only for the single active candidate. Timeout aborts this source
    /// and leaves `last_source` intact, so a caller cannot loop forever by
    /// repeatedly forcing the same accepted prefix.
    pub(super) async fn wait_ready(&mut self, wait_timeout: Duration) -> bool {
        self.refresh();
        if self
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.output.is_some() && !attempt.cancelled)
        {
            return true;
        }
        let result = {
            let Some(task) = self
                .attempt
                .as_mut()
                .and_then(|attempt| attempt.task.as_mut())
            else {
                return false;
            };
            tokio::time::timeout(wait_timeout, task).await
        };
        match result {
            Ok(Ok(Ok(output))) => {
                let Some(attempt) = &mut self.attempt else {
                    return false;
                };
                attempt.task = None;
                if attempt.cancelled {
                    self.attempt = None;
                    return false;
                }
                attempt.output = Some(output);
                true
            }
            Ok(Ok(Err(_))) => {
                self.attempt = None;
                false
            }
            Ok(Err(error)) if error.is_panic() => {
                if let Some(attempt) = &mut self.attempt {
                    attempt.task = None;
                }
                self.attempt = None;
                resume_unwind(error.into_panic());
            }
            Ok(Err(error)) if error.is_cancelled() => {
                let was_cancelled = self
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.cancelled);
                self.attempt = None;
                if !was_cancelled {
                    self.fail(ParallelCompactionFault::RuntimeUnavailable);
                }
                false
            }
            Ok(Err(_)) => {
                self.attempt = None;
                self.fail(ParallelCompactionFault::RuntimeUnavailable);
                false
            }
            Err(_) => {
                if let Some(attempt) = &mut self.attempt {
                    attempt.cancelled = true;
                    if let Some(task) = &attempt.task {
                        task.abort();
                    }
                }
                self.fail(ParallelCompactionFault::Timeout);
                let joined = {
                    let Some(task) = self
                        .attempt
                        .as_mut()
                        .and_then(|attempt| attempt.task.as_mut())
                    else {
                        return false;
                    };
                    (&mut *task).await
                };
                if let Err(error) = joined {
                    if error.is_panic() {
                        if let Some(attempt) = &mut self.attempt {
                            attempt.task = None;
                        }
                        self.attempt = None;
                        resume_unwind(error.into_panic());
                    }
                }
                self.attempt = None;
                false
            }
        }
    }

    pub(super) fn installed(&mut self, id: u64) {
        if self
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.id == id)
        {
            self.attempt = None;
            self.phase(ParallelCompactionPhase::Installed);
        }
    }

    /// The worker is independent of the foreground stream and cannot mutate its
    /// accepted state. Background callers start after handoff; forced callers
    /// may start before handoff and must wait for a validated candidate.
    pub(super) fn start(&mut self, source: CompactionSource, transport: CompactionTransport) {
        if self.attempt.is_some() {
            return;
        }
        let Some(id) = self.status.borrow().attempt.checked_add(1) else {
            return;
        };
        self.last_source = Some(source.fingerprint);
        self.status.send_replace(ParallelCompactionStatus {
            attempt: id,
            phase: ParallelCompactionPhase::Running,
            fault: None,
        });
        let request = transport.summary_request(&source);
        let (request, runtime) = match (request, tokio::runtime::Handle::try_current()) {
            (Ok(request), Ok(runtime)) => (request, runtime),
            (Err(fault), _) => {
                self.fail(fault);
                return;
            }
            (_, Err(_)) => {
                self.fail(ParallelCompactionFault::RuntimeUnavailable);
                return;
            }
        };
        let status = self.status.clone();
        let summary_source = source.clone();
        let task = runtime.spawn(async move {
            let result = compact(transport, request, summary_source).await;
            status.send_if_modified(|status| {
                if status.attempt != id || status.phase != ParallelCompactionPhase::Running {
                    return false;
                }
                status.phase = if result.is_ok() {
                    ParallelCompactionPhase::Ready
                } else {
                    ParallelCompactionPhase::Failed
                };
                status.fault = result.as_ref().err().copied();
                true
            });
            result
        });
        self.attempt = Some(Attempt {
            id,
            source,
            task: Some(task),
            output: None,
            cancelled: false,
        });
    }

    fn fail(&self, fault: ParallelCompactionFault) {
        self.status.send_modify(|status| {
            status.phase = ParallelCompactionPhase::Failed;
            status.fault = Some(fault);
        });
    }

    pub(super) fn reset(&mut self) {
        if let Some(attempt) = &mut self.attempt {
            attempt.cancelled = true;
            attempt.output = None;
            if let Some(task) = &attempt.task {
                task.abort();
            } else {
                self.attempt = None;
            }
            self.phase(ParallelCompactionPhase::Cancelled);
        }
        self.last_source = None;
    }

    pub(super) async fn shutdown(&mut self) {
        self.reset();
        if let Some(mut attempt) = self.attempt.take() {
            if let Some(task) = attempt.task.take() {
                if let Err(error) = task.await {
                    if error.is_panic() {
                        resume_unwind(error.into_panic());
                    }
                }
            }
        }
    }
}

impl Drop for ParallelCompaction {
    fn drop(&mut self) {
        self.reset();
    }
}

async fn compact(
    transport: CompactionTransport,
    request: reqwest::Request,
    source: CompactionSource,
) -> Result<Vec<Value>, ParallelCompactionFault> {
    let response = transport
        .client
        .execute(request)
        .await
        .map_err(transport_fault)?;
    if !response.status().is_success() {
        return Err(ParallelCompactionFault::HttpStatus(
            response.status().as_u16(),
        ));
    }
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        return Err(ParallelCompactionFault::Protocol);
    }
    let limit = transport.response_limit;
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ParallelCompactionFault::Limit);
    }
    let mut wire_bytes = 0_usize;
    let mut event_limiter = SseWireLimiter::new(transport.event_limit);
    let stream = response
        .bytes_stream()
        .map(move |chunk| {
            let chunk = chunk.map_err(transport_fault)?;
            wire_bytes = wire_bytes
                .checked_add(chunk.len())
                .ok_or(ParallelCompactionFault::Limit)?;
            if wire_bytes > limit {
                return Err(ParallelCompactionFault::Limit);
            }
            event_limiter
                .observe(&chunk)
                .map_err(|_| ParallelCompactionFault::Limit)?;
            Ok(chunk)
        })
        .eventsource();
    tokio::pin!(stream);
    let mut summary = SummaryStream::default();
    while let Some(event) = tokio::time::timeout(transport.read_timeout, stream.next())
        .await
        .map_err(|_| ParallelCompactionFault::Timeout)?
    {
        let event = event.map_err(|error| match error {
            EventStreamError::Transport(fault) => fault,
            _ => ParallelCompactionFault::Protocol,
        })?;
        if sse_event_size(&event).is_none_or(|size| size > transport.event_limit) {
            return Err(ParallelCompactionFault::Limit);
        }
        if event.data == "[DONE]" {
            return Err(ParallelCompactionFault::Protocol);
        }
        let frame: OpenAiWireEvent =
            serde_json::from_str(&event.data).map_err(|_| ParallelCompactionFault::Protocol)?;
        if let Some(text) = summary.event(frame, transport.text_limit)? {
            return build_compacted_history(
                &source.input,
                &text,
                transport.context_window_tokens.get(),
            );
        }
    }
    Err(ParallelCompactionFault::Protocol)
}

fn transport_fault(error: reqwest::Error) -> ParallelCompactionFault {
    if error.is_timeout() {
        ParallelCompactionFault::Timeout
    } else {
        ParallelCompactionFault::Transport
    }
}

#[cfg(test)]
mod tests {
    use std::{panic::AssertUnwindSafe, sync::Arc};

    use serde_json::json;
    use tokio::sync::{oneshot, Semaphore};

    use super::*;

    fn context(requested_bytes: usize, provider_bytes: usize) -> CompactionContext {
        CompactionContext {
            requested_context: Some(ContextEstimate::from_bytes(requested_bytes)),
            provider_context: ContextEstimate::from_bytes(provider_bytes),
            compactable_context: ContextEstimate::from_bytes(provider_bytes),
            context_window_tokens: NonZeroU64::new(250_000).unwrap(),
            reserved_output_tokens: None,
        }
    }

    #[test]
    fn ratio_is_strict_and_uses_rounded_up_byte_estimates() {
        let policy = ContextRatioPolicy::default();
        assert_eq!(ContextEstimate::from_bytes(0).tokens, 0);
        assert_eq!(ContextEstimate::from_bytes(5).tokens, 2);
        assert!(!policy.should_compact(&context(5, 80)));
        assert!(policy.should_compact(&context(5, 81)));
        let smaller = ContextRatioPolicy {
            provider_context_multiple: NonZeroU64::new(11).unwrap(),
        };
        assert!(!smaller.should_compact(&context(5, 88)));
        assert!(smaller.should_compact(&context(5, 89)));
    }

    #[test]
    fn ratio_handles_missing_context_and_large_values_without_overflow() {
        let mut context = context(0, 0);
        let policy = ContextRatioPolicy::default();
        assert!(!policy.should_compact(&context));
        context.requested_context = None;
        context.provider_context.tokens = u64::MAX;
        assert!(!policy.should_compact(&context));
        context.requested_context = Some(ContextEstimate {
            bytes: usize::MAX,
            tokens: u64::MAX,
        });
        assert!(!ContextRatioPolicy {
            provider_context_multiple: NonZeroU64::new(u64::MAX).unwrap(),
        }
        .should_compact(&context));
    }

    #[test]
    fn token_threshold_has_an_inclusive_default_even_without_requested_context() {
        let policy = TokenThresholdPolicy::default();
        let mut context = context(0, 0);
        context.requested_context = None;
        assert_eq!(policy.threshold_tokens(&context), 225_000);
        context.provider_context = ContextEstimate::from_bytes(899_996);
        assert!(!policy.should_compact(&context));
        context.provider_context = ContextEstimate::from_bytes(899_997);
        assert!(policy.should_compact(&context));
    }

    #[test]
    fn threshold_uses_the_configured_window_and_reserves_output_headroom() {
        let mut context = context(0, 0);
        context.context_window_tokens = NonZeroU64::new(128_000).unwrap();
        context.reserved_output_tokens = Some(8_000);
        let policy = TokenThresholdPolicy::default();
        assert_eq!(policy.threshold_tokens(&context), 108_000);
        let override_policy = TokenThresholdPolicy {
            token_limit: NonZeroU64::new(80_000),
        };
        assert_eq!(override_policy.threshold_tokens(&context), 80_000);
        let higher_policy = TokenThresholdPolicy {
            token_limit: NonZeroU64::new(500_000),
        };
        assert_eq!(higher_policy.threshold_tokens(&context), 108_000);
        context.context_window_tokens = NonZeroU64::new(1_000_000).unwrap();
        context.reserved_output_tokens = None;
        assert_eq!(policy.threshold_tokens(&context), 900_000);
        assert_eq!(higher_policy.threshold_tokens(&context), 500_000);
    }

    #[test]
    fn threshold_arithmetic_cannot_wrap_or_overflow() {
        let mut context = context(0, 0);
        context.context_window_tokens = NonZeroU64::new(u64::MAX).unwrap();
        let policy = TokenThresholdPolicy::default();
        assert_eq!(
            policy.threshold_tokens(&context),
            (u128::from(u64::MAX) * 9 / 10) as u64
        );
        context.context_window_tokens = NonZeroU64::new(8).unwrap();
        context.reserved_output_tokens = Some(9);
        assert_eq!(policy.threshold_tokens(&context), 0);
    }

    #[test]
    fn default_policy_compacts_for_either_growth_ratio_or_token_threshold() {
        let policy = DefaultCompactionPolicy::default();
        assert!(!policy.should_compact(&context(400, 4000)));
        assert!(policy.should_compact(&context(400, 4004)));
        let large_current = context(900_000, 900_000);
        assert!(!policy.context_ratio.should_compact(&large_current));
        assert!(policy.should_compact(&large_current));
        let overridden = DefaultCompactionPolicy {
            token_threshold: TokenThresholdPolicy {
                token_limit: NonZeroU64::new(300_000),
            },
            ..Default::default()
        };
        assert!(
            overridden.should_compact(&large_current),
            "an explicit threshold stays within the declared window"
        );
        assert!(overridden.should_compact(&context(400, 4004)));
    }

    fn artifact() -> Value {
        json!({
            "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"Earlier context summary", "annotations":[]}],
        })
    }

    #[test]
    fn summary_request_uses_current_model_and_reserves_output_within_its_window() {
        use crate::provider::{
            codex_http_v1::{CodexHttpV1Options, CodexReasoning},
            ModelSpec,
        };

        let mut transport = CompactionTransport {
            client: reqwest::Client::new(),
            config: OpenAIConfig::new()
                .with_api_base("http://localhost/v1")
                .with_api_key("test"),
            encoder: CodexHttpV1Encoder::new(
                CodexHttpV1Options::new(
                    ModelSpec::new("current-model", 128_000).unwrap(),
                    None,
                    Some(CodexReasoning::max_detailed()),
                    Some("current-cache-key"),
                )
                .unwrap()
                .with_max_output_tokens(64)
                .unwrap()
                .with_temperature(0.6)
                .unwrap(),
            ),
            request_limit: 100_000,
            response_limit: 10_000,
            event_limit: 10_000,
            text_limit: 5_000,
            read_timeout: Duration::from_secs(1),
            context_window_tokens: NonZeroU64::new(128_000).unwrap(),
            reserved_output_tokens: 128,
        };
        let source = CompactionSource::new(
            vec![json!({
                "type":"message", "role":"user", "content":"history ".repeat(512),
            })],
            "Business instructions".into(),
        )
        .unwrap();
        let request = transport.summary_request(&source).unwrap();
        assert_eq!(request.url().as_str(), "http://localhost/v1/responses");
        let bytes = request.body().unwrap().as_bytes().unwrap();
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(body["model"], "current-model");
        assert_eq!(
            body["reasoning"],
            json!({"effort":"max", "summary":"detailed"})
        );
        assert_eq!(body["input"], json!(summary_input(&source.input)));
        assert_eq!(body["instructions"], source.instructions);
        assert_eq!(body["temperature"], 0.6);
        assert_eq!(body["prompt_cache_key"], "current-cache-key");
        assert_eq!(body["max_output_tokens"], 64);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("context_management").is_none());
        assert!(body.get("previous_response_id").is_none());

        let input_tokens = ContextEstimate::from_bytes(bytes.len()).tokens;
        transport.context_window_tokens = NonZeroU64::new(input_tokens + 128).unwrap();
        assert!(transport.summary_request(&source).is_ok());
        transport.context_window_tokens = NonZeroU64::new(input_tokens + 127).unwrap();
        assert_eq!(
            transport.summary_request(&source).unwrap_err(),
            ParallelCompactionFault::Limit
        );
        transport.reserved_output_tokens = 0;
        transport.context_window_tokens = NonZeroU64::new(input_tokens + 63).unwrap();
        assert_eq!(
            transport.summary_request(&source).unwrap_err(),
            ParallelCompactionFault::Limit
        );

        transport.context_window_tokens = NonZeroU64::new(1).unwrap();
        assert_eq!(
            transport.summary_request(&source).unwrap_err(),
            ParallelCompactionFault::Limit
        );
        transport.request_limit = 1;
        assert_eq!(
            transport.summary_request(&source).unwrap_err(),
            ParallelCompactionFault::Limit
        );
    }

    fn source() -> CompactionSource {
        CompactionSource::new(
            vec![json!({"type":"message", "content":"past"})],
            "policy".into(),
        )
        .unwrap()
    }

    fn with_task(
        task: JoinHandle<Result<Vec<Value>, ParallelCompactionFault>>,
    ) -> ParallelCompaction {
        let mut compaction = ParallelCompaction::new(ContextRatioPolicy::default());
        compaction.status.send_replace(ParallelCompactionStatus {
            attempt: 1,
            phase: ParallelCompactionPhase::Running,
            fault: None,
        });
        compaction.attempt = Some(Attempt {
            id: 1,
            source: source(),
            task: Some(task),
            output: None,
            cancelled: false,
        });
        compaction
    }

    #[test]
    fn policy_panics_propagate_before_starting_a_worker() {
        let compaction = ParallelCompaction::new(|_: &CompactionContext| -> bool {
            std::panic::panic_any(319_u32);
        });
        let error = std::panic::catch_unwind(AssertUnwindSafe(|| {
            compaction.select_source(Some(source()), &context(4, 100))
        }))
        .err()
        .unwrap();
        assert_eq!(*error.downcast::<u32>().unwrap(), 319);
        assert!(compaction.attempt.is_none());
        assert_eq!(
            compaction.monitor().status().phase,
            ParallelCompactionPhase::Idle
        );
    }

    #[tokio::test]
    async fn shutdown_awaits_the_cancelled_workers_destructor() {
        struct OnDrop(Arc<Semaphore>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.add_permits(1);
            }
        }
        let dropped = Arc::new(Semaphore::new(0));
        let guard = OnDrop(dropped.clone());
        let (started, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending().await
        });
        let mut compaction = with_task(task);
        started_rx.await.unwrap();
        compaction.shutdown().await;
        assert_eq!(dropped.available_permits(), 1);
        assert!(compaction.attempt.is_none());
        assert_eq!(
            compaction.monitor().status().phase,
            ParallelCompactionPhase::Cancelled
        );
    }

    #[tokio::test]
    async fn dropping_a_wait_keeps_the_worker_owned_until_shutdown() {
        struct OnDrop(Arc<Semaphore>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.add_permits(1);
            }
        }
        let dropped = Arc::new(Semaphore::new(0));
        let guard = OnDrop(dropped.clone());
        let (started, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending().await
        });
        let mut compaction = with_task(task);
        started_rx.await.unwrap();

        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            compaction.wait_ready(Duration::from_secs(60)),
        )
        .await
        .is_err());
        assert!(compaction.attempt.is_some());
        assert_eq!(dropped.available_permits(), 0);

        compaction.shutdown().await;
        assert_eq!(dropped.available_permits(), 1);
        assert_eq!(
            compaction.monitor().status().phase,
            ParallelCompactionPhase::Cancelled
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_after_timeout_keeps_the_aborted_handle_owned() {
        struct OnDrop(Arc<Semaphore>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.add_permits(1);
            }
        }
        let dropped = Arc::new(Semaphore::new(0));
        let guard = OnDrop(dropped.clone());
        let (started, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
            Ok(vec![artifact()])
        });
        let mut compaction = with_task(task);
        started_rx.await.unwrap();

        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            compaction.wait_ready(Duration::from_millis(10)),
        )
        .await
        .is_err());
        assert!(compaction.attempt.is_some());
        assert_eq!(
            compaction.monitor().status(),
            ParallelCompactionStatus {
                attempt: 1,
                phase: ParallelCompactionPhase::Failed,
                fault: Some(ParallelCompactionFault::Timeout),
            }
        );

        compaction.shutdown().await;
        assert_eq!(dropped.available_permits(), 1);
        assert!(compaction.attempt.is_none());
    }

    #[tokio::test]
    async fn waiting_an_aborted_attempt_preserves_cancelled_status() {
        let task = tokio::spawn(std::future::pending());
        let mut compaction = with_task(task);
        compaction.reset();

        assert!(!compaction.wait_ready(Duration::from_secs(1)).await);
        assert!(compaction.attempt.is_none());
        assert_eq!(
            compaction.monitor().status(),
            ParallelCompactionStatus {
                attempt: 1,
                phase: ParallelCompactionPhase::Cancelled,
                fault: None,
            }
        );
    }

    #[tokio::test]
    async fn refresh_classifies_external_worker_cancellation() {
        let task = tokio::spawn(std::future::pending());
        let abort = task.abort_handle();
        let mut compaction = with_task(task);
        abort.abort();
        while !compaction
            .attempt
            .as_ref()
            .and_then(|attempt| attempt.task.as_ref())
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }

        compaction.refresh();
        assert!(compaction.attempt.is_none());
        assert_eq!(
            compaction.monitor().status(),
            ParallelCompactionStatus {
                attempt: 1,
                phase: ParallelCompactionPhase::Failed,
                fault: Some(ParallelCompactionFault::RuntimeUnavailable),
            }
        );
    }

    #[tokio::test]
    async fn waiting_a_panicked_worker_consumes_the_handle_before_propagating() {
        let (started, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            started.send(()).unwrap();
            std::panic::panic_any(941_u32);
        });
        let mut compaction = with_task(task);
        started_rx.await.unwrap();

        let error = AssertUnwindSafe(compaction.wait_ready(Duration::from_secs(1)))
            .catch_unwind()
            .await
            .unwrap_err();
        assert_eq!(*error.downcast::<u32>().unwrap(), 941);
        assert!(compaction.attempt.is_none());
        compaction.shutdown().await;
    }

    #[tokio::test]
    async fn worker_panic_keeps_its_original_payload_at_the_owner_boundary() {
        let (started, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            started.send(()).unwrap();
            std::panic::panic_any(731_u32);
        });
        let mut compaction = with_task(task);
        started_rx.await.unwrap();
        let error =
            std::panic::catch_unwind(AssertUnwindSafe(|| compaction.refresh())).unwrap_err();
        assert_eq!(*error.downcast::<u32>().unwrap(), 731);
        // A consumed panic cannot be propagated twice during cleanup.
        compaction.shutdown().await;
    }
}
