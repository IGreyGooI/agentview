use std::collections::{BTreeMap, HashMap, HashSet};

use ::async_openai::types::responses::{
    MessagePhase as OpenAiMessagePhase, OutputContent, OutputItem, OutputMessageContent,
    OutputStatus, OutputTextContent, ReasoningItem, ResponseOutputTextAnnotationAddedEvent,
    SummaryPart,
};
#[cfg(any(feature = "legacy-provider-port", test))]
use serde_json::json;
use serde_json::{Map, Value};

#[cfg(any(feature = "legacy-provider-port", test))]
use crate::transcript::CanonicalInputItem;
#[cfg(feature = "legacy-provider-port")]
use crate::{component::execution::ProviderResponseLedgerReason, transcript::ProviderExtension};
use crate::{
    component::execution::{
        ProviderFault, ProviderResponseCompletedReconciliation, ProviderResponseMessageTextReason,
        ProviderResponseOutputIdentityDetail, ProviderResponseOutputIdentityKindPair,
        ProviderResponseOutputIdentityLifecycleState, ProviderResponseOutputIdentityMappingBasis,
        ProviderResponseOutputIdentityObservedMessageDistance,
        ProviderResponseOutputIdentityObservedMessageRelation,
        ProviderResponseOutputIdentityObservedSpan,
        ProviderResponseOutputIdentityObservedSpanEntry,
        ProviderResponseOutputIdentityObservedTextRelation,
        ProviderResponseOutputIdentityPhaseRelation, ProviderResponseOutputIdentityReason,
        ProviderResponseOutputIdentityStructure, ProviderResponseReconciliationContentPresence,
        ProviderResponseReconciliationIdPresence, ProviderResponseReconciliationIdRelation,
        ProviderResponseReconciliationItemKind, ProviderResponseReconciliationItemStatus,
        ProviderResponseReconciliationObservedTextState, ProviderResponseReconciliationPhase,
        ProviderResponseReconciliationResponseStatus, ProviderResponseReconciliationTextPresence,
        ProviderResponseReconciliationTextRelation,
    },
    transcript::AssistantPhase,
};

#[cfg(feature = "legacy-provider-port")]
const OPENAI_PROVIDER: &str = "openai";
#[cfg(feature = "legacy-provider-port")]
const REASONING_CAPABILITY: &str = "reasoning.encrypted_content";
#[cfg(feature = "legacy-provider-port")]
const REASONING_SCHEMA_VERSION: u32 = 1;

struct TextOutputKey {
    item_id: String,
    output_index: u64,
    content_index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputKind {
    Message,
    Reasoning,
    Compaction,
}

impl OutputKind {
    fn shape_reason(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Reasoning => "reasoning",
            Self::Compaction => "compaction",
        }
    }
}

struct OutputLifecycle {
    id: Option<String>,
    kind: OutputKind,
    phase: Option<AssistantPhase>,
    text: Option<TextLifecycle>,
    done: bool,
    sealed_private: Option<SealedOpenAiPrivateOutput>,
    added_sequence: Option<u64>,
    done_sequence: Option<u64>,
}

#[derive(Default)]
struct TextLifecycle {
    delta: String,
    delta_observed: bool,
    completed: Option<String>,
    content_part_added: bool,
    content_part_done: Option<OutputTextContent>,
    content_part_added_sequence: Option<u64>,
    first_delta_sequence: Option<u64>,
    last_delta_sequence: Option<u64>,
    delta_count: u64,
    text_done_sequence: Option<u64>,
    content_part_done_sequence: Option<u64>,
}

#[cfg(any(feature = "legacy-provider-port", test))]
pub(super) struct CompletedOpenAiOutput {
    pub(super) output_items: Vec<CanonicalInputItem>,
    pub(super) wire_items: Vec<Value>,
    pub(super) final_text: String,
    #[cfg(feature = "legacy-provider-port")]
    pub(super) output_text_bytes: usize,
}

/// A supported private provider item becomes causal history at item.done.
/// `response.completed` may only reconcile this exact normalized fact.
#[derive(Clone)]
pub(super) struct SealedOpenAiPrivateOutput {
    pub(super) output_index: u64,
    #[cfg(feature = "legacy-provider-port")]
    pub(super) output_item: Option<CanonicalInputItem>,
    pub(super) wire_item: Value,
}

struct ParsedOutputItem {
    output_index: u64,
    id: Option<String>,
    kind: OutputKind,
    raw: Value,
    body: ParsedOutputItemBody,
}

enum ParsedOutputItemBody {
    Sdk(OutputItem),
    TerminalMessage(TerminalMessage),
}

