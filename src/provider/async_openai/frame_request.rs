//! Frame-native OpenAI Responses request state.
//!
//! This module owns only provider wire continuation and compact canonical
//! coverage proofs. It never retains a Component projection, shared canonical
//! history, semantic diff baseline, or ToolOutput staging.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    component::execution::reaction::{Frame, FrameBasis, FrameRevision},
    provider::codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Error},
    transcript::{CanonicalInputItem, InstructionAuthority},
};

use super::reaction_fault::OpenAiFailureClass;

const PREFIX_PROOF_DOMAIN: &[u8] = b"agentview:openai-responses:canonical-prefix:v1";
const COMPACTION_INSTRUCTIONS_PROOF_DOMAIN: &[u8] =
    b"agentview:openai-responses:compaction-instructions:v1";

/// Versioned binding between an opaque compaction artifact and the normalized
/// System instructions under which the provider produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionInstructionsProof([u8; 32]);

impl CompactionInstructionsProof {
    fn for_normalized_instructions(instructions: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(COMPACTION_INSTRUCTIONS_PROOF_DOMAIN);
        hasher.update(instructions.as_bytes());
        Self(hasher.finalize().into())
    }
}

/// Versioned proof that one wire snapshot represents an exact canonical prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CanonicalPrefixProof {
    item_count: usize,
    digest: [u8; 32],
}

impl CanonicalPrefixProof {
    fn empty() -> Self {
        Self {
            item_count: 0,
            digest: Sha256::digest(PREFIX_PROOF_DOMAIN).into(),
        }
    }

    fn extended(self, item: &CanonicalInputItem) -> Result<Self, ResponsesFrameRequestFault> {
        let encoded = serde_json_canonicalizer::to_vec(item)
            .map_err(|_| ResponsesFrameRequestFault::Canonicalization)?;
        let item_bytes = u64::try_from(encoded.len())
            .map_err(|_| ResponsesFrameRequestFault::Canonicalization)?;
        let item_count = self
            .item_count
            .checked_add(1)
            .ok_or(ResponsesFrameRequestFault::Canonicalization)?;
        let mut hasher = Sha256::new();
        hasher.update(PREFIX_PROOF_DOMAIN);
        hasher.update(self.digest);
        hasher.update(item_bytes.to_be_bytes());
        hasher.update(encoded);
        Ok(Self {
            item_count,
            digest: hasher.finalize().into(),
        })
    }

    fn extend<'a>(
        mut self,
        items: impl IntoIterator<Item = &'a CanonicalInputItem>,
    ) -> Result<Self, ResponsesFrameRequestFault> {
        for item in items {
            self = self.extended(item)?;
        }
        Ok(self)
    }

    #[cfg(test)]
    pub(super) const fn item_count(self) -> usize {
        self.item_count
    }
}

/// Provider wire snapshot accepted at one Frame revision.
#[derive(Debug, Clone)]
pub(super) struct ResponsesFrameRequestState {
    revision: FrameRevision,
    wire_input: Vec<Value>,
    instructions: String,
    accepted_prefix: CanonicalPrefixProof,
    wire_coverage: CanonicalPrefixProof,
    compaction_instructions: Option<CompactionInstructionsProof>,
    pending_calls: BTreeMap<u64, PendingWireCall>,
    last_output_index: Option<u64>,
}

#[derive(Debug, Clone)]
struct PendingWireCall {
    call_id: String,
    wire_item: Value,
}

impl ResponsesFrameRequestState {
    pub(super) fn prepare(
        previous: Option<&Self>,
        frame: &Frame,
        encoder: &CodexHttpV1Encoder,
        max_serialized_request_body_bytes: usize,
    ) -> Result<PreparedResponsesFrameRequest, ResponsesFrameRequestFault> {
        let all_items = ordered_frame_items(frame);
        let system = frame_system_instructions(frame, encoder)?;

        let (mut state, skip) = match frame.basis() {
            FrameBasis::Full => Self::prepare_full_base(previous, frame, &all_items, system)?,
            FrameBasis::DeltaFrom(base) => {
                Self::prepare_delta_base(previous, frame, base, &all_items)?
            }
        };

        for item in all_items.iter().skip(skip) {
            state.lower_and_append(item, encoder)?;
        }
        state.accepted_prefix = match frame.basis() {
            FrameBasis::Full => CanonicalPrefixProof::empty().extend(all_items.iter().copied())?,
            FrameBasis::DeltaFrom(_) => previous
                .expect("Delta base was validated")
                .accepted_prefix
                .extend(all_items.iter().copied())?,
        };
        state.wire_coverage = state.accepted_prefix;
        state.revision = frame.revision();
        state.last_output_index = None;

        let request_body = encoder
            .encode_frame_request_bounded(
                &state.wire_input,
                &state.instructions,
                frame.submission().tools().names(),
                max_serialized_request_body_bytes,
            )
            .map_err(ResponsesFrameRequestFault::Encoding)?;

        Ok(PreparedResponsesFrameRequest {
            request_body,
            state,
        })
    }

