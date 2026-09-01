use std::{fmt, future::Future, pin::Pin};

use crate::component::execution::{ToolCall, ToolOutput};

use super::declaration::{Component, ComponentNode};

type ToolFuture = Pin<Box<dyn Future<Output = Result<ToolOutput, String>> + Send + 'static>>;

/// Declares one native provider tool and its reaction-local handler.
pub struct NativeToolCall;

impl NativeToolCall {
    pub fn named(name: &'static str) -> NativeToolCallNamed {
        NativeToolCallNamed { name }
    }
}

pub struct NativeToolCallNamed {
    name: &'static str,
}

impl NativeToolCallNamed {
    pub fn on_call<Handler, HandlerFuture, Error>(self, mut handler: Handler) -> Component
    where
        Handler: FnMut(ToolCall) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<ToolOutput, Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Component::from_node(ComponentNode::NativeToolCall(Box::new(
            NativeToolCallDeclaration {
                name: self.name,
                invoke: Box::new(move |call| {
                    let future = handler(call);
                    Box::pin(async move { future.await.map_err(|error| error.to_string()) })
                        as ToolFuture
                }),
            },
        )))
    }
}

pub(crate) struct NativeToolCallDeclaration {
    name: &'static str,
    invoke: Box<dyn FnMut(ToolCall) -> ToolFuture + Send + 'static>,
}

impl NativeToolCallDeclaration {
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn start(&mut self, call: ToolCall) -> Result<ToolFuture, NativeToolDispatchFault> {
        let call_id = call.call_id().to_owned();
        if call.name() != self.name {
            return Err(NativeToolDispatchFault::WrongTool {
                call_id,
                name: call.name().to_owned(),
            });
        }
        Ok((self.invoke)(call))
    }
}

pub(crate) async fn await_output(
    call_id: String,
    future: ToolFuture,
) -> Result<ToolOutput, NativeToolDispatchFault> {
    match future.await {
        Ok(output) if output.call_id() == call_id => Ok(output),
        Ok(_) => Err(NativeToolDispatchFault::OutputCallIdMismatch { call_id }),
        Err(message) => Err(NativeToolDispatchFault::Handler { call_id, message }),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeToolDispatchFault {
    #[error("unsupported tool `{name}` for call `{call_id}`")]
    WrongTool { call_id: String, name: String },
    #[error("tool call `{call_id}` handler failed: {message}")]
    Handler { call_id: String, message: String },
    #[error("tool handler returned output for a different call than `{call_id}`")]
    OutputCallIdMismatch { call_id: String },
}
