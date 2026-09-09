use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io::{self, Write},
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    component::execution::{
        ProjectionAppendPolicy, ProjectionDiffState, ProjectionExecutionScope, RenderedProjection,
        RenderedProjectionFragment, RenderedProjectionNode, ToolDefinition, ToolOutput,
    },
    provider::codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Error},
    transcript::{
        AssistantPhase, CanonicalInputItem, CanonicalTranscript, CanonicalTranscriptError,
        InstructionAuthority,
    },
};

use super::artifact_binding::OpenAiInlineArtifactBinding;
use super::output::SealedOpenAiPrivateOutput;

#[derive(Clone)]
pub(super) struct OpenAiContinuation {
    submitted_projection: RenderedProjection,
    submitted_items: Vec<CanonicalInputItem>,
    unclaimed_provider_outputs: Vec<CanonicalInputItem>,
    wire_input: Vec<Value>,
    history_epoch: u64,
    semantic_budget: ContinuationSemanticBudget,
    native_tools: Vec<ToolDefinition>,
    open_text_outputs: BTreeMap<u64, OpenTextOutput>,
    partial_records: Vec<PartialTextRecord>,
    sealed_text_outputs: HashSet<u64>,
    sealed_private_outputs: BTreeSet<u64>,
    sealed_output_order: BTreeMap<u64, SealedOutputPlacement>,
    #[cfg(test)]
    reconciliation_index_probes: usize,
}

/// A disposable reconciliation cache, deliberately separate from causal
/// history. Its scope fences host remounts even when ComponentId paths repeat.
#[derive(Debug, Clone)]
pub(super) struct ProjectionDiffMemo {
    history_epoch: u64,
    scope: Option<ProjectionExecutionScope>,
    diff_state: ProjectionDiffState,
    diff_sidecars: HashMap<SemanticSidecarKey, SemanticUsage>,
}

impl ProjectionDiffMemo {
    fn valid_for(&self, history_epoch: u64, scope: Option<ProjectionExecutionScope>) -> bool {
        self.history_epoch == history_epoch && self.scope == scope
    }
}

/// One exact outbound request plus the causal and memo candidates that may be
/// advanced together only when transport accepts the handoff.
pub(super) struct ResponsesInputGateReady {
    pub(super) request_body: Vec<u8>,
    /// Causal history candidate: installed only at transport handoff.
    pub(super) candidate: OpenAiContinuation,
    /// Disposable projection-diff memo candidate: installed with the causal
    /// candidate, but never treated as causal history.
    pub(super) artifact_binding: OpenAiInlineArtifactBinding,
    /// The complete provider-owned staging snapshot that this exact request
    /// encodes. Handoff must compare-and-consume the whole table.
    pub(super) staged_output_receipt: Vec<(u64, ToolOutput)>,
}

#[derive(Debug, Clone)]
struct OpenTextOutput {
    text: String,
    phase: Option<AssistantPhase>,
}

#[derive(Debug, Clone)]
struct PartialTextRecord {
    output_index: u64,
    delta: String,
}

/// One response-local sealed fact. The map is intentionally reset at every
/// Input Gate handoff; its sole job is to preserve a provider response's
/// output_index order when item.done frames arrive out of order.
#[derive(Clone)]
struct SealedOutputPlacement {
    semantic_output: Option<CanonicalInputItem>,
    wire_item: Value,
}

impl OpenAiContinuation {
    #[cfg(test)]
    pub(super) fn prepare(
        previous: Option<&Self>,
        artifact_binding: Option<&OpenAiInlineArtifactBinding>,
        current: RenderedProjection,
        encoder: &CodexHttpV1Encoder,
    ) -> Result<ResponsesInputGateReady, OpenAiContinuationError> {
        Self::prepare_bounded(
            previous,
            artifact_binding,
            &[],
            current,
            encoder,
            usize::MAX,
        )
    }

    pub(super) fn prepare_bounded(
        previous: Option<&Self>,
        artifact_binding: Option<&OpenAiInlineArtifactBinding>,
        staged_output_receipt: &[(u64, ToolOutput)],
        current: RenderedProjection,
        encoder: &CodexHttpV1Encoder,
        max_serialized_request_body_bytes: usize,
    ) -> Result<ResponsesInputGateReady, OpenAiContinuationError> {
        // Staged ToolOutputs are new outbound input, not retained replay. The
        // candidate below is private and disposable until the same GateReady
        // crosses transport handoff.
        let staged_previous = match (previous, staged_output_receipt.is_empty()) {
            (Some(previous), false) => {
                let mut candidate = previous.clone();
                candidate.append_tool_outputs(staged_output_receipt.iter().cloned())?;
                Some(candidate)
            }
            (Some(previous), true) => Some(previous.clone()),
            (None, false) => return Err(OpenAiContinuationError::InvalidEncodedRequest),
            (None, true) => None,
        };
        let previous = staged_previous.as_ref();
        let semantic_budget =
            ContinuationSemanticBudget::from_request_limit(max_serialized_request_body_bytes);
        let previous_memo = match (previous, artifact_binding) {
            (Some(previous), Some(binding)) => binding
                .projection_diff_memo()
                .filter(|memo| memo.valid_for(previous.history_epoch, current.execution_scope())),
            _ => None,
        };
        let current_scope = current.execution_scope();
        let force_full_projection = previous.is_some()
            && (artifact_binding.is_none()
                || (current_scope.is_some() && previous_memo.is_none())
                || previous.is_some_and(|previous| {
                    previous.submitted_projection.execution_scope() != current_scope
                }));
        let previous_diff_sidecars = previous_memo.map(|memo| &memo.diff_sidecars);
        let diff_sidecars =
            reconcile_semantic_diff_sidecars(previous_diff_sidecars, &current, semantic_budget)?;
        let diff_sidecar_usage = semantic_sidecar_usage(&diff_sidecars)?;
        validate_semantic_projection(&current, diff_sidecar_usage, semantic_budget)?;
        let previous_diff = previous_memo.map(|memo| &memo.diff_state);
        let prepared_diff = ProjectionDiffState::prepare(previous_diff, &current);
        let current = match current_scope {
            Some(scope) => prepared_diff.submission.with_execution_scope(scope),
            None => prepared_diff.submission,
        };
        let append_policy = prepared_diff.append_policy;
        let current_transcript = current.to_transcript()?;
        let candidate_history_epoch = match previous {
            Some(previous) => previous
                .history_epoch
                .checked_add(1)
                .ok_or(OpenAiContinuationError::InvalidEncodedRequest)?,
            None => 1,
        };
        let candidate_memo = ProjectionDiffMemo {
            history_epoch: candidate_history_epoch,
            scope: current.execution_scope(),
            diff_state: prepared_diff.candidate,
            diff_sidecars,
        };

        let (request_body, candidate, artifact_binding) = match previous {
            Some(previous) => {
                let reconciled = reconcile_projection(
                    previous,
                    &current,
                    &append_policy,
                    force_full_projection,
                )?;
                validate_semantic_state(
                    &reconciled.submitted_projection,
                    &reconciled.submitted_items,
                    &reconciled.unclaimed_provider_outputs,
                    diff_sidecar_usage,
                    semantic_budget,
                )?;
                CanonicalTranscript::validate_sequence(
                    reconciled
                        .submitted_items
                        .iter()
                        .chain(&reconciled.unclaimed_provider_outputs),
                )?;
                ensure_all_tool_calls_closed(
                    reconciled
                        .submitted_items
                        .iter()
                        .chain(&reconciled.unclaimed_provider_outputs),
                )?;
                let typed_request = encoder
                    .request_with_native_tools(&current_transcript, current.native_tools())?;
                let canonical_input = typed_request.canonical_input()?;
                if provider_input_items(&current).count() != canonical_input.len() {
                    return Err(OpenAiContinuationError::InvalidEncodedRequest);
                }
                let new_input = reconciled
                    .new_input_indices
                    .iter()
                    .map(|index| canonical_input[*index].clone())
                    .collect::<Vec<_>>();
                // Instructions are part of the current projection, never a
                // stale sidecar. Losing a memo/binding only loses the diff
                // baseline; causal replay remains intact.
                let artifact_binding =
                    OpenAiInlineArtifactBinding::new(typed_request.instructions().to_owned())
                        .with_projection_diff_memo(candidate_memo);
                let wire_input = previous
                    .wire_input
                    .iter()
                    .cloned()
                    .chain(new_input)
                    .collect::<Vec<_>>();
                let request_body = typed_request.encode_with_input_and_instructions_bounded(
                    &wire_input,
                    artifact_binding.instructions(),
                    max_serialized_request_body_bytes,
                )?;
                (
                    request_body,
                    Self {
                        submitted_projection: reconciled.submitted_projection,
                        submitted_items: reconciled.submitted_items,
                        unclaimed_provider_outputs: reconciled.unclaimed_provider_outputs,
                        wire_input,
                        history_epoch: candidate_history_epoch,
                        semantic_budget,
                        native_tools: current.native_tools().to_vec(),
                        open_text_outputs: BTreeMap::new(),
                        partial_records: Vec::new(),
                        sealed_text_outputs: HashSet::new(),
                        sealed_private_outputs: BTreeSet::new(),
                        sealed_output_order: BTreeMap::new(),
                        #[cfg(test)]
                        reconciliation_index_probes: reconciled.reconciliation_index_probes,
                    },
                    artifact_binding,
                )
            }
            None => {
                validate_semantic_state(
                    &current,
                    current_transcript.items(),
                    &[],
                    diff_sidecar_usage,
                    semantic_budget,
                )?;
                ensure_all_tool_calls_closed(current_transcript.items())?;
                let typed_request = encoder
                    .request_with_native_tools(&current_transcript, current.native_tools())?;
                let current_binding =
                    OpenAiInlineArtifactBinding::new(typed_request.instructions().to_owned())
                        .with_projection_diff_memo(candidate_memo);
                let native_tools = current.native_tools().to_vec();
                let request_body =
                    typed_request.encode_bounded(max_serialized_request_body_bytes)?;
                let canonical_input = typed_request.canonical_input()?;
                if provider_input_items(&current).count() != canonical_input.len() {
                    return Err(OpenAiContinuationError::InvalidEncodedRequest);
                }
                (
                    request_body,
                    Self {
                        submitted_projection: current,
                        submitted_items: current_transcript.into_items(),
                        unclaimed_provider_outputs: Vec::new(),
                        wire_input: canonical_input,
                        history_epoch: candidate_history_epoch,
                        semantic_budget,
                        native_tools,
                        open_text_outputs: BTreeMap::new(),
                        partial_records: Vec::new(),
                        sealed_text_outputs: HashSet::new(),
                        sealed_private_outputs: BTreeSet::new(),
                        sealed_output_order: BTreeMap::new(),
                        #[cfg(test)]
                        reconciliation_index_probes: 0,
                    },
                    current_binding,
                )
            }
        };

        Ok(ResponsesInputGateReady {
            request_body,
            candidate,
            artifact_binding,
            staged_output_receipt: staged_output_receipt.to_vec(),
        })
    }

