//! Typed command declarations collected from the mounted Component tree.

use std::{fmt, future::Future, marker::PhantomData, pin::Pin};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::{
    component::{execution::CommandOutcome, signal::HookMount},
    pom::{Document, MixedContent, RawTextNode, XmlName, XmlNode},
};

use super::{
    async_task::ComponentTaskContext,
    declaration::{Component, ComponentNode},
    ComponentAttemptFault,
};

/// One named string argument exposed by the command-line adapter.
///
/// The same input is also available as a JSON object through `--stdin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandArgument {
    pub name: &'static str,
    pub description: &'static str,
    pub example: &'static str,
}

/// Static metadata shared by a CLI client and its mounted action callback.
///
/// The input is a JSON object decoded with `serde`. Use an empty struct for a
/// command without arguments, and `#[serde(deny_unknown_fields)]` when extra
/// fields should be rejected. This descriptor owns no application state.
pub struct Command<Input> {
    name: &'static str,
    description: &'static str,
    argument: Option<CommandArgument>,
    input: PhantomData<fn(Input)>,
}

impl<Input> Copy for Command<Input> {}

impl<Input> Clone for Command<Input> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Input> Command<Input> {
    pub const fn new(
        name: &'static str,
        description: &'static str,
        argument: Option<CommandArgument>,
    ) -> Self {
        Self {
            name,
            description,
            argument,
            input: PhantomData,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn description(&self) -> &'static str {
        self.description
    }

    pub const fn argument(&self) -> Option<CommandArgument> {
        self.argument
    }
}

impl<Input: DeserializeOwned> Command<Input> {
    /// Share this command's metadata and typed validation with a CLI parser.
    pub const fn definition(&self) -> CommandDefinition {
        CommandDefinition {
            name: self.name,
            description: self.description,
            argument: self.argument,
            validate: validate_input::<Input>,
        }
    }

    /// Validate client input against the same type used by the mounted callback.
    pub fn validate_input(&self, input: &Value) -> Result<(), String> {
        validate_input::<Input>(input)
    }
}

/// A command descriptor with its input type retained as a validator.
///
/// Use [`Command::definition`] to collect commands with different input types in
/// one [`CommandParser`](crate::component::execution::CommandParser).
#[derive(Clone, Copy)]
pub struct CommandDefinition {
    name: &'static str,
    description: &'static str,
    argument: Option<CommandArgument>,
    validate: fn(&Value) -> Result<(), String>,
}

impl CommandDefinition {
    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn description(&self) -> &'static str {
        self.description
    }

    pub const fn argument(&self) -> Option<CommandArgument> {
        self.argument
    }

    pub fn validate_input(&self, input: &Value) -> Result<(), String> {
        (self.validate)(input)
    }

    pub(crate) fn validate_metadata(&self) -> Result<(), ComponentAttemptFault> {
        validate_metadata(self.name, self.argument)
    }
}

/// A CLI action declared beside the Component state it operates on.
///
/// `view! { CliCommand { command: ..., enabled: ..., on_call: ... } }`
/// binds a typed synchronous callback. Collection happens during tree traversal;
/// callbacks run only when the application's command barrier dispatches input.
pub struct CliCommand;

impl CliCommand {
    pub fn props() -> CliCommandProps {
        CliCommandProps {
            command: (),
            enabled: true,
            invoke: (),
        }
    }
}

/// Properties for a command with a typed, state-capturing callback.
pub struct CliCommandProps<Descriptor = (), Callback = ()> {
    command: Descriptor,
    enabled: bool,
    invoke: Callback,
}

impl CliCommandProps {
    pub fn command<Input>(self, command: Command<Input>) -> CliCommandProps<Command<Input>> {
        CliCommandProps {
            command,
            enabled: self.enabled,
            invoke: (),
        }
    }
}

