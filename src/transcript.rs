//! Provider-neutral canonical model history.
//!
//! The transcript owns semantic POM and causal tool facts. Provider request
//! DTOs, remote cursors, retry state, and SDK types belong to later layers.

use std::collections::HashSet;

#[cfg(test)]
use std::cell::Cell;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::pom::ResolvedDocument;

/// Instruction authority carried by one canonical POM item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionAuthority {
    System,
    Developer,
}

/// Conversation participant carried by one canonical POM item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
}

/// Semantic phase of provider-authored assistant text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantPhase {
    Commentary,
    FinalAnswer,
}

/// Provider-scoped fact retained for audit or same-provider replay.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderExtension {
    provider: String,
    capability: String,
    schema_version: u32,
    payload: Value,
}

impl ProviderExtension {
    pub fn new(
        provider: impl Into<String>,
        capability: impl Into<String>,
        schema_version: u32,
        payload: Value,
    ) -> Result<Self, CanonicalTranscriptError> {
        let provider = validate_identifier("provider", provider.into())?;
        let capability = validate_identifier("capability", capability.into())?;
        if schema_version == 0 {
            return Err(CanonicalTranscriptError::ZeroSchemaVersion);
        }
        Ok(Self {
            provider,
            capability,
            schema_version,
            payload,
        })
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn capability(&self) -> &str {
        &self.capability
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn payload(&self) -> &Value {
        &self.payload
    }

    fn validate(&self) -> Result<(), CanonicalTranscriptError> {
        validate_identifier_ref("provider", &self.provider)?;
        validate_identifier_ref("capability", &self.capability)?;
        if self.schema_version == 0 {
            return Err(CanonicalTranscriptError::ZeroSchemaVersion);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ProviderExtension {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireProviderExtension {
            provider: String,
            capability: String,
            schema_version: u32,
            payload: Value,
        }

        let wire = WireProviderExtension::deserialize(deserializer)?;
        let extension = Self::new(
            wire.provider,
            wire.capability,
            wire.schema_version,
            wire.payload,
        )
        .map_err(D::Error::custom)?;
        Ok(extension)
    }
}

/// One ordered provider-neutral input fact.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum CanonicalInputItem {
    Instruction {
        authority: InstructionAuthority,
        pom: ResolvedDocument,
    },
    Message {
        role: ConversationRole,
        pom: ResolvedDocument,
    },
    AssistantText {
        text: String,
        phase: Option<AssistantPhase>,
    },
    ToolCall {
        call_id: String,
        name: String,
        raw_arguments: String,
    },
    ToolResult {
        call_id: String,
        content: String,
    },
    ProviderExtension(ProviderExtension),
}

impl<'de> Deserialize<'de> for CanonicalInputItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
        enum WireCanonicalInputItem {
            Instruction {
                authority: InstructionAuthority,
                pom: ResolvedDocument,
            },
            Message {
                role: ConversationRole,
                pom: ResolvedDocument,
            },
            AssistantText {
                text: String,
                phase: Option<AssistantPhase>,
            },
            ToolCall {
                call_id: String,
                name: String,
                raw_arguments: String,
            },
            ToolResult {
                call_id: String,
                content: String,
            },
            ProviderExtension(ProviderExtension),
        }

        let item = match WireCanonicalInputItem::deserialize(deserializer)? {
            WireCanonicalInputItem::Instruction { authority, pom } => {
                Self::Instruction { authority, pom }
            }
            WireCanonicalInputItem::Message { role, pom } => Self::Message { role, pom },
            WireCanonicalInputItem::AssistantText { text, phase } => {
                Self::AssistantText { text, phase }
            }
            WireCanonicalInputItem::ToolCall {
                call_id,
                name,
                raw_arguments,
            } => Self::ToolCall {
                call_id,
                name,
                raw_arguments,
            },
            WireCanonicalInputItem::ToolResult { call_id, content } => {
                Self::ToolResult { call_id, content }
            }
            WireCanonicalInputItem::ProviderExtension(extension) => {
                Self::ProviderExtension(extension)
            }
        };
        item.validate().map_err(D::Error::custom)?;
        Ok(item)
    }
}

impl CanonicalInputItem {
    pub fn instruction(authority: InstructionAuthority, pom: ResolvedDocument) -> Self {
        Self::Instruction { authority, pom }
    }

