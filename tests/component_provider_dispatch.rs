use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::advanced::{
        experimental::{TurnPublication, TurnPublisher},
        lifecycle::{
            mount_system_epoch, MountedEpoch, PreparedUserTurn, SystemMountContext, SystemView,
        },
        persistence::*,
        provider::*,
    },
    component::*,
    prelude::*,
};
use serde_json::json;
use tokio::sync::Notify;

struct LocalChannels;

impl TurnChannels for LocalChannels {
    type Output = String;
    type Live = String;
    type Commit = String;
    type Diagnostic = String;
}

struct RootChannels;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootOutput(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootLive(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootCommit(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootDiagnostic(String);

impl TurnChannels for RootChannels {
    type Output = RootOutput;
    type Live = RootLive;
    type Commit = RootCommit;
    type Diagnostic = RootDiagnostic;
}

#[derive(Debug, Clone)]
struct Initialization {
    group: &'static str,
    marker: u32,
    attempt: String,
}

#[derive(Debug, Clone)]
struct Invocation {
    group: &'static str,
    name: String,
    invocation_id: String,
    sequence: u64,
    attempt: String,
    invocation_epoch: String,
    invocation_turn: String,
    invocation_capability: String,
}

#[derive(Debug, Default)]
struct Trace {
    initializations: Vec<Initialization>,
    invocations: Vec<Invocation>,
    finishes: Vec<&'static str>,
    aborts: Vec<&'static str>,
}

struct MountProps {
    trace: Arc<Mutex<Trace>>,
}

struct TurnProps {
    marker: u32,
}

#[derive(Debug, thiserror::Error)]
#[error("dispatcher infrastructure failed")]
struct DispatcherFailure;

#[derive(Debug)]
struct RecordingDispatcher {
    group: &'static str,
    marker: u32,
    trace: Arc<Mutex<Trace>>,
    fail_abort: bool,
}

#[async_trait::async_trait]
impl ProviderDispatcher<LocalChannels> for RecordingDispatcher {
    type Error = DispatcherFailure;

    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<LocalChannels>, Self::Error> {
        assert_eq!(
            Some(context.invocation_key().invocation_id()),
            call.invocation_id(),
        );
        if call.arguments()["infra"] == true {
            return Err(DispatcherFailure);
        }
        self.trace.lock().unwrap().invocations.push(Invocation {
            group: self.group,
            name: call.name().to_owned(),
            invocation_id: call
                .invocation_id()
                .expect("runtime validates invocation identity before dispatch")
                .to_owned(),
            sequence: context.sequence(),
            attempt: context.identity().provider_attempt_id().to_string(),
            invocation_epoch: context.invocation_key().epoch_id().to_string(),
            invocation_turn: context.invocation_key().turn_instance_id().to_string(),
            invocation_capability: context.invocation_key().capability_id().to_string(),
        });

        let response = if call.arguments()["reject"] == true {
            ProviderToolResponse::error("rejected", "application rejected this call")
        } else {
            ProviderToolResponse::success(json!({
                "group": self.group,
                "marker": self.marker,
                "name": call.name(),
            }))
        };
        let label = format!("{}:{}:{}", self.group, self.marker, call.name());
        Ok(ProviderDispatchUpdate::new(
            response,
            StreamUpdate::from_emission(TurnEmission::Output(label.clone()))
                .with_emission(TurnEmission::Live(format!("live:{label}")))
                .with_emission(TurnEmission::Commit(format!("commit:{label}")))
                .with_diagnostic(format!("diagnostic:{label}")),
        ))
    }

    async fn finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<LocalChannels>, String>, Self::Error> {
        self.trace.lock().unwrap().finishes.push(self.group);
        Ok(
            StreamUpdate::from_emission(TurnEmission::Live(format!("finish-live:{}", self.group)))
                .with_emission(TurnEmission::Commit(format!(
                    "finish-commit:{}",
                    self.group
                ))),
        )
    }

    async fn abort(
        &mut self,
        _context: &ProviderDispatcherAbortContext,
    ) -> Result<ProviderDispatcherAbortAck, Self::Error> {
        self.trace.lock().unwrap().aborts.push(self.group);
        if self.fail_abort {
            Err(DispatcherFailure)
        } else {
            Ok(ProviderDispatcherAbortAck::CleanupCompleted)
        }
    }
}

fn tool_spec(name: &str) -> ProviderToolSpec {
    ProviderToolSpec::new(
        name,
        format!("Run {name}"),
        json!({ "type": "object", "properties": {} }),
    )
    .unwrap()
}

fn native_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<RootChannels, TurnProps> {
    let primary_trace = Arc::clone(&cx.props().trace);
    let primary = provider_tools_with_context(
        "primary",
        [tool_spec("alpha"), tool_spec("beta")],
        move |cx: &ProviderDispatcherCx<'_, TurnProps, LocalChannels>| {
            primary_trace
                .lock()
                .unwrap()
                .initializations
                .push(Initialization {
                    group: "primary",
                    marker: cx.props().marker,
                    attempt: cx.identity().provider_attempt_id().to_string(),
                });
            Ok(RecordingDispatcher {
                group: "primary",
                marker: cx.props().marker,
                trace: Arc::clone(&primary_trace),
                fail_abort: false,
            })
        },
    );
    let secondary_trace = Arc::clone(&cx.props().trace);
    let secondary = provider_tool_with_context(
        "secondary",
        tool_spec("gamma"),
        move |cx: &ProviderDispatcherCx<'_, TurnProps, LocalChannels>| {
            secondary_trace
                .lock()
                .unwrap()
                .initializations
                .push(Initialization {
                    group: "secondary",
                    marker: cx.props().marker,
                    attempt: cx.identity().provider_attempt_id().to_string(),
                });
            Ok(RecordingDispatcher {
                group: "secondary",
                marker: cx.props().marker,
                trace: Arc::clone(&secondary_trace),
                fail_abort: true,
            })
        },
    );
    let map = || {
        TurnChannelMap::new(
            |output| RootOutput(format!("root:{output}")),
            |live| RootLive(format!("root:{live}")),
            |commit| RootCommit(format!("root:{commit}")),
            |diagnostic| RootDiagnostic(format!("root:{diagnostic}")),
        )
    };
    system_view(component((
        primary.map_channels(map()),
        secondary.map_channels(map()),
    )))
}

#[derive(Default)]
struct LiveTrace {
    effects: Vec<(u64, String, RootLive)>,
    aborts: usize,
}

struct RecordingLiveRuntime {
    trace: Arc<Mutex<LiveTrace>>,
}

#[async_trait::async_trait]
impl LiveEffectRuntime<RootLive> for RecordingLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        effect: RootLive,
    ) -> Result<(), Self::Error> {
        self.trace.lock().unwrap().effects.push((
            context.sequence(),
            context.origin().route().to_string(),
            effect,
        ));
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        self.trace.lock().unwrap().aborts += 1;
        Ok(if context.applied_effects() == 0 {
            LiveEffectAbortAck::NoEffectsApplied
        } else {
            LiveEffectAbortAck::CompensationCompleted
        })
    }
}

