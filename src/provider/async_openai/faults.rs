use crate::component::execution::{
    ProviderFault, ProviderFaultCode, ProviderFaultKind, ProviderResponseCompletedReconciliation,
    ProviderResponseLedgerReason, ProviderResponseMessageTextReason,
    ProviderResponseOutputIdentityDetail, ProviderResponseOutputIdentityReason,
};
use serde_json::{Map, Value};

#[derive(Clone, Copy)]
pub(super) enum OpenAiApi {
    Responses,
    ChatCompletions,
}

#[derive(Debug)]
pub(super) enum OpenAiBodyStreamFault {
    ClassifiedTransport(ProviderFaultCode),
    BodyLimit,
    EventLimit,
}

pub(super) fn request_transport_fault(api: OpenAiApi, error: &reqwest::Error) -> ProviderFault {
    let code = if error.is_timeout() {
        ProviderFaultCode::RequestTimeout
    } else if error.is_connect() {
        ProviderFaultCode::ConnectSecureTransport
    } else if error.is_builder() {
        ProviderFaultCode::RequestPreparation
    } else if error.is_request() {
        ProviderFaultCode::RequestTransport
    } else {
        ProviderFaultCode::Transport
    };
    let message = match api {
        OpenAiApi::Responses => "OpenAI API transport failed",
        OpenAiApi::ChatCompletions => "Chat Completions API transport failed",
    };
    ProviderFault::retryable_transport(message).with_code(code)
}

pub(super) fn stream_error_code(error: &reqwest::Error) -> ProviderFaultCode {
    if error.is_timeout() {
        ProviderFaultCode::StreamTimeout
    } else if error.is_decode() {
        ProviderFaultCode::ResponseProtocol
    } else {
        ProviderFaultCode::StreamTransport
    }
}

pub(super) fn stream_transport_fault(api: OpenAiApi, code: ProviderFaultCode) -> ProviderFault {
    let message = match api {
        OpenAiApi::Responses => "OpenAI API returned an invalid streaming response",
        OpenAiApi::ChatCompletions => "Chat Completions API returned an invalid streaming response",
    };
    ProviderFault::retryable_transport(message).with_code(code)
}

pub(super) fn response_body_limit_fault(api: OpenAiApi) -> ProviderFault {
    let message = match api {
        OpenAiApi::Responses => "OpenAI API response exceeded configured body limit",
        OpenAiApi::ChatCompletions => {
            "Chat Completions API response exceeded configured body limit"
        }
    };
    ProviderFault::model_rejected(message).with_code(ProviderFaultCode::ResponseBodyLimit)
}

pub(super) fn stream_event_limit_fault(api: OpenAiApi) -> ProviderFault {
    let message = match api {
        OpenAiApi::Responses => "OpenAI SSE event exceeded configured event limit",
        OpenAiApi::ChatCompletions => "Chat Completions SSE event exceeded configured event limit",
    };
    ProviderFault::model_rejected(message).with_code(ProviderFaultCode::StreamEventLimit)
}

pub(super) fn output_limit_fault(api: OpenAiApi) -> ProviderFault {
    let message = match api {
        OpenAiApi::Responses => "OpenAI output text exceeded configured output limit",
        OpenAiApi::ChatCompletions => {
            "Chat Completions output text exceeded configured output limit"
        }
    };
    ProviderFault::model_rejected(message).with_code(ProviderFaultCode::OutputLimit)
}

pub(super) fn serialized_request_body_limit_fault() -> ProviderFault {
    ProviderFault::model_rejected(
        "OpenAI Responses serialized outbound request body exceeded configured limit",
    )
    .with_code(ProviderFaultCode::RequestPreparation)
}

pub(super) fn redacted_status_fault(api: OpenAiApi, status: reqwest::StatusCode) -> ProviderFault {
    let code = match status {
        reqwest::StatusCode::UNAUTHORIZED => ProviderFaultCode::Authentication,
        reqwest::StatusCode::FORBIDDEN => ProviderFaultCode::Authorization,
        reqwest::StatusCode::TOO_MANY_REQUESTS => ProviderFaultCode::RateLimited,
        _ => ProviderFaultCode::UpstreamStatus,
    };
    let message = match api {
        OpenAiApi::Responses => format!(
            "OpenAI API request failed with HTTP status {}",
            status.as_u16()
        ),
        OpenAiApi::ChatCompletions => format!(
            "Chat Completions API request failed with HTTP status {}",
            status.as_u16()
        ),
    };
    let fault = if matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error() {
        ProviderFault::retryable_transport(message)
    } else {
        ProviderFault::model_rejected(message)
    };
    fault.with_code(code)
}

