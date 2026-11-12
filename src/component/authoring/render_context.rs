use std::{fmt, future::Future};

use crate::component::{
    execution::{command::CommandFeedback, DriverDemandHandle, ProviderEvent},
    signal::{self as signal_kernel, HookKind},
    task::MountTaskHandle,
    ApplicationId, EventContext,
};

use super::{
    application_exit::ApplicationExitControl,
    async_task::{ComponentTaskContext, MountTaskStart},
    declaration::Placement,
    definition::ComponentDefinitionError,
    event_input::{EventInputOrigin, EventSelector},
    event_listener::EventListenerDeclaration,
    preparation::{CommandWaitDeclaration, PreparationDeclaration, PreparationSet},
    ApplicationExitHandle, Coroutine, CoroutineInbox, ReactionCompletionDeclaration,
    ReactionRequest, Signal,
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
    application_exit: Option<&'render ApplicationExitControl>,
    attempt_local_allowed: bool,
    definition_fault: Option<ComponentDefinitionError>,
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
        application_exit: Option<&'render ApplicationExitControl>,
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
            application_exit,
            attempt_local_allowed: true,
            definition_fault: None,
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
        application_exit: Option<&'render ApplicationExitControl>,
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
            application_exit,
            attempt_local_allowed: false,
            definition_fault: None,
        }
    }

    #[doc(hidden)]
    pub fn use_reaction_completion_at<Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        handler: Handler,
    ) where
        Handler: FnOnce(EventContext) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.try_use_reaction_completion_at(site, handler)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_preparation_at<Loader, LoaderFuture, Error>(&mut self, site: u32, loader: Loader)
    where
        Loader: FnOnce() -> LoaderFuture + Send + 'static,
        LoaderFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.try_use_preparation_at(site, loader)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_wait_for_command_at(&mut self, site: u32) {
        self.try_use_wait_for_command_at(site)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_signal_at<T>(&mut self, site: u32, initialize: impl FnOnce() -> T) -> Signal<T>
    where
        T: Send + Sync + 'static,
    {
        self.try_use_signal_at(site, initialize)
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
        Handler: FnMut(EventContext, Event) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.try_use_provider_event_handler_at(site, selector, handler)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_application_id_at(&mut self, site: u32) -> ApplicationId {
        self.try_use_application_id_at(site)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_reaction_request_at(&mut self, site: u32) -> ReactionRequest {
        self.try_use_reaction_request_at(site)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_application_exit_at(&mut self, site: u32) -> ApplicationExitHandle {
        self.try_use_application_exit_at(site)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    #[doc(hidden)]
    pub fn use_future_at<Factory, TaskFuture>(&mut self, site: u32, factory: Factory)
    where
        Factory: FnOnce() -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.try_use_future_at(site, factory)
            .unwrap_or_else(|fault| panic!("{fault}"))
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
        self.try_use_coroutine_at(site, capacity, service)
            .unwrap_or_else(|fault| panic!("{fault}"))
    }

    /// Dynamic definitions report invalid hook declarations as ordinary update
    /// failures. Keep these failures sticky even if an external renderer ignores
    /// an individual hook's returned error.
    fn definition_hook<T>(
        &mut self,
        declare: impl FnOnce(&mut Self) -> Result<T, ComponentDefinitionError>,
    ) -> Result<T, ComponentDefinitionError> {
        if let Some(fault) = &self.definition_fault {
            return Err(fault.clone());
        }
        let result = declare(self);
        if let Err(fault) = &result {
            self.definition_fault = Some(fault.clone());
        }
        result
    }

    fn definition_hook_allowed(
        &self,
        capability: &'static str,
    ) -> Result<(), ComponentDefinitionError> {
        if self.attempt_local_allowed {
            Ok(())
        } else {
            Err(ComponentDefinitionError::new(format!(
                "System component declared generation-local {capability}"
            )))
        }
    }

    pub(crate) fn definition_result(&self) -> Result<(), ComponentDefinitionError> {
        match &self.definition_fault {
            Some(fault) => Err(fault.clone()),
            None => Ok(()),
        }
    }

    pub(crate) fn try_use_signal_at<T>(
        &mut self,
        site: u32,
        initialize: impl FnOnce() -> T,
    ) -> Result<Signal<T>, ComponentDefinitionError>
    where
        T: Send + Sync + 'static,
    {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("signal")?;
            context
                .signals
                .use_signal_at(site, initialize)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))
        })
    }

    pub(crate) fn try_use_preparation_at<Loader, LoaderFuture, Error>(
        &mut self,
        site: u32,
        loader: Loader,
    ) -> Result<(), ComponentDefinitionError>
    where
        Loader: FnOnce() -> LoaderFuture + Send + 'static,
        LoaderFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("preparation")?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::Preparation)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            context
                .preparations
                .push(PreparationDeclaration::new(loader, mount));
            Ok(())
        })
    }

    pub(crate) fn try_use_wait_for_command_at(
        &mut self,
        site: u32,
    ) -> Result<(), ComponentDefinitionError> {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("command wait")?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::CommandWait)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            let feedback = context
                .signals
                .use_signal_at(site, CommandFeedback::default)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            context
                .preparations
                .push_command_wait(CommandWaitDeclaration {
                    mount,
                    feedback,
                    placement: Placement::User,
                });
            Ok(())
        })
    }

    pub(crate) fn try_use_provider_event_handler_at<Event, Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        selector: EventSelector<ProviderEvent, Event>,
        handler: Handler,
    ) -> Result<(), ComponentDefinitionError>
    where
        Event: Clone + Send + Sync + 'static,
        Handler: FnMut(EventContext, Event) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("provider event handler")?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::ProviderEventHandler)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            let task_context = context
                .tasks
                .cloned()
                .map(|tasks| ComponentTaskContext::new(mount.clone(), tasks));
            context
                .provider_handlers
                .push(EventListenerDeclaration::provider_handler(
                    context.event_origin,
                    selector,
                    handler,
                    mount,
                    task_context,
                ));
            Ok(())
        })
    }

    pub(crate) fn try_use_reaction_completion_at<Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        handler: Handler,
    ) -> Result<(), ComponentDefinitionError>
    where
        Handler: FnOnce(EventContext) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("reaction completion handler")?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::ReactionCompletion)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            let task_context = context
                .tasks
                .cloned()
                .map(|tasks| ComponentTaskContext::new(mount.clone(), tasks));
            context
                .reaction_completions
                .push(ReactionCompletionDeclaration::new(
                    handler,
                    mount,
                    task_context,
                ));
            Ok(())
        })
    }

    pub(crate) fn try_use_application_id_at(
        &mut self,
        site: u32,
    ) -> Result<ApplicationId, ComponentDefinitionError> {
        self.definition_hook(move |context| {
            context
                .signals
                .use_marker_at(site, HookKind::ApplicationId)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            Ok(context.event_origin.application_id())
        })
    }

    pub(crate) fn try_use_reaction_request_at(
        &mut self,
        site: u32,
    ) -> Result<ReactionRequest, ComponentDefinitionError> {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("reaction request")?;
            let demand = context.driver_demand.cloned().ok_or_else(|| {
                ComponentDefinitionError::new(
                    "Component requires unavailable reaction request capability",
                )
            })?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::ReactionRequest)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            Ok(ReactionRequest::new(mount, demand))
        })
    }

    pub(crate) fn try_use_application_exit_at(
        &mut self,
        site: u32,
    ) -> Result<ApplicationExitHandle, ComponentDefinitionError> {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("application exit")?;
            let exit = context.application_exit.cloned().ok_or_else(|| {
                ComponentDefinitionError::new(
                    "Component requires unavailable application exit capability",
                )
            })?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::ApplicationExit)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            Ok(ApplicationExitHandle::for_mount(mount, exit))
        })
    }

    pub(crate) fn try_use_future_at<Factory, TaskFuture>(
        &mut self,
        site: u32,
        factory: Factory,
    ) -> Result<(), ComponentDefinitionError>
    where
        Factory: FnOnce() -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("future")?;
            let tasks = context.tasks.cloned().ok_or_else(|| {
                ComponentDefinitionError::new("Component requires unavailable future capability")
            })?;
            let mount = context
                .signals
                .use_marker_at(site, HookKind::Future)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            if mount.is_new_mount() {
                context.task_starts.push(MountTaskStart::future(
                    ComponentTaskContext::new(mount, tasks),
                    factory,
                ));
            }
            Ok(())
        })
    }

    pub(crate) fn try_use_coroutine_at<Message, Factory, TaskFuture>(
        &mut self,
        site: u32,
        capacity: usize,
        service: Factory,
    ) -> Result<Coroutine<Message>, ComponentDefinitionError>
    where
        Message: Send + 'static,
        Factory: FnOnce(CoroutineInbox<Message>) -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.definition_hook(move |context| {
            context.definition_hook_allowed("coroutine")?;
            let tasks = context.tasks.cloned().ok_or_else(|| {
                ComponentDefinitionError::new("Component requires unavailable coroutine capability")
            })?;
            if capacity == 0 {
                return Err(ComponentDefinitionError::new(
                    "Component coroutine capacity must be non-zero",
                ));
            }
            let (candidate_sender, receiver) = tokio::sync::mpsc::channel(capacity);
            let (mount, sender) = context
                .signals
                .use_retained_at(site, HookKind::Coroutine, || candidate_sender)
                .map_err(|fault| ComponentDefinitionError::new(fault.to_string()))?;
            if mount.is_new_mount() {
                context.task_starts.push(MountTaskStart::coroutine(
                    ComponentTaskContext::new(mount.clone(), tasks),
                    receiver,
                    service,
                ));
            }
            Ok(Coroutine::new(mount, sender.as_ref().clone()))
        })
    }
}
