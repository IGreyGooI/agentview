//! External-consumer proof for the advanced durable mounted-host facade.
//!
//! The test owns every application-facing integration port: opaque CAS/outbox
//! storage, outbox fingerprinting, a stateful provider, a reducer, and Commit
//! staging. AgentView must retain its private owner/session mutation machinery
//! while still supporting Create, atomic publication, replay, and reopen.

use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::{
        advanced::{
            host::MountedHostRuntime,
            persistence::{
                CommitContract, CommitStager, CommitStagingContext, DurableMountedAgentFactory,
                DurableMountedHostConfig, DurableMountedStateBackend, DurableOutboxFingerprint,
                MountedStateBlob, MountedStateGeneration, MountedStateSnapshot, MountedStateWrite,
                MountedStateWriteOutcome, PublicationCandidateFingerprint, StagedCommit,
                StagedOutbox,
            },
            provider::{
                AttachedProviderEpoch, DurableMountedProviderExecutor, FallibleProviderWirePort,
                MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
                ProviderAdapterContract, ProviderCancellationToken, ProviderDispatchContext,
                ProviderDispatchUpdate, ProviderDispatcher, ProviderEpochAttachRequest,
                ProviderEpochReceipt, ProviderEpochRehydrateRequest, ProviderToolCall,
                ProviderToolResponse, ProviderTurnCursor, ProviderWireEvent, ProviderWireFault,
            },
        },
        durable_provider_contract, CapturedMountedHarnessDefinition, DurableCallId,
        DurableCallInputId, DurableEpochDefinition, DurableSessionId, MountedCallInput,
        MountedCallLifecycle, MountedCallOutcome, MountedCallRecoveryReason,
        MountedHarnessDefinition, MountedHostBindings, MountedOpenError, MountedStartError,
        MountedTurnCapture, MountedWaitError, NoLiveEffects, ProviderCapabilityContract,
        ProviderDispatcherRegistry, SessionReduceContext, SessionReducer, TurnCaptureContext,
    },
    llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit, TextTurnEvent},
    prelude::*,
};
use serde_json::json;
use sha2::{Digest, Sha256};

struct DurableChannels;

impl TurnChannels for DurableChannels {
    type Output = Never;
    type Live = Never;
    type Commit = DurableCommit;
    type Diagnostic = String;
}

#[derive(Debug)]
enum DurableCommit {
    PersistAck,
}

#[derive(Clone)]
struct TurnProps {
    task: String,
}

#[derive(Default)]
struct Capture;

#[async_trait::async_trait]
impl MountedTurnCapture for Capture {
    type Transcript = String;
    type ContextState = ();
    type CallProps = str;
    type TurnProps = TurnProps;
    type Source = str;
    type Error = Infallible;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<'_, String, (), str, str>,
    ) -> Result<Self::TurnProps, Self::Error> {
        Ok(TurnProps {
            task: format!("{}:{}", context.call_props(), context.source()),
        })
    }
}

#[derive(Default)]
struct Reducer;

impl SessionReducer<String, (), DurableChannels> for Reducer {
    type Error = Infallible;

    fn reduce(
        &self,
        session: &mut AgentSession<String, ()>,
        context: SessionReduceContext<'_, String, DurableChannels>,
        executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        session.push_history(format!("user:{}", context.request().user()));
        session.extend_history(executor_commit.append);
        Ok(TurnFlow::Wait)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the reducer could not establish a durable turn result")]
struct DefaultFailureReducerError;

/// Deliberately leaves `failure_disposition` at the trait default. This is the
/// external-consumer regression seam for the conservative recovery contract.
struct DefaultFailureReducer;

impl SessionReducer<String, (), DurableChannels> for DefaultFailureReducer {
    type Error = DefaultFailureReducerError;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, ()>,
        _context: SessionReduceContext<'_, String, DurableChannels>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Err(DefaultFailureReducerError)
    }
}

struct DurableCommitStager;

impl CommitStager<DurableChannels> for DurableCommitStager {
    type Payload = String;
    type Error = Infallible;

