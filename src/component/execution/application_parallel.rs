//! Application-owned asynchronous reactions over one mounted Component tree.

use std::{
    any::Any,
    borrow::Cow,
    collections::HashMap,
    fmt,
    future::{Future, IntoFuture},
    ops::ControlFlow,
    panic::{resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use super::{
    application_fault_from_outer_driver_boundary, begin_outer_driver_boundary, pump_provider_facts,
    submit_prepared_frame, Application as CoreApplication, ApplicationFault, ApplicationFaultCode,
    ApplicationFaultKind, ApplicationFaultReason, ApplicationFaultStage, ProjectionSnapshot,
    ReactionCancellationControl, APPLICATION_TERMINATED_AFTER_TASK_PANIC,
};
use crate::component::{
    authoring::{
        application_exit::ApplicationExitControl, streaming_attempt::StreamingToolAbortCause,
        ApplicationExitHandle, Component, ExitReason, InternalEventInput, RenderBindings,
    },
    execution::{
        driver_demand::DriverDemand,
        frame::FrameSession,
        streaming::{StreamingSupervisor, StreamingToolRecoveryStatus},
        CommandInput, ForkableReactionPort, Frame, ProviderEvent, ProviderFactStream, ReactionPort,
        ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason, RenderedProjection,
        ResettableReactionPort, SubmitFault, TargetDeclaration,
    },
    provenance::EventScope,
    task::TaskPanicMonitor,
    ApplicationId, ReactionId, ReactionKind,
};

/// The runtime result of one reaction. Business success remains application data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionOutcome {
    Completed,
    Interrupted,
    Exited(ExitReason),
}

impl ReactionOutcome {
    pub const fn is_break(self) -> bool {
        matches!(self, Self::Exited(_))
    }
    pub const fn is_continue(self) -> bool {
        matches!(self, Self::Completed)
    }
}
impl From<ControlFlow<ExitReason>> for ReactionOutcome {
    fn from(flow: ControlFlow<ExitReason>) -> Self {
        match flow {
            ControlFlow::Continue(()) => Self::Completed,
            ControlFlow::Break(reason) => Self::Exited(reason),
        }
    }
}

impl PartialEq<ControlFlow<ExitReason>> for ReactionOutcome {
    fn eq(&self, other: &ControlFlow<ExitReason>) -> bool {
        matches!((self, other), (Self::Completed, ControlFlow::Continue(())))
            || matches!((self, other), (Self::Exited(a), ControlFlow::Break(b)) if a == b)
    }
}

type PanicPayload = Box<dyn Any + Send + 'static>;
#[derive(Default)]
struct PanicState(Mutex<Option<PanicPayload>>, std::sync::atomic::AtomicBool);
impl PanicState {
    fn raise(&self) {
        let payload = self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(payload) = payload {
            resume_unwind(payload);
        }
    }
}

#[derive(Clone)]
enum Completion {
    Result(Result<ReactionOutcome, ApplicationFault>),
    Panic(Arc<PanicState>),
}

type RecoverySender =
    tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<StreamingToolRecoveryStatus>>;
#[derive(Clone)]
struct RecoveryControl {
    requests: Option<RecoverySender>,
    observer: StreamingSupervisor,
}

struct ReactionState {
    id: ReactionId,
    kind: ReactionKind,
    lineage: ReactionId,
    interrupted: std::sync::atomic::AtomicBool,
    cancel: Notify,
    completion: Mutex<Option<Completion>>,
    changed: Notify,
    recovery: Mutex<Option<RecoveryControl>>,
}

impl ReactionState {
    fn interrupt(&self) {
        self.interrupted
            .store(true, std::sync::atomic::Ordering::Release);
        self.cancel.notify_waiters();
    }
    async fn cancelled(&self) {
        loop {
            let changed = self.cancel.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.interrupted.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
    fn finish(&self, result: Completion) {
        *self.completion.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
        self.changed.notify_waiters();
    }
}

trait ReactionOwner: Send + Sync {
    fn continue_reaction(
        self: Arc<Self>,
        source: &ReactionState,
    ) -> Result<Reaction, ApplicationFault>;
}

/// A cloneable control and completion handle. Dropping it never cancels work.
#[derive(Clone)]
pub struct Reaction {
    state: Arc<ReactionState>,
    owner: Weak<dyn ReactionOwner>,
}

impl fmt::Debug for Reaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reaction")
            .field("id", &self.id())
            .field("kind", &self.kind())
            .finish()
    }
}

impl Reaction {
    pub fn id(&self) -> ReactionId {
        self.state.id
    }
    pub fn kind(&self) -> ReactionKind {
        self.state.kind
    }

    /// Request interruption of this operation; await it to observe settlement.
    pub fn interrupt(&self) -> Result<(), ApplicationFault> {
        if self
            .state
            .completion
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
        {
            return Ok(());
        }
        if self.owner.upgrade().is_none() {
            return Err(owner_closed());
        }
        self.state.interrupt();
        Ok(())
    }

    /// Continue a settled Temporary's own history, with a new reaction identity.
    /// Main continuations are initiated through `Application::react`.
    pub fn react(&self) -> Result<Reaction, ApplicationFault> {
        self.owner
            .upgrade()
            .ok_or_else(owner_closed)?
            .continue_reaction(&self.state)
    }
}

/// An independent waiter for a reaction's retained terminal result.
pub struct ReactionWait(
    Pin<Box<dyn Future<Output = Result<ReactionOutcome, ApplicationFault>> + Send>>,
);
impl Future for ReactionWait {
    type Output = Result<ReactionOutcome, ApplicationFault>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}
impl IntoFuture for Reaction {
    type Output = Result<ReactionOutcome, ApplicationFault>;
    type IntoFuture = ReactionWait;
    fn into_future(self) -> ReactionWait {
        ReactionWait(Box::pin(async move {
            loop {
                let changed = self.state.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let completion = self
                    .state
                    .completion
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                match completion {
                    Some(Completion::Result(result)) => return result,
                    Some(Completion::Panic(panic)) => {
                        panic.raise();
                        return Err(owner_closed());
                    }
                    None => changed.await,
                }
            }
        }))
    }
}

fn owner_closed() -> ApplicationFault {
    ApplicationFault::terminal(
        ApplicationFaultStage::Reaction,
        ApplicationFaultCode::Unavailable,
        ApplicationFaultReason::ComponentRuntime,
    )
}
fn not_ready() -> ApplicationFault {
    ApplicationFault::from_port(
        ApplicationFaultStage::Reaction,
        ReactionPortFault::retryable(
            ReactionPortFaultCode::Rejected,
            ReactionPortFaultReason::ForkNotReady,
        ),
    )
}

// A main request temporarily owns the port. Preparation still owns the same
// Component runtime; no placeholder port can accidentally perform a submission.
struct PortSlot<P>(Option<P>);
#[async_trait]
impl<P: ReactionPort> ReactionPort for PortSlot<P> {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.0.as_mut().ok_or_else(port_busy)?.declare()
    }
    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.0.as_mut().ok_or_else(port_busy)?.submit(frame).await
    }
    fn fork_temporary(&mut self) -> Result<Box<dyn ReactionPort>, ReactionPortFault> {
        self.0.as_mut().ok_or_else(port_busy)?.fork_temporary()
    }
    async fn shutdown(&mut self) {
        if let Some(port) = &mut self.0 {
            port.shutdown().await;
        }
    }
}
fn port_busy() -> ReactionPortFault {
    ReactionPortFault::retryable(
        ReactionPortFaultCode::Rejected,
        ReactionPortFaultReason::ForkNotReady,
    )
}
#[async_trait]
impl<P: ForkableReactionPort> ForkableReactionPort for PortSlot<P> {
    async fn fork(&mut self) -> Result<Self, ReactionPortFault> {
        Ok(Self(Some(
            self.0.as_mut().ok_or_else(port_busy)?.fork().await?,
        )))
    }
    fn validate_fork_frame(&self, frame: &Frame) -> Result<(), ReactionPortFault> {
        self.0
            .as_ref()
            .ok_or_else(port_busy)?
            .validate_fork_frame(frame)
    }
}
impl<P: ResettableReactionPort> ResettableReactionPort for PortSlot<P> {
    fn reset_model_context(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.0.as_mut().ok_or_else(port_busy)?.reset_model_context()
    }
}

