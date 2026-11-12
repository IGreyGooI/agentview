//! Deterministic OpenAI Responses JSON for the `codex-http-v1` profile.
//!
//! This module owns only history policy and request DTO serialization. HTTP,
//! SSE, retry, and `async-openai` integration are intentionally separate.

use std::{
    borrow::Cow,
    collections::HashSet,
    io::{self, Write},
};

use crate::{
    component::execution::{DecisionDeclaration, DecisionQuestion, ToolDefinition},
    pom_renderer::{render_pom_document, PomRenderError},
    transcript::{
        AssistantPhase, AssistantTextStatus, CanonicalInputItem, CanonicalTranscript,
        ConversationRole, InstructionAuthority, ProviderExtension,
        ASSISTANT_OUTPUT_INTERRUPTED_MARKER,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{HistoryPolicy, ModelSpec, ProviderRequestEncoder};

pub const CODEX_HTTP_V1_PROFILE: &str = "codex-http-v1";
pub const CODEX_HTTP_V1_CODEX_REVISION: &str = "4f1992732c832fe125608980a03ec2b66710c4e4";

const OPENAI_PROVIDER: &str = "openai";
const REASONING_CAPABILITY: &str = "reasoning.encrypted_content";
const REASONING_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DecisionTextOptions {
    format: DecisionJsonFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DecisionJsonFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    schema: Value,
    strict: bool,
}

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

/// Typed request-side reasoning controls for Responses-compatible endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodexReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<ReasoningSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

impl CodexReasoning {
    pub const fn max_detailed() -> Self {
        Self {
            effort: Some(ReasoningEffort::Max),
            summary: Some(ReasoningSummary::Detailed),
            enabled: None,
        }
    }

    /// Disables reasoning on endpoints supporting OpenRouter's `enabled` control.
    pub const fn disabled() -> Self {
        Self {
            effort: None,
            summary: None,
            enabled: Some(false),
        }
    }

    /// Disables reasoning on models supporting the Responses `none` effort.
    pub const fn none() -> Self {
        Self {
            effort: Some(ReasoningEffort::None),
            summary: None,
            enabled: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningEffort {
    Max,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningSummary {
    Detailed,
}

/// OpenRouter endpoint restrictions, preference order, and parameter support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenRouterProviderRouting {
    only: Vec<String>,
    order: Vec<String>,
    require_parameters: bool,
}

impl OpenRouterProviderRouting {
    /// Restricts eligible endpoints to `only`, preferring those in `order`.
    /// Endpoint slugs are passed through without rewriting variant suffixes.
    pub fn new(
        only: Vec<String>,
        order: Vec<String>,
        require_parameters: bool,
    ) -> Result<Self, CodexHttpV1Error> {
        if only.is_empty()
            || only.iter().chain(&order).any(|slug| {
                slug.is_empty()
                    || slug
                        .chars()
                        .any(|character| character.is_whitespace() || character.is_control())
            })
        {
            return Err(CodexHttpV1Error::InvalidOpenRouterProviderRouting);
        }
        Ok(Self {
            only,
            order,
            require_parameters,
        })
    }
}

/// Immutable inputs that vary between `codex-http-v1` operations.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexHttpV1Options {
    model: ModelSpec,
    tools: Option<Vec<CodexFunctionTool>>,
    reasoning: Option<CodexReasoning>,
    openrouter_provider_routing: Option<OpenRouterProviderRouting>,
    prompt_cache_key: Option<String>,
    max_output_tokens: Option<u32>,
    temperature: Option<f64>,
    explicit_assistant_status: bool,
    parallel_tool_calls: bool,
    prompt_cache_comparison_response_id: Option<String>,
    ark_caching: bool,
    decision_output: Option<DecisionTextOptions>,
}

impl CodexHttpV1Options {
    /// Configures requests for a model with an explicit context capacity.
    /// Model identity and capacity are validated together by [`ModelSpec::new`].
    pub fn new(
        model: ModelSpec,
        tools: Option<Vec<CodexFunctionTool>>,
        reasoning: Option<CodexReasoning>,
        prompt_cache_key: Option<impl Into<String>>,
    ) -> Result<Self, CodexHttpV1Error> {
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
            openrouter_provider_routing: None,
            prompt_cache_key,
            max_output_tokens: None,
            temperature: None,
            explicit_assistant_status: false,
            parallel_tool_calls: false,
            prompt_cache_comparison_response_id: None,
            ark_caching: false,
            decision_output: None,
        })
    }

    /// The model identity and capacity shared by all requests using these options.
    pub const fn model(&self) -> &ModelSpec {
        &self.model
    }

    /// Applies the same OpenRouter endpoint routing to foreground and summaries.
    pub fn with_openrouter_provider_routing(mut self, routing: OpenRouterProviderRouting) -> Self {
        self.openrouter_provider_routing = Some(routing);
        self
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

    /// Sets sampling temperature for endpoints and models supporting it.
    /// The default omits this optional field for reasoning-model compatibility.
    pub fn with_temperature(mut self, temperature: f64) -> Result<Self, CodexHttpV1Error> {
        if !(0.0..=2.0).contains(&temperature) {
            return Err(CodexHttpV1Error::InvalidTemperature);
        }
        self.temperature = Some(temperature);
        Ok(self)
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

    /// Allows multiple native calls in one model response. Tool admission and
    /// execution remain owned by the Application.
    pub fn with_parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.parallel_tool_calls = enabled;
        self
    }

    /// Enables Volcengine Ark Session caching on the native Responses port.
    ///
    /// Disabled by default. When enabled, the port stores responses remotely
    /// and continues compatible turns with only their new input. Ark receives
    /// System instructions as a first-turn input message, and inherits tools
    /// on continued turns. Ephemeral input, changed tools, and recovery can
    /// require a fresh request. This is an Ark extension, not an OpenAI prompt
    /// cache hint; enable it only for an endpoint and model that support it.
    /// Legacy ProviderPort submission does not support this mode.
    pub fn with_ark_caching(mut self, enabled: bool) -> Self {
        self.ark_caching = enabled;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CodexPromptCacheOptions {
    comparison_response_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ArkCaching {
    Enabled,
}

/// AgentView-owned request DTO suitable for a later BYOT transport adapter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexHttpV1Request {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(rename = "text", skip_serializing_if = "Option::is_none")]
    decision_output: Option<DecisionTextOptions>,
    #[serde(skip_serializing_if = "String::is_empty")]
    instructions: String,
    input: Vec<CodexInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CodexFunctionTool>>,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Option<CodexReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<OpenRouterProviderRouting>,
    store: bool,
    stream: bool,
    include: Vec<IncludedField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<CodexPromptCacheOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caching: Option<ArkCaching>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CodexHttpV1WireRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(rename = "text", skip_serializing_if = "Option::is_none")]
    decision_output: Option<&'a DecisionTextOptions>,
    #[serde(skip_serializing_if = "str::is_empty")]
    instructions: &'a str,
    input: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [CodexFunctionTool]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    reasoning: Option<&'a CodexReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'a OpenRouterProviderRouting>,
    store: bool,
    stream: bool,
    include: &'a [IncludedField],
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<&'a CodexPromptCacheOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caching: Option<ArkCaching>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
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
            temperature: request.temperature,
            decision_output: request.decision_output.as_ref(),
            instructions,
            input,
            tools: request.tools.as_deref(),
            tool_choice: Some(request.tool_choice),
            parallel_tool_calls: Some(request.parallel_tool_calls),
            reasoning: request.reasoning.as_ref(),
            provider: request.provider.as_ref(),
            store: request.store,
            stream: request.stream,
            include: &request.include,
            prompt_cache_key: request.prompt_cache_key.as_deref(),
            prompt_cache_options: request.prompt_cache_options.as_ref(),
            caching: request.caching,
            previous_response_id: request.previous_response_id.as_deref(),
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
        CanonicalInputItem::DecisionRequest { declaration } => {
            let request = serde_json::json!({
                "questions": declaration.questions(),
                "answer_instructions": "Answer every declared question independently against the observed application state. Return an answers object keyed by question name, with each answer using its declared type and fields. Noul is the estimated probability of yes; choice selects a declared label; score is a probability-weighted zero-based position on the declared scale. Preserve probability values as numbers and use type refusal when a question cannot be answered. Any probability or confidence you supply is a model estimate. Return answers directly without tools or confirmation.",
            });
            input.push(CodexInputItem::message(
                MessageRole::User,
                TextContent::InputText {
                    text: request.to_string(),
                },
            ));
        }
        CanonicalInputItem::DecisionOutput { decision } => {
            input.push(CodexInputItem::assistant_text(
                decision
                    .raw_json()
                    .or_else(|| decision.refusal())
                    .unwrap_or_default()
                    .to_owned(),
                None,
                AssistantInputStatus::Completed,
                assistant_status,
            ));
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

    pub(crate) const fn model(&self) -> &ModelSpec {
        self.options.model()
    }

    pub(crate) const fn ark_caching_enabled(&self) -> bool {
        self.options.ark_caching
    }

    pub(crate) const fn parallel_tool_calls_enabled(&self) -> bool {
        self.options.parallel_tool_calls
    }

    /// Builds an isolated encoder for requests that cannot retain remote input.
    pub(crate) fn without_ark_caching(&self) -> Self {
        Self::new(self.options.clone().with_ark_caching(false))
    }

    /// Derives request-local structured output from the submitted Frame.
    /// The configured encoder never retains a previous judgment's contract.
    pub(crate) fn for_decision(&self, declaration: &DecisionDeclaration) -> Self {
        let mut encoder = self.without_ark_caching();
        encoder.options.tools = None;
        encoder.options.parallel_tool_calls = false;
        encoder.options.prompt_cache_comparison_response_id = None;
        encoder.options.decision_output = Some(DecisionTextOptions {
            format: DecisionJsonFormat {
                kind: "json_schema",
                name: "agentview_decision",
                schema: decision_envelope_schema(declaration),
                // Optional native answer metadata is checked by canonical
                // admission; do not require endpoint-specific strict schemas.
                strict: false,
            },
        });
        encoder
    }

    pub(crate) fn set_parallel_tool_calls(&mut self, enabled: bool) {
        self.options.parallel_tool_calls = enabled;
    }

    pub(crate) fn set_prompt_cache_comparison_response_id(&mut self, id: Option<String>) {
        self.options.prompt_cache_comparison_response_id = id;
    }

    /// An ordinary, isolated model request. Summarization never enables API
    /// compaction, inherits business tools, or continues the foreground response.
    pub(crate) fn encode_summary_request_bounded(
        &self,
        input: &[Value],
        instructions: &str,
        max_output_tokens: u64,
        limit: usize,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        #[derive(Serialize)]
        struct HistoricalContext<'a> {
            kind: &'static str,
            items: &'a [Value],
        }
        // The foreground window contains live developer instructions and tool
        // calls. Replaying their roles in the independent summary request can
        // make it continue the business response instead of summarizing it.
        // Quote the entire wire prefix as data in one user message: roles and
        // call IDs remain data, without parsing or rewriting rendered POM.
        // Bound both encodings; a limit/encoding error abandons only this
        // summary attempt and leaves the foreground window available.
        let history = serialize_json_bounded(
            &HistoricalContext {
                kind: "historical_context",
                items: input,
            },
            limit,
        )?;
        let history = String::from_utf8(history).map_err(|error| {
            CodexHttpV1Error::Serialize(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                error,
            )))
        })?;
        let quoted_input = [serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": history}],
        })];
        #[derive(Serialize)]
        struct Request<'a> {
            model: &'a str,
            input: &'a [Value],
            instructions: &'a str,
            tools: [(); 0],
            tool_choice: &'static str,
            stream: bool,
            store: bool,
            max_output_tokens: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            reasoning: Option<CodexReasoning>,
            #[serde(skip_serializing_if = "Option::is_none")]
            provider: Option<&'a OpenRouterProviderRouting>,
        }
        serialize_json_bounded(
            &Request {
                model: self.model().id(),
                input: &quoted_input,
                instructions,
                tools: [],
                tool_choice: "none",
                stream: false,
                store: false,
                max_output_tokens,
                reasoning: self.options.reasoning,
                provider: self.options.openrouter_provider_routing.as_ref(),
            },
            limit,
        )
    }

    pub fn request(
        &self,
        transcript: &CanonicalTranscript,
    ) -> Result<CodexHttpV1Request, CodexHttpV1Error> {
        self.request_with_native_tools(transcript, &[])
    }

    #[allow(dead_code)] // Retained for legacy name-only callers.
    pub(crate) fn request_with_native_tool_names(
        &self,
        transcript: &CanonicalTranscript,
        native_tool_names: &[String],
    ) -> Result<CodexHttpV1Request, CodexHttpV1Error> {
        let native_tools = native_tool_names
            .iter()
            .map(|name| {
                if name.is_empty() {
                    return Err(CodexHttpV1Error::InvalidToolName { name: name.clone() });
                }
                Ok(ToolDefinition::legacy_name_only(name.clone())
                    .expect("a non-empty compatibility native tool name is valid"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.request_with_native_tools(transcript, &native_tools)
    }

    /// Builds a request from complete Component-native tool definitions.
    pub(crate) fn request_with_native_tools(
        &self,
        transcript: &CanonicalTranscript,
        native_tools: &[ToolDefinition],
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
        let tools = if native_tools.is_empty() {
            self.options.tools.clone()
        } else {
            Some(native_function_tools(native_tools)?)
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
        exact_tools: &[ToolDefinition],
        limit: usize,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        self.encode_frame_request_with_previous_response_id_bounded(
            input,
            instructions,
            exact_tools,
            None,
            limit,
        )
    }

    /// Encodes one native request, inheriting Ark's remote prefix when present.
    /// The caller proves that the response covers exactly the omitted history
    /// and that the System snapshot and tool catalog are unchanged.
    pub(crate) fn encode_frame_request_with_previous_response_id_bounded(
        &self,
        input: &[Value],
        instructions: &str,
        exact_tools: &[ToolDefinition],
        previous_response_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        let mut request = self.request_from_history(
            CodexHttpV1History {
                instructions: String::new(),
                input: Vec::new(),
            },
            Some(native_function_tools(exact_tools)?),
        );
        if !self.ark_caching_enabled() {
            return request.encode_with_input_and_instructions_bounded(input, instructions, limit);
        }

        request.store = true;
        request.caching = Some(ArkCaching::Enabled);
        let input = if previous_response_id.is_none() && !instructions.is_empty() {
            let mut values = Vec::with_capacity(input.len() + 1);
            values.push(serde_json::json!({
                "type": "message",
                "role": "system",
                "content": [{"type": "input_text", "text": instructions}],
            }));
            values.extend_from_slice(input);
            Cow::Owned(values)
        } else {
            Cow::Borrowed(input)
        };
        let mut wire = CodexHttpV1WireRequest::with_input_and_instructions(&request, &input, "");
        wire.previous_response_id = previous_response_id;
        if previous_response_id.is_some() {
            wire.tools = None;
            wire.tool_choice = None;
            wire.parallel_tool_calls = None;
        }
        serialize_json_bounded(&wire, limit)
    }

    /// Continues a stored parent without saving the temporary child response.
    /// The parent already supplies System instructions and the tool catalog.
    pub(crate) fn encode_temporary_frame_request_bounded(
        &self,
        input: &[Value],
        exact_tools: &[ToolDefinition],
        previous_response_id: &str,
        limit: usize,
    ) -> Result<Vec<u8>, CodexHttpV1Error> {
        let request = self.request_from_history(
            CodexHttpV1History {
                instructions: String::new(),
                input: Vec::new(),
            },
            Some(native_function_tools(exact_tools)?),
        );
        let mut wire = CodexHttpV1WireRequest::with_input_and_instructions(&request, input, "");
        wire.previous_response_id = Some(previous_response_id);
        wire.tools = None;
        wire.tool_choice = None;
        wire.parallel_tool_calls = None;
        serialize_json_bounded(&wire, limit)
    }

    /// Measures the complete effective context, independent of upload limits.
    /// Cached turns still consume the model's context window even when the
    /// actual request contains only the suffix after a remote response.
    pub(crate) fn encoded_frame_context_bytes(
        &self,
        input: &[Value],
        instructions: &str,
        exact_tools: &[ToolDefinition],
    ) -> Result<usize, CodexHttpV1Error> {
        self.encode_frame_request_with_previous_response_id_bounded(
            input,
            instructions,
            exact_tools,
            None,
            usize::MAX,
        )
        .map(|body| body.len())
    }

    fn request_from_history(
        &self,
        history: CodexHttpV1History,
        tools: Option<Vec<CodexFunctionTool>>,
    ) -> CodexHttpV1Request {
        CodexHttpV1Request {
            model: self.model().id().to_owned(),
            max_output_tokens: self.options.max_output_tokens,
            temperature: self.options.temperature,
            decision_output: self.options.decision_output.clone(),
            instructions: history.instructions,
            input: history.input,
            tools,
            tool_choice: if self.options.decision_output.is_some() {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            },
            parallel_tool_calls: self.options.parallel_tool_calls,
            reasoning: self.options.reasoning,
            provider: self.options.openrouter_provider_routing.clone(),
            store: false,
            stream: true,
            include: vec![IncludedField::ReasoningEncryptedContent],
            prompt_cache_key: self.options.prompt_cache_key.clone(),
            prompt_cache_options: self
                .options
                .prompt_cache_comparison_response_id
                .clone()
                .map(|comparison_response_id| CodexPromptCacheOptions {
                    comparison_response_id,
                }),
            caching: None,
            previous_response_id: None,
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
    native_tools: &[ToolDefinition],
) -> Result<Vec<CodexFunctionTool>, CodexHttpV1Error> {
    native_tools
        .iter()
        .map(|tool| {
            CodexFunctionTool::new(
                tool.name(),
                tool.description(),
                tool.input_schema().clone(),
                tool.strict(),
            )
        })
        .collect()
}

/// A private structured-output contract for the native question primitives.
/// Callers supply questions, never an arbitrary output schema.
fn decision_answers_schema(declaration: &DecisionDeclaration) -> Value {
    let questions = declaration.questions();
    let properties: serde_json::Map<String, Value> = questions
        .iter()
        .map(|(name, question)| {
            (
                name.clone(),
                serde_json::json!({
                    "type": "object",
                    "oneOf": [decision_answer_schema(question), {
                        "properties": {"type": {"enum": ["refusal"]}},
                        "required": ["type"],
                        "additionalProperties": false,
                    }],
                }),
            )
        })
        .collect();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": questions.keys().collect::<Vec<_>>(),
        "additionalProperties": false,
    })
}

fn decision_answer_schema(question: &DecisionQuestion) -> Value {
    let probability = serde_json::json!({"type": "number", "minimum": 0, "maximum": 1});
    let (kind, value_name, value_schema) = match question {
        DecisionQuestion::Noul { .. } => ("noul", "noul", probability.clone()),
        DecisionQuestion::Choice { criteria, .. } => (
            "choice",
            "choice",
            serde_json::json!({"enum": criteria.keys().collect::<Vec<_>>()}),
        ),
        DecisionQuestion::Score { criteria, .. } => (
            "score",
            "score",
            serde_json::json!({"type": "number", "minimum": 0, "maximum": criteria.len().saturating_sub(1)}),
        ),
        DecisionQuestion::Box { .. } => (
            "box",
            "box",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "top": probability, "bottom": probability,
                    "left": probability, "right": probability,
                },
                "required": ["top", "bottom", "left", "right"],
                "additionalProperties": false,
            }),
        ),
    };
    let mut properties = serde_json::Map::from_iter([
        ("type".to_owned(), serde_json::json!({"enum": [kind]})),
        (value_name.to_owned(), value_schema),
    ]);
    if kind != "noul" {
        properties.insert("confidence".to_owned(), probability.clone());
    }
    let labels = match question {
        DecisionQuestion::Choice { criteria, .. } => Some(criteria.keys().cloned().collect()),
        DecisionQuestion::Score { criteria, .. } => {
            Some((0..criteria.len()).map(|index| index.to_string()).collect())
        }
        _ => None,
    };
    if let Some(labels) = labels {
        let labels: Vec<String> = labels;
        let probabilities: serde_json::Map<String, Value> = labels
            .iter()
            .map(|label| (label.clone(), probability.clone()))
            .collect();
        properties.insert(
            "probabilities".to_owned(),
            serde_json::json!({"type": "object", "properties": probabilities, "additionalProperties": false}),
        );
        if kind == "score" {
            let legend: serde_json::Map<String, Value> = labels
                .into_iter()
                .map(|label| {
                    (
                        label,
                        serde_json::json!({"type": ["string", "object", "array"]}),
                    )
                })
                .collect();
            properties.insert(
                "legend".to_owned(),
                serde_json::json!({"type": "object", "properties": legend, "additionalProperties": false}),
            );
        }
    }
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": ["type", value_name],
        "additionalProperties": false,
    })
}

