use std::{fmt, future::Future};

use crate::component::{
    execution::{DriverDemandHandle, ProviderEvent},
    signal::{self as signal_kernel, HookKind},
    task::MountTaskHandle,
};

use super::{
    async_task::{ComponentTaskContext, MountTaskStart},
    event_input::{EventInputOrigin, EventSelector},
    event_listener::EventListenerDeclaration,
    preparation::{PreparationDeclaration, PreparationSet},
    Coroutine, CoroutineInbox, ReactionCompletionDeclaration, ReactionRequest, Signal,
};

/// Runtime hook authority passed only to repeatable Component renderers.
#[doc(hidden)]
pub struct HookRenderContext<'render> {
    signals: &'render mut signal_kernel::SignalRenderScope,
    event_origin: EventInputOrigin,
    provider_handlers: &'render mut Vec<EventListenerDeclaration>,
    reaction_completions: &'render mut Vec<ReactionCompletionDeclaration>,
    preparations: &'render mut PreparationSet,
    task_starts: &'render mut Vec<MountTaskStart>,
    driver_demand: Option<&'render DriverDemandHandle>,
    tasks: Option<&'render MountTaskHandle>,
    attempt_local_allowed: bool,
}

impl<'render> HookRenderContext<'render> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the render context borrows each generation-local capability separately"
    )]
    pub(crate) fn new(
        signals: &'render mut signal_kernel::SignalRenderScope,
        event_origin: EventInputOrigin,
        provider_handlers: &'render mut Vec<EventListenerDeclaration>,
        reaction_completions: &'render mut Vec<ReactionCompletionDeclaration>,
        preparations: &'render mut PreparationSet,
        task_starts: &'render mut Vec<MountTaskStart>,
        driver_demand: Option<&'render DriverDemandHandle>,
        tasks: Option<&'render MountTaskHandle>,
    ) -> Self {
        Self {
            signals,
            event_origin,
            provider_handlers,
            reaction_completions,
            preparations,
            task_starts,
            driver_demand,
            tasks,
            attempt_local_allowed: true,
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the render context borrows each generation-local capability separately"
    )]
    pub(crate) fn for_system(
        signals: &'render mut signal_kernel::SignalRenderScope,
        event_origin: EventInputOrigin,
        provider_handlers: &'render mut Vec<EventListenerDeclaration>,
        reaction_completions: &'render mut Vec<ReactionCompletionDeclaration>,
        preparations: &'render mut PreparationSet,
        task_starts: &'render mut Vec<MountTaskStart>,
        driver_demand: Option<&'render DriverDemandHandle>,
        tasks: Option<&'render MountTaskHandle>,
    ) -> Self {
        Self {
            signals,
            event_origin,
            provider_handlers,
            reaction_completions,
            preparations,
            task_starts,
            driver_demand,
            tasks,
            attempt_local_allowed: false,
        }
    }

    #[doc(hidden)]
    pub fn use_reaction_completion_at<Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        handler: Handler,
    ) where
        Handler: FnOnce() -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        if !self.attempt_local_allowed {
            panic!("System component declared generation-local reaction completion handler");
        }
        let mount = self
            .signals
            .use_marker_at(site, HookKind::ReactionCompletion)
            .unwrap_or_else(|fault| panic!("{fault}"));
        let task_context = self
            .tasks
            .cloned()
            .map(|tasks| ComponentTaskContext::new(mount.clone(), tasks));
        self.reaction_completions
            .push(ReactionCompletionDeclaration::new(
                handler,
                mount,
                task_context,
            ));
    }

    #[doc(hidden)]
    pub fn use_preparation_at<Loader, LoaderFuture, Error>(&mut self, site: u32, loader: Loader)
    where
        Loader: FnOnce() -> LoaderFuture + Send + 'static,
        LoaderFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        if !self.attempt_local_allowed {
            panic!("System component declared generation-local preparation");
        }
        let mount = self
            .signals
            .use_marker_at(site, HookKind::Preparation)
            .unwrap_or_else(|fault| panic!("{fault}"));
        self.preparations
            .push(PreparationDeclaration::new(loader, mount));
    }

    #[doc(hidden)]
    pub fn use_signal_at<T>(&mut self, site: u32, initialize: impl FnOnce() -> T) -> Signal<T>
    where
        T: Send + Sync + 'static,
    {
        if !self.attempt_local_allowed {
            panic!("System component declared generation-local signal");
        }
        self.signals
            .use_signal_at(site, initialize)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_provider_event_handler_at<Event, Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        selector: EventSelector<ProviderEvent, Event>,
        handler: Handler,
    ) where
        Event: Clone + Send + Sync + 'static,
        Handler: FnMut(Event) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        if !self.attempt_local_allowed {
            panic!("System component declared generation-local provider event handler");
        }
        let mount = self
            .signals
            .use_marker_at(site, HookKind::ProviderEventHandler)
            .unwrap_or_else(|fault| panic!("{fault}"));
        let task_context = self
            .tasks
            .cloned()
            .map(|tasks| ComponentTaskContext::new(mount.clone(), tasks));
        self.provider_handlers
            .push(EventListenerDeclaration::provider_handler(
                self.event_origin,
                selector,
                handler,
                mount,
                task_context,
            ));
    }

    #[doc(hidden)]
    pub fn use_reaction_request_at(&mut self, site: u32) -> ReactionRequest {
        if !self.attempt_local_allowed {
            panic!("System component declared generation-local reaction request");
        }
        let demand = self.driver_demand.cloned().unwrap_or_else(|| {
            panic!("Component requires unavailable reaction request capability")
        });
        let mount = self
            .signals
            .use_marker_at(site, HookKind::ReactionRequest)
            .unwrap_or_else(|fault| panic!("{fault}"));
        ReactionRequest::new(mount, demand)
    }

    #[doc(hidden)]
    pub fn use_future_at<Factory, TaskFuture>(&mut self, site: u32, factory: Factory)
    where
        Factory: FnOnce() -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.ensure_task_hook_allowed("future");
        let tasks = self
            .tasks
            .cloned()
            .expect("task hook capability was checked");
        let mount = self
            .signals
            .use_marker_at(site, HookKind::Future)
            .unwrap_or_else(|fault| panic!("{fault}"));
        if mount.is_new_mount() {
            self.task_starts.push(MountTaskStart::future(
                ComponentTaskContext::new(mount, tasks),
                factory,
            ));
        }
    }

    #[doc(hidden)]
    pub fn use_coroutine_at<Message, Factory, TaskFuture>(
        &mut self,
        site: u32,
        capacity: usize,
        service: Factory,
    ) -> Coroutine<Message>
    where
        Message: Send + 'static,
        Factory: FnOnce(CoroutineInbox<Message>) -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.ensure_task_hook_allowed("coroutine");
        assert!(
            capacity > 0,
            "Component coroutine capacity must be non-zero"
        );
        let tasks = self
            .tasks
            .cloned()
            .expect("task hook capability was checked");
        let (candidate_sender, receiver) = tokio::sync::mpsc::channel(capacity);
        let (mount, sender) = self
            .signals
            .use_retained_at(site, HookKind::Coroutine, || candidate_sender)
            .unwrap_or_else(|fault| panic!("{fault}"));
        if mount.is_new_mount() {
            self.task_starts.push(MountTaskStart::coroutine(
                ComponentTaskContext::new(mount.clone(), tasks),
                receiver,
                service,
            ));
        }
        Coroutine::new(mount, sender.as_ref().clone())
    }

    fn ensure_task_hook_allowed(&self, capability: &'static str) {
        if !self.attempt_local_allowed {
            panic!("System component declared generation-local {capability}");
        }
        if self.tasks.is_none() {
            panic!("Component requires unavailable {capability} capability");
        }
    }
}
