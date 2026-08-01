//! Provider-driven mounted Chess loop with typed Commit publication.
//!
//! The provider is scripted so this example stays deterministic, but it uses
//! the real mounted provider, streaming reducer, durable publication, and
//! outbox boundaries. Run it with a Stockfish-compatible binary:
//!
//! `cargo run --example chess_engine_mounted_agentloop`

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    convert::Infallible,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::{
        advanced::{
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
                ProviderAdapterContract, ProviderCancellationToken, ProviderEpochAttachRequest,
                ProviderEpochReceipt, ProviderEpochRehydrateRequest, ProviderWireEvent,
                ProviderWireFault,
            },
        },
        prelude::*,
        CapturedMountedHarnessDefinition, DurableCallId, DurableCallInputId, DurableSessionId,
        LiveAbortContext, LiveEffectAbortAck, LiveEffectContext, LiveEffectRuntime, MountedAgent,
        MountedCallInput, MountedCallOutcome, MountedTurnCapture, SessionReduceContext,
        SessionReducer, SessionReducerFailureDisposition, TurnCaptureContext,
    },
    llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit, TextTurnEvent},
    prelude::{AgentSession, PromptContext, TurnBindingCx, TurnFlow},
    semantic_view::AgentViewCollect,
};
use serde_json::json;
use sha2::{Digest, Sha256};

#[path = "chess_engine_agent/support.rs"]
#[allow(dead_code)]
mod chess_support;

use chess_support::{
    chess_user_document, ChessAction, ChessDiagnostic, ChessGameSource, ChessMoveContract,
    ChessSystemPolicyPromptView, ChessView, StockfishEngine,
};

#[derive(Debug, Clone)]
struct ChessCallProps {
    task: String,
    turn_id: String,
}

#[derive(Debug, Clone)]
struct ChessTurnProps {
    context: ChessView,
    legal_moves: Arc<BTreeSet<String>>,
    expected_ply: usize,
    task: String,
    turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChessCommit {
    action: ChessAction,
    expected_ply: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChessAgentDiagnostic {
    Contract(ChessDiagnostic),
    IllegalForCapturedPosition(String),
    MultipleMoves,
    NoMove,
}

struct ChessChannels;

impl TurnChannels for ChessChannels {
    type Output = ChessAction;
    type Live = ChessAction;
    type Commit = ChessCommit;
    type Diagnostic = ChessAgentDiagnostic;
}

#[derive(Default)]
struct ChessCapture;

#[async_trait::async_trait]
impl MountedTurnCapture for ChessCapture {
    type Transcript = String;
    type ContextState = ();
    type CallProps = ChessCallProps;
    type TurnProps = ChessTurnProps;
    type Source = ChessGameSource;
    type Error = Infallible;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<'_, String, (), ChessCallProps, ChessGameSource>,
    ) -> Result<Self::TurnProps, Self::Error> {
        let view = ChessView::collect(&context.source().snapshot());
        let legal_moves = view
            .legal_uci_moves()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        let expected_ply = view.move_history().len();
        Ok(ChessTurnProps {
            context: view,
            legal_moves: Arc::new(legal_moves),
            expected_ply,
            task: context.call_props().task.clone(),
            turn_id: context.call_props().turn_id.clone(),
        })
    }
}

struct ChessReducerState {
    selected: Option<ChessAction>,
    completed: Option<ChessAction>,
    invalid: bool,
    opened: usize,
    legal_moves: Arc<BTreeSet<String>>,
    expected_ply: usize,
}

#[view(component)]
fn choose_move() -> DurableComponent<ChessChannels, ChessTurnProps> {
    StreamingXml::<TurnEmission<ChessChannels>, ChessAgentDiagnostic>::new(
        ChessMoveContract.build_root()?,
    )
    .try_state_with(|turn: &TurnBindingCx<'_, ChessTurnProps, ChessChannels>| {
        Ok::<_, Infallible>(ChessReducerState {
            selected: None,
            completed: None,
            invalid: false,
            opened: 0,
            legal_moves: Arc::clone(&turn.props().legal_moves),
            expected_ply: turn.props().expected_ply,
        })
    })
    .on_open(|state, element| {
        state.opened += 1;
        if state.opened != 1 {
            state.invalid = true;
            return StreamUpdate::from_diagnostic(ChessAgentDiagnostic::MultipleMoves);
        }
        let action = match ChessMoveContract.decode_element(element) {
            Ok(action) => action,
            Err(diagnostic) => {
                state.invalid = true;
                return StreamUpdate::from_diagnostic(ChessAgentDiagnostic::Contract(diagnostic));
            }
        };
        if !state.legal_moves.contains(action.uci()) {
            state.invalid = true;
            return StreamUpdate::from_diagnostic(
                ChessAgentDiagnostic::IllegalForCapturedPosition(action.uci().to_owned()),
            );
        }
        state.selected = Some(action.clone());
        StreamUpdate::from_emission(TurnEmission::Live(action))
    })
    .on_complete(|state, element| {
        if state.invalid || state.opened != 1 {
            return StreamUpdate::new();
        }
        let action = match ChessMoveContract.decode_element(element) {
            Ok(action) => action,
            Err(diagnostic) => {
                state.invalid = true;
                return StreamUpdate::from_diagnostic(ChessAgentDiagnostic::Contract(diagnostic));
            }
        };
        if state.selected.as_ref() != Some(&action) {
            state.invalid = true;
            return StreamUpdate::from_diagnostic(ChessAgentDiagnostic::MultipleMoves);
        }
        state.completed = Some(action);
        StreamUpdate::new()
    })
    .on_finish(|state| match (state.invalid, state.completed.clone()) {
        (false, Some(action)) => StreamUpdate::from_emission(TurnEmission::Output(action.clone()))
            .with_emission(TurnEmission::Commit(ChessCommit {
                action,
                expected_ply: state.expected_ply,
            })),
        _ => StreamUpdate::from_diagnostic(ChessAgentDiagnostic::NoMove),
    })
    .into_durable_component(RuntimeContract::new("example.chess.choose-move", "v2")?)
}

