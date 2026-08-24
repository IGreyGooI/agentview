use std::{
    any::Any,
    fmt,
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
};

use futures::FutureExt;

type HandlerFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;

pub(crate) struct AsyncHandler<Input> {
    invoke: Box<dyn FnMut(Input) -> Result<HandlerFuture, HandlerFault> + Send + 'static>,
}

impl<Input> AsyncHandler<Input>
where
    Input: Send + 'static,
{
    pub(crate) fn new<Handler, HandlerFutureType, Error>(mut handler: Handler) -> Self
    where
        Handler: FnMut(Input) -> HandlerFutureType + Send + 'static,
        HandlerFutureType: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Self {
            invoke: Box::new(move |input| {
                catch_unwind(AssertUnwindSafe(|| handler(input)))
                    .map(|future| {
                        Box::pin(async move { future.await.map_err(|error| error.to_string()) })
                            as HandlerFuture
                    })
                    .map_err(|panic| HandlerFault::InvocationPanicked {
                        message: panic_message(&*panic),
                    })
            }),
        }
    }

    pub(crate) async fn invoke(&mut self, input: Input) -> Result<(), HandlerFault> {
        let future = (self.invoke)(input)?;
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(HandlerFault::ReturnedError { message }),
            Err(panic) => Err(HandlerFault::FuturePanicked {
                message: panic_message(&*panic),
            }),
        }
    }
}

pub(crate) struct AsyncOnceHandler {
    invoke: Option<Box<dyn FnOnce() -> Result<HandlerFuture, HandlerFault> + Send + 'static>>,
}

impl AsyncOnceHandler {
    pub(crate) fn new<Handler, HandlerFutureType, Error>(handler: Handler) -> Self
    where
        Handler: FnOnce() -> HandlerFutureType + Send + 'static,
        HandlerFutureType: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Self {
            invoke: Some(Box::new(move || {
                catch_unwind(AssertUnwindSafe(handler))
                    .map(|future| {
                        Box::pin(async move { future.await.map_err(|error| error.to_string()) })
                            as HandlerFuture
                    })
                    .map_err(|panic| HandlerFault::InvocationPanicked {
                        message: panic_message(&*panic),
                    })
            })),
        }
    }

    pub(crate) async fn invoke(mut self) -> Result<(), HandlerFault> {
        let invoke = self.invoke.take().ok_or(HandlerFault::AlreadyInvoked)?;
        let future = invoke()?;
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(HandlerFault::ReturnedError { message }),
            Err(panic) => Err(HandlerFault::FuturePanicked {
                message: panic_message(&*panic),
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HandlerFault {
    #[error("callback invocation panicked: {message}")]
    InvocationPanicked { message: String },
    #[error("callback future panicked: {message}")]
    FuturePanicked { message: String },
    #[error("callback returned an error: {message}")]
    ReturnedError { message: String },
    #[error("one-shot callback was already invoked")]
    AlreadyInvoked,
}

pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("non-string panic payload"))
}