    pub(super) fn with_outputs(
        mut self,
        output_items: Vec<CanonicalInputItem>,
        wire_items: Vec<Value>,
    ) -> Result<Self, OpenAiContinuationError> {
        let text_was_sealed = !self.sealed_text_outputs.is_empty();
        let output_items = if !text_was_sealed {
            output_items
        } else {
            output_items
                .into_iter()
                .filter(|item| !matches!(item, CanonicalInputItem::AssistantText { .. }))
                .collect()
        };
        validate_semantic_state_with_outputs(
            &self.submitted_projection,
            &self.submitted_items,
            &self.unclaimed_provider_outputs,
            &output_items,
            SemanticUsage::default(),
            self.semantic_budget,
        )?;
        CanonicalTranscript::validate_sequence(
            self.submitted_items
                .iter()
                .chain(&self.unclaimed_provider_outputs)
                .chain(&output_items),
        )?;
        self.unclaimed_provider_outputs
            .extend(output_items.into_iter().filter(is_public_output));
        let terminal_compacts = wire_items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"));
        self.wire_input
            .extend(wire_items.into_iter().filter(|item| {
                !text_was_sealed
                    || terminal_compacts
                    || item.get("role").and_then(Value::as_str) != Some("assistant")
            }));
        self.normalize_compaction_window();
        Ok(self)
    }

    /// Install one already-validated private output fact at item.done. This
    /// does not wait for response.completed and is candidate-first so a local
    /// encoding failure cannot partially mutate causal history.
    pub(super) fn seal_private_output(
        &mut self,
        sealed: SealedOpenAiPrivateOutput,
    ) -> Result<(), OpenAiContinuationError> {
        if self.sealed_private_outputs.contains(&sealed.output_index)
            || self.sealed_output_order.contains_key(&sealed.output_index)
        {
            return Err(OpenAiContinuationError::InvalidEncodedRequest);
        }
        let mut candidate = self.clone();
        candidate.insert_sealed_output(
            sealed.output_index,
            sealed.output_item,
            sealed.wire_item,
        )?;
        candidate.sealed_private_outputs.insert(sealed.output_index);
        candidate.normalize_compaction_window();
        *self = candidate;
        Ok(())
    }

