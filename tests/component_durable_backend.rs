use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::component::{
    advanced::{
        persistence::{
            CommitContract, CommitStager, CommitStagingContext, DurableMountedAgentFactory,
            DurableMountedHostConfig, DurableMountedStateBackend, DurableOutboxFingerprint,
            MountedStateBlob, MountedStateGeneration, MountedStateSnapshot, MountedStateWrite,
            MountedStateWriteOutcome, PublicationCandidateFingerprint, StagedCommit,
        },
        provider::{
            AttachedProviderEpoch, DurableMountedProviderExecutor, FallibleProviderWirePort,
            MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
            ProviderAdapterContract, ProviderCancellationToken, ProviderEpochAttachRequest,
            ProviderEpochReceipt, ProviderEpochRehydrateRequest, ProviderWireEvent,
        },
    },
    durable_system,
    host::prelude::MountedCallOutcome,
    pom_view, user_view, DurableCallId, DurableCallInputId, DurableEpochDefinition,
    DurableSessionId, MountedCallInput, MountedHarnessDefinition, MountedTurnCapture, Never,
    NoLiveEffects, SessionReduceContext, SessionReducer, TurnCaptureContext, TurnChannels,
    UserTurnContext, UserView,
};
use agentview::{
    llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit, TextTurnEvent},
    prelude::*,
};
use serde_json::json;

struct ExternalBackend;

#[async_trait::async_trait]
impl DurableMountedStateBackend<String> for ExternalBackend {
    type Error = Infallible;

    async fn load(
        &self,
        _session_id: &DurableSessionId,
    ) -> Result<MountedStateSnapshot, Self::Error> {
        Ok(MountedStateSnapshot::missing(100))
    }

    async fn compare_exchange(
        &self,
        request: MountedStateWrite<'_, String>,
    ) -> Result<MountedStateWriteOutcome, Self::Error> {
        assert_eq!(request.session_id().as_str(), "external-session");
        Ok(MountedStateWriteOutcome::committed(
            MountedStateGeneration::new("external-generation-1").unwrap(),
            101,
        ))
    }
}

#[tokio::test]
async fn external_crate_can_implement_the_opaque_state_backend() {
    let snapshot = ExternalBackend
        .load(&DurableSessionId::new("external-session").unwrap())
        .await
        .unwrap();

    assert!(snapshot.state().is_none());
    assert_eq!(snapshot.observed_at_unix_ms(), 100);
}

struct DurableChannels;

impl TurnChannels for DurableChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[derive(Clone)]
struct DurableCapture;

#[async_trait::async_trait]
impl MountedTurnCapture for DurableCapture {
    type Transcript = String;
    type ContextState = ();
    type CallProps = String;
    type TurnProps = String;
    type Source = ();
    type Error = Infallible;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<'_, String, (), String, ()>,
    ) -> Result<Self::TurnProps, Self::Error> {
        Ok(context.call_props().clone())
    }
}

struct DurableReducer;

impl SessionReducer<String, (), DurableChannels> for DurableReducer {
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

#[derive(Clone, Default)]
struct DurableProvider {
    attaches: Arc<AtomicUsize>,
    rehydrates: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
    users: Arc<Mutex<Vec<String>>>,
}

struct DurableProviderEpoch;

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for DurableProvider {
    type Error = Infallible;
    type Epoch = DurableProviderEpoch;

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
        request: MountedProviderRequest<String>,
        _wire: &mut dyn FallibleProviderWirePort,
        _cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<String>, Self::Error> {
        self.users.lock().unwrap().push(request.user().to_owned());
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(MountedProviderExit::completed(ExecutorCommit::new([
            format!("assistant:{}", request.user()),
        ])))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for DurableProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("durable-backend-test-provider", 1).unwrap()
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        self.attaches.fetch_add(1, Ordering::SeqCst);
        let receipt = ProviderEpochReceipt::new(
            "durable-backend-test-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "remote_session": "durable-backend-test" }),
        )
        .unwrap();
        Ok(AttachedProviderEpoch::new(DurableProviderEpoch, receipt))
    }

    async fn rehydrate_durable_epoch(
        &self,
        _request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        self.rehydrates.fetch_add(1, Ordering::SeqCst);
        Ok(DurableProviderEpoch)
    }
}

#[derive(Default)]
struct DurableBackendState {
    generation: u64,
    state: Option<MountedStateBlob>,
    writes: usize,
}

#[derive(Default)]
struct ReopenBackend {
    state: Mutex<DurableBackendState>,
}

impl ReopenBackend {
    fn snapshot(state: &DurableBackendState) -> MountedStateSnapshot {
        match state.state.as_ref() {
            Some(blob) => MountedStateSnapshot::present(
                MountedStateGeneration::new(format!("generation-{}", state.generation)).unwrap(),
                blob.clone(),
                100,
            ),
            None => MountedStateSnapshot::missing(100),
        }
    }

