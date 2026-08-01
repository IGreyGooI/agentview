//! This integration test intentionally uses only the documented public surface.
//! It proves durable tree authoring, explicit one-shot erasure, and the public
//! in-memory mounted-owner lifecycle through an external consumer boundary.

use std::{
    convert::Infallible,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
};

use agentview::component::{
    advanced::{
        host::{LiveEffectRuntimeBindingContext, LiveEffectRuntimeFactory},
        lifecycle::{
            mount_system_epoch, mount_system_epoch_with_contract, SystemMountContext, SystemView,
        },
        provider::{
            durable_provider_tool, durable_provider_tool_with_context, provider_tools_with_context,
            AttachedProviderEpoch, DurableMountedProviderExecutor, FallibleProviderWirePort,
            MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
            ProviderAdapterContract, ProviderAdapterError, ProviderCancellationToken,
            ProviderDispatchContext, ProviderDispatchFailure, ProviderDispatchUpdate,
            ProviderDispatcher, ProviderEpochAttachRequest, ProviderEpochReceipt,
            ProviderEpochRehydrateRequest, ProviderToolCall, ProviderToolResponse,
            ProviderTurnCursor, ProviderWireAck, ProviderWireEvent, ProviderWireFault,
        },
    },
    durable_provider_contract,
    host::prelude::*,
    DurableCallId, DurableCallInputId, DurableSessionId, InMemoryMountedAgentFactory,
    MountedHarnessDefinition, ProviderAttemptIdentity, ProviderCapabilityContract,
    SessionReduceContext, SessionReducer,
};
use agentview::llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit};
use agentview::prelude::*;
use serde_json::json;
use tokio::{sync::Notify, time::Duration};

struct PublicChannels;

impl TurnChannels for PublicChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = String;
}

struct PublicLiveChannels;

impl TurnChannels for PublicLiveChannels {
    type Output = Never;
    type Live = PublicLiveEffect;
    type Commit = Never;
    type Diagnostic = String;
}

#[derive(Debug)]
enum PublicLiveEffect {
    Opened,
}

/// A reusable feature can keep its own effect contract and explicitly lift it
/// only at the harness boundary.
struct LocalFeatureChannels;

impl TurnChannels for LocalFeatureChannels {
    type Output = Never;
    type Live = LocalFeatureLiveEffect;
    type Commit = Never;
    type Diagnostic = String;
}

#[derive(Debug)]
enum LocalFeatureLiveEffect {
    Opened,
}

#[derive(Debug, thiserror::Error)]
enum PublicLiveProviderError {
    #[error("public live provider wire failed: {0}")]
    Wire(#[from] ProviderWireFault),
    #[error("public live provider expected a native tool result acknowledgement")]
    MissingToolResult,
}

#[derive(Clone)]
struct PublicLiveEpoch(ProviderEpochReceipt);

#[derive(Default)]
struct PublicLiveProvider {
    complete_after_emit: bool,
    emitted_text: Option<&'static str>,
    emit_native_tool: bool,
    replace_history_once: bool,
    cancellation_cursor_unchanged: bool,
    cancellation_cursor_resume: bool,
    cancellation_cursor_invalid: bool,
    attaches: AtomicUsize,
    preparations: AtomicUsize,
    executions: AtomicUsize,
    entered: Notify,
    joined: AtomicUsize,
    rehydrates: AtomicUsize,
    rehydrates_with_cursor: AtomicUsize,
    request_cursors: StdMutex<Vec<Option<ProviderTurnCursor>>>,
    tool_acknowledgements: StdMutex<Vec<PublicToolAcknowledgement>>,
}

#[derive(Debug, Clone, PartialEq)]
struct PublicToolAcknowledgement {
    invocation_id: Option<String>,
    result_correlation_id: Option<String>,
    name: String,
    response: ProviderToolResponse,
    replayed: bool,
}

impl PublicLiveProvider {
    fn completing() -> Self {
        Self {
            complete_after_emit: true,
            ..Self::default()
        }
    }

    fn completing_with_text(emitted_text: &'static str) -> Self {
        Self {
            complete_after_emit: true,
            emitted_text: Some(emitted_text),
            ..Self::default()
        }
    }

    fn completing_with_native_tool() -> Self {
        Self {
            complete_after_emit: true,
            emit_native_tool: true,
            ..Self::default()
        }
    }

    fn completing_after_replacement() -> Self {
        Self {
            complete_after_emit: true,
            replace_history_once: true,
            ..Self::default()
        }
    }

    fn cancelling_without_cursor_change() -> Self {
        Self {
            cancellation_cursor_unchanged: true,
            ..Self::default()
        }
    }

    fn cancelling_with_resume_cursor() -> Self {
        Self {
            cancellation_cursor_resume: true,
            ..Self::default()
        }
    }

    fn cancelling_with_invalid_resume_cursor() -> Self {
        Self {
            cancellation_cursor_invalid: true,
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct PublicLiveReducer;

impl SessionReducer<String, usize, PublicLiveChannels> for PublicLiveReducer {
    type Error = Infallible;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, usize>,
        _context: SessionReduceContext<'_, String, PublicLiveChannels>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Ok(TurnFlow::Wait)
    }
}

#[derive(Default)]
struct PublicLiveContinuationReducer;

impl SessionReducer<String, usize, PublicLiveChannels> for PublicLiveContinuationReducer {
    type Error = Infallible;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, usize>,
        context: SessionReduceContext<'_, String, PublicLiveChannels>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Ok(if context.turn_index() == 0 {
            TurnFlow::Continue
        } else {
            TurnFlow::Wait
        })
    }
}

struct PublicLiveRuntime {
    applied: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
    abort_identities: Arc<StdMutex<Vec<ProviderAttemptIdentity>>>,
}

struct BindingIdRecordingLiveRuntime {
    binding_ids: Arc<StdMutex<Vec<String>>>,
}

struct BindingIdRecordingLiveFactory {
    binding_ids: Arc<StdMutex<Vec<String>>>,
}

impl LiveEffectRuntimeFactory<PublicLiveChannels, str, str, TurnProps>
    for BindingIdRecordingLiveFactory
{
    type Runtime = BindingIdRecordingLiveRuntime;
    type Error = Infallible;

    fn bind(
        &self,
        _context: LiveEffectRuntimeBindingContext<'_, str, str, TurnProps>,
    ) -> Result<Self::Runtime, Self::Error> {
        Ok(BindingIdRecordingLiveRuntime {
            binding_ids: Arc::clone(&self.binding_ids),
        })
    }
}

#[async_trait::async_trait]
impl LiveEffectRuntime<PublicLiveEffect> for BindingIdRecordingLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        effect: PublicLiveEffect,
    ) -> Result<(), Self::Error> {
        match effect {
            PublicLiveEffect::Opened => self
                .binding_ids
                .lock()
                .unwrap()
                .push(context.origin().binding_id().to_string()),
        }
        Ok(())
    }

    async fn abort(
        &mut self,
        _context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        Ok(LiveEffectAbortAck::NoEffectsApplied)
    }
}

#[derive(Debug, Clone)]
struct PublicLiveBindingObservation {
    session_id: String,
    epoch_contract_id: String,
    call_id: String,
    input_id: String,
    turn_index: u64,
    call_props: String,
    source: String,
    task: String,
    attempt: ProviderAttemptIdentity,
}

struct PublicLiveFactory {
    bindings: Arc<StdMutex<Vec<PublicLiveBindingObservation>>>,
    applied: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
    abort_identities: Arc<StdMutex<Vec<ProviderAttemptIdentity>>>,
}

impl LiveEffectRuntimeFactory<PublicLiveChannels, str, str, TurnProps> for PublicLiveFactory {
    type Runtime = PublicLiveRuntime;
    type Error = Infallible;