#[derive(Default)]
struct RecordingPublisher {
    results: Vec<ProviderToolResult>,
}

#[async_trait::async_trait]
impl TurnPublisher for RecordingPublisher {
    type Error = Infallible;

    async fn publish(&mut self, publication: TurnPublication<'_>) -> Result<(), Self::Error> {
        self.results
            .extend_from_slice(publication.provider_results());
        Ok(())
    }
}

fn call(id: &str, name: &str) -> ProviderToolCall {
    ProviderToolCall::new(id, name, json!({}))
}

fn prepared<'a>(
    epoch: &'a MountedEpoch<RootChannels, TurnProps>,
    props: &'a TurnProps,
) -> PreparedUserTurn<'a, 'a, RootChannels, TurnProps> {
    let turn = epoch.begin_turn("native-tools");
    turn.prepare_user(props, |_| user_view(())).unwrap()
}

#[derive(Debug)]
struct InitDropDispatcher {
    drops: Arc<AtomicUsize>,
}

impl Drop for InitDropDispatcher {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ProviderDispatcher<RootChannels> for InitDropDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<RootChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("unused"),
            StreamUpdate::new(),
        ))
    }
}

struct InitFailureProps {
    drops: Arc<AtomicUsize>,
}

fn provider_init_failure_system(
    cx: SystemMountContext<'_, InitFailureProps>,
) -> SystemView<RootChannels, TurnProps> {
    let drops = Arc::clone(&cx.props().drops);
    system_view(component((
        provider_tool("ready", tool_spec("ready"), move || InitDropDispatcher {
            drops: Arc::clone(&drops),
        }),
        provider_tool_with_context(
            "broken",
            tool_spec("broken"),
            |_: &ProviderDispatcherCx<'_, TurnProps, RootChannels>| {
                Err::<InitDropDispatcher, _>(ProviderDispatchFailure::new(DispatcherFailure))
            },
        ),
    )))
}