    fn stage(
        &self,
        _context: CommitStagingContext<'_>,
        commit: &DurableCommit,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        match commit {
            DurableCommit::PersistAck => Ok(StagedCommit::new(
                CommitContract::new("external.durable-ack", 1)
                    .expect("the static Commit contract is valid"),
                "ack".to_owned(),
            )),
        }
    }
}

struct ExternalOutboxFingerprint;

fn fingerprint_field(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

impl DurableOutboxFingerprint<String> for ExternalOutboxFingerprint {
    type Error = Infallible;

    fn fingerprint(
        &self,
        outbox: &StagedOutbox<String>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        // AgentView combines this host-owned digest with its private mutation,
        // provider output, and request identity before persisting it.
        let mut digest = Sha256::new();
        digest.update(b"external-durable-outbox/v1\0");
        digest.update((outbox.len() as u64).to_be_bytes());
        for item in outbox.items() {
            fingerprint_field(&mut digest, &item.id().to_string());
            fingerprint_field(&mut digest, item.contract().key());
            digest.update(item.contract().schema_version().to_be_bytes());
            fingerprint_field(&mut digest, item.payload());
        }
        Ok(
            PublicationCandidateFingerprint::new(format!("sha256:{:x}", digest.finalize()))
                .expect("a SHA-256 fingerprint is a valid durable key"),
        )
    }
}

#[derive(Default)]
struct ExternalBackendState {
    snapshot: Option<MountedStateSnapshot>,
    outbox: Vec<(String, String)>,
    writes: u64,
}

/// A deliberately opaque application store. It persists AgentView's state
/// blob byte-for-byte and records typed outbox rows while holding one mutex,
/// standing in for one database transaction.
#[derive(Default)]
struct ExternalBackend {
    state: Mutex<ExternalBackendState>,
    clock: AtomicUsize,
    outbox_conflicts_remaining: AtomicUsize,
    outbox_conflicts_observed: AtomicUsize,
}

impl ExternalBackend {
    fn now(&self) -> u64 {
        u64::try_from(self.clock.fetch_add(1, Ordering::SeqCst) + 1)
            .expect("test clock fits in u64")
    }

    fn outbox(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().outbox.clone()
    }

    fn state_blob(&self) -> Vec<u8> {
        self.state
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .and_then(MountedStateSnapshot::state)
            .expect("a successful mounted operation persists opaque state")
            .as_bytes()
            .to_vec()
    }

    fn inject_outbox_conflict_once(&self) {
        self.outbox_conflicts_remaining.store(1, Ordering::SeqCst);
    }

    fn outbox_conflicts_observed(&self) -> usize {
        self.outbox_conflicts_observed.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl DurableMountedStateBackend<String> for ExternalBackend {
    type Error = Infallible;

    async fn load(
        &self,
        _session_id: &DurableSessionId,
    ) -> Result<MountedStateSnapshot, Self::Error> {
        let state = self.state.lock().unwrap();
        Ok(state
            .snapshot
            .clone()
            .unwrap_or_else(|| MountedStateSnapshot::missing(self.now())))
    }

    async fn compare_exchange(
        &self,
        request: MountedStateWrite<'_, String>,
    ) -> Result<MountedStateWriteOutcome, Self::Error> {
        assert_eq!(request.session_id().as_str(), "external-durable-session");
        let mut state = self.state.lock().unwrap();
        let now = self.now();
        let current = state
            .snapshot
            .clone()
            .unwrap_or_else(|| MountedStateSnapshot::missing(now));

        if request.expected_generation() != current.generation() {
            return Ok(MountedStateWriteOutcome::conflict(current));
        }
        if request
            .must_commit_before_unix_ms()
            .is_some_and(|deadline| deadline < now)
        {
            return Ok(MountedStateWriteOutcome::deadline_elapsed(current));
        }
        if request.outbox().is_some_and(|outbox| !outbox.is_empty())
            && self
                .outbox_conflicts_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        {
            self.outbox_conflicts_observed
                .fetch_add(1, Ordering::SeqCst);
            return Ok(MountedStateWriteOutcome::conflict(current));
        }

        // State replacement and all typed outbox inserts share this critical
        // section; a real backend must perform the equivalent database txn.
        if let Some(outbox) = request.outbox() {
            state.outbox.extend(
                outbox
                    .items()
                    .iter()
                    .map(|item| (item.contract().key().to_owned(), item.payload().clone())),
            );
        }
        state.writes += 1;
        let generation = MountedStateGeneration::new(format!("external-{}", state.writes))
            .expect("generated test version is valid");
        state.snapshot = Some(MountedStateSnapshot::present(
            generation.clone(),
            MountedStateBlob::new(request.state().as_bytes().to_vec()),
            now,
        ));
        Ok(MountedStateWriteOutcome::committed(generation, now))
    }
}

#[derive(Clone)]
struct ProviderEpoch(ProviderEpochReceipt);

#[derive(Default)]
struct Provider {
    attaches: AtomicUsize,
    rehydrates: AtomicUsize,
    executions: AtomicUsize,
    attached_systems: Mutex<Vec<String>>,
    cursors_seen: Mutex<Vec<bool>>,
    execution_threads: Mutex<Vec<std::thread::ThreadId>>,
    users: Mutex<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
enum ProviderError {
    #[error(transparent)]
    Wire(#[from] ProviderWireFault),
}

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for Provider {
    type Error = ProviderError;
    type Epoch = ProviderEpoch;

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
        wire: &mut dyn FallibleProviderWirePort,
        _cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<String>, Self::Error> {
        self.execution_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.cursors_seen
            .lock()
            .unwrap()
            .push(request.provider_cursor().is_some());
        self.users.lock().unwrap().push(request.user().to_owned());
        wire.submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(
            "<durable_ack />".to_owned(),
        )))
        .await?;
        let turn = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(MountedProviderExit::completed_with_cursor(
            ExecutorCommit::empty(),
            ProviderTurnCursor::new(
                epoch.0.adapter(),
                epoch.0.schema_version(),
                epoch.0.durable_epoch_id().clone(),
                epoch.0.artifact_fingerprint().clone(),
                json!({ "turn": turn }),
            )
            .expect("the test cursor has matching durable lineage"),
        ))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for Provider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("external-durable-provider", 1)
            .expect("the static provider contract is valid")
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        self.attaches.fetch_add(1, Ordering::SeqCst);
        assert!(!request.system().is_empty());
        self.attached_systems
            .lock()
            .unwrap()
            .push(request.system().to_owned());
        let receipt = ProviderEpochReceipt::new(
            "external-durable-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "remote_session": "external-session-7" }),
        )
        .expect("the test receipt is valid");
        Ok(AttachedProviderEpoch::new(
            ProviderEpoch(receipt.clone()),
            receipt,
        ))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        self.rehydrates.fetch_add(1, Ordering::SeqCst);
        assert!(request.cursor().is_some());
        assert_eq!(
            request.receipt().artifact_fingerprint(),
            request.fingerprint()
        );
        Ok(ProviderEpoch(request.receipt().clone()))
    }
}

#[view(component)]
fn durable_policy(system_renders: Arc<AtomicUsize>) -> PomView {
    system_renders.fetch_add(1, Ordering::SeqCst);
    pom_view(Document::from_xml(XmlNode::new(
        XmlName::try_from("external_durable_policy").expect("static tag is valid"),
    )))
}

#[view(component)]
fn durable_ack() -> DurableComponent<DurableChannels, TurnProps> {
    StreamingXml::<TurnEmission<DurableChannels>, String>::new(XmlNode::new(
        XmlName::try_from("durable_ack").expect("static tag is valid"),
    ))
    .state_with(|| ())
    .on_open(|_, _| StreamUpdate::from_emission(TurnEmission::Commit(DurableCommit::PersistAck)))
    .into_durable_component(
        RuntimeContract::new("external.durable-ack", "v1")
            .expect("the static runtime contract is valid"),
    )
}

#[derive(Debug)]
struct DurableRegistryDispatcher;

#[async_trait::async_trait]
impl ProviderDispatcher<DurableChannels> for DurableRegistryDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<DurableChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("ok"),
            StreamUpdate::new(),
        ))
    }
}