enum LanePort<P> {
    Main(P),
    Temporary(Box<dyn ReactionPort>),
}
#[async_trait]
impl<P: ReactionPort> ReactionPort for LanePort<P> {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        match self {
            Self::Main(p) => p.declare(),
            Self::Temporary(p) => p.declare(),
        }
    }
    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        match self {
            Self::Main(p) => p.submit(frame).await,
            Self::Temporary(p) => p.submit(frame).await,
        }
    }
    async fn shutdown(&mut self) {
        match self {
            Self::Main(p) => p.shutdown().await,
            Self::Temporary(p) => p.shutdown().await,
        }
    }
}
struct Lane<P> {
    port: LanePort<P>,
    session: FrameSession,
    streaming: StreamingSupervisor,
    completion_bindings: Option<RenderBindings<ProviderEvent>>,
}
struct Temporary {
    port: Box<dyn ReactionPort>,
    session: FrameSession,
    streaming: StreamingSupervisor,
    owner: Option<Weak<ReactionState>>,
}
struct Checkpoint {
    port: Box<dyn ReactionPort>,
    session: FrameSession,
}
struct Admission<P> {
    closed: bool,
    maintenance: bool,
    main: bool,
    active: HashMap<ReactionId, Arc<ReactionState>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    checkpoint: Option<Checkpoint>,
    continuations: HashMap<ReactionId, Temporary>,
    retired: Vec<Box<dyn ReactionPort>>,
    discarded: Vec<ReactionId>,
    orphaned: Vec<Lane<P>>,
}
#[derive(Clone)]
struct Snapshot {
    projection: RenderedProjection,
    revision: u64,
    dirty: bool,
    prepared: bool,
}
struct Shared<P: ReactionPort + 'static> {
    core: AsyncMutex<Option<CoreApplication<PortSlot<P>>>>,
    admission: Mutex<Admission<P>>,
    snapshot: Mutex<Snapshot>,
    app_id: ApplicationId,
    exit: ApplicationExitControl,
    demand: Arc<DriverDemand>,
    monitor: TaskPanicMonitor,
    state: Arc<std::sync::atomic::AtomicU8>,
    panic: Arc<PanicState>,
    settled: Notify,
}

struct Maintenance<P: ReactionPort + 'static>(Arc<Shared<P>>);
impl<P: ReactionPort + 'static> Drop for Maintenance<P> {
    fn drop(&mut self) {
        self.0
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .maintenance = false;
    }
}

/// Retains resources and resolves waiters even if the executor drops the task.
struct OwnedReaction<P: ReactionPort + 'static> {
    owner: Arc<Shared<P>>,
    state: Arc<ReactionState>,
    lane: Option<Lane<P>>,
    armed: bool,
}
impl<P: ReactionPort + 'static> Drop for OwnedReaction<P> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.owner.state.store(
            APPLICATION_TERMINATED_AFTER_TASK_PANIC,
            std::sync::atomic::Ordering::Release,
        );
        self.state
            .recovery
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        {
            let mut admission = self
                .owner
                .admission
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(lane) = self.lane.take() {
                admission.orphaned.push(lane);
            }
            admission.active.remove(&self.state.id);
            if self.state.kind == ReactionKind::Main {
                admission.main = false;
            }
            for state in admission.active.values() {
                state.interrupt();
            }
        }
        self.state.finish(Completion::Result(Err(owner_closed())));
        self.owner.settled.notify_waiters();
    }
}