/// Both adapters admit the same answers map. Responses generates this one
/// private envelope, while the native endpoint adds its own usage metadata.
fn decision_envelope_schema(declaration: &DecisionDeclaration) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {"answers": decision_answers_schema(declaration)},
        "required": ["answers"],
        "additionalProperties": false,
    })
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
    None,
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
        && name
            .chars()
            .all(|character| unicode_ident::is_xid_continue(character) || character == '-');
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
    #[error(
        "invalid Codex function tool name `{name}`; expected a non-empty name using Unicode identifier characters or hyphens"
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
    #[error("Responses temperature must be finite and between zero and two")]
    InvalidTemperature,
    #[error("OpenRouter routing requires eligible endpoints and non-empty endpoint slugs without whitespace or control characters")]
    InvalidOpenRouterProviderRouting,
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
        component::execution::ToolDefinition, pom::Document,
        pom_resolution::resolve_artifact_document,
    };
    use serde_json::json;

    fn prompt_document(text: &str) -> crate::pom::ResolvedDocument {
        resolve_artifact_document(Document::from_raw_text(text)).unwrap()
    }

    fn ark_encoder(enabled: bool) -> CodexHttpV1Encoder {
        CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                ModelSpec::new("doubao-seed-character-260628", 128_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap()
            .with_ark_caching(enabled),
        )
    }

    #[test]
    fn native_questions_generate_the_private_responses_answer_contract() {
        use std::collections::BTreeMap;

        let declaration = DecisionDeclaration::new(BTreeMap::from([
            (
                "is_bug".to_owned(),
                DecisionQuestion::noul("Is this a defect?"),
            ),
            (
                "team".to_owned(),
                DecisionQuestion::choice(
                    "Who owns this ticket?",
                    BTreeMap::from([
                        ("payments".to_owned(), json!("Billing and checkout")),
                        ("account".to_owned(), json!("Login and access")),
                    ]),
                ),
            ),
            (
                "urgency".to_owned(),
                DecisionQuestion::score(
                    "How urgent is it?",
                    vec![json!("Can wait"), json!("Blocking")],
                ),
            ),
            (
                "region".to_owned(),
                DecisionQuestion::bounding_box("The product"),
            ),
        ]))
        .unwrap();
        let schema = decision_envelope_schema(&declaration);
        let validator = jsonschema::validator_for(&schema).unwrap();
        let valid = json!({"answers": {
            "is_bug": {"type":"noul","noul":0.9},
            "team": {"type":"choice","choice":"payments","confidence":0.8,"probabilities":{"payments":0.8,"account":0.2}},
            "urgency": {"type":"score","score":0.95},
            "region": {"type":"box","box":{"top":0.1,"bottom":0.8,"left":0.2,"right":0.9}},
        }});
        assert!(validator.is_valid(&valid));
        for (name, answer) in [
            ("is_bug", json!({"type":"noul","noul":1.1})),
            ("is_bug", json!({"type":"score","score":0.5})),
            ("team", json!({"type":"choice","choice":"unknown"})),
            (
                "team",
                json!({"type":"choice","choice":"payments","confidence":1.1}),
            ),
            (
                "team",
                json!({"type":"choice","choice":"payments","probabilities":{"unknown":0.8}}),
            ),
            ("urgency", json!({"type":"score","score":2})),
            (
                "region",
                json!({"type":"box","box":{"top":-0.1,"bottom":0.8,"left":0.2,"right":0.9}}),
            ),
        ] {
            let mut invalid = valid.clone();
            invalid["answers"][name] = answer;
            assert!(!validator.is_valid(&invalid));
        }
        let mut missing = valid.clone();
        missing["answers"].as_object_mut().unwrap().remove("team");
        assert!(!validator.is_valid(&missing));
        let mut extra = valid.clone();
        extra["answers"]["other"] = json!({"type":"noul","noul":0.5});
        assert!(!validator.is_valid(&extra));
        let mut refused = valid;
        refused["answers"]["team"] = json!({"type":"refusal"});
        assert!(validator.is_valid(&refused));
    }

    #[test]
    fn frame_decision_format_is_local_and_reuses_model_options_without_remote_history() {
        use std::collections::BTreeMap;
        let configured = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                ModelSpec::new("fast-model", 32_000).unwrap(),
                None,
                Some(CodexReasoning::none()),
                Some("shared-prefix"),
            )
            .unwrap()
            .with_ark_caching(true)
            .with_parallel_tool_calls(true)
            .with_max_output_tokens(128)
            .unwrap(),
        );
        let decision = DecisionDeclaration::new(BTreeMap::from([(
            "gate".to_owned(),
            DecisionQuestion::noul("Is the gate ready?"),
        )]))
        .unwrap();
        let encoder = configured.for_decision(&decision);
        let body: Value = serde_json::from_slice(
            &encoder
                .encode_frame_request_bounded(&[], "", &[], usize::MAX)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["model"], "fast-model");
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["prompt_cache_key"], "shared-prefix");
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["store"], false);
        assert!(body.get("caching").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(configured.ark_caching_enabled());
        let ordinary: Value = serde_json::from_slice(
            &configured
                .encode_frame_request_bounded(&[], "", &[], usize::MAX)
                .unwrap(),
        )
        .unwrap();
        assert!(ordinary.get("text").is_none());
        assert_eq!(ordinary["parallel_tool_calls"], true);
    }

    #[test]
    fn ark_caching_is_opt_in_and_limited_to_native_foreground_requests() {
        let default = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                ModelSpec::new("test-model", 128_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap(),
        );
        assert!(!default.ark_caching_enabled());
        let ordinary: Value = serde_json::from_slice(
            &default
                .encode_frame_request_bounded(&[], "system", &[], usize::MAX)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ordinary["store"], false);
        assert_eq!(ordinary["instructions"], "system");
        assert_eq!(ordinary["tool_choice"], "auto");
        assert_eq!(ordinary["parallel_tool_calls"], false);
        assert!(ordinary.get("caching").is_none());
        assert!(ordinary.get("previous_response_id").is_none());

        let enabled = ark_encoder(true);
        assert!(enabled.ark_caching_enabled());
        for body in [
            enabled.encode_request(&CanonicalTranscript::new()).unwrap(),
            enabled
                .encode_summary_request_bounded(&[], "summarize", 64, usize::MAX)
                .unwrap(),
            enabled
                .without_ark_caching()
                .encode_frame_request_bounded(&[], "system", &[], usize::MAX)
                .unwrap(),
        ] {
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["store"], false);
            assert!(body.get("caching").is_none());
            assert!(body.get("previous_response_id").is_none());
        }
    }

    #[test]
    fn ark_cached_requests_move_system_to_input_and_inherit_tools_on_continuation() {
        let mut encoder = ark_encoder(true);
        encoder.set_parallel_tool_calls(true);
        assert!(encoder.parallel_tool_calls_enabled());
        let tools =
            [ToolDefinition::new("lookup", "Read the status", json!({"type":"object"})).unwrap()];
        let first_input = [json!({"role":"user","content":"Read the status"})];
        let first: Value = serde_json::from_slice(
            &encoder
                .encode_frame_request_bounded(
                    &first_input,
                    "Use the lookup tool",
                    &tools,
                    usize::MAX,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["store"], true);
        assert_eq!(first["caching"], json!({"type":"enabled"}));
        assert_eq!(first["tool_choice"], "auto");
        assert_eq!(first["parallel_tool_calls"], true);
        assert!(first.get("instructions").is_none());
        assert!(first.get("previous_response_id").is_none());
        assert_eq!(first["input"][0]["role"], "system");
        assert_eq!(
            first["input"][0]["content"][0]["text"],
            "Use the lookup tool"
        );
        assert_eq!(first["input"][1], first_input[0]);
        assert_eq!(first["tools"][0]["name"], "lookup");

        let suffix = [json!({"type":"function_call_output","call_id":"call_1","output":"ready"})];
        let body = encoder
            .encode_frame_request_with_previous_response_id_bounded(
                &suffix,
                "Use the lookup tool",
                &tools,
                Some("resp_1"),
                usize::MAX,
            )
            .unwrap();
        let second: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(second["store"], true);
        assert_eq!(second["caching"], json!({"type":"enabled"}));
        assert_eq!(second["previous_response_id"], "resp_1");
        assert_eq!(second["input"], json!(suffix));
        assert!(second.get("instructions").is_none());
        assert!(second.get("tools").is_none());
        assert!(second.get("tool_choice").is_none());
        assert!(second.get("parallel_tool_calls").is_none());
        assert_eq!(
            encoder
                .encode_frame_request_with_previous_response_id_bounded(
                    &suffix,
                    "Use the lookup tool",
                    &tools,
                    Some("resp_1"),
                    body.len(),
                )
                .unwrap(),
            body
        );
        assert!(matches!(
            encoder.encode_frame_request_with_previous_response_id_bounded(
                &suffix,
                "Use the lookup tool",
                &tools,
                Some("resp_1"),
                body.len() - 1,
            ),
            Err(CodexHttpV1Error::SerializedRequestBodyLimit)
        ));
    }

    #[test]
    fn ark_context_estimate_includes_full_history_and_ignores_upload_budget() {
        let encoder = ark_encoder(true);
        let suffix = [json!({"role":"user","content":"next"})];
        let history = [
            json!({"role":"user","content":"retained".repeat(1024)}),
            suffix[0].clone(),
        ];
        let upload = encoder
            .encode_frame_request_with_previous_response_id_bounded(
                &suffix,
                "",
                &[],
                Some("resp_1"),
                usize::MAX,
            )
            .unwrap();
        let full_bytes = encoder
            .encoded_frame_context_bytes(&history, "", &[])
            .unwrap();
        assert!(full_bytes > upload.len());
        assert_eq!(
            full_bytes,
            encoder
                .encode_frame_request_bounded(&history, "", &[], usize::MAX)
                .unwrap()
                .len()
        );
        let request: Value = serde_json::from_slice(&upload).unwrap();
        assert_eq!(request["input"], json!(suffix));
    }

    #[test]
    fn retained_input_encoder_preserves_wire_order_and_non_input_options() {
        let options = CodexHttpV1Options::new(
            crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
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
                prompt_document("Retain these instructions."),
            ),
            CanonicalInputItem::instruction(
                InstructionAuthority::Developer,
                prompt_document("Replace this typed input."),
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
                "Retain these instructions.",
            )
            .unwrap();

        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"model":"gpt-5.6-codex","instructions":"Retain these instructions.","input":[{"encrypted_content":"opaque","id":"cmp_7","type":"compaction"}],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":{"effort":"max","summary":"detailed"},"store":false,"stream":true,"include":["reasoning.encrypted_content"],"prompt_cache_key":"retained-session"}"#
        );
    }

    fn openrouter_routing() -> OpenRouterProviderRouting {
        OpenRouterProviderRouting::new(
            vec!["together".into(), "fireworks/us".into()],
            vec!["together".into(), "fireworks/us".into()],
            true,
        )
        .unwrap()
    }

    #[test]
    fn reasoning_modes_and_routing_survive_all_request_encodings() {
        let expected_routing = json!({
            "only": ["together", "fireworks/us"],
            "order": ["together", "fireworks/us"],
            "require_parameters": true,
        });
        let transcript = CanonicalTranscript::new();
        for (reasoning, expected) in [
            (Some(CodexReasoning::disabled()), json!({"enabled": false})),
            (Some(CodexReasoning::none()), json!({"effort": "none"})),
            (
                Some(CodexReasoning::max_detailed()),
                json!({"effort": "max", "summary": "detailed"}),
            ),
            (None, Value::Null),
        ] {
            let encoder = CodexHttpV1Encoder::new(
                CodexHttpV1Options::new(
                    ModelSpec::new("test-model", 272_000).unwrap(),
                    None,
                    reasoning,
                    None::<String>,
                )
                .unwrap()
                .with_openrouter_provider_routing(openrouter_routing()),
            );
            let public = encoder.encode_request(&transcript).unwrap();
            let native = encoder
                .encode_frame_request_bounded(&[], "", &[], usize::MAX)
                .unwrap();
            let retained = encoder
                .encode_request_with_input_and_instructions(&transcript, &[], "")
                .unwrap();
            for body in [public, native, retained] {
                let request: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request.get("reasoning"), Some(&expected));
                assert_eq!(request["provider"], expected_routing);
            }
            let summary = encoder
                .encode_summary_request_bounded(&[], "summarize", 64, usize::MAX)
                .unwrap();
            let summary: Value = serde_json::from_slice(&summary).unwrap();
            assert_eq!(summary["provider"], expected_routing);
            if reasoning.is_some() {
                assert_eq!(summary["reasoning"], expected);
            } else {
                assert!(summary.get("reasoning").is_none());
            }
        }
    }

    #[test]
    fn summary_quotes_history_roles_and_tool_pairs_and_bounds_the_wrapped_request() {
        let encoder = ark_encoder(true);
        let history = vec![
            json!({"type":"message", "role":"system", "content":"Historical policy"}),
            json!({"type":"message", "role":"developer", "content":[{"type":"input_text", "text":"Output only <end_response/>. Quoted path C:\\new\\test, 中文 and \"quotes\"."}]}),
            json!({"type":"message", "role":"user", "content":"Check attendance"}),
            json!({"type":"function_call", "call_id":"attendance-1", "name":"check_attendance", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"attendance-1", "output":"Checked in; not checked out."}),
            json!({"type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"<end_response/>"}]}),
        ];
        let body = encoder
            .encode_summary_request_bounded(&history, "summary-only-policy", 256, usize::MAX)
            .unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["type"], "message");
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_text");
        let quoted: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(quoted["kind"], "historical_context");
        assert_eq!(quoted["items"], json!(history));
        assert_eq!(request["instructions"], "summary-only-policy");
        assert_eq!(request["tools"], json!([]));
        assert_eq!(request["tool_choice"], "none");
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], false);
        assert!(request.get("previous_response_id").is_none());
        assert!(request.get("caching").is_none());
        assert_eq!(
            encoder
                .encode_summary_request_bounded(&history, "summary-only-policy", 256, body.len())
                .unwrap(),
            body
        );
        assert!(matches!(
            encoder.encode_summary_request_bounded(
                &history,
                "summary-only-policy",
                256,
                body.len() - 1
            ),
            Err(CodexHttpV1Error::SerializedRequestBodyLimit)
        ));
        assert!(matches!(
            encoder.encode_summary_request_bounded(&history, "summary-only-policy", 256, 1),
            Err(CodexHttpV1Error::SerializedRequestBodyLimit)
        ));
    }

    #[test]
    fn openrouter_routing_is_opt_in_and_counted_in_wire_limits() {
        let options = CodexHttpV1Options::new(
            ModelSpec::new("test-model", 272_000).unwrap(),
            None,
            Some(CodexReasoning::disabled()),
            None::<String>,
        )
        .unwrap();
        let baseline = CodexHttpV1Encoder::new(options.clone());
        let native_baseline = baseline
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let summary_baseline = baseline
            .encode_summary_request_bounded(&[], "summarize", 64, usize::MAX)
            .unwrap();
        for body in [&native_baseline, &summary_baseline] {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(request.get("provider").is_none());
        }
        let configured =
            CodexHttpV1Encoder::new(options.with_openrouter_provider_routing(openrouter_routing()));
        assert!(matches!(
            configured.encode_frame_request_bounded(&[], "", &[], native_baseline.len()),
            Err(CodexHttpV1Error::SerializedRequestBodyLimit)
        ));
        assert!(matches!(
            configured.encode_summary_request_bounded(&[], "summarize", 64, summary_baseline.len()),
            Err(CodexHttpV1Error::SerializedRequestBodyLimit)
        ));
    }

    #[test]
    fn openrouter_routing_rejects_empty_or_malformed_endpoint_slugs() {
        for (only, order) in [
            (vec![], vec![]),
            (vec!["".into()], vec![]),
            (vec!["together".into()], vec!["fireworks /us".into()]),
            (vec!["together\n".into()], vec![]),
        ] {
            assert!(matches!(
                OpenRouterProviderRouting::new(only, order, true),
                Err(CodexHttpV1Error::InvalidOpenRouterProviderRouting)
            ));
        }
    }

    #[test]
    fn max_output_tokens_is_optional_in_public_and_native_requests() {
        let default_encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap(),
        );
        let default_public = default_encoder
            .request(&CanonicalTranscript::new())
            .unwrap();
        let default_public: Value = serde_json::to_value(default_public).unwrap();
        assert!(default_public.get("max_output_tokens").is_none());
        assert!(default_public.get("temperature").is_none());
        let default_native = default_encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let default_native: Value = serde_json::from_slice(&default_native).unwrap();
        assert!(default_native.get("max_output_tokens").is_none());
        assert!(default_native.get("temperature").is_none());

        let configured_encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap()
            .with_max_output_tokens(1_024)
            .unwrap()
            .with_temperature(0.2)
            .unwrap(),
        );
        let configured_public = configured_encoder
            .request(&CanonicalTranscript::new())
            .unwrap();
        let configured_public: Value = serde_json::to_value(configured_public).unwrap();
        assert_eq!(configured_public["max_output_tokens"], json!(1_024));
        assert_eq!(configured_public["temperature"], json!(0.2));
        let configured_native = configured_encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let configured_native: Value = serde_json::from_slice(&configured_native).unwrap();
        assert_eq!(configured_native["max_output_tokens"], json!(1_024));
        assert_eq!(configured_native["temperature"], json!(0.2));
    }

    #[test]
    fn max_output_tokens_rejects_zero() {
        assert!(matches!(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>
            )
            .unwrap()
            .with_max_output_tokens(0),
            Err(CodexHttpV1Error::ZeroMaxOutputTokens)
        ));
    }

    #[test]
    fn temperature_rejects_nonfinite_and_out_of_range_values() {
        for temperature in [f64::NAN, f64::INFINITY, -0.1, 2.1] {
            assert!(matches!(
                CodexHttpV1Options::new(
                    crate::provider::ModelSpec::new("test-model", 272_000).unwrap(),
                    None,
                    None,
                    None::<String>
                )
                .unwrap()
                .with_temperature(temperature),
                Err(CodexHttpV1Error::InvalidTemperature)
            ));
        }
    }

    #[test]
    fn ordinary_requests_never_ask_the_server_to_manage_context() {
        let encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap(),
        );
        let public =
            serde_json::to_value(encoder.request(&CanonicalTranscript::new()).unwrap()).unwrap();
        assert!(public.get("context_management").is_none());
        let native = encoder
            .encode_frame_request_bounded(&[], "", &[], usize::MAX)
            .unwrap();
        let native: Value = serde_json::from_slice(&native).unwrap();
        assert!(native.get("context_management").is_none());
    }

    #[test]
    fn explicit_assistant_status_is_opt_in_and_preserves_interruption_semantics() {
        let assistant_history = CanonicalTranscript::new()
            .appended(CanonicalInputItem::message(
                ConversationRole::Assistant,
                prompt_document("completed history"),
            ))
            .unwrap();
        let default = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap(),
        );
        let default_body: Value =
            serde_json::to_value(default.request(&assistant_history).unwrap()).unwrap();
        assert!(default_body["input"][0].get("status").is_none());

        let explicit = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
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
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                Some(vec![CodexFunctionTool::new(
                    "configured_tool",
                    "old",
                    json!({}),
                    false,
                )
                .unwrap()]),
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
    }

    #[test]
    fn native_frame_request_uses_the_complete_tool_definition() {
        let encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap(),
        );
        let tool = ToolDefinition::new(
            "lookup_status",
            "Look up the latest status for an account.",
            json!({
                "type": "object",
                "properties": {
                    "account_id": {"type": "string", "description": "Stable account id."},
                    "verbose": {"type": "boolean"},
                },
                "required": ["account_id"],
                "additionalProperties": false,
            }),
        )
        .unwrap()
        .with_strict(true);

        let body = encoder
            .encode_frame_request_bounded(&[], "", &[tool], usize::MAX)
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "lookup_status");
        assert_eq!(
            body["tools"][0]["description"],
            "Look up the latest status for an account."
        );
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["account_id"]["description"],
            "Stable account id."
        );
        assert_eq!(body["tools"][0]["strict"], true);
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
            CodexHttpV1Options::new(
                crate::provider::ModelSpec::new("gpt-5.6-codex", 272_000).unwrap(),
                None,
                None,
                None::<String>,
            )
            .unwrap(),
        );
        let system = CanonicalInputItem::instruction(
            InstructionAuthority::System,
            prompt_document("Current instructions."),
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
