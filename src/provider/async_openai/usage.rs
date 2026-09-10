use std::sync::Arc;

use serde_json::{Map, Value};

use crate::component::execution::ProviderFault;

use super::faults::{response_event_shape_fault, ResponseEventReason};

pub(super) type ResponseUsageObserver = dyn Fn(OpenAiResponsesUsage) + Send + Sync;

/// Validated accounting reported by one accepted OpenAI Responses terminal event.
///
/// Observer callbacks receive this value synchronously while the response stream is polled and
/// must therefore remain bounded and nonblocking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenAiResponsesUsage {
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl OpenAiResponsesUsage {
    /// Returns the provider-reported input token count when usage was present.
    pub fn input_tokens(self) -> Option<u64> {
        self.input_tokens
    }

    /// Returns the provider-reported cached input token count when present.
    pub fn cached_tokens(self) -> Option<u64> {
        self.cached_tokens
    }

    pub fn output_tokens(self) -> Option<u64> {
        self.output_tokens
    }

    pub fn total_tokens(self) -> Option<u64> {
        self.total_tokens
    }
}

pub(super) fn validated_response_usage(
    payload: &Map<String, Value>,
) -> Result<OpenAiResponsesUsage, ProviderFault> {
    let response = payload
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(invalid_response_usage_fault)?;
    let Some(usage) = response.get("usage") else {
        return Ok(unreported_usage());
    };
    if usage.is_null() {
        return Ok(unreported_usage());
    }
    let usage = usage.as_object().ok_or_else(invalid_response_usage_fault)?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_response_usage_fault)?;
    let cached_tokens = match usage.get("input_tokens_details") {
        None | Some(Value::Null) => None,
        Some(details) => {
            let details = details
                .as_object()
                .ok_or_else(invalid_response_usage_fault)?;
            match details.get("cached_tokens") {
                None | Some(Value::Null) => None,
                Some(cached_tokens) => Some(
                    cached_tokens
                        .as_u64()
                        .filter(|cached_tokens| *cached_tokens <= input_tokens)
                        .ok_or_else(invalid_response_usage_fault)?,
                ),
            }
        }
    };
    let output_tokens = optional_usage_count(usage, "output_tokens")?;
    let total_tokens = optional_usage_count(usage, "total_tokens")?;
    match (output_tokens, total_tokens) {
        (None, None) => {}
        (Some(output_tokens), Some(total_tokens))
            if input_tokens.checked_add(output_tokens) == Some(total_tokens) => {}
        _ => return Err(invalid_response_usage_fault()),
    }
    Ok(OpenAiResponsesUsage {
        input_tokens: Some(input_tokens),
        cached_tokens,
        output_tokens,
        total_tokens,
    })
}

fn unreported_usage() -> OpenAiResponsesUsage {
    OpenAiResponsesUsage {
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        total_tokens: None,
    }
}

fn optional_usage_count(
    usage: &Map<String, Value>,
    field: &str,
) -> Result<Option<u64>, ProviderFault> {
    match usage.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(invalid_response_usage_fault),
    }
}

fn invalid_response_usage_fault() -> ProviderFault {
    response_event_shape_fault(
        "response.completed",
        ResponseEventReason::Envelope,
        ProviderFault::model_rejected("OpenAI response usage has an invalid shape"),
    )
}

pub(super) fn observe_response_usage(
    observer: &Option<Arc<ResponseUsageObserver>>,
    usage: OpenAiResponsesUsage,
) {
    if let Some(observer) = observer {
        observer(usage);
    }
}
