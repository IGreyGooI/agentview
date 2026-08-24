//! Deterministic OpenAI Responses JSON for the `codex-http-v1` profile.
//!
//! This module owns only history policy and request DTO serialization. HTTP,
//! SSE, retry, and `async-openai` integration are intentionally separate.

use std::{
    collections::HashSet,
    io::{self, Write},
};

use crate::{
    pom_renderer::{render_pom_document, PomRenderError},
    transcript::{
        AssistantPhase, CanonicalInputItem, CanonicalTranscript, ConversationRole,
        InstructionAuthority, ProviderExtension,
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
        })
    }
}

/// AgentView-owned request DTO suitable for a later BYOT transport adapter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexHttpV1Request {
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    instructions: String,
    input: Vec<CodexInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CodexFunctionTool>>,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Option<CodexReasoning>,
    context_management: Vec<ContextManagement>,
    store: bool,
    stream: bool,
    include: Vec<IncludedField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CodexHttpV1WireRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    instructions: &'a str,
    input: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [CodexFunctionTool]>,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Option<&'a CodexReasoning>,
    context_management: &'a [ContextManagement],
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
            instructions,
            input,
            tools: request.tools.as_deref(),
            tool_choice: request.tool_choice,
            parallel_tool_calls: request.parallel_tool_calls,
            reasoning: request.reasoning.as_ref(),
            context_management: &request.context_management,
            store: request.store,
            stream: request.stream,
            include: &request.include,
            prompt_cache_key: request.prompt_cache_key.as_deref(),
        }
    }
}

impl CodexHttpV1Request {
    pub(crate) fn instructions(&self) -> &str {
        &self.instructions
    }

    pub(crate) fn canonical_input(&self) -> Result<Vec<Value>, CodexHttpV1Error> {
        self.input
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

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
        let mut instructions = None;
        let mut input = Vec::with_capacity(transcript.items().len());

        for item in transcript.items() {
            match item {
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::System,
                    pom,
                } => {
                    if instructions.is_some() {
                        return Err(CodexHttpV1Error::MultipleSystemInstructions);
                    }
                    instructions = Some(render_pom_document(pom)?);
                }
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::Developer,
                    pom,
                } => input.push(CodexInputItem::message(
                    MessageRole::Developer,
                    TextContent::InputText {
                        text: render_pom_document(pom)?,
                    },
                )),
                CanonicalInputItem::Message { role, pom } => {
                    let (role, content) = match role {
                        ConversationRole::User => (
                            MessageRole::User,
                            TextContent::InputText {
                                text: render_pom_document(pom)?,
                            },
                        ),
                        ConversationRole::Assistant => (
                            MessageRole::Assistant,
                            TextContent::OutputText {
                                text: render_pom_document(pom)?,
                            },
                        ),
                    };
                    input.push(CodexInputItem::message(role, content));
                }
                CanonicalInputItem::AssistantText { text, phase } => {
                    input.push(CodexInputItem::assistant_text(text.clone(), *phase));
                }
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
        }

        Ok(CodexHttpV1History {
            instructions: instructions.unwrap_or_default(),
            input,
        })
    }
}

/// Builds and serializes one deterministic Codex HTTP request.
#[derive(Debug, Clone)]
pub struct CodexHttpV1Encoder {
    options: CodexHttpV1Options,
    history_policy: CodexHttpV1HistoryPolicy,
}

impl CodexHttpV1Encoder {
    pub const fn new(options: CodexHttpV1Options) -> Self {
        Self {
            options,
            history_policy: CodexHttpV1HistoryPolicy,
        }
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
        let history = self.history_policy.project_history(transcript)?;
        let tools = if native_tool_names.is_empty() {
            self.options.tools.clone()
        } else {
            Some(
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
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        Ok(CodexHttpV1Request {
            model: self.options.model.clone(),
            instructions: history.instructions,
            input: history.input,
            tools,
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: false,
            reasoning: self.options.reasoning,
            context_management: vec![ContextManagement::server_side_compaction()],
            store: false,
            stream: true,
            include: vec![IncludedField::ReasoningEncryptedContent],
            prompt_cache_key: self.options.prompt_cache_key.clone(),
        })
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
        }
    }

    fn assistant_text(text: String, phase: Option<AssistantPhase>) -> Self {
        Self::Message {
            role: MessageRole::Assistant,
            content: vec![TextContent::OutputText { text }],
            phase,
        }
    }
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
    #[error("codex-http-v1 accepts at most one System instruction")]
    MultipleSystemInstructions,
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