impl<Descriptor, Callback> CliCommandProps<Descriptor, Callback> {
    /// Advertise current availability and reject input while disabled. Enabled
    /// callbacks still validate authoritative business state.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

impl<Input: DeserializeOwned + 'static> CliCommandProps<Command<Input>> {
    /// Bind a synchronous action. An `Ok` value is the command's serializable
    /// business outcome; `Err` reports a runtime handler fault. Invalid input is
    /// rejected before invoking the callback. Captures never become arguments.
    pub fn on_call<Handler, Output, Error>(
        self,
        mut handler: Handler,
    ) -> CliCommandProps<Command<Input>, CliCommandCallback>
    where
        Handler: FnMut(Input) -> Result<Output, Error> + Send + 'static,
        Output: Serialize,
        Error: fmt::Display,
    {
        CliCommandProps {
            command: self.command,
            enabled: self.enabled,
            invoke: CliCommandCallback {
                invoke: Box::new(move |input| {
                    let input = match decode_input::<Input>(input) {
                        Ok(input) => input,
                        Err(message) => {
                            return Box::pin(async move {
                                Ok(CommandOutcome::Rejected {
                                    code: "invalid_arguments".to_owned(),
                                    message,
                                })
                            });
                        }
                    };
                    let output =
                        handler(input)
                            .map_err(|error| error.to_string())
                            .and_then(|output| {
                                serde_json::to_value(output)
                                    .map(CommandOutcome::Output)
                                    .map_err(|error| error.to_string())
                            });
                    Box::pin(std::future::ready(output))
                }),
            },
        }
    }
}

/// A typed callback after its JSON input boundary has been bound.
#[doc(hidden)]
pub struct CliCommandCallback {
    invoke: CommandCallback,
}

pub(crate) type CommandFuture =
    Pin<Box<dyn Future<Output = Result<CommandOutcome, String>> + Send + 'static>>;

pub(crate) type CommandCallback = Box<dyn FnMut(Value) -> CommandFuture + Send + 'static>;

impl<Input> CliCommandProps<Command<Input>, CliCommandCallback> {
    pub fn build(self) -> Component {
        Component::from_node(ComponentNode::CliCommand(Box::new(CliCommandDeclaration {
            name: self.command.name,
            description: self.command.description.to_owned(),
            argument: self.command.argument,
            input_schema: None,
            enabled: self.enabled,
            invoke: self.invoke.invoke,
            mount: None,
            task_context: None,
        })))
    }
}

pub(crate) struct CliCommandDeclaration {
    name: &'static str,
    description: String,
    argument: Option<CommandArgument>,
    input_schema: Option<Value>,
    enabled: bool,
    invoke: CommandCallback,
    mount: Option<HookMount>,
    task_context: Option<ComponentTaskContext>,
}