#[view(component)]
fn chess_agent() -> MountedFeature<ChessChannels, ChessTurnProps> {
    MountedFeature::try_new(
        durable_system((
            ChessSystemPolicyPromptView::default().build_root()?,
            choose_move(),
        )),
        |turn| {
            chess_user_document(
                turn.props().context.clone(),
                turn.props().task.clone(),
                turn.props().turn_id.clone(),
            )
        },
    )
}

fn chess_definition() -> CapturedMountedHarnessDefinition<ChessChannels, ChessCapture> {
    chess_agent()
        .into_harness(
            EpochContractId::new("example/chess-mounted-agentloop/v1")
                .expect("the static Chess epoch id is valid"),
        )
        .with_capture(ChessCapture)
}

#[derive(Default)]
struct ChessSessionReducer;

#[derive(Debug, thiserror::Error)]
enum ChessSessionError {
    #[error("strict Chess reply contract failed: {0}")]
    Contract(#[from] ChessDiagnostic),
    #[error("strict Chess reply produced {count} diagnostic(s)")]
    Diagnostics { count: usize },
    #[error("strict Chess reply produced {count} final output(s), expected exactly one")]
    OutputCardinality { count: usize },
    #[error("strict Chess reply output does not match its canonical XML envelope")]
    OutputMismatch,
}

impl SessionReducer<String, (), ChessChannels> for ChessSessionReducer {
    type Error = ChessSessionError;

    fn reduce(
        &self,
        session: &mut AgentSession<String, ()>,
        context: SessionReduceContext<'_, String, ChessChannels>,
        executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        let record = context.record();
        let decoded = ChessMoveContract.decode_reply(record.raw_output())?;
        if !record.diagnostics().is_empty() {
            return Err(ChessSessionError::Diagnostics {
                count: record.diagnostics().len(),
            });
        }
        let [output] = record.outputs() else {
            return Err(ChessSessionError::OutputCardinality {
                count: record.outputs().len(),
            });
        };
        if output != &decoded {
            return Err(ChessSessionError::OutputMismatch);
        }

        session.push_history(format!("user:{}", context.request().user()));
        session.extend_history(executor_commit.append);
        Ok(TurnFlow::Wait)
    }

