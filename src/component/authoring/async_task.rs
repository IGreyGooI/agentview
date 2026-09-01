//! Component-scoped asynchronous task authoring primitives.

use std::{
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use tokio::sync::mpsc;

use crate::component::{
    signal::HookMount,
    task::{MountTaskHandle, MountTaskScope, MountTaskSupervisorError},
};

type BoxMountTaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

tokio::task_local! {
    static COMPONENT_TASK_CONTEXT: ComponentTaskContext;
}

/// Runtime capability inherited by every task owned by one Component mount.
///
/// Handler dispatch enters this context explicitly. Tasks registered through
/// it inherit the same context, allowing nested [`spawn`] calls without
/// exposing mount identity or the task supervisor to Component code.
#[derive(Clone)]
pub(crate) struct ComponentTaskContext {
    mount: HookMount,
    tasks: MountTaskHandle,
}

impl ComponentTaskContext {
    pub(crate) fn new(mount: HookMount, tasks: MountTaskHandle) -> Self {
        Self { mount, tasks }
    }

    pub(crate) fn mount_scope(&self) -> MountTaskScope {
        MountTaskScope::new(self.mount.component.clone(), self.mount.generation)
    }

    /// Run a committed callback with this mount's task capability installed.
    pub(crate) fn scope<F>(&self, future: F) -> impl Future<Output = F::Output> + Send + 'static
    where
        F: Future + Send + 'static,
    {
        COMPONENT_TASK_CONTEXT.scope(self.clone(), future)
    }

    pub(crate) fn scope_sync<R>(&self, operation: impl FnOnce() -> R) -> R {
        COMPONENT_TASK_CONTEXT.sync_scope(self.clone(), operation)
    }

    /// Register one anonymous mount-scoped task.
    pub(crate) fn register<F>(&self, future: F) -> Result<(), SpawnError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let scope = self.mount_scope();
        let scoped = self.scope(future);
        self.mount
            .with_active(|| self.tasks.start(scope, scoped))
            .ok_or(SpawnError::StaleMount)?
            .map_err(SpawnError::from)
    }

    /// Start one bounded, mount-scoped coroutine service.
    #[cfg(test)]
    pub(crate) fn start_coroutine<Message, Factory, TaskFuture>(
        &self,
        capacity: usize,
        service: Factory,
    ) -> Result<Coroutine<Message>, SpawnError>
    where
        Message: Send + 'static,
        Factory: FnOnce(CoroutineInbox<Message>) -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        assert!(
            capacity > 0,
            "Component coroutine capacity must be non-zero"
        );
        let (sender, receiver) = mpsc::channel(capacity);
        let coroutine = Coroutine {
            mount: self.mount.clone(),
            sender,
        };
        self.register(async move {
            service(CoroutineInbox { receiver }).await;
        })?;
        Ok(coroutine)
    }
}

/// One deferred start emitted by a successful Component render candidate.
pub(crate) struct MountTaskStart {
    context: ComponentTaskContext,
    future: BoxMountTaskFuture,
}

impl MountTaskStart {
    pub(crate) fn future<Factory, TaskFuture>(
        context: ComponentTaskContext,
        factory: Factory,
    ) -> Self
    where
        Factory: FnOnce() -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        Self {
            context,
            future: Box::pin(async move { factory().await }),
        }
    }

    pub(crate) fn coroutine<Message, Factory, TaskFuture>(
        context: ComponentTaskContext,
        receiver: mpsc::Receiver<Message>,
        service: Factory,
    ) -> Self
    where
        Message: Send + 'static,
        Factory: FnOnce(CoroutineInbox<Message>) -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        Self {
            context,
            future: Box::pin(async move { service(CoroutineInbox { receiver }).await }),
        }
    }

    pub(crate) fn start(self) -> Result<(), SpawnError> {
        self.context.register(self.future)
    }
}

/// Failure to register a Component-scoped task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    #[error("spawn requires a committed Component task context")]
    ContextUnavailable,
    #[error("Component tasks require an active Tokio runtime")]
    RuntimeUnavailable,
    #[error("the task belongs to a stale Component mount")]
    StaleMount,
    #[error("the Component task runtime is closed")]
    RuntimeClosed,
    #[error("the Component task runtime has observed a task panic")]
    RuntimePanicked,
}

impl From<MountTaskSupervisorError> for SpawnError {
    fn from(error: MountTaskSupervisorError) -> Self {
        match error {
            MountTaskSupervisorError::RuntimeUnavailable => Self::RuntimeUnavailable,
            MountTaskSupervisorError::Closed => Self::RuntimeClosed,
            MountTaskSupervisorError::Panicked => Self::RuntimePanicked,
            MountTaskSupervisorError::StaleMount => Self::StaleMount,
        }
    }
}