    fn writes(&self) -> usize {
        self.state.lock().unwrap().writes
    }
}

#[async_trait::async_trait]
impl DurableMountedStateBackend<Never> for ReopenBackend {
    type Error = Infallible;

    async fn load(
        &self,
        _session_id: &DurableSessionId,
    ) -> Result<MountedStateSnapshot, Self::Error> {
        Ok(Self::snapshot(&self.state.lock().unwrap()))
    }

    async fn compare_exchange(
        &self,
        request: MountedStateWrite<'_, Never>,
    ) -> Result<MountedStateWriteOutcome, Self::Error> {
        let mut state = self.state.lock().unwrap();
        let current = Self::snapshot(&state);
        if request.expected_generation() != current.generation() {
            return Ok(MountedStateWriteOutcome::conflict(current));
        }
        if request
            .must_commit_before_unix_ms()
            .is_some_and(|deadline| deadline <= 100)
        {
            return Ok(MountedStateWriteOutcome::deadline_elapsed(current));
        }
        state.generation += 1;
        state.state = Some(request.state().clone());
        state.writes += 1;
        Ok(MountedStateWriteOutcome::committed(
            MountedStateGeneration::new(format!("generation-{}", state.generation)).unwrap(),
            100,
        ))
    }
}

struct NeverStager;

impl CommitStager<DurableChannels> for NeverStager {
    type Payload = Never;
    type Error = Infallible;

    fn stage(
        &self,
        _context: CommitStagingContext<'_>,
        commit: &Never,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        commit.absurd()
    }
}

struct EmptyOutboxFingerprint;

impl DurableOutboxFingerprint<Never> for EmptyOutboxFingerprint {
    type Error = Infallible;

    fn fingerprint(
        &self,
        outbox: &agentview::component::advanced::persistence::StagedOutbox<Never>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        assert!(outbox.is_empty());
        Ok(PublicationCandidateFingerprint::new("durable-backend-empty-outbox/v1").unwrap())
    }
}

#[view(component)]
fn durable_policy(system_renders: Arc<AtomicUsize>) -> PomView {
    system_renders.fetch_add(1, Ordering::SeqCst);
    pom_view(Document::from_xml(XmlNode::new(
        XmlName::try_from("durable_backend_policy").unwrap(),
    )))
}

fn durable_definition(
    system_renders: Arc<AtomicUsize>,
) -> agentview::component::CapturedMountedHarnessDefinition<DurableChannels, DurableCapture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("durable-backend-external/v1").unwrap(),
        durable_system(durable_policy(system_renders)),
    );
    MountedHarnessDefinition::new(epoch, durable_user).with_capture(DurableCapture)
}

fn durable_user(context: UserTurnContext<'_, String>) -> UserView {
    let mut node = XmlNode::new(XmlName::try_from("task").unwrap());
    node.push(MixedContent::text(TextNode::new(context.props())));
    user_view(Document::from_xml(node))
}

fn no_live_effects() -> NoLiveEffects {
    NoLiveEffects
}

type DurableBackendFactory = DurableMountedAgentFactory<
    DurableChannels,
    String,
    (),
    DurableProvider,
    DurableReducer,
    fn() -> NoLiveEffects,
    ReopenBackend,
    NeverStager,
    EmptyOutboxFingerprint,
>;

fn durable_factory(
    backend: Arc<ReopenBackend>,
    provider: Arc<DurableProvider>,
) -> DurableBackendFactory {
    DurableMountedAgentFactory::new(
        DurableSessionId::new("durable-backend-external-session").unwrap(),
        backend,
        AgentSession::new(PromptContext::<String, ()>::without_system()),
        provider,
        "durable-backend-external-model",
        128,
        DurableMountedHostConfig::new(
            "durable-backend-external-host/v1",
            "durable-backend-external-runtime/v1",
        ),
        DurableReducer,
        no_live_effects as fn() -> NoLiveEffects,
        NeverStager,
        EmptyOutboxFingerprint,
    )
    .unwrap()
}

