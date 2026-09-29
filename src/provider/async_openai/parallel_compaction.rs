//! Speculative provider-private compaction. Workers produce candidates; only
//! foreground handoff can install them. No worker owns application capabilities.

use std::{collections::HashSet, num::NonZeroU64, panic::resume_unwind, time::Duration};

use async_openai::config::{Config, OpenAIConfig};
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{sync::watch, task::JoinHandle};

use super::frame_request::{ResponsesFrameRequestFault, ResponsesFrameRequestState};
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
    pub(super) read_timeout: Duration,
    pub(super) context_window_tokens: NonZeroU64,
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
        let request = transport
            .encoder
            .encode_compaction_request_bounded(
                &source.input,
                &source.instructions,
                transport.request_limit,
            )
            .map_err(|_| ParallelCompactionFault::Limit)
            .and_then(|body| {
                if ContextEstimate::from_bytes(body.len()).tokens
                    > transport.context_window_tokens.get()
                {
                    return Err(ParallelCompactionFault::Limit);
                }
                transport
                    .client
                    .post(transport.config.url("/responses/compact"))
                    .headers(transport.config.headers())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .build()
                    .map_err(|_| ParallelCompactionFault::Transport)
            });
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
        let input_bytes = source.input_bytes;
        let task = runtime.spawn(async move {
            let result = compact(transport, request, input_bytes).await;
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
    input_bytes: usize,
) -> Result<Vec<Value>, ParallelCompactionFault> {
    let response = transport.client.execute(request).await.map_err(|error| {
        if error.is_timeout() {
            ParallelCompactionFault::Timeout
        } else {
            ParallelCompactionFault::Transport
        }
    })?;
    if !response.status().is_success() {
        return Err(ParallelCompactionFault::HttpStatus(
            response.status().as_u16(),
        ));
    }
    let limit = transport.response_limit;
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ParallelCompactionFault::Limit);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(transport.read_timeout, stream.next())
        .await
        .map_err(|_| ParallelCompactionFault::Timeout)?
    {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                ParallelCompactionFault::Timeout
            } else {
                ParallelCompactionFault::Transport
            }
        })?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ParallelCompactionFault::Limit);
        }
        bytes.extend_from_slice(&chunk);
    }
    validate_output(&bytes, input_bytes)
}

fn validate_output(
    bytes: &[u8],
    input_bytes: usize,
) -> Result<Vec<Value>, ParallelCompactionFault> {
    let mut response: Value =
        serde_json::from_slice(bytes).map_err(|_| ParallelCompactionFault::Protocol)?;
    if response.get("object").and_then(Value::as_str) != Some("response.compaction") {
        return Err(ParallelCompactionFault::Protocol);
    }
    let output = response
        .get_mut("output")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .ok_or(ParallelCompactionFault::Protocol)?;
    let mut has_compaction = false;
    let mut pending_calls = HashSet::new();
    let mut seen_calls = HashSet::new();
    for item in &output {
        if item
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "in_progress" || status == "incomplete")
        {
            return Err(ParallelCompactionFault::Protocol);
        }
        match item.get("type").and_then(Value::as_str) {
            Some("compaction") => {
                if !item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                {
                    return Err(ParallelCompactionFault::Protocol);
                }
                has_compaction = true;
            }
            Some("function_call" | "function_call_output") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or(ParallelCompactionFault::Protocol)?;
                if item["type"] == "function_call" {
                    if !seen_calls.insert(call_id) {
                        return Err(ParallelCompactionFault::Protocol);
                    }
                    pending_calls.insert(call_id);
                } else if !pending_calls.remove(call_id) {
                    return Err(ParallelCompactionFault::Protocol);
                }
            }
            Some(kind) if !kind.is_empty() => {}
            _ => return Err(ParallelCompactionFault::Protocol),
        }
    }
    if !has_compaction || !pending_calls.is_empty() {
        return Err(ParallelCompactionFault::Protocol);
    }
    let output_bytes = serde_json::to_vec(&output)
        .map_err(|_| ParallelCompactionFault::Protocol)?
        .len();
    if output_bytes >= input_bytes {
        return Err(ParallelCompactionFault::NoReduction);
    }
    // Retained items are part of the API's complete replacement window.
    Ok(output)
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
        json!({"type":"compaction", "encrypted_content":"opaque"})
    }

    #[test]
    fn output_keeps_every_retained_item_in_order_and_requires_reduction() {
        let output = vec![
            json!({"type":"message", "role":"user", "content":"retained"}),
            artifact(),
            json!({"type":"function_call", "call_id":"closed", "name":"lookup", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"closed", "output":"done"}),
        ];
        let bytes = serde_json::to_vec(&json!({
            "object":"response.compaction", "output":output,
        }))
        .unwrap();
        let size = serde_json::to_vec(&output).unwrap().len();
        assert_eq!(validate_output(&bytes, size + 1).unwrap(), output);
        assert_eq!(
            validate_output(&bytes, size),
            Err(ParallelCompactionFault::NoReduction)
        );
    }

    #[test]
    fn malformed_or_causally_open_compaction_output_is_rejected() {
        for output in [
            json!([]),
            json!([{"type":"message"}]),
            json!([artifact(), {"type":"compaction", "encrypted_content":""}]),
            json!([artifact(), {"type":17}]),
            json!([artifact(), {"type":"message", "status":"incomplete"}]),
            json!([artifact(), {"type":"function_call", "call_id":"open"}]),
            json!([artifact(), {"type":"function_call_output", "call_id":"orphan"}]),
            json!([artifact(), {"type":"function_call", "call_id":"a"}, {"type":"function_call", "call_id":"a"}, {"type":"function_call_output", "call_id":"a"}]),
        ] {
            let bytes = serde_json::to_vec(&json!({
                "object":"response.compaction", "output":output,
            }))
            .unwrap();
            assert_eq!(
                validate_output(&bytes, usize::MAX),
                Err(ParallelCompactionFault::Protocol),
                "{output}"
            );
        }
        for bytes in [
            b"invalid JSON".as_slice(),
            br#"{"object":"response","output":[]}"#,
            br#"{"object":"response.compaction"}"#,
        ] {
            assert_eq!(
                validate_output(bytes, usize::MAX),
                Err(ParallelCompactionFault::Protocol)
            );
        }
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