    fn bind(
        &self,
        context: LiveEffectRuntimeBindingContext<'_, str, str, TurnProps>,
    ) -> Result<Self::Runtime, Self::Error> {
        self.bindings
            .lock()
            .unwrap()
            .push(PublicLiveBindingObservation {
                session_id: context.session_id().to_string(),
                epoch_contract_id: context.epoch_contract_id().to_string(),
                call_id: context.call_id().to_string(),
                input_id: context.input_id().to_string(),
                turn_index: context.turn_index(),
                call_props: context.call_props().to_owned(),
                source: context.source().to_owned(),
                task: context.turn_props().task.clone(),
                attempt: context.attempt().clone(),
            });
        Ok(PublicLiveRuntime {
            applied: Arc::clone(&self.applied),
            aborted: Arc::clone(&self.aborted),
            abort_identities: Arc::clone(&self.abort_identities),
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("public test rejected the Live runtime binding")]
struct PublicLiveBindingError;

struct FailOncePublicLiveFactory {
    bindings: Arc<AtomicUsize>,
    applied: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
    abort_identities: Arc<StdMutex<Vec<ProviderAttemptIdentity>>>,
}

impl LiveEffectRuntimeFactory<PublicLiveChannels, str, str, TurnProps>
    for FailOncePublicLiveFactory
{
    type Runtime = PublicLiveRuntime;
    type Error = PublicLiveBindingError;

    fn bind(
        &self,
        _context: LiveEffectRuntimeBindingContext<'_, str, str, TurnProps>,
    ) -> Result<Self::Runtime, Self::Error> {
        if self.bindings.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(PublicLiveBindingError);
        }
        Ok(PublicLiveRuntime {
            applied: Arc::clone(&self.applied),
            aborted: Arc::clone(&self.aborted),
            abort_identities: Arc::clone(&self.abort_identities),
        })
    }
}

#[async_trait::async_trait]
impl LiveEffectRuntime<PublicLiveEffect> for PublicLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        effect: PublicLiveEffect,
    ) -> Result<(), Self::Error> {
        match effect {
            PublicLiveEffect::Opened => {
                self.applied.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        self.abort_identities
            .lock()
            .unwrap()
            .push(context.identity().clone());
        self.aborted.fetch_add(1, Ordering::SeqCst);
        Ok(if context.applied_effects() == 0 {
            LiveEffectAbortAck::NoEffectsApplied
        } else {
            LiveEffectAbortAck::CompensationCompleted
        })
    }
}

// This function is compiled from outside the crate. It deliberately needs no
// persistence version, receipt, request id, or lease to consume a completed
// mounted call.
#[allow(dead_code)]
fn inspect_public_mounted_outcome(outcome: MountedCallOutcome<PublicChannels>) {
    match outcome {
        MountedCallOutcome::Executed {
            records,
            publications,
            result,
        } => {
            let _ = records.len();
            let _ = result.call_id();
            let _ = result.input_id();
            let _ = result.terminal_publication().publication_id();
            for publication in publications {
                let _ = publication.publication_id();
                let _ = publication.queued_outbox_items();
            }
        }
        MountedCallOutcome::Replayed { result } => {
            let _ = result.call_id();
            let _ = result.input_id();
            let _ = result.terminal_publication().publication_id();
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn inspect_public_mounted_result(result: MountedCallResult, publication: MountedTurnPublication) {
    let _ = result.call_id();
    let _ = result.input_id();
    let _ = result.terminal_publication().publication_id();
    let _ = publication.publication_id();
    let _ = publication.queued_outbox_items();
}

#[test]
fn public_mounted_call_input_owns_one_exact_snapshot() {
    let props = Arc::new(TurnProps {
        user_renders: Arc::new(AtomicUsize::new(0)),
        task: "inspect the plaza".to_owned(),
    });
    let source = Arc::new("public-source-v1".to_owned());
    let input = MountedCallInput::new(
        DurableCallId::new("public-call-42").unwrap(),
        DurableCallInputId::new("public-call-42/input-v1").unwrap(),
        "player-turn",
        Arc::clone(&props),
        Arc::clone(&source),
    )
    .unwrap();

    assert_eq!(input.call_id().as_str(), "public-call-42");
    assert_eq!(input.input_id().as_str(), "public-call-42/input-v1");
    assert_eq!(input.call_label(), "player-turn");
    assert!(Arc::ptr_eq(input.props(), &props));
    assert!(Arc::ptr_eq(input.source(), &source));

    assert!(matches!(
        MountedCallInput::new(
            DurableCallId::new("invalid-call").unwrap(),
            DurableCallInputId::new("invalid-call/input-v1").unwrap(),
            "",
            props,
            source,
        ),
        Err(MountedCallInputError::InvalidLabel { .. })
    ));
}

struct PublicDurableProvider;

#[derive(Clone)]
enum PublicLifecycleEpoch {
    Durable(ProviderEpochReceipt),
}

#[derive(Default)]
struct PublicLifecycleProvider {
    durable_attaches: AtomicUsize,
    rehydrates: AtomicUsize,
    rehydrates_with_cursor: AtomicUsize,
    executions: AtomicUsize,
    attached_systems: StdMutex<Vec<String>>,
    users: StdMutex<Vec<String>>,
    operations: StdMutex<Vec<PublicProviderOperationObservation>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicProviderOperationObservation {
    call_label: String,
    session_id: String,
    durable_epoch_id: String,
    call_id: String,
    input_id: String,
    turn_index: u64,
    request_id: String,
    remote_idempotency_key: String,
}

fn observe_provider_operation(
    request: &MountedProviderRequest<String>,
) -> PublicProviderOperationObservation {
    let operation = request
        .provider_operation()
        .expect("durable providers receive an idempotency operation");
    PublicProviderOperationObservation {
        call_label: request.call_label().to_owned(),
        session_id: operation.session_id().to_owned(),
        durable_epoch_id: operation.durable_epoch_id().to_owned(),
        call_id: operation.call_id().to_owned(),
        input_id: operation.input_id().to_owned(),
        turn_index: operation.turn_index(),
        request_id: operation.publication_request_id().to_owned(),
        remote_idempotency_key: operation.remote_idempotency_key(),
    }
}

#[derive(Default)]
struct PublicLifecycleReducer;

impl SessionReducer<String, usize, PublicChannels> for PublicLifecycleReducer {
    type Error = Infallible;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, usize>,
        _context: SessionReduceContext<'_, String, PublicChannels>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Ok(TurnFlow::Wait)
    }
}

#[derive(Default)]
struct PublicContinuationReducer;

impl SessionReducer<String, usize, PublicChannels> for PublicContinuationReducer {
    type Error = Infallible;

    fn reduce(
        &self,
        session: &mut AgentSession<String, usize>,
        context: SessionReduceContext<'_, String, PublicChannels>,
        executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        session.push_history(format!("user:{}", context.request().user()));
        session.extend_history(executor_commit.append);
        Ok(if context.turn_index() == 0 {
            TurnFlow::Continue
        } else {
            TurnFlow::Wait
        })
    }
}

#[derive(Default)]
struct PublicOverBudgetContinuationReducer;

impl SessionReducer<String, usize, PublicChannels> for PublicOverBudgetContinuationReducer {
    type Error = Infallible;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, usize>,
        context: SessionReduceContext<'_, String, PublicChannels>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Ok(if context.turn_index() < 2 {
            TurnFlow::Continue
        } else {
            TurnFlow::Wait
        })
    }
}

fn no_live_effects() -> NoLiveEffects {
    NoLiveEffects
}

type PublicInMemoryFactory = InMemoryMountedAgentFactory<
    PublicChannels,
    String,
    usize,
    PublicLifecycleProvider,
    PublicLifecycleReducer,
    fn() -> NoLiveEffects,
>;

type PublicRegistryLiveFactory = InMemoryMountedAgentFactory<
    PublicChannels,
    String,
    usize,
    PublicLiveProvider,
    PublicLifecycleReducer,
    fn() -> NoLiveEffects,
>;

#[test]
fn public_provider_adapter_configuration_uses_provider_errors() {
    assert!(matches!(
        ProviderAdapterContract::new("", 1),
        Err(ProviderAdapterError::InvalidAdapter { .. })
    ));
    assert!(matches!(
        ProviderAdapterContract::new("public-test-provider", 0),
        Err(ProviderAdapterError::ZeroReceiptSchemaVersion)
    ));
}

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for PublicDurableProvider {
    type Error = Infallible;
    type Epoch = ();

    async fn prepare_context(
        &self,
        _epoch: &Self::Epoch,
        _request: &MountedProviderRequest<String>,
        _budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<String>, Self::Error> {
        Ok(ContextPreparation::Ready)
    }

    async fn execute(
        &self,
        _epoch: &Self::Epoch,
        _request: MountedProviderRequest<String>,
        _wire: &mut dyn FallibleProviderWirePort,
        _cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<String>, Self::Error> {
        Ok(MountedProviderExit::completed(ExecutorCommit::new(
            std::iter::empty::<String>(),
        )))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for PublicDurableProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("public-test-provider", 1)
            .expect("the public test adapter contract is valid")
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        assert!(!request.system().is_empty() || request.tools().is_empty());
        let receipt = ProviderEpochReceipt::new(
            "public-test-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "binding": "public-test" }),
        )
        .expect("the public test receipt is valid");
        Ok(AttachedProviderEpoch::new((), receipt))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        assert_eq!(request.receipt().adapter(), "public-test-provider");
        assert_eq!(
            request.receipt().artifact_fingerprint(),
            request.fingerprint()
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for PublicLifecycleProvider {
    type Error = Infallible;
    type Epoch = PublicLifecycleEpoch;

    async fn prepare_context(
        &self,
        _epoch: &Self::Epoch,
        _request: &MountedProviderRequest<String>,
        _budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<String>, Self::Error> {
        Ok(ContextPreparation::Ready)
    }

    async fn execute(
        &self,
        epoch: &Self::Epoch,
        request: MountedProviderRequest<String>,
        _wire: &mut dyn FallibleProviderWirePort,
        _cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<String>, Self::Error> {
        self.operations
            .lock()
            .unwrap()
            .push(observe_provider_operation(&request));
        self.users.lock().unwrap().push(request.user().to_owned());
        let turn = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        let commit = ExecutorCommit::new(std::iter::empty::<String>());
        let PublicLifecycleEpoch::Durable(receipt) = epoch;
        Ok(MountedProviderExit::completed_with_cursor(
            commit,
            ProviderTurnCursor::new(
                receipt.adapter(),
                receipt.schema_version(),
                receipt.durable_epoch_id().clone(),
                receipt.artifact_fingerprint().clone(),
                json!({ "turn": turn }),
            )
            .expect("the lifecycle cursor is valid"),
        ))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for PublicLifecycleProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("public-lifecycle-provider", 1)
            .expect("the lifecycle provider contract is valid")
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        self.durable_attaches.fetch_add(1, Ordering::SeqCst);
        assert!(!request.system().is_empty());
        self.attached_systems
            .lock()
            .unwrap()
            .push(request.system().to_owned());
        let receipt = ProviderEpochReceipt::new(
            "public-lifecycle-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "attachment": "public-lifecycle" }),
        )
        .expect("the lifecycle receipt is valid");
        Ok(AttachedProviderEpoch::new(
            PublicLifecycleEpoch::Durable(receipt.clone()),
            receipt,
        ))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        self.rehydrates.fetch_add(1, Ordering::SeqCst);
        if request.cursor().is_some() {
            self.rehydrates_with_cursor.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(
            request.receipt().artifact_fingerprint(),
            request.fingerprint()
        );
        Ok(PublicLifecycleEpoch::Durable(request.receipt().clone()))
    }
}

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for PublicLiveProvider {
    type Error = PublicLiveProviderError;
    type Epoch = PublicLiveEpoch;

    async fn prepare_context(
        &self,
        _epoch: &Self::Epoch,
        _request: &MountedProviderRequest<String>,
        _budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<String>, Self::Error> {
        let preparation = self.preparations.fetch_add(1, Ordering::SeqCst);
        Ok(if self.replace_history_once && preparation == 0 {
            ContextPreparation::ReplaceHistory {
                history: vec!["compacted-public-history".to_owned()],
            }
        } else {
            ContextPreparation::Ready
        })
    }

    async fn execute(
        &self,
        epoch: &Self::Epoch,
        request: MountedProviderRequest<String>,
        wire: &mut dyn FallibleProviderWirePort,
        cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<String>, Self::Error> {
        assert_eq!(epoch.0.adapter(), "public-live-provider");
        self.request_cursors
            .lock()
            .unwrap()
            .push(request.provider_cursor().cloned());
        if self.emit_native_tool {
            let acknowledgement = wire
                .submit(ProviderWireEvent::Tool(
                    ProviderToolCall::new(
                        "public-native-tool-1",
                        "lookup_world_state",
                        json!({ "place": "forum" }),
                    )
                    .with_result_correlation_id("public-native-tool-result-1"),
                ))
                .await?;
            let ProviderWireAck::ToolResult { result, replayed } = acknowledgement else {
                return Err(PublicLiveProviderError::MissingToolResult);
            };
            self.tool_acknowledgements
                .lock()
                .unwrap()
                .push(PublicToolAcknowledgement {
                    invocation_id: result.invocation_id().map(str::to_owned),
                    result_correlation_id: result.result_correlation_id().map(str::to_owned),
                    name: result.name().to_owned(),
                    response: result.response().clone(),
                    replayed,
                });
        } else {
            wire.submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(
                self.emitted_text.unwrap_or("<public_live />").to_owned(),
            )))
            .await?;
        }
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_waiters();
        if self.complete_after_emit {
            return Ok(MountedProviderExit::completed(ExecutorCommit::empty()));
        }
        let reason = cancellation.cancelled().await;
        self.joined.fetch_add(1, Ordering::SeqCst);
        Ok(if self.cancellation_cursor_unchanged {
            MountedProviderExit::cancelled_without_cursor_change(reason)
        } else if self.cancellation_cursor_resume {
            MountedProviderExit::cancelled_with_resume_cursor(
                reason,
                ProviderTurnCursor::new(
                    "public-live-provider",
                    1,
                    epoch.0.durable_epoch_id().clone(),
                    epoch.0.artifact_fingerprint().clone(),
                    json!({ "turn": "resume" }),
                )
                .expect("the matching public resume cursor is valid"),
            )
        } else if self.cancellation_cursor_invalid {
            MountedProviderExit::cancelled_with_resume_cursor(
                reason,
                ProviderTurnCursor::new(
                    "foreign-public-live-provider",
                    1,
                    epoch.0.durable_epoch_id().clone(),
                    epoch.0.artifact_fingerprint().clone(),
                    json!({ "turn": "foreign" }),
                )
                .expect("the malformed lineage fixture is structurally valid"),
            )
        } else {
            MountedProviderExit::cancelled(reason)
        })
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for PublicLiveProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("public-live-provider", 1).unwrap()
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        self.attaches.fetch_add(1, Ordering::SeqCst);
        let receipt = ProviderEpochReceipt::new(
            "public-live-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "attachment": "public-live" }),
        )
        .unwrap();
        Ok(AttachedProviderEpoch::new(
            PublicLiveEpoch(receipt.clone()),
            receipt,
        ))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        self.rehydrates.fetch_add(1, Ordering::SeqCst);
        if request.cursor().is_some() {
            self.rehydrates_with_cursor.fetch_add(1, Ordering::SeqCst);
        }
        Ok(PublicLiveEpoch(request.receipt().clone()))
    }
}

#[derive(Default)]
struct SelectionState;

#[derive(Debug)]
struct PublicDispatcher;

#[async_trait::async_trait]
impl ProviderDispatcher<PublicChannels> for PublicDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<PublicChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("ok"),
            StreamUpdate::new(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PublicDispatcherInvocation {
    task: String,
    invocation_id: Option<String>,
    result_correlation_id: Option<String>,
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug)]
struct RecordingPublicDispatcher {
    task: String,
    invocations: Arc<StdMutex<Vec<PublicDispatcherInvocation>>>,
}

#[async_trait::async_trait]
impl ProviderDispatcher<PublicChannels> for RecordingPublicDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<PublicChannels>, Self::Error> {
        self.invocations
            .lock()
            .unwrap()
            .push(PublicDispatcherInvocation {
                task: self.task.clone(),
                invocation_id: call.invocation_id().map(str::to_owned),
                result_correlation_id: call.result_correlation_id().map(str::to_owned),
                name: call.name().to_owned(),
                arguments: call.arguments().clone(),
            });
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success(json!({
                "place": call.arguments()["place"],
                "task": self.task,
            })),
            StreamUpdate::new(),
        ))
    }
}

fn public_registry_contract(contract_version: &str, tool_name: &str) -> ProviderCapabilityContract {
    ProviderCapabilityContract::new(
        "public.world-registry",
        contract_version,
        [ProviderToolSpec::new(
            tool_name,
            "Read immutable world state",
            json!({ "type": "object", "properties": {} }),
        )
        .unwrap()],
    )
    .unwrap()
}

#[view(component)]
fn public_registry_capability() -> DurableComponent<PublicChannels, TurnProps> {
    durable_provider_contract(
        public_registry_contract("v1", "lookup_world_state"),
        Document::from_xml(XmlNode::new(
            XmlName::try_from("public_world_registry").unwrap(),
        )),
    )
}

struct MountProps {
    system_renders: Arc<AtomicUsize>,
}

struct TurnProps {
    user_renders: Arc<AtomicUsize>,
    task: String,
}

struct PublicCapture {
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl MountedTurnCapture for PublicCapture {
    type Transcript = String;
    type ContextState = usize;
    type CallProps = str;
    type TurnProps = TurnProps;
    type Source = str;
    type Error = Infallible;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<'_, String, usize, str, str>,
    ) -> Result<Self::TurnProps, Self::Error> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        Ok(TurnProps {
            user_renders: Arc::clone(&self.user_renders),
            task: format!(
                "{}:{}:history-{}",
                context.call_props(),
                context.source(),
                context.context().history().len()
            ),
        })
    }
}

#[view(component)]
fn public_selection() -> DurableComponent<PublicChannels, TurnProps> {
    StreamingXml::<TurnEmission<PublicChannels>, String>::new(XmlNode::new(
        XmlName::try_from("select_intent").unwrap(),
    ))
    .state_with(SelectionState::default)
    .into_durable_component(RuntimeContract::new("public.select-intent", "v1").unwrap())
}

#[view(component)]
fn public_lifecycle_policy(system_renders: Arc<AtomicUsize>) -> PomView {
    system_renders.fetch_add(1, Ordering::SeqCst);
    pom_view(Document::from_xml(XmlNode::new(
        XmlName::try_from("public_lifecycle_policy").unwrap(),
    )))
}

#[view(component)]
fn public_lifecycle_policy_named(
    system_renders: Arc<AtomicUsize>,
    policy_name: &'static str,
) -> PomView {
    system_renders.fetch_add(1, Ordering::SeqCst);
    pom_view(Document::from_xml(XmlNode::new(
        XmlName::try_from(policy_name).unwrap(),
    )))
}

fn public_lifecycle_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
    system_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicChannels, PublicCapture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("public-lifecycle/v1").unwrap(),
        durable_system((public_lifecycle_policy(system_renders), public_selection())),
    );
    MountedHarnessDefinition::new(epoch, public_user)
        .with_turn_loop_policy(TurnLoopPolicy::new(NonZeroUsize::new(2).unwrap()))
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

fn public_registry_definition(
    epoch_contract_id: &str,
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
    system_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicChannels, PublicCapture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new(epoch_contract_id).unwrap(),
        durable_system((
            public_lifecycle_policy(system_renders),
            public_registry_capability(),
        )),
    );
    MountedHarnessDefinition::new(epoch, public_user).with_capture(PublicCapture {
        captures,
        user_renders,
    })
}

fn public_registry_factory(
    session_id: &str,
    provider: Arc<PublicLifecycleProvider>,
) -> PublicInMemoryFactory {
    PublicInMemoryFactory::new(
        DurableSessionId::new(session_id).unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        provider,
        "public-provider-registry-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap()
}

fn public_dispatcher_registry(
    host_implementation_version: &str,
    initializations: Arc<AtomicUsize>,
) -> ProviderDispatcherRegistry<PublicChannels, TurnProps> {
    let mut registry = ProviderDispatcherRegistry::<PublicChannels, TurnProps>::new();
    registry
        .register(
            public_registry_contract("v1", "lookup_world_state"),
            host_implementation_version.to_owned(),
            move || {
                initializations.fetch_add(1, Ordering::SeqCst);
                PublicDispatcher
            },
        )
        .unwrap();
    registry
}

fn contextual_public_dispatcher_registry(
    host_implementation_version: &str,
    initializations: Arc<AtomicUsize>,
    observed_tasks: Arc<StdMutex<Vec<String>>>,
) -> ProviderDispatcherRegistry<PublicChannels, TurnProps> {
    let mut registry = ProviderDispatcherRegistry::<PublicChannels, TurnProps>::new();
    registry
        .register_with_context(
            public_registry_contract("v1", "lookup_world_state"),
            host_implementation_version.to_owned(),
            move |context| {
                initializations.fetch_add(1, Ordering::SeqCst);
                observed_tasks
                    .lock()
                    .unwrap()
                    .push(context.props().task.clone());
                Ok::<_, ProviderDispatchFailure>(PublicDispatcher)
            },
        )
        .unwrap();
    registry
}

fn recording_public_dispatcher_registry(
    initializations: Arc<AtomicUsize>,
    invocations: Arc<StdMutex<Vec<PublicDispatcherInvocation>>>,
) -> ProviderDispatcherRegistry<PublicChannels, TurnProps> {
    let mut registry = ProviderDispatcherRegistry::<PublicChannels, TurnProps>::new();
    registry
        .register_with_context(
            public_registry_contract("v1", "lookup_world_state"),
            "public-native-tool-host/v1",
            move |context| {
                initializations.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ProviderDispatchFailure>(RecordingPublicDispatcher {
                    task: context.props().task.clone(),
                    invocations: Arc::clone(&invocations),
                })
            },
        )
        .unwrap();
    registry
}

fn public_lifecycle_definition_with_policy(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
    system_renders: Arc<AtomicUsize>,
    policy_name: &'static str,
    turn_loop_max_turns: NonZeroUsize,
) -> CapturedMountedHarnessDefinition<PublicChannels, PublicCapture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("public-lifecycle/v1").unwrap(),
        durable_system((
            public_lifecycle_policy_named(system_renders, policy_name),
            public_selection(),
        )),
    );
    MountedHarnessDefinition::new(epoch, public_user)
        .with_turn_loop_policy(TurnLoopPolicy::new(turn_loop_max_turns))
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

#[view(component)]
fn fallible_user_feature() -> MountedFeature<PublicChannels, TurnProps> {
    MountedFeature::try_new(
        durable_system(Document::from_xml(XmlNode::new(
            XmlName::try_from("fallible_user_policy").unwrap(),
        ))),
        |context| -> Result<Document, ComponentError> {
            context.props().user_renders.fetch_add(1, Ordering::SeqCst);
            let root = XmlName::try_from(context.props().task.as_str())?;
            Ok(Document::from_xml(XmlNode::new(root)))
        },
    )
}

fn fallible_user_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicChannels, PublicCapture> {
    fallible_user_feature()
        .into_harness(EpochContractId::new("public/fallible-user/v1").unwrap())
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

#[view(component)]
fn public_live_stream() -> DurableComponent<PublicLiveChannels, TurnProps> {
    StreamingXml::<TurnEmission<PublicLiveChannels>, String>::new(XmlNode::new(
        XmlName::try_from("public_live").unwrap(),
    ))
    .state_with(|| ())
    .on_open(|_, _| StreamUpdate::from_emission(TurnEmission::Live(PublicLiveEffect::Opened)))
    .into_durable_component(RuntimeContract::new("public.live-stream", "v1").unwrap())
}

fn public_live_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
    system_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("public-live/v1").unwrap(),
        durable_system((
            public_lifecycle_policy(system_renders),
            public_live_stream(),
        )),
    );
    MountedHarnessDefinition::new(epoch, public_user)
        .with_turn_loop_policy(TurnLoopPolicy::new(NonZeroUsize::new(2).unwrap()))
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

#[view(component)]
fn local_feature_live_stream() -> DurableComponent<LocalFeatureChannels, TurnProps> {
    StreamingXml::<TurnEmission<LocalFeatureChannels>, String>::new(XmlNode::new(
        XmlName::try_from("public_live").unwrap(),
    ))
    .state_with(|| ())
    .on_open(|_, _| StreamUpdate::from_emission(TurnEmission::Live(LocalFeatureLiveEffect::Opened)))
    .into_durable_component(RuntimeContract::new("public.local-feature-live", "v1").unwrap())
}

#[view(component)]
fn local_feature(
    user_renders: Arc<AtomicUsize>,
) -> MountedFeature<LocalFeatureChannels, TurnProps> {
    MountedFeature::new(
        durable_system(local_feature_live_stream()),
        move |context| {
            user_renders.fetch_add(1, Ordering::SeqCst);
            feature_user_fragment("local_feature_task", &context.props().task)
        },
    )
}

#[view(component)]
fn positional_local_feature(
    user_renders: Arc<AtomicUsize>,
    runtime_name: &'static str,
    route_name: &'static str,
) -> MountedFeature<LocalFeatureChannels, TurnProps> {
    let runtime = StreamingXml::<TurnEmission<LocalFeatureChannels>, String>::new(XmlNode::new(
        XmlName::try_from(route_name).unwrap(),
    ))
    .state_with(|| ())
    .on_open(|_, _| StreamUpdate::from_emission(TurnEmission::Live(LocalFeatureLiveEffect::Opened)))
    .into_durable_component(RuntimeContract::new(runtime_name, "v1").unwrap());

    MountedFeature::new(durable_system(runtime), move |context| {
        user_renders.fetch_add(1, Ordering::SeqCst);
        feature_user_fragment("local_feature_task", &context.props().task)
    })
}

#[view(component)]
fn composed_local_feature(
    user_renders: Arc<AtomicUsize>,
) -> MountedFeature<PublicLiveChannels, TurnProps> {
    map_local_feature(positional_local_feature(
        Arc::clone(&user_renders),
        "public.local-feature-parent-first",
        "public_live_parent_first",
    ))
    .compose(map_local_feature(positional_local_feature(
        user_renders,
        "public.local-feature-parent-second",
        "public_live_parent_second",
    )))
}

fn mapped_local_feature_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    keyed_mapped_local_feature_definition(captures, user_renders, "local-feature")
}

fn keyed_mapped_local_feature_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
    key: &'static str,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    map_local_feature(local_feature(Arc::clone(&user_renders)).key(key))
        .into_harness(EpochContractId::new("public/local-feature-map/v1").unwrap())
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

fn map_local_feature(
    feature: MountedFeature<LocalFeatureChannels, TurnProps>,
) -> MountedFeature<PublicLiveChannels, TurnProps> {
    feature.map_channels(
        TurnChannelMap::<LocalFeatureChannels, PublicLiveChannels>::builder()
            .output(Never::absurd)
            .live(|effect| match effect {
                LocalFeatureLiveEffect::Opened => PublicLiveEffect::Opened,
            })
            .commit(Never::absurd)
            .diagnostic(|diagnostic| diagnostic)
            .build(),
    )
}

fn positional_local_feature_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    map_local_feature(positional_local_feature(
        Arc::clone(&user_renders),
        "public.local-feature-positional-first",
        "public_live_positional_first",
    ))
    .compose(map_local_feature(positional_local_feature(
        Arc::clone(&user_renders),
        "public.local-feature-positional-second",
        "public_live_positional_second",
    )))
    .into_harness(EpochContractId::new("public/local-feature-map/v1").unwrap())
    .with_capture(PublicCapture {
        captures,
        user_renders,
    })
}

fn keyed_composed_local_feature_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    composed_local_feature(Arc::clone(&user_renders))
        .key("composed-parent")
        .into_harness(EpochContractId::new("public/local-feature-keyed-parent/v1").unwrap())
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

fn duplicate_keyed_local_feature_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    map_local_feature(local_feature(Arc::clone(&user_renders)).key("duplicate-feature"))
        .compose(map_local_feature(
            local_feature(Arc::clone(&user_renders)).key("duplicate-feature"),
        ))
        .into_harness(EpochContractId::new("public/local-feature-duplicate-key/v1").unwrap())
        .with_capture(PublicCapture {
            captures,
            user_renders,
        })
}

fn direct_keyed_feature_definition(
    captures: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicLiveChannels, PublicCapture> {
    MountedFeature::<PublicLiveChannels, TurnProps>::user_only(|context| {
        feature_user_fragment("direct_feature_task", &context.props().task)
    })
    .key("requires-component")
    .into_harness(EpochContractId::new("public/direct-feature-key/v1").unwrap())
    .with_capture(PublicCapture {
        captures,
        user_renders,
    })
}

fn public_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<PublicChannels, TurnProps> {
    cx.props().system_renders.fetch_add(1, Ordering::SeqCst);

    let tool_spec = ProviderToolSpec::new(
        "lookup_world_state",
        "Look up immutable world state for this turn",
        json!({ "type": "object", "properties": {} }),
    )
    .unwrap();

    system_view(
        durable_system((
            Document::from_xml(XmlNode::new(XmlName::try_from("public_policy").unwrap())),
            public_selection(),
            durable_provider_tool(
                RuntimeContract::new("public.world-lookup", "v1").unwrap(),
                (),
                tool_spec,
                || PublicDispatcher,
            ),
        ))
        .into_one_shot_component(),
    )
}

fn public_user(cx: UserTurnContext<'_, TurnProps>) -> UserView {
    cx.props().user_renders.fetch_add(1, Ordering::SeqCst);
    let mut task = XmlNode::new(XmlName::try_from("task").unwrap());
    task.push(MixedContent::text(TextNode::new(&cx.props().task)));
    user_view(Document::from_xml(task))
}

struct ProjectedFeatureProps {
    selection_seed: usize,
}

struct ProjectedHarnessProps {
    feature: ProjectedFeatureProps,
    _unrelated_task: String,
}

struct ProjectionMountProps {
    binding_props: Arc<StdMutex<Vec<usize>>>,
    dispatcher_props: Arc<StdMutex<Vec<usize>>>,
}

#[derive(Debug)]
struct ProjectedBinding;

impl BindingInstance<PublicChannels> for ProjectedBinding {}

#[derive(Debug)]
struct ProjectedDispatcher;

#[async_trait::async_trait]
impl ProviderDispatcher<PublicChannels> for ProjectedDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<PublicChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("ok"),
            StreamUpdate::new(),
        ))
    }
}