#[derive(Clone, Copy)]
pub(super) enum ResponseEventReason {
    Envelope,
    Sequence,
    LifecycleIdentity,
    LedgerMismatch,
    TerminalOrder,
    UnsupportedOutput,
    StreamCompletion,
}

impl ResponseEventReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Envelope => "envelope",
            Self::Sequence => "sequence",
            Self::LifecycleIdentity => "lifecycle_identity",
            Self::LedgerMismatch => "ledger_mismatch",
            Self::TerminalOrder => "terminal_order",
            Self::UnsupportedOutput => "unsupported_output",
            Self::StreamCompletion => "stream_completion",
        }
    }
}

struct ResponseEventShapeDiagnosis {
    event_type: &'static str,
    reason: ResponseEventReason,
    response_ledger_reason: Option<ProviderResponseLedgerReason>,
    response_message_text_reason: Option<ProviderResponseMessageTextReason>,
    response_completed_reconciliation: Option<ProviderResponseCompletedReconciliation>,
    response_output_identity_reason: Option<ProviderResponseOutputIdentityReason>,
    response_output_identity_detail: Option<ProviderResponseOutputIdentityDetail>,
}

impl ResponseEventShapeDiagnosis {
    fn new(event_type: &'static str, reason: ResponseEventReason) -> Self {
        Self {
            event_type,
            reason,
            response_ledger_reason: None,
            response_message_text_reason: None,
            response_completed_reconciliation: None,
            response_output_identity_reason: None,
            response_output_identity_detail: None,
        }
    }
}

pub(super) fn response_event_shape_fault(
    event_type: &str,
    reason: ResponseEventReason,
    fault: ProviderFault,
) -> ProviderFault {
    diagnosed_response_event_shape_fault(
        ResponseEventShapeDiagnosis::new(allowlisted_response_event_type(event_type), reason),
        fault,
    )
}

pub(super) fn response_completed_ledger_fault(
    reason: ProviderResponseLedgerReason,
    message_text_reason: Option<ProviderResponseMessageTextReason>,
    response_completed_reconciliation: Option<ProviderResponseCompletedReconciliation>,
    output_identity_reason: Option<ProviderResponseOutputIdentityReason>,
    output_identity_detail: Option<ProviderResponseOutputIdentityDetail>,
    fault: ProviderFault,
) -> ProviderFault {
    diagnosed_response_event_shape_fault(
        ResponseEventShapeDiagnosis {
            response_ledger_reason: Some(reason),
            response_message_text_reason: message_text_reason,
            response_completed_reconciliation,
            response_output_identity_reason: output_identity_reason,
            response_output_identity_detail: output_identity_detail,
            ..ResponseEventShapeDiagnosis::new(
                "response.completed",
                ResponseEventReason::LedgerMismatch,
            )
        },
        fault,
    )
}

pub(super) fn response_output_item_added_fault(
    reason: ProviderResponseLedgerReason,
    fault: ProviderFault,
) -> ProviderFault {
    diagnosed_response_event_shape_fault(
        ResponseEventShapeDiagnosis {
            response_ledger_reason: Some(reason),
            ..ResponseEventShapeDiagnosis::new(
                "response.output_item.added",
                ResponseEventReason::LifecycleIdentity,
            )
        },
        fault,
    )
}

pub(super) fn response_event_envelope_fault(message: &'static str) -> ProviderFault {
    diagnosed_response_event_shape_fault(
        ResponseEventShapeDiagnosis::new("unknown", ResponseEventReason::Envelope),
        ProviderFault::retryable_transport(message),
    )
}

pub(super) fn response_stream_completion_fault(fault: ProviderFault) -> ProviderFault {
    diagnosed_response_event_shape_fault(
        ResponseEventShapeDiagnosis::new("stream_end", ResponseEventReason::StreamCompletion),
        fault,
    )
}

pub(super) fn required_string(
    payload: &Map<String, Value>,
    field: &str,
) -> Result<String, ProviderFault> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ProviderFault::model_rejected(format!(
                "OpenAI streaming event is missing string field {field}"
            ))
            .with_code(ProviderFaultCode::ResponseEventShape)
        })
}

pub(super) fn unsupported_content_part_fault() -> ProviderFault {
    ProviderFault::model_rejected("OpenAI message content part is not supported output text")
}

pub(super) fn unsupported_output_item_fault() -> ProviderFault {
    ProviderFault::model_rejected(
        "native or unsupported OpenAI output items are not supported by this adapter version",
    )
}