fn durable_registry_contract() -> ProviderCapabilityContract {
    ProviderCapabilityContract::new(
        "external.durable-registry",
        "v1",
        [ProviderToolSpec::new(
            "lookup_durable_state",
            "Read immutable durable state",
            json!({ "type": "object", "properties": {} }),
        )
        .expect("the static provider tool spec is valid")],
    )
    .expect("the static provider capability contract is valid")
}

#[view(component)]
fn durable_registry_capability() -> DurableComponent<DurableChannels, TurnProps> {
    durable_provider_contract(
        durable_registry_contract(),
        Document::from_xml(XmlNode::new(
            XmlName::try_from("external_durable_registry").expect("static tag is valid"),
        )),
    )
}

fn user(context: UserTurnContext<'_, TurnProps>) -> UserView {
    let mut task = XmlNode::new(XmlName::try_from("task").expect("static tag is valid"));
    let mut body = XmlNode::new(XmlName::try_from("body").expect("static tag is valid"));
    body.push(MixedContent::text(TextNode::new(&context.props().task)));
    task.push(MixedContent::xml_slot(DiffSlot::present(
        DiffStrategy::Recursive,
        body,
    )));
    user_view(Document::build(|blocks| {
        blocks.xml_slot(DiffSlot::present(DiffStrategy::Recursive, task));
    }))
}

