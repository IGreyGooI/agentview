use std::collections::{HashMap, VecDeque};

use serde::Serialize;

use crate::{
    component::execution::{RenderedProjection, RenderedProjectionNode},
    pom_renderer::render_pom_document,
    provider::projection_diff::{ProjectionAppendPolicy, ProjectionDiffState},
    transcript::{
        AssistantPhase, CanonicalInputItem, CanonicalTranscriptError, ConversationRole,
        InstructionAuthority,
    },
};

use super::{OpenAiChatCompletionsError, OpenAiChatCompletionsOptions};

#[derive(Clone)]
pub(super) struct ChatHistory {
    history_epoch: u64,
    system_instructions: Vec<CanonicalInputItem>,
    submitted_projection: RenderedProjection,
    submitted_items: Vec<CanonicalInputItem>,
    unclaimed_provider_outputs: Vec<CanonicalInputItem>,
    wire_messages: Vec<ChatMessage>,
    open_text: String,
    partial_records: Vec<String>,
}

pub(super) struct PreparedChatHistory {
    pub(super) request_body: Vec<u8>,
    pub(super) candidate: ChatHistory,
    pub(super) memo: ChatDiffMemo,
}

pub(super) struct ChatDiffMemo {
    history_epoch: u64,
    scope: Option<crate::component::execution::ProjectionExecutionScope>,
    diff_state: ProjectionDiffState,
}

impl ChatDiffMemo {
    fn valid_for(
        &self,
        history_epoch: u64,
        scope: Option<crate::component::execution::ProjectionExecutionScope>,
    ) -> bool {
        self.history_epoch == history_epoch && self.scope == scope
    }
}

impl ChatHistory {
    pub(super) fn prepare(
        previous: Option<&Self>,
        memo: Option<&ChatDiffMemo>,
        current: RenderedProjection,
        options: &OpenAiChatCompletionsOptions,
        max_serialized_request_body_bytes: usize,
    ) -> Result<PreparedChatHistory, OpenAiChatCompletionsError> {
        if !current.native_tool_names().is_empty() {
            return Err(OpenAiChatCompletionsError::UnsupportedNativeToolDeclarations);
        }
        let current_system_instructions = projection_system_instructions(&current);
        ensure_system_authority(previous, &current_system_instructions)?;
        let previous_diff = match (previous, memo) {
            (Some(history), Some(memo))
                if memo.valid_for(history.history_epoch, current.execution_scope()) =>
            {
                Some(&memo.diff_state)
            }
            _ => None,
        };
        let force_full = previous.is_some() && previous_diff.is_none();
        let prepared_diff = ProjectionDiffState::prepare(previous_diff, &current);
        let current = prepared_diff.submission;
        let memo_scope = current.execution_scope();
        let append_policy = prepared_diff.append_policy;
        let current_messages = lower_projection(&current)?;
        let candidate = if let Some(previous) = previous {
            let reconciled = reconcile_projection(previous, &current, &append_policy, force_full)?;
            let wire_messages = previous
                .wire_messages
                .iter()
                .cloned()
                .chain(
                    reconciled
                        .new_input_indices
                        .iter()
                        .map(|index| current_messages[*index].clone()),
                )
                .collect();
            Self {
                history_epoch: previous
                    .history_epoch
                    .checked_add(1)
                    .ok_or(OpenAiChatCompletionsError::InvalidProjectionReconciliation)?,
                system_instructions: previous.system_instructions.clone(),
                submitted_projection: reconciled.submitted_projection,
                submitted_items: reconciled.submitted_items,
                unclaimed_provider_outputs: reconciled.unclaimed_provider_outputs,
                wire_messages,
                open_text: String::new(),
                partial_records: Vec::new(),
            }
        } else {
            Self {
                history_epoch: 1,
                system_instructions: current_system_instructions,
                submitted_items: projection_items(&current).cloned().collect(),
                submitted_projection: current,
                unclaimed_provider_outputs: Vec::new(),
                wire_messages: current_messages,
                open_text: String::new(),
                partial_records: Vec::new(),
            }
        };
        ensure_all_tool_calls_closed(
            &candidate.submitted_items,
            &candidate.unclaimed_provider_outputs,
        )?;
        let candidate_epoch = candidate.history_epoch;
        let request_body = serde_json::to_vec(&ChatRequest {
            model: options.model(),
            messages: &candidate.wire_messages,
            stream: true,
            tool_choice: ToolChoice::None,
        })?;
        if request_body.len() > max_serialized_request_body_bytes {
            return Err(OpenAiChatCompletionsError::SerializedRequestBodyLimit);
        }
        Ok(PreparedChatHistory {
            request_body,
            candidate,
            memo: ChatDiffMemo {
                history_epoch: candidate_epoch,
                scope: memo_scope,
                diff_state: prepared_diff.candidate,
            },
        })
    }

