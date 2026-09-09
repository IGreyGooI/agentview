use std::{collections::HashSet, str::FromStr};
#[cfg(feature = "legacy-provider-port")]
use std::{pin::Pin, sync::Arc};

#[cfg(feature = "legacy-provider-port")]
use async_trait::async_trait;
#[cfg(feature = "legacy-provider-port")]
use futures::Stream;

use crate::{
    component::authoring::__private::EventSelector,
    llm_call::TextTurnEvent,
    pom::{Document, ResolvedDocument},
    transcript::{CanonicalInputItem, CanonicalTranscript, CanonicalTranscriptError},
};

use super::ToolDefinition;

/// One complete provider-neutral projection of current Component state.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedProjection {
    nodes: Vec<RenderedProjectionNode>,
    native_tools: Vec<ToolDefinition>,
    execution_scope: Option<ProjectionExecutionScope>,
}

/// Neutral runtime provenance for a ComponentHost mount. Providers may use it
/// to fence private reconciliation state; it is not model-visible input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ProjectionExecutionScope {
    pub(crate) host_instance: u64,
    pub(crate) mount_generation: u64,
}

impl RenderedProjection {
    pub fn from_nodes(nodes: Vec<RenderedProjectionNode>) -> Result<Self, RenderedProjectionError> {
        let mut identities = HashSet::with_capacity(nodes.len());
        for node in &nodes {
            if node.identity.is_empty() {
                return Err(RenderedProjectionError::EmptyNodeIdentity);
            }
            if !identities.insert(node.identity.as_str()) {
                return Err(RenderedProjectionError::DuplicateNodeIdentity {
                    identity: node.identity.clone(),
                });
            }
            node.validate()?;
        }
        Ok(Self {
            nodes,
            native_tools: Vec::new(),
            execution_scope: None,
        })
    }

    pub fn nodes(&self) -> &[RenderedProjectionNode] {
        &self.nodes
    }

