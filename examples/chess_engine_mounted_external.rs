//! Mounted external chess interaction.
//!
//! This directly reproduces the compatibility chess example's `System ->
//! observe -> act -> wait view -> asynchronous Stockfish -> hook` behavior.
//! The component owns only POM. `InMemoryChessHost` is deliberately a local
//! demonstration of `MountedExternalPort`; it uses one mutex to make the fake
//! domain/state/outbox/wake transaction visible, but it is not a production
//! durable database adapter. Its in-memory outbox shows the required User
//! delivery receipt, acknowledgement, and delta-baseline boundary.
//!
//! The canonical multi-process client is the AgentView binary:
//!
//! `cargo build --bin agentview`
//! `target/debug/agentview chess --help`
//!
//! Run `cargo run --example chess_engine_mounted_external` for the direct
//! single-process teaching trace. It prints the current action handle and
//! accepts the same explicit `agentview chess ack <handle>` followed by
//! `agentview chess act <handle> '<move uci="..." />'` protocol.

use std::{
    collections::BTreeMap,
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        advanced::{
            external::{
                ExternalActionTicket, ExternalCommitIdentity, ExternalCommitResolution,
                ExternalEpochAcquireRequest, ExternalEpochActivation, ExternalEpochAdmission,
                ExternalEpochArtifact, ExternalEpochComplete, ExternalEpochLease,
                ExternalFingerprint, ExternalObservation, ExternalObserveOutcome, ExternalReply,
                ExternalReplyId, ExternalSourceRevision, ExternalStateMutation,
                ExternalStateSnapshot, ExternalStateWrite, ExternalStateWriteOutcome,
                ExternalSystemDeliveryReceipt, ExternalUserDeliveryAckOutcome,
                ExternalUserDeliveryCandidate, ExternalUserDeliveryLane,
                ExternalUserDeliveryReceipt, ExternalUserFrame, ExternalUserResyncOutcome,
                ExternalWakeCursor, MountedExternalController, MountedExternalHarnessDefinition,
                MountedExternalPort,
            },
            persistence::{MountedStateBlob, MountedStateGeneration},
        },
        prelude::*,
        DurableCallId, DurableCallInputId, DurableSessionId,
    },
    control::ControlReply,
    semantic_view::AgentViewCollect,
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines},
    sync::Notify,
};

#[allow(dead_code)]
#[path = "chess/mod.rs"]
mod chess_support;

use chess_support::{
    chess_user_document, ChessAction, ChessGameSource, ChessReplyContract,
    ChessSystemPolicyPromptView, ChessView, StockfishEngine,
};

#[derive(Debug, Clone)]
struct ChessTurnProps {
    context: ChessView,
    task: String,
    turn_id: String,
}

#[view(component)]
fn chess_agent() -> PromptComponent<ChessTurnProps> {
    try_prompt_component(
        ChessSystemPolicyPromptView::default(),
        |props: &ChessTurnProps| {
            chess_user_document(
                props.context.clone(),
                props.task.clone(),
                props.turn_id.clone(),
            )
        },
    )
}

fn chess_definition() -> MountedExternalHarnessDefinition<ChessTurnProps, ChessReplyContract> {
    MountedExternalHarnessDefinition::new(
        chess_agent(),
        ExternalReply::new(ChessReplyContract::new()),
        EpochContractId::new("example/chess-external/v2").unwrap(),
    )
}

enum ChessEpoch {
    Empty,
    Creating(ExternalEpochLease),
    Active {
        artifact: ExternalEpochArtifact,
        system_delivery_receipt: ExternalSystemDeliveryReceipt,
    },
}

struct ChessHostState {
    epoch: ChessEpoch,
    state: Option<MountedStateBlob>,
    generation: Option<MountedStateGeneration>,
    next_generation: u64,
    board_revision: u64,
    // This is part of the host's authoritative chess transaction. A POM view
    // may describe the phase, but it is never the authorization source.
    player_action_available: bool,
    wake: u64,
    outbox_count: u64,
    delivered_system: Option<String>,
    user_outbox: BTreeMap<String, ChessUserDelivery>,
    resolutions: BTreeMap<String, ExternalCommitResolution>,
}

#[derive(Debug, Clone)]
struct ChessUserDelivery {
    lane: ExternalUserDeliveryLane,
    system_delivery_receipt: ExternalSystemDeliveryReceipt,
    base_receipt: Option<ExternalUserDeliveryReceipt>,
    user_fingerprint: ExternalFingerprint,
    rendered_user: String,
    acknowledged: bool,
    tombstoned: bool,
}

impl ChessUserDelivery {
    fn from_candidate(candidate: &ExternalUserDeliveryCandidate) -> Self {
        Self {
            lane: candidate.lane(),
            system_delivery_receipt: candidate.system_delivery_receipt().clone(),
            base_receipt: candidate.base_receipt().cloned(),
            user_fingerprint: candidate.user_fingerprint().clone(),
            rendered_user: candidate.rendered_user().to_owned(),
            acknowledged: false,
            tombstoned: false,
        }
    }

    fn matches_candidate(&self, candidate: &ExternalUserDeliveryCandidate) -> bool {
        self.lane == candidate.lane()
            && self.system_delivery_receipt == *candidate.system_delivery_receipt()
            && self.base_receipt.as_ref() == candidate.base_receipt()
            && self.user_fingerprint == *candidate.user_fingerprint()
            && self.rendered_user == candidate.rendered_user()
    }
}

impl ChessHostState {
    fn new() -> Self {
        Self {
            epoch: ChessEpoch::Empty,
            state: None,
            generation: None,
            next_generation: 0,
            board_revision: 1,
            player_action_available: true,
            wake: 0,
            outbox_count: 0,
            delivered_system: None,
            user_outbox: BTreeMap::new(),
            resolutions: BTreeMap::new(),
        }
    }

    fn source_revision(&self) -> ExternalSourceRevision {
        ExternalSourceRevision::new(format!("chess-board-{}", self.board_revision)).unwrap()
    }

    fn wake_cursor(&self) -> ExternalWakeCursor {
        ExternalWakeCursor::new(format!("chess-wake-{}", self.wake)).unwrap()
    }

    fn snapshot(&self) -> ExternalStateSnapshot {
        match (&self.generation, &self.state) {
            (Some(generation), Some(state)) => ExternalStateSnapshot::present(
                generation.clone(),
                state.clone(),
                self.wake_cursor(),
            ),
            (None, None) => ExternalStateSnapshot::missing(self.wake_cursor()),
            _ => panic!("opaque AgentView state and CAS generation must match"),
        }
    }

    fn next_generation(&mut self) -> MountedStateGeneration {
        self.next_generation += 1;
        MountedStateGeneration::new(format!("chess-generation-{}", self.next_generation)).unwrap()
    }

    fn publish_user_delivery(
        &mut self,
        candidate: &ExternalUserDeliveryCandidate,
    ) -> Result<(), ChessHostError> {
        let active_system_receipt = match &self.epoch {
            ChessEpoch::Active {
                system_delivery_receipt,
                ..
            } => system_delivery_receipt,
            _ => {
                return Err(ChessHostError::new(
                    "User delivery cannot publish before System activation",
                ))
            }
        };
        if candidate.system_delivery_receipt() != active_system_receipt {
            return Err(ChessHostError::new(format!(
                "User delivery `{}` does not depend on the active System receipt",
                candidate.receipt()
            )));
        }
        if candidate.lane() == ExternalUserDeliveryLane::Presentation
            && candidate.base_receipt().is_some()
        {
            return Err(ChessHostError::new(format!(
                "presentation delivery `{}` must not advance the prompt baseline",
                candidate.receipt()
            )));
        }
        if let Some(base_receipt) = candidate.base_receipt() {
            let base = self.user_outbox.get(base_receipt.as_str()).ok_or_else(|| {
                ChessHostError::new(format!(
                    "User delivery `{}` references unknown baseline `{base_receipt}`",
                    candidate.receipt()
                ))
            })?;
            if base.lane != ExternalUserDeliveryLane::Prompt || !base.acknowledged {
                return Err(ChessHostError::new(format!(
                    "User delivery `{}` references an unacknowledged prompt baseline `{base_receipt}`",
                    candidate.receipt()
                )));
            }
        }

        let key = candidate.receipt().as_str().to_owned();
        if let Some(existing) = self.user_outbox.get(&key) {
            if existing.matches_candidate(candidate) {
                return Ok(());
            }
            return Err(ChessHostError::new(format!(
                "User delivery receipt collision for `{}`",
                candidate.receipt()
            )));
        }
        self.user_outbox
            .insert(key, ChessUserDelivery::from_candidate(candidate));
        Ok(())
    }

