use std::{fmt, future::Future};

use crate::component::execution::ProviderEvent;

use super::event_input::EventSelector;

/// Typed selector accepted by [`use_provider_event_handler`].
pub type ProviderEventSelector<Event> = EventSelector<ProviderEvent, Event>;

/// Declare an ordered Provider event handler in the owning Component.
///
/// The handler is outside `view!`: the complete projection remains a pure
/// description of Provider-visible state. A direct call inside a
/// `#[component]` function is rewritten to the mounted hook context.
#[track_caller]
pub fn use_provider_event_handler<Event, Handler, HandlerFuture, Error>(
    _selector: ProviderEventSelector<Event>,
    _handler: Handler,
) where
    Event: Clone + Send + Sync + 'static,
    Handler: FnMut(Event) -> HandlerFuture + Send + 'static,
    HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: fmt::Display + Send + 'static,
{
    panic!("use_provider_event_handler must be called directly inside a #[component] function")
}