#[test]
fn dispatcher_initialization_failure_has_full_origin_and_drops_inert_prior_state() {
    let drops = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &InitFailureProps {
            drops: Arc::clone(&drops),
        },
        provider_init_failure_system,
    )
    .unwrap();
    let props = TurnProps { marker: 11 };
    let turn = epoch.begin_turn("provider-init-failure");
    let expected_turn = turn.id();
    let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();

    let error = prepared
        .start_provider_attempt(RecordingLiveRuntime {
            trace: Arc::new(Mutex::new(LiveTrace::default())),
        })
        .unwrap_err();
    let ProviderAttemptStartError::Dispatcher(fault) = error else {
        panic!("expected dispatcher initialization failure");
    };
    assert_eq!(fault.phase(), ProviderDispatchPhase::Initialize);
    assert!(fault
        .origin()
        .capability_id()
        .to_string()
        .ends_with("::broken"));
    assert_eq!(fault.origin().attempt().epoch_id(), epoch.id());
    assert_eq!(fault.origin().attempt().turn_instance_id(), expected_turn);
    assert_eq!(
        fault.origin().attempt().call_label(),
        "provider-init-failure"
    );
    assert_eq!(fault.origin().tool_name(), None);
    assert_eq!(fault.origin().invocation_id(), None);
    assert_eq!(fault.origin().result_correlation_id(), None);

    // Attempt initialization is synchronous and inert. Before an attempt is
    // returned, already-created state is dropped; it must not own resources
    // that require async abort. Such resources belong in dispatch/abort.
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tool_groups_share_state_map_channels_and_release_commit_after_publication() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    assert_eq!(epoch.provider_capabilities().len(), 2);
    assert_eq!(
        epoch.provider_capabilities().capabilities()[0]
            .specs()
            .len(),
        2
    );
    assert!(trace.lock().unwrap().initializations.is_empty());

    let props = TurnProps { marker: 7 };
    let prepared = prepared(&epoch, &props);
    let live = Arc::new(Mutex::new(LiveTrace::default()));
    let mut attempt = prepared
        .start_provider_attempt(RecordingLiveRuntime {
            trace: Arc::clone(&live),
        })
        .unwrap();
    assert_eq!(attempt.dispatcher_count(), 2);
    assert_eq!(attempt.tool_count(), 3);
    let attempt_id = attempt.identity().provider_attempt_id().to_string();

    for (id, name) in [("call-1", "alpha"), ("call-2", "gamma"), ("call-3", "beta")] {
        let mut tool_call = call(id, name);
        if id == "call-1" {
            tool_call = tool_call.with_result_correlation_id("provider-correlation-1");
        }
        let outcome = attempt.call_tool(tool_call).await.unwrap();
        assert!(!outcome.replayed());
        assert!(!outcome.result().is_error());
        assert_eq!(outcome.result().invocation_id(), Some(id));
        assert_eq!(
            outcome.result().result_correlation_id(),
            (id == "call-1").then_some("provider-correlation-1")
        );
        assert_eq!(outcome.result().name(), name);
        assert_eq!(outcome.update().emissions().len(), 1);
        assert_eq!(outcome.update().diagnostics().len(), 1);
    }

    {
        let trace = trace.lock().unwrap();
        assert_eq!(trace.initializations.len(), 2);
        assert_eq!(
            trace
                .initializations
                .iter()
                .map(|item| item.group)
                .collect::<Vec<_>>(),
            ["primary", "secondary"]
        );
        assert!(trace
            .initializations
            .iter()
            .all(|item| item.marker == 7 && item.attempt == attempt_id));
        assert_eq!(
            trace
                .invocations
                .iter()
                .map(|item| (item.group, item.name.as_str(), item.sequence))
                .collect::<Vec<_>>(),
            [
                ("primary", "alpha", 0),
                ("secondary", "gamma", 1),
                ("primary", "beta", 2),
            ]
        );
        assert!(trace
            .invocations
            .iter()
            .all(|item| item.attempt == attempt_id));
        assert!(trace
            .invocations
            .iter()
            .all(|item| !item.invocation_epoch.is_empty()
                && !item.invocation_turn.is_empty()
                && !item.invocation_capability.is_empty()
                && !item.invocation_id.is_empty()));
    }

    let finished = attempt.finish().await.unwrap();
    assert!(finished.update().emissions().is_empty());
    assert_eq!(trace.lock().unwrap().finishes, ["primary", "secondary"]);
    assert_eq!(live.lock().unwrap().effects.len(), 5);

    let mut publisher = RecordingPublisher::default();
    let published = finished.publish_with(&mut publisher).await.unwrap();
    assert_eq!(publisher.results.len(), 3);
    assert_eq!(
        publisher.results[0].result_correlation_id(),
        Some("provider-correlation-1")
    );
    assert_eq!(published.results().len(), 3);
    assert_eq!(
        published.results()[0].result_correlation_id(),
        Some("provider-correlation-1")
    );
    assert_eq!(published.pending_commits().len(), 5);
    assert!(published
        .pending_commits()
        .iter()
        .all(|commit| commit.0.starts_with("root:")));
}