    fn acknowledge_user_delivery(
        &mut self,
        receipt: &ExternalUserDeliveryReceipt,
    ) -> Result<(), ChessHostError> {
        let delivery = self.user_outbox.get_mut(receipt.as_str()).ok_or_else(|| {
            ChessHostError::new(format!("unknown User delivery receipt `{receipt}`"))
        })?;
        if delivery.lane != ExternalUserDeliveryLane::Prompt {
            return Err(ChessHostError::new(format!(
                "presentation delivery `{receipt}` cannot advance the prompt baseline"
            )));
        }
        if delivery.tombstoned {
            return Err(ChessHostError::new(format!(
                "User delivery `{receipt}` was cancelled before acknowledgement"
            )));
        }
        delivery.acknowledged = true;
        Ok(())
    }

    fn cancel_user_delivery(
        &mut self,
        receipt: &ExternalUserDeliveryReceipt,
    ) -> Result<(), ChessHostError> {
        let delivery = self.user_outbox.get_mut(receipt.as_str()).ok_or_else(|| {
            ChessHostError::new(format!("unknown User delivery receipt `{receipt}`"))
        })?;
        // A delivery already acknowledged by the consumer is immutable history.
        // An unacknowledged row must be durably withdrawn before a successor
        // may reuse the prompt lane.
        if !delivery.acknowledged {
            delivery.tombstoned = true;
        }
        Ok(())
    }
}

/// Local-only host showing the transaction shape required by the controller.
///
/// A production implementation uses one durable transaction authority rather
/// than this process-local mutex. Keeping all mutations in this one method is
/// intentional: it makes the otherwise easy-to-miss domain/state/outbox/wake
/// coupling visible in a runnable example.
struct InMemoryChessHost {
    source: ChessGameSource,
    state: Mutex<ChessHostState>,
    wake: Notify,
}

#[derive(Debug, thiserror::Error)]
#[error("in-memory chess host failed: {message}")]
struct ChessHostError {
    message: String,
}

impl ChessHostError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl InMemoryChessHost {
    fn new() -> Self {
        Self {
            source: ChessGameSource::new(),
            state: Mutex::new(ChessHostState::new()),
            wake: Notify::new(),
        }
    }

    fn capture(
        &self,
        task: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> ExternalObservation<ChessTurnProps> {
        let state = self.state.lock().unwrap();
        let revision = state.source_revision();
        let context = ChessView::collect(&self.source.snapshot());
        let props = ChessTurnProps {
            context,
            task: task.into(),
            turn_id: turn_id.into(),
        };
        if state.player_action_available {
            ExternalObservation::actionable_from_props(revision, props)
        } else {
            ExternalObservation::passive_from_props(revision, props)
        }
    }

    fn delivered_system(&self) -> Option<String> {
        self.state.lock().unwrap().delivered_system.clone()
    }

    fn player_action_available(&self) -> bool {
        self.state.lock().unwrap().player_action_available
    }

    fn next_player_task(&self) -> &'static str {
        if self.player_action_available() {
            "Choose white's next move."
        } else {
            "Stockfish failed; wait for host recovery."
        }
    }

    fn user_delivery(
        &self,
        receipt: &ExternalUserDeliveryReceipt,
    ) -> anyhow::Result<ChessUserDelivery> {
        self.state
            .lock()
            .unwrap()
            .user_outbox
            .get(receipt.as_str())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("host did not retain User delivery `{receipt}`"))
    }

    fn rendered_user(&self, receipt: &ExternalUserDeliveryReceipt) -> anyhow::Result<String> {
        let delivery = self.user_delivery(receipt)?;
        if delivery.tombstoned {
            anyhow::bail!("User delivery `{receipt}` was tombstoned")
        }
        Ok(delivery.rendered_user)
    }

    /// Run Stockfish against one captured board revision, then publish the
    /// resulting domain change and wake under the host's transaction lock.
    async fn apply_engine_move(&self, engine: &StockfishEngine) -> anyhow::Result<String> {
        let (expected_revision, snapshot) = {
            let state = self.state.lock().unwrap();
            (state.source_revision(), self.source.snapshot())
        };
        let selected = engine.best_move(&snapshot).await;

        let mut state = self.state.lock().unwrap();
        if state.source_revision() != expected_revision {
            anyhow::bail!("chess board changed while Stockfish was evaluating {expected_revision}");
        }
        let outcome = match selected {
            Ok(uci) => match self.source.apply_engine_uci(&uci) {
                Ok(()) => Ok(uci),
                Err(error) => {
                    self.source
                        .record_engine_failure(format!("stockfish move failed: {error}"));
                    Err(error)
                }
            },
            Err(error) => {
                self.source
                    .record_engine_failure(format!("stockfish failed: {error}"));
                Err(error)
            }
        };
        state.player_action_available = outcome.is_ok();
        state.board_revision += 1;
        state.wake += 1;
        drop(state);
        self.wake.notify_waiters();
        outcome
    }

    fn identity_key(identity: &ExternalCommitIdentity) -> String {
        format!("{identity:?}")
    }
}

#[async_trait::async_trait]
impl MountedExternalPort<ChessAction> for InMemoryChessHost {
    type Error = ChessHostError;

    async fn acquire_epoch(
        &self,
        request: ExternalEpochAcquireRequest<'_>,
    ) -> Result<ExternalEpochAdmission, Self::Error> {
        let mut state = self.state.lock().unwrap();
        match &state.epoch {
            ChessEpoch::Empty => {
                let lease = ExternalEpochLease::new(
                    request.session_id().clone(),
                    request.epoch_contract_id().clone(),
                    1,
                    "chess-epoch-lease-1",
                )
                .map_err(|error| ChessHostError::new(error.to_string()))?;
                state.epoch = ChessEpoch::Creating(lease.clone());
                Ok(ExternalEpochAdmission::Create { lease })
            }
            ChessEpoch::Creating(lease) => Ok(ExternalEpochAdmission::InFlight {
                generation: lease.generation(),
            }),
            ChessEpoch::Active {
                artifact,
                system_delivery_receipt,
            } if artifact.session_id() == request.session_id()
                && artifact.epoch_contract_id() == request.epoch_contract_id() =>
            {
                Ok(ExternalEpochAdmission::Existing {
                    artifact: artifact.clone(),
                    system_delivery_receipt: system_delivery_receipt.clone(),
                    state: state.snapshot(),
                })
            }
            ChessEpoch::Active { artifact, .. } => Ok(ExternalEpochAdmission::ContractMismatch {
                active: artifact.epoch_contract_id().clone(),
                requested: request.epoch_contract_id().clone(),
            }),
        }
    }

    async fn complete_epoch(
        &self,
        request: ExternalEpochComplete<'_>,
    ) -> Result<ExternalEpochActivation, Self::Error> {
        let mut state = self.state.lock().unwrap();
        match &state.epoch {
            ChessEpoch::Creating(lease) if lease == request.lease() => {}
            _ => {
                return Err(ChessHostError::new(
                    "only the Create lease may install System",
                ))
            }
        }
        let system_delivery_receipt = ExternalSystemDeliveryReceipt::new("chess-system-delivery/1")
            .map_err(|error| ChessHostError::new(error.to_string()))?;
        state.delivered_system = Some(request.rendered_system().to_owned());
        state.state = Some(request.initial_state().clone());
        state.generation = Some(state.next_generation());
        state.epoch = ChessEpoch::Active {
            artifact: request.artifact().clone(),
            system_delivery_receipt: system_delivery_receipt.clone(),
        };
        Ok(ExternalEpochActivation::new(
            state.snapshot(),
            system_delivery_receipt,
        ))
    }

