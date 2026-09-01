//! Sanitized fault boundary for the frame-native OpenAI Responses port.

use crate::component::execution::reaction::{
    ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultKind, ReactionPortFaultReason,
};

/// Provider-private failure class retained before the public fault boundary.
///
/// The variants are deliberately closed and payload-free. Detailed transport,
/// HTTP, SSE, or model diagnostics remain on [`OpenAiReactionFailure`] and are
/// never consulted by [`map_openai_fault`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenAiFailureClass {
    DeclarationStateLost,
    RequestPreparation,
    RequestBodyLimit,
    RequestTransport,
    Authentication,
    Authorization,
    RateLimited,
    UpstreamRetryable,
    UpstreamRejected,
    ResponseProtocolRetryable,
    ResponseProtocolViolation,
    StreamTransport,
    StreamTimeout,
    ResponseBodyLimit,
    StreamEventLimit,
    OutputLimit,
    Internal,
}

/// One classified OpenAI failure plus provider-private diagnostic context.
#[derive(Debug)]
pub(super) struct OpenAiReactionFailure {
    class: OpenAiFailureClass,
    diagnostic: OpenAiPrivateDiagnostic,
}

#[derive(Debug)]
pub(super) enum OpenAiPrivateDiagnostic {
    Static(&'static str),
    Message(String),
    HttpStatus(u16),
}

impl OpenAiReactionFailure {
    pub(super) const fn static_diagnostic(
        class: OpenAiFailureClass,
        diagnostic: &'static str,
    ) -> Self {
        Self {
            class,
            diagnostic: OpenAiPrivateDiagnostic::Static(diagnostic),
        }
    }

    pub(super) fn message(class: OpenAiFailureClass, diagnostic: impl Into<String>) -> Self {
        Self {
            class,
            diagnostic: OpenAiPrivateDiagnostic::Message(diagnostic.into()),
        }
    }

    pub(super) fn http_status(status: reqwest::StatusCode) -> Self {
        let class = match status {
            reqwest::StatusCode::UNAUTHORIZED => OpenAiFailureClass::Authentication,
            reqwest::StatusCode::FORBIDDEN => OpenAiFailureClass::Authorization,
            reqwest::StatusCode::TOO_MANY_REQUESTS => OpenAiFailureClass::RateLimited,
            _ if matches!(status.as_u16(), 408 | 409) || status.is_server_error() => {
                OpenAiFailureClass::UpstreamRetryable
            }
            _ => OpenAiFailureClass::UpstreamRejected,
        };
        Self {
            class,
            diagnostic: OpenAiPrivateDiagnostic::HttpStatus(status.as_u16()),
        }
    }

