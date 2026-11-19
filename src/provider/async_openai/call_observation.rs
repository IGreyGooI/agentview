//! Payload-free lifecycle accounting for each native Responses HTTP attempt.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use super::stream_observation::{
    OpenAiResponsesStreamEventKind, StreamDiagnostics, StreamObserver,
};
use crate::component::execution::reaction::{Frame, FrameBasis, FrameRevision, TargetIdentity};

pub(in crate::provider) type CallObserver = dyn Fn(OpenAiResponsesCallEvent) + Send + Sync;

/// The HTTP work that caused a call. A required wait on an already running
/// background worker keeps that worker's original kind and Frame identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiResponsesCallKind {
    Foreground,
    BackgroundCompaction,
    ForcedCompaction,
}

impl OpenAiResponsesCallKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::BackgroundCompaction => "background_compaction",
            Self::ForcedCompaction => "forced_compaction",
        }
    }
}

/// Transport/adapter outcome, independent of application or history commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiResponsesCallOutcome {
    Completed,
    Cancelled,
    Failed,
    Incomplete,
}

impl OpenAiResponsesCallOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Incomplete => "incomplete",
        }
    }
}

/// One actual HTTP attempt, without prompt, output text, URL or credentials.
///
/// Frame identity is the immutable trigger of this call, including for an
/// independent summary worker. IDs are unique across provider forks and
/// process lifetimes. Reported usage is retained as structured JSON, including
/// provider-specific token/cost details. `None` means unknown, never zero.
/// Response identifiers are bounded; headers are limited to request-ID fields.
#[derive(Debug, Clone)]
pub struct OpenAiResponsesCallSnapshot {
    pub call_id: String,
    pub kind: OpenAiResponsesCallKind,
    pub target: TargetIdentity,
    pub frame_revision: FrameRevision,
    pub frame_basis: FrameBasis,
    pub model: String,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub outcome: Option<OpenAiResponsesCallOutcome>,
    pub http_status: Option<u16>,
    pub response_id: Option<String>,
    pub request_id: Option<String>,
    pub response_model: Option<String>,
    pub usage: Option<Value>,
}

/// Exactly one start and one terminal notification for a polled HTTP attempt.
///
/// Preparation rejection produces no call. Dropping active work produces
/// `Cancelled`; it does not establish whether the upstream billed that work.
/// As with runtime cleanup, another panic's unwind does not invoke callbacks.
#[derive(Debug, Clone)]
pub enum OpenAiResponsesCallEvent {
    Started(OpenAiResponsesCallSnapshot),
    Finished(OpenAiResponsesCallSnapshot),
}

#[derive(Debug, Clone, Copy)]
pub(in crate::provider) struct CallOrigin {
    target: TargetIdentity,
    revision: FrameRevision,
    basis: FrameBasis,
}

impl CallOrigin {
    pub(in crate::provider) fn from_frame(frame: &Frame) -> Self {
        Self {
            target: frame.target(),
            revision: frame.revision(),
            basis: frame.basis(),
        }
    }
}

struct ActiveCall {
    observer: Option<Arc<CallObserver>>,
    stream_observer: Option<Arc<StreamObserver>>,
    stream: Option<StreamDiagnostics>,
    snapshot: OpenAiResponsesCallSnapshot,
    started: Option<Instant>,
    finished: bool,
}

/// Owned by the transport future and then its response stream/summary worker.
#[derive(Default)]
pub(in crate::provider) struct CallAccounting(Option<ActiveCall>);

impl CallAccounting {
    pub(in crate::provider) fn new(
        observer: &Option<Arc<CallObserver>>,
        stream_observer: &Option<Arc<StreamObserver>>,
        origin: CallOrigin,
        kind: OpenAiResponsesCallKind,
        model: &str,
    ) -> Self {
        Self(
            (observer.is_some() || stream_observer.is_some()).then(|| ActiveCall {
                observer: observer.clone(),
                stream_observer: stream_observer.clone(),
                stream: None,
                snapshot: OpenAiResponsesCallSnapshot {
                    call_id: String::new(),
                    kind,
                    target: origin.target,
                    frame_revision: origin.revision,
                    frame_basis: origin.basis,
                    model: model.to_owned(),
                    started_at_unix_ms: 0,
                    finished_at_unix_ms: None,
                    elapsed_ms: None,
                    outcome: None,
                    http_status: None,
                    response_id: None,
                    request_id: None,
                    response_model: None,
                    usage: None,
                },
                started: None,
                finished: false,
            }),
        )
    }

    /// Call immediately before the first poll that can hand work to HTTP.
    pub(in crate::provider) fn start(&mut self) {
        let Some(call) = &mut self.0 else { return };
        if call.started.is_some() {
            return;
        }
        call.snapshot.call_id = next_call_id();
        call.snapshot.started_at_unix_ms = unix_ms();
        let started = Instant::now();
        call.started = Some(started);
        call.stream = call.stream_observer.as_ref().map(|observer| {
            StreamDiagnostics::new(
                Arc::clone(observer),
                call.snapshot.call_id.clone(),
                call.snapshot.frame_revision,
                call.snapshot.kind,
                started,
            )
        });
        if let Some(observer) = &call.observer {
            notify(
                observer,
                OpenAiResponsesCallEvent::Started(call.snapshot.clone()),
            );
        }
    }