fn definition(
    system_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<DurableChannels, Capture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("external/durable-factory/v1").expect("static id is valid"),
        durable_system((durable_policy(system_renders), durable_ack())),
    );
    MountedHarnessDefinition::new(epoch, user).with_capture(Capture)
}

fn registry_definition(
    system_renders: Arc<AtomicUsize>,
) -> CapturedMountedHarnessDefinition<DurableChannels, Capture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("external/durable-registry-factory/v1").expect("static id is valid"),
        durable_system((
            durable_policy(system_renders),
            durable_ack(),
            durable_registry_capability(),
        )),
    );
    MountedHarnessDefinition::new(epoch, user).with_capture(Capture)
}

fn durable_dispatcher_registry(
    host_implementation_version: &str,
    initializations: Arc<AtomicUsize>,
) -> ProviderDispatcherRegistry<DurableChannels, TurnProps> {
    let mut registry = ProviderDispatcherRegistry::new();
    registry
        .register(
            durable_registry_contract(),
            host_implementation_version.to_owned(),
            move || {
                initializations.fetch_add(1, Ordering::SeqCst);
                DurableRegistryDispatcher
            },
        )
        .expect("the host registry matches the author contract");
    registry
}

fn no_live_effects() -> NoLiveEffects {
    NoLiveEffects
}

type ExternalDurableFactory<ReducerImpl> = DurableMountedAgentFactory<
    DurableChannels,
    String,
    (),
    Provider,
    ReducerImpl,
    fn() -> NoLiveEffects,
    ExternalBackend,
    DurableCommitStager,
    ExternalOutboxFingerprint,
>;

fn factory_with_reducer<ReducerImpl>(
    backend: Arc<ExternalBackend>,
    provider: Arc<Provider>,
    reducer: ReducerImpl,
) -> ExternalDurableFactory<ReducerImpl> {
    DurableMountedAgentFactory::new(
        DurableSessionId::new("external-durable-session").expect("static id is valid"),
        backend,
        AgentSession::new(PromptContext::<String, ()>::without_system()),
        provider,
        "external-durable-model",
        128,
        DurableMountedHostConfig::new("external-host/v1", "external-runtime/v1"),
        reducer,
        no_live_effects as fn() -> NoLiveEffects,
        DurableCommitStager,
        ExternalOutboxFingerprint,
    )
    .expect("the mounted owner runtime starts")
}