struct RootCommitStager;

impl CommitStager<RootChannels> for RootCommitStager {
    type Payload = String;
    type Error = Infallible;

    fn stage(
        &self,
        context: CommitStagingContext<'_>,
        commit: &RootCommit,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        Ok(StagedCommit::new(
            CommitContract::new("test.root-commit", 1).unwrap(),
            format!("{}={}", context.item_id(), commit.0),
        ))
    }
}

struct NativeFingerprintFactory;

impl PublicationFingerprintFactory<usize, String, u64> for NativeFingerprintFactory {
    type Error = DurableKeyError;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<'_, usize, String, u64>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        PublicationCandidateFingerprint::new(format!(
            "native-v1|request={}|revision={}|mutation={}|raw={:?}|results={:?}|outbox={:?}",
            context.request_id(),
            context.expected_revision(),
            context.mutation().value(),
            context.raw_output(),
            context.provider_results(),
            context.outbox(),
        ))
    }
}

#[derive(Debug)]
struct NativePublicationRecord {
    mutation: usize,
    results: usize,
    items: Vec<(OutboxItemId, String)>,
    receipt: PublicationReceipt<u64>,
}

#[derive(Default)]
struct NativePublicationStore {
    record: Mutex<Option<NativePublicationRecord>>,
}

#[derive(Debug, thiserror::Error)]
enum NativePublicationStoreError {
    #[error("native publication store already contains a different request id")]
    DifferentRequestId,

    #[error("native publication request id was reused with a different fingerprint")]
    FingerprintCollision,
}

#[async_trait::async_trait]
impl PublicationStore<usize, String> for NativePublicationStore {
    type Version = u64;
    type Error = NativePublicationStoreError;

    async fn publish(
        &self,
        request: PublicationRequest<'_, usize, String, u64>,
    ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
        let mut record = self.record.lock().unwrap();
        if let Some(record) = record.as_ref() {
            return if record.receipt.request_id() != request.request_id() {
                Err(PublicationWriteError::Rejected(
                    NativePublicationStoreError::DifferentRequestId,
                ))
            } else if record.receipt.fingerprint().as_bytes() != request.fingerprint().as_bytes() {
                Err(PublicationWriteError::Rejected(
                    NativePublicationStoreError::FingerprintCollision,
                ))
            } else {
                Ok(record.receipt.clone())
            };
        }
        let receipt = PublicationReceipt::new(
            request.request_id().clone(),
            request.fingerprint().clone(),
            PublicationId::new("native-publication-1").unwrap(),
            request.expected_revision() + 1,
            u64::try_from(request.outbox().len()).unwrap(),
        );
        *record = Some(NativePublicationRecord {
            mutation: *request.mutation().value(),
            results: request.provider_results().len(),
            items: request
                .outbox()
                .items()
                .iter()
                .map(|item| (item.id().clone(), item.payload().clone()))
                .collect(),
            receipt: receipt.clone(),
        });
        Ok(receipt)
    }

    async fn resolve(
        &self,
        request_id: &PublicationRequestId,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
        match self.record.lock().unwrap().as_ref() {
            Some(record)
                if record.receipt.request_id() == request_id
                    && record.receipt.fingerprint().as_bytes() == fingerprint.as_bytes() =>
            {
                Ok(PublicationResolution::Published(record.receipt.clone()))
            }
            Some(record) if record.receipt.request_id() == request_id => {
                Err(PublicationResolveError::CandidateCollision(
                    NativePublicationStoreError::FingerprintCollision,
                ))
            }
            Some(_) | None => Ok(PublicationResolution::NotCommitted),
        }
    }
}