    fn prepare_full_base(
        previous: Option<&Self>,
        frame: &Frame,
        all_items: &[&CanonicalInputItem],
        instructions: String,
    ) -> Result<(Self, usize), ResponsesFrameRequestFault> {
        let Some(previous) = previous else {
            return Ok((
                Self {
                    revision: frame.revision(),
                    wire_input: Vec::new(),
                    instructions,
                    accepted_prefix: CanonicalPrefixProof::empty(),
                    wire_coverage: CanonicalPrefixProof::empty(),
                    compaction_instructions: None,
                    pending_calls: BTreeMap::new(),
                    last_output_index: None,
                },
                0,
            ));
        };

        if previous.compaction_instructions.is_some_and(|proof| {
            proof != CompactionInstructionsProof::for_normalized_instructions(&instructions)
        }) {
            return Err(ResponsesFrameRequestFault::CompactionInstructionsMismatch);
        }

        let covered = previous.wire_coverage.item_count;
        if covered > all_items.len() {
            return Err(ResponsesFrameRequestFault::CoverageMismatch);
        }
        let observed =
            CanonicalPrefixProof::empty().extend(all_items[..covered].iter().copied())?;
        if observed != previous.wire_coverage {
            return Err(ResponsesFrameRequestFault::CoverageMismatch);
        }
        Ok((
            Self {
                revision: frame.revision(),
                wire_input: previous.wire_input.clone(),
                instructions,
                accepted_prefix: CanonicalPrefixProof::empty(),
                wire_coverage: previous.wire_coverage,
                compaction_instructions: previous.compaction_instructions,
                pending_calls: previous.pending_calls.clone(),
                last_output_index: None,
            },
            covered,
        ))
    }

    fn prepare_delta_base(
        previous: Option<&Self>,
        frame: &Frame,
        base: FrameRevision,
        all_items: &[&CanonicalInputItem],
    ) -> Result<(Self, usize), ResponsesFrameRequestFault> {
        let previous = previous.ok_or(ResponsesFrameRequestFault::MissingDeltaBaseline)?;
        if previous.revision != base {
            return Err(ResponsesFrameRequestFault::DeltaBaselineMismatch);
        }
        let pending = previous
            .wire_coverage
            .item_count
            .checked_sub(previous.accepted_prefix.item_count)
            .ok_or(ResponsesFrameRequestFault::CoverageMismatch)?;
        if pending > all_items.len() {
            return Err(ResponsesFrameRequestFault::CoverageMismatch);
        }
        let observed = previous
            .accepted_prefix
            .extend(all_items[..pending].iter().copied())?;
        if observed != previous.wire_coverage {
            return Err(ResponsesFrameRequestFault::CoverageMismatch);
        }

        Ok((
            Self {
                revision: frame.revision(),
                wire_input: previous.wire_input.clone(),
                instructions: previous.instructions.clone(),
                accepted_prefix: previous.accepted_prefix,
                wire_coverage: previous.wire_coverage,
                compaction_instructions: previous.compaction_instructions,
                pending_calls: previous.pending_calls.clone(),
                last_output_index: None,
            },
            pending,
        ))
    }

    fn lower_and_append(
        &mut self,
        item: &CanonicalInputItem,
        encoder: &CodexHttpV1Encoder,
    ) -> Result<(), ResponsesFrameRequestFault> {
        if is_system(item) {
            return Err(ResponsesFrameRequestFault::UnexpectedSystemItem);
        }
        let (_, values) = encoder
            .lower_canonical_item(item)
            .map_err(ResponsesFrameRequestFault::Encoding)?
            .into_parts();
        self.wire_input.extend(values);
        if let CanonicalInputItem::ToolResult { call_id, .. } = item {
            self.pending_calls
                .retain(|_, call| call.call_id != *call_id);
        }
        Ok(())
    }

