use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::*,
    component::{
        advanced::{
            experimental::{
                mount, MountPlanError, MountedStreamingAttempt, StreamingAttemptError,
                TurnPublication, TurnPublisher,
            },
            lifecycle::{
                mount_system_epoch, MountedEpoch, SystemMountContext, SystemMountError, SystemView,
            },
            persistence::*,
        },
        system_view, user_view, BindingAbortAck, BindingAbortReason, BindingPhase,
        LiveAbortContext, LiveEffectAbortAck, LiveEffectContext, LiveEffectRuntime, TurnBindingCx,
    },
    prelude::*,
};
use tokio::sync::Notify;

#[derive(Debug, Default, Clone, Copy)]
struct DiscardLiveEffects;

#[async_trait::async_trait]
impl<L> LiveEffectRuntime<L> for DiscardLiveEffects
where
    L: Send + 'static,
{
    type Error = Infallible;

    async fn apply(&mut self, _context: &LiveEffectContext, _effect: L) -> Result<(), Self::Error> {
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

struct RecordedLiveEffect<L> {
    route: String,
    phase: BindingPhase,
    sequence: u64,
    effect: L,
}

struct RecordingLiveState<L> {
    effects: Vec<RecordedLiveEffect<L>>,
    aborts: Vec<LiveAbortContext>,
}

impl<L> Default for RecordingLiveState<L> {
    fn default() -> Self {
        Self {
            effects: Vec::new(),
            aborts: Vec::new(),
        }
    }
}

struct RecordingLiveEffects<L> {
    state: Arc<Mutex<RecordingLiveState<L>>>,
}

fn recording_live_effects<L>() -> (RecordingLiveEffects<L>, Arc<Mutex<RecordingLiveState<L>>>) {
    let state = Arc::new(Mutex::new(RecordingLiveState::default()));
    (
        RecordingLiveEffects {
            state: Arc::clone(&state),
        },
        state,
    )
}

#[async_trait::async_trait]
impl<L> LiveEffectRuntime<L> for RecordingLiveEffects<L>
where
    L: Send + 'static,
{
    type Error = Infallible;

    async fn apply(&mut self, context: &LiveEffectContext, effect: L) -> Result<(), Self::Error> {
        self.state.lock().unwrap().effects.push(RecordedLiveEffect {
            route: context.origin().route().to_string(),
            phase: context.phase(),
            sequence: context.sequence(),
            effect,
        });
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        self.state.lock().unwrap().aborts.push(context.clone());
        Ok(if context.applied_effects() == 0 {
            LiveEffectAbortAck::NoEffectsApplied
        } else {
            LiveEffectAbortAck::CompensationCompleted
        })
    }
}

#[derive(Default)]
struct RecordingPublisher {
    publications: Vec<(String, String)>,
}

#[async_trait::async_trait]
impl TurnPublisher for RecordingPublisher {
    type Error = Infallible;

    async fn publish(&mut self, publication: TurnPublication<'_>) -> Result<(), Self::Error> {
        self.publications.push((
            publication.identity().provider_attempt_id().to_string(),
            publication.raw_output().to_owned(),
        ));
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("publication rejected")]
struct PublicationRejected;

#[derive(Default)]
struct FailingPublisher {
    calls: usize,
}

#[async_trait::async_trait]
impl TurnPublisher for FailingPublisher {
    type Error = PublicationRejected;

    async fn publish(&mut self, _publication: TurnPublication<'_>) -> Result<(), Self::Error> {
        self.calls += 1;
        Err(PublicationRejected)
    }
}

fn contract(name: &str) -> XmlNode {
    XmlNode::new(XmlName::try_from(name).expect("test tag is valid"))
}

fn start_attempt<C>(epoch: &MountedEpoch<C>) -> MountedStreamingAttempt<C>
where
    C: TurnChannels,
{
    let turn = epoch.begin_turn("test-turn");
    turn.prepare_user(&(), |_| user_view(()))
        .unwrap()
        .start_streaming_attempt(DiscardLiveEffects)
        .unwrap()
}

struct CounterChannels;

impl TurnChannels for CounterChannels {
    type Output = usize;
    type Live = String;
    type Commit = usize;
    type Diagnostic = String;
}

struct CounterCommitStager;

impl CommitStager<CounterChannels> for CounterCommitStager {
    type Payload = usize;
    type Error = Infallible;

    fn stage(
        &self,
        _context: CommitStagingContext<'_>,
        commit: &usize,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        Ok(StagedCommit::new(
            CommitContract::new("test.counter", 1).unwrap(),
            *commit,
        ))
    }
}

struct CounterFingerprintFactory;

impl PublicationFingerprintFactory<usize, usize, u64> for CounterFingerprintFactory {
    type Error = DurableKeyError;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<'_, usize, usize, u64>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        PublicationCandidateFingerprint::new(format!(
            "counter-v1|request={}|revision={}|mutation={}|raw={:?}|results={:?}|outbox={:?}",
            context.request_id(),
            context.expected_revision(),
            context.mutation().value(),
            context.raw_output(),
            context.provider_results(),
            context.outbox(),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("fingerprint policy rejected the staged candidate")]
struct FingerprintPolicyError;

struct RejectingFingerprintFactory;

impl PublicationFingerprintFactory<usize, usize, u64> for RejectingFingerprintFactory {
    type Error = FingerprintPolicyError;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<'_, usize, usize, u64>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        assert_eq!(context.outbox().items()[0].payload(), &1);
        Err(FingerprintPolicyError)
    }
}

#[derive(Debug)]
struct CounterPublicationRecord {
    mutation: usize,
    payloads: Vec<usize>,
    receipt: PublicationReceipt<u64>,
}

#[derive(Default)]
struct CounterPublicationStore {
    record: Mutex<Option<CounterPublicationRecord>>,
}

#[derive(Debug, thiserror::Error)]
enum CounterPublicationStoreError {
    #[error("counter publication store already contains a different request id")]
    DifferentRequestId,

    #[error("counter publication request id was reused with a different fingerprint")]
    FingerprintCollision,
}

#[async_trait::async_trait]
impl PublicationStore<usize, usize> for CounterPublicationStore {
    type Version = u64;
    type Error = CounterPublicationStoreError;

    async fn publish(
        &self,
        request: PublicationRequest<'_, usize, usize, u64>,
    ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
        let mut record = self.record.lock().unwrap();
        if let Some(record) = record.as_ref() {
            return if record.receipt.request_id() != request.request_id() {
                Err(PublicationWriteError::Rejected(
                    CounterPublicationStoreError::DifferentRequestId,
                ))
            } else if record.receipt.fingerprint().as_bytes() != request.fingerprint().as_bytes() {
                Err(PublicationWriteError::Rejected(
                    CounterPublicationStoreError::FingerprintCollision,
                ))
            } else {
                Ok(record.receipt.clone())
            };
        }
        let receipt = PublicationReceipt::new(
            request.request_id().clone(),
            request.fingerprint().clone(),
            PublicationId::new("counter-publication-1").unwrap(),
            request.expected_revision() + 1,
            u64::try_from(request.outbox().len()).unwrap(),
        );
        *record = Some(CounterPublicationRecord {
            mutation: *request.mutation().value(),
            payloads: request
                .outbox()
                .items()
                .iter()
                .map(|item| *item.payload())
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
                    CounterPublicationStoreError::FingerprintCollision,
                ))
            }
            Some(_) | None => Ok(PublicationResolution::NotCommitted),
        }
    }
}

