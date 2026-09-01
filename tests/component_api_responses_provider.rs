#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises the Responses ProviderPort adapter"
)]

use std::{
    convert::Infallible,
    io::{self, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            ApplicationHost, ApplicationHostFault, EngineObservation, EngineObserver,
            ProviderEvent, ProviderEventStream, ProviderFault, ProviderFaultCode,
            ProviderFaultKind, ProviderIdentity, ProviderPort, ProviderResponseEventReason,
            ProviderResponseEventType, ProviderResponseLedgerReason,
            ProviderResponseMessageTextReason, ProviderResponseOutputIdentityReason,
            RenderedProjection, RenderedProjectionNode,
        },
        prelude::*,
        ComponentHost,
    },
    pom::{Document, ResolvedDocument, TextNode, XmlNode},
    pom_resolution::resolve_artifact_document,
    provider::{
        async_openai::{
            AsyncOpenAiConfigError, AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig,
        },
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};
use async_openai::types::responses::ResponseStreamEvent;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::post,
    Router,
};
use futures::{FutureExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::instrument::WithSubscriber;

const BINDING: &str = "responses-provider-test-binding";
const LOOPBACK_PROXY_CHILD_ENV: &str = "AGENTVIEW_LOOPBACK_PROXY_TEST_CHILD";
const AMBIENT_HEADERS_CHILD_ENV: &str = "AGENTVIEW_RESPONSES_AMBIENT_HEADERS_TEST_CHILD";
const SAMPLE_CODEX_ORACLE_BODY: &[u8] = br#"{"model":"gpt-5.6-codex","instructions":"<sample_policy>Follow the response protocol.</sample_policy>","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"<sample_request>Return one result.&#10;turn\\_id: turn-1</sample_request>"}]}],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":null,"context_management":[{"type":"compaction","compact_threshold":200000}],"store":false,"stream":true,"include":["reasoning.encrypted_content"]}"#;

#[test]
fn serialized_responses_request_body_limit_must_be_nonzero() {
    let config = AsyncOpenAiTransportConfig::new("http://127.0.0.1:1", "test-token").unwrap();

    let error = match config.with_responses_serialized_request_body_limit(0) {
        Ok(_) => panic!("a zero serialized request-body limit was accepted"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        AsyncOpenAiConfigError::InvalidSerializedRequestBodyLimit
    );
    assert_eq!(
        error.to_string(),
        "OpenAI Responses serialized outbound request body limit must be non-zero"
    );
}

#[test]
fn responses_transport_rejects_non_ascii_api_keys_without_exposure() {
    let sentinel = "non-ascii-api-key-密";
    let error = match AsyncOpenAiTransportConfig::new("http://127.0.0.1:1", sentinel) {
        Ok(_) => panic!("a non-ASCII API key was accepted"),
        Err(error) => error,
    };

    assert_eq!(error, AsyncOpenAiConfigError::InvalidApiKey);
    assert!(!format!("{error:?}\n{error}").contains(sentinel));
}

#[derive(Clone)]
struct DiffPromptProps {
    value: String,
}

#[component]
fn diff_prompt_application(
    props: DiffPromptProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let value = props.value;
    view! {
        #[diff(slot = "state")]
        current_state { "{value}" }
    }
}

fn diff_projection(value: &str) -> RenderedProjection {
    let mut host = ComponentHost::new(
        diff_prompt_application,
        DiffPromptProps {
            value: value.to_owned(),
        },
    );
    host.render().unwrap().projection().clone()
}

#[derive(Clone)]
struct NativeToolLifecycleProps {
    projection: String,
}

#[component]
fn native_tool_lifecycle_application(
    props: NativeToolLifecycleProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let projection = props.projection;
    view! {
        lifecycle_projection { "{projection}" }
        {
            NativeToolCall::named("lifecycle_lookup")
                .on_call(|call| async move {
                    Ok::<_, Infallible>(call.output("lifecycle-tool-result"))
                })
        }
    }
}

#[derive(Clone, Copy)]
enum ServerReply {
    Completed,
    CompletedWithUsage,
    CompletedWithInconsistentUsage,
    CompletedWithUsageFixture(UsageFixture),
    CompletedWithoutTerminalOutput,
    PlaceholderCompleted,
    CompletedSecondMove,
    CompactionWithText,
    SealedReasoningThenEof,
    SealedReasoningOnlyThenEof,
    OpenTextThenSealedReasoningEof,
    SealedRealShapedReasoningOnlyThenEof,
    SealedReasoningOmittedFromCompleted,
    SealedReasoningEmptyTerminal,
    SealedCompactionThenEof,
    ObservationalMessageCompletedThenIncomplete,
    ObservationalMessageIncompleteThenCompleted,
    ObservationalReasoning,
    ObservationalCompaction,
    TerminalOutputIdentity(TerminalOutputIdentityCase),
    TerminalMessageCompatibility(TerminalMessageCompatibilityCase),
    TerminalMessageDiagnostic(TerminalMessageDiagnosticCase),
    WrongContentType,
    DelayedHeaders,
    Unauthorized,
    Forbidden,
    TooManyRequests,
    ServiceUnavailable,
    RequestTimeout,
    Conflict,
    MissingCompleted,
    StalledStream,
    OversizedEvent,
    OversizedDoneEvent,
    OversizedWireComment,
    ChunkedOversizedBody,
    ActiveUnterminatedStream,
    OutputOverflow,
    OutputLimitExact,
    OutputLimitPlusOne,
    StandardOutputLifecycle,
    StandardOutputAnnotationLifecycle,
    OutputTextAnnotationWithNullIndex,
    ReasoningWithText,
    ReasoningWithEmptyContent,
    ReasoningWithOmittedDoneEmptyCompletedContent,
    ReasoningWithEmptyDoneOmittedCompletedContent,
    ReasoningWithUnknownExtensions,
    ReasoningWithNullAddedContent,
    ReasoningWithNullDoneContent,
    ReasoningWithNullCompletedContent,
    ReasoningWithPlaintextAddedContent,
    ReasoningWithPlaintextDoneContent,
    ReasoningWithPlaintextCompletedContent,
    ReasoningTextDeltaWithOmittedTerminalContent,
    ReasoningTextDoneWithEmptyTerminalContent,
    ReasoningWithMixedMalformedAddedContent,
    ReasoningWithMixedMalformedDoneContent,
    ReasoningWithMixedMalformedCompletedContent,
    PhasedMultilineText,
    OutputItemIdentityDrift,
    ResponseIdentityDrift,
    ContentPartDrift,
    UnknownOutputEvent,
    WaitForDispatch,
    WaitAfterTextDone,
    MalformedSse,
    EventMissingType,
    EventMissingTypeAfterTerminal,
    EventNonStringType,
    EventMissingSequence,
    EventNonIntegerSequence,
    InvalidUtf8Sse,
    NativeTool,
    NativeToolLifecycleToolOnly,
    NativeToolLifecycleToolOnlyWithUsage,
    NativeToolMissingOutputIndex,
    NativeToolItemWithText,
    NativeToolDoneWithText,
    NativeToolOnlyInCompletedOutput,
    TerminalIdentityDriftWithNativeTool,
    UnsupportedContentPartMissingFields,
    Failed,
    Incomplete,
    Error,
    DuplicateDone,
    DeltaAfterTextDone,
    DuplicateTerminal,
    EventAfterTerminal,
    NonMonotonicSequence,
    MismatchedTextIdentity,
    OutputItemAddedLifecycle(OutputItemAddedLifecycleFault),
    UnsealedPrivateAtTerminal(UnsealedPrivateKind),
}

#[derive(Clone, Copy)]
enum TerminalMessageCompatibilityCase {
    MissingId,
    NullId,
    MissingStatus,
    NullStatus,
    MissingRole,
    NullRole,
    MissingAnnotations,
    NullAnnotations,
    UnknownExtensions,
    MissingTerminalPhaseAfterExplicitFinalAnswer,
    NullTerminalPhaseAfterExplicitFinalAnswer,
    ExplicitFinalAnswerAfterMissingLifecyclePhase,
    ExplicitFinalAnswerAfterNullLifecyclePhase,
}

#[derive(Clone, Copy)]
enum TerminalMessageDiagnosticCase {
    InvalidShape,
    TextMismatch,
    OrphanTextMismatch,
    PhaseMismatch,
}

#[derive(Clone, Copy)]
enum TerminalOutputPrefix {
    Reasoning,
    TwoReasoning,
    ThreeReasoning,
    Compaction,
}

#[derive(Clone, Copy)]
enum TerminalMessageIdentity {
    Stable,
    Missing,
    Null,
    Unknown,
    Regenerated,
}

#[derive(Clone, Copy)]
struct TerminalOutputIdentityCase {
    prefix: Option<TerminalOutputPrefix>,
    message_identity: TerminalMessageIdentity,
    observed_commentary_terminal_phase_missing: bool,
}

impl TerminalOutputIdentityCase {
    const fn new(
        prefix: Option<TerminalOutputPrefix>,
        message_identity: TerminalMessageIdentity,
    ) -> Self {
        Self {
            prefix,
            message_identity,
            observed_commentary_terminal_phase_missing: false,
        }
    }

    const fn commentary_phase_drift(message_identity: TerminalMessageIdentity) -> Self {
        Self {
            prefix: Some(TerminalOutputPrefix::Reasoning),
            message_identity,
            observed_commentary_terminal_phase_missing: true,
        }
    }
}

#[derive(Clone, Copy)]
enum OutputItemAddedLifecycleFault {
    OutputIndex,
    MissingItem,
    ItemType,
    ItemShape,
    ItemId,
    ItemStatus,
    OutputIdentity,
}

#[derive(Clone, Copy)]
enum UnsealedPrivateKind {
    Reasoning,
    Compaction,
}

#[derive(Clone, Copy)]
enum UsageFixture {
    MissingCached,
    NegativeInput,
    FractionalInput,
    NonObjectDetails,
    NegativeCached,
    CachedExceedsInput,
    OutputWithoutTotal,
}

#[derive(Clone)]
struct ServerState {
    attempts: Arc<AtomicUsize>,
    bodies: mpsc::UnboundedSender<Vec<u8>>,
    replies: Arc<Vec<ServerReply>>,
    dispatch_notify: Option<Arc<Notify>>,
    release_notify: Option<Arc<Notify>>,
}

fn completed_response() -> Response {
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<result value=\\\"al\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"pha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"sequence_number\":4,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"sequence_number\":5,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"sequence_number\":6,\"response\":{\"id\":\"resp_sample_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}]}}\n\n"
        ),
    )
        .into_response()
}

fn completed_with_usage_response() -> Response {
    completed_response_with_usage(serde_json::json!({
        "input_tokens": 321,
        "input_tokens_details": {"cached_tokens": 256},
        "output_tokens": 21,
        "total_tokens": 342,
    }))
}

fn completed_with_inconsistent_usage_response() -> Response {
    completed_response_with_usage(serde_json::json!({
        "input_tokens": 321,
        "input_tokens_details": {"cached_tokens": 256},
        "output_tokens": 21,
        "total_tokens": 999,
        "private_usage_note": "usage-secret-must-not-be-logged",
    }))
}

fn completed_with_usage_fixture_response(case: UsageFixture) -> Response {
    let usage = match case {
        UsageFixture::MissingCached => serde_json::json!({"input_tokens": 321}),
        UsageFixture::NegativeInput => serde_json::json!({"input_tokens": -1}),
        UsageFixture::FractionalInput => serde_json::json!({"input_tokens": 1.5}),
        UsageFixture::NonObjectDetails => serde_json::json!({
            "input_tokens": 321,
            "input_tokens_details": "private-usage-detail",
        }),
        UsageFixture::NegativeCached => serde_json::json!({
            "input_tokens": 321,
            "input_tokens_details": {"cached_tokens": -1},
        }),
        UsageFixture::CachedExceedsInput => serde_json::json!({
            "input_tokens": 321,
            "input_tokens_details": {"cached_tokens": 322},
        }),
        UsageFixture::OutputWithoutTotal => serde_json::json!({
            "input_tokens": 321,
            "output_tokens": 21,
        }),
    };
    completed_response_with_usage(usage)
}

fn completed_response_with_usage(usage: serde_json::Value) -> Response {
    let message_added = serde_json::json!({
        "id": "msg_usage",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_usage",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_usage", "output_index": 0, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "msg_usage", "output_index": 0, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": message_done.clone()}),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 5,
            "response": {
                "id": "resp_usage",
                "status": "completed",
                "output": [message_done],
                "usage": usage,
            },
        }),
    ])
}

fn completed_without_terminal_output_response() -> Response {
    let message_added = serde_json::json!({
        "id": "msg_without_terminal_output",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_without_terminal_output",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "<result value=\"alpha\" />", "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_without_terminal_output", "output_index": 0, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "msg_without_terminal_output", "output_index": 0, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": message_done}),
        serde_json::json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "resp_without_terminal_output", "status": "completed"}}),
    ])
}

fn native_tool_lifecycle_tool_only_response() -> Response {
    native_tool_lifecycle_response(None)
}

fn native_tool_lifecycle_tool_only_with_usage_response() -> Response {
    native_tool_lifecycle_response(Some(serde_json::json!({
        "input_tokens": 144,
        "input_tokens_details": {"cached_tokens": 128},
        "output_tokens": 12,
        "total_tokens": 156,
    })))
}

fn native_tool_lifecycle_response(usage: Option<serde_json::Value>) -> Response {
    let call = serde_json::json!({
        "id": "item_lifecycle_lookup",
        "type": "function_call",
        "status": "completed",
        "call_id": "call_lifecycle_lookup",
        "name": "lifecycle_lookup",
        "arguments": "{}",
    });
    let mut completed_response = serde_json::json!({
        "id": "resp_lifecycle_lookup",
        "status": "completed",
        "output": [call.clone()],
    });
    if let Some(usage) = usage {
        completed_response
            .as_object_mut()
            .expect("native terminal response is an object")
            .insert("usage".to_owned(), usage);
    }
    sse_response(vec![
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 1,
            "response": {"id": "resp_lifecycle_lookup", "status": "in_progress"},
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 2,
            "output_index": 0,
            "item": {
                "id": "item_lifecycle_lookup",
                "type": "function_call",
                "status": "in_progress",
                "call_id": "call_lifecycle_lookup",
                "name": "lifecycle_lookup",
                "arguments": "",
            },
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": 3,
            "item_id": "item_lifecycle_lookup",
            "output_index": 0,
            "delta": "{}",
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 4,
            "item_id": "item_lifecycle_lookup",
            "output_index": 0,
            "arguments": "{}",
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 5,
            "output_index": 0,
            "item": call.clone(),
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 6,
            "response": completed_response,
        }),
    ])
}

fn second_move_completed_response() -> Response {
    let message_added = serde_json::json!({
        "id": "msg_2",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_2",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"gamma\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_2", "output_index": 0, "content_index": 0, "delta": "<result value=\"gamma\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "msg_2", "output_index": 0, "content_index": 0, "text": "<result value=\"gamma\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "resp_sample_2", "status": "completed", "output": [message_done]}}),
    ])
}

fn compaction_response() -> Response {
    let compaction = serde_json::json!({
        "id": "cmp_1",
        "type": "compaction",
        "encrypted_content": "opaque-compaction-secret",
        "created_by": "server-side-compaction",
    });
    let message_added = serde_json::json!({
        "id": "msg_compacted_1",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_compacted_1",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": compaction.clone()}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": compaction.clone()}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_compacted_1", "output_index": 1, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_compacted_1", "output_index": 1, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_compacted_1", "status": "completed", "output": [compaction, message_done]}}),
    ])
}

fn sealed_reasoning_then_eof_response() -> Response {
    let reasoning_added = reasoning_added(None);
    let reasoning_done = reasoning_done(None);
    let message_added = serde_json::json!({
        "id": "msg_after_reasoning",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_after_reasoning",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "<result value=\"alpha\" />", "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_after_reasoning", "output_index": 1, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_after_reasoning", "output_index": 1, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done}),
    ])
}

fn sealed_reasoning_only_then_eof_response() -> Response {
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added(None)}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done(None)}),
    ])
}

fn open_text_then_sealed_reasoning_eof_response() -> Response {
    let message_added = serde_json::json!({
        "id": "msg_open_before_reasoning",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let reasoning_added = serde_json::json!({
        "id": "rs_after_open_text",
        "type": "reasoning",
        "status": "in_progress",
        "summary": [],
    });
    let reasoning_done = serde_json::json!({
        "id": "rs_after_open_text",
        "type": "reasoning",
        "status": "completed",
        "summary": [],
        "encrypted_content": "sealed-after-open-text",
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_open_before_reasoning", "output_index": 0, "content_index": 0, "delta": "visible-open-text-0"}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 1, "item": reasoning_done}),
    ])
}

fn sealed_real_shaped_reasoning_only_then_eof_response() -> Response {
    let reasoning_added = serde_json::json!({
        "id": "rs_eof_four_field",
        "type": "reasoning",
        "status": "in_progress",
        "summary": [],
    });
    let reasoning_done = serde_json::json!({
        "type": "reasoning",
        "id": "rs_eof_four_field",
        "summary": [],
        "encrypted_content": "encrypted-eof-four-field",
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done}),
    ])
}

fn sealed_compaction_then_eof_response() -> Response {
    let compaction = serde_json::json!({
        "id": "cmp_sealed_eof",
        "type": "compaction",
        "encrypted_content": "sealed-compaction-payload",
    });
    let message_added = serde_json::json!({
        "id": "msg_after_compaction",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_after_compaction",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "<result value=\"alpha\" />", "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": compaction.clone()}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": compaction}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_after_compaction", "output_index": 1, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_after_compaction", "output_index": 1, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done}),
    ])
}

fn observational_message_response(added_status: &str, done_status: &str) -> Response {
    let message_snapshot = |status: &str, phase: &str, text: &str| {
        serde_json::json!({
            "id": "msg_observed",
            "type": "message",
            "status": status,
            "role": "assistant",
            "phase": phase,
            "content": [{
                "type": "output_text",
                "text": text,
                "annotations": [],
            }],
        })
    };
    let added = message_snapshot(added_status, "commentary", "added snapshot");
    let done = message_snapshot(done_status, "final_answer", "done snapshot");
    let terminal = message_snapshot("completed", "final_answer", "<result value=\"alpha\" />");
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": done}),
        serde_json::json!({"type": "response.completed", "sequence_number": 3, "response": {"id": "resp_observational_message", "status": "completed", "output": [terminal]}}),
    ])
}

fn observational_reasoning_response() -> Response {
    let reasoning_added = serde_json::json!({
        "type": "reasoning",
        "status": "incomplete",
        "summary": [{"type": "summary_text", "text": "Observed summary."}],
    });
    let reasoning_done = serde_json::json!({
        "type": "reasoning",
        "summary": [],
    });
    let terminal_reasoning = serde_json::json!({
        "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "Authoritative summary."}],
        "encrypted_content": "authoritative-encrypted-reasoning",
    });
    let message_added = serde_json::json!({
        "id": "msg_reasoning_observed",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_reasoning_observed",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "delta": "<result value=\"alpha\" />", "item_id": "msg_reasoning_observed", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "text": "<result value=\"alpha\" />", "item_id": "msg_reasoning_observed", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_observational_reasoning", "status": "completed", "output": [terminal_reasoning, message_done]}}),
    ])
}

fn observational_compaction_response() -> Response {
    let compaction = |encrypted_content: &str| {
        serde_json::json!({
            "id": "cmp_observed",
            "type": "compaction",
            "encrypted_content": encrypted_content,
            "created_by": "server-side-compaction",
        })
    };
    let terminal_message = serde_json::json!({
        "id": "msg_terminal_only",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": compaction("added-compaction-snapshot")}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": compaction("done-compaction-snapshot")}),
        serde_json::json!({"type": "response.completed", "sequence_number": 3, "response": {"id": "resp_observational_compaction", "status": "completed", "output": [compaction("authoritative-compaction"), terminal_message]}}),
    ])
}

fn unsealed_private_at_terminal_response(kind: UnsealedPrivateKind) -> Response {
    let (private, response_id) = match kind {
        UnsealedPrivateKind::Reasoning => (
            serde_json::json!({
                "id": "unsealed-reasoning-private-sentinel",
                "type": "reasoning",
                "status": "in_progress",
                "summary": [],
            }),
            "resp_unsealed_reasoning",
        ),
        UnsealedPrivateKind::Compaction => (
            serde_json::json!({
                "id": "unsealed-compaction-private-sentinel",
                "type": "compaction",
                "encrypted_content": "unsealed-compaction-content-sentinel",
            }),
            "resp_unsealed_compaction",
        ),
    };
    let message_added = serde_json::json!({
        "id": "msg_after_unsealed_private",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_after_unsealed_private",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": private}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 3, "item_id": "msg_after_unsealed_private", "output_index": 1, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 4, "item_id": "msg_after_unsealed_private", "output_index": 1, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 5, "output_index": 1, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 6, "response": {"id": response_id, "status": "completed", "output": [message_done]}}),
    ])
}

fn placeholder_completed_response() -> Response {
    let text = "output-a";
    let message_added = serde_json::json!({
        "id": "item-placeholder-a",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "item-placeholder-a",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "item-placeholder-a", "output_index": 0, "content_index": 0, "delta": text}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "item-placeholder-a", "output_index": 0, "content_index": 0, "text": text}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "response-placeholder-a", "status": "completed", "output": [message_done]}}),
    ])
}

