use serde_json::{json, Value};

use crate::{
    component::execution::{
        Frame, ReactionPortFault as Fault, ReactionPortFaultCode as Code,
        ReactionPortFaultReason as Reason,
    },
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, ConversationRole},
};

use super::{wire, Accepted, MAX_REQUEST_BYTES};

pub(super) struct Request {
    pub(super) configuration: Value,
    pub(super) commands: Vec<(String, Option<&'static str>)>,
}

pub(super) fn encode(
    frame: &Frame,
    accepted: Option<&Accepted>,
    model: &str,
    voice: &str,
) -> Result<Request, Fault> {
    let submission = frame.submission();
    let mut context = Vec::new();
    let mut results = Vec::new();
    let replay = if accepted.is_none() {
        submission.replay()
    } else {
        &[]
    };
    for item in replay
        .iter()
        .chain(submission.staged_inputs())
        .chain(submission.projection().items())
    {
        match item {
            CanonicalInputItem::Instruction { .. } => {}
            CanonicalInputItem::Message { role, pom } => {
                let role = match role {
                    ConversationRole::User => "user",
                    ConversationRole::Assistant => "assistant",
                };
                context.push(message(
                    role,
                    render_pom_document(pom).map_err(|_| preparation_fault())?,
                ));
            }
            CanonicalInputItem::AssistantText { text, .. } => {
                context.push(message("assistant", text.clone()))
            }
            CanonicalInputItem::ToolResult { call_id, content } => {
                // The preceding exchange owns these provider call IDs. Its results
                // were sent in-band or retired with a cancelled response. Sending
                // them here again could resolve a call outside its own response.
                if !accepted.is_some_and(|state| state.tool_calls.contains(call_id)) {
                    results.push((call_id.clone(), content.clone()));
                }
            }
            _ => return Err(preparation_fault()),
        }
    }
    let tools = submission.tools().definitions();
    // The service has no strict-schema setting or decision endpoint. Reject
    // the unsupported Frame before sending audio or applying remote state.
    if submission.decision().is_some() || tools.iter().any(|tool| tool.strict()) {
        return Err(preparation_fault());
    }

    // Scoped instructions can be restored by the next update. A conversation
    // history item would survive the exchange and repeat in later responses.
    let mut instructions = Vec::new();
    for item in frame
        .complete_instructions()
        .iter()
        .chain(submission.ephemeral())
    {
        let CanonicalInputItem::Instruction { pom, .. } = item else {
            return Err(preparation_fault());
        };
        instructions.push(render_pom_document(pom).map_err(|_| preparation_fault())?);
    }
    let instructions = instructions.join("\n\n");

    let update = match accepted {
        None => wire::session_create(model, voice, &instructions, tools),
        Some(_) => wire::session_update(model, voice, &instructions, tools),
    };
    let configuration = update["session"].clone();
    let mut commands = Vec::new();
    if accepted.is_none_or(|state| state.configuration != configuration) {
        let ack = if accepted.is_none() {
            "session.created"
        } else {
            "session.updated"
        };
        commands.push((update.to_string(), Some(ack)));
    }
    if !results.is_empty() {
        commands.push((wire::tool_results(&results).to_string(), None));
    }
    if !context.is_empty() {
        commands.push((
            json!({
                "type": "conversation.item.create", "event_id": wire::id("event"), "items": context,
            })
            .to_string(),
            Some("conversation.item.added"),
        ));
    }
    if commands
        .iter()
        .map(|(command, _)| command.len())
        .sum::<usize>()
        > MAX_REQUEST_BYTES
    {
        return Err(Fault::retryable(Code::Limit, Reason::RequestPreparation));
    }
    Ok(Request {
        configuration,
        commands,
    })
}

fn message(role: &str, text: String) -> Value {
    json!({
        "id": wire::id("item"), "type": "message", "role": role,
        "content": [{"type": "input_text", "text": text}],
    })
}

pub(super) fn preparation_fault() -> Fault {
    Fault::retryable(Code::Rejected, Reason::RequestPreparation)
}