#[view(component)]
fn projected_feature(
    binding_props: Arc<StdMutex<Vec<usize>>>,
    dispatcher_props: Arc<StdMutex<Vec<usize>>>,
) -> Component<PublicChannels, ProjectedFeatureProps> {
    let tool = ProviderToolSpec::new(
        "projected_lookup",
        "Read the projected feature props",
        json!({ "type": "object", "properties": {} }),
    )?;
    component((
        binding_factory_with_context::<PublicChannels, ProjectedFeatureProps, _>(
            "projected_selection",
            Document::from_xml(XmlNode::new(
                XmlName::try_from("projected_selection").unwrap(),
            )),
            RuntimeRoute::xml("projected_selection").unwrap(),
            move |context| {
                binding_props
                    .lock()
                    .unwrap()
                    .push(context.props().selection_seed);
                Ok(ProjectedBinding)
            },
        ),
        provider_tools_with_context::<PublicChannels, ProjectedFeatureProps, _, _>(
            "projected_lookup",
            [tool],
            move |context| {
                dispatcher_props
                    .lock()
                    .unwrap()
                    .push(context.props().selection_seed);
                Ok(ProjectedDispatcher)
            },
        ),
    ))
}

fn projected_system(
    cx: SystemMountContext<'_, ProjectionMountProps>,
) -> SystemView<PublicChannels, ProjectedHarnessProps> {
    system_view(
        projected_feature(
            Arc::clone(&cx.props().binding_props),
            Arc::clone(&cx.props().dispatcher_props),
        )
        .project_props(|props: &ProjectedHarnessProps| &props.feature),
    )
}

