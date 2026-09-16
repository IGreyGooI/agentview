//! Shared argv and JSON input handling for application command adapters.

use std::{collections::HashSet, io::Read};

use serde_json::json;

use super::{command::InputObject, CommandCall};
use crate::component::authoring::CommandDefinition;

/// An invalid command invocation or input stream.
#[derive(Debug, thiserror::Error)]
pub enum CommandParseError {
    #[error("expected a command")]
    MissingCommand,
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("unexpected arguments for {0}")]
    UnexpectedArguments(String),
    #[error("invalid command catalog: {0}")]
    InvalidCatalog(String),
    #[error("invalid command input: {0}")]
    InvalidInput(String),
    #[error("stdin exceeds {0} bytes")]
    InputTooLarge(usize),
    #[error("read command input from stdin: {0}")]
    ReadInput(#[source] std::io::Error),
    #[error("expected stdin JSON such as {example}: {source}")]
    InvalidJson {
        example: String,
        #[source]
        source: serde_json::Error,
    },
}

/// A client-side catalog built from the same descriptors as mounted CLI actions.
///
/// It supports no arguments or one named string argument, with `--stdin` as its
/// JSON alternative. The application chooses its default command, reads process
/// arguments, and owns transport and output. No callback runs while parsing.
#[derive(Clone, Copy)]
pub struct CommandParser<'a> {
    commands: &'a [CommandDefinition],
}

impl<'a> CommandParser<'a> {
    pub const fn new(commands: &'a [CommandDefinition]) -> Self {
        Self { commands }
    }

    /// Parse arguments after the executable/session options and validate the
    /// typed input. The reader is consumed only for `--stdin`, up to the supplied
    /// byte limit plus one byte to detect an oversized payload.
    pub fn parse(
        &self,
        args: &[&str],
        stdin: impl Read,
        max_input_bytes: usize,
    ) -> Result<CommandCall, CommandParseError> {
        let (name, args) = args
            .split_first()
            .ok_or(CommandParseError::MissingCommand)?;
        let definition = self.definition(name)?;
        let input = match (definition.argument(), args) {
            (None, []) => json!({}),
            (Some(argument), [flag, value]) if *flag == format!("--{}", argument.name) => {
                json!({ (argument.name): value })
            }
            (Some(argument), ["--stdin"]) => {
                let read_limit = u64::try_from(max_input_bytes)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);
                let mut bytes = Vec::new();
                stdin
                    .take(read_limit)
                    .read_to_end(&mut bytes)
                    .map_err(CommandParseError::ReadInput)?;
                if bytes.len() > max_input_bytes {
                    return Err(CommandParseError::InputTooLarge(max_input_bytes));
                }
                serde_json::from_slice::<InputObject>(&bytes)
                    .map_err(|source| CommandParseError::InvalidJson {
                        example: json!({ (argument.name): argument.example }).to_string(),
                        source,
                    })?
                    .0
            }
            _ => return Err(CommandParseError::UnexpectedArguments((*name).to_owned())),
        };
        definition
            .validate_input(&input)
            .map_err(CommandParseError::InvalidInput)?;
        Ok(CommandCall::new(*name, input))
    }

    /// Validate a decoded adapter request against the shared command catalog.
    /// The mounted runtime separately checks its current callback availability.
    pub fn validate(&self, call: &CommandCall) -> Result<(), CommandParseError> {
        self.definition(call.name())?
            .validate_input(call.input())
            .map_err(CommandParseError::InvalidInput)
    }

    /// Command rows to include in an application's help text. Session options
    /// and lifecycle commands belong to the adapter's surrounding help text.
    pub fn help(&self) -> String {
        let mut help = String::new();
        for command in self.commands {
            let syntax = match command.argument() {
                Some(argument) => {
                    format!(
                        "{} --{} {}",
                        command.name(),
                        argument.name,
                        argument.example
                    )
                }
                None => command.name().to_owned(),
            };
            help.push_str(&format!("  {syntax:<22} {}\n", command.description()));
            if let Some(argument) = command.argument() {
                help.push_str(&format!(
                    "  {:<22} Read {} from stdin\n",
                    format!("{} --stdin", command.name()),
                    json!({ (argument.name): argument.example })
                ));
            }
        }
        help
    }

    fn definition(&self, name: &str) -> Result<&CommandDefinition, CommandParseError> {
        let mut names = HashSet::new();
        for command in self.commands {
            command
                .validate_metadata()
                .map_err(|error| CommandParseError::InvalidCatalog(error.to_string()))?;
            if !names.insert(command.name()) {
                return Err(CommandParseError::InvalidCatalog(format!(
                    "duplicate command `{}`",
                    command.name()
                )));
            }
        }
        self.commands
            .iter()
            .find(|command| command.name() == name)
            .ok_or_else(|| CommandParseError::UnknownCommand(name.to_owned()))
    }
}