    fn normalize_compaction_window(&mut self) {
        if let Some(compaction_index) = self
            .wire_input
            .iter()
            .rposition(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        {
            let pending_call_ids = self.pending_tool_call_ids();
            let retained_calls = self.wire_input[..compaction_index]
                .iter()
                .filter(|item| {
                    item.get("type").and_then(Value::as_str) == Some("function_call")
                        && item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .is_some_and(|call_id| pending_call_ids.contains(call_id))
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut compacted = self.wire_input.split_off(compaction_index);
            compacted.splice(1..1, retained_calls);
            self.wire_input = compacted;
        }
    }

    /// Append a visible partial before its corresponding ProviderEvent is published.
    pub(super) fn record_text_partial(
        &mut self,
        output_index: u64,
        delta: impl Into<String>,
        phase: Option<AssistantPhase>,
    ) -> Result<(), OpenAiContinuationError> {
        let delta = delta.into();
        let mut open_text_outputs = self.open_text_outputs.clone();
        let output = open_text_outputs
            .entry(output_index)
            .or_insert_with(|| OpenTextOutput {
                text: String::new(),
                phase,
            });
        if output.phase != phase {
            return Err(OpenAiContinuationError::InvalidEncodedRequest);
        }
        output.text.push_str(&delta);

        let mut partial_records = self.partial_records.clone();
        partial_records.push(PartialTextRecord {
            output_index,
            delta,
        });
        self.open_text_outputs = open_text_outputs;
        self.partial_records = partial_records;
        Ok(())
    }

    /// Admit a publishable partial only when the exact legal replay obtained
    /// by aborting it can still be encoded within the outbound request bound.
    pub(super) fn record_text_partial_bounded(
        &mut self,
        output_index: u64,
        delta: impl Into<String>,
        phase: Option<AssistantPhase>,
        artifact_binding: &OpenAiInlineArtifactBinding,
        encoder: &CodexHttpV1Encoder,
        max_serialized_request_body_bytes: usize,
    ) -> Result<(), OpenAiContinuationError> {
        let delta = delta.into();
        let mut candidate = self.clone();
        candidate.record_text_partial(output_index, delta, phase)?;
        let mut aborted_replay = candidate.clone();
        aborted_replay.abort_open_outputs();
        aborted_replay.encode_partial_replay_snapshot_bounded(
            artifact_binding,
            encoder,
            max_serialized_request_body_bytes,
        )?;
        *self = candidate;
        Ok(())
    }

    /// Materialize every open text output when its stream cannot complete.
    pub(super) fn abort_open_outputs(&mut self) {
        if self.open_text_outputs.is_empty() {
            return;
        }

        // These records were already admitted one delta at a time before
        // publication. Materialization is consequently infallible: losing
        // visible text during Drop would violate causal history.
        for (output_index, output) in std::mem::take(&mut self.open_text_outputs) {
            let next = self
                .sealed_output_order
                .iter()
                .find(|(ordinal, _)| **ordinal > output_index)
                .map(|(_, placement)| placement);
            let semantic_output =
                CanonicalInputItem::interrupted_assistant_text(output.text.clone(), output.phase);
            let semantic_insertion = next
                .and_then(|placement| placement.semantic_output.as_ref())
                .and_then(|later| {
                    self.unclaimed_provider_outputs
                        .iter()
                        .rposition(|item| item == later)
                })
                .unwrap_or(self.unclaimed_provider_outputs.len());
            self.unclaimed_provider_outputs
                .insert(semantic_insertion, semantic_output);

            let wire = assistant_text_wire(&output.text, output.phase);
            let wire_insertion = next
                .and_then(|placement| {
                    self.wire_input
                        .iter()
                        .rposition(|item| item == &placement.wire_item)
                })
                .unwrap_or(self.wire_input.len());
            self.wire_input.insert(wire_insertion, wire);
        }
        self.wire_input.push(interruption_marker_wire());
    }

    pub(super) fn seal_text_output(
        &mut self,
        output_index: u64,
        text: impl Into<String>,
        phase: Option<AssistantPhase>,
    ) -> Result<(), OpenAiContinuationError> {
        if self.sealed_text_outputs.contains(&output_index)
            || self.sealed_output_order.contains_key(&output_index)
        {
            return Err(OpenAiContinuationError::InvalidEncodedRequest);
        }
        let text = text.into();
        if let Some(open) = self.open_text_outputs.get(&output_index) {
            let accumulated = self
                .partial_records
                .iter()
                .filter(|record| record.output_index == output_index)
                .map(|record| record.delta.as_str())
                .collect::<String>();
            if open.text != text || accumulated != text || open.phase != phase {
                return Err(OpenAiContinuationError::InvalidEncodedRequest);
            }
        }
        let mut candidate = self.clone();
        candidate.open_text_outputs.remove(&output_index);
        candidate.insert_sealed_output(
            output_index,
            Some(CanonicalInputItem::assistant_text(text.clone(), phase)),
            assistant_text_wire(&text, phase),
        )?;
        candidate.sealed_text_outputs.insert(output_index);
        *self = candidate;
        Ok(())
    }

    fn insert_sealed_output(
        &mut self,
        output_index: u64,
        semantic_output: Option<CanonicalInputItem>,
        wire_item: Value,
    ) -> Result<(), OpenAiContinuationError> {
        if self.sealed_output_order.contains_key(&output_index) {
            return Err(OpenAiContinuationError::InvalidEncodedRequest);
        }

        let next = self
            .sealed_output_order
            .iter()
            .find(|(ordinal, _)| **ordinal > output_index)
            .map(|(_, placement)| placement);
        let mut unclaimed_provider_outputs = self.unclaimed_provider_outputs.clone();
        if let Some(output) = semantic_output.as_ref() {
            let insertion = next
                .and_then(|placement| placement.semantic_output.as_ref())
                .and_then(|later| {
                    unclaimed_provider_outputs
                        .iter()
                        .rposition(|item| item == later)
                })
                .unwrap_or(unclaimed_provider_outputs.len());
            unclaimed_provider_outputs.insert(insertion, output.clone());
        }
        validate_semantic_state(
            &self.submitted_projection,
            &self.submitted_items,
            &unclaimed_provider_outputs,
            SemanticUsage::default(),
            self.semantic_budget,
        )?;
        CanonicalTranscript::validate_sequence(
            self.submitted_items
                .iter()
                .chain(&unclaimed_provider_outputs),
        )?;

        let mut wire_input = self.wire_input.clone();
        let insertion = next
            .and_then(|placement| {
                wire_input
                    .iter()
                    .rposition(|item| item == &placement.wire_item)
            })
            .unwrap_or(wire_input.len());
        wire_input.insert(insertion, wire_item.clone());

        self.unclaimed_provider_outputs = unclaimed_provider_outputs;
        self.wire_input = wire_input;
        self.sealed_output_order.insert(
            output_index,
            SealedOutputPlacement {
                semantic_output,
                wire_item,
            },
        );
        Ok(())
    }

    pub(super) fn complete_open_outputs(&mut self) -> Result<(), OpenAiContinuationError> {
        if self.open_text_outputs.is_empty() {
            Ok(())
        } else {
            Err(OpenAiContinuationError::InvalidEncodedRequest)
        }
    }

    pub(super) fn has_pending_tool_calls(&self) -> bool {
        ensure_all_tool_calls_closed(
            self.submitted_items
                .iter()
                .chain(&self.unclaimed_provider_outputs),
        )
        .is_err()
    }

    fn pending_tool_call_ids(&self) -> HashSet<&str> {
        let mut pending = VecDeque::new();
        for item in &self.unclaimed_provider_outputs {
            match item {
                CanonicalInputItem::ToolCall { call_id, .. } => pending.push_back(call_id.as_str()),
                CanonicalInputItem::ToolResult { call_id, .. }
                    if pending.front().copied() == Some(call_id.as_str()) =>
                {
                    pending.pop_front();
                }
                _ => {}
            }
        }
        pending.into_iter().collect()
    }

    pub(super) fn append_tool_outputs(
        &mut self,
        outputs: impl IntoIterator<Item = (u64, crate::component::execution::ToolOutput)>,
    ) -> Result<(), OpenAiContinuationError> {
        let mut outputs = outputs.into_iter().collect::<Vec<_>>();
        outputs.sort_by_key(|(ordinal, _)| *ordinal);
        let canonical = outputs
            .iter()
            .map(|(_, output)| CanonicalInputItem::tool_result(output.call_id(), output.content()))
            .collect::<Result<Vec<_>, _>>()?;
        let wire = outputs
            .iter()
            .map(|(_, output)| {
                serde_json::json!({
                    "type": "function_call_output",
                    "call_id": output.call_id(),
                    "output": output.content(),
                })
            })
            .collect::<Vec<_>>();
        self.append_provider_outputs(canonical, wire)
    }

    pub(super) fn append_tool_call(
        &mut self,
        call: &crate::component::execution::ToolCall,
    ) -> Result<(), OpenAiContinuationError> {
        self.append_provider_outputs(
            vec![CanonicalInputItem::tool_call(
                call.call_id(),
                call.name(),
                call.raw_arguments(),
            )?],
            vec![serde_json::json!({
                "type": "function_call",
                "call_id": call.call_id(),
                "name": call.name(),
                "arguments": call.raw_arguments(),
            })],
        )
    }

    fn append_provider_outputs(
        &mut self,
        outputs: Vec<CanonicalInputItem>,
        wire_items: Vec<Value>,
    ) -> Result<(), OpenAiContinuationError> {
        let mut unclaimed_provider_outputs = self.unclaimed_provider_outputs.clone();
        unclaimed_provider_outputs.extend(outputs);
        validate_semantic_state(
            &self.submitted_projection,
            &self.submitted_items,
            &unclaimed_provider_outputs,
            SemanticUsage::default(),
            self.semantic_budget,
        )?;
        CanonicalTranscript::validate_sequence(
            self.submitted_items
                .iter()
                .chain(&unclaimed_provider_outputs),
        )?;

        let mut merged_wire_input = self.wire_input.clone();
        merged_wire_input.extend(wire_items);
        self.unclaimed_provider_outputs = unclaimed_provider_outputs;
        self.wire_input = merged_wire_input;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn encode_retained_snapshot(
        &self,
        artifact_binding: &OpenAiInlineArtifactBinding,
        encoder: &CodexHttpV1Encoder,
    ) -> Result<Vec<u8>, OpenAiContinuationError> {
        self.encode_retained_snapshot_bounded(artifact_binding, encoder, usize::MAX)
    }

    pub(super) fn encode_retained_snapshot_bounded(
        &self,
        artifact_binding: &OpenAiInlineArtifactBinding,
        encoder: &CodexHttpV1Encoder,
        max_serialized_request_body_bytes: usize,
    ) -> Result<Vec<u8>, OpenAiContinuationError> {
        // Snapshot the exact validated retained input without a future projection.
        ensure_all_tool_calls_closed(
            self.submitted_items
                .iter()
                .chain(&self.unclaimed_provider_outputs),
        )?;
        let transcript = crate::transcript::CanonicalTranscript::new();
        let typed_request = encoder.request_with_native_tools(&transcript, &self.native_tools)?;
        Ok(typed_request.encode_with_input_and_instructions_bounded(
            &self.wire_input,
            artifact_binding.instructions(),
            max_serialized_request_body_bytes,
        )?)
    }

    /// Size-check a candidate partial before publication. An open response may
    /// contain a completed ToolCall whose result is deliberately still staged;
    /// that closure is enforced only by the next Input Gate, never while later
    /// provider output is arriving in this response.
    fn encode_partial_replay_snapshot_bounded(
        &self,
        artifact_binding: &OpenAiInlineArtifactBinding,
        encoder: &CodexHttpV1Encoder,
        max_serialized_request_body_bytes: usize,
    ) -> Result<Vec<u8>, OpenAiContinuationError> {
        let transcript = crate::transcript::CanonicalTranscript::new();
        let typed_request = encoder.request_with_native_tools(&transcript, &self.native_tools)?;
        Ok(typed_request.encode_with_input_and_instructions_bounded(
            &self.wire_input,
            artifact_binding.instructions(),
            max_serialized_request_body_bytes,
        )?)
    }

    #[cfg(test)]
    fn reconciliation_index_probes(&self) -> usize {
        self.reconciliation_index_probes
    }
}

fn assistant_text_wire(text: &str, phase: Option<AssistantPhase>) -> Value {
    let mut item = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
    });
    if let Some(phase) = phase {
        item["phase"] = serde_json::to_value(phase).expect("AssistantPhase serializes");
    }
    item
}

fn interruption_marker_wire() -> Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "[agentview: assistant output interrupted before completion]",
        }],
    })
}

fn ensure_all_tool_calls_closed<'a>(
    items: impl IntoIterator<Item = &'a CanonicalInputItem>,
) -> Result<(), OpenAiContinuationError> {
    let mut pending = VecDeque::new();
    let mut seen_calls = HashSet::new();
    let mut seen_outputs = HashSet::new();
    for item in items {
        match item {
            CanonicalInputItem::ToolCall { call_id, .. } => {
                if !seen_calls.insert(call_id.as_str()) {
                    return Err(OpenAiContinuationError::InvalidEncodedRequest);
                }
                pending.push_back(call_id.as_str());
            }
            CanonicalInputItem::ToolResult { call_id, .. }
                if !seen_outputs.insert(call_id.as_str())
                    || pending.pop_front() != Some(call_id.as_str()) =>
            {
                return Err(OpenAiContinuationError::InvalidEncodedRequest);
            }
            _ => {}
        }
    }
    if let Some(call_id) = pending.pop_front() {
        return Err(OpenAiContinuationError::PendingToolCall {
            call_id: call_id.to_owned(),
        });
    }
    Ok(())
}

struct ReconciledProjection {
    submitted_projection: RenderedProjection,
    submitted_items: Vec<CanonicalInputItem>,
    unclaimed_provider_outputs: Vec<CanonicalInputItem>,
    new_input_indices: Vec<usize>,
    #[cfg(test)]
    reconciliation_index_probes: usize,
}