fn factory(
    backend: Arc<ExternalBackend>,
    provider: Arc<Provider>,
) -> ExternalDurableFactory<Reducer> {
    factory_with_reducer(backend, provider, Reducer)
}

fn factory_with_runtime(
    runtime: MountedHostRuntime,
    backend: Arc<ExternalBackend>,
    provider: Arc<Provider>,
) -> ExternalDurableFactory<Reducer> {
    DurableMountedAgentFactory::new_with_runtime(
        runtime,
        DurableSessionId::new("external-durable-session").expect("static id is valid"),
        backend,
        AgentSession::new(PromptContext::<String, ()>::without_system()),
        provider,
        "external-durable-model",
        128,
        DurableMountedHostConfig::new("external-host/v1", "external-runtime/v1"),
        Reducer,
        no_live_effects as fn() -> NoLiveEffects,
        DurableCommitStager,
        ExternalOutboxFingerprint,
    )
}

fn input(call_id: &str) -> MountedCallInput<str, str> {
    input_with_task(call_id, "inspect the external store")
}

fn input_with_task(call_id: &str, task: &str) -> MountedCallInput<str, str> {
    MountedCallInput::new(
        DurableCallId::new(call_id).expect("static call id is valid"),
        DurableCallInputId::new(format!("{call_id}/input-v1")).expect("static input id is valid"),
        "external-durable-turn",
        Arc::<str>::from(task),
        Arc::<str>::from("external-source"),
    )
    .expect("static call input is valid")
}

