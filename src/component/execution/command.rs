//! Application-owned external command input. It is independent of provider facts.

use std::{collections::VecDeque, fmt};

use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::RenderedProjection;
use crate::component::signal::HookMount;
use crate::pom::{BlockChildren, BlockContent, Document, MixedContent, TextNode, XmlName, XmlNode};

/// An external invocation, decoded by a CLI or another application adapter.
///
/// Deserialization requires an input object with distinct top-level field names
/// and rejects unknown envelope fields. Programmatic calls are validated when
/// dispatched, so invalid model input can receive ordinary rejection feedback.
#[derive(Clone, Debug, Serialize)]
pub struct CommandCall {
    #[serde(rename = "action")]
    command: String,
    input: Value,
}

impl<'de> Deserialize<'de> for CommandCall {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CallVisitor;

        impl<'de> Visitor<'de> for CallVisitor {
            type Value = CommandCall;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object containing action and input")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut command = None;
                let mut input = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "action" | "command" => {
                            if command.is_some() {
                                return Err(de::Error::duplicate_field("action"));
                            }
                            command = Some(map.next_value::<String>()?);
                        }
                        "input" => {
                            if input.is_some() {
                                return Err(de::Error::duplicate_field("input"));
                            }
                            input = Some(map.next_value::<InputObject>()?.0);
                        }
                        _ => return Err(de::Error::unknown_field(&key, &["action", "input"])),
                    }
                }
                Ok(CommandCall {
                    command: command.ok_or_else(|| de::Error::missing_field("action"))?,
                    input: input.ok_or_else(|| de::Error::missing_field("input"))?,
                })
            }
        }

        deserializer.deserialize_map(CallVisitor)
    }
}

impl CommandCall {
    pub fn new(command: impl Into<String>, input: Value) -> Self {
        Self {
            command: command.into(),
            input,
        }
    }

    pub fn name(&self) -> &str {
        &self.command
    }

    pub fn input(&self) -> &Value {
        &self.input
    }
}

// Preserve the input object boundary before Value can silently discard duplicate
// field names. The same decoder is used for stdin and serialized adapter calls.
#[derive(Deserialize)]
#[serde(transparent)]
pub(super) struct InputObject(
    #[serde(deserialize_with = "deserialize_input_object")] pub(super) Value,
);

fn deserialize_input_object<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    struct ObjectVisitor;

    impl<'de> Visitor<'de> for ObjectVisitor {
        type Value = Value;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a JSON object with distinct field names")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut fields = serde_json::Map::new();
            while let Some((key, value)) = map.next_entry::<String, Value>()? {
                if fields.insert(key.clone(), value).is_some() {
                    return Err(de::Error::custom(format!("duplicate input field: {key}")));
                }
            }
            Ok(Value::Object(fields))
        }
    }

    deserializer.deserialize_map(ObjectVisitor)
}

/// A callback's serialized result, or an ordinary invocation rejection.
/// Business rejection may also be represented in an application's output value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Output(Value),
    Rejected { code: String, message: String },
}

/// The result and complete prepared view belonging to one command.
#[derive(Clone, Debug)]
pub struct CommandResponse {
    pub call: CommandCall,
    pub outcome: CommandOutcome,
    pub projection: RenderedProjection,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum CommandInputError {
    #[error("application command input is closed")]
    Closed,
    #[error("command callback failed: {0}")]
    Handler(String),
    #[error("command execution was interrupted; inspect application state before retrying")]
    Interrupted,
    #[error("application preparation failed; inspect application state before retrying")]
    PreparationFailed,
}

/// Cloneable ingress for one Application's bounded command queue.
#[derive(Clone)]
pub struct CommandSender {
    sender: mpsc::Sender<QueuedCommand>,
}

impl CommandSender {
    /// Enqueue once and await the prepared result. Dropping this future after
    /// enqueue does not retract the accepted command or undo its effects.
    pub async fn submit(&self, call: CommandCall) -> Result<CommandResponse, CommandInputError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(QueuedCommand::Call {
                call,
                reply: Some(reply),
            })
            .await
            .map_err(|_| CommandInputError::Closed)?;
        response.await.unwrap_or(Err(CommandInputError::Closed))
    }