fn reconcile_projection(
    previous: &OpenAiContinuation,
    current: &RenderedProjection,
    append_policy: &ProjectionAppendPolicy,
    force_full_projection: bool,
) -> Result<ReconciledProjection, OpenAiContinuationError> {
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
    let unclaimed_provider_outputs = previous.unclaimed_provider_outputs.clone();
    let mut unclaimed_output_index = ItemOccurrenceIndex::new(&unclaimed_provider_outputs)?;
    let mut claimed_unclaimed_outputs = vec![false; unclaimed_provider_outputs.len()];
    let mut new_input_indices = Vec::new();
    let mut provider_input_index = 0;

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

    let mut submitted_indexes = submitted
        .iter()
        .map(|node| ItemOccurrenceIndex::new(&node.items))
        .collect::<Result<Vec<_>, _>>()?;
    let mut submitted_items = previous.submitted_items.clone();
    for current_node in current.nodes() {
        for (item_index, item) in current_node.items().iter().enumerate() {
            if is_system_instruction(item) {
                continue;
            }
            let current_input_index = provider_input_index;
            provider_input_index += 1;
            let node_index = node_indexes
                .get(current_node.identity())
                .copied()
                .ok_or(OpenAiContinuationError::InvalidEncodedRequest)?;
            if !force_full_projection
                && !append_policy.requires_append(current_node.identity(), item_index)
                && submitted_indexes[node_index].claim(item)?.is_some()
            {
                continue;
            }
            let node = &mut submitted[node_index];
            if let Some(index) = unclaimed_output_index.claim(item)? {
                claimed_unclaimed_outputs[index] = true;
                node.items.push(item.clone());
                submitted_items.push(item.clone());
                continue;
            }
            node.items.push(item.clone());
            submitted_items.push(item.clone());
            new_input_indices.push(current_input_index);
        }
    }

    if provider_input_index != provider_input_items(current).count() {
        return Err(OpenAiContinuationError::InvalidEncodedRequest);
    }
    let unclaimed_provider_outputs = unclaimed_provider_outputs
        .into_iter()
        .zip(claimed_unclaimed_outputs)
        .filter_map(|(output, claimed)| (!claimed).then_some(output))
        .collect();
    let submitted_projection = RenderedProjection::from_nodes(
        submitted
            .into_iter()
            .map(|node| RenderedProjectionNode::new(node.identity, node.items))
            .collect(),
    )?;
    let submitted_projection = match current.execution_scope() {
        Some(scope) => submitted_projection.with_execution_scope(scope),
        None => submitted_projection,
    };
    Ok(ReconciledProjection {
        submitted_projection,
        submitted_items,
        unclaimed_provider_outputs,
        new_input_indices,
        #[cfg(test)]
        reconciliation_index_probes: submitted_indexes
            .iter()
            .map(ItemOccurrenceIndex::probes)
            .sum::<usize>()
            + unclaimed_output_index.probes(),
    })
}

struct MutableSubmittedNode {
    identity: String,
    items: Vec<CanonicalInputItem>,
}

struct ItemOccurrenceIndex {
    occurrences: HashMap<Vec<u8>, VecDeque<usize>>,
    #[cfg(test)]
    probes: usize,
}

impl ItemOccurrenceIndex {
    fn new(items: &[CanonicalInputItem]) -> Result<Self, OpenAiContinuationError> {
        let mut occurrences = HashMap::<_, VecDeque<_>>::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            occurrences
                .entry(submission_item_key(item)?)
                .or_default()
                .push_back(index);
        }
        Ok(Self {
            occurrences,
            #[cfg(test)]
            probes: 0,
        })
    }

    fn claim(
        &mut self,
        current: &CanonicalInputItem,
    ) -> Result<Option<usize>, OpenAiContinuationError> {
        let key = submission_item_key(current)?;
        #[cfg(test)]
        {
            self.probes += 1;
        }
        Ok(self.occurrences.get_mut(&key).and_then(VecDeque::pop_front))
    }

    #[cfg(test)]
    fn probes(&self) -> usize {
        self.probes
    }
}

fn submission_item_key(item: &CanonicalInputItem) -> Result<Vec<u8>, OpenAiContinuationError> {
    let result = match item {
        CanonicalInputItem::AssistantText {
            text,
            phase,
            status,
        } => serde_json::to_vec(&(0_u8, text, normalized_final_phase(*phase), status)),
        _ => serde_json::to_vec(&(1_u8, item)),
    };
    result.map_err(|_| OpenAiContinuationError::InvalidEncodedRequest)
}

fn normalized_final_phase(phase: Option<AssistantPhase>) -> Option<AssistantPhase> {
    match phase {
        None | Some(AssistantPhase::FinalAnswer) => Some(AssistantPhase::FinalAnswer),
        commentary => commentary,
    }
}

fn provider_input_items(
    projection: &RenderedProjection,
) -> impl Iterator<Item = &CanonicalInputItem> {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter(|item| !is_system_instruction(item))
}

fn is_public_output(item: &CanonicalInputItem) -> bool {
    !matches!(
        item,
        CanonicalInputItem::AssistantText {
            phase: Some(crate::transcript::AssistantPhase::Commentary),
            ..
        } | CanonicalInputItem::ProviderExtension(_)
    )
}

fn is_system_instruction(item: &CanonicalInputItem) -> bool {
    matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    )
}

