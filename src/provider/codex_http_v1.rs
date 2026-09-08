//! Deterministic OpenAI Responses JSON for the `codex-http-v1` profile.
//!
//! This module owns only history policy and request DTO serialization. HTTP,
//! SSE, retry, and `async-openai` integration are intentionally separate.

use std::{
    collections::HashSet,
    io::{self, Write},
};

use crate::{
    pom_renderer::{PomRenderError, render_pom_document},
    transcript::{
        ASSISTANT_OUTPUT_INTERRUPTED_MARKER, AssistantPhase, AssistantTextStatus,
        CanonicalInputItem, CanonicalTranscript, ConversationRole, InstructionAuthority,
        ProviderExtension,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{HistoryPolicy, ProviderRequestEncoder};

pub const CODEX_HTTP_V1_PROFILE: &str = "codex-http-v1";
pub const CODEX_HTTP_V1_CODEX_REVISION: &str = "4f1992732c832fe125608980a03ec2b66710c4e4";

const OPENAI_PROVIDER: &str = "openai";
const REASONING_CAPABILITY: &str = "reasoning.encrypted_content";
const REASONING_SCHEMA_VERSION: u32 = 1;
const FUNCTION_NAME_MAX_LEN: usize = 128;
const DEFAULT_COMPACTION_THRESHOLD: u32 = 200_000;

/// One function tool in the Codex Responses request schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexFunctionTool {
    description: String,
    name: String,
    parameters: Value,
    strict: bool,
    #[serde(rename = "type")]
    kind: FunctionToolKind,
}

impl CodexFunctionTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        strict: bool,
    ) -> Result<Self, CodexHttpV1Error> {
        let name = name.into();
        validate_function_name(&name)?;
        if !parameters.is_object() {
            return Err(CodexHttpV1Error::ToolParametersMustBeObject { name });
        }
        Ok(Self {
            description: description.into(),
            name,
            parameters,
            strict,
            kind: FunctionToolKind::Function,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum FunctionToolKind {
    Function,
}

/// Codex request-side reasoning controls used by the pinned oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodexReasoning {
    effort: ReasoningEffort,
    summary: ReasoningSummary,
}

impl CodexReasoning {
    pub const fn max_detailed() -> Self {
        Self {
            effort: ReasoningEffort::Max,
            summary: ReasoningSummary::Detailed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningEffort {
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningSummary {
    Detailed,
}

/// Immutable inputs that vary between `codex-http-v1` operations.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexHttpV1Options {
    model: String,
    tools: Option<Vec<CodexFunctionTool>>,
    reasoning: Option<CodexReasoning>,
    prompt_cache_key: Option<String>,
    max_output_tokens: Option<u32>,
    context_management: bool,
    explicit_assistant_status: bool,
}

impl CodexHttpV1Options {
    pub fn new(
        model: impl Into<String>,
        tools: Option<Vec<CodexFunctionTool>>,
        reasoning: Option<CodexReasoning>,
        prompt_cache_key: Option<impl Into<String>>,
    ) -> Result<Self, CodexHttpV1Error> {
        let model = model.into();
        if model.is_empty() {
            return Err(CodexHttpV1Error::EmptyModel);
        }

        if let Some(duplicate) = duplicate_tool_name(tools.as_deref().unwrap_or_default()) {
            return Err(CodexHttpV1Error::DuplicateToolName { name: duplicate });
        }

        let prompt_cache_key = prompt_cache_key.map(Into::into);
        if prompt_cache_key.as_deref().is_some_and(str::is_empty) {
            return Err(CodexHttpV1Error::EmptyPromptCacheKey);
        }

        Ok(Self {
            model,
            tools,
            reasoning,
            prompt_cache_key,
            max_output_tokens: None,
            context_management: true,
            explicit_assistant_status: false,
        })
    }

    /// Limits model-generated output tokens for this request profile.
    pub fn with_max_output_tokens(
        mut self,
        max_output_tokens: u32,
    ) -> Result<Self, CodexHttpV1Error> {
        if max_output_tokens == 0 {
            return Err(CodexHttpV1Error::ZeroMaxOutputTokens);
        }
        self.max_output_tokens = Some(max_output_tokens);
        Ok(self)
    }

    /// Omits the optional Responses `context_management` field.
    ///
    /// The default retains the Codex server-side compaction configuration.
    /// Call this only for Responses-compatible endpoints that reject that
    /// Codex-specific request extension.
    pub fn without_context_management(mut self) -> Self {
        self.context_management = false;
        self
    }

    /// Includes a terminal status on canonical assistant messages.
    ///
    /// The default preserves the standard minimal Responses input shape. Use
    /// this for compatible endpoints which require replayed assistant items to
    /// explicitly distinguish completed and interrupted output.
    pub fn with_explicit_assistant_status(mut self) -> Self {
        self.explicit_assistant_status = true;
        self
    }
}

/// AgentView-owned request DTO suitable for a later BYOT transport adapter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexHttpV1Request {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    instructions: String,
    input: Vec<CodexInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CodexFunctionTool>>,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Option<CodexReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<Vec<ContextManagement>>,
    store: bool,
    stream: bool,
    include: Vec<IncludedField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CodexHttpV1WireRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "str::is_empty")]
    instructions: &'a str,
    input: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [CodexFunctionTool]>,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Option<&'a CodexReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<&'a [ContextManagement]>,
    store: bool,
    stream: bool,
    include: &'a [IncludedField],
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

impl<'a> CodexHttpV1WireRequest<'a> {
    fn with_input_and_instructions(
        request: &'a CodexHttpV1Request,
        input: &'a [Value],
        instructions: &'a str,
    ) -> Self {
        Self {
            model: &request.model,
            max_output_tokens: request.max_output_tokens,
            instructions,
            input,
            tools: request.tools.as_deref(),
            tool_choice: request.tool_choice,
            parallel_tool_calls: request.parallel_tool_calls,
            reasoning: request.reasoning.as_ref(),
            context_management: request.context_management.as_deref(),
            store: request.store,
            stream: request.stream,
            include: &request.include,
            prompt_cache_key: request.prompt_cache_key.as_deref(),
        }
    }
}

impl CodexHttpV1Request {
    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn instructions(&self) -> &str {
        &self.instructions
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn canonical_input(&self) -> Result<Vec<Value>, CodexHttpV1Error> {
        self.input
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn encode_bounded(&self, limit: usize) -> Result<Vec<u8>, CodexHttpV1Error> {
        serialize_json_bounded(self, limit)
    }

    pub(crate) fn encode_with_input_and_instructions_bounded(
        &self,
        input: &[Value],
        instructions: &str,
        limit: usize,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        serialize_json_bounded(
            &CodexHttpV1WireRequest::with_input_and_instructions(self, input, instructions),
            limit,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct ContextManagement {
    #[serde(rename = "type")]
    kind: ContextManagementType,
    compact_threshold: u32,
}

impl ContextManagement {
    const fn server_side_compaction() -> Self {
        Self {
            kind: ContextManagementType::Compaction,
            compact_threshold: DEFAULT_COMPACTION_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ContextManagementType {
    Compaction,
}

/// Provider history after `codex-http-v1` capability checks and POM rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexHttpV1History {
    instructions: String,
    input: Vec<CodexInputItem>,
}

/// Fail-closed projection from canonical history to Codex Responses input.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexHttpV1HistoryPolicy;

impl HistoryPolicy for CodexHttpV1HistoryPolicy {
    type History = CodexHttpV1History;
    type Error = CodexHttpV1Error;

    fn project_history(
        &self,
        transcript: &CanonicalTranscript,
    ) -> Result<Self::History, Self::Error> {
        project_codex_items(
            transcript.items(),
            InterruptedTextEncoding::Reject,
            AssistantStatusEncoding::Omit,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptedTextEncoding {
    Reject,
    AssistantAndBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantStatusEncoding {
    Omit,
    Explicit,
}

impl AssistantStatusEncoding {
    const fn enabled(self) -> bool {
        matches!(self, Self::Explicit)
    }
}

fn project_codex_items(
    items: &[CanonicalInputItem],
    interrupted_text: InterruptedTextEncoding,
    assistant_status: AssistantStatusEncoding,
) -> Result<CodexHttpV1History, CodexHttpV1Error> {
    let mut instructions = None;
    let mut input = Vec::with_capacity(items.len());

    for item in items {
        let lowered = lower_codex_item(item, interrupted_text, assistant_status)?;
        if let Some(current) = lowered.instructions {
            if instructions.replace(current).is_some() {
                return Err(CodexHttpV1Error::MultipleSystemInstructions);
            }
        }
        input.extend(lowered.input);
    }

    Ok(CodexHttpV1History {
        instructions: instructions.unwrap_or_default(),
        input,
    })
}

struct LoweredCodexItem {
    instructions: Option<String>,
    input: Vec<CodexInputItem>,
}

fn lower_codex_item(
    item: &CanonicalInputItem,
    interrupted_text: InterruptedTextEncoding,
    assistant_status: AssistantStatusEncoding,
) -> Result<LoweredCodexItem, CodexHttpV1Error> {
    let mut instructions = None;
    let mut input = Vec::with_capacity(2);
    match item {
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            pom,
        } => instructions = Some(render_pom_document(pom)?),
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            pom,
        } => input.push(CodexInputItem::message(
            MessageRole::Developer,
            TextContent::InputText {
                text: render_pom_document(pom)?,
            },
        )),
        CanonicalInputItem::Message { role, pom } => match role {
            ConversationRole::User => input.push(CodexInputItem::message(
                MessageRole::User,
                TextContent::InputText {
                    text: render_pom_document(pom)?,
                },
            )),
            ConversationRole::Assistant => input.push(CodexInputItem::assistant_text(
                render_pom_document(pom)?,
                None,
                AssistantInputStatus::Completed,
                assistant_status,
            )),
        },
        CanonicalInputItem::AssistantText {
            text,
            phase,
            status: AssistantTextStatus::Sealed,
        } => input.push(CodexInputItem::assistant_text(
            text.clone(),
            *phase,
            AssistantInputStatus::Completed,
            assistant_status,
        )),
        CanonicalInputItem::AssistantText {
            text,
            phase,
            status: AssistantTextStatus::Interrupted,
        } => match interrupted_text {
            InterruptedTextEncoding::Reject => {
                return Err(CodexHttpV1Error::InterruptedAssistantTextUnsupported);
            }
            InterruptedTextEncoding::AssistantAndBoundary => {
                input.push(CodexInputItem::assistant_text(
                    text.clone(),
                    *phase,
                    AssistantInputStatus::Incomplete,
                    assistant_status,
                ));
                input.push(CodexInputItem::message(
                    MessageRole::User,
                    TextContent::InputText {
                        text: ASSISTANT_OUTPUT_INTERRUPTED_MARKER.to_owned(),
                    },
                ));
            }
        },
        CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } => {
            validate_function_name(name)?;
            input.push(CodexInputItem::FunctionCall {
                name: name.clone(),
                arguments: raw_arguments.clone(),
                call_id: call_id.clone(),
            });
        }
        CanonicalInputItem::ToolResult { call_id, content } => {
            input.push(CodexInputItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: content.clone(),
            });
        }
        CanonicalInputItem::ProviderExtension(extension) => {
            input.push(reasoning_input(extension)?);
        }
    }
    Ok(LoweredCodexItem {
        instructions,
        input,
    })
}

/// Native Frame lowering for one canonical item.
///
/// System state occupies the separate instructions lane and therefore yields
/// no input values. Ordinary items yield one value, except interrupted
/// assistant text, which yields the visible output and its boundary marker.
#[allow(dead_code)] // Wired into AsyncOpenAiResponsesProvider during Phase 6 migration.
pub(crate) struct CodexHttpV1LoweredItem {
    instructions: Option<String>,
    input: Vec<Value>,
}

impl CodexHttpV1LoweredItem {
    #[allow(dead_code)] // Wired into AsyncOpenAiResponsesProvider during Phase 6 migration.
    pub(crate) fn into_parts(self) -> (Option<String>, Vec<Value>) {
        (self.instructions, self.input)
    }
}

/// Builds and serializes one deterministic Codex HTTP request.
#[derive(Debug, Clone)]
pub struct CodexHttpV1Encoder {
    options: CodexHttpV1Options,
}

impl CodexHttpV1Encoder {
    pub const fn new(options: CodexHttpV1Options) -> Self {
        Self { options }
    }

    pub fn request(
        &self,
        transcript: &CanonicalTranscript,
    ) -> Result<CodexHttpV1Request, CodexHttpV1Error> {
        self.request_with_native_tool_names(transcript, &[])
    }

    pub(crate) fn request_with_native_tool_names(
        &self,
        transcript: &CanonicalTranscript,
        native_tool_names: &[String],
    ) -> Result<CodexHttpV1Request, CodexHttpV1Error> {
        let history = project_codex_items(
            transcript.items(),
            InterruptedTextEncoding::Reject,
            if self.options.explicit_assistant_status {
                AssistantStatusEncoding::Explicit
            } else {
                AssistantStatusEncoding::Omit
            },
        )?;
        let tools = if native_tool_names.is_empty() {
            self.options.tools.clone()
        } else {
            Some(native_function_tools(native_tool_names)?)
        };
        Ok(self.request_from_history(history, tools))
    }

    /// Lowers one already-validated Frame item without requiring its section to
    /// be a standalone canonical transcript.
    ///
    /// This matters for Delta sections, where a ToolResult may close a ToolCall
    /// held by the accepted checkpoint. Callers concatenate returned groups in
    /// `replay -> staged_inputs -> projection` order.
    #[allow(dead_code)] // Wired into AsyncOpenAiResponsesProvider during Phase 6 migration.
    pub(crate) fn lower_canonical_item(
        &self,
        item: &CanonicalInputItem,
    ) -> Result<CodexHttpV1LoweredItem, CodexHttpV1Error> {
        let lowered = lower_codex_item(
            item,
            InterruptedTextEncoding::AssistantAndBoundary,
            if self.options.explicit_assistant_status {
                AssistantStatusEncoding::Explicit
            } else {
                AssistantStatusEncoding::Omit
            },
        )?;
        let input = lowered
            .input
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CodexHttpV1LoweredItem {
            instructions: lowered.instructions,
            input,
        })
    }

    /// Encodes the exact native Frame wire candidate under its complete tool
    /// catalog. An empty catalog is serialized as `tools: []`; it never falls
    /// back to tools configured on the encoder.
    #[allow(dead_code)] // Wired into AsyncOpenAiResponsesProvider during Phase 6 migration.
    pub(crate) fn encode_frame_request_bounded(
        &self,
        input: &[Value],
        instructions: &str,
        exact_tool_names: &[String],
        limit: usize,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        let request = self.request_from_history(
            CodexHttpV1History {
                instructions: String::new(),
                input: Vec::new(),
            },
            Some(native_function_tools(exact_tool_names)?),
        );
        request.encode_with_input_and_instructions_bounded(input, instructions, limit)
    }

    fn request_from_history(
        &self,
        history: CodexHttpV1History,
        tools: Option<Vec<CodexFunctionTool>>,
    ) -> CodexHttpV1Request {
        CodexHttpV1Request {
            model: self.options.model.clone(),
            max_output_tokens: self.options.max_output_tokens,
            instructions: history.instructions,
            input: history.input,
            tools,
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: false,
            reasoning: self.options.reasoning,
            context_management: self
                .options
                .context_management
                .then(|| vec![ContextManagement::server_side_compaction()]),
            store: false,
            stream: true,
            include: vec![IncludedField::ReasoningEncryptedContent],
            prompt_cache_key: self.options.prompt_cache_key.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn encode_request_with_input_and_instructions(
        &self,
        transcript: &CanonicalTranscript,
        input: &[serde_json::Value],
        instructions: &str,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        let typed_request = self.request(transcript)?;
        let request = CodexHttpV1WireRequest::with_input_and_instructions(
            &typed_request,
            input,
            instructions,
        );
        Ok(serde_json::to_vec(&request)?)
    }
}

fn native_function_tools(
    native_tool_names: &[String],
) -> Result<Vec<CodexFunctionTool>, CodexHttpV1Error> {
    native_tool_names
        .iter()
        .map(|name| {
            CodexFunctionTool::new(
                name.clone(),
                "AgentView native tool",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": true,
                }),
                false,
            )
        })
        .collect()
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
    limit_exceeded: bool,
}

impl BoundedJsonWriter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            limit_exceeded: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.bytes.len()
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    fn limit_exceeded(&self) -> bool {
        self.limit_exceeded
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other(
                "serialized JSON exceeded configured limit",
            ));
        };
        if next_len > self.limit {
            self.limit_exceeded = true;
            return Err(io::Error::other(
                "serialized JSON exceeded configured limit",
            ));
        }
        if next_len > self.bytes.capacity() {
            let target_capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(next_len)
                .max(64.min(self.limit))
                .min(self.limit);
            self.bytes
                .try_reserve_exact(target_capacity - self.bytes.len())
                .map_err(|_| io::Error::other("serialized JSON buffer allocation failed"))?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_json_bounded<T: Serialize>(
    value: &T,
    limit: usize,
) -> Result<Vec<u8>, CodexHttpV1Error> {
    let mut writer = BoundedJsonWriter::new(limit);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.into_inner()),
        Err(_) if writer.limit_exceeded() => Err(CodexHttpV1Error::SerializedRequestBodyLimit),
        Err(error) => Err(CodexHttpV1Error::Serialize(error)),
    }
}

impl ProviderRequestEncoder for CodexHttpV1Encoder {
    type Error = CodexHttpV1Error;

    fn encode_request(&self, transcript: &CanonicalTranscript) -> Result<Vec<u8>, Self::Error> {
        Ok(serde_json::to_vec(&self.request(transcript)?)?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexInputItem {
    Message {
        role: MessageRole,
        content: Vec<TextContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<AssistantPhase>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<AssistantInputStatus>,
    },
    Reasoning {
        summary: Vec<ReasoningSummaryText>,
        content: Value,
        encrypted_content: String,
    },
    FunctionCall {
        name: String,
        arguments: String,
        call_id: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

impl CodexInputItem {
    fn message(role: MessageRole, content: TextContent) -> Self {
        Self::Message {
            role,
            content: vec![content],
            phase: None,
            status: None,
        }
    }

    fn assistant_text(
        text: String,
        phase: Option<AssistantPhase>,
        status: AssistantInputStatus,
        status_encoding: AssistantStatusEncoding,
    ) -> Self {
        Self::Message {
            role: MessageRole::Assistant,
            content: vec![TextContent::OutputText { text }],
            phase,
            status: status_encoding.enabled().then_some(status),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AssistantInputStatus {
    Completed,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum MessageRole {
    Developer,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TextContent {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasoningExtensionPayload {
    summary: Vec<ReasoningSummaryText>,
    content: Value,
    encrypted_content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasoningSummaryText {
    #[serde(rename = "type")]
    kind: SummaryTextKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SummaryTextKind {
    SummaryText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ToolChoice {
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum IncludedField {
    #[serde(rename = "reasoning.encrypted_content")]
    ReasoningEncryptedContent,
}

fn reasoning_input(extension: &ProviderExtension) -> Result<CodexInputItem, CodexHttpV1Error> {
    if extension.provider() != OPENAI_PROVIDER
        || extension.capability() != REASONING_CAPABILITY
        || extension.schema_version() != REASONING_SCHEMA_VERSION
    {
        return Err(CodexHttpV1Error::UnsupportedProviderExtension {
            provider: extension.provider().to_owned(),
            capability: extension.capability().to_owned(),
            schema_version: extension.schema_version(),
        });
    }

    let payload: ReasoningExtensionPayload = serde_json::from_value(extension.payload().clone())
        .map_err(|error| CodexHttpV1Error::InvalidProviderExtensionPayload {
            message: error.to_string(),
        })?;
    if !payload.content.is_null() || payload.encrypted_content.is_empty() {
        return Err(CodexHttpV1Error::InvalidProviderExtensionPayload {
            message: "content must be null and encrypted_content must be non-empty".to_owned(),
        });
    }

    Ok(CodexInputItem::Reasoning {
        summary: payload.summary,
        content: payload.content,
        encrypted_content: payload.encrypted_content,
    })
}

fn duplicate_tool_name(tools: &[CodexFunctionTool]) -> Option<String> {
    let mut names = HashSet::with_capacity(tools.len());
    tools
        .iter()
        .find_map(|tool| (!names.insert(tool.name.as_str())).then(|| tool.name.clone()))
}

fn validate_function_name(name: &str) -> Result<(), CodexHttpV1Error> {
    let valid = !name.is_empty()
        && name.len() <= FUNCTION_NAME_MAX_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(CodexHttpV1Error::InvalidToolName {
            name: name.to_owned(),
        })
    }
}

/// Invalid profile input, history projection, or deterministic serialization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodexHttpV1Error {
    #[error("Codex model must be non-empty")]
    EmptyModel,
    #[error(
        "invalid Codex function tool name `{name}`; expected 1-128 ASCII [A-Za-z0-9_-] characters"
    )]
    InvalidToolName { name: String },
    #[error("Codex function tool `{name}` parameters must be a JSON object")]
    ToolParametersMustBeObject { name: String },
    #[error("Codex function tool `{name}` is declared more than once")]
    DuplicateToolName { name: String },
    #[error("Codex prompt cache key must be non-empty when present")]
    EmptyPromptCacheKey,
    #[error("Codex max output tokens must be positive when configured")]
    ZeroMaxOutputTokens,
    #[error("codex-http-v1 accepts at most one System instruction")]
    MultipleSystemInstructions,
    #[error("legacy codex-http-v1 history cannot encode interrupted assistant text")]
    InterruptedAssistantTextUnsupported,
    #[error(
        "codex-http-v1 does not accept provider extension {provider}/{capability}@{schema_version}"
    )]
    UnsupportedProviderExtension {
        provider: String,
        capability: String,
        schema_version: u32,
    },
    #[error("invalid openai reasoning.encrypted_content payload: {message}")]
    InvalidProviderExtensionPayload { message: String },
    #[error("Codex serialized request body exceeded configured limit")]
    SerializedRequestBodyLimit,
    #[error(transparent)]
    PomRender(#[from] PomRenderError),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pom::{Document, TextNode, XmlNode},
        pom_resolution::resolve_artifact_document,
    };
    use serde_json::json;

    fn xml_document(name: &str, text: &str) -> crate::pom::ResolvedDocument {
        let node = XmlNode::try_build(name, |children| {
            children.text(TextNode::new(text));
            Ok(())
        })
        .unwrap();
        resolve_artifact_document(Document::from_xml(node)).unwrap()
    }

    #[test]
    fn retained_input_encoder_preserves_wire_order_and_non_input_options() {
        let options = CodexHttpV1Options::new(
            "gpt-5.6-codex",
            None,
            Some(CodexReasoning::max_detailed()),
            Some("retained-session"),
        )
        .unwrap();
        let input = vec![json!({
            "type": "compaction",
            "id": "cmp_7",
            "encrypted_content": "opaque"
        })];
        let transcript = [
            CanonicalInputItem::instruction(
                InstructionAuthority::System,
                xml_document("system", "Retain these instructions."),
            ),
            CanonicalInputItem::instruction(
                InstructionAuthority::Developer,
                xml_document("developer", "Replace this typed input."),
            ),
        ]
        .into_iter()
        .try_fold(CanonicalTranscript::new(), |transcript, item| {
            transcript.appended(item)
        })
        .unwrap();

        let encoded = CodexHttpV1Encoder::new(options)
            .encode_request_with_input_and_instructions(
                &transcript,
                &input,
                "<system>Retain these instructions.</system>",
            )
            .unwrap();

        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"model":"gpt-5.6-codex","instructions":"<system>Retain these instructions.</system>","input":[{"encrypted_content":"opaque","id":"cmp_7","type":"compaction"}],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":{"effort":"max","summary":"detailed"},"context_management":[{"type":"compaction","compact_threshold":200000}],"store":false,"stream":true,"include":["reasoning.encrypted_content"],"prompt_cache_key":"retained-session"}"#
        );
    }

    #[test]
    fn max_output_tokens_is_optional_in_public_and_native_requests() {
        let default_encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap(),
        );
        let default_public = default_encoder
            .request(&CanonicalTranscript::new())
            .unwrap();
        let default_public: Value = serde_json::to_value(default_public).unwrap();
        assert!(default_public.get("max_output_tokens").is_none());
        let default_native = default_encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let default_native: Value = serde_json::from_slice(&default_native).unwrap();
        assert!(default_native.get("max_output_tokens").is_none());

        let configured_encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)
                .unwrap()
                .with_max_output_tokens(1_024)
                .unwrap(),
        );
        let configured_public = configured_encoder
            .request(&CanonicalTranscript::new())
            .unwrap();
        let configured_public: Value = serde_json::to_value(configured_public).unwrap();
        assert_eq!(configured_public["max_output_tokens"], json!(1_024));
        let configured_native = configured_encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let configured_native: Value = serde_json::from_slice(&configured_native).unwrap();
        assert_eq!(configured_native["max_output_tokens"], json!(1_024));
    }

    #[test]
    fn max_output_tokens_rejects_zero() {
        assert!(matches!(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)
                .unwrap()
                .with_max_output_tokens(0),
            Err(CodexHttpV1Error::ZeroMaxOutputTokens)
        ));
    }

    #[test]
    fn context_management_defaults_to_compaction_and_can_be_omitted() {
        let default_encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap(),
        );
        let default_public: Value = serde_json::to_value(
            default_encoder
                .request(&CanonicalTranscript::new())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            default_public["context_management"],
            json!([{"type": "compaction", "compact_threshold": 200_000}])
        );
        let default_native = default_encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let default_native: Value = serde_json::from_slice(&default_native).unwrap();
        assert_eq!(
            default_native["context_management"],
            json!([{"type": "compaction", "compact_threshold": 200_000}])
        );

        let disabled_encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)
                .unwrap()
                .without_context_management(),
        );
        let disabled_public: Value = serde_json::to_value(
            disabled_encoder
                .request(&CanonicalTranscript::new())
                .unwrap(),
        )
        .unwrap();
        assert!(disabled_public.get("context_management").is_none());
        let disabled_native = disabled_encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let disabled_native: Value = serde_json::from_slice(&disabled_native).unwrap();
        assert!(disabled_native.get("context_management").is_none());
    }

    #[test]
    fn explicit_assistant_status_is_opt_in_and_preserves_interruption_semantics() {
        let assistant_history = CanonicalTranscript::new()
            .appended(CanonicalInputItem::message(
                ConversationRole::Assistant,
                xml_document("say", "completed history"),
            ))
            .unwrap();
        let default = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap(),
        );
        let default_body: Value =
            serde_json::to_value(default.request(&assistant_history).unwrap()).unwrap();
        assert!(default_body["input"][0].get("status").is_none());

        let explicit = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)
                .unwrap()
                .with_explicit_assistant_status(),
        );
        let explicit_body: Value =
            serde_json::to_value(explicit.request(&assistant_history).unwrap()).unwrap();
        assert_eq!(explicit_body["input"][0]["status"], json!("completed"));

        let interrupted = CanonicalInputItem::interrupted_assistant_text("partial", None);
        let (_, input) = explicit
            .lower_canonical_item(&interrupted)
            .unwrap()
            .into_parts();
        assert_eq!(input[0]["status"], json!("incomplete"));
        assert_eq!(input[0]["content"][0]["text"], json!("partial"));
        assert_eq!(input[1]["role"], json!("user"));
        assert_eq!(
            input[1]["content"][0]["text"],
            json!(ASSISTANT_OUTPUT_INTERRUPTED_MARKER)
        );
    }

    #[test]
    fn native_frame_items_expand_interrupted_text_and_accept_delta_tool_result() {
        let encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                "gpt-5.6-codex",
                Some(vec![
                    CodexFunctionTool::new("configured_tool", "old", json!({}), false).unwrap(),
                ]),
                None,
                None::<String>,
            )
            .unwrap(),
        );
        let interrupted = CanonicalInputItem::interrupted_assistant_text(
            "visible partial",
            Some(AssistantPhase::Commentary),
        );
        let tool_result =
            CanonicalInputItem::tool_result("call-from-checkpoint", "tool result").unwrap();
        let (instructions, mut input) = encoder
            .lower_canonical_item(&interrupted)
            .unwrap()
            .into_parts();
        let (tool_instructions, tool_input) = encoder
            .lower_canonical_item(&tool_result)
            .unwrap()
            .into_parts();
        input.extend(tool_input);

        assert_eq!(instructions, None);
        assert_eq!(tool_instructions, None);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["phase"], "commentary");
        assert_eq!(input[0]["content"][0]["text"], "visible partial");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(
            input[1]["content"][0]["text"],
            ASSISTANT_OUTPUT_INTERRUPTED_MARKER
        );
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call-from-checkpoint");
        assert_eq!(input[2]["output"], "tool result");

        let body = encoder
            .encode_frame_request_bounded(&input, "", &[], usize::MAX)
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["tools"], json!([]));
        assert!(!body.to_string().contains("configured_tool"));
    }

    #[test]
    fn legacy_history_policy_keeps_interrupted_text_fail_closed() {
        let transcript = CanonicalTranscript::new()
            .appended(CanonicalInputItem::interrupted_assistant_text(
                "visible partial",
                None,
            ))
            .unwrap();

        assert!(matches!(
            CodexHttpV1HistoryPolicy.project_history(&transcript),
            Err(CodexHttpV1Error::InterruptedAssistantTextUnsupported)
        ));
    }

    #[test]
    fn native_frame_request_uses_system_lane_and_inclusive_wire_limit() {
        let encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap(),
        );
        let system = CanonicalInputItem::instruction(
            InstructionAuthority::System,
            xml_document("system", "Current instructions."),
        );
        let (instructions, input) = encoder.lower_canonical_item(&system).unwrap().into_parts();
        let instructions = instructions.unwrap();

        assert!(input.is_empty());
        let body = encoder
            .encode_frame_request_bounded(&input, &instructions, &[], usize::MAX)
            .unwrap();
        assert_eq!(
            encoder
                .encode_frame_request_bounded(&input, &instructions, &[], body.len())
                .unwrap(),
            body
        );
        assert!(matches!(
            encoder.encode_frame_request_bounded(&input, &instructions, &[], body.len() - 1,),
            Err(CodexHttpV1Error::SerializedRequestBodyLimit)
        ));
    }

    #[test]
    fn bounded_json_writer_preserves_exact_bytes_without_allocating_past_limit() {
        let value = json!({"payload": "\\\"".repeat(1024)});
        let expected = serde_json::to_vec(&value).unwrap();

        let mut exact = BoundedJsonWriter::new(expected.len());
        serde_json::to_writer(&mut exact, &value).unwrap();
        assert_eq!(exact.len(), expected.len());
        assert!(exact.capacity() <= expected.len());
        assert_eq!(exact.into_inner(), expected);

        let limit = expected.len() - 1;
        let mut rejected = BoundedJsonWriter::new(limit);
        assert!(serde_json::to_writer(&mut rejected, &value).is_err());
        assert!(rejected.limit_exceeded());
        assert!(rejected.len() <= limit);
        assert!(rejected.capacity() <= limit);
    }
}