impl CliCommandDeclaration {
    pub(crate) fn json(
        name: &'static str,
        description: String,
        enabled: bool,
        input_schema: Value,
        invoke: CommandCallback,
    ) -> Self {
        Self {
            name,
            description,
            argument: None,
            input_schema: Some(input_schema),
            enabled,
            invoke,
            mount: None,
            task_context: None,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn mount(&mut self, mount: HookMount, task_context: Option<ComponentTaskContext>) {
        self.mount = Some(mount);
        self.task_context = task_context;
    }

    pub(crate) async fn invoke(&mut self, input: Value) -> Result<CommandOutcome, String> {
        let Some(_permit) = self.mount.as_ref().and_then(HookMount::authorize) else {
            return Ok(CommandOutcome::Rejected {
                code: "command_unavailable".to_owned(),
                message: format!("Command `{}` is no longer mounted.", self.name),
            });
        };
        if !self.enabled {
            return Ok(CommandOutcome::Rejected {
                code: "command_disabled".to_owned(),
                message: format!("Command `{}` is currently disabled.", self.name),
            });
        }
        match self.task_context.clone() {
            Some(context) => {
                let future = context.scope_sync(|| (self.invoke)(input));
                context.scope(future).await
            }
            None => (self.invoke)(input).await,
        }
    }

    pub(crate) fn prompt_document(&self, prefix: &str) -> Result<Document, ComponentAttemptFault> {
        validate_metadata(self.name, self.argument)?;

        if let Some(schema) = &self.input_schema {
            return self.json_prompt_document(schema);
        }

        let command = if prefix.is_empty() {
            self.name.to_owned()
        } else {
            format!("{prefix} {}", self.name)
        };
        let mut actions = XmlNode::new(XmlName::new("actions").expect("static name"));
        match self.argument {
            None => actions.push(MixedContent::xml(self.action_node(command, "no arguments"))),
            Some(argument) => {
                actions.push(MixedContent::xml(self.action_node(
                    format!("{command} --{} TOKEN", argument.name),
                    argument.description,
                )));
                actions.push(MixedContent::xml(self.action_node(
                    format!("{command} --stdin"),
                    format!(
                        "JSON on stdin: {}; {}",
                        json!({ (argument.name): argument.example }),
                        argument.description,
                    ),
                )));
            }
        }
        Ok(Document::from_xml(actions))
    }

    fn json_prompt_document(&self, schema: &Value) -> Result<Document, ComponentAttemptFault> {
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(ComponentAttemptFault::RuntimeInvariant {
                message: format!(
                    "Action `{}` input schema must have type `object`",
                    self.name
                ),
            });
        }
        let mut action = XmlNode::new(XmlName::new("action").expect("static name"));
        for (name, value) in [
            ("name", self.name.to_owned()),
            ("description", self.description.clone()),
            ("enabled", self.enabled.to_string()),
        ] {
            action
                .push_attribute(XmlName::new(name).expect("static name"), value)
                .expect("unique attribute");
        }
        let mut input_schema = XmlNode::new(XmlName::new("input_schema").expect("static name"));
        input_schema.push(MixedContent::raw_text(RawTextNode::new(schema.to_string())));
        action.push(MixedContent::xml(input_schema));
        let mut stdin = XmlNode::new(XmlName::new("stdin").expect("static name"));
        stdin.push(MixedContent::raw_text(RawTextNode::new(format!(
            "Send one JSON object per line with \"action\":{} and \"input\" set to an object matching input_schema.",
            json!(self.name),
        ))));
        action.push(MixedContent::xml(stdin));
        let mut actions = XmlNode::new(XmlName::new("actions").expect("static name"));
        actions.push(MixedContent::xml(action));
        Ok(Document::from_xml(actions))
    }

    fn action_node(&self, command: String, input: impl Into<String>) -> XmlNode {
        let mut action = XmlNode::new(XmlName::new("action").expect("static name"));
        for (name, value) in [
            ("command", command),
            ("description", self.description.to_owned()),
            ("enabled", self.enabled.to_string()),
            ("input", input.into()),
        ] {
            action
                .push_attribute(XmlName::new(name).expect("static name"), value)
                .expect("unique attribute");
        }
        action
    }
}

fn validate_input<Input: DeserializeOwned>(input: &Value) -> Result<(), String> {
    decode_input::<Input>(input.clone()).map(|_| ())
}

pub(crate) fn decode_input<Input: DeserializeOwned>(input: Value) -> Result<Input, String> {
    if !input.is_object() {
        return Err("Command input must be a JSON object.".to_owned());
    }
    serde_json::from_value(input).map_err(|error| error.to_string())
}

fn validate_metadata(
    name: &str,
    argument: Option<CommandArgument>,
) -> Result<(), ComponentAttemptFault> {
    validate_token("command", name)?;
    if let Some(argument) = argument {
        validate_token("argument", argument.name)?;
        if argument.name == "stdin" {
            return Err(ComponentAttemptFault::RuntimeInvariant {
                message: "CLI argument `stdin` conflicts with the JSON input option".to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_token(kind: &str, value: &str) -> Result<(), ComponentAttemptFault> {
    if !value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ComponentAttemptFault::RuntimeInvariant {
            message: format!(
                "CLI {kind} `{value}` must start with an ASCII letter or digit and contain only ASCII letters, digits, `_`, or `-`"
            ),
        });
    }
    Ok(())
}