    /// Structured provider-private observability payload.
    ///
    /// This value must not be formatted into a public or Application fault.
    pub(super) const fn diagnostic(&self) -> &OpenAiPrivateDiagnostic {
        &self.diagnostic
    }
}

/// The only frame-native OpenAI boundary allowed to construct a public fault.
///
/// Mapping depends exclusively on the closed class. In particular it never
/// parses or formats the provider-private diagnostic payload.
pub(super) fn map_openai_fault(failure: &OpenAiReactionFailure) -> ReactionPortFault {
    observe_private_failure(failure);
    use OpenAiFailureClass as Class;
    use ReactionPortFaultCode as Code;
    use ReactionPortFaultReason as Reason;

    let (kind, code, reason) = match failure.class {
        Class::DeclarationStateLost => (
            ReactionPortFaultKind::Terminal,
            Code::Internal,
            Reason::Declaration,
        ),
        Class::RequestPreparation => (
            ReactionPortFaultKind::Terminal,
            Code::Rejected,
            Reason::RequestPreparation,
        ),
        Class::RequestBodyLimit => (
            ReactionPortFaultKind::Terminal,
            Code::Limit,
            Reason::RequestPreparation,
        ),
        Class::RequestTransport => (
            ReactionPortFaultKind::Retryable,
            Code::Unavailable,
            Reason::Transport,
        ),
        Class::Authentication => (
            ReactionPortFaultKind::Terminal,
            Code::Rejected,
            Reason::Authentication,
        ),
        Class::Authorization => (
            ReactionPortFaultKind::Terminal,
            Code::Rejected,
            Reason::Authorization,
        ),
        Class::RateLimited => (
            ReactionPortFaultKind::Retryable,
            Code::Unavailable,
            Reason::RateLimited,
        ),
        Class::UpstreamRetryable => (
            ReactionPortFaultKind::Retryable,
            Code::Unavailable,
            Reason::UpstreamRejected,
        ),
        Class::UpstreamRejected => (
            ReactionPortFaultKind::Terminal,
            Code::Rejected,
            Reason::UpstreamRejected,
        ),
        Class::ResponseProtocolRetryable => (
            ReactionPortFaultKind::Retryable,
            Code::Protocol,
            Reason::ResponseProtocol,
        ),
        Class::ResponseProtocolViolation => (
            ReactionPortFaultKind::Terminal,
            Code::Protocol,
            Reason::ResponseProtocol,
        ),
        Class::StreamTransport => (
            ReactionPortFaultKind::Retryable,
            Code::Unavailable,
            Reason::StreamTransport,
        ),
        Class::StreamTimeout => (
            ReactionPortFaultKind::Retryable,
            Code::Unavailable,
            Reason::StreamTimeout,
        ),
        Class::ResponseBodyLimit | Class::StreamEventLimit => (
            ReactionPortFaultKind::Terminal,
            Code::Limit,
            Reason::ResponseProtocol,
        ),
        Class::OutputLimit => (
            ReactionPortFaultKind::Terminal,
            Code::Limit,
            Reason::OutputLimit,
        ),
        Class::Internal => (
            ReactionPortFaultKind::Terminal,
            Code::Internal,
            Reason::Other,
        ),
    };

    match kind {
        ReactionPortFaultKind::Retryable => ReactionPortFault::retryable(code, reason),
        ReactionPortFaultKind::Terminal => ReactionPortFault::terminal(code, reason),
    }
}

fn observe_private_failure(failure: &OpenAiReactionFailure) {
    let (diagnostic_kind, diagnostic_bytes, http_status) = match failure.diagnostic() {
        OpenAiPrivateDiagnostic::Static(value) => ("static", value.len(), None),
        OpenAiPrivateDiagnostic::Message(value) => ("message", value.len(), None),
        OpenAiPrivateDiagnostic::HttpStatus(status) => ("http_status", 0, Some(*status)),
    };
    tracing::debug!(
        class = ?failure.class,
        diagnostic_kind,
        diagnostic_bytes,
        http_status,
        "OpenAI Frame-native reaction fault"
    );
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::component::execution::reaction::{
        ReactionPortFaultCode, ReactionPortFaultKind, ReactionPortFaultReason, SubmitFault,
    };

    use super::{map_openai_fault, OpenAiFailureClass as Class, OpenAiReactionFailure as Failure};

    #[test]
    fn every_private_class_has_one_exact_public_classification() {
        use ReactionPortFaultCode as Code;
        use ReactionPortFaultKind as Kind;
        use ReactionPortFaultReason as Reason;

        let cases = [
            (
                Class::DeclarationStateLost,
                Kind::Terminal,
                Code::Internal,
                Reason::Declaration,
            ),
            (
                Class::RequestPreparation,
                Kind::Terminal,
                Code::Rejected,
                Reason::RequestPreparation,
            ),
            (
                Class::RequestBodyLimit,
                Kind::Terminal,
                Code::Limit,
                Reason::RequestPreparation,
            ),
            (
                Class::RequestTransport,
                Kind::Retryable,
                Code::Unavailable,
                Reason::Transport,
            ),
            (
                Class::Authentication,
                Kind::Terminal,
                Code::Rejected,
                Reason::Authentication,
            ),
            (
                Class::Authorization,
                Kind::Terminal,
                Code::Rejected,
                Reason::Authorization,
            ),
            (
                Class::RateLimited,
                Kind::Retryable,
                Code::Unavailable,
                Reason::RateLimited,
            ),
            (
                Class::UpstreamRetryable,
                Kind::Retryable,
                Code::Unavailable,
                Reason::UpstreamRejected,
            ),
            (
                Class::UpstreamRejected,
                Kind::Terminal,
                Code::Rejected,
                Reason::UpstreamRejected,
            ),
            (
                Class::ResponseProtocolRetryable,
                Kind::Retryable,
                Code::Protocol,
                Reason::ResponseProtocol,
            ),
            (
                Class::ResponseProtocolViolation,
                Kind::Terminal,
                Code::Protocol,
                Reason::ResponseProtocol,
            ),
            (
                Class::StreamTransport,
                Kind::Retryable,
                Code::Unavailable,
                Reason::StreamTransport,
            ),
            (
                Class::StreamTimeout,
                Kind::Retryable,
                Code::Unavailable,
                Reason::StreamTimeout,
            ),
            (
                Class::ResponseBodyLimit,
                Kind::Terminal,
                Code::Limit,
                Reason::ResponseProtocol,
            ),
            (
                Class::StreamEventLimit,
                Kind::Terminal,
                Code::Limit,
                Reason::ResponseProtocol,
            ),
            (
                Class::OutputLimit,
                Kind::Terminal,
                Code::Limit,
                Reason::OutputLimit,
            ),
            (
                Class::Internal,
                Kind::Terminal,
                Code::Internal,
                Reason::Other,
            ),
        ];

        for (class, kind, code, reason) in cases {
            let fault = map_openai_fault(&Failure::static_diagnostic(class, "private"));
            assert_eq!(fault.kind(), kind, "wrong kind for {class:?}");
            assert_eq!(fault.code(), code, "wrong code for {class:?}");
            assert_eq!(fault.reason(), reason, "wrong reason for {class:?}");
        }
    }

    #[test]
    fn http_statuses_keep_retry_and_auth_semantics() {
        use ReactionPortFaultCode as Code;
        use ReactionPortFaultKind as Kind;
        use ReactionPortFaultReason as Reason;

        for (status, kind, code, reason) in [
            (401, Kind::Terminal, Code::Rejected, Reason::Authentication),
            (403, Kind::Terminal, Code::Rejected, Reason::Authorization),
            (429, Kind::Retryable, Code::Unavailable, Reason::RateLimited),
            (
                408,
                Kind::Retryable,
                Code::Unavailable,
                Reason::UpstreamRejected,
            ),
            (
                409,
                Kind::Retryable,
                Code::Unavailable,
                Reason::UpstreamRejected,
            ),
            (
                500,
                Kind::Retryable,
                Code::Unavailable,
                Reason::UpstreamRejected,
            ),
            (
                400,
                Kind::Terminal,
                Code::Rejected,
                Reason::UpstreamRejected,
            ),
        ] {
            let failure = Failure::http_status(reqwest::StatusCode::from_u16(status).unwrap());
            let fault = map_openai_fault(&failure);
            assert_eq!(
                (fault.kind(), fault.code(), fault.reason()),
                (kind, code, reason)
            );
        }
    }

    #[test]
    fn private_diagnostics_never_reach_public_or_submit_errors() {
        const SENTINEL: &str = "openai-private-wire-sentinel\nsecret-body";
        let failure = Failure::message(Class::ResponseProtocolViolation, SENTINEL);
        let public = map_openai_fault(&failure);
        let submit = SubmitFault::Rejected(public);

        for rendered in [
            format!("{public}"),
            format!("{public:?}"),
            format!("{submit}"),
            format!("{submit:?}"),
        ] {
            assert!(!rendered.contains(SENTINEL));
            assert!(!rendered.contains("secret-body"));
        }
        assert!(public.source().is_none());
        if let Some(source) = submit.source() {
            assert!(!source.to_string().contains(SENTINEL));
        }
    }
}
