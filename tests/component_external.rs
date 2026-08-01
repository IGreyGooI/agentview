//! Black-box proof for the mounted external action boundary.
//!
//! The in-memory port deliberately owns a fake domain row, opaque AgentView
//! state, reply resolution ledger, immutable User-delivery outbox, and wake
//! cursor under one mutex. It is a small stand-in for the database transaction
//! a real host must implement.

use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        advanced::{
            external::{
                ExternalActionLifecycle, ExternalActionRoute, ExternalActionTicket,
                ExternalActionToken, ExternalCommitIdentity, ExternalCommitResolution,
                ExternalEpochAcquireRequest, ExternalEpochActivation, ExternalEpochAdmission,
                ExternalEpochArtifact, ExternalEpochComplete, ExternalEpochLease,
                ExternalFingerprint, ExternalObservation, ExternalObserveOutcome,
                ExternalRecoveryOutcome, ExternalReplyContract, ExternalReplyContractId,
                ExternalReplyId, ExternalSourceRevision, ExternalStateMutation,
                ExternalStateSnapshot, ExternalStateWrite, ExternalStateWriteOutcome,
                ExternalSystemDeliveryReceipt, ExternalUserDeliveryAckOutcome,
                ExternalUserDeliveryCandidate, ExternalUserDeliveryLane,
                ExternalUserDeliveryReceipt, ExternalUserFrame, ExternalUserResyncOutcome,
                ExternalWakeCursor, MountedExternalController, MountedExternalControllerError,
                MountedExternalPort,
            },
            persistence::{MountedStateBlob, MountedStateGeneration},
        },
        prelude::*,
        DurableCallId, DurableCallInputId, DurableSessionId, MountedExternalHarnessDefinition,
    },
    control::ControlReply,
    AgentView,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Notify;

#[derive(Clone)]
struct ExternalProps {
    value: String,
    passive: bool,
}

#[derive(AgentView)]
#[agent_view(document)]
struct ExternalSystem {
    #[view(paragraph)]
    instruction: &'static str,
}

#[derive(AgentView)]
#[agent_view(document)]
struct ExternalReplyGrammar {
    #[view(paragraph)]
    instruction: &'static str,
}

#[derive(AgentView)]
#[agent_view(kind = "external_context")]
struct ExternalContext {
    #[view(diff)]
    value: String,
}

#[derive(AgentView)]
#[agent_view(document)]
struct ExternalUser {
    #[view(name = "external_context", diff)]
    context: ExternalContext,
}

#[view(component)]
fn external_harness() -> ExternalPromptComponent<ExternalProps> {
    try_external_prompt_component(
        ExternalSystem {
            instruction: "Apply one typed external action.",
        },
        |props: &ExternalProps| {
            let user = ExternalUser {
                context: ExternalContext {
                    value: props.value.clone(),
                },
            };
            if props.passive {
                ExternalUserView::passive_presentation(user)
            } else {
                ExternalUserView::actionable(user)
            }
        },
    )
}