const SEMANTIC_BUDGET_MULTIPLIER: usize = 8;
const SEMANTIC_ITEM_ACCOUNTING_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SemanticSidecarKey {
    node_identity: String,
    structural_path: Vec<usize>,
    slot: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct SemanticUsage {
    items: usize,
    bytes: usize,
}

impl SemanticUsage {
    fn checked_add(self, additional: Self) -> Result<Self, OpenAiContinuationError> {
        Ok(Self {
            items: self
                .items
                .checked_add(additional.items)
                .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?,
            bytes: self
                .bytes
                .checked_add(additional.bytes)
                .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?,
        })
    }

    fn checked_sub(self, removed: Self) -> Result<Self, OpenAiContinuationError> {
        Ok(Self {
            items: self
                .items
                .checked_sub(removed.items)
                .ok_or(OpenAiContinuationError::InvalidEncodedRequest)?,
            bytes: self
                .bytes
                .checked_sub(removed.bytes)
                .ok_or(OpenAiContinuationError::InvalidEncodedRequest)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ContinuationSemanticBudget {
    max_items: usize,
    max_bytes: usize,
}

impl ContinuationSemanticBudget {
    fn from_request_limit(request_limit: usize) -> Self {
        let max_bytes = request_limit.saturating_mul(SEMANTIC_BUDGET_MULTIPLIER);
        Self {
            max_items: (max_bytes / SEMANTIC_ITEM_ACCOUNTING_BYTES).max(1),
            max_bytes,
        }
    }

    #[cfg(test)]
    const fn for_test(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items,
            max_bytes,
        }
    }

    #[cfg(test)]
    fn measured_bytes(items: &[CanonicalInputItem]) -> Result<usize, OpenAiContinuationError> {
        let mut meter = SemanticMeter::new(Self::for_test(usize::MAX, usize::MAX));
        meter.observe_items(items.iter())?;
        Ok(meter.bytes)
    }
}

#[cfg(test)]
fn validate_semantic_items(
    parts: &[&[CanonicalInputItem]],
    budget: ContinuationSemanticBudget,
) -> Result<(), OpenAiContinuationError> {
    let mut meter = SemanticMeter::new(budget);
    for items in parts {
        meter.observe_items(items.iter())?;
    }
    CanonicalTranscript::validate_sequence(parts.iter().flat_map(|items| items.iter()))?;
    Ok(())
}

fn validate_semantic_projection(
    projection: &RenderedProjection,
    initial_usage: SemanticUsage,
    budget: ContinuationSemanticBudget,
) -> Result<(), OpenAiContinuationError> {
    let mut meter = SemanticMeter::with_usage(budget, initial_usage)?;
    meter.observe_projection(projection)?;
    CanonicalTranscript::validate_sequence(
        projection.nodes().iter().flat_map(|node| node.items()),
    )?;
    Ok(())
}

fn validate_semantic_state(
    projection: &RenderedProjection,
    submitted_items: &[CanonicalInputItem],
    unclaimed_outputs: &[CanonicalInputItem],
    initial_usage: SemanticUsage,
    budget: ContinuationSemanticBudget,
) -> Result<(), OpenAiContinuationError> {
    let mut meter = SemanticMeter::with_usage(budget, initial_usage)?;
    meter.observe_projection(projection)?;
    meter.observe_items(submitted_items.iter())?;
    meter.observe_items(unclaimed_outputs.iter())
}

fn validate_semantic_state_with_outputs(
    projection: &RenderedProjection,
    submitted_items: &[CanonicalInputItem],
    unclaimed_outputs: &[CanonicalInputItem],
    output_items: &[CanonicalInputItem],
    initial_usage: SemanticUsage,
    budget: ContinuationSemanticBudget,
) -> Result<(), OpenAiContinuationError> {
    let mut meter = SemanticMeter::with_usage(budget, initial_usage)?;
    meter.observe_projection(projection)?;
    meter.observe_items(submitted_items.iter())?;
    meter.observe_items(unclaimed_outputs.iter())?;
    meter.observe_items(output_items.iter().filter(|item| is_public_output(item)))
}

struct SemanticMeter {
    budget: ContinuationSemanticBudget,
    items: usize,
    bytes: usize,
}

impl SemanticMeter {
    const fn new(budget: ContinuationSemanticBudget) -> Self {
        Self {
            budget,
            items: 0,
            bytes: 0,
        }
    }

    fn with_usage(
        budget: ContinuationSemanticBudget,
        usage: SemanticUsage,
    ) -> Result<Self, OpenAiContinuationError> {
        if usage.items > budget.max_items || usage.bytes > budget.max_bytes {
            return Err(OpenAiContinuationError::SemanticHistoryLimit);
        }
        Ok(Self {
            budget,
            items: usage.items,
            bytes: usage.bytes,
        })
    }

    const fn usage(&self) -> SemanticUsage {
        SemanticUsage {
            items: self.items,
            bytes: self.bytes,
        }
    }

    fn observe_projection(
        &mut self,
        projection: &RenderedProjection,
    ) -> Result<(), OpenAiContinuationError> {
        for node in projection.nodes() {
            self.observe_entry_bytes(node.identity().len())?;
            self.observe_items(node.items().iter())?;
        }
        Ok(())
    }

    fn observe_items<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a CanonicalInputItem>,
    ) -> Result<(), OpenAiContinuationError> {
        for item in items {
            self.observe_serialized_entry(item)?;
        }
        Ok(())
    }

    fn observe_serialized_entry(
        &mut self,
        value: &impl Serialize,
    ) -> Result<(), OpenAiContinuationError> {
        self.observe_entry_bytes(0)?;
        let mut writer = SemanticByteWriter {
            bytes: &mut self.bytes,
            limit: self.budget.max_bytes,
            limit_exceeded: false,
        };
        if serde_json::to_writer(&mut writer, value).is_err() {
            return if writer.limit_exceeded {
                Err(OpenAiContinuationError::SemanticHistoryLimit)
            } else {
                Err(OpenAiContinuationError::InvalidEncodedRequest)
            };
        }
        Ok(())
    }

    fn observe_entry_bytes(&mut self, payload_bytes: usize) -> Result<(), OpenAiContinuationError> {
        self.items = self
            .items
            .checked_add(1)
            .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?;
        if self.items > self.budget.max_items {
            return Err(OpenAiContinuationError::SemanticHistoryLimit);
        }
        let additional = SEMANTIC_ITEM_ACCOUNTING_BYTES
            .checked_add(payload_bytes)
            .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?;
        self.bytes = self
            .bytes
            .checked_add(additional)
            .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?;
        if self.bytes > self.budget.max_bytes {
            return Err(OpenAiContinuationError::SemanticHistoryLimit);
        }
        Ok(())
    }
}

fn reconcile_semantic_diff_sidecars(
    previous: Option<&HashMap<SemanticSidecarKey, SemanticUsage>>,
    current: &RenderedProjection,
    budget: ContinuationSemanticBudget,
) -> Result<HashMap<SemanticSidecarKey, SemanticUsage>, OpenAiContinuationError> {
    let mut candidate = previous.cloned().unwrap_or_default();
    let mut total = semantic_sidecar_usage(&candidate)?;
    SemanticMeter::with_usage(budget, total)?;

    for node in current.nodes() {
        for template in node.diff_templates() {
            for fragment in template.fragments() {
                let RenderedProjectionFragment::Diff {
                    diff_index,
                    authored,
                    complete,
                } = fragment
                else {
                    continue;
                };
                let (key, usage) =
                    measure_semantic_diff_sidecar(node, *diff_index, authored, complete, budget)?;
                let without_previous = match candidate.get(&key) {
                    Some(previous_usage) => total.checked_sub(*previous_usage)?,
                    None => total,
                };
                let next = without_previous.checked_add(usage)?;
                SemanticMeter::with_usage(budget, next)?;
                candidate.insert(key, usage);
                total = next;
            }
        }
    }

    Ok(candidate)
}

fn measure_semantic_diff_sidecar(
    node: &RenderedProjectionNode,
    diff_index: usize,
    authored: &impl Serialize,
    complete: &impl Serialize,
    budget: ContinuationSemanticBudget,
) -> Result<(SemanticSidecarKey, SemanticUsage), OpenAiContinuationError> {
    let diff = node
        .diffs()
        .get(diff_index)
        .ok_or(OpenAiContinuationError::InvalidEncodedRequest)?;
    let path_bytes = diff
        .structural_path()
        .len()
        .checked_mul(std::mem::size_of::<usize>())
        .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?;
    let key_bytes = node
        .identity()
        .len()
        .checked_add(diff.slot().len())
        .and_then(|bytes| bytes.checked_add(path_bytes))
        .ok_or(OpenAiContinuationError::SemanticHistoryLimit)?;
    let mut meter = SemanticMeter::new(budget);
    meter.observe_entry_bytes(key_bytes)?;
    meter.observe_serialized_entry(authored)?;
    meter.observe_serialized_entry(complete)?;
    Ok((
        SemanticSidecarKey {
            node_identity: node.identity().to_owned(),
            structural_path: diff.structural_path().to_vec(),
            slot: diff.slot().to_owned(),
        },
        meter.usage(),
    ))
}

fn semantic_sidecar_usage(
    sidecars: &HashMap<SemanticSidecarKey, SemanticUsage>,
) -> Result<SemanticUsage, OpenAiContinuationError> {
    sidecars
        .values()
        .try_fold(SemanticUsage::default(), |total, usage| {
            total.checked_add(*usage)
        })
}

struct SemanticByteWriter<'a> {
    bytes: &'a mut usize,
    limit: usize,
    limit_exceeded: bool,
}

impl Write for SemanticByteWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("semantic history size overflow"));
        };
        if next > self.limit {
            self.limit_exceeded = true;
            return Err(io::Error::other("semantic history limit exceeded"));
        }
        *self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OpenAiContinuationError {
    #[error(transparent)]
    Encode(#[from] CodexHttpV1Error),
    #[error(transparent)]
    Canonical(#[from] CanonicalTranscriptError),
    #[error(transparent)]
    Projection(#[from] crate::component::execution::RenderedProjectionError),
    #[error("Codex request encoder produced an invalid request shape")]
    InvalidEncodedRequest,
    #[error("OpenAI continuation semantic history exceeds the configured internal limit")]
    SemanticHistoryLimit,
    #[error("OpenAI continuation has an unresolved tool call")]
    PendingToolCall { call_id: String },
}

impl OpenAiContinuationError {
    pub(super) fn is_serialized_request_body_limit(&self) -> bool {
        matches!(
            self,
            Self::Encode(CodexHttpV1Error::SerializedRequestBodyLimit)
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        component::execution::{
            ProjectionExecutionScope, RenderedProjection, RenderedProjectionDiffMarker,
            RenderedProjectionFragment, RenderedProjectionItemTemplate, RenderedProjectionNode,
        },
        pom::{BlockChildren, Document, ResolvedDocument, TextNode, XmlNode},
        pom_resolution::resolve_system_document,
        provider::codex_http_v1::{
            CodexFunctionTool, CodexHttpV1Encoder, CodexHttpV1Options, CodexReasoning,
        },
        transcript::{
            reset_construction_work, take_construction_work, AssistantPhase, AssistantTextStatus,
            CanonicalInputItem, ConversationRole,
        },
    };

    use super::{
        ensure_all_tool_calls_closed, validate_semantic_items, ContinuationSemanticBudget,
        OpenAiContinuation, OpenAiContinuationError, OpenAiInlineArtifactBinding,
        SealedOpenAiPrivateOutput,
    };

    fn projection(nodes: &[(&str, &[&str])]) -> RenderedProjection {
        let nodes = nodes
            .iter()
            .map(|(identity, items)| {
                RenderedProjectionNode::new(
                    *identity,
                    items
                        .iter()
                        .map(|item| CanonicalInputItem::assistant_text(*item, None))
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        RenderedProjection::from_nodes(nodes).unwrap()
    }

    fn input_texts(request_body: &[u8]) -> Vec<String> {
        let request: serde_json::Value = serde_json::from_slice(request_body).unwrap();
        request["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["content"][0]["text"].as_str().unwrap().to_owned())
            .collect()
    }

    fn encoder() -> CodexHttpV1Encoder {
        CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap(),
        )
    }

    fn projection_with_diff_sidecar(identity: &str, sidecar_bytes: usize) -> RenderedProjection {
        let authored = Document::from_xml(
            XmlNode::try_build("state", |children| {
                children.text(TextNode::new("x".repeat(sidecar_bytes)));
                Ok(())
            })
            .unwrap(),
        );
        let complete = resolve_system_document(authored.clone());
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::with_diff_templates(
            identity,
            vec![CanonicalInputItem::message(
                ConversationRole::User,
                ResolvedDocument::new(BlockChildren::new()),
            )],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![RenderedProjectionItemTemplate::new(
                0,
                vec![RenderedProjectionFragment::Diff {
                    diff_index: 0,
                    authored,
                    complete,
                }],
            )],
        )])
        .unwrap()
    }

    #[test]
    fn many_small_outputs_have_linear_transcript_construction_work() {
        const OUTPUT_COUNT: usize = 256;
        let encoder = encoder();
        let output_items = (0..OUTPUT_COUNT)
            .map(|index| CanonicalInputItem::assistant_text(format!("output-{index}"), None))
            .collect::<Vec<_>>();
        let wire_items = (0..OUTPUT_COUNT)
            .map(|index| json!({"type": "test-output", "index": index}))
            .collect::<Vec<_>>();

        reset_construction_work();
        let _candidate =
            OpenAiContinuation::prepare(None, None, projection(&[("only", &["input"])]), &encoder)
                .unwrap()
                .candidate
                .with_outputs(output_items, wire_items)
                .unwrap();
        let construction_work = take_construction_work();

        assert_eq!(construction_work, OUTPUT_COUNT + 4);
    }

    #[test]
    fn semantic_byte_budget_is_inclusive_at_the_exact_boundary() {
        let items = vec![CanonicalInputItem::assistant_text("boundary", None)];
        let exact_bytes = ContinuationSemanticBudget::measured_bytes(&items).unwrap();
        let exact = ContinuationSemanticBudget::for_test(1, exact_bytes);

        validate_semantic_items(&[items.as_slice()], exact).unwrap();
        let error = validate_semantic_items(
            &[items.as_slice()],
            ContinuationSemanticBudget::for_test(1, exact_bytes - 1),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            OpenAiContinuationError::SemanticHistoryLimit
        ));
    }

    #[test]
    fn oversized_current_history_is_rejected_before_full_transcript_copy() {
        let encoder = encoder();
        let current = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "only",
            vec![CanonicalInputItem::assistant_text("x".repeat(4_096), None)],
        )])
        .unwrap();

        reset_construction_work();
        let error =
            match OpenAiContinuation::prepare_bounded(None, None, &[], current, &encoder, 64) {
                Ok(_) => panic!("oversized semantic history must fail closed"),
                Err(error) => error,
            };
        let construction_work = take_construction_work();

        assert!(matches!(
            error,
            OpenAiContinuationError::SemanticHistoryLimit
        ));
        assert_eq!(construction_work, 0);
    }

    #[test]
    fn oversized_diff_sidecar_is_rejected_before_candidate_clones() {
        let encoder = encoder();
        let current = projection_with_diff_sidecar("only", 16_384);

        reset_construction_work();
        let error =
            match OpenAiContinuation::prepare_bounded(None, None, &[], current, &encoder, 1_024) {
                Ok(_) => panic!("oversized retained diff sidecar must fail closed"),
                Err(error) => error,
            };
        let construction_work = take_construction_work();

        assert!(matches!(
            error,
            OpenAiContinuationError::SemanticHistoryLimit
        ));
        assert_eq!(construction_work, 0);
    }

    #[test]
    fn retained_diff_sidecars_share_one_cumulative_semantic_budget() {
        let encoder = encoder();
        let first = OpenAiContinuation::prepare_bounded(
            None,
            None,
            &[],
            projection_with_diff_sidecar("first", 2_500),
            &encoder,
            1_024,
        )
        .unwrap();

        reset_construction_work();
        let error = match OpenAiContinuation::prepare_bounded(
            Some(&first.candidate),
            Some(&first.artifact_binding),
            &[],
            projection_with_diff_sidecar("second", 2_500),
            &encoder,
            1_024,
        ) {
            Ok(_) => panic!("cumulative retained diff sidecars must fail closed"),
            Err(error) => error,
        };
        let construction_work = take_construction_work();

        assert!(matches!(
            error,
            OpenAiContinuationError::SemanticHistoryLimit
        ));
        assert_eq!(construction_work, 0);
    }

    #[test]
    fn updating_one_diff_sidecar_replaces_its_previous_budget_charge() {
        let encoder = encoder();
        let first = OpenAiContinuation::prepare_bounded(
            None,
            None,
            &[],
            projection_with_diff_sidecar("stable", 2_500),
            &encoder,
            1_024,
        )
        .unwrap();

        OpenAiContinuation::prepare_bounded(
            Some(&first.candidate),
            Some(&first.artifact_binding),
            &[],
            projection_with_diff_sidecar("stable", 2_500),
            &encoder,
            1_024,
        )
        .expect("one retained diff provenance has one semantic budget charge");
    }

    #[test]
    fn accumulated_small_outputs_fail_closed_at_the_semantic_item_budget() {
        let encoder = encoder();
        let prepared = OpenAiContinuation::prepare_bounded(
            None,
            None,
            &[],
            projection(&[("only", &["input"])]),
            &encoder,
            1_024,
        )
        .unwrap();
        let output_items = (0..256)
            .map(|index| CanonicalInputItem::assistant_text(format!("output-{index}"), None))
            .collect::<Vec<_>>();
        let wire_items = (0..256)
            .map(|index| json!({"type": "test-output", "index": index}))
            .collect::<Vec<_>>();

        let error = match prepared.candidate.with_outputs(output_items, wire_items) {
            Ok(_) => panic!("semantic item budget overflow must fail closed"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            OpenAiContinuationError::SemanticHistoryLimit
        ));
    }

    #[test]
    fn direct_duplicate_unclaimed_tool_output_is_rejected() {
        let encoder = encoder();
        let projection = projection(&[("only", &["input"])]);
        let mut candidate = OpenAiContinuation::prepare(None, None, projection, &encoder)
            .unwrap()
            .candidate;
        let call = crate::component::execution::ToolCall::new("call-1", "lookup", "{}").unwrap();
        candidate.append_tool_call(&call).unwrap();
        let candidate = candidate
            .with_outputs(
                vec![CanonicalInputItem::tool_result("call-1", "first").unwrap()],
                vec![json!({"type": "test-output", "index": 1})],
            )
            .unwrap();

        let error = match candidate.with_outputs(
            vec![CanonicalInputItem::tool_result("call-1", "second").unwrap()],
            vec![json!({"type": "test-output", "index": 2})],
        ) {
            Ok(_) => panic!("duplicate unclaimed output must fail closed"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            OpenAiContinuationError::Canonical(
                crate::transcript::CanonicalTranscriptError::DuplicateToolResult { ref call_id }
            ) if call_id == "call-1"
        ));
    }

    #[test]
    fn pending_provider_tool_call_blocks_the_next_wire_snapshot() {
        let encoder = encoder();
        let initial =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["input"])]), &encoder)
                .unwrap()
                .candidate
                .with_outputs(
                    vec![CanonicalInputItem::tool_call("call-1", "lookup", "{}").unwrap()],
                    vec![json!({
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "lookup",
                        "arguments": "{}",
                    })],
                )
                .unwrap();

        let next = OpenAiContinuation::prepare(
            Some(&initial),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            projection(&[("root", &["input"])]),
            &encoder,
        );

        assert!(
            next.is_err(),
            "a pending provider ToolCall must prevent the next request from being compiled"
        );
    }

    #[test]
    fn fresh_pending_tool_call_blocks_the_first_wire_snapshot() {
        let encoder = encoder();
        let projection = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "root",
            vec![CanonicalInputItem::tool_call("call-1", "lookup", "{}").unwrap()],
        )])
        .unwrap();

        let error = match OpenAiContinuation::prepare(None, None, projection, &encoder) {
            Ok(_) => panic!("a fresh bare ToolCall must not reach transport"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            OpenAiContinuationError::PendingToolCall { ref call_id } if call_id == "call-1"
        ));
    }

    #[test]
    fn tool_outputs_replay_in_provider_call_ordinal_not_completion_order() {
        let encoder = encoder();
        let mut continuation =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["input"])]), &encoder)
                .unwrap()
                .candidate;
        let first = crate::component::execution::ToolCall::new("call-1", "first", "{}").unwrap();
        let second = crate::component::execution::ToolCall::new("call-2", "second", "{}").unwrap();
        continuation.append_tool_call(&first).unwrap();
        continuation.append_tool_call(&second).unwrap();
        continuation
            .append_tool_outputs(vec![
                (1, second.output("second-result")),
                (0, first.output("first-result")),
            ])
            .unwrap();

        let prepared = OpenAiContinuation::prepare(
            Some(&continuation),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            projection(&[("root", &["input"])]),
            &encoder,
        )
        .unwrap();
        let input = serde_json::from_slice::<serde_json::Value>(&prepared.request_body).unwrap()
            ["input"]
            .as_array()
            .unwrap()
            .clone();
        let outputs = input
            .into_iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect::<Vec<_>>();

        assert_eq!(outputs[0]["call_id"], "call-1");
        assert_eq!(outputs[1]["call_id"], "call-2");
    }

    #[test]
    fn partial_after_a_sealed_tool_call_is_retainable_before_its_next_gate_closure() {
        let encoder = encoder();
        let binding = OpenAiInlineArtifactBinding::new(String::new());
        let mut continuation =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["input"])]), &encoder)
                .unwrap()
                .candidate;
        let call = crate::component::execution::ToolCall::new("call-1", "lookup", "{}").unwrap();
        continuation.append_tool_call(&call).unwrap();

        continuation
            .record_text_partial_bounded(1, "later text", None, &binding, &encoder, usize::MAX)
            .expect(
                "a pending ToolCall blocks only the next outbound Gate, not later provider output",
            );

        assert_eq!(
            continuation
                .open_text_outputs
                .get(&1)
                .map(|output| output.text.as_str()),
            Some("later text")
        );
    }

    #[test]
    fn sealed_private_outputs_replay_by_output_index_not_done_arrival_order() {
        let encoder = encoder();
        let mut continuation =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["input"])]), &encoder)
                .unwrap()
                .candidate;

        // Responses may seal a later text item before an earlier private item.
        // Replay remains in provider output_index order, not SSE arrival order.
        continuation
            .seal_text_output(1, "later text", None)
            .unwrap();
        continuation
            .seal_private_output(SealedOpenAiPrivateOutput {
                output_index: 0,
                output_item: None,
                wire_item: json!({
                    "id": "rs_0",
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": "encrypted-0",
                }),
            })
            .unwrap();

        let request: serde_json::Value = serde_json::from_slice(
            &continuation
                .encode_retained_snapshot(
                    &OpenAiInlineArtifactBinding::new(String::new()),
                    &encoder,
                )
                .unwrap(),
        )
        .unwrap();
        let input = request["input"].as_array().unwrap();
        let reasoning_index = input
            .iter()
            .position(|item| item["type"] == "reasoning")
            .unwrap();
        let text_index = input
            .iter()
            .position(|item| item.to_string().contains("later text"))
            .unwrap();
        assert!(reasoning_index < text_index, "replayed input: {input:?}");
    }

    #[test]
    fn compaction_retains_a_pending_tool_call_for_its_later_ordered_output() {
        let encoder = encoder();
        let binding = OpenAiInlineArtifactBinding::new(String::new());
        let mut continuation =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["input"])]), &encoder)
                .unwrap()
                .candidate;
        let call = crate::component::execution::ToolCall::new("call-1", "lookup", "{}").unwrap();
        continuation.append_tool_call(&call).unwrap();
        let mut continuation = continuation
            .with_outputs(
                vec![CanonicalInputItem::assistant_text("provider text", None)],
                vec![json!({
                    "id": "cmp_1",
                    "type": "compaction",
                    "encrypted_content": "opaque"
                })],
            )
            .unwrap();
        continuation
            .append_tool_outputs(vec![(0, call.output("tool-result"))])
            .unwrap();

        let request: serde_json::Value = serde_json::from_slice(
            &continuation
                .encode_retained_snapshot(&binding, &encoder)
                .expect("a compacted pending call and its output remain wire-legal"),
        )
        .unwrap();
        let input = request["input"].as_array().unwrap();
        let call_index = input
            .iter()
            .position(|item| item["type"] == "function_call" && item["call_id"] == "call-1")
            .expect("compaction must retain the pending function call wire identity");
        assert_eq!(input[call_index + 1]["type"], "function_call_output");
        assert_eq!(input[call_index + 1]["call_id"], "call-1");
    }

    #[test]
    fn reversed_hand_built_tool_outputs_fail_the_exact_closure_proof() {
        let first = CanonicalInputItem::tool_call("call-1", "first", "{}").unwrap();
        let second = CanonicalInputItem::tool_call("call-2", "second", "{}").unwrap();
        let reversed_first = CanonicalInputItem::tool_result("call-2", "second").unwrap();
        let reversed_second = CanonicalInputItem::tool_result("call-1", "first").unwrap();

        assert!(ensure_all_tool_calls_closed(
            [&first, &second, &reversed_first, &reversed_second,]
        )
        .is_err());
    }

    #[test]
    fn aborted_partial_text_is_replayed_with_its_visible_content_and_marker() {
        let encoder = encoder();
        let mut initial =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["input"])]), &encoder)
                .unwrap()
                .candidate;

        initial
            .record_text_partial(7, "visible partial", None)
            .unwrap();
        initial.abort_open_outputs();

        assert!(matches!(
            initial.unclaimed_provider_outputs.as_slice(),
            [CanonicalInputItem::AssistantText {
                text,
                status: AssistantTextStatus::Interrupted,
                ..
            }] if text == "visible partial"
        ));

        let next = OpenAiContinuation::prepare(
            Some(&initial),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            projection(&[("root", &["input"])]),
            &encoder,
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&next.request_body).unwrap();
        let input = request["input"].as_array().unwrap();

        assert!(input.iter().any(|item| {
            item["role"] == "assistant" && item["content"][0]["text"] == "visible partial"
        }));
        assert!(input.iter().any(|item| {
            item["role"] == "user"
                && item["content"][0]["text"]
                    == "[agentview: assistant output interrupted before completion]"
        }));
    }

    #[test]
    fn strict_extension_reuses_the_confirmed_wire_window() {
        let encoder = encoder();
        let first =
            OpenAiContinuation::prepare(None, None, projection(&[("first", &["first"])]), &encoder)
                .unwrap()
                .candidate
                .with_outputs(
                    vec![CanonicalInputItem::assistant_text(
                        "provider",
                        Some(AssistantPhase::FinalAnswer),
                    )],
                    vec![json!({
                        "id": "msg_1",
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "provider", "annotations": []}],
                    })],
                )
                .unwrap();
        let current = projection(&[("first", &["first", "provider", "second"])]);

        let prepared = OpenAiContinuation::prepare(
            Some(&first),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            current.clone(),
            &encoder,
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&prepared.request_body).unwrap();

        assert_eq!(prepared.candidate.submitted_projection, current);
        assert_eq!(request["input"][1]["id"], "msg_1");
        assert_eq!(request["input"][2]["content"][0]["text"], "second");
    }

    #[test]
    fn latest_compaction_replaces_the_older_wire_prefix_only() {
        let encoder = encoder();
        let candidate = OpenAiContinuation::prepare(
            None,
            None,
            projection(&[("first", &["first"])]),
            &encoder,
        )
        .unwrap()
        .candidate
        .with_outputs(
            vec![CanonicalInputItem::assistant_text("provider", None)],
            vec![
                json!({"id": "cmp_1", "type": "compaction", "encrypted_content": "opaque"}),
                json!({
                    "id": "msg_1",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "provider", "annotations": []}],
                }),
            ],
        )
        .unwrap();

        assert_eq!(candidate.wire_input.len(), 2);
        assert_eq!(candidate.wire_input[0]["type"], "compaction");
        assert_eq!(candidate.wire_input[0]["encrypted_content"], "opaque");
    }

    #[test]
    fn missing_artifact_binding_keeps_causal_replay_and_resends_full_projection() {
        let encoder = encoder();
        let first =
            OpenAiContinuation::prepare(None, None, projection(&[("first", &["first"])]), &encoder)
                .unwrap()
                .candidate
                .with_outputs(
                    vec![CanonicalInputItem::assistant_text("provider", None)],
                    vec![json!({
                        "id": "msg_1",
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "provider", "annotations": []}],
                    })],
                )
                .unwrap();

        let prepared = OpenAiContinuation::prepare(
            Some(&first),
            None,
            projection(&[("first", &["first", "provider", "second"])]),
            &encoder,
        )
        .expect("a missing binding preserves causal replay with a full projection resend");
        let request: serde_json::Value = serde_json::from_slice(&prepared.request_body).unwrap();
        let input = request["input"].as_array().unwrap();

        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["id"], "msg_1");
        assert_eq!(input[1]["content"][0]["text"], "provider");
    }

    #[test]
    fn missing_continuation_discards_the_orphaned_artifact_binding() {
        let encoder = encoder();
        let orphaned_binding = OpenAiInlineArtifactBinding::new("stale instructions".to_owned());

        let prepared = OpenAiContinuation::prepare(
            None,
            Some(&orphaned_binding),
            projection(&[("first", &["current"])]),
            &encoder,
        )
        .expect("a missing continuation starts Fresh from the complete projection");
        let request: serde_json::Value = serde_json::from_slice(&prepared.request_body).unwrap();
        let input = request["input"].as_array().unwrap();

        assert!(request.get("instructions").is_none());
        assert_eq!(prepared.artifact_binding.instructions(), "");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "current");
        assert!(input[0].get("id").is_none());
    }

    #[test]
    fn compatible_session_appends_only_unsubmitted_items_per_node() {
        let encoder = encoder();
        let previous = OpenAiContinuation::prepare(
            None,
            None,
            projection(&[("left", &["A"]), ("right", &["O"])]),
            &encoder,
        )
        .unwrap()
        .candidate;
        let binding = OpenAiInlineArtifactBinding::new(String::new());

        let next = OpenAiContinuation::prepare(
            Some(&previous),
            Some(&binding),
            projection(&[("left", &["A", "B"]), ("right", &["O", "P"])]),
            &encoder,
        )
        .unwrap();
        let next_request: serde_json::Value = serde_json::from_slice(&next.request_body).unwrap();
        let next_input = next_request["input"].as_array().unwrap();
        assert_eq!(next_input.len(), 4);
        assert_eq!(next_input[2]["content"][0]["text"], "B");
        assert_eq!(next_input[3]["content"][0]["text"], "P");

        let later = OpenAiContinuation::prepare(
            Some(&next.candidate),
            Some(&binding),
            projection(&[("left", &["A", "B", "C"]), ("right", &["O"])]),
            &encoder,
        )
        .unwrap();
        let later_request: serde_json::Value = serde_json::from_slice(&later.request_body).unwrap();
        let later_input = later_request["input"].as_array().unwrap();
        assert_eq!(later_input.len(), 5);
        assert_eq!(later_input[4]["content"][0]["text"], "C");
    }

    #[test]
    fn compatible_session_counts_occurrences_and_appends_in_current_node_order() {
        let encoder = encoder();
        let first = OpenAiContinuation::prepare(
            None,
            None,
            projection(&[("left", &["A", "A"]), ("right", &["O"])]),
            &encoder,
        )
        .unwrap();
        assert_eq!(input_texts(&first.request_body), ["A", "A", "O"]);
        let binding = OpenAiInlineArtifactBinding::new(String::new());

        let prepared = OpenAiContinuation::prepare(
            Some(&first.candidate),
            Some(&binding),
            projection(&[("right", &["O", "P"]), ("left", &["A", "A", "A"])]),
            &encoder,
        )
        .unwrap();

        assert_eq!(
            input_texts(&prepared.request_body),
            ["A", "A", "O", "P", "A"]
        );
    }

    #[test]
    fn omission_and_reappearance_keep_identity_while_identity_change_resubmits() {
        let encoder = encoder();
        let binding = OpenAiInlineArtifactBinding::new(String::new());
        let previous =
            OpenAiContinuation::prepare(None, None, projection(&[("stable", &["A"])]), &encoder)
                .unwrap()
                .candidate;

        let omitted = OpenAiContinuation::prepare(
            Some(&previous),
            Some(&binding),
            projection(&[("other", &[])]),
            &encoder,
        )
        .unwrap();
        assert_eq!(input_texts(&omitted.request_body), ["A"]);

        let reappeared = OpenAiContinuation::prepare(
            Some(&omitted.candidate),
            Some(&binding),
            projection(&[("stable", &["A", "B"])]),
            &encoder,
        )
        .unwrap();
        assert_eq!(input_texts(&reappeared.request_body), ["A", "B"]);

        let replaced = OpenAiContinuation::prepare(
            Some(&reappeared.candidate),
            Some(&binding),
            projection(&[("replacement", &["A"])]),
            &encoder,
        )
        .unwrap();
        assert_eq!(input_texts(&replaced.request_body), ["A", "B", "A"]);
    }

    #[test]
    fn exact_duplicate_keeps_the_compatible_submitted_window() {
        let encoder = encoder();
        let current = projection(&[("only", &["A"])]);
        let previous = OpenAiContinuation::prepare(None, None, current.clone(), &encoder)
            .unwrap()
            .candidate;

        let prepared = OpenAiContinuation::prepare(
            Some(&previous),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            current,
            &encoder,
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&prepared.request_body).unwrap();

        assert_eq!(request["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn missing_or_remounted_diff_memo_resends_full_projection_without_losing_history() {
        let encoder = encoder();
        let scope_a = ProjectionExecutionScope {
            host_instance: 7,
            mount_generation: 1,
        };
        let scope_b = ProjectionExecutionScope {
            host_instance: 7,
            mount_generation: 2,
        };
        let first_projection = projection(&[("root", &["A"])]).with_execution_scope(scope_a);
        let first = OpenAiContinuation::prepare(None, None, first_projection, &encoder).unwrap();

        let memo_lost = OpenAiContinuation::prepare(
            Some(&first.candidate),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            projection(&[("root", &["A"])]).with_execution_scope(scope_a),
            &encoder,
        )
        .unwrap();
        assert_eq!(input_texts(&memo_lost.request_body), ["A", "A"]);

        let remounted = OpenAiContinuation::prepare(
            Some(&first.candidate),
            Some(&first.artifact_binding),
            projection(&[("root", &["A"])]).with_execution_scope(scope_b),
            &encoder,
        )
        .unwrap();
        assert_eq!(input_texts(&remounted.request_body), ["A", "A"]);
    }

    #[test]
    fn handed_off_faulted_projection_remains_the_next_causal_baseline() {
        let encoder = encoder();
        let first =
            OpenAiContinuation::prepare(None, None, projection(&[("root", &["A"])]), &encoder)
                .unwrap();
        // This prepared gate represents B after its immutable request crossed
        // transport; a later stream fault does not undo its candidate.
        let faulted_b = OpenAiContinuation::prepare(
            Some(&first.candidate),
            Some(&first.artifact_binding),
            projection(&[("root", &["A", "B"])]),
            &encoder,
        )
        .unwrap();
        let next = OpenAiContinuation::prepare(
            Some(&faulted_b.candidate),
            Some(&faulted_b.artifact_binding),
            projection(&[("root", &["A", "B", "C"])]),
            &encoder,
        )
        .unwrap();

        assert_eq!(input_texts(&next.request_body), ["A", "B", "C"]);
    }

    #[test]
    fn exact_duplicate_reconciliation_uses_one_index_probe_per_item() {
        const ITEM_COUNT: usize = 128;
        let encoder = encoder();
        let items = (0..ITEM_COUNT)
            .map(|index| CanonicalInputItem::assistant_text(format!("item-{index}"), None))
            .collect::<Vec<_>>();
        let current =
            RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("only", items)])
                .unwrap();
        let previous = OpenAiContinuation::prepare(None, None, current.clone(), &encoder)
            .unwrap()
            .candidate;

        let prepared = OpenAiContinuation::prepare(
            Some(&previous),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            current,
            &encoder,
        )
        .unwrap();

        assert_eq!(prepared.candidate.reconciliation_index_probes(), ITEM_COUNT);
    }

    #[test]
    fn provider_output_reconciliation_uses_at_most_two_index_probes_per_item() {
        const OUTPUT_COUNT: usize = 128;
        let encoder = encoder();
        let output_items = (0..OUTPUT_COUNT)
            .map(|index| CanonicalInputItem::assistant_text(format!("provider-{index}"), None))
            .collect::<Vec<_>>();
        let wire_items = (0..OUTPUT_COUNT)
            .map(|index| {
                json!({
                    "id": format!("msg_{index}"),
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": format!("provider-{index}"),
                        "annotations": [],
                    }],
                })
            })
            .collect::<Vec<_>>();
        let previous = OpenAiContinuation::prepare(
            None,
            None,
            projection(&[("only", &["authored"])]),
            &encoder,
        )
        .unwrap()
        .candidate
        .with_outputs(output_items.clone(), wire_items)
        .unwrap();
        let current = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "only",
            std::iter::once(CanonicalInputItem::assistant_text("authored", None))
                .chain(output_items)
                .collect(),
        )])
        .unwrap();

        let prepared = OpenAiContinuation::prepare(
            Some(&previous),
            Some(&OpenAiInlineArtifactBinding::new(String::new())),
            current,
            &encoder,
        )
        .unwrap();

        assert_eq!(
            prepared.candidate.reconciliation_index_probes(),
            1 + (OUTPUT_COUNT * 2)
        );
    }

    #[test]
    fn retained_snapshot_preserves_encoder_binding_and_pruned_wire_without_future_projection() {
        let encoder = CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                "snapshot-model",
                Some(vec![CodexFunctionTool::new(
                    "snapshot_tool",
                    "Snapshot tool description",
                    json!({"type": "object", "properties": {}}),
                    true,
                )
                .unwrap()]),
                Some(CodexReasoning::max_detailed()),
                Some("snapshot-cache-key"),
            )
            .unwrap(),
        );
        let current = projection(&[("current", &["current-input"])]);
        let compaction = json!({
            "id": "cmp_snapshot",
            "type": "compaction",
            "encrypted_content": "opaque-snapshot",
        });
        let message = json!({
            "id": "msg_snapshot",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "provider", "annotations": []}],
        });
        let candidate = OpenAiContinuation::prepare(None, None, current, &encoder)
            .unwrap()
            .candidate
            .with_outputs(
                vec![CanonicalInputItem::assistant_text("provider", None)],
                vec![compaction.clone(), message.clone()],
            )
            .unwrap();
        let binding = OpenAiInlineArtifactBinding::new("confirmed binding".to_owned());

        let snapshot = candidate
            .encode_retained_snapshot(&binding, &encoder)
            .unwrap();
        let snapshot: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();

        assert_eq!(snapshot["model"], "snapshot-model");
        assert_eq!(snapshot["tools"][0]["name"], "snapshot_tool");
        assert_eq!(snapshot["reasoning"]["effort"], "max");
        assert_eq!(snapshot["reasoning"]["summary"], "detailed");
        assert_eq!(snapshot["prompt_cache_key"], "snapshot-cache-key");
        assert_eq!(snapshot["instructions"], "confirmed binding");
        assert_eq!(snapshot["input"], json!([compaction, message]));
        assert!(!snapshot.to_string().contains("arbitrary-future-projection"));

        let future = projection(&[("future", &["arbitrary-future-projection"])]);
        let future_request =
            OpenAiContinuation::prepare(Some(&candidate), Some(&binding), future, &encoder)
                .unwrap();
        assert!(String::from_utf8(future_request.request_body)
            .unwrap()
            .contains("arbitrary-future-projection"));
    }
}