    pub fn message(role: ConversationRole, pom: ResolvedDocument) -> Self {
        Self::Message { role, pom }
    }

    pub fn assistant_text(text: impl Into<String>, phase: Option<AssistantPhase>) -> Self {
        Self::AssistantText {
            text: text.into(),
            phase,
        }
    }

    pub fn tool_call(
        call_id: impl Into<String>,
        name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Result<Self, CanonicalTranscriptError> {
        let call_id = validate_identifier("tool call id", call_id.into())?;
        let name = validate_identifier("tool name", name.into())?;
        let raw_arguments = raw_arguments.into();
        validate_tool_arguments(&call_id, &raw_arguments)?;
        Ok(Self::ToolCall {
            call_id,
            name,
            raw_arguments,
        })
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<Self, CanonicalTranscriptError> {
        Ok(Self::ToolResult {
            call_id: validate_identifier("tool call id", call_id.into())?,
            content: content.into(),
        })
    }

    pub fn provider_extension(extension: ProviderExtension) -> Self {
        Self::ProviderExtension(extension)
    }

    fn validate(&self) -> Result<(), CanonicalTranscriptError> {
        match self {
            Self::Instruction { .. } | Self::Message { .. } | Self::AssistantText { .. } => Ok(()),
            Self::ToolCall {
                call_id,
                name,
                raw_arguments,
            } => {
                validate_identifier_ref("tool call id", call_id)?;
                validate_identifier_ref("tool name", name)?;
                validate_tool_arguments(call_id, raw_arguments)
            }
            Self::ToolResult { call_id, .. } => validate_identifier_ref("tool call id", call_id),
            Self::ProviderExtension(extension) => extension.validate(),
        }
    }
}

/// Session-local ordinal of the last canonical transcript item.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TranscriptRevision(u64);

impl TranscriptRevision {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Immutable ordered canonical history.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CanonicalTranscript {
    items: Vec<CanonicalInputItem>,
}

impl CanonicalTranscript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revision(&self) -> TranscriptRevision {
        TranscriptRevision(
            u64::try_from(self.items.len()).expect("Vec length always fits into u64"),
        )
    }

    pub fn items(&self) -> &[CanonicalInputItem] {
        &self.items
    }

    /// Builds an immutable transcript from an owned sequence in linear work.
    pub(crate) fn try_from_items(
        items: Vec<CanonicalInputItem>,
    ) -> Result<Self, CanonicalTranscriptError> {
        Self::validate_sequence(&items)?;
        Ok(Self { items })
    }

    pub(crate) fn try_from_item_refs<'a>(
        items: impl IntoIterator<Item = &'a CanonicalInputItem>,
    ) -> Result<Self, CanonicalTranscriptError> {
        let items = items
            .into_iter()
            .map(|item| {
                record_construction_work(1);
                item.clone()
            })
            .collect();
        Self::try_from_items(items)
    }

    pub(crate) fn into_items(self) -> Vec<CanonicalInputItem> {
        self.items
    }

    pub(crate) fn validate_sequence<'a>(
        items: impl IntoIterator<Item = &'a CanonicalInputItem>,
    ) -> Result<(), CanonicalTranscriptError> {
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
        for item in items {
            record_construction_work(1);
            item.validate()?;
            match item {
                CanonicalInputItem::ToolCall { call_id, .. } => {
                    if !calls.insert(call_id.as_str()) {
                        return Err(CanonicalTranscriptError::DuplicateToolCall {
                            call_id: call_id.clone(),
                        });
                    }
                }
                CanonicalInputItem::ToolResult { call_id, .. } => {
                    if !calls.contains(call_id.as_str()) {
                        return Err(CanonicalTranscriptError::UnknownToolCall {
                            call_id: call_id.clone(),
                        });
                    }
                    if !results.insert(call_id.as_str()) {
                        return Err(CanonicalTranscriptError::DuplicateToolResult {
                            call_id: call_id.clone(),
                        });
                    }
                }
                CanonicalInputItem::Instruction { .. }
                | CanonicalInputItem::Message { .. }
                | CanonicalInputItem::AssistantText { .. }
                | CanonicalInputItem::ProviderExtension(_) => {}
            }
        }
        Ok(())
    }

    pub fn appended(&self, item: CanonicalInputItem) -> Result<Self, CanonicalTranscriptError> {
        item.validate()?;
        self.validate_causal_append(&item)?;
        record_construction_work(self.items.len().saturating_add(1));
        let mut items = self.items.clone();
        items.push(item);
        Ok(Self { items })
    }