fn terminal_output_identity_response(case: TerminalOutputIdentityCase) -> Response {
    let text = "output-b";
    let reasoning_prefix = |id: &str| {
        (
            serde_json::json!({
                "id": id,
                "type": "reasoning",
                "status": "in_progress",
                "summary": [],
            }),
            serde_json::json!({
                "id": id,
                "type": "reasoning",
                "status": "completed",
                "summary": [],
                "encrypted_content": "encrypted-terminal-prefix",
            }),
        )
    };
    let (prefixes, message_output_index) = match case.prefix {
        Some(TerminalOutputPrefix::Reasoning) => (vec![reasoning_prefix("item-prefix")], 1),
        Some(TerminalOutputPrefix::TwoReasoning) => (
            vec![
                reasoning_prefix("item-prefix"),
                reasoning_prefix("item-intermediate"),
            ],
            2,
        ),
        Some(TerminalOutputPrefix::ThreeReasoning) => (
            vec![
                reasoning_prefix("item-prefix"),
                reasoning_prefix("item-intermediate"),
                reasoning_prefix("item-final-prefix"),
            ],
            3,
        ),
        Some(TerminalOutputPrefix::Compaction) => {
            let compaction = serde_json::json!({
                "id": "item-prefix",
                "type": "compaction",
                "encrypted_content": "opaque-placeholder",
            });
            (vec![(compaction.clone(), compaction)], 1)
        }
        None => (Vec::new(), 0),
    };
    let mut message_added = serde_json::json!({
        "id": "item-streamed-b",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let mut message_done = serde_json::json!({
        "id": "item-streamed-b",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    });
    if case.observed_commentary_terminal_phase_missing {
        message_added["phase"] = serde_json::json!("commentary");
        message_done["phase"] = serde_json::json!("commentary");
    }
    let mut terminal_message = message_done.clone();
    if case.observed_commentary_terminal_phase_missing {
        terminal_message
            .as_object_mut()
            .expect("terminal message fixture is an object")
            .remove("phase");
    }
    match case.message_identity {
        TerminalMessageIdentity::Stable => {}
        TerminalMessageIdentity::Missing => {
            terminal_message
                .as_object_mut()
                .expect("terminal message fixture is an object")
                .remove("id");
        }
        TerminalMessageIdentity::Null => {
            terminal_message["id"] = serde_json::Value::Null;
        }
        TerminalMessageIdentity::Unknown => {
            terminal_message["id"] = serde_json::json!("item-terminal-unknown");
        }
        TerminalMessageIdentity::Regenerated => {
            terminal_message["id"] = serde_json::json!("item-terminal-regenerated");
        }
    }

    let terminal_prefixes = prefixes
        .iter()
        .map(|(_, done)| done.clone())
        .collect::<Vec<_>>();
    let mut events = Vec::new();
    for (output_index, (added, done)) in prefixes.into_iter().enumerate() {
        let sequence_number = output_index as u64 * 2 + 1;
        events.push(serde_json::json!({"type": "response.output_item.added", "sequence_number": sequence_number, "output_index": output_index, "item": added}));
        events.push(serde_json::json!({"type": "response.output_item.done", "sequence_number": sequence_number + 1, "output_index": output_index, "item": done}));
    }
    let sequence_offset = message_output_index * 2;
    events.extend([
        serde_json::json!({"type": "response.output_item.added", "sequence_number": sequence_offset + 1, "output_index": message_output_index, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": sequence_offset + 2, "item_id": "item-streamed-b", "output_index": message_output_index, "content_index": 0, "delta": text}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": sequence_offset + 3, "item_id": "item-streamed-b", "output_index": message_output_index, "content_index": 0, "text": text}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": sequence_offset + 4, "output_index": message_output_index, "item": message_done}),
        serde_json::json!({"type": "response.completed", "sequence_number": sequence_offset + 5, "response": {"id": "response-placeholder-b", "status": "completed", "output": terminal_prefixes.into_iter().chain(std::iter::once(terminal_message)).collect::<Vec<_>>()}}),
    ]);
    sse_response(events)
}

fn terminal_message_compatibility_response(case: TerminalMessageCompatibilityCase) -> Response {
    let text = "<result value=\"alpha\" />";
    let mut message_added = serde_json::json!({
        "id": "msg_terminal_compatibility",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let mut message_done = serde_json::json!({
        "id": "msg_terminal_compatibility",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": [],
        }],
    });
    let mut terminal_message = message_done.clone();
    let terminal = terminal_message
        .as_object_mut()
        .expect("terminal compatibility message is an object");
    match case {
        TerminalMessageCompatibilityCase::MissingId => {
            terminal.remove("id");
        }
        TerminalMessageCompatibilityCase::NullId => {
            terminal.insert("id".to_owned(), serde_json::Value::Null);
        }
        TerminalMessageCompatibilityCase::MissingStatus => {
            terminal.remove("status");
        }
        TerminalMessageCompatibilityCase::NullStatus => {
            terminal.insert("status".to_owned(), serde_json::Value::Null);
        }
        TerminalMessageCompatibilityCase::MissingRole => {
            terminal.remove("role");
        }
        TerminalMessageCompatibilityCase::NullRole => {
            terminal.insert("role".to_owned(), serde_json::Value::Null);
        }
        TerminalMessageCompatibilityCase::MissingAnnotations => {
            terminal_message["content"][0]
                .as_object_mut()
                .expect("terminal output_text is an object")
                .remove("annotations");
        }
        TerminalMessageCompatibilityCase::NullAnnotations => {
            terminal_message["content"][0]["annotations"] = serde_json::Value::Null;
        }
        TerminalMessageCompatibilityCase::UnknownExtensions => {
            terminal.insert(
                "future_message_metadata".to_owned(),
                serde_json::json!({"ordinary": true}),
            );
            terminal_message["content"][0]["future_content_metadata"] =
                serde_json::json!({"ordinary": true});
        }
        TerminalMessageCompatibilityCase::MissingTerminalPhaseAfterExplicitFinalAnswer => {
            message_added["phase"] = serde_json::json!("final_answer");
            message_done["phase"] = serde_json::json!("final_answer");
            terminal.remove("id");
            terminal.remove("status");
        }
        TerminalMessageCompatibilityCase::NullTerminalPhaseAfterExplicitFinalAnswer => {
            message_added["phase"] = serde_json::json!("final_answer");
            message_done["phase"] = serde_json::json!("final_answer");
            terminal.insert("phase".to_owned(), serde_json::Value::Null);
        }
        TerminalMessageCompatibilityCase::ExplicitFinalAnswerAfterMissingLifecyclePhase => {
            terminal.insert("phase".to_owned(), serde_json::json!("final_answer"));
        }
        TerminalMessageCompatibilityCase::ExplicitFinalAnswerAfterNullLifecyclePhase => {
            message_added["phase"] = serde_json::Value::Null;
            message_done["phase"] = serde_json::Value::Null;
            terminal.insert("phase".to_owned(), serde_json::json!("final_answer"));
        }
    }

    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_terminal_compatibility", "output_index": 0, "content_index": 0, "delta": text}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "msg_terminal_compatibility", "output_index": 0, "content_index": 0, "text": text}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": message_done}),
        serde_json::json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "resp_terminal_compatibility", "status": "completed", "output": [terminal_message]}}),
    ])
}

fn terminal_message_diagnostic_response(case: TerminalMessageDiagnosticCase) -> Response {
    let observed_text = "observed-placeholder";
    if matches!(case, TerminalMessageDiagnosticCase::OrphanTextMismatch) {
        let observed_message = serde_json::json!({
            "id": "observed-item-placeholder",
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": [],
        });
        let terminal_message = serde_json::json!({
            "id": "terminal-item-placeholder",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "terminal-placeholder",
                "annotations": [],
            }],
        });
        return sse_response(vec![
            serde_json::json!({"type": "response.created", "sequence_number": 1, "response": {"id": "response-placeholder", "status": "in_progress"}}),
            serde_json::json!({"type": "response.in_progress", "sequence_number": 2, "response": {"id": "response-placeholder", "status": "in_progress"}}),
            serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": observed_message}),
            serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "observed-item-placeholder", "output_index": 1, "content_index": 0, "delta": observed_text}),
            serde_json::json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "response-placeholder", "status": "completed", "output": [terminal_message]}}),
        ]);
    }
    let terminal_text = match case {
        TerminalMessageDiagnosticCase::TextMismatch => "terminal-placeholder",
        TerminalMessageDiagnosticCase::InvalidShape
        | TerminalMessageDiagnosticCase::PhaseMismatch => observed_text,
        TerminalMessageDiagnosticCase::OrphanTextMismatch => unreachable!(),
    };
    let mut message_added = serde_json::json!({
        "id": "item-placeholder",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let mut message_done = serde_json::json!({
        "id": "item-placeholder",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": observed_text,
            "annotations": [],
        }],
    });
    let mut terminal_message = serde_json::json!({
        "id": "item-placeholder",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": terminal_text,
            "annotations": [],
        }],
    });
    match case {
        TerminalMessageDiagnosticCase::InvalidShape => {
            terminal_message
                .as_object_mut()
                .expect("terminal diagnostic fixture is an object")
                .remove("content");
        }
        TerminalMessageDiagnosticCase::TextMismatch => {}
        TerminalMessageDiagnosticCase::OrphanTextMismatch => unreachable!(),
        TerminalMessageDiagnosticCase::PhaseMismatch => {
            message_added["phase"] = serde_json::json!("commentary");
            message_done["phase"] = serde_json::json!("commentary");
            terminal_message["phase"] = serde_json::json!("final_answer");
        }
    }
    let empty_part = serde_json::json!({
        "type": "output_text",
        "text": "",
        "annotations": [],
    });
    let completed_part = serde_json::json!({
        "type": "output_text",
        "text": observed_text,
        "annotations": [],
    });

    sse_response(vec![
        serde_json::json!({"type": "response.created", "sequence_number": 1, "response": {"id": "response-placeholder", "status": "in_progress"}}),
        serde_json::json!({"type": "response.in_progress", "sequence_number": 2, "response": {"id": "response-placeholder", "status": "in_progress"}}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.content_part.added", "sequence_number": 4, "item_id": "item-placeholder", "output_index": 0, "content_index": 0, "part": empty_part}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 5, "item_id": "item-placeholder", "output_index": 0, "content_index": 0, "delta": observed_text}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 6, "item_id": "item-placeholder", "output_index": 0, "content_index": 0, "text": observed_text}),
        serde_json::json!({"type": "response.content_part.done", "sequence_number": 7, "item_id": "item-placeholder", "output_index": 0, "content_index": 0, "part": completed_part}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 8, "output_index": 0, "item": message_done}),
        serde_json::json!({"type": "response.completed", "sequence_number": 9, "response": {"id": "response-placeholder", "status": "completed", "output": [terminal_message]}}),
    ])
}

fn sse_response(events: Vec<serde_json::Value>) -> Response {
    let body = events
        .into_iter()
        .map(|event| format!("data: {}\n\n", serde_json::to_string(&event).unwrap()))
        .collect::<String>();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn output_item_added_lifecycle_fault_response(fault: OutputItemAddedLifecycleFault) -> Response {
    let valid_item = serde_json::json!({
        "id": "msg_synthetic",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let event = |sequence_number, output_index, item| {
        let mut event = serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
        });
        let object = event.as_object_mut().expect("synthetic event object");
        if let Some(output_index) = output_index {
            object.insert("output_index".to_owned(), output_index);
        }
        if let Some(item) = item {
            object.insert("item".to_owned(), item);
        }
        event
    };

    let events = match fault {
        OutputItemAddedLifecycleFault::OutputIndex => {
            vec![event(1, None, Some(valid_item))]
        }
        OutputItemAddedLifecycleFault::MissingItem => {
            vec![event(1, Some(serde_json::json!(0)), None)]
        }
        OutputItemAddedLifecycleFault::ItemType => vec![event(
            1,
            Some(serde_json::json!(0)),
            Some(serde_json::json!({
                "id": "msg_synthetic",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            })),
        )],
        OutputItemAddedLifecycleFault::ItemShape => vec![event(
            1,
            Some(serde_json::json!(0)),
            Some(serde_json::json!({
                "id": "msg_synthetic",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": "invalid",
            })),
        )],
        OutputItemAddedLifecycleFault::ItemId => vec![event(
            1,
            Some(serde_json::json!(0)),
            Some(serde_json::json!({
                "id": "",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            })),
        )],
        OutputItemAddedLifecycleFault::ItemStatus => vec![event(
            1,
            Some(serde_json::json!(0)),
            Some(serde_json::json!({
                "id": "cmp_synthetic",
                "type": "compaction",
                "encrypted_content": "",
                "created_by": "server-side-compaction",
            })),
        )],
        OutputItemAddedLifecycleFault::OutputIdentity => vec![
            event(1, Some(serde_json::json!(0)), Some(valid_item.clone())),
            event(2, Some(serde_json::json!(1)), Some(valid_item)),
        ],
    };
    sse_response(events)
}

fn output_annotation_lifecycle_response(
    annotation_event: serde_json::Value,
    annotation: serde_json::Value,
) -> Response {
    let message_added = serde_json::json!({
        "id": "msg_annotated",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_annotated",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [annotation],
        }],
    });
    let content_done = message_done["content"][0].clone();
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.content_part.added", "sequence_number": 2, "item_id": "msg_annotated", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 3, "item_id": "msg_annotated", "output_index": 0, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        annotation_event,
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_annotated", "output_index": 0, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.content_part.done", "sequence_number": 6, "item_id": "msg_annotated", "output_index": 0, "content_index": 0, "part": content_done}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 7, "output_index": 0, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 8, "response": {"id": "resp_annotated", "status": "completed", "output": [message_done]}}),
    ])
}

fn reasoning_response(
    added_content: Option<serde_json::Value>,
    done_content: Option<serde_json::Value>,
    completed_content: Option<serde_json::Value>,
) -> Response {
    let reasoning_added = reasoning_added(added_content);
    let reasoning_done_item = reasoning_done(done_content);
    let reasoning_completed = reasoning_done(completed_content);
    let message_added = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done_item}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "delta": "<result value=\"alpha\" />", "item_id": "msg_1", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "text": "<result value=\"alpha\" />", "item_id": "msg_1", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_reasoning", "status": "completed", "output": [reasoning_completed, message_done]}}),
    ])
}

fn reasoning_response_with_unknown_extensions() -> Response {
    const SENTINEL: &str = "unknown-reasoning-retention-secret-sentinel";
    let reasoning_added = reasoning_added(None);
    let mut reasoning_done_item = reasoning_done(None);
    let mut reasoning_completed = reasoning_done(None);
    for reasoning in [&mut reasoning_done_item, &mut reasoning_completed] {
        let object = reasoning
            .as_object_mut()
            .expect("reasoning extension fixture is an object");
        object.insert(
            "future_reasoning_text".to_owned(),
            serde_json::json!(SENTINEL),
        );
        object.insert(
            "future_metadata".to_owned(),
            serde_json::json!({"plaintext_reasoning": SENTINEL}),
        );
    }
    let message_added = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done_item}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "delta": "<result value=\"alpha\" />", "item_id": "msg_1", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "text": "<result value=\"alpha\" />", "item_id": "msg_1", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_reasoning_extensions", "status": "completed", "output": [reasoning_completed, message_done]}}),
    ])
}

fn reasoning_added(content: Option<serde_json::Value>) -> serde_json::Value {
    content.map_or_else(
        || {
            serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "status": "in_progress",
                "summary": [],
            })
        },
        |content| {
            serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "status": "in_progress",
                "summary": [],
                "content": content,
            })
        },
    )
}

fn reasoning_done(content: Option<serde_json::Value>) -> serde_json::Value {
    content.map_or_else(
        || {
            serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "status": "completed",
                "summary": [{"type": "summary_text", "text": "Inspect the result constraints."}],
                "encrypted_content": "encrypted-reasoning-1",
            })
        },
        |content| {
            serde_json::json!({
                "id": "rs_1",
                "type": "reasoning",
                "status": "completed",
                "summary": [{"type": "summary_text", "text": "Inspect the result constraints."}],
                "content": content,
                "encrypted_content": "encrypted-reasoning-1",
            })
        },
    )
}

fn sealed_reasoning_omitted_from_completed_response() -> Response {
    let reasoning_added = serde_json::json!({
        "id": "rs_live_shape",
        "type": "reasoning",
        "status": "in_progress",
        "summary": [],
    });
    // This is the retained wire shape observed across the supplied Codex
    // request sample. It deliberately omits both status and content.
    let reasoning_done = serde_json::json!({
        "id": "rs_live_shape",
        "type": "reasoning",
        "summary": [],
        "encrypted_content": "encrypted-live-shape",
    });
    let message_added = serde_json::json!({
        "id": "msg_after_reasoning",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_after_reasoning",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "<result value=\"alpha\" />", "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_after_reasoning", "output_index": 1, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_after_reasoning", "output_index": 1, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done.clone()}),
        // The terminal summary is reconciliation-only. It intentionally omits
        // the private item that was already sealed at output_item.done.
        serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_live_shape", "status": "completed", "output": [message_done]}}),
    ])
}

fn sealed_reasoning_empty_terminal_response() -> Response {
    let reasoning_added = serde_json::json!({
        "id": "rs_empty_terminal",
        "type": "reasoning",
        "status": "in_progress",
        "summary": [],
    });
    let reasoning_done = serde_json::json!({
        "id": "rs_empty_terminal",
        "type": "reasoning",
        "summary": [],
        "encrypted_content": "encrypted-empty-terminal",
    });
    let message_added = serde_json::json!({
        "id": "msg_empty_terminal",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_empty_terminal",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "<result value=\"alpha\" />", "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": reasoning_done}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_empty_terminal", "output_index": 1, "content_index": 0, "delta": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_empty_terminal", "output_index": 1, "content_index": 0, "text": "<result value=\"alpha\" />"}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 1, "item": message_done}),
        serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_empty_terminal", "status": "completed", "output": []}}),
    ])
}

fn plaintext_reasoning_content() -> serde_json::Value {
    serde_json::json!([{
        "type": "reasoning_text",
        "text": "plaintext-reasoning-secret",
    }])
}

fn mixed_malformed_reasoning_content() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "reasoning_text",
            "text": "plaintext-reasoning-mixed-secret",
        },
        {
            "type": "reasoning_text",
            "text": {"malformed": "mixed-malformed-reasoning-secret"},
        },
    ])
}

fn reasoning_text_stream_response(
    reasoning_event: serde_json::Value,
    terminal_content: Option<serde_json::Value>,
) -> Response {
    let _: ResponseStreamEvent = serde_json::from_value(reasoning_event.clone())
        .expect("reasoning text fixture must match the pinned async-openai schema");
    let reasoning_added = reasoning_added(None);
    let reasoning_done_item = reasoning_done(terminal_content.clone());
    let reasoning_completed = reasoning_done(terminal_content);
    let message_added = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "<result value=\"alpha\" />",
            "annotations": [],
        }],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": reasoning_added}),
        reasoning_event,
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0, "item": reasoning_done_item}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 4, "output_index": 1, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 5, "delta": "<result value=\"alpha\" />", "item_id": "msg_1", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 6, "text": "<result value=\"alpha\" />", "item_id": "msg_1", "output_index": 1, "content_index": 0}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 7, "output_index": 1, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 8, "response": {"id": "resp_reasoning_text", "status": "completed", "output": [reasoning_completed, message_done]}}),
    ])
}