struct FreshTarget<P: ReactionPort + 'static, Q: ReactionPort + 'static> {
    port: Option<Q>,
    owner: Arc<Shared<P>>,
}
impl<P: ReactionPort + 'static, Q: ReactionPort + 'static> Drop for FreshTarget<P, Q> {
    fn drop(&mut self) {
        if let Some(mut port) = self.port.take() {
            let owner = self.owner.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let task = runtime.spawn(async move {
                    if let Err(payload) = AssertUnwindSafe(port.shutdown()).catch_unwind().await {
                        owner.fail(payload);
                    }
                });
                self.owner
                    .admission
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .tasks
                    .push(task);
            }
        }
    }
}

/// Owns one mounted tree and all asynchronous main and temporary reactions.
pub struct Application<P: ReactionPort + 'static> {
    shared: Arc<Shared<P>>,
    shutdown: bool,
}

impl<P: ReactionPort + 'static> Application<P> {
    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
        port: P,
    ) -> Result<Self, ApplicationFault> {
        CoreApplication::mount(root, PortSlot(Some(port))).map(Self::from_core)
    }
    pub fn mount_with_commands(
        root: impl Fn() -> Component + Send + Sync + 'static,
        port: P,
        input: CommandInput,
    ) -> Result<Self, ApplicationFault> {
        CoreApplication::mount_with_commands(root, PortSlot(Some(port)), input).map(Self::from_core)
    }
    pub(crate) fn mount_with_events(
        root: impl Fn(InternalEventInput<ProviderEvent>) -> Component + Send + Sync + 'static,
        port: P,
    ) -> Result<Self, ApplicationFault> {
        CoreApplication::mount_with_events(root, PortSlot(Some(port))).map(Self::from_core)
    }
    fn from_core(core: CoreApplication<PortSlot<P>>) -> Self {
        let snapshot = snapshot(&core);
        let shared = Arc::new(Shared {
            app_id: core.application_id(),
            exit: core.application_exit.clone(),
            demand: core.driver_demand.clone(),
            monitor: core.tasks.panic_monitor(),
            state: core.state.clone(),
            panic: Arc::new(PanicState::default()),
            core: AsyncMutex::new(Some(core)),
            snapshot: Mutex::new(snapshot),
            settled: Notify::new(),
            admission: Mutex::new(Admission {
                closed: false,
                maintenance: false,
                main: false,
                active: HashMap::new(),
                tasks: Vec::new(),
                checkpoint: None,
                continuations: HashMap::new(),
                retired: Vec::new(),
                discarded: Vec::new(),
                orphaned: Vec::new(),
            }),
        });
        Self {
            shared,
            shutdown: false,
        }
    }
    pub fn application_id(&self) -> ApplicationId {
        self.shared.app_id
    }
    pub fn exit_handle(&self) -> ApplicationExitHandle {
        self.shared.exit.handle()
    }
    pub(crate) fn has_command_wait(&self) -> bool {
        self.shared
            .core
            .try_lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(CoreApplication::has_command_wait))
            .unwrap_or(false)
    }
    pub(crate) async fn wait_for_command_request(
        &mut self,
    ) -> Result<ControlFlow<ExitReason>, ApplicationFault> {
        let mut guard = self.shared.core.lock().await;
        guard
            .as_mut()
            .ok_or_else(owner_closed)?
            .wait_for_command_request()
            .await
    }
    pub fn current_projection(&self) -> ProjectionSnapshot<'_> {
        let data = if let Ok(core) = self.shared.core.try_lock() {
            core.as_ref().map(snapshot).unwrap_or_else(|| {
                self.shared
                    .snapshot
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
            })
        } else {
            let mut data = self
                .shared
                .snapshot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            data.dirty = true;
            data.prepared = false;
            data
        };
        ProjectionSnapshot {
            projection: Cow::Owned(data.projection),
            revision: data.revision,
            dirty: data.dirty,
            prepared: data.prepared,
        }
    }

    /// Admit and start one operation without borrowing the application until EOF.
    pub fn react(&mut self) -> Result<Reaction, ApplicationFault> {
        self.shared.clone().admit(None)
    }

    pub async fn prepare(&mut self) -> Result<ControlFlow<ExitReason>, ApplicationFault> {
        self.shared.check()?;
        if self.shared.has_recovery() {
            return Err(ApplicationFault::streaming_recovery());
        }
        let mut core = self.shared.core.lock().await;
        let core = core.as_mut().ok_or_else(owner_closed)?;
        core.components.set_reaction_history_scope(None);
        let result = core.prepare().await;
        self.shared.publish(core);
        result
    }
    pub async fn run(&mut self) -> Result<ExitReason, ApplicationFault> {
        loop {
            let current = self
                .shared
                .admission
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .active
                .values()
                .find(|state| state.kind == ReactionKind::Main)
                .cloned();
            let reaction = match current {
                Some(state) => {
                    let owner: Arc<dyn ReactionOwner> = self.shared.clone();
                    Reaction {
                        state,
                        owner: Arc::downgrade(&owner),
                    }
                }
                None => self.react()?,
            };
            match reaction.await? {
                ReactionOutcome::Exited(reason) => return Ok(reason),
                ReactionOutcome::Completed | ReactionOutcome::Interrupted => {
                    tokio::task::yield_now().await
                }
            }
        }
    }
    pub fn take_reaction_request(&self) -> Result<bool, ApplicationFault> {
        self.shared.check()?;
        self.shared.demand.take().map_err(|fault| {
            ApplicationFault::from_driver_demand(ApplicationFaultStage::Reaction, fault)
        })
    }
    pub async fn wait_for_reaction_request(&mut self) -> Result<(), ApplicationFault> {
        self.shared.check()?;
        tokio::select! {
            result = self.shared.demand.wait() => result.map_err(|fault| ApplicationFault::from_driver_demand(ApplicationFaultStage::Reaction, fault)),
            _ = self.shared.monitor.wait() => { self.shared.check()?; Err(owner_closed()) }
        }
    }

    /// Recover retained per-reaction effects without starting another request.
    pub async fn recover_streaming_attempt(
        &mut self,
    ) -> Result<StreamingToolRecoveryStatus, ApplicationFault> {
        self.shared.check()?;
        let requests: Vec<_> = self
            .shared
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .values()
            .filter_map(|state| {
                state
                    .recovery
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .map(|recovery| (state.clone(), recovery))
            })
            .collect();
        if requests.is_empty() {
            return Ok(StreamingToolRecoveryStatus::NotRequired);
        }
        let mut attempts = Vec::new();
        let mut requested = false;
        let mut still_required = false;
        let mut in_flight = false;
        for (state, request) in requests {
            if request.observer.busy() || request.requests.is_none() {
                attempts.extend(request.observer.reports());
                in_flight = true;
                continue;
            }
            let (reply, result) = tokio::sync::oneshot::channel();
            if request
                .requests
                .expect("checked recovery sender")
                .send(reply)
                .is_err()
            {
                continue;
            }
            match result.await.map_err(|_| owner_closed())? {
                StreamingToolRecoveryStatus::Recovered {
                    attempts: reports,
                    reaction_requested,
                } => {
                    attempts.extend(reports);
                    requested |= reaction_requested;
                    loop {
                        let done = state.changed.notified();
                        tokio::pin!(done);
                        done.as_mut().enable();
                        if state
                            .completion
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .is_some()
                        {
                            break;
                        }
                        done.await;
                    }
                    self.shared.check()?;
                }
                StreamingToolRecoveryStatus::InFlight { attempts: reports } => {
                    attempts.extend(reports);
                    in_flight = true;
                }
                StreamingToolRecoveryStatus::StillRequired { attempts: reports } => {
                    attempts.extend(reports);
                    still_required = true;
                }
                StreamingToolRecoveryStatus::NotRequired => {}
            }
        }
        let attempts = attempts.into_boxed_slice();
        Ok(if in_flight {
            StreamingToolRecoveryStatus::InFlight { attempts }
        } else if still_required {
            StreamingToolRecoveryStatus::StillRequired { attempts }
        } else {
            StreamingToolRecoveryStatus::Recovered {
                attempts,
                reaction_requested: requested,
            }
        })
    }
    pub async fn fork_to<Q: ReactionPort + 'static>(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        target: Q,
    ) -> Result<Application<Q>, ApplicationFault> {
        self.fork_to_impl(root, target, None).await
    }
    pub async fn fork_to_with_commands<Q: ReactionPort + 'static>(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        target: Q,
        input: CommandInput,
    ) -> Result<Application<Q>, ApplicationFault> {
        self.fork_to_impl(root, target, Some(input)).await
    }
    async fn fork_to_impl<Q: ReactionPort + 'static>(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        target: Q,
        input: Option<CommandInput>,
    ) -> Result<Application<Q>, ApplicationFault> {
        let mut candidate = FreshTarget {
            port: Some(target),
            owner: self.shared.clone(),
        };
        let _maintenance = self.shared.reserve_maintenance()?;
        let mut guard = self.shared.core.lock().await;
        let core = guard.as_mut().ok_or_else(owner_closed)?;
        let target = candidate.port.take().expect("owned fresh target");
        let child = match input {
            Some(input) => {
                core.fork_to_with_commands(root, PortSlot(Some(target)), input)
                    .await?
            }
            None => core.fork_to(root, PortSlot(Some(target))).await?,
        };
        Ok(Application::from_core(child))
    }
    /// Start cleanup immediately. Cancelling its waiter leaves cleanup owned.
    pub fn shutdown(mut self) -> impl Future<Output = Result<(), ApplicationFault>> + Send {
        self.shutdown = true;
        let shared = self.shared.clone();
        shared.close();
        let task = tokio::spawn(async move { shared.shutdown_all().await });
        async move {
            match task.await {
                Ok(result) => result,
                Err(error) if error.is_panic() => resume_unwind(error.into_panic()),
                Err(_) => Err(owner_closed()),
            }
        }
    }
}
impl<P: ForkableReactionPort> Application<P> {
    pub async fn fork(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
    ) -> Result<Self, ApplicationFault> {
        let _maintenance = self.shared.reserve_maintenance()?;
        let mut guard = self.shared.core.lock().await;
        let child = guard.as_mut().ok_or_else(owner_closed)?.fork(root).await?;
        Ok(Self::from_core(child))
    }
    pub async fn fork_with_commands(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        input: CommandInput,
    ) -> Result<Self, ApplicationFault> {
        let _maintenance = self.shared.reserve_maintenance()?;
        let mut guard = self.shared.core.lock().await;
        let child = guard
            .as_mut()
            .ok_or_else(owner_closed)?
            .fork_with_commands(root, input)
            .await?;
        Ok(Self::from_core(child))
    }
}
impl<P: ResettableReactionPort + 'static> Application<P> {
    pub fn reset_model_context(&mut self) -> Result<(), ApplicationFault> {
        let _maintenance = self.shared.reserve_maintenance()?;
        let mut guard = self.shared.core.try_lock().map_err(|_| not_ready())?;
        let core = guard.as_mut().ok_or_else(owner_closed)?;
        core.reset_model_context()?;
        let mut admission = self
            .shared
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(checkpoint) = admission.checkpoint.take() {
            admission.retired.push(checkpoint.port);
        }
        self.shared.publish(core);
        Ok(())
    }
}
impl<P: ReactionPort + 'static> Drop for Application<P> {
    fn drop(&mut self) {
        if !self.shutdown {
            self.shared.close();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let shared = self.shared.clone();
                runtime.spawn(async move {
                    let _ = shared.shutdown_all().await;
                });
            }
        }
    }
}
fn snapshot<P: ReactionPort>(core: &CoreApplication<P>) -> Snapshot {
    let value = core.current_projection();
    Snapshot {
        projection: value.projection().clone(),
        revision: value.revision(),
        dirty: value.is_dirty(),
        prepared: value.is_prepared(),
    }
}