#[tokio::test]
async fn native_only_attempt_stages_commit_before_atomic_publication() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    let catalog = epoch.provider_tool_catalog();
    assert_eq!(catalog.epoch_id(), epoch.id());
    assert_eq!(
        catalog
            .specs()
            .iter()
            .map(ProviderToolSpec::name)
            .collect::<Vec<_>>(),
        ["alpha", "beta", "gamma"]
    );

    let props = TurnProps { marker: 21 };
    let prepared = prepared(&epoch, &props);
    let mut attempt = prepared
        .start_provider_attempt(RecordingLiveRuntime {
            trace: Arc::new(Mutex::new(LiveTrace::default())),
        })
        .unwrap();
    let _ = attempt.call_tool(call("durable-1", "alpha")).await.unwrap();
    let finished = attempt.finish().await.unwrap();

    let mut publication = finished
        .stage_durable_publication(
            PublicationRequestId::new("native-session/turn-1").unwrap(),
            40_u64,
            PreparedSessionMutation::new(3_usize),
            &RootCommitStager,
            &NativeFingerprintFactory,
        )
        .unwrap();
    assert_eq!(publication.phase(), PublicationPhase::Ready);
    assert_eq!(publication.candidate().unwrap().outbox().len(), 3);

    let store = NativePublicationStore::default();
    let receipt = publication.publish(&store).await.unwrap();
    assert_eq!(receipt.session_revision(), &41);
    assert_eq!(receipt.outbox_count(), 3);
    let published = publication.into_published().await.unwrap();
    assert_eq!(published.results().len(), 1);

    let record = store.record.lock().unwrap();
    let record = record.as_ref().unwrap();
    assert_eq!(record.mutation, 3);
    assert_eq!(record.results, 1);
    assert_eq!(record.items.len(), 3);
    assert_eq!(record.items[0].0.index(), 0);
    assert_eq!(record.items[1].0.index(), 1);
    assert_eq!(record.items[2].0.index(), 2);
    assert!(record
        .items
        .iter()
        .all(|(_, payload)| payload.contains("root:")));
}

#[tokio::test]
async fn duplicate_call_replays_result_without_io_or_effects_and_collision_is_terminal() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    let props = TurnProps { marker: 9 };
    let prepared = prepared(&epoch, &props);
    let live = Arc::new(Mutex::new(LiveTrace::default()));
    let mut attempt = prepared
        .start_provider_attempt(RecordingLiveRuntime {
            trace: Arc::clone(&live),
        })
        .unwrap();

    let first_call =
        call("same", "alpha").with_result_correlation_id("provider-correlation-replay");
    let first = attempt.call_tool(first_call.clone()).await.unwrap();
    let replay = attempt.call_tool(first_call).await.unwrap();
    assert!(!first.replayed());
    assert!(replay.replayed());
    assert_eq!(first.result(), replay.result());
    assert_eq!(
        replay.result().result_correlation_id(),
        Some("provider-correlation-replay")
    );
    assert!(replay.update().is_empty());
    assert_eq!(trace.lock().unwrap().invocations.len(), 1);
    assert_eq!(live.lock().unwrap().effects.len(), 1);

    let collision = attempt.call_tool(call("same", "beta")).await.unwrap_err();
    let ProviderToolAttemptError::InvocationIdCollision(collision) = collision else {
        panic!("expected invocation collision");
    };
    assert_eq!(collision.invocation_id(), "same");
    assert_eq!(
        collision.first().result_correlation_id(),
        Some("provider-correlation-replay")
    );
    assert_eq!(collision.conflicting().result_correlation_id(), None);
    assert!(matches!(
        attempt.call_tool(call("later", "alpha")).await,
        Err(ProviderToolAttemptError::Terminal)
    ));
    let report = attempt.abort(BindingAbortReason::ProviderFailure).await;
    assert_eq!(
        report.live().acknowledgement(),
        Some(LiveEffectAbortAck::CompensationCompleted)
    );
    assert_eq!(trace.lock().unwrap().aborts, ["secondary", "primary"]);
    assert_eq!(report.dispatchers().len(), 2);
    assert!(matches!(
        report.dispatchers()[0].outcome(),
        ProviderDispatcherAbortOutcome::Failed(_)
    ));
    assert!(matches!(
        report.dispatchers()[1].outcome(),
        ProviderDispatcherAbortOutcome::Acknowledged(ProviderDispatcherAbortAck::CleanupCompleted)
    ));
    assert!(!report.cleanup_acknowledged());
}