fn definition(
    contract: TestContract,
) -> MountedExternalHarnessDefinition<ExternalProps, TestContract> {
    ExternalReply::new(contract).into_harness(
        external_harness(),
        EpochContractId::new("test/external/v1").unwrap(),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum TestAction {
    Increment,
}

#[derive(Clone)]
struct TestContract {
    route: ExternalActionRoute,
    id: ExternalReplyContractId,
}

impl TestContract {
    fn v1() -> Self {
        Self {
            route: ExternalActionRoute::new("test.increment").unwrap(),
            id: ExternalReplyContractId::new("test.increment.reply/v1").unwrap(),
        }
    }

    fn v2() -> Self {
        Self {
            route: ExternalActionRoute::new("test.increment").unwrap(),
            id: ExternalReplyContractId::new("test.increment.reply/v2").unwrap(),
        }
    }
}

impl ExternalReplyContract for TestContract {
    type Action = TestAction;
    type Diagnostic = String;
    type System = ExternalReplyGrammar;

    fn system(&self) -> Self::System {
        ExternalReplyGrammar {
            instruction: "Reply only with {\"action\":\"increment\"}.",
        }
    }

    fn route(&self) -> &ExternalActionRoute {
        &self.route
    }

    fn contract_id(&self) -> &ExternalReplyContractId {
        &self.id
    }

    fn decode(&self, reply: &ControlReply) -> Result<Self::Action, Self::Diagnostic> {
        match reply
            .as_structured()
            .and_then(|value| value.get("action"))
            .and_then(serde_json::Value::as_str)
        {
            Some("increment") => Ok(TestAction::Increment),
            _ => Err("expected {\"action\":\"increment\"}".to_owned()),
        }
    }
}

#[derive(Default)]
struct CommitGate {
    started: Notify,
    allow: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleGenerationMutation {
    Publish,
    PrepareReply,
    Commit,
    Cancel,
    MarkRecovery,
}

impl StaleGenerationMutation {
    fn matches(self, mutation: &ExternalStateMutation<'_, TestAction>) -> bool {
        matches!(
            (self, mutation),
            (
                Self::Publish,
                ExternalStateMutation::PublishActionable { .. }
                    | ExternalStateMutation::PublishPassive { .. }
            ) | (
                Self::PrepareReply,
                ExternalStateMutation::PrepareReply { .. }
            ) | (Self::Commit, ExternalStateMutation::Commit { .. })
                | (Self::Cancel, ExternalStateMutation::Cancel { .. })
                | (
                    Self::MarkRecovery,
                    ExternalStateMutation::MarkRecovery { .. }
                )
        )
    }
}

enum EpochState {
    Empty,
    Creating(ExternalEpochLease),
    Active {
        artifact: ExternalEpochArtifact,
        system_delivery_receipt: ExternalSystemDeliveryReceipt,
    },
}

/// Host-owned immutable publication record. The prompt metadata and bytes are
/// fixed at insertion; acknowledgement and cancellation are delivery state.
#[derive(Debug, Clone)]
struct TestUserDelivery {
    lane: ExternalUserDeliveryLane,
    system_delivery_receipt: ExternalSystemDeliveryReceipt,
    base_receipt: Option<ExternalUserDeliveryReceipt>,
    user_fingerprint: ExternalFingerprint,
    rendered_user: String,
    action_token: Option<ExternalActionToken>,
    acknowledged: bool,
    cancelled: bool,
}

impl TestUserDelivery {
    fn from_candidate(
        candidate: &ExternalUserDeliveryCandidate,
        action_token: Option<&ExternalActionToken>,
    ) -> Self {
        Self {
            lane: candidate.lane(),
            system_delivery_receipt: candidate.system_delivery_receipt().clone(),
            base_receipt: candidate.base_receipt().cloned(),
            user_fingerprint: candidate.user_fingerprint().clone(),
            rendered_user: candidate.rendered_user().to_owned(),
            action_token: action_token.cloned(),
            acknowledged: false,
            cancelled: false,
        }
    }

    fn matches_candidate(
        &self,
        candidate: &ExternalUserDeliveryCandidate,
        action_token: Option<&ExternalActionToken>,
    ) -> bool {
        self.lane == candidate.lane()
            && self.system_delivery_receipt == *candidate.system_delivery_receipt()
            && self.base_receipt.as_ref() == candidate.base_receipt()
            && self.user_fingerprint == *candidate.user_fingerprint()
            && self.rendered_user == candidate.rendered_user()
            && self.action_token.as_ref() == action_token
    }
}

struct PortState {
    epoch: EpochState,
    state: Option<MountedStateBlob>,
    generation: Option<MountedStateGeneration>,
    next_generation: u64,
    wake: u64,
    source_revision: ExternalSourceRevision,
    domain_value: u64,
    outbox_count: u64,
    system_installs: u64,
    delivered_system: Option<String>,
    user_outbox: BTreeMap<String, TestUserDelivery>,
    next_indeterminate: Option<ExternalCommitResolution>,
    next_stale_generation: Option<StaleGenerationMutation>,
    resolutions: BTreeMap<String, ExternalCommitResolution>,
    commit_gate: Option<Arc<CommitGate>>,
}

impl PortState {
    fn new() -> Self {
        Self {
            epoch: EpochState::Empty,
            state: None,
            generation: None,
            next_generation: 0,
            wake: 0,
            source_revision: ExternalSourceRevision::new("source-1").unwrap(),
            domain_value: 0,
            outbox_count: 0,
            system_installs: 0,
            delivered_system: None,
            user_outbox: BTreeMap::new(),
            next_indeterminate: None,
            next_stale_generation: None,
            resolutions: BTreeMap::new(),
            commit_gate: None,
        }
    }

    fn wake_cursor(&self) -> ExternalWakeCursor {
        ExternalWakeCursor::new(format!("wake-{}", self.wake)).unwrap()
    }

    fn snapshot(&self) -> ExternalStateSnapshot {
        match (&self.generation, &self.state) {
            (Some(generation), Some(state)) => ExternalStateSnapshot::present(
                generation.clone(),
                state.clone(),
                self.wake_cursor(),
            ),
            (None, None) => ExternalStateSnapshot::missing(self.wake_cursor()),
            _ => panic!("opaque state and generation must advance together"),
        }
    }

    fn next_generation(&mut self) -> MountedStateGeneration {
        self.next_generation += 1;
        MountedStateGeneration::new(format!("generation-{}", self.next_generation)).unwrap()
    }

    fn publish_user_delivery(
        &mut self,
        candidate: &ExternalUserDeliveryCandidate,
        action_token: Option<&ExternalActionToken>,
    ) {
        let active_system_receipt = match &self.epoch {
            EpochState::Active {
                system_delivery_receipt,
                ..
            } => system_delivery_receipt.clone(),
            _ => panic!("User delivery cannot publish before System activation"),
        };
        assert_eq!(
            candidate.system_delivery_receipt(),
            &active_system_receipt,
            "every User outbox row must depend on the active System receipt"
        );
        if candidate.lane() == ExternalUserDeliveryLane::Presentation {
            assert!(
                candidate.base_receipt().is_none(),
                "presentation deliveries must not consume the prompt baseline"
            );
        }
        if let Some(base_receipt) = candidate.base_receipt() {
            let base = self
                .user_outbox
                .get(base_receipt.as_str())
                .expect("a delta delivery must reference a retained outbox row");
            assert_eq!(base.lane, ExternalUserDeliveryLane::Prompt);
            assert!(
                base.acknowledged,
                "a delta delivery must reference an acknowledged prompt"
            );
            assert!(
                !base.cancelled,
                "a delta delivery must not reference a cancelled prompt"
            );
        }

        let key = candidate.receipt().as_str().to_owned();
        if let Some(existing) = self.user_outbox.get(&key) {
            assert!(
                existing.matches_candidate(candidate, action_token),
                "a User delivery receipt must always resolve to the same immutable row"
            );
            return;
        }
        self.user_outbox.insert(
            key,
            TestUserDelivery::from_candidate(candidate, action_token),
        );
    }

    fn acknowledge_user_delivery(&mut self, receipt: &ExternalUserDeliveryReceipt) {
        let delivery = self
            .user_outbox
            .get_mut(receipt.as_str())
            .expect("acknowledgement must reference a retained User outbox row");
        assert_eq!(delivery.lane, ExternalUserDeliveryLane::Prompt);
        assert!(
            !delivery.cancelled,
            "a cancelled User delivery cannot be acknowledged"
        );
        delivery.acknowledged = true;
    }

    fn cancel_user_delivery(
        &mut self,
        token: &ExternalActionToken,
        receipt: &ExternalUserDeliveryReceipt,
    ) {
        let delivery = self
            .user_outbox
            .get_mut(receipt.as_str())
            .expect("cancellation must reference the exact retained User delivery");
        assert_eq!(delivery.action_token.as_ref(), Some(token));
        if !delivery.acknowledged {
            assert!(
                !delivery.cancelled,
                "an unacknowledged User delivery must be tombstoned exactly once"
            );
            delivery.cancelled = true;
        }
    }
}

struct TestPort {
    state: Mutex<PortState>,
    wake: Notify,
}

impl TestPort {
    fn new() -> Self {
        Self {
            state: Mutex::new(PortState::new()),
            wake: Notify::new(),
        }
    }

    fn set_source_revision(&self, value: &str) {
        self.state.lock().unwrap().source_revision = ExternalSourceRevision::new(value).unwrap();
    }

    fn make_next_commit_indeterminate(&self, resolution: ExternalCommitResolution) {
        self.state.lock().unwrap().next_indeterminate = Some(resolution);
    }

    fn make_next_success_report_stale_generation(&self, mutation: StaleGenerationMutation) {
        self.state.lock().unwrap().next_stale_generation = Some(mutation);
    }

    fn block_next_commit(&self) -> Arc<CommitGate> {
        let gate = Arc::new(CommitGate::default());
        self.state.lock().unwrap().commit_gate = Some(Arc::clone(&gate));
        gate
    }

    fn domain_value(&self) -> u64 {
        self.state.lock().unwrap().domain_value
    }

    fn outbox_count(&self) -> u64 {
        self.state.lock().unwrap().outbox_count
    }

    fn system_installs(&self) -> u64 {
        self.state.lock().unwrap().system_installs
    }

    fn delivered_system(&self) -> Option<String> {
        self.state.lock().unwrap().delivered_system.clone()
    }

    fn user_delivery(&self, receipt: &ExternalUserDeliveryReceipt) -> Option<TestUserDelivery> {
        self.state
            .lock()
            .unwrap()
            .user_outbox
            .get(receipt.as_str())
            .cloned()
    }

    fn force_state_schema_version(&self, schema_version: u32) {
        let mut state = self.state.lock().unwrap();
        let current = state
            .state
            .as_ref()
            .expect("test port has initialized controller state");
        let mut encoded: serde_json::Value =
            serde_json::from_slice(current.as_bytes()).expect("controller state is JSON");
        encoded["schema_version"] = serde_json::Value::from(schema_version);
        state.state = Some(MountedStateBlob::new(
            serde_json::to_vec(&encoded).expect("controller state remains JSON"),
        ));
    }

    fn identity_key(identity: &ExternalCommitIdentity) -> String {
        format!("{identity:?}")
    }
}

#[async_trait::async_trait]
impl MountedExternalPort<TestAction> for TestPort {
    type Error = Infallible;

    async fn acquire_epoch(
        &self,
        request: ExternalEpochAcquireRequest<'_>,
    ) -> Result<ExternalEpochAdmission, Self::Error> {
        let mut state = self.state.lock().unwrap();
        match &state.epoch {
            EpochState::Empty => {
                let lease = ExternalEpochLease::new(
                    request.session_id().clone(),
                    request.epoch_contract_id().clone(),
                    1,
                    "lease-1",
                )
                .unwrap();
                state.epoch = EpochState::Creating(lease.clone());
                Ok(ExternalEpochAdmission::Create { lease })
            }
            EpochState::Creating(lease) => Ok(ExternalEpochAdmission::InFlight {
                generation: lease.generation(),
            }),
            EpochState::Active {
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
            EpochState::Active { artifact, .. } => Ok(ExternalEpochAdmission::ContractMismatch {
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
            EpochState::Creating(lease) => assert_eq!(lease, request.lease()),
            _ => panic!("complete_epoch requires the sole Create lease"),
        }
        assert_eq!(
            request.artifact().generation(),
            request.lease().generation()
        );
        assert!(!request.rendered_system().is_empty());
        let system_delivery_receipt =
            ExternalSystemDeliveryReceipt::new("test/external-system-delivery/1").unwrap();
        state.delivered_system = Some(request.rendered_system().to_owned());
        state.state = Some(request.initial_state().clone());
        state.generation = Some(state.next_generation());
        state.epoch = EpochState::Active {
            artifact: request.artifact().clone(),
            system_delivery_receipt: system_delivery_receipt.clone(),
        };
        state.system_installs += 1;
        Ok(ExternalEpochActivation::new(
            state.snapshot(),
            system_delivery_receipt,
        ))
    }

    async fn abandon_epoch(&self, lease: &ExternalEpochLease) -> Result<(), Self::Error> {
        let mut state = self.state.lock().unwrap();
        if matches!(&state.epoch, EpochState::Creating(current) if current == lease) {
            state.epoch = EpochState::Empty;
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
        request: ExternalStateWrite<'_, TestAction>,
    ) -> Result<ExternalStateWriteOutcome, Self::Error> {
        let gate = if matches!(request.mutation(), ExternalStateMutation::Commit { .. }) {
            self.state.lock().unwrap().commit_gate.take()
        } else {
            None
        };
        if let Some(gate) = gate {
            gate.started.notify_waiters();
            gate.allow.notified().await;
        }

        let mut state = self.state.lock().unwrap();
        if request.expected_generation() != state.generation.as_ref() {
            return Ok(ExternalStateWriteOutcome::Conflict {
                current: state.snapshot(),
            });
        }
        let mutation = request.mutation();
        if mutation
            .source_revision()
            .is_some_and(|revision| revision != &state.source_revision)
        {
            return Ok(ExternalStateWriteOutcome::SourceStale {
                current: state.snapshot(),
                actual: state.source_revision.clone(),
            });
        }

        if state
            .next_stale_generation
            .is_some_and(|target| target.matches(mutation))
        {
            state.next_stale_generation = None;
            return Ok(ExternalStateWriteOutcome::Committed {
                generation: request
                    .expected_generation()
                    .expect("controller writes always have a prior generation")
                    .clone(),
                wake_cursor: state.wake_cursor(),
            });
        }

        match mutation {
            ExternalStateMutation::PublishActionable {
                delivery, ticket, ..
            } => state.publish_user_delivery(delivery, Some(ticket.token())),
            ExternalStateMutation::PublishPassive { delivery, .. } => {
                state.publish_user_delivery(delivery, None)
            }
            ExternalStateMutation::AcknowledgeUserDelivery { receipt } => {
                state.acknowledge_user_delivery(receipt)
            }
            ExternalStateMutation::RequestUserDocumentResync => {}
            ExternalStateMutation::Cancel {
                token,
                delivery_receipt,
            } => state.cancel_user_delivery(token, delivery_receipt),
            ExternalStateMutation::PrepareReply { .. }
            | ExternalStateMutation::Commit { .. }
            | ExternalStateMutation::MarkRecovery { .. } => {}
            _ => unreachable!("test port must be updated for new external mutations"),
        }

        if let ExternalStateMutation::Commit { identity, action } = mutation {
            let key = Self::identity_key(identity);
            if let Some(resolution) = state.next_indeterminate.take() {
                state.resolutions.insert(key.clone(), resolution);
                if resolution != ExternalCommitResolution::Committed {
                    return Ok(ExternalStateWriteOutcome::Indeterminate);
                }
            }
            match action {
                TestAction::Increment => state.domain_value += 1,
            }
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

fn session() -> DurableSessionId {
    DurableSessionId::new("external-session").unwrap()
}

fn source(value: &str) -> ExternalSourceRevision {
    ExternalSourceRevision::new(value).unwrap()
}

fn observation(revision: &str, value: impl Into<String>) -> ExternalObservation<ExternalProps> {
    ExternalObservation::from_props(
        source(revision),
        ExternalProps {
            value: value.into(),
            passive: false,
        },
    )
}

fn passive_observation(
    revision: &str,
    value: impl Into<String>,
) -> ExternalObservation<ExternalProps> {
    ExternalObservation::from_props(
        source(revision),
        ExternalProps {
            value: value.into(),
            passive: true,
        },
    )
}

fn reply() -> ControlReply {
    ControlReply::structured(json!({ "action": "increment" }))
}

fn invalid_reply() -> ControlReply {
    ControlReply::structured(json!({ "action": "wrong" }))
}

fn ticket(outcome: ExternalObserveOutcome) -> ExternalActionTicket {
    match outcome {
        ExternalObserveOutcome::Frame(ExternalUserFrame::Actionable(ticket)) => ticket,
        ExternalObserveOutcome::SourceStale { .. } => panic!("test expected a fresh ticket"),
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(_)) => {
            panic!("test expected an actionable frame")
        }
        _ => panic!("test expected a known observation outcome"),
    }
}

type TestController = MountedExternalController<ExternalProps, TestContract, TestPort>;

async fn acknowledge_ticket(controller: &TestController, ticket: &ExternalActionTicket) {
    assert_eq!(
        controller
            .acknowledge_user_delivery(ticket.delivery_receipt())
            .await
            .unwrap(),
        ExternalUserDeliveryAckOutcome::Acknowledged
    );
}

fn reply_id(value: &str) -> ExternalReplyId {
    ExternalReplyId::new(value).unwrap()
}

#[tokio::test]
async fn mounted_external_requires_acknowledged_user_delivery_before_act() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    let ticket = ticket(
        controller
            .observe(
                DurableCallId::new("unacknowledged-call").unwrap(),
                DurableCallInputId::new("unacknowledged-input").unwrap(),
                observation("source-1", "the host must acknowledge this first"),
            )
            .await
            .unwrap(),
    );
    let delivery = port.user_delivery(ticket.delivery_receipt()).unwrap();
    assert_eq!(delivery.lane, ExternalUserDeliveryLane::Prompt);
    assert!(!delivery.acknowledged);

    assert!(matches!(
        controller
            .act(ticket.token(), reply_id("too-early"), reply())
            .await,
        Err(MountedExternalControllerError::UserDeliveryNotAcknowledged { receipt })
            if receipt == *ticket.delivery_receipt()
    ));
    assert!(matches!(
        controller.lookup(ticket.token()).await.unwrap(),
        ExternalActionLifecycle::AwaitingDelivery { .. }
    ));
    assert_eq!(port.domain_value(), 0);
}

#[tokio::test]
async fn mounted_external_preserves_system_user_replay_hook_and_source_boundaries() {
    let port = Arc::new(TestPort::new());
    let opened = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap();
    assert_eq!(
        opened.system_delivery_receipt().as_str(),
        "test/external-system-delivery/1"
    );
    assert_eq!(
        port.delivered_system().as_deref(),
        Some("Apply one typed external action.\n\nReply only with {\"action\":\"increment\"}.")
    );
    let controller = opened.into_controller();

    let reopened = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened.system_delivery_receipt().as_str(),
        "test/external-system-delivery/1"
    );
    let peer = reopened.into_controller();
    assert_eq!(port.system_installs(), 1);
    assert_eq!(
        port.delivered_system().as_deref(),
        Some("Apply one typed external action.\n\nReply only with {\"action\":\"increment\"}.")
    );

    let first = ticket(
        controller
            .observe(
                DurableCallId::new("call-1").unwrap(),
                DurableCallInputId::new("input-1").unwrap(),
                observation("source-1", "first user value"),
            )
            .await
            .unwrap(),
    );
    let first_delivery = port.user_delivery(first.delivery_receipt()).unwrap();
    assert_eq!(first_delivery.lane, ExternalUserDeliveryLane::Prompt);
    assert_eq!(
        first_delivery.system_delivery_receipt.as_str(),
        "test/external-system-delivery/1"
    );
    assert!(first_delivery.base_receipt.is_none());
    assert!(!first_delivery.acknowledged);
    assert!(first_delivery.rendered_user.contains("first user value"));
    let same_active_ticket = ticket(
        peer.observe(
            DurableCallId::new("call-1").unwrap(),
            DurableCallInputId::new("input-1").unwrap(),
            observation("source-1", "replacement owner must not rerender"),
        )
        .await
        .unwrap(),
    );
    assert_eq!(same_active_ticket, first);
    let replayed_delivery = port
        .user_delivery(same_active_ticket.delivery_receipt())
        .unwrap();
    assert_eq!(
        replayed_delivery.rendered_user,
        first_delivery.rendered_user
    );
    assert!(!replayed_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));

    acknowledge_ticket(&peer, &first).await;
    assert!(
        port.user_delivery(first.delivery_receipt())
            .unwrap()
            .acknowledged
    );
    assert_eq!(
        controller
            .acknowledge_user_delivery(first.delivery_receipt())
            .await
            .unwrap(),
        ExternalUserDeliveryAckOutcome::AlreadyAcknowledged
    );

    assert!(matches!(
        controller
            .act(first.token(), reply_id("invalid-1"), invalid_reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::InvalidReply { .. }
    ));
    assert_eq!(port.domain_value(), 0);
    assert_eq!(port.outbox_count(), 0);
    assert!(matches!(
        controller.lookup(first.token()).await.unwrap(),
        ExternalActionLifecycle::AwaitingReply { .. }
    ));

    let committed = match controller
        .act(first.token(), reply_id("reply-1"), reply())
        .await
        .unwrap()
    {
        agentview::component::advanced::external::ExternalActOutcome::Applied(commit) => commit,
        _ => panic!("test expected a committed action"),
    };
    assert_eq!(committed.wake_cursor().as_str(), "wake-1");
    assert_eq!(port.domain_value(), 1);
    assert_eq!(port.outbox_count(), 1);

    let replayed = match peer
        .act(first.token(), reply_id("reply-1"), reply())
        .await
        .unwrap()
    {
        agentview::component::advanced::external::ExternalActOutcome::Replayed(commit) => commit,
        _ => panic!("test expected a replay"),
    };
    assert_eq!(replayed.wake_cursor().as_str(), "wake-1");
    assert_eq!(port.domain_value(), 1);
    assert!(matches!(
        peer.act(
            first.token(),
            reply_id("reply-1"),
            ControlReply::structured(json!({ "action": "different" })),
        )
        .await,
        Err(MountedExternalControllerError::ReplyIdCollision { .. })
    ));

    let hooked = peer
        .hook(
            &ExternalWakeCursor::new("wake-0").unwrap(),
            DurableCallId::new("call-2").unwrap(),
            DurableCallInputId::new("input-2").unwrap(),
            || observation("source-1", "fresh hooked value"),
        )
        .await
        .unwrap();
    assert_eq!(hooked.wake_cursor().as_str(), "wake-1");
    let hooked_ticket = ticket(hooked.into_observation());
    assert!(port
        .user_delivery(hooked_ticket.delivery_receipt())
        .unwrap()
        .rendered_user
        .contains("fresh hooked value"));
    peer.cancel(hooked_ticket.token()).await.unwrap();
    assert!(
        port.user_delivery(hooked_ticket.delivery_receipt())
            .unwrap()
            .cancelled
    );

    port.set_source_revision("source-2");
    let stale_ticket = ticket(
        controller
            .observe(
                DurableCallId::new("call-3").unwrap(),
                DurableCallInputId::new("input-3").unwrap(),
                observation("source-2", "will become stale"),
            )
            .await
            .unwrap(),
    );
    // Delivery acknowledgement confirms immutable bytes and remains valid even
    // if the authoritative domain revision changes after publication.
    port.set_source_revision("source-3");
    acknowledge_ticket(&controller, &stale_ticket).await;
    assert!(matches!(
        controller
            .act(stale_ticket.token(), reply_id("stale-reply"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::SourceStale { .. }
    ));
    assert_eq!(port.domain_value(), 1);
    controller.cancel(stale_ticket.token()).await.unwrap();
}

#[tokio::test]
async fn mounted_external_passive_frame_has_no_action_token_and_is_superseded_without_cancel() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();
    let replacement = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    let passive = match controller
        .observe(
            DurableCallId::new("passive-call").unwrap(),
            DurableCallInputId::new("passive-input").unwrap(),
            passive_observation("source-1", "wait for the domain wake"),
        )
        .await
        .unwrap()
    {
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(presentation)) => presentation,
        outcome => panic!("expected passive presentation, got {outcome:?}"),
    };
    let passive_delivery = port.user_delivery(passive.delivery_receipt()).unwrap();
    assert_eq!(
        passive_delivery.lane,
        ExternalUserDeliveryLane::Presentation
    );
    assert!(passive_delivery.base_receipt.is_none());
    assert!(!passive_delivery.acknowledged);
    assert!(passive_delivery
        .rendered_user
        .contains("wait for the domain wake"));

    let replayed = replacement
        .observe(
            DurableCallId::new("passive-call").unwrap(),
            DurableCallInputId::new("passive-input").unwrap(),
            passive_observation("source-1", "must not rerender a replacement value"),
        )
        .await
        .unwrap();
    assert!(matches!(
        replayed,
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(ref current))
            if current == &passive
    ));

    let actionable = ticket(
        replacement
            .observe(
                DurableCallId::new("after-passive-call").unwrap(),
                DurableCallInputId::new("after-passive-input").unwrap(),
                observation("source-1", "act after the wake"),
            )
            .await
            .unwrap(),
    );
    assert_eq!(actionable.token().turn_index(), passive.turn_index() + 1);
    acknowledge_ticket(&replacement, &actionable).await;
    assert!(matches!(
        replacement
            .act(actionable.token(), reply_id("after-passive-reply"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::Applied(_)
    ));
    assert_eq!(port.domain_value(), 1);
}

#[tokio::test]
async fn mounted_external_uses_acknowledged_prompt_cursor_for_delta_and_keeps_presentations_full() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    let first = ticket(
        controller
            .observe(
                DurableCallId::new("cursor-call-1").unwrap(),
                DurableCallInputId::new("cursor-input-1").unwrap(),
                observation("source-1", "first owner value"),
            )
            .await
            .unwrap(),
    );
    let first_delivery = port.user_delivery(first.delivery_receipt()).unwrap();
    assert_eq!(first_delivery.lane, ExternalUserDeliveryLane::Prompt);
    assert!(first_delivery.base_receipt.is_none());
    assert!(!first_delivery.acknowledged);
    assert!(!first_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));
    assert!(first_delivery.rendered_user.contains("first owner value"));

    // A replacement owner replays the durable ticket and its exact outbox row.
    let replacement = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap();
    assert_eq!(
        replacement.system_delivery_receipt().as_str(),
        "test/external-system-delivery/1"
    );
    let replacement = replacement.into_controller();
    let replayed_first = ticket(
        replacement
            .observe(
                DurableCallId::new("cursor-call-1").unwrap(),
                DurableCallInputId::new("cursor-input-1").unwrap(),
                observation("source-1", "must not replace retained prompt bytes"),
            )
            .await
            .unwrap(),
    );
    assert_eq!(replayed_first, first);
    assert_eq!(
        port.user_delivery(replayed_first.delivery_receipt())
            .unwrap()
            .rendered_user,
        first_delivery.rendered_user
    );

    acknowledge_ticket(&replacement, &first).await;
    assert!(
        port.user_delivery(first.delivery_receipt())
            .unwrap()
            .acknowledged
    );
    assert!(matches!(
        controller
            .act(first.token(), reply_id("cursor-commit"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::Applied(_)
    ));

    let passive = match controller
        .observe(
            DurableCallId::new("cursor-passive-call").unwrap(),
            DurableCallInputId::new("cursor-passive-input").unwrap(),
            passive_observation("source-1", "passive presentation value"),
        )
        .await
        .unwrap()
    {
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(presentation)) => presentation,
        outcome => panic!("expected a passive presentation, got {outcome:?}"),
    };
    let passive_delivery = port.user_delivery(passive.delivery_receipt()).unwrap();
    assert_eq!(
        passive_delivery.lane,
        ExternalUserDeliveryLane::Presentation
    );
    assert!(passive_delivery.base_receipt.is_none());
    assert!(!passive_delivery.acknowledged);
    assert!(!passive_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));
    assert!(passive_delivery
        .rendered_user
        .contains("passive presentation value"));

    let replayed_passive = replacement
        .observe(
            DurableCallId::new("cursor-passive-call").unwrap(),
            DurableCallInputId::new("cursor-passive-input").unwrap(),
            passive_observation("source-1", "must not replace retained presentation bytes"),
        )
        .await
        .unwrap();
    assert!(matches!(
        replayed_passive,
        ExternalObserveOutcome::Frame(ExternalUserFrame::Passive(ref current))
            if current == &passive
    ));

    // Presentation does not advance the prompt cursor. The second prompt is
    // therefore a delta from the acknowledged first prompt, not from passive.
    let later = ticket(
        replacement
            .observe(
                DurableCallId::new("cursor-call-2").unwrap(),
                DurableCallInputId::new("cursor-input-2").unwrap(),
                observation("source-1", "later owner value"),
            )
            .await
            .unwrap(),
    );
    let later_delivery = port.user_delivery(later.delivery_receipt()).unwrap();
    assert_eq!(later_delivery.lane, ExternalUserDeliveryLane::Prompt);
    assert_eq!(
        later_delivery.base_receipt.as_ref(),
        Some(first.delivery_receipt())
    );
    assert!(!later_delivery.acknowledged);
    assert!(later_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));
    assert!(later_delivery.rendered_user.contains("later owner value"));

    let replayed_later = ticket(
        controller
            .observe(
                DurableCallId::new("cursor-call-2").unwrap(),
                DurableCallInputId::new("cursor-input-2").unwrap(),
                observation("source-1", "must not replace retained delta bytes"),
            )
            .await
            .unwrap(),
    );
    assert_eq!(replayed_later, later);
    controller.cancel(later.token()).await.unwrap();
    assert!(
        port.user_delivery(later.delivery_receipt())
            .unwrap()
            .cancelled
    );

    // A safely tombstoned, unacknowledged delta cannot become a baseline. The
    // controller persists a full-resync fence for its successor.
    let after_cancel = ticket(
        controller
            .observe(
                DurableCallId::new("cursor-call-3").unwrap(),
                DurableCallInputId::new("cursor-input-3").unwrap(),
                observation("source-1", "full after cancelled delivery"),
            )
            .await
            .unwrap(),
    );
    let after_cancel_delivery = port.user_delivery(after_cancel.delivery_receipt()).unwrap();
    assert!(after_cancel_delivery.base_receipt.is_none());
    assert!(!after_cancel_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));
    assert!(after_cancel_delivery
        .rendered_user
        .contains("full after cancelled delivery"));
    acknowledge_ticket(&controller, &after_cancel).await;
    controller.cancel(after_cancel.token()).await.unwrap();
}

#[tokio::test]
async fn mounted_external_user_only_resync_keeps_system_and_resumes_delta_after_ack() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    let first = ticket(
        controller
            .observe(
                DurableCallId::new("resync-call-1").unwrap(),
                DurableCallInputId::new("resync-input-1").unwrap(),
                observation("source-1", "first baseline"),
            )
            .await
            .unwrap(),
    );
    acknowledge_ticket(&controller, &first).await;
    assert!(matches!(
        controller
            .act(first.token(), reply_id("resync-commit-1"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::Applied(_)
    ));
    assert_eq!(port.system_installs(), 1);

    let abandoned_delta = ticket(
        controller
            .observe(
                DurableCallId::new("resync-call-2").unwrap(),
                DurableCallInputId::new("resync-input-2").unwrap(),
                observation("source-1", "delta whose consumer baseline was lost"),
            )
            .await
            .unwrap(),
    );
    let abandoned_delivery = port
        .user_delivery(abandoned_delta.delivery_receipt())
        .unwrap();
    assert_eq!(
        abandoned_delivery.base_receipt.as_ref(),
        Some(first.delivery_receipt())
    );
    assert!(abandoned_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));

    assert_eq!(
        controller.request_user_document_resync().await.unwrap(),
        ExternalUserResyncOutcome::Requested
    );
    assert!(
        port.user_delivery(abandoned_delta.delivery_receipt())
            .unwrap()
            .cancelled,
        "the undelivered delta must be tombstoned before its full replacement"
    );
    assert_eq!(
        controller.request_user_document_resync().await.unwrap(),
        ExternalUserResyncOutcome::AlreadyRequested
    );
    let full = ticket(
        controller
            .observe(
                DurableCallId::new("resync-call-3").unwrap(),
                DurableCallInputId::new("resync-input-3").unwrap(),
                observation("source-1", "full after consumer baseline loss"),
            )
            .await
            .unwrap(),
    );
    let full_delivery = port.user_delivery(full.delivery_receipt()).unwrap();
    assert!(full_delivery.base_receipt.is_none());
    assert!(!full_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));
    assert_eq!(
        port.system_installs(),
        1,
        "User resync must not resend System"
    );

    acknowledge_ticket(&controller, &full).await;
    assert!(matches!(
        controller
            .act(full.token(), reply_id("resync-commit-3"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::Applied(_)
    ));
    let delta = ticket(
        controller
            .observe(
                DurableCallId::new("resync-call-4").unwrap(),
                DurableCallInputId::new("resync-input-4").unwrap(),
                observation("source-1", "delta after resync acknowledgement"),
            )
            .await
            .unwrap(),
    );
    let delta_delivery = port.user_delivery(delta.delivery_receipt()).unwrap();
    assert_eq!(
        delta_delivery.base_receipt.as_ref(),
        Some(full.delivery_receipt())
    );
    assert!(delta_delivery
        .rendered_user
        .contains("rendering_mode=\"delta\""));
    assert_eq!(port.system_installs(), 1);
}

#[tokio::test]
async fn mounted_external_rejects_pre_user_delivery_state_schema() {
    let port = Arc::new(TestPort::new());
    MountedExternalController::open(definition(TestContract::v1()), Arc::clone(&port), session())
        .await
        .unwrap();
    port.force_state_schema_version(4);

    let error = match MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    {
        Ok(_) => panic!("schema-v4 external state must not reopen under User-delivery semantics"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        MountedExternalControllerError::StateSchemaMismatch {
            expected: 5,
            actual: 4,
        }
    ));
    assert_eq!(port.system_installs(), 1);
}

#[tokio::test]
async fn mounted_external_recovers_indeterminate_commits_and_fences_unknowns() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    let retry_ticket = ticket(
        controller
            .observe(
                DurableCallId::new("retry-call").unwrap(),
                DurableCallInputId::new("retry-input").unwrap(),
                observation("source-1", "retry"),
            )
            .await
            .unwrap(),
    );
    acknowledge_ticket(&controller, &retry_ticket).await;
    port.make_next_commit_indeterminate(ExternalCommitResolution::NotCommitted);
    let retry_identity = match controller
        .act(retry_ticket.token(), reply_id("retry-reply"), reply())
        .await
        .unwrap()
    {
        agentview::component::advanced::external::ExternalActOutcome::Indeterminate {
            identity,
        } => identity,
        _ => panic!("test expected an indeterminate commit"),
    };
    assert!(matches!(
        controller.recover(&retry_identity).await.unwrap(),
        ExternalRecoveryOutcome::Retry
    ));
    assert!(matches!(
        controller
            .act(retry_ticket.token(), reply_id("retry-reply"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::Applied(_)
    ));
    assert_eq!(port.domain_value(), 1);

    let unknown_ticket = ticket(
        controller
            .observe(
                DurableCallId::new("unknown-call").unwrap(),
                DurableCallInputId::new("unknown-input").unwrap(),
                observation("source-1", "unknown"),
            )
            .await
            .unwrap(),
    );
    acknowledge_ticket(&controller, &unknown_ticket).await;
    port.make_next_commit_indeterminate(ExternalCommitResolution::Unknown);
    let unknown_identity = match controller
        .act(unknown_ticket.token(), reply_id("unknown-reply"), reply())
        .await
        .unwrap()
    {
        agentview::component::advanced::external::ExternalActOutcome::Indeterminate {
            identity,
        } => identity,
        _ => panic!("test expected an indeterminate commit"),
    };
    assert!(matches!(
        controller.recover(&unknown_identity).await.unwrap(),
        ExternalRecoveryOutcome::RecoveryRequired
    ));
    assert!(matches!(
        controller.lookup(unknown_ticket.token()).await.unwrap(),
        ExternalActionLifecycle::RecoveryRequired { .. }
    ));
    assert_eq!(port.domain_value(), 1);
}

#[tokio::test]
async fn mounted_external_cancel_wins_cross_owner_race_and_decoder_id_is_durable() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();
    let peer = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    assert!(matches!(
        MountedExternalController::open(
            definition(TestContract::v2()),
            Arc::clone(&port),
            session(),
        )
        .await,
        Err(MountedExternalControllerError::StateReplyContractMismatch { .. })
    ));

    let ticket = ticket(
        controller
            .observe(
                DurableCallId::new("race-call").unwrap(),
                DurableCallInputId::new("race-input").unwrap(),
                observation("source-1", "race"),
            )
            .await
            .unwrap(),
    );
    acknowledge_ticket(&controller, &ticket).await;
    let gate = port.block_next_commit();
    let started = gate.started.notified();
    let token = ticket.token().clone();
    let committing = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .act(&token, reply_id("race-reply"), reply())
                .await
        }
    });
    started.await;
    assert!(matches!(
        peer.cancel(ticket.token()).await.unwrap(),
        agentview::component::advanced::external::ExternalCancelOutcome::Cancelled
    ));
    gate.allow.notify_waiters();
    assert!(matches!(
        committing.await.unwrap(),
        Err(MountedExternalControllerError::TokenCancelled { .. })
    ));
    assert_eq!(port.domain_value(), 0);
    assert_eq!(port.outbox_count(), 0);
}

#[tokio::test]
async fn mounted_external_rejects_success_without_a_new_generation() {
    let port = Arc::new(TestPort::new());
    let controller = MountedExternalController::open(
        definition(TestContract::v1()),
        Arc::clone(&port),
        session(),
    )
    .await
    .unwrap()
    .into_controller();

    port.make_next_success_report_stale_generation(StaleGenerationMutation::Publish);
    assert!(matches!(
        controller
            .observe(
                DurableCallId::new("generation-observe").unwrap(),
                DurableCallInputId::new("generation-observe-input").unwrap(),
                observation("source-1", "observe fault"),
            )
            .await,
        Err(MountedExternalControllerError::GenerationNotAdvanced { .. })
    ));

    let action_ticket = ticket(
        controller
            .observe(
                DurableCallId::new("generation-action").unwrap(),
                DurableCallInputId::new("generation-action-input").unwrap(),
                observation("source-1", "action fault"),
            )
            .await
            .unwrap(),
    );
    acknowledge_ticket(&controller, &action_ticket).await;
    port.make_next_success_report_stale_generation(StaleGenerationMutation::PrepareReply);
    assert!(matches!(
        controller
            .act(action_ticket.token(), reply_id("generation-reply"), reply())
            .await,
        Err(MountedExternalControllerError::GenerationNotAdvanced { .. })
    ));
    assert!(matches!(
        controller.lookup(action_ticket.token()).await.unwrap(),
        ExternalActionLifecycle::AwaitingReply { .. }
    ));

    port.make_next_success_report_stale_generation(StaleGenerationMutation::Commit);
    let indeterminate = match controller
        .act(action_ticket.token(), reply_id("generation-reply"), reply())
        .await
        .unwrap()
    {
        agentview::component::advanced::external::ExternalActOutcome::Indeterminate {
            identity,
        } => identity,
        _ => panic!("a commit with a stale generation must require recovery"),
    };
    assert_eq!(port.domain_value(), 0);
    assert_eq!(port.outbox_count(), 0);
    assert!(matches!(
        controller.recover(&indeterminate).await.unwrap(),
        ExternalRecoveryOutcome::Retry
    ));
    assert!(matches!(
        controller
            .act(action_ticket.token(), reply_id("generation-reply"), reply())
            .await
            .unwrap(),
        agentview::component::advanced::external::ExternalActOutcome::Applied(_)
    ));

    let cancel_ticket = ticket(
        controller
            .observe(
                DurableCallId::new("generation-cancel").unwrap(),
                DurableCallInputId::new("generation-cancel-input").unwrap(),
                observation("source-1", "cancel fault"),
            )
            .await
            .unwrap(),
    );
    port.make_next_success_report_stale_generation(StaleGenerationMutation::Cancel);
    assert!(matches!(
        controller.cancel(cancel_ticket.token()).await,
        Err(MountedExternalControllerError::GenerationNotAdvanced { .. })
    ));
    assert!(matches!(
        controller.lookup(cancel_ticket.token()).await.unwrap(),
        ExternalActionLifecycle::AwaitingDelivery { .. }
    ));
    controller.cancel(cancel_ticket.token()).await.unwrap();
    assert!(
        port.user_delivery(cancel_ticket.delivery_receipt())
            .unwrap()
            .cancelled
    );

    let recovery_ticket = ticket(
        controller
            .observe(
                DurableCallId::new("generation-recovery").unwrap(),
                DurableCallInputId::new("generation-recovery-input").unwrap(),
                observation("source-1", "recovery fault"),
            )
            .await
            .unwrap(),
    );
    acknowledge_ticket(&controller, &recovery_ticket).await;
    port.make_next_commit_indeterminate(ExternalCommitResolution::Unknown);
    let recovery_identity = match controller
        .act(
            recovery_ticket.token(),
            reply_id("generation-recovery-reply"),
            reply(),
        )
        .await
        .unwrap()
    {
        agentview::component::advanced::external::ExternalActOutcome::Indeterminate {
            identity,
        } => identity,
        _ => panic!("test setup must create an unknown commit"),
    };
    port.make_next_success_report_stale_generation(StaleGenerationMutation::MarkRecovery);
    assert!(matches!(
        controller.recover(&recovery_identity).await,
        Err(MountedExternalControllerError::GenerationNotAdvanced { .. })
    ));
    assert!(matches!(
        controller.lookup(recovery_ticket.token()).await.unwrap(),
        ExternalActionLifecycle::Prepared { .. }
    ));
    assert!(matches!(
        controller.recover(&recovery_identity).await.unwrap(),
        ExternalRecoveryOutcome::RecoveryRequired
    ));
}