    async fn abandon_epoch(&self, lease: &ExternalEpochLease) -> Result<(), Self::Error> {
        let mut state = self.state.lock().unwrap();
        if matches!(&state.epoch, ChessEpoch::Creating(current) if current == lease) {
            state.epoch = ChessEpoch::Empty;
        }
        Ok(())
    }

    async fn load(
        &self,
        _session_id: &DurableSessionId,
    ) -> Result<ExternalStateSnapshot, Self::Error> {
        Ok(self.state.lock().unwrap().snapshot())
    }

    async fn compare_exchange(
        &self,
        request: ExternalStateWrite<'_, ChessAction>,
    ) -> Result<ExternalStateWriteOutcome, Self::Error> {
        let mut state = self.state.lock().unwrap();
        if request.expected_generation() != state.generation.as_ref() {
            return Ok(ExternalStateWriteOutcome::Conflict {
                current: state.snapshot(),
            });
        }
        let mutation = request.mutation();
        if matches!(
            mutation,
            ExternalStateMutation::PrepareReply { .. } | ExternalStateMutation::Commit { .. }
        ) && !state.player_action_available
        {
            return Err(ChessHostError::new(
                "player action is unavailable while Stockfish is pending",
            ));
        }
        if mutation
            .source_revision()
            .is_some_and(|revision| revision != &state.source_revision())
        {
            return Ok(ExternalStateWriteOutcome::SourceStale {
                current: state.snapshot(),
                actual: state.source_revision(),
            });
        }
        if let ExternalStateMutation::PrepareReply { action, .. } = mutation {
            self.source
                .validate_legal_uci(&action.uci)
                .map_err(|error| {
                    ChessHostError::new(format!("domain rejected `{}`: {error}", action.uci))
                })?;
        }
        if let Some(delivery) = mutation.user_delivery() {
            state.publish_user_delivery(delivery)?;
        }
        if let ExternalStateMutation::AcknowledgeUserDelivery { receipt } = mutation {
            state.acknowledge_user_delivery(receipt)?;
        }
        if let ExternalStateMutation::Cancel {
            delivery_receipt, ..
        } = mutation
        {
            state.cancel_user_delivery(delivery_receipt)?;
        }

        if let ExternalStateMutation::Commit { identity, action } = mutation {
            let key = Self::identity_key(identity);
            if state
                .resolutions
                .get(&key)
                .is_some_and(|resolution| *resolution == ExternalCommitResolution::Committed)
            {
                return Ok(ExternalStateWriteOutcome::Committed {
                    generation: state.generation.clone().unwrap(),
                    wake_cursor: state.wake_cursor(),
                });
            }
            self.source.apply_legal_uci(&action.uci).map_err(|error| {
                ChessHostError::new(format!("domain rejected `{}`: {error}", action.uci))
            })?;
            state.player_action_available = false;
            state.board_revision += 1;
            state.outbox_count += 1;
            state.wake += 1;
            state.state = Some(request.state().clone());
            state.generation = Some(state.next_generation());
            state
                .resolutions
                .insert(key, ExternalCommitResolution::Committed);
            let generation = state.generation.clone().unwrap();
            let wake_cursor = state.wake_cursor();
            drop(state);
            self.wake.notify_waiters();
            return Ok(ExternalStateWriteOutcome::Committed {
                generation,
                wake_cursor,
            });
        }

        state.state = Some(request.state().clone());
        state.generation = Some(state.next_generation());
        Ok(ExternalStateWriteOutcome::Committed {
            generation: state.generation.clone().unwrap(),
            wake_cursor: state.wake_cursor(),
        })
    }

    async fn wait_for_wake(
        &self,
        _session_id: &DurableSessionId,
        after: &ExternalWakeCursor,
    ) -> Result<ExternalWakeCursor, Self::Error> {
        loop {
            let notified = self.wake.notified();
            let cursor = self.state.lock().unwrap().wake_cursor();
            if &cursor != after {
                return Ok(cursor);
            }
            notified.await;
        }
    }

    async fn resolve_commit(
        &self,
        identity: &ExternalCommitIdentity,
    ) -> Result<ExternalCommitResolution, Self::Error> {
        Ok(*self
            .state
            .lock()
            .unwrap()
            .resolutions
            .get(&Self::identity_key(identity))
            .unwrap_or(&ExternalCommitResolution::NotCommitted))
    }
}

#[allow(dead_code)] // Used by this example's black-box tests.
fn ticket(outcome: ExternalObserveOutcome) -> anyhow::Result<ExternalActionTicket> {
    match outcome {
        ExternalObserveOutcome::Frame(ExternalUserFrame::Actionable(ticket)) => Ok(ticket),
        ExternalObserveOutcome::SourceStale { actual, .. } => {
            anyhow::bail!("captured chess board became stale at {actual}")
        }
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(_)) => {
            anyhow::bail!("expected an actionable chess frame")
        }
        _ => anyhow::bail!("unexpected external observation outcome"),
    }
}

fn frame(outcome: ExternalObserveOutcome) -> anyhow::Result<ExternalUserFrame> {
    match outcome {
        ExternalObserveOutcome::Frame(frame) => Ok(frame),
        ExternalObserveOutcome::SourceStale { actual, .. } => {
            anyhow::bail!("captured chess board became stale at {actual}")
        }
        _ => anyhow::bail!("unexpected external observation outcome"),
    }
}

fn presentation(
    outcome: ExternalObserveOutcome,
) -> anyhow::Result<agentview::component::advanced::external::ExternalPassivePresentation> {
    match outcome {
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(presentation)) => Ok(presentation),
        ExternalObserveOutcome::SourceStale { actual, .. } => {
            anyhow::bail!("captured chess board became stale at {actual}")
        }
        ExternalObserveOutcome::Frame(ExternalUserFrame::Actionable(_)) => {
            anyhow::bail!("expected a passive chess presentation")
        }
        _ => anyhow::bail!("unexpected external observation outcome"),
    }
}