    /// Wait for the next complete prepared view without invoking an action or
    /// changing its feedback. Observations use the same queue and explicit
    /// `use_wait_for_command()` barrier as action input.
    pub async fn observe(&self) -> Result<RenderedProjection, CommandInputError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(QueuedCommand::Observe {
                reply: Some(reply),
                feedback: None,
            })
            .await
            .map_err(|_| CommandInputError::Closed)?;
        response.await.unwrap_or(Err(CommandInputError::Closed))
    }

    /// Publish an input decoding diagnostic through the root wait's view, then
    /// observe the prepared state. The next action clears this diagnostic.
    pub async fn feedback(
        &self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<RenderedProjection, CommandInputError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(QueuedCommand::Observe {
                reply: Some(reply),
                feedback: Some((code.into(), message.into())),
            })
            .await
            .map_err(|_| CommandInputError::Closed)?;
        response.await.unwrap_or(Err(CommandInputError::Closed))
    }
}

enum QueuedCommand {
    Call {
        call: CommandCall,
        reply: Option<oneshot::Sender<Result<CommandResponse, CommandInputError>>>,
    },
    Observe {
        reply: Option<oneshot::Sender<Result<RenderedProjection, CommandInputError>>>,
        feedback: Option<(String, String)>,
    },
}

impl QueuedCommand {
    fn fail(&mut self, error: CommandInputError) {
        match self {
            Self::Call { reply, .. } => {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(error));
                }
            }
            Self::Observe { reply, .. } => {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(error));
                }
            }
        }
    }
}

pub(crate) enum PendingCommandState {
    Received,
    Started,
    Applied(CommandOutcome),
    Observed,
}

struct PendingCommand {
    request: QueuedCommand,
    state: PendingCommandState,
    origin: HookMount,
}

/// An inbox moved into `Application::mount_with_commands` by its owner.
/// Components access it by declaring `use_wait_for_command()`.
pub struct CommandInput {
    receiver: mpsc::Receiver<QueuedCommand>,
    pending: Option<PendingCommand>,
    prefix: String,
}

impl CommandInput {
    pub fn channel(capacity: usize) -> (CommandSender, Self) {
        assert!(capacity > 0, "command input capacity must be non-zero");
        let (sender, receiver) = mpsc::channel(capacity);
        (
            CommandSender { sender },
            Self {
                receiver,
                pending: None,
                prefix: String::new(),
            },
        )
    }

    /// Shell-quoted executable and session arguments prepended to advertised commands.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn belongs_to(&self, mount: &HookMount) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.origin.component == mount.component
                && pending.origin.generation == mount.generation
                && pending.origin.slot == mount.slot
        })
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn state(&self) -> Option<&PendingCommandState> {
        self.pending.as_ref().map(|pending| &pending.state)
    }

    pub(crate) fn call(&self) -> Option<&CommandCall> {
        match &self.pending.as_ref()?.request {
            QueuedCommand::Call { call, .. } => Some(call),
            QueuedCommand::Observe { .. } => None,
        }
    }

    pub(crate) fn feedback(&self) -> Option<&(String, String)> {
        match &self.pending.as_ref()?.request {
            QueuedCommand::Observe { feedback, .. } => feedback.as_ref(),
            _ => None,
        }
    }

    pub(crate) async fn receive(&mut self, origin: HookMount) -> bool {
        if self.pending.is_none() {
            let Some(request) = self.receiver.recv().await else {
                return false;
            };
            // No await between receiving and retaining ownership.
            self.pending = Some(PendingCommand {
                request,
                state: PendingCommandState::Received,
                origin,
            });
        }
        true
    }

    pub(crate) fn start(&mut self) {
        self.pending.as_mut().expect("received command").state = PendingCommandState::Started;
    }

    pub(crate) fn applied(&mut self, outcome: CommandOutcome) {
        self.pending.as_mut().expect("received command").state =
            PendingCommandState::Applied(outcome);
    }

    pub(crate) fn observed(&mut self) {
        self.pending.as_mut().expect("received observation").state = PendingCommandState::Observed;
    }

    pub(crate) fn fail_reply(&mut self, error: CommandInputError) {
        if let Some(pending) = self.pending.as_mut() {
            pending.request.fail(error);
        }
    }

    pub(crate) fn fail_command(&mut self, error: CommandInputError) {
        self.fail_reply(error);
        self.pending = None;
    }

    pub(crate) fn complete(&mut self, projection: &RenderedProjection) {
        if !matches!(
            self.state(),
            Some(PendingCommandState::Applied(_) | PendingCommandState::Observed)
        ) {
            return;
        }
        let pending = self.pending.take().expect("completed request");
        match (pending.request, pending.state) {
            (QueuedCommand::Call { call, reply }, PendingCommandState::Applied(outcome)) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(CommandResponse {
                        call,
                        outcome,
                        projection: projection.clone(),
                    }));
                }
            }
            (QueuedCommand::Observe { reply, .. }, PendingCommandState::Observed) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(projection.clone()));
                }
            }
            _ => unreachable!("request completion must match its kind"),
        }
    }

    pub(crate) fn close(&mut self) {
        self.receiver.close();
        self.fail_command(CommandInputError::Closed);
        while let Ok(mut queued) = self.receiver.try_recv() {
            queued.fail(CommandInputError::Closed);
        }
    }
}

