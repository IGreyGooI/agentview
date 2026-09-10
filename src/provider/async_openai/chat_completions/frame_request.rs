//! Frame-native Chat Completions request state.
//!
//! The state retained here is only the target's accepted wire baseline. Shared
//! canonical history, Component projection reconciliation, and semantic diff
//! selection remain owned by the private `FrameSession`.

use crate::{
    component::execution::reaction::{Frame, FrameBasis, FrameRevision},
    transcript::{CanonicalInputItem, InstructionAuthority},
};

use super::{
    history::{encode_request, lower_item, ChatMessage},
    OpenAiChatCompletionsError, OpenAiChatCompletionsOptions,
};
use crate::provider::async_openai::reaction_fault::OpenAiFailureClass;

#[derive(Clone)]
pub(super) struct ChatFrameRequestState {
    revision: FrameRevision,
    wire_messages: Vec<ChatMessage>,
}

impl ChatFrameRequestState {
    pub(super) fn prepare(
        previous: Option<&Self>,
        frame: &Frame,
        options: &OpenAiChatCompletionsOptions,
        max_serialized_request_body_bytes: usize,
    ) -> Result<PreparedChatFrameRequest, ChatFrameRequestFault> {
        if !frame.submission().tools().definitions().is_empty() {
            return Err(ChatFrameRequestFault::UnsupportedNativeToolDeclarations);
        }

        let mut wire_messages = match frame.basis() {
            FrameBasis::Full => Vec::new(),
            FrameBasis::DeltaFrom(base) => {
                let previous = previous.ok_or(ChatFrameRequestFault::MissingDeltaBaseline)?;
                if previous.revision != base {
                    return Err(ChatFrameRequestFault::DeltaBaselineMismatch);
                }
                previous.wire_messages.clone()
            }
        };

        let system = frame_system_item(frame)?;
        if let Some(system) = system {
            wire_messages.extend(lower_item(system).map_err(ChatFrameRequestFault::Lowering)?);
        }
        for item in ordered_non_system_items(frame) {
            wire_messages.extend(lower_item(item).map_err(ChatFrameRequestFault::Lowering)?);
        }

        let request_body =
            encode_request(options, &wire_messages, max_serialized_request_body_bytes)
                .map_err(ChatFrameRequestFault::Lowering)?;
        Ok(PreparedChatFrameRequest {
            request_body,
            state: Self {
                revision: frame.revision(),
                wire_messages,
            },
        })
    }

    pub(super) const fn revision(&self) -> FrameRevision {
        self.revision
    }
}