/// Snapshot returned by the mounted chess facade used by the repository CLI.
///
/// `prompt` is the host-owned User delivery. `view` is only a convenient full
/// inspection render for the local CLI; it is not an additional prompt
/// delivery and therefore never participates in the delta cursor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountedChessCliSnapshot {
    pub epoch: u64,
    pub turn_id: String,
    pub view: String,
    pub prompt: String,
    pub delivery_receipt: String,
    pub actionable: bool,
    pub action_handle: Option<String>,
    pub prompt_mode: Option<MountedChessPromptMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum MountedChessPromptMode {
    Full,
    Delta { base_delivery: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum MountedChessSystemAttachment {
    InstallSystemOnce {
        delivery_id: String,
        document: String,
    },
    Attached {
        delivery_id: String,
    },
}

impl MountedChessSystemAttachment {
    pub fn delivery_id(&self) -> &str {
        match self {
            Self::InstallSystemOnce { delivery_id, .. } | Self::Attached { delivery_id } => {
                delivery_id
            }
        }
    }
}

#[derive(Clone)]
struct MountedChessCliFrame {
    frame: ExternalUserFrame,
    turn_id: String,
}

#[derive(Clone)]
struct MountedChessCliReplay {
    raw_reply: String,
    ticket: ExternalActionTicket,
    response: MountedChessCliSnapshot,
}

/// Small in-process adapter for the repository's demo daemon.
///
/// It intentionally uses the example's in-memory port, but it drives the
/// exact mounted-external state machine: System is delivered only on epoch
/// creation. The external consumer must explicitly acknowledge the currently
/// rendered Actionable delivery before `act`; only acknowledged Actionable
/// frames become delta bases.
/// Passive Stockfish frames are always self-contained presentations.
pub struct MountedExternalChessCli {
    host: Arc<InMemoryChessHost>,
    controller: MountedExternalController<ChessTurnProps, ChessReplyContract, InMemoryChessHost>,
    system_delivery_id: String,
    system_document: String,
    system_acknowledged: bool,
    engine: StockfishEngine,
    next_frame_index: u64,
    current: Option<MountedChessCliFrame>,
    pending_wake: Option<ExternalWakeCursor>,
    last_action: Option<MountedChessCliReplay>,
}

impl MountedExternalChessCli {
    pub async fn open(stockfish_command: impl Into<String>) -> anyhow::Result<Self> {
        let host = Arc::new(InMemoryChessHost::new());
        let opened = MountedExternalController::open(
            chess_definition(),
            Arc::clone(&host),
            DurableSessionId::new("agentview-cli/chess-session-1")?,
        )
        .await?;
        let system_delivery_id = opened.system_delivery_receipt().as_str().to_owned();
        let system_document = host
            .delivered_system()
            .ok_or_else(|| anyhow::anyhow!("example host did not retain the System delivery"))?;

        Ok(Self {
            host,
            controller: opened.into_controller(),
            system_delivery_id,
            system_document,
            system_acknowledged: false,
            engine: StockfishEngine::new(stockfish_command.into()),
            next_frame_index: 1,
            current: None,
            pending_wake: None,
            last_action: None,
        })
    }

    pub fn system_attachment(&self) -> MountedChessSystemAttachment {
        if self.system_acknowledged {
            MountedChessSystemAttachment::Attached {
                delivery_id: self.system_delivery_id.clone(),
            }
        } else {
            MountedChessSystemAttachment::InstallSystemOnce {
                delivery_id: self.system_delivery_id.clone(),
                document: self.system_document.clone(),
            }
        }
    }

    pub fn acknowledge_system(&mut self, delivery_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            delivery_id == self.system_delivery_id,
            "unknown Chess System delivery `{delivery_id}`"
        );
        self.system_acknowledged = true;
        Ok(())
    }

    fn ensure_system_acknowledged(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.system_acknowledged,
            "Chess System must be acknowledged before User delivery"
        );
        Ok(())
    }

    pub async fn observe(&mut self) -> anyhow::Result<MountedChessCliSnapshot> {
        self.ensure_system_acknowledged()?;
        if let Some(current) = self.current.as_ref() {
            return self.snapshot(current);
        }
        if self.pending_wake.is_some() {
            anyhow::bail!("Stockfish is pending; call `agentview chess hook`");
        }

        let frame_index = self.next_frame_index;
        let turn_id = Self::turn_id(frame_index);
        let observed = self
            .controller
            .observe(
                Self::call_id(frame_index)?,
                Self::input_id(frame_index)?,
                self.host
                    .capture("Choose white's next move.", turn_id.clone()),
            )
            .await?;
        let frame = frame(observed)?;
        anyhow::ensure!(
            matches!(frame, ExternalUserFrame::Actionable(_)),
            "expected an actionable chess frame"
        );
        self.next_frame_index += 1;
        let current = MountedChessCliFrame { frame, turn_id };
        let snapshot = self.snapshot(&current)?;
        self.current = Some(current);
        Ok(snapshot)
    }

    pub async fn acknowledge_user_delivery(
        &mut self,
        action_handle: &str,
    ) -> anyhow::Result<ExternalUserDeliveryAckOutcome> {
        self.ensure_system_acknowledged()?;
        let current = self.current.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no active chess turn; run `agentview chess observe` first")
        })?;
        let ticket = current.frame.action_ticket().ok_or_else(|| {
            anyhow::anyhow!("the current Chess frame is Passive and has no action handle")
        })?;
        anyhow::ensure!(
            ticket.delivery_receipt().as_str() == action_handle,
            "stale Chess action handle `{action_handle}`; current handle is `{}`",
            ticket.delivery_receipt()
        );
        Ok(self
            .controller
            .acknowledge_user_delivery(ticket.delivery_receipt())
            .await?)
    }

    /// Submit raw reply bytes against the exact acknowledged Actionable frame.
    /// Delivery acknowledgement is intentionally a separate operation.
    pub async fn act(
        &mut self,
        action_handle: &str,
        raw_reply: String,
    ) -> anyhow::Result<MountedChessCliSnapshot> {
        self.ensure_system_acknowledged()?;
        if let Some(replay) = self.last_action.as_ref() {
            if replay.ticket.delivery_receipt().as_str() == action_handle {
                anyhow::ensure!(
                    replay.raw_reply == raw_reply,
                    "Chess action handle `{action_handle}` was already answered with different reply bytes"
                );
                anyhow::ensure!(
                    self.current.as_ref().is_some_and(|current| matches!(
                        current.frame,
                        ExternalUserFrame::Passive(_)
                    )),
                    "the replayed Chess action is no longer the current transition"
                );
                match self
                    .controller
                    .act(
                        replay.ticket.token(),
                        Self::reply_id(replay.ticket.delivery_receipt(), &raw_reply)?,
                        ControlReply::text(raw_reply),
                    )
                    .await?
                {
                    agentview::component::advanced::external::ExternalActOutcome::Replayed(_) => {
                        return Ok(replay.response.clone());
                    }
                    outcome => {
                        anyhow::bail!(
                            "expected a replay for the already committed chess reply, got {outcome:?}"
                        );
                    }
                }
            }
        }

        let current = self.current.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no active chess turn; run `agentview chess observe` first")
        })?;
        let ticket =
            current.frame.action_ticket().cloned().ok_or_else(|| {
                anyhow::anyhow!("Stockfish is pending; call `agentview chess hook`")
            })?;
        anyhow::ensure!(
            ticket.delivery_receipt().as_str() == action_handle,
            "stale Chess action handle `{action_handle}`; current handle is `{}`",
            ticket.delivery_receipt()
        );
        let committed = match self
            .controller
            .act(
                ticket.token(),
                Self::reply_id(ticket.delivery_receipt(), &raw_reply)?,
                ControlReply::text(raw_reply.clone()),
            )
            .await?
        {
            agentview::component::advanced::external::ExternalActOutcome::Applied(commit)
            | agentview::component::advanced::external::ExternalActOutcome::Replayed(commit) => {
                commit
            }
            agentview::component::advanced::external::ExternalActOutcome::InvalidReply {
                diagnostic,
            } => anyhow::bail!("reply rejected: {diagnostic}"),
            agentview::component::advanced::external::ExternalActOutcome::SourceStale {
                actual,
                ..
            } => anyhow::bail!("chess position changed while awaiting a reply: {actual}"),
            agentview::component::advanced::external::ExternalActOutcome::Indeterminate {
                identity,
            } => anyhow::bail!("chess commit requires recovery: {identity:?}"),
            _ => anyhow::bail!("unsupported external chess action outcome"),
        };

        // Publish the waiting view before starting Stockfish. It is a Passive
        // delivery, so it cannot consume the just-acknowledged prompt cursor.
        let frame_index = self.next_frame_index;
        let turn_id = Self::turn_id(frame_index);
        let observed = self
            .controller
            .observe(
                Self::call_id(frame_index)?,
                Self::input_id(frame_index)?,
                self.host
                    .capture("Wait for the engine reply.", turn_id.clone()),
            )
            .await?;
        let waiting = presentation(observed)?;
        self.next_frame_index += 1;
        let current = MountedChessCliFrame {
            frame: ExternalUserFrame::Passive(waiting),
            turn_id,
        };
        let response = self.snapshot(&current)?;
        self.pending_wake = Some(committed.wake_cursor().clone());
        self.last_action = Some(MountedChessCliReplay {
            raw_reply,
            ticket,
            response: response.clone(),
        });
        self.current = Some(current);

        let host = Arc::clone(&self.host);
        let engine = self.engine.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = host.apply_engine_move(&engine).await;
        });

        Ok(response)
    }

    pub async fn request_user_resync(
        &mut self,
    ) -> anyhow::Result<(ExternalUserResyncOutcome, MountedChessCliSnapshot)> {
        self.ensure_system_acknowledged()?;
        let replaces_actionable = self
            .current
            .as_ref()
            .is_some_and(|current| matches!(current.frame, ExternalUserFrame::Actionable(_)));
        let outcome = self.controller.request_user_document_resync().await?;
        if replaces_actionable || self.current.is_none() {
            self.current = None;
            self.last_action = None;
            return Ok((outcome, self.observe().await?));
        }
        let current = self
            .current
            .as_ref()
            .expect("the Passive current frame was checked above");
        Ok((outcome, self.snapshot(current)?))
    }

    pub async fn hook(&mut self) -> anyhow::Result<MountedChessCliSnapshot> {
        self.ensure_system_acknowledged()?;
        let current = self.current.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no active chess turn; run `agentview chess observe` first")
        })?;
        anyhow::ensure!(
            matches!(current.frame, ExternalUserFrame::Passive(_)),
            "the current chess frame is actionable; submit `agentview chess act ...` first"
        );
        let after = self
            .pending_wake
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Stockfish is not pending"))?
            .clone();

        let frame_index = self.next_frame_index;
        let turn_id = Self::turn_id(frame_index);
        let captured_turn_id = turn_id.clone();
        let host = Arc::clone(&self.host);
        let task = host.next_player_task().to_owned();
        let hooked = self
            .controller
            .hook(
                &after,
                Self::call_id(frame_index)?,
                Self::input_id(frame_index)?,
                move || host.capture(task, captured_turn_id),
            )
            .await?;
        let frame = frame(hooked.into_observation())?;
        self.next_frame_index += 1;
        let current = MountedChessCliFrame { frame, turn_id };
        let response = self.snapshot(&current)?;
        self.current = Some(current);
        self.pending_wake = None;
        self.last_action = None;
        Ok(response)
    }

    pub async fn close(&mut self) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        let Some(ticket) = current.frame.action_ticket() else {
            return;
        };
        let _ = self.controller.cancel(ticket.token()).await;
    }

    fn snapshot(&self, current: &MountedChessCliFrame) -> anyhow::Result<MountedChessCliSnapshot> {
        let receipt = current.frame.delivery_receipt();
        let delivery = self.host.user_delivery(receipt)?;
        let action_handle = current
            .frame
            .action_ticket()
            .map(|ticket| ticket.delivery_receipt().as_str().to_owned());
        let prompt_mode = action_handle.as_ref().map(|_| match delivery.base_receipt {
            Some(base) => MountedChessPromptMode::Delta {
                base_delivery: base.as_str().to_owned(),
            },
            None => MountedChessPromptMode::Full,
        });
        Ok(MountedChessCliSnapshot {
            epoch: Self::frame_epoch(&current.frame),
            turn_id: current.turn_id.clone(),
            view: self.host.rendered_view()?,
            prompt: self.host.rendered_user(receipt)?,
            delivery_receipt: receipt.as_str().to_owned(),
            actionable: current.frame.action_ticket().is_some(),
            action_handle,
            prompt_mode,
        })
    }

    fn call_id(frame_index: u64) -> anyhow::Result<DurableCallId> {
        Ok(DurableCallId::new(format!(
            "agentview-cli-chess-call-{frame_index}"
        ))?)
    }

    fn input_id(frame_index: u64) -> anyhow::Result<DurableCallInputId> {
        Ok(DurableCallInputId::new(format!(
            "agentview-cli-chess-input-{frame_index}"
        ))?)
    }

    fn turn_id(frame_index: u64) -> String {
        format!("turn-{frame_index}")
    }

    fn frame_epoch(frame: &ExternalUserFrame) -> u64 {
        let turn_index = match frame {
            ExternalUserFrame::Actionable(ticket) => ticket.token().turn_index(),
            ExternalUserFrame::Passive(presentation) => presentation.turn_index(),
            _ => unreachable!("the mounted controller only constructs known chess frame kinds"),
        };
        turn_index.saturating_sub(1)
    }

    fn reply_id(
        receipt: &ExternalUserDeliveryReceipt,
        command: &str,
    ) -> anyhow::Result<ExternalReplyId> {
        let digest = Sha256::digest(format!("{}\0{command}", receipt.as_str()).as_bytes());
        let suffix = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(ExternalReplyId::new(format!(
            "agentview-cli-chess-reply-{suffix}"
        ))?)
    }
}

