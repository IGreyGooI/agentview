//! Mount-scoped asynchronous task supervision.
//!
//! The supervisor is deliberately independent from Component rendering. A
//! render transaction linearizes mount validity, then uses this module's
//! non-blocking `start` and `retire` operations while it still owns that fence.

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::Poll,
};

use futures::FutureExt;
use tokio::{
    runtime::Handle,
    sync::{mpsc, Notify},
    task::{AbortHandle, Id as TokioTaskId, JoinError, JoinHandle, JoinSet},
};

use super::ComponentId;

type PanicPayload = Box<dyn Any + Send + 'static>;
type BoxTaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Identity of one mounted Component generation in the task registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MountTaskScope {
    component: ComponentId,
    generation: u64,
}

impl MountTaskScope {
    pub(crate) fn new(component: ComponentId, generation: u64) -> Self {
        Self {
            component,
            generation,
        }
    }

    pub(crate) fn component(&self) -> &ComponentId {
        &self.component
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }
}

type RetirementClaim = HashMap<ComponentId, u64>;

/// A short-lived stale-start fence owned by one queued or active retirement.
///
/// Permanent stale authority belongs to `MountFence`: registration holds that
/// fence through `start`, while unmount invalidates it before calling `retire`.
/// Therefore either `Start` is sent before invalidation or it is never sent.
/// The core mutex serializes those sends and the actor consumes them FIFO, so a
/// claim is no longer needed after every matching task has aborted and joined.
#[derive(Default)]
struct ActiveRetirementClaims {
    generations: HashMap<ComponentId, Vec<u64>>,
}

impl ActiveRetirementClaims {
    fn install(&mut self, claim: &RetirementClaim) {
        for (component, generation) in claim {
            self.generations
                .entry(component.clone())
                .or_default()
                .push(*generation);
        }
    }

    fn release(&mut self, claim: &RetirementClaim) {
        for (component, generation) in claim {
            let remove_component = self.generations.get_mut(component).is_some_and(|active| {
                if let Some(index) = active.iter().position(|candidate| candidate == generation) {
                    active.swap_remove(index);
                }
                active.is_empty()
            });
            if remove_component {
                self.generations.remove(component);
            }
        }
    }

    fn rejects(&self, scope: &MountTaskScope) -> bool {
        self.generations
            .get(scope.component())
            .is_some_and(|active| {
                active
                    .iter()
                    .any(|generation| scope.generation() <= *generation)
            })
    }

    #[cfg(test)]
    fn component_count(&self) -> usize {
        self.generations.len()
    }

    fn clear(&mut self) {
        self.generations.clear();
    }
}

fn retirement_claim(scopes: &HashSet<MountTaskScope>) -> RetirementClaim {
    let mut claim: RetirementClaim = HashMap::new();
    for scope in scopes {
        claim
            .entry(scope.component().clone())
            .and_modify(|generation| *generation = (*generation).max(scope.generation()))
            .or_insert(scope.generation());
    }
    claim
}

/// Payload-free failure at the task-supervisor boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MountTaskSupervisorError {
    #[error("Component tasks require an active Tokio runtime")]
    RuntimeUnavailable,
    #[error("the Component task supervisor is closed")]
    Closed,
    #[error("the Component task supervisor has observed a task panic")]
    Panicked,
    #[error("the Component task scope belongs to a retired mount")]
    StaleMount,
}

/// Application-owned task supervisor.
///
/// The Tokio actor and its `JoinSet` are created only when the first task is
/// accepted. Dropping this owner synchronously closes submission and aborts the
/// actor; dropping the actor aborts every task still owned by its `JoinSet`.
pub(crate) struct MountTaskSupervisor {
    core: Arc<SupervisorCore>,
}

impl MountTaskSupervisor {
    pub(crate) fn new() -> Self {
        Self {
            core: Arc::new(SupervisorCore::new()),
        }
    }

    pub(crate) fn handle(&self) -> MountTaskHandle {
        MountTaskHandle {
            core: Arc::downgrade(&self.core),
        }
    }

    pub(crate) fn panic_monitor(&self) -> TaskPanicMonitor {
        TaskPanicMonitor {
            core: Arc::clone(&self.core),
        }
    }