impl<P: ReactionPort + 'static> Shared<P> {
    fn reserve_maintenance(self: &Arc<Self>) -> Result<Maintenance<P>, ApplicationFault> {
        self.check()?;
        let mut admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if admission.closed || admission.maintenance || !admission.active.is_empty() {
            return Err(not_ready());
        }
        admission.maintenance = true;
        Ok(Maintenance(self.clone()))
    }
    fn has_recovery(&self) -> bool {
        self.admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .values()
            .any(|state| {
                state
                    .recovery
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some()
            })
    }
    fn publish_recovery_fence(&self, request: bool) {
        let admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        let pending = admission.active.values().any(|state| {
            state
                .recovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some()
        });
        self.demand.settle_reaction(pending, request);
    }
    fn check(&self) -> Result<(), ApplicationFault> {
        self.panic.raise();
        begin_outer_driver_boundary(&self.state, &self.monitor).map_err(|fault| {
            application_fault_from_outer_driver_boundary(ApplicationFaultStage::Reaction, fault)
        })?;
        if self
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closed
        {
            return Err(owner_closed());
        }
        Ok(())
    }
    fn check_idle(&self) -> Result<(), ApplicationFault> {
        self.check()?;
        if !self
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .is_empty()
        {
            return Err(not_ready());
        }
        Ok(())
    }
    fn publish(&self, core: &CoreApplication<PortSlot<P>>) {
        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = snapshot(core);
    }
    fn admit(
        self: Arc<Self>,
        source: Option<&ReactionState>,
    ) -> Result<Reaction, ApplicationFault> {
        self.check()?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| owner_closed())?;
        let mut admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if admission.closed {
            return Err(owner_closed());
        }
        if admission.maintenance {
            return Err(not_ready());
        }
        let abandoned: Vec<_> = admission
            .continuations
            .iter()
            .filter_map(|(lineage, temporary)| {
                temporary
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.strong_count() == 0)
                    .then_some(*lineage)
            })
            .collect();
        for lineage in abandoned {
            if let Some(temporary) = admission.continuations.remove(&lineage) {
                admission.retired.push(temporary.port);
            }
            admission.discarded.push(lineage);
        }
        let id = ReactionId::fresh();
        let (kind, lineage, temporary) = if let Some(source) = source {
            if source.kind != ReactionKind::Temporary
                || source
                    .completion
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_none()
                || admission
                    .active
                    .values()
                    .any(|state| state.lineage == source.lineage)
            {
                return Err(not_ready());
            }
            if admission
                .continuations
                .get(&source.lineage)
                .and_then(|temporary| temporary.owner.as_ref())
                .and_then(Weak::upgrade)
                .is_none_or(|owner| owner.id != source.id)
            {
                return Err(not_ready());
            }
            let temporary = admission
                .continuations
                .remove(&source.lineage)
                .ok_or_else(not_ready)?;
            (ReactionKind::Temporary, source.lineage, Some(temporary))
        } else if !admission.main {
            admission.main = true;
            (ReactionKind::Main, id, None)
        } else {
            let checkpoint = admission.checkpoint.as_mut().ok_or_else(not_ready)?;
            let fork =
                std::panic::catch_unwind(AssertUnwindSafe(|| checkpoint.port.fork_temporary()));
            let mut port = match fork {
                Ok(result) => result.map_err(|fault| {
                    ApplicationFault::from_port(ApplicationFaultStage::Reaction, fault)
                })?,
                Err(payload) => {
                    drop(admission);
                    self.fail(payload);
                    self.panic.raise();
                    return Err(owner_closed());
                }
            };
            let candidate = std::panic::catch_unwind(AssertUnwindSafe(|| {
                port.declare()
                    .map_err(|fault| {
                        ApplicationFault::from_port(ApplicationFaultStage::Declaration, fault)
                    })
                    .and_then(|declaration| {
                        checkpoint.session.fork(&declaration).map_err(|fault| {
                            ApplicationFault::from_session(ApplicationFaultStage::Reaction, fault)
                        })
                    })
            }));
            let session = match candidate {
                Ok(Ok(session)) => session,
                Ok(Err(fault)) => {
                    admission.retired.push(port);
                    return Err(fault);
                }
                Err(payload) => {
                    admission.retired.push(port);
                    drop(admission);
                    self.fail(payload);
                    self.panic.raise();
                    return Err(owner_closed());
                }
            };
            (
                ReactionKind::Temporary,
                id,
                Some(Temporary {
                    port,
                    session,
                    streaming: StreamingSupervisor::default(),
                    owner: None,
                }),
            )
        };
        let state = Arc::new(ReactionState {
            id,
            kind,
            lineage,
            interrupted: std::sync::atomic::AtomicBool::new(false),
            cancel: Notify::new(),
            completion: Mutex::new(None),
            changed: Notify::new(),
            recovery: Mutex::new(None),
        });
        admission.active.insert(id, state.clone());
        admission.tasks.retain(|task| !task.is_finished());
        let owned = OwnedReaction {
            owner: self.clone(),
            state: state.clone(),
            armed: true,
            lane: temporary.map(|temporary| Lane {
                port: LanePort::Temporary(temporary.port),
                session: temporary.session,
                streaming: temporary.streaming,
                completion_bindings: None,
            }),
        };
        admission.tasks.push(runtime.spawn(Self::drive(owned)));
        let owner: Arc<dyn ReactionOwner> = self.clone();
        Ok(Reaction {
            state,
            owner: Arc::downgrade(&owner),
        })
    }

    async fn drive(mut owned: OwnedReaction<P>) {
        let self_owner = owned.owner.clone();
        let state = owned.state.clone();
        self_owner.drive_owned(&state, &mut owned.lane).await;
        owned.armed = false;
    }

    async fn drive_owned(&self, state: &Arc<ReactionState>, lane: &mut Option<Lane<P>>) {
        let cancellation = ReactionCancellationControl::new(self.monitor.clone());
        let operation = AssertUnwindSafe(self.drive_inner(state, lane, &cancellation))
            .catch_unwind()
            .await;
        let mut completion = match operation {
            Ok(result) => Completion::Result(result),
            Err(payload) => {
                self.fail(payload);
                Completion::Panic(self.panic.clone())
            }
        };
        // Keep all owned protocol state alive until its effects have settled,
        // including when a callback or provider unwinds.
        if let Some(lane) = lane.as_mut() {
            let cleanup = AssertUnwindSafe(self.settle(state, lane, &mut completion))
                .catch_unwind()
                .await;
            if let Err(payload) = cleanup {
                self.fail(payload);
                completion = Completion::Panic(self.panic.clone());
                self.drain_panicked_streaming(&mut lane.streaming).await;
            }
        }
        state
            .recovery
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.publish_recovery_fence(false);
        let retained = AssertUnwindSafe(async {
            let mut core_guard = self.core.lock().await;
            if let Some(core) = core_guard.as_mut() {
                if let Some(lane) = lane.take() {
                    match lane.port {
                        LanePort::Main(port) => {
                            core.port.0 = Some(port);
                            core.session = lane.session;
                            let mut candidate = None;
                            if matches!(
                                completion,
                                Completion::Result(Ok(ReactionOutcome::Completed))
                            ) && core.session.can_fork()
                            {
                                let fork = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                    core.port
                                        .0
                                        .as_mut()
                                        .expect("restored main port")
                                        .fork_temporary()
                                }));
                                match fork {
                                    Err(payload) => {
                                        self.fail(payload);
                                        completion = Completion::Panic(self.panic.clone());
                                    }
                                    Ok(Err(_)) => {}
                                    Ok(Ok(mut branch)) => {
                                        let declared =
                                            std::panic::catch_unwind(AssertUnwindSafe(|| {
                                                branch.declare().ok().and_then(|declaration| {
                                                    core.session.fork(&declaration).ok()
                                                })
                                            }));
                                        match declared {
                                            Ok(Some(session)) => {
                                                candidate = Some(Checkpoint {
                                                    port: branch,
                                                    session,
                                                })
                                            }
                                            other => {
                                                self.admission
                                                    .lock()
                                                    .unwrap_or_else(|p| p.into_inner())
                                                    .retired
                                                    .push(branch);
                                                if let Err(payload) = other {
                                                    self.fail(payload);
                                                    completion =
                                                        Completion::Panic(self.panic.clone());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            let mut admission =
                                self.admission.lock().unwrap_or_else(|p| p.into_inner());
                            if let Some(candidate) = candidate {
                                if let Some(old) = admission.checkpoint.replace(candidate) {
                                    admission.retired.push(old.port);
                                }
                            }
                        }
                        LanePort::Temporary(port) => {
                            self.admission
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .continuations
                                .insert(
                                    state.lineage,
                                    Temporary {
                                        port,
                                        session: lane.session,
                                        streaming: lane.streaming,
                                        owner: Some(Arc::downgrade(state)),
                                    },
                                );
                        }
                    }
                }
                self.publish(core);
            }
        })
        .catch_unwind()
        .await;
        if let Err(payload) = retained {
            self.fail(payload);
            completion = Completion::Panic(self.panic.clone());
        }
        let retired = {
            std::mem::take(
                &mut self
                    .admission
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retired,
            )
        };
        for mut port in retired {
            if let Err(payload) = AssertUnwindSafe(port.shutdown()).catch_unwind().await {
                self.fail(payload);
                completion = Completion::Panic(self.panic.clone());
            }
        }
        {
            let mut admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
            admission.active.remove(&state.id);
            if state.kind == ReactionKind::Main {
                admission.main = false;
            }
        }
        if self.panic.1.load(std::sync::atomic::Ordering::Acquire) {
            completion = Completion::Panic(self.panic.clone());
        }
        state.finish(completion);
        self.settled.notify_waiters();
    }

    fn fail(&self, payload: PanicPayload) {
        let mut panic = self.panic.0.lock().unwrap_or_else(|p| p.into_inner());
        if !self.panic.1.swap(true, std::sync::atomic::Ordering::AcqRel) {
            *panic = Some(payload);
        }
        self.state.store(
            APPLICATION_TERMINATED_AFTER_TASK_PANIC,
            std::sync::atomic::Ordering::Release,
        );
        drop(panic);
        let admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        for reaction in admission.active.values() {
            reaction.interrupt();
        }
    }

    async fn drain_panicked_streaming(&self, streaming: &mut StreamingSupervisor) {
        if streaming.pending() {
            if let Err(payload) =
                AssertUnwindSafe(streaming.abort(StreamingToolAbortCause::RuntimeFault))
                    .catch_unwind()
                    .await
            {
                self.fail(payload);
            }
            while !streaming.clear() {
                if let Err(payload) = AssertUnwindSafe(streaming.recover()).catch_unwind().await {
                    self.fail(payload);
                }
                if !streaming.clear() {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
            if let Some(payload) = streaming.take_panic_if_clear() {
                self.fail(payload);
            }
            streaming.release();
        }
    }

    async fn drive_inner(
        &self,
        state: &ReactionState,
        lane: &mut Option<Lane<P>>,
        cancellation: &ReactionCancellationControl,
    ) -> Result<ReactionOutcome, ApplicationFault> {
        // Admission owns identity already. Preparation remains one serialized
        // mutation of the mounted runtime, independently of all network streams.
        let prepare = async {
            let mut guard = self.core.lock().await;
            let core = guard.as_mut().ok_or_else(owner_closed)?;
            let discarded = std::mem::take(
                &mut self
                    .admission
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .discarded,
            );
            for lineage in discarded {
                core.components.discard_temporary_history(lineage);
            }
            core.check_task_panic(ApplicationFaultStage::Preparation)?;
            if state.kind == ReactionKind::Main {
                core.refresh_declaration()?;
            }
            core.components.set_reaction_history_scope(
                (state.kind == ReactionKind::Temporary).then_some(state.lineage),
            );
            let prepared = core.prepare_with_exit().await;
            self.publish(core);
            let (projection, bindings) = match prepared? {
                ControlFlow::Break(reason) => return Ok(ControlFlow::Break(reason)),
                ControlFlow::Continue(prepared) => prepared,
            };
            if state.kind == ReactionKind::Main {
                let session = FrameSession::new(&core.declaration).map_err(|fault| {
                    ApplicationFault::from_session(ApplicationFaultStage::Declaration, fault)
                })?;
                let session = std::mem::replace(&mut core.session, session);
                let port = core.port.0.take().ok_or_else(not_ready)?;
                *lane = Some(Lane {
                    port: LanePort::Main(port),
                    session,
                    streaming: StreamingSupervisor::default(),
                    completion_bindings: None,
                });
            }
            let receipt = (state.kind == ReactionKind::Main)
                .then(|| {
                    core.command_input
                        .as_ref()
                        .map(CommandInput::feedback_receipt)
                })
                .flatten();
            Ok::<_, ApplicationFault>(ControlFlow::Continue((projection, bindings, receipt)))
        };
        let prepared = tokio::select! {
            biased;
            _ = state.cancelled() => return Ok(ReactionOutcome::Interrupted),
            task = self.monitor.wait() => { let _ = task; cancellation.suppress(); self.check()?; return Err(owner_closed()); }
            prepared = prepare => prepared?,
        };
        let (projection, mut bindings, receipt) = match prepared {
            ControlFlow::Break(reason) => return Ok(ReactionOutcome::Exited(reason)),
            ControlFlow::Continue(prepared) => prepared,
        };
        let lane = lane.as_mut().expect("prepared request owns a lane");
        let result = {
            let submission = self.submit(
                state,
                lane,
                &projection,
                &mut bindings,
                cancellation,
                receipt,
            );
            tokio::pin!(submission);
            tokio::select! {
                biased;
                _ = state.cancelled() => Ok(ReactionOutcome::Interrupted),
                task = self.monitor.wait() => { let _ = task; cancellation.suppress(); self.check()?; Err(owner_closed()) }
                result = &mut submission => result,
            }
        };
        if lane.streaming.normal_eof
            && lane.streaming.pending()
            && !lane.streaming.clear()
            && matches!(result, Err(fault) if fault.kind() == ApplicationFaultKind::RecoveryRequired)
        {
            lane.completion_bindings = Some(bindings);
        }
        result
    }

    async fn submit(
        &self,
        state: &ReactionState,
        lane: &mut Lane<P>,
        projection: &RenderedProjection,
        bindings: &mut RenderBindings<ProviderEvent>,
        cancellation: &ReactionCancellationControl,
        receipt: Option<crate::component::execution::command::CommandFeedbackReceipt>,
    ) -> Result<ReactionOutcome, ApplicationFault> {
        for attempt in 0..2 {
            if let Err(reason) = self.exit.try_begin_submission() {
                return Ok(ReactionOutcome::Exited(reason));
            }
            let declaration = lane.port.declare().map_err(|fault| {
                ApplicationFault::from_port(ApplicationFaultStage::Declaration, fault)
            })?;
            lane.session
                .observe_declaration(&declaration)
                .map_err(|fault| {
                    ApplicationFault::from_session(ApplicationFaultStage::Declaration, fault)
                })?;
            let prepared = lane
                .session
                .prepare(&declaration, projection)
                .map_err(|fault| {
                    ApplicationFault::from_session(ApplicationFaultStage::FramePrepare, fault)
                })?;
            let revision = prepared.revision();
            let facts =
                match submit_prepared_frame(&mut lane.port, &mut lane.session, prepared).await {
                    Ok(facts) => facts,
                    Err(SubmitFault::ContinuityChanged) if attempt == 0 => continue,
                    Err(fault) => return Err(ApplicationFault::from_submit(fault)),
                };
            if let Some(receipt) = &receipt {
                receipt.commit();
            }
            let mut scope =
                EventScope::reaction_with_kind(self.app_id, state.id, state.kind, revision);
            if state.kind == ReactionKind::Temporary {
                scope = scope.with_temporary_lineage(state.lineage);
            }
            bindings.set_event_scope(scope);
            let mut lease = lane
                .streaming
                .start(bindings.take_streaming_contracts(), scope)?;
            let result = pump_provider_facts(
                &mut lane.session,
                bindings,
                facts,
                cancellation.clone(),
                &mut lane.streaming,
                true,
            )
            .await;
            lease.complete();
            result?;
            if lane.streaming.pending() && !lane.streaming.clear() {
                return Err(ApplicationFault::streaming_recovery());
            }
            return Ok(match self.exit.reason() {
                Some(reason) => ReactionOutcome::Exited(reason),
                None => ReactionOutcome::Completed,
            });
        }
        unreachable!("second continuity failure returns a classified fault")
    }

    async fn settle(&self, state: &ReactionState, lane: &mut Lane<P>, completion: &mut Completion) {
        let normal = matches!(
            completion,
            Completion::Result(Ok(ReactionOutcome::Completed))
        );
        let initial_fault = match completion {
            Completion::Result(Err(fault)) => Some(*fault),
            _ => None,
        };
        let accepted_recovery = initial_fault
            .is_some_and(|fault| fault.kind() == ApplicationFaultKind::RecoveryRequired)
            && lane.streaming.normal_eof;
        let managed = lane.streaming.pending();
        if managed && !normal && !accepted_recovery {
            lane.streaming.saved_fault = initial_fault
                .filter(|fault| fault.kind() != ApplicationFaultKind::RecoveryRequired);
            *state.recovery.lock().unwrap_or_else(|p| p.into_inner()) = Some(RecoveryControl {
                requests: None,
                observer: lane.streaming.clone(),
            });
            self.publish_recovery_fence(false);
            let cause = if matches!(
                completion,
                Completion::Result(Ok(ReactionOutcome::Interrupted | ReactionOutcome::Exited(_)))
            ) {
                StreamingToolAbortCause::Cancelled
            } else {
                StreamingToolAbortCause::RuntimeFault
            };
            lane.streaming.abort(cause).await;
        }
        let reconcile = self.settle_projection(state, lane).await;
        if let Err(fault) = reconcile {
            lane.streaming.saved_fault.get_or_insert(fault);
            if matches!(completion, Completion::Result(Ok(_))) {
                *completion = Completion::Result(Err(fault));
            }
        }
        if managed && (!lane.streaming.clear() || reconcile.is_err()) {
            let (requests, mut recoveries) = tokio::sync::mpsc::unbounded_channel();
            loop {
                *state.recovery.lock().unwrap_or_else(|p| p.into_inner()) = Some(RecoveryControl {
                    requests: Some(requests.clone()),
                    observer: lane.streaming.clone(),
                });
                self.publish_recovery_fence(false);
                let closing = self
                    .admission
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closed;
                let request = if closing {
                    None
                } else {
                    recoveries.recv().await
                };
                if !lane.streaming.clear() {
                    lane.streaming.recover().await;
                }
                let reconciled = self.settle_projection(state, lane).await;
                if let Err(fault) = reconciled {
                    lane.streaming.saved_fault.get_or_insert(fault);
                    if matches!(completion, Completion::Result(Ok(_))) {
                        *completion = Completion::Result(Err(fault));
                    }
                }
                let clear = lane.streaming.clear() && reconciled.is_ok();
                if let Some(reply) = request {
                    let status = if clear {
                        StreamingToolRecoveryStatus::Recovered {
                            attempts: lane.streaming.reports(),
                            reaction_requested: lane.streaming.reaction_requested(),
                        }
                    } else if lane.streaming.busy() {
                        StreamingToolRecoveryStatus::InFlight {
                            attempts: lane.streaming.reports(),
                        }
                    } else {
                        StreamingToolRecoveryStatus::StillRequired {
                            attempts: lane.streaming.reports(),
                        }
                    };
                    let _ = reply.send(status);
                }
                if clear {
                    break;
                }
                if closing {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
        }
        state
            .recovery
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(payload) = lane.streaming.take_panic_if_clear() {
            resume_unwind(payload);
        }
        let requested = managed && lane.streaming.reaction_requested();
        if managed {
            lane.streaming.release();
        }
        self.publish_recovery_fence(requested);
    }

    async fn settle_projection(
        &self,
        state: &ReactionState,
        lane: &mut Lane<P>,
    ) -> Result<(), ApplicationFault> {
        if !lane.streaming.clear() {
            return Ok(());
        }
        if self
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closed
        {
            return Ok(());
        }
        if self.state.load(std::sync::atomic::Ordering::Acquire) != super::APPLICATION_READY {
            return Ok(());
        }
        if let Some(payload) = lane.streaming.take_panic_if_clear() {
            resume_unwind(payload);
        }
        if let Some(mut bindings) = lane.completion_bindings.take() {
            if !state.interrupted.load(std::sync::atomic::Ordering::Acquire)
                && !lane
                    .streaming
                    .reports()
                    .iter()
                    .any(|report| report.fault.is_some())
            {
                bindings.finish_normal().await.map_err(|fault| {
                    ApplicationFault::from_attempt(ApplicationFaultStage::Binding, fault)
                })?;
            }
        }
        let mut guard = self.core.lock().await;
        if let Some(core) = guard.as_mut() {
            if core.components.is_dirty() {
                core.reconcile_components(ApplicationFaultStage::PostReconcile)
                    .await?;
            }
            self.publish(core);
        }
        Ok(())
    }

    fn close(&self) {
        let mut admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        admission.closed = true;
        for state in admission.active.values() {
            state.interrupt();
            if let Some(recovery) = state
                .recovery
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                let (reply, _) = tokio::sync::oneshot::channel();
                if let Some(requests) = &recovery.requests {
                    let _ = requests.send(reply);
                }
            }
        }
        self.exit.close();
    }
    async fn shutdown_all(&self) -> Result<(), ApplicationFault> {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if self
                .admission
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .active
                .is_empty()
            {
                break;
            }
            settled.await;
        }
        let (tasks, mut ports, orphaned) = {
            let mut admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
            let mut ports = std::mem::take(&mut admission.retired);
            if let Some(checkpoint) = admission.checkpoint.take() {
                ports.push(checkpoint.port);
            }
            ports.extend(
                admission
                    .continuations
                    .drain()
                    .map(|(_, temporary)| temporary.port),
            );
            (
                std::mem::take(&mut admission.tasks),
                ports,
                std::mem::take(&mut admission.orphaned),
            )
        };
        let mut panic = None;
        for mut lane in orphaned {
            if let Err(payload) = AssertUnwindSafe(lane.port.shutdown()).catch_unwind().await {
                panic.get_or_insert(payload);
            }
        }
        for task in tasks {
            if let Err(error) = task.await {
                if error.is_panic() {
                    panic.get_or_insert(error.into_panic());
                }
            }
        }
        for port in &mut ports {
            if let Err(payload) = AssertUnwindSafe(port.shutdown()).catch_unwind().await {
                panic.get_or_insert(payload);
            }
        }
        let core = self.core.lock().await.take();
        let result = match core {
            Some(core) => match AssertUnwindSafe(core.shutdown()).catch_unwind().await {
                Ok(result) => result,
                Err(payload) => {
                    panic.get_or_insert(payload);
                    Err(owner_closed())
                }
            },
            None => Ok(()),
        };
        self.panic.raise();
        if let Some(payload) = panic {
            resume_unwind(payload);
        }
        result
    }
}
impl<P: ReactionPort + 'static> ReactionOwner for Shared<P> {
    fn continue_reaction(
        self: Arc<Self>,
        source: &ReactionState,
    ) -> Result<Reaction, ApplicationFault> {
        self.admit(Some(source))
    }
}