async fn responses_endpoint(State(state): State<ServerState>, body: Bytes) -> Response {
    let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
    state.bodies.send(body.to_vec()).unwrap();

    let reply = state
        .replies
        .get(attempt)
        .or_else(|| state.replies.last())
        .copied()
        .expect("test server has at least one reply");

    match reply {
        ServerReply::Completed => completed_response(),
        ServerReply::CompletedWithUsage => completed_with_usage_response(),
        ServerReply::CompletedWithInconsistentUsage => {
            completed_with_inconsistent_usage_response()
        }
        ServerReply::CompletedWithUsageFixture(case) => {
            completed_with_usage_fixture_response(case)
        }
        ServerReply::CompletedWithoutTerminalOutput => completed_without_terminal_output_response(),
        ServerReply::NativeToolLifecycleToolOnly => native_tool_lifecycle_tool_only_response(),
        ServerReply::NativeToolLifecycleToolOnlyWithUsage => {
            native_tool_lifecycle_tool_only_with_usage_response()
        }
        ServerReply::PlaceholderCompleted => placeholder_completed_response(),
        ServerReply::CompletedSecondMove => second_move_completed_response(),
        ServerReply::CompactionWithText => compaction_response(),
        ServerReply::SealedReasoningThenEof => sealed_reasoning_then_eof_response(),
        ServerReply::SealedReasoningOnlyThenEof => sealed_reasoning_only_then_eof_response(),
        ServerReply::OpenTextThenSealedReasoningEof => {
            open_text_then_sealed_reasoning_eof_response()
        }
        ServerReply::SealedRealShapedReasoningOnlyThenEof => {
            sealed_real_shaped_reasoning_only_then_eof_response()
        }
        ServerReply::SealedReasoningOmittedFromCompleted => {
            sealed_reasoning_omitted_from_completed_response()
        }
        ServerReply::SealedReasoningEmptyTerminal => sealed_reasoning_empty_terminal_response(),
        ServerReply::SealedCompactionThenEof => sealed_compaction_then_eof_response(),
        ServerReply::ObservationalMessageCompletedThenIncomplete => {
            observational_message_response("completed", "incomplete")
        }
        ServerReply::ObservationalMessageIncompleteThenCompleted => {
            observational_message_response("incomplete", "completed")
        }
        ServerReply::ObservationalReasoning => observational_reasoning_response(),
        ServerReply::ObservationalCompaction => observational_compaction_response(),
        ServerReply::TerminalOutputIdentity(case) => terminal_output_identity_response(case),
        ServerReply::TerminalMessageCompatibility(case) => {
            terminal_message_compatibility_response(case)
        }
        ServerReply::TerminalMessageDiagnostic(case) => {
            terminal_message_diagnostic_response(case)
        }
        ServerReply::WrongContentType => {
            let mut response = completed_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/plain"),
            );
            response
        }
        ServerReply::DelayedHeaders => {
            tokio::time::sleep(Duration::from_millis(250)).await;
            completed_response()
        }
        ServerReply::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "authentication-secret-must-not-be-logged",
        )
            .into_response(),
        ServerReply::Forbidden => (
            StatusCode::FORBIDDEN,
            "authorization-secret-must-not-be-logged",
        )
            .into_response(),
        ServerReply::TooManyRequests => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate-limit-secret-must-not-be-logged",
        )
            .into_response(),
        ServerReply::ServiceUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream-secret-must-not-be-logged",
        )
            .into_response(),
        ServerReply::RequestTimeout => (
            StatusCode::REQUEST_TIMEOUT,
            "request-timeout-secret-must-not-be-logged",
        )
            .into_response(),
        ServerReply::Conflict => (
            StatusCode::CONFLICT,
            "conflict-secret-must-not-be-logged",
        )
            .into_response(),
        ServerReply::MissingCompleted => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.output_item.added\n",
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<result value=\\\"al\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"pha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "event: response.output_text.done\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":4,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "event: response.output_item.done\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":5,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}}\n\n"
            ),
        )
            .into_response(),
        ServerReply::StalledStream => {
            let stream = futures::stream::unfold(false, |sent| async move {
                if sent {
                    futures::future::pending::<Option<(Result<Bytes, Infallible>, bool)>>().await
                } else {
                    Some((
                        Ok(Bytes::from_static(
                            b"data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"stalled-stream-secret\",\"status\":\"in_progress\"}}\n\n",
                        )),
                        true,
                    ))
                }
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        ServerReply::OversizedEvent => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,",
                "\"delta\":\"oversized-event-secret-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\",",
                "\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::OversizedDoneEvent => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: oversized-done-secret-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
                "data: [DONE]\n\n"
            ),
        )
            .into_response(),
        ServerReply::OversizedWireComment => {
            let stream = futures::stream::once(async {
                Ok::<_, Infallible>(Bytes::from_static(
                    b": oversized-wire-comment-secret-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
                ))
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        ServerReply::ChunkedOversizedBody => {
            let stream = futures::stream::iter([
                Ok::<_, Infallible>(Bytes::from_static(
                    b": first-body-chunk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
                )),
                Ok::<_, Infallible>(Bytes::from_static(
                    b": second-body-chunk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
                )),
            ]);
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        ServerReply::ActiveUnterminatedStream => {
            let stream = futures::stream::unfold(1_u64, |sequence| async move {
                tokio::time::sleep(Duration::from_millis(15)).await;
                let frame = format!(
                    "data: {{\"type\":\"response.heartbeat\",\"sequence_number\":{sequence}}}\n\n"
                );
                Some((Ok::<_, Infallible>(Bytes::from(frame)), sequence + 1))
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        ServerReply::OutputOverflow => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"12345678\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"output-overflow-secret\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::OutputLimitExact => output_limit_exact_response(),
        ServerReply::OutputLimitPlusOne => output_limit_plus_one_response(),
        ServerReply::StandardOutputLifecycle => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"<result value=\\\"al\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":4,\"delta\":\"pha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":5,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.content_part.done\",\"sequence_number\":6,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":7,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":8,\"response\":{\"id\":\"resp_standard_lifecycle\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}]}}\n\n"
            ),
        )
            .into_response(),
        ServerReply::StandardOutputAnnotationLifecycle => {
            let annotation = serde_json::json!({
                "type": "url_citation",
                "start_index": 0,
                "end_index": 21,
                "title": "Synthetic citation",
                "url": "https://example.invalid/citation",
                "future_metadata": "annotation-canonicalization-secret",
            });
            let annotation_event = serde_json::json!({
                "type": "response.output_text.annotation.added",
                "sequence_number": 4,
                "item_id": "msg_annotated",
                "output_index": 0,
                "content_index": 0,
                "annotation_index": 0,
                "annotation": annotation.clone(),
            });
            let _: ResponseStreamEvent = serde_json::from_value(annotation_event.clone())
                .expect("annotation fixture must match the pinned async-openai schema");
            output_annotation_lifecycle_response(annotation_event, annotation)
        }
        ServerReply::OutputTextAnnotationWithNullIndex => {
            let annotation = serde_json::json!({
                "type": "url_citation",
                "start_index": 0,
                "end_index": 21,
                "title": "Null index citation",
                "url": "https://example.invalid/null-index-secret",
            });
            let annotation_event = serde_json::json!({
                "type": "response.output_text.annotation.added",
                "sequence_number": 4,
                "item_id": "msg_annotated",
                "output_index": 0,
                "content_index": 0,
                "annotation_index": null,
                "annotation": annotation.clone(),
            });
            assert!(
                serde_json::from_value::<ResponseStreamEvent>(annotation_event.clone()).is_err()
            );
            output_annotation_lifecycle_response(annotation_event, annotation)
        }
        ServerReply::ReasoningWithText => reasoning_response(None, None, None),
        ServerReply::ReasoningWithEmptyContent => {
            let empty = serde_json::json!([]);
            reasoning_response(Some(empty.clone()), Some(empty.clone()), Some(empty))
        }
        ServerReply::ReasoningWithOmittedDoneEmptyCompletedContent => {
            reasoning_response(None, None, Some(serde_json::json!([])))
        }
        ServerReply::ReasoningWithEmptyDoneOmittedCompletedContent => {
            reasoning_response(None, Some(serde_json::json!([])), None)
        }
        ServerReply::ReasoningWithUnknownExtensions => {
            reasoning_response_with_unknown_extensions()
        }
        ServerReply::ReasoningWithNullAddedContent => {
            reasoning_response(Some(serde_json::Value::Null), None, None)
        }
        ServerReply::ReasoningWithNullDoneContent => {
            reasoning_response(None, Some(serde_json::Value::Null), None)
        }
        ServerReply::ReasoningWithNullCompletedContent => {
            reasoning_response(None, None, Some(serde_json::Value::Null))
        }
        ServerReply::ReasoningWithPlaintextAddedContent => reasoning_response(
            Some(plaintext_reasoning_content()),
            Some(serde_json::json!([])),
            Some(serde_json::json!([])),
        ),
        ServerReply::ReasoningWithPlaintextDoneContent => reasoning_response(
            Some(serde_json::json!([])),
            Some(plaintext_reasoning_content()),
            Some(serde_json::json!([])),
        ),
        ServerReply::ReasoningWithPlaintextCompletedContent => reasoning_response(
            Some(serde_json::json!([])),
            Some(serde_json::json!([])),
            Some(plaintext_reasoning_content()),
        ),
        ServerReply::ReasoningTextDeltaWithOmittedTerminalContent => {
            reasoning_text_stream_response(
                serde_json::json!({
                    "type": "response.reasoning_text.delta",
                    "sequence_number": 2,
                    "item_id": "rs_1",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": "plaintext-reasoning-delta-secret",
                }),
                None,
            )
        }
        ServerReply::ReasoningTextDoneWithEmptyTerminalContent => reasoning_text_stream_response(
            serde_json::json!({
                "type": "response.reasoning_text.done",
                "sequence_number": 2,
                "item_id": "rs_1",
                "output_index": 0,
                "content_index": 0,
                "text": "plaintext-reasoning-done-secret",
            }),
            Some(serde_json::json!([])),
        ),
        ServerReply::ReasoningWithMixedMalformedAddedContent => reasoning_response(
            Some(mixed_malformed_reasoning_content()),
            Some(serde_json::json!([])),
            Some(serde_json::json!([])),
        ),
        ServerReply::ReasoningWithMixedMalformedDoneContent => reasoning_response(
            Some(serde_json::json!([])),
            Some(mixed_malformed_reasoning_content()),
            Some(serde_json::json!([])),
        ),
        ServerReply::ReasoningWithMixedMalformedCompletedContent => reasoning_response(
            Some(serde_json::json!([])),
            Some(serde_json::json!([])),
            Some(mixed_malformed_reasoning_content()),
        ),
        ServerReply::PhasedMultilineText => {
            let commentary = "Thinking\ncarefully.";
            let final_text = "<result\nvalue=\"alpha\" />";
            let commentary_added = serde_json::json!({
                "id": "msg_commentary",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "phase": "commentary",
                "content": [],
            });
            let commentary_done = serde_json::json!({
                "id": "msg_commentary",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "phase": "commentary",
                "content": [{
                    "type": "output_text",
                    "text": commentary,
                    "annotations": [],
                }],
            });
            let final_added = serde_json::json!({
                "id": "msg_final",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "phase": "final_answer",
                "content": [],
            });
            let final_done = serde_json::json!({
                "id": "msg_final",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{
                    "type": "output_text",
                    "text": final_text,
                    "annotations": [],
                }],
            });
            sse_response(vec![
                serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": commentary_added}),
                serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_commentary", "output_index": 0, "content_index": 0, "delta": commentary}),
                serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "msg_commentary", "output_index": 0, "content_index": 0, "text": commentary}),
                serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": commentary_done.clone()}),
                serde_json::json!({"type": "response.output_item.added", "sequence_number": 5, "output_index": 1, "item": final_added}),
                serde_json::json!({"type": "response.output_text.delta", "sequence_number": 6, "item_id": "msg_final", "output_index": 1, "content_index": 0, "delta": final_text}),
                serde_json::json!({"type": "response.output_text.done", "sequence_number": 7, "item_id": "msg_final", "output_index": 1, "content_index": 0, "text": final_text}),
                serde_json::json!({"type": "response.output_item.done", "sequence_number": 8, "output_index": 1, "item": final_done.clone()}),
                serde_json::json!({"type": "response.completed", "sequence_number": 9, "response": {"id": "resp_phased", "status": "completed", "output": [commentary_done, final_done]}}),
            ])
        }
        ServerReply::OutputItemIdentityDrift => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_original\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_original\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_original\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_changed\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_identity_drift\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_changed\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}]}}\n\n"
            ),
        )
            .into_response(),
        ServerReply::ResponseIdentityDrift => {
            let text = "<result value=\"alpha\" />";
            let added = serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            });
            let done = serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            });
            sse_response(vec![
                serde_json::json!({"type": "response.created", "sequence_number": 1, "response": {"id": "resp_original", "status": "in_progress"}}),
                serde_json::json!({"type": "response.in_progress", "sequence_number": 2, "response": {"id": "resp_original", "status": "in_progress"}}),
                serde_json::json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 0, "item": added}),
                serde_json::json!({"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": text}),
                serde_json::json!({"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": text}),
                serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 0, "item": done.clone()}),
                serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_changed", "status": "completed", "output": [done]}}),
            ])
        }
        ServerReply::ContentPartDrift => {
            let text = "<result value=\"alpha\" />";
            let added = serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            });
            let done = serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            });
            sse_response(vec![
                serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": added}),
                serde_json::json!({"type": "response.content_part.added", "sequence_number": 2, "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
                serde_json::json!({"type": "response.output_text.delta", "sequence_number": 3, "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": text}),
                serde_json::json!({"type": "response.output_text.done", "sequence_number": 4, "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": text}),
                serde_json::json!({"type": "response.content_part.done", "sequence_number": 5, "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "changed", "annotations": []}}),
                serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 0, "item": done.clone()}),
                serde_json::json!({"type": "response.completed", "sequence_number": 7, "response": {"id": "resp_content_drift", "status": "completed", "output": [done]}}),
            ])
        }
        ServerReply::UnknownOutputEvent => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.output_unknown-secret-sentinel\",\"sequence_number\":1}\n\n",
        )
            .into_response(),
        ServerReply::WaitForDispatch => {
            let notify = state.dispatch_notify.expect("dispatch notification");
            let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(3);
            tokio::spawn(async move {
                tx.send(Ok(Bytes::from_static(
                    concat!(
                        "event: response.output_item.added\n",
                        "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                        "event: response.output_text.delta\n",
                        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
                    )
                    .as_bytes(),
                )))
                .await
                .unwrap();
                tokio::time::timeout(Duration::from_secs(2), notify.notified())
                    .await
                    .expect("Component handler did not receive delta before terminal event");
                tx.send(Ok(Bytes::from_static(
                    concat!(
                        "event: response.output_text.done\n",
                        "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                        "event: response.output_item.done\n",
                        "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}}\n\n",
                        "event: response.completed\n",
                        "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_probe_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}]}}\n\n"
                    )
                    .as_bytes(),
                )))
                .await
                .unwrap();
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        ServerReply::WaitAfterTextDone => {
            let done_notify = state.dispatch_notify.expect("text-done notification");
            let release_notify = state.release_notify.expect("completion release notification");
            let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(3);
            tokio::spawn(async move {
                tx.send(Ok(Bytes::from_static(
                    concat!(
                        "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                        "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
                    )
                    .as_bytes(),
                )))
                .await
                .unwrap();
                done_notify.notify_one();
                release_notify.notified().await;
                tx.send(Ok(Bytes::from_static(
                    concat!(
                        "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}}\n\n",
                        "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_delayed_complete\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}]}}\n\n"
                    )
                        .as_bytes(),
                )))
                .await
                .unwrap();
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
        ServerReply::MalformedSse => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.output_text.delta\n",
                "data: malformed-sse-secret-sentinel\n\n"
            ),
        )
            .into_response(),
        ServerReply::EventMissingType => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"sequence_number\":1,\"detail\":\"event-shape-secret-sentinel\"}\n\n",
        )
            .into_response(),
        ServerReply::EventMissingTypeAfterTerminal => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}]}}\n\n",
                "data: {\"sequence_number\":6,\"detail\":\"event-shape-secret-sentinel\"}\n\n"
            ),
        )
            .into_response(),
        ServerReply::EventNonStringType => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":{},\"sequence_number\":1,\"detail\":\"event-shape-secret-sentinel\"}\n\n",
        )
            .into_response(),
        ServerReply::EventMissingSequence => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.heartbeat\",\"detail\":\"event-shape-secret-sentinel\"}\n\n",
        )
            .into_response(),
        ServerReply::EventNonIntegerSequence => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.heartbeat\",\"sequence_number\":\"1\",\"detail\":\"event-shape-secret-sentinel\"}\n\n",
        )
            .into_response(),
        ServerReply::InvalidUtf8Sse => Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(Bytes::from_static(b"data: \xff\n\n")))
            .expect("invalid UTF-8 SSE fixture is a valid HTTP response"),
        ServerReply::NativeTool => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.function_call_arguments.delta\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":1,\"delta\":\"{}\",\"item_id\":\"call_1\",\"output_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::NativeToolMissingOutputIndex => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"item\":{\"id\":\"call_1\",\"type\":\"function_call\",\"status\":\"in_progress\",\"call_id\":\"call_1\",\"name\":\"native-secret-missing-index\",\"arguments\":\"\"}}\n\n",
        )
            .into_response(),
        ServerReply::UnsupportedContentPartMissingFields => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.content_part.added\",\"sequence_number\":1,\"part\":{\"type\":\"refusal\"}}\n\n",
        )
            .into_response(),
        ServerReply::NativeToolItemWithText => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"call_1\",\"type\":\"function_call\",\"status\":\"in_progress\",\"call_id\":\"call_1\",\"name\":\"native-secret-tool\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":2,\"output_index\":0,\"item\":{\"id\":\"call_1\",\"type\":\"function_call\",\"status\":\"completed\",\"call_id\":\"call_1\",\"name\":\"native-secret-tool\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":4,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_native_with_text\",\"status\":\"completed\"}}\n\n"
            ),
        )
            .into_response(),
        ServerReply::NativeToolDoneWithText => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"call_done\",\"type\":\"function_call\",\"status\":\"completed\",\"call_id\":\"call_done\",\"name\":\"native-secret-done-tool\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":1,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::NativeToolOnlyInCompletedOutput => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_native_terminal\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<result value=\\\"alpha\\\" />\",\"annotations\":[]}]},{\"id\":\"call_terminal\",\"type\":\"function_call\",\"status\":\"completed\",\"call_id\":\"call_terminal\",\"name\":\"native-secret-terminal-tool\",\"arguments\":\"{}\"}]}}\n\n"
            ),
        )
            .into_response(),
        ServerReply::TerminalIdentityDriftWithNativeTool => {
            let text = "<result value=\"alpha\" />";
            let done = serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            });
            let tool = serde_json::json!({
                "id": "call_terminal",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_terminal",
                "name": "native-secret-combined-tool",
                "arguments": "{}",
            });
            sse_response(vec![
                serde_json::json!({"type": "response.created", "sequence_number": 1, "response": {"id": "resp_original", "status": "in_progress"}}),
                serde_json::json!({"type": "response.output_item.added", "sequence_number": 2, "output_index": 0, "item": {"id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
                serde_json::json!({"type": "response.output_text.delta", "sequence_number": 3, "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": text}),
                serde_json::json!({"type": "response.output_text.done", "sequence_number": 4, "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": text}),
                serde_json::json!({"type": "response.output_item.done", "sequence_number": 5, "output_index": 0, "item": done.clone()}),
                serde_json::json!({"type": "response.completed", "sequence_number": 6, "response": {"id": "resp_changed", "status": "completed", "output": [done, tool]}}),
            ])
        }
        ServerReply::Failed => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":1}\n\n",
        )
            .into_response(),
        ServerReply::Incomplete => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"sequence_number\":1}\n\n",
        )
            .into_response(),
        ServerReply::Error => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "event: error\ndata: {\"type\":\"error\",\"sequence_number\":1,\"message\":\"redacted\"}\n\n",
        )
            .into_response(),
        ServerReply::DuplicateDone => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":4,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::DeltaAfterTextDone => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":4,\"delta\":\"late-delta-secret\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::DuplicateTerminal => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":6,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n\n"
            ),
        )
            .into_response(),
        ServerReply::EventAfterTerminal => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<ok />\",\"annotations\":[]}]}]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":6,\"delta\":\"late\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::NonMonotonicSequence => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"<ok\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::MismatchedTextIdentity => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"<ok />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<ok />\",\"item_id\":\"msg_2\",\"output_index\":0,\"content_index\":0}\n\n"
            ),
        )
            .into_response(),
        ServerReply::OutputItemAddedLifecycle(fault) => {
            output_item_added_lifecycle_fault_response(fault)
        }
        ServerReply::UnsealedPrivateAtTerminal(kind) => {
            unsealed_private_at_terminal_response(kind)
        }
    }
}

fn output_limit_exact_response() -> Response {
    let text = "x".repeat(256 * 1024);
    let message_added = serde_json::json!({
        "id": "msg_output_limit",
        "type": "message",
        "status": "in_progress",
        "role": "assistant",
        "content": [],
    });
    let message_done = serde_json::json!({
        "id": "msg_output_limit",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    });
    sse_response(vec![
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": message_added}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_output_limit", "output_index": 0, "content_index": 0, "delta": text}),
        serde_json::json!({"type": "response.output_text.done", "sequence_number": 3, "item_id": "msg_output_limit", "output_index": 0, "content_index": 0, "text": text}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": message_done.clone()}),
        serde_json::json!({"type": "response.completed", "sequence_number": 5, "response": {"id": "resp_output_limit", "status": "completed", "output": [message_done]}}),
    ])
}

fn output_limit_plus_one_response() -> Response {
    let exact = "x".repeat(256 * 1024);
    sse_response(vec![
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "id": "msg_output_limit",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            },
        }),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 2, "item_id": "msg_output_limit", "output_index": 0, "content_index": 0, "delta": exact}),
        serde_json::json!({"type": "response.output_text.delta", "sequence_number": 3, "item_id": "msg_output_limit", "output_index": 0, "content_index": 0, "delta": "!"}),
    ])
}

#[derive(Clone)]
struct RedirectServerState {
    source_attempts: Arc<AtomicUsize>,
    target_attempts: Arc<AtomicUsize>,
    target_authorizations: mpsc::UnboundedSender<String>,
    target_bodies: mpsc::UnboundedSender<Vec<u8>>,
}

async fn redirect_source_endpoint(State(state): State<RedirectServerState>) -> Redirect {
    state.source_attempts.fetch_add(1, Ordering::SeqCst);
    Redirect::temporary("/redirected-responses")
}

async fn redirect_target_endpoint(
    State(state): State<RedirectServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.target_attempts.fetch_add(1, Ordering::SeqCst);
    state
        .target_authorizations
        .send(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
        )
        .unwrap();
    state.target_bodies.send(body.to_vec()).unwrap();
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"<result value=\\\"al\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"pha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"<result value=\\\"alpha\\\" />\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"id\":\"resp_sample_redirect\",\"status\":\"completed\"}}\n\n"
        ),
    )
        .into_response()
}

#[derive(Clone)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("captured log lock").extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture_subscriber(
    logs: Arc<Mutex<Vec<u8>>>,
) -> impl tracing::Subscriber + Send + Sync + 'static {
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || CapturedWriter(Arc::clone(&logs)))
        .finish()
}

async fn spawn_server(
    reply: ServerReply,
) -> (
    String,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_server_with_notify(reply, None, None).await
}

async fn spawn_header_capture_server() -> (
    String,
    mpsc::UnboundedReceiver<HeaderMap>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    async fn capture_headers(
        State(headers): State<mpsc::UnboundedSender<HeaderMap>>,
        request_headers: HeaderMap,
    ) -> Response {
        headers.send(request_headers).unwrap();
        completed_response()
    }

    let (header_tx, header_rx) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/responses", post(capture_headers))
        .with_state(header_tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("http://{address}"), header_rx, shutdown_tx, server)
}

async fn spawn_sequence_server(
    replies: Vec<ServerReply>,
) -> (
    String,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_sequence_server_with_notify(replies, None, None).await
}

async fn spawn_server_with_notify(
    reply: ServerReply,
    dispatch_notify: Option<Arc<Notify>>,
    release_notify: Option<Arc<Notify>>,
) -> (
    String,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_sequence_server_with_notify(vec![reply], dispatch_notify, release_notify).await
}

async fn spawn_sequence_server_with_notify(
    replies: Vec<ServerReply>,
    dispatch_notify: Option<Arc<Notify>>,
    release_notify: Option<Arc<Notify>>,
) -> (
    String,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    assert!(!replies.is_empty(), "test server needs at least one reply");
    let (body_tx, body_rx) = mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/responses", post(responses_endpoint))
        .with_state(ServerState {
            attempts: Arc::clone(&attempts),
            bodies: body_tx,
            replies: Arc::new(replies),
            dispatch_notify,
            release_notify,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (
        format!("http://{address}"),
        attempts,
        body_rx,
        shutdown_tx,
        server,
    )
}

async fn spawn_redirect_server() -> (
    String,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<String>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let source_attempts = Arc::new(AtomicUsize::new(0));
    let target_attempts = Arc::new(AtomicUsize::new(0));
    let (target_authorization_tx, target_authorization_rx) = mpsc::unbounded_channel();
    let (target_body_tx, target_body_rx) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/responses", post(redirect_source_endpoint))
        .route("/redirected-responses", post(redirect_target_endpoint))
        .with_state(RedirectServerState {
            source_attempts: Arc::clone(&source_attempts),
            target_attempts: Arc::clone(&target_attempts),
            target_authorizations: target_authorization_tx,
            target_bodies: target_body_tx,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (
        format!("http://{address}"),
        source_attempts,
        target_attempts,
        target_authorization_rx,
        target_body_rx,
        shutdown_tx,
        server,
    )
}

#[derive(Clone)]
struct DispatchProbeProps {
    delta_notify: Arc<Notify>,
    complete_count: Arc<AtomicUsize>,
    complete_notify: Arc<Notify>,
}

#[component]
fn dispatch_probe_root(props: DispatchProbeProps, events: EventInput<ProviderEvent>) -> Component {
    let delta_notify = props.delta_notify;
    let complete_count = props.complete_count;
    let complete_notify = props.complete_notify;
    let text = events.select(ProviderEvent::TEXT);

    view! {
        #[system_once]
        dispatch_probe_system { "Dispatch probe." }

        {
            EventListener::observe("responses.dispatch-probe", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let delta_notify = Arc::clone(&delta_notify);
                    let complete_count = Arc::clone(&complete_count);
                    let complete_notify = Arc::clone(&complete_notify);
                    async move {
                        match event {
                            TextTurnEvent::TextDelta(_) => delta_notify.notify_one(),
                            TextTurnEvent::TextComplete(_) => {
                                complete_count.fetch_add(1, Ordering::SeqCst);
                                complete_notify.notify_one();
                            }
                        }
                        Ok::<(), Infallible>(())
                    }
                })
        }
    }
}

fn provider(api_base: String) -> AsyncOpenAiResponsesProvider {
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token").unwrap();
    provider_with_config(config)
}

fn provider_with_config(config: AsyncOpenAiTransportConfig) -> AsyncOpenAiResponsesProvider {
    provider_with_options(
        config,
        CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap(),
    )
}

fn provider_with_options(
    config: AsyncOpenAiTransportConfig,
    options: CodexHttpV1Options,
) -> AsyncOpenAiResponsesProvider {
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING).unwrap();
    AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options))
}

struct ExecuteReturnProbe {
    inner: AsyncOpenAiResponsesProvider,
    returned: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl ProviderPort for ExecuteReturnProbe {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        let stream = self.inner.execute(projection).await?;
        self.returned.store(true, Ordering::SeqCst);
        Ok(stream)
    }
}

struct SubmissionAfterExecuteObserver {
    execute_returned: Arc<std::sync::atomic::AtomicBool>,
}

impl EngineObserver for SubmissionAfterExecuteObserver {
    fn observe(&mut self, observation: &EngineObservation) {
        if matches!(observation, EngineObservation::InputSubmitted) {
            assert!(
                self.execute_returned.load(Ordering::SeqCst),
                "ApplicationHost observed input submission before the provider completed handoff"
            );
        }
    }
}