/// Start a one-shot task owned by the current Component mount.
///
/// This function is valid in a committed provider-event handler, coroutine, or
/// another Component-owned task. It does not run during Component rendering.
pub fn spawn<F>(future: F) -> Result<(), SpawnError>
where
    F: Future<Output = ()> + Send + 'static,
{
    COMPONENT_TASK_CONTEXT
        .try_with(|context| context.register(future))
        .map_err(|_| SpawnError::ContextUnavailable)?
}

/// Declare one future that starts after its Component mount commits.
///
/// This hook must be called directly inside a `#[component]` function. Macro
/// lowering binds it to a lexical hook slot; calling it through an unrecognized
/// helper fails closed.
#[track_caller]
pub fn use_future<Factory, TaskFuture>(_factory: Factory)
where
    Factory: FnOnce() -> TaskFuture + Send + 'static,
    TaskFuture: Future<Output = ()> + Send + 'static,
{
    panic!("use_future must be called directly inside a #[component] function")
}

/// Sender for one bounded Component-owned coroutine inbox.
pub struct Coroutine<Message> {
    mount: HookMount,
    sender: mpsc::Sender<Message>,
}

impl<Message> Clone for Coroutine<Message> {
    fn clone(&self) -> Self {
        Self {
            mount: self.mount.clone(),
            sender: self.sender.clone(),
        }
    }
}

impl<Message> fmt::Debug for Coroutine<Message> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Coroutine")
            .field("closed", &self.sender.is_closed())
            .field("capacity", &self.sender.capacity())
            .finish_non_exhaustive()
    }
}

impl<Message> Coroutine<Message> {
    pub(crate) fn new(mount: HookMount, sender: mpsc::Sender<Message>) -> Self {
        Self { mount, sender }
    }

    /// Enqueue one message without crossing the owning mount's retirement.
    ///
    /// Capacity is reserved asynchronously. The reserved slot is committed
    /// synchronously under the mount fence, so either this send wins before
    /// retirement or it returns the original message as stale.
    pub async fn send(&self, message: Message) -> Result<(), CoroutineSendError<Message>> {
        if self.mount.with_active(|| ()).is_none() {
            return Err(CoroutineSendError::StaleMount(message));
        }

        let permit = match self.sender.clone().reserve_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return Err(if self.mount.with_active(|| ()).is_some() {
                    CoroutineSendError::Closed(message)
                } else {
                    CoroutineSendError::StaleMount(message)
                });
            }
        };

        let mut message = Some(message);
        let committed = self.mount.with_active(|| {
            permit.send(
                message
                    .take()
                    .expect("a coroutine message is committed at most once"),
            );
        });
        match committed {
            Some(()) => Ok(()),
            None => Err(CoroutineSendError::StaleMount(
                message.expect("a stale coroutine send retains its message"),
            )),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }
}

/// A message rejected by a bounded Component coroutine.
#[non_exhaustive]
pub enum CoroutineSendError<Message> {
    StaleMount(Message),
    Closed(Message),
}

impl<Message> CoroutineSendError<Message> {
    pub fn into_inner(self) -> Message {
        match self {
            Self::StaleMount(message) | Self::Closed(message) => message,
        }
    }

    pub const fn is_stale_mount(&self) -> bool {
        matches!(self, Self::StaleMount(_))
    }

    pub const fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

impl<Message> fmt::Debug for CoroutineSendError<Message> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleMount(_) => formatter.write_str("CoroutineSendError::StaleMount(..)"),
            Self::Closed(_) => formatter.write_str("CoroutineSendError::Closed(..)"),
        }
    }
}

impl<Message> fmt::Display for CoroutineSendError<Message> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleMount(_) => {
                formatter.write_str("the coroutine belongs to a stale Component mount")
            }
            Self::Closed(_) => formatter.write_str("the Component coroutine inbox is closed"),
        }
    }
}

impl<Message> Error for CoroutineSendError<Message> {}

/// Bounded FIFO inbox owned by a Component coroutine service.
pub struct CoroutineInbox<Message> {
    receiver: mpsc::Receiver<Message>,
}

impl<Message> CoroutineInbox<Message> {
    pub async fn recv(&mut self) -> Option<Message> {
        self.receiver.recv().await
    }

    pub fn close(&mut self) {
        self.receiver.close();
    }

    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }
}