    pub(super) fn with_output(
        mut self,
        output: String,
    ) -> Result<Self, OpenAiChatCompletionsError> {
        if self.open_text != output {
            return Err(OpenAiChatCompletionsError::InvalidProjectionReconciliation);
        }
        let output_item = CanonicalInputItem::assistant_text(output.clone(), None);
        let mut validated = transcript_from_items(&self.submitted_items)?;
        for item in &self.unclaimed_provider_outputs {
            validated = validated.appended(item.clone())?;
        }
        validated.appended(output_item.clone())?;
        self.unclaimed_provider_outputs.push(output_item);
        self.wire_messages
            .push(ChatMessage::text(ChatRole::Assistant, output));
        self.open_text.clear();
        self.partial_records.clear();
        Ok(self)
    }

    pub(super) fn record_text_partial(
        &mut self,
        delta: impl Into<String>,
    ) -> Result<(), OpenAiChatCompletionsError> {
        let delta = delta.into();
        let mut candidate = self.clone();
        candidate.open_text.push_str(&delta);
        candidate.partial_records.push(delta);
        *self = candidate;
        Ok(())
    }

    pub(super) fn abort_open_output(&mut self) {
        if self.open_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.open_text);
        self.partial_records.clear();
        self.unclaimed_provider_outputs
            .push(CanonicalInputItem::assistant_text(text.clone(), None));
        self.wire_messages
            .push(ChatMessage::text(ChatRole::Assistant, text));
        self.wire_messages.push(ChatMessage::text(
            ChatRole::User,
            "[agentview: assistant output interrupted before completion]".to_owned(),
        ));
    }
}

fn ensure_system_authority(
    previous: Option<&ChatHistory>,
    current: &[CanonicalInputItem],
) -> Result<(), OpenAiChatCompletionsError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous.system_instructions == current {
        Ok(())
    } else {
        Err(OpenAiChatCompletionsError::SystemInstructionChanged)
    }
}

fn projection_system_instructions(projection: &RenderedProjection) -> Vec<CanonicalInputItem> {
    projection_items(projection)
        .filter(|item| {
            matches!(
                item,
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::System,
                    ..
                }
            )
        })
        .cloned()
        .collect()
}

#[derive(Clone, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    tool_choice: ToolChoice,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum ToolChoice {
    None,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
enum ChatMessage {
    Text(ChatTextMessage),
    AssistantToolCall(ChatAssistantToolCallMessage),
    Tool(ChatToolMessage),
}

impl ChatMessage {
    fn text(role: ChatRole, content: String) -> Self {
        Self::Text(ChatTextMessage { role, content })
    }
}

#[derive(Clone, Serialize)]
struct ChatTextMessage {
    role: ChatRole,
    content: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum ChatRole {
    System,
    Developer,
    User,
    Assistant,
}

#[derive(Clone, Serialize)]
struct ChatAssistantToolCallMessage {
    role: AssistantRole,
    content: Option<String>,
    tool_calls: Vec<ChatToolCall>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum AssistantRole {
    Assistant,
}

#[derive(Clone, Serialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: FunctionKind,
    function: ChatFunctionCall,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum FunctionKind {
    Function,
}

#[derive(Clone, Serialize)]
struct ChatFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Clone, Serialize)]
struct ChatToolMessage {
    role: ToolRole,
    tool_call_id: String,
    content: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum ToolRole {
    Tool,
}

fn lower_projection(
    projection: &RenderedProjection,
) -> Result<Vec<ChatMessage>, OpenAiChatCompletionsError> {
    projection_items(projection).map(lower_item).collect()
}

fn projection_items(projection: &RenderedProjection) -> impl Iterator<Item = &CanonicalInputItem> {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items().iter())
}

