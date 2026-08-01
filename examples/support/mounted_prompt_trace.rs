//! Example-only mounted prompt-trace host kept out of component authoring.

use std::{
    collections::VecDeque,
    convert::Infallible,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        advanced::provider::{
            AttachedProviderEpoch, DurableMountedProviderExecutor, FallibleProviderWirePort,
            MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
            ProviderAdapterContract, ProviderCancellationToken, ProviderEpochAttachRequest,
            ProviderEpochReceipt, ProviderEpochRehydrateRequest, ProviderWireEvent,
            ProviderWireFault,
        },
        host::prelude::*,
        DurableCallId, DurableCallInputId, DurableSessionId, InMemoryMountedAgentFactory,
        SessionReduceContext, SessionReducer,
    },
    llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit, TextTurnEvent},
    prelude::*,
};
use serde_json::json;

/// Compatibility name used by the prompt-trace host's existing examples.
#[allow(dead_code)]
pub type PromptOnlyChannels = NoTurnChannels;

struct DirectCapture<Props>(PhantomData<fn() -> Props>);

impl<Props> DirectCapture<Props> {
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

#[async_trait::async_trait]
impl<Props> MountedTurnCapture for DirectCapture<Props>
where
    Props: Clone + Send + Sync + 'static,
{
    type Transcript = String;
    type ContextState = ();
    type CallProps = Props;
    type TurnProps = Props;
    type Source = ();
    type Error = Infallible;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<'_, String, (), Props, ()>,
    ) -> Result<Self::TurnProps, Self::Error> {
        Ok(context.call_props().clone())
    }
}

#[derive(Clone)]
struct RecordingEpoch(ProviderEpochReceipt);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamingTraceEvent {
    ChunkSubmitted(String),
    LiveApplied { route: String },
    ChunkAccepted(String),
}

#[derive(Debug, thiserror::Error)]
enum RecordingProviderError {
    #[error("recording provider wire failed: {0}")]
    Wire(#[from] ProviderWireFault),
}

#[derive(Default)]
struct RecordingProvider {
    systems: Mutex<Vec<String>>,
    users: Mutex<Vec<String>>,
    scripts: Mutex<VecDeque<Vec<String>>>,
    events: Arc<Mutex<Vec<StreamingTraceEvent>>>,
}

#[async_trait::async_trait]
impl MountedProviderExecutor<String> for RecordingProvider {
    type Error = RecordingProviderError;
    type Epoch = RecordingEpoch;

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
        debug_assert_eq!(epoch.0.adapter(), "mounted-prompt-trace-provider");
        self.users.lock().unwrap().push(request.user().to_owned());
        let script = self.scripts.lock().unwrap().pop_front();
        for chunk in script.unwrap_or_default() {
            self.events
                .lock()
                .unwrap()
                .push(StreamingTraceEvent::ChunkSubmitted(chunk.clone()));
            wire.submit(ProviderWireEvent::Text(TextTurnEvent::TextDelta(
                chunk.clone(),
            )))
            .await?;
            self.events
                .lock()
                .unwrap()
                .push(StreamingTraceEvent::ChunkAccepted(chunk));
        }
        Ok(MountedProviderExit::completed(ExecutorCommit::empty()))
    }
}

#[async_trait::async_trait]
impl DurableMountedProviderExecutor<String> for RecordingProvider {
    fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
        ProviderAdapterContract::new("mounted-prompt-trace-provider", 1).unwrap()
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
            "mounted-prompt-trace-provider",
            1,
            request.durable_epoch_id().clone(),
            request.fingerprint().clone(),
            json!({ "binding": "mounted-prompt-trace" }),
        )
        .unwrap();
        Ok(AttachedProviderEpoch::new(
            RecordingEpoch(receipt.clone()),
            receipt,
        ))
    }

    async fn rehydrate_durable_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Epoch, Self::Error> {
        Ok(RecordingEpoch(request.receipt().clone()))
    }
}

struct WaitReducer<C>(PhantomData<fn() -> C>);