    fn validate_causal_append(
        &self,
        item: &CanonicalInputItem,
    ) -> Result<(), CanonicalTranscriptError> {
        match item {
            CanonicalInputItem::ToolCall { call_id, .. } => {
                if self.items.iter().any(|item| {
                    matches!(item, CanonicalInputItem::ToolCall { call_id: prior, .. } if prior == call_id)
                }) {
                    return Err(CanonicalTranscriptError::DuplicateToolCall {
                        call_id: call_id.clone(),
                    });
                }
            }
            CanonicalInputItem::ToolResult { call_id, .. } => {
                let has_call = self.items.iter().any(|item| {
                    matches!(item, CanonicalInputItem::ToolCall { call_id: prior, .. } if prior == call_id)
                });
                if !has_call {
                    return Err(CanonicalTranscriptError::UnknownToolCall {
                        call_id: call_id.clone(),
                    });
                }
                if self.items.iter().any(|item| {
                    matches!(item, CanonicalInputItem::ToolResult { call_id: prior, .. } if prior == call_id)
                }) {
                    return Err(CanonicalTranscriptError::DuplicateToolResult {
                        call_id: call_id.clone(),
                    });
                }
            }
            CanonicalInputItem::Instruction { .. }
            | CanonicalInputItem::Message { .. }
            | CanonicalInputItem::AssistantText { .. }
            | CanonicalInputItem::ProviderExtension(_) => {}
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CanonicalTranscript {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireTranscript {
            items: Vec<CanonicalInputItem>,
        }

        let wire = WireTranscript::deserialize(deserializer)?;
        Self::try_from_items(wire.items).map_err(D::Error::custom)
    }
}

/// Invalid canonical history or item input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CanonicalTranscriptError {
    #[error("invalid {kind} `{value}`; identifiers must be non-empty ASCII tokens")]
    InvalidIdentifier { kind: &'static str, value: String },

    #[error("tool call `{call_id}` arguments are not valid JSON: {message}")]
    InvalidToolArguments { call_id: String, message: String },

    #[error("tool call `{call_id}` arguments must be a JSON object")]
    ToolArgumentsMustBeObject { call_id: String },

    #[error("provider extension schema version must be greater than zero")]
    ZeroSchemaVersion,

    #[error("canonical tool call `{call_id}` already exists")]
    DuplicateToolCall { call_id: String },

    #[error("canonical tool result references unknown call `{call_id}`")]
    UnknownToolCall { call_id: String },

    #[error("canonical tool result for call `{call_id}` already exists")]
    DuplicateToolResult { call_id: String },
}

fn validate_identifier(
    kind: &'static str,
    value: String,
) -> Result<String, CanonicalTranscriptError> {
    validate_identifier_ref(kind, &value)?;
    Ok(value)
}

fn validate_identifier_ref(
    kind: &'static str,
    value: &str,
) -> Result<(), CanonicalTranscriptError> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        });
    if valid {
        Ok(())
    } else {
        Err(CanonicalTranscriptError::InvalidIdentifier {
            kind,
            value: value.to_owned(),
        })
    }
}

fn validate_tool_arguments(
    call_id: &str,
    raw_arguments: &str,
) -> Result<(), CanonicalTranscriptError> {
    let value: Value = serde_json::from_str(raw_arguments).map_err(|error| {
        CanonicalTranscriptError::InvalidToolArguments {
            call_id: call_id.to_owned(),
            message: error.to_string(),
        }
    })?;
    if value.is_object() {
        Ok(())
    } else {
        Err(CanonicalTranscriptError::ToolArgumentsMustBeObject {
            call_id: call_id.to_owned(),
        })
    }
}

#[cfg(test)]
thread_local! {
    static CONSTRUCTION_WORK: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn record_construction_work(additional: usize) {
    CONSTRUCTION_WORK.with(|work| work.set(work.get().saturating_add(additional)));
}

#[cfg(not(test))]
fn record_construction_work(_additional: usize) {}

#[cfg(test)]
pub(crate) fn reset_construction_work() {
    CONSTRUCTION_WORK.with(|work| work.set(0));
}

#[cfg(test)]
pub(crate) fn take_construction_work() -> usize {
    CONSTRUCTION_WORK.with(|work| work.replace(0))
}