pub(super) fn unsupported_reasoning_content_fault() -> ProviderFault {
    ProviderFault::model_rejected(
        "plaintext OpenAI reasoning content is not supported by this adapter version",
    )
}

pub(super) fn has_unsupported_lifecycle_item(payload: &Map<String, Value>) -> bool {
    payload
        .get("item")
        .is_some_and(has_explicitly_unsupported_item_type)
}

pub(super) fn has_unsupported_content_part(payload: &Map<String, Value>) -> bool {
    payload
        .get("part")
        .and_then(Value::as_object)
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|item_type| item_type != "output_text")
}

pub(super) fn has_unsupported_completed_item(payload: &Map<String, Value>) -> bool {
    payload
        .get("response")
        .and_then(Value::as_object)
        .and_then(|response| response.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(has_explicitly_unsupported_item_type))
}

pub(super) fn has_plaintext_reasoning_lifecycle_content(payload: &Map<String, Value>) -> bool {
    payload
        .get("item")
        .is_some_and(has_plaintext_reasoning_content)
}

pub(super) fn has_plaintext_reasoning_completed_content(payload: &Map<String, Value>) -> bool {
    payload
        .get("response")
        .and_then(Value::as_object)
        .and_then(|response| response.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(has_plaintext_reasoning_content))
}

fn has_plaintext_reasoning_content(item: &Value) -> bool {
    let Some(item) = item.as_object() else {
        return false;
    };
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return false;
    }
    item.get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            !content.is_empty()
                && content.iter().all(|part| {
                    part.get("type").and_then(Value::as_str) == Some("reasoning_text")
                        && part.get("text").is_some_and(Value::is_string)
                })
        })
}

fn has_explicitly_unsupported_item_type(item: &Value) -> bool {
    item.as_object()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|item_type| !matches!(item_type, "message" | "reasoning" | "compaction"))
}

pub(super) fn is_native_tool_event(event_type: &str) -> bool {
    event_type.contains("_call") || event_type.contains("mcp_")
}

fn diagnosed_response_event_shape_fault(
    diagnosis: ResponseEventShapeDiagnosis,
    fault: ProviderFault,
) -> ProviderFault {
    let ResponseEventShapeDiagnosis {
        event_type,
        reason,
        response_ledger_reason,
        response_message_text_reason,
        response_completed_reconciliation,
        response_output_identity_reason,
        response_output_identity_detail,
    } = diagnosis;
    let mut diagnostic = match (response_ledger_reason, response_output_identity_reason) {
        (None, None) => {
            format!(
                "response_event_type={event_type}; response_event_reason={}",
                reason.as_str()
            )
        }
        (Some(ledger_reason), None) => {
            format!(
                "response_event_type={event_type}; response_event_reason={}; \
                 response_ledger_reason={}",
                reason.as_str(),
                ledger_reason.code()
            )
        }
        (Some(ledger_reason), Some(identity_reason)) => {
            format!(
                "response_event_type={event_type}; response_event_reason={}; \
                 response_ledger_reason={}; response_output_identity_reason={}",
                reason.as_str(),
                ledger_reason.code(),
                identity_reason.code()
            )
        }
        (None, Some(_)) => unreachable!("output identity reason requires a ledger reason"),
    };
    if let Some(reason) = response_message_text_reason {
        assert_eq!(
            response_ledger_reason,
            Some(ProviderResponseLedgerReason::MessageText),
            "message text reason requires the message text ledger reason"
        );
        assert!(
            response_output_identity_reason.is_none() && response_output_identity_detail.is_none(),
            "message text and output identity diagnostics are mutually exclusive"
        );
        diagnostic.push_str(&format!("; response_message_text_reason={}", reason.code()));
    }
    if let Some(reconciliation) = response_completed_reconciliation {
        assert_eq!(
            response_message_text_reason,
            Some(reconciliation.branch()),
            "completed reconciliation requires its exact message text branch"
        );
        let encoded = serde_json::to_string(&reconciliation)
            .expect("the closed response reconciliation snapshot is serializable");
        diagnostic.push_str("; response_completed_reconciliation=");
        diagnostic.push_str(&encoded);
    }
    if let Some(detail) = response_output_identity_detail {
        assert_eq!(
            response_ledger_reason,
            Some(ProviderResponseLedgerReason::OutputIdentity),
            "output identity detail requires the output identity ledger reason"
        );
        assert_eq!(
            response_output_identity_reason,
            Some(ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal),
            "output identity detail requires the kind-at-terminal-ordinal reason"
        );
        diagnostic.push_str(&format!(
            "; response_output_identity_mapping_basis={}; \
             response_output_identity_kind_pair={}; \
             response_output_identity_observed_message_relation={}; \
             response_output_identity_observed_text_relation={}; \
             response_output_identity_lifecycle_state={}",
            detail.mapping_basis().code(),
            detail.kind_pair().code(),
            detail.observed_message_relation().code(),
            detail.observed_text_relation().code(),
            detail.resolved_lifecycle_state().code(),
        ));
        if let Some(structure) = detail.structure() {
            let encoded = serde_json::to_string(&structure)
                .expect("the closed output identity structure is serializable");
            diagnostic.push_str("; response_output_identity_structure=");
            diagnostic.push_str(&encoded);
        }
    }
    let message = format!("{} [{diagnostic}]", fault.message());
    let diagnosed = match fault.kind() {
        ProviderFaultKind::RetryableTransport => ProviderFault::retryable_transport(message),
        ProviderFaultKind::ModelRejected => ProviderFault::model_rejected(message),
    };
    diagnosed.with_code(ProviderFaultCode::ResponseEventShape)
}