    pub(in crate::provider) fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    pub(in crate::provider) fn headers(
        &mut self,
        status: u16,
        headers: &reqwest::header::HeaderMap,
    ) {
        let Some(call) = &mut self.0 else { return };
        let first = call.snapshot.http_status.is_none();
        call.snapshot.http_status = Some(status);
        call.snapshot.request_id = [
            "x-request-id",
            "request-id",
            "x-tt-logid",
            "x-amzn-requestid",
        ]
        .into_iter()
        .find_map(|name| headers.get(name)?.to_str().ok().and_then(bounded_id));
        if first {
            if let Some(stream) = &call.stream {
                stream.emit(OpenAiResponsesStreamEventKind::Headers {
                    status,
                    request_id: call.snapshot.request_id.clone(),
                });
            }
        }
    }

    pub(super) fn stream(&self) -> Option<StreamDiagnostics> {
        self.0.as_ref().and_then(|call| call.stream.clone())
    }

    pub(super) fn request(&self, body: &[u8]) {
        if let Some(stream) = self.stream() {
            stream.emit(OpenAiResponsesStreamEventKind::Request {
                body: Arc::from(body),
            });
        }
    }

    pub(super) fn sse(&self, event: &eventsource_stream::Event) {
        if let Some(stream) = self.stream() {
            stream.emit(OpenAiResponsesStreamEventKind::Sse {
                event: event.event.clone(),
                id: event.id.clone(),
                data: event.data.clone(),
            });
        }
    }

    /// Retain only accounting fields from an already size-limited JSON body.
    pub(in crate::provider) fn response(&mut self, response: &Map<String, Value>) {
        let Some(call) = &mut self.0 else { return };
        if let Some(id) = response
            .get("id")
            .and_then(Value::as_str)
            .and_then(bounded_id)
        {
            call.snapshot.response_id = Some(id);
        }
        if let Some(model) = response
            .get("model")
            .and_then(Value::as_str)
            .and_then(bounded_id)
        {
            call.snapshot.response_model = Some(model);
        }
        if call.snapshot.request_id.is_none() {
            call.snapshot.request_id = response
                .get("request_id")
                .and_then(Value::as_str)
                .and_then(bounded_id);
        }
        if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
            call.snapshot.usage = Some(usage.clone());
        }
    }

    pub(in crate::provider) fn response_body(&mut self, body: &[u8]) {
        if !self.is_enabled() {
            return;
        }
        if let Some(stream) = self.stream() {
            stream.emit(OpenAiResponsesStreamEventKind::ResponseBody {
                body: Arc::from(body),
            });
        }
        if let Ok(Value::Object(body)) = serde_json::from_slice(body) {
            let response = body
                .get("response")
                .and_then(Value::as_object)
                .unwrap_or(&body);
            self.response(response);
        }
    }

    pub(in crate::provider) fn finish(&mut self, outcome: OpenAiResponsesCallOutcome) {
        let Some(call) = &mut self.0 else { return };
        let Some(started) = call.started else { return };
        if call.finished {
            return;
        }
        call.finished = true;
        call.snapshot.finished_at_unix_ms = Some(unix_ms());
        call.snapshot.elapsed_ms =
            Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
        call.snapshot.outcome = Some(outcome);
        if let Some(stream) = &call.stream {
            stream.emit(OpenAiResponsesStreamEventKind::Finished { outcome });
        }
        if let Some(observer) = &call.observer {
            notify(
                observer,
                OpenAiResponsesCallEvent::Finished(call.snapshot.clone()),
            );
        }
    }
}

impl Drop for CallAccounting {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.finish(OpenAiResponsesCallOutcome::Cancelled);
        }
    }
}

fn notify(observer: &Arc<CallObserver>, event: OpenAiResponsesCallEvent) {
    // Accounting is a diagnostic sink: its failure cannot cancel or retry a
    // billed request, change provider continuity, or replace business output.
    let _ = catch_unwind(AssertUnwindSafe(|| observer(event)));
}

fn bounded_id(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn next_call_id() -> String {
    static NAMESPACE: OnceLock<String> = OnceLock::new();
    static NEXT_CALL: AtomicU64 = AtomicU64::new(1);
    let namespace = NAMESPACE.get_or_init(|| {
        let mut random = [0_u8; 16];
        if getrandom::fill(&mut random).is_ok() {
            format!("{:032x}", u128::from_be_bytes(random))
        } else {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("{nanos:032x}-{:x}", std::process::id())
        }
    });
    format!(
        "{namespace}-{:016x}",
        NEXT_CALL.fetch_add(1, Ordering::Relaxed)
    )
}