#[view(component)]
fn projected_durable_feature(
    binding_props: Arc<StdMutex<Vec<usize>>>,
    dispatcher_props: Arc<StdMutex<Vec<usize>>>,
) -> DurableSystem<PublicChannels, ProjectedFeatureProps> {
    let tool = ProviderToolSpec::new(
        "projected_durable_lookup",
        "Read the projected durable feature props",
        json!({ "type": "object", "properties": {} }),
    )?;
    durable_system((
        durable_binding_factory_with_context::<PublicChannels, ProjectedFeatureProps, _>(
            RuntimeContract::new("public.projected-selection", "v1").unwrap(),
            Document::from_xml(XmlNode::new(
                XmlName::try_from("projected_durable_selection").unwrap(),
            )),
            RuntimeRoute::xml("projected_durable_selection").unwrap(),
            move |context| {
                binding_props
                    .lock()
                    .unwrap()
                    .push(context.props().selection_seed);
                Ok(ProjectedBinding)
            },
        ),
        durable_provider_tool_with_context::<PublicChannels, ProjectedFeatureProps, _>(
            RuntimeContract::new("public.projected-lookup", "v1").unwrap(),
            (),
            tool,
            move |context| {
                dispatcher_props
                    .lock()
                    .unwrap()
                    .push(context.props().selection_seed);
                Ok(ProjectedDispatcher)
            },
        ),
    ))
}

fn projected_durable_system(
    cx: SystemMountContext<'_, ProjectionMountProps>,
) -> SystemView<PublicChannels, ProjectedHarnessProps> {
    system_view(
        projected_durable_feature(
            Arc::clone(&cx.props().binding_props),
            Arc::clone(&cx.props().dispatcher_props),
        )
        .project_props(|props: &ProjectedHarnessProps| &props.feature)
        .into_one_shot_component(),
    )
}

struct FeatureTaskProps {
    task: String,
}

struct FeatureTurnProps {
    intent: ProjectedFeatureProps,
    task: FeatureTaskProps,
}

struct FeatureCallProps {
    selection_seed: usize,
    task: String,
}

struct FeatureCapture {
    captures: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl MountedTurnCapture for FeatureCapture {
    type Transcript = String;
    type ContextState = usize;
    type CallProps = FeatureCallProps;
    type TurnProps = FeatureTurnProps;
    type Source = str;
    type Error = Infallible;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<'_, String, usize, FeatureCallProps, str>,
    ) -> Result<Self::TurnProps, Self::Error> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        Ok(FeatureTurnProps {
            intent: ProjectedFeatureProps {
                selection_seed: context.call_props().selection_seed,
            },
            task: FeatureTaskProps {
                task: format!("{}:{}", context.call_props().task, context.source()),
            },
        })
    }
}

fn feature_user_fragment(name: &str, value: impl AsRef<str>) -> PomView {
    let mut node = XmlNode::new(XmlName::try_from(name).unwrap());
    node.push(MixedContent::text(TextNode::new(value.as_ref())));
    pom_view(Document::from_xml(node))
}

#[view(component)]
fn reusable_feature_policy(system_renders: Arc<AtomicUsize>) -> PomView {
    system_renders.fetch_add(1, Ordering::SeqCst);
    pom_view(Document::from_xml(XmlNode::new(
        XmlName::try_from("reusable_feature_policy").unwrap(),
    )))
}

#[view(component)]
fn reusable_intent_feature(
    binding_props: Arc<StdMutex<Vec<usize>>>,
    dispatcher_props: Arc<StdMutex<Vec<usize>>>,
    system_renders: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> MountedFeature<PublicChannels, ProjectedFeatureProps> {
    MountedFeature::new(
        durable_system((
            reusable_feature_policy(system_renders),
            projected_durable_feature(binding_props, dispatcher_props),
        )),
        move |context| {
            user_renders.fetch_add(1, Ordering::SeqCst);
            feature_user_fragment("intent_seed", context.props().selection_seed.to_string())
        },
    )
}

#[view(component)]
fn reusable_task_feature(
    user_renders: Arc<AtomicUsize>,
) -> MountedFeature<PublicChannels, FeatureTaskProps> {
    MountedFeature::new(
        durable_system(Document::from_xml(XmlNode::new(
            XmlName::try_from("feature_task_policy").unwrap(),
        ))),
        move |context| {
            user_renders.fetch_add(1, Ordering::SeqCst);
            feature_user_fragment("task", &context.props().task)
        },
    )
}

fn reusable_feature_definition(
    captures: Arc<AtomicUsize>,
    binding_props: Arc<StdMutex<Vec<usize>>>,
    dispatcher_props: Arc<StdMutex<Vec<usize>>>,
    system_renders: Arc<AtomicUsize>,
    user_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<PublicChannels, FeatureCapture> {
    reusable_intent_feature(
        binding_props,
        dispatcher_props,
        system_renders,
        Arc::clone(&user_renders),
    )
    .key("intent-feature")
    .project_props(|props: &FeatureTurnProps| &props.intent)
    .compose(
        reusable_task_feature(user_renders)
            .key("task-feature")
            .project_props(|props: &FeatureTurnProps| &props.task),
    )
    .into_harness(EpochContractId::new("public/reusable-feature/v1").unwrap())
    .with_capture(FeatureCapture { captures })
}

#[test]
fn durable_authoring_can_be_explicitly_erased_for_one_shot_mounts() {
    let system_renders = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch_with_contract(
        EpochContractId::new("public-authoring/v1").unwrap(),
        &MountProps {
            system_renders: Arc::clone(&system_renders),
        },
        public_system,
    )
    .unwrap();

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(
        epoch.epoch_contract_id().unwrap().as_str(),
        "public-authoring/v1"
    );
    assert_eq!(epoch.binding_factories().len(), 1);
    assert_eq!(epoch.provider_capabilities().len(), 1);
    assert_eq!(
        epoch.provider_tool_catalog().specs()[0].name(),
        "lookup_world_state"
    );

    let user_renders = Arc::new(AtomicUsize::new(0));
    for task in ["inspect the plaza", "continue after compaction"] {
        let plan = epoch
            .prepare_user(
                &TurnProps {
                    user_renders: Arc::clone(&user_renders),
                    task: task.to_owned(),
                },
                public_user,
            )
            .unwrap();
        let (resolved, _) =
            resolve_user_document(plan.into_document(), &UserDocumentCursor::default()).unwrap();
        assert_eq!(
            render_pom_document(&resolved).unwrap(),
            format!("<task>{task}</task>")
        );
        assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    }
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
}

#[test]
fn public_component_props_projection_rebinds_factory_and_dispatcher_contexts() {
    let binding_props = Arc::new(StdMutex::new(Vec::new()));
    let dispatcher_props = Arc::new(StdMutex::new(Vec::new()));
    let epoch = mount_system_epoch(
        &ProjectionMountProps {
            binding_props: Arc::clone(&binding_props),
            dispatcher_props: Arc::clone(&dispatcher_props),
        },
        projected_system,
    )
    .unwrap();

    assert_eq!(epoch.binding_factories().len(), 1);
    assert_eq!(epoch.provider_capabilities().len(), 1);
    let props = ProjectedHarnessProps {
        feature: ProjectedFeatureProps { selection_seed: 41 },
        _unrelated_task: "ignored by the reusable feature".to_owned(),
    };
    let prepared = epoch
        .begin_turn("projected-feature")
        .prepare_user(&props, |_| user_view(()))
        .unwrap();
    let _attempt = prepared.start_streaming_attempt(NoLiveEffects).unwrap();

    assert_eq!(*binding_props.lock().unwrap(), vec![41]);
    assert_eq!(*dispatcher_props.lock().unwrap(), vec![41]);
}

#[test]
fn public_durable_feature_props_projection_preserves_one_contract_tree() {
    let binding_props = Arc::new(StdMutex::new(Vec::new()));
    let dispatcher_props = Arc::new(StdMutex::new(Vec::new()));
    let epoch = mount_system_epoch_with_contract(
        EpochContractId::new("public-projected-feature/v1").unwrap(),
        &ProjectionMountProps {
            binding_props: Arc::clone(&binding_props),
            dispatcher_props: Arc::clone(&dispatcher_props),
        },
        projected_durable_system,
    )
    .unwrap();

    assert_eq!(epoch.binding_factories().len(), 1);
    assert_eq!(epoch.provider_capabilities().len(), 1);
    let props = ProjectedHarnessProps {
        feature: ProjectedFeatureProps { selection_seed: 73 },
        _unrelated_task: "the durable feature cannot access this field".to_owned(),
    };
    let prepared = epoch
        .begin_turn("projected-durable-feature")
        .prepare_user(&props, |_| user_view(()))
        .unwrap();
    let _attempt = prepared.start_streaming_attempt(NoLiveEffects).unwrap();

    assert_eq!(*binding_props.lock().unwrap(), vec![73]);
    assert_eq!(*dispatcher_props.lock().unwrap(), vec![73]);
}

#[tokio::test]
async fn public_fallible_user_renderer_stops_before_provider_execution() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = PublicInMemoryFactory::new(
        DurableSessionId::new("public/fallible-user").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-fallible-user-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(fallible_user_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();

    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-fallible-user-call").unwrap(),
                DurableCallInputId::new("public-fallible-user-call/input-v1").unwrap(),
                "fallible-user",
                Arc::<str>::from("invalid user root"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        call.wait().await,
        Err(MountedWaitError::PreparationFailed { .. })
    ));
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn public_mounted_features_compose_durable_and_user_contributions() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = PublicInMemoryFactory::new(
        DurableSessionId::new("public/reusable-feature").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-reusable-feature-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let binding_props = Arc::new(StdMutex::new(Vec::new()));
    let dispatcher_props = Arc::new(StdMutex::new(Vec::new()));
    let system_renders = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(reusable_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&binding_props),
            Arc::clone(&dispatcher_props),
            Arc::clone(&system_renders),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();

    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 0);
    {
        let attached_systems = provider.attached_systems.lock().unwrap();
        let system = attached_systems
            .first()
            .expect("the mounted feature tree attaches one System prompt");
        assert!(system.contains("projected_durable_selection"));
        assert!(system.contains("feature_task_policy"));
        assert!(
            system.find("projected_durable_selection") < system.find("feature_task_policy"),
            "composed features preserve durable System order: {system}"
        );
    }

    let source = Arc::<str>::from("feature-source");
    for (call, input, selection_seed, task) in [
        (
            "public-feature-call-1",
            "public-feature-call-1/input-v1",
            17,
            "inspect plaza",
        ),
        (
            "public-feature-call-2",
            "public-feature-call-2/input-v1",
            29,
            "inspect forum",
        ),
    ] {
        let mut call = agent
            .start(
                MountedCallInput::new(
                    DurableCallId::new(call).unwrap(),
                    DurableCallInputId::new(input).unwrap(),
                    call,
                    Arc::new(FeatureCallProps {
                        selection_seed,
                        task: task.to_owned(),
                    }),
                    Arc::clone(&source),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            call.wait().await.unwrap(),
            MountedCallOutcome::Executed { .. }
        ));
    }

    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 4);
    assert_eq!(*binding_props.lock().unwrap(), vec![17, 29]);
    assert_eq!(*dispatcher_props.lock().unwrap(), vec![17, 29]);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.users.lock().unwrap().as_slice(),
        [
            "<intent_seed>17</intent_seed>\n\n<task>inspect plaza:feature-source</task>",
            "<intent_seed>29</intent_seed>\n\n<task>inspect forum:feature-source</task>",
        ]
    );
}