pub(super) struct PreparedChatFrameRequest {
    pub(super) request_body: Vec<u8>,
    pub(super) state: ChatFrameRequestState,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ChatFrameRequestFault {
    #[error("Delta Frame has no accepted Chat Completions wire baseline")]
    MissingDeltaBaseline,
    #[error("Delta Frame base does not match the Chat Completions wire baseline")]
    DeltaBaselineMismatch,
    #[error("System input appeared outside the Full Component snapshot")]
    UnexpectedSystemItem,
    #[error("Full Component snapshot contains more than one System item")]
    MultipleSystemItems,
    #[error("Chat Completions does not support native tool declarations")]
    UnsupportedNativeToolDeclarations,
    #[error(transparent)]
    Lowering(OpenAiChatCompletionsError),
}

impl ChatFrameRequestFault {
    pub(super) fn failure_class(&self) -> OpenAiFailureClass {
        match self {
            Self::Lowering(OpenAiChatCompletionsError::SerializedRequestBodyLimit) => {
                OpenAiFailureClass::RequestBodyLimit
            }
            Self::MissingDeltaBaseline
            | Self::DeltaBaselineMismatch
            | Self::UnexpectedSystemItem
            | Self::MultipleSystemItems
            | Self::UnsupportedNativeToolDeclarations
            | Self::Lowering(_) => OpenAiFailureClass::RequestPreparation,
        }
    }
}

fn ordered_non_system_items(frame: &Frame) -> impl Iterator<Item = &CanonicalInputItem> {
    frame
        .submission()
        .replay()
        .iter()
        .chain(frame.submission().staged_inputs())
        .chain(frame.submission().projection().items())
        .filter(|item| !is_system(item))
}

fn frame_system_item(frame: &Frame) -> Result<Option<&CanonicalInputItem>, ChatFrameRequestFault> {
    if frame
        .submission()
        .replay()
        .iter()
        .chain(frame.submission().staged_inputs())
        .any(is_system)
    {
        return Err(ChatFrameRequestFault::UnexpectedSystemItem);
    }
    let mut systems = frame
        .submission()
        .projection()
        .items()
        .iter()
        .filter(|item| is_system(item));
    let first = systems.next();
    if systems.next().is_some() {
        return Err(ChatFrameRequestFault::MultipleSystemItems);
    }
    match frame.basis() {
        FrameBasis::DeltaFrom(_) if first.is_some() => {
            Err(ChatFrameRequestFault::UnexpectedSystemItem)
        }
        FrameBasis::DeltaFrom(_) => Ok(None),
        FrameBasis::Full => Ok(first),
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

    use serde_json::Value;

    use crate::{
        component::execution::reaction::{
            Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
            FrameSubmission, ProjectionSubmission, TargetContinuity, TargetEpoch, TargetIdentity,
            ToolCatalog,
        },
        pom::{Document, TextNode, XmlNode},
        pom_resolution::resolve_system_document,
        transcript::{CanonicalInputItem, InstructionAuthority},
    };

    use super::{ChatFrameRequestFault, ChatFrameRequestState};
    use crate::provider::async_openai::chat_completions::OpenAiChatCompletionsOptions;

    fn options() -> OpenAiChatCompletionsOptions {
        OpenAiChatCompletionsOptions::new("test-model").unwrap()
    }

    fn target() -> TargetIdentity {
        TargetIdentity::new(NonZeroU128::new(81).unwrap())
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
            NonZeroU128::new(93).unwrap(),
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
        staged_inputs: Vec<CanonicalInputItem>,
        projection: Vec<CanonicalInputItem>,
        tools: Vec<&str>,
    ) -> Frame {
        frame(
            revision(sequence),
            TargetContinuity::FullRequired { epoch: epoch() },
            FrameBasis::Full,
            replay,
            staged_inputs,
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

    fn contents(request: &[u8]) -> Vec<String> {
        body(request)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn full_lifts_system_then_lowers_exact_typed_section_order() {
        let frame = full(
            1,
            vec![CanonicalInputItem::assistant_text("replay", None)],
            vec![CanonicalInputItem::assistant_text("staged", None)],
            vec![
                system("policy"),
                CanonicalInputItem::assistant_text("projection", None),
            ],
            Vec::new(),
        );

        let prepared = ChatFrameRequestState::prepare(None, &frame, &options(), 64 * 1024).unwrap();
        let body = body(&prepared.request_body);

        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            contents(&prepared.request_body),
            ["<system>policy</system>", "replay", "staged", "projection"]
        );
    }

    #[test]
    fn delta_appends_the_new_canonical_tail_exactly_once() {
        let initial = full(
            1,
            Vec::new(),
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("authored", None)],
            Vec::new(),
        );
        let state = ChatFrameRequestState::prepare(None, &initial, &options(), 64 * 1024)
            .unwrap()
            .state;
        let next = delta(
            2,
            revision(1),
            vec![CanonicalInputItem::assistant_text("answer", None)],
            Vec::new(),
            vec![CanonicalInputItem::assistant_text("next", None)],
        );

        let prepared =
            ChatFrameRequestState::prepare(Some(&state), &next, &options(), 64 * 1024).unwrap();

        assert_eq!(
            contents(&prepared.request_body),
            ["authored", "answer", "next"]
        );
        assert_eq!(prepared.state.revision(), revision(2));
    }

    #[test]
    fn delta_rejects_missing_or_mismatched_baseline() {
        let initial = full(1, Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let state = ChatFrameRequestState::prepare(None, &initial, &options(), 64 * 1024)
            .unwrap()
            .state;

        let missing = delta(2, revision(1), Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            ChatFrameRequestState::prepare(None, &missing, &options(), 64 * 1024),
            Err(ChatFrameRequestFault::MissingDeltaBaseline)
        ));

        let wrong_base = delta(3, revision(2), Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            ChatFrameRequestState::prepare(Some(&state), &wrong_base, &options(), 64 * 1024),
            Err(ChatFrameRequestFault::DeltaBaselineMismatch)
        ));
    }

    #[test]
    fn full_rebuild_ignores_the_old_wire_baseline() {
        let initial = full(
            1,
            Vec::new(),
            Vec::new(),
            vec![
                system("old"),
                CanonicalInputItem::assistant_text("old", None),
            ],
            Vec::new(),
        );
        let state = ChatFrameRequestState::prepare(None, &initial, &options(), 64 * 1024)
            .unwrap()
            .state;
        let replacement = full(
            2,
            vec![CanonicalInputItem::assistant_text("retained", None)],
            Vec::new(),
            vec![system("new")],
            Vec::new(),
        );

        let prepared =
            ChatFrameRequestState::prepare(Some(&state), &replacement, &options(), 64 * 1024)
                .unwrap();

        assert_eq!(
            contents(&prepared.request_body),
            ["<system>new</system>", "retained"]
        );
    }

    #[test]
    fn native_tool_catalog_and_invalid_system_placement_fail_before_encoding() {
        let tools = full(1, Vec::new(), Vec::new(), Vec::new(), vec!["lookup"]);
        assert!(matches!(
            ChatFrameRequestState::prepare(None, &tools, &options(), 64 * 1024),
            Err(ChatFrameRequestFault::UnsupportedNativeToolDeclarations)
        ));

        let system_in_replay = full(
            1,
            vec![system("invalid")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            ChatFrameRequestState::prepare(None, &system_in_replay, &options(), 64 * 1024),
            Err(ChatFrameRequestFault::UnexpectedSystemItem)
        ));

        let initial = full(1, Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let state = ChatFrameRequestState::prepare(None, &initial, &options(), 64 * 1024)
            .unwrap()
            .state;
        let delta_system = delta(
            2,
            revision(1),
            Vec::new(),
            Vec::new(),
            vec![system("invalid")],
        );
        assert!(matches!(
            ChatFrameRequestState::prepare(Some(&state), &delta_system, &options(), 64 * 1024),
            Err(ChatFrameRequestFault::UnexpectedSystemItem)
        ));
    }

    #[test]
    fn exact_serialized_body_limit_is_inclusive() {
        let make = || {
            full(
                1,
                Vec::new(),
                Vec::new(),
                vec![CanonicalInputItem::assistant_text("payload", None)],
                Vec::new(),
            )
        };
        let exact = ChatFrameRequestState::prepare(None, &make(), &options(), 64 * 1024)
            .unwrap()
            .request_body
            .len();

        assert!(ChatFrameRequestState::prepare(None, &make(), &options(), exact).is_ok());
        assert!(matches!(
            ChatFrameRequestState::prepare(None, &make(), &options(), exact - 1),
            Err(ChatFrameRequestFault::Lowering(
                crate::provider::async_openai::chat_completions::OpenAiChatCompletionsError::SerializedRequestBodyLimit
            ))
        ));
    }
}