fn dispatch_probe_provider(api_base: String) -> AsyncOpenAiResponsesProvider {
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token").unwrap();
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING).unwrap();
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap();
    AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options))
}

fn sample_projection(task: &str, turn_id: &str) -> RenderedProjection {
    sample_projection_with_policy("Follow the response protocol.", task, turn_id)
}

fn sample_projection_with_policy(rules: &str, task: &str, turn_id: &str) -> RenderedProjection {
    fn xml_document(name: &str, text: &str) -> ResolvedDocument {
        let node = XmlNode::try_build(name, |children| {
            children.text(TextNode::new(text));
            Ok(())
        })
        .expect("provider test XML is valid");
        resolve_artifact_document(Document::from_xml(node))
            .expect("provider test projection resolves")
    }

    root_projection(vec![
        CanonicalInputItem::instruction(
            InstructionAuthority::System,
            xml_document("sample_policy", rules),
        ),
        CanonicalInputItem::message(
            ConversationRole::User,
            xml_document("sample_request", &format!("{task}\nturn_id: {turn_id}")),
        ),
    ])
}

fn default_sample_projection() -> RenderedProjection {
    sample_projection("Return one result.", "turn-1")
}

fn root_projection(items: Vec<CanonicalInputItem>) -> RenderedProjection {
    RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", items)])
        .expect("the test root projection identity is valid")
}

fn assistant_projection(text: impl Into<String>) -> RenderedProjection {
    root_projection(vec![CanonicalInputItem::assistant_text(text, None)])
}

fn root_items(projection: &RenderedProjection) -> &[CanonicalInputItem] {
    assert_eq!(projection.nodes().len(), 1, "expected one test root node");
    let root = &projection.nodes()[0];
    assert_eq!(root.identity(), "root");
    root.items()
}

fn strict_extension_projection(
    previous: &RenderedProjection,
    confirmed_assistant: &str,
    next: &RenderedProjection,
) -> RenderedProjection {
    assert_eq!(
        root_items(previous).first(),
        root_items(next).first(),
        "a strict extension retains the confirmed system instruction"
    );
    semantic_extension_projection(previous, confirmed_assistant, next)
}

fn semantic_extension_projection(
    previous: &RenderedProjection,
    confirmed_assistant: &str,
    next: &RenderedProjection,
) -> RenderedProjection {
    let mut items = root_items(previous).to_vec();
    items[0] = root_items(next)
        .first()
        .expect("next sample projection contains its current system instruction")
        .clone();
    items.push(CanonicalInputItem::assistant_text(
        confirmed_assistant,
        None,
    ));
    items.push(
        root_items(next)
            .get(1)
            .expect("next sample projection contains its current user message")
            .clone(),
    );
    root_projection(items)
}

async fn execute_provider<P>(
    provider: &mut P,
    projection: RenderedProjection,
) -> Result<Vec<ProviderEvent>, ProviderFault>
where
    P: ProviderPort,
{
    let execution = trace_provider(provider, projection).await;
    match execution.fault {
        Some(fault) => Err(fault),
        None => Ok(execution.events),
    }
}

struct ProviderExecution {
    events: Vec<ProviderEvent>,
    fault: Option<ProviderFault>,
}

async fn trace_provider<P>(provider: &mut P, projection: RenderedProjection) -> ProviderExecution
where
    P: ProviderPort,
{
    let mut stream = match provider.execute(projection).await {
        Ok(stream) => stream,
        Err(fault) => {
            return ProviderExecution {
                events: Vec::new(),
                fault: Some(fault),
            };
        }
    };
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(fault) => {
                return ProviderExecution {
                    events,
                    fault: Some(fault),
                };
            }
        }
    }
    ProviderExecution {
        events,
        fault: None,
    }
}

fn expect_provider_failure(result: Result<Vec<ProviderEvent>, ProviderFault>) -> ProviderFault {
    match result {
        Ok(_) => panic!("provider execution unexpectedly succeeded"),
        Err(fault) => fault,
    }
}

fn text_trace(events: Vec<ProviderEvent>) -> Vec<(&'static str, String)> {
    events
        .into_iter()
        .map(|event| match event {
            ProviderEvent::Text(TextTurnEvent::TextDelta(text)) => ("delta", text),
            ProviderEvent::Text(TextTurnEvent::TextComplete(text)) => ("complete", text),
            _ => panic!("Responses adapter emitted a non-text event"),
        })
        .collect()
}

fn assert_serialized_request_body_limit_fault(fault: &ProviderFault, excluded: &str) {
    assert_eq!(fault.code(), ProviderFaultCode::RequestPreparation);
    assert_eq!(
        fault.message(),
        "OpenAI Responses serialized outbound request body exceeded configured limit"
    );
    assert_eq!(
        fault.to_string(),
        "provider ModelRejected: OpenAI Responses serialized outbound request body exceeded configured limit"
    );
    assert!(!format!("{fault:?}\n{fault}").contains(excluded));
}

