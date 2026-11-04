use std::{fmt, future::Future, pin::Pin};

use crate::component::{signal::HookMount, EventContext};

use super::{async_task::ComponentTaskContext, handler::HandlerFault};

type CompletionFuture = Pin<Box<dyn Future<Output = Result<(), HandlerFault>> + Send + 'static>>;
type CompletionHandler = Box<dyn FnOnce(EventContext) -> CompletionFuture + Send + 'static>;

/// Declare work that runs after one normally completed provider reaction.
///
/// The callback runs after provider events, derived streaming events, and XML
/// diagnostics discovered at normal EOF. This hook must be called directly
/// inside a `#[component]` function so it can be bound to a lexical hook slot.
#[track_caller]
pub fn use_reaction_completion<Handler, HandlerFuture, Error>(_handler: Handler)
where
    Handler: FnOnce(EventContext) -> HandlerFuture + Send + 'static,
    HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: fmt::Display + Send + 'static,
{
    panic!("use_reaction_completion must be called directly inside a #[component] function")
}

pub(crate) struct ReactionCompletionDeclaration {
    handler: Option<CompletionHandler>,
    mount: HookMount,
    task_context: Option<ComponentTaskContext>,
}

impl ReactionCompletionDeclaration {
    pub(crate) fn new<Handler, HandlerFuture, Error>(
        handler: Handler,
        mount: HookMount,
        task_context: Option<ComponentTaskContext>,
    ) -> Self
    where
        Handler: FnOnce(EventContext) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Self {
            handler: Some(Box::new(move |context| {
                let future = handler(context);
                Box::pin(async move {
                    future.await.map_err(|error| HandlerFault::ReturnedError {
                        message: error.to_string(),
                    })
                })
            })),
            mount,
            task_context,
        }
    }

    pub(crate) async fn dispatch(
        &mut self,
        context: EventContext,
    ) -> Result<(), ReactionCompletionDispatchFault> {
        let _mount_permit = self
            .mount
            .authorize()
            .ok_or(ReactionCompletionDispatchFault::StaleMount)?;
        let task_context = self.task_context.clone();
        let future = match &task_context {
            Some(task_context) => task_context.scope_sync(|| self.start(context))?,
            None => self.start(context)?,
        };
        match task_context {
            Some(context) => context.scope(future).await?,
            None => future.await?,
        }
        Ok(())
    }

    fn start(
        &mut self,
        context: EventContext,
    ) -> Result<CompletionFuture, ReactionCompletionDispatchFault> {
        let handler = self
            .handler
            .take()
            .ok_or(ReactionCompletionDispatchFault::AlreadyDispatched)?;
        Ok(handler(context))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReactionCompletionDispatchFault {
    #[error("reaction completion handler belongs to a stale Component mount")]
    StaleMount,
    #[error("reaction completion handler was already dispatched")]
    AlreadyDispatched,
    #[error(transparent)]
    Handler(#[from] HandlerFault),
}