impl<C> SessionReducer<String, (), C> for WaitReducer<C>
where
    C: TurnChannels,
{
    type Error = Infallible;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, ()>,
        _context: SessionReduceContext<'_, String, C>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Ok(TurnFlow::Wait)
    }
}

struct RecordingLive<L> {
    effects: Arc<Mutex<Vec<L>>>,
    events: Arc<Mutex<Vec<StreamingTraceEvent>>>,
}

#[async_trait::async_trait]
impl<L> LiveEffectRuntime<L> for RecordingLive<L>
where
    L: Send + 'static,
{
    type Error = Infallible;

    async fn apply(&mut self, context: &LiveEffectContext, effect: L) -> Result<(), Self::Error> {
        self.effects.lock().unwrap().push(effect);
        self.events
            .lock()
            .unwrap()
            .push(StreamingTraceEvent::LiveApplied {
                route: context.origin().route().to_string(),
            });
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

pub struct PromptTrace {
    system: String,
    users: Vec<String>,
}

impl PromptTrace {
    pub fn system(&self) -> &str {
        &self.system
    }

    pub fn users(&self) -> &[String] {
        &self.users
    }
}

/// Example-only mounted host. It keeps provider, reducer, and durable-call
/// plumbing out of an authoring example so the example can focus on its POM.
pub struct MountedPromptTrace<C, Props>
where
    Props: Clone + Send + Sync + 'static,
    C: TurnChannels<Commit = Never>,
{
    agent: MountedAgent<C, Props>,
    provider: Arc<RecordingProvider>,
    #[allow(dead_code)]
    live_effects: Arc<Mutex<Vec<C::Live>>>,
    completed_turns: usize,
}

impl<C, Props> MountedPromptTrace<C, Props>
where
    Props: Clone + Send + Sync + 'static,
    C: TurnChannels<Commit = Never>,
{
    /// Run one logical turn with a newly captured User snapshot.
    pub async fn run_turn(&mut self, props: Props) -> anyhow::Result<MountedCallOutcome<C>> {
        let call_number = self.completed_turns + 1;
        let call_id = format!("mounted-prompt-trace-call-{call_number}");
        let mut call = self
            .agent
            .start(MountedCallInput::new(
                DurableCallId::new(&call_id)?,
                DurableCallInputId::new(format!("{call_id}/input-v1"))?,
                format!("mounted-prompt-trace-{call_number}"),
                Arc::new(props),
                Arc::new(()),
            )?)
            .await?;
        let outcome = call.wait().await?;
        self.completed_turns = call_number;
        Ok(outcome)
    }

    /// Return the System attachment and every completed User request.
    pub fn trace(&self) -> anyhow::Result<PromptTrace> {
        let systems = self.provider.systems.lock().unwrap().clone();
        let users = self.provider.users.lock().unwrap().clone();
        anyhow::ensure!(
            systems.len() == 1,
            "expected one durable System attachment, got {}",
            systems.len()
        );
        anyhow::ensure!(
            users.len() == self.completed_turns,
            "expected {} fresh User prompts, got {}",
            self.completed_turns,
            users.len()
        );
        Ok(PromptTrace {
            system: systems.into_iter().next().unwrap(),
            users,
        })
    }

    #[allow(dead_code)]
    fn take_live_effects(&self) -> Vec<C::Live> {
        std::mem::take(&mut *self.live_effects.lock().unwrap())
    }

    #[allow(dead_code)]
    fn events(&self) -> Vec<StreamingTraceEvent> {
        self.provider.events.lock().unwrap().clone()
    }
}

/// Mount a POM harness against the example-only recording host.
pub async fn mount<C, Props>(
    definition: MountedHarnessDefinition<C, Props>,
) -> anyhow::Result<MountedPromptTrace<C, Props>>
where
    Props: Clone + Send + Sync + 'static,
    C: TurnChannels<Commit = Never>,
{
    mount_with_scripts(definition, Vec::new()).await
}

async fn mount_with_scripts<C, Props>(
    definition: MountedHarnessDefinition<C, Props>,
    scripts: Vec<Vec<String>>,
) -> anyhow::Result<MountedPromptTrace<C, Props>>
where
    Props: Clone + Send + Sync + 'static,
    C: TurnChannels<Commit = Never>,
{
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingProvider {
        scripts: Mutex::new(scripts.into()),
        events: Arc::clone(&events),
        ..RecordingProvider::default()
    });
    let live_effects = Arc::new(Mutex::new(Vec::new()));
    let live_factory = {
        let effects = Arc::clone(&live_effects);
        move || RecordingLive {
            effects: Arc::clone(&effects),
            events: Arc::clone(&events),
        }
    };
    let factory =
        InMemoryMountedAgentFactory::<C, String, (), RecordingProvider, WaitReducer<C>, _>::new(
            DurableSessionId::new("example/mounted-prompt-trace/session")?,
            AgentSession::new(PromptContext::<String, ()>::without_system()),
            Arc::clone(&provider),
            "example-model",
            128,
            WaitReducer(PhantomData),
            live_factory,
        )?;
    let agent = factory
        .open(definition.with_capture(DirectCapture::new()))
        .await?;
    Ok(MountedPromptTrace {
        agent,
        provider,
        live_effects,
        completed_turns: 0,
    })
}

#[allow(dead_code)]
pub async fn run_turns<C, Props>(
    definition: MountedHarnessDefinition<C, Props>,
    turns: Vec<Props>,
) -> anyhow::Result<PromptTrace>
where
    Props: Clone + Send + Sync + 'static,
    C: TurnChannels<Commit = Never>,
{
    let expected_user_count = turns.len();
    let mut mounted = mount(definition).await?;
    for props in turns {
        let _outcome = mounted.run_turn(props).await?;
    }
    let trace = mounted.trace()?;
    anyhow::ensure!(
        trace.users().len() == expected_user_count,
        "expected {expected_user_count} fresh User prompts, got {}",
        trace.users().len()
    );
    Ok(trace)
}

#[allow(dead_code)]
pub struct StreamingTrace<C>
where
    C: TurnChannels,
{
    prompt: PromptTrace,
    records: Vec<TurnRecord<C>>,
    live_effects: Vec<C::Live>,
    events: Vec<StreamingTraceEvent>,
}

#[allow(dead_code)]
impl<C> StreamingTrace<C>
where
    C: TurnChannels,
{
    pub fn system(&self) -> &str {
        self.prompt.system()
    }

    pub fn users(&self) -> &[String] {
        self.prompt.users()
    }

    pub fn records(&self) -> &[TurnRecord<C>] {
        &self.records
    }

    pub fn live_effects(&self) -> &[C::Live] {
        &self.live_effects
    }

    pub fn events(&self) -> &[StreamingTraceEvent] {
        &self.events
    }
}

#[allow(dead_code)]
pub async fn run_streamed_turns<C, Props>(
    definition: MountedHarnessDefinition<C, Props>,
    turns: Vec<(Props, Vec<String>)>,
) -> anyhow::Result<StreamingTrace<C>>
where
    Props: Clone + Send + Sync + 'static,
    C: TurnChannels<Commit = Never>,
{
    let expected_user_count = turns.len();
    let (props, scripts): (Vec<_>, Vec<_>) = turns.into_iter().unzip();
    let mut mounted = mount_with_scripts(definition, scripts).await?;
    let mut records = Vec::new();
    for props in props {
        match mounted.run_turn(props).await? {
            MountedCallOutcome::Executed {
                records: mut turn_records,
                ..
            } => records.append(&mut turn_records),
            MountedCallOutcome::Replayed { .. } => {
                anyhow::bail!("the scripted example unexpectedly replayed a call")
            }
            _ => anyhow::bail!("the scripted example observed an unsupported mounted call outcome"),
        }
    }
    let prompt = mounted.trace()?;
    anyhow::ensure!(
        prompt.users().len() == expected_user_count,
        "expected {expected_user_count} fresh User prompts, got {}",
        prompt.users().len()
    );
    Ok(StreamingTrace {
        prompt,
        records,
        live_effects: mounted.take_live_effects(),
        events: mounted.events(),
    })
}