async fn run_durable_turn(
    agent: &agentview::component::MountedAgent<DurableChannels, String, ()>,
    call: &str,
    task: &str,
) {
    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new(call).unwrap(),
                DurableCallInputId::new(format!("{call}/input-v1")).unwrap(),
                call,
                Arc::new(task.to_owned()),
                Arc::new(()),
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

#[tokio::test]
async fn external_durable_host_reopens_without_rerendering_or_reattaching_system() {
    let backend = Arc::new(ReopenBackend::default());
    let provider = Arc::new(DurableProvider::default());
    let system_renders = Arc::new(AtomicUsize::new(0));

    let first_factory = durable_factory(Arc::clone(&backend), Arc::clone(&provider));
    let first = first_factory
        .open(durable_definition(Arc::clone(&system_renders)))
        .await
        .unwrap();
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    run_durable_turn(&first, "durable-first", "inspect plaza").await;
    drop(first);
    drop(first_factory);

    let reopened_factory = durable_factory(Arc::clone(&backend), Arc::clone(&provider));
    let reopened = reopened_factory
        .open(durable_definition(Arc::clone(&system_renders)))
        .await
        .unwrap();
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(provider.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.rehydrates.load(Ordering::SeqCst), 1);
    run_durable_turn(&reopened, "durable-second", "inspect forum").await;

    assert_eq!(provider.executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.users.lock().unwrap().as_slice(),
        ["<task>inspect plaza</task>", "<task>inspect forum</task>"],
    );
    assert!(backend.writes() >= 6);
}

struct OutboxChannels;

impl TurnChannels for OutboxChannels {
    type Output = Never;
    type Live = Never;
    type Commit = String;
    type Diagnostic = String;
}

struct OutboxReducer;

impl SessionReducer<String, (), OutboxChannels> for OutboxReducer {
    type Error = Infallible;

    fn reduce(
        &self,
        session: &mut AgentSession<String, ()>,
        context: SessionReduceContext<'_, String, OutboxChannels>,
        executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        session.push_history(format!("user:{}", context.request().user()));
        session.extend_history(executor_commit.append);
        Ok(TurnFlow::Wait)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("provider wire rejected the durable Commit stream: {0}")]
struct OutboxProviderError(String);

#[derive(Default)]
struct OutboxProvider {
    executions: AtomicUsize,
}

struct OutboxProviderEpoch;

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for OutboxProvider {
    type Error = OutboxProviderError;
    type Epoch = OutboxProviderEpoch;

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
        wire: &mut dyn FallibleProviderWirePort,
        _cancellation: ProviderCancellationToken,
    ) -> Result<MountedProviderExit<String>, Self::Error> {
        let output = "<persist_selection />".to_owned();
        wire.submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(
            output.clone(),
        )))
        .await
        .map_err(|error| OutboxProviderError(error.to_string()))?;
        wire.submit(ProviderWireEvent::Text(TextTurnEvent::TextComplete(output)))
            .await
            .map_err(|error| OutboxProviderError(error.to_string()))?;
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(MountedProviderExit::completed(ExecutorCommit::new([
            "assistant:persisted-selection".to_owned(),
        ])))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for OutboxProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("durable-backend-outbox-test-provider", 1).unwrap()
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        let receipt = ProviderEpochReceipt::new(
            "durable-backend-outbox-test-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "remote_session": "durable-backend-outbox-test" }),
        )
        .unwrap();
        Ok(AttachedProviderEpoch::new(OutboxProviderEpoch, receipt))
    }

    async fn rehydrate_durable_epoch(
        &self,
        _request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        Ok(OutboxProviderEpoch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedOutboxItem {
    id: String,
    contract_key: String,
    schema_version: u32,
    payload: String,
}

#[derive(Default)]
struct OutboxBackendState {
    generation: u64,
    state: Option<MountedStateBlob>,
    outbox: Vec<PersistedOutboxItem>,
}

#[derive(Default)]
struct OutboxBackend {
    state: Mutex<OutboxBackendState>,
}

impl OutboxBackend {
    fn snapshot(state: &OutboxBackendState) -> MountedStateSnapshot {
        match state.state.as_ref() {
            Some(blob) => MountedStateSnapshot::present(
                MountedStateGeneration::new(format!("outbox-generation-{}", state.generation))
                    .unwrap(),
                blob.clone(),
                100,
            ),
            None => MountedStateSnapshot::missing(100),
        }
    }

    fn persisted_outbox(&self) -> Vec<PersistedOutboxItem> {
        self.state.lock().unwrap().outbox.clone()
    }
}

#[async_trait::async_trait]
impl DurableMountedStateBackend<String> for OutboxBackend {
    type Error = Infallible;

    async fn load(
        &self,
        _session_id: &DurableSessionId,
    ) -> Result<MountedStateSnapshot, Self::Error> {
        Ok(Self::snapshot(&self.state.lock().unwrap()))
    }

    async fn compare_exchange(
        &self,
        request: MountedStateWrite<'_, String>,
    ) -> Result<MountedStateWriteOutcome, Self::Error> {
        let mut state = self.state.lock().unwrap();
        let current = Self::snapshot(&state);
        if request.expected_generation() != current.generation() {
            return Ok(MountedStateWriteOutcome::conflict(current));
        }
        if request
            .must_commit_before_unix_ms()
            .is_some_and(|deadline| deadline <= 100)
        {
            return Ok(MountedStateWriteOutcome::deadline_elapsed(current));
        }

        let outbox = request
            .outbox()
            .into_iter()
            .flat_map(|outbox| outbox.items())
            .map(|item| PersistedOutboxItem {
                id: item.id().to_string(),
                contract_key: item.contract().key().to_owned(),
                schema_version: item.contract().schema_version(),
                payload: item.payload().clone(),
            })
            .collect::<Vec<_>>();
        state.generation += 1;
        state.state = Some(request.state().clone());
        state.outbox.extend(outbox);
        Ok(MountedStateWriteOutcome::committed(
            MountedStateGeneration::new(format!("outbox-generation-{}", state.generation)).unwrap(),
            100,
        ))
    }
}

struct SelectionCommitStager;

impl CommitStager<OutboxChannels> for SelectionCommitStager {
    type Payload = String;
    type Error = Infallible;

    fn stage(
        &self,
        _context: CommitStagingContext<'_>,
        commit: &String,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        Ok(StagedCommit::new(
            CommitContract::new("durable-backend.selection", 1).unwrap(),
            commit.clone(),
        ))
    }
}

struct SelectionOutboxFingerprint;

impl DurableOutboxFingerprint<String> for SelectionOutboxFingerprint {
    type Error = Infallible;

    fn fingerprint(
        &self,
        outbox: &agentview::component::advanced::persistence::StagedOutbox<String>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        let canonical = outbox
            .items()
            .iter()
            .map(|item| {
                format!(
                    "{}:{}:{}:{}",
                    item.id(),
                    item.contract().key(),
                    item.contract().schema_version(),
                    item.payload()
                )
            })
            .collect::<Vec<_>>()
            .join("|");
        Ok(
            PublicationCandidateFingerprint::new(format!("durable-backend-outbox/v1:{canonical}"))
                .unwrap(),
        )
    }
}

#[view(component)]
fn persisted_selection() -> agentview::component::DurableComponent<OutboxChannels, String> {
    StreamingXml::<TurnEmission<OutboxChannels>, String>::new(XmlNode::new(
        XmlName::try_from("persist_selection").unwrap(),
    ))
    .state_with(|| ())
    .on_complete(|_, _| {
        StreamUpdate::from_emission(TurnEmission::Commit("selection:plaza".to_owned()))
    })
    .into_durable_component(
        RuntimeContract::new("durable-backend.persist-selection", "v1").unwrap(),
    )
}

fn outbox_definition(
) -> agentview::component::CapturedMountedHarnessDefinition<OutboxChannels, DurableCapture> {
    let epoch = DurableEpochDefinition::new(
        EpochContractId::new("durable-backend-outbox/v1").unwrap(),
        durable_system((
            Document::from_xml(XmlNode::new(XmlName::try_from("outbox_policy").unwrap())),
            persisted_selection(),
        )),
    );
    MountedHarnessDefinition::new(epoch, durable_user).with_capture(DurableCapture)
}

type DurableOutboxFactory = DurableMountedAgentFactory<
    OutboxChannels,
    String,
    (),
    OutboxProvider,
    OutboxReducer,
    fn() -> NoLiveEffects,
    OutboxBackend,
    SelectionCommitStager,
    SelectionOutboxFingerprint,
>;

fn outbox_factory(
    backend: Arc<OutboxBackend>,
    provider: Arc<OutboxProvider>,
) -> DurableOutboxFactory {
    DurableMountedAgentFactory::new(
        DurableSessionId::new("durable-backend-outbox-session").unwrap(),
        backend,
        AgentSession::new(PromptContext::<String, ()>::without_system()),
        provider,
        "durable-backend-outbox-model",
        128,
        DurableMountedHostConfig::new(
            "durable-backend-outbox-host/v1",
            "durable-backend-outbox-runtime/v1",
        ),
        OutboxReducer,
        no_live_effects as fn() -> NoLiveEffects,
        SelectionCommitStager,
        SelectionOutboxFingerprint,
    )
    .unwrap()
}

#[tokio::test]
async fn external_durable_host_persists_typed_commit_in_transactional_outbox() {
    let backend = Arc::new(OutboxBackend::default());
    let provider = Arc::new(OutboxProvider::default());
    let agent = outbox_factory(Arc::clone(&backend), Arc::clone(&provider))
        .open(outbox_definition())
        .await
        .unwrap();

    let mut call = agent
        .start(
            MountedCallInput::new(
                DurableCallId::new("outbox-call-1").unwrap(),
                DurableCallInputId::new("outbox-call-1/input-v1").unwrap(),
                "record-selection",
                Arc::new("inspect plaza".to_owned()),
                Arc::new(()),
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
    let outbox = backend.persisted_outbox();
    assert_eq!(outbox.len(), 1);
    assert!(!outbox[0].id.is_empty());
    assert_eq!(outbox[0].contract_key, "durable-backend.selection");
    assert_eq!(outbox[0].schema_version, 1);
    assert_eq!(outbox[0].payload, "selection:plaza");
}