#[derive(Debug)]
struct CounterState {
    value: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for CounterState {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct CounterMountProps {
    initializations: Arc<AtomicUsize>,
    finishes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

fn mounted_counter(props: &CounterMountProps) -> Component<CounterChannels> {
    let initializations = Arc::clone(&props.initializations);
    let drops = Arc::clone(&props.drops);
    let finishes = Arc::clone(&props.finishes);
    StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("counter"))
        .state_with(move || {
            initializations.fetch_add(1, Ordering::SeqCst);
            CounterState {
                value: 0,
                drops: Arc::clone(&drops),
            }
        })
        .on_open(|state, _| vec![TurnEmission::Live(format!("opened:{}", state.value))])
        .on_complete(|state, _| {
            state.value += 1;
            vec![TurnEmission::Output(state.value)]
        })
        .on_finish(move |state| {
            finishes.fetch_add(1, Ordering::SeqCst);
            vec![TurnEmission::Commit(state.value)]
        })
        .into_component()
}

fn counter_system(cx: SystemMountContext<'_, CounterMountProps>) -> SystemView<CounterChannels> {
    system_view(mounted_counter(cx.props()))
}

#[tokio::test]
async fn mounted_factory_is_lazy_and_creates_fresh_state_for_every_attempt() {
    let initializations = Arc::new(AtomicUsize::new(0));
    let finishes = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &CounterMountProps {
            initializations: Arc::clone(&initializations),
            finishes: Arc::clone(&finishes),
            drops: Arc::clone(&drops),
        },
        counter_system,
    )
    .unwrap();

    assert_eq!(initializations.load(Ordering::SeqCst), 0);
    epoch.prepare_user(&(), |_| user_view(())).unwrap();
    assert_eq!(initializations.load(Ordering::SeqCst), 0);

    let mut first = start_attempt(&epoch);
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    let first_update = first
        .on_event(TextTurnEvent::TextDelta("<counter />".to_owned()))
        .await
        .unwrap();
    assert_eq!(first_update.emissions(), &[TurnEmission::Output(1)]);
    let finished = first.finish_stream().await.unwrap();
    assert!(finished.update().emissions().is_empty());
    assert_eq!(finishes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let report = finished.abort(BindingAbortReason::PublishFailure).await;
    assert_eq!(report.bindings().len(), 1);
    assert_eq!(
        report.bindings()[0].acknowledgement(),
        BindingAbortAck::NoCleanupNeeded
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let mut second = start_attempt(&epoch);
    assert_eq!(initializations.load(Ordering::SeqCst), 2);
    let second_update = second
        .on_event(TextTurnEvent::TextDelta("<counter />".to_owned()))
        .await
        .unwrap();
    assert_eq!(second_update.emissions(), &[TurnEmission::Output(1)]);
    second.abort(BindingAbortReason::Cancelled).await;
    assert_eq!(finishes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn streaming_commit_is_staged_before_atomic_publication_and_not_released_afterward() {
    let epoch = mount_system_epoch(
        &CounterMountProps {
            initializations: Arc::new(AtomicUsize::new(0)),
            finishes: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        },
        counter_system,
    )
    .unwrap();
    let mut attempt = start_attempt(&epoch);
    let _ = attempt
        .on_event(TextTurnEvent::TextDelta("<counter />".to_owned()))
        .await
        .unwrap();
    let finished = attempt.finish_stream().await.unwrap();

    let mut publication = finished
        .stage_durable_publication(
            PublicationRequestId::new("counter-session/turn-1").unwrap(),
            6_u64,
            PreparedSessionMutation::new(17_usize),
            &CounterCommitStager,
            &CounterFingerprintFactory,
        )
        .unwrap();
    assert_eq!(publication.phase(), PublicationPhase::Ready);
    assert_eq!(publication.candidate().unwrap().outbox().len(), 1);
    assert_eq!(
        publication.candidate().unwrap().outbox().items()[0].payload(),
        &1
    );

    let store = CounterPublicationStore::default();
    let receipt = publication.publish(&store).await.unwrap();
    assert_eq!(receipt.session_revision(), &7);
    assert_eq!(receipt.outbox_count(), 1);
    let published = publication.into_published().await.unwrap();
    assert_eq!(published.receipt(), &receipt);
    assert_eq!(published.raw_output(), "<counter />");

    let record = store.record.lock().unwrap();
    let record = record.as_ref().unwrap();
    assert_eq!(record.mutation, 17);
    assert_eq!(record.payloads, [1]);
}

#[tokio::test]
async fn fingerprint_failure_returns_finished_attempt_and_unchanged_staging_plan() {
    let epoch = mount_system_epoch(
        &CounterMountProps {
            initializations: Arc::new(AtomicUsize::new(0)),
            finishes: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        },
        counter_system,
    )
    .unwrap();
    let mut attempt = start_attempt(&epoch);
    let _ = attempt
        .on_event(TextTurnEvent::TextDelta("<counter />".to_owned()))
        .await
        .unwrap();
    let finished = attempt.finish_stream().await.unwrap();

    let failure = finished
        .stage_durable_publication(
            PublicationRequestId::new("counter-session/retry-1").unwrap(),
            9_u64,
            PreparedSessionMutation::new(23_usize),
            &CounterCommitStager,
            &RejectingFingerprintFactory,
        )
        .unwrap_err();
    assert!(matches!(
        failure.source_error(),
        PublicationStagingFailure::Fingerprint(FingerprintPolicyError)
    ));
    assert_eq!(
        failure.plan().request_id().as_str(),
        "counter-session/retry-1"
    );
    assert_eq!(failure.plan().expected_revision(), &9);
    assert_eq!(failure.plan().mutation().value(), &23);

    let (finished, plan, _source) = failure.into_parts();
    let (request_id, expected_revision, mutation) = plan.into_parts();
    let staged = finished
        .stage_durable_publication(
            request_id,
            expected_revision,
            mutation,
            &CounterCommitStager,
            &CounterFingerprintFactory,
        )
        .unwrap();
    assert_eq!(
        staged.candidate().unwrap().request_id().as_str(),
        "counter-session/retry-1"
    );
    let finished = staged.into_finished_if_ready().unwrap();
    let _ = finished.abort(BindingAbortReason::Cancelled).await;
}

fn ordered_factory(name: &'static str) -> Component<CounterChannels> {
    StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract(name))
        .state_with(|| ())
        .on_complete(move |_, _| vec![TurnEmission::Live(name.to_owned())])
        .into_component()
}

fn ordered_system(_: SystemMountContext<'_, ()>) -> SystemView<CounterChannels> {
    system_view(component((
        mount("First", ordered_factory("a")),
        mount("Second", ordered_factory("b")),
    )))
}

#[tokio::test]
async fn one_shared_parser_preserves_wire_order_across_routes() {
    let epoch = mount_system_epoch(&(), ordered_system).unwrap();
    let (runtime, live) = recording_live_effects();
    let turn = epoch.begin_turn("wire-order");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();

    let update = attempt
        .on_event(TextTurnEvent::TextDelta("<b /><a />".to_owned()))
        .await
        .unwrap();
    assert!(update.emissions().is_empty());
    let live = live.lock().unwrap();
    assert_eq!(
        live.effects
            .iter()
            .map(|delivery| delivery.effect.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
    assert_eq!(
        live.effects
            .iter()
            .map(|delivery| delivery.sequence)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(live
        .effects
        .iter()
        .all(|delivery| delivery.phase == BindingPhase::Complete));
}

struct GatedLiveEffects {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl LiveEffectRuntime<String> for GatedLiveEffects {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        _effect: String,
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

struct BackpressureMountProps {
    later_callbacks: Arc<AtomicUsize>,
}

fn backpressure_system(
    cx: SystemMountContext<'_, BackpressureMountProps>,
) -> SystemView<CounterChannels> {
    let first = ordered_factory("b");
    let later_callbacks = Arc::clone(&cx.props().later_callbacks);
    let later = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("a"))
        .state_with(|| ())
        .on_complete(move |_, _| {
            later_callbacks.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
        .into_component();
    system_view(component((mount("First", first), mount("Later", later))))
}

#[tokio::test]
async fn live_runtime_is_awaited_before_the_next_tag_in_the_same_chunk() {
    let later_callbacks = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &BackpressureMountProps {
            later_callbacks: Arc::clone(&later_callbacks),
        },
        backpressure_system,
    )
    .unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let turn = epoch.begin_turn("backpressure");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared
        .start_streaming_attempt(GatedLiveEffects {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        })
        .unwrap();

    let event = tokio::spawn(async move {
        let result = attempt
            .on_event(TextTurnEvent::TextDelta("<b /><a />".to_owned()))
            .await;
        (attempt, result)
    });
    entered.notified().await;
    assert_eq!(later_callbacks.load(Ordering::SeqCst), 0);

    release.notify_one();
    let (attempt, result) = event.await.unwrap();
    assert!(result.unwrap().emissions().is_empty());
    assert_eq!(later_callbacks.load(Ordering::SeqCst), 1);
    attempt.abort(BindingAbortReason::Cancelled).await;
}

fn phrase_system(_: SystemMountContext<'_, ()>) -> SystemView<CounterChannels> {
    system_view(
        StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("phrase"))
            .state_with(|| 0usize)
            .on_stream(|emitted, element| {
                let delta = element.content[*emitted..].to_owned();
                *emitted = element.content.len();
                vec![TurnEmission::Live(delta)]
            })
            .on_complete(|emitted, element| {
                let delta = element.content[*emitted..].to_owned();
                *emitted = element.content.len();
                StreamUpdate::new()
                    .with_emission(TurnEmission::Live(delta))
                    .with_emission(TurnEmission::Output(element.content.len()))
            })
            .into_component(),
    )
}

#[tokio::test]
async fn closing_chunk_emits_its_append_before_the_close_output() {
    let epoch = mount_system_epoch(&(), phrase_system).unwrap();
    let (runtime, live) = recording_live_effects();
    let turn = epoch.begin_turn("phrase");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();

    let first = attempt
        .on_event(TextTurnEvent::TextDelta("<phrase>Hello".to_owned()))
        .await
        .unwrap();
    assert!(first.emissions().is_empty());

    let closing = attempt
        .on_event(TextTurnEvent::TextDelta(" there</phrase>".to_owned()))
        .await
        .unwrap();
    assert_eq!(
        closing.emissions(),
        &[TurnEmission::Output("Hello there".len())]
    );
    let live = live.lock().unwrap();
    assert_eq!(
        live.effects
            .iter()
            .map(|delivery| delivery.effect.as_str())
            .collect::<Vec<_>>(),
        ["Hello", " there"]
    );
    assert_eq!(live.effects[0].phase, BindingPhase::Stream);
    assert_eq!(live.effects[1].phase, BindingPhase::Complete);
}

struct LocalChannels;

impl TurnChannels for LocalChannels {
    type Output = u8;
    type Live = String;
    type Commit = u16;
    type Diagnostic = &'static str;
}

struct RootChannels;

#[derive(Debug, PartialEq, Eq)]
enum RootOutput {
    Parsed(u8),
}

#[derive(Debug, PartialEq, Eq)]
enum RootLive {
    Append(String),
}

#[derive(Debug, PartialEq, Eq)]
enum RootCommit {
    Save(u16),
}

#[derive(Debug, PartialEq, Eq)]
enum RootDiagnostic {
    Parse(&'static str),
}

impl TurnChannels for RootChannels {
    type Output = RootOutput;
    type Live = RootLive;
    type Commit = RootCommit;
    type Diagnostic = RootDiagnostic;
}

fn local_multi_lane() -> Component<LocalChannels> {
    StreamingXml::<TurnEmission<LocalChannels>, &'static str>::new(contract("mapped"))
        .state_with(|| ())
        .on_complete(|_, _| {
            StreamUpdate::new()
                .with_emission(TurnEmission::Live("now".to_owned()))
                .with_emission(TurnEmission::Output(7))
                .with_emission(TurnEmission::Commit(11))
                .with_diagnostic("observed")
        })
        .into_component()
}

fn mapped_system(_: SystemMountContext<'_, ()>) -> SystemView<RootChannels> {
    system_view(local_multi_lane().map_channels(TurnChannelMap::new(
        RootOutput::Parsed,
        RootLive::Append,
        RootCommit::Save,
        RootDiagnostic::Parse,
    )))
}

#[tokio::test]
async fn mounted_instance_maps_every_lane_after_factory_instantiation() {
    let epoch = mount_system_epoch(&(), mapped_system).unwrap();
    let (runtime, live) = recording_live_effects();
    let turn = epoch.begin_turn("mapped");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();
    let update = attempt
        .on_event(TextTurnEvent::TextDelta("<mapped />".to_owned()))
        .await
        .unwrap();

    assert_eq!(
        update.emissions(),
        &[TurnEmission::Output(RootOutput::Parsed(7))]
    );
    assert_eq!(update.diagnostics(), &[RootDiagnostic::Parse("observed")]);
    {
        let live = live.lock().unwrap();
        assert_eq!(live.effects.len(), 1);
        assert_eq!(live.effects[0].route, "xml:mapped");
        assert_eq!(live.effects[0].effect, RootLive::Append("now".to_owned()));
    }

    let finished = attempt.finish_stream().await.unwrap();
    let mut publisher = RecordingPublisher::default();
    let published = finished.publish_with(&mut publisher).await.unwrap();
    assert_eq!(published.pending_commits(), &[RootCommit::Save(11)]);
}

#[tokio::test]
async fn publication_failure_keeps_commits_private_and_can_compensate_live_effects() {
    let epoch = mount_system_epoch(&(), mapped_system).unwrap();
    let (runtime, _live) = recording_live_effects();
    let turn = epoch.begin_turn("publish-failure");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();
    let update = attempt
        .on_event(TextTurnEvent::TextDelta("<mapped />".to_owned()))
        .await
        .unwrap();
    assert_eq!(
        update.emissions(),
        &[TurnEmission::Output(RootOutput::Parsed(7))]
    );

    let finished = attempt.finish_stream().await.unwrap();
    let identity = finished.identity().clone();
    let mut publisher = FailingPublisher::default();
    let failure = finished.publish_with(&mut publisher).await.unwrap_err();
    assert_eq!(failure.identity(), &identity);
    assert!(failure.provider_results().is_empty());
    assert_eq!(publisher.calls, 1);

    let report = failure.abort(BindingAbortReason::PublishFailure).await;
    assert_eq!(report.identity(), &identity);
    assert_eq!(
        report.live().acknowledgement(),
        Some(LiveEffectAbortAck::CompensationCompleted)
    );
}

struct AbortProps {
    completed: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

fn abort_system(cx: SystemMountContext<'_, AbortProps>) -> SystemView<CounterChannels> {
    let completed = Arc::clone(&cx.props().completed);
    let finished = Arc::clone(&cx.props().finished);
    system_view(
        StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("abortable"))
            .state_with(|| ())
            .on_complete(move |_, _| {
                completed.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            })
            .on_finish(move |_| {
                finished.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            })
            .into_component(),
    )
}

#[tokio::test]
async fn abort_discards_incomplete_parser_input_without_complete_or_finish() {
    let completed = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &AbortProps {
            completed: Arc::clone(&completed),
            finished: Arc::clone(&finished),
        },
        abort_system,
    )
    .unwrap();
    let mut attempt = start_attempt(&epoch);
    let _ = attempt
        .on_event(TextTurnEvent::TextDelta("<abortable>partial".to_owned()))
        .await
        .unwrap();

    let report = attempt.abort(BindingAbortReason::ProviderFailure).await;
    assert_eq!(report.bindings().len(), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    assert_eq!(finished.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_logical_turn_gives_each_retry_fresh_attempt_and_live_scope_ids() {
    let epoch = mount_system_epoch(&(), ordered_system).unwrap();
    let turn = epoch.begin_turn("same-label");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let first = prepared
        .start_streaming_attempt(DiscardLiveEffects)
        .unwrap();
    let first_identity = first.identity().clone();
    first.abort(BindingAbortReason::ProviderFailure).await;

    let second = prepared
        .start_streaming_attempt(DiscardLiveEffects)
        .unwrap();
    assert_eq!(first_identity.epoch_id(), second.identity().epoch_id());
    assert_eq!(
        first_identity.turn_instance_id(),
        second.identity().turn_instance_id()
    );
    assert_ne!(
        first_identity.provider_attempt_id(),
        second.identity().provider_attempt_id()
    );
    assert_ne!(
        first_identity.live_scope_id(),
        second.identity().live_scope_id()
    );
    second.abort(BindingAbortReason::Cancelled).await;
}

#[derive(Debug, thiserror::Error)]
#[error("test binding failure")]
struct TestBindingFailure;

#[derive(Debug)]
struct ContextObservation {
    value: usize,
    epoch: String,
    turn: String,
    attempt: String,
    live_scope: String,
    binding: String,
    route: String,
    call_label: String,
}

struct ContextMountProps {
    observations: Arc<Mutex<Vec<ContextObservation>>>,
}

struct ContextTurnProps {
    value: usize,
    secret: &'static str,
}

fn contextual_system(
    cx: SystemMountContext<'_, ContextMountProps>,
) -> SystemView<CounterChannels, ContextTurnProps> {
    let observations = Arc::clone(&cx.props().observations);
    system_view(
        StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("contextual"))
            .try_state_with(
                move |turn: &TurnBindingCx<'_, ContextTurnProps, CounterChannels>| {
                    observations.lock().unwrap().push(ContextObservation {
                        value: turn.props().value,
                        epoch: turn.epoch_id().to_string(),
                        turn: turn.turn_instance_id().to_string(),
                        attempt: turn.provider_attempt_id().to_string(),
                        live_scope: turn.live_scope_id().to_string(),
                        binding: turn.binding_id().to_string(),
                        route: turn.route().to_string(),
                        call_label: turn.call_label().to_owned(),
                    });
                    Ok::<_, TestBindingFailure>(turn.props().value)
                },
            )
            .on_complete(|value, _| vec![TurnEmission::Output(*value)])
            .into_component(),
    )
}

#[tokio::test]
async fn typed_turn_props_and_runtime_identity_exist_only_at_attempt_start() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let epoch = mount_system_epoch(
        &ContextMountProps {
            observations: Arc::clone(&observations),
        },
        contextual_system,
    )
    .unwrap();
    let props = ContextTurnProps {
        value: 41,
        secret: "must-not-enter-system",
    };

    assert_eq!(epoch.rendered_system(), "<contextual />");
    assert!(!epoch.rendered_system().contains(props.secret));
    let turn = epoch.begin_turn("reused-label");
    let discarded_props = ContextTurnProps {
        value: 17,
        secret: "discarded",
    };
    let discarded = turn
        .prepare_user(&discarded_props, |_| user_view(()))
        .unwrap();
    drop(discarded);
    assert!(observations.lock().unwrap().is_empty());

    let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();
    let mut first = prepared
        .start_streaming_attempt(DiscardLiveEffects)
        .unwrap();
    let first_update = first
        .on_event(TextTurnEvent::TextDelta("<contextual />".to_owned()))
        .await
        .unwrap();
    assert_eq!(first_update.emissions(), &[TurnEmission::Output(41)]);
    first.abort(BindingAbortReason::ProviderFailure).await;

    let second = prepared
        .start_streaming_attempt(DiscardLiveEffects)
        .unwrap();
    second.abort(BindingAbortReason::Cancelled).await;

    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].value, 41);
    assert_eq!(observations[0].epoch, observations[1].epoch);
    assert_eq!(observations[0].turn, observations[1].turn);
    assert_ne!(observations[0].attempt, observations[1].attempt);
    assert_ne!(observations[0].live_scope, observations[1].live_scope);
    assert_eq!(observations[0].route, "xml:contextual");
    assert!(observations[0].binding.ends_with("::stream"));
    assert_eq!(observations[0].call_label, "reused-label");
}

struct InitFailureMountProps {
    first_initializations: Arc<AtomicUsize>,
    first_aborts: Arc<AtomicUsize>,
}

struct InitFailureTurnProps;

fn init_failure_system(
    cx: SystemMountContext<'_, InitFailureMountProps>,
) -> SystemView<CounterChannels, InitFailureTurnProps> {
    let first_initializations = Arc::clone(&cx.props().first_initializations);
    let first_aborts = Arc::clone(&cx.props().first_aborts);
    let first = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("first"))
        .state_with(move || {
            first_initializations.fetch_add(1, Ordering::SeqCst);
        })
        .on_abort(move |_, reason| {
            assert_eq!(reason, BindingAbortReason::InitializationFailure);
            first_aborts.fetch_add(1, Ordering::SeqCst);
            BindingAbortAck::LocalCleanupCompleted
        })
        .into_component();
    let second = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("second"))
        .try_state_with(
            |_: &TurnBindingCx<'_, InitFailureTurnProps, CounterChannels>| {
                Err::<(), _>(TestBindingFailure)
            },
        )
        .into_component();
    system_view(component((mount("First", first), mount("Second", second))))
}

#[test]
fn factory_initialization_is_atomic_and_failure_has_binding_origin() {
    let first_initializations = Arc::new(AtomicUsize::new(0));
    let first_aborts = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &InitFailureMountProps {
            first_initializations: Arc::clone(&first_initializations),
            first_aborts: Arc::clone(&first_aborts),
        },
        init_failure_system,
    )
    .unwrap();

    let props = InitFailureTurnProps;
    let turn = epoch.begin_turn("init-failure");
    let prepared = turn.prepare_user(&props, |_| user_view(())).unwrap();
    let error = prepared
        .start_streaming_attempt(DiscardLiveEffects)
        .unwrap_err();
    let error = error.binding_fault().expect("XML initialization failed");
    assert_eq!(error.phase(), BindingPhase::Initialize);
    assert_eq!(error.origin().route().to_string(), "xml:second");
    assert!(error
        .origin()
        .binding_id()
        .to_string()
        .ends_with("::stream"));
    assert_eq!(first_initializations.load(Ordering::SeqCst), 1);
    assert_eq!(first_aborts.load(Ordering::SeqCst), 1);
}

struct ReducerFailureMountProps {
    later_callbacks: Arc<AtomicUsize>,
}

fn reducer_failure_system(
    cx: SystemMountContext<'_, ReducerFailureMountProps>,
) -> SystemView<CounterChannels> {
    let later_callbacks = Arc::clone(&cx.props().later_callbacks);
    let failing = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("b"))
        .state_with(|| ())
        .try_on_complete(
            |_, _| -> Result<Vec<TurnEmission<CounterChannels>>, TestBindingFailure> {
                Err(TestBindingFailure)
            },
        )
        .into_component();
    let later = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("a"))
        .state_with(|| ())
        .on_complete(move |_, _| {
            later_callbacks.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
        .into_component();
    system_view(component((
        mount("Failing", failing),
        mount("Later", later),
    )))
}

#[tokio::test]
async fn reducer_failure_is_terminal_and_stops_later_tags_in_the_same_chunk() {
    let later_callbacks = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &ReducerFailureMountProps {
            later_callbacks: Arc::clone(&later_callbacks),
        },
        reducer_failure_system,
    )
    .unwrap();
    let mut attempt = start_attempt(&epoch);

    let error = attempt
        .on_event(TextTurnEvent::TextDelta("<b /><a />".to_owned()))
        .await
        .unwrap_err();
    let StreamingAttemptError::Binding(fault) = error else {
        panic!("expected an attributed binding failure");
    };
    assert_eq!(fault.phase(), BindingPhase::Complete);
    assert_eq!(fault.origin().route().to_string(), "xml:b");
    assert_eq!(later_callbacks.load(Ordering::SeqCst), 0);
    assert!(matches!(
        attempt
            .on_event(TextTurnEvent::TextDelta("<a />".to_owned()))
            .await,
        Err(StreamingAttemptError::Terminal)
    ));
    attempt.abort(BindingAbortReason::ParserFailure).await;
}

#[derive(Debug, thiserror::Error)]
#[error("test live runtime failure")]
struct TestLiveRuntimeFailure;

struct FailingLiveEffects {
    order: Arc<Mutex<Vec<&'static str>>>,
    abort_context: Arc<Mutex<Option<LiveAbortContext>>>,
}

struct AbortFailingLiveEffects {
    order: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl LiveEffectRuntime<String> for AbortFailingLiveEffects {
    type Error = TestLiveRuntimeFailure;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        _effect: String,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn abort(
        &mut self,
        _context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        self.order.lock().unwrap().push("live-abort-failed");
        Err(TestLiveRuntimeFailure)
    }
}

#[async_trait::async_trait]
impl LiveEffectRuntime<String> for FailingLiveEffects {
    type Error = TestLiveRuntimeFailure;

    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        _effect: String,
    ) -> Result<(), Self::Error> {
        if context.sequence() == 1 {
            return Err(TestLiveRuntimeFailure);
        }
        self.order.lock().unwrap().push("live-apply");
        Ok(())
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        self.order.lock().unwrap().push("live-abort");
        *self.abort_context.lock().unwrap() = Some(context.clone());
        Ok(LiveEffectAbortAck::CompensationCompleted)
    }
}

struct LiveFailureMountProps {
    later_callbacks: Arc<AtomicUsize>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

fn live_failure_system(
    cx: SystemMountContext<'_, LiveFailureMountProps>,
) -> SystemView<CounterChannels> {
    let first_abort_order = Arc::clone(&cx.props().order);
    let failing = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("b"))
        .state_with(|| ())
        .on_complete(|_, _| {
            StreamUpdate::new()
                .with_emission(TurnEmission::Live("first".to_owned()))
                .with_emission(TurnEmission::Live("second".to_owned()))
                .with_emission(TurnEmission::Output(99))
        })
        .on_abort(move |_, _| {
            first_abort_order.lock().unwrap().push("binding-b-abort");
            BindingAbortAck::LocalCleanupCompleted
        })
        .into_component();
    let later_callbacks = Arc::clone(&cx.props().later_callbacks);
    let later_abort_order = Arc::clone(&cx.props().order);
    let later = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("a"))
        .state_with(|| ())
        .on_complete(move |_, _| {
            later_callbacks.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
        .on_abort(move |_, _| {
            later_abort_order.lock().unwrap().push("binding-a-abort");
            BindingAbortAck::LocalCleanupCompleted
        })
        .into_component();
    system_view(component((
        mount("Failing", failing),
        mount("Later", later),
    )))
}

#[tokio::test]
async fn live_failure_is_terminal_and_abort_compensates_before_local_teardown() {
    let later_callbacks = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let abort_context = Arc::new(Mutex::new(None));
    let epoch = mount_system_epoch(
        &LiveFailureMountProps {
            later_callbacks: Arc::clone(&later_callbacks),
            order: Arc::clone(&order),
        },
        live_failure_system,
    )
    .unwrap();
    let turn = epoch.begin_turn("live-failure");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared
        .start_streaming_attempt(FailingLiveEffects {
            order: Arc::clone(&order),
            abort_context: Arc::clone(&abort_context),
        })
        .unwrap();

    let error = attempt
        .on_event(TextTurnEvent::TextDelta("<b /><a />".to_owned()))
        .await
        .unwrap_err();
    let StreamingAttemptError::Live(fault) = error else {
        panic!("expected an attributed live runtime failure");
    };
    assert_eq!(fault.context().origin().route().to_string(), "xml:b");
    assert_eq!(fault.context().phase(), BindingPhase::Complete);
    assert_eq!(fault.context().sequence(), 1);
    assert_eq!(later_callbacks.load(Ordering::SeqCst), 0);

    let report = attempt.abort(BindingAbortReason::ParserFailure).await;
    assert_eq!(
        report.live().acknowledgement(),
        Some(LiveEffectAbortAck::CompensationCompleted)
    );
    assert!(report
        .bindings()
        .iter()
        .all(|binding| { binding.acknowledgement() == BindingAbortAck::LocalCleanupCompleted }));
    let abort_context = abort_context.lock().unwrap().clone().unwrap();
    assert_eq!(abort_context.applied_effects(), 1);
    assert_eq!(
        order.lock().unwrap().as_slice(),
        [
            "live-apply",
            "live-abort",
            "binding-b-abort",
            "binding-a-abort"
        ]
    );
}

#[tokio::test]
async fn live_compensation_failure_still_runs_every_local_abort_reducer() {
    let later_callbacks = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let epoch = mount_system_epoch(
        &LiveFailureMountProps {
            later_callbacks,
            order: Arc::clone(&order),
        },
        live_failure_system,
    )
    .unwrap();
    let turn = epoch.begin_turn("abort-failure");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared
        .start_streaming_attempt(AbortFailingLiveEffects {
            order: Arc::clone(&order),
        })
        .unwrap();
    let _ = attempt
        .on_event(TextTurnEvent::TextDelta("<b />".to_owned()))
        .await
        .unwrap();

    let report = attempt.abort(BindingAbortReason::Cancelled).await;
    assert!(report.live().failure().is_some());
    assert!(report
        .bindings()
        .iter()
        .all(|binding| { binding.acknowledgement() == BindingAbortAck::LocalCleanupCompleted }));
    assert_eq!(
        order.lock().unwrap().as_slice(),
        ["live-abort-failed", "binding-b-abort", "binding-a-abort"]
    );
}

struct StrictFinishMountProps {
    finishes: Arc<AtomicUsize>,
    aborts_with_state: Arc<AtomicUsize>,
}

fn strict_finish_system(
    cx: SystemMountContext<'_, StrictFinishMountProps>,
) -> SystemView<CounterChannels> {
    let finishes = Arc::clone(&cx.props().finishes);
    let aborts_with_state = Arc::clone(&cx.props().aborts_with_state);
    system_view(
        StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("strict"))
            .state_with(|| 7usize)
            .on_finish(move |_| {
                finishes.fetch_add(1, Ordering::SeqCst);
                vec![TurnEmission::Commit(7)]
            })
            .on_abort(move |state, _| {
                if *state == 7 {
                    aborts_with_state.fetch_add(1, Ordering::SeqCst);
                }
                BindingAbortAck::LocalCleanupCompleted
            })
            .into_component(),
    )
}

#[tokio::test]
async fn strict_eof_failure_skips_finish_and_keeps_state_for_abort() {
    let finishes = Arc::new(AtomicUsize::new(0));
    let aborts_with_state = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &StrictFinishMountProps {
            finishes: Arc::clone(&finishes),
            aborts_with_state: Arc::clone(&aborts_with_state),
        },
        strict_finish_system,
    )
    .unwrap();
    let mut attempt = start_attempt(&epoch);
    let _ = attempt
        .on_event(TextTurnEvent::TextDelta("<strict>partial".to_owned()))
        .await
        .unwrap();