impl Drop for CommandInput {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone, Default)]
pub(crate) struct CommandFeedback {
    sequence: u64,
    records: VecDeque<(u64, String, CommandOutcome)>,
    input_feedback: Option<(String, String)>,
}

impl CommandFeedback {
    pub(crate) fn record(&mut self, call: &CommandCall, outcome: CommandOutcome) {
        self.input_feedback = None;
        self.sequence = self.sequence.saturating_add(1);
        if self.records.len() == 8 {
            self.records.pop_front();
        }
        self.records
            .push_back((self.sequence, call.name().to_owned(), outcome));
    }

    pub(crate) fn input_feedback(&mut self, code: String, message: String) {
        self.input_feedback = Some((code, message));
    }

    pub(crate) fn prompt_document(&self) -> Result<Document, String> {
        self.build_document().map_err(|error| error.to_string())
    }

    fn build_document(&self) -> Result<Document, crate::pom::PomError> {
        let mut root = XmlNode::new(XmlName::new("command_feedback")?);
        root.push_attribute(XmlName::new("sequence")?, self.sequence.to_string())?;
        if let Some((code, message)) = &self.input_feedback {
            let mut feedback = XmlNode::new(XmlName::new("input_feedback")?);
            feedback.push_attribute(XmlName::new("code")?, feedback_display(code, 128))?;
            feedback.push(MixedContent::text(TextNode::new(feedback_display(
                message, 4096,
            ))));
            root.push(MixedContent::xml(feedback));
        }
        for (sequence, command, outcome) in &self.records {
            let mut entry = XmlNode::new(XmlName::new("command_result")?);
            entry.push_attribute(XmlName::new("sequence")?, sequence.to_string())?;
            entry.push_attribute(XmlName::new("command")?, feedback_display(command, 512))?;
            let text = match outcome {
                CommandOutcome::Output(value) => {
                    entry.push_attribute(XmlName::new("status")?, "completed")?;
                    value.to_string()
                }
                CommandOutcome::Rejected { code, message } => {
                    entry.push_attribute(XmlName::new("status")?, "rejected")?;
                    entry.push_attribute(XmlName::new("code")?, feedback_display(code, 128))?;
                    message.clone()
                }
            };
            entry.push(MixedContent::text(TextNode::new(feedback_display(
                &text, 4096,
            ))));
            root.push(MixedContent::xml(entry));
        }
        let mut children = BlockChildren::new();
        children.push(BlockContent::xml(root));
        Ok(Document::new(children))
    }
}

// Bound the displayed fields after XML/Markdown escaping and JSON string
// encoding, so all eight retained records leave room for the application's
// current view. The original invocation and output stay in CommandResponse.
fn feedback_display(value: &str, encoded_budget: usize) -> String {
    const TRUNCATED: &str = "… (truncated)";
    let suffix_bytes: usize = TRUNCATED.chars().map(feedback_encoded_bytes).sum();
    let content_budget = encoded_budget.saturating_sub(suffix_bytes);
    let mut displayed = String::with_capacity(value.len().min(encoded_budget));
    let mut encoded_bytes = 0;
    for character in value.chars() {
        if matches!(character, '\0'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')
        {
            // XML forbids these characters. Keep each escape intact when
            // truncating so even malformed input remains useful feedback.
            let escaped = character.escape_unicode();
            encoded_bytes += escaped.clone().map(feedback_encoded_bytes).sum::<usize>();
            if encoded_bytes > content_budget {
                displayed.push_str(TRUNCATED);
                break;
            }
            displayed.extend(escaped);
        } else {
            encoded_bytes += feedback_encoded_bytes(character);
            if encoded_bytes > content_budget {
                displayed.push_str(TRUNCATED);
                break;
            }
            displayed.push(character);
        }
    }
    displayed
}

// A conservative per-character bound for both XML attributes and Markdown text
// inside XML, including subsequent JSON escaping of the rendered projection.
fn feedback_encoded_bytes(character: char) -> usize {
    match character {
        '\"' | '\'' | ' ' | '\t' | '\n' | '\r' => 6,
        '&' => 5,
        '<' | '>' | '\\' => 4,
        '`' | '*' | '_' | '[' | ']' | '#' | '-' | '+' | '~' | '.' | ')' => 3,
        _ => character.len_utf8(),
    }
}
