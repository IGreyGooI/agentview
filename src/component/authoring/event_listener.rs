#[cfg(any(feature = "legacy-provider-port", test))]
use std::marker::PhantomData;
use std::{any::Any, fmt, future::Future, pin::Pin, sync::Arc};

#[cfg(any(feature = "legacy-provider-port", test))]
use super::declaration::{Component, ComponentNode};
use super::{
    async_task::ComponentTaskContext,
    event_input::{
        EventInputOrigin, EventRouteDescriptor, EventRouteProjectionFault, EventRouteTopology,
        EventSelector,
    },
    handler::{AsyncHandler, HandlerFault},
    InternalEventInput as EventInput,
};
use crate::component::{execution::ProviderEvent, signal::HookMount};

type EventHandlerFuture =
    Pin<Box<dyn Future<Output = Result<(), EventListenerDispatchFault>> + Send + 'static>>;

/// Prompt-free generic multicast listener declaration.
#[cfg(any(feature = "legacy-provider-port", test))]
pub struct EventListener;

#[cfg(any(feature = "legacy-provider-port", test))]
impl EventListener {
    pub fn observe(
        identity: &'static str,
        implementation_version: &'static str,
    ) -> EventListenerBuilder {
        EventListenerBuilder {
            identity,
            implementation_version,
        }
    }
}

#[cfg(any(feature = "legacy-provider-port", test))]
pub struct EventListenerBuilder {
    identity: &'static str,
    implementation_version: &'static str,
}

#[cfg(any(feature = "legacy-provider-port", test))]
impl EventListenerBuilder {
    pub fn listen_to<E>(self, input: EventInput<E>) -> ListeningEvent<E>
    where
        E: Send + Sync + 'static,
    {
        ListeningEvent {
            identity: self.identity,
            implementation_version: self.implementation_version,
            route: input.route_descriptor(),
            marker: PhantomData,
        }
    }
}

#[cfg(any(feature = "legacy-provider-port", test))]
pub struct ListeningEvent<E> {
    identity: &'static str,
    implementation_version: &'static str,
    route: Arc<EventRouteDescriptor>,
    marker: PhantomData<fn() -> E>,
}

#[cfg(any(feature = "legacy-provider-port", test))]
impl<E> ListeningEvent<E>
where
    E: Clone + Send + Sync + 'static,
{
    pub fn on_event<Handler, HandlerFuture, Error>(self, handler: Handler) -> Component
    where
        Handler: FnMut(E) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Component::from_node(ComponentNode::EventListener(EventListenerDeclaration {
            identity: self.identity,
            implementation_version: self.implementation_version,
            route: self.route,
            handler: Box::new(TypedEventHandler {
                handler: AsyncHandler::new(handler),
            }),
            mount: None,
            task_context: None,
        }))
    }
}

pub(crate) struct EventListenerDeclaration {
    identity: &'static str,
    implementation_version: &'static str,
    route: Arc<EventRouteDescriptor>,
    handler: Box<dyn ErasedEventHandler>,
    mount: Option<HookMount>,
    task_context: Option<ComponentTaskContext>,
}

impl EventListenerDeclaration {
    pub(crate) fn provider_handler<Event, Handler, HandlerFuture, Error>(
        origin: EventInputOrigin,
        selector: EventSelector<ProviderEvent, Event>,
        handler: Handler,
        mount: HookMount,
        task_context: Option<ComponentTaskContext>,
    ) -> Self
    where
        Event: Clone + Send + Sync + 'static,
        Handler: FnMut(Event) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        let route = EventInput::<ProviderEvent>::from_origin(origin)
            .select(selector)
            .route_descriptor();
        Self {
            identity: "agentview.provider-event-handler",
            implementation_version: "v1",
            route,
            handler: Box::new(TypedEventHandler {
                handler: AsyncHandler::new(handler),
            }),
            mount: Some(mount),
            task_context,
        }
    }

    pub(crate) fn identity(&self) -> &'static str {
        self.identity
    }

    pub(crate) fn implementation_version(&self) -> &'static str {
        self.implementation_version
    }

    pub(crate) fn route(&self) -> &Arc<EventRouteDescriptor> {
        &self.route
    }

    pub(crate) fn topology(&self) -> EventRouteTopology {
        self.route.topology()
    }

    pub(crate) async fn dispatch_root<Root>(
        &mut self,
        root: &Root,
    ) -> Result<bool, EventListenerDispatchFault>
    where
        Root: Send + Sync + 'static,
    {
        let _mount_permit = match &self.mount {
            Some(mount) => Some(
                mount
                    .authorize()
                    .ok_or(EventListenerDispatchFault::StaleMount)?,
            ),
            None => None,
        };
        let task_context = self.task_context.clone();
        let started = match &task_context {
            Some(context) => context.scope_sync(|| self.start_root(root))?,
            None => self.start_root(root)?,
        };
        let Some(future) = started else {
            return Ok(false);
        };
        match task_context {
            Some(context) => context.scope(future).await?,
            None => future.await?,
        }
        Ok(true)
    }

    fn start_root<Root>(
        &mut self,
        root: &Root,
    ) -> Result<Option<EventHandlerFuture>, EventListenerDispatchFault>
    where
        Root: Send + Sync + 'static,
    {
        let Some(event) = self.route.project_root(root)? else {
            return Ok(None);
        };
        self.handler.start(event).map(Some)
    }
}

trait ErasedEventHandler: Send {
    fn start(
        &mut self,
        event: &(dyn Any + Send + Sync),
    ) -> Result<EventHandlerFuture, EventListenerDispatchFault>;
}

struct TypedEventHandler<E> {
    handler: AsyncHandler<E>,
}

impl<E> ErasedEventHandler for TypedEventHandler<E>
where
    E: Clone + Send + Sync + 'static,
{
    fn start(
        &mut self,
        event: &(dyn Any + Send + Sync),
    ) -> Result<EventHandlerFuture, EventListenerDispatchFault> {
        let event = event.downcast_ref::<E>().cloned().ok_or(
            EventListenerDispatchFault::HandlerTypeMismatch {
                expected: std::any::type_name::<E>(),
            },
        )?;
        let future = self.handler.start(event)?;
        Ok(Box::pin(async move {
            future.await.map_err(EventListenerDispatchFault::from)
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EventListenerDispatchFault {
    #[error("provider event handler belongs to a stale Component mount")]
    StaleMount,
    #[error(transparent)]
    Route(#[from] EventRouteProjectionFault),
    #[error("event listener handler expected `{expected}`")]
    HandlerTypeMismatch { expected: &'static str },
    #[error(transparent)]
    Handler(#[from] HandlerFault),
}