    let failure = attempt.finish_stream().await.unwrap_err();
    let StreamingAttemptError::Binding(fault) = failure.error() else {
        panic!("expected strict EOF binding failure");
    };
    assert_eq!(fault.phase(), BindingPhase::Finalize);
    assert_eq!(finishes.load(Ordering::SeqCst), 0);
    let report = failure.abort(BindingAbortReason::ParserFailure).await;
    assert_eq!(
        report.bindings()[0].acknowledgement(),
        BindingAbortAck::LocalCleanupCompleted
    );
    assert_eq!(aborts_with_state.load(Ordering::SeqCst), 1);
}

struct FinishFailureMountProps {
    aborts_with_finished_state: Arc<AtomicUsize>,
}

fn finish_failure_system(
    cx: SystemMountContext<'_, FinishFailureMountProps>,
) -> SystemView<CounterChannels> {
    let aborts_with_finished_state = Arc::clone(&cx.props().aborts_with_finished_state);
    system_view(
        StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("finish_fails"))
            .state_with(|| 0usize)
            .try_on_finish(
                |state| -> Result<Vec<TurnEmission<CounterChannels>>, TestBindingFailure> {
                    *state = 9;
                    Err(TestBindingFailure)
                },
            )
            .on_abort(move |state, _| {
                if *state == 9 {
                    aborts_with_finished_state.fetch_add(1, Ordering::SeqCst);
                }
                BindingAbortAck::LocalCleanupCompleted
            })
            .into_component(),
    )
}