fn lower_item(item: &CanonicalInputItem) -> Result<ChatMessage, OpenAiChatCompletionsError> {
    match item {
        CanonicalInputItem::Instruction { authority, pom } => {
            let role = match authority {
                InstructionAuthority::System => ChatRole::System,
                InstructionAuthority::Developer => ChatRole::Developer,
            };
            Ok(ChatMessage::text(role, render_pom_document(pom)?))
        }
        CanonicalInputItem::Message { role, pom } => {
            let role = match role {
                ConversationRole::User => ChatRole::User,
                ConversationRole::Assistant => ChatRole::Assistant,
            };
            Ok(ChatMessage::text(role, render_pom_document(pom)?))
        }
        CanonicalInputItem::AssistantText { text, .. } => {
            Ok(ChatMessage::text(ChatRole::Assistant, text.clone()))
        }
        CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } => Ok(ChatMessage::AssistantToolCall(
            ChatAssistantToolCallMessage {
                role: AssistantRole::Assistant,
                content: None,
                tool_calls: vec![ChatToolCall {
                    id: call_id.clone(),
                    kind: FunctionKind::Function,
                    function: ChatFunctionCall {
                        name: name.clone(),
                        arguments: raw_arguments.clone(),
                    },
                }],
            },
        )),
        CanonicalInputItem::ToolResult { call_id, content } => {
            Ok(ChatMessage::Tool(ChatToolMessage {
                role: ToolRole::Tool,
                tool_call_id: call_id.clone(),
                content: content.clone(),
            }))
        }
        CanonicalInputItem::ProviderExtension(extension) => {
            Err(OpenAiChatCompletionsError::UnsupportedProviderExtension {
                provider: extension.provider().to_owned(),
                capability: extension.capability().to_owned(),
                schema_version: extension.schema_version(),
            })
        }
    }
}

struct ReconciledProjection {
    submitted_projection: RenderedProjection,
    submitted_items: Vec<CanonicalInputItem>,
    unclaimed_provider_outputs: Vec<CanonicalInputItem>,
    new_input_indices: Vec<usize>,
}