    fn failure_disposition(&self, _error: &Self::Error) -> SessionReducerFailureDisposition {
        SessionReducerFailureDisposition::Reject
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChessCommitPayload {
    uci: String,
    expected_ply: usize,
}

struct ChessCommitStager;

impl CommitStager<ChessChannels> for ChessCommitStager {
    type Payload = ChessCommitPayload;
    type Error = Infallible;

    fn stage(
        &self,
        _context: CommitStagingContext<'_>,
        commit: &ChessCommit,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        Ok(StagedCommit::new(
            CommitContract::new("example.chess.player-move", 1)
                .expect("the static Chess Commit contract is valid"),
            ChessCommitPayload {
                uci: commit.action.uci().to_owned(),
                expected_ply: commit.expected_ply,
            },
        ))
    }
}

struct ChessOutboxFingerprint;

fn hash_field(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

impl DurableOutboxFingerprint<ChessCommitPayload> for ChessOutboxFingerprint {
    type Error = Infallible;

    fn fingerprint(
        &self,
        outbox: &StagedOutbox<ChessCommitPayload>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        let mut digest = Sha256::new();
        digest.update(b"example.chess.outbox/v1\0");
        for item in outbox.items() {
            hash_field(&mut digest, &item.id().to_string());
            hash_field(&mut digest, item.contract().key());
            digest.update(item.contract().schema_version().to_be_bytes());
            hash_field(&mut digest, &item.payload().uci);
            digest.update((item.payload().expected_ply as u64).to_be_bytes());
        }
        Ok(
            PublicationCandidateFingerprint::new(format!("sha256:{:x}", digest.finalize()))
                .expect("a SHA-256 digest is a valid publication fingerprint"),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredChessCommit {
    payload: ChessCommitPayload,
    delivered: bool,
}

#[derive(Default)]
struct ChessBackendState {
    generation: u64,
    state: Option<MountedStateBlob>,
    outbox: BTreeMap<String, StoredChessCommit>,
}

#[derive(Default)]
struct ChessBackend {
    state: Mutex<ChessBackendState>,
    clock: AtomicU64,
    publications: AtomicUsize,
}

impl ChessBackend {
    fn now(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn snapshot(&self, state: &ChessBackendState, now: u64) -> MountedStateSnapshot {
        match state.state.as_ref() {
            Some(blob) => MountedStateSnapshot::present(
                MountedStateGeneration::new(format!("chess-generation-{}", state.generation))
                    .expect("the generated mounted revision is valid"),
                blob.clone(),
                now,
            ),
            None => MountedStateSnapshot::missing(now),
        }
    }

    /// Deliver every pending typed Commit exactly once by stable outbox id.
    ///
    /// The move-history check closes the local crash window between applying a
    /// move and marking its row delivered. A production host performs the same
    /// idempotency check in its domain/outbox transaction.
    fn deliver_pending(&self, source: &ChessGameSource) -> anyhow::Result<Vec<String>> {
        let mut state = self.state.lock().unwrap();
        let mut delivered = Vec::new();
        for item in state.outbox.values_mut().filter(|item| !item.delivered) {
            let view = ChessView::collect(&source.snapshot());
            let history = view.move_history();
            match history.get(item.payload.expected_ply).copied() {
                Some(existing) if existing == item.payload.uci => {}
                Some(existing) => anyhow::bail!(
                    "Chess outbox expected ply {} to be `{}`, found `{existing}`",
                    item.payload.expected_ply,
                    item.payload.uci
                ),
                None if history.len() == item.payload.expected_ply => {
                    source.apply_legal_uci(&item.payload.uci)?;
                }
                None => anyhow::bail!(
                    "Chess outbox expected ply {}, current history has {} plies",
                    item.payload.expected_ply,
                    history.len()
                ),
            }
            item.delivered = true;
            delivered.push(item.payload.uci.clone());
        }
        Ok(delivered)
    }

    fn outbox_counts(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        (
            state.outbox.len(),
            state.outbox.values().filter(|item| item.delivered).count(),
        )
    }

    #[cfg(test)]
    fn publication_count(&self) -> usize {
        self.publications.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl DurableMountedStateBackend<ChessCommitPayload> for ChessBackend {
    type Error = Infallible;

    async fn load(
        &self,
        _session_id: &DurableSessionId,
    ) -> Result<MountedStateSnapshot, Self::Error> {
        let state = self.state.lock().unwrap();
        Ok(self.snapshot(&state, self.now()))
    }

    async fn compare_exchange(
        &self,
        request: MountedStateWrite<'_, ChessCommitPayload>,
    ) -> Result<MountedStateWriteOutcome, Self::Error> {
        let mut state = self.state.lock().unwrap();
        let now = self.now();
        let current = self.snapshot(&state, now);
        if request.expected_generation() != current.generation() {
            return Ok(MountedStateWriteOutcome::conflict(current));
        }
        if request
            .must_commit_before_unix_ms()
            .is_some_and(|deadline| deadline <= now)
        {
            return Ok(MountedStateWriteOutcome::deadline_elapsed(current));
        }

        if request.outbox().is_some() {
            self.publications.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(outbox) = request.outbox() {
            for item in outbox.items() {
                let key = item.id().to_string();
                let candidate = StoredChessCommit {
                    payload: item.payload().clone(),
                    delivered: false,
                };
                match state.outbox.get(&key) {
                    Some(existing) if existing == &candidate => {}
                    Some(_) => panic!("outbox identity collision for {key}"),
                    None => {
                        state.outbox.insert(key, candidate);
                    }
                }
            }
        }
        state.generation += 1;
        state.state = Some(request.state().clone());
        let generation =
            MountedStateGeneration::new(format!("chess-generation-{}", state.generation))
                .expect("the generated mounted revision is valid");
        Ok(MountedStateWriteOutcome::committed(generation, now))
    }
}

#[derive(Clone)]
struct ScriptedEpoch(ProviderEpochReceipt);

#[derive(Default)]
struct ScriptedProvider {
    systems: Mutex<Vec<String>>,
    users: Mutex<Vec<String>>,
    scripts: Mutex<VecDeque<Vec<String>>>,
    executions: AtomicUsize,
}

impl ScriptedProvider {
    fn with_scripts(scripts: impl IntoIterator<Item = Vec<String>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            ..Self::default()
        }
    }

    fn systems(&self) -> Vec<String> {
        self.systems.lock().unwrap().clone()
    }

    fn users(&self) -> Vec<String> {
        self.users.lock().unwrap().clone()
    }
}

#[derive(Debug, thiserror::Error)]
enum ScriptedProviderError {
    #[error(transparent)]
    Wire(#[from] ProviderWireFault),
    #[error("the scripted provider has no response for this turn")]
    MissingResponse,
}

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for ScriptedProvider {
    type Error = ScriptedProviderError;
    type Epoch = ScriptedEpoch;

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
        debug_assert_eq!(epoch.0.adapter(), "example-chess-scripted-provider");
        self.users.lock().unwrap().push(request.user().to_owned());
        let chunks = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ScriptedProviderError::MissingResponse)?;
        for chunk in chunks {
            wire.submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(chunk)))
                .await?;
        }
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(MountedProviderExit::completed(ExecutorCommit::empty()))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for ScriptedProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("example-chess-scripted-provider", 1)
            .expect("the static provider contract is valid")
    }

    async fn attach_durable_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
        self.systems
            .lock()
            .unwrap()
            .push(request.system().to_owned());
        let receipt = ProviderEpochReceipt::new(
            "example-chess-scripted-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "conversation": "example-chess-agentloop" }),
        )
        .expect("the static provider receipt is valid");
        Ok(AttachedProviderEpoch::new(
            ScriptedEpoch(receipt.clone()),
            receipt,
        ))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        Ok(ScriptedEpoch(request.receipt().clone()))
    }
}

#[derive(Default)]
struct ChessLiveProbe {
    moves: Arc<Mutex<Vec<String>>>,
}

struct ChessLiveRuntime {
    moves: Arc<Mutex<Vec<String>>>,
    applied: Vec<String>,
}

#[async_trait::async_trait]
impl LiveEffectRuntime<ChessAction> for ChessLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        effect: ChessAction,
    ) -> Result<(), Self::Error> {
        let uci = effect.uci().to_owned();
        self.moves.lock().unwrap().push(uci.clone());
        self.applied.push(uci);
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        if context.applied_effects() == 0 {
            return Ok(LiveEffectAbortAck::NoEffectsApplied);
        }

        let mut moves = self.moves.lock().unwrap();
        for applied in self.applied.drain(..).rev() {
            if let Some(index) = moves.iter().rposition(|candidate| candidate == &applied) {
                moves.remove(index);
            }
        }
        Ok(LiveEffectAbortAck::CompensationCompleted)
    }
}

fn call_input(
    index: usize,
    task: &str,
    source: &ChessGameSource,
) -> anyhow::Result<MountedCallInput<ChessCallProps, ChessGameSource>> {
    Ok(MountedCallInput::new(
        DurableCallId::new(format!("chess-agentloop-call-{index}"))?,
        DurableCallInputId::new(format!("chess-agentloop-call-{index}/input-v1"))?,
        format!("chess-agentloop-turn-{index}"),
        Arc::new(ChessCallProps {
            task: task.to_owned(),
            turn_id: format!("chess-agentloop-turn-{index}"),
        }),
        Arc::new(source.clone()),
    )?)
}

async fn run_player_turn(
    agent: &MountedAgent<ChessChannels, ChessCallProps, ChessGameSource>,
    backend: &ChessBackend,
    source: &ChessGameSource,
    index: usize,
) -> anyhow::Result<MountedCallOutcome<ChessChannels>> {
    let mut call = agent
        .start(call_input(index, "Choose White's next move.", source)?)
        .await?;
    let outcome = call.wait().await?;
    backend.deliver_pending(source)?;
    Ok(outcome)
}

async fn open_agent(
    backend: Arc<ChessBackend>,
    provider: Arc<ScriptedProvider>,
    live: Arc<ChessLiveProbe>,
) -> anyhow::Result<MountedAgent<ChessChannels, ChessCallProps, ChessGameSource>> {
    let live_factory = move || ChessLiveRuntime {
        moves: Arc::clone(&live.moves),
        applied: Vec::new(),
    };
    let factory = DurableMountedAgentFactory::new(
        DurableSessionId::new("example/chess-mounted-agentloop/session-1")?,
        backend,
        AgentSession::new(PromptContext::<String, ()>::without_system()),
        provider,
        "example-chess-model",
        256,
        DurableMountedHostConfig::new(
            "example.chess-mounted-agentloop-host/v1",
            "example.chess-mounted-agentloop-bindings/v1",
        ),
        ChessSessionReducer,
        live_factory,
        ChessCommitStager,
        ChessOutboxFingerprint,
    )?;
    Ok(factory.open(chess_definition()).await?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = ChessGameSource::new();
    let backend = Arc::new(ChessBackend::default());
    let provider = Arc::new(ScriptedProvider::with_scripts([
        vec!["<move uci=\"e2".to_owned(), "e4\" />".to_owned()],
        vec!["<move uci=\"g1f3\" />".to_owned()],
    ]));
    let live = Arc::new(ChessLiveProbe::default());
    let agent = open_agent(
        Arc::clone(&backend),
        Arc::clone(&provider),
        Arc::clone(&live),
    )
    .await?;

    let first = run_player_turn(&agent, &backend, &source, 1).await?;
    anyhow::ensure!(matches!(first, MountedCallOutcome::Executed { .. }));

    let stockfish = StockfishEngine::new(
        std::env::var("AGENTVIEW_STOCKFISH_BIN").unwrap_or_else(|_| "stockfish".to_owned()),
    );
    let engine_move = stockfish.best_move(&source.snapshot()).await?;
    source.apply_engine_uci(&engine_move)?;

    let second = run_player_turn(&agent, &backend, &source, 2).await?;
    anyhow::ensure!(matches!(second, MountedCallOutcome::Executed { .. }));

    let systems = provider.systems();
    let users = provider.users();
    println!("SYSTEM attachments={}", systems.len());
    println!("USER 1 mode=full\n{}", users[0]);
    println!("ENGINE move={engine_move}");
    println!("USER 2 mode=delta\n{}", users[1]);
    println!("LIVE previews={:?}", live.moves.lock().unwrap());
    println!("OUTBOX total/delivered={:?}", backend.outbox_counts());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_once_full_then_committed_delta_and_replay_uses_the_outbox() -> anyhow::Result<()>
    {
        let source = ChessGameSource::new();
        let backend = Arc::new(ChessBackend::default());
        let provider = Arc::new(ScriptedProvider::with_scripts([
            vec!["<move uci=\"e2".to_owned(), "e4\" />".to_owned()],
            vec!["<move uci=\"g1f3\" />".to_owned()],
        ]));
        let live = Arc::new(ChessLiveProbe::default());
        let agent = open_agent(
            Arc::clone(&backend),
            Arc::clone(&provider),
            Arc::clone(&live),
        )
        .await?;

        assert!(matches!(
            run_player_turn(&agent, &backend, &source, 1).await?,
            MountedCallOutcome::Executed { .. }
        ));
        assert_eq!(backend.outbox_counts(), (1, 1));

        let mut replay = agent
            .start(call_input(1, "Choose White's next move.", &source)?)
            .await?;
        assert!(matches!(
            replay.wait().await?,
            MountedCallOutcome::Replayed { .. }
        ));
        assert!(backend.deliver_pending(&source)?.is_empty());
        assert_eq!(provider.executions.load(Ordering::SeqCst), 1);

        source.apply_engine_uci("e7e5")?;
        assert!(matches!(
            run_player_turn(&agent, &backend, &source, 2).await?,
            MountedCallOutcome::Executed { .. }
        ));

        let systems = provider.systems();
        let users = provider.users();
        assert_eq!(systems.len(), 1);
        assert_eq!(users.len(), 2);
        assert!(!users[0].contains("rendering_mode=\"delta\""));
        assert!(users[0].contains("<side_to_move>white</side_to_move>"));
        assert!(users[1].contains("rendering_mode=\"delta\""));
        assert!(users[1].contains("<move>e2e4</move>"));
        assert!(users[1].contains("<move>e7e5</move>"));
        assert_eq!(backend.outbox_counts(), (2, 2));
        assert_eq!(backend.publication_count(), 2);
        assert_eq!(
            *live.moves.lock().unwrap(),
            vec!["e2e4".to_owned(), "g1f3".to_owned()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_complete_aborts_live_and_does_not_publish_the_user_cursor(
    ) -> anyhow::Result<()> {
        let source = ChessGameSource::new();
        let backend = Arc::new(ChessBackend::default());
        let provider = Arc::new(ScriptedProvider::with_scripts([vec![
            "<move uci=\"e2e4\">prose</move>".to_owned(),
        ]]));
        let live = Arc::new(ChessLiveProbe::default());
        let agent = open_agent(
            Arc::clone(&backend),
            Arc::clone(&provider),
            Arc::clone(&live),
        )
        .await?;

        let failure = run_player_turn(&agent, &backend, &source, 1)
            .await
            .expect_err("semantic-invalid XML must fail before publication");
        assert!(!failure.to_string().is_empty());
        assert!(live.moves.lock().unwrap().is_empty());
        assert_eq!(backend.outbox_counts(), (0, 0));
        assert_eq!(backend.publication_count(), 0);
        assert!(ChessView::collect(&source.snapshot())
            .move_history()
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_second_move_aborts_live_before_commit_publication() -> anyhow::Result<()> {
        let source = ChessGameSource::new();
        let backend = Arc::new(ChessBackend::default());
        let provider = Arc::new(ScriptedProvider::with_scripts([vec![
            "<move uci=\"e2e4\"/><move uci=\"d2d4\"/>".to_owned(),
        ]]));
        let live = Arc::new(ChessLiveProbe::default());
        let agent = open_agent(
            Arc::clone(&backend),
            Arc::clone(&provider),
            Arc::clone(&live),
        )
        .await?;

        run_player_turn(&agent, &backend, &source, 1)
            .await
            .expect_err("multiple moves must fail before publication");
        assert!(live.moves.lock().unwrap().is_empty());
        assert_eq!(backend.outbox_counts(), (0, 0));
        assert_eq!(backend.publication_count(), 0);
        assert!(ChessView::collect(&source.snapshot())
            .move_history()
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn prose_around_a_move_is_not_a_valid_provider_envelope() -> anyhow::Result<()> {
        let source = ChessGameSource::new();
        let backend = Arc::new(ChessBackend::default());
        let provider = Arc::new(ScriptedProvider::with_scripts([vec![
            "prefix <move uci=\"e2e4\" /> suffix".to_owned(),
        ]]));
        let live = Arc::new(ChessLiveProbe::default());
        let agent = open_agent(
            Arc::clone(&backend),
            Arc::clone(&provider),
            Arc::clone(&live),
        )
        .await?;

        run_player_turn(&agent, &backend, &source, 1)
            .await
            .expect_err("the provider and external bindings must share one strict envelope");
        assert!(live.moves.lock().unwrap().is_empty());
        assert_eq!(backend.outbox_counts(), (0, 0));
        assert_eq!(backend.publication_count(), 0);
        assert!(ChessView::collect(&source.snapshot())
            .move_history()
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rejected_reply_leaves_the_next_logical_call_on_a_full_user_baseline(
    ) -> anyhow::Result<()> {
        let source = ChessGameSource::new();
        let backend = Arc::new(ChessBackend::default());
        let provider = Arc::new(ScriptedProvider::with_scripts([
            vec!["<move uci=\"e2e4\">prose</move>".to_owned()],
            vec!["<move uci=\"e2e4\" />".to_owned()],
        ]));
        let live = Arc::new(ChessLiveProbe::default());
        let agent = open_agent(
            Arc::clone(&backend),
            Arc::clone(&provider),
            Arc::clone(&live),
        )
        .await?;

        run_player_turn(&agent, &backend, &source, 1)
            .await
            .expect_err("the first reply must be rejected without advancing the baseline");

        assert!(matches!(
            run_player_turn(&agent, &backend, &source, 2).await?,
            MountedCallOutcome::Executed { .. }
        ));

        let users = provider.users();
        assert_eq!(users.len(), 2);
        assert!(!users[0].contains("rendering_mode=\"delta\""));
        assert!(!users[1].contains("rendering_mode=\"delta\""));
        assert!(users[1].contains("<side_to_move>white</side_to_move>"));
        Ok(())
    }

    #[tokio::test]
    async fn rejected_reply_preserves_the_last_committed_delta_baseline() -> anyhow::Result<()> {
        let source = ChessGameSource::new();
        let backend = Arc::new(ChessBackend::default());
        let provider = Arc::new(ScriptedProvider::with_scripts([
            vec!["<move uci=\"e2e4\" />".to_owned()],
            vec!["<move uci=\"g1f3\">prose</move>".to_owned()],
            vec!["<move uci=\"g1f3\" />".to_owned()],
        ]));
        let live = Arc::new(ChessLiveProbe::default());
        let agent = open_agent(
            Arc::clone(&backend),
            Arc::clone(&provider),
            Arc::clone(&live),
        )
        .await?;

        assert!(matches!(
            run_player_turn(&agent, &backend, &source, 1).await?,
            MountedCallOutcome::Executed { .. }
        ));
        source.apply_engine_uci("e7e5")?;

        run_player_turn(&agent, &backend, &source, 2)
            .await
            .expect_err("the second reply must be rejected without replacing the baseline");
        assert_eq!(backend.outbox_counts(), (1, 1));
        assert_eq!(backend.publication_count(), 1);

        assert!(matches!(
            run_player_turn(&agent, &backend, &source, 3).await?,
            MountedCallOutcome::Executed { .. }
        ));

        let users = provider.users();
        assert_eq!(users.len(), 3);
        assert!(!users[0].contains("rendering_mode=\"delta\""));
        assert!(users[1].contains("rendering_mode=\"delta\""));
        assert!(users[2].contains("rendering_mode=\"delta\""));
        let second_context = users[1]
            .split_once("\n\n<chess_task>")
            .expect("the User document retains the complete task section")
            .0;
        let third_context = users[2]
            .split_once("\n\n<chess_task>")
            .expect("the User document retains the complete task section")
            .0;
        assert_eq!(second_context, third_context);
        assert!(users[1].contains("chess-agentloop-turn-2"));
        assert!(users[2].contains("chess-agentloop-turn-3"));
        assert_eq!(backend.outbox_counts(), (2, 2));
        assert_eq!(backend.publication_count(), 2);
        Ok(())
    }
}
