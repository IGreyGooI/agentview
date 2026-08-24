use std::{any::Any, fmt, future::Future, marker::PhantomData, pin::Pin, sync::Arc};

use super::{
    declaration::{Component, ComponentNode},
    event_input::{EventRouteDescriptor, EventRouteProjectionFault, EventRouteTopology},
    handler::{AsyncHandler, HandlerFault},
    EventInput,
};

/// Prompt-free generic multicast listener declaration.
pub struct EventListener;

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

pub struct EventListenerBuilder {
    identity: &'static str,
    implementation_version: &'static str,
}

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

pub struct ListeningEvent<E> {
    identity: &'static str,
    implementation_version: &'static str,
    route: Arc<EventRouteDescriptor>,
    marker: PhantomData<fn() -> E>,
}

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
        }))
    }
}

pub(crate) struct EventListenerDeclaration {
    identity: &'static str,
    implementation_version: &'static str,
    route: Arc<EventRouteDescriptor>,
    handler: Box<dyn ErasedEventHandler>,
}

impl EventListenerDeclaration {
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
        let Some(event) = self.route.project_root(root)? else {
            return Ok(false);
        };
        self.handler.invoke(event).await?;
        Ok(true)
    }
}

trait ErasedEventHandler: Send {
    fn invoke<'handler>(
        &'handler mut self,
        event: &(dyn Any + Send + Sync),
    ) -> Pin<Box<dyn Future<Output = Result<(), EventListenerDispatchFault>> + Send + 'handler>>;
}

struct TypedEventHandler<E> {
    handler: AsyncHandler<E>,
}

impl<E> ErasedEventHandler for TypedEventHandler<E>
where
    E: Clone + Send + Sync + 'static,
{
    fn invoke<'handler>(
        &'handler mut self,
        event: &(dyn Any + Send + Sync),
    ) -> Pin<Box<dyn Future<Output = Result<(), EventListenerDispatchFault>> + Send + 'handler>>
    {
        let event = event.downcast_ref::<E>().cloned();
        Box::pin(async move {
            let event = event.ok_or(EventListenerDispatchFault::HandlerTypeMismatch {
                expected: std::any::type_name::<E>(),
            })?;
            self.handler.invoke(event).await?;
            Ok(())
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EventListenerDispatchFault {
    #[error(transparent)]
    Route(#[from] EventRouteProjectionFault),
    #[error("event listener handler expected `{expected}`")]
    HandlerTypeMismatch { expected: &'static str },
    #[error(transparent)]
    Handler(#[from] HandlerFault),
}