struct TerminalMessage {
    phase: Option<OpenAiMessagePhase>,
    output_text: OutputTextContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ResponseOutputLedgerError {
    MissingResponseObject,
    InvalidResponse,
    ResponseIdentity,
    MissingAuthoritativeOutput,
    OutputIndexRange,
    MissingOutputItemType,
    UnsupportedOutputItem,
    InvalidOutputItemShape(OutputKind),
    EmptyOutputItemId,
    InvalidCompletedItem,
    TerminalOutputIdentity(ProviderResponseOutputIdentityReason),
    TerminalOutputKindConflict(Box<ProviderResponseOutputIdentityDetail>),
    InvalidMessageText {
        terminal_output_index: u64,
    },
    TerminalMessageMismatch {
        terminal_output_index: Option<u64>,
        observed_lifecycle_index: u64,
    },
    TerminalMessagePhaseMismatch {
        terminal_output_index: u64,
        observed_lifecycle_index: u64,
    },
    #[cfg(any(feature = "legacy-provider-port", test))]
    MultipleFinalMessages,
    MissingFinalMessage,
    Canonicalization,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ResponseOutputLedgerFailure {
    error: ResponseOutputLedgerError,
    response_completed_reconciliation: Option<Box<ProviderResponseCompletedReconciliation>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResponseOutputItemLifecycleError {
    InvalidOutputIndex,
    MissingItem,
    MissingItemType,
    UnsupportedItemType,
    InvalidItemShape,
    EmptyItemId,
    InvalidAddedItem,
    ReusedIdentity,
}

impl ResponseOutputItemLifecycleError {
    #[cfg(feature = "legacy-provider-port")]
    pub(super) fn response_ledger_reason(self) -> ProviderResponseLedgerReason {
        match self {
            Self::InvalidOutputIndex => ProviderResponseLedgerReason::OutputIndex,
            Self::MissingItem => ProviderResponseLedgerReason::OutputItemMissing,
            Self::MissingItemType | Self::UnsupportedItemType => {
                ProviderResponseLedgerReason::OutputItemType
            }
            Self::InvalidItemShape => ProviderResponseLedgerReason::OutputItemShape,
            Self::EmptyItemId => ProviderResponseLedgerReason::OutputItemId,
            Self::InvalidAddedItem => ProviderResponseLedgerReason::OutputItemStatus,
            Self::ReusedIdentity => ProviderResponseLedgerReason::OutputIdentity,
        }
    }

    pub(super) fn into_provider_fault(self) -> ProviderFault {
        ProviderFault::model_rejected(match self {
            Self::InvalidOutputIndex => {
                "OpenAI streaming event is missing an integer identity field"
            }
            Self::MissingItem => "OpenAI output item lifecycle is missing item",
            Self::MissingItemType => "OpenAI output item is missing type",
            Self::UnsupportedItemType => {
                "native or unsupported OpenAI output items are not supported by this adapter version"
            }
            Self::InvalidItemShape => "OpenAI output item has an invalid supported shape",
            Self::EmptyItemId => "OpenAI output item has an empty id",
            Self::InvalidAddedItem => "OpenAI output item has an invalid supported shape",
            Self::ReusedIdentity => "OpenAI output_item.added reused an output identity",
        })
    }
}

impl ResponseOutputLedgerError {
    #[cfg(feature = "legacy-provider-port")]
    pub(super) fn response_ledger_reason(&self) -> ProviderResponseLedgerReason {
        match self {
            Self::MissingResponseObject | Self::InvalidResponse => {
                ProviderResponseLedgerReason::ResponseObject
            }
            Self::ResponseIdentity => ProviderResponseLedgerReason::ResponseIdentity,
            Self::MissingAuthoritativeOutput => ProviderResponseLedgerReason::AuthoritativeOutput,
            Self::MissingOutputItemType | Self::UnsupportedOutputItem => {
                ProviderResponseLedgerReason::OutputItemType
            }
            Self::InvalidOutputItemShape(_) => ProviderResponseLedgerReason::OutputItemShape,
            Self::EmptyOutputItemId => ProviderResponseLedgerReason::OutputItemId,
            Self::InvalidCompletedItem => ProviderResponseLedgerReason::OutputItemStatus,
            Self::OutputIndexRange => ProviderResponseLedgerReason::OutputIndex,
            Self::TerminalOutputIdentity(_) | Self::TerminalOutputKindConflict(_) => {
                ProviderResponseLedgerReason::OutputIdentity
            }
            Self::InvalidMessageText { .. }
            | Self::TerminalMessageMismatch { .. }
            | Self::TerminalMessagePhaseMismatch { .. } => {
                ProviderResponseLedgerReason::MessageText
            }
            #[cfg(any(feature = "legacy-provider-port", test))]
            Self::MultipleFinalMessages => ProviderResponseLedgerReason::FinalMessageCount,
            Self::MissingFinalMessage => ProviderResponseLedgerReason::FinalMessageMissing,
            Self::Canonicalization => ProviderResponseLedgerReason::Canonicalization,
        }
    }

    pub(super) fn into_provider_fault(self) -> ProviderFault {
        if let Self::InvalidOutputItemShape(kind) = self {
            return ProviderFault::model_rejected(format!(
                "OpenAI output item has an invalid supported shape \
                 [response_output_item_shape_reason={}]",
                kind.shape_reason()
            ));
        }
        ProviderFault::model_rejected(match self {
            Self::MissingResponseObject => "response.completed is missing response object",
            Self::InvalidResponse => "response.completed has invalid id or status",
            Self::ResponseIdentity => {
                "OpenAI response identity changed before response.completed"
            }
            Self::MissingAuthoritativeOutput => {
                "response.completed is missing authoritative output"
            }
            Self::OutputIndexRange => "OpenAI response output index exceeded supported range",
            Self::MissingOutputItemType => "OpenAI output item is missing type",
            Self::UnsupportedOutputItem => {
                "native or unsupported OpenAI output items are not supported by this adapter version"
            }
            Self::InvalidOutputItemShape(_) => unreachable!("handled above"),
            Self::EmptyOutputItemId => "OpenAI output item has an empty id",
            Self::InvalidCompletedItem => {
                "OpenAI output_item.done has an invalid status or content shape"
            }
            Self::TerminalOutputIdentity(_) | Self::TerminalOutputKindConflict(_) => {
                "OpenAI terminal output identity conflicts with observed output items"
            }
            Self::InvalidMessageText { .. } => {
                "OpenAI assistant message must contain exactly one output_text part"
            }
            Self::TerminalMessageMismatch { .. } => {
                "OpenAI terminal message does not match streamed text"
            }
            Self::TerminalMessagePhaseMismatch { .. } => {
                "OpenAI terminal message phase does not match observed message"
            }
            #[cfg(any(feature = "legacy-provider-port", test))]
            Self::MultipleFinalMessages => {
                "OpenAI response contains more than one final assistant message"
            }
            Self::MissingFinalMessage => "OpenAI response has no final assistant message",
            Self::Canonicalization => "OpenAI terminal output could not be canonicalized",
        })
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn response_output_identity_reason(
        &self,
    ) -> Option<ProviderResponseOutputIdentityReason> {
        match self {
            Self::TerminalOutputIdentity(reason) => Some(*reason),
            Self::TerminalOutputKindConflict(_) => {
                Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
            }
            _ => None,
        }
    }

    pub(super) fn response_message_text_reason(&self) -> Option<ProviderResponseMessageTextReason> {
        match self {
            Self::InvalidMessageText { .. } => {
                Some(ProviderResponseMessageTextReason::InvalidTerminalMessageShape)
            }
            Self::TerminalMessageMismatch { .. } => {
                Some(ProviderResponseMessageTextReason::TerminalObservedTextMismatch)
            }
            Self::TerminalMessagePhaseMismatch { .. } => {
                Some(ProviderResponseMessageTextReason::ApplicablePhaseMismatch)
            }
            _ => None,
        }
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn response_output_identity_detail(
        &self,
    ) -> Option<ProviderResponseOutputIdentityDetail> {
        match self {
            Self::TerminalOutputKindConflict(detail) => Some(**detail),
            _ => None,
        }
    }

    fn message_text_focus(&self) -> Option<(Option<u64>, Option<u64>)> {
        match self {
            Self::InvalidMessageText {
                terminal_output_index,
            } => Some((Some(*terminal_output_index), None)),
            Self::TerminalMessageMismatch {
                terminal_output_index,
                observed_lifecycle_index,
            } => Some((*terminal_output_index, Some(*observed_lifecycle_index))),
            Self::TerminalMessagePhaseMismatch {
                terminal_output_index,
                observed_lifecycle_index,
            } => Some((
                Some(*terminal_output_index),
                Some(*observed_lifecycle_index),
            )),
            _ => None,
        }
    }
}

impl ResponseOutputLedgerFailure {
    #[cfg(test)]
    fn ledger_error(self) -> ResponseOutputLedgerError {
        self.error
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(super) fn response_ledger_reason(&self) -> ProviderResponseLedgerReason {
        self.error.response_ledger_reason()
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn response_message_text_reason(&self) -> Option<ProviderResponseMessageTextReason> {
        self.error.response_message_text_reason()
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn response_completed_reconciliation(
        &self,
    ) -> Option<ProviderResponseCompletedReconciliation> {
        self.response_completed_reconciliation.as_deref().copied()
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn response_output_identity_reason(
        &self,
    ) -> Option<ProviderResponseOutputIdentityReason> {
        self.error.response_output_identity_reason()
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn response_output_identity_detail(
        &self,
    ) -> Option<ProviderResponseOutputIdentityDetail> {
        self.error.response_output_identity_detail()
    }

    pub(super) fn into_provider_fault(self) -> ProviderFault {
        self.error.into_provider_fault()
    }
}

#[derive(Default)]
pub(super) struct OpenAiOutputLedger {
    items: BTreeMap<u64, OutputLifecycle>,
    output_identities: OutputIdentityIndex,
    response_id: Option<String>,
    response_created: bool,
    response_in_progress: bool,
    response_created_sequence: Option<u64>,
    response_in_progress_sequence: Option<u64>,
}

#[derive(Default)]
struct OutputIdentityIndex {
    output_indices: HashSet<u64>,
    output_ids: HashMap<String, u64>,
    #[cfg(test)]
    operations: usize,
}

impl OutputIdentityIndex {
    fn admit(&mut self, output_index: u64, id: Option<&str>) -> bool {
        #[cfg(test)]
        {
            self.operations += 1;
        }
        if !self.output_indices.insert(output_index) {
            return false;
        }

        if let Some(id) = id {
            #[cfg(test)]
            {
                self.operations += 1;
            }
            if self.output_ids.contains_key(id) {
                #[cfg(test)]
                {
                    self.operations += 1;
                }
                let removed = self.output_indices.remove(&output_index);
                debug_assert!(removed, "newly admitted output index must be present");
                return false;
            }
            self.output_ids.insert(id.to_owned(), output_index);
        }
        true
    }

    fn bind_present_id(&mut self, output_index: u64, id: Option<&str>) -> bool {
        let Some(id) = id else {
            return true;
        };
        match self.output_ids.get(id) {
            Some(index) => *index == output_index,
            None => {
                self.output_ids.insert(id.to_owned(), output_index);
                true
            }
        }
    }

    fn index_for_id(&self, id: &str) -> Option<u64> {
        self.output_ids.get(id).copied()
    }

    #[cfg(test)]
    fn operations(&self) -> usize {
        self.operations
    }
}

impl OpenAiOutputLedger {
    pub(super) fn message_text(&self, output_index: u64) -> Option<(&str, Option<AssistantPhase>)> {
        let lifecycle = self.items.get(&output_index)?;
        let text = lifecycle.text.as_ref()?;
        Some((text.completed.as_deref()?, lifecycle.phase))
    }

    pub(super) fn message_phase(&self, output_index: u64) -> Option<AssistantPhase> {
        self.items.get(&output_index)?.phase
    }

    pub(super) fn record_response_created(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ProviderFault> {
        if self.response_created {
            return Err(invalid_lifecycle(
                "OpenAI response emitted more than one response.created event",
            ));
        }
        let id = response_lifecycle_id(payload, "in_progress")?;
        self.bind_response_id(id)?;
        self.response_created = true;
        self.response_created_sequence = optional_sequence(payload);
        Ok(())
    }

    pub(super) fn record_response_in_progress(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ProviderFault> {
        if self.response_in_progress {
            return Err(invalid_lifecycle(
                "OpenAI response emitted more than one response.in_progress event",
            ));
        }
        let id = response_lifecycle_id(payload, "in_progress")?;
        self.bind_response_id(id)?;
        self.response_in_progress = true;
        self.response_in_progress_sequence = optional_sequence(payload);
        Ok(())
    }

    pub(super) fn record_added(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ResponseOutputItemLifecycleError> {
        let parsed = parse_lifecycle_item(payload)?;
        validate_observed_item(&parsed)?;
        if !self
            .output_identities
            .admit(parsed.output_index, parsed.id.as_deref())
        {
            return Err(ResponseOutputItemLifecycleError::ReusedIdentity);
        }
        self.items.insert(
            parsed.output_index,
            OutputLifecycle {
                id: parsed.id,
                kind: parsed.kind,
                phase: assistant_phase(&parsed.body),
                text: (parsed.kind == OutputKind::Message).then(TextLifecycle::default),
                done: false,
                sealed_private: None,
                added_sequence: optional_sequence(payload),
                done_sequence: None,
            },
        );
        Ok(())
    }

    pub(super) fn record_done(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<Option<SealedOpenAiPrivateOutput>, ProviderFault> {
        let parsed = parse_lifecycle_item(payload)
            .map_err(ResponseOutputItemLifecycleError::into_provider_fault)?;
        validate_observed_item(&parsed)
            .map_err(ResponseOutputItemLifecycleError::into_provider_fault)?;
        let sealed_private = sealed_private_output(&parsed)
            .map_err(ResponseOutputLedgerError::into_provider_fault)?;
        let Some(lifecycle) = self.items.get(&parsed.output_index) else {
            return Err(invalid_lifecycle(
                "OpenAI output_item.done did not follow output_item.added",
            ));
        };
        if lifecycle.kind != parsed.kind
            || present_ids_conflict(lifecycle.id.as_deref(), parsed.id.as_deref())
        {
            return Err(invalid_lifecycle(
                "OpenAI output item identity changed between added and done",
            ));
        }
        if lifecycle.done {
            return Err(invalid_lifecycle(
                "OpenAI output item emitted more than one done event",
            ));
        }
        if !self
            .output_identities
            .bind_present_id(parsed.output_index, parsed.id.as_deref())
        {
            return Err(invalid_lifecycle(
                "OpenAI output_item.done reused an output identity",
            ));
        }
        let retained_message_phase = if parsed.kind == OutputKind::Message
            && lifecycle.text.as_ref().and_then(observed_text).is_some()
        {
            let done_phase = assistant_phase(&parsed.body);
            if !message_phase_classes_match(lifecycle.phase, done_phase) {
                return Err(invalid_lifecycle(
                    "OpenAI output item phase changed between added and done",
                ));
            }
            Some(strongest_message_phase(lifecycle.phase, done_phase))
        } else {
            None
        };
        let lifecycle = self
            .items
            .get_mut(&parsed.output_index)
            .expect("validated output lifecycle");
        if lifecycle.id.is_none() {
            lifecycle.id = parsed.id;
        }
        if let Some(phase) = retained_message_phase {
            lifecycle.phase = phase;
        }
        lifecycle.done = true;
        lifecycle.sealed_private = sealed_private.clone();
        lifecycle.done_sequence = optional_sequence(payload);
        Ok(sealed_private)
    }

    pub(super) fn record_content_part_added(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ProviderFault> {
        let part = output_text_part(payload)?;
        if !part.text.is_empty() {
            return Err(invalid_lifecycle(
                "OpenAI content_part.added output text was not empty",
            ));
        }
        let lifecycle = self.message_lifecycle_mut(payload)?;
        let text = lifecycle.text.as_mut().expect("message text lifecycle");
        if text.content_part_added {
            return Err(invalid_lifecycle(
                "OpenAI message emitted more than one content_part.added event",
            ));
        }
        text.content_part_added = true;
        text.content_part_added_sequence = optional_sequence(payload);
        Ok(())
    }

    pub(super) fn record_content_part_done(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ProviderFault> {
        let part = output_text_part(payload)?;
        let lifecycle = self.message_lifecycle_mut(payload)?;
        let text = lifecycle.text.as_mut().expect("message text lifecycle");
        if !text.content_part_added {
            return Err(invalid_lifecycle(
                "OpenAI content_part.done did not follow content_part.added",
            ));
        }
        if text.content_part_done.is_some() {
            return Err(invalid_lifecycle(
                "OpenAI message emitted more than one content_part.done event",
            ));
        }
        if text.completed.as_deref() != Some(part.text.as_str()) {
            return Err(invalid_lifecycle(
                "OpenAI content_part.done does not match output_text.done",
            ));
        }
        text.content_part_done = Some(part);
        text.content_part_done_sequence = optional_sequence(payload);
        Ok(())
    }

    pub(super) fn record_text_delta(
        &mut self,
        payload: &Map<String, Value>,
        delta: &str,
    ) -> Result<bool, ProviderFault> {
        let sequence = optional_sequence(payload);
        let lifecycle = self.message_lifecycle_mut(payload)?;
        let text = lifecycle.text.as_mut().expect("message text lifecycle");
        if text.completed.is_some() {
            return Err(invalid_lifecycle(
                "OpenAI text delta followed output_text.done",
            ));
        }
        text.delta.push_str(delta);
        text.delta_observed = true;
        text.first_delta_sequence = text.first_delta_sequence.or(sequence);
        text.last_delta_sequence = sequence.or(text.last_delta_sequence);
        text.delta_count = text.delta_count.saturating_add(1);
        Ok(lifecycle.phase != Some(AssistantPhase::Commentary))
    }

    pub(super) fn record_text_done(
        &mut self,
        payload: &Map<String, Value>,
        completed: &str,
    ) -> Result<(), ProviderFault> {
        let sequence = optional_sequence(payload);
        let lifecycle = self.message_lifecycle_mut(payload)?;
        let text = lifecycle.text.as_mut().expect("message text lifecycle");
        if text.completed.is_some() {
            return Err(invalid_lifecycle(
                "OpenAI response produced more than one text completion for one message",
            ));
        }
        if text.delta != completed {
            return Err(invalid_lifecycle(
                "OpenAI output_text.done does not equal accumulated deltas",
            ));
        }
        text.completed = Some(completed.to_owned());
        text.text_done_sequence = sequence;
        Ok(())
    }

    pub(super) fn record_text_annotation_added(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<(), ProviderFault> {
        serde_json::from_value::<ResponseOutputTextAnnotationAddedEvent>(Value::Object(
            payload.clone(),
        ))
        .map_err(|_| {
            invalid_lifecycle("OpenAI output text annotation event has an invalid shape")
        })?;
        let lifecycle = self.message_lifecycle_mut(payload)?;
        let text = lifecycle.text.as_ref().expect("message text lifecycle");
        if text.completed.is_some() {
            return Err(invalid_lifecycle(
                "OpenAI output text annotation followed output_text.done",
            ));
        }
        Ok(())
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    pub(super) fn complete(
        &self,
        payload: &Map<String, Value>,
    ) -> Result<CompletedOpenAiOutput, ResponseOutputLedgerFailure> {
        self.complete_inner(payload).map_err(|error| {
            let response_completed_reconciliation =
                self.completed_reconciliation_snapshot(payload, &error);
            ResponseOutputLedgerFailure {
                error,
                response_completed_reconciliation: response_completed_reconciliation.map(Box::new),
            }
        })
    }

    /// Validates a Frame-native terminal response without requiring a primary
    /// text output. Tool-only, reasoning-only, and commentary-only reactions
    /// still have to reconcile every observed public/private lifecycle.
    pub(super) fn validate_completed_allowing_no_primary(
        &self,
        payload: &Map<String, Value>,
    ) -> Result<(), ResponseOutputLedgerFailure> {
        self.validate_completed_allowing_no_primary_inner(payload)
            .map_err(|error| {
                let response_completed_reconciliation =
                    self.completed_reconciliation_snapshot(payload, &error);
                ResponseOutputLedgerFailure {
                    error,
                    response_completed_reconciliation: response_completed_reconciliation
                        .map(Box::new),
                }
            })
    }

    #[cfg(any(feature = "legacy-provider-port", test))]
    fn complete_inner(
        &self,
        payload: &Map<String, Value>,
    ) -> Result<CompletedOpenAiOutput, ResponseOutputLedgerError> {
        self.reconcile_completed_terminal(payload)?;
        self.completed_from_sealed_lifecycle()
    }

    fn validate_completed_allowing_no_primary_inner(
        &self,
        payload: &Map<String, Value>,
    ) -> Result<(), ResponseOutputLedgerError> {
        self.reconcile_completed_terminal(payload)?;
        for lifecycle in self.items.values() {
            if !lifecycle.done {
                return Err(ResponseOutputLedgerError::InvalidCompletedItem);
            }
            match lifecycle.kind {
                OutputKind::Reasoning | OutputKind::Compaction
                    if lifecycle.sealed_private.is_none() =>
                {
                    return Err(ResponseOutputLedgerError::InvalidCompletedItem);
                }
                OutputKind::Reasoning | OutputKind::Compaction => {}
                OutputKind::Message
                    if lifecycle.text.as_ref().and_then(observed_text).is_none() =>
                {
                    return Err(ResponseOutputLedgerError::MissingFinalMessage);
                }
                OutputKind::Message => {}
            }
        }
        Ok(())
    }

    fn reconcile_completed_terminal(
        &self,
        payload: &Map<String, Value>,
    ) -> Result<(), ResponseOutputLedgerError> {
        let response = completed_response(payload)?;
        if self
            .response_id
            .as_deref()
            .is_some_and(|id| id != response.id)
        {
            return Err(ResponseOutputLedgerError::ResponseIdentity);
        }
        if let Some(terminal_value) = response.object.get("output") {
            let terminal = terminal_value
                .as_array()
                .ok_or(ResponseOutputLedgerError::MissingAuthoritativeOutput)?;
            let mut parsed = terminal
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let output_index = u64::try_from(index)
                        .map_err(|_| ResponseOutputLedgerError::OutputIndexRange)?;
                    let parsed = parse_terminal_output_item(output_index, item.clone())?;
                    validate_terminal_item(&parsed)?;
                    Ok(parsed)
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.reconcile_terminal_items(&mut parsed)?;
        }
        Ok(())
    }

    /// A Responses completion frame is metadata, not a second output source.
    /// Some compatible streams omit `response.output` entirely after every
    /// item was already sealed. In that form, only the completed lifecycle may
    /// supply the final public text; no terminal item is invented or adopted.
    #[cfg(any(feature = "legacy-provider-port", test))]
    fn completed_from_sealed_lifecycle(
        &self,
    ) -> Result<CompletedOpenAiOutput, ResponseOutputLedgerError> {
        let mut final_text = None;
        #[cfg(feature = "legacy-provider-port")]
        let mut output_text_bytes = 0_usize;
        let mut output_items = Vec::new();

        for lifecycle in self.items.values() {
            if !lifecycle.done {
                return Err(ResponseOutputLedgerError::InvalidCompletedItem);
            }
            match lifecycle.kind {
                OutputKind::Reasoning | OutputKind::Compaction
                    if lifecycle.sealed_private.is_none() =>
                {
                    return Err(ResponseOutputLedgerError::InvalidCompletedItem);
                }
                OutputKind::Reasoning | OutputKind::Compaction => {}
                OutputKind::Message => {
                    let text = lifecycle
                        .text
                        .as_ref()
                        .and_then(observed_text)
                        .ok_or(ResponseOutputLedgerError::MissingFinalMessage)?;
                    #[cfg(feature = "legacy-provider-port")]
                    {
                        output_text_bytes = output_text_bytes.saturating_add(text.len());
                    }
                    let phase = lifecycle.phase;
                    output_items.push(CanonicalInputItem::assistant_text(text, phase));
                    #[cfg(any(feature = "legacy-provider-port", test))]
                    if phase != Some(AssistantPhase::Commentary)
                        && final_text.replace(text.to_owned()).is_some()
                    {
                        return Err(ResponseOutputLedgerError::MultipleFinalMessages);
                    }
                }
            }
        }

        Ok(CompletedOpenAiOutput {
            output_items,
            wire_items: Vec::new(),
            final_text: final_text.ok_or(ResponseOutputLedgerError::MissingFinalMessage)?,
            #[cfg(feature = "legacy-provider-port")]
            output_text_bytes,
        })
    }

    fn bind_response_id(&mut self, id: String) -> Result<(), ProviderFault> {
        if self
            .response_id
            .as_deref()
            .is_some_and(|current| current != id)
        {
            return Err(invalid_lifecycle(
                "OpenAI response identity changed during its lifecycle",
            ));
        }
        self.response_id = Some(id);
        Ok(())
    }

    fn reconcile_terminal_items(
        &self,
        terminal: &mut [ParsedOutputItem],
    ) -> Result<(), ResponseOutputLedgerError> {
        let mut terminal_ids = HashSet::new();
        let mut reconciled_lifecycle_indices = HashSet::new();
        let mut last_reconciled_lifecycle_index: Option<u64> = None;
        let terminal_output_count = bounded_len(terminal.len());
        let terminal_final_message_count = bounded_len(
            terminal
                .iter()
                .filter(|item| {
                    item.kind == OutputKind::Message
                        && assistant_phase(&item.body) != Some(AssistantPhase::Commentary)
                })
                .count(),
        );
        for item in terminal.iter_mut() {
            if let Some(id) = item.id.as_deref() {
                if !terminal_ids.insert(id.to_owned()) {
                    return Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                        ProviderResponseOutputIdentityReason::DuplicateTerminalId,
                    ));
                }
            }
            let known_lifecycle_index = item
                .id
                .as_deref()
                .and_then(|id| self.output_identities.index_for_id(id));
            let mapping_basis = match (item.id.as_deref(), known_lifecycle_index) {
                (_, Some(_)) => ProviderResponseOutputIdentityMappingBasis::KnownId,
                (None, None) => ProviderResponseOutputIdentityMappingBasis::OrdinalMissingId,
                (Some(_), None) => ProviderResponseOutputIdentityMappingBasis::OrdinalUnknownId,
            };
            let lifecycle_index = known_lifecycle_index.unwrap_or_else(|| {
                self.items
                    .get(&item.output_index)
                    .and_then(|resolved| self.reprojected_missing_id_message_index(item, resolved))
                    .unwrap_or(item.output_index)
            });
            let lifecycle = self.items.get(&lifecycle_index);
            if let Some(lifecycle) = lifecycle {
                if lifecycle_index < item.output_index {
                    return Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                        ProviderResponseOutputIdentityReason::KnownIdBeforeTerminalOrdinal,
                    ));
                }
                if last_reconciled_lifecycle_index
                    .is_some_and(|previous| previous >= lifecycle_index)
                {
                    return Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                        ProviderResponseOutputIdentityReason::NonMonotonicLifecycleMapping,
                    ));
                }
                if lifecycle.kind != item.kind {
                    return Err(ResponseOutputLedgerError::TerminalOutputKindConflict(
                        Box::new(self.kind_conflict_detail(
                            item,
                            lifecycle,
                            mapping_basis,
                            terminal_output_count,
                            terminal_final_message_count,
                            &reconciled_lifecycle_indices,
                        )),
                    ));
                }
                if item.kind == OutputKind::Message
                    && lifecycle.text.as_ref().and_then(observed_text).is_some()
                {
                    if !message_phases_reconcile(lifecycle, item) {
                        return Err(ResponseOutputLedgerError::TerminalMessagePhaseMismatch {
                            terminal_output_index: item.output_index,
                            observed_lifecycle_index: lifecycle_index,
                        });
                    }
                    let retained_phase =
                        strongest_message_phase(lifecycle.phase, assistant_phase(&item.body));
                    retain_terminal_message_phase(&mut item.body, retained_phase);
                }
                if present_ids_conflict(lifecycle.id.as_deref(), item.id.as_deref()) {
                    return Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                        ProviderResponseOutputIdentityReason::SameIndexIdConflict,
                    ));
                }
                reconciled_lifecycle_indices.insert(lifecycle_index);
                last_reconciled_lifecycle_index = Some(lifecycle_index);
                if !lifecycle.done {
                    return Err(ResponseOutputLedgerError::InvalidCompletedItem);
                }
            } else {
                return Err(ResponseOutputLedgerError::InvalidCompletedItem);
            }
            if matches!(item.kind, OutputKind::Reasoning | OutputKind::Compaction) {
                match lifecycle.and_then(|lifecycle| lifecycle.sealed_private.as_ref()) {
                    Some(sealed) => {
                        if sealed.output_index != lifecycle_index
                            || sealed.wire_item != canonical_wire_item(item)?
                        {
                            return Err(ResponseOutputLedgerError::InvalidCompletedItem);
                        }
                    }
                    None => {
                        return Err(ResponseOutputLedgerError::InvalidCompletedItem);
                    }
                }
            }
            match &item.body {
                ParsedOutputItemBody::TerminalMessage(message) => {
                    let text = message.output_text.text.as_str();
                    let observed = lifecycle
                        .and_then(|lifecycle| lifecycle.text.as_ref())
                        .and_then(observed_text)
                        .ok_or(ResponseOutputLedgerError::TerminalMessageMismatch {
                            terminal_output_index: Some(item.output_index),
                            observed_lifecycle_index: lifecycle_index,
                        })?;
                    if observed != text {
                        return Err(ResponseOutputLedgerError::TerminalMessageMismatch {
                            terminal_output_index: Some(item.output_index),
                            observed_lifecycle_index: lifecycle_index,
                        });
                    }
                }
                ParsedOutputItemBody::Sdk(OutputItem::Reasoning(_)) => {}
                ParsedOutputItemBody::Sdk(OutputItem::Compaction(_)) => {}
                ParsedOutputItemBody::Sdk(_) => {
                    return Err(ResponseOutputLedgerError::UnsupportedOutputItem)
                }
            }
        }
        Ok(())
    }

    fn completed_reconciliation_snapshot(
        &self,
        payload: &Map<String, Value>,
        error: &ResponseOutputLedgerError,
    ) -> Option<ProviderResponseCompletedReconciliation> {
        let branch = error.response_message_text_reason()?;
        let response_completed_sequence = optional_sequence(payload)?;
        let response = payload.get("response").and_then(Value::as_object);
        let terminal_output = response
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array);
        let (terminal_output_index, observed_lifecycle_index) = error.message_text_focus()?;
        let terminal_item = terminal_output_index.and_then(|index| {
            usize::try_from(index)
                .ok()
                .and_then(|index| terminal_output.and_then(|items| items.get(index)))
        });
        let observed_lifecycle_index = observed_lifecycle_index.or_else(|| {
            terminal_output_index
                .and_then(|index| self.diagnostic_lifecycle_index(index, terminal_item))
        });
        let observed_lifecycle = observed_lifecycle_index.and_then(|index| self.items.get(&index));
        let terminal_id = terminal_item.and_then(|item| item.as_object()?.get("id"));
        let observed_id = observed_lifecycle.and_then(|item| item.id.as_deref());
        let terminal_id_value = terminal_id
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let content = terminal_content_diagnostic(terminal_item);
        let (observed_text_state, observed_text_bytes, observed_text_value) =
            observed_text_diagnostic(observed_lifecycle);

        Some(ProviderResponseCompletedReconciliation {
            branch,
            response_created_sequence: self.response_created_sequence,
            response_in_progress_sequence: self.response_in_progress_sequence,
            response_completed_sequence,
            response_status: reconciliation_response_status(
                response.and_then(|response| response.get("status")),
            ),
            terminal_output_count: terminal_output.map_or(0, |items| bounded_len(items.len())),
            observed_lifecycle_count: bounded_len(self.items.len()),
            terminal_output_index,
            observed_lifecycle_index,
            terminal_item_kind: reconciliation_item_kind(terminal_item),
            observed_item_kind: observed_lifecycle
                .map_or(ProviderResponseReconciliationItemKind::Missing, |item| {
                    reconciliation_output_kind(item.kind)
                }),
            terminal_item_status: reconciliation_item_status(
                terminal_item.and_then(|item| item.as_object()?.get("status")),
            ),
            observed_lifecycle_state: observed_lifecycle.map(|item| {
                if item.done {
                    ProviderResponseOutputIdentityLifecycleState::Done
                } else {
                    ProviderResponseOutputIdentityLifecycleState::AddedOnly
                }
            }),
            terminal_phase: reconciliation_phase(
                terminal_item.and_then(|item| item.as_object()?.get("phase")),
            ),
            observed_phase: observed_lifecycle
                .map_or(ProviderResponseReconciliationPhase::Missing, |item| {
                    reconciliation_assistant_phase(item.phase)
                }),
            terminal_id_presence: reconciliation_id_presence(terminal_id),
            observed_id_presence: if observed_lifecycle.is_none() {
                ProviderResponseReconciliationIdPresence::Missing
            } else {
                observed_id.map_or(ProviderResponseReconciliationIdPresence::Missing, |_| {
                    ProviderResponseReconciliationIdPresence::Present
                })
            },
            id_relation: reconciliation_id_relation(
                terminal_item.is_some(),
                terminal_id_value,
                observed_lifecycle.is_some(),
                observed_id,
            ),
            mapping_basis: terminal_item.and_then(|_| {
                reconciliation_mapping_basis(
                    terminal_id,
                    terminal_id_value,
                    &self.output_identities,
                )
            }),
            terminal_content_presence: content.presence,
            terminal_content_part_count: content.part_count,
            terminal_output_text_part_count: content.output_text_part_count,
            terminal_refusal_part_count: content.refusal_part_count,
            terminal_other_part_count: content.other_part_count,
            terminal_malformed_part_count: content.malformed_part_count,
            terminal_text_presence: content.text_presence,
            terminal_text_bytes: content.text_bytes,
            observed_text_state,
            observed_text_bytes,
            text_relation: reconciliation_text_relation(content.text_value, observed_text_value),
            output_item_added_sequence: observed_lifecycle.and_then(|item| item.added_sequence),
            content_part_added_sequence: observed_lifecycle
                .and_then(|item| item.text.as_ref())
                .and_then(|text| text.content_part_added_sequence),
            first_text_delta_sequence: observed_lifecycle
                .and_then(|item| item.text.as_ref())
                .and_then(|text| text.first_delta_sequence),
            last_text_delta_sequence: observed_lifecycle
                .and_then(|item| item.text.as_ref())
                .and_then(|text| text.last_delta_sequence),
            text_delta_count: observed_lifecycle
                .and_then(|item| item.text.as_ref())
                .map_or(0, |text| text.delta_count),
            text_done_sequence: observed_lifecycle
                .and_then(|item| item.text.as_ref())
                .and_then(|text| text.text_done_sequence),
            content_part_done_sequence: observed_lifecycle
                .and_then(|item| item.text.as_ref())
                .and_then(|text| text.content_part_done_sequence),
            output_item_done_sequence: observed_lifecycle.and_then(|item| item.done_sequence),
        })
    }

    fn diagnostic_lifecycle_index(
        &self,
        terminal_output_index: u64,
        terminal_item: Option<&Value>,
    ) -> Option<u64> {
        terminal_item
            .and_then(|item| item.as_object())
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .and_then(|id| self.output_identities.index_for_id(id))
            .or_else(|| {
                self.items
                    .contains_key(&terminal_output_index)
                    .then_some(terminal_output_index)
            })
    }

    fn reprojected_missing_id_message_index(
        &self,
        terminal: &ParsedOutputItem,
        resolved: &OutputLifecycle,
    ) -> Option<u64> {
        if terminal.id.is_some()
            || terminal.raw.get("id").is_some()
            || terminal.kind != OutputKind::Message
            || resolved.kind != OutputKind::Reasoning
            || !resolved.done
        {
            return None;
        }
        let text = match &terminal.body {
            ParsedOutputItemBody::TerminalMessage(message) => message.output_text.text.as_str(),
            ParsedOutputItemBody::Sdk(_) => return None,
        };

        let mut expected_index = terminal.output_index;
        for (candidate_index, candidate) in self.items.range(terminal.output_index..) {
            if *candidate_index != expected_index || !candidate.done {
                return None;
            }
            match candidate.kind {
                OutputKind::Reasoning => {
                    expected_index = candidate_index.checked_add(1)?;
                }
                OutputKind::Message => {
                    if !message_phases_reconcile(candidate, terminal) {
                        return None;
                    }
                    return candidate
                        .text
                        .as_ref()
                        .and_then(observed_text)
                        .is_some_and(|observed| observed == text)
                        .then_some(*candidate_index);
                }
                OutputKind::Compaction => return None,
            }
        }
        None
    }

    fn kind_conflict_detail(
        &self,
        terminal: &ParsedOutputItem,
        resolved: &OutputLifecycle,
        mapping_basis: ProviderResponseOutputIdentityMappingBasis,
        terminal_output_count: u64,
        terminal_final_message_count: u64,
        reconciled_lifecycle_indices: &HashSet<u64>,
    ) -> ProviderResponseOutputIdentityDetail {
        let kind_pair = match (terminal.kind, resolved.kind) {
            (OutputKind::Message, OutputKind::Reasoning) => {
                ProviderResponseOutputIdentityKindPair::TerminalMessageOverReasoning
            }
            (OutputKind::Message, OutputKind::Compaction) => {
                ProviderResponseOutputIdentityKindPair::TerminalMessageOverCompaction
            }
            _ => ProviderResponseOutputIdentityKindPair::Other,
        };
        let (observed_message_relation, observed_message_ordinal, observed_message) =
            self.observed_message_relation(terminal.output_index);
        let observed_text_relation = match (&terminal.body, observed_message) {
            (ParsedOutputItemBody::TerminalMessage(message), Some(observed)) => {
                observed.text.as_ref().and_then(observed_text).map_or(
                    ProviderResponseOutputIdentityObservedTextRelation::NotObserved,
                    |text| {
                        if text == message.output_text.text {
                            ProviderResponseOutputIdentityObservedTextRelation::Match
                        } else {
                            ProviderResponseOutputIdentityObservedTextRelation::Mismatch
                        }
                    },
                )
            }
            _ => ProviderResponseOutputIdentityObservedTextRelation::NotObserved,
        };
        let resolved_lifecycle_state = if resolved.done {
            ProviderResponseOutputIdentityLifecycleState::Done
        } else {
            ProviderResponseOutputIdentityLifecycleState::AddedOnly
        };
        let terminal_id_presence =
            reconciliation_id_presence(terminal.raw.as_object().and_then(|item| item.get("id")));
        let terminal_phase =
            reconciliation_phase(terminal.raw.as_object().and_then(|item| item.get("phase")));
        let observed_phase = observed_message
            .map_or(ProviderResponseReconciliationPhase::Missing, |message| {
                reconciliation_assistant_phase(message.phase)
            });
        let phase_relation = if terminal_phase == observed_phase {
            ProviderResponseOutputIdentityPhaseRelation::Exact
        } else {
            ProviderResponseOutputIdentityPhaseRelation::Mismatch
        };
        let observed_message_delta =
            observed_message_ordinal.and_then(|ordinal| ordinal.checked_sub(terminal.output_index));
        let observed_message_distance = match observed_message_delta {
            None => ProviderResponseOutputIdentityObservedMessageDistance::None,
            Some(0) => ProviderResponseOutputIdentityObservedMessageDistance::SameOrdinal,
            Some(1) => ProviderResponseOutputIdentityObservedMessageDistance::One,
            Some(_) => ProviderResponseOutputIdentityObservedMessageDistance::MoreThanOne,
        };
        let observed_span_end = observed_message_ordinal.unwrap_or(u64::MAX);
        let (observed_span, observed_span_count, observed_span_truncated, observed_span_contiguous) =
            ProviderResponseOutputIdentityObservedSpan::collect_bounded(
                terminal.output_index,
                self.items
                    .range(terminal.output_index..=observed_span_end)
                    .map(|(ordinal, lifecycle)| {
                        ProviderResponseOutputIdentityObservedSpanEntry::new(
                            *ordinal,
                            reconciliation_output_kind(lifecycle.kind),
                            if lifecycle.id.is_some() {
                                ProviderResponseReconciliationIdPresence::Present
                            } else {
                                ProviderResponseReconciliationIdPresence::Missing
                            },
                            if lifecycle.done {
                                ProviderResponseOutputIdentityLifecycleState::Done
                            } else {
                                ProviderResponseOutputIdentityLifecycleState::AddedOnly
                            },
                        )
                    }),
            );
        let (matched_message_text_state, observed_text_bytes, _) =
            observed_text_diagnostic(observed_message);
        let matched_message_lifecycle_state = observed_message.map(|message| {
            if message.done {
                ProviderResponseOutputIdentityLifecycleState::Done
            } else {
                ProviderResponseOutputIdentityLifecycleState::AddedOnly
            }
        });
        let terminal_text_bytes = match &terminal.body {
            ParsedOutputItemBody::TerminalMessage(message) => {
                bounded_len(message.output_text.text.len())
            }
            ParsedOutputItemBody::Sdk(_) => 0,
        };
        let observed_message_count = bounded_len(
            self.items
                .values()
                .filter(|lifecycle| lifecycle.kind == OutputKind::Message)
                .count(),
        );
        let unreconciled_observed_message_count = bounded_len(
            self.items
                .iter()
                .filter(|(ordinal, lifecycle)| {
                    lifecycle.kind == OutputKind::Message
                        && !reconciled_lifecycle_indices.contains(ordinal)
                })
                .count(),
        );
        let structure = ProviderResponseOutputIdentityStructure {
            terminal_id_presence,
            terminal_ordinal: terminal.output_index,
            first_observed_message_ordinal: observed_message_ordinal,
            observed_message_delta,
            observed_message_distance,
            observed_span,
            observed_span_count,
            observed_span_truncated,
            observed_span_contiguous,
            terminal_phase,
            observed_phase,
            phase_relation,
            matched_message_lifecycle_state,
            matched_message_text_state,
            terminal_text_bytes,
            observed_text_bytes,
            terminal_output_count,
            terminal_final_message_count,
            observed_lifecycle_count: bounded_len(self.items.len()),
            observed_message_count,
            unreconciled_observed_message_count,
        };

        ProviderResponseOutputIdentityDetail::new(
            mapping_basis,
            kind_pair,
            observed_message_relation,
            observed_text_relation,
            resolved_lifecycle_state,
        )
        .with_structure(structure)
    }

    fn observed_message_relation(
        &self,
        terminal_ordinal: u64,
    ) -> (
        ProviderResponseOutputIdentityObservedMessageRelation,
        Option<u64>,
        Option<&OutputLifecycle>,
    ) {
        let mut expected_index = terminal_ordinal;
        let mut contiguous = true;
        for (index, lifecycle) in self.items.range(terminal_ordinal..) {
            if *index != expected_index {
                contiguous = false;
            }
            if lifecycle.kind == OutputKind::Message {
                let relation = if *index == terminal_ordinal {
                    ProviderResponseOutputIdentityObservedMessageRelation::SameOrdinal
                } else if contiguous {
                    ProviderResponseOutputIdentityObservedMessageRelation::NextAfterContiguousNonText
                } else {
                    ProviderResponseOutputIdentityObservedMessageRelation::Other
                };
                return (relation, Some(*index), Some(lifecycle));
            }
            expected_index = index.saturating_add(1);
        }
        (
            ProviderResponseOutputIdentityObservedMessageRelation::None,
            None,
            None,
        )
    }

    fn message_lifecycle_mut(
        &mut self,
        payload: &Map<String, Value>,
    ) -> Result<&mut OutputLifecycle, ProviderFault> {
        let key = TextOutputKey {
            item_id: required_nonempty_string(payload, "item_id")?,
            output_index: required_u64(payload, "output_index")?,
            content_index: required_u64(payload, "content_index")?,
        };
        if key.content_index != 0 {
            return Err(invalid_lifecycle(
                "OpenAI text output used an unsupported content index",
            ));
        }
        let lifecycle = self.items.get_mut(&key.output_index).ok_or_else(|| {
            invalid_lifecycle("OpenAI text frame did not follow message output_item.added")
        })?;
        if lifecycle.kind != OutputKind::Message
            || lifecycle.id.as_deref() != Some(key.item_id.as_str())
        {
            return Err(invalid_lifecycle(
                "OpenAI text frame does not match its message output item",
            ));
        }
        if lifecycle.done {
            return Err(invalid_lifecycle(
                "OpenAI text frame followed output_item.done",
            ));
        }
        Ok(lifecycle)
    }

    #[cfg(test)]
    fn identity_admission_probes(&self) -> usize {
        self.output_identities.operations()
    }
}

struct TerminalContentDiagnostic<'a> {
    presence: ProviderResponseReconciliationContentPresence,
    part_count: u64,
    output_text_part_count: u64,
    refusal_part_count: u64,
    other_part_count: u64,
    malformed_part_count: u64,
    text_presence: ProviderResponseReconciliationTextPresence,
    text_bytes: u64,
    text_value: Option<&'a str>,
}

fn terminal_content_diagnostic(terminal_item: Option<&Value>) -> TerminalContentDiagnostic<'_> {
    let content = terminal_item
        .and_then(Value::as_object)
        .and_then(|item| item.get("content"));
    let presence = match content {
        None => ProviderResponseReconciliationContentPresence::Missing,
        Some(Value::Null) => ProviderResponseReconciliationContentPresence::Null,
        Some(Value::Array(_)) => ProviderResponseReconciliationContentPresence::Array,
        Some(_) => ProviderResponseReconciliationContentPresence::Invalid,
    };
    let parts = content.and_then(Value::as_array);
    let mut output_text_part_count = 0_u64;
    let mut refusal_part_count = 0_u64;
    let mut other_part_count = 0_u64;
    let mut malformed_part_count = 0_u64;
    let mut first_output_text = None;
    if let Some(parts) = parts {
        for part in parts {
            let Some(part) = part.as_object() else {
                malformed_part_count = malformed_part_count.saturating_add(1);
                continue;
            };
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    output_text_part_count = output_text_part_count.saturating_add(1);
                    first_output_text = first_output_text.or(Some(part));
                }
                Some("refusal") => {
                    refusal_part_count = refusal_part_count.saturating_add(1);
                }
                Some(_) => {
                    other_part_count = other_part_count.saturating_add(1);
                }
                None => {
                    malformed_part_count = malformed_part_count.saturating_add(1);
                }
            }
        }
    }
    let text = first_output_text.and_then(|part| part.get("text"));
    let text_value = text.and_then(Value::as_str);
    TerminalContentDiagnostic {
        presence,
        part_count: parts.map_or(0, |parts| bounded_len(parts.len())),
        output_text_part_count,
        refusal_part_count,
        other_part_count,
        malformed_part_count,
        text_presence: match text {
            None => ProviderResponseReconciliationTextPresence::Missing,
            Some(Value::Null) => ProviderResponseReconciliationTextPresence::Null,
            Some(Value::String(_)) => ProviderResponseReconciliationTextPresence::String,
            Some(_) => ProviderResponseReconciliationTextPresence::Invalid,
        },
        text_bytes: text_value.map_or(0, |text| bounded_len(text.len())),
        text_value,
    }
}

fn observed_text_diagnostic(
    lifecycle: Option<&OutputLifecycle>,
) -> (
    ProviderResponseReconciliationObservedTextState,
    u64,
    Option<&str>,
) {
    let Some(text) = lifecycle.and_then(|item| item.text.as_ref()) else {
        return (
            ProviderResponseReconciliationObservedTextState::NotObserved,
            0,
            None,
        );
    };
    if let Some(completed) = text.completed.as_deref() {
        return (
            ProviderResponseReconciliationObservedTextState::Completed,
            bounded_len(completed.len()),
            Some(completed),
        );
    }
    if text.delta_observed {
        return (
            ProviderResponseReconciliationObservedTextState::Delta,
            bounded_len(text.delta.len()),
            Some(text.delta.as_str()),
        );
    }
    (
        ProviderResponseReconciliationObservedTextState::NotObserved,
        0,
        None,
    )
}

fn reconciliation_response_status(
    status: Option<&Value>,
) -> ProviderResponseReconciliationResponseStatus {
    match status {
        None => ProviderResponseReconciliationResponseStatus::Missing,
        Some(Value::Null) => ProviderResponseReconciliationResponseStatus::Null,
        Some(Value::String(value)) => match value.as_str() {
            "completed" => ProviderResponseReconciliationResponseStatus::Completed,
            "in_progress" => ProviderResponseReconciliationResponseStatus::InProgress,
            "failed" => ProviderResponseReconciliationResponseStatus::Failed,
            "incomplete" => ProviderResponseReconciliationResponseStatus::Incomplete,
            _ => ProviderResponseReconciliationResponseStatus::Other,
        },
        Some(_) => ProviderResponseReconciliationResponseStatus::Invalid,
    }
}

fn reconciliation_item_kind(item: Option<&Value>) -> ProviderResponseReconciliationItemKind {
    match item {
        None => ProviderResponseReconciliationItemKind::Missing,
        Some(Value::Null) => ProviderResponseReconciliationItemKind::Null,
        Some(Value::Object(item)) => match item.get("type") {
            None => ProviderResponseReconciliationItemKind::Missing,
            Some(Value::Null) => ProviderResponseReconciliationItemKind::Null,
            Some(Value::String(value)) => match value.as_str() {
                "message" => ProviderResponseReconciliationItemKind::Message,
                "reasoning" => ProviderResponseReconciliationItemKind::Reasoning,
                "compaction" => ProviderResponseReconciliationItemKind::Compaction,
                _ => ProviderResponseReconciliationItemKind::Other,
            },
            Some(_) => ProviderResponseReconciliationItemKind::Invalid,
        },
        Some(_) => ProviderResponseReconciliationItemKind::Invalid,
    }
}

fn reconciliation_output_kind(kind: OutputKind) -> ProviderResponseReconciliationItemKind {
    match kind {
        OutputKind::Message => ProviderResponseReconciliationItemKind::Message,
        OutputKind::Reasoning => ProviderResponseReconciliationItemKind::Reasoning,
        OutputKind::Compaction => ProviderResponseReconciliationItemKind::Compaction,
    }
}

fn reconciliation_item_status(status: Option<&Value>) -> ProviderResponseReconciliationItemStatus {
    match status {
        None => ProviderResponseReconciliationItemStatus::Missing,
        Some(Value::Null) => ProviderResponseReconciliationItemStatus::Null,
        Some(Value::String(value)) => match value.as_str() {
            "in_progress" => ProviderResponseReconciliationItemStatus::InProgress,
            "completed" => ProviderResponseReconciliationItemStatus::Completed,
            "incomplete" => ProviderResponseReconciliationItemStatus::Incomplete,
            _ => ProviderResponseReconciliationItemStatus::Other,
        },
        Some(_) => ProviderResponseReconciliationItemStatus::Invalid,
    }
}

fn reconciliation_phase(phase: Option<&Value>) -> ProviderResponseReconciliationPhase {
    match phase {
        None => ProviderResponseReconciliationPhase::Missing,
        Some(Value::Null) => ProviderResponseReconciliationPhase::Null,
        Some(Value::String(value)) => match value.as_str() {
            "commentary" => ProviderResponseReconciliationPhase::Commentary,
            "final_answer" => ProviderResponseReconciliationPhase::FinalAnswer,
            _ => ProviderResponseReconciliationPhase::Other,
        },
        Some(_) => ProviderResponseReconciliationPhase::Invalid,
    }
}

fn reconciliation_assistant_phase(
    phase: Option<AssistantPhase>,
) -> ProviderResponseReconciliationPhase {
    match phase {
        None => ProviderResponseReconciliationPhase::Missing,
        Some(AssistantPhase::Commentary) => ProviderResponseReconciliationPhase::Commentary,
        Some(AssistantPhase::FinalAnswer) => ProviderResponseReconciliationPhase::FinalAnswer,
    }
}

fn reconciliation_id_presence(id: Option<&Value>) -> ProviderResponseReconciliationIdPresence {
    match id {
        None => ProviderResponseReconciliationIdPresence::Missing,
        Some(Value::Null) => ProviderResponseReconciliationIdPresence::Null,
        Some(Value::String(value)) if value.is_empty() => {
            ProviderResponseReconciliationIdPresence::Empty
        }
        Some(Value::String(_)) => ProviderResponseReconciliationIdPresence::Present,
        Some(_) => ProviderResponseReconciliationIdPresence::Invalid,
    }
}

fn reconciliation_id_relation(
    terminal_item_present: bool,
    terminal_id: Option<&str>,
    observed_lifecycle_present: bool,
    observed_id: Option<&str>,
) -> ProviderResponseReconciliationIdRelation {
    match (terminal_id, observed_id) {
        (Some(terminal), Some(observed)) if terminal == observed => {
            ProviderResponseReconciliationIdRelation::Equal
        }
        (Some(_), Some(_)) => ProviderResponseReconciliationIdRelation::Mismatch,
        (Some(_), None) => ProviderResponseReconciliationIdRelation::TerminalOnly,
        (None, Some(_)) => ProviderResponseReconciliationIdRelation::ObservedOnly,
        (None, None) if terminal_item_present && observed_lifecycle_present => {
            ProviderResponseReconciliationIdRelation::BothAbsent
        }
        (None, None) => ProviderResponseReconciliationIdRelation::NotComparable,
    }
}

fn reconciliation_mapping_basis(
    terminal_id: Option<&Value>,
    terminal_id_value: Option<&str>,
    identities: &OutputIdentityIndex,
) -> Option<ProviderResponseOutputIdentityMappingBasis> {
    match (terminal_id, terminal_id_value) {
        (None | Some(Value::Null), None) => {
            Some(ProviderResponseOutputIdentityMappingBasis::OrdinalMissingId)
        }
        (Some(Value::String(_)), Some(id)) if identities.index_for_id(id).is_some() => {
            Some(ProviderResponseOutputIdentityMappingBasis::KnownId)
        }
        (Some(Value::String(_)), Some(_)) => {
            Some(ProviderResponseOutputIdentityMappingBasis::OrdinalUnknownId)
        }
        _ => None,
    }
}

fn reconciliation_text_relation(
    terminal_text: Option<&str>,
    observed_text: Option<&str>,
) -> ProviderResponseReconciliationTextRelation {
    match (terminal_text, observed_text) {
        (Some(terminal), Some(observed)) if terminal == observed => {
            ProviderResponseReconciliationTextRelation::Equal
        }
        (Some(_), Some(_)) => ProviderResponseReconciliationTextRelation::Mismatch,
        (Some(_), None) => ProviderResponseReconciliationTextRelation::TerminalOnly,
        (None, Some(_)) => ProviderResponseReconciliationTextRelation::ObservedOnly,
        (None, None) => ProviderResponseReconciliationTextRelation::NotComparable,
    }
}

fn bounded_len(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn optional_sequence(payload: &Map<String, Value>) -> Option<u64> {
    payload.get("sequence_number").and_then(Value::as_u64)
}

struct CompletedResponse<'a> {
    id: &'a str,
    object: &'a Map<String, Value>,
}

fn completed_response(
    payload: &Map<String, Value>,
) -> Result<CompletedResponse<'_>, ResponseOutputLedgerError> {
    let response = payload
        .get("response")
        .and_then(Value::as_object)
        .ok_or(ResponseOutputLedgerError::MissingResponseObject)?;
    let id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() || status != "completed" {
        return Err(ResponseOutputLedgerError::InvalidResponse);
    }
    Ok(CompletedResponse {
        id,
        object: response,
    })
}

fn response_lifecycle_id(
    payload: &Map<String, Value>,
    expected_status: &str,
) -> Result<String, ProviderFault> {
    let response = payload
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_lifecycle("OpenAI response lifecycle is missing response object"))?;
    let id = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid_lifecycle("OpenAI response lifecycle has an invalid id"))?;
    if response.get("status").and_then(Value::as_str) != Some(expected_status) {
        return Err(invalid_lifecycle(
            "OpenAI response lifecycle has an invalid status",
        ));
    }
    Ok(id.to_owned())
}

fn output_text_part(payload: &Map<String, Value>) -> Result<OutputTextContent, ProviderFault> {
    let part = payload
        .get("part")
        .cloned()
        .ok_or_else(|| invalid_lifecycle("OpenAI content part lifecycle is missing part"))?;
    match serde_json::from_value::<OutputContent>(part)
        .map_err(|_| invalid_lifecycle("OpenAI content part has an invalid shape"))?
    {
        OutputContent::OutputText(text) => Ok(text),
        OutputContent::Refusal(_) | OutputContent::ReasoningText(_) => Err(invalid_lifecycle(
            "OpenAI message content part is not supported output text",
        )),
    }
}

fn parse_lifecycle_item(
    payload: &Map<String, Value>,
) -> Result<ParsedOutputItem, ResponseOutputItemLifecycleError> {
    let output_index = payload
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or(ResponseOutputItemLifecycleError::InvalidOutputIndex)?;
    let raw = payload
        .get("item")
        .cloned()
        .ok_or(ResponseOutputItemLifecycleError::MissingItem)?;
    parse_output_item(output_index, raw)
}

fn parse_output_item(
    output_index: u64,
    raw: Value,
) -> Result<ParsedOutputItem, ResponseOutputItemLifecycleError> {
    let item_type = raw
        .as_object()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .ok_or(ResponseOutputItemLifecycleError::MissingItemType)?;
    if !matches!(item_type, "message" | "reasoning" | "compaction") {
        return Err(ResponseOutputItemLifecycleError::UnsupportedItemType);
    }
    let typed: OutputItem = serde_json::from_value(raw.clone())
        .map_err(|_| ResponseOutputItemLifecycleError::InvalidItemShape)?;
    let (kind, id) = match &typed {
        OutputItem::Message(message) => (OutputKind::Message, Some(message.id.clone())),
        OutputItem::Reasoning(reasoning) => (OutputKind::Reasoning, reasoning.id.clone()),
        OutputItem::Compaction(compaction) => (OutputKind::Compaction, Some(compaction.id.clone())),
        _ => return Err(ResponseOutputItemLifecycleError::UnsupportedItemType),
    };
    if id.as_deref().is_some_and(str::is_empty) {
        return Err(ResponseOutputItemLifecycleError::EmptyItemId);
    }
    Ok(ParsedOutputItem {
        output_index,
        id,
        kind,
        raw,
        body: ParsedOutputItemBody::Sdk(typed),
    })
}

fn parse_terminal_output_item(
    output_index: u64,
    raw: Value,
) -> Result<ParsedOutputItem, ResponseOutputLedgerError> {
    let item_type = raw
        .as_object()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .ok_or(ResponseOutputLedgerError::MissingOutputItemType)?;
    let kind = match item_type {
        "message" => OutputKind::Message,
        "reasoning" => OutputKind::Reasoning,
        "compaction" => OutputKind::Compaction,
        _ => return Err(ResponseOutputLedgerError::UnsupportedOutputItem),
    };
    let (id, body) = match kind {
        OutputKind::Message => {
            let (id, message) = parse_terminal_message(output_index, &raw)?;
            (id, ParsedOutputItemBody::TerminalMessage(message))
        }
        OutputKind::Reasoning | OutputKind::Compaction => {
            let typed: OutputItem = serde_json::from_value(raw.clone())
                .map_err(|_| ResponseOutputLedgerError::InvalidOutputItemShape(kind))?;
            let id = match &typed {
                OutputItem::Reasoning(reasoning) => reasoning.id.clone(),
                OutputItem::Compaction(compaction) => Some(compaction.id.clone()),
                _ => return Err(ResponseOutputLedgerError::UnsupportedOutputItem),
            };
            (id, ParsedOutputItemBody::Sdk(typed))
        }
    };
    if id.as_deref().is_some_and(str::is_empty) {
        return Err(ResponseOutputLedgerError::EmptyOutputItemId);
    }
    Ok(ParsedOutputItem {
        output_index,
        id,
        kind,
        raw,
        body,
    })
}

fn parse_terminal_message(
    output_index: u64,
    raw: &Value,
) -> Result<(Option<String>, TerminalMessage), ResponseOutputLedgerError> {
    let object = raw
        .as_object()
        .ok_or(ResponseOutputLedgerError::InvalidOutputItemShape(
            OutputKind::Message,
        ))?;
    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if id.is_empty() => {
            return Err(ResponseOutputLedgerError::EmptyOutputItemId)
        }
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => {
            return Err(ResponseOutputLedgerError::InvalidOutputItemShape(
                OutputKind::Message,
            ))
        }
    };
    match object.get("status") {
        None | Some(Value::Null) => {}
        Some(Value::String(status)) if status == "completed" => {}
        Some(Value::String(_)) => return Err(ResponseOutputLedgerError::InvalidCompletedItem),
        Some(_) => {
            return Err(ResponseOutputLedgerError::InvalidOutputItemShape(
                OutputKind::Message,
            ))
        }
    }
    match object.get("role") {
        None | Some(Value::Null) => {}
        Some(Value::String(role)) if role == "assistant" => {}
        Some(Value::String(_)) => return Err(ResponseOutputLedgerError::InvalidCompletedItem),
        Some(_) => {
            return Err(ResponseOutputLedgerError::InvalidOutputItemShape(
                OutputKind::Message,
            ))
        }
    }
    let phase = match object.get("phase") {
        None | Some(Value::Null) => None,
        Some(phase) => Some(
            serde_json::from_value::<OpenAiMessagePhase>(phase.clone()).map_err(|_| {
                ResponseOutputLedgerError::InvalidOutputItemShape(OutputKind::Message)
            })?,
        ),
    };
    let [content] = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or(ResponseOutputLedgerError::InvalidMessageText {
            terminal_output_index: output_index,
        })?
        .as_slice()
    else {
        return Err(ResponseOutputLedgerError::InvalidMessageText {
            terminal_output_index: output_index,
        });
    };
    let content = content
        .as_object()
        .ok_or(ResponseOutputLedgerError::InvalidMessageText {
            terminal_output_index: output_index,
        })?;
    if content.get("type").and_then(Value::as_str) != Some("output_text") {
        return Err(ResponseOutputLedgerError::InvalidMessageText {
            terminal_output_index: output_index,
        });
    }
    let text = content.get("text").and_then(Value::as_str).ok_or(
        ResponseOutputLedgerError::InvalidMessageText {
            terminal_output_index: output_index,
        },
    )?;
    let annotations = match content.get("annotations") {
        None | Some(Value::Null) => Value::Array(Vec::new()),
        Some(annotations) => annotations.clone(),
    };
    let mut projected = Map::new();
    projected.insert("type".to_owned(), Value::String("output_text".to_owned()));
    projected.insert("text".to_owned(), Value::String(text.to_owned()));
    projected.insert("annotations".to_owned(), annotations);
    if let Some(logprobs) = content.get("logprobs") {
        projected.insert("logprobs".to_owned(), logprobs.clone());
    }
    let output_text = match serde_json::from_value::<OutputMessageContent>(Value::Object(projected))
        .map_err(|_| ResponseOutputLedgerError::InvalidOutputItemShape(OutputKind::Message))?
    {
        OutputMessageContent::OutputText(output_text) => output_text,
        OutputMessageContent::Refusal(_) => {
            return Err(ResponseOutputLedgerError::InvalidMessageText {
                terminal_output_index: output_index,
            })
        }
    };
    Ok((id, TerminalMessage { phase, output_text }))
}

fn validate_observed_item(item: &ParsedOutputItem) -> Result<(), ResponseOutputItemLifecycleError> {
    match &item.body {
        ParsedOutputItemBody::Sdk(OutputItem::Message(_)) => Ok(()),
        ParsedOutputItemBody::Sdk(OutputItem::Reasoning(reasoning))
            if valid_empty_reasoning_content(&item.raw, reasoning)
                && valid_reasoning_summary(reasoning) =>
        {
            Ok(())
        }
        ParsedOutputItemBody::Sdk(OutputItem::Compaction(compaction))
            if !compaction.encrypted_content.is_empty() =>
        {
            Ok(())
        }
        ParsedOutputItemBody::Sdk(OutputItem::Reasoning(_))
        | ParsedOutputItemBody::Sdk(OutputItem::Compaction(_)) => {
            Err(ResponseOutputItemLifecycleError::InvalidAddedItem)
        }
        ParsedOutputItemBody::Sdk(_) | ParsedOutputItemBody::TerminalMessage(_) => {
            Err(ResponseOutputItemLifecycleError::UnsupportedItemType)
        }
    }
}

fn validate_terminal_item(item: &ParsedOutputItem) -> Result<(), ResponseOutputLedgerError> {
    match &item.body {
        ParsedOutputItemBody::TerminalMessage(_) => Ok(()),
        ParsedOutputItemBody::Sdk(OutputItem::Reasoning(reasoning))
            if reasoning.status != Some(OutputStatus::InProgress)
                && valid_empty_reasoning_content(&item.raw, reasoning)
                && reasoning
                    .encrypted_content
                    .as_deref()
                    .is_some_and(|content| !content.is_empty())
                && valid_reasoning_summary(reasoning) =>
        {
            Ok(())
        }
        ParsedOutputItemBody::Sdk(OutputItem::Compaction(compaction))
            if !compaction.encrypted_content.is_empty() =>
        {
            Ok(())
        }
        ParsedOutputItemBody::Sdk(OutputItem::Message(_))
        | ParsedOutputItemBody::Sdk(OutputItem::Reasoning(_))
        | ParsedOutputItemBody::Sdk(OutputItem::Compaction(_)) => {
            Err(ResponseOutputLedgerError::InvalidCompletedItem)
        }
        ParsedOutputItemBody::Sdk(_) => Err(ResponseOutputLedgerError::UnsupportedOutputItem),
    }
}

fn sealed_private_output(
    item: &ParsedOutputItem,
) -> Result<Option<SealedOpenAiPrivateOutput>, ResponseOutputLedgerError> {
    match &item.body {
        ParsedOutputItemBody::Sdk(OutputItem::Reasoning(_reasoning)) => {
            validate_terminal_item(item)?;
            Ok(Some(SealedOpenAiPrivateOutput {
                output_index: item.output_index,
                #[cfg(feature = "legacy-provider-port")]
                output_item: Some(reasoning_output(_reasoning)?),
                wire_item: canonical_wire_item(item)?,
            }))
        }
        ParsedOutputItemBody::Sdk(OutputItem::Compaction(_)) => {
            validate_terminal_item(item)?;
            Ok(Some(SealedOpenAiPrivateOutput {
                output_index: item.output_index,
                #[cfg(feature = "legacy-provider-port")]
                output_item: None,
                wire_item: canonical_wire_item(item)?,
            }))
        }
        ParsedOutputItemBody::Sdk(OutputItem::Message(_)) => Ok(None),
        ParsedOutputItemBody::Sdk(_) | ParsedOutputItemBody::TerminalMessage(_) => {
            Err(ResponseOutputLedgerError::UnsupportedOutputItem)
        }
    }
}

fn assistant_phase(item: &ParsedOutputItemBody) -> Option<AssistantPhase> {
    let phase = match item {
        ParsedOutputItemBody::Sdk(OutputItem::Message(message)) => message.phase,
        ParsedOutputItemBody::TerminalMessage(message) => message.phase,
        ParsedOutputItemBody::Sdk(_) => None,
    };
    phase.map(|phase| match phase {
        OpenAiMessagePhase::Commentary => AssistantPhase::Commentary,
        OpenAiMessagePhase::FinalAnswer => AssistantPhase::FinalAnswer,
    })
}

fn message_phases_reconcile(observed: &OutputLifecycle, terminal: &ParsedOutputItem) -> bool {
    let terminal_phase = assistant_phase(&terminal.body);
    if observed.phase == terminal_phase {
        return true;
    }

    observed.done
        && observed
            .text
            .as_ref()
            .and_then(|text| text.completed.as_deref())
            .is_some()
        && message_phase_classes_match(observed.phase, terminal_phase)
}

fn message_phase_classes_match(
    left: Option<AssistantPhase>,
    right: Option<AssistantPhase>,
) -> bool {
    matches!(left, Some(AssistantPhase::Commentary))
        == matches!(right, Some(AssistantPhase::Commentary))
}

fn strongest_message_phase(
    left: Option<AssistantPhase>,
    right: Option<AssistantPhase>,
) -> Option<AssistantPhase> {
    if matches!(left, Some(AssistantPhase::Commentary))
        || matches!(right, Some(AssistantPhase::Commentary))
    {
        Some(AssistantPhase::Commentary)
    } else if matches!(left, Some(AssistantPhase::FinalAnswer))
        || matches!(right, Some(AssistantPhase::FinalAnswer))
    {
        Some(AssistantPhase::FinalAnswer)
    } else {
        None
    }
}

fn retain_terminal_message_phase(item: &mut ParsedOutputItemBody, phase: Option<AssistantPhase>) {
    let ParsedOutputItemBody::TerminalMessage(message) = item else {
        return;
    };
    message.phase = phase.map(|phase| match phase {
        AssistantPhase::Commentary => OpenAiMessagePhase::Commentary,
        AssistantPhase::FinalAnswer => OpenAiMessagePhase::FinalAnswer,
    });
}

fn valid_reasoning_summary(reasoning: &ReasoningItem) -> bool {
    reasoning.summary.iter().all(|part| match part {
        SummaryPart::SummaryText(summary) => !summary.text.is_empty(),
    })
}

fn present_ids_conflict(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn observed_text(text: &TextLifecycle) -> Option<&str> {
    text.completed
        .as_deref()
        .or_else(|| text.delta_observed.then_some(text.delta.as_str()))
}

fn canonical_wire_item(item: &ParsedOutputItem) -> Result<Value, ResponseOutputLedgerError> {
    let mut wire = Map::new();
    match &item.body {
        ParsedOutputItemBody::TerminalMessage(message) => {
            let text = &message.output_text;
            let mut content = Map::new();
            content.insert("type".to_owned(), Value::String("output_text".to_owned()));
            content.insert("text".to_owned(), Value::String(text.text.clone()));
            content.insert(
                "annotations".to_owned(),
                serde_json::to_value(&text.annotations)
                    .map_err(|_| ResponseOutputLedgerError::Canonicalization)?,
            );
            if item
                .raw
                .get("content")
                .and_then(Value::as_array)
                .and_then(|content| content.first())
                .and_then(Value::as_object)
                .is_some_and(|content| content.contains_key("logprobs"))
            {
                content.insert(
                    "logprobs".to_owned(),
                    serde_json::to_value(&text.logprobs)
                        .map_err(|_| ResponseOutputLedgerError::Canonicalization)?,
                );
            }
            if let Some(id) = &item.id {
                wire.insert("id".to_owned(), Value::String(id.clone()));
            }
            wire.insert("type".to_owned(), Value::String("message".to_owned()));
            wire.insert("status".to_owned(), Value::String("completed".to_owned()));
            wire.insert("role".to_owned(), Value::String("assistant".to_owned()));
            wire.insert(
                "content".to_owned(),
                Value::Array(vec![Value::Object(content)]),
            );
            if let Some(phase) = message.phase {
                wire.insert(
                    "phase".to_owned(),
                    serde_json::to_value(phase)
                        .map_err(|_| ResponseOutputLedgerError::Canonicalization)?,
                );
            }
        }
        ParsedOutputItemBody::Sdk(OutputItem::Reasoning(reasoning)) => {
            if let Some(id) = &item.id {
                wire.insert("id".to_owned(), Value::String(id.clone()));
            }
            wire.insert("type".to_owned(), Value::String("reasoning".to_owned()));
            if let Some(status) = reasoning.status {
                wire.insert(
                    "status".to_owned(),
                    serde_json::to_value(status)
                        .map_err(|_| ResponseOutputLedgerError::Canonicalization)?,
                );
            }
            wire.insert(
                "summary".to_owned(),
                serde_json::to_value(&reasoning.summary)
                    .map_err(|_| ResponseOutputLedgerError::Canonicalization)?,
            );
            wire.insert(
                "encrypted_content".to_owned(),
                Value::String(
                    reasoning
                        .encrypted_content
                        .clone()
                        .ok_or(ResponseOutputLedgerError::Canonicalization)?,
                ),
            );
        }
        ParsedOutputItemBody::Sdk(OutputItem::Compaction(compaction)) => {
            wire.insert("id".to_owned(), Value::String(compaction.id.clone()));
            wire.insert("type".to_owned(), Value::String("compaction".to_owned()));
            wire.insert(
                "encrypted_content".to_owned(),
                Value::String(compaction.encrypted_content.clone()),
            );
        }
        ParsedOutputItemBody::Sdk(_) => {
            return Err(ResponseOutputLedgerError::UnsupportedOutputItem)
        }
    }
    Ok(Value::Object(wire))
}

fn valid_empty_reasoning_content(raw: &Value, reasoning: &ReasoningItem) -> bool {
    match raw.get("content") {
        None => reasoning.content.is_none(),
        Some(Value::Array(content)) => {
            content.is_empty() && reasoning.content.as_ref().is_some_and(Vec::is_empty)
        }
        Some(_) => false,
    }
}

#[cfg(feature = "legacy-provider-port")]
fn reasoning_output(
    reasoning: &ReasoningItem,
) -> Result<CanonicalInputItem, ResponseOutputLedgerError> {
    let summary = serde_json::to_value(&reasoning.summary)
        .map_err(|_| ResponseOutputLedgerError::Canonicalization)?;
    let encrypted_content = reasoning
        .encrypted_content
        .as_deref()
        .ok_or(ResponseOutputLedgerError::Canonicalization)?;
    let extension = ProviderExtension::new(
        OPENAI_PROVIDER,
        REASONING_CAPABILITY,
        REASONING_SCHEMA_VERSION,
        json!({
            "summary": summary,
            "content": null,
            "encrypted_content": encrypted_content,
        }),
    )
    .map_err(|_| ResponseOutputLedgerError::Canonicalization)?;
    Ok(CanonicalInputItem::provider_extension(extension))
}

fn required_nonempty_string(
    payload: &Map<String, Value>,
    field: &str,
) -> Result<String, ProviderFault> {
    let value = payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            invalid_lifecycle("OpenAI streaming event has a missing or empty string field")
        })?;
    Ok(value.to_owned())
}