#[tokio::test]
async fn public_mounted_feature_reopen_does_not_render_system_or_user() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = PublicInMemoryFactory::new(
        DurableSessionId::new("public/reusable-feature-reopen").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-reusable-feature-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let binding_props = Arc::new(StdMutex::new(Vec::new()));
    let dispatcher_props = Arc::new(StdMutex::new(Vec::new()));
    let system_renders = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));

    let first = factory
        .open(reusable_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&binding_props),
            Arc::clone(&dispatcher_props),
            Arc::clone(&system_renders),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 0);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    drop(first);

    let reopened = factory
        .open(reusable_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&binding_props),
            Arc::clone(&dispatcher_props),
            Arc::clone(&system_renders),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 0);
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    drop(reopened);
}

#[test]
fn public_mounted_definition_keeps_system_and_user_roots_separate() {
    let user_renders = Arc::new(AtomicUsize::new(0));
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("public-mounted-definition/v1").unwrap(),
        durable_system((
            Document::from_xml(XmlNode::new(XmlName::try_from("public_policy").unwrap())),
            public_selection(),
        )),
    );
    let definition = MountedHarnessDefinition::new(epoch, public_user);

    assert_eq!(
        definition.epoch().epoch_contract_id().as_str(),
        "public-mounted-definition/v1"
    );
    let props = TurnProps {
        user_renders: Arc::clone(&user_renders),
        task: "inspect the plaza".to_owned(),
    };
    let _ = public_user(UserTurnContext::new(&props));
    assert_eq!(user_renders.load(Ordering::SeqCst), 1);

    let captures = Arc::new(AtomicUsize::new(0));
    let captured = definition.with_capture(PublicCapture {
        captures: Arc::clone(&captures),
        user_renders,
    });
    let _: &CapturedMountedHarnessDefinition<PublicChannels, PublicCapture> = &captured;
    assert_eq!(
        captured.epoch().epoch_contract_id().as_str(),
        "public-mounted-definition/v1"
    );
    assert_eq!(captures.load(Ordering::SeqCst), 0);
}

#[test]
fn external_provider_can_implement_durable_attach_and_pom_free_rehydrate() {
    let provider = PublicDurableProvider;
    let contract = provider.durable_provider_adapter_contract();
    assert_eq!(contract.adapter(), "public-test-provider");
    assert_eq!(contract.receipt_schema_version(), 1);
}