impl<Message> Stream for CoroutineInbox<Message> {
    type Item = Message;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

/// Declare one bounded coroutine owned by the current Component mount.
///
/// The service starts after mount commit and is aborted when that mount is
/// retired. Calling this function outside direct `#[component]` macro lowering
/// fails closed.
#[track_caller]
pub fn use_coroutine<Message, Factory, TaskFuture>(
    _capacity: usize,
    _service: Factory,
) -> Coroutine<Message>
where
    Message: Send + 'static,
    Factory: FnOnce(CoroutineInbox<Message>) -> TaskFuture + Send + 'static,
    TaskFuture: Future<Output = ()> + Send + 'static,
{
    panic!("use_coroutine must be called directly inside a #[component] function")
}

#[cfg(test)]
mod tests {
    use std::{future::pending, time::Duration};

    use tokio::sync::oneshot;

    use super::*;
    use crate::component::{
        signal::{HookKind, SignalRuntime},
        task::MountTaskSupervisor,
        ComponentId,
    };

    fn active_mount(signals: &SignalRuntime) -> HookMount {
        let mut render = signals.begin_render().unwrap();
        let mount = render
            .render_component(ComponentId::root(), |scope| {
                scope.use_marker_at(0, HookKind::ReactionRequest)
            })
            .unwrap();
        render.commit();
        mount
    }

    #[test]
    fn spawn_without_a_committed_task_context_fails_closed() {
        assert_eq!(spawn(async {}), Err(SpawnError::ContextUnavailable));
    }

    #[tokio::test]
    async fn registered_tasks_inherit_context_for_nested_spawn() {
        let signals = SignalRuntime::new();
        let mount = active_mount(&signals);
        let mut supervisor = MountTaskSupervisor::new();
        let context = ComponentTaskContext::new(mount, supervisor.handle());
        let (outer_tx, outer_rx) = oneshot::channel();
        let (nested_tx, nested_rx) = oneshot::channel();

        context
            .register(async move {
                spawn(async move {
                    let _ = nested_tx.send(());
                })
                .unwrap();
                let _ = outer_tx.send(());
            })
            .unwrap();

        outer_rx.await.unwrap();
        nested_rx.await.unwrap();
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retired_mount_context_stays_stale_after_task_cleanup_completes() {
        let signals = SignalRuntime::new();
        let mount = active_mount(&signals);
        let scope = MountTaskScope::new(mount.component.clone(), mount.generation);
        let mut supervisor = MountTaskSupervisor::new();
        let context = ComponentTaskContext::new(mount, supervisor.handle());
        context.register(pending()).unwrap();

        signals.invalidate_all().unwrap();
        supervisor.retire([scope]).unwrap().wait().await.unwrap();

        assert_eq!(context.register(async {}), Err(SpawnError::StaleMount));
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn coroutine_is_bounded_fifo_and_stale_send_retains_the_message() {
        let signals = SignalRuntime::new();
        let mount = active_mount(&signals);
        let scope = MountTaskScope::new(mount.component.clone(), mount.generation);
        let mut supervisor = MountTaskSupervisor::new();
        let context = ComponentTaskContext::new(mount, supervisor.handle());
        let (release_tx, release_rx) = oneshot::channel();
        let (received_tx, received_rx) = oneshot::channel();
        let coroutine = context
            .start_coroutine(1, move |mut inbox| async move {
                let _ = release_rx.await;
                let first = inbox.recv().await.unwrap();
                let second = inbox.recv().await.unwrap();
                let _ = received_tx.send([first, second]);
                pending::<()>().await;
            })
            .unwrap();

        coroutine.send(1).await.unwrap();
        let second_sender = coroutine.clone();
        let mut second = tokio::spawn(async move { second_sender.send(2).await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err());
        release_tx.send(()).unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(received_rx.await.unwrap(), [1, 2]);

        signals.invalidate_all().unwrap();
        let stale = coroutine.send(3).await.unwrap_err();
        assert!(stale.is_stale_mount());
        assert_eq!(stale.into_inner(), 3);
        supervisor.retire([scope]).unwrap().wait().await.unwrap();
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn completed_coroutine_reports_closed_and_retains_the_message() {
        let signals = SignalRuntime::new();
        let mount = active_mount(&signals);
        let mut supervisor = MountTaskSupervisor::new();
        let context = ComponentTaskContext::new(mount, supervisor.handle());
        let (completed_tx, completed_rx) = oneshot::channel();
        let coroutine = context
            .start_coroutine(1, move |_inbox: CoroutineInbox<u32>| async move {
                let _ = completed_tx.send(());
            })
            .unwrap();
        completed_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !coroutine.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let closed = coroutine.send(7).await.unwrap_err();
        assert!(closed.is_closed());
        assert_eq!(closed.into_inner(), 7);
        supervisor.shutdown().await.unwrap();
    }
}