#[tokio::test]
async fn responses_usage_observer_reports_validated_cache_counts() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::CompletedWithUsage).await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_by_provider = Arc::clone(&observed);
    let mut provider = provider(api_base).with_response_usage_observer(move |usage| {
        observed_by_provider
            .lock()
            .unwrap()
            .push((usage.input_tokens(), usage.cached_tokens()));
    });

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .unwrap();

    assert_eq!(text_trace(events).last().unwrap().0, "complete");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(*observed.lock().unwrap(), vec![(Some(321), Some(256))]);

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn responses_usage_rejects_internally_inconsistent_counts_without_payload_exposure() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::CompletedWithInconsistentUsage).await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_by_provider = Arc::clone(&observed);
    let mut provider = provider(api_base).with_response_usage_observer(move |usage| {
        observed_by_provider.lock().unwrap().push(usage);
    });

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert!(observed.lock().unwrap().is_empty());
    assert!(!format!("{fault:?}\n{fault}").contains("usage-secret-must-not-be-logged"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn responses_usage_distinguishes_unreported_usage_from_missing_cache_detail() {
    for (reply, expected) in [
        (ServerReply::Completed, (None, None)),
        (
            ServerReply::CompletedWithUsageFixture(UsageFixture::MissingCached),
            (Some(321), None),
        ),
    ] {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_by_provider = Arc::clone(&observed);
        let mut provider = provider(api_base).with_response_usage_observer(move |usage| {
            observed_by_provider
                .lock()
                .unwrap()
                .push((usage.input_tokens(), usage.cached_tokens()));
        });

        execute_provider(&mut provider, default_sample_projection())
            .await
            .expect("absent optional usage fields remain observable");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(*observed.lock().unwrap(), vec![expected]);
        let _ = shutdown.send(());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn responses_usage_rejects_untrusted_shapes_before_observer_delivery() {
    for case in [
        UsageFixture::NegativeInput,
        UsageFixture::FractionalInput,
        UsageFixture::NonObjectDetails,
        UsageFixture::NegativeCached,
        UsageFixture::CachedExceedsInput,
        UsageFixture::OutputWithoutTotal,
    ] {
        let (api_base, attempts, _bodies, shutdown, server) =
            spawn_server(ServerReply::CompletedWithUsageFixture(case)).await;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_by_provider = Arc::clone(&observed);
        let mut provider = provider(api_base).with_response_usage_observer(move |usage| {
            observed_by_provider.lock().unwrap().push(usage);
        });

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
        assert!(observed.lock().unwrap().is_empty());
        assert!(!format!("{fault:?}\n{fault}").contains("private-usage-detail"));
        let _ = shutdown.send(());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn responses_usage_observer_panic_propagates() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::CompletedWithUsage).await;
    let mut provider = provider(api_base).with_response_usage_observer(|_| {
        panic!("observer panic must stay outside provider behavior");
    });

    let panic =
        std::panic::AssertUnwindSafe(execute_provider(&mut provider, default_sample_projection()))
            .catch_unwind()
            .await;

    assert!(panic.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let _ = shutdown.send(());
    server.await.unwrap();
}

fn assert_native_tool_lifecycle_fault(fault: &ProviderFault, event_type: &str) {
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(
        fault.message(),
        format!(
            "OpenAI function call lifecycle is invalid \
             [response_event_type={event_type}; response_event_reason=ledger_mismatch]"
        )
    );
}

#[tokio::test]
async fn serialized_responses_request_body_limit_uses_exact_json_bytes_before_dispatch() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let raw_bytes = 512;
    let exact_projection = assistant_projection("p".repeat(raw_bytes));
    let mut calibration = provider(api_base.clone());
    let mut calibration_stream = calibration
        .execute(exact_projection.clone())
        .await
        .expect("default request limit permits calibration dispatch");
    let _ = calibration_stream.next().await;
    drop(calibration_stream);
    let exact_body = bodies.recv().await.unwrap();
    let exact_limit = exact_body.len();
    assert!(
        exact_limit > raw_bytes,
        "the serialized envelope has structure"
    );

    let exact_config = AsyncOpenAiTransportConfig::new(api_base.clone(), "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(exact_limit)
        .unwrap();
    let mut exact_provider = provider_with_config(exact_config);
    let mut exact_stream = exact_provider
        .execute(exact_projection)
        .await
        .expect("the exact serialized request-body limit is inclusive");
    let _ = exact_stream.next().await;
    drop(exact_stream);
    assert_eq!(bodies.recv().await.unwrap().len(), exact_limit);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let plus_one_config = AsyncOpenAiTransportConfig::new(api_base.clone(), "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(exact_limit)
        .unwrap();
    let mut plus_one_provider = provider_with_config(plus_one_config);
    let plus_one_sentinel = format!("{}x", "p".repeat(raw_bytes));
    let plus_one_fault = match plus_one_provider
        .execute(assistant_projection(plus_one_sentinel.clone()))
        .await
    {
        Ok(stream) => {
            drop(stream);
            panic!("limit plus one unexpectedly dispatched")
        }
        Err(fault) => fault,
    };
    assert_serialized_request_body_limit_fault(&plus_one_fault, &plus_one_sentinel);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let escaped_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(exact_limit)
        .unwrap();
    let mut escaped_provider = provider_with_config(escaped_config);
    let escaped_sentinel = "\"".repeat(raw_bytes);
    let escaped_fault = match escaped_provider
        .execute(assistant_projection(escaped_sentinel.clone()))
        .await
    {
        Ok(stream) => {
            drop(stream);
            panic!("escaped JSON growth unexpectedly dispatched")
        }
        Err(fault) => fault,
    };
    assert_serialized_request_body_limit_fault(&escaped_fault, &escaped_sentinel);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn equal_length_replacement_rejection_preserves_the_confirmed_continuation() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let original = assistant_projection("equal-original-sentinel");
    assert_eq!(
        "equal-original-sentinel".len(),
        "equal-replaced-sentinel".len()
    );

    let mut calibration = provider(api_base.clone());
    execute_provider(&mut calibration, original.clone())
        .await
        .expect("calibration response completes");
    let _calibration_initial = bodies.recv().await.unwrap();
    let mut calibration_stream = calibration
        .execute(original.clone())
        .await
        .expect("calibration continuation dispatches");
    let _ = calibration_stream.next().await;
    drop(calibration_stream);
    let confirmed_snapshot = bodies.recv().await.unwrap();

    let limited_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(confirmed_snapshot.len())
        .unwrap();
    let mut limited = provider_with_config(limited_config);
    let limited_execution = trace_provider(&mut limited, original.clone()).await;
    let fault = limited_execution
        .fault
        .expect("an unretainable partial is rejected before it is published");
    assert_serialized_request_body_limit_fault(&fault, "equal-original-sentinel");
    assert!(limited_execution.events.is_empty());
    let _limited_initial = bodies.recv().await.unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn post_response_snapshot_limit_plus_one_fails_before_completion_and_adoption() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let projection = assistant_projection("post-response-snapshot-sentinel");
    let mut calibration = provider(api_base.clone());

    execute_provider(&mut calibration, projection.clone())
        .await
        .expect("first calibration response completes");
    let calibration_first = bodies.recv().await.unwrap();
    execute_provider(&mut calibration, projection.clone())
        .await
        .expect("second calibration response completes");
    let first_retained_snapshot = bodies.recv().await.unwrap();
    let mut third_stream = calibration
        .execute(projection.clone())
        .await
        .expect("third calibration request exposes the next exact snapshot");
    let _ = third_stream.next().await;
    drop(third_stream);
    let second_snapshot = bodies.recv().await.unwrap();
    let limit = second_snapshot.len() - 1;
    assert!(first_retained_snapshot.len() <= limit);
    assert_eq!(second_snapshot.len(), limit + 1);

    let limited_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(limit)
        .unwrap();
    let mut limited = provider_with_config(limited_config);
    let failed = trace_provider(&mut limited, projection).await;
    let fault = failed
        .fault
        .expect("an unretainable delta returns a stream fault before publication");
    assert_serialized_request_body_limit_fault(&fault, "post-response-snapshot-sentinel");
    assert!(
        failed.events.is_empty(),
        "a delta that cannot be replayed with its interruption marker is never published"
    );
    let rejected_request = bodies.recv().await.unwrap();
    assert_eq!(rejected_request, calibration_first);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn compacted_post_response_snapshot_uses_the_exact_serialized_limit() {
    let (calibration_base, _, mut calibration_bodies, calibration_shutdown, calibration_server) =
        spawn_server(ServerReply::CompactionWithText).await;
    let projection = assistant_projection("compaction-request-limit-sentinel");
    let mut calibration = provider(calibration_base);
    execute_provider(&mut calibration, projection.clone())
        .await
        .expect("calibration compaction completes");
    let _calibration_first = calibration_bodies.recv().await.unwrap();
    let mut calibration_stream = calibration
        .execute(projection.clone())
        .await
        .expect("calibration exposes the compacted snapshot");
    let _ = calibration_stream.next().await;
    drop(calibration_stream);
    let compacted_snapshot = calibration_bodies.recv().await.unwrap();
    calibration_shutdown.send(()).unwrap();
    calibration_server.await.unwrap();

    let compacted_request: serde_json::Value = serde_json::from_slice(&compacted_snapshot).unwrap();
    assert_eq!(compacted_request["input"].as_array().unwrap().len(), 2);
    assert_eq!(compacted_request["input"][0]["type"], "compaction");
    assert!(!String::from_utf8(compacted_snapshot.clone())
        .unwrap()
        .contains("compaction-request-limit-sentinel"));

    let (rejected_base, rejected_attempts, _rejected_bodies, rejected_shutdown, rejected_server) =
        spawn_server(ServerReply::CompactionWithText).await;
    let rejected_config = AsyncOpenAiTransportConfig::new(rejected_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(compacted_snapshot.len() - 1)
        .unwrap();
    let mut rejected = provider_with_config(rejected_config);
    let rejected_execution = trace_provider(&mut rejected, projection.clone()).await;
    let fault = rejected_execution
        .fault
        .expect("a compacted retained snapshot over the limit is rejected");
    assert_serialized_request_body_limit_fault(&fault, "opaque-compaction-secret");
    assert!(text_trace(rejected_execution.events)
        .iter()
        .all(|(kind, _)| *kind != "complete"));
    assert_eq!(rejected_attempts.load(Ordering::SeqCst), 1);
    rejected_shutdown.send(()).unwrap();
    rejected_server.await.unwrap();

    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::CompactionWithText).await;
    let limited_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(compacted_snapshot.len())
        .unwrap();
    let mut limited = provider_with_config(limited_config);
    let limited_execution = trace_provider(&mut limited, projection.clone()).await;
    let fault = limited_execution
        .fault
        .expect("an unretainable visible partial is rejected before publication");
    assert_serialized_request_body_limit_fault(&fault, "opaque-compaction-secret");
    assert!(limited_execution.events.is_empty());
    let _limited_first = bodies.recv().await.unwrap();

    let mut continued_stream = limited
        .execute(projection)
        .await
        .expect("the rejected partial leaves the causal input retryable");
    let _ = continued_stream.next().await;
    drop(continued_stream);
    let continued_body = bodies.recv().await.unwrap();
    assert_ne!(continued_body, compacted_snapshot);
    let request: serde_json::Value = serde_json::from_slice(&continued_body).unwrap();
    assert_eq!(request["input"][0]["type"], "compaction");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn request_limit_applies_after_compacted_continuation_reconciliation() {
    let retained_text = "r".repeat(8 * 1024);
    let initial_projection = assistant_projection(retained_text.clone());
    let (calibration_base, _, mut calibration_bodies, calibration_shutdown, calibration_server) =
        spawn_server(ServerReply::CompactionWithText).await;
    let mut calibration = provider(calibration_base);
    execute_provider(&mut calibration, initial_projection.clone())
        .await
        .expect("calibration compaction completes");
    let initial_limit = calibration_bodies.recv().await.unwrap().len();
    calibration_shutdown.send(()).unwrap();
    calibration_server.await.unwrap();

    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::CompactionWithText).await;
    let request_limit = initial_limit + 1024;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(request_limit)
        .unwrap();
    let mut limited = provider_with_config(config);
    execute_provider(&mut limited, initial_projection)
        .await
        .expect("the bounded initial request can retain a recoverable partial");
    let initial_body = bodies.recv().await.unwrap();
    assert_eq!(initial_body.len(), initial_limit);
    assert!(initial_body.len() <= request_limit);

    let extended_projection = root_projection(vec![
        CanonicalInputItem::assistant_text(retained_text, None),
        CanonicalInputItem::assistant_text("post-compaction-delta", None),
    ]);
    execute_provider(&mut limited, extended_projection)
        .await
        .expect("the compacted continuation is compiled inside the same request bound");
    let reconciled_body = bodies.recv().await.unwrap();
    assert!(reconciled_body.len() < initial_limit);
    assert!(reconciled_body.len() <= request_limit);
    let request: serde_json::Value = serde_json::from_slice(&reconciled_body).unwrap();
    assert!(request.get("previous_response_id").is_none());
    assert_eq!(request["input"].as_array().unwrap().len(), 3);
    assert_eq!(request["input"][0]["type"], "compaction");
    assert_eq!(request["input"][1]["role"], "assistant");
    assert!(request["input"][2]
        .to_string()
        .contains("post-compaction-delta"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn reasoning_retained_snapshot_overflow_is_sanitized_before_text_complete() {
    let (calibration_base, _, mut calibration_bodies, calibration_shutdown, calibration_server) =
        spawn_server(ServerReply::ReasoningWithText).await;
    let projection = assistant_projection("reasoning-request-limit-sentinel");
    let mut calibration = provider(calibration_base);
    execute_provider(&mut calibration, projection.clone())
        .await
        .expect("calibration reasoning response completes");
    let _calibration_first = calibration_bodies.recv().await.unwrap();
    let mut calibration_stream = calibration
        .execute(projection.clone())
        .await
        .expect("calibration exposes the reasoning snapshot");
    let _ = calibration_stream.next().await;
    drop(calibration_stream);
    let reasoning_snapshot = calibration_bodies.recv().await.unwrap();
    calibration_shutdown.send(()).unwrap();
    calibration_server.await.unwrap();
    let reasoning_request: serde_json::Value = serde_json::from_slice(&reasoning_snapshot).unwrap();
    assert!(reasoning_request["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "reasoning"));

    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ReasoningWithText).await;
    let limited_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(reasoning_snapshot.len() - 1)
        .unwrap();
    let mut limited = provider_with_config(limited_config);
    let execution = trace_provider(&mut limited, projection).await;
    let fault = execution
        .fault
        .expect("reasoning snapshot over the limit returns a provider fault");
    assert_serialized_request_body_limit_fault(&fault, "encrypted-reasoning-1");
    assert!(text_trace(execution.events)
        .iter()
        .all(|(kind, _)| *kind != "complete"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn wire_bytes_match_the_domain_neutral_oracle() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let mut provider = provider(api_base);

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("Responses reaction completes");

    assert_eq!(
        text_trace(events).last(),
        Some(&("complete", String::from("<result value=\"alpha\" />")))
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let wire_body = bodies.recv().await.unwrap();

    assert_eq!(wire_body, SAMPLE_CODEX_ORACLE_BODY);
    let body: serde_json::Value = serde_json::from_slice(&wire_body).unwrap();
    assert_eq!(body["model"], "gpt-5.6-codex");
    assert_eq!(body["parallel_tool_calls"], false);
    assert!(body["instructions"]
        .as_str()
        .unwrap()
        .contains("<sample_policy>Follow the response protocol.</sample_policy>"));
    assert!(body["input"].is_array());
    assert!(body.get("history").is_none());
    assert!(body.get("candidate").is_none());

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn continuation_replays_validated_history_and_exact_duplicate_reuses_context() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::Completed,
        ServerReply::CompletedSecondMove,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);
    let first_projection = default_sample_projection();

    let first_events = execute_provider(&mut provider, first_projection.clone())
        .await
        .expect("first full projection succeeds");
    assert_eq!(
        text_trace(first_events).last(),
        Some(&("complete", String::from("<result value=\"alpha\" />")))
    );

    let next_projection = sample_projection("Return a second result.", "turn-2");
    let strict_extension = strict_extension_projection(
        &first_projection,
        "<result value=\"alpha\" />",
        &next_projection,
    );
    execute_provider(&mut provider, strict_extension.clone())
        .await
        .expect("strictly extended full projection succeeds");

    execute_provider(&mut provider, strict_extension)
        .await
        .expect("an exact duplicate reuses the compatible Provider context");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    let first_body: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let extended_body: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let duplicate_body: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(first_body["input"].as_array().unwrap().len(), 1);
    assert_eq!(first_body["store"], false);
    assert!(first_body.get("previous_response_id").is_none());
    let continued_input = extended_body["input"].as_array().unwrap();
    assert_eq!(extended_body["store"], false);
    assert!(extended_body.get("previous_response_id").is_none());
    assert_eq!(extended_body["instructions"], first_body["instructions"]);
    assert_eq!(continued_input.len(), 3);
    assert_eq!(continued_input[0]["role"], "user");
    assert_eq!(continued_input[1]["role"], "assistant");
    assert_eq!(continued_input[2]["role"], "user");
    assert!(continued_input[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn-2"));
    assert!(extended_body.get("history").is_none());
    let duplicate_input = duplicate_body["input"].as_array().unwrap();
    assert_eq!(duplicate_body["store"], false);
    assert!(duplicate_body.get("previous_response_id").is_none());
    assert_eq!(duplicate_body["instructions"], first_body["instructions"]);
    assert_eq!(duplicate_input.len(), 4);
    assert_eq!(duplicate_input[3]["role"], "assistant");

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn responses_timeout_retry_reuses_the_last_confirmed_request_bytes() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::Completed,
        ServerReply::StalledStream,
        ServerReply::CompletedSecondMove,
    ])
    .await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_millis(50),
        )
        .unwrap();
    let mut components = ComponentHost::new(
        diff_prompt_application,
        DiffPromptProps {
            value: String::from("input-a"),
        },
    );
    let mut host = ApplicationHost::new(provider_with_config(config));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("the first reaction confirms provider history");
    components.set_props(DiffPromptProps {
        value: String::from("input-b"),
    });
    let fault = host
        .dispatch_llm_reaction(&mut components)
        .await
        .expect_err("the stalled response must time out");
    let ApplicationHostFault::ProviderExecution(provider_fault) = fault else {
        panic!("unexpected timeout fault: {fault:?}");
    };
    assert_eq!(provider_fault.code(), ProviderFaultCode::StreamTimeout);

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("the timeout retry starts from confirmed provider history");

    let _confirmed = bodies.recv().await.unwrap();
    let timed_out = bodies.recv().await.unwrap();
    let retry = bodies.recv().await.unwrap();
    assert!(
        retry == timed_out,
        "retry request bytes differ from the timed-out request"
    );
    let retry: serde_json::Value = serde_json::from_slice(&retry).unwrap();
    assert_eq!(retry["store"], false);
    assert!(retry.get("previous_response_id").is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn replayed_request_preserves_prompt_cache_key_with_validated_history() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::Completed,
        ServerReply::CompletedSecondMove,
    ])
    .await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token").unwrap();
    let options =
        CodexHttpV1Options::new("gpt-5.6-codex", None, None, Some("stable-prompt-cache-key"))
            .unwrap();
    let mut provider = provider_with_options(config, options);

    execute_provider(&mut provider, diff_projection("input-a"))
        .await
        .expect("initial response succeeds");
    execute_provider(&mut provider, diff_projection("input-b"))
        .await
        .expect("replayed response succeeds");

    let first: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let replayed: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(first["prompt_cache_key"], "stable-prompt-cache-key");
    assert_eq!(replayed["prompt_cache_key"], first["prompt_cache_key"]);
    assert!(replayed.get("previous_response_id").is_none());
    let input = replayed["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert!(input[0].to_string().contains("input-a"));
    assert_eq!(input[1]["role"], "assistant");
    assert!(input[2].to_string().contains("input-b"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_terminal_response_id_retains_handed_off_input_and_sealed_output() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::Completed,
        ServerReply::ResponseIdentityDrift,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, diff_projection("input-a"))
        .await
        .expect("initial response succeeds");
    let fault =
        expect_provider_failure(execute_provider(&mut provider, diff_projection("input-b")).await);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    execute_provider(&mut provider, diff_projection("input-b"))
        .await
        .expect("retry replays facts committed before terminal identity validation");

    let _first = bodies.recv().await.unwrap();
    let rejected: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(retry.get("previous_response_id").is_none());
    let rejected_input = rejected["input"].as_array().unwrap();
    let input = retry["input"].as_array().unwrap();
    assert_eq!(&input[..3], rejected_input);
    assert_eq!(input.len(), 5);
    assert!(input[0].to_string().contains("input-a"));
    assert_eq!(input[1]["role"], "assistant");
    assert!(input[2].to_string().contains("input-b"));
    assert_eq!(input[3]["role"], "assistant");
    assert!(input[3]
        .to_string()
        .contains("<result value=\\\"alpha\\\" />"));
    assert!(input[4].to_string().contains("input-b"));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn continued_context_replays_history_with_current_system_policy() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::CompactionWithText,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(
        &mut provider,
        sample_projection("Return one result.", "binding-turn-1"),
    )
    .await
    .expect("first compacted response confirms the inline binding");
    execute_provider(
        &mut provider,
        sample_projection_with_policy(
            "Replacement rules must wait for Fresh provider context.",
            "Return a second result.",
            "binding-turn-2",
        ),
    )
    .await
    .expect("next complete projection reuses the confirmed Provider context");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let first: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let continued: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(first["instructions"]
        .as_str()
        .unwrap()
        .contains("Follow the response protocol."));
    assert_ne!(continued["instructions"], first["instructions"]);
    assert!(continued["instructions"]
        .as_str()
        .unwrap()
        .contains("Replacement rules"));
    let input = continued["input"].as_array().unwrap();
    assert!(continued.get("previous_response_id").is_none());
    assert_eq!(input.len(), 3);
    assert_eq!(input[0]["type"], "compaction");
    assert_eq!(input[1]["role"], "assistant");
    assert!(input[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("binding-turn-2"));

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn fresh_recovery_uses_current_binding_after_private_context_loss() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::CompactionWithText,
        ServerReply::Completed,
    ])
    .await;
    let first_projection = default_sample_projection();
    {
        let mut provider = provider(api_base.clone());
        execute_provider(&mut provider, first_projection)
            .await
            .expect("first Provider context completes");
    }

    let next_projection = sample_projection_with_policy(
        "Fresh recovery uses the currently rendered rules.",
        "Return a second result.",
        "fresh-binding-turn-2",
    );
    let mut recovered_provider = provider(api_base);
    execute_provider(&mut recovered_provider, next_projection)
        .await
        .expect("replacement Provider starts Fresh from the complete projection");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let _first = bodies.recv().await.unwrap();
    let recovered: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(recovered["instructions"]
        .as_str()
        .unwrap()
        .contains("Fresh recovery uses the currently rendered rules."));
    let input = recovered["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    assert!(input[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("fresh-binding-turn-2"));
    assert!(!serde_json::to_string(input)
        .unwrap()
        .contains("opaque-compaction-secret"));

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn compaction_prunes_the_private_wire_window_and_replays_canonical_input() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::CompactionWithText).await;
    let mut provider = provider(api_base);
    let first_projection = default_sample_projection();

    execute_provider(&mut provider, first_projection.clone())
        .await
        .expect("first compacted response succeeds");
    let next_projection = sample_projection("Return a second result.", "turn-2");
    execute_provider(
        &mut provider,
        strict_extension_projection(
            &first_projection,
            "<result value=\"alpha\" />",
            &next_projection,
        ),
    )
    .await
    .expect("strict extension reuses the compacted wire window");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let first: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(
        first["context_management"],
        serde_json::json!([{"type": "compaction", "compact_threshold": 200000}])
    );
    assert_eq!(first["store"], false);
    assert!(first.get("previous_response_id").is_none());
    assert!(second.get("previous_response_id").is_none());

    let input = second["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(
        input[0],
        serde_json::json!({
            "id": "cmp_1",
            "type": "compaction",
            "encrypted_content": "opaque-compaction-secret",
        })
    );
    assert_eq!(input[1]["role"], "assistant");
    assert!(input[1]
        .to_string()
        .contains("<result value=\\\"alpha\\\" />"));
    assert_eq!(input[2]["role"], "user");
    assert!(input[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn-2"));
    assert!(!serde_json::to_string(input).unwrap().contains("turn-1"));

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn missing_terminal_keeps_validated_compaction_and_sealed_output() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::CompactionWithText,
        ServerReply::MissingCompleted,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);
    let first_projection = default_sample_projection();

    execute_provider(&mut provider, first_projection.clone())
        .await
        .expect("first compacted response succeeds");
    let next_projection = sample_projection("Return a second result.", "turn-2");
    let strict_extension = strict_extension_projection(
        &first_projection,
        "<result value=\"alpha\" />",
        &next_projection,
    );
    let fault =
        expect_provider_failure(execute_provider(&mut provider, strict_extension.clone()).await);
    assert!(fault.to_string().contains("without response.completed"));
    execute_provider(&mut provider, strict_extension)
        .await
        .expect("retry keeps the validated compacted window and sealed stream output");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let _first = bodies.recv().await.unwrap();
    let faulted: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(retry.get("previous_response_id").is_none());
    let faulted_input = faulted["input"].as_array().unwrap();
    let retry_input = retry["input"].as_array().unwrap();
    assert_eq!(&retry_input[..3], faulted_input);
    assert_eq!(retry_input.len(), 4);
    assert_eq!(retry_input[0]["type"], "compaction");
    assert!(retry_input[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn-2"));
    assert_eq!(retry_input[3]["role"], "assistant");
    assert!(retry_input[3]
        .to_string()
        .contains("<result value=\\\"alpha\\\" />"));
    assert!(serde_json::to_string(&retry)
        .unwrap()
        .contains("opaque-compaction-secret"));

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn private_items_sealed_at_done_survive_missing_terminal_and_replay_once_in_ordinal_order() {
    let cases = [
        (
            "reasoning",
            ServerReply::SealedReasoningThenEof,
            "reasoning",
            "encrypted-reasoning-1",
        ),
        (
            "compaction",
            ServerReply::SealedCompactionThenEof,
            "compaction",
            "sealed-compaction-payload",
        ),
    ];

    for (name, first_reply, private_type, private_payload) in cases {
        let (api_base, attempts, mut bodies, shutdown, server) =
            spawn_sequence_server(vec![first_reply, ServerReply::Completed]).await;
        let mut provider = provider(api_base);
        let projection = default_sample_projection();

        let fault =
            expect_provider_failure(execute_provider(&mut provider, projection.clone()).await);
        assert!(
            fault.to_string().contains("without response.completed"),
            "case {name}"
        );
        execute_provider(&mut provider, projection)
            .await
            .expect("retry replays every private item sealed before the missing terminal");

        let _first = bodies.recv().await.expect("faulted request body");
        let retry: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.expect("retry request body")).unwrap();
        let input = retry["input"].as_array().expect("retry has input items");
        let private_index = input
            .iter()
            .position(|item| {
                item["type"] == private_type && item.to_string().contains(private_payload)
            })
            .expect("sealed private output survives the terminal fault");
        let text_index = input
            .iter()
            .position(|item| {
                item["role"] == "assistant"
                    && item.to_string().contains("<result value=\\\"alpha\\\" />")
            })
            .expect("later sealed assistant text survives the terminal fault");
        assert!(
            private_index < text_index,
            "case {name}: provider ordinal is retained"
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == private_type
                    && item.to_string().contains(private_payload))
                .count(),
            1,
            "case {name}: reconciliation cannot append a duplicate private item"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn real_shaped_reasoning_done_replays_exact_wire_object_after_eof_before_completed() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::SealedRealShapedReasoningOnlyThenEof,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);
    let projection = default_sample_projection();

    let fault = expect_provider_failure(execute_provider(&mut provider, projection.clone()).await);
    assert!(fault.to_string().contains("without response.completed"));
    execute_provider(&mut provider, projection)
        .await
        .expect("the sealed reasoning item remains replayable after EOF");

    let _first_request = bodies.recv().await.unwrap();
    let replay: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let reasoning = replay["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["id"] == "rs_eof_four_field")
        .collect::<Vec<_>>();
    assert_eq!(reasoning.len(), 1);
    assert_eq!(
        reasoning[0],
        &serde_json::json!({
            "type": "reasoning",
            "id": "rs_eof_four_field",
            "summary": [],
            "encrypted_content": "encrypted-eof-four-field",
        })
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn completed_omits_sealed_real_shaped_reasoning_and_replay_keeps_it_before_public_output() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::SealedReasoningOmittedFromCompleted,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("terminal reconciliation must not re-adopt an already sealed private item");
    execute_provider(
        &mut provider,
        sample_projection("Return a second result.", "live-shape-replay-turn-2"),
    )
    .await
    .expect("the sealed reasoning item remains replayable after terminal omission");

    let _first_request = bodies.recv().await.unwrap();
    let replay: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(replay.get("previous_response_id").is_none());
    let input = replay["input"].as_array().unwrap();
    let reasoning_positions = input
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (item["id"] == "rs_live_shape").then_some((index, item)))
        .collect::<Vec<_>>();
    assert_eq!(reasoning_positions.len(), 1);
    let (reasoning_index, reasoning) = reasoning_positions[0];
    assert_eq!(
        reasoning,
        &serde_json::json!({
            "id": "rs_live_shape",
            "type": "reasoning",
            "summary": [],
            "encrypted_content": "encrypted-live-shape",
        })
    );
    assert!(reasoning.get("status").is_none());
    assert!(reasoning.get("content").is_none());
    let public_index = input
        .iter()
        .position(|item| {
            item["role"] == "assistant"
                && item.to_string().contains("<result value=\\\"alpha\\\" />")
        })
        .expect("sealed public message is replayed in legal assistant form");
    let new_projection_index = input
        .iter()
        .position(|item| item.to_string().contains("live-shape-replay-turn-2"))
        .expect("new projection input follows retained response output");
    assert!(reasoning_index < public_index);
    assert!(public_index < new_projection_index);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn completed_empty_output_is_metadata_only_after_sealed_reasoning_and_text() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::SealedReasoningEmptyTerminal,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("an empty terminal list cannot discard done-time causal facts");
    execute_provider(
        &mut provider,
        sample_projection("Return a second result.", "empty-terminal-replay-turn-2"),
    )
    .await
    .expect("done-time reasoning and text remain replayable");

    let _first_request = bodies.recv().await.unwrap();
    let replay: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let input = replay["input"].as_array().unwrap();
    let reasoning_index = input
        .iter()
        .position(|item| item["id"] == "rs_empty_terminal")
        .expect("sealed reasoning is retained without terminal adoption");
    let public_index = input
        .iter()
        .position(|item| {
            item["role"] == "assistant"
                && item.to_string().contains("<result value=\\\"alpha\\\" />")
        })
        .expect("sealed text is retained without terminal adoption");
    let projection_index = input
        .iter()
        .position(|item| item.to_string().contains("empty-terminal-replay-turn-2"))
        .expect("new input follows the sealed response output");
    assert!(reasoning_index < public_index);
    assert!(public_index < projection_index);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn completed_without_output_reconciles_already_sealed_public_history() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::CompletedWithoutTerminalOutput,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("completion metadata must not be the first public-output commit");
    assert_eq!(
        text_trace(events),
        vec![
            ("delta", String::from("<result value=\"alpha\" />")),
            ("complete", String::from("<result value=\"alpha\" />")),
        ]
    );
    execute_provider(
        &mut provider,
        sample_projection("Return a second result.", "no-terminal-output-turn-2"),
    )
    .await
    .expect("already sealed text is replayable without terminal output adoption");

    let _first = bodies.recv().await.unwrap();
    let replay: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(replay
        .to_string()
        .contains("<result value=\\\"alpha\\\" />"));
    assert!(replay.to_string().contains("no-terminal-output-turn-2"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn retained_private_output_overflow_preserves_causal_history_and_blocks_retry_before_http() {
    let projection = default_sample_projection();
    let (
        calibration_base,
        _calibration_attempts,
        mut calibration_bodies,
        calibration_shutdown,
        calibration_server,
    ) = spawn_server(ServerReply::SealedReasoningOnlyThenEof).await;
    let mut calibration = provider(calibration_base);
    let _ = expect_provider_failure(execute_provider(&mut calibration, projection.clone()).await);
    let initial_body = calibration_bodies
        .recv()
        .await
        .expect("calibration request body");
    calibration_shutdown.send(()).unwrap();
    calibration_server.await.unwrap();

    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::SealedReasoningOnlyThenEof).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(initial_body.len())
        .unwrap();
    let mut provider = provider_with_config(config);

    let first_fault =
        expect_provider_failure(execute_provider(&mut provider, projection.clone()).await);
    assert!(first_fault
        .to_string()
        .contains("without response.completed"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let retry_fault = match provider.execute(projection).await {
        Ok(_) => panic!("retained sealed output over the exact bound must block before handoff"),
        Err(fault) => fault,
    };
    assert_serialized_request_body_limit_fault(&retry_fault, "encrypted-reasoning-1");
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "retry sends no HTTP");

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn dropped_stream_retains_published_partial_with_interruption_after_compaction() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::CompactionWithText,
        ServerReply::MissingCompleted,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);
    let first_projection = default_sample_projection();

    execute_provider(&mut provider, first_projection.clone())
        .await
        .expect("first compacted response succeeds");
    let next_projection = sample_projection("Return a second result.", "turn-2");
    let strict_extension = strict_extension_projection(
        &first_projection,
        "<result value=\"alpha\" />",
        &next_projection,
    );
    let mut dropped = provider
        .execute(strict_extension.clone())
        .await
        .expect("second stream starts");
    assert!(dropped.next().await.transpose().unwrap().is_some());
    drop(dropped);

    execute_provider(&mut provider, strict_extension)
        .await
        .expect("retry keeps the published partial and its interruption fact");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let _first = bodies.recv().await.unwrap();
    let dropped: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(retry.get("previous_response_id").is_none());
    let dropped_input = dropped["input"].as_array().unwrap();
    let retry_input = retry["input"].as_array().unwrap();
    assert_eq!(&retry_input[..3], dropped_input);
    assert_eq!(retry_input.len(), 5);
    assert_eq!(retry_input[0]["type"], "compaction");
    assert!(retry_input[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn-2"));
    assert_eq!(retry_input[3]["role"], "assistant");
    assert!(retry_input[3].to_string().contains("<result value=\\\"al"));
    assert!(retry_input[4]
        .to_string()
        .contains("[agentview: assistant output interrupted before completion]"));

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn dropped_open_text_before_sealed_private_retries_in_output_index_order() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::OpenTextThenSealedReasoningEof,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);
    let projection = default_sample_projection();

    let mut dropped = provider
        .execute(projection.clone())
        .await
        .expect("the response stream starts");
    let partial = dropped
        .next()
        .await
        .expect("the visible partial is published")
        .expect("the visible partial is valid");
    assert!(matches!(
        partial,
        ProviderEvent::Text(TextTurnEvent::TextDelta(ref text)) if text == "visible-open-text-0"
    ));
    let fault = dropped
        .next()
        .await
        .expect("EOF without response.completed is surfaced")
        .expect_err("the unterminated response must fail");
    assert!(fault.message().contains("without response.completed"));
    drop(dropped);

    execute_provider(&mut provider, projection)
        .await
        .expect("retry compiles the aborted and sealed output history");

    let _dropped_request = bodies.recv().await.unwrap();
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retained_order = retry["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| {
            if item["id"] == "rs_after_open_text" {
                Some("sealed-private-1")
            } else if item.to_string().contains("visible-open-text-0") {
                Some("aborted-text-0")
            } else if item
                .to_string()
                .contains("[agentview: assistant output interrupted before completion]")
            {
                Some("interruption-marker")
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        retained_order,
        ["aborted-text-0", "sealed-private-1", "interruption-marker"]
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn drop_after_text_complete_retains_validated_context_before_public_eof() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::CompactionWithText,
        ServerReply::Completed,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);
    let first_projection = default_sample_projection();

    execute_provider(&mut provider, first_projection)
        .await
        .expect("first compacted response succeeds");
    let next_projection = sample_projection("Return a second result.", "turn-2");
    let mut dropped = provider
        .execute(next_projection.clone())
        .await
        .expect("second stream starts");
    let mut saw_complete = false;
    while let Some(event) = dropped.next().await.transpose().unwrap() {
        if matches!(event, ProviderEvent::Text(TextTurnEvent::TextComplete(_))) {
            saw_complete = true;
            break;
        }
    }
    assert!(
        saw_complete,
        "the stream yielded TextComplete before its EOF"
    );
    drop(dropped);

    execute_provider(&mut provider, next_projection)
        .await
        .expect("retry starts from the terminal context retained at response.completed");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let _first = bodies.recv().await.unwrap();
    let dropped_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retry_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_ne!(retry_request, dropped_request);
    assert!(retry_request.get("previous_response_id").is_none());
    let retry_input = retry_request["input"].as_array().unwrap();
    assert_eq!(retry_input.len(), 4);
    assert_eq!(retry_input[0]["type"], "compaction");
    assert!(retry_input[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("turn-2"));
    assert_eq!(retry_input[3]["role"], "assistant");

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn application_host_dispatches_delta_before_provider_emits_completed() {
    let notify = Arc::new(Notify::new());
    let complete_count = Arc::new(AtomicUsize::new(0));
    let complete_notify = Arc::new(Notify::new());
    let (api_base, attempts, _bodies, shutdown, server) = spawn_server_with_notify(
        ServerReply::WaitForDispatch,
        Some(Arc::clone(&notify)),
        None,
    )
    .await;
    let mut components = ComponentHost::new(
        dispatch_probe_root,
        DispatchProbeProps {
            delta_notify: notify,
            complete_count: Arc::clone(&complete_count),
            complete_notify: Arc::clone(&complete_notify),
        },
    );
    let mut host = ApplicationHost::new(dispatch_probe_provider(api_base));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("dispatch probe reaction completes");
    tokio::time::timeout(Duration::from_secs(1), complete_notify.notified())
        .await
        .expect("TextComplete reaches the async handler");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(complete_count.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn text_complete_waits_for_response_completed_before_port_emission() {
    let text_done = Arc::new(Notify::new());
    let release_completed = Arc::new(Notify::new());
    let (api_base, attempts, _bodies, shutdown, server) = spawn_server_with_notify(
        ServerReply::WaitAfterTextDone,
        Some(Arc::clone(&text_done)),
        Some(Arc::clone(&release_completed)),
    )
    .await;
    let mut provider = dispatch_probe_provider(api_base);
    let projection = {
        let mut components = ComponentHost::new(
            dispatch_probe_root,
            DispatchProbeProps {
                delta_notify: Arc::new(Notify::new()),
                complete_count: Arc::new(AtomicUsize::new(0)),
                complete_notify: Arc::new(Notify::new()),
            },
        );
        components
            .render()
            .expect("dispatch probe projection renders")
            .projection()
            .clone()
    };
    let mut stream = provider
        .execute(projection)
        .await
        .expect("delayed completion stream is established");

    let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("delta arrives before response.completed")
        .expect("delta event exists")
        .expect("delta event is valid");
    assert!(matches!(
        first,
        ProviderEvent::Text(TextTurnEvent::TextDelta(ref text)) if text == "<ok />"
    ));
    tokio::time::timeout(Duration::from_secs(1), text_done.notified())
        .await
        .expect("server emitted response.output_text.done");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .is_err(),
        "response.output_text.done must not emit TextComplete"
    );

    release_completed.notify_one();
    let complete = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("response.completed releases TextComplete")
        .expect("TextComplete event exists")
        .expect("TextComplete event is valid");
    assert!(matches!(
        complete,
        ProviderEvent::Text(TextTurnEvent::TextComplete(ref text)) if text == "<ok />"
    ));
    assert!(tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("stream reaches EOF after TextComplete")
        .is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn standard_output_item_lifecycle_frames_preserve_typed_text_completion() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::StandardOutputLifecycle).await;
    let mut provider = provider(api_base);

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("standard output lifecycle succeeds");

    assert_eq!(
        text_trace(events),
        vec![
            ("delta", String::from("<result value=\"al")),
            ("delta", String::from("pha\" />")),
            ("complete", String::from("<result value=\"alpha\" />")),
        ]
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_message_snapshots_cannot_supply_unsealed_text() {
    let cases = [
        (
            "completed-then-incomplete",
            ServerReply::ObservationalMessageCompletedThenIncomplete,
        ),
        (
            "incomplete-then-completed",
            ServerReply::ObservationalMessageIncompleteThenCompleted,
        ),
    ];

    for (name, reply) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );
        assert_eq!(
            fault.code(),
            ProviderFaultCode::ResponseEventShape,
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn response_completed_rejects_added_unsealed_private_lifecycles() {
    let cases = [
        ("reasoning", UnsealedPrivateKind::Reasoning),
        ("compaction", UnsealedPrivateKind::Compaction),
    ];

    for (name, kind) in cases {
        let (api_base, attempts, _bodies, shutdown, server) =
            spawn_server(ServerReply::UnsealedPrivateAtTerminal(kind)).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );
        assert_eq!(
            fault.code(),
            ProviderFaultCode::ResponseEventShape,
            "case {name}"
        );
        assert_eq!(
            fault
                .response_event_diagnostic()
                .and_then(|diagnostic| diagnostic.response_ledger_reason()),
            Some(ProviderResponseLedgerReason::OutputItemStatus),
            "case {name}"
        );
        let rendered_fault = format!("{fault:?}\n{}", fault.message());
        assert!(!rendered_fault.contains("sentinel"), "case {name}");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn reasoning_done_without_exact_encrypted_content_is_rejected_immediately() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ObservationalReasoning).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);
    let diagnostic = fault
        .response_event_diagnostic()
        .expect("invalid reasoning done remains a typed lifecycle fault");
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(
        diagnostic.event_type(),
        ProviderResponseEventType::ResponseOutputItemDone
    );
    assert_eq!(
        diagnostic.reason(),
        ProviderResponseEventReason::LifecycleIdentity
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn compaction_terminal_body_drift_is_rejected_after_item_done_seals_it() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ObservationalCompaction).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_message_defaults_optional_shape_fields_and_ignores_extensions() {
    let cases = [
        ("missing-id", TerminalMessageCompatibilityCase::MissingId),
        ("null-id", TerminalMessageCompatibilityCase::NullId),
        (
            "missing-status",
            TerminalMessageCompatibilityCase::MissingStatus,
        ),
        ("null-status", TerminalMessageCompatibilityCase::NullStatus),
        (
            "missing-role",
            TerminalMessageCompatibilityCase::MissingRole,
        ),
        ("null-role", TerminalMessageCompatibilityCase::NullRole),
        (
            "missing-annotations",
            TerminalMessageCompatibilityCase::MissingAnnotations,
        ),
        (
            "null-annotations",
            TerminalMessageCompatibilityCase::NullAnnotations,
        ),
        (
            "unknown-extensions",
            TerminalMessageCompatibilityCase::UnknownExtensions,
        ),
        (
            "missing-terminal-phase-after-explicit-final-answer",
            TerminalMessageCompatibilityCase::MissingTerminalPhaseAfterExplicitFinalAnswer,
        ),
        (
            "null-terminal-phase-after-explicit-final-answer",
            TerminalMessageCompatibilityCase::NullTerminalPhaseAfterExplicitFinalAnswer,
        ),
        (
            "explicit-final-answer-after-missing-lifecycle-phase",
            TerminalMessageCompatibilityCase::ExplicitFinalAnswerAfterMissingLifecyclePhase,
        ),
        (
            "explicit-final-answer-after-null-lifecycle-phase",
            TerminalMessageCompatibilityCase::ExplicitFinalAnswerAfterNullLifecyclePhase,
        ),
    ];
    let mut rejected = Vec::new();

    for (name, case) in cases {
        let (api_base, attempts, _bodies, shutdown, server) =
            spawn_server(ServerReply::TerminalMessageCompatibility(case)).await;
        let mut provider = provider(api_base);
        let execution = execute_provider(&mut provider, default_sample_projection()).await;

        match execution {
            Ok(events) => assert_eq!(
                text_trace(events),
                vec![
                    ("delta", String::from("<result value=\"alpha\" />")),
                    ("complete", String::from("<result value=\"alpha\" />")),
                ],
                "case {name}"
            ),
            Err(_) => rejected.push(name),
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    assert!(
        rejected.is_empty(),
        "terminal message compatibility cases rejected: {rejected:?}"
    );
}

#[tokio::test]
async fn final_phase_compatibility_retains_explicit_phase_in_continuation() {
    let cases = [
        (
            "observed-final-terminal-missing",
            TerminalMessageCompatibilityCase::MissingTerminalPhaseAfterExplicitFinalAnswer,
        ),
        (
            "observed-final-terminal-null",
            TerminalMessageCompatibilityCase::NullTerminalPhaseAfterExplicitFinalAnswer,
        ),
        (
            "observed-missing-terminal-final",
            TerminalMessageCompatibilityCase::ExplicitFinalAnswerAfterMissingLifecyclePhase,
        ),
        (
            "observed-null-terminal-final",
            TerminalMessageCompatibilityCase::ExplicitFinalAnswerAfterNullLifecyclePhase,
        ),
    ];

    for (name, case) in cases {
        let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
            ServerReply::TerminalMessageCompatibility(case),
            ServerReply::PlaceholderCompleted,
        ])
        .await;
        let mut provider = provider(api_base);

        execute_provider(&mut provider, diff_projection("input-a"))
            .await
            .unwrap_or_else(|fault| panic!("case {name} first response failed: {fault}"));
        execute_provider(&mut provider, diff_projection("input-b"))
            .await
            .unwrap_or_else(|fault| panic!("case {name} continuation failed: {fault}"));

        let _first_request = bodies.recv().await.unwrap();
        let continued_request: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(
            continued_request.get("previous_response_id").is_none(),
            "case {name}"
        );
        assert_eq!(
            continued_request["input"].as_array().unwrap().len(),
            3,
            "case {name}"
        );
        assert!(continued_request["input"][0]
            .to_string()
            .contains("input-a"));
        assert_eq!(continued_request["input"][1]["role"], "assistant");
        assert!(continued_request["input"][2]
            .to_string()
            .contains("input-b"));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn second_response_accepts_known_message_after_reconciled_non_text_prefix() {
    for (name, case) in [
        (
            "reconciled-reasoning-prefix",
            TerminalOutputIdentityCase::new(
                Some(TerminalOutputPrefix::Reasoning),
                TerminalMessageIdentity::Stable,
            ),
        ),
        (
            "reconciled-compaction-prefix",
            TerminalOutputIdentityCase::new(
                Some(TerminalOutputPrefix::Compaction),
                TerminalMessageIdentity::Stable,
            ),
        ),
    ] {
        let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
            ServerReply::PlaceholderCompleted,
            ServerReply::TerminalOutputIdentity(case),
            ServerReply::PlaceholderCompleted,
        ])
        .await;
        let mut provider = provider(api_base);

        execute_provider(&mut provider, diff_projection("input-a"))
            .await
            .expect("the first response succeeds");
        let events = execute_provider(&mut provider, diff_projection("input-b"))
            .await
            .unwrap_or_else(|fault| panic!("case {name} rejected: {fault}"));

        assert_eq!(
            text_trace(events),
            vec![
                ("delta", String::from("output-b")),
                ("complete", String::from("output-b")),
            ],
            "case {name}"
        );
        execute_provider(&mut provider, diff_projection("input-c"))
            .await
            .unwrap_or_else(|fault| panic!("case {name} continuation rejected: {fault}"));

        let _initial_request = bodies.recv().await.unwrap();
        let _second_request = bodies.recv().await.unwrap();
        let continued: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(
            continued.get("previous_response_id").is_none(),
            "case {name}"
        );
        let input = continued["input"].as_array().unwrap();
        assert!(input.len() > 2, "case {name}");
        if name == "reconciled-compaction-prefix" {
            assert_eq!(input[0]["type"], "compaction");
        } else {
            assert!(input[0].to_string().contains("input-a"));
        }
        assert!(input.last().unwrap().to_string().contains("input-c"));
        assert!(input
            .iter()
            .any(|item| item.to_string().contains("output-b")));
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn second_response_reprojects_a_missing_id_message_over_three_done_reasoning_items() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::PlaceholderCompleted,
        ServerReply::TerminalOutputIdentity(TerminalOutputIdentityCase::new(
            Some(TerminalOutputPrefix::ThreeReasoning),
            TerminalMessageIdentity::Missing,
        )),
        ServerReply::PlaceholderCompleted,
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, diff_projection("input-a"))
        .await
        .expect("placeholder first response succeeds");
    let events = execute_provider(&mut provider, diff_projection("input-b"))
        .await
        .expect("the exact ID-less second response is accepted");
    assert_eq!(
        text_trace(events),
        vec![
            ("delta", String::from("output-b")),
            ("complete", String::from("output-b")),
        ]
    );

    execute_provider(&mut provider, diff_projection("input-c"))
        .await
        .expect("accepted reconciled output remains continuable");

    let _first_request = bodies.recv().await.unwrap();
    let _second_request = bodies.recv().await.unwrap();
    let continued: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(continued.get("previous_response_id").is_none());
    let input = continued["input"].as_array().unwrap();
    assert!(input.len() > 3);
    assert!(input[0].to_string().contains("input-a"));
    assert!(input
        .iter()
        .any(|item| item.to_string().contains("output-b")));
    assert!(input.last().unwrap().to_string().contains("input-c"));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn second_response_rejects_missing_id_reprojection_across_phase_drift() {
    let (api_base, attempts, _bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::PlaceholderCompleted,
        ServerReply::TerminalOutputIdentity(TerminalOutputIdentityCase::commentary_phase_drift(
            TerminalMessageIdentity::Missing,
        )),
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, diff_projection("input-a"))
        .await
        .expect("placeholder first response succeeds");
    let fault =
        expect_provider_failure(execute_provider(&mut provider, diff_projection("input-b")).await);
    assert_eq!(
        fault
            .response_event_diagnostic()
            .and_then(|diagnostic| diagnostic.response_ledger_reason()),
        Some(ProviderResponseLedgerReason::MessageText)
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn exact_private_terminal_prefixes_allow_null_message_reprojection() {
    for (label, case) in [
        (
            "one-reasoning-prefix",
            TerminalOutputIdentityCase::new(
                Some(TerminalOutputPrefix::Reasoning),
                TerminalMessageIdentity::Null,
            ),
        ),
        (
            "two-reasoning-prefixes",
            TerminalOutputIdentityCase::new(
                Some(TerminalOutputPrefix::TwoReasoning),
                TerminalMessageIdentity::Null,
            ),
        ),
    ] {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_sequence_server(vec![
            ServerReply::PlaceholderCompleted,
            ServerReply::TerminalOutputIdentity(case),
        ])
        .await;
        let mut provider = provider(api_base);

        execute_provider(&mut provider, diff_projection("input-a"))
            .await
            .expect("placeholder first response succeeds");
        let events = execute_provider(&mut provider, diff_projection("input-b"))
            .await
            .unwrap_or_else(|fault| panic!("case {label} rejected an exact terminal: {fault}"));
        assert_eq!(
            text_trace(events),
            vec![
                ("delta", String::from("output-b")),
                ("complete", String::from("output-b")),
            ],
            "case {label}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "case {label}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn second_response_rejects_known_message_phase_drift() {
    let (api_base, attempts, _bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::PlaceholderCompleted,
        ServerReply::TerminalOutputIdentity(TerminalOutputIdentityCase::commentary_phase_drift(
            TerminalMessageIdentity::Stable,
        )),
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, diff_projection("input-a"))
        .await
        .expect("placeholder first response succeeds");
    let fault =
        expect_provider_failure(execute_provider(&mut provider, diff_projection("input-b")).await);

    assert_eq!(
        fault
            .response_event_diagnostic()
            .and_then(|diagnostic| diagnostic.response_ledger_reason()),
        Some(ProviderResponseLedgerReason::MessageText)
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn message_text_faults_retain_branch_complete_completed_reconciliation_structure() {
    let cases = [
        (
            TerminalMessageDiagnosticCase::InvalidShape,
            ProviderResponseMessageTextReason::InvalidTerminalMessageShape,
            "missing",
            "observed_only",
        ),
        (
            TerminalMessageDiagnosticCase::TextMismatch,
            ProviderResponseMessageTextReason::TerminalObservedTextMismatch,
            "array",
            "mismatch",
        ),
        (
            TerminalMessageDiagnosticCase::PhaseMismatch,
            ProviderResponseMessageTextReason::ApplicablePhaseMismatch,
            "array",
            "equal",
        ),
    ];

    for (case, expected_branch, expected_content_presence, expected_text_relation) in cases {
        let (api_base, attempts, _bodies, shutdown, server) =
            spawn_server(ServerReply::TerminalMessageDiagnostic(case)).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );
        let diagnostic = fault
            .response_event_diagnostic()
            .expect("message text failure remains a typed response diagnostic");
        let reconciliation = serde_json::to_value(
            diagnostic
                .response_completed_reconciliation()
                .expect("message text failure retains one structural snapshot"),
        )
        .unwrap();

        assert_eq!(
            diagnostic.response_message_text_reason(),
            Some(expected_branch)
        );
        assert_eq!(reconciliation["branch"], expected_branch.code());
        assert_eq!(reconciliation["response_created_sequence"], 1);
        assert_eq!(reconciliation["response_in_progress_sequence"], 2);
        assert_eq!(reconciliation["response_completed_sequence"], 9);
        assert_eq!(reconciliation["response_status"], "completed");
        assert_eq!(reconciliation["terminal_output_count"], 1);
        assert_eq!(reconciliation["observed_lifecycle_count"], 1);
        assert_eq!(reconciliation["terminal_output_index"], 0);
        assert_eq!(reconciliation["observed_lifecycle_index"], 0);
        assert_eq!(reconciliation["terminal_item_kind"], "message");
        assert_eq!(reconciliation["observed_item_kind"], "message");
        assert_eq!(reconciliation["terminal_item_status"], "completed");
        assert_eq!(reconciliation["observed_lifecycle_state"], "done");
        assert_eq!(reconciliation["terminal_id_presence"], "present");
        assert_eq!(reconciliation["observed_id_presence"], "present");
        assert_eq!(reconciliation["id_relation"], "equal");
        assert_eq!(reconciliation["mapping_basis"], "known_id");
        assert_eq!(
            reconciliation["terminal_content_presence"],
            expected_content_presence
        );
        assert_eq!(reconciliation["observed_text_state"], "completed");
        assert_eq!(reconciliation["text_relation"], expected_text_relation);
        assert_eq!(reconciliation["output_item_added_sequence"], 3);
        assert_eq!(reconciliation["content_part_added_sequence"], 4);
        assert_eq!(reconciliation["first_text_delta_sequence"], 5);
        assert_eq!(reconciliation["last_text_delta_sequence"], 5);
        assert_eq!(reconciliation["text_delta_count"], 1);
        assert_eq!(reconciliation["text_done_sequence"], 6);
        assert_eq!(reconciliation["content_part_done_sequence"], 7);
        assert_eq!(reconciliation["output_item_done_sequence"], 8);
        assert!(!fault.message().contains("observed-placeholder"));
        assert!(!fault.message().contains("terminal-placeholder"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn orphan_message_text_mismatch_retains_provider_structural_reconciliation() {
    let (api_base, attempts, _bodies, shutdown, server) = spawn_server(
        ServerReply::TerminalMessageDiagnostic(TerminalMessageDiagnosticCase::OrphanTextMismatch),
    )
    .await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    let diagnostic = fault
        .response_event_diagnostic()
        .expect("the terminal-only message remains a typed response fault");
    assert!(diagnostic.response_completed_reconciliation().is_none());
    assert!(!fault.message().contains("observed-placeholder"));
    assert!(!fault.message().contains("terminal-placeholder"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn second_response_rejects_present_unknown_or_regenerated_ids_after_reconciled_prefixes() {
    for (prefix_name, prefix) in [
        ("reasoning", TerminalOutputPrefix::Reasoning),
        ("compaction", TerminalOutputPrefix::Compaction),
    ] {
        for (id_name, message_identity) in [
            ("unknown", TerminalMessageIdentity::Unknown),
            ("regenerated", TerminalMessageIdentity::Regenerated),
        ] {
            let (api_base, attempts, _bodies, shutdown, server) = spawn_sequence_server(vec![
                ServerReply::PlaceholderCompleted,
                ServerReply::TerminalOutputIdentity(TerminalOutputIdentityCase::new(
                    Some(prefix),
                    message_identity,
                )),
            ])
            .await;
            let mut provider = provider(api_base);
            execute_provider(&mut provider, diff_projection("input-a"))
                .await
                .expect("placeholder first response succeeds");

            let fault = expect_provider_failure(
                execute_provider(&mut provider, diff_projection("input-b")).await,
            );
            let diagnostic = fault
                .response_event_diagnostic()
                .expect("same-ordinal identity conflict remains typed");
            assert_eq!(
                diagnostic.response_output_identity_reason(),
                Some(ProviderResponseOutputIdentityReason::SameIndexIdConflict),
                "{prefix_name}/{id_name}"
            );
            assert!(
                diagnostic.response_output_identity_detail().is_none(),
                "same-index identity drift has no invented kind-conflict detail: {prefix_name}/{id_name}"
            );
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                2,
                "{prefix_name}/{id_name}"
            );
            shutdown.send(()).unwrap();
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn second_response_rejects_a_regenerated_present_terminal_message_id() {
    let (api_base, attempts, _bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::Completed,
        ServerReply::TerminalOutputIdentity(TerminalOutputIdentityCase::new(
            None,
            TerminalMessageIdentity::Regenerated,
        )),
    ])
    .await;
    let mut provider = provider(api_base);
    execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("the first response succeeds");

    let fault = expect_provider_failure(
        execute_provider(
            &mut provider,
            sample_projection("Return a second result.", "identity-turn-2"),
        )
        .await,
    );
    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(
        fault
            .response_event_diagnostic()
            .and_then(|diagnostic| diagnostic.response_ledger_reason()),
        Some(ProviderResponseLedgerReason::OutputIdentity)
    );
    assert_eq!(
        fault
            .response_event_diagnostic()
            .and_then(|diagnostic| diagnostic.response_output_identity_reason()),
        Some(ProviderResponseOutputIdentityReason::SameIndexIdConflict)
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn private_body_drift_precedes_terminal_text_output_limit() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ObservationalCompaction).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(4_096, 4_096, 8)
        .unwrap();
    let mut provider = provider_with_config(config);

    let execution = trace_provider(&mut provider, default_sample_projection()).await;
    let fault = execution
        .fault
        .expect("private output body drift must fail before terminal text size accounting");

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert!(execution.events.is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn standard_output_text_annotation_is_ignored_and_terminal_response_is_replayable() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::StandardOutputAnnotationLifecycle,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("standard annotation lifecycle succeeds");
    assert_eq!(
        text_trace(events),
        vec![
            ("delta", String::from("<result value=\"alpha\" />")),
            ("complete", String::from("<result value=\"alpha\" />")),
        ],
        "annotation metadata must not emit model text"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let _initial_request = bodies.recv().await.unwrap();

    execute_provider(
        &mut provider,
        sample_projection(
            "Choose one move from the next complete projection.",
            "annotation-turn-2",
        ),
    )
    .await
    .expect("terminal annotations remain valid continuation output");
    let continued_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(!continued_request
        .to_string()
        .contains("annotation-canonicalization-secret"));
    assert!(continued_request.get("previous_response_id").is_none());
    assert_eq!(continued_request["input"].as_array().unwrap().len(), 3);
    assert_eq!(continued_request["input"][1]["role"], "assistant");
    assert!(continued_request["input"][2]
        .to_string()
        .contains("annotation-turn-2"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn output_text_annotation_null_index_is_a_payload_free_shape_rejection() {
    const SENTINEL: &str = "null-index-secret";
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::OutputTextAnnotationWithNullIndex).await;
    let mut provider = provider(api_base);

    let execution = trace_provider(&mut provider, default_sample_projection()).await;
    assert_eq!(
        text_trace(execution.events),
        vec![("delta", String::from("<result value=\"alpha\" />"))]
    );
    let fault = execution
        .fault
        .expect("schema-invalid annotation metadata must be rejected");
    assert_eq!(
        (fault.kind(), fault.code()),
        (
            ProviderFaultKind::ModelRejected,
            ProviderFaultCode::ResponseEventShape,
        )
    );
    assert_eq!(
        fault.message(),
        concat!(
            "OpenAI output text annotation event has an invalid shape ",
            "[response_event_type=response.output_text.annotation.added; ",
            "response_event_reason=ledger_mismatch]"
        )
    );
    assert!(!format!("{fault:?}\n{fault}").contains(SENTINEL));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn encrypted_reasoning_stays_private_without_breaking_projection_continuation() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::ReasoningWithText).await;
    let mut provider = provider(api_base);

    let first_events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("reasoning plus text response succeeds");
    assert_eq!(
        text_trace(first_events).last(),
        Some(&("complete", String::from("<result value=\"alpha\" />")))
    );
    execute_provider(
        &mut provider,
        sample_projection_with_policy(
            "Replacement rules wait until Provider context is Fresh.",
            "Choose one move from the next complete projection.",
            "reasoning-turn-2",
        ),
    )
    .await
    .expect("private reasoning does not make the complete projection incompatible");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let _first_request = bodies.recv().await.unwrap();
    let continued_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let instructions = continued_request["instructions"].as_str().unwrap();
    assert!(instructions.contains("Replacement rules wait until Provider context is Fresh."));
    assert!(!instructions.contains("Follow the response protocol."));
    let input = continued_request["input"].as_array().unwrap();
    assert!(continued_request.get("previous_response_id").is_none());
    assert_eq!(input.len(), 4);
    assert!(input[3]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("reasoning-turn-2"));
    assert!(continued_request
        .to_string()
        .contains("encrypted-reasoning-1"));
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn unknown_reasoning_extensions_are_not_retained_or_replayed() {
    const SENTINEL: &str = "unknown-reasoning-retention-secret-sentinel";
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::ReasoningWithUnknownExtensions).await;
    let mut provider = provider(api_base);
    let projection = default_sample_projection();

    execute_provider(&mut provider, projection.clone())
        .await
        .expect("unknown response extensions are tolerated");
    let _initial_request = bodies.recv().await.unwrap();
    let mut continued_stream = provider
        .execute(projection)
        .await
        .expect("canonical retained output dispatches");
    let _ = continued_stream.next().await;
    drop(continued_stream);
    let continued_body = bodies.recv().await.unwrap();
    assert!(!String::from_utf8_lossy(&continued_body).contains(SENTINEL));

    let continued_request: serde_json::Value = serde_json::from_slice(&continued_body).unwrap();
    assert!(continued_request.get("previous_response_id").is_none());
    let input = continued_request["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert!(input.iter().any(|item| item["type"] == "reasoning"));
    assert!(input.iter().any(|item| item["role"] == "assistant"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn reasoning_empty_content_arrays_complete_and_are_retained() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::ReasoningWithEmptyContent).await;
    let mut provider = provider(api_base);

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("empty reasoning content arrays are valid");
    assert_eq!(
        text_trace(events).last(),
        Some(&("complete", String::from("<result value=\"alpha\" />")))
    );
    execute_provider(
        &mut provider,
        sample_projection(
            "Choose one move from the next complete projection.",
            "empty-reasoning-content-turn-2",
        ),
    )
    .await
    .expect("empty reasoning content is retained for continuation");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let _first_request = bodies.recv().await.unwrap();
    let continued_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert!(continued_request.get("previous_response_id").is_none());
    assert_eq!(continued_request["input"].as_array().unwrap().len(), 4);
    assert!(continued_request["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "reasoning"));
    assert!(continued_request
        .to_string()
        .contains("encrypted-reasoning-1"));
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn reasoning_omitted_and_empty_terminal_spellings_reconcile_and_continue() {
    let cases = [
        (
            "done-omitted",
            ServerReply::ReasoningWithOmittedDoneEmptyCompletedContent,
            true,
        ),
        (
            "terminal-omitted",
            ServerReply::ReasoningWithEmptyDoneOmittedCompletedContent,
            false,
        ),
    ];

    for (name, reply, _retained_has_content) in cases {
        let (api_base, attempts, mut bodies, shutdown, server) =
            spawn_sequence_server(vec![reply, ServerReply::Completed]).await;
        let mut provider = provider(api_base);

        let events = execute_provider(&mut provider, default_sample_projection())
            .await
            .expect("omitted and empty reasoning spellings reconcile");
        assert_eq!(
            text_trace(events).last(),
            Some(&("complete", String::from("<result value=\"alpha\" />"))),
            "case {name}"
        );
        let _initial_request = bodies.recv().await.unwrap();

        execute_provider(
            &mut provider,
            sample_projection(
                "Choose one move from the next complete projection.",
                &format!("reasoning-equivalence-{name}"),
            ),
        )
        .await
        .expect("equivalent reasoning output is retained for continuation");
        let continued: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        assert!(continued.get("previous_response_id").is_none());
        assert_eq!(continued["input"].as_array().unwrap().len(), 4);
        assert!(continued["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning"));
        assert!(continued.to_string().contains("encrypted-reasoning-1"));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "case {name}");

        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn reasoning_null_content_is_rejected_at_each_lifecycle_boundary() {
    let cases = [
        ("added", ServerReply::ReasoningWithNullAddedContent),
        ("done", ServerReply::ReasoningWithNullDoneContent),
        ("completed", ServerReply::ReasoningWithNullCompletedContent),
    ];

    for (name, reply) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(
            fault.kind(),
            ProviderFaultKind::ModelRejected,
            "case {name}"
        );
        assert_eq!(
            fault.code(),
            ProviderFaultCode::ResponseEventShape,
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn plaintext_reasoning_content_is_rejected_as_unsupported_without_leakage() {
    let cases = [
        (
            "added",
            ServerReply::ReasoningWithPlaintextAddedContent,
            false,
        ),
        (
            "done",
            ServerReply::ReasoningWithPlaintextDoneContent,
            false,
        ),
        (
            "completed",
            ServerReply::ReasoningWithPlaintextCompletedContent,
            true,
        ),
    ];

    for (name, reply, retains_sealed_text) in cases {
        let (api_base, attempts, mut bodies, shutdown, server) =
            spawn_sequence_server(vec![reply, ServerReply::Completed]).await;
        let mut provider = provider(api_base);
        let projection = default_sample_projection();

        let fault =
            expect_provider_failure(execute_provider(&mut provider, projection.clone()).await);

        assert_eq!(
            fault.kind(),
            ProviderFaultKind::ModelRejected,
            "case {name}"
        );
        assert_eq!(
            fault.code(),
            ProviderFaultCode::ModelRejected,
            "case {name}"
        );
        assert_eq!(
            fault.message(),
            "plaintext OpenAI reasoning content is not supported by this adapter version",
            "case {name}"
        );
        assert!(
            !format!("{fault:?}\n{fault}").contains("plaintext-reasoning-secret"),
            "case {name}"
        );
        let rejected_request: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        execute_provider(&mut provider, projection)
            .await
            .expect("plaintext rejection retains only the already handed-off input");
        let retry_request: serde_json::Value =
            serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
        let rejected_input = rejected_request["input"].as_array().unwrap();
        let retry_input = retry_request["input"].as_array().unwrap();
        if retains_sealed_text {
            assert_eq!(
                &retry_input[..rejected_input.len()],
                &rejected_input[..],
                "case {name}"
            );
            let retained = &retry_input[rejected_input.len()..];
            assert_eq!(retained.len(), 2, "case {name}");
            assert_eq!(retained[0]["type"], "reasoning", "case {name}");
            assert_eq!(
                retained[0]["encrypted_content"], "encrypted-reasoning-1",
                "case {name}"
            );
            assert_eq!(retained[1]["role"], "assistant", "case {name}");
            assert!(retained[1]
                .to_string()
                .contains("<result value=\\\"alpha\\\" />"));
        } else {
            assert_eq!(retry_request, rejected_request, "case {name}");
        }
        assert!(
            !retry_request
                .to_string()
                .contains("plaintext-reasoning-secret"),
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

async fn assert_reasoning_text_stream_rejection(reply: ServerReply, sentinel: &str) {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_sequence_server(vec![reply, ServerReply::Completed]).await;
    let mut provider = provider(api_base);
    let projection = default_sample_projection();

    let execution = trace_provider(&mut provider, projection.clone()).await;
    assert!(
        execution.events.is_empty(),
        "plaintext reasoning must not publish partial or complete text"
    );
    let fault = execution
        .fault
        .expect("plaintext reasoning stream metadata must be rejected");
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
    assert_eq!(fault.code(), ProviderFaultCode::ModelRejected);
    assert_eq!(
        fault.message(),
        "plaintext OpenAI reasoning content is not supported by this adapter version"
    );
    assert!(!format!("{fault:?}\n{fault}").contains(sentinel));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "the adapter must not amplify a deterministic rejection"
    );

    let rejected_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    execute_provider(&mut provider, projection)
        .await
        .expect("reasoning stream rejection retains only the already handed-off input");
    let retry_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(retry_request, rejected_request);
    assert!(!retry_request.to_string().contains(sentinel));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn reasoning_text_delta_with_omitted_terminal_content_is_rejected_immediately() {
    assert_reasoning_text_stream_rejection(
        ServerReply::ReasoningTextDeltaWithOmittedTerminalContent,
        "plaintext-reasoning-delta-secret",
    )
    .await;
}

#[tokio::test]
async fn reasoning_text_done_with_empty_terminal_content_is_rejected_immediately() {
    assert_reasoning_text_stream_rejection(
        ServerReply::ReasoningTextDoneWithEmptyTerminalContent,
        "plaintext-reasoning-done-secret",
    )
    .await;
}

#[tokio::test]
async fn mixed_malformed_reasoning_content_is_typed_shape_rejected_without_leakage() {
    const SENTINELS: [&str; 2] = [
        "plaintext-reasoning-mixed-secret",
        "mixed-malformed-reasoning-secret",
    ];
    let cases = [
        (
            "added",
            ServerReply::ReasoningWithMixedMalformedAddedContent,
            "response.output_item.added",
        ),
        (
            "done",
            ServerReply::ReasoningWithMixedMalformedDoneContent,
            "response.output_item.done",
        ),
        (
            "completed",
            ServerReply::ReasoningWithMixedMalformedCompletedContent,
            "response.completed",
        ),
    ];

    for (name, reply, event_type) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let execution = trace_provider(&mut provider, default_sample_projection()).await;
        let public_text = text_trace(execution.events);
        assert!(
            public_text
                .iter()
                .all(|(_, text)| SENTINELS.iter().all(|sentinel| !text.contains(sentinel))),
            "case {name}"
        );
        assert!(
            public_text.iter().all(|(kind, _)| *kind != "complete"),
            "case {name}"
        );
        let fault = execution
            .fault
            .expect("mixed malformed reasoning content must be typed-shape rejected");
        assert_eq!(
            (fault.kind(), fault.code()),
            (
                ProviderFaultKind::ModelRejected,
                ProviderFaultCode::ResponseEventShape,
            ),
            "case {name}"
        );
        let reason = if name == "completed" {
            "ledger_mismatch"
        } else {
            "lifecycle_identity"
        };
        let ledger_reason = if matches!(name, "added" | "completed") {
            "; response_ledger_reason=output_item_shape"
        } else {
            ""
        };
        let output_item_shape_reason = if name == "completed" {
            " [response_output_item_shape_reason=reasoning]"
        } else {
            ""
        };
        assert_eq!(
            fault.message(),
            format!(
                "OpenAI output item has an invalid supported shape{output_item_shape_reason} \
                 [response_event_type={event_type}; response_event_reason={reason}{ledger_reason}]"
            ),
            "case {name}"
        );
        for sentinel in SENTINELS {
            assert!(
                !format!("{fault:?}\n{fault}").contains(sentinel),
                "case {name}"
            );
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");

        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn terminal_output_item_shape_fault_identifies_the_allowlisted_item_kind() {
    const SENTINELS: [&str; 2] = [
        "plaintext-reasoning-mixed-secret",
        "mixed-malformed-reasoning-secret",
    ];
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ReasoningWithMixedMalformedCompletedContent).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(
        fault.message(),
        "OpenAI output item has an invalid supported shape \
         [response_output_item_shape_reason=reasoning] \
         [response_event_type=response.completed; response_event_reason=ledger_mismatch; \
         response_ledger_reason=output_item_shape]"
    );
    assert_eq!(
        fault
            .response_event_diagnostic()
            .expect("terminal shape fault remains typed")
            .response_ledger_reason(),
        Some(ProviderResponseLedgerReason::OutputItemShape)
    );
    for sentinel in SENTINELS {
        assert!(!format!("{fault:?}\n{fault}").contains(sentinel));
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn diff_marked_component_state_is_lowered_against_the_accepted_responses_baseline() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, diff_projection("A"))
        .await
        .expect("first diff projection succeeds");
    let first: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(
        first["input"][0]["content"][0]["text"],
        "<current_state>A</current_state>"
    );

    execute_provider(&mut provider, diff_projection("A+"))
        .await
        .expect("second diff projection succeeds");
    let second: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let submitted = second["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(submitted, "<current_state>A+</current_state>");

    execute_provider(&mut provider, diff_projection("A"))
        .await
        .expect("third diff projection succeeds");
    let third: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let submitted = third["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(submitted, "<current_state>A</current_state>");

    execute_provider(&mut provider, diff_projection("A+"))
        .await
        .expect("repeated diff projection succeeds");
    let fourth: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let submitted = fourth["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(submitted, "<current_state>A+</current_state>");
    assert_eq!(attempts.load(Ordering::SeqCst), 4);

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn faulted_atomic_diff_submission_is_retained_and_scope_change_resends_full_projection() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::Completed,
        ServerReply::MissingCompleted,
        ServerReply::Completed,
    ])
    .await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, diff_projection("A"))
        .await
        .expect("accepted atomic baseline succeeds");
    let fault =
        expect_provider_failure(execute_provider(&mut provider, diff_projection("B")).await);
    assert!(fault.to_string().contains("without response.completed"));
    execute_provider(&mut provider, diff_projection("B"))
        .await
        .expect("retry preserves the handed-off B input and sends full for the new host scope");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let _accepted = bodies.recv().await.unwrap();
    let faulted: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let faulted_input = faulted["input"].as_array().unwrap();
    let retry_input = retry["input"].as_array().unwrap();
    assert_eq!(&retry_input[..3], faulted_input);
    assert_eq!(retry_input.len(), 5);
    assert_eq!(retry_input[3]["role"], "assistant");
    assert!(retry_input[3]
        .to_string()
        .contains("<result value=\\\"alpha\\\" />"));
    assert_eq!(
        retry_input.last().unwrap()["content"][0]["text"],
        "<current_state>B</current_state>"
    );

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn phased_messages_route_only_final_text_without_breaking_continuation() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::PhasedMultilineText).await;
    let mut provider = provider(api_base);

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("phased multiline response succeeds");
    assert_eq!(
        text_trace(events),
        vec![
            ("delta", String::from("<result\nvalue=\"alpha\" />")),
            ("complete", String::from("<result\nvalue=\"alpha\" />")),
        ]
    );
    execute_provider(
        &mut provider,
        sample_projection_with_policy(
            "Replacement rules wait until Provider context is Fresh.",
            "Choose one move from the next complete projection.",
            "commentary-turn-2",
        ),
    )
    .await
    .expect("private commentary does not make the complete projection incompatible");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let _first_request = bodies.recv().await.unwrap();
    let continued_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let input = continued_request["input"].as_array().unwrap();
    assert!(continued_request.get("previous_response_id").is_none());
    assert!(input.len() > 2);
    assert!(input.last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("commentary-turn-2"));
    assert!(input.iter().any(|item| item["role"] == "assistant"));
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn output_item_added_and_done_identity_drift_is_rejected() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::OutputItemIdentityDrift).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn http_faults_have_structural_sanitized_codes() {
    let cases = [
        (
            "authentication",
            ServerReply::Unauthorized,
            ProviderFaultKind::ModelRejected,
            ProviderFaultCode::Authentication,
            "authentication-secret-must-not-be-logged",
        ),
        (
            "authorization",
            ServerReply::Forbidden,
            ProviderFaultKind::ModelRejected,
            ProviderFaultCode::Authorization,
            "authorization-secret-must-not-be-logged",
        ),
        (
            "rate-limit",
            ServerReply::TooManyRequests,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::RateLimited,
            "rate-limit-secret-must-not-be-logged",
        ),
        (
            "upstream-status",
            ServerReply::ServiceUnavailable,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::UpstreamStatus,
            "upstream-secret-must-not-be-logged",
        ),
    ];

    for (name, reply, expected_kind, expected_code, secret) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(
            (fault.kind(), fault.code()),
            (expected_kind, expected_code),
            "case {name}"
        );
        assert!(!fault.to_string().contains(secret), "case {name}");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn response_failure_stages_have_distinct_payload_free_codes() {
    let cases = [
        (
            "http-status",
            ServerReply::ServiceUnavailable,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::UpstreamStatus,
        ),
        (
            "response-content-type",
            ServerReply::WrongContentType,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::ResponseContentType,
        ),
        (
            "sse-decode",
            ServerReply::InvalidUtf8Sse,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::StreamDecode,
        ),
        (
            "event-json",
            ServerReply::MalformedSse,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::ResponseEventJson,
        ),
        (
            "event-shape-json",
            ServerReply::EventMissingType,
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "event-shape",
            ServerReply::OutputItemIdentityDrift,
            ProviderFaultKind::ModelRejected,
            ProviderFaultCode::ResponseEventShape,
        ),
    ];

    for (name, reply, expected_kind, expected_code) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(
            (fault.kind(), fault.code()),
            (expected_kind, expected_code),
            "case {name}"
        );
        assert!(
            !fault.to_string().contains("event-shape-secret-sentinel"),
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn response_event_shape_fault_kind_and_code_policy_is_explicit() {
    let cases = [
        (
            "missing-type",
            ServerReply::EventMissingType,
            ProviderFaultKind::RetryableTransport,
        ),
        (
            "non-string-type",
            ServerReply::EventNonStringType,
            ProviderFaultKind::RetryableTransport,
        ),
        (
            "missing-sequence",
            ServerReply::EventMissingSequence,
            ProviderFaultKind::RetryableTransport,
        ),
        (
            "non-integer-sequence",
            ServerReply::EventNonIntegerSequence,
            ProviderFaultKind::RetryableTransport,
        ),
        (
            "missing-type-after-terminal",
            ServerReply::EventMissingTypeAfterTerminal,
            ProviderFaultKind::RetryableTransport,
        ),
        (
            "non-monotonic-sequence",
            ServerReply::NonMonotonicSequence,
            ProviderFaultKind::ModelRejected,
        ),
        (
            "valid-event-after-terminal",
            ServerReply::EventAfterTerminal,
            ProviderFaultKind::ModelRejected,
        ),
        (
            "output-identity-drift",
            ServerReply::OutputItemIdentityDrift,
            ProviderFaultKind::ModelRejected,
        ),
    ];

    for (name, reply, expected_kind) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(
            (fault.kind(), fault.code()),
            (expected_kind, ProviderFaultCode::ResponseEventShape),
            "case {name}"
        );
        assert!(
            !format!("{fault:?}\n{fault}").contains("event-shape-secret-sentinel"),
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn async_openai_does_not_hide_a_second_http_attempt() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ServiceUnavailable).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert!(!fault
        .to_string()
        .contains("upstream-secret-must-not-be-logged"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn transient_408_and_409_statuses_return_fault_without_a_hidden_retry() {
    let cases = [
        (
            "request-timeout",
            ServerReply::RequestTimeout,
            "request-timeout-secret-must-not-be-logged",
        ),
        (
            "conflict",
            ServerReply::Conflict,
            "conflict-secret-must-not-be-logged",
        ),
    ];

    for (name, reply, secret) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(
            (fault.kind(), fault.code()),
            (
                ProviderFaultKind::RetryableTransport,
                ProviderFaultCode::UpstreamStatus,
            ),
            "case {name}"
        );
        assert!(!fault.to_string().contains(secret), "case {name}");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn unknown_output_event_type_is_rejected_without_leaking_the_frame() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::UnknownOutputEvent).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert!(!fault.to_string().contains("unknown-secret-sentinel"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn response_event_shape_diagnostics_are_allowlisted_and_payload_free() {
    let cases = [
        (
            "envelope",
            ServerReply::EventMissingType,
            "response_event_type=unknown; response_event_reason=envelope",
        ),
        (
            "sequence",
            ServerReply::NonMonotonicSequence,
            "response_event_type=response.output_text.delta; response_event_reason=sequence",
        ),
        (
            "lifecycle-identity",
            ServerReply::OutputItemIdentityDrift,
            "response_event_type=response.output_item.done; response_event_reason=lifecycle_identity",
        ),
        (
            "ledger-mismatch",
            ServerReply::DeltaAfterTextDone,
            "response_event_type=response.output_text.delta; response_event_reason=ledger_mismatch",
        ),
        (
            "terminal-order",
            ServerReply::EventAfterTerminal,
            "response_event_type=response.output_text.delta; response_event_reason=terminal_order",
        ),
        (
            "unsupported-output",
            ServerReply::UnknownOutputEvent,
            "response_event_type=response.output_other; response_event_reason=unsupported_output",
        ),
        (
            "stream-completion",
            ServerReply::MissingCompleted,
            "response_event_type=stream_end; response_event_reason=stream_completion",
        ),
    ];

    for (name, reply, expected_diagnostic) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(
            fault.code(),
            ProviderFaultCode::ResponseEventShape,
            "case {name}"
        );
        assert!(fault.message().contains(expected_diagnostic), "case {name}");
        assert!(
            !format!("{fault:?}\n{fault}").contains("unknown-secret-sentinel"),
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn output_item_added_lifecycle_identity_has_a_closed_payload_free_subreason() {
    let cases = [
        (
            "output-index",
            OutputItemAddedLifecycleFault::OutputIndex,
            "output_index",
        ),
        (
            "missing-item",
            OutputItemAddedLifecycleFault::MissingItem,
            "output_item_missing",
        ),
        (
            "item-type",
            OutputItemAddedLifecycleFault::ItemType,
            "output_item_type",
        ),
        (
            "item-shape",
            OutputItemAddedLifecycleFault::ItemShape,
            "output_item_shape",
        ),
        (
            "item-id",
            OutputItemAddedLifecycleFault::ItemId,
            "output_item_id",
        ),
        (
            "item-status",
            OutputItemAddedLifecycleFault::ItemStatus,
            "output_item_status",
        ),
        (
            "output-identity",
            OutputItemAddedLifecycleFault::OutputIdentity,
            "output_identity",
        ),
    ];

    for (name, lifecycle_fault, expected_subreason) in cases {
        let (api_base, attempts, _bodies, shutdown, server) =
            spawn_server(ServerReply::OutputItemAddedLifecycle(lifecycle_fault)).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );
        let diagnostic = fault
            .response_event_diagnostic()
            .expect("output_item.added failure remains a typed diagnostic");

        assert_eq!(
            diagnostic.event_type(),
            ProviderResponseEventType::ResponseOutputItemAdded,
            "case {name}"
        );
        assert_eq!(
            diagnostic.reason(),
            ProviderResponseEventReason::LifecycleIdentity,
            "case {name}"
        );
        assert_eq!(
            diagnostic
                .response_ledger_reason()
                .map(ProviderResponseLedgerReason::code),
            Some(expected_subreason),
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn deterministic_response_fault_kind_and_code_matrix_is_non_retryable() {
    let cases = [
        (
            "body-limit",
            ServerReply::Completed,
            (128, 4_096, 4_096),
            ProviderFaultCode::ResponseBodyLimit,
            "<result value=",
        ),
        (
            "event-limit",
            ServerReply::OversizedEvent,
            (4_096, 128, 4_096),
            ProviderFaultCode::StreamEventLimit,
            "oversized-event-secret",
        ),
        (
            "output-limit",
            ServerReply::OutputOverflow,
            (4_096, 512, 8),
            ProviderFaultCode::OutputLimit,
            "output-overflow-secret",
        ),
        (
            "unsupported-output-event",
            ServerReply::UnknownOutputEvent,
            (4_096, 512, 4_096),
            ProviderFaultCode::ResponseEventShape,
            "unknown-secret-sentinel",
        ),
    ];

    let mut actual = Vec::with_capacity(cases.len());
    for (name, reply, (body_limit, event_limit, output_limit), expected_code, sentinel) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
            .unwrap()
            .with_response_limits(body_limit, event_limit, output_limit)
            .unwrap();
        let mut provider = provider_with_config(config);

        let execution = trace_provider(&mut provider, default_sample_projection()).await;
        let fault = execution
            .fault
            .expect("deterministic response failure must return a fault");

        actual.push((name, fault.kind(), fault.code()));
        assert_eq!(fault.code(), expected_code, "case {name}");
        assert!(
            !format!("{fault:?}\n{fault}").contains(sentinel),
            "case {name}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");

        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    assert_eq!(
        actual,
        vec![
            (
                "body-limit",
                ProviderFaultKind::ModelRejected,
                ProviderFaultCode::ResponseBodyLimit,
            ),
            (
                "event-limit",
                ProviderFaultKind::ModelRejected,
                ProviderFaultCode::StreamEventLimit,
            ),
            (
                "output-limit",
                ProviderFaultKind::ModelRejected,
                ProviderFaultCode::OutputLimit,
            ),
            (
                "unsupported-output-event",
                ProviderFaultKind::ModelRejected,
                ProviderFaultCode::ResponseEventShape,
            ),
        ]
    );
}

#[tokio::test]
async fn redirects_never_replay_the_frozen_body_or_bearer_token() {
    let (
        api_base,
        source_attempts,
        target_attempts,
        mut target_authorizations,
        mut target_bodies,
        shutdown,
        server,
    ) = spawn_redirect_server().await;
    let mut provider = provider(api_base);

    let _fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(source_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(target_attempts.load(Ordering::SeqCst), 0);
    assert!(target_authorizations.try_recv().is_err());
    assert!(target_bodies.try_recv().is_err());
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn explicit_api_base_ignores_ambient_openai_routing_headers() {
    if std::env::var_os(AMBIENT_HEADERS_CHILD_ENV).is_none() {
        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("explicit_api_base_ignores_ambient_openai_routing_headers")
            .arg("--exact")
            .arg("--nocapture")
            .env(AMBIENT_HEADERS_CHILD_ENV, "1")
            .env("OPENAI_API_KEY", "ambient-token")
            .env("OPENAI_ORG_ID", "invalid\norganization")
            .env("OPENAI_PROJECT_ID", "invalid\nproject")
            .status()
            .await
            .unwrap();
        assert!(
            status.success(),
            "isolated ambient-header regression child failed"
        );
        return;
    }

    let (api_base, mut headers, shutdown, server) = spawn_header_capture_server().await;
    let mut provider = provider(api_base);

    execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("explicit API configuration must complete a real request");

    let headers = headers
        .recv()
        .await
        .expect("loopback server received headers");
    assert_eq!(headers[header::AUTHORIZATION], "Bearer test-token");
    assert!(!headers.contains_key("openai-organization"));
    assert!(!headers.contains_key("openai-project"));
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn loopback_http_bypasses_environment_proxy_for_body_and_token() {
    if std::env::var_os(LOOPBACK_PROXY_CHILD_ENV).is_none() {
        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("loopback_http_bypasses_environment_proxy_for_body_and_token")
            .arg("--exact")
            .arg("--nocapture")
            .env(LOOPBACK_PROXY_CHILD_ENV, "1")
            .status()
            .await
            .unwrap();
        assert!(status.success(), "isolated proxy regression child failed");
        return;
    }

    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
    for key in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        std::env::set_var(key, &proxy_url);
    }
    for key in ["NO_PROXY", "no_proxy"] {
        std::env::set_var(key, "");
    }
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let captured_proxy_bytes = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::clone(&proxy_hits);
    let captured = Arc::clone(&captured_proxy_bytes);
    let proxy_task = tokio::spawn(async move {
        if let Ok((mut socket, _)) = proxy_listener.accept().await {
            hits.fetch_add(1, Ordering::SeqCst);
            let mut buffer = vec![0_u8; 16 * 1024];
            if let Ok(Ok(read)) =
                tokio::time::timeout(Duration::from_secs(1), socket.read(&mut buffer)).await
            {
                captured.lock().unwrap().extend_from_slice(&buffer[..read]);
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    });
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let mut provider = provider(api_base);

    let result = execute_provider(&mut provider, default_sample_projection()).await;

    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "loopback request reached the environment proxy with {} captured bytes",
        captured_proxy_bytes.lock().unwrap().len()
    );
    assert!(result.is_ok());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
    proxy_task.abort();
    let _ = proxy_task.await;
}

#[tokio::test]
async fn successful_non_sse_response_is_rejected_before_parsing() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::WrongContentType).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseContentType);
    assert!(fault.to_string().contains("successful non-SSE response"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[test]
fn transport_timeouts_and_response_limits_must_be_nonzero() {
    let config =
        AsyncOpenAiTransportConfig::new("https://api.openai.com/v1", "test-token").unwrap();
    assert!(config
        .with_timeouts(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(1)
        )
        .is_err());

    let config =
        AsyncOpenAiTransportConfig::new("https://api.openai.com/v1", "test-token").unwrap();
    assert!(config.with_response_limits(1, 0, 1).is_err());
}

#[tokio::test]
async fn request_preparation_fault_has_a_structural_code_without_authored_content() {
    let secret = "request-preparation-secret";
    let authored = Document::build(|block| {
        block.paragraph(|inline| inline.try_text(secret).unwrap());
    });
    let authored = resolve_artifact_document(authored).unwrap();
    let system = CanonicalInputItem::instruction(InstructionAuthority::System, authored);
    let projection = root_projection(vec![system.clone(), system]);
    let mut provider = provider("http://127.0.0.1:1/v1".to_owned());

    let fault = expect_provider_failure(execute_provider(&mut provider, projection).await);

    assert_eq!(fault.code(), ProviderFaultCode::RequestPreparation);
    assert!(!fault.to_string().contains(secret));
}

#[tokio::test]
async fn connection_failure_has_an_honest_secure_transport_bucket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let api_base = format!("http://{address}/v1");
    let mut provider = provider(api_base.clone());

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        execute_provider(&mut provider, default_sample_projection()),
    )
    .await
    .expect("loopback connection refusal did not complete");
    let fault = expect_provider_failure(result);

    assert_eq!(
        (fault.kind(), fault.code()),
        (
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::ConnectSecureTransport,
        )
    );
    assert!(!fault.to_string().contains(&api_base));
}

#[tokio::test]
async fn total_request_timeout_bounds_delayed_response_headers() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::DelayedHeaders).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(1),
            Duration::from_millis(50),
            Duration::from_secs(1),
        )
        .unwrap();
    let mut provider = provider_with_config(config);

    let mut stream = tokio::time::timeout(
        Duration::from_millis(100),
        provider.execute(default_sample_projection()),
    )
    .await
    .expect("Input Gate handoff returns the stream before response headers")
    .expect("delayed headers are a post-handoff stream failure");
    let fault = stream
        .next()
        .await
        .expect("post-handoff request timeout produces one stream item")
        .expect_err("delayed response headers must fail inside the stream");

    assert_eq!(
        (fault.kind(), fault.code()),
        (
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::RequestTimeout,
        )
    );
    assert!(fault.to_string().contains("transport failed"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn total_request_timeout_bounds_an_active_unterminated_stream() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ActiveUnterminatedStream).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(1),
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .unwrap();
    let mut provider = provider_with_config(config);

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        execute_provider(&mut provider, default_sample_projection()),
    )
    .await
    .expect("adapter total request timeout did not fire");
    let fault = expect_provider_failure(result);

    assert_eq!(
        (fault.kind(), fault.code()),
        (
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::StreamTimeout,
        )
    );
    assert!(fault.to_string().contains("streaming response"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn read_timeout_bounds_a_stalled_sse_stream_without_leaking_content() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::StalledStream).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_millis(50),
        )
        .unwrap();
    let mut provider = provider_with_config(config);

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        execute_provider(&mut provider, default_sample_projection()),
    )
    .await
    .expect("adapter read timeout did not fire");
    let fault = expect_provider_failure(result);
    assert_eq!(
        (fault.kind(), fault.code()),
        (
            ProviderFaultKind::RetryableTransport,
            ProviderFaultCode::StreamTimeout,
        )
    );
    let rendered = fault.to_string();

    assert!(
        rendered.contains("streaming response"),
        "unexpected fault: {rendered}"
    );
    assert!(!rendered.contains("stalled-stream-secret"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn response_body_limit_fails_closed_before_completed_is_accepted() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(128, 4_096, 4_096)
        .unwrap();
    let mut provider = provider_with_config(config);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseBodyLimit);
    assert!(fault.to_string().contains("configured body limit"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cumulative_body_limit_applies_without_a_content_length() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ChunkedOversizedBody).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(128, 4_096, 4_096)
        .unwrap();
    let mut provider = provider_with_config(config);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseBodyLimit);
    assert!(fault.to_string().contains("configured body limit"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn sse_event_limit_fails_closed_without_leaking_event_data() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::OversizedEvent).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(4_096, 128, 4_096)
        .unwrap();
    let mut provider = provider_with_config(config);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);
    assert_eq!(fault.code(), ProviderFaultCode::StreamEventLimit);
    let rendered = fault.to_string();

    assert!(rendered.contains("configured event limit"));
    assert!(!rendered.contains("oversized-event-secret"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn sse_event_limit_also_applies_to_done_sentinels() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::OversizedDoneEvent).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(4_096, 64, 4_096)
        .unwrap();
    let mut provider = provider_with_config(config);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);
    assert_eq!(fault.code(), ProviderFaultCode::StreamEventLimit);
    let rendered = fault.to_string();

    assert!(rendered.contains("configured event limit"));
    assert!(!rendered.contains("oversized-done-secret"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn sse_wire_event_limit_rejects_unterminated_comments_before_parsing() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::OversizedWireComment).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(4_096, 64, 4_096)
        .unwrap();
    let mut provider = provider_with_config(config);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);
    assert_eq!(fault.code(), ProviderFaultCode::StreamEventLimit);
    let rendered = fault.to_string();

    assert!(rendered.contains("configured event limit"));
    assert!(!rendered.contains("oversized-wire-comment-secret"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cumulative_output_text_limit_is_inclusive_at_256_kib() {
    let (exact_base, exact_attempts, _exact_bodies, exact_shutdown, exact_server) =
        spawn_server(ServerReply::OutputLimitExact).await;
    let exact_config = AsyncOpenAiTransportConfig::new(exact_base, "test-token")
        .unwrap()
        .with_response_limits(16 * 1024 * 1024, 2 * 1024 * 1024, 256 * 1024)
        .unwrap();
    let mut exact_provider = provider_with_config(exact_config);

    let exact_events = execute_provider(&mut exact_provider, default_sample_projection())
        .await
        .expect("exactly 256 KiB of cumulative output text is accepted");
    let exact_trace = text_trace(exact_events);
    assert_eq!(
        exact_trace
            .iter()
            .map(|(kind, text)| (*kind, text.len()))
            .collect::<Vec<_>>(),
        vec![("delta", 256 * 1024), ("complete", 256 * 1024)]
    );
    assert_eq!(exact_attempts.load(Ordering::SeqCst), 1);
    exact_shutdown.send(()).unwrap();
    exact_server.await.unwrap();

    let (overflow_base, overflow_attempts, _overflow_bodies, overflow_shutdown, overflow_server) =
        spawn_server(ServerReply::OutputLimitPlusOne).await;
    let overflow_config = AsyncOpenAiTransportConfig::new(overflow_base, "test-token")
        .unwrap()
        .with_response_limits(16 * 1024 * 1024, 2 * 1024 * 1024, 256 * 1024)
        .unwrap();
    let mut overflow_provider = provider_with_config(overflow_config);

    let overflow = trace_provider(&mut overflow_provider, default_sample_projection()).await;
    let fault = overflow
        .fault
        .expect("cumulative output limit plus one returns a provider fault");
    assert_eq!(fault.code(), ProviderFaultCode::OutputLimit);
    assert_eq!(
        text_trace(overflow.events)
            .iter()
            .map(|(kind, text)| (*kind, text.len()))
            .collect::<Vec<_>>(),
        vec![("delta", 256 * 1024)]
    );
    assert!(!format!("{fault:?}\n{fault}").contains('!'));
    assert_eq!(overflow_attempts.load(Ordering::SeqCst), 1);
    overflow_shutdown.send(()).unwrap();
    overflow_server.await.unwrap();
}

#[tokio::test]
async fn accumulated_output_limit_fails_closed_without_leaking_delta_text() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::OutputOverflow).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(4_096, 512, 8)
        .unwrap();
    let mut provider = provider_with_config(config);

    let execution = trace_provider(&mut provider, default_sample_projection()).await;
    let fault = execution
        .fault
        .expect("output overflow returns a provider fault");
    assert_eq!(fault.code(), ProviderFaultCode::OutputLimit);
    let rendered = fault.to_string();
    let emitted = text_trace(execution.events);

    assert!(rendered.contains("configured output limit"));
    assert!(!rendered.contains("output-overflow-secret"));
    assert_eq!(emitted, vec![("delta", String::from("12345678"))]);
    assert!(!emitted
        .iter()
        .any(|(_, text)| text.contains("output-overflow-secret")));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn eof_without_response_completed_returns_a_provider_fault() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::MissingCompleted).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert!(fault.to_string().contains("without response.completed"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn malformed_sse_is_redacted_before_error_and_tracing() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::MalformedSse).await;
    let mut provider = provider(api_base);
    let logs = Arc::new(Mutex::new(Vec::new()));

    let result = execute_provider(&mut provider, default_sample_projection())
        .with_subscriber(capture_subscriber(Arc::clone(&logs)))
        .await;
    let fault = expect_provider_failure(result);
    let rendered_logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventJson);
    assert!(!fault.to_string().contains("malformed-sse-secret-sentinel"));
    assert!(!rendered_logs.contains("malformed-sse-secret-sentinel"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_arguments_delta_without_added_item_fails_closed() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::NativeTool).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_terminal_reports_validated_usage_exactly_once() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::NativeToolLifecycleToolOnlyWithUsage).await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_by_provider = Arc::clone(&observed);
    let mut provider = provider(api_base).with_response_usage_observer(move |usage| {
        observed_by_provider
            .lock()
            .unwrap()
            .push((usage.input_tokens(), usage.cached_tokens()));
    });

    let events = execute_provider(&mut provider, default_sample_projection())
        .await
        .expect("native function-call terminal remains accepted");

    assert!(events
        .iter()
        .any(|event| matches!(event, ProviderEvent::ToolCall(_))));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(*observed.lock().unwrap(), vec![(Some(144), Some(128))]);

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_lifecycle_stages_one_output_for_the_next_gate_submission() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::NativeToolLifecycleToolOnly,
        ServerReply::Completed,
    ])
    .await;
    let mut components = ComponentHost::new(
        native_tool_lifecycle_application,
        NativeToolLifecycleProps {
            projection: String::from("first lifecycle projection"),
        },
    );
    let mut host = ApplicationHost::new(provider(api_base));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("a complete native function-call lifecycle is dispatched to its bound handler");
    components.set_props(NativeToolLifecycleProps {
        projection: String::from("second lifecycle projection"),
    });
    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("the following request closes the staged native tool call");

    let first: serde_json::Value = serde_json::from_slice(
        &bodies
            .recv()
            .await
            .expect("first native lifecycle request body"),
    )
    .expect("first native lifecycle request is JSON");
    let second: serde_json::Value = serde_json::from_slice(
        &bodies
            .recv()
            .await
            .expect("second native lifecycle request body"),
    )
    .expect("second native lifecycle request is JSON");

    assert_eq!(first["tools"][0]["name"], "lifecycle_lookup");
    let input = second["input"]
        .as_array()
        .expect("second native lifecycle request has wire input");
    let call_position = input
        .iter()
        .position(|item| {
            item["type"] == "function_call"
                && item["call_id"] == "call_lifecycle_lookup"
                && item["name"] == "lifecycle_lookup"
                && item["arguments"] == "{}"
        })
        .expect("the sealed ToolCall is retained before the next request");
    let output = input
        .get(call_position + 1)
        .expect("the ToolOutput immediately follows its ToolCall");
    assert_eq!(output["type"], "function_call_output");
    assert_eq!(output["call_id"], "call_lifecycle_lookup");
    assert_eq!(output["output"], "lifecycle-tool-result");
    assert_eq!(
        input
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "call_lifecycle_lookup"
            })
            .count(),
        1,
        "the Gate consumes exactly the handler result it snapshotted"
    );
    assert!(
        input
            .iter()
            .skip(call_position + 2)
            .any(|item| item.to_string().contains("second lifecycle projection")),
        "new projection input follows the ordered ToolCall closure"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn execute_primes_transport_before_returning_an_unpolled_responses_stream() {
    let (api_base, attempts, mut bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let mut provider = provider(api_base);

    let unpolled = provider
        .execute(assistant_projection("handed-off-input-is-retained"))
        .await
        .expect("the returned stream represents an already-polled transport handoff");
    drop(unpolled);

    let mut active = provider
        .execute(assistant_projection("actual-input-after-unpolled-drop"))
        .await
        .expect("the next request replays the handed-off causal input");
    let _ = active.next().await;
    drop(active);

    let first_body: serde_json::Value = serde_json::from_slice(
        &bodies
            .recv()
            .await
            .expect("at least the next polled request reaches the test server"),
    )
    .unwrap();
    let replayed = if first_body
        .to_string()
        .contains("actual-input-after-unpolled-drop")
    {
        first_body
    } else {
        serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(1), bodies.recv())
                .await
                .expect("the next polled request is not blocked by a primed cancelled request")
                .expect("the next polled request reaches the test server"),
        )
        .unwrap()
    };
    let replay = replayed.to_string();
    assert!(replay.contains("handed-off-input-is-retained"));
    assert!(replay.contains("actual-input-after-unpolled-drop"));
    assert!(
        (1..=2).contains(&attempts.load(Ordering::SeqCst)),
        "the primed request may be cancelled before or after loopback receipt"
    );
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn application_host_observes_responses_submission_after_execute_handoff() {
    let (api_base, _attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::Completed).await;
    let execute_returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = ExecuteReturnProbe {
        inner: provider(api_base),
        returned: Arc::clone(&execute_returned),
    };
    let mut components = ComponentHost::new(
        native_tool_lifecycle_application,
        NativeToolLifecycleProps {
            projection: String::from("observer causal handoff projection"),
        },
    );
    let mut host = ApplicationHost::new(probe).with_observer(SubmissionAfterExecuteObserver {
        execute_returned: Arc::clone(&execute_returned),
    });

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("the real Responses provider primes its private Gate before observer submission");
    assert!(execute_returned.load(Ordering::SeqCst));
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn staged_native_tool_output_survives_local_gate_limit_failure_and_closes_on_retry() {
    let (api_base, attempts, mut bodies, shutdown, server) = spawn_sequence_server(vec![
        ServerReply::NativeToolLifecycleToolOnly,
        ServerReply::Completed,
    ])
    .await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_responses_serialized_request_body_limit(4 * 1024)
        .unwrap();
    let mut components = ComponentHost::new(
        native_tool_lifecycle_application,
        NativeToolLifecycleProps {
            projection: String::from("first staged-output projection"),
        },
    );
    let mut host = ApplicationHost::new(provider_with_config(config));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("the native tool handler stages its output");
    let _first = bodies.recv().await.expect("first request body");
    components.set_props(NativeToolLifecycleProps {
        projection: "local-gate-limit-sentinel".repeat(256),
    });
    let fault = host
        .dispatch_llm_reaction(&mut components)
        .await
        .expect_err("the oversized second submission fails before HTTP handoff");
    let ApplicationHostFault::ProviderSetup(fault) = fault else {
        panic!("unexpected local Gate failure: {fault:?}");
    };
    assert_serialized_request_body_limit_fault(&fault, "local-gate-limit-sentinel");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    components.set_props(NativeToolLifecycleProps {
        projection: String::from("retry after local gate failure"),
    });
    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("the unchanged staged result closes the next valid request");
    let retry: serde_json::Value = serde_json::from_slice(
        &bodies
            .recv()
            .await
            .expect("retry request body after local failure"),
    )
    .unwrap();
    let input = retry["input"].as_array().unwrap();
    let call_index = input
        .iter()
        .position(|item| {
            item["type"] == "function_call" && item["call_id"] == "call_lifecycle_lookup"
        })
        .expect("the retained native ToolCall precedes its staged closure");
    assert_eq!(input[call_index + 1]["type"], "function_call_output");
    assert_eq!(input[call_index + 1]["call_id"], "call_lifecycle_lookup");
    assert_eq!(input[call_index + 1]["output"], "lifecycle-tool-result");
    assert_eq!(
        input
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "call_lifecycle_lookup"
            })
            .count(),
        1,
        "the retry consumes the staged result exactly once"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_item_done_without_arguments_done_fails_closed() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::NativeToolItemWithText).await;
    let mut provider = provider(api_base);
    let logs = Arc::new(Mutex::new(Vec::new()));

    let result = execute_provider(&mut provider, default_sample_projection())
        .with_subscriber(capture_subscriber(Arc::clone(&logs)))
        .await;
    let fault = expect_provider_failure(result);
    let rendered_logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();

    assert_native_tool_lifecycle_fault(&fault, "response.output_item.done");
    assert!(!fault.to_string().contains("native-secret-tool"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(!rendered_logs.contains("native-secret-tool"));
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_added_without_output_index_fails_before_lifecycle_identity() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::NativeToolMissingOutputIndex).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_native_tool_lifecycle_fault(&fault, "response.output_item.added");
    assert!(!fault.to_string().contains("native-secret-missing-index"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn unsupported_content_part_precedes_malformed_shape() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::UnsupportedContentPartMissingFields).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ModelRejected);
    assert_eq!(
        fault.message(),
        "OpenAI message content part is not supported output text"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_done_without_added_or_extra_terminal_call_fails_closed() {
    let cases = [
        (
            "done-without-added",
            ServerReply::NativeToolDoneWithText,
            "native-secret-done-tool",
            "response.output_item.done",
        ),
        (
            "extra-terminal-only-call",
            ServerReply::NativeToolOnlyInCompletedOutput,
            "native-secret-terminal-tool",
            "response.completed",
        ),
    ];

    for (name, reply, secret, event_type) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);
        let logs = Arc::new(Mutex::new(Vec::new()));

        let result = execute_provider(&mut provider, default_sample_projection())
            .with_subscriber(capture_subscriber(Arc::clone(&logs)))
            .await;
        let fault = expect_provider_failure(result);
        let rendered_logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();

        assert_native_tool_lifecycle_fault(&fault, event_type);
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        assert!(!fault.to_string().contains(secret), "case {name}");
        assert!(!rendered_logs.contains(secret), "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn terminal_native_tool_lifecycle_failure_precedes_message_identity_drift() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::TerminalIdentityDriftWithNativeTool).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_native_tool_lifecycle_fault(&fault, "response.completed");
    assert!(!fault.to_string().contains("native-secret-combined-tool"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn delta_after_text_done_is_a_redacted_protocol_fault() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::DeltaAfterTextDone).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert!(!fault.message().contains("late-delta-secret"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn response_completed_ledger_fault_has_a_closed_payload_free_subreason() {
    let (api_base, attempts, _bodies, shutdown, server) =
        spawn_server(ServerReply::ResponseIdentityDrift).await;
    let mut provider = provider(api_base);

    let fault =
        expect_provider_failure(execute_provider(&mut provider, default_sample_projection()).await);

    assert_eq!(fault.code(), ProviderFaultCode::ResponseEventShape);
    assert!(fault.message().ends_with(
        "[response_event_type=response.completed; response_event_reason=ledger_mismatch; \
         response_ledger_reason=response_identity]"
    ));
    let diagnostic = fault
        .response_event_diagnostic()
        .expect("the response event diagnostic remains typed");
    assert_eq!(
        diagnostic.event_type(),
        ProviderResponseEventType::ResponseCompleted
    );
    assert_eq!(
        diagnostic.reason(),
        ProviderResponseEventReason::LedgerMismatch
    );
    assert_eq!(
        diagnostic.response_ledger_reason(),
        Some(ProviderResponseLedgerReason::ResponseIdentity)
    );
    assert!(!fault.message().contains("resp_original"));
    assert!(!fault.message().contains("resp_changed"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_terminal_and_ordering_matrix_is_rejected() {
    let cases = [
        (
            "failed",
            ServerReply::Failed,
            ProviderFaultCode::ModelRejected,
        ),
        (
            "incomplete",
            ServerReply::Incomplete,
            ProviderFaultCode::ModelRejected,
        ),
        (
            "error",
            ServerReply::Error,
            ProviderFaultCode::ModelRejected,
        ),
        (
            "duplicate-done",
            ServerReply::DuplicateDone,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "duplicate-terminal",
            ServerReply::DuplicateTerminal,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "event-after-terminal",
            ServerReply::EventAfterTerminal,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "non-monotonic-sequence",
            ServerReply::NonMonotonicSequence,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "mismatched-text-identity",
            ServerReply::MismatchedTextIdentity,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "response-identity-drift",
            ServerReply::ResponseIdentityDrift,
            ProviderFaultCode::ResponseEventShape,
        ),
        (
            "content-part-drift",
            ServerReply::ContentPartDrift,
            ProviderFaultCode::ResponseEventShape,
        ),
    ];

    for (name, reply, expected) in cases {
        let (api_base, attempts, _bodies, shutdown, server) = spawn_server(reply).await;
        let mut provider = provider(api_base);

        let fault = expect_provider_failure(
            execute_provider(&mut provider, default_sample_projection()).await,
        );

        assert_eq!(fault.code(), expected, "case {name}");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "case {name}");
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }
}