#[tokio::test]
async fn public_in_memory_factory_uses_the_authoritative_owner_lifecycle() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = PublicInMemoryFactory::new(
        DurableSessionId::new("public/in-memory-lifecycle").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-lifecycle-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let first_captures = Arc::new(AtomicUsize::new(0));
    let first_user_renders = Arc::new(AtomicUsize::new(0));
    let first = factory
        .open(public_lifecycle_definition(
            Arc::clone(&first_captures),
            Arc::clone(&first_user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let stale_owner = factory
        .open(public_lifecycle_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates_with_cursor.load(Ordering::SeqCst), 0);

    let first_call_id = DurableCallId::new("public-in-memory-call-1").unwrap();
    let first_input_id = DurableCallInputId::new("public-in-memory-call-1/input-v1").unwrap();
    let second_call_id = DurableCallId::new("public-in-memory-call-2").unwrap();
    let second_input_id = DurableCallInputId::new("public-in-memory-call-2/input-v1").unwrap();
    let source = Arc::<str>::from("public-source");
    assert!(first
        .lookup(&DurableCallId::new("public-in-memory-missing").unwrap())
        .await
        .unwrap()
        .is_none());
    let mut first_call = first
        .start(
            MountedCallInput::new(
                first_call_id.clone(),
                first_input_id.clone(),
                "public-lifecycle-first",
                Arc::<str>::from("inspect the plaza"),
                Arc::clone(&source),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        first_call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(first_captures.load(Ordering::SeqCst), 1);
    assert_eq!(first_user_renders.load(Ordering::SeqCst), 1);
    let first_snapshot = first.lookup(&first_call_id).await.unwrap().unwrap();
    assert_eq!(first_snapshot.call_id(), &first_call_id);
    assert_eq!(first_snapshot.input_id(), &first_input_id);
    assert_eq!(first_snapshot.next_turn_index(), 1);
    match first_snapshot.lifecycle() {
        MountedCallLifecycle::Settled { result } => {
            assert_eq!(result.call_id(), &first_call_id);
            assert_eq!(result.input_id(), &first_input_id);
        }
        lifecycle => panic!("expected settled lookup, got {lifecycle:?}"),
    }
    assert!(matches!(
        first.reattach(&first_call_id).await.unwrap(),
        MountedCallReattachment::Observed { snapshot }
            if matches!(snapshot.lifecycle(), MountedCallLifecycle::Settled { .. })
    ));
    assert!(matches!(
        first
            .reattach(&DurableCallId::new("public-in-memory-missing").unwrap())
            .await
            .unwrap(),
        MountedCallReattachment::NotFound
    ));

    let mut second_call = first
        .start(
            MountedCallInput::new(
                second_call_id.clone(),
                second_input_id.clone(),
                "public-lifecycle-second",
                Arc::<str>::from("inspect the forum"),
                Arc::clone(&source),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        second_call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(first_captures.load(Ordering::SeqCst), 2);
    assert_eq!(first_user_renders.load(Ordering::SeqCst), 2);

    stale_owner.reload().await.unwrap();
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 2);
    assert_eq!(provider.rehydrates_with_cursor.load(Ordering::SeqCst), 1);
    drop(first);
    drop(stale_owner);
    let reopened_factory = factory.clone();
    drop(factory);

    let reopened = reopened_factory
        .open(public_lifecycle_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 3);
    assert_eq!(provider.rehydrates_with_cursor.load(Ordering::SeqCst), 2);
    let reopened_first = reopened.lookup(&first_call_id).await.unwrap().unwrap();
    assert!(matches!(
        reopened_first.lifecycle(),
        MountedCallLifecycle::Settled { .. }
    ));
    assert!(matches!(
        reopened.reattach(&first_call_id).await.unwrap(),
        MountedCallReattachment::Observed { snapshot }
            if matches!(snapshot.lifecycle(), MountedCallLifecycle::Settled { .. })
    ));

    let mut replay = reopened
        .start(
            MountedCallInput::new(
                second_call_id,
                second_input_id,
                "public-lifecycle-replay",
                Arc::<str>::from("ignored replay props"),
                source,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(replay.wait().await.unwrap().is_replayed());
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(first_captures.load(Ordering::SeqCst), 2);
    assert_eq!(first_user_renders.load(Ordering::SeqCst), 2);

    {
        let operations = provider.operations.lock().unwrap();
        assert_eq!(operations.len(), 2);
        assert_eq!(operations[0].call_label, "public-lifecycle-first");
        assert_eq!(operations[0].session_id, "public/in-memory-lifecycle");
        assert_eq!(operations[0].call_id, first_call_id.as_str());
        assert_eq!(operations[0].input_id, first_input_id.as_str());
        assert_eq!(operations[0].turn_index, 0);
        assert_eq!(
            operations[0].request_id,
            "public/in-memory-lifecycle/public-in-memory-call-1/turn-0"
        );
        assert_eq!(operations[1].call_label, "public-lifecycle-second");
        assert_eq!(operations[1].session_id, "public/in-memory-lifecycle");
        assert_eq!(operations[1].call_id, "public-in-memory-call-2");
        assert_eq!(operations[1].input_id, "public-in-memory-call-2/input-v1");
        assert_eq!(operations[1].turn_index, 0);
        assert_eq!(
            operations[1].request_id,
            "public/in-memory-lifecycle/public-in-memory-call-2/turn-0"
        );
        assert_ne!(operations[0].durable_epoch_id, "");
        assert_eq!(
            operations[0].durable_epoch_id,
            operations[1].durable_epoch_id
        );
        assert_ne!(
            operations[0].remote_idempotency_key,
            operations[1].remote_idempotency_key
        );
        assert!(operations.iter().all(|operation| operation
            .remote_idempotency_key
            .starts_with("agentview.provider-operation.v1:")));
    }

    let mismatch = reopened
        .start(
            MountedCallInput::new(
                first_call_id.clone(),
                DurableCallInputId::new("public-in-memory-call-1/input-v2").unwrap(),
                "public-lifecycle-mismatch",
                Arc::<str>::from("changed input"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        mismatch,
        MountedStartError::CallInputMismatch {
            call_id: actual_call_id,
            expected,
            actual,
        } if actual_call_id == first_call_id
            && expected == first_input_id
            && actual.as_str() == "public-in-memory-call-1/input-v2"
    ));
}

#[tokio::test]
async fn public_reopen_keeps_the_installed_system_when_the_contract_id_is_unchanged() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = PublicInMemoryFactory::new(
        DurableSessionId::new("public/in-memory-system-policy").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-policy-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));

    let first = factory
        .open(public_lifecycle_definition_with_policy(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
            "public_policy_a",
            NonZeroUsize::new(2).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    drop(first);

    let reopened = factory
        .open(public_lifecycle_definition_with_policy(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
            "public_policy_b",
            NonZeroUsize::new(2).unwrap(),
        ))
        .await
        .unwrap();

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    let attached_systems = provider.attached_systems.lock().unwrap();
    assert_eq!(attached_systems.len(), 1);
    assert!(attached_systems[0].contains("public_policy_a"));
    assert!(!attached_systems[0].contains("public_policy_b"));
    drop(attached_systems);
    drop(reopened);
}

#[tokio::test]
async fn public_reopen_rejects_a_changed_harness_loop_policy_without_rerendering_system() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = PublicInMemoryFactory::new(
        DurableSessionId::new("public/in-memory-loop-policy-contract").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-loop-policy-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));

    let first = factory
        .open(public_lifecycle_definition_with_policy(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
            "public_policy_a",
            NonZeroUsize::new(2).unwrap(),
        ))
        .await
        .unwrap();
    drop(first);

    let error = factory
        .open(public_lifecycle_definition_with_policy(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
            "public_policy_a",
            NonZeroUsize::new(3).unwrap(),
        ))
        .await
        .expect_err("a changed durable loop policy must require a new epoch");

    assert!(matches!(error, MountedOpenError::ContractMismatch { .. }));
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
    assert_eq!(provider.attached_systems.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn public_harness_loop_policy_recaptures_user_for_each_continuation() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = InMemoryMountedAgentFactory::<
        PublicChannels,
        String,
        usize,
        PublicLifecycleProvider,
        PublicContinuationReducer,
        fn() -> NoLiveEffects,
    >::new(
        DurableSessionId::new("public/in-memory-continuation").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-continuation-model",
        128,
        PublicContinuationReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_lifecycle_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-continuation-call").unwrap(),
                DurableCallInputId::new("public-continuation-call/input-v1").unwrap(),
                "public-continuation",
                Arc::<str>::from("retry"),
                Arc::<str>::from("source"),
            )
            .unwrap()
            .with_turn_cap(NonZeroUsize::new(3).unwrap()),
        )
        .await
        .unwrap();

    let outcome = call.wait().await.unwrap();
    let MountedCallOutcome::Executed { records, .. } = outcome else {
        panic!("the first owner must execute the continuation chain");
    };
    assert_eq!(records.len(), 2);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        *provider.users.lock().unwrap(),
        [
            "<task>retry:source:history-0</task>",
            "<task>retry:source:history-1</task>",
        ]
    );
}

#[tokio::test]
async fn public_harness_loop_policy_rejects_a_larger_call_cap() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = InMemoryMountedAgentFactory::<
        PublicChannels,
        String,
        usize,
        PublicLifecycleProvider,
        PublicOverBudgetContinuationReducer,
        fn() -> NoLiveEffects,
    >::new(
        DurableSessionId::new("public/in-memory-harness-loop-policy").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-harness-loop-policy-model",
        128,
        PublicOverBudgetContinuationReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_lifecycle_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();

    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-harness-loop-policy-call").unwrap(),
                DurableCallInputId::new("public-harness-loop-policy-call/input-v1").unwrap(),
                "public-harness-loop-policy",
                Arc::<str>::from("continue past the policy"),
                Arc::<str>::from("source"),
            )
            .unwrap()
            .with_turn_cap(NonZeroUsize::new(3).unwrap()),
        )
        .await
        .unwrap();

    assert!(matches!(
        call.wait().await,
        Err(MountedWaitError::Unavailable { message }) if message.contains("max_turns=2")
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_turn_cap_can_tighten_a_harness_loop_policy() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = InMemoryMountedAgentFactory::<
        PublicChannels,
        String,
        usize,
        PublicLifecycleProvider,
        PublicContinuationReducer,
        fn() -> NoLiveEffects,
    >::new(
        DurableSessionId::new("public/in-memory-turn-cap").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-turn-cap-model",
        128,
        PublicContinuationReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_lifecycle_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();

    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-turn-cap-call").unwrap(),
                DurableCallInputId::new("public-turn-cap-call/input-v1").unwrap(),
                "public-turn-cap",
                Arc::<str>::from("stop after the first turn"),
                Arc::<str>::from("source"),
            )
            .unwrap()
            .with_turn_cap(NonZeroUsize::MIN),
        )
        .await
        .unwrap();

    assert!(matches!(
        call.wait().await,
        Err(MountedWaitError::Unavailable { message }) if message.contains("max_turns=1")
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_in_memory_factories_are_isolated_even_with_the_same_session_id() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let session_id = DurableSessionId::new("public/in-memory-isolated-hosts").unwrap();
    let first_factory = PublicInMemoryFactory::new(
        session_id.clone(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-isolated-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let second_factory = PublicInMemoryFactory::new(
        session_id,
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-isolated-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));

    let _first = first_factory
        .open(public_lifecycle_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let _second = second_factory
        .open(public_lifecycle_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();

    assert_eq!(system_renders.load(Ordering::SeqCst), 2);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 2);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn public_in_memory_factory_awaits_live_effects_and_joins_cancellation() {
    let provider = Arc::new(PublicLiveProvider::default());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let live_bindings = Arc::new(StdMutex::new(Vec::new()));
    let live_abort_identities = Arc::new(StdMutex::new(Vec::new()));
    let live_factory = PublicLiveFactory {
        bindings: Arc::clone(&live_bindings),
        applied: Arc::clone(&live_applied),
        aborted: Arc::clone(&live_aborted),
        abort_identities: Arc::clone(&live_abort_identities),
    };
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-live").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-live-model",
        128,
        PublicLiveReducer,
        live_factory,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-call").unwrap(),
                DurableCallInputId::new("public-live-call/input-v1").unwrap(),
                "public-live",
                Arc::<str>::from("stream and wait"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 1 {
            break;
        }
        entered.await;
    }
    assert_eq!(live_applied.load(Ordering::SeqCst), 1);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 0);
    let running = tokio::time::timeout(
        Duration::from_millis(20),
        agent.lookup(&DurableCallId::new("public-live-call").unwrap()),
    )
    .await
    .expect("lookup must not block behind an active call")
    .unwrap()
    .expect("the active call remains registered after admission");
    assert!(matches!(running.lifecycle(), MountedCallLifecycle::Running));
    drop(call);
    let mut call = match agent
        .reattach(&DurableCallId::new("public-live-call").unwrap())
        .await
        .unwrap()
    {
        MountedCallReattachment::Attached { call } => call,
        other => panic!("expected same-process reattachment, got {other:?}"),
    };
    let concurrent = match agent
        .reattach(&DurableCallId::new("public-live-call").unwrap())
        .await
        .unwrap()
    {
        MountedCallReattachment::Observed { snapshot } => snapshot,
        other => panic!("expected an observation while a handle is attached, got {other:?}"),
    };
    assert!(matches!(
        concurrent.lifecycle(),
        MountedCallLifecycle::Running
    ));

    assert!(tokio::time::timeout(Duration::from_millis(20), call.wait())
        .await
        .is_err());
    assert_eq!(call.cancel().await, MountedCallCancellation::Requested);
    assert!(matches!(
        call.wait().await,
        Err(MountedWaitError::RecoveryRequired { .. })
    ));
    let recovery = agent
        .lookup(&DurableCallId::new("public-live-call").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        recovery.lifecycle(),
        MountedCallLifecycle::RecoveryRequired {
            reason: MountedCallRecoveryReason::CancellationIndeterminate
        }
    ));

    let successor_id = DurableCallId::new("public-live-successor").unwrap();
    let successor_error = agent
        .start(
            MountedCallInput::new(
                successor_id.clone(),
                DurableCallInputId::new("public-live-successor/input-v1").unwrap(),
                "public-live-successor",
                Arc::<str>::from("must remain fenced"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        successor_error,
        MountedStartError::RecoveryRequired { call_id } if call_id == DurableCallId::new("public-live-call").unwrap()
    ));

    assert_eq!(provider.joined.load(Ordering::SeqCst), 1);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 1);

    let bindings = live_bindings.lock().unwrap();
    assert_eq!(bindings.len(), 1);
    let binding = &bindings[0];
    assert_eq!(binding.session_id, "public/in-memory-live");
    assert_eq!(binding.epoch_contract_id, "public-live/v1");
    assert_eq!(binding.call_id, "public-live-call");
    assert_eq!(binding.input_id, "public-live-call/input-v1");
    assert_eq!(binding.turn_index, 0);
    assert_eq!(binding.call_props, "stream and wait");
    assert_eq!(binding.source, "public-source");
    assert_eq!(binding.task, "stream and wait:public-source:history-0");
    assert_eq!(binding.attempt.call_label(), "public-live");
    assert_eq!(
        live_abort_identities.lock().unwrap().as_slice(),
        std::slice::from_ref(&binding.attempt)
    );
}

#[tokio::test]
async fn public_in_memory_invalid_resume_cursor_requires_recovery() {
    let provider = Arc::new(PublicLiveProvider::cancelling_with_invalid_resume_cursor());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-invalid-resume-cursor").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-invalid-resume-cursor-model",
        128,
        PublicLiveReducer,
        PublicLiveFactory {
            bindings: Arc::new(StdMutex::new(Vec::new())),
            applied: Arc::clone(&live_applied),
            aborted: Arc::clone(&live_aborted),
            abort_identities: Arc::new(StdMutex::new(Vec::new())),
        },
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let call_id = DurableCallId::new("public-invalid-resume-cursor-call").unwrap();
    let mut call = agent
        .start(
            MountedCallInput::new(
                call_id.clone(),
                DurableCallInputId::new("public-invalid-resume-cursor-call/input-v1").unwrap(),
                "public-invalid-resume-cursor",
                Arc::<str>::from("cancel with invalid resume cursor"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 1 {
            break;
        }
        entered.await;
    }
    assert_eq!(call.cancel().await, MountedCallCancellation::Requested);
    assert!(matches!(
        call.wait().await,
        Err(MountedWaitError::RecoveryRequired { .. })
    ));
    let successor = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-invalid-resume-cursor-successor").unwrap(),
                DurableCallInputId::new("public-invalid-resume-cursor-successor/input-v1").unwrap(),
                "public-invalid-resume-cursor-successor",
                Arc::<str>::from("must remain fenced"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        successor,
        MountedStartError::RecoveryRequired { call_id: actual } if actual == call_id
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(provider.joined.load(Ordering::SeqCst), 1);
    assert_eq!(live_applied.load(Ordering::SeqCst), 1);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_in_memory_cancellation_with_unchanged_cursor_releases_successor() {
    let provider = Arc::new(PublicLiveProvider::cancelling_without_cursor_change());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let live_factory = PublicLiveFactory {
        bindings: Arc::new(StdMutex::new(Vec::new())),
        applied: Arc::clone(&live_applied),
        aborted: Arc::clone(&live_aborted),
        abort_identities: Arc::new(StdMutex::new(Vec::new())),
    };
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-live-unchanged-cancel").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-live-unchanged-cancel-model",
        128,
        PublicLiveReducer,
        live_factory,
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();

    let mut first = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-unchanged-first").unwrap(),
                DurableCallInputId::new("public-live-unchanged-first/input-v1").unwrap(),
                "public-live-unchanged-first",
                Arc::<str>::from("cancel with unchanged cursor"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 1 {
            break;
        }
        entered.await;
    }
    assert_eq!(first.cancel().await, MountedCallCancellation::Requested);
    assert!(matches!(
        first.wait().await,
        Err(MountedWaitError::Cancelled)
    ));

    let mut successor = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-unchanged-successor").unwrap(),
                DurableCallInputId::new("public-live-unchanged-successor/input-v1").unwrap(),
                "public-live-unchanged-successor",
                Arc::<str>::from("the next independent call"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .expect("a stopped call with an unchanged provider cursor must release a successor");
    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 2 {
            break;
        }
        entered.await;
    }
    assert_eq!(successor.cancel().await, MountedCallCancellation::Requested);
    assert!(matches!(
        successor.wait().await,
        Err(MountedWaitError::Cancelled)
    ));

    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(provider.joined.load(Ordering::SeqCst), 2);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(live_applied.load(Ordering::SeqCst), 2);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn public_in_memory_resume_cursor_survives_successor_and_reopen() {
    let provider = Arc::new(PublicLiveProvider::cancelling_with_resume_cursor());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-live-resume-cancel").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-live-resume-cancel-model",
        128,
        PublicLiveReducer,
        PublicLiveFactory {
            bindings: Arc::new(StdMutex::new(Vec::new())),
            applied: Arc::clone(&live_applied),
            aborted: Arc::clone(&live_aborted),
            abort_identities: Arc::new(StdMutex::new(Vec::new())),
        },
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();

    let mut first = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-resume-first").unwrap(),
                DurableCallInputId::new("public-live-resume-first/input-v1").unwrap(),
                "public-live-resume-first",
                Arc::<str>::from("cancel with an authoritative resume cursor"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 1 {
            break;
        }
        entered.await;
    }
    assert_eq!(first.cancel().await, MountedCallCancellation::Requested);
    assert!(matches!(
        first.wait().await,
        Err(MountedWaitError::Cancelled)
    ));

    let mut successor = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-resume-successor").unwrap(),
                DurableCallInputId::new("public-live-resume-successor/input-v1").unwrap(),
                "public-live-resume-successor",
                Arc::<str>::from("the next call receives the accepted cursor"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .expect("an authoritative resume cursor must release a successor");
    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 2 {
            break;
        }
        entered.await;
    }
    {
        let cursors = provider.request_cursors.lock().unwrap();
        assert_eq!(cursors.len(), 2);
        assert!(cursors[0].is_none());
        let cursor = cursors[1]
            .as_ref()
            .expect("successor request receives the cancellation cursor");
        assert_eq!(cursor.adapter(), "public-live-provider");
        assert_eq!(cursor.value(), &json!({ "turn": "resume" }));
    }
    assert_eq!(successor.cancel().await, MountedCallCancellation::Requested);
    assert!(matches!(
        successor.wait().await,
        Err(MountedWaitError::Cancelled)
    ));

    drop(successor);
    drop(first);
    drop(agent);

    let reopened = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .expect("reopen rehydrates the accepted cursor without another System render");
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates_with_cursor.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);

    let mut after_reopen = reopened
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-resume-reopened").unwrap(),
                DurableCallInputId::new("public-live-resume-reopened/input-v1").unwrap(),
                "public-live-resume-reopened",
                Arc::<str>::from("the reopened owner receives the accepted cursor"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .expect("a reopened owner remains usable after cursor rehydrate");
    loop {
        let entered = provider.entered.notified();
        if provider.executions.load(Ordering::SeqCst) == 3 {
            break;
        }
        entered.await;
    }
    assert!(provider.request_cursors.lock().unwrap()[2].is_some());
    assert_eq!(
        after_reopen.cancel().await,
        MountedCallCancellation::Requested
    );
    assert!(matches!(
        after_reopen.wait().await,
        Err(MountedWaitError::Cancelled)
    ));

    assert_eq!(provider.executions.load(Ordering::SeqCst), 3);
    assert_eq!(provider.joined.load(Ordering::SeqCst), 3);
    assert_eq!(captures.load(Ordering::SeqCst), 3);
    assert_eq!(user_renders.load(Ordering::SeqCst), 3);
    assert_eq!(live_applied.load(Ordering::SeqCst), 3);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn public_mounted_feature_maps_a_local_streaming_contract_to_root_channels() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_factory = PublicLiveFactory {
        bindings: Arc::new(StdMutex::new(Vec::new())),
        applied: Arc::clone(&live_applied),
        aborted: Arc::new(AtomicUsize::new(0)),
        abort_identities: Arc::new(StdMutex::new(Vec::new())),
    };
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/local-feature-map").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-local-feature-model",
        128,
        PublicLiveReducer,
        live_factory,
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(mapped_local_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();

    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-local-feature-call").unwrap(),
                DurableCallInputId::new("public-local-feature-call/input-v1").unwrap(),
                "public-local-feature",
                Arc::<str>::from("inspect local contract"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(live_applied.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_mounted_feature_keeps_keyed_binding_identity_after_reopen() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let binding_ids = Arc::new(StdMutex::new(Vec::new()));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/keyed-feature-reopen").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-keyed-feature-model",
        128,
        PublicLiveReducer,
        BindingIdRecordingLiveFactory {
            binding_ids: Arc::clone(&binding_ids),
        },
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));

    let first = factory
        .open(keyed_mapped_local_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            "stable-feature",
        ))
        .await
        .unwrap();
    let mut first_call = first
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-keyed-feature-first").unwrap(),
                DurableCallInputId::new("public-keyed-feature-first/input-v1").unwrap(),
                "public-keyed-feature-first",
                Arc::<str>::from("first keyed feature turn"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        first_call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));
    drop(first);

    let reopened = factory
        .open(keyed_mapped_local_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            "stable-feature",
        ))
        .await
        .unwrap();
    let mut reopened_call = reopened
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-keyed-feature-reopened").unwrap(),
                DurableCallInputId::new("public-keyed-feature-reopened/input-v1").unwrap(),
                "public-keyed-feature-reopened",
                Arc::<str>::from("reopened keyed feature turn"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        reopened_call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));

    let binding_ids = binding_ids.lock().unwrap();
    assert_eq!(binding_ids.len(), 2);
    assert_eq!(binding_ids[0], binding_ids[1]);
    assert!(binding_ids[0].contains("local_feature[stable-feature]"));
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_mounted_feature_key_change_rejects_same_epoch_contract() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/keyed-feature-contract").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-keyed-feature-contract-model",
        128,
        PublicLiveReducer,
        BindingIdRecordingLiveFactory {
            binding_ids: Arc::new(StdMutex::new(Vec::new())),
        },
    )
    .unwrap();

    let first = factory
        .open(keyed_mapped_local_feature_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            "first-feature",
        ))
        .await
        .unwrap();
    drop(first);

    let error = factory
        .open(keyed_mapped_local_feature_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            "second-feature",
        ))
        .await
        .expect_err("a component key change needs a new durable epoch contract");

    assert!(matches!(error, MountedOpenError::ContractMismatch { .. }));
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn public_mounted_feature_same_name_positional_siblings_have_distinct_binding_ids() {
    let provider = Arc::new(PublicLiveProvider::completing_with_text(
        "<public_live_positional_first /><public_live_positional_second />",
    ));
    let binding_ids = Arc::new(StdMutex::new(Vec::new()));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/positional-feature-siblings").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-positional-feature-model",
        128,
        PublicLiveReducer,
        BindingIdRecordingLiveFactory {
            binding_ids: Arc::clone(&binding_ids),
        },
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(positional_local_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();
    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-positional-feature-call").unwrap(),
                DurableCallInputId::new("public-positional-feature-call/input-v1").unwrap(),
                "public-positional-feature",
                Arc::<str>::from("render sibling fragments"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));

    let binding_ids = binding_ids.lock().unwrap();
    assert_eq!(binding_ids.len(), 2);
    assert_ne!(binding_ids[0], binding_ids[1]);
    assert!(binding_ids
        .iter()
        .all(|binding_id| binding_id.contains("positional_local_feature#")));
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_mounted_feature_keyed_parent_groups_multiple_user_fragments() {
    let provider = Arc::new(PublicLiveProvider::completing_with_text(
        "<public_live_parent_first /><public_live_parent_second />",
    ));
    let binding_ids = Arc::new(StdMutex::new(Vec::new()));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/keyed-composed-feature-parent").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-keyed-composed-feature-model",
        128,
        PublicLiveReducer,
        BindingIdRecordingLiveFactory {
            binding_ids: Arc::clone(&binding_ids),
        },
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(keyed_composed_local_feature_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
        ))
        .await
        .unwrap();
    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-keyed-composed-feature-call").unwrap(),
                DurableCallInputId::new("public-keyed-composed-feature-call/input-v1").unwrap(),
                "public-keyed-composed-feature",
                Arc::<str>::from("render grouped child fragments"),
                Arc::<str>::from("public-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        call.wait().await.unwrap(),
        MountedCallOutcome::Executed { .. }
    ));

    let binding_ids = binding_ids.lock().unwrap();
    assert_eq!(binding_ids.len(), 2);
    assert!(binding_ids
        .iter()
        .all(|binding_id| binding_id.contains("composed_local_feature[composed-parent]")));
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_mounted_feature_duplicate_key_is_rejected_before_provider_attach() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/duplicate-feature-key").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-duplicate-feature-key-model",
        128,
        PublicLiveReducer,
        BindingIdRecordingLiveFactory {
            binding_ids: Arc::new(StdMutex::new(Vec::new())),
        },
    )
    .unwrap();

    let error = factory
        .open(duplicate_keyed_local_feature_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ))
        .await
        .expect_err("duplicate component keys must fail before mounting a provider epoch");
    assert!(matches!(error, MountedOpenError::Configuration { .. }));
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 0);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn public_mounted_feature_direct_key_requires_a_component_boundary() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/direct-feature-key").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-direct-feature-key-model",
        128,
        PublicLiveReducer,
        BindingIdRecordingLiveFactory {
            binding_ids: Arc::new(StdMutex::new(Vec::new())),
        },
    )
    .unwrap();

    let error = factory
        .open(direct_keyed_feature_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ))
        .await
        .expect_err("a manually flattened feature cannot receive a component key");
    assert!(matches!(error, MountedOpenError::Configuration { .. }));
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 0);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn discarded_context_replacement_candidates_do_not_bind_live_runtimes() {
    let provider = Arc::new(PublicLiveProvider::completing_after_replacement());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let live_bindings = Arc::new(StdMutex::new(Vec::new()));
    let live_abort_identities = Arc::new(StdMutex::new(Vec::new()));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-live-replacement").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-live-replacement-model",
        128,
        PublicLiveReducer,
        PublicLiveFactory {
            bindings: Arc::clone(&live_bindings),
            applied: Arc::clone(&live_applied),
            aborted: Arc::clone(&live_aborted),
            abort_identities: Arc::clone(&live_abort_identities),
        },
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-replacement-call").unwrap(),
                DurableCallInputId::new("public-live-replacement-call/input-v1").unwrap(),
                "public-live-replacement",
                Arc::<str>::from("replace then stream"),
                Arc::<str>::from("replacement-source"),
            )
            .unwrap()
            .with_max_preparation_rewrites(1),
        )
        .await
        .unwrap();

    assert!(matches!(
        call.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 1
    ));
    assert_eq!(provider.preparations.load(Ordering::SeqCst), 2);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(live_applied.load(Ordering::SeqCst), 1);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 0);
    assert!(live_abort_identities.lock().unwrap().is_empty());
    let bindings = live_bindings.lock().unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0].task,
        "replace then stream:replacement-source:history-1"
    );
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn live_bind_failure_starts_no_provider_and_releases_the_call_reservation() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let binding_attempts = Arc::new(AtomicUsize::new(0));
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let live_abort_identities = Arc::new(StdMutex::new(Vec::new()));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-live-bind-retry").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-live-bind-retry-model",
        128,
        PublicLiveReducer,
        FailOncePublicLiveFactory {
            bindings: Arc::clone(&binding_attempts),
            applied: Arc::clone(&live_applied),
            aborted: Arc::clone(&live_aborted),
            abort_identities: Arc::clone(&live_abort_identities),
        },
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let call_id = DurableCallId::new("public-live-bind-retry-call").unwrap();
    let input_id = DurableCallInputId::new("public-live-bind-retry-call/input-v1").unwrap();
    let input = || {
        MountedCallInput::new(
            call_id.clone(),
            input_id.clone(),
            "public-live-bind-retry",
            Arc::<str>::from("retry the same reserved call"),
            Arc::<str>::from("bind-retry-source"),
        )
        .unwrap()
    };

    let mut failed = agent.start(input()).await.unwrap();
    let first_error = match failed.wait().await {
        Ok(_) => panic!("the first Live binding unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(
        matches!(first_error, MountedWaitError::PreparationFailed { .. }),
        "unexpected first binding error: {first_error:?}"
    );
    assert_eq!(binding_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 0);

    let mut retry = agent.start(input()).await.unwrap();
    assert!(matches!(
        retry.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 1
    ));
    assert_eq!(binding_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(provider.preparations.load(Ordering::SeqCst), 2);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(live_applied.load(Ordering::SeqCst), 1);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 0);
    assert!(live_abort_identities.lock().unwrap().is_empty());
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn public_live_factory_is_fresh_per_continuation_replay_and_reopen() {
    let provider = Arc::new(PublicLiveProvider::completing());
    let live_applied = Arc::new(AtomicUsize::new(0));
    let live_aborted = Arc::new(AtomicUsize::new(0));
    let live_bindings = Arc::new(StdMutex::new(Vec::new()));
    let live_abort_identities = Arc::new(StdMutex::new(Vec::new()));
    let factory = InMemoryMountedAgentFactory::<
        PublicLiveChannels,
        String,
        usize,
        PublicLiveProvider,
        PublicLiveContinuationReducer,
        _,
    >::new(
        DurableSessionId::new("public/in-memory-live-continuation").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-live-continuation-model",
        128,
        PublicLiveContinuationReducer,
        PublicLiveFactory {
            bindings: Arc::clone(&live_bindings),
            applied: Arc::clone(&live_applied),
            aborted: Arc::clone(&live_aborted),
            abort_identities: Arc::clone(&live_abort_identities),
        },
    )
    .unwrap();
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory
        .open(public_live_definition(
            Arc::clone(&captures),
            Arc::clone(&user_renders),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let call_id = DurableCallId::new("public-live-continuation-call").unwrap();
    let input_id = DurableCallInputId::new("public-live-continuation-call/input-v1").unwrap();
    let source = Arc::<str>::from("public-live-continuation-source");
    let mut call = agent
        .start(
            MountedCallInput::new(
                call_id.clone(),
                input_id.clone(),
                "public-live-continuation",
                Arc::<str>::from("continue the live call"),
                Arc::clone(&source),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        call.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 2
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(live_applied.load(Ordering::SeqCst), 2);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 0);
    assert!(live_abort_identities.lock().unwrap().is_empty());

    {
        let bindings = live_bindings.lock().unwrap();
        assert_eq!(bindings.len(), 2);
        let first = &bindings[0];
        let second = &bindings[1];
        assert_eq!(first.call_id, call_id.as_str());
        assert_eq!(second.call_id, call_id.as_str());
        assert_eq!(first.input_id, input_id.as_str());
        assert_eq!(second.input_id, input_id.as_str());
        assert_eq!(first.turn_index, 0);
        assert_eq!(second.turn_index, 1);
        assert_ne!(
            first.attempt.provider_attempt_id(),
            second.attempt.provider_attempt_id()
        );
        assert_ne!(
            first.attempt.live_scope_id(),
            second.attempt.live_scope_id()
        );
    }

    let mut replay = agent
        .start(
            MountedCallInput::new(
                call_id,
                input_id,
                "public-live-continuation-replay",
                Arc::<str>::from("ignored replay props"),
                source,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(replay.wait().await.unwrap().is_replayed());
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(live_bindings.lock().unwrap().len(), 2);

    drop(agent);
    let reopened_factory = factory.clone();
    drop(factory);
    let reopened = reopened_factory
        .open(public_live_definition(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&system_renders),
        ))
        .await
        .unwrap();
    let mut reopened_call = reopened
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-live-reopened-call").unwrap(),
                DurableCallInputId::new("public-live-reopened-call/input-v1").unwrap(),
                "public-live-reopened",
                Arc::<str>::from("start a new live call"),
                Arc::<str>::from("public-live-reopened-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        reopened_call.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 2
    ));

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 4);
    assert_eq!(live_applied.load(Ordering::SeqCst), 4);
    assert_eq!(live_aborted.load(Ordering::SeqCst), 0);
    let bindings = live_bindings.lock().unwrap();
    assert_eq!(bindings.len(), 4);
    assert_eq!(bindings[2].turn_index, 0);
    assert_eq!(bindings[3].turn_index, 1);
    assert_ne!(
        bindings[1].attempt.provider_attempt_id(),
        bindings[2].attempt.provider_attempt_id()
    );
    assert_ne!(
        bindings[1].attempt.live_scope_id(),
        bindings[2].attempt.live_scope_id()
    );
}

#[test]
fn public_provider_dispatcher_registry_rejects_duplicate_bindings() {
    let contract = public_registry_contract("v1", "lookup_world_state");
    let mut registry = ProviderDispatcherRegistry::<PublicChannels, TurnProps>::new();
    registry
        .register(contract.clone(), "public-host/v1", || PublicDispatcher)
        .unwrap();

    assert!(matches!(
        registry.register(contract, "public-host/v1", || PublicDispatcher),
        Err(ProviderDispatcherRegistryError::DuplicateBinding { declaration_id })
            if declaration_id == "public.world-registry"
    ));
}

#[tokio::test]
async fn missing_host_binding_fails_before_system_render_or_attach() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = public_registry_factory(
        "public/provider-registry-missing-binding",
        Arc::clone(&provider),
    );
    let system_renders = Arc::new(AtomicUsize::new(0));
    let definition = public_registry_definition(
        "public/provider-registry-missing-binding/v1",
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::clone(&system_renders),
    );

    let error = factory
        .open_with_bindings(definition, MountedHostBindings::new())
        .await
        .expect_err("a pure provider contract requires a host registry binding");

    assert!(matches!(
        error,
        MountedOpenError::Configuration { message }
            if message.contains("provider dispatcher registry")
                && message.contains("no host dispatcher binding")
    ));
    assert_eq!(system_renders.load(Ordering::SeqCst), 0);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 0);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_contract_version_mismatch_fails_before_system_render_or_attach() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = public_registry_factory(
        "public/provider-registry-version-mismatch",
        Arc::clone(&provider),
    );
    let system_renders = Arc::new(AtomicUsize::new(0));
    let definition = public_registry_definition(
        "public/provider-registry-version-mismatch/v1",
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::clone(&system_renders),
    );
    let mut registry = ProviderDispatcherRegistry::new();
    registry
        .register(
            public_registry_contract("v2", "lookup_world_state"),
            "public-host/v1",
            || PublicDispatcher,
        )
        .unwrap();

    let error = factory
        .open_with_provider_dispatchers(definition, registry)
        .await
        .expect_err("the host cannot bind a different author contract version");

    assert!(matches!(
        error,
        MountedOpenError::Configuration { message }
            if message.contains("author contract version mismatch")
    ));
    assert_eq!(system_renders.load(Ordering::SeqCst), 0);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_tool_schema_mismatch_fails_before_system_render_or_attach() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory = public_registry_factory(
        "public/provider-registry-schema-mismatch",
        Arc::clone(&provider),
    );
    let system_renders = Arc::new(AtomicUsize::new(0));
    let definition = public_registry_definition(
        "public/provider-registry-schema-mismatch/v1",
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::clone(&system_renders),
    );
    let mut registry = ProviderDispatcherRegistry::new();
    registry
        .register(
            public_registry_contract("v1", "lookup_different_state"),
            "public-host/v1",
            || PublicDispatcher,
        )
        .unwrap();

    let error = factory
        .open_with_provider_dispatchers(definition, registry)
        .await
        .expect_err("the host schema must exactly match author order and shape");

    assert!(matches!(
        error,
        MountedOpenError::Configuration { message }
            if message.contains("tool schema/order")
    ));
    assert_eq!(system_renders.load(Ordering::SeqCst), 0);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_registry_creates_one_fresh_dispatcher_per_final_ready_attempt() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory =
        public_registry_factory("public/provider-registry-attempts", Arc::clone(&provider));
    let system_renders = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let initializations = Arc::new(AtomicUsize::new(0));
    let observed_tasks = Arc::new(StdMutex::new(Vec::new()));
    let definition = public_registry_definition(
        "public/provider-registry-attempts/v1",
        Arc::clone(&captures),
        Arc::clone(&user_renders),
        Arc::clone(&system_renders),
    );
    let agent = factory
        .open_with_bindings(
            definition,
            MountedHostBindings::with_provider_dispatchers(contextual_public_dispatcher_registry(
                "public-host/v1",
                Arc::clone(&initializations),
                Arc::clone(&observed_tasks),
            )),
        )
        .await
        .unwrap();

    assert_eq!(initializations.load(Ordering::SeqCst), 0);
    let mut first = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-provider-registry-call-1").unwrap(),
                DurableCallInputId::new("public-provider-registry-call-1/input-v1").unwrap(),
                "public-provider-registry-first",
                Arc::<str>::from("inspect the first turn"),
                Arc::<str>::from("registry-source-1"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        first.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 1
    ));
    assert_eq!(initializations.load(Ordering::SeqCst), 1);

    drop(agent);
    let reopened = factory
        .open_with_bindings(
            public_registry_definition(
                "public/provider-registry-attempts/v1",
                Arc::clone(&captures),
                Arc::clone(&user_renders),
                Arc::clone(&system_renders),
            ),
            MountedHostBindings::with_provider_dispatchers(contextual_public_dispatcher_registry(
                "public-host/v1",
                Arc::clone(&initializations),
                Arc::clone(&observed_tasks),
            )),
        )
        .await
        .unwrap();
    assert_eq!(initializations.load(Ordering::SeqCst), 1);

    let mut second = reopened
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-provider-registry-call-2").unwrap(),
                DurableCallInputId::new("public-provider-registry-call-2/input-v1").unwrap(),
                "public-provider-registry-second",
                Arc::<str>::from("inspect the second turn"),
                Arc::<str>::from("registry-source-2"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        second.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 1
    ));

    assert_eq!(initializations.load(Ordering::SeqCst), 2);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observed_tasks.lock().unwrap(),
        [
            "inspect the first turn:registry-source-1:history-0",
            "inspect the second turn:registry-source-2:history-0",
        ]
    );
}