fn allowlisted_response_event_type(event_type: &str) -> &'static str {
    match event_type {
        "response.created" => "response.created",
        "response.in_progress" => "response.in_progress",
        "response.content_part.added" => "response.content_part.added",
        "response.content_part.done" => "response.content_part.done",
        "response.output_text.delta" => "response.output_text.delta",
        "response.output_text.annotation.added" => "response.output_text.annotation.added",
        "response.output_text.done" => "response.output_text.done",
        "response.reasoning_text.delta" => "response.reasoning_text.delta",
        "response.reasoning_text.done" => "response.reasoning_text.done",
        "response.completed" => "response.completed",
        "response.output_item.added" => "response.output_item.added",
        "response.output_item.done" => "response.output_item.done",
        "response.failed" => "response.failed",
        "response.incomplete" => "response.incomplete",
        "error" => "error",
        event_type if event_type.starts_with("response.output_") => "response.output_other",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use crate::component::execution::{
        ProviderFault, ProviderResponseCompletedReconciliation, ProviderResponseLedgerReason,
        ProviderResponseMessageTextReason,
    };

    use super::response_completed_ledger_fault;

    #[test]
    fn response_completed_fault_retains_each_closed_message_text_reason() {
        let cases = [
            ProviderResponseMessageTextReason::InvalidTerminalMessageShape,
            ProviderResponseMessageTextReason::TerminalObservedTextMismatch,
            ProviderResponseMessageTextReason::ApplicablePhaseMismatch,
        ];

        for reason in cases {
            let expected = reconciliation_snapshot_fixture(reason.code());
            let reconciliation =
                serde_json::from_value::<ProviderResponseCompletedReconciliation>(expected.clone())
                    .expect("synthetic reconciliation snapshot is typed");
            let fault = response_completed_ledger_fault(
                ProviderResponseLedgerReason::MessageText,
                Some(reason),
                Some(reconciliation),
                None,
                None,
                ProviderFault::model_rejected("provider-placeholder"),
            );
            let diagnostic = fault
                .response_event_diagnostic()
                .expect("message text fault remains typed");

            assert_eq!(
                diagnostic.response_ledger_reason(),
                Some(ProviderResponseLedgerReason::MessageText)
            );
            assert_eq!(diagnostic.response_message_text_reason(), Some(reason));
            assert_eq!(
                diagnostic.response_completed_reconciliation(),
                Some(reconciliation)
            );
            assert_eq!(serde_json::to_value(reconciliation).unwrap(), expected);
            assert!(fault.message().contains(&format!(
                "response_message_text_reason={}; response_completed_reconciliation=",
                reason.code()
            )));
        }
    }

    fn reconciliation_snapshot_fixture(branch: &str) -> serde_json::Value {
        serde_json::json!({
            "branch": branch,
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
            "terminal_phase": "final_answer",
            "observed_phase": "commentary",
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
            "terminal_text_bytes": 20,
            "observed_text_state": "completed",
            "observed_text_bytes": 20,
            "text_relation": "equal",
            "output_item_added_sequence": 3,
            "content_part_added_sequence": 4,
            "first_text_delta_sequence": 5,
            "last_text_delta_sequence": 5,
            "text_delta_count": 1,
            "text_done_sequence": 6,
            "content_part_done_sequence": 7,
            "output_item_done_sequence": 8,
        })
    }
}