#[tokio::test]
async fn expected_and_unknown_tool_errors_are_results_while_infrastructure_failure_is_terminal() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    let props = TurnProps { marker: 5 };
    let prepared = prepared(&epoch, &props);
    let live = Arc::new(Mutex::new(LiveTrace::default()));
    let mut attempt = prepared
        .start_provider_attempt(RecordingLiveRuntime { trace: live })
        .unwrap();

    let rejected = ProviderToolCall::new("reject", "alpha", json!({ "reject": true }));
    assert!(attempt
        .call_tool(rejected)
        .await
        .unwrap()
        .result()
        .is_error());
    assert!(attempt
        .call_tool(call("unknown", "not_mounted"))
        .await
        .unwrap()
        .result()
        .is_error());

    let calls_before_malformed = trace.lock().unwrap().invocations.len();
    let invalid_name = ProviderToolCall::new("invalid-name", "not a valid/tool name", json!({}))
        .with_result_correlation_id("provider-invalid-name");
    let invalid_name = attempt.call_tool(invalid_name).await.unwrap();
    assert!(matches!(
        invalid_name.result().response(),
        ProviderToolResponse::Error { code, .. } if code == "unknown_tool"
    ));
    assert_eq!(
        invalid_name.result().result_correlation_id(),
        Some("provider-invalid-name")
    );

    for (id, arguments) in [("scalar", json!(7)), ("array", json!([1, 2]))] {
        let invalid_arguments = ProviderToolCall::new(id, "alpha", arguments);
        assert!(matches!(
            attempt
                .call_tool(invalid_arguments)
                .await
                .unwrap()
                .result()
                .response(),
            ProviderToolResponse::Error { code, .. } if code == "invalid_arguments"
        ));
    }
    assert_eq!(
        trace.lock().unwrap().invocations.len(),
        calls_before_malformed
    );
    let missing_invocation = ProviderToolCall::new("", "alpha", json!({}))
        .with_result_correlation_id("provider-missing-invocation");
    let missing_invocation = attempt.call_tool(missing_invocation).await.unwrap();
    assert!(matches!(
        missing_invocation.result().response(),
        ProviderToolResponse::Error { code, .. } if code == "invalid_invocation_id"
    ));
    assert_eq!(missing_invocation.result().invocation_id(), None);
    assert_eq!(
        missing_invocation.result().result_correlation_id(),
        Some("provider-missing-invocation")
    );

    assert!(!attempt
        .call_tool(call("after-errors", "beta"))
        .await
        .unwrap()
        .result()
        .is_error());

    let infra = ProviderToolCall::new("infra", "alpha", json!({ "infra": true }))
        .with_result_correlation_id("provider-infra");
    let error = attempt.call_tool(infra).await.unwrap_err();
    let ProviderToolAttemptError::Dispatcher(fault) = error else {
        panic!("expected terminal dispatcher failure");
    };
    assert_eq!(fault.origin().invocation_id(), Some("infra"));
    assert_eq!(
        fault.origin().result_correlation_id(),
        Some("provider-infra")
    );
    assert!(matches!(
        attempt.call_tool(call("blocked", "beta")).await,
        Err(ProviderToolAttemptError::Terminal)
    ));
    attempt.abort(BindingAbortReason::ProviderFailure).await;
}

#[derive(Debug)]
struct MoveOnlyOutput(Box<str>);

#[derive(Debug)]
struct MoveOnlyDiagnostic(Box<str>);

struct MoveOnlyChannels;

impl TurnChannels for MoveOnlyChannels {
    type Output = MoveOnlyOutput;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = MoveOnlyDiagnostic;
}

#[derive(Debug)]
struct FinishOnlyDispatcher;

#[async_trait::async_trait]
impl ProviderDispatcher<MoveOnlyChannels> for FinishOnlyDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<MoveOnlyChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("unused"),
            StreamUpdate::new(),
        ))
    }

    async fn finish(
        &mut self,
    ) -> Result<StreamUpdate<TurnEmission<MoveOnlyChannels>, MoveOnlyDiagnostic>, Self::Error> {
        Ok(
            StreamUpdate::from_emission(TurnEmission::Output(MoveOnlyOutput(
                "finish-output".into(),
            )))
            .with_diagnostic(MoveOnlyDiagnostic("finish-diagnostic".into())),
        )
    }
}

fn move_only_system(_: SystemMountContext<'_, ()>) -> SystemView<MoveOnlyChannels, ()> {
    system_view(provider_tool(
        "finish-only",
        tool_spec("finish_only"),
        || FinishOnlyDispatcher,
    ))
}

fn assert_move_only_update(
    update: StreamUpdate<TurnEmission<MoveOnlyChannels>, MoveOnlyDiagnostic>,
) {
    let (emissions, diagnostics) = update.into_parts();
    let mut emissions = emissions.into_iter();
    let Some(TurnEmission::Output(MoveOnlyOutput(output))) = emissions.next() else {
        panic!("expected one move-only finish output");
    };
    assert_eq!(&*output, "finish-output");
    assert!(emissions.next().is_none());

    let mut diagnostics = diagnostics.into_iter();
    let MoveOnlyDiagnostic(diagnostic) = diagnostics.next().expect("finish diagnostic");
    assert_eq!(&*diagnostic, "finish-diagnostic");
    assert!(diagnostics.next().is_none());
}