#[tokio::test]
async fn provider_host_implementation_drift_rejects_reopen_without_system_render() {
    let provider = Arc::new(PublicLifecycleProvider::default());
    let factory =
        public_registry_factory("public/provider-registry-host-drift", Arc::clone(&provider));
    let system_renders = Arc::new(AtomicUsize::new(0));
    let initializations = Arc::new(AtomicUsize::new(0));
    let first = factory
        .open_with_provider_dispatchers(
            public_registry_definition(
                "public/provider-registry-host-drift/v1",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::clone(&system_renders),
            ),
            public_dispatcher_registry("public-host/v1", Arc::clone(&initializations)),
        )
        .await
        .unwrap();
    drop(first);

    let error = factory
        .open_with_provider_dispatchers(
            public_registry_definition(
                "public/provider-registry-host-drift/v1",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::clone(&system_renders),
            ),
            public_dispatcher_registry("public-host/v2", initializations),
        )
        .await
        .expect_err("host implementation drift must require a new durable epoch");

    assert!(matches!(error, MountedOpenError::ContractMismatch { .. }));
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.durable_attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pure_provider_contract_dispatches_native_tool_over_the_public_provider_wire() {
    let provider = Arc::new(PublicLiveProvider::completing_with_native_tool());
    let factory = PublicRegistryLiveFactory::new(
        DurableSessionId::new("public/provider-registry-native-tool").unwrap(),
        AgentSession::new(PromptContext::<String, usize>::without_system()),
        Arc::clone(&provider),
        "public-provider-registry-native-tool-model",
        128,
        PublicLifecycleReducer,
        no_live_effects as fn() -> NoLiveEffects,
    )
    .unwrap();
    let captures = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let system_renders = Arc::new(AtomicUsize::new(0));
    let initializations = Arc::new(AtomicUsize::new(0));
    let invocations = Arc::new(StdMutex::new(Vec::new()));
    let agent = factory
        .open_with_bindings(
            public_registry_definition(
                "public/provider-registry-native-tool/v1",
                Arc::clone(&captures),
                Arc::clone(&user_renders),
                Arc::clone(&system_renders),
            ),
            MountedHostBindings::with_provider_dispatchers(recording_public_dispatcher_registry(
                Arc::clone(&initializations),
                Arc::clone(&invocations),
            )),
        )
        .await
        .unwrap();

    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("public-provider-registry-native-tool-call").unwrap(),
                DurableCallInputId::new("public-provider-registry-native-tool-call/input-v1")
                    .unwrap(),
                "public-provider-registry-native-tool",
                Arc::<str>::from("consult city ledger"),
                Arc::<str>::from("public-native-tool-source"),
            )
            .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        call.wait().await.unwrap(),
        MountedCallOutcome::Executed { records, .. } if records.len() == 1
    ));
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    assert_eq!(
        *invocations.lock().unwrap(),
        [PublicDispatcherInvocation {
            task: "consult city ledger:public-native-tool-source:history-0".to_owned(),
            invocation_id: Some("public-native-tool-1".to_owned()),
            result_correlation_id: Some("public-native-tool-result-1".to_owned()),
            name: "lookup_world_state".to_owned(),
            arguments: json!({ "place": "forum" }),
        }]
    );
    assert_eq!(
        *provider.tool_acknowledgements.lock().unwrap(),
        [PublicToolAcknowledgement {
            invocation_id: Some("public-native-tool-1".to_owned()),
            result_correlation_id: Some("public-native-tool-result-1".to_owned()),
            name: "lookup_world_state".to_owned(),
            response: ProviderToolResponse::success(json!({
                "place": "forum",
                "task": "consult city ledger:public-native-tool-source:history-0",
            })),
            replayed: false,
        }]
    );
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
}