impl InMemoryChessHost {
    fn rendered_view(&self) -> anyhow::Result<String> {
        let view = ChessView::collect(&self.source.snapshot());
        let document = Document::from_xml(view.build_root()?);
        Ok(agentview::pom_renderer::render_pom_document(
            &agentview::pom_resolution::resolve_system_document(document),
        )?)
    }
}

async fn next_cli_command<R>(lines: &mut Lines<R>) -> anyhow::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        print!("chess> ");
        std::io::stdout().flush()?;
        let Some(line) = lines.next_line().await? else {
            return Ok(None);
        };
        let line = line.trim();
        if line.eq_ignore_ascii_case("quit") || line.eq_ignore_ascii_case("exit") {
            return Ok(None);
        }
        if !line.is_empty() {
            return Ok(Some(line.to_owned()));
        }
    }
}

fn parse_ack_command(command: &str, expected_handle: &str) -> anyhow::Result<()> {
    let expected = format!("agentview chess ack {expected_handle}");
    anyhow::ensure!(command == expected, "expected `{expected}`");
    Ok(())
}

fn parse_act_command(command: &str, expected_handle: &str) -> anyhow::Result<String> {
    let arguments = command
        .strip_prefix("agentview chess act ")
        .ok_or_else(|| anyhow::anyhow!("expected `agentview chess act <handle> <raw-xml>`"))?;
    let split = arguments
        .find(char::is_whitespace)
        .ok_or_else(|| anyhow::anyhow!("missing raw XML reply after the action handle"))?;
    let handle = &arguments[..split];
    anyhow::ensure!(
        handle == expected_handle,
        "stale Chess action handle `{handle}`; current handle is `{expected_handle}`"
    );

    let raw_reply = arguments[split..].trim();
    let raw_reply = if let Some(inner) = raw_reply
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    {
        inner
    } else if raw_reply.starts_with('\'') || raw_reply.ends_with('\'') {
        anyhow::bail!("raw XML must be unquoted or enclosed by matching single quotes");
    } else {
        raw_reply
    };
    anyhow::ensure!(!raw_reply.is_empty(), "raw XML reply must not be empty");
    Ok(raw_reply.to_owned())
}