#[tokio::test]
async fn native_finish_updates_can_move_non_clone_values_before_or_after_publication() {
    let epoch = mount_system_epoch(&(), move_only_system).unwrap();

    let first_turn = epoch.begin_turn("take-before-publication");
    let mut first_finished = first_turn
        .prepare_user(&(), |_| user_view(()))
        .unwrap()
        .start_provider_attempt(NoLiveEffects)
        .unwrap()
        .finish()
        .await
        .unwrap();
    assert_move_only_update(first_finished.take_update());
    assert!(first_finished.update().is_empty());
    let mut publisher = RecordingPublisher::default();
    let first_published = first_finished.publish_with(&mut publisher).await.unwrap();
    assert!(first_published.update().is_empty());

    let second_turn = epoch.begin_turn("take-after-publication");
    let second_finished = second_turn
        .prepare_user(&(), |_| user_view(()))
        .unwrap()
        .start_provider_attempt(NoLiveEffects)
        .unwrap()
        .finish()
        .await
        .unwrap();
    let mut second_published = second_finished.publish_with(&mut publisher).await.unwrap();
    assert_move_only_update(second_published.take_update());
    assert!(second_published.update().is_empty());
}

struct GatedLiveRuntime {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl LiveEffectRuntime<RootLive> for GatedLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        _effect: RootLive,
    ) -> Result<(), Self::Error> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        Ok(if context.applied_effects() == 0 {
            LiveEffectAbortAck::NoEffectsApplied
        } else {
            LiveEffectAbortAck::CompensationCompleted
        })
    }
}

#[tokio::test]
async fn provider_call_does_not_return_until_its_live_effect_is_acknowledged() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    let props = TurnProps { marker: 3 };
    let prepared = prepared(&epoch, &props);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let attempt = prepared
        .start_provider_attempt(GatedLiveRuntime {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })
        .unwrap();

    let task = tokio::spawn(async move {
        let mut attempt = attempt;
        let outcome = attempt.call_tool(call("gated", "alpha")).await;
        (attempt, outcome)
    });
    entered.notified().await;
    assert_eq!(trace.lock().unwrap().invocations.len(), 1);
    assert!(!task.is_finished());
    release.notify_one();

    let (attempt, outcome) = task.await.unwrap();
    assert!(!outcome.unwrap().result().is_error());
    attempt.abort(BindingAbortReason::Cancelled).await;
}

#[derive(Debug, thiserror::Error)]
#[error("forced provider live failure")]
struct ForcedLiveFailure;

struct FailingProviderLiveRuntime;

#[async_trait::async_trait]
impl LiveEffectRuntime<RootLive> for FailingProviderLiveRuntime {
    type Error = ForcedLiveFailure;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        _effect: RootLive,
    ) -> Result<(), Self::Error> {
        Err(ForcedLiveFailure)
    }

    async fn abort(
        &mut self,
        _context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        Ok(LiveEffectAbortAck::NoEffectsApplied)
    }
}

#[tokio::test]
async fn provider_live_failure_retains_the_call_identity_without_changing_xml_origin() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    let props = TurnProps { marker: 13 };
    let prepared = prepared(&epoch, &props);
    let mut attempt = prepared
        .start_provider_attempt(FailingProviderLiveRuntime)
        .unwrap();
    let tool_call =
        call("live-failure", "alpha").with_result_correlation_id("provider-live-failure");

    let error = attempt.call_tool(tool_call).await.unwrap_err();
    let ProviderToolAttemptError::Live(fault) = error else {
        panic!("expected provider-attributed live failure");
    };
    let identity = fault.call_identity().expect("provider call identity");
    assert_eq!(identity.invocation_id(), Some("live-failure"));
    assert_eq!(
        identity.result_correlation_id(),
        Some("provider-live-failure")
    );
    assert_eq!(
        fault.source_fault().context().phase(),
        BindingPhase::Dispatch
    );
    assert_eq!(
        fault.source_fault().context().origin().route().to_string(),
        "provider:alpha"
    );
    attempt.abort(BindingAbortReason::ProviderFailure).await;
}

#[derive(Debug, thiserror::Error)]
#[error("forced publication failure")]
struct ForcedPublicationFailure;

struct RejectingPublisher;

#[async_trait::async_trait]
impl TurnPublisher for RejectingPublisher {
    type Error = ForcedPublicationFailure;

    async fn publish(&mut self, _publication: TurnPublication<'_>) -> Result<(), Self::Error> {
        Err(ForcedPublicationFailure)
    }
}

#[tokio::test]
async fn publication_failure_exposes_results_needed_to_correlate_before_abort() {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let epoch = mount_system_epoch(
        &MountProps {
            trace: Arc::clone(&trace),
        },
        native_system,
    )
    .unwrap();
    let props = TurnProps { marker: 17 };
    let prepared = prepared(&epoch, &props);
    let mut attempt = prepared
        .start_provider_attempt(RecordingLiveRuntime {
            trace: Arc::new(Mutex::new(LiveTrace::default())),
        })
        .unwrap();
    let _ = attempt
        .call_tool(
            call("publish-failure", "alpha").with_result_correlation_id("provider-publish-failure"),
        )
        .await
        .unwrap();
    let finished = attempt.finish().await.unwrap();
    let failure = match finished.publish_with(&mut RejectingPublisher).await {
        Ok(_) => panic!("publisher should reject the attempt"),
        Err(failure) => failure,
    };
    assert_eq!(failure.identity().epoch_id(), epoch.id());
    assert_eq!(failure.results().len(), 1);
    assert_eq!(
        failure.results()[0].result_correlation_id(),
        Some("provider-publish-failure")
    );
    failure.abort(BindingAbortReason::PublishFailure).await;
}