    /// Records one public output after the release barrier admits its index.
    pub(super) fn append_public_output(
        &mut self,
        output_index: u64,
        canonical_item: &CanonicalInputItem,
        wire_item: Value,
    ) -> Result<(), ResponsesFrameRequestFault> {
        self.advance_output_index(output_index)?;
        self.wire_coverage = self.wire_coverage.extended(canonical_item)?;
        if let CanonicalInputItem::ToolCall { call_id, .. } = canonical_item {
            self.pending_calls.insert(
                output_index,
                PendingWireCall {
                    call_id: call_id.clone(),
                    wire_item: wire_item.clone(),
                },
            );
        }
        self.wire_input.push(wire_item);
        Ok(())
    }

    /// Records one provider-private output after the release barrier admits it.
    pub(super) fn append_private_output(
        &mut self,
        output_index: u64,
        kind: PrivateOutputKind,
        wire_item: Value,
    ) -> Result<(), ResponsesFrameRequestFault> {
        self.advance_output_index(output_index)?;
        match kind {
            PrivateOutputKind::Reasoning => self.wire_input.push(wire_item),
            PrivateOutputKind::Compaction => {
                let mut compacted = Vec::with_capacity(1 + self.pending_calls.len());
                compacted.push(wire_item);
                compacted.extend(
                    self.pending_calls
                        .values()
                        .map(|call| call.wire_item.clone()),
                );
                self.wire_input = compacted;
                self.compaction_instructions = Some(
                    CompactionInstructionsProof::for_normalized_instructions(&self.instructions),
                );
            }
        }
        Ok(())
    }

    fn advance_output_index(
        &mut self,
        output_index: u64,
    ) -> Result<(), ResponsesFrameRequestFault> {
        if self
            .last_output_index
            .is_some_and(|previous| output_index <= previous)
        {
            return Err(ResponsesFrameRequestFault::OutputOrder);
        }
        self.last_output_index = Some(output_index);
        Ok(())
    }

    pub(super) const fn revision(&self) -> FrameRevision {
        self.revision
    }

    #[cfg(test)]
    pub(super) const fn accepted_prefix(&self) -> CanonicalPrefixProof {
        self.accepted_prefix
    }

    #[cfg(test)]
    pub(super) const fn wire_coverage(&self) -> CanonicalPrefixProof {
        self.wire_coverage
    }

    #[cfg(test)]
    fn wire_input(&self) -> &[Value] {
        &self.wire_input
    }
}