#[tokio::test]
async fn external_durable_factory_persists_outbox_reopens_without_system_and_replays_calls() {
    let backend = Arc::new(ExternalBackend::default());
    let provider = Arc::new(Provider::default());
    let system_renders = Arc::new(AtomicUsize::new(0));

    {
        let first = factory(Arc::clone(&backend), Arc::clone(&provider))
            .open(definition(Arc::clone(&system_renders)))
            .await
            .expect("first open creates the durable epoch");
        backend.inject_outbox_conflict_once();
        let mut first_call = first
            .start(input("external-call-1"))
            .await
            .expect("first call is admitted");
        assert!(matches!(
            first_call.wait().await.expect("first call completes"),
            MountedCallOutcome::Executed { publications, .. }
                if publications.len() == 1 && publications[0].queued_outbox_items() == 1
        ));

        let first_call_id = DurableCallId::new("external-call-1").expect("static id is valid");
        let first_snapshot = first
            .lookup(&first_call_id)
            .await
            .expect("the settled call can be looked up")
            .expect("the admitted call remains queryable");
        assert_eq!(first_snapshot.call_id(), &first_call_id);
        assert_eq!(first_snapshot.next_turn_index(), 1);
        assert!(matches!(
            first_snapshot.lifecycle(),
            MountedCallLifecycle::Settled { result } if result.call_id() == &first_call_id
        ));
    }

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(backend.outbox_conflicts_observed(), 1);
    assert_eq!(
        backend.outbox(),
        vec![("external.durable-ack".to_owned(), "ack".to_owned())]
    );
    let persisted_state: serde_json::Value =
        serde_json::from_slice(&backend.state_blob()).expect("the test state envelope is JSON");
    assert!(
        persisted_state
            .pointer("/user_document_cursor/slots/task")
            .is_some(),
        "the durable mounted owner must persist its acknowledged User baseline"
    );

    let reopened = factory(Arc::clone(&backend), Arc::clone(&provider))
        .open(definition(Arc::clone(&system_renders)))
        .await
        .expect("a new factory rehydrates durable state");
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);

    let first_call_id = DurableCallId::new("external-call-1").expect("static id is valid");
    let reopened_snapshot = reopened
        .lookup(&first_call_id)
        .await
        .expect("the reopened owner can look up the durable call")
        .expect("the settled call remains queryable after reopen");
    assert!(matches!(
        reopened_snapshot.lifecycle(),
        MountedCallLifecycle::Settled { result } if result.call_id() == &first_call_id
    ));

    let mut replay = reopened
        .start(input("external-call-1"))
        .await
        .expect("settled call can be replayed");
    assert!(matches!(
        replay.wait().await.expect("replay completes"),
        MountedCallOutcome::Replayed { .. }
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(backend.outbox().len(), 1);

    let mut second_call = reopened
        .start(input_with_task(
            "external-call-2",
            "inspect the external store after reopen",
        ))
        .await
        .expect("new call is admitted after reopen");
    assert!(matches!(
        second_call.wait().await.expect("second call completes"),
        MountedCallOutcome::Executed { .. }
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(*provider.cursors_seen.lock().unwrap(), vec![false, true]);
    assert_eq!(backend.outbox().len(), 2);
    let users = provider.users.lock().unwrap();
    assert_eq!(users.len(), 2);
    assert!(
        users[1].contains("rendering_mode=\"delta\""),
        "a replacement owner must continue the acknowledged User POM delta lineage"
    );
}

#[tokio::test]
async fn default_reducer_failure_requires_recovery_without_outbox_or_successor() {
    let backend = Arc::new(ExternalBackend::default());
    let provider = Arc::new(Provider::default());
    let system_renders = Arc::new(AtomicUsize::new(0));
    let agent = factory_with_reducer(
        Arc::clone(&backend),
        Arc::clone(&provider),
        DefaultFailureReducer,
    )
    .open(definition(Arc::clone(&system_renders)))
    .await
    .expect("the durable epoch opens before the reducer is exercised");

    let failed_call_id = DurableCallId::new("default-reducer-failure").unwrap();
    let mut failed = agent
        .start(input("default-reducer-failure"))
        .await
        .expect("the first call is durably admitted");
    assert!(matches!(
        failed.wait().await,
        Err(MountedWaitError::RecoveryRequired { .. })
    ));

    let snapshot = agent
        .lookup(&failed_call_id)
        .await
        .expect("the recovery-fenced call remains inspectable")
        .expect("the admitted call remains in the durable ledger");
    assert!(matches!(
        snapshot.lifecycle(),
        MountedCallLifecycle::RecoveryRequired {
            reason: MountedCallRecoveryReason::SessionReductionIndeterminate,
        }
    ));
    assert!(
        backend.outbox().is_empty(),
        "a failed reducer must not publish its staged Commit outbox"
    );

    let successor = agent
        .start(input("default-reducer-failure-successor"))
        .await
        .expect_err("an indeterminate reducer failure must fence a successor call");
    assert!(matches!(
        successor,
        MountedStartError::RecoveryRequired { call_id } if call_id == failed_call_id
    ));
    assert_eq!(provider.executions.load(Ordering::SeqCst), 1);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
}

#[test]
fn external_durable_factories_share_the_caller_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the caller runtime starts");
    let host_runtime = MountedHostRuntime::new(runtime.handle().clone());
    let caller_thread = std::thread::current().id();
    let backend = Arc::new(ExternalBackend::default());
    let provider = Arc::new(Provider::default());
    let system_renders = Arc::new(AtomicUsize::new(0));

    // These are separate durable factories, not clones. Both must schedule on
    // the caller's current-thread runtime rather than each creating workers.
    let first_factory = factory_with_runtime(
        host_runtime.clone(),
        Arc::clone(&backend),
        Arc::clone(&provider),
    );
    let reopened_factory =
        factory_with_runtime(host_runtime, Arc::clone(&backend), Arc::clone(&provider));

    runtime.block_on(async {
        let first = first_factory
            .open(definition(Arc::clone(&system_renders)))
            .await
            .expect("the first factory creates the durable epoch");
        let mut first_call = first
            .start(input("external-runtime-call-1"))
            .await
            .expect("the first call is admitted");
        assert!(matches!(
            first_call.wait().await.expect("the first call completes"),
            MountedCallOutcome::Executed { .. }
        ));
        drop(first);
        drop(first_factory);

        let reopened = reopened_factory
            .open(definition(Arc::clone(&system_renders)))
            .await
            .expect("the second factory rehydrates the same durable epoch");
        let mut reopened_call = reopened
            .start(input("external-runtime-call-2"))
            .await
            .expect("the reopened call is admitted");
        assert!(matches!(
            reopened_call
                .wait()
                .await
                .expect("the reopened call completes"),
            MountedCallOutcome::Executed { .. }
        ));
    });

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    assert_eq!(
        *provider.execution_threads.lock().unwrap(),
        vec![caller_thread, caller_thread],
        "both factories must run provider work on the caller-owned runtime"
    );
    assert_eq!(
        runtime.block_on(async {
            tokio::task::yield_now().await;
            "caller runtime still running"
        }),
        "caller runtime still running",
        "dropping mounted factories and calls must not stop the caller runtime"
    );
}

#[tokio::test]
async fn durable_factory_persists_provider_registry_version_and_rebinds_per_attempt() {
    let backend = Arc::new(ExternalBackend::default());
    let provider = Arc::new(Provider::default());
    let system_renders = Arc::new(AtomicUsize::new(0));
    let initializations = Arc::new(AtomicUsize::new(0));

    let first_factory = factory(Arc::clone(&backend), Arc::clone(&provider));
    let first = first_factory
        .open_with_bindings(
            registry_definition(Arc::clone(&system_renders)),
            MountedHostBindings::with_provider_dispatchers(durable_dispatcher_registry(
                "external-dispatcher/v1",
                Arc::clone(&initializations),
            )),
        )
        .await
        .expect("the matching registry creates the durable epoch");
    assert_eq!(initializations.load(Ordering::SeqCst), 0);

    let mut first_call = first
        .start(input("external-registry-call-1"))
        .await
        .expect("the first registry-backed call is admitted");
    assert!(matches!(
        first_call.wait().await.expect("the first call completes"),
        MountedCallOutcome::Executed { .. }
    ));
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    drop(first);
    drop(first_factory);

    let drifted_factory = factory(Arc::clone(&backend), Arc::clone(&provider));
    let drift = drifted_factory
        .open_with_bindings(
            registry_definition(Arc::clone(&system_renders)),
            MountedHostBindings::with_provider_dispatchers(durable_dispatcher_registry(
                "external-dispatcher/v2",
                Arc::clone(&initializations),
            )),
        )
        .await
        .expect_err("a changed host dispatcher implementation requires a new epoch");
    assert!(matches!(drift, MountedOpenError::ContractMismatch { .. }));
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 0);
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    drop(drifted_factory);

    let reopened_factory = factory(Arc::clone(&backend), Arc::clone(&provider));
    let reopened = reopened_factory
        .open_with_bindings(
            registry_definition(Arc::clone(&system_renders)),
            MountedHostBindings::with_provider_dispatchers(durable_dispatcher_registry(
                "external-dispatcher/v1",
                Arc::clone(&initializations),
            )),
        )
        .await
        .expect("the original host implementation rehydrates the durable epoch");
    assert_eq!(initializations.load(Ordering::SeqCst), 1);

    let mut second_call = reopened
        .start(input("external-registry-call-2"))
        .await
        .expect("the reopened registry-backed call is admitted");
    assert!(matches!(
        second_call.wait().await.expect("the second call completes"),
        MountedCallOutcome::Executed { .. }
    ));

    assert_eq!(initializations.load(Ordering::SeqCst), 2);
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
}
