use std::{fmt, future::Future, pin::Pin};

type RawHandlerFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
pub(crate) type HandlerFuture =
    Pin<Box<dyn Future<Output = Result<(), HandlerFault>> + Send + 'static>>;

pub(crate) struct AsyncHandler<Input> {
    invoke: Box<dyn FnMut(Input) -> RawHandlerFuture + Send + 'static>,
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
                let future = handler(input);
                Box::pin(async move { future.await.map_err(|error| error.to_string()) })
                    as RawHandlerFuture
            }),
        }
    }

    pub(crate) async fn invoke(&mut self, input: Input) -> Result<(), HandlerFault> {
        self.start(input)?.await
    }

    pub(crate) fn start(&mut self, input: Input) -> Result<HandlerFuture, HandlerFault> {
        let future = (self.invoke)(input);
        Ok(Box::pin(async move { await_handler_future(future).await }))
    }
}

async fn await_handler_future(future: RawHandlerFuture) -> Result<(), HandlerFault> {
    future
        .await
        .map_err(|message| HandlerFault::ReturnedError { message })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HandlerFault {
    #[error("callback returned an error: {message}")]
    ReturnedError { message: String },
}