#[derive(Debug)]
pub(super) struct PreparedResponsesFrameRequest {
    pub(super) request_body: Vec<u8>,
    pub(super) state: ResponsesFrameRequestState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrivateOutputKind {
    Reasoning,
    Compaction,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ResponsesFrameRequestFault {
    #[error("Delta Frame has no accepted Responses wire baseline")]
    MissingDeltaBaseline,
    #[error("Delta Frame base does not match the Responses wire baseline")]
    DeltaBaselineMismatch,
    #[error("Responses canonical prefix coverage does not match the Frame")]
    CoverageMismatch,
    #[error("Responses private compaction was sealed under different System instructions")]
    CompactionInstructionsMismatch,
    #[error("System input appeared outside the Full Component snapshot")]
    UnexpectedSystemItem,
    #[error("Full Component snapshot contains more than one System item")]
    MultipleSystemItems,
    #[error("Responses output release order regressed")]
    OutputOrder,
    #[error("Responses canonical prefix proof could not be encoded")]
    Canonicalization,
    #[error(transparent)]
    Encoding(#[from] CodexHttpV1Error),
}

impl ResponsesFrameRequestFault {
    pub(super) fn failure_class(&self) -> OpenAiFailureClass {
        match self {
            Self::Encoding(CodexHttpV1Error::SerializedRequestBodyLimit) => {
                OpenAiFailureClass::RequestBodyLimit
            }
            Self::MissingDeltaBaseline
            | Self::DeltaBaselineMismatch
            | Self::CoverageMismatch
            | Self::CompactionInstructionsMismatch
            | Self::UnexpectedSystemItem
            | Self::MultipleSystemItems
            | Self::OutputOrder
            | Self::Canonicalization
            | Self::Encoding(_) => OpenAiFailureClass::RequestPreparation,
        }
    }
}

fn ordered_frame_items(frame: &Frame) -> Vec<&CanonicalInputItem> {
    frame
        .submission()
        .replay()
        .iter()
        .chain(frame.submission().staged_inputs())
        .chain(frame.submission().projection().items())
        .filter(|item| !is_system(item))
        .collect()
}

fn frame_system_instructions(
    frame: &Frame,
    encoder: &CodexHttpV1Encoder,
) -> Result<String, ResponsesFrameRequestFault> {
    if frame
        .submission()
        .replay()
        .iter()
        .chain(frame.submission().staged_inputs())
        .any(is_system)
    {
        return Err(ResponsesFrameRequestFault::UnexpectedSystemItem);
    }
    let systems = frame
        .submission()
        .projection()
        .items()
        .iter()
        .filter(|item| is_system(item))
        .collect::<Vec<_>>();
    match frame.basis() {
        FrameBasis::DeltaFrom(_) if !systems.is_empty() => {
            Err(ResponsesFrameRequestFault::UnexpectedSystemItem)
        }
        FrameBasis::DeltaFrom(_) => Ok(String::new()),
        FrameBasis::Full if systems.len() > 1 => {
            Err(ResponsesFrameRequestFault::MultipleSystemItems)
        }
        FrameBasis::Full => systems.first().map_or(Ok(String::new()), |item| {
            encoder
                .lower_canonical_item(item)
                .map_err(ResponsesFrameRequestFault::Encoding)
                .and_then(|lowered| {
                    let (instructions, input) = lowered.into_parts();
                    if !input.is_empty() {
                        return Err(ResponsesFrameRequestFault::UnexpectedSystemItem);
                    }
                    instructions.ok_or(ResponsesFrameRequestFault::UnexpectedSystemItem)
                })
        }),
    }
}

fn is_system(item: &CanonicalInputItem) -> bool {
    matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU128, NonZeroU64};

    use serde_json::{json, Value};

    use crate::{
        component::execution::reaction::{
            Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
            FrameSubmission, ProjectionSubmission, TargetContinuity, TargetEpoch, TargetIdentity,
            ToolCatalog,
        },
        pom::{Document, TextNode, XmlNode},
        pom_resolution::resolve_system_document,
        provider::codex_http_v1::{
            CodexFunctionTool, CodexHttpV1Encoder, CodexHttpV1Error, CodexHttpV1Options,
        },
        transcript::{
            AssistantPhase, CanonicalInputItem, InstructionAuthority,
            ASSISTANT_OUTPUT_INTERRUPTED_MARKER,
        },
    };

    use super::{PrivateOutputKind, ResponsesFrameRequestFault, ResponsesFrameRequestState};

    fn encoder() -> CodexHttpV1Encoder {
        CodexHttpV1Encoder::new(
            CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap(),
        )
    }

    fn encoder_with_configured_tool() -> CodexHttpV1Encoder {
        CodexHttpV1Encoder::new(
            CodexHttpV1Options::new(
                "test-model",
                Some(vec![CodexFunctionTool::new(
                    "configured_only",
                    "must not leak into a Frame request",
                    json!({}),
                    false,
                )
                .unwrap()]),
                None,
                None::<String>,
            )
            .unwrap(),
        )
    }

    fn target() -> TargetIdentity {
        TargetIdentity::new(NonZeroU128::new(73).unwrap())
    }

    fn epoch() -> TargetEpoch {
        TargetEpoch::new(NonZeroU64::MIN)
    }

    fn profile() -> FrameProfile {
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 64 * 1024,
                max_component_bytes: 16 * 1024,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        )
    }

    fn revision(sequence: u64) -> FrameRevision {
        FrameRevision::new(
            NonZeroU128::new(91).unwrap(),
            target(),
            epoch(),
            NonZeroU64::new(sequence).unwrap(),
        )
    }

    fn frame(
        revision: FrameRevision,
        prepared_against: TargetContinuity,
        basis: FrameBasis,
        replay: Vec<CanonicalInputItem>,
        staged_inputs: Vec<CanonicalInputItem>,
        projection: Vec<CanonicalInputItem>,
        tools: Vec<&str>,
    ) -> Frame {
        let submission = FrameSubmission::from_compiled(
            replay,
            staged_inputs,
            ProjectionSubmission::new(projection),
            ToolCatalog::new(tools.into_iter().map(str::to_owned).collect()).unwrap(),
            Vec::new(),
        );
        Frame::from_compiled(
            revision,
            target(),
            epoch(),
            prepared_against,
            profile(),
            basis,
            submission,
        )
        .unwrap()
    }

    fn full(
        sequence: u64,
        replay: Vec<CanonicalInputItem>,
        projection: Vec<CanonicalInputItem>,
        tools: Vec<&str>,
    ) -> Frame {
        frame(
            revision(sequence),
            TargetContinuity::FullRequired { epoch: epoch() },
            FrameBasis::Full,
            replay,
            Vec::new(),
            projection,
            tools,
        )
    }

    fn delta(
        sequence: u64,
        base: FrameRevision,
        replay: Vec<CanonicalInputItem>,
        staged_inputs: Vec<CanonicalInputItem>,
        projection: Vec<CanonicalInputItem>,
    ) -> Frame {
        frame(
            revision(sequence),
            TargetContinuity::Accepted {
                epoch: epoch(),
                revision: base,
            },
            FrameBasis::DeltaFrom(base),
            replay,
            staged_inputs,
            projection,
            Vec::new(),
        )
    }

    fn system(text: &str) -> CanonicalInputItem {
        let node = XmlNode::try_build("system", |children| {
            children.text(TextNode::new(text));
            Ok(())
        })
        .unwrap();
        CanonicalInputItem::instruction(
            InstructionAuthority::System,
            resolve_system_document(Document::from_xml(node)),
        )
    }

    fn body(request: &[u8]) -> Value {
        serde_json::from_slice(request).unwrap()
    }

    fn input(request: &[u8]) -> Vec<Value> {
        body(request)["input"].as_array().unwrap().clone()
    }

    fn tool_names(request: &[u8]) -> Vec<String> {
        body(request)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn initial_full_uses_snapshot_system_and_exact_tool_catalog() {
        let authored = CanonicalInputItem::assistant_text("authored", None);
        let frame = full(
            1,
            Vec::new(),
            vec![system("policy"), authored],
            vec!["lookup", "search"],
        );

        let prepared =
            ResponsesFrameRequestState::prepare(None, &frame, &encoder(), 64 * 1024).unwrap();
        let body = body(&prepared.request_body);

        assert_eq!(body["instructions"], "<system>policy</system>");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["content"][0]["text"], "authored");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][1]["name"], "search");
        assert_eq!(prepared.state.accepted_prefix().item_count(), 1);
        assert_eq!(
            prepared.state.accepted_prefix(),
            prepared.state.wire_coverage()
        );
    }

    #[test]
    fn lowers_typed_sections_in_replay_staged_projection_order() {
        let replay = vec![CanonicalInputItem::assistant_text("replay", None)];
        let staged = vec![CanonicalInputItem::tool_result("call-1", "staged").unwrap()];
        let projection = vec![CanonicalInputItem::assistant_text("projection", None)];
        let frame = frame(
            revision(1),
            TargetContinuity::FullRequired { epoch: epoch() },
            FrameBasis::Full,
            replay,
            staged,
            projection,
            Vec::new(),
        );

        let prepared =
            ResponsesFrameRequestState::prepare(None, &frame, &encoder(), 64 * 1024).unwrap();
        let input = input(&prepared.request_body);

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["content"][0]["text"], "replay");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["output"], "staged");
        assert_eq!(input[2]["content"][0]["text"], "projection");
    }

    #[test]
    fn delta_reuses_ordered_private_and_public_outputs_without_resubmitting_public_item() {
        let initial = full(
            1,
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("authored", None)],
            Vec::new(),
        );
        let mut state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        state
            .append_private_output(
                0,
                PrivateOutputKind::Reasoning,
                json!({"type": "reasoning", "encrypted_content": "opaque"}),
            )
            .unwrap();
        let answer = CanonicalInputItem::assistant_text("answer", None);
        state
            .append_public_output(
                1,
                &answer,
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}]
                }),
            )
            .unwrap();

        let next = delta(
            2,
            revision(1),
            vec![answer],
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("next", None)],
        );
        let prepared =
            ResponsesFrameRequestState::prepare(Some(&state), &next, &encoder(), 64 * 1024)
                .unwrap();
        let input = body(&prepared.request_body)["input"]
            .as_array()
            .unwrap()
            .clone();

        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"][0]["text"], "authored");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["content"][0]["text"], "answer");
        assert_eq!(input[3]["content"][0]["text"], "next");
        assert_eq!(prepared.state.accepted_prefix().item_count(), 3);
    }

    #[test]
    fn compaction_keeps_pending_call_and_delta_appends_its_output() {
        let initial = full(1, Vec::new(), Vec::new(), Vec::new());
        let mut state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let call = CanonicalInputItem::tool_call("call-1", "lookup", "{}").unwrap();
        state
            .append_public_output(
                0,
                &call,
                json!({
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": "{}"
                }),
            )
            .unwrap();
        state
            .append_private_output(
                1,
                PrivateOutputKind::Compaction,
                json!({
                    "type": "compaction",
                    "id": "cmp-1",
                    "encrypted_content": "opaque"
                }),
            )
            .unwrap();
        assert_eq!(state.wire_input().len(), 2);
        assert_eq!(state.wire_input()[0]["type"], "compaction");
        assert_eq!(state.wire_input()[1]["call_id"], "call-1");

        let result = CanonicalInputItem::tool_result("call-1", "done").unwrap();
        let next = delta(2, revision(1), vec![call], vec![result], Vec::new());
        let prepared =
            ResponsesFrameRequestState::prepare(Some(&state), &next, &encoder(), 64 * 1024)
                .unwrap();
        let input = body(&prepared.request_body)["input"]
            .as_array()
            .unwrap()
            .clone();

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "compaction");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "done");
    }

    #[test]
    fn full_recovery_requires_exact_versioned_prefix_coverage() {
        let initial = full(
            1,
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("first", None)],
            Vec::new(),
        );
        let state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let mismatched = full(
            2,
            vec![CanonicalInputItem::assistant_text("different", None)],
            Vec::new(),
            Vec::new(),
        );

        let error =
            ResponsesFrameRequestState::prepare(Some(&state), &mismatched, &encoder(), 64 * 1024)
                .unwrap_err();
        assert!(matches!(
            error,
            ResponsesFrameRequestFault::CoverageMismatch
        ));
    }

    #[test]
    fn full_recovers_from_private_compaction_with_matching_canonical_coverage() {
        let authored = CanonicalInputItem::assistant_text("authored", None);
        let initial = full(1, Vec::new(), vec![authored.clone()], Vec::new());
        let mut state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let provider_output = CanonicalInputItem::assistant_text("provider", None);
        state
            .append_public_output(
                0,
                &provider_output,
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "provider"}]
                }),
            )
            .unwrap();
        state
            .append_private_output(
                1,
                PrivateOutputKind::Compaction,
                json!({
                    "type": "compaction",
                    "id": "cmp-1",
                    "encrypted_content": "opaque"
                }),
            )
            .unwrap();

        let next = full(
            2,
            vec![authored, provider_output],
            vec![CanonicalInputItem::assistant_text("after", None)],
            Vec::new(),
        );
        let prepared =
            ResponsesFrameRequestState::prepare(Some(&state), &next, &encoder(), 64 * 1024)
                .unwrap();
        let input = input(&prepared.request_body);

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "compaction");
        assert_eq!(input[1]["content"][0]["text"], "after");
        assert_eq!(prepared.state.accepted_prefix().item_count(), 3);
        assert_eq!(
            prepared.state.accepted_prefix(),
            prepared.state.wire_coverage()
        );
    }

    #[test]
    fn compacted_full_recovery_requires_the_same_system_snapshot() {
        let authored = CanonicalInputItem::assistant_text("authored", None);
        let initial = full(
            1,
            Vec::new(),
            vec![system("stable"), authored.clone()],
            Vec::new(),
        );
        let mut state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        state
            .append_private_output(
                0,
                PrivateOutputKind::Compaction,
                json!({
                    "type": "compaction",
                    "id": "cmp-system",
                    "encrypted_content": "opaque"
                }),
            )
            .unwrap();
        assert!(state.compaction_instructions.is_some());

        let same = full(
            2,
            vec![authored.clone()],
            vec![system("stable")],
            Vec::new(),
        );
        let recovered =
            ResponsesFrameRequestState::prepare(Some(&state), &same, &encoder(), 64 * 1024)
                .unwrap();
        assert_eq!(
            body(&recovered.request_body)["instructions"],
            "<system>stable</system>"
        );
        assert_eq!(input(&recovered.request_body)[0]["type"], "compaction");

        let changed = full(
            2,
            vec![authored.clone()],
            vec![system("changed")],
            Vec::new(),
        );
        assert!(matches!(
            ResponsesFrameRequestState::prepare(Some(&state), &changed, &encoder(), 64 * 1024),
            Err(ResponsesFrameRequestFault::CompactionInstructionsMismatch)
        ));

        let cleared = full(2, vec![authored], Vec::new(), Vec::new());
        assert!(matches!(
            ResponsesFrameRequestState::prepare(Some(&state), &cleared, &encoder(), 64 * 1024),
            Err(ResponsesFrameRequestFault::CompactionInstructionsMismatch)
        ));
    }

    #[test]
    fn full_recovery_rejects_coverage_count_larger_than_candidate() {
        let initial = full(
            1,
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("covered", None)],
            Vec::new(),
        );
        let state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let shorter = full(2, Vec::new(), Vec::new(), Vec::new());

        let error =
            ResponsesFrameRequestState::prepare(Some(&state), &shorter, &encoder(), 64 * 1024)
                .unwrap_err();

        assert!(matches!(
            error,
            ResponsesFrameRequestFault::CoverageMismatch
        ));
    }

    #[test]
    fn delta_requires_an_exact_accepted_revision_baseline() {
        let exact_base = revision(1);
        let without_baseline = delta(2, exact_base, Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            ResponsesFrameRequestState::prepare(None, &without_baseline, &encoder(), 64 * 1024),
            Err(ResponsesFrameRequestFault::MissingDeltaBaseline)
        ));

        let initial = full(1, Vec::new(), Vec::new(), Vec::new());
        let state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let wrong_base = revision(7);
        let mismatched = delta(8, wrong_base, Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            ResponsesFrameRequestState::prepare(Some(&state), &mismatched, &encoder(), 64 * 1024),
            Err(ResponsesFrameRequestFault::DeltaBaselineMismatch)
        ));

        let exact = delta(
            2,
            exact_base,
            Vec::new(),
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("delta", None)],
        );
        let prepared =
            ResponsesFrameRequestState::prepare(Some(&state), &exact, &encoder(), 64 * 1024)
                .unwrap();
        assert_eq!(prepared.state.revision(), revision(2));
        assert_eq!(
            input(&prepared.request_body)[0]["content"][0]["text"],
            "delta"
        );
    }

    #[test]
    fn without_compaction_delta_preserves_system_while_full_replaces_and_clears_it() {
        let authored = CanonicalInputItem::assistant_text("authored", None);
        let initial = full(
            1,
            Vec::new(),
            vec![system("old"), authored.clone()],
            Vec::new(),
        );
        let initial =
            ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024).unwrap();
        assert!(initial.state.compaction_instructions.is_none());
        assert_eq!(
            body(&initial.request_body)["instructions"],
            "<system>old</system>"
        );

        let delta_item = CanonicalInputItem::assistant_text("delta", None);
        let unchanged = delta(
            2,
            revision(1),
            Vec::new(),
            Vec::new(),
            vec![delta_item.clone()],
        );
        let unchanged = ResponsesFrameRequestState::prepare(
            Some(&initial.state),
            &unchanged,
            &encoder(),
            64 * 1024,
        )
        .unwrap();
        assert_eq!(
            body(&unchanged.request_body)["instructions"],
            "<system>old</system>"
        );

        let replacement_item = CanonicalInputItem::assistant_text("replacement", None);
        let replaced = full(
            3,
            vec![authored.clone(), delta_item.clone()],
            vec![system("new"), replacement_item.clone()],
            Vec::new(),
        );
        let replaced = ResponsesFrameRequestState::prepare(
            Some(&unchanged.state),
            &replaced,
            &encoder(),
            64 * 1024,
        )
        .unwrap();
        assert_eq!(
            body(&replaced.request_body)["instructions"],
            "<system>new</system>"
        );

        let cleared = full(
            4,
            vec![authored, delta_item, replacement_item],
            Vec::new(),
            Vec::new(),
        );
        let cleared = ResponsesFrameRequestState::prepare(
            Some(&replaced.state),
            &cleared,
            &encoder(),
            64 * 1024,
        )
        .unwrap();
        assert!(body(&cleared.request_body).get("instructions").is_none());
    }

    #[test]
    fn every_frame_uses_its_exact_tool_snapshot_including_empty_and_changed() {
        let encoder = encoder_with_configured_tool();
        let initial = full(1, Vec::new(), Vec::new(), vec!["zeta", "alpha"]);
        let initial =
            ResponsesFrameRequestState::prepare(None, &initial, &encoder, 64 * 1024).unwrap();
        assert_eq!(tool_names(&initial.request_body), vec!["alpha", "zeta"]);

        let empty = delta(2, revision(1), Vec::new(), Vec::new(), Vec::new());
        let empty =
            ResponsesFrameRequestState::prepare(Some(&initial.state), &empty, &encoder, 64 * 1024)
                .unwrap();
        assert!(tool_names(&empty.request_body).is_empty());
        assert!(!String::from_utf8_lossy(&empty.request_body).contains("configured_only"));

        let changed = frame(
            revision(3),
            TargetContinuity::Accepted {
                epoch: epoch(),
                revision: revision(2),
            },
            FrameBasis::DeltaFrom(revision(2)),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec!["beta"],
        );
        let changed =
            ResponsesFrameRequestState::prepare(Some(&empty.state), &changed, &encoder, 64 * 1024)
                .unwrap();
        assert_eq!(tool_names(&changed.request_body), vec!["beta"]);
    }

    #[test]
    fn interrupted_item_expands_without_disturbing_canonical_item_coverage() {
        let interrupted = CanonicalInputItem::interrupted_assistant_text(
            "partial",
            Some(AssistantPhase::Commentary),
        );
        let sealed = CanonicalInputItem::assistant_text("sealed", None);
        let frame = full(1, Vec::new(), vec![interrupted, sealed], Vec::new());

        let prepared =
            ResponsesFrameRequestState::prepare(None, &frame, &encoder(), 64 * 1024).unwrap();
        let input = input(&prepared.request_body);

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["phase"], "commentary");
        assert_eq!(input[0]["content"][0]["text"], "partial");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(
            input[1]["content"][0]["text"],
            ASSISTANT_OUTPUT_INTERRUPTED_MARKER
        );
        assert_eq!(input[2]["content"][0]["text"], "sealed");
        assert_eq!(prepared.state.accepted_prefix().item_count(), 2);
    }

    #[test]
    fn serialized_body_limit_is_inclusive_at_the_complete_frame_boundary() {
        let frame = full(
            1,
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("bounded", None)],
            vec!["lookup"],
        );
        let unbounded =
            ResponsesFrameRequestState::prepare(None, &frame, &encoder(), usize::MAX).unwrap();
        let exact_len = unbounded.request_body.len();

        let exact =
            ResponsesFrameRequestState::prepare(None, &frame, &encoder(), exact_len).unwrap();
        assert_eq!(exact.request_body, unbounded.request_body);
        assert!(matches!(
            ResponsesFrameRequestState::prepare(None, &frame, &encoder(), exact_len - 1),
            Err(ResponsesFrameRequestFault::Encoding(
                CodexHttpV1Error::SerializedRequestBodyLimit
            ))
        ));
    }

    #[test]
    fn staged_tool_output_is_appended_once_and_not_replayed_again() {
        let initial = full(1, Vec::new(), Vec::new(), vec!["lookup"]);
        let mut state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let call = CanonicalInputItem::tool_call("call-1", "lookup", "{}").unwrap();
        state
            .append_public_output(
                0,
                &call,
                json!({
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": "{}"
                }),
            )
            .unwrap();
        let result = CanonicalInputItem::tool_result("call-1", "done").unwrap();
        let with_output = frame(
            revision(2),
            TargetContinuity::Accepted {
                epoch: epoch(),
                revision: revision(1),
            },
            FrameBasis::DeltaFrom(revision(1)),
            vec![call.clone()],
            vec![result.clone()],
            Vec::new(),
            vec!["lookup"],
        );
        let with_output =
            ResponsesFrameRequestState::prepare(Some(&state), &with_output, &encoder(), 64 * 1024)
                .unwrap();
        let first_input = input(&with_output.request_body);
        assert_eq!(
            first_input
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count(),
            1
        );
        assert!(with_output.state.pending_calls.is_empty());

        let no_new_output = delta(3, revision(2), Vec::new(), Vec::new(), Vec::new());
        let no_new_output = ResponsesFrameRequestState::prepare(
            Some(&with_output.state),
            &no_new_output,
            &encoder(),
            64 * 1024,
        )
        .unwrap();
        let second_input = input(&no_new_output.request_body);
        assert_eq!(
            second_input
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count(),
            1
        );
        assert_eq!(second_input, first_input);
    }

    #[test]
    fn delta_cannot_smuggle_a_system_replacement() {
        let initial = full(1, Vec::new(), vec![system("old")], Vec::new());
        let state = ResponsesFrameRequestState::prepare(None, &initial, &encoder(), 64 * 1024)
            .unwrap()
            .state;
        let malformed = delta(2, revision(1), Vec::new(), Vec::new(), vec![system("new")]);

        let error =
            ResponsesFrameRequestState::prepare(Some(&state), &malformed, &encoder(), 64 * 1024)
                .unwrap_err();
        assert!(matches!(
            error,
            ResponsesFrameRequestFault::UnexpectedSystemItem
        ));
    }
}