fn required_u64(payload: &Map<String, Value>, field: &str) -> Result<u64, ProviderFault> {
    payload.get(field).and_then(Value::as_u64).ok_or_else(|| {
        invalid_lifecycle("OpenAI streaming event is missing an integer identity field")
    })
}

fn invalid_lifecycle(message: &'static str) -> ProviderFault {
    ProviderFault::model_rejected(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().expect("test JSON object").clone()
    }

    fn lifecycle(output_index: u64, item: Value) -> Map<String, Value> {
        object(json!({"output_index": output_index, "item": item}))
    }

    fn message(id: &str, status: &str, text: Option<&str>) -> Value {
        let content = text.map_or_else(
            || json!([]),
            |text| {
                json!([{
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                }])
            },
        );
        json!({
            "id": id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": content,
        })
    }

    fn message_with_phase(id: &str, status: &str, text: Option<&str>, phase: &str) -> Value {
        let mut message = message(id, status, text);
        message["phase"] = json!(phase);
        message
    }

    #[derive(Clone, Copy, Debug)]
    enum SyntheticMessagePhaseField {
        Missing,
        Null,
        Commentary,
        FinalAnswer,
    }

    fn message_with_phase_field(
        id: &str,
        status: &str,
        text: Option<&str>,
        phase: SyntheticMessagePhaseField,
    ) -> Value {
        let mut message = message(id, status, text);
        match phase {
            SyntheticMessagePhaseField::Missing => {}
            SyntheticMessagePhaseField::Null => message["phase"] = Value::Null,
            SyntheticMessagePhaseField::Commentary => message["phase"] = json!("commentary"),
            SyntheticMessagePhaseField::FinalAnswer => message["phase"] = json!("final_answer"),
        }
        message
    }

    fn reasoning(
        id: &str,
        status: &str,
        summary_text: Option<&str>,
        encrypted: Option<&str>,
    ) -> Value {
        let summary = summary_text.map_or_else(
            || json!([]),
            |text| json!([{"type": "summary_text", "text": text}]),
        );
        encrypted.map_or_else(
            || {
                json!({
                    "id": id,
                    "type": "reasoning",
                    "status": status,
                    "summary": summary,
                })
            },
            |encrypted_content| {
                json!({
                    "id": id,
                    "type": "reasoning",
                    "status": status,
                    "summary": summary,
                    "encrypted_content": encrypted_content,
                })
            },
        )
    }

    fn reasoning_with_content(mut item: Value, content: Option<Value>) -> Value {
        let object = item.as_object_mut().expect("reasoning test item object");
        match content {
            Some(content) => {
                object.insert("content".to_owned(), content);
            }
            None => {
                object.remove("content");
            }
        }
        item
    }

    fn compaction(id: &str, encrypted_content: &str) -> Value {
        json!({
            "id": id,
            "type": "compaction",
            "encrypted_content": encrypted_content,
            "created_by": "server-side-compaction",
        })
    }

    fn text_frame(item_id: &str, output_index: u64, content_index: u64) -> Map<String, Value> {
        object(json!({
            "item_id": item_id,
            "output_index": output_index,
            "content_index": content_index,
        }))
    }

    fn annotation_frame(
        item_id: &str,
        output_index: u64,
        content_index: u64,
    ) -> Map<String, Value> {
        object(json!({
            "sequence_number": 3,
            "item_id": item_id,
            "output_index": output_index,
            "content_index": content_index,
            "annotation_index": 0,
            "annotation": {
                "type": "url_citation",
                "start_index": 0,
                "end_index": 6,
                "title": "Synthetic",
                "url": "https://example.invalid/citation",
            },
        }))
    }

    fn completed(output: Vec<Value>) -> Map<String, Value> {
        object(json!({
            "response": {
                "id": "resp_test",
                "status": "completed",
                "output": output,
            }
        }))
    }

    fn with_sequence(mut payload: Map<String, Value>, sequence_number: u64) -> Map<String, Value> {
        payload.insert("sequence_number".to_owned(), json!(sequence_number));
        payload
    }

    fn completed_with_sequence(output: Vec<Value>, sequence_number: u64) -> Map<String, Value> {
        with_sequence(completed(output), sequence_number)
    }

    fn response_lifecycle(id: &str, status: &str) -> Map<String, Value> {
        object(json!({
            "response": {
                "id": id,
                "status": status,
            }
        }))
    }

    fn content_part(
        item_id: &str,
        output_index: u64,
        content_index: u64,
        text: &str,
    ) -> Map<String, Value> {
        object(json!({
            "item_id": item_id,
            "output_index": output_index,
            "content_index": content_index,
            "part": {
                "type": "output_text",
                "text": text,
                "annotations": [],
            },
        }))
    }

    fn completed_message_ledger() -> (OpenAiOutputLedger, Value) {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 0, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        let done = message("msg_1", "completed", Some("<ok />"));
        ledger.record_done(&lifecycle(0, done.clone())).unwrap();
        (ledger, done)
    }

    fn prefixed_message_ledger(prefix: Value) -> (OpenAiOutputLedger, Value) {
        let mut ledger = OpenAiOutputLedger::default();
        ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
        ledger.record_done(&lifecycle(0, prefix)).unwrap();
        ledger
            .record_added(&lifecycle(1, message("item-message", "in_progress", None)))
            .unwrap();
        let frame = text_frame("item-message", 1, 0);
        ledger.record_text_delta(&frame, "output-text").unwrap();
        ledger.record_text_done(&frame, "output-text").unwrap();
        let done = message("item-message", "completed", Some("output-text"));
        ledger.record_done(&lifecycle(1, done.clone())).unwrap();
        (ledger, done)
    }

    fn doubly_prefixed_message_ledger(
        intermediate_added: Value,
        intermediate_done: Option<Value>,
    ) -> (OpenAiOutputLedger, Value) {
        let mut ledger = OpenAiOutputLedger::default();
        let prefix = reasoning("item-prefix", "completed", None, Some("encrypted-prefix"));
        ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
        ledger.record_done(&lifecycle(0, prefix)).unwrap();
        ledger
            .record_added(&lifecycle(1, intermediate_added))
            .unwrap();
        if let Some(done) = intermediate_done {
            ledger.record_done(&lifecycle(1, done)).unwrap();
        }
        ledger
            .record_added(&lifecycle(2, message("item-message", "in_progress", None)))
            .unwrap();
        let frame = text_frame("item-message", 2, 0);
        ledger.record_text_delta(&frame, "output-text").unwrap();
        ledger.record_text_done(&frame, "output-text").unwrap();
        let done = message("item-message", "completed", Some("output-text"));
        ledger.record_done(&lifecycle(2, done.clone())).unwrap();
        (ledger, done)
    }

    fn replace_message_id(mut message: Value, id: Option<Value>) -> Value {
        let object = message.as_object_mut().expect("message test item object");
        match id {
            Some(id) => {
                object.insert("id".to_owned(), id);
            }
            None => {
                object.remove("id");
            }
        }
        message
    }

    fn completion_error(
        result: Result<CompletedOpenAiOutput, ResponseOutputLedgerFailure>,
    ) -> ResponseOutputLedgerFailure {
        match result {
            Ok(_) => panic!("terminal fixture unexpectedly completed"),
            Err(error) => error,
        }
    }

    #[test]
    fn message_text_ledger_errors_have_branch_complete_payload_free_reasons() {
        let base_message = message("item-placeholder", "completed", Some("text-placeholder"));
        let invalid_messages = [
            {
                let mut invalid = base_message.clone();
                invalid
                    .as_object_mut()
                    .expect("message fixture is an object")
                    .remove("content");
                invalid
            },
            {
                let mut invalid = base_message.clone();
                invalid["content"] = json!([]);
                invalid
            },
            {
                let mut invalid = base_message.clone();
                invalid["content"] = json!(["part-placeholder"]);
                invalid
            },
            {
                let mut invalid = base_message.clone();
                invalid["content"] = json!([{
                    "type": "other-placeholder",
                    "text": "text-placeholder",
                }]);
                invalid
            },
            {
                let mut invalid = base_message;
                invalid["content"] = json!([{"type": "output_text"}]);
                invalid
            },
        ];
        for invalid_message in invalid_messages {
            let error = completion_error(
                OpenAiOutputLedger::default().complete(&completed(vec![invalid_message])),
            );
            assert_eq!(
                error.response_message_text_reason(),
                Some(ProviderResponseMessageTextReason::InvalidTerminalMessageShape)
            );
        }

        let mut text_ledger = OpenAiOutputLedger::default();
        text_ledger
            .record_added(&lifecycle(
                0,
                message("item-placeholder", "in_progress", None),
            ))
            .unwrap();
        let observed_frame = text_frame("item-placeholder", 0, 0);
        text_ledger
            .record_text_delta(&observed_frame, "text-placeholder")
            .unwrap();
        text_ledger
            .record_text_done(&observed_frame, "text-placeholder")
            .unwrap();
        let text_mismatch = completion_error(text_ledger.complete(&completed(vec![message(
            "item-placeholder",
            "completed",
            Some("different-placeholder"),
        )])));

        let mut unreconciled_ledger = OpenAiOutputLedger::default();
        unreconciled_ledger
            .record_added(&lifecycle(
                1,
                message("observed-placeholder", "in_progress", None),
            ))
            .unwrap();
        let unreconciled_frame = text_frame("observed-placeholder", 1, 0);
        unreconciled_ledger
            .record_text_delta(&unreconciled_frame, "observed-placeholder")
            .unwrap();
        let unreconciled_text =
            completion_error(unreconciled_ledger.complete(&completed(vec![message(
                "terminal-placeholder",
                "completed",
                Some("terminal-placeholder"),
            )])));

        let mut phase_ledger = OpenAiOutputLedger::default();
        phase_ledger
            .record_added(&lifecycle(
                0,
                message_with_phase("item-placeholder", "in_progress", None, "commentary"),
            ))
            .unwrap();
        let frame = text_frame("item-placeholder", 0, 0);
        phase_ledger
            .record_text_delta(&frame, "text-placeholder")
            .unwrap();
        phase_ledger
            .record_text_done(&frame, "text-placeholder")
            .unwrap();
        let phase_done = message_with_phase(
            "item-placeholder",
            "completed",
            Some("text-placeholder"),
            "commentary",
        );
        phase_ledger
            .record_done(&lifecycle(0, phase_done.clone()))
            .unwrap();
        let phase_mismatch =
            completion_error(phase_ledger.complete(&completed(vec![message_with_phase(
                "item-placeholder",
                "completed",
                Some("text-placeholder"),
                "final_answer",
            )])));

        assert_eq!(
            [text_mismatch, unreconciled_text, phase_mismatch]
                .map(|failure| failure.response_message_text_reason()),
            [
                None,
                None,
                Some(ProviderResponseMessageTextReason::ApplicablePhaseMismatch),
            ]
        );
    }

    #[test]
    fn message_text_failures_capture_one_branch_complete_structural_reconciliation_snapshot() {
        let mut invalid_ledger = OpenAiOutputLedger::default();
        invalid_ledger
            .record_response_created(&with_sequence(
                response_lifecycle("resp_test", "in_progress"),
                1,
            ))
            .unwrap();
        let mut invalid_terminal = message(
            "item-placeholder",
            "completed",
            Some("terminal-placeholder"),
        );
        invalid_terminal
            .as_object_mut()
            .expect("terminal fixture is an object")
            .remove("content");
        let invalid_error = completion_error(
            invalid_ledger.complete(&completed_with_sequence(vec![invalid_terminal], 2)),
        );
        if let Some(snapshot) = invalid_error.response_completed_reconciliation() {
            assert_eq!(
                serde_json::to_value(snapshot).unwrap(),
                json!({
                    "branch": "invalid_terminal_message_shape",
                    "response_created_sequence": 1,
                    "response_in_progress_sequence": null,
                    "response_completed_sequence": 2,
                    "response_status": "completed",
                    "terminal_output_count": 1,
                    "observed_lifecycle_count": 0,
                    "terminal_output_index": 0,
                    "observed_lifecycle_index": null,
                    "terminal_item_kind": "message",
                    "observed_item_kind": "missing",
                    "terminal_item_status": "completed",
                    "observed_lifecycle_state": null,
                    "terminal_phase": "missing",
                    "observed_phase": "missing",
                    "terminal_id_presence": "present",
                    "observed_id_presence": "missing",
                    "id_relation": "terminal_only",
                    "mapping_basis": "ordinal_unknown_id",
                    "terminal_content_presence": "missing",
                    "terminal_content_part_count": 0,
                    "terminal_output_text_part_count": 0,
                    "terminal_refusal_part_count": 0,
                    "terminal_other_part_count": 0,
                    "terminal_malformed_part_count": 0,
                    "terminal_text_presence": "missing",
                    "terminal_text_bytes": 0,
                    "observed_text_state": "not_observed",
                    "observed_text_bytes": 0,
                    "text_relation": "not_comparable",
                    "output_item_added_sequence": null,
                    "content_part_added_sequence": null,
                    "first_text_delta_sequence": null,
                    "last_text_delta_sequence": null,
                    "text_delta_count": 0,
                    "text_done_sequence": null,
                    "content_part_done_sequence": null,
                    "output_item_done_sequence": null,
                })
            );
        }

        let observed_text = "observed-placeholder";
        let terminal_text = "terminal-placeholder";
        let mut mismatch_ledger = OpenAiOutputLedger::default();
        mismatch_ledger
            .record_response_created(&with_sequence(
                response_lifecycle("resp_test", "in_progress"),
                1,
            ))
            .unwrap();
        mismatch_ledger
            .record_response_in_progress(&with_sequence(
                response_lifecycle("resp_test", "in_progress"),
                2,
            ))
            .unwrap();
        mismatch_ledger
            .record_added(&with_sequence(
                lifecycle(0, message("item-placeholder", "in_progress", None)),
                3,
            ))
            .unwrap();
        mismatch_ledger
            .record_content_part_added(&with_sequence(
                content_part("item-placeholder", 0, 0, ""),
                4,
            ))
            .unwrap();
        mismatch_ledger
            .record_text_delta(
                &with_sequence(text_frame("item-placeholder", 0, 0), 5),
                observed_text,
            )
            .unwrap();
        mismatch_ledger
            .record_text_done(
                &with_sequence(text_frame("item-placeholder", 0, 0), 6),
                observed_text,
            )
            .unwrap();
        mismatch_ledger
            .record_content_part_done(&with_sequence(
                content_part("item-placeholder", 0, 0, observed_text),
                7,
            ))
            .unwrap();
        mismatch_ledger
            .record_done(&with_sequence(
                lifecycle(
                    0,
                    message("item-placeholder", "completed", Some(observed_text)),
                ),
                8,
            ))
            .unwrap();
        let mismatch_error = completion_error(mismatch_ledger.complete(&completed_with_sequence(
            vec![message(
                "item-placeholder",
                "completed",
                Some(terminal_text),
            )],
            9,
        )));
        assert_eq!(
            serde_json::to_value(
                mismatch_error
                    .response_completed_reconciliation()
                    .expect("text mismatch keeps structural evidence"),
            )
            .unwrap(),
            json!({
                "branch": "terminal_observed_text_mismatch",
                "response_created_sequence": 1,
                "response_in_progress_sequence": 2,
                "response_completed_sequence": 9,
                "response_status": "completed",
                "terminal_output_count": 1,
                "observed_lifecycle_count": 1,
                "terminal_output_index": 0,
                "observed_lifecycle_index": 0,
                "terminal_item_kind": "message",
                "observed_item_kind": "message",
                "terminal_item_status": "completed",
                "observed_lifecycle_state": "done",
                "terminal_phase": "missing",
                "observed_phase": "missing",
                "terminal_id_presence": "present",
                "observed_id_presence": "present",
                "id_relation": "equal",
                "mapping_basis": "known_id",
                "terminal_content_presence": "array",
                "terminal_content_part_count": 1,
                "terminal_output_text_part_count": 1,
                "terminal_refusal_part_count": 0,
                "terminal_other_part_count": 0,
                "terminal_malformed_part_count": 0,
                "terminal_text_presence": "string",
                "terminal_text_bytes": terminal_text.len(),
                "observed_text_state": "completed",
                "observed_text_bytes": observed_text.len(),
                "text_relation": "mismatch",
                "output_item_added_sequence": 3,
                "content_part_added_sequence": 4,
                "first_text_delta_sequence": 5,
                "last_text_delta_sequence": 5,
                "text_delta_count": 1,
                "text_done_sequence": 6,
                "content_part_done_sequence": 7,
                "output_item_done_sequence": 8,
            })
        );

        let mut orphan_ledger = OpenAiOutputLedger::default();
        orphan_ledger
            .record_added(&with_sequence(
                lifecycle(1, message("observed-placeholder", "in_progress", None)),
                1,
            ))
            .unwrap();
        orphan_ledger
            .record_text_delta(
                &with_sequence(text_frame("observed-placeholder", 1, 0), 2),
                observed_text,
            )
            .unwrap();
        let orphan_error = completion_error(orphan_ledger.complete(&completed_with_sequence(
            vec![message(
                "terminal-placeholder",
                "completed",
                Some(terminal_text),
            )],
            3,
        )));
        if let Some(snapshot) = orphan_error.response_completed_reconciliation() {
            let orphan_snapshot = serde_json::to_value(snapshot).unwrap();
            assert_eq!(orphan_snapshot["terminal_output_count"], 1);
            assert_eq!(orphan_snapshot["observed_lifecycle_count"], 1);
            assert_eq!(orphan_snapshot["branch"], "terminal_observed_text_mismatch");
            assert_eq!(orphan_snapshot["terminal_output_index"], Value::Null);
            assert_eq!(orphan_snapshot["observed_lifecycle_index"], 1);
            assert_eq!(orphan_snapshot["observed_text_state"], "delta");
            assert_eq!(orphan_snapshot["text_relation"], "observed_only");
        }

        let mut phase_ledger = OpenAiOutputLedger::default();
        phase_ledger
            .record_added(&with_sequence(
                lifecycle(
                    0,
                    message_with_phase("item-placeholder", "in_progress", None, "commentary"),
                ),
                1,
            ))
            .unwrap();
        phase_ledger
            .record_text_delta(
                &with_sequence(text_frame("item-placeholder", 0, 0), 2),
                observed_text,
            )
            .unwrap();
        phase_ledger
            .record_text_done(
                &with_sequence(text_frame("item-placeholder", 0, 0), 3),
                observed_text,
            )
            .unwrap();
        let phase_error = completion_error(phase_ledger.complete(&completed_with_sequence(
            vec![message_with_phase(
                "item-placeholder",
                "completed",
                Some(observed_text),
                "final_answer",
            )],
            4,
        )));
        let phase_snapshot = serde_json::to_value(
            phase_error
                .response_completed_reconciliation()
                .expect("phase mismatch keeps structural evidence"),
        )
        .unwrap();
        assert_eq!(phase_snapshot["branch"], "applicable_phase_mismatch");
        assert_eq!(phase_snapshot["terminal_phase"], "final_answer");
        assert_eq!(phase_snapshot["observed_phase"], "commentary");
        assert_eq!(phase_snapshot["text_relation"], "equal");
    }

    #[test]
    fn lifecycle_rejects_out_of_order_duplicate_and_drifting_items() {
        let completed_message = message("msg_1", "completed", Some("<ok />"));
        assert!(OpenAiOutputLedger::default()
            .record_done(&lifecycle(0, completed_message.clone()))
            .is_err());

        let mut ledger = OpenAiOutputLedger::default();
        let added = lifecycle(0, message("msg_1", "in_progress", None));
        ledger.record_added(&added).unwrap();
        assert!(ledger.record_added(&added).is_err());
        assert!(ledger
            .record_done(&lifecycle(
                0,
                message("msg_changed", "completed", Some("<ok />")),
            ))
            .is_err());
        let frame = text_frame("msg_1", 0, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        ledger
            .record_done(&lifecycle(0, completed_message.clone()))
            .unwrap();
        assert!(ledger
            .record_done(&lifecycle(0, completed_message))
            .is_err());
    }

    #[test]
    fn output_identity_admission_uses_bounded_index_operations_per_item() {
        const ITEM_COUNT: u64 = 128;
        let mut ledger = OpenAiOutputLedger::default();

        for output_index in 0..ITEM_COUNT {
            ledger
                .record_added(&lifecycle(
                    output_index,
                    compaction(&format!("cmp_{output_index}"), "opaque"),
                ))
                .unwrap();
        }
        assert_eq!(
            ledger.identity_admission_probes(),
            usize::try_from(ITEM_COUNT * 2).unwrap(),
            "each accepted item needs one output-index and one item-ID insertion"
        );
        assert!(ledger
            .record_added(&lifecycle(ITEM_COUNT, compaction("cmp_0", "opaque")))
            .is_err());
        assert_eq!(
            ledger.identity_admission_probes(),
            usize::try_from(ITEM_COUNT * 2).unwrap() + 3,
            "a duplicate ID rolls back its newly admitted output index"
        );
        ledger
            .record_added(&lifecycle(ITEM_COUNT, compaction("cmp_new", "opaque")))
            .expect("the rolled-back output index remains available");
        assert!(ledger
            .record_added(&lifecycle(0, compaction("cmp_other", "opaque")))
            .is_err());

        assert_eq!(
            ledger.identity_admission_probes(),
            usize::try_from(ITEM_COUNT * 2).unwrap() + 6,
            "a duplicate output index stops after one indexed operation"
        );
    }

    #[test]
    fn response_identity_is_stable_across_created_in_progress_and_completed() {
        let (mut ledger, done) = completed_message_ledger();
        ledger
            .record_response_created(&response_lifecycle("resp_test", "in_progress"))
            .unwrap();
        ledger
            .record_response_in_progress(&response_lifecycle("resp_test", "in_progress"))
            .unwrap();
        assert!(ledger
            .record_response_in_progress(&response_lifecycle("resp_changed", "in_progress"))
            .is_err());
        assert!(ledger
            .complete(&object(json!({
                "response": {
                    "id": "resp_changed",
                    "status": "completed",
                    "output": [done.clone()],
                }
            })))
            .is_err());
        assert!(ledger.complete(&completed(vec![done])).is_ok());
    }

    #[test]
    fn content_part_lifecycle_is_paired_and_matches_streamed_text() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        let added = content_part("msg_1", 0, 0, "");
        let done = content_part("msg_1", 0, 0, "<ok />");
        ledger.record_content_part_added(&added).unwrap();
        assert!(ledger.record_content_part_added(&added).is_err());

        let frame = text_frame("msg_1", 0, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        assert!(ledger
            .record_content_part_done(&content_part("msg_1", 0, 0, "changed"))
            .is_err());
        ledger.record_content_part_done(&done).unwrap();
        assert!(ledger.record_content_part_done(&done).is_err());

        let completed_message = message("msg_1", "completed", Some("<ok />"));
        ledger
            .record_done(&lifecycle(0, completed_message.clone()))
            .unwrap();
        assert!(ledger.complete(&completed(vec![completed_message])).is_ok());

        let mut missing_part_done = OpenAiOutputLedger::default();
        missing_part_done
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        missing_part_done.record_content_part_added(&added).unwrap();
        missing_part_done
            .record_text_delta(&frame, "<ok />")
            .unwrap();
        missing_part_done
            .record_text_done(&frame, "<ok />")
            .unwrap();
        let terminal = message("msg_1", "completed", Some("<ok />"));
        missing_part_done
            .record_done(&lifecycle(0, terminal.clone()))
            .unwrap();
        assert!(missing_part_done
            .complete(&completed(vec![terminal]))
            .is_ok());
    }

    #[test]
    fn text_frames_require_added_open_messages_and_stable_identity() {
        assert!(OpenAiOutputLedger::default()
            .record_text_delta(&text_frame("msg_1", 0, 0), "text")
            .is_err());

        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(0, reasoning("rs_1", "in_progress", None, None)))
            .unwrap();
        assert!(ledger
            .record_text_delta(&text_frame("rs_1", 0, 0), "text")
            .is_err());
        assert!(ledger
            .record_text_delta(&text_frame("rs_1", 0, 1), "text")
            .is_err());

        let mut messages = OpenAiOutputLedger::default();
        messages
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        messages
            .record_added(&lifecycle(1, message("msg_2", "in_progress", None)))
            .unwrap();
        let first = text_frame("msg_1", 0, 0);
        let second = text_frame("msg_2", 1, 0);
        assert!(messages.record_text_delta(&first, "first").unwrap());
        assert!(messages.record_text_delta(&second, "second").unwrap());
        messages.record_text_done(&first, "first").unwrap();
        assert!(messages.record_text_delta(&first, "late").is_err());
        messages
            .record_done(&lifecycle(0, message("msg_1", "completed", Some("first"))))
            .unwrap();
        assert!(messages.record_text_done(&first, "first").is_err());
    }

    #[test]
    fn annotation_frames_validate_schema_identity_and_open_text_lifecycle() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        let text = text_frame("msg_1", 0, 0);
        ledger.record_text_delta(&text, "<ok />").unwrap();
        let valid = annotation_frame("msg_1", 0, 0);
        ledger.record_text_annotation_added(&valid).unwrap();

        let mut missing_annotation = valid.clone();
        missing_annotation.remove("annotation");
        assert!(ledger
            .record_text_annotation_added(&missing_annotation)
            .is_err());
        let mut null_index = valid.clone();
        null_index.insert("annotation_index".to_owned(), Value::Null);
        assert!(ledger.record_text_annotation_added(&null_index).is_err());
        assert!(ledger
            .record_text_annotation_added(&annotation_frame("msg_changed", 0, 0))
            .is_err());
        assert!(ledger
            .record_text_annotation_added(&annotation_frame("msg_1", 0, 1))
            .is_err());

        ledger.record_text_done(&text, "<ok />").unwrap();
        assert!(ledger.record_text_annotation_added(&valid).is_err());
        ledger
            .record_done(&lifecycle(0, message("msg_1", "completed", Some("<ok />"))))
            .unwrap();
        assert!(ledger.record_text_annotation_added(&valid).is_err());
    }

    #[test]
    fn terminal_output_is_optional_reconciliation_after_done_time_sealing() {
        let missing_output = object(json!({
            "response": {"id": "resp_test", "status": "completed"}
        }));
        assert!(OpenAiOutputLedger::default()
            .complete(&missing_output)
            .is_err());
        assert!(OpenAiOutputLedger::default()
            .complete(&completed(Vec::new()))
            .is_err());

        let mut observed_without_final = OpenAiOutputLedger::default();
        observed_without_final
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        assert!(matches!(
            observed_without_final
                .complete(&completed(Vec::new()))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::InvalidCompletedItem)
        ));

        let mut missing_done = OpenAiOutputLedger::default();
        missing_done
            .record_added(&lifecycle(0, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 0, 0);
        missing_done.record_text_delta(&frame, "<ok />").unwrap();
        missing_done.record_text_done(&frame, "<ok />").unwrap();
        let terminal = message("msg_1", "completed", Some("<ok />"));
        assert!(missing_done.complete(&completed(vec![terminal])).is_err());

        let (ledger, done) = completed_message_ledger();
        assert!(ledger
            .complete(&completed(vec![message(
                "msg_1",
                "completed",
                Some("changed"),
            )]))
            .is_err());
        let complete = ledger.complete(&completed(vec![done])).unwrap();
        assert_eq!(complete.output_items.len(), 1);
        assert_eq!(complete.final_text, "<ok />");

        let mut omitted_stream = OpenAiOutputLedger::default();
        omitted_stream
            .record_added(&lifecycle(1, message("msg_omitted", "in_progress", None)))
            .unwrap();
        let omitted_frame = text_frame("msg_omitted", 1, 0);
        omitted_stream
            .record_text_delta(&omitted_frame, "observed")
            .unwrap();
        assert!(omitted_stream
            .complete(&completed(vec![message(
                "msg_terminal",
                "completed",
                Some("terminal"),
            )]))
            .is_err());
    }

    #[test]
    fn terminal_identity_accepts_known_messages_shifted_by_omitted_non_text_prefixes() {
        for prefix in [
            reasoning("rs_prefix", "completed", None, Some("encrypted-prefix")),
            compaction("cmp_prefix", "opaque"),
        ] {
            let mut ledger = OpenAiOutputLedger::default();
            ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
            ledger.record_done(&lifecycle(0, prefix)).unwrap();
            ledger
                .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
                .unwrap();
            let frame = text_frame("msg_1", 1, 0);
            ledger.record_text_delta(&frame, "<ok />").unwrap();
            ledger.record_text_done(&frame, "<ok />").unwrap();
            let done = message("msg_1", "completed", Some("<ok />"));
            ledger.record_done(&lifecycle(1, done.clone())).unwrap();

            assert!(
                ledger.complete(&completed(vec![done])).is_ok(),
                "an omitted non-text prefix must not change the known message identity"
            );
        }
    }

    #[test]
    fn terminal_identity_reprojects_a_missing_id_message_over_done_reasoning() {
        let (ledger, done) = prefixed_message_ledger(reasoning(
            "item-prefix",
            "completed",
            None,
            Some("encrypted-prefix"),
        ));
        let terminal = replace_message_id(done, None);

        let complete = ledger
            .complete(&completed(vec![terminal.clone()]))
            .expect("the exact observed message may satisfy the ID-less terminal item");

        assert_eq!(complete.final_text, "output-text");
        assert_eq!(complete.output_items.len(), 1);
        assert!(complete.wire_items.is_empty());
    }

    #[test]
    fn terminal_identity_reprojects_a_missing_id_message_over_three_done_reasoning_items() {
        let mut ledger = OpenAiOutputLedger::default();
        for output_index in 0..3 {
            let id = format!("item-reasoning-{output_index}");
            ledger
                .record_added(&lifecycle(
                    output_index,
                    reasoning(&id, "in_progress", None, None),
                ))
                .unwrap();
            ledger
                .record_done(&lifecycle(
                    output_index,
                    reasoning(&id, "completed", None, Some("encrypted-prefix")),
                ))
                .unwrap();
        }
        ledger
            .record_added(&lifecycle(
                3,
                message_with_phase("item-message", "in_progress", None, "final_answer"),
            ))
            .unwrap();
        let frame = text_frame("item-message", 3, 0);
        ledger.record_text_delta(&frame, "output-text").unwrap();
        ledger.record_text_done(&frame, "output-text").unwrap();
        let done = message_with_phase(
            "item-message",
            "completed",
            Some("output-text"),
            "final_answer",
        );
        ledger.record_done(&lifecycle(3, done.clone())).unwrap();
        let mut terminal = replace_message_id(done, None);
        terminal
            .as_object_mut()
            .expect("terminal message fixture is an object")
            .remove("phase");

        let complete = ledger
            .complete(&completed(vec![terminal]))
            .expect("the matching message may follow three contiguous done reasoning items");

        assert_eq!(complete.final_text, "output-text");
        assert_eq!(complete.output_items.len(), 1);
        assert!(complete.wire_items.is_empty());
    }

    #[test]
    fn terminal_identity_reprojection_rejects_compaction_at_the_terminal_ordinal() {
        let (ledger, done) =
            prefixed_message_ledger(compaction("item-prefix", "opaque-placeholder"));
        let terminal = replace_message_id(done, None);

        let error = completion_error(ledger.complete(&completed(vec![terminal])));
        let detail = error
            .response_output_identity_detail()
            .expect("the compaction conflict remains diagnostic");

        assert_eq!(detail.mapping_basis().code(), "ordinal_missing_id");
        assert_eq!(
            detail.kind_pair().code(),
            "terminal_message_over_compaction"
        );
        assert_eq!(detail.observed_text_relation().code(), "match");
        assert_eq!(detail.resolved_lifecycle_state().code(), "done");
    }

    #[test]
    fn terminal_identity_reprojection_rejects_a_null_terminal_id() {
        let (ledger, done) = prefixed_message_ledger(reasoning(
            "item-prefix",
            "completed",
            None,
            Some("encrypted-prefix"),
        ));
        let terminal = replace_message_id(done, Some(Value::Null));

        let error = completion_error(ledger.complete(&completed(vec![terminal])));
        let detail = error
            .response_output_identity_detail()
            .expect("a null identity remains diagnostic");

        assert_eq!(detail.mapping_basis().code(), "ordinal_missing_id");
        assert_eq!(detail.kind_pair().code(), "terminal_message_over_reasoning");
        assert_eq!(detail.observed_text_relation().code(), "match");
        let detail = serde_json::to_value(detail).expect("identity detail serializes");
        assert_eq!(detail["structure"]["terminal_id_presence"], "null");
        assert_eq!(detail["structure"]["terminal_ordinal"], 0);
        assert_eq!(detail["structure"]["first_observed_message_ordinal"], 1);
        assert_eq!(detail["structure"]["observed_message_delta"], 1);
        assert_eq!(detail["structure"]["observed_message_distance"], "one");
        assert_eq!(
            detail["structure"]["observed_span"],
            json!([
                {
                    "ordinal": 0,
                    "kind": "reasoning",
                    "id_presence": "present",
                    "lifecycle_state": "done",
                },
                {
                    "ordinal": 1,
                    "kind": "message",
                    "id_presence": "present",
                    "lifecycle_state": "done",
                },
            ])
        );
        assert_eq!(detail["structure"]["observed_span_count"], 2);
        assert_eq!(detail["structure"]["observed_span_truncated"], false);
        assert_eq!(detail["structure"]["observed_span_contiguous"], true);
        assert_eq!(detail["structure"]["terminal_phase"], "missing");
        assert_eq!(detail["structure"]["observed_phase"], "missing");
        assert_eq!(detail["structure"]["phase_relation"], "exact");
        assert_eq!(
            detail["structure"]["matched_message_lifecycle_state"],
            "done"
        );
        assert_eq!(
            detail["structure"]["matched_message_text_state"],
            "completed"
        );
        assert_eq!(detail["structure"]["terminal_text_bytes"], 11);
        assert_eq!(detail["structure"]["observed_text_bytes"], 11);
        assert_eq!(detail["structure"]["terminal_output_count"], 1);
        assert_eq!(detail["structure"]["terminal_final_message_count"], 1);
        assert_eq!(detail["structure"]["observed_lifecycle_count"], 2);
        assert_eq!(detail["structure"]["observed_message_count"], 1);
        assert_eq!(
            detail["structure"]["unreconciled_observed_message_count"],
            1
        );
    }

    #[test]
    fn terminal_identity_reprojection_requires_done_reasoning() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(
                0,
                reasoning("item-prefix", "in_progress", None, None),
            ))
            .unwrap();
        ledger
            .record_added(&lifecycle(1, message("item-message", "in_progress", None)))
            .unwrap();
        let frame = text_frame("item-message", 1, 0);
        ledger.record_text_delta(&frame, "output-text").unwrap();
        ledger.record_text_done(&frame, "output-text").unwrap();
        let done = message("item-message", "completed", Some("output-text"));
        ledger.record_done(&lifecycle(1, done.clone())).unwrap();
        let terminal = replace_message_id(done, None);

        let error = completion_error(ledger.complete(&completed(vec![terminal])));
        let detail = error
            .response_output_identity_detail()
            .expect("the added-only conflict remains diagnostic");
        assert_eq!(detail.observed_text_relation().code(), "match");
        assert_eq!(detail.resolved_lifecycle_state().code(), "added_only");
    }

    #[test]
    fn terminal_identity_reprojection_requires_exact_observed_phase() {
        for terminal_phase in [None, Some("final_answer")] {
            let mut ledger = OpenAiOutputLedger::default();
            let prefix = reasoning("item-prefix", "completed", None, Some("encrypted-prefix"));
            ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
            ledger.record_done(&lifecycle(0, prefix)).unwrap();
            ledger
                .record_added(&lifecycle(
                    1,
                    message_with_phase("item-message", "in_progress", None, "commentary"),
                ))
                .unwrap();
            let frame = text_frame("item-message", 1, 0);
            ledger.record_text_delta(&frame, "output-text").unwrap();
            ledger.record_text_done(&frame, "output-text").unwrap();
            let done = message_with_phase(
                "item-message",
                "completed",
                Some("output-text"),
                "commentary",
            );
            ledger.record_done(&lifecycle(1, done.clone())).unwrap();
            let mut terminal = replace_message_id(done, None);
            match terminal_phase {
                Some(phase) => terminal["phase"] = json!(phase),
                None => {
                    terminal
                        .as_object_mut()
                        .expect("terminal message fixture is an object")
                        .remove("phase");
                }
            }

            let error = completion_error(ledger.complete(&completed(vec![terminal])));
            assert_eq!(
                error.response_output_identity_reason(),
                Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
            );
            let detail = serde_json::to_value(
                error
                    .response_output_identity_detail()
                    .expect("phase drift retains structural identity evidence"),
            )
            .expect("identity detail serializes");
            assert_eq!(detail["structure"]["terminal_id_presence"], "missing");
            assert_eq!(detail["structure"]["first_observed_message_ordinal"], 1);
            assert_eq!(detail["structure"]["observed_message_distance"], "one");
            assert_eq!(detail["structure"]["observed_phase"], "commentary");
            assert_eq!(detail["structure"]["phase_relation"], "mismatch");
            assert_eq!(
                detail["structure"]["terminal_phase"],
                terminal_phase.unwrap_or("missing")
            );
        }
    }

    #[test]
    fn terminal_identity_reprojects_missing_id_across_final_phase_representations() {
        use SyntheticMessagePhaseField::{FinalAnswer, Missing, Null};

        for (observed_phase, terminal_phase) in [
            (Missing, FinalAnswer),
            (Null, FinalAnswer),
            (FinalAnswer, Missing),
            (FinalAnswer, Null),
        ] {
            let mut ledger = OpenAiOutputLedger::default();
            let prefix = reasoning("item-prefix", "completed", None, Some("encrypted-prefix"));
            ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
            ledger.record_done(&lifecycle(0, prefix)).unwrap();
            ledger
                .record_added(&lifecycle(
                    1,
                    message_with_phase_field("item-message", "in_progress", None, observed_phase),
                ))
                .unwrap();
            let frame = text_frame("item-message", 1, 0);
            ledger.record_text_delta(&frame, "output-text").unwrap();
            ledger.record_text_done(&frame, "output-text").unwrap();
            let done = message_with_phase_field(
                "item-message",
                "completed",
                Some("output-text"),
                observed_phase,
            );
            ledger.record_done(&lifecycle(1, done)).unwrap();
            let mut terminal = message_with_phase_field(
                "item-message",
                "completed",
                Some("output-text"),
                terminal_phase,
            );
            terminal
                .as_object_mut()
                .expect("terminal message is an object")
                .remove("id");

            let complete = ledger
                .complete(&completed(vec![terminal]))
                .unwrap_or_else(|error| {
                    panic!(
                        "missing-ID final phases were not reprojected: observed={observed_phase:?}, terminal={terminal_phase:?}, error={:?}",
                        error.ledger_error()
                    )
                });
            assert_eq!(complete.final_text, "output-text");
            assert!(complete.wire_items.is_empty());
        }
    }

    #[test]
    fn terminal_identity_reprojection_requires_a_later_observed_message() {
        let mut ledger = OpenAiOutputLedger::default();
        let prefix = reasoning("item-prefix", "completed", None, Some("encrypted-prefix"));
        ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
        ledger.record_done(&lifecycle(0, prefix)).unwrap();
        let terminal = replace_message_id(
            message("item-terminal", "completed", Some("output-text")),
            None,
        );

        let error = completion_error(ledger.complete(&completed(vec![terminal])));
        let detail = error
            .response_output_identity_detail()
            .expect("the missing observation remains diagnostic");
        assert_eq!(detail.observed_message_relation().code(), "none");
        assert_eq!(detail.observed_text_relation().code(), "not_observed");
        assert_eq!(detail.resolved_lifecycle_state().code(), "done");
    }

    #[test]
    fn terminal_identity_reprojection_requires_a_contiguous_observation_span() {
        let mut ledger = OpenAiOutputLedger::default();
        let prefix = reasoning("item-prefix", "completed", None, Some("encrypted-prefix"));
        ledger.record_added(&lifecycle(0, prefix.clone())).unwrap();
        ledger.record_done(&lifecycle(0, prefix)).unwrap();
        ledger
            .record_added(&lifecycle(2, message("item-message", "in_progress", None)))
            .unwrap();
        let frame = text_frame("item-message", 2, 0);
        ledger.record_text_delta(&frame, "output-text").unwrap();
        ledger.record_text_done(&frame, "output-text").unwrap();
        let done = message("item-message", "completed", Some("output-text"));
        ledger.record_done(&lifecycle(2, done.clone())).unwrap();
        let terminal = replace_message_id(done, None);

        let error = completion_error(ledger.complete(&completed(vec![terminal])));
        let detail = error
            .response_output_identity_detail()
            .expect("the sparse observation remains diagnostic");
        assert_eq!(detail.observed_message_relation().code(), "other");
        assert_eq!(detail.observed_text_relation().code(), "match");
    }

    #[test]
    fn terminal_identity_reprojection_rejects_an_intermediate_compaction() {
        let intermediate = compaction("item-intermediate", "opaque-placeholder");
        let (ledger, done) =
            doubly_prefixed_message_ledger(intermediate.clone(), Some(intermediate));
        let terminal = replace_message_id(done, None);

        assert_eq!(
            completion_error(ledger.complete(&completed(vec![terminal])))
                .response_output_identity_reason(),
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
        );
    }

    #[test]
    fn terminal_identity_reprojection_rejects_intermediate_added_only_reasoning() {
        let intermediate = reasoning("item-intermediate", "in_progress", None, None);
        let (ledger, done) = doubly_prefixed_message_ledger(intermediate, None);
        let terminal = replace_message_id(done, None);

        assert_eq!(
            completion_error(ledger.complete(&completed(vec![terminal])))
                .response_output_identity_reason(),
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
        );
    }

    #[test]
    fn terminal_identity_reprojects_over_intermediate_done_reasoning() {
        let added = reasoning("item-intermediate", "in_progress", None, None);
        let done_reasoning = reasoning(
            "item-intermediate",
            "completed",
            None,
            Some("encrypted-intermediate"),
        );
        let (ledger, done) = doubly_prefixed_message_ledger(added, Some(done_reasoning));
        let terminal = replace_message_id(done, None);

        let complete = ledger
            .complete(&completed(vec![terminal]))
            .expect("a completed reasoning prefix may be omitted from terminal output");

        assert_eq!(complete.final_text, "output-text");
        assert_eq!(complete.output_items.len(), 1);
        assert!(complete.wire_items.is_empty());
    }

    #[test]
    fn terminal_identity_reconciliation_rejects_known_message_phase_drift() {
        for terminal_phase in [None, Some("final_answer")] {
            let mut ledger = OpenAiOutputLedger::default();
            ledger
                .record_added(&lifecycle(
                    0,
                    message_with_phase("item-message", "in_progress", None, "commentary"),
                ))
                .unwrap();
            let frame = text_frame("item-message", 0, 0);
            ledger.record_text_delta(&frame, "output-text").unwrap();
            ledger.record_text_done(&frame, "output-text").unwrap();
            let done = message_with_phase(
                "item-message",
                "completed",
                Some("output-text"),
                "commentary",
            );
            ledger.record_done(&lifecycle(0, done.clone())).unwrap();
            let mut terminal = done;
            match terminal_phase {
                Some(phase) => terminal["phase"] = json!(phase),
                None => {
                    terminal
                        .as_object_mut()
                        .expect("terminal message fixture is an object")
                        .remove("phase");
                }
            }

            assert!(ledger.complete(&completed(vec![terminal])).is_err());
        }
    }

    #[test]
    fn terminal_phase_reconciliation_accepts_only_equivalent_applicable_phases() {
        use SyntheticMessagePhaseField::{Commentary, FinalAnswer, Missing, Null};

        let cases = [
            (Missing, Missing, true),
            (Missing, Null, true),
            (Missing, FinalAnswer, true),
            (Missing, Commentary, false),
            (Null, Missing, true),
            (Null, Null, true),
            (Null, FinalAnswer, true),
            (Null, Commentary, false),
            (FinalAnswer, Missing, true),
            (FinalAnswer, Null, true),
            (FinalAnswer, FinalAnswer, true),
            (FinalAnswer, Commentary, false),
            (Commentary, Missing, false),
            (Commentary, Null, false),
            (Commentary, FinalAnswer, false),
            (Commentary, Commentary, true),
        ];

        for (observed_phase, terminal_phase, equivalent) in cases {
            let mut ledger = OpenAiOutputLedger::default();
            ledger
                .record_added(&lifecycle(
                    0,
                    message_with_phase_field("item-message", "in_progress", None, observed_phase),
                ))
                .unwrap();
            let frame = text_frame("item-message", 0, 0);
            let dispatched = ledger.record_text_delta(&frame, "output-text").unwrap();
            assert_eq!(
                dispatched,
                !matches!(observed_phase, Commentary),
                "observed={observed_phase:?}, terminal={terminal_phase:?}"
            );
            ledger.record_text_done(&frame, "output-text").unwrap();
            ledger
                .record_done(&lifecycle(
                    0,
                    message_with_phase_field(
                        "item-message",
                        "completed",
                        Some("output-text"),
                        observed_phase,
                    ),
                ))
                .unwrap();

            let terminal_message = message_with_phase_field(
                "item-message",
                "completed",
                Some("output-text"),
                terminal_phase,
            );
            let terminal = if equivalent && matches!(terminal_phase, Commentary) {
                vec![
                    terminal_message,
                    message_with_phase_field(
                        "item-final",
                        "completed",
                        Some("final-output"),
                        FinalAnswer,
                    ),
                ]
            } else {
                vec![terminal_message]
            };
            let result = ledger.complete(&completed(terminal));

            if equivalent && matches!(terminal_phase, Commentary) {
                assert!(result.is_err());
            } else if equivalent {
                let complete = result.unwrap_or_else(|error| {
                    panic!(
                        "equivalent phases were rejected: observed={observed_phase:?}, terminal={terminal_phase:?}, error={:?}",
                        error.ledger_error()
                    )
                });
                let expected_final = if matches!(terminal_phase, Commentary) {
                    "final-output"
                } else {
                    "output-text"
                };
                assert_eq!(complete.final_text, expected_final);
                if !matches!(terminal_phase, Commentary) {
                    let retained_phase = match complete.output_items.first() {
                        Some(CanonicalInputItem::AssistantText { phase, .. }) => *phase,
                        _ => panic!("the reconciled message is retained as assistant text"),
                    };
                    let explicit_final_supplied = matches!(observed_phase, FinalAnswer);
                    assert_eq!(
                        retained_phase,
                        explicit_final_supplied.then_some(AssistantPhase::FinalAnswer),
                        "observed={observed_phase:?}, terminal={terminal_phase:?}"
                    );
                    assert!(complete.wire_items.is_empty());
                }
            } else {
                let error = match result {
                    Ok(_) => panic!(
                        "non-equivalent phases were accepted: observed={observed_phase:?}, terminal={terminal_phase:?}"
                    ),
                    Err(error) => error.ledger_error(),
                };
                assert_eq!(
                    error,
                    ResponseOutputLedgerError::TerminalMessagePhaseMismatch {
                        terminal_output_index: 0,
                        observed_lifecycle_index: 0,
                    },
                    "non-equivalent phases were accepted: observed={observed_phase:?}, terminal={terminal_phase:?}"
                );
            }
        }
    }

    #[test]
    fn streamed_output_item_done_uses_final_phase_class_and_rejects_commentary_crossings() {
        use SyntheticMessagePhaseField::{Commentary, FinalAnswer, Missing, Null};

        for (added_phase, done_phase) in [
            (Missing, FinalAnswer),
            (Null, FinalAnswer),
            (FinalAnswer, Missing),
            (FinalAnswer, Null),
        ] {
            let mut ledger = OpenAiOutputLedger::default();
            ledger
                .record_added(&lifecycle(
                    0,
                    message_with_phase_field("item-message", "in_progress", None, added_phase),
                ))
                .unwrap();
            let frame = text_frame("item-message", 0, 0);
            ledger.record_text_delta(&frame, "output-text").unwrap();
            ledger.record_text_done(&frame, "output-text").unwrap();
            ledger
                .record_done(&lifecycle(
                    0,
                    message_with_phase_field(
                        "item-message",
                        "completed",
                        Some("output-text"),
                        done_phase,
                    ),
                ))
                .unwrap_or_else(|error| {
                    panic!(
                        "equivalent streamed phases were rejected: added={added_phase:?}, done={done_phase:?}, error={error}"
                    )
                });
            assert_eq!(
                ledger.items.get(&0).map(|item| item.phase),
                Some(Some(AssistantPhase::FinalAnswer))
            );
        }

        for (added_phase, done_phase) in [
            (Commentary, Missing),
            (Commentary, Null),
            (Commentary, FinalAnswer),
            (Missing, Commentary),
            (Null, Commentary),
            (FinalAnswer, Commentary),
        ] {
            let mut ledger = OpenAiOutputLedger::default();
            ledger
                .record_added(&lifecycle(
                    0,
                    message_with_phase_field("item-message", "in_progress", None, added_phase),
                ))
                .unwrap();
            let frame = text_frame("item-message", 0, 0);
            ledger.record_text_delta(&frame, "output-text").unwrap();
            ledger.record_text_done(&frame, "output-text").unwrap();
            assert!(
                ledger
                    .record_done(&lifecycle(
                        0,
                        message_with_phase_field(
                            "item-message",
                            "completed",
                            Some("output-text"),
                            done_phase,
                        ),
                    ))
                    .is_err(),
                "commentary crossing was accepted: added={added_phase:?}, done={done_phase:?}"
            );
        }
    }

    #[test]
    fn terminal_identity_reconciliation_keeps_unframed_snapshots_observational() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(
                0,
                message_with_phase(
                    "item-message",
                    "in_progress",
                    Some("snapshot-placeholder"),
                    "commentary",
                ),
            ))
            .unwrap();
        ledger
            .record_done(&lifecycle(
                0,
                message_with_phase(
                    "item-message",
                    "completed",
                    Some("snapshot-placeholder"),
                    "final_answer",
                ),
            ))
            .unwrap();
        let terminal = message_with_phase(
            "item-message",
            "completed",
            Some("output-text"),
            "final_answer",
        );

        assert!(ledger.complete(&completed(vec![terminal])).is_err());
    }

    #[test]
    fn terminal_identity_reprojection_does_not_orphan_an_observed_message() {
        let (mut ledger, done) = prefixed_message_ledger(reasoning(
            "item-prefix",
            "completed",
            None,
            Some("encrypted-prefix"),
        ));
        ledger
            .record_added(&lifecycle(2, message("item-trailing", "in_progress", None)))
            .unwrap();
        ledger
            .record_done(&lifecycle(
                2,
                message("item-trailing", "completed", Some("unobserved")),
            ))
            .unwrap();
        let terminal = replace_message_id(done, None);

        assert!(ledger.complete(&completed(vec![terminal])).is_err());
    }

    #[test]
    fn terminal_identity_rejects_unknown_and_regenerated_ids_after_omitted_prefixes() {
        for (prefix_name, prefix) in [
            (
                "reasoning",
                reasoning("item-prefix", "completed", None, Some("encrypted-prefix")),
            ),
            ("compaction", compaction("item-prefix", "opaque")),
        ] {
            for (id_name, terminal_id) in [
                ("unknown", "item-terminal-unknown"),
                ("regenerated", "item-terminal-regenerated"),
            ] {
                let (ledger, done) = prefixed_message_ledger(prefix.clone());
                let terminal = replace_message_id(done, Some(json!(terminal_id)));

                assert_eq!(
                    completion_error(ledger.complete(&completed(vec![terminal])))
                        .response_output_identity_reason(),
                    Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal),
                    "{prefix_name}/{id_name} must remain an identity conflict",
                );
            }
        }
    }

    #[test]
    fn terminal_identity_keeps_known_cross_kind_substitutions_rejected() {
        let (reasoning_ledger, reasoning_done) = prefixed_message_ledger(reasoning(
            "item-prefix",
            "completed",
            None,
            Some("encrypted-prefix"),
        ));
        let terminal_message = replace_message_id(reasoning_done, Some(json!("item-prefix")));
        assert_eq!(
            completion_error(reasoning_ledger.complete(&completed(vec![terminal_message])))
                .response_output_identity_reason(),
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
        );

        let mut message_ledger = OpenAiOutputLedger::default();
        message_ledger
            .record_added(&lifecycle(0, message("item-message", "in_progress", None)))
            .unwrap();
        let terminal_reasoning = reasoning(
            "item-message",
            "completed",
            Some("summary-placeholder"),
            Some("encrypted-placeholder"),
        );
        assert_eq!(
            completion_error(message_ledger.complete(&completed(vec![terminal_reasoning])))
                .response_output_identity_reason(),
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
        );

        let mut compaction_ledger = OpenAiOutputLedger::default();
        compaction_ledger
            .record_added(&lifecycle(
                0,
                compaction("item-compaction", "opaque-placeholder"),
            ))
            .unwrap();
        let terminal_message = message("item-compaction", "completed", Some("output-text"));
        assert_eq!(
            completion_error(compaction_ledger.complete(&completed(vec![terminal_message])))
                .response_output_identity_reason(),
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
        );
    }

    #[test]
    fn terminal_kind_conflict_diagnostic_distinguishes_binding_shape_and_observation() {
        let (missing_ledger, missing_done) = prefixed_message_ledger(reasoning(
            "item-prefix",
            "completed",
            None,
            Some("encrypted-prefix"),
        ));
        let mut missing_terminal = replace_message_id(missing_done, None);
        missing_terminal["content"][0]["text"] = json!("different-output");
        let missing_error =
            completion_error(missing_ledger.complete(&completed(vec![missing_terminal])));
        let missing = missing_error
            .response_output_identity_detail()
            .expect("kind conflict exposes a closed diagnostic detail");
        assert_eq!(missing.mapping_basis().code(), "ordinal_missing_id");
        assert_eq!(
            missing.kind_pair().code(),
            "terminal_message_over_reasoning"
        );
        assert_eq!(
            missing.observed_message_relation().code(),
            "next_after_contiguous_nontext"
        );
        assert_eq!(missing.observed_text_relation().code(), "mismatch");
        assert_eq!(missing.resolved_lifecycle_state().code(), "done");

        let (unknown_ledger, unknown_done) =
            prefixed_message_ledger(compaction("item-prefix", "opaque-placeholder"));
        let mut unknown_terminal =
            replace_message_id(unknown_done, Some(json!("item-terminal-unknown")));
        unknown_terminal["content"][0]["text"] = json!("different-output");
        let unknown_error =
            completion_error(unknown_ledger.complete(&completed(vec![unknown_terminal])));
        let unknown = unknown_error
            .response_output_identity_detail()
            .expect("unknown identity kind conflict is classified");
        assert_eq!(unknown.mapping_basis().code(), "ordinal_unknown_id");
        assert_eq!(
            unknown.kind_pair().code(),
            "terminal_message_over_compaction"
        );
        assert_eq!(
            unknown.observed_message_relation().code(),
            "next_after_contiguous_nontext"
        );
        assert_eq!(unknown.observed_text_relation().code(), "mismatch");
        assert_eq!(unknown.resolved_lifecycle_state().code(), "done");

        let mut prefix_only = OpenAiOutputLedger::default();
        prefix_only
            .record_added(&lifecycle(
                0,
                reasoning("item-prefix", "in_progress", None, None),
            ))
            .unwrap();
        let terminal = replace_message_id(
            message("item-terminal", "completed", Some("output-text")),
            None,
        );
        let prefix_only_error = completion_error(prefix_only.complete(&completed(vec![terminal])));
        let prefix_only = prefix_only_error
            .response_output_identity_detail()
            .expect("added-only conflict is classified");
        assert_eq!(prefix_only.observed_message_relation().code(), "none");
        assert_eq!(prefix_only.observed_text_relation().code(), "not_observed");
        assert_eq!(prefix_only.resolved_lifecycle_state().code(), "added_only");

        let (known_ledger, known_done) =
            prefixed_message_ledger(compaction("item-prefix", "opaque-placeholder"));
        let known_terminal = replace_message_id(known_done, Some(json!("item-prefix")));
        let known_error = completion_error(known_ledger.complete(&completed(vec![known_terminal])));
        let known = known_error
            .response_output_identity_detail()
            .expect("known cross-kind conflict is classified");
        assert_eq!(known.mapping_basis().code(), "known_id");

        let mut same_ordinal = OpenAiOutputLedger::default();
        same_ordinal
            .record_added(&lifecycle(0, message("item-message", "in_progress", None)))
            .unwrap();
        let terminal_reasoning = reasoning(
            "item-message",
            "completed",
            Some("summary-placeholder"),
            Some("encrypted-placeholder"),
        );
        let same_ordinal_error =
            completion_error(same_ordinal.complete(&completed(vec![terminal_reasoning])));
        let same_ordinal = same_ordinal_error
            .response_output_identity_detail()
            .expect("same-ordinal conflict is classified");
        assert_eq!(same_ordinal.kind_pair().code(), "other");
        assert_eq!(
            same_ordinal.observed_message_relation().code(),
            "same_ordinal"
        );
    }

    #[test]
    fn terminal_identity_rejects_regenerated_rebound_and_duplicate_present_ids() {
        let (ledger, _) = completed_message_ledger();
        assert!(matches!(
            ledger
                .complete(&completed(vec![message(
                    "msg_regenerated",
                    "completed",
                    Some("<ok />"),
                )]))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                ProviderResponseOutputIdentityReason::SameIndexIdConflict
            ))
        ));

        let mut rebound = OpenAiOutputLedger::default();
        rebound
            .record_added(&lifecycle(0, compaction("cmp_1", "opaque")))
            .unwrap();
        assert_eq!(
            completion_error(rebound.complete(&completed(vec![message(
                "cmp_1",
                "completed",
                Some("<ok />"),
            )])))
            .response_output_identity_reason(),
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal)
        );

        assert!(OpenAiOutputLedger::default()
            .complete(&completed(vec![
                message("msg_duplicate", "completed", Some("first")),
                message("msg_duplicate", "completed", Some("second")),
            ]))
            .is_err());

        let mut omitted_message = OpenAiOutputLedger::default();
        omitted_message
            .record_added(&lifecycle(0, message("msg_omitted", "in_progress", None)))
            .unwrap();
        omitted_message
            .record_done(&lifecycle(
                0,
                message("msg_omitted", "completed", Some("unobserved")),
            ))
            .unwrap();
        omitted_message
            .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 1, 0);
        omitted_message.record_text_delta(&frame, "<ok />").unwrap();
        omitted_message.record_text_done(&frame, "<ok />").unwrap();
        let done = message("msg_1", "completed", Some("<ok />"));
        omitted_message
            .record_done(&lifecycle(1, done.clone()))
            .unwrap();
        assert!(omitted_message.complete(&completed(vec![done])).is_err());

        let mut sparse = OpenAiOutputLedger::default();
        sparse
            .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 1, 0);
        sparse.record_text_delta(&frame, "<ok />").unwrap();
        sparse.record_text_done(&frame, "<ok />").unwrap();
        let done = message("msg_1", "completed", Some("<ok />"));
        sparse.record_done(&lifecycle(1, done.clone())).unwrap();
        assert!(sparse.complete(&completed(vec![done])).is_ok());
    }

    #[test]
    fn terminal_identity_rejects_an_unreconciled_trailing_message_without_text_frames() {
        let (mut ledger, done) = completed_message_ledger();
        ledger
            .record_added(&lifecycle(1, message("msg_trailing", "in_progress", None)))
            .unwrap();
        ledger
            .record_done(&lifecycle(
                1,
                message("msg_trailing", "completed", Some("unobserved")),
            ))
            .unwrap();

        assert!(ledger.complete(&completed(vec![done])).is_err());
    }

    #[test]
    fn terminal_only_compaction_is_rejected_without_a_sealed_lifecycle_item() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 1, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        let done = message("msg_1", "completed", Some("<ok />"));
        ledger.record_done(&lifecycle(1, done.clone())).unwrap();

        assert!(matches!(
            ledger
                .complete(&completed(vec![compaction("cmp_terminal", "opaque"), done]))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::InvalidCompletedItem)
        ));
    }

    #[test]
    fn terminal_only_reasoning_is_rejected_without_a_sealed_lifecycle_item() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 1, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        let done = message("msg_1", "completed", Some("<ok />"));
        ledger.record_done(&lifecycle(1, done.clone())).unwrap();

        assert!(matches!(
            ledger
                .complete(&completed(vec![
                    reasoning(
                        "rs_terminal",
                        "completed",
                        Some("summary"),
                        Some("encrypted")
                    ),
                    done,
                ]))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::InvalidCompletedItem)
        ));
    }

    #[test]
    fn terminal_identity_preserves_order_for_multi_item_observational_subsequences() {
        let fixture = || {
            let mut ledger = OpenAiOutputLedger::default();
            for index in 0..3 {
                let item = compaction(&format!("cmp_{index}"), "opaque");
                ledger
                    .record_added(&lifecycle(index, item.clone()))
                    .unwrap();
                ledger.record_done(&lifecycle(index, item)).unwrap();
            }
            ledger
                .record_added(&lifecycle(3, message("msg_1", "in_progress", None)))
                .unwrap();
            let frame = text_frame("msg_1", 3, 0);
            ledger.record_text_delta(&frame, "<ok />").unwrap();
            ledger.record_text_done(&frame, "<ok />").unwrap();
            let done = message("msg_1", "completed", Some("<ok />"));
            ledger.record_done(&lifecycle(3, done.clone())).unwrap();
            (ledger, done)
        };

        let (ledger, done) = fixture();
        assert!(ledger
            .complete(&completed(vec![
                compaction("cmp_0", "opaque"),
                compaction("cmp_2", "opaque"),
                done,
            ]))
            .is_ok());

        let (ledger, done) = fixture();
        assert!(matches!(
            ledger
                .complete(&completed(vec![
                    compaction("cmp_2", "opaque"),
                    compaction("cmp_1", "opaque"),
                    done,
                ]))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                ProviderResponseOutputIdentityReason::NonMonotonicLifecycleMapping
            ))
        ));

        let (ledger, done) = fixture();
        assert!(matches!(
            ledger
                .complete(&completed(vec![
                    compaction("cmp_1", "opaque"),
                    compaction("cmp_0", "opaque"),
                    done,
                ]))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::TerminalOutputIdentity(
                ProviderResponseOutputIdentityReason::KnownIdBeforeTerminalOrdinal
            ))
        ));
    }

    #[test]
    fn reasoning_reconciliation_treats_omitted_and_empty_content_as_equivalent() {
        let cases = [
            ("done-omitted", None, Some(json!([]))),
            ("terminal-omitted", Some(json!([])), None),
        ];

        for (name, done_content, terminal_content) in cases {
            let mut ledger = OpenAiOutputLedger::default();
            ledger
                .record_added(&lifecycle(0, reasoning("rs_1", "in_progress", None, None)))
                .unwrap();
            let done_reasoning = reasoning_with_content(
                reasoning("rs_1", "completed", Some("summary"), Some("encrypted")),
                done_content,
            );
            ledger.record_done(&lifecycle(0, done_reasoning)).unwrap();

            ledger
                .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
                .unwrap();
            let frame = text_frame("msg_1", 1, 0);
            ledger.record_text_delta(&frame, "<ok />").unwrap();
            ledger.record_text_done(&frame, "<ok />").unwrap();
            let done_message = message("msg_1", "completed", Some("<ok />"));
            ledger
                .record_done(&lifecycle(1, done_message.clone()))
                .unwrap();

            let terminal_reasoning = reasoning_with_content(
                reasoning("rs_1", "completed", Some("summary"), Some("encrypted")),
                terminal_content,
            );
            assert!(
                ledger
                    .complete(&completed(vec![terminal_reasoning, done_message]))
                    .is_ok(),
                "case {name}"
            );
        }
    }

    #[test]
    fn terminal_reasoning_body_drift_is_rejected_after_done_time_sealing() {
        let mut ledger = OpenAiOutputLedger::default();
        ledger
            .record_added(&lifecycle(0, reasoning("rs_1", "in_progress", None, None)))
            .unwrap();
        ledger
            .record_done(&lifecycle(
                0,
                reasoning("rs_1", "completed", Some("summary"), Some("encrypted-a")),
            ))
            .unwrap();
        ledger
            .record_added(&lifecycle(1, message("msg_1", "in_progress", None)))
            .unwrap();
        let frame = text_frame("msg_1", 1, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        let done_message = message("msg_1", "completed", Some("<ok />"));
        ledger
            .record_done(&lifecycle(1, done_message.clone()))
            .unwrap();

        assert!(matches!(
            ledger
                .complete(&completed(vec![
                    reasoning("rs_1", "completed", Some("summary"), Some("encrypted-b")),
                    done_message,
                ]))
                .map_err(ResponseOutputLedgerFailure::ledger_error),
            Err(ResponseOutputLedgerError::InvalidCompletedItem)
        ));
    }

    #[test]
    fn compaction_is_retained_in_canonical_request_shape_without_becoming_semantic_history() {
        let mut ledger = OpenAiOutputLedger::default();
        let compacted = compaction("cmp_1", "opaque-compaction-payload");
        ledger
            .record_added(&lifecycle(0, compacted.clone()))
            .unwrap();
        let sealed = ledger
            .record_done(&lifecycle(0, compacted.clone()))
            .unwrap()
            .expect("valid compaction becomes a sealed causal fact at item.done");
        assert_eq!(
            sealed.wire_item,
            json!({
                "id": "cmp_1",
                "type": "compaction",
                "encrypted_content": "opaque-compaction-payload",
            })
        );

        let added_message = message("msg_1", "in_progress", None);
        ledger.record_added(&lifecycle(1, added_message)).unwrap();
        let frame = text_frame("msg_1", 1, 0);
        ledger.record_text_delta(&frame, "<ok />").unwrap();
        ledger.record_text_done(&frame, "<ok />").unwrap();
        let done_message = message("msg_1", "completed", Some("<ok />"));
        ledger
            .record_done(&lifecycle(1, done_message.clone()))
            .unwrap();

        let complete = ledger
            .complete(&completed(vec![compacted.clone(), done_message.clone()]))
            .unwrap();

        assert_eq!(complete.output_items.len(), 1);
        assert!(complete.wire_items.is_empty());
        assert_eq!(complete.final_text, "<ok />");
    }

    #[test]
    fn supported_output_shapes_fail_closed_at_authoritative_publication() {
        let mut ledger = OpenAiOutputLedger::default();
        assert!(ledger
            .record_added(&lifecycle(0, json!({"type": "function_call"})))
            .is_err());
        assert!(ledger
            .record_added(&lifecycle(0, message("", "in_progress", None)))
            .is_err());
        ledger
            .record_added(&lifecycle(0, message("msg_1", "completed", None)))
            .unwrap();
        assert!(ledger
            .complete(&completed(vec![message(
                "msg_1",
                "completed",
                Some("<ok />"),
            )]))
            .is_err());

        let mut incomplete_final = OpenAiOutputLedger::default();
        incomplete_final
            .record_added(&lifecycle(0, message("msg_1", "incomplete", None)))
            .unwrap();
        assert!(incomplete_final
            .complete(&completed(vec![message(
                "msg_1",
                "incomplete",
                Some("<ok />"),
            )]))
            .is_err());

        let mut missing_encrypted = OpenAiOutputLedger::default();
        missing_encrypted
            .record_added(&lifecycle(0, reasoning("rs_1", "in_progress", None, None)))
            .unwrap();
        let observed = reasoning("rs_1", "completed", Some("summary"), None);
        assert!(missing_encrypted
            .record_done(&lifecycle(0, observed))
            .is_err());

        let mut invalid_reasoning = OpenAiOutputLedger::default();
        invalid_reasoning
            .record_added(&lifecycle(0, reasoning("rs_1", "in_progress", None, None)))
            .unwrap();
        assert!(invalid_reasoning
            .record_done(&lifecycle(
                0,
                reasoning("rs_1", "completed", Some(""), Some("encrypted")),
            ))
            .is_err());

        let done = reasoning("rs_1", "completed", Some("summary"), Some("encrypted"));
        invalid_reasoning
            .record_done(&lifecycle(0, done.clone()))
            .unwrap();
        assert!(invalid_reasoning.complete(&completed(vec![done])).is_err());
    }
}
