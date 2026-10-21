//! Application-owned external command input. It is independent of provider facts.

use std::{collections::VecDeque, fmt};

use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::RenderedProjection;
use crate::component::authoring::command_feedback::{
    CommandFeedbackView, CommandResultView, InputFeedbackView,
};
use crate::component::signal::HookMount;

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

pub(crate) struct CommandFrameResponse {
    pub(crate) outcomes: Vec<(CommandCall, CommandOutcome)>,
    pub(crate) projection: RenderedProjection,
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

    /// Retain one complete stdin frame in the queue until every action has
    /// prepared. A dropped waiter does not retract the accepted frame.
    pub(crate) async fn submit_frame(
        &self,
        calls: Vec<CommandCall>,
    ) -> Result<CommandFrameResponse, CommandInputError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(QueuedCommand::Frame {
                calls: calls.into(),
                outcomes: Vec::new(),
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
    Frame {
        calls: VecDeque<CommandCall>,
        outcomes: Vec<(CommandCall, CommandOutcome)>,
        reply: Option<oneshot::Sender<Result<CommandFrameResponse, CommandInputError>>>,
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
            Self::Frame { reply, .. } => {
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
    reserved: Option<QueuedCommand>,
    pending: Option<PendingCommand>,
    prefix: String,
    new_feedback_frame: bool,
}

impl CommandInput {
    pub fn channel(capacity: usize) -> (CommandSender, Self) {
        assert!(capacity > 0, "command input capacity must be non-zero");
        let (sender, receiver) = mpsc::channel(capacity);
        (
            CommandSender { sender },
            Self {
                receiver,
                reserved: None,
                pending: None,
                prefix: String::new(),
                new_feedback_frame: true,
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

    pub(crate) fn finish_feedback_frame(&mut self) {
        self.new_feedback_frame = true;
    }

    pub(crate) fn take_new_feedback_frame(&mut self) -> bool {
        std::mem::take(&mut self.new_feedback_frame)
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
            QueuedCommand::Frame { calls, .. } => calls.front(),
            QueuedCommand::Observe { .. } => None,
        }
    }

    pub(crate) fn feedback(&self) -> Option<&(String, String)> {
        match &self.pending.as_ref()?.request {
            QueuedCommand::Observe { feedback, .. } => feedback.as_ref(),
            _ => None,
        }
    }

    /// The stdin owner waits for demand before running ordinary preparations.
    /// Keep the request unbound until preparation reaches the current wait mount.
    pub(crate) async fn reserve_next(&mut self) -> bool {
        if self.pending.is_some() || self.reserved.is_some() {
            return true;
        }
        // recv is cancellation safe; retain ownership in the same poll that
        // receives the entry, before the caller can begin another operation.
        self.reserved = self.receiver.recv().await;
        self.reserved.is_some()
    }

    pub(crate) async fn receive(&mut self, origin: HookMount) -> bool {
        if self.pending.is_none() {
            let request = match self.reserved.take() {
                Some(request) => Some(request),
                None => self.receiver.recv().await,
            };
            let Some(request) = request else {
                return false;
            };
            if matches!(&request, QueuedCommand::Frame { calls, .. } if !calls.is_empty()) {
                self.finish_feedback_frame();
            }
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
        let PendingCommand {
            request,
            state,
            origin,
        } = self.pending.take().expect("completed request");
        match (request, state) {
            (QueuedCommand::Call { call, reply }, PendingCommandState::Applied(outcome)) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(CommandResponse {
                        call,
                        outcome,
                        projection: projection.clone(),
                    }));
                }
            }
            (
                QueuedCommand::Frame {
                    mut calls,
                    mut outcomes,
                    reply,
                },
                PendingCommandState::Applied(outcome),
            ) => {
                let call = calls.pop_front().expect("completed frame action");
                outcomes.push((call, outcome));
                if calls.is_empty() {
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(CommandFrameResponse {
                            outcomes,
                            projection: projection.clone(),
                        }));
                    }
                } else {
                    // Keep ownership of this queue entry across preparations;
                    // no observation or other frame can interrupt its actions.
                    self.pending = Some(PendingCommand {
                        request: QueuedCommand::Frame {
                            calls,
                            outcomes,
                            reply,
                        },
                        state: PendingCommandState::Received,
                        origin,
                    });
                }
            }
            (
                QueuedCommand::Frame {
                    outcomes, reply, ..
                },
                PendingCommandState::Observed,
            ) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(CommandFrameResponse {
                        outcomes,
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
        if let Some(mut reserved) = self.reserved.take() {
            reserved.fail(CommandInputError::Closed);
        }
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
    records: Vec<(u64, String, CommandOutcome)>,
    input_feedback: Option<(String, String)>,
}

impl CommandFeedback {
    pub(crate) fn record(&mut self, call: &CommandCall, outcome: CommandOutcome, new_frame: bool) {
        self.input_feedback = None;
        self.sequence = self.sequence.saturating_add(1);
        if new_frame {
            self.records.clear();
        }
        self.records
            .push((self.sequence, call.name().to_owned(), outcome));
    }

    pub(crate) fn input_feedback(&mut self, code: String, message: String) {
        self.input_feedback = Some((code, message));
    }

    pub(crate) fn snapshot(&self) -> CommandFeedbackView {
        CommandFeedbackView {
            sequence: self.sequence,
            input_feedback: self
                .input_feedback
                .as_ref()
                .map(|(code, text)| InputFeedbackView {
                    code: code.clone(),
                    text: text.clone(),
                }),
            records: self
                .records
                .iter()
                .map(|(sequence, command, outcome)| {
                    let (status, code, text) = match outcome {
                        CommandOutcome::Output(value) => ("completed", None, value.to_string()),
                        CommandOutcome::Rejected { code, message } => {
                            ("rejected", Some(code.clone()), message.clone())
                        }
                    };
                    CommandResultView {
                        sequence: *sequence,
                        command: command.clone(),
                        status,
                        code,
                        text,
                    }
                })
                .collect(),
        }
    }
}