    #[allow(dead_code)] // Retained for name-only compatibility fixtures.
    pub(crate) fn with_native_tool_names(
        nodes: Vec<RenderedProjectionNode>,
        native_tool_names: Vec<String>,
    ) -> Result<Self, RenderedProjectionError> {
        let native_tools = native_tool_names
            .into_iter()
            .map(|name| {
                let error_name = name.clone();
                ToolDefinition::legacy_name_only(name).map_err(|_| {
                    RenderedProjectionError::DuplicateNativeToolName { name: error_name }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::with_native_tools(nodes, native_tools)
    }

    /// Attaches complete native tool declarations to this projection.
    pub fn with_native_tools(
        nodes: Vec<RenderedProjectionNode>,
        native_tools: Vec<ToolDefinition>,
    ) -> Result<Self, RenderedProjectionError> {
        let projection = Self::from_nodes(nodes)?;
        let mut seen = HashSet::with_capacity(native_tools.len());
        for tool in &native_tools {
            if !seen.insert(tool.name()) {
                return Err(RenderedProjectionError::DuplicateNativeToolName {
                    name: tool.name().to_owned(),
                });
            }
        }
        Ok(Self {
            native_tools,
            ..projection
        })
    }

    /// Complete native tool declarations in Component structural order.
    pub fn native_tools(&self) -> &[ToolDefinition] {
        &self.native_tools
    }

    pub(crate) fn with_execution_scope(mut self, scope: ProjectionExecutionScope) -> Self {
        self.execution_scope = Some(scope);
        self
    }

    pub(crate) fn execution_scope(&self) -> Option<ProjectionExecutionScope> {
        self.execution_scope
    }

    pub fn to_transcript(&self) -> Result<CanonicalTranscript, CanonicalTranscriptError> {
        CanonicalTranscript::try_from_item_refs(
            self.nodes.iter().flat_map(|node| node.items.iter()),
        )
    }
}

/// One mounted Component node in structural render order.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedProjectionNode {
    identity: String,
    items: Vec<CanonicalInputItem>,
    diffs: Vec<RenderedProjectionDiffMarker>,
    diff_templates: Vec<RenderedProjectionItemTemplate>,
}

impl RenderedProjectionNode {
    pub fn new(identity: impl Into<String>, items: Vec<CanonicalInputItem>) -> Self {
        Self {
            identity: identity.into(),
            items,
            diffs: Vec::new(),
            diff_templates: Vec::new(),
        }
    }

    pub(crate) fn with_diff_templates(
        identity: impl Into<String>,
        items: Vec<CanonicalInputItem>,
        diffs: Vec<RenderedProjectionDiffMarker>,
        diff_templates: Vec<RenderedProjectionItemTemplate>,
    ) -> Self {
        Self {
            identity: identity.into(),
            items,
            diffs,
            diff_templates,
        }
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn items(&self) -> &[CanonicalInputItem] {
        &self.items
    }

    /// Diff-marked POM fragments owned by this Component node.
    pub fn diffs(&self) -> &[RenderedProjectionDiffMarker] {
        &self.diffs
    }

    pub(crate) fn diff_templates(&self) -> &[RenderedProjectionItemTemplate] {
        &self.diff_templates
    }

    fn validate(&self) -> Result<(), RenderedProjectionError> {
        let mut item_indexes = HashSet::with_capacity(self.diffs.len());
        let mut addresses = HashSet::with_capacity(self.diffs.len());
        for diff in &self.diffs {
            if diff.item_index >= self.items.len() {
                return Err(RenderedProjectionError::DiffItemOutOfBounds {
                    identity: self.identity.clone(),
                    item_index: diff.item_index,
                    item_count: self.items.len(),
                });
            }
            if !item_indexes.insert((
                diff.item_index,
                diff.structural_path.clone(),
                diff.slot.clone(),
            )) || !addresses.insert((diff.structural_path.clone(), diff.slot.clone()))
            {
                return Err(RenderedProjectionError::DuplicateDiffAddress {
                    identity: self.identity.clone(),
                    slot: diff.slot.clone(),
                });
            }
        }

        let mut template_items = HashSet::with_capacity(self.diff_templates.len());
        let mut referenced_diffs = vec![false; self.diffs.len()];
        for template in &self.diff_templates {
            if template.item_index >= self.items.len() {
                return Err(self.invalid_diff_template(format!(
                    "item index {} is outside {} items",
                    template.item_index,
                    self.items.len()
                )));
            }
            if !template_items.insert(template.item_index) {
                return Err(self.invalid_diff_template(format!(
                    "item index {} has more than one template",
                    template.item_index
                )));
            }
            let mut has_diff = false;
            for fragment in &template.fragments {
                let RenderedProjectionFragment::Diff { diff_index, .. } = fragment else {
                    continue;
                };
                has_diff = true;
                let Some(diff) = self.diffs.get(*diff_index) else {
                    return Err(self.invalid_diff_template(format!(
                        "diff index {diff_index} is outside {} diffs",
                        self.diffs.len()
                    )));
                };
                if diff.item_index != template.item_index {
                    return Err(self.invalid_diff_template(format!(
                        "diff index {diff_index} belongs to item {}, not template item {}",
                        diff.item_index, template.item_index
                    )));
                }
                if std::mem::replace(&mut referenced_diffs[*diff_index], true) {
                    return Err(self.invalid_diff_template(format!(
                        "diff index {diff_index} is referenced more than once"
                    )));
                }
            }
            if !has_diff {
                return Err(self.invalid_diff_template(format!(
                    "item index {} contains no diff fragment",
                    template.item_index
                )));
            }
        }
        if let Some(diff_index) = referenced_diffs.iter().position(|referenced| !referenced) {
            return Err(self.invalid_diff_template(format!(
                "diff index {diff_index} is not referenced by a template"
            )));
        }
        Ok(())
    }

    fn invalid_diff_template(&self, message: String) -> RenderedProjectionError {
        RenderedProjectionError::InvalidDiffTemplate {
            identity: self.identity.clone(),
            message,
        }
    }
}

/// Stable marker for one `#[diff(slot = "...")]` POM fragment.
///
/// This identifies where a Provider may compute a diff; it is not a computed delta.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderedProjectionDiffMarker {
    item_index: usize,
    structural_path: Vec<usize>,
    slot: String,
}

impl RenderedProjectionDiffMarker {
    pub(crate) fn new(
        item_index: usize,
        structural_path: Vec<usize>,
        slot: impl Into<String>,
    ) -> Self {
        Self {
            item_index,
            structural_path,
            slot: slot.into(),
        }
    }

    pub fn item_index(&self) -> usize {
        self.item_index
    }

    pub fn structural_path(&self) -> &[usize] {
        &self.structural_path
    }

    pub fn slot(&self) -> &str {
        &self.slot
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RenderedProjectionItemTemplate {
    item_index: usize,
    fragments: Vec<RenderedProjectionFragment>,
}

impl RenderedProjectionItemTemplate {
    pub(crate) fn new(item_index: usize, fragments: Vec<RenderedProjectionFragment>) -> Self {
        Self {
            item_index,
            fragments,
        }
    }

    pub(crate) fn item_index(&self) -> usize {
        self.item_index
    }

    pub(crate) fn fragments(&self) -> &[RenderedProjectionFragment] {
        &self.fragments
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RenderedProjectionFragment {
    Complete(ResolvedDocument),
    Diff {
        diff_index: usize,
        authored: Document,
        complete: ResolvedDocument,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RenderedProjectionError {
    #[error("rendered projection node identity must be non-empty")]
    EmptyNodeIdentity,
    #[error("rendered projection contains duplicate node identity `{identity}`")]
    DuplicateNodeIdentity { identity: String },
    #[error(
        "rendered projection node `{identity}` diff item index {item_index} is outside {item_count} items"
    )]
    DiffItemOutOfBounds {
        identity: String,
        item_index: usize,
        item_count: usize,
    },
    #[error("rendered projection node `{identity}` contains duplicate diff slot `{slot}`")]
    DuplicateDiffAddress { identity: String, slot: String },
    #[error("rendered projection node `{identity}` has an invalid diff template: {message}")]
    InvalidDiffTemplate { identity: String, message: String },
    #[error("rendered projection contains duplicate or empty native tool name `{name}`")]
    DuplicateNativeToolName { name: String },
}

/// A complete, provider-neutral native function call.
#[derive(Debug, Clone)]
pub struct ToolCall {
    call_id: String,
    name: String,
    raw_arguments: String,
    #[cfg(feature = "legacy-provider-port")]
    output_sink: Option<Arc<dyn ToolOutputSink>>,
    #[cfg(feature = "legacy-provider-port")]
    output_ordinal: Option<u64>,
}

/// One provider-neutral answer to exactly one completed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    call_id: String,
    content: String,
}

impl ToolOutput {
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
    pub fn content(&self) -> &str {
        &self.content
    }
}

impl ToolCall {
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Result<Self, crate::transcript::CanonicalTranscriptError> {
        let item = crate::transcript::CanonicalInputItem::tool_call(call_id, name, raw_arguments)?;
        let crate::transcript::CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } = item
        else {
            unreachable!("tool call constructor returns a tool call")
        };
        Ok(Self {
            call_id,
            name,
            raw_arguments,
            #[cfg(feature = "legacy-provider-port")]
            output_sink: None,
            #[cfg(feature = "legacy-provider-port")]
            output_ordinal: None,
        })
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn raw_arguments(&self) -> &str {
        &self.raw_arguments
    }

    pub fn output(&self, content: impl Into<String>) -> ToolOutput {
        ToolOutput {
            call_id: self.call_id.clone(),
            content: content.into(),
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn with_output_sink(
        mut self,
        ordinal: u64,
        output_sink: Arc<dyn ToolOutputSink>,
    ) -> Result<Self, ()> {
        output_sink.register(ordinal, &self.call_id)?;
        self.output_sink = Some(output_sink);
        self.output_ordinal = Some(ordinal);
        Ok(self)
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn publish_output(&self, output: ToolOutput) -> Result<(), ()> {
        self.output_sink
            .as_ref()
            .ok_or(())?
            .accept(self.output_ordinal.ok_or(())?, output)
    }
}

/// Private provider-owned result table used by a reaction-local ToolCall lane.
///
/// Acceptance is synchronous so Runtime observation cannot report a lane as
/// closed before the provider owns its one allowed result.
#[cfg(feature = "legacy-provider-port")]
pub(crate) trait ToolOutputSink: std::fmt::Debug + Send + Sync {
    fn register(&self, ordinal: u64, call_id: &str) -> Result<(), ()>;
    fn accept(&self, ordinal: u64, output: ToolOutput) -> Result<(), ()>;
}

impl PartialEq for ToolCall {
    fn eq(&self, other: &Self) -> bool {
        self.call_id == other.call_id
            && self.name == other.name
            && self.raw_arguments == other.raw_arguments
    }
}

impl Eq for ToolCall {}

/// One framework-owned, provider-neutral LLM output event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderEvent {
    Text(TextTurnEvent),
    ToolCall(ToolCall),
}

impl ProviderEvent {
    pub const TEXT: EventSelector<Self, TextTurnEvent> = EventSelector::__component_events_v1(
        "agentview.provider-event.text",
        0,
        |event| match event {
            Self::Text(text) => Some(text),
            Self::ToolCall(_) => None,
        },
    );
}

#[cfg(feature = "legacy-provider-port")]
pub type ProviderEventStream<'a> =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderFault>> + Send + 'a>>;

/// The only public model-backend boundary.
#[cfg(feature = "legacy-provider-port")]
#[deprecated(note = "use `ReactionPort`")]
#[async_trait]
pub trait ProviderPort: Send {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIdentity {
    provider: String,
    profile: String,
    profile_version: u32,
    binding: String,
}

impl ProviderIdentity {
    pub fn new(
        provider: impl Into<String>,
        profile: impl Into<String>,
        profile_version: u32,
        binding: impl Into<String>,
    ) -> Result<Self, ProviderIdentityError> {
        if profile_version == 0 {
            return Err(ProviderIdentityError::new(
                "provider profile version must be nonzero",
            ));
        }
        Ok(Self {
            provider: valid_token("provider", provider.into())?,
            profile: valid_token("provider profile", profile.into())?,
            profile_version,
            binding: valid_token("provider binding", binding.into())?,
        })
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn profile_version(&self) -> u32 {
        self.profile_version
    }

    pub fn binding(&self) -> &str {
        &self.binding
    }
}

impl FromStr for ProviderIdentity {
    type Err = ProviderIdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = value.splitn(4, ':');
        let provider = fields.next().unwrap_or_default();
        let profile = fields.next().unwrap_or_default();
        let version = fields
            .next()
            .ok_or_else(|| ProviderIdentityError::new("provider identity is missing version"))?
            .parse::<u32>()
            .map_err(|_| ProviderIdentityError::new("provider identity version is invalid"))?;
        let binding = fields
            .next()
            .ok_or_else(|| ProviderIdentityError::new("provider identity is missing binding"))?;
        Self::new(provider, profile, version, binding)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid provider identity: {message}")]
pub struct ProviderIdentityError {
    message: String,
}

impl ProviderIdentityError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

fn valid_token(kind: &'static str, value: String) -> Result<String, ProviderIdentityError> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        });
    if valid {
        Ok(value)
    } else {
        Err(ProviderIdentityError::new(format!(
            "invalid {kind}; expected a non-empty ASCII token"
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFaultKind {
    RetryableTransport,
    ModelRejected,
}

/// Sanitized structural classification for provider observability.
///
/// This code describes where a failure occurred without carrying upstream
/// content. It is independent from [`ProviderFaultKind`], which retains the
/// provider's coarse retry semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFaultCode {
    Transport,
    RequestPreparation,
    ConnectSecureTransport,
    RequestTransport,
    RequestTimeout,
    Authentication,
    Authorization,
    RateLimited,
    UpstreamStatus,
    ResponseProtocol,
    ResponseContentType,
    StreamDecode,
    ResponseEventJson,
    ResponseEventShape,
    StreamTransport,
    StreamTimeout,
    ResponseBodyLimit,
    StreamEventLimit,
    OutputLimit,
    ModelRejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResponseEventType {
    Unknown,
    StreamEnd,
    ResponseCreated,
    ResponseInProgress,
    ResponseContentPartAdded,
    ResponseContentPartDone,
    ResponseOutputTextDelta,
    ResponseOutputTextAnnotationAdded,
    ResponseOutputTextDone,
    ResponseReasoningTextDelta,
    ResponseReasoningTextDone,
    ResponseCompleted,
    ResponseOutputItemAdded,
    ResponseOutputItemDone,
    ResponseFailed,
    ResponseIncomplete,
    Error,
    ResponseOutputOther,
}

impl ProviderResponseEventType {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::StreamEnd => "stream_end",
            Self::ResponseCreated => "response.created",
            Self::ResponseInProgress => "response.in_progress",
            Self::ResponseContentPartAdded => "response.content_part.added",
            Self::ResponseContentPartDone => "response.content_part.done",
            Self::ResponseOutputTextDelta => "response.output_text.delta",
            Self::ResponseOutputTextAnnotationAdded => "response.output_text.annotation.added",
            Self::ResponseOutputTextDone => "response.output_text.done",
            Self::ResponseReasoningTextDelta => "response.reasoning_text.delta",
            Self::ResponseReasoningTextDone => "response.reasoning_text.done",
            Self::ResponseCompleted => "response.completed",
            Self::ResponseOutputItemAdded => "response.output_item.added",
            Self::ResponseOutputItemDone => "response.output_item.done",
            Self::ResponseFailed => "response.failed",
            Self::ResponseIncomplete => "response.incomplete",
            Self::Error => "error",
            Self::ResponseOutputOther => "response.output_other",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "stream_end" => Self::StreamEnd,
            "response.created" => Self::ResponseCreated,
            "response.in_progress" => Self::ResponseInProgress,
            "response.content_part.added" => Self::ResponseContentPartAdded,
            "response.content_part.done" => Self::ResponseContentPartDone,
            "response.output_text.delta" => Self::ResponseOutputTextDelta,
            "response.output_text.annotation.added" => Self::ResponseOutputTextAnnotationAdded,
            "response.output_text.done" => Self::ResponseOutputTextDone,
            "response.reasoning_text.delta" => Self::ResponseReasoningTextDelta,
            "response.reasoning_text.done" => Self::ResponseReasoningTextDone,
            "response.completed" => Self::ResponseCompleted,
            "response.output_item.added" => Self::ResponseOutputItemAdded,
            "response.output_item.done" => Self::ResponseOutputItemDone,
            "response.failed" => Self::ResponseFailed,
            "response.incomplete" => Self::ResponseIncomplete,
            "error" => Self::Error,
            "response.output_other" => Self::ResponseOutputOther,
            _ => Self::Unknown,
        }
    }
}

impl serde::Serialize for ProviderResponseEventType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.code())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResponseEventReason {
    Unknown,
    Envelope,
    Sequence,
    LifecycleIdentity,
    LedgerMismatch,
    TerminalOrder,
    UnsupportedOutput,
    StreamCompletion,
}

impl ProviderResponseEventReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Envelope => "envelope",
            Self::Sequence => "sequence",
            Self::LifecycleIdentity => "lifecycle_identity",
            Self::LedgerMismatch => "ledger_mismatch",
            Self::TerminalOrder => "terminal_order",
            Self::UnsupportedOutput => "unsupported_output",
            Self::StreamCompletion => "stream_completion",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "envelope" => Self::Envelope,
            "sequence" => Self::Sequence,
            "lifecycle_identity" => Self::LifecycleIdentity,
            "ledger_mismatch" => Self::LedgerMismatch,
            "terminal_order" => Self::TerminalOrder,
            "unsupported_output" => Self::UnsupportedOutput,
            "stream_completion" => Self::StreamCompletion,
            _ => Self::Unknown,
        }
    }
}

impl serde::Serialize for ProviderResponseEventReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.code())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResponseLedgerReason {
    Unknown,
    ResponseObject,
    ResponseIdentity,
    AuthoritativeOutput,
    OutputItemType,
    OutputItemMissing,
    OutputItemShape,
    OutputItemId,
    OutputItemStatus,
    OutputCount,
    OutputIndex,
    OutputIdentity,
    OutputDone,
    MessageCompletion,
    MessageText,
    FinalMessageCount,
    FinalMessageMissing,
    Canonicalization,
}

impl ProviderResponseLedgerReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ResponseObject => "response_object",
            Self::ResponseIdentity => "response_identity",
            Self::AuthoritativeOutput => "authoritative_output",
            Self::OutputItemType => "output_item_type",
            Self::OutputItemMissing => "output_item_missing",
            Self::OutputItemShape => "output_item_shape",
            Self::OutputItemId => "output_item_id",
            Self::OutputItemStatus => "output_item_status",
            Self::OutputCount => "output_count",
            Self::OutputIndex => "output_index",
            Self::OutputIdentity => "output_identity",
            Self::OutputDone => "output_done",
            Self::MessageCompletion => "message_completion",
            Self::MessageText => "message_text",
            Self::FinalMessageCount => "final_message_count",
            Self::FinalMessageMissing => "final_message_missing",
            Self::Canonicalization => "canonicalization",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "response_object" => Self::ResponseObject,
            "response_identity" => Self::ResponseIdentity,
            "authoritative_output" => Self::AuthoritativeOutput,
            "output_item_type" => Self::OutputItemType,
            "output_item_missing" => Self::OutputItemMissing,
            "output_item_shape" => Self::OutputItemShape,
            "output_item_id" => Self::OutputItemId,
            "output_item_status" => Self::OutputItemStatus,
            "output_count" => Self::OutputCount,
            "output_index" => Self::OutputIndex,
            "output_identity" => Self::OutputIdentity,
            "output_done" => Self::OutputDone,
            "message_completion" => Self::MessageCompletion,
            "message_text" => Self::MessageText,
            "final_message_count" => Self::FinalMessageCount,
            "final_message_missing" => Self::FinalMessageMissing,
            "canonicalization" => Self::Canonicalization,
            _ => Self::Unknown,
        }
    }
}

impl serde::Serialize for ProviderResponseLedgerReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.code())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseMessageTextReason {
    InvalidTerminalMessageShape,
    TerminalObservedTextMismatch,
    ApplicablePhaseMismatch,
}

impl ProviderResponseMessageTextReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidTerminalMessageShape => "invalid_terminal_message_shape",
            Self::TerminalObservedTextMismatch => "terminal_observed_text_mismatch",
            Self::ApplicablePhaseMismatch => "applicable_phase_mismatch",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "invalid_terminal_message_shape" => Some(Self::InvalidTerminalMessageShape),
            "terminal_observed_text_mismatch" => Some(Self::TerminalObservedTextMismatch),
            "applicable_phase_mismatch" => Some(Self::ApplicablePhaseMismatch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationResponseStatus {
    Missing,
    Null,
    Completed,
    InProgress,
    Failed,
    Incomplete,
    Other,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationItemKind {
    Missing,
    Null,
    Message,
    Reasoning,
    Compaction,
    Other,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationItemStatus {
    Missing,
    Null,
    InProgress,
    Completed,
    Incomplete,
    Other,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationPhase {
    Missing,
    Null,
    Commentary,
    FinalAnswer,
    Other,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationIdPresence {
    Missing,
    Null,
    Empty,
    Present,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationIdRelation {
    NotComparable,
    BothAbsent,
    TerminalOnly,
    ObservedOnly,
    Equal,
    Mismatch,
}

impl ProviderResponseReconciliationIdRelation {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotComparable => "not_comparable",
            Self::BothAbsent => "both_absent",
            Self::TerminalOnly => "terminal_only",
            Self::ObservedOnly => "observed_only",
            Self::Equal => "equal",
            Self::Mismatch => "mismatch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationContentPresence {
    Missing,
    Null,
    Array,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationTextPresence {
    Missing,
    Null,
    String,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationObservedTextState {
    NotObserved,
    Delta,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseReconciliationTextRelation {
    NotComparable,
    Equal,
    Mismatch,
    TerminalOnly,
    ObservedOnly,
}

impl ProviderResponseReconciliationTextRelation {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotComparable => "not_comparable",
            Self::Equal => "equal",
            Self::Mismatch => "mismatch",
            Self::TerminalOnly => "terminal_only",
            Self::ObservedOnly => "observed_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderResponseCompletedReconciliation {
    pub(crate) branch: ProviderResponseMessageTextReason,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) response_created_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) response_in_progress_sequence: Option<u64>,
    pub(crate) response_completed_sequence: u64,
    pub(crate) response_status: ProviderResponseReconciliationResponseStatus,
    pub(crate) terminal_output_count: u64,
    pub(crate) observed_lifecycle_count: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) terminal_output_index: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) observed_lifecycle_index: Option<u64>,
    pub(crate) terminal_item_kind: ProviderResponseReconciliationItemKind,
    pub(crate) observed_item_kind: ProviderResponseReconciliationItemKind,
    pub(crate) terminal_item_status: ProviderResponseReconciliationItemStatus,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) observed_lifecycle_state: Option<ProviderResponseOutputIdentityLifecycleState>,
    pub(crate) terminal_phase: ProviderResponseReconciliationPhase,
    pub(crate) observed_phase: ProviderResponseReconciliationPhase,
    pub(crate) terminal_id_presence: ProviderResponseReconciliationIdPresence,
    pub(crate) observed_id_presence: ProviderResponseReconciliationIdPresence,
    pub(crate) id_relation: ProviderResponseReconciliationIdRelation,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) mapping_basis: Option<ProviderResponseOutputIdentityMappingBasis>,
    pub(crate) terminal_content_presence: ProviderResponseReconciliationContentPresence,
    pub(crate) terminal_content_part_count: u64,
    pub(crate) terminal_output_text_part_count: u64,
    pub(crate) terminal_refusal_part_count: u64,
    pub(crate) terminal_other_part_count: u64,
    pub(crate) terminal_malformed_part_count: u64,
    pub(crate) terminal_text_presence: ProviderResponseReconciliationTextPresence,
    pub(crate) terminal_text_bytes: u64,
    pub(crate) observed_text_state: ProviderResponseReconciliationObservedTextState,
    pub(crate) observed_text_bytes: u64,
    pub(crate) text_relation: ProviderResponseReconciliationTextRelation,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) output_item_added_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) content_part_added_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) first_text_delta_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) last_text_delta_sequence: Option<u64>,
    pub(crate) text_delta_count: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) text_done_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) content_part_done_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) output_item_done_sequence: Option<u64>,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer)
}