#[tokio::test]
async fn finish_failure_keeps_mutated_state_available_to_abort() {
    let aborts_with_finished_state = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &FinishFailureMountProps {
            aborts_with_finished_state: Arc::clone(&aborts_with_finished_state),
        },
        finish_failure_system,
    )
    .unwrap();
    let mut attempt = start_attempt(&epoch);
    let _ = attempt
        .on_event(TextTurnEvent::TextDelta("<finish_fails />".to_owned()))
        .await
        .unwrap();

    let failure = attempt.finish_stream().await.unwrap_err();
    let StreamingAttemptError::Binding(fault) = failure.error() else {
        panic!("expected finish binding failure");
    };
    assert_eq!(fault.phase(), BindingPhase::Finish);
    failure.abort(BindingAbortReason::ParserFailure).await;
    assert_eq!(aborts_with_finished_state.load(Ordering::SeqCst), 1);
}

fn finish_live_system(_: SystemMountContext<'_, ()>) -> SystemView<CounterChannels> {
    system_view(
        StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("finish_live"))
            .state_with(|| ())
            .on_finish(|_| vec![TurnEmission::Live("finished".to_owned())])
            .into_component(),
    )
}

#[tokio::test]
async fn finish_live_effect_is_awaited_in_the_same_scope() {
    let epoch = mount_system_epoch(&(), finish_live_system).unwrap();
    let (runtime, live) = recording_live_effects();
    let turn = epoch.begin_turn("finish-live");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let attempt = prepared.start_streaming_attempt(runtime).unwrap();
    let finished = attempt.finish_stream().await.unwrap();
    assert!(finished.update().emissions().is_empty());
    {
        let live = live.lock().unwrap();
        assert_eq!(live.effects.len(), 1);
        assert_eq!(live.effects[0].effect, "finished");
        assert_eq!(live.effects[0].phase, BindingPhase::Finish);
    }
    let report = finished.abort(BindingAbortReason::PublishFailure).await;
    assert_eq!(
        report.live().acknowledgement(),
        Some(LiveEffectAbortAck::CompensationCompleted)
    );
}