fn reconcile_projection(
    previous: &ChatHistory,
    current: &RenderedProjection,
    append_policy: &ProjectionAppendPolicy,
    force_full: bool,
) -> Result<ReconciledProjection, OpenAiChatCompletionsError> {
    let mut submitted = previous
        .submitted_projection
        .nodes()
        .iter()
        .map(|node| MutableSubmittedNode {
            identity: node.identity().to_owned(),
            items: node.items().to_vec(),
        })
        .collect::<Vec<_>>();
    let mut node_indexes = submitted
        .iter()
        .enumerate()
        .map(|(index, node)| (node.identity.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut unclaimed_provider_outputs = previous.unclaimed_provider_outputs.clone();
    let mut new_input_indices = Vec::new();

    for current_node in current.nodes() {
        if node_indexes.contains_key(current_node.identity()) {
            continue;
        }
        let index = submitted.len();
        submitted.push(MutableSubmittedNode {
            identity: current_node.identity().to_owned(),
            items: Vec::new(),
        });
        node_indexes.insert(current_node.identity().to_owned(), index);
    }

    let mut matched = submitted
        .iter()
        .map(|node| vec![false; node.items.len()])
        .collect::<Vec<_>>();
    let mut submitted_items = previous.submitted_items.clone();
    let mut current_input_index = 0;
    for current_node in current.nodes() {
        for (item_index, item) in current_node.items().iter().enumerate() {
            let node_index = node_indexes
                .get(current_node.identity())
                .copied()
                .ok_or(OpenAiChatCompletionsError::InvalidProjectionReconciliation)?;
            let node = &mut submitted[node_index];
            if !force_full
                && !append_policy.requires_append(current_node.identity(), item_index)
                && claim_equal_item(&node.items, &mut matched[node_index], item)
            {
                current_input_index += 1;
                continue;
            }
            if let Some(index) = unclaimed_provider_outputs
                .iter()
                .position(|output| items_match_for_submission(output, item))
            {
                unclaimed_provider_outputs.remove(index);
                node.items.push(item.clone());
                submitted_items.push(item.clone());
                current_input_index += 1;
                continue;
            }
            node.items.push(item.clone());
            submitted_items.push(item.clone());
            new_input_indices.push(current_input_index);
            current_input_index += 1;
        }
    }

    let submitted_nodes = submitted
        .into_iter()
        .map(|node| RenderedProjectionNode::new(node.identity, node.items))
        .collect();
    let mut submitted_projection = RenderedProjection::with_native_tool_names(
        submitted_nodes,
        current.native_tool_names().to_vec(),
    )?;
    if let Some(scope) = current.execution_scope() {
        submitted_projection = submitted_projection.with_execution_scope(scope);
    }
    Ok(ReconciledProjection {
        submitted_projection,
        submitted_items,
        unclaimed_provider_outputs,
        new_input_indices,
    })
}

struct MutableSubmittedNode {
    identity: String,
    items: Vec<CanonicalInputItem>,
}

fn claim_equal_item(
    submitted: &[CanonicalInputItem],
    matched: &mut [bool],
    current: &CanonicalInputItem,
) -> bool {
    let Some(index) = submitted
        .iter()
        .zip(matched.iter())
        .position(|(item, matched)| !*matched && items_match_for_submission(item, current))
    else {
        return false;
    };
    matched[index] = true;
    true
}

fn items_match_for_submission(left: &CanonicalInputItem, right: &CanonicalInputItem) -> bool {
    match (left, right) {
        (
            CanonicalInputItem::AssistantText {
                text: left_text,
                phase: left_phase,
            },
            CanonicalInputItem::AssistantText {
                text: right_text,
                phase: right_phase,
            },
        ) => {
            left_text == right_text
                && normalized_final_phase(*left_phase) == normalized_final_phase(*right_phase)
        }
        _ => left == right,
    }
}

fn normalized_final_phase(phase: Option<AssistantPhase>) -> Option<AssistantPhase> {
    match phase {
        None | Some(AssistantPhase::FinalAnswer) => Some(AssistantPhase::FinalAnswer),
        commentary => commentary,
    }
}

fn transcript_from_items(
    items: &[CanonicalInputItem],
) -> Result<crate::transcript::CanonicalTranscript, CanonicalTranscriptError> {
    items.iter().cloned().try_fold(
        crate::transcript::CanonicalTranscript::new(),
        |transcript, item| transcript.appended(item),
    )
}

fn ensure_all_tool_calls_closed(
    submitted: &[CanonicalInputItem],
    outputs: &[CanonicalInputItem],
) -> Result<(), OpenAiChatCompletionsError> {
    let items = submitted.iter().chain(outputs);
    crate::transcript::CanonicalTranscript::validate_sequence(items.clone())?;
    let mut pending = VecDeque::new();
    for item in items {
        match item {
            CanonicalInputItem::ToolCall { call_id, .. } => pending.push_back(call_id.as_str()),
            CanonicalInputItem::ToolResult { call_id, .. } => {
                if pending.pop_front() != Some(call_id.as_str()) {
                    return Err(OpenAiChatCompletionsError::InvalidProjectionReconciliation);
                }
            }
            _ if !pending.is_empty() => {
                return Err(OpenAiChatCompletionsError::InvalidProjectionReconciliation);
            }
            _ => {}
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(OpenAiChatCompletionsError::InvalidProjectionReconciliation)
    }
}

impl From<CanonicalTranscriptError> for OpenAiChatCompletionsError {
    fn from(error: CanonicalTranscriptError) -> Self {
        Self::Canonical(error)
    }
}