fn max_player_moves() -> anyhow::Result<Option<usize>> {
    let Some(value) = std::env::var_os("AGENTVIEW_CHESS_MAX_PLAYER_MOVES") else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    let parsed = value.parse::<usize>()?;
    anyhow::ensure!(
        parsed > 0,
        "AGENTVIEW_CHESS_MAX_PLAYER_MOVES must be positive"
    );
    Ok(Some(parsed))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let host = Arc::new(InMemoryChessHost::new());
    let session = DurableSessionId::new("example/chess-external/session-1")?;
    let opened =
        MountedExternalController::open(chess_definition(), Arc::clone(&host), session).await?;
    println!(
        "SYSTEM (host-owned delivery {})\n{}",
        opened.system_delivery_receipt(),
        host.delivered_system()
            .expect("the local port records System before activation")
    );
    let controller = opened.into_controller();

    let mut frame_sequence = 1_u64;
    let mut user_index = 1_u64;
    let mut completed_player_moves = 0_usize;
    let move_limit = max_player_moves()?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut current = frame(
        controller
            .observe(
                DurableCallId::new("chess-call-1")?,
                DurableCallInputId::new("chess-input-1")?,
                host.capture("Choose white's next move.", "chess-turn-1"),
            )
            .await?,
    )?;
    let mut current_was_printed = false;

    loop {
        if !current_was_printed {
            println!(
                "\nUSER {user_index}\n{}",
                host.rendered_user(current.delivery_receipt())?
            );
        }
        let action = match &current {
            ExternalUserFrame::Actionable(ticket) => ticket.clone(),
            ExternalUserFrame::Passive(_) => break,
            _ => anyhow::bail!("unsupported external chess frame"),
        };
        let action_handle = action.delivery_receipt().as_str();
        println!("\nACTION HANDLE {action_handle}");
        println!("ACK WITH: agentview chess ack {action_handle}");
        loop {
            let Some(command) = next_cli_command(&mut lines).await? else {
                controller.cancel(action.token()).await?;
                println!("\nCLI CLOSED");
                return Ok(());
            };
            match parse_ack_command(&command, action_handle) {
                Ok(()) => break,
                Err(error) => eprintln!("CLI REJECTED: {error}"),
            }
        }
        controller
            .acknowledge_user_delivery(action.delivery_receipt())
            .await?;
        println!("ACT WITH: agentview chess act {action_handle} '<move uci=\"e2e4\" />'");

        let mut reply_attempt = 0_u64;
        let committed = loop {
            let Some(command) = next_cli_command(&mut lines).await? else {
                controller.cancel(action.token()).await?;
                println!("\nCLI CLOSED");
                return Ok(());
            };
            let raw_reply = match parse_act_command(&command, action_handle) {
                Ok(raw_reply) => raw_reply,
                Err(error) => {
                    eprintln!("CLI REJECTED: {error}");
                    continue;
                }
            };
            reply_attempt += 1;
            let result = controller
                .act(
                    action.token(),
                    ExternalReplyId::new(format!("chess-reply-{user_index}-{reply_attempt}"))?,
                    ControlReply::text(raw_reply),
                )
                .await;
            match result {
                Ok(agentview::component::advanced::external::ExternalActOutcome::Applied(
                    commit,
                ))
                | Ok(agentview::component::advanced::external::ExternalActOutcome::Replayed(
                    commit,
                )) => break commit,
                Ok(
                    agentview::component::advanced::external::ExternalActOutcome::InvalidReply {
                        diagnostic,
                    },
                ) => eprintln!("REPLY REJECTED: {diagnostic}"),
                Ok(agentview::component::advanced::external::ExternalActOutcome::SourceStale {
                    actual,
                    ..
                }) => {
                    controller.cancel(action.token()).await?;
                    anyhow::bail!("chess position changed while awaiting a reply: {actual}");
                }
                Ok(
                    agentview::component::advanced::external::ExternalActOutcome::Indeterminate {
                        identity,
                    },
                ) => anyhow::bail!("chess commit requires recovery: {identity:?}"),
                Err(
                    agentview::component::advanced::external::MountedExternalControllerError::Port(
                        error,
                    ),
                ) => eprintln!("ACTION REJECTED: {error}"),
                Err(error) => return Err(error.into()),
                Ok(outcome) => anyhow::bail!("unsupported external action outcome: {outcome:?}"),
            }
        };
        completed_player_moves += 1;
        println!("\nPLAYER COMMITTED at {}", committed.wake_cursor());

        frame_sequence += 1;
        user_index += 1;
        let waiting = presentation(
            controller
                .observe(
                    DurableCallId::new(format!("chess-call-{frame_sequence}"))?,
                    DurableCallInputId::new(format!("chess-input-{frame_sequence}"))?,
                    host.capture(
                        "Wait for the engine reply.",
                        format!("chess-turn-{frame_sequence}"),
                    ),
                )
                .await?,
        )?;
        println!(
            "\nUSER {user_index} (waiting for engine)\n{}",
            host.rendered_user(waiting.delivery_receipt())?
        );

        let engine = StockfishEngine::new(
            std::env::var("AGENTVIEW_STOCKFISH_BIN").unwrap_or_else(|_| "stockfish".to_owned()),
        );
        let engine_task = tokio::spawn({
            let host = Arc::clone(&host);
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                host.apply_engine_move(&engine).await
            }
        });

        frame_sequence += 1;
        user_index += 1;
        let hooked = controller
            .hook(
                committed.wake_cursor(),
                DurableCallId::new(format!("chess-call-{frame_sequence}"))?,
                DurableCallInputId::new(format!("chess-input-{frame_sequence}"))?,
                || {
                    host.capture(
                        host.next_player_task(),
                        format!("chess-turn-{frame_sequence}"),
                    )
                },
            )
            .await?;
        let next = frame(hooked.into_observation())?;
        // The recovery/successor frame is visible before the engine task result
        // is reported. This prevents an undiscoverable durable frame on error.
        println!(
            "\nUSER {user_index}\n{}",
            host.rendered_user(next.delivery_receipt())?
        );

        match engine_task.await {
            Ok(Ok(uci)) => println!("\nENGINE COMMITTED {uci}"),
            Ok(Err(error)) => {
                println!("\nENGINE FAILED {error}");
                break;
            }
            Err(error) => {
                println!("\nENGINE TASK FAILED {error}");
                break;
            }
        }

        if move_limit.is_some_and(|limit| completed_player_moves >= limit) {
            if let ExternalUserFrame::Actionable(ticket) = &next {
                controller.cancel(ticket.token()).await?;
            }
            break;
        }
        current = next;
        current_was_printed = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentview::{
        component::advanced::external::ExternalReplyContract, pom_renderer::render_pom_document,
        pom_resolution::resolve_system_document, AgentView,
    };
    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    fn mock_stockfish_script(best_move: &str) -> (std::path::PathBuf, String) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "agentview-mounted-mock-stockfish-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("stockfish");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    uci) echo "id name mounted-mockfish"; echo "uciok" ;;
    isready) echo "readyok" ;;
    go*) echo "bestmove {best_move}"; exit 0 ;;
    quit) exit 0 ;;
  esac
