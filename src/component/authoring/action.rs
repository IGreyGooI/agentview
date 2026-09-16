//! Structured actions authored alongside the view that exposes them.

use std::{fmt, future::Future};

use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::component::execution::CommandOutcome;

use super::{
    cli_command::{decode_input, CliCommandDeclaration, CommandCallback, CommandFuture},
    declaration::{Component, ComponentNode},
};

/// An application action receiving a JSON object from stdin or another adapter.
///
/// Declare `Action { name, description, enabled, on_call }` in `view!`. The
/// callback's input type derives `Deserialize` and `JsonSchema`; the mounted view
/// advertises that schema and the runtime decodes input before calling it. A
/// callback without arguments can be written as `||` and accepts an empty object.
/// Collection and execution use the application's explicit
/// [`use_wait_for_command`](super::use_wait_for_command) barrier.
pub struct Action;

impl Action {
    pub fn props() -> ActionProps {
        ActionProps {
            name: (),
            description: String::new(),
            enabled: true,
            callback: (),
        }
    }
}

/// Properties for a JSON action with an inline, state-capturing callback.
pub struct ActionProps<Name = (), Callback = ()> {
    name: Name,
    description: String,
    enabled: bool,
    callback: Callback,
}

impl<Name, Callback> ActionProps<Name, Callback> {
    pub fn name(self, name: &'static str) -> ActionProps<&'static str, Callback> {
        ActionProps {
            name,
            description: self.description,
            enabled: self.enabled,
            callback: self.callback,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Advertise availability and reject input without invoking a disabled
    /// callback. Enabled callbacks still validate current business state.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

impl<Name> ActionProps<Name> {
    /// Bind a synchronous or asynchronous callback accepting a typed object or no arguments.
    ///
    /// `Ok` carries the serializable business outcome. Expected rejections belong
    /// in that outcome and the application view; `Err` reports a handler fault.
    /// Callback captures are refreshed on render and never become model inputs.
    pub fn on_call<Arguments, Handler, Return, Mode>(
        self,
        handler: Handler,
    ) -> ActionProps<Name, ActionCallback>
    where
        Handler: ActionHandler<Arguments, Return, Mode>,
    {
        ActionProps {
            name: self.name,
            description: self.description,
            enabled: self.enabled,
            callback: handler.into_action_callback(),
        }
    }
}

impl ActionProps<&'static str, ActionCallback> {
    pub fn build(self) -> Component {
        Component::from_node(ComponentNode::CliCommand(Box::new(
            CliCommandDeclaration::json(
                self.name,
                self.description,
                self.enabled,
                self.callback.schema,
                self.callback.invoke,
            ),
        )))
    }
}

/// Erased action callback with the schema inferred from its argument type.
#[doc(hidden)]
pub struct ActionCallback {
    schema: Value,
    invoke: CommandCallback,
}

/// Callback conversion used to infer typed and zero-argument closure signatures.
#[doc(hidden)]
pub trait ActionHandler<Arguments, Return, Mode>: Send + 'static {
    fn into_action_callback(self) -> ActionCallback;
}

impl<Input, Handler, Return, Mode> ActionHandler<(Input,), Return, Mode> for Handler
where
    Input: DeserializeOwned + JsonSchema + 'static,
    Handler: FnMut(Input) -> Return + Send + 'static,
    Return: ActionCallbackReturn<Mode>,
{
    fn into_action_callback(mut self) -> ActionCallback {
        ActionCallback {
            schema: schemars::schema_for!(Input).to_value(),
            invoke: Box::new(move |input| {
                let input = match decode_input::<Input>(input) {
                    Ok(input) => input,
                    Err(message) => {
                        return Box::pin(async move { Ok(invalid_input(message)) });
                    }
                };
                self(input).into_action_future()
            }),
        }
    }
}

impl<Handler, Return, Mode> ActionHandler<(), Return, Mode> for Handler
where
    Handler: FnMut() -> Return + Send + 'static,
    Return: ActionCallbackReturn<Mode>,
{
    fn into_action_callback(mut self) -> ActionCallback {
        ActionCallback {
            schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            invoke: Box::new(move |input| {
                if !input.as_object().is_some_and(|fields| fields.is_empty()) {
                    return Box::pin(async move {
                        Ok(invalid_input(
                            "This action takes no input fields; send an empty JSON object."
                                .to_owned(),
                        ))
                    });
                }
                self().into_action_future()
            }),
        }
    }
}

/// Conversion used by Action callbacks to accept sync and async results.
#[doc(hidden)]
pub trait ActionCallbackReturn<Mode> {
    fn into_action_future(self) -> CommandFuture;
}

#[doc(hidden)]
pub enum SyncActionCallback {}
#[doc(hidden)]
pub enum AsyncActionCallback {}

impl<Output, Error> ActionCallbackReturn<SyncActionCallback> for Result<Output, Error>
where
    Output: Serialize,
    Error: fmt::Display,
{
    fn into_action_future(self) -> CommandFuture {
        Box::pin(std::future::ready(serialize_output(self)))
    }
}

impl<HandlerFuture, Output, Error> ActionCallbackReturn<AsyncActionCallback> for HandlerFuture
where
    HandlerFuture: Future<Output = Result<Output, Error>> + Send + 'static,
    Output: Serialize,
    Error: fmt::Display,
{
    fn into_action_future(self) -> CommandFuture {
        Box::pin(async move { serialize_output(self.await) })
    }
}

fn serialize_output<Output: Serialize, Error: fmt::Display>(
    result: Result<Output, Error>,
) -> Result<CommandOutcome, String> {
    let output = result.map_err(|error| error.to_string())?;
    serde_json::to_value(output)
        .map(CommandOutcome::Output)
        .map_err(|error| error.to_string())
}

fn invalid_input(message: String) -> CommandOutcome {
    CommandOutcome::Rejected {
        code: "invalid_arguments".to_owned(),
        message,
    }
}