    /// Register a task without awaiting or polling the user future.
    #[cfg(test)]
    pub(crate) fn start<F>(
        &self,
        scope: MountTaskScope,
        future: F,
    ) -> Result<(), MountTaskSupervisorError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.core.start(scope, Box::pin(future))
    }

    /// Fence scopes and request cancellation of every task they own.
    pub(crate) fn retire(
        &self,
        scopes: impl IntoIterator<Item = MountTaskScope>,
    ) -> Result<TaskRetirement, MountTaskSupervisorError> {
        self.core.retire(scopes)
    }

    #[cfg(test)]
    async fn bookkeeping(&self) -> Result<TaskSupervisorBookkeeping, MountTaskSupervisorError> {
        self.core.bookkeeping().await
    }

    /// Gracefully abort and join all tasks plus the actor itself.
    pub(crate) async fn shutdown(&mut self) -> Result<(), MountTaskSupervisorError> {
        self.core.ensure_current_actor_runtime()?;
        if self.core.has_actor_failed() && !self.core.has_panicked() {
            return Err(MountTaskSupervisorError::Closed);
        }
        let Some(actor) = self.core.begin_shutdown(false) else {
            return if self.core.has_panicked() {
                Err(MountTaskSupervisorError::Panicked)
            } else {
                Ok(())
            };
        };
        let ActorControl {
            runtime_id,
            sender,
            mut join,
        } = actor;
        let _ = sender.send(ActorCommand::Shutdown);
        drop(sender);
        let joined = std::future::poll_fn(|context| {
            let on_owning_runtime =
                Handle::try_current().is_ok_and(|current| current.id() == runtime_id);
            if !on_owning_runtime {
                join.abort();
                self.core.close_after_actor_failure();
                return Poll::Ready(Err(MountTaskSupervisorError::Closed));
            }
            match Pin::new(&mut join).poll(context) {
                Poll::Ready(result) => Poll::Ready(Ok(result)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        if joined.is_err() {
            self.core.close_after_actor_failure();
            return Err(MountTaskSupervisorError::Closed);
        }
        if self.core.has_panicked() {
            Err(MountTaskSupervisorError::Panicked)
        } else if self.core.has_actor_failed() {
            Err(MountTaskSupervisorError::Closed)
        } else {
            Ok(())
        }
    }
}

impl Default for MountTaskSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MountTaskSupervisor {
    fn drop(&mut self) {
        let Some(actor) = self.core.begin_shutdown(true) else {
            return;
        };
        let ActorControl {
            runtime_id: _,
            sender,
            join,
        } = actor;
        let _ = sender.send(ActorCommand::Shutdown);
        join.abort();
        // Rust Drop cannot await. The cancellation request is synchronous, and
        // dropping the actor future drops its JoinSet, which aborts owned tasks.
    }
}

/// Cloneable submission capability installed in committed Component scopes.
#[derive(Clone)]
pub(crate) struct MountTaskHandle {
    core: Weak<SupervisorCore>,
}

impl MountTaskHandle {
    pub(crate) fn preflight(&self) -> Result<(), MountTaskSupervisorError> {
        self.core
            .upgrade()
            .ok_or(MountTaskSupervisorError::Closed)?
            .preflight()
    }

    pub(crate) fn start<F>(
        &self,
        scope: MountTaskScope,
        future: F,
    ) -> Result<(), MountTaskSupervisorError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.core
            .upgrade()
            .ok_or(MountTaskSupervisorError::Closed)?
            .start(scope, Box::pin(future))
    }
}

/// Cancellation-safe completion fence for one retirement request.
#[derive(Clone)]
pub(crate) struct TaskRetirement {
    state: Arc<RetirementState>,
}

impl TaskRetirement {
    fn pending(core: &Arc<SupervisorCore>) -> Self {
        Self {
            state: Arc::new(RetirementState::new(
                RetirementStatus::Pending,
                Arc::downgrade(core),
            )),
        }
    }

    fn completed() -> Self {
        Self {
            state: Arc::new(RetirementState::new(
                RetirementStatus::Complete,
                Weak::new(),
            )),
        }
    }

    pub(crate) async fn wait(&self) -> Result<(), MountTaskSupervisorError> {
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let finished = std::future::poll_fn(|context| {
                match *lock_unpoisoned(&self.state.status) {
                    RetirementStatus::Pending => {}
                    RetirementStatus::Complete => return Poll::Ready(Ok(true)),
                    RetirementStatus::Closed => {
                        return Poll::Ready(Err(MountTaskSupervisorError::Closed));
                    }
                }
                let Some(core) = self.state.core.upgrade() else {
                    return Poll::Ready(Err(MountTaskSupervisorError::Closed));
                };
                if let Err(error) = core.ensure_current_actor_runtime() {
                    return Poll::Ready(Err(error));
                }
                match changed.as_mut().poll(context) {
                    Poll::Ready(()) => Poll::Ready(Ok(false)),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await?;
            if finished {
                return Ok(());
            }
        }
    }

    fn complete(&self) {
        self.state.finish(RetirementStatus::Complete);
    }

    fn close(&self) {
        self.state.finish(RetirementStatus::Closed);
    }
}

struct RetirementState {
    status: Mutex<RetirementStatus>,
    changed: Notify,
    core: Weak<SupervisorCore>,
}

impl RetirementState {
    fn new(status: RetirementStatus, core: Weak<SupervisorCore>) -> Self {
        Self {
            status: Mutex::new(status),
            changed: Notify::new(),
            core,
        }
    }

    fn finish(&self, next: RetirementStatus) {
        let mut status = lock_unpoisoned(&self.status);
        if *status == RetirementStatus::Pending {
            *status = next;
            drop(status);
            self.changed.notify_waiters();
        }
    }

    fn is_pending(&self) -> bool {
        *lock_unpoisoned(&self.status) == RetirementStatus::Pending
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetirementStatus {
    Pending,
    Complete,
    Closed,
}

/// Waiter for the first supervised task panic.
#[derive(Clone)]
pub(crate) struct TaskPanicMonitor {
    core: Arc<SupervisorCore>,
}

/// Synchronous supervisor state used to arbitrate task failure against a
/// concurrently completed application operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskSupervisorStatus {
    Healthy,
    Panicked,
    Closed,
}

impl TaskPanicMonitor {
    pub(crate) fn status(&self) -> TaskSupervisorStatus {
        self.core.fence_foreign_runtime();
        self.core.status()
    }

    /// Resolve as soon as the first panic is latched, before sibling drain.
    pub(crate) async fn wait(&self) -> Result<(), MountTaskSupervisorError> {
        self.core.wait_for_panic().await
    }

    /// Take the first panic payload without waiting for sibling cleanup.
    pub(crate) fn take_payload(&self) -> Option<PanicPayload> {
        lock_unpoisoned(&self.core.state).panic_payload.take()
    }
}

struct SupervisorCore {
    state: Mutex<SupervisorState>,
    changed: Notify,
}

impl SupervisorCore {
    fn new() -> Self {
        Self {
            state: Mutex::new(SupervisorState {
                accepting: true,
                closed: false,
                panicked: false,
                panic_payload: None,
                actor_failed: false,
                actor: None,
                retirement_claims: ActiveRetirementClaims::default(),
                retirements: Vec::new(),
            }),
            changed: Notify::new(),
        }
    }

    fn start(
        self: &Arc<Self>,
        scope: MountTaskScope,
        future: BoxTaskFuture,
    ) -> Result<(), MountTaskSupervisorError> {
        self.ensure_current_actor_runtime()?;
        let mut state = lock_unpoisoned(&self.state);
        Self::ensure_accepting(&state)?;
        if state.retirement_claims.rejects(&scope) {
            return Err(MountTaskSupervisorError::StaleMount);
        }
        Self::ensure_actor(self, &mut state)?;
        let sent = state
            .actor
            .as_ref()
            .expect("a started supervisor has an actor")
            .sender
            .send(ActorCommand::Start { scope, future });
        if sent.is_err() {
            state.accepting = false;
            state.closed = true;
            Self::close_retirements(&mut state);
            drop(state);
            self.changed.notify_waiters();
            return Err(MountTaskSupervisorError::Closed);
        }
        Ok(())
    }

    fn preflight(&self) -> Result<(), MountTaskSupervisorError> {
        self.ensure_current_actor_runtime()?;
        let state = lock_unpoisoned(&self.state);
        Self::ensure_accepting(&state)?;
        drop(state);
        Handle::try_current()
            .map(|_| ())
            .map_err(|_| MountTaskSupervisorError::RuntimeUnavailable)
    }

    fn retire(
        self: &Arc<Self>,
        scopes: impl IntoIterator<Item = MountTaskScope>,
    ) -> Result<TaskRetirement, MountTaskSupervisorError> {
        self.ensure_current_actor_runtime()?;
        let scopes = scopes.into_iter().collect::<HashSet<_>>();
        if scopes.is_empty() {
            return Ok(TaskRetirement::completed());
        }

        let mut state = lock_unpoisoned(&self.state);
        Self::ensure_accepting(&state)?;
        Self::prune_retirements(&mut state);
        let Some(sender) = state.actor.as_ref().map(|actor| actor.sender.clone()) else {
            return Ok(TaskRetirement::completed());
        };

        let claim = retirement_claim(&scopes);
        state.retirement_claims.install(&claim);
        let retirement = TaskRetirement::pending(self);
        state.retirements.push(Arc::downgrade(&retirement.state));
        let sent = sender.send(ActorCommand::Retire {
            scopes,
            claim,
            retirement: retirement.clone(),
        });
        if sent.is_err() {
            state.accepting = false;
            state.closed = true;
            Self::close_retirements(&mut state);
            drop(state);
            self.changed.notify_waiters();
            return Err(MountTaskSupervisorError::Closed);
        }
        Ok(retirement)
    }

    fn complete_retirement(&self, claim: &RetirementClaim, retirement: &TaskRetirement) {
        {
            let mut state = lock_unpoisoned(&self.state);
            state.retirement_claims.release(claim);
            let completed = Arc::as_ptr(&retirement.state);
            state.retirements.retain(|candidate| {
                candidate.as_ptr() != completed && candidate.strong_count() > 0
            });
        }
        retirement.complete();
    }

    fn prune_retirements(state: &mut SupervisorState) {
        state.retirements.retain(|retirement| {
            retirement
                .upgrade()
                .is_some_and(|retirement| retirement.is_pending())
        });
    }

    fn ensure_actor(
        core: &Arc<Self>,
        state: &mut SupervisorState,
    ) -> Result<(), MountTaskSupervisorError> {
        if state.actor.is_some() {
            return Ok(());
        }
        let runtime =
            Handle::try_current().map_err(|_| MountTaskSupervisorError::RuntimeUnavailable)?;
        let runtime_id = runtime.id();
        let (sender, receiver) = mpsc::unbounded_channel();
        let lifecycle = ActorLifecycle::new(Arc::downgrade(core));
        let join = runtime.spawn(run_actor(lifecycle, receiver));
        state.actor = Some(ActorControl {
            runtime_id,
            sender,
            join,
        });
        core.changed.notify_waiters();
        Ok(())
    }

    fn ensure_current_actor_runtime(&self) -> Result<(), MountTaskSupervisorError> {
        let mut state = lock_unpoisoned(&self.state);
        let Some(runtime_id) = state.actor.as_ref().map(|actor| actor.runtime_id) else {
            return Ok(());
        };
        let current =
            Handle::try_current().map_err(|_| MountTaskSupervisorError::RuntimeUnavailable)?;
        if runtime_id != current.id() {
            Self::fence_actor_runtime_mismatch(&mut state);
            drop(state);
            self.changed.notify_waiters();
            return Err(MountTaskSupervisorError::Closed);
        }
        Ok(())
    }

    fn fence_foreign_runtime(&self) {
        let Ok(current) = Handle::try_current() else {
            return;
        };
        let mut state = lock_unpoisoned(&self.state);
        if state
            .actor
            .as_ref()
            .is_some_and(|actor| actor.runtime_id != current.id())
        {
            Self::fence_actor_runtime_mismatch(&mut state);
            drop(state);
            self.changed.notify_waiters();
        }
    }

    fn fence_actor_runtime_mismatch(state: &mut SupervisorState) {
        state.accepting = false;
        state.closed = true;
        state.actor_failed = true;
        if let Some(actor) = &state.actor {
            actor.join.abort();
        }
        Self::close_retirements(state);
    }

    fn ensure_accepting(state: &SupervisorState) -> Result<(), MountTaskSupervisorError> {
        if state.panicked {
            Err(MountTaskSupervisorError::Panicked)
        } else if !state.accepting || state.closed {
            Err(MountTaskSupervisorError::Closed)
        } else {
            Ok(())
        }
    }

    fn begin_shutdown(&self, dropping: bool) -> Option<ActorControl> {
        let mut state = lock_unpoisoned(&self.state);
        state.accepting = false;
        let actor = state.actor.take();
        if actor.is_none() || dropping {
            state.closed = true;
        }
        if dropping {
            Self::close_retirements(&mut state);
        }
        drop(state);
        self.changed.notify_waiters();
        actor
    }

    fn close_after_actor_failure(&self) {
        let mut state = lock_unpoisoned(&self.state);
        state.accepting = false;
        state.closed = true;
        state.actor_failed = true;
        Self::close_retirements(&mut state);
        drop(state);
        self.changed.notify_waiters();
    }

    fn close_retirements(state: &mut SupervisorState) {
        state.retirement_claims.clear();
        for retirement in std::mem::take(&mut state.retirements) {
            if let Some(retirement) = retirement.upgrade() {
                TaskRetirement { state: retirement }.close();
            }
        }
    }

    fn latch_panic(&self, payload: PanicPayload) {
        let mut state = lock_unpoisoned(&self.state);
        if !state.panicked {
            state.accepting = false;
            state.panicked = true;
            state.panic_payload = Some(payload);
            drop(state);
            self.changed.notify_waiters();
        }
    }

    fn mark_actor_finished(&self) {
        let mut state = lock_unpoisoned(&self.state);
        state.accepting = false;
        state.closed = true;
        drop(state);
        self.changed.notify_waiters();
    }

    fn has_panicked(&self) -> bool {
        lock_unpoisoned(&self.state).panicked
    }

    fn has_actor_failed(&self) -> bool {
        lock_unpoisoned(&self.state).actor_failed
    }

    fn status(&self) -> TaskSupervisorStatus {
        let state = lock_unpoisoned(&self.state);
        if state.panicked {
            TaskSupervisorStatus::Panicked
        } else if !state.accepting || state.closed {
            TaskSupervisorStatus::Closed
        } else {
            TaskSupervisorStatus::Healthy
        }
    }

    async fn wait_for_panic(&self) -> Result<(), MountTaskSupervisorError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let finished = std::future::poll_fn(|context| {
                let state = lock_unpoisoned(&self.state);
                if state.panicked {
                    return Poll::Ready(Ok(true));
                }
                if state.closed {
                    return Poll::Ready(Err(if state.panicked {
                        MountTaskSupervisorError::Panicked
                    } else {
                        MountTaskSupervisorError::Closed
                    }));
                }
                drop(state);
                if let Err(error) = self.ensure_current_actor_runtime() {
                    return Poll::Ready(Err(error));
                }
                match changed.as_mut().poll(context) {
                    Poll::Ready(()) => Poll::Ready(Ok(false)),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await?;
            if finished {
                return Ok(());
            }
        }
    }

    #[cfg(test)]
    async fn bookkeeping(&self) -> Result<TaskSupervisorBookkeeping, MountTaskSupervisorError> {
        self.ensure_current_actor_runtime()?;
        let sender = {
            let mut state = lock_unpoisoned(&self.state);
            Self::ensure_accepting(&state)?;
            Self::prune_retirements(&mut state);
            state
                .actor
                .as_ref()
                .map(|actor| actor.sender.clone())
                .ok_or(MountTaskSupervisorError::Closed)?
        };
        let (reply, mut response) = tokio::sync::oneshot::channel();
        sender
            .send(ActorCommand::Bookkeeping { reply })
            .map_err(|_| MountTaskSupervisorError::Closed)?;
        let actor = std::future::poll_fn(|context| {
            if let Err(error) = self.ensure_current_actor_runtime() {
                return Poll::Ready(Err(error));
            }
            match Pin::new(&mut response).poll(context) {
                Poll::Ready(Ok(actor)) => Poll::Ready(Ok(actor)),
                Poll::Ready(Err(_)) => Poll::Ready(Err(MountTaskSupervisorError::Closed)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        let state = lock_unpoisoned(&self.state);
        Ok(TaskSupervisorBookkeeping {
            core_retiring_components: state.retirement_claims.component_count(),
            core_retirement_waiters: state.retirements.len(),
            actor_retiring_components: actor.retiring_components,
            actor_live_tasks: actor.live_tasks,
            actor_pending_retirements: actor.pending_retirements,
        })
    }
}

struct SupervisorState {
    accepting: bool,
    closed: bool,
    panicked: bool,
    panic_payload: Option<PanicPayload>,
    actor_failed: bool,
    actor: Option<ActorControl>,
    retirement_claims: ActiveRetirementClaims,
    retirements: Vec<Weak<RetirementState>>,
}

struct ActorControl {
    runtime_id: tokio::runtime::Id,
    sender: mpsc::UnboundedSender<ActorCommand>,
    join: JoinHandle<()>,
}

enum ActorCommand {
    Start {
        scope: MountTaskScope,
        future: BoxTaskFuture,
    },
    Retire {
        scopes: HashSet<MountTaskScope>,
        claim: RetirementClaim,
        retirement: TaskRetirement,
    },
    Shutdown,
    #[cfg(test)]
    Bookkeeping {
        reply: tokio::sync::oneshot::Sender<ActorBookkeeping>,
    },
    #[cfg(test)]
    PanicForTest,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActorBookkeeping {
    retiring_components: usize,
    live_tasks: usize,
    pending_retirements: usize,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskSupervisorBookkeeping {
    core_retiring_components: usize,
    core_retirement_waiters: usize,
    actor_retiring_components: usize,
    actor_live_tasks: usize,
    actor_pending_retirements: usize,
}

struct ActorTask {
    scope: MountTaskScope,
    abort: AbortHandle,
}

struct ActorRetirement {
    remaining: HashSet<TokioTaskId>,
    claim: RetirementClaim,
    retirement: TaskRetirement,
}

struct ActorLifecycle {
    core: Weak<SupervisorCore>,
    armed: bool,
}

impl ActorLifecycle {
    fn new(core: Weak<SupervisorCore>) -> Self {
        Self { core, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActorLifecycle {
    fn drop(&mut self) {
        if self.armed {
            if let Some(core) = self.core.upgrade() {
                core.close_after_actor_failure();
            }
        }
    }
}

async fn run_actor(mut lifecycle: ActorLifecycle, commands: mpsc::UnboundedReceiver<ActorCommand>) {
    let core = lifecycle.core.clone();
    match AssertUnwindSafe(run_actor_inner(core, commands))
        .catch_unwind()
        .await
    {
        Ok(()) => lifecycle.disarm(),
        Err(payload) => {
            if let Some(core) = lifecycle.core.upgrade() {
                core.latch_panic(payload);
            }
        }
    }
}

async fn run_actor_inner(
    core: Weak<SupervisorCore>,
    mut commands: mpsc::UnboundedReceiver<ActorCommand>,
) {
    let mut tasks = JoinSet::<()>::new();
    let mut registry = HashMap::<TokioTaskId, ActorTask>::new();
    let mut retirement_claims = ActiveRetirementClaims::default();
    let mut retirements = Vec::<ActorRetirement>::new();
    loop {
        enum Next {
            Command(Option<ActorCommand>),
            Completion(Option<Result<(TokioTaskId, ()), JoinError>>),
        }

        let next = if tasks.is_empty() {
            Next::Command(commands.recv().await)
        } else {
            tokio::select! {
                biased;
                completion = tasks.join_next_with_id() => Next::Completion(completion),
                command = commands.recv() => Next::Command(command),
            }
        };

        match next {
            Next::Command(Some(ActorCommand::Start { scope, future })) => {
                if retirement_claims.rejects(&scope) {
                    if let Some(payload) = drop_task_future(future) {
                        if let Some(core) = core.upgrade() {
                            core.latch_panic(payload);
                        }
                        break;
                    }
                    continue;
                }
                let abort = tasks.spawn(future);
                registry.insert(abort.id(), ActorTask { scope, abort });
            }
            Next::Command(Some(ActorCommand::Retire {
                scopes,
                claim,
                retirement,
            })) => {
                retirement_claims.install(&claim);
                let remaining = registry
                    .iter()
                    .filter_map(|(id, task)| scopes.contains(&task.scope).then_some(*id))
                    .collect::<HashSet<_>>();
                for id in &remaining {
                    registry
                        .get(id)
                        .expect("retirement task came from the registry")
                        .abort
                        .abort();
                }
                if remaining.is_empty() {
                    complete_actor_retirement(
                        &core,
                        &mut retirement_claims,
                        ActorRetirement {
                            remaining,
                            claim,
                            retirement,
                        },
                    );
                } else {
                    retirements.push(ActorRetirement {
                        remaining,
                        claim,
                        retirement,
                    });
                }
            }
            Next::Completion(Some(completion)) => {
                let finished = finish_actor_task(completion, &mut registry, &mut retirements);
                let terminal_panic = finished.panic.is_some();
                if let Some(payload) = finished.panic {
                    if let Some(core) = core.upgrade() {
                        core.latch_panic(payload);
                    }
                }
                for retirement in finished.completed_retirements {
                    complete_actor_retirement(&core, &mut retirement_claims, retirement);
                }
                if terminal_panic {
                    break;
                }
            }
            Next::Command(Some(ActorCommand::Shutdown))
            | Next::Command(None)
            | Next::Completion(None) => break,
            #[cfg(test)]
            Next::Command(Some(ActorCommand::Bookkeeping { reply })) => {
                let _ = reply.send(ActorBookkeeping {
                    retiring_components: retirement_claims.component_count(),
                    live_tasks: registry.len(),
                    pending_retirements: retirements.len(),
                });
            }
            #[cfg(test)]
            Next::Command(Some(ActorCommand::PanicForTest)) => {
                panic!("injected task supervisor actor panic")
            }
        }
    }

    commands.close();
    // Abort already-running siblings before dropping queued futures. A queued
    // future may have blocking cleanup; it must not delay the cancellation
    // signal or publication of an already-latched panic.
    tasks.abort_all();
    let mut queued_retirements = Vec::new();
    while let Ok(command) = commands.try_recv() {
        match command {
            ActorCommand::Start { future, .. } => {
                if let Some(payload) = drop_task_future(future) {
                    if let Some(core) = core.upgrade() {
                        core.latch_panic(payload);
                    }
                }
            }
            ActorCommand::Retire {
                claim, retirement, ..
            } => queued_retirements.push((claim, retirement)),
            ActorCommand::Shutdown => {}
            #[cfg(test)]
            ActorCommand::Bookkeeping { .. } => {}
            #[cfg(test)]
            ActorCommand::PanicForTest => {}
        }
    }

    while let Some(completion) = tasks.join_next_with_id().await {
        let finished = finish_actor_task(completion, &mut registry, &mut retirements);
        if let Some(payload) = finished.panic {
            if let Some(core) = core.upgrade() {
                core.latch_panic(payload);
            }
        }
        for retirement in finished.completed_retirements {
            complete_actor_retirement(&core, &mut retirement_claims, retirement);
        }
    }
    debug_assert!(registry.is_empty());
    for retirement in retirements {
        complete_actor_retirement(&core, &mut retirement_claims, retirement);
    }
    for (claim, retirement) in queued_retirements {
        complete_retirement(&core, &claim, retirement);
    }

    if let Some(core) = core.upgrade() {
        core.mark_actor_finished();
    }
}

fn complete_actor_retirement(
    core: &Weak<SupervisorCore>,
    active: &mut ActiveRetirementClaims,
    retirement: ActorRetirement,
) {
    active.release(&retirement.claim);
    complete_retirement(core, &retirement.claim, retirement.retirement);
}

fn complete_retirement(
    core: &Weak<SupervisorCore>,
    claim: &RetirementClaim,
    retirement: TaskRetirement,
) {
    if let Some(core) = core.upgrade() {
        core.complete_retirement(claim, &retirement);
    } else {
        retirement.complete();
    }
}

fn drop_task_future(future: BoxTaskFuture) -> Option<PanicPayload> {
    catch_unwind(AssertUnwindSafe(|| drop(future))).err()
}

fn finish_actor_task(
    completion: Result<(TokioTaskId, ()), JoinError>,
    registry: &mut HashMap<TokioTaskId, ActorTask>,
    retirements: &mut Vec<ActorRetirement>,
) -> FinishedActorTask {
    let (id, panic) = match completion {
        Ok((id, ())) => (id, None),
        Err(error) => {
            let id = error.id();
            let panic = error.is_panic().then(|| error.into_panic());
            (id, panic)
        }
    };
    registry.remove(&id);

    let mut completed_retirements = Vec::new();
    let mut index = 0;
    while index < retirements.len() {
        retirements[index].remaining.remove(&id);
        if retirements[index].remaining.is_empty() {
            completed_retirements.push(retirements.swap_remove(index));
        } else {
            index += 1;
        }
    }
    FinishedActorTask {
        panic,
        completed_retirements,
    }
}

struct FinishedActorTask {
    panic: Option<PanicPayload>,
    completed_retirements: Vec<ActorRetirement>,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc as std_mpsc, Arc,
        },
        task::Poll,
        time::Duration,
    };

    use futures::poll;
    use tokio::sync::oneshot;

    use super::*;

    fn scope(generation: u64) -> MountTaskScope {
        MountTaskScope::new(ComponentId::root(), generation)
    }

    fn child_scope(position: usize) -> MountTaskScope {
        MountTaskScope::new(ComponentId::root().child("item", position), 1)
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct MustNotPoll {
        dropped: Arc<AtomicBool>,
    }

    impl Future for MustNotPoll {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            panic!("task registration polled the user future inline")
        }
    }

    impl Drop for MustNotPoll {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn first_start_requires_a_tokio_runtime_without_starting_an_actor() {
        let supervisor = MountTaskSupervisor::new();
        assert_eq!(
            supervisor.start(scope(1), async {}),
            Err(MountTaskSupervisorError::RuntimeUnavailable)
        );
    }

    #[tokio::test]
    async fn an_internal_actor_panic_is_transported_to_the_monitor() {
        let mut supervisor = MountTaskSupervisor::new();
        let monitor = supervisor.panic_monitor();
        assert_eq!(monitor.status(), TaskSupervisorStatus::Healthy);
        supervisor.start(scope(1), pending()).unwrap();

        let commands = lock_unpoisoned(&supervisor.core.state)
            .actor
            .as_ref()
            .expect("starting a task installs the actor")
            .sender
            .clone();
        commands.send(ActorCommand::PanicForTest).unwrap();
        monitor.wait().await.unwrap();

        assert_eq!(monitor.status(), TaskSupervisorStatus::Panicked);
        let payload = monitor.take_payload().expect("actor panic payload");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"injected task supervisor actor panic")
        );
        assert_eq!(
            supervisor.shutdown().await,
            Err(MountTaskSupervisorError::Panicked)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registration_is_nonblocking_and_retirement_drops_an_unpolled_task() {
        let mut supervisor = MountTaskSupervisor::new();
        let dropped = Arc::new(AtomicBool::new(false));
        supervisor
            .start(
                scope(1),
                MustNotPoll {
                    dropped: Arc::clone(&dropped),
                },
            )
            .unwrap();

        supervisor.retire([scope(1)]).unwrap().wait().await.unwrap();
        assert!(dropped.load(Ordering::Acquire));
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn normal_completion_only_removes_the_task() {
        let mut supervisor = MountTaskSupervisor::new();
        let monitor = supervisor.panic_monitor();
        let (completed_tx, completed_rx) = oneshot::channel();
        supervisor
            .start(scope(1), async move {
                let _ = completed_tx.send(());
            })
            .unwrap();
        completed_rx.await.unwrap();

        supervisor.retire([scope(1)]).unwrap().wait().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), monitor.wait())
                .await
                .is_err()
        );
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn repeated_same_component_retirement_keeps_bookkeeping_bounded() {
        const GENERATIONS: u64 = 512;

        let mut supervisor = MountTaskSupervisor::new();
        for generation in 1..=GENERATIONS {
            let (started, observed_start) = oneshot::channel();
            supervisor
                .start(scope(generation), async move {
                    let _ = started.send(());
                    pending::<()>().await;
                })
                .unwrap();
            observed_start.await.unwrap();
            supervisor
                .retire([scope(generation)])
                .unwrap()
                .wait()
                .await
                .unwrap();
        }

        assert_eq!(
            supervisor.bookkeeping().await.unwrap(),
            TaskSupervisorBookkeeping {
                core_retiring_components: 0,
                core_retirement_waiters: 0,
                actor_retiring_components: 0,
                actor_live_tasks: 0,
                actor_pending_retirements: 0,
            }
        );
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn distinct_component_retirements_converge_to_current_cardinality() {
        const COMPONENTS: usize = 512;

        let mut supervisor = MountTaskSupervisor::new();
        for position in 0..COMPONENTS {
            supervisor.start(child_scope(position), pending()).unwrap();
        }
        assert_eq!(
            supervisor.bookkeeping().await.unwrap(),
            TaskSupervisorBookkeeping {
                core_retiring_components: 0,
                core_retirement_waiters: 0,
                actor_retiring_components: 0,
                actor_live_tasks: COMPONENTS,
                actor_pending_retirements: 0,
            }
        );

        supervisor
            .retire((0..COMPONENTS - 1).map(child_scope))
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            supervisor.bookkeeping().await.unwrap(),
            TaskSupervisorBookkeeping {
                core_retiring_components: 0,
                core_retirement_waiters: 0,
                actor_retiring_components: 0,
                actor_live_tasks: 1,
                actor_pending_retirements: 0,
            }
        );

        supervisor
            .retire([child_scope(COMPONENTS - 1)])
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            supervisor.bookkeeping().await.unwrap(),
            TaskSupervisorBookkeeping {
                core_retiring_components: 0,
                core_retirement_waiters: 0,
                actor_retiring_components: 0,
                actor_live_tasks: 0,
                actor_pending_retirements: 0,
            }
        );
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retirement_aborts_only_the_selected_mount_and_awaits_drop() {
        let mut supervisor = MountTaskSupervisor::new();
        let first_dropped = Arc::new(AtomicBool::new(false));
        let second_dropped = Arc::new(AtomicBool::new(false));
        let (first_started_tx, first_started_rx) = oneshot::channel();
        let (second_started_tx, second_started_rx) = oneshot::channel();

        let first_probe = Arc::clone(&first_dropped);
        supervisor
            .start(scope(1), async move {
                let _guard = DropFlag(first_probe);
                let _ = first_started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        let second_probe = Arc::clone(&second_dropped);
        supervisor
            .start(scope(2), async move {
                let _guard = DropFlag(second_probe);
                let _ = second_started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        first_started_rx.await.unwrap();
        second_started_rx.await.unwrap();

        supervisor.retire([scope(1)]).unwrap().wait().await.unwrap();
        assert!(first_dropped.load(Ordering::Acquire));
        assert!(!second_dropped.load(Ordering::Acquire));

        supervisor.shutdown().await.unwrap();
        assert!(second_dropped.load(Ordering::Acquire));
    }

    struct BlockingDrop {
        started: Option<oneshot::Sender<()>>,
        release: std_mpsc::Receiver<()>,
    }

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            let _ = self.release.recv();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retirement_claim_rejects_stale_start_until_abort_join() {
        let mut supervisor = MountTaskSupervisor::new();
        let (task_started_tx, task_started_rx) = oneshot::channel();
        let (drop_started_tx, drop_started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        supervisor
            .start(scope(1), async move {
                let _guard = BlockingDrop {
                    started: Some(drop_started_tx),
                    release: release_rx,
                };
                let _ = task_started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        task_started_rx.await.unwrap();

        let retirement = supervisor.retire([scope(1)]).unwrap();
        drop_started_rx.await.unwrap();
        assert_eq!(
            supervisor.start(scope(1), async {}),
            Err(MountTaskSupervisorError::StaleMount)
        );
        assert_eq!(
            supervisor.bookkeeping().await.unwrap(),
            TaskSupervisorBookkeeping {
                core_retiring_components: 1,
                core_retirement_waiters: 1,
                actor_retiring_components: 1,
                actor_live_tasks: 1,
                actor_pending_retirements: 1,
            }
        );

        release_tx.send(()).unwrap();
        retirement.wait().await.unwrap();
        assert_eq!(
            supervisor.bookkeeping().await.unwrap(),
            TaskSupervisorBookkeeping {
                core_retiring_components: 0,
                core_retirement_waiters: 0,
                actor_retiring_components: 0,
                actor_live_tasks: 0,
                actor_pending_retirements: 0,
            }
        );
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_a_retirement_waiter_does_not_cancel_cleanup() {
        let mut supervisor = MountTaskSupervisor::new();
        let (task_started_tx, task_started_rx) = oneshot::channel();
        let (drop_started_tx, drop_started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        supervisor
            .start(scope(1), async move {
                let _guard = BlockingDrop {
                    started: Some(drop_started_tx),
                    release: release_rx,
                };
                let _ = task_started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        task_started_rx.await.unwrap();

        let retirement = supervisor.retire([scope(1)]).unwrap();
        drop_started_rx.await.unwrap();
        let mut cancelled = Box::pin(retirement.wait());
        assert!(matches!(poll!(cancelled.as_mut()), Poll::Pending));
        drop(cancelled);

        release_tx.send(()).unwrap();
        retirement.wait().await.unwrap();
        retirement.wait().await.unwrap();
        supervisor.shutdown().await.unwrap();
    }

    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("secondary task panic");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn first_task_panic_is_available_while_siblings_are_aborted_and_drained() {
        let mut supervisor = MountTaskSupervisor::new();
        let monitor = supervisor.panic_monitor();
        let sibling_dropped = Arc::new(AtomicBool::new(false));
        let (sibling_started_tx, sibling_started_rx) = oneshot::channel();
        let sibling_probe = Arc::clone(&sibling_dropped);
        supervisor
            .start(scope(1), async move {
                let _guard = DropFlag(sibling_probe);
                let _ = sibling_started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        let (secondary_started_tx, secondary_started_rx) = oneshot::channel();
        supervisor
            .start(scope(1), async move {
                let _guard = PanicOnDrop;
                let _ = secondary_started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        let (panic_tx, panic_rx) = oneshot::channel();
        supervisor
            .start(scope(1), async move {
                let _ = panic_rx.await;
                panic!("primary task panic");
            })
            .unwrap();
        sibling_started_rx.await.unwrap();
        secondary_started_rx.await.unwrap();

        panic_tx.send(()).unwrap();
        monitor.wait().await.unwrap();
        assert_eq!(monitor.status(), TaskSupervisorStatus::Panicked);
        let payload = monitor.take_payload().unwrap();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"primary task panic"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !sibling_dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(sibling_dropped.load(Ordering::Acquire));
        assert_eq!(
            supervisor.start(scope(2), async {}),
            Err(MountTaskSupervisorError::Panicked)
        );
        assert_eq!(
            supervisor.shutdown().await,
            Err(MountTaskSupervisorError::Panicked)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owner_drop_synchronously_aborts_the_actor_and_owned_tasks() {
        let supervisor = MountTaskSupervisor::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        supervisor
            .start(scope(1), async move {
                struct NotifyDrop(Option<oneshot::Sender<()>>);
                impl Drop for NotifyDrop {
                    fn drop(&mut self) {
                        if let Some(dropped) = self.0.take() {
                            let _ = dropped.send(());
                        }
                    }
                }
                let _guard = NotifyDrop(Some(dropped_tx));
                let _ = started_tx.send(());
                pending::<()>().await;
            })
            .unwrap();
        started_rx.await.unwrap();

        drop(supervisor);

        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("dropping the owner must cancel the task")
            .unwrap();
    }

    #[test]
    fn owning_runtime_shutdown_closes_monitor_retirement_and_later_starts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (supervisor, handle, monitor, retirement) = runtime.block_on(async {
            let supervisor = MountTaskSupervisor::new();
            let handle = supervisor.handle();
            let monitor = supervisor.panic_monitor();
            let (started, observed_start) = oneshot::channel();
            supervisor
                .start(scope(1), async move {
                    let _ = started.send(());
                    pending::<()>().await;
                })
                .unwrap();
            observed_start.await.unwrap();
            let retirement = supervisor.retire([scope(1)]).unwrap();
            assert!(matches!(
                *lock_unpoisoned(&retirement.state.status),
                RetirementStatus::Pending
            ));
            (supervisor, handle, monitor, retirement)
        });

        drop(runtime);

        let replacement_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        replacement_runtime.block_on(async {
            assert_eq!(monitor.status(), TaskSupervisorStatus::Closed);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), monitor.wait())
                    .await
                    .expect("the monitor must not hang after its owning runtime closes"),
                Err(MountTaskSupervisorError::Closed)
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), retirement.wait())
                    .await
                    .expect("retirement must not hang after its owning runtime closes"),
                Err(MountTaskSupervisorError::Closed)
            );
            assert_eq!(
                handle.start(scope(2), async {}),
                Err(MountTaskSupervisorError::Closed)
            );
            assert_eq!(
                supervisor.start(scope(2), async {}),
                Err(MountTaskSupervisorError::Closed)
            );
        });
    }

    #[test]
    fn idle_owning_runtime_is_fenced_before_foreign_runtime_operations() {
        let owning_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (supervisor, handle, monitor, monitor_wait, retirement_wait) =
            owning_runtime.block_on(async {
                let supervisor = MountTaskSupervisor::new();
                let handle = supervisor.handle();
                let monitor = supervisor.panic_monitor();
                let (started, observed_start) = oneshot::channel();
                supervisor
                    .start(scope(1), async move {
                        let _ = started.send(());
                        pending::<()>().await;
                    })
                    .unwrap();
                observed_start.await.unwrap();
                let retirement = supervisor.retire([scope(1)]).unwrap();
                assert!(matches!(
                    *lock_unpoisoned(&retirement.state.status),
                    RetirementStatus::Pending
                ));
                let mut monitor_wait = Box::pin({
                    let monitor = monitor.clone();
                    async move { monitor.wait().await }
                });
                let mut retirement_wait = Box::pin({
                    let retirement = retirement.clone();
                    async move { retirement.wait().await }
                });
                assert!(matches!(poll!(monitor_wait.as_mut()), Poll::Pending));
                assert!(matches!(poll!(retirement_wait.as_mut()), Poll::Pending));
                (supervisor, handle, monitor, monitor_wait, retirement_wait)
            });
        assert_eq!(monitor.status(), TaskSupervisorStatus::Healthy);

        let foreign_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        foreign_runtime.block_on(async {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), monitor_wait)
                    .await
                    .expect("a migrated monitor wait must fail instead of hanging"),
                Err(MountTaskSupervisorError::Closed)
            );
            assert_eq!(monitor.status(), TaskSupervisorStatus::Closed);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), retirement_wait)
                    .await
                    .expect("a migrated retirement wait must fail instead of hanging"),
                Err(MountTaskSupervisorError::Closed)
            );
            assert_eq!(
                handle.start(scope(2), async {}),
                Err(MountTaskSupervisorError::Closed)
            );
            assert!(matches!(
                supervisor.retire([scope(2)]),
                Err(MountTaskSupervisorError::Closed)
            ));
        });

        drop(foreign_runtime);
        drop(owning_runtime);
    }

    #[test]
    fn pending_shutdown_fails_closed_when_resumed_on_a_foreign_runtime() {
        let owning_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (monitor, retirement, shutdown) = owning_runtime.block_on(async {
            let mut supervisor = MountTaskSupervisor::new();
            let monitor = supervisor.panic_monitor();
            let (started, observed_start) = oneshot::channel();
            supervisor
                .start(scope(1), async move {
                    let _ = started.send(());
                    pending::<()>().await;
                })
                .unwrap();
            observed_start.await.unwrap();
            let retirement = supervisor.retire([scope(1)]).unwrap();
            let mut shutdown = Box::pin(async move { supervisor.shutdown().await });
            assert!(matches!(poll!(shutdown.as_mut()), Poll::Pending));
            (monitor, retirement, shutdown)
        });

        let foreign_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        foreign_runtime.block_on(async {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), shutdown)
                    .await
                    .expect("a migrated shutdown must fail instead of hanging"),
                Err(MountTaskSupervisorError::Closed)
            );
            assert_eq!(monitor.status(), TaskSupervisorStatus::Closed);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), retirement.wait())
                    .await
                    .expect("migrated shutdown must close pending retirements"),
                Err(MountTaskSupervisorError::Closed)
            );
        });

        drop(foreign_runtime);
        drop(owning_runtime);
    }
}