done
"#
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        (directory, script.to_string_lossy().into_owned())
    }

    #[test]
    fn chess_reply_contract_decodes_the_advertised_semantic_xml() {
        let action = ChessReplyContract::new()
            .decode(&ControlReply::text("<move uci=\"e7e8q\" />"))
            .unwrap();

        assert_eq!(action.uci, "e7e8q");
    }

    #[test]
    fn chess_reply_contract_rejects_unadvertised_reply_shapes() {
        let contract = ChessReplyContract::new();
        let replies = [
            ControlReply::structured(serde_json::json!({ "uci": "e2e4" })),
            ControlReply::text("e2e4"),
            ControlReply::text("<move uci=\"e2e4\" note=\"not-allowed\" />"),
            ControlReply::text("<reply uci=\"e2e4\" />"),
        ];

        for reply in replies {
            assert!(
                contract.decode(&reply).is_err(),
                "reply must not decode outside the System grammar: {reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn external_harness_binding_preserves_the_shared_system_bytes() {
        let expected = render_pom_document(&resolve_system_document(
            chess_support::ChessSystemPromptView::default()
                .build_root()
                .unwrap(),
        ))
        .unwrap();
        let host = Arc::new(InMemoryChessHost::new());
        let session = DurableSessionId::new("example/chess-external/system-proof").unwrap();

        let opened =
            MountedExternalController::open(chess_definition(), Arc::clone(&host), session)
                .await
                .unwrap();

        assert_eq!(
            opened.system_delivery_receipt().as_str(),
            "chess-system-delivery/1"
        );
        assert_eq!(host.delivered_system().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn direct_runner_uses_canonical_handle_and_raw_xml_commands() {
        let handle = "sha256:v1:test-action-handle";
        assert!(parse_ack_command(&format!("agentview chess ack {handle}"), handle).is_ok());
        assert!(parse_ack_command("agentview chess ack stale", handle).is_err());

        assert_eq!(
            parse_act_command(
                &format!("agentview chess act {handle} '<move uci=\"e2e4\" />'"),
                handle,
            )
            .unwrap(),
            "<move uci=\"e2e4\" />"
        );
        assert_eq!(
            parse_act_command(
                &format!("agentview chess act {handle} <move uci=\"d2d4\" />"),
                handle,
            )
            .unwrap(),
            "<move uci=\"d2d4\" />"
        );
        assert!(
            parse_act_command("agentview chess act stale '<move uci=\"e2e4\" />'", handle,)
                .is_err()
        );
        assert!(parse_act_command(
            "agentview chess act --piece P --from e2 --to e4 --uci e2e4",
            handle,
        )
        .is_err());
    }

    #[test]
    fn external_turn_user_pom_does_not_repeat_system_policy_or_reply_grammar() {
        let source = ChessGameSource::new();
        let context = ChessView::collect(&source.snapshot());
        let user =
            chess_user_document(context, "Choose white's next move.", "chess-turn-1").unwrap();
        let rendered = render_pom_document(&resolve_system_document(user)).unwrap();

        assert!(rendered.contains("<chess_task>"));
        assert!(rendered.contains("<active_turn_id>chess-turn-1</active_turn_id>"));
        assert!(!rendered.contains("<reasoning_policy>"));
        assert!(!rendered.contains("<reply_contract"));
    }

    #[tokio::test]
    async fn cancelling_an_unacknowledged_delivery_tombstones_it_and_forces_a_full_successor() {
        let host = Arc::new(InMemoryChessHost::new());
        let opened = MountedExternalController::open(
            chess_definition(),
            Arc::clone(&host),
            DurableSessionId::new("example/chess-external/cancel-proof").unwrap(),
        )
        .await
        .unwrap();
        let controller = opened.into_controller();

        let first = ticket(
            controller
                .observe(
                    DurableCallId::new("cancel-call-1").unwrap(),
                    DurableCallInputId::new("cancel-input-1").unwrap(),
                    host.capture("Choose white's next move.", "cancel-turn-1"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let first_receipt = first.delivery_receipt().clone();
        controller.cancel(first.token()).await.unwrap();
        assert!(host.user_delivery(&first_receipt).unwrap().tombstoned);
        assert!(host.rendered_user(&first_receipt).is_err());

        let successor = ticket(
            controller
                .observe(
                    DurableCallId::new("cancel-call-2").unwrap(),
                    DurableCallInputId::new("cancel-input-2").unwrap(),
                    host.capture("Choose white's next move.", "cancel-turn-2"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let successor_delivery = host.user_delivery(successor.delivery_receipt()).unwrap();
        assert!(successor_delivery.base_receipt.is_none());
        assert!(!successor_delivery
            .rendered_user
            .contains("rendering_mode=\"delta\""));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mounted_replica_preserves_three_visible_chess_states() {
        let host = Arc::new(InMemoryChessHost::new());
        let opened = MountedExternalController::open(
            chess_definition(),
            Arc::clone(&host),
            DurableSessionId::new("example/chess-external/replica-proof").unwrap(),
        )
        .await
        .unwrap();
        let controller = opened.into_controller();

        let first = ticket(
            controller
                .observe(
                    DurableCallId::new("replica-call-1").unwrap(),
                    DurableCallInputId::new("replica-input-1").unwrap(),
                    host.capture("Choose white's next move.", "replica-turn-1"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let first_delivery = host.user_delivery(first.delivery_receipt()).unwrap();
        assert_eq!(first_delivery.lane, ExternalUserDeliveryLane::Prompt);
        assert_eq!(
            first_delivery.system_delivery_receipt.as_str(),
            "chess-system-delivery/1"
        );
        assert!(first_delivery.base_receipt.is_none());
        assert!(!first_delivery.acknowledged);
        let first_user = first_delivery.rendered_user;
        assert!(first_user.contains("<side_to_move>white</side_to_move>"));
        assert!(first_user.contains("<move_history />"));
        assert!(!first_user.contains("rendering_mode=\"delta\""));

        controller
            .acknowledge_user_delivery(first.delivery_receipt())
            .await
            .unwrap();
        assert!(
            host.user_delivery(first.delivery_receipt())
                .unwrap()
                .acknowledged
        );

        let illegal = controller
            .act(
                first.token(),
                ExternalReplyId::new("replica-illegal-reply").unwrap(),
                ControlReply::text("<move uci=\"e2e5\" />"),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            illegal,
            agentview::component::advanced::external::MountedExternalControllerError::Port(_)
        ));

        let committed = match controller
            .act(
                first.token(),
                ExternalReplyId::new("replica-reply-1").unwrap(),
                ControlReply::text("<move uci=\"e2e4\" />"),
            )
            .await
            .unwrap()
        {
            agentview::component::advanced::external::ExternalActOutcome::Applied(commit) => commit,
            outcome => panic!("expected mounted player commit, got {outcome:?}"),
        };
        assert!(!host.player_action_available());

        let waiting = presentation(
            controller
                .observe(
                    DurableCallId::new("replica-call-2").unwrap(),
                    DurableCallInputId::new("replica-input-2").unwrap(),
                    host.capture("Wait for the engine reply.", "replica-turn-2"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let waiting_delivery = host.user_delivery(waiting.delivery_receipt()).unwrap();
        assert_eq!(
            waiting_delivery.lane,
            ExternalUserDeliveryLane::Presentation
        );
        assert!(waiting_delivery.base_receipt.is_none());
        assert!(!waiting_delivery.acknowledged);
        let waiting_user = waiting_delivery.rendered_user;
        assert!(waiting_user.contains("<side_to_move>black</side_to_move>"));
        assert!(waiting_user.contains("<move>e2e4</move>"));
        assert!(waiting_user.contains("<pending>true</pending>"));
        assert!(!waiting_user.contains("rendering_mode=\"delta\""));
        assert_eq!(waiting.turn_index(), 2);

        let (directory, script) = mock_stockfish_script("e7e5");
        let engine_task = tokio::spawn({
            let host = Arc::clone(&host);
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                host.apply_engine_move(&StockfishEngine::new(script)).await
            }
        });
        let wake = controller
            .wait_for_wake(committed.wake_cursor())
            .await
            .unwrap();
        assert_eq!(wake.as_str(), "chess-wake-2");
        let third = ticket(
            controller
                .hook(
                    committed.wake_cursor(),
                    DurableCallId::new("replica-call-3").unwrap(),
                    DurableCallInputId::new("replica-input-3").unwrap(),
                    || host.capture("Choose white's next move.", "replica-turn-3"),
                )
                .await
                .unwrap()
                .into_observation(),
        )
        .unwrap();
        assert_eq!(engine_task.await.unwrap().unwrap(), "e7e5");
        let third_delivery = host.user_delivery(third.delivery_receipt()).unwrap();
        assert_eq!(third_delivery.lane, ExternalUserDeliveryLane::Prompt);
        assert_eq!(
            third_delivery.base_receipt.as_ref(),
            Some(first.delivery_receipt())
        );
        assert!(!third_delivery.acknowledged);
        let third_user = third_delivery.rendered_user;
        assert!(third_user.contains("<side_to_move>white</side_to_move>"));
        assert!(third_user.contains("<move>e2e4</move>"));
        assert!(third_user.contains("<move>e7e5</move>"));
        assert!(third_user.contains("<pending>false</pending>"));
        assert!(third_user.contains("<last_move>e7e5</last_move>"));
        assert!(third_user.contains("rendering_mode=\"delta\""));
        assert!(host.player_action_available());

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn consecutive_actionable_prompts_advance_the_acknowledged_delta_base() {
        let host = Arc::new(InMemoryChessHost::new());
        let opened = MountedExternalController::open(
            chess_definition(),
            Arc::clone(&host),
            DurableSessionId::new("example/chess-external/delta-chain-proof").unwrap(),
        )
        .await
        .unwrap();
        let controller = opened.into_controller();

        let first = ticket(
            controller
                .observe(
                    DurableCallId::new("delta-chain-call-1").unwrap(),
                    DurableCallInputId::new("delta-chain-input-1").unwrap(),
                    host.capture("Choose white's next move.", "delta-chain-turn-1"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        controller
            .acknowledge_user_delivery(first.delivery_receipt())
            .await
            .unwrap();
        let first_commit = match controller
            .act(
                first.token(),
                ExternalReplyId::new("delta-chain-reply-1").unwrap(),
                ControlReply::text("<move uci=\"e2e4\" />"),
            )
            .await
            .unwrap()
        {
            agentview::component::advanced::external::ExternalActOutcome::Applied(commit) => commit,
            outcome => panic!("expected first player commit, got {outcome:?}"),
        };
        let first_waiting = presentation(
            controller
                .observe(
                    DurableCallId::new("delta-chain-call-2").unwrap(),
                    DurableCallInputId::new("delta-chain-input-2").unwrap(),
                    host.capture("Wait for the engine reply.", "delta-chain-turn-2"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            host.user_delivery(first_waiting.delivery_receipt())
                .unwrap()
                .lane,
            ExternalUserDeliveryLane::Presentation
        );

        let (first_directory, first_script) = mock_stockfish_script("e7e5");
        let first_engine = tokio::spawn({
            let host = Arc::clone(&host);
            async move {
                host.apply_engine_move(&StockfishEngine::new(first_script))
                    .await
            }
        });
        let third = ticket(
            controller
                .hook(
                    first_commit.wake_cursor(),
                    DurableCallId::new("delta-chain-call-3").unwrap(),
                    DurableCallInputId::new("delta-chain-input-3").unwrap(),
                    || host.capture("Choose white's next move.", "delta-chain-turn-3"),
                )
                .await
                .unwrap()
                .into_observation(),
        )
        .unwrap();
        assert_eq!(first_engine.await.unwrap().unwrap(), "e7e5");
        let third_delivery = host.user_delivery(third.delivery_receipt()).unwrap();
        assert_eq!(
            third_delivery.base_receipt.as_ref(),
            Some(first.delivery_receipt())
        );
        assert!(third_delivery
            .rendered_user
            .contains("rendering_mode=\"delta\""));

        controller
            .acknowledge_user_delivery(third.delivery_receipt())
            .await
            .unwrap();
        let third_commit = match controller
            .act(
                third.token(),
                ExternalReplyId::new("delta-chain-reply-3").unwrap(),
                ControlReply::text("<move uci=\"g1f3\" />"),
            )
            .await
            .unwrap()
        {
            agentview::component::advanced::external::ExternalActOutcome::Applied(commit) => commit,
            outcome => panic!("expected second player commit, got {outcome:?}"),
        };
        let second_waiting = presentation(
            controller
                .observe(
                    DurableCallId::new("delta-chain-call-4").unwrap(),
                    DurableCallInputId::new("delta-chain-input-4").unwrap(),
                    host.capture("Wait for the engine reply.", "delta-chain-turn-4"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            host.user_delivery(second_waiting.delivery_receipt())
                .unwrap()
                .lane,
            ExternalUserDeliveryLane::Presentation
        );

        let (second_directory, second_script) = mock_stockfish_script("b8c6");
        let second_engine = tokio::spawn({
            let host = Arc::clone(&host);
            async move {
                host.apply_engine_move(&StockfishEngine::new(second_script))
                    .await
            }
        });
        let fifth = ticket(
            controller
                .hook(
                    third_commit.wake_cursor(),
                    DurableCallId::new("delta-chain-call-5").unwrap(),
                    DurableCallInputId::new("delta-chain-input-5").unwrap(),
                    || host.capture("Choose white's next move.", "delta-chain-turn-5"),
                )
                .await
                .unwrap()
                .into_observation(),
        )
        .unwrap();
        assert_eq!(second_engine.await.unwrap().unwrap(), "b8c6");
        let fifth_delivery = host.user_delivery(fifth.delivery_receipt()).unwrap();
        assert_eq!(
            fifth_delivery.base_receipt.as_ref(),
            Some(third.delivery_receipt())
        );
        assert_ne!(
            fifth_delivery.base_receipt.as_ref(),
            Some(first.delivery_receipt())
        );
        assert!(fifth_delivery
            .rendered_user
            .contains("rendering_mode=\"delta\""));
        assert!(fifth_delivery.rendered_user.contains("g1f3"));
        assert!(fifth_delivery.rendered_user.contains("b8c6"));

        fs::remove_dir_all(first_directory).unwrap();
        fs::remove_dir_all(second_directory).unwrap();
    }

    #[tokio::test]
    async fn engine_failure_delivers_the_recovery_frame_before_its_error_is_observed() {
        let host = Arc::new(InMemoryChessHost::new());
        let opened = MountedExternalController::open(
            chess_definition(),
            Arc::clone(&host),
            DurableSessionId::new("example/chess-external/failure-proof").unwrap(),
        )
        .await
        .unwrap();
        let controller = opened.into_controller();

        let first = ticket(
            controller
                .observe(
                    DurableCallId::new("failure-call-1").unwrap(),
                    DurableCallInputId::new("failure-input-1").unwrap(),
                    host.capture("Choose white's next move.", "failure-turn-1"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let first_delivery = host.user_delivery(first.delivery_receipt()).unwrap();
        assert_eq!(first_delivery.lane, ExternalUserDeliveryLane::Prompt);
        assert!(first_delivery.base_receipt.is_none());
        assert!(!first_delivery.acknowledged);
        controller
            .acknowledge_user_delivery(first.delivery_receipt())
            .await
            .unwrap();
        assert!(
            host.user_delivery(first.delivery_receipt())
                .unwrap()
                .acknowledged
        );

        let committed = match controller
            .act(
                first.token(),
                ExternalReplyId::new("failure-reply-1").unwrap(),
                ControlReply::text("<move uci=\"e2e4\" />"),
            )
            .await
            .unwrap()
        {
            agentview::component::advanced::external::ExternalActOutcome::Applied(commit) => commit,
            outcome => panic!("expected mounted player commit, got {outcome:?}"),
        };
        assert!(!host.player_action_available());

        let waiting = presentation(
            controller
                .observe(
                    DurableCallId::new("failure-call-2").unwrap(),
                    DurableCallInputId::new("failure-input-2").unwrap(),
                    host.capture("Wait for the engine reply.", "failure-turn-2"),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let waiting_delivery = host.user_delivery(waiting.delivery_receipt()).unwrap();
        assert_eq!(
            waiting_delivery.lane,
            ExternalUserDeliveryLane::Presentation
        );
        assert!(!waiting_delivery.acknowledged);
        assert!(waiting_delivery
            .rendered_user
            .contains("<pending>true</pending>"));

        let missing_stockfish = std::env::temp_dir().join(format!(
            "agentview-no-stockfish-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        assert!(!missing_stockfish.exists());
        let engine_task = tokio::spawn({
            let host = Arc::clone(&host);
            let engine = StockfishEngine::new(missing_stockfish.to_string_lossy().into_owned());
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                host.apply_engine_move(&engine).await
            }
        });

        // The hook returns the durable recovery frame before the caller
        // observes the task result. This is the ordering `main` relies on.
        let third = presentation(
            controller
                .hook(
                    committed.wake_cursor(),
                    DurableCallId::new("failure-call-3").unwrap(),
                    DurableCallInputId::new("failure-input-3").unwrap(),
                    || host.capture(host.next_player_task(), "failure-turn-3"),
                )
                .await
                .unwrap()
                .into_observation(),
        )
        .unwrap();
        assert_eq!(third.turn_index(), 3);
        let third_delivery = host.user_delivery(third.delivery_receipt()).unwrap();
        assert_eq!(third_delivery.lane, ExternalUserDeliveryLane::Presentation);
        assert!(third_delivery.base_receipt.is_none());
        assert!(!third_delivery.acknowledged);
        assert!(!third_delivery
            .rendered_user
            .contains("rendering_mode=\"delta\""));
        assert!(third_delivery
            .rendered_user
            .contains("<pending>false</pending>"));
        assert!(third_delivery.rendered_user.contains("<last_error>"));
        assert!(third_delivery
            .rendered_user
            .contains("stockfish failed: failed to start stockfish"));
        assert!(!host.player_action_available());

        let failure = engine_task.await.unwrap().unwrap_err();
        assert!(failure.to_string().contains("failed to start stockfish"));
    }
}