#[tokio::test]
async fn text_complete_without_deltas_is_parsed_once() {
    let epoch = mount_system_epoch(&(), ordered_system).unwrap();
    let turn = epoch.begin_turn("complete-only");
    let (runtime, live) = recording_live_effects();
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();
    let update = attempt
        .on_event(TextTurnEvent::TextComplete("<b /><a />".to_owned()))
        .await
        .unwrap();
    assert!(update.emissions().is_empty());
    assert_eq!(
        live.lock()
            .unwrap()
            .effects
            .iter()
            .map(|delivery| delivery.effect.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
    let finished = attempt.finish_stream().await.unwrap();
    let identity = finished.identity().clone();
    let mut publisher = RecordingPublisher::default();
    let published = finished.publish_with(&mut publisher).await.unwrap();
    assert_eq!(published.receipt().identity(), &identity);
    assert!(published.pending_commits().is_empty());
    assert_eq!(publisher.publications.len(), 1);
}

#[tokio::test]
async fn text_complete_feeds_only_the_suffix_missing_from_deltas() {
    let epoch = mount_system_epoch(&(), ordered_system).unwrap();
    let turn = epoch.begin_turn("complete-suffix");
    let (runtime, live) = recording_live_effects();
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();

    let delta_update = attempt
        .on_event(TextTurnEvent::TextDelta("<b />".to_owned()))
        .await
        .unwrap();
    let complete_update = attempt
        .on_event(TextTurnEvent::TextComplete("<b /><a />".to_owned()))
        .await
        .unwrap();
    assert!(delta_update.is_empty());
    assert!(complete_update.is_empty());

    assert_eq!(attempt.raw_output(), "<b /><a />");
    assert_eq!(
        live.lock()
            .unwrap()
            .effects
            .iter()
            .map(|delivery| delivery.effect.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
    let finished = attempt.finish_stream().await.unwrap();
    finished.abort(BindingAbortReason::Cancelled).await;
}

#[tokio::test]
async fn divergent_text_complete_is_terminal_without_replaying_deltas() {
    let epoch = mount_system_epoch(&(), ordered_system).unwrap();
    let turn = epoch.begin_turn("complete-mismatch");
    let (runtime, live) = recording_live_effects();
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let mut attempt = prepared.start_streaming_attempt(runtime).unwrap();

    let delta_update = attempt
        .on_event(TextTurnEvent::TextDelta("<b />".to_owned()))
        .await
        .unwrap();
    assert!(delta_update.is_empty());
    let error = attempt
        .on_event(TextTurnEvent::TextComplete("<a />".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StreamingAttemptError::TextCompletionMismatch {
            delta_bytes: 5,
            completion_bytes: 5,
        }
    ));
    assert_eq!(
        live.lock()
            .unwrap()
            .effects
            .iter()
            .map(|delivery| delivery.effect.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );
    assert!(matches!(
        attempt
            .on_event(TextTurnEvent::TextDelta("<a />".to_owned()))
            .await,
        Err(StreamingAttemptError::Terminal)
    ));

    let report = attempt.abort(BindingAbortReason::ProviderFailure).await;
    assert_eq!(
        report.live().acknowledgement(),
        Some(LiveEffectAbortAck::CompensationCompleted)
    );
}

struct DuplicateProps {
    initializations: Arc<AtomicUsize>,
}

fn duplicate_system(cx: SystemMountContext<'_, DuplicateProps>) -> SystemView<CounterChannels> {
    let first_count = Arc::clone(&cx.props().initializations);
    let second_count = Arc::clone(&cx.props().initializations);
    let first = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("same"))
        .state_with(move || {
            first_count.fetch_add(1, Ordering::SeqCst);
        })
        .into_component();
    let second = StreamingXml::<TurnEmission<CounterChannels>, String>::new(contract("same"))
        .state_with(move || {
            second_count.fetch_add(1, Ordering::SeqCst);
        })
        .into_component();
    system_view(component((mount("First", first), mount("Second", second))))
}

#[test]
fn duplicate_derived_routes_fail_at_mount_before_state_creation() {
    let initializations = Arc::new(AtomicUsize::new(0));
    let result = mount_system_epoch(
        &DuplicateProps {
            initializations: Arc::clone(&initializations),
        },
        duplicate_system,
    );

    assert!(matches!(
        result,
        Err(SystemMountError::Plan(
            MountPlanError::DuplicateBindingRoute { .. }
        ))
    ));
    assert_eq!(initializations.load(Ordering::SeqCst), 0);
}