impl ProviderResponseCompletedReconciliation {
    pub fn branch(self) -> ProviderResponseMessageTextReason {
        self.branch
    }

    pub fn response_completed_sequence(self) -> u64 {
        self.response_completed_sequence
    }

    pub fn terminal_output_count(self) -> u64 {
        self.terminal_output_count
    }

    pub fn observed_lifecycle_count(self) -> u64 {
        self.observed_lifecycle_count
    }

    pub fn terminal_output_index(self) -> Option<u64> {
        self.terminal_output_index
    }

    pub fn observed_lifecycle_index(self) -> Option<u64> {
        self.observed_lifecycle_index
    }

    pub fn id_relation(self) -> ProviderResponseReconciliationIdRelation {
        self.id_relation
    }

    pub fn text_relation(self) -> ProviderResponseReconciliationTextRelation {
        self.text_relation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityReason {
    DuplicateTerminalId,
    KnownIdBeforeTerminalOrdinal,
    NonMonotonicLifecycleMapping,
    SparseLifecycleMapping,
    UnreconciledObservedMessage,
    KindAtTerminalOrdinal,
    SameIndexIdConflict,
}

impl ProviderResponseOutputIdentityReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::DuplicateTerminalId => "duplicate_terminal_id",
            Self::KnownIdBeforeTerminalOrdinal => "known_id_before_terminal_ordinal",
            Self::NonMonotonicLifecycleMapping => "non_monotonic_lifecycle_mapping",
            Self::SparseLifecycleMapping => "sparse_lifecycle_mapping",
            Self::UnreconciledObservedMessage => "unreconciled_observed_message",
            Self::KindAtTerminalOrdinal => "kind_at_terminal_ordinal",
            Self::SameIndexIdConflict => "same_index_id_conflict",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "duplicate_terminal_id" => Some(Self::DuplicateTerminalId),
            "known_id_before_terminal_ordinal" => Some(Self::KnownIdBeforeTerminalOrdinal),
            "non_monotonic_lifecycle_mapping" => Some(Self::NonMonotonicLifecycleMapping),
            "sparse_lifecycle_mapping" => Some(Self::SparseLifecycleMapping),
            "unreconciled_observed_message" => Some(Self::UnreconciledObservedMessage),
            "kind_at_terminal_ordinal" => Some(Self::KindAtTerminalOrdinal),
            "same_index_id_conflict" => Some(Self::SameIndexIdConflict),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityMappingBasis {
    KnownId,
    OrdinalMissingId,
    OrdinalUnknownId,
}

impl ProviderResponseOutputIdentityMappingBasis {
    pub fn code(self) -> &'static str {
        match self {
            Self::KnownId => "known_id",
            Self::OrdinalMissingId => "ordinal_missing_id",
            Self::OrdinalUnknownId => "ordinal_unknown_id",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "known_id" => Some(Self::KnownId),
            "ordinal_missing_id" => Some(Self::OrdinalMissingId),
            "ordinal_unknown_id" => Some(Self::OrdinalUnknownId),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityKindPair {
    TerminalMessageOverReasoning,
    TerminalMessageOverCompaction,
    Other,
}

impl ProviderResponseOutputIdentityKindPair {
    pub fn code(self) -> &'static str {
        match self {
            Self::TerminalMessageOverReasoning => "terminal_message_over_reasoning",
            Self::TerminalMessageOverCompaction => "terminal_message_over_compaction",
            Self::Other => "other",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "terminal_message_over_reasoning" => Some(Self::TerminalMessageOverReasoning),
            "terminal_message_over_compaction" => Some(Self::TerminalMessageOverCompaction),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityObservedMessageRelation {
    None,
    #[serde(rename = "next_after_contiguous_nontext")]
    NextAfterContiguousNonText,
    SameOrdinal,
    Other,
}

impl ProviderResponseOutputIdentityObservedMessageRelation {
    pub fn code(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NextAfterContiguousNonText => "next_after_contiguous_nontext",
            Self::SameOrdinal => "same_ordinal",
            Self::Other => "other",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "none" => Some(Self::None),
            "next_after_contiguous_nontext" => Some(Self::NextAfterContiguousNonText),
            "same_ordinal" => Some(Self::SameOrdinal),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityObservedTextRelation {
    NotObserved,
    Match,
    Mismatch,
}

impl ProviderResponseOutputIdentityObservedTextRelation {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotObserved => "not_observed",
            Self::Match => "match",
            Self::Mismatch => "mismatch",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "not_observed" => Some(Self::NotObserved),
            "match" => Some(Self::Match),
            "mismatch" => Some(Self::Mismatch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityLifecycleState {
    AddedOnly,
    Done,
}

impl ProviderResponseOutputIdentityLifecycleState {
    pub fn code(self) -> &'static str {
        match self {
            Self::AddedOnly => "added_only",
            Self::Done => "done",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "added_only" => Some(Self::AddedOnly),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

const PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityObservedMessageDistance {
    None,
    SameOrdinal,
    One,
    MoreThanOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputIdentityPhaseRelation {
    Exact,
    Mismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderResponseOutputIdentityObservedSpanEntry {
    pub(crate) ordinal: u64,
    pub(crate) kind: ProviderResponseReconciliationItemKind,
    pub(crate) id_presence: ProviderResponseReconciliationIdPresence,
    pub(crate) lifecycle_state: ProviderResponseOutputIdentityLifecycleState,
}

impl ProviderResponseOutputIdentityObservedSpanEntry {
    pub(crate) const fn new(
        ordinal: u64,
        kind: ProviderResponseReconciliationItemKind,
        id_presence: ProviderResponseReconciliationIdPresence,
        lifecycle_state: ProviderResponseOutputIdentityLifecycleState,
    ) -> Self {
        Self {
            ordinal,
            kind,
            id_presence,
            lifecycle_state,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderResponseOutputIdentityObservedSpan {
    entries: [Option<ProviderResponseOutputIdentityObservedSpanEntry>;
        PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT],
    len: u8,
}

impl ProviderResponseOutputIdentityObservedSpan {
    pub(crate) fn collect_bounded(
        terminal_ordinal: u64,
        entries: impl IntoIterator<Item = ProviderResponseOutputIdentityObservedSpanEntry>,
    ) -> (Self, u64, bool, bool) {
        let mut bounded = [None; PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT];
        let mut count = 0_u64;
        let mut expected_ordinal = terminal_ordinal;
        let mut contiguous = true;

        for entry in entries {
            contiguous &= entry.ordinal == expected_ordinal;
            expected_ordinal = entry.ordinal.saturating_add(1);
            if let Ok(index) = usize::try_from(count) {
                if index < bounded.len() {
                    bounded[index] = Some(entry);
                }
            }
            count = count.saturating_add(1);
        }

        let len = count.min(PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT as u64) as u8;
        (
            Self {
                entries: bounded,
                len,
            },
            count,
            count > PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT as u64,
            contiguous,
        )
    }

    fn entries(&self) -> impl Iterator<Item = &ProviderResponseOutputIdentityObservedSpanEntry> {
        self.entries[..usize::from(self.len)]
            .iter()
            .filter_map(Option::as_ref)
    }
}

impl serde::Serialize for ProviderResponseOutputIdentityObservedSpan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_seq(self.entries())
    }
}

impl<'de> serde::Deserialize<'de> for ProviderResponseOutputIdentityObservedSpan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BoundedObservedSpanVisitor;

        impl<'de> serde::de::Visitor<'de> for BoundedObservedSpanVisitor {
            type Value = ProviderResponseOutputIdentityObservedSpan;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    formatter,
                    "at most {PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT} observed output identities"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut entries = [None; PROVIDER_RESPONSE_OUTPUT_IDENTITY_OBSERVED_SPAN_LIMIT];
                let mut len = 0_usize;
                while let Some(entry) = sequence.next_element()? {
                    if len == entries.len() {
                        return Err(serde::de::Error::invalid_length(len + 1, &self));
                    }
                    entries[len] = Some(entry);
                    len += 1;
                }
                Ok(ProviderResponseOutputIdentityObservedSpan {
                    entries,
                    len: len as u8,
                })
            }
        }

        deserializer.deserialize_seq(BoundedObservedSpanVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderResponseOutputIdentityStructure {
    pub(crate) terminal_id_presence: ProviderResponseReconciliationIdPresence,
    pub(crate) terminal_ordinal: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) first_observed_message_ordinal: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) observed_message_delta: Option<u64>,
    pub(crate) observed_message_distance: ProviderResponseOutputIdentityObservedMessageDistance,
    pub(crate) observed_span: ProviderResponseOutputIdentityObservedSpan,
    pub(crate) observed_span_count: u64,
    pub(crate) observed_span_truncated: bool,
    pub(crate) observed_span_contiguous: bool,
    pub(crate) terminal_phase: ProviderResponseReconciliationPhase,
    pub(crate) observed_phase: ProviderResponseReconciliationPhase,
    pub(crate) phase_relation: ProviderResponseOutputIdentityPhaseRelation,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) matched_message_lifecycle_state:
        Option<ProviderResponseOutputIdentityLifecycleState>,
    pub(crate) matched_message_text_state: ProviderResponseReconciliationObservedTextState,
    pub(crate) terminal_text_bytes: u64,
    pub(crate) observed_text_bytes: u64,
    pub(crate) terminal_output_count: u64,
    pub(crate) terminal_final_message_count: u64,
    pub(crate) observed_lifecycle_count: u64,
    pub(crate) observed_message_count: u64,
    pub(crate) unreconciled_observed_message_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ProviderResponseOutputIdentityDetail {
    mapping_basis: ProviderResponseOutputIdentityMappingBasis,
    kind_pair: ProviderResponseOutputIdentityKindPair,
    observed_message_relation: ProviderResponseOutputIdentityObservedMessageRelation,
    observed_text_relation: ProviderResponseOutputIdentityObservedTextRelation,
    resolved_lifecycle_state: ProviderResponseOutputIdentityLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    structure: Option<ProviderResponseOutputIdentityStructure>,
}

impl ProviderResponseOutputIdentityDetail {
    pub const fn new(
        mapping_basis: ProviderResponseOutputIdentityMappingBasis,
        kind_pair: ProviderResponseOutputIdentityKindPair,
        observed_message_relation: ProviderResponseOutputIdentityObservedMessageRelation,
        observed_text_relation: ProviderResponseOutputIdentityObservedTextRelation,
        resolved_lifecycle_state: ProviderResponseOutputIdentityLifecycleState,
    ) -> Self {
        Self {
            mapping_basis,
            kind_pair,
            observed_message_relation,
            observed_text_relation,
            resolved_lifecycle_state,
            structure: None,
        }
    }

    pub(crate) const fn with_structure(
        self,
        structure: ProviderResponseOutputIdentityStructure,
    ) -> Self {
        Self {
            structure: Some(structure),
            ..self
        }
    }

    pub fn mapping_basis(self) -> ProviderResponseOutputIdentityMappingBasis {
        self.mapping_basis
    }

    pub fn kind_pair(self) -> ProviderResponseOutputIdentityKindPair {
        self.kind_pair
    }

    pub fn observed_message_relation(
        self,
    ) -> ProviderResponseOutputIdentityObservedMessageRelation {
        self.observed_message_relation
    }

    pub fn observed_text_relation(self) -> ProviderResponseOutputIdentityObservedTextRelation {
        self.observed_text_relation
    }

    pub fn resolved_lifecycle_state(self) -> ProviderResponseOutputIdentityLifecycleState {
        self.resolved_lifecycle_state
    }

    pub fn structure(self) -> Option<ProviderResponseOutputIdentityStructure> {
        self.structure
    }

    fn from_codes(
        mapping_basis: &str,
        kind_pair: &str,
        observed_message_relation: &str,
        observed_text_relation: &str,
        resolved_lifecycle_state: &str,
        structure: Option<ProviderResponseOutputIdentityStructure>,
    ) -> Option<Self> {
        Some(
            Self::new(
                ProviderResponseOutputIdentityMappingBasis::from_code(mapping_basis)?,
                ProviderResponseOutputIdentityKindPair::from_code(kind_pair)?,
                ProviderResponseOutputIdentityObservedMessageRelation::from_code(
                    observed_message_relation,
                )?,
                ProviderResponseOutputIdentityObservedTextRelation::from_code(
                    observed_text_relation,
                )?,
                ProviderResponseOutputIdentityLifecycleState::from_code(resolved_lifecycle_state)?,
            )
            .with_optional_structure(structure),
        )
    }

    const fn with_optional_structure(
        self,
        structure: Option<ProviderResponseOutputIdentityStructure>,
    ) -> Self {
        Self { structure, ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseOutputItemShapeReason {
    Message,
    Reasoning,
    Compaction,
}

impl ProviderResponseOutputItemShapeReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Reasoning => "reasoning",
            Self::Compaction => "compaction",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "message" => Some(Self::Message),
            "reasoning" => Some(Self::Reasoning),
            "compaction" => Some(Self::Compaction),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderResponseEventDiagnostic {
    event_type: ProviderResponseEventType,
    reason: ProviderResponseEventReason,
    response_ledger_reason: Option<ProviderResponseLedgerReason>,
    response_message_text_reason: Option<ProviderResponseMessageTextReason>,
    response_completed_reconciliation: Option<ProviderResponseCompletedReconciliation>,
    response_output_identity_reason: Option<ProviderResponseOutputIdentityReason>,
    response_output_identity_detail: Option<ProviderResponseOutputIdentityDetail>,
    response_output_item_shape_reason: Option<ProviderResponseOutputItemShapeReason>,
}

impl ProviderResponseEventDiagnostic {
    pub const fn new(
        event_type: ProviderResponseEventType,
        reason: ProviderResponseEventReason,
    ) -> Self {
        Self {
            event_type,
            reason,
            response_ledger_reason: None,
            response_message_text_reason: None,
            response_completed_reconciliation: None,
            response_output_identity_reason: None,
            response_output_identity_detail: None,
            response_output_item_shape_reason: None,
        }
    }

    const fn with_response_message_text_reason(
        self,
        response_message_text_reason: ProviderResponseMessageTextReason,
    ) -> Self {
        Self {
            response_message_text_reason: Some(response_message_text_reason),
            ..self
        }
    }

    const fn with_response_completed_reconciliation(
        self,
        response_completed_reconciliation: ProviderResponseCompletedReconciliation,
    ) -> Self {
        Self {
            response_completed_reconciliation: Some(response_completed_reconciliation),
            ..self
        }
    }

    const fn with_response_output_identity_reason(
        self,
        response_output_identity_reason: ProviderResponseOutputIdentityReason,
    ) -> Self {
        Self {
            response_output_identity_reason: Some(response_output_identity_reason),
            ..self
        }
    }

    const fn with_response_output_identity_detail(
        self,
        response_output_identity_detail: ProviderResponseOutputIdentityDetail,
    ) -> Self {
        Self {
            response_output_identity_detail: Some(response_output_identity_detail),
            ..self
        }
    }

    pub const fn with_response_ledger_reason(
        self,
        response_ledger_reason: ProviderResponseLedgerReason,
    ) -> Self {
        Self {
            response_ledger_reason: Some(response_ledger_reason),
            ..self
        }
    }

    const fn with_response_output_item_shape_reason(
        self,
        response_output_item_shape_reason: ProviderResponseOutputItemShapeReason,
    ) -> Self {
        Self {
            response_output_item_shape_reason: Some(response_output_item_shape_reason),
            ..self
        }
    }

    pub const fn unknown() -> Self {
        Self::new(
            ProviderResponseEventType::Unknown,
            ProviderResponseEventReason::Unknown,
        )
    }

    pub fn event_type(self) -> ProviderResponseEventType {
        self.event_type
    }

    pub fn reason(self) -> ProviderResponseEventReason {
        self.reason
    }

    pub fn response_ledger_reason(self) -> Option<ProviderResponseLedgerReason> {
        self.response_ledger_reason
    }

    pub fn response_message_text_reason(self) -> Option<ProviderResponseMessageTextReason> {
        self.response_message_text_reason
    }

    pub fn response_completed_reconciliation(
        self,
    ) -> Option<ProviderResponseCompletedReconciliation> {
        self.response_completed_reconciliation
    }

    pub fn response_output_identity_reason(self) -> Option<ProviderResponseOutputIdentityReason> {
        self.response_output_identity_reason
    }

    pub fn response_output_identity_detail(self) -> Option<ProviderResponseOutputIdentityDetail> {
        self.response_output_identity_detail
    }

    pub fn response_output_item_shape_reason(
        self,
    ) -> Option<ProviderResponseOutputItemShapeReason> {
        self.response_output_item_shape_reason
    }

    fn from_message(message: &str) -> Self {
        const PREFIX: &str = " [response_event_type=";

        let Some((_, suffix)) = message.rsplit_once(PREFIX) else {
            return Self::unknown();
        };
        let Some(suffix) = suffix.strip_suffix(']') else {
            return Self::unknown();
        };
        let mut fields = suffix.split("; ");
        let Some(event_type) = fields.next() else {
            return Self::unknown();
        };
        let Some(reason) = fields
            .next()
            .and_then(|field| field.strip_prefix("response_event_reason="))
        else {
            return Self::unknown();
        };
        let mut response_ledger_reason = None;
        let mut response_message_text_reason = None;
        let mut response_completed_reconciliation = None;
        let mut response_output_identity_reason = None;
        let mut mapping_basis = None;
        let mut kind_pair = None;
        let mut observed_message_relation = None;
        let mut observed_text_relation = None;
        let mut lifecycle_state = None;
        let mut structure = None;
        for field in fields {
            if let Some(code) = field.strip_prefix("response_ledger_reason=") {
                response_ledger_reason = Some(ProviderResponseLedgerReason::from_code(code));
            } else if let Some(code) = field.strip_prefix("response_message_text_reason=") {
                response_message_text_reason = ProviderResponseMessageTextReason::from_code(code);
            } else if let Some(value) = field.strip_prefix("response_completed_reconciliation=") {
                response_completed_reconciliation =
                    serde_json::from_str::<ProviderResponseCompletedReconciliation>(value).ok();
            } else if let Some(code) = field.strip_prefix("response_output_identity_reason=") {
                response_output_identity_reason =
                    ProviderResponseOutputIdentityReason::from_code(code);
            } else if let Some(code) = field.strip_prefix("response_output_identity_mapping_basis=")
            {
                mapping_basis = Some(code);
            } else if let Some(code) = field.strip_prefix("response_output_identity_kind_pair=") {
                kind_pair = Some(code);
            } else if let Some(code) =
                field.strip_prefix("response_output_identity_observed_message_relation=")
            {
                observed_message_relation = Some(code);
            } else if let Some(code) =
                field.strip_prefix("response_output_identity_observed_text_relation=")
            {
                observed_text_relation = Some(code);
            } else if let Some(code) =
                field.strip_prefix("response_output_identity_lifecycle_state=")
            {
                lifecycle_state = Some(code);
            } else if let Some(value) = field.strip_prefix("response_output_identity_structure=") {
                structure = Some(match structure {
                    None => {
                        serde_json::from_str::<ProviderResponseOutputIdentityStructure>(value).ok()
                    }
                    Some(_) => None,
                });
            }
        }
        let response_output_identity_detail = match (
            mapping_basis,
            kind_pair,
            observed_message_relation,
            observed_text_relation,
            lifecycle_state,
            structure,
        ) {
            (
                Some(mapping),
                Some(kind),
                Some(message),
                Some(text),
                Some(lifecycle),
                None | Some(Some(_)),
            ) => ProviderResponseOutputIdentityDetail::from_codes(
                mapping,
                kind,
                message,
                text,
                lifecycle,
                structure.flatten(),
            ),
            _ => None,
        };
        let diagnostic = Self::new(
            ProviderResponseEventType::from_code(event_type),
            ProviderResponseEventReason::from_code(reason),
        );
        let diagnostic = match response_ledger_reason {
            Some(reason)
                if (diagnostic.event_type == ProviderResponseEventType::ResponseCompleted
                    && diagnostic.reason == ProviderResponseEventReason::LedgerMismatch)
                    || (diagnostic.event_type
                        == ProviderResponseEventType::ResponseOutputItemAdded
                        && diagnostic.reason == ProviderResponseEventReason::LifecycleIdentity) =>
            {
                diagnostic.with_response_ledger_reason(reason)
            }
            _ => diagnostic,
        };
        let diagnostic = match response_output_identity_reason {
            Some(reason)
                if diagnostic.event_type == ProviderResponseEventType::ResponseCompleted
                    && diagnostic.reason == ProviderResponseEventReason::LedgerMismatch
                    && diagnostic.response_ledger_reason
                        == Some(ProviderResponseLedgerReason::OutputIdentity) =>
            {
                diagnostic.with_response_output_identity_reason(reason)
            }
            _ => diagnostic,
        };
        let diagnostic = match response_message_text_reason {
            Some(reason)
                if diagnostic.event_type == ProviderResponseEventType::ResponseCompleted
                    && diagnostic.reason == ProviderResponseEventReason::LedgerMismatch
                    && diagnostic.response_ledger_reason
                        == Some(ProviderResponseLedgerReason::MessageText) =>
            {
                diagnostic.with_response_message_text_reason(reason)
            }
            _ => diagnostic,
        };
        let diagnostic = match response_completed_reconciliation {
            Some(reconciliation)
                if diagnostic.event_type == ProviderResponseEventType::ResponseCompleted
                    && diagnostic.reason == ProviderResponseEventReason::LedgerMismatch
                    && diagnostic.response_ledger_reason
                        == Some(ProviderResponseLedgerReason::MessageText)
                    && diagnostic.response_message_text_reason == Some(reconciliation.branch()) =>
            {
                diagnostic.with_response_completed_reconciliation(reconciliation)
            }
            _ => diagnostic,
        };
        let diagnostic = match response_output_identity_detail {
            Some(detail)
                if diagnostic.response_output_identity_reason
                    == Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal) =>
            {
                diagnostic.with_response_output_identity_detail(detail)
            }
            _ => diagnostic,
        };
        if diagnostic.event_type == ProviderResponseEventType::ResponseCompleted
            && diagnostic.reason == ProviderResponseEventReason::LedgerMismatch
            && diagnostic.response_ledger_reason
                == Some(ProviderResponseLedgerReason::OutputItemShape)
        {
            const SHAPE_PREFIX: &str = " [response_output_item_shape_reason=";
            if let Some(shape_reason) = message
                .rsplit_once(SHAPE_PREFIX)
                .and_then(|(_, suffix)| suffix.split_once(']'))
                .and_then(|(code, _)| ProviderResponseOutputItemShapeReason::from_code(code))
            {
                return diagnostic.with_response_output_item_shape_reason(shape_reason);
            }
        }
        diagnostic
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("provider {kind:?}: {message}")]
pub struct ProviderFault {
    kind: ProviderFaultKind,
    code: ProviderFaultCode,
    message: String,
}

impl ProviderFault {
    pub fn retryable_transport(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderFaultKind::RetryableTransport,
            code: ProviderFaultCode::Transport,
            message: message.into(),
        }
    }

    pub fn model_rejected(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderFaultKind::ModelRejected,
            code: ProviderFaultCode::ModelRejected,
            message: message.into(),
        }
    }

    /// Returns a copy with a structural observability code attached.
    ///
    /// This does not change the coarse kind and therefore cannot opt a failure
    /// into or out of a caller's retry policy.
    pub fn with_code(self, code: ProviderFaultCode) -> Self {
        Self { code, ..self }
    }

    pub fn kind(&self) -> ProviderFaultKind {
        self.kind
    }

    pub fn code(&self) -> ProviderFaultCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn response_event_diagnostic(&self) -> Option<ProviderResponseEventDiagnostic> {
        (self.code == ProviderFaultCode::ResponseEventShape)
            .then(|| ProviderResponseEventDiagnostic::from_message(&self.message))
    }
}

#[cfg(test)]
mod response_output_identity_diagnostic_tests {
    use super::{ProviderFault, ProviderFaultCode};

    const LEGACY_IDENTITY_DIAGNOSTIC: &str = "response_event_type=response.completed; response_event_reason=ledger_mismatch; \
         response_ledger_reason=output_identity; \
         response_output_identity_reason=kind_at_terminal_ordinal; \
         response_output_identity_mapping_basis=ordinal_missing_id; \
         response_output_identity_kind_pair=terminal_message_over_reasoning; \
         response_output_identity_observed_message_relation=next_after_contiguous_nontext; \
         response_output_identity_observed_text_relation=match; \
         response_output_identity_lifecycle_state=done";
    const VALID_IDENTITY_STRUCTURE: &str = concat!(
        "{\"terminal_id_presence\":\"null\",\"terminal_ordinal\":0,",
        "\"first_observed_message_ordinal\":1,\"observed_message_delta\":1,",
        "\"observed_message_distance\":\"one\",\"observed_span\":[",
        "{\"ordinal\":0,\"kind\":\"reasoning\",\"id_presence\":\"present\",",
        "\"lifecycle_state\":\"done\"},{\"ordinal\":1,\"kind\":\"message\",",
        "\"id_presence\":\"present\",\"lifecycle_state\":\"done\"}],",
        "\"observed_span_count\":2,\"observed_span_truncated\":false,",
        "\"observed_span_contiguous\":true,\"terminal_phase\":\"missing\",",
        "\"observed_phase\":\"missing\",\"phase_relation\":\"exact\",",
        "\"matched_message_lifecycle_state\":\"done\",",
        "\"matched_message_text_state\":\"completed\",\"terminal_text_bytes\":8,",
        "\"observed_text_bytes\":8,\"terminal_output_count\":1,",
        "\"terminal_final_message_count\":1,\"observed_lifecycle_count\":2,",
        "\"observed_message_count\":1,\"unreconciled_observed_message_count\":1}"
    );

    fn diagnosed_fault(extra: &str) -> ProviderFault {
        ProviderFault::model_rejected(format!(
            "sanitized identity failure [{LEGACY_IDENTITY_DIAGNOSTIC}{extra}]"
        ))
        .with_code(ProviderFaultCode::ResponseEventShape)
    }

    #[test]
    fn present_malformed_output_identity_structure_suppresses_typed_detail() {
        let diagnostic = diagnosed_fault("; response_output_identity_structure={}")
            .response_event_diagnostic()
            .expect("response event diagnostics remain available");

        assert!(diagnostic.response_output_identity_detail().is_none());
    }

    #[test]
    fn absent_output_identity_structure_preserves_legacy_typed_detail() {
        let detail = diagnosed_fault("")
            .response_event_diagnostic()
            .and_then(|diagnostic| diagnostic.response_output_identity_detail())
            .expect("legacy five-field identity diagnostics remain typed");

        assert!(detail.structure().is_none());
    }

    #[test]
    fn duplicate_output_identity_structure_suppresses_typed_detail() {
        let diagnostic = diagnosed_fault(&format!(
            "; response_output_identity_structure={VALID_IDENTITY_STRUCTURE}; \
             response_output_identity_structure={VALID_IDENTITY_STRUCTURE}"
        ))
        .response_event_diagnostic()
        .expect("response event diagnostics remain available");

        assert!(diagnostic.response_output_identity_detail().is_none());
    }
}

#[cfg(test)]
mod projection_validation_tests {
    use crate::{
        pom::{BlockChildren, Document, ResolvedDocument},
        transcript::{CanonicalInputItem, ConversationRole},
    };

    use super::{
        RenderedProjection, RenderedProjectionDiffMarker, RenderedProjectionFragment,
        RenderedProjectionItemTemplate, RenderedProjectionNode,
    };

    fn item() -> CanonicalInputItem {
        CanonicalInputItem::message(
            ConversationRole::User,
            ResolvedDocument::new(BlockChildren::new()),
        )
    }

    fn fragment(diff_index: usize) -> RenderedProjectionFragment {
        RenderedProjectionFragment::Diff {
            diff_index,
            authored: Document::new(BlockChildren::new()),
            complete: ResolvedDocument::new(BlockChildren::new()),
        }
    }

    #[test]
    fn malformed_diff_templates_fail_projection_validation() {
        let out_of_bounds_template = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item()],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![RenderedProjectionItemTemplate::new(1, vec![fragment(0)])],
        );
        assert!(RenderedProjection::from_nodes(vec![out_of_bounds_template]).is_err());

        let duplicate_template = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item()],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![
                RenderedProjectionItemTemplate::new(0, vec![fragment(0)]),
                RenderedProjectionItemTemplate::new(0, vec![fragment(0)]),
            ],
        );
        assert!(RenderedProjection::from_nodes(vec![duplicate_template]).is_err());

        let out_of_bounds_fragment = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item()],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![RenderedProjectionItemTemplate::new(0, vec![fragment(1)])],
        );
        assert!(RenderedProjection::from_nodes(vec![out_of_bounds_fragment]).is_err());

        let mismatched_fragment_item = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item(), item()],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![RenderedProjectionItemTemplate::new(1, vec![fragment(0)])],
        );
        assert!(RenderedProjection::from_nodes(vec![mismatched_fragment_item]).is_err());

        let orphaned_diff = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item()],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            Vec::new(),
        );
        assert!(RenderedProjection::from_nodes(vec![orphaned_diff]).is_err());
    }
}