struct CombinedChannels;

impl TurnChannels for CombinedChannels {
    type Output = String;
    type Live = String;
    type Commit = String;
    type Diagnostic = String;
}

struct CombinedMountProps {
    identities: Arc<Mutex<Vec<(&'static str, String)>>>,
}

struct CombinedTurnProps;

#[derive(Debug)]
struct IdentityDispatcher {
    identities: Arc<Mutex<Vec<(&'static str, String)>>>,
}

#[async_trait::async_trait]
impl ProviderDispatcher<CombinedChannels> for IdentityDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<CombinedChannels>, Self::Error> {
        self.identities.lock().unwrap().push((
            "provider-call",
            context.identity().provider_attempt_id().to_string(),
        ));
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("ok"),
            StreamUpdate::new(),
        ))
    }
}

fn combined_system(
    cx: SystemMountContext<'_, CombinedMountProps>,
) -> SystemView<CombinedChannels, CombinedTurnProps> {
    let xml_identities = Arc::clone(&cx.props().identities);
    let streaming = StreamingXml::<TurnEmission<CombinedChannels>, String>::new(XmlNode::new(
        XmlName::try_from("selection").unwrap(),
    ))
    .try_state_with(
        move |cx: &TurnBindingCx<'_, CombinedTurnProps, CombinedChannels>| {
            xml_identities
                .lock()
                .unwrap()
                .push(("xml-init", cx.provider_attempt_id().to_string()));
            Ok::<(), Infallible>(())
        },
    )
    .on_complete(|_, _| vec![TurnEmission::Output("selection".to_owned())])
    .into_component();

    let provider_identities = Arc::clone(&cx.props().identities);
    let provider = provider_tool_with_context(
        "native",
        tool_spec("native_action"),
        move |cx: &ProviderDispatcherCx<'_, CombinedTurnProps, CombinedChannels>| {
            provider_identities.lock().unwrap().push((
                "provider-init",
                cx.identity().provider_attempt_id().to_string(),
            ));
            Ok(IdentityDispatcher {
                identities: Arc::clone(&provider_identities),
            })
        },
    );
    system_view(component((streaming, provider)))
}

#[derive(Debug, Default)]
struct DiscardStringLive;

#[async_trait::async_trait]
impl LiveEffectRuntime<String> for DiscardStringLive {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        _effect: String,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        Ok(if context.applied_effects() == 0 {
            LiveEffectAbortAck::NoEffectsApplied
        } else {
            LiveEffectAbortAck::CompensationCompleted
        })
    }
}

#[tokio::test]
async fn combined_xml_and_native_tools_share_one_attempt_identity_and_effect_scope() {
    let identities = Arc::new(Mutex::new(Vec::new()));
    let epoch = mount_system_epoch(
        &CombinedMountProps {
            identities: Arc::clone(&identities),
        },
        combined_system,
    )
    .unwrap();
    let props = CombinedTurnProps;
    let turn = epoch.begin_turn("combined");
    let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();
    assert!(matches!(
        prepared.start_provider_attempt(DiscardStringLive),
        Err(ProviderAttemptStartError::EventBindingsRequireCombinedAttempt { count: 1 })
    ));

    let mut attempt = prepared.start_streaming_attempt(DiscardStringLive).unwrap();
    assert_eq!(attempt.binding_count(), 1);
    assert_eq!(attempt.dispatcher_count(), 1);
    let identity = attempt.identity().provider_attempt_id().to_string();
    let tool_outcome = attempt
        .call_tool(call("native-1", "native_action"))
        .await
        .unwrap();
    assert!(!tool_outcome.result().is_error());
    let update = attempt
        .on_event(TextTurnEvent::TextComplete("<selection />".to_owned()))
        .await
        .unwrap();
    assert_eq!(
        update.emissions(),
        &[TurnEmission::Output("selection".to_owned())]
    );

    assert_eq!(
        identities.lock().unwrap().as_slice(),
        [
            ("xml-init", identity.clone()),
            ("provider-init", identity.clone()),
            ("provider-call", identity),
        ]
    );
    let report = attempt.abort(BindingAbortReason::Cancelled).await;
    assert_eq!(report.bindings().len(), 1);
    assert_eq!(report.dispatchers().len(), 1);
}
