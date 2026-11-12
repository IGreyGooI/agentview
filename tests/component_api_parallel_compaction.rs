use agentview::provider::ModelSpec;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFault, ApplicationFaultCode, ApplicationFaultReason,
            ProviderEvent, ProviderIdentity, ReactionPortFaultReason,
        },
        prelude::*,
    },
    provider::{
        async_openai::{
            AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig, CompactionContext,
            CompactionPolicy, ContextRatioPolicy, OpenAiResponsesObservation,
            OpenAiResponsesObservationError, OpenAiResponsesObserver, ParallelCompactionFault,
            ParallelCompactionMonitor, ParallelCompactionPhase,
        },
        codex_http_v1::{
            CodexHttpV1Encoder, CodexHttpV1Options, CodexReasoning, OpenRouterProviderRouting,
        },
    },
};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};

#[path = "support/parallel_compaction_call_accounting.rs"]
mod call_accounting;
#[path = "support/pom.rs"]
mod pom_support;
use pom_support::{frame_document, InspectPom};

#[derive(Clone)]
struct ServerState {
    calls: Arc<AtomicUsize>,
    requests: mpsc::UnboundedSender<Value>,
    compactions: mpsc::UnboundedSender<Value>,
    release: Arc<Semaphore>,
    compact_reply: Value,
    compact_status: StatusCode,
    unsupported_requests: Arc<AtomicUsize>,
}

struct Server {
    base: String,
    requests: mpsc::UnboundedReceiver<Value>,
    compactions: mpsc::UnboundedReceiver<Value>,
    release: Arc<Semaphore>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    unsupported_requests: Arc<AtomicUsize>,
}

impl Server {
    async fn start(compact_reply: Value) -> Self {
        Self::start_with_status(compact_reply, StatusCode::OK).await
    }

    async fn start_with_status(compact_reply: Value, compact_status: StatusCode) -> Self {
        let (requests, request_rx) = mpsc::unbounded_channel();
        let (compactions, compaction_rx) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let unsupported_requests = Arc::new(AtomicUsize::new(0));
        let state = ServerState {
            calls: Arc::new(AtomicUsize::new(0)),
            requests,
            compactions,
            release: release.clone(),
            compact_reply,
            compact_status,
            unsupported_requests: unsupported_requests.clone(),
        };
        let router = Router::new()
            .route("/responses", post(responses))
            .fallback(unsupported)
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            base,
            requests: request_rx,
            compactions: compaction_rx,
            release,
            stop,
            task,
            unsupported_requests,
        }
    }

    async fn finish(self) {
        self.release.add_permits(32);
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            self.unsupported_requests.load(Ordering::SeqCst),
            0,
            "only the ordinary /responses endpoint is supported"
        );
    }
}

async fn unsupported(State(state): State<ServerState>) -> StatusCode {
    state.unsupported_requests.fetch_add(1, Ordering::SeqCst);
    StatusCode::NOT_FOUND
}

async fn responses(
    state: State<ServerState>,
    Json(request): Json<Value>,
) -> axum::response::Response {
    assert!(request.get("context_management").is_none());
    assert!(!request["input"].as_array().unwrap().iter().any(|item| {
        matches!(
            item["type"].as_str(),
            Some("compaction" | "compaction_trigger")
        )
    }));
    if request["stream"] == false {
        assert_eq!(request["tools"], json!([]));
        assert_eq!(request["tool_choice"], "none");
        assert_eq!(request["store"], false);
        assert!(request.get("previous_response_id").is_none());
        assert!(request["max_output_tokens"].as_u64().unwrap() > 0);
        compact(state, Json(request)).await.into_response()
    } else {
        foreground(state, Json(request)).await.into_response()
    }
}

async fn foreground(
    State(state): State<ServerState>,
    Json(request): Json<Value>,
) -> impl IntoResponse {
    state.requests.send(request).unwrap();
    let index = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let events = if index <= 3 {
        let item = json!({"id":format!("item_{index}"), "type":"function_call", "call_id":format!("call_{index}"), "name":"advance", "arguments":"{}", "status":"completed"});
        vec![
            json!({"type":"response.output_item.added", "sequence_number":1, "output_index":0, "item":{"id":format!("item_{index}"),"type":"function_call","call_id":format!("call_{index}"),"name":"advance","arguments":"","status":"in_progress"}}),
            json!({"type":"response.function_call_arguments.delta", "sequence_number":2,"output_index":0,"item_id":format!("item_{index}"),"delta":"{}"}),
            json!({"type":"response.function_call_arguments.done", "sequence_number":3,"output_index":0,"item_id":format!("item_{index}"),"arguments":"{}"}),
            json!({"type":"response.output_item.done", "sequence_number":4,"output_index":0,"item":item}),
            json!({"type":"response.completed", "sequence_number":5,"response":{"id":format!("resp_{index}"),"status":"completed","output":[item]}}),
        ]
    } else {
        vec![
            json!({"type":"response.completed","sequence_number":1,"response":{"id":format!("resp_{index}"),"status":"completed","output":[]}}),
        ]
    };
    let body: String = events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body)
}

async fn compact(
    State(state): State<ServerState>,
    Json(request): Json<Value>,
) -> impl IntoResponse {
    state.compactions.send(request).unwrap();
    state.release.acquire().await.unwrap().forget();
    (state.compact_status, Json(state.compact_reply))
}

struct BusinessState {
    count: usize,
    memo: String,
}

#[component]
fn counter(text_events: Arc<AtomicUsize>) -> Component {
    let state = use_signal(|| BusinessState {
        count: 0,
        memo: "old context ".repeat(8192),
    });
    let (count, memo) = state
        .with(|state| (state.count, state.memo.clone()))
        .unwrap();
    use_provider_event_handler(ProviderEvent::TEXT, move |_ctx, _| {
        text_events.fetch_add(1, Ordering::SeqCst);
        async { Ok::<(), std::convert::Infallible>(()) }
    });
    view! {
        #[developer]
        { memo }
        #[user]
        { format!("counter value: {count}") }
        NativeToolCall {
            name: "advance",
            description: "Increment the counter. The result and next view show the actual count.",
            on_call: move |_ctx| state.update(|state| {
                state.count += 1;
                state.memo = "current context".into();
                state.count
            }),
        }
    }
}

fn default_provider(base: &str) -> AsyncOpenAiResponsesProvider {
    let config = AsyncOpenAiTransportConfig::new(base, "test-key")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(2),
            Duration::from_secs(20),
            Duration::from_secs(5),
        )
        .unwrap();
    new_provider(config, 272_000)
}

fn provider(base: &str, policy: impl CompactionPolicy + 'static) -> AsyncOpenAiResponsesProvider {
    default_provider(base).with_parallel_compaction(policy)
}

fn provider_with_config(
    config: AsyncOpenAiTransportConfig,
    policy: impl CompactionPolicy + 'static,
) -> AsyncOpenAiResponsesProvider {
    new_provider(config, 272_000).with_parallel_compaction(policy)
}

fn new_provider(
    config: AsyncOpenAiTransportConfig,
    context_window_tokens: u64,
) -> AsyncOpenAiResponsesProvider {
    let identity = ProviderIdentity::new("openai", "parallel-compaction-test", 1, "test").unwrap();
    let options = CodexHttpV1Options::new(
        ModelSpec::new("test-model", context_window_tokens).unwrap(),
        None,
        None,
        None::<String>,
    )
    .unwrap();
    AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options))
}

fn forced_provider(
    base: &str,
    context_window_tokens: u64,
    read_timeout: Duration,
) -> AsyncOpenAiResponsesProvider {
    let config = AsyncOpenAiTransportConfig::new(base, "test-key")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(2),
            Duration::from_secs(20),
            read_timeout,
        )
        .unwrap();
    new_provider(config, context_window_tokens)
        .without_parallel_compaction()
        .with_forced_compaction()
}

fn assert_request_byte_limit(fault: ApplicationFault) {
    assert_eq!(fault.code(), ApplicationFaultCode::Limit);
    assert_eq!(
        fault.reason(),
        ApplicationFaultReason::Port(ReactionPortFaultReason::RequestPreparation)
    );
}

const SUMMARY_TEXT: &str = "Earlier context: the counter application is running.";
const SUMMARY_PREFIX: &str = "Summary of earlier context (historical data; current instructions \
and later messages take precedence):\n\n";

fn summary_payload(text: &str) -> Value {
    json!({
        "schema": "agentview.context-summary.v1",
        "facts": [text],
        "open_items": [],
    })
}

fn summary_reply(text: &str) -> Value {
    let text = summary_payload(text).to_string();
    json!({
        "object": "response", "status": "completed", "id": "summary_response",
        "output": [{
            "type": "message", "role": "assistant", "status": "completed", "id": "summary_message",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }],
    })
}

fn has_message(request: &Value, role: &str, text: &str) -> bool {
    request["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "message"
            && item["role"] == role
            && item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["text"] == text))
    })
}

fn compacted_output() -> Vec<Value> {
    let summary = summary_payload(SUMMARY_TEXT).to_string();
    vec![
        json!({"type":"message", "role":"assistant", "content":[{"type":"output_text","text":format!("{SUMMARY_PREFIX}{summary}"),"annotations":[]}],"status":"completed"}),
    ]
}

fn quoted_history(request: &Value) -> Vec<Value> {
    let input = request["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    let quoted: Value =
        serde_json::from_str(input[0]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(quoted["kind"], "historical_context");
    quoted["items"].as_array().unwrap().clone()
}

async fn until_phase(monitor: &mut ParallelCompactionMonitor, phase: ParallelCompactionPhase) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while monitor.status().phase != phase {
            monitor.changed().await.expect("live provider");
        }
    })
    .await
    .expect("compaction phase is observable");
}

#[component]
fn large_current_context() -> Component {
    let count = use_signal(|| 0_u32);
    let value = count.with(|count| *count).unwrap();
    let context = "needed context ".repeat(65_536);
    view! {
        #[developer]
        { context }
        { format!("counter value: {value}") }
        NativeToolCall {
            name: "advance",
            description: "Increment the counter; the next view includes the updated count.",
            on_call: move |_ctx| count.update(|value| { *value += 1; *value }),
        }
    }
}

#[component]
fn replacing_large_context() -> Component {
    let count = use_signal(|| 0_u32);
    let value = count.with(|count| *count).unwrap();
    let context = format!("generation {value}: {}", "needed context ".repeat(4_096));
    view! {
        { context }
        NativeToolCall {
            name: "advance",
            description: "Increment the generation and replace the large current context.",
            on_call: move |_ctx| count.update(|value| { *value += 1; *value }),
        }
    }
}

const ATTENDANCE_FACT: &str = "用户今天已打上班卡，还没打下班卡。";
const BUSINESS_END_RULE: &str = "Business replies must end with <end_response/>.";

#[derive(AgentView)]
#[agent_view(kind = "retained_history")]
struct RetainedHistoryView {
    #[view(element)]
    fact: &'static str,
    #[view(element)]
    business_output_rule: &'static str,
}

#[component]
fn retained_history_context() -> Component {
    let count = use_signal(|| 0_u32);
    let value = count.with(|count| *count).unwrap();
    let context = if value == 0 {
        "earlier unrelated context ".repeat(4096)
    } else {
        "current context".into()
    };
    // Keep the stable history across a role boundary from the changing prompt.
    // Contiguous developer inputs can merge and resend the old fact when the
    // context changes, masking a lost prefix with newly transmitted history.
    view! {
        #[developer]
        #[diff(slot = "retained_history")]
        { RetainedHistoryView { fact: ATTENDANCE_FACT, business_output_rule: BUSINESS_END_RULE } }
        #[user]
        { format!("{context}\nfollow-up {value}") }
        NativeToolCall {
            name: "advance",
            description: "Advance the discussion; the next view exposes the actual progress.",
            on_call: move |_ctx| count.update(|value| { *value += 1; *value }),
        }
    }
}

fn contains_text(request: &Value, text: &str) -> bool {
    request["input"].as_array().unwrap().iter().any(|item| {
        item["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["text"]
                    .as_str()
                    .is_some_and(|value| value.contains(text))
            })
        })
    })
}

#[tokio::test]
async fn factual_summary_preserves_known_state_and_the_latest_foreground_tail() {
    let mut server = Server::start(summary_reply(ATTENDANCE_FACT)).await;
    let provider = provider(&server.base, ContextRatioPolicy::default());
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let transmitted_facts = Arc::new(Mutex::new(Vec::new()));
    let capture = transmitted_facts.clone();
    let mut app = Application::mount(retained_history_context, provider)
        .unwrap()
        .with_frame_observer(move |event| {
            let projection = frame_document(event.frame().submission().canonical_bytes());
            capture
                .lock()
                .unwrap()
                .push(projection.has_field("fact", ATTENDANCE_FACT));
        });

    for _ in 0..2 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        server.requests.recv().await.unwrap();
    }
    let summary_request = server.compactions.recv().await.unwrap();
    let source = quoted_history(&summary_request);
    assert!(contains_text(&json!({"input": source}), ATTENDANCE_FACT));
    assert!(contains_text(&json!({"input": source}), BUSINESS_END_RULE));
    assert!(source.iter().any(|item| item["role"] == "developer"));

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let while_compacting = server.requests.recv().await.unwrap();
    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let compacted = server.requests.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert!(contains_text(&compacted, ATTENDANCE_FACT));
    assert!(contains_text(&compacted, "follow-up 3"));
    let input = compacted["input"].as_array().unwrap();
    let summary_text = input[0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        summary_text.strip_prefix(SUMMARY_PREFIX).unwrap(),
        summary_payload(ATTENDANCE_FACT).to_string(),
        "the model's summary body is retained verbatim"
    );
    let summary: Value =
        serde_json::from_str(summary_text.strip_prefix(SUMMARY_PREFIX).unwrap()).unwrap();
    assert_eq!(summary["facts"], json!([ATTENDANCE_FACT]));
    assert_eq!(
        *transmitted_facts.lock().unwrap(),
        [true, false, false, false],
        "the compacted request recovers the fact from the summary, not repeated projection input"
    );
    let previous_tail = &while_compacting["input"].as_array().unwrap()[source.len()..];
    assert_eq!(&input[1..1 + previous_tail.len()], previous_tail);
    for index in 1..=3 {
        let id = format!("call_{index}");
        assert_eq!(
            input
                .iter()
                .filter(|item| { item["type"] == "function_call_output" && item["call_id"] == id })
                .count(),
            1
        );
    }
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn default_threshold_starts_compaction_without_a_small_requested_context() {
    let output = compacted_output();
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = default_provider(&server.base);
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let mut app = Application::mount(large_current_context, provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let first = server.requests.recv().await.unwrap();
    assert!(first.to_string().len().div_ceil(4) >= 244_800);
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Idle);

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let second = server.requests.recv().await.unwrap();
    // The complete current Component context still contains the entire large
    // developer message, so the 1/10 trigger cannot account for this attempt.
    assert!(second.to_string().len() < first.to_string().len() * 2);
    tokio::time::timeout(Duration::from_secs(5), server.compactions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);
    assert!(has_message(&second, "user", "counter value: 1"));

    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let next = server.requests.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert!(next["input"].as_array().unwrap().starts_with(&output));
    assert!(has_message(&next, "user", "counter value: 2"));
    assert!(next["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["call_id"] == "call_2" && item["output"] == "2"));
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn background_summarization_can_be_disabled_independently_of_request_limits() {
    let mut server = Server::start(json!({})).await;
    let provider = default_provider(&server.base).without_parallel_compaction();
    assert!(provider.parallel_compaction_monitor().is_none());
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    for _ in 0..3 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        let request = server.requests.recv().await.unwrap();
        assert!(request.get("context_management").is_none());
        assert!(has_message(
            &request,
            "developer",
            &"old context ".repeat(8192)
        ));
    }
    app.shutdown().await.unwrap();
    assert!(server.compactions.try_recv().is_err());
    server.finish().await;
}

#[tokio::test]
async fn context_estimates_do_not_reject_when_compaction_is_disabled_or_has_no_source() {
    for forced in [false, true] {
        let mut server = Server::start(json!({})).await;
        let config = AsyncOpenAiTransportConfig::new(&server.base, "test-key").unwrap();
        let mut provider = new_provider(config, 200_000).without_parallel_compaction();
        if forced { provider = provider.with_forced_compaction(); }
        let mut app = Application::mount(large_current_context, provider).unwrap();
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        let request = server.requests.recv().await.unwrap();
        assert!(serde_json::to_vec(&request).unwrap().len().div_ceil(4) > 200_000);
        assert!(server.compactions.try_recv().is_err());
        app.shutdown().await.unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn serialized_request_byte_limit_still_rejects_before_handoff() {
    let mut server = Server::start(json!({})).await;
    let config = AsyncOpenAiTransportConfig::new(&server.base, "test-key").unwrap()
        .with_responses_serialized_request_body_limit(1).unwrap();
    let provider = new_provider(config, 200_000).without_parallel_compaction();
    let mut app = Application::mount(large_current_context, provider).unwrap();
    assert_request_byte_limit(app.react(false).unwrap().await.unwrap_err());
    assert!(server.requests.try_recv().is_err());
    assert!(server.compactions.try_recv().is_err());
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn forced_only_compaction_waits_before_sending_the_oversized_foreground() {
    let output = compacted_output();
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = forced_provider(&server.base, 20_000, Duration::from_secs(5));
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let mut app = Application::mount(replacing_large_context, provider).unwrap();

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let first = server.requests.recv().await.unwrap();
    assert!(first.to_string().len().div_ceil(4) <= 20_000);
    assert!(server.compactions.try_recv().is_err());

    let compacted = {
        let second = std::future::IntoFuture::into_future(app.react(false).unwrap());
        tokio::pin!(second);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                request = server.compactions.recv() => request.expect("forced compact request"),
                result = &mut second => panic!("foreground completed before forced compaction: {result:?}"),
            }
        })
        .await
        .unwrap();
        assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);
        assert!(server.requests.try_recv().is_err());

        server.release.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(5), &mut second)
            .await
            .unwrap()
            .unwrap()
            .is_continue());
        server.requests.recv().await.unwrap()
    };
    assert!(compacted["input"].as_array().unwrap().starts_with(&output));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert_eq!(monitor.status().attempt, 1);
    assert!(server.compactions.try_recv().is_err());

    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn cancelling_a_forced_wait_keeps_the_same_candidate_for_retry() {
    let output = compacted_output();
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = forced_provider(&server.base, 20_000, Duration::from_secs(5));
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let mut app = Application::mount(replacing_large_context, provider).unwrap();

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    let pending_control = app.react(false).unwrap();
    {
        let pending = std::future::IntoFuture::into_future(pending_control.clone());
        tokio::pin!(pending);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                request = server.compactions.recv() => request.expect("forced compact request"),
                result = &mut pending => panic!("foreground completed before forced compaction: {result:?}"),
            }
        })
        .await
        .unwrap();
    }
    pending_control.interrupt().unwrap();
    assert_eq!(
        pending_control.await.unwrap(),
        agentview::component::execution::ReactionOutcome::Interrupted
    );
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);
    assert_eq!(monitor.status().attempt, 1);
    assert!(server.requests.try_recv().is_err());

    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let retry = server.requests.recv().await.unwrap();
    assert!(retry["input"].as_array().unwrap().starts_with(&output));
    assert_eq!(
        retry["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "call_1")
            .count(),
        1
    );
    assert!(has_message(
        &retry,
        "user",
        &format!("generation 1: {}", "needed context ".repeat(4_096))
    ));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert_eq!(monitor.status().attempt, 1);
    assert!(server.compactions.try_recv().is_err());

    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn forced_compaction_does_not_block_or_compact_an_under_limit_frame() {
    let mut server = Server::start(json!({})).await;
    let provider = forced_provider(&server.base, 272_000, Duration::from_secs(5));
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();

    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_secs(2), app.react(false).unwrap())
                .await
                .unwrap()
                .unwrap()
                .is_continue()
        );
        server.requests.recv().await.unwrap();
    }
    assert!(server.compactions.try_recv().is_err());
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Idle);

    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn forced_compaction_timeout_continues_the_original_foreground_once() {
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = forced_provider(&server.base, 20_000, Duration::from_millis(100));
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let mut app = Application::mount(replacing_large_context, provider).unwrap();

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    {
        let pending = std::future::IntoFuture::into_future(app.react(false).unwrap());
        tokio::pin!(pending);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                request = server.compactions.recv() => request.expect("forced compact request"),
                result = &mut pending => panic!("foreground completed before compact timeout: {result:?}"),
            }
        }).await.unwrap();
        assert!(server.requests.try_recv().is_err());
        assert!(tokio::time::timeout(Duration::from_secs(2), &mut pending)
            .await.unwrap().unwrap().is_continue());
    }
    let request = server.requests.recv().await.unwrap();
    assert!(has_message(&request, "user", &format!("generation 1: {}", "needed context ".repeat(4_096))));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Failed);
    assert_eq!(monitor.status().fault, Some(ParallelCompactionFault::Timeout));
    assert_eq!(monitor.status().attempt, 1);
    assert!(server.compactions.try_recv().is_err());
    assert!(server.requests.try_recv().is_err());
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn forced_candidate_with_a_high_estimate_is_installed_without_compacting_again() {
    let mut server = Server::start(summary_reply(&"x".repeat(30_000))).await;
    let provider = forced_provider(&server.base, 20_000, Duration::from_secs(5));
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let mut app = Application::mount(replacing_large_context, provider).unwrap();

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    {
        let pending = std::future::IntoFuture::into_future(app.react(false).unwrap());
        tokio::pin!(pending);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                request = server.compactions.recv() => request.expect("forced compact request"),
                result = &mut pending => panic!("foreground completed before forced compaction: {result:?}"),
            }
        }).await.unwrap();
        assert!(server.requests.try_recv().is_err());
        server.release.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(5), &mut pending)
            .await.unwrap().unwrap().is_continue());
    }
    let request = server.requests.recv().await.unwrap();
    assert!(serde_json::to_vec(&request).unwrap().len().div_ceil(4) > 20_000);
    assert!(has_message(&request, "user", &format!("generation 1: {}", "needed context ".repeat(4_096))));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert_eq!(monitor.status().attempt, 1);
    assert!(server.compactions.try_recv().is_err());
    assert!(server.requests.try_recv().is_err());
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn forced_compaction_http_protocol_and_no_reduction_failures_preserve_foreground() {
    for (reply, status, expected) in [
        (json!({}), StatusCode::TOO_MANY_REQUESTS, ParallelCompactionFault::HttpStatus(429)),
        (json!({"status":"completed","output":[]}), StatusCode::OK, ParallelCompactionFault::Protocol),
        (summary_reply(&"unreduced ".repeat(30_000)), StatusCode::OK, ParallelCompactionFault::NoReduction),
    ] {
        let mut server = Server::start_with_status(reply, status).await;
        let provider = forced_provider(&server.base, 20_000, Duration::from_secs(5));
        let monitor = provider.parallel_compaction_monitor().unwrap();
        let mut app = Application::mount(replacing_large_context, provider).unwrap();
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        server.requests.recv().await.unwrap();
        server.release.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(5), app.react(false).unwrap())
            .await.unwrap().unwrap().is_continue());
        server.compactions.recv().await.unwrap();
        let request = server.requests.recv().await.unwrap();
        assert!(has_message(&request, "user", &format!("generation 1: {}", "needed context ".repeat(4_096))));
        assert_eq!(monitor.status().phase, ParallelCompactionPhase::Failed);
        assert_eq!(monitor.status().fault, Some(expected));
        assert_eq!(monitor.status().attempt, 1);
        assert!(server.compactions.try_recv().is_err());
        app.shutdown().await.unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn forced_limit_waits_for_the_existing_background_worker() {
    let output = compacted_output();
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let config = AsyncOpenAiTransportConfig::new(&server.base, "test-key")
        .unwrap()
        .with_timeouts(
            Duration::from_secs(2),
            Duration::from_secs(20),
            Duration::from_secs(5),
        )
        .unwrap();
    let provider = new_provider(config, 35_000)
        .with_parallel_compaction(|_: &CompactionContext| true)
        .with_forced_compaction();
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let mut app = Application::mount(replacing_large_context, provider).unwrap();

    for _ in 0..2 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        server.requests.recv().await.unwrap();
    }
    server.compactions.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);

    let compacted = {
        let third = std::future::IntoFuture::into_future(app.react(false).unwrap());
        tokio::pin!(third);
        server.release.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(5), &mut third)
            .await
            .unwrap()
            .unwrap()
            .is_continue());
        server.requests.recv().await.unwrap()
    };
    assert!(compacted["input"].as_array().unwrap().starts_with(&output));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert_eq!(monitor.status().attempt, 1);
    assert!(server.compactions.try_recv().is_err());

    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn disabled_reasoning_and_endpoint_routing_survive_tool_feedback_and_compaction() {
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let options = CodexHttpV1Options::new(
        ModelSpec::new("test-model", 272_000).unwrap(),
        None,
        Some(CodexReasoning::disabled()),
        None::<String>,
    )
    .unwrap()
    .with_openrouter_provider_routing(
        OpenRouterProviderRouting::new(
            vec!["together".into(), "fireworks/us".into()],
            vec!["together".into(), "fireworks/us".into()],
            true,
        )
        .unwrap(),
    );
    let provider = AsyncOpenAiResponsesProvider::new(
        AsyncOpenAiTransportConfig::new(&server.base, "test-key").unwrap(),
        ProviderIdentity::new("openai", "parallel-compaction-test", 1, "test").unwrap(),
        CodexHttpV1Encoder::new(options),
    );
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let root_events = events.clone();
    let mut app = Application::mount(move || counter(root_events.clone()), provider).unwrap();

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let first = server.requests.recv().await.unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let second = server.requests.recv().await.unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(5), server.compactions.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(second["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| { item["call_id"] == "call_1" && item["output"] == "1" }));
    assert!(has_message(&second, "user", "counter value: 1"));
    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let compacted = server.requests.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    assert!(compacted["input"]
        .as_array()
        .unwrap()
        .starts_with(&compacted_output()));
    assert!(compacted["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| { item["call_id"] == "call_2" && item["output"] == "2" }));
    assert!(has_message(&compacted, "user", "counter value: 2"));
    for request in [first, second, summary, compacted] {
        assert_eq!(request["reasoning"], json!({"enabled": false}));
        assert_eq!(
            request["provider"],
            json!({
                "only": ["together", "fireworks/us"],
                "order": ["together", "fireworks/us"],
                "require_parameters": true,
            })
        );
    }
    assert_eq!(events.load(Ordering::SeqCst), 0);
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn default_parallel_compaction_keeps_foreground_interactive_and_preserves_new_tail() {
    let output = compacted_output();
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = default_provider(&server.base);
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let root_events = events.clone();
    let mut app = Application::mount(move || counter(root_events.clone()), provider).unwrap();

    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let first = server.requests.recv().await.unwrap();
    assert!(first.get("context_management").is_none());
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Idle);

    // The compact handler stays blocked until explicitly released. The second
    // and third foreground reactions must still return with completed tools.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), app.react(false).unwrap())
            .await
            .unwrap()
            .unwrap()
            .is_continue()
    );
    let second = server.requests.recv().await.unwrap();
    let compact_request = tokio::time::timeout(Duration::from_secs(5), server.compactions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(compact_request["model"], "test-model");
    assert_eq!(compact_request["tools"], json!([]));
    assert_eq!(compact_request["stream"], false);
    let prefix = quoted_history(&compact_request);
    assert!(!prefix.iter().any(|item| item["type"] == "function_call"));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);
    assert!(second["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["call_id"] == "call_1" && item["output"] == "1"));
    assert!(has_message(&second, "user", "counter value: 1"));

    assert!(
        tokio::time::timeout(Duration::from_secs(5), app.react(false).unwrap())
            .await
            .unwrap()
            .unwrap()
            .is_continue()
    );
    let third = server.requests.recv().await.unwrap();
    assert!(third["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["call_id"] == "call_2" && item["output"] == "2"));
    assert!(has_message(&third, "user", "counter value: 2"));
    assert!(server.compactions.try_recv().is_err());

    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let fourth = server.requests.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    let input = fourth["input"].as_array().unwrap();
    assert!(input.starts_with(&output));
    let previous_tail = &third["input"].as_array().unwrap()[prefix.len()..];
    assert_eq!(
        &input[output.len()..output.len() + previous_tail.len()],
        previous_tail
    );
    for index in 1..=3 {
        let id = format!("call_{index}");
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "function_call" && item["call_id"] == id)
                .count(),
            1
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "function_call_output" && item["call_id"] == id)
                .count(),
            1
        );
    }
    assert!(has_message(&fourth, "user", "counter value: 3"));
    assert!(fourth.to_string().len() < first.to_string().len() / 10);
    assert_eq!(
        events.load(Ordering::SeqCst),
        0,
        "compact output never reaches Component handlers"
    );
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn custom_policy_sees_complete_requested_context_even_when_delta_is_small() {
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let seen = Arc::new(Mutex::new(Vec::<CompactionContext>::new()));
    let capture = seen.clone();
    let provider = provider(&server.base, move |context: &CompactionContext| {
        capture.lock().unwrap().push(*context);
        false
    });
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    for _ in 0..3 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        server.requests.recv().await.unwrap();
    }
    {
        let contexts = seen.lock().unwrap();
        assert_eq!(contexts.len(), 2);
        for context in contexts.iter() {
            let requested = context.requested_context.unwrap();
            assert!(
                requested.bytes > 100,
                "complete state and tool definitions, not an empty Delta"
            );
            assert_eq!(requested.tokens, requested.bytes.div_ceil(4) as u64);
            assert!(requested.tokens * 10 < context.provider_context.tokens);
            assert!(context.compactable_context.bytes > 1000);
            assert_eq!(context.context_window_tokens.get(), 272_000);
        }
    }
    assert!(server.compactions.try_recv().is_err());
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Idle);
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn failed_compaction_keeps_foreground_context_and_reports_failure() {
    let mut server =
        Server::start(json!({"object":"response","status":"completed","output":[]})).await;
    let provider = provider(&server.base, ContextRatioPolicy::default());
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    server.compactions.recv().await.unwrap();
    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Failed).await;
    assert_eq!(
        monitor.status().fault,
        Some(ParallelCompactionFault::Protocol)
    );
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let next = server.requests.recv().await.unwrap();
    assert!(has_message(
        &next,
        "developer",
        &"old context ".repeat(8192)
    ));
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[derive(Default)]
struct RejectSummaryReceipt {
    entered: Notify,
    release: Notify,
    requests: AtomicUsize,
}

#[async_trait::async_trait]
impl OpenAiResponsesObserver for RejectSummaryReceipt {
    async fn observe(
        &self,
        event: OpenAiResponsesObservation,
    ) -> Result<(), OpenAiResponsesObservationError> {
        match event {
            OpenAiResponsesObservation::Request { body, .. } => {
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["stream"], false);
                assert_eq!(body["tools"], json!([]));
                assert!(body.get("prompt_cache_options").is_none());
                self.requests.fetch_add(1, Ordering::SeqCst);
            }
            OpenAiResponsesObservation::Response { completed, .. } => {
                assert!(completed);
                self.entered.notify_one();
                self.release.notified().await;
                return Err(OpenAiResponsesObservationError);
            }
            _ => {}
        }
        Ok(())
    }
}

#[tokio::test]
async fn summary_receipt_barrier_blocks_installation_and_preserves_foreground_results() {
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let observer = Arc::new(RejectSummaryReceipt::default());
    let mut provider = provider(&server.base, ContextRatioPolicy::default())
        .with_parallel_tool_calls(true)
        .with_compaction_observer(observer.clone());
    provider
        .set_prompt_cache_comparison_response_id(Some("foreground_baseline".into()))
        .unwrap();
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let first = server.requests.recv().await.unwrap();
    assert_eq!(first["parallel_tool_calls"], true);
    assert_eq!(
        first["prompt_cache_options"]["comparison_response_id"],
        "foreground_baseline"
    );
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    server.compactions.recv().await.unwrap();
    server.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), observer.entered.notified())
        .await
        .unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let while_receipt_pending = server.requests.recv().await.unwrap();
    assert!(has_message(
        &while_receipt_pending,
        "developer",
        &"old context ".repeat(8192)
    ));
    assert!(while_receipt_pending["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| { item["call_id"] == "call_2" && item["output"] == "2" }));
    observer.release.notify_one();
    until_phase(&mut monitor, ParallelCompactionPhase::Failed).await;
    assert_eq!(
        monitor.status().fault,
        Some(ParallelCompactionFault::Observation)
    );
    assert_eq!(observer.requests.load(Ordering::SeqCst), 1);
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let after_refusal = server.requests.recv().await.unwrap();
    assert!(has_message(
        &after_refusal,
        "developer",
        &"old context ".repeat(8192)
    ));
    assert!(after_refusal["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| { item["call_id"] == "call_3" && item["output"] == "3" }));
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn shutdown_cancels_and_joins_a_pending_compaction() {
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = provider(&server.base, ContextRatioPolicy::default());
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.compactions.recv().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), app.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Cancelled);
    while monitor.changed().await.is_some() {}
    server.finish().await;
}

#[tokio::test]
async fn background_http_limits_and_timeouts_report_sanitized_faults_without_breaking_foreground() {
    for expected in [
        ParallelCompactionFault::HttpStatus(429),
        ParallelCompactionFault::Limit,
        ParallelCompactionFault::Timeout,
    ] {
        let reply = summary_reply(&"large result ".repeat(1024));
        let status = if expected == ParallelCompactionFault::HttpStatus(429) {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::OK
        };
        let mut server = Server::start_with_status(reply, status).await;
        let config = AsyncOpenAiTransportConfig::new(&server.base, "test-key")
            .unwrap()
            .with_response_limits(4096, 2048, 1024)
            .unwrap()
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap();
        let provider = provider_with_config(config, ContextRatioPolicy::default());
        let mut monitor = provider.parallel_compaction_monitor().unwrap();
        let events = Arc::new(AtomicUsize::new(0));
        let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
        for _ in 0..2 {
            assert!(app.react(false).unwrap().await.unwrap().is_continue());
            server.requests.recv().await.unwrap();
        }
        server.compactions.recv().await.unwrap();
        if expected != ParallelCompactionFault::Timeout {
            server.release.add_permits(1);
        }
        until_phase(&mut monitor, ParallelCompactionPhase::Failed).await;
        assert_eq!(monitor.status().fault, Some(expected));
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        let next = server.requests.recv().await.unwrap();
        assert!(has_message(
            &next,
            "developer",
            &"old context ".repeat(8192)
        ));
        assert!(next["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["call_id"] == "call_2" && item["output"] == "2"));
        app.shutdown().await.unwrap();
        server.finish().await;
    }
}

#[tokio::test]
async fn reset_invalidates_a_pending_compaction_before_its_late_response() {
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let provider = provider(&server.base, ContextRatioPolicy::default());
    let monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    for _ in 0..4 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        server.requests.recv().await.unwrap();
    }
    server.compactions.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Running);
    app.reset_model_context().unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Cancelled);
    server.release.add_permits(1);
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let next = server.requests.recv().await.unwrap();
    assert!(!next["input"]
        .as_array()
        .unwrap()
        .starts_with(&compacted_output()));
    assert!(!has_message(
        &next,
        "developer",
        &"old context ".repeat(8192)
    ));
    assert!(has_message(&next, "user", "counter value: 3"));
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Cancelled);
    app.shutdown().await.unwrap();
    server.finish().await;
}

struct BlockFirstCompactedRequest {
    block: AtomicBool,
    entered: Notify,
}

#[async_trait::async_trait]
impl OpenAiResponsesObserver for BlockFirstCompactedRequest {
    async fn observe(
        &self,
        event: OpenAiResponsesObservation,
    ) -> Result<(), OpenAiResponsesObservationError> {
        if matches!(event, OpenAiResponsesObservation::Request { .. })
            && self.block.swap(false, Ordering::SeqCst)
        {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn cancelling_before_handoff_keeps_the_candidate_and_tool_receipts_for_retry() {
    let output = compacted_output();
    let mut server = Server::start(summary_reply(SUMMARY_TEXT)).await;
    let observer = Arc::new(BlockFirstCompactedRequest {
        block: AtomicBool::new(false),
        entered: Notify::new(),
    });
    let provider =
        provider(&server.base, ContextRatioPolicy::default()).with_observer(observer.clone());
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let events = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(events.clone()), provider).unwrap();
    for _ in 0..2 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
        server.requests.recv().await.unwrap();
    }
    server.compactions.recv().await.unwrap();
    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    observer.block.store(true, Ordering::SeqCst);
    let pending_control = app.react(false).unwrap();
    {
        let pending = std::future::IntoFuture::into_future(pending_control.clone());
        tokio::pin!(pending);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = observer.entered.notified() => {}
                result = &mut pending => panic!("reaction completed before the handoff gate: {result:?}"),
            }
        }).await.unwrap();
    }
    pending_control.interrupt().unwrap();
    assert_eq!(
        pending_control.await.unwrap(),
        agentview::component::execution::ReactionOutcome::Interrupted
    );
    assert!(server.requests.try_recv().is_err());
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Ready);
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let retry = server.requests.recv().await.unwrap();
    assert_eq!(monitor.status().phase, ParallelCompactionPhase::Installed);
    let input = retry["input"].as_array().unwrap();
    assert!(input.starts_with(&output));
    for index in 1..=2 {
        let id = format!("call_{index}");
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "function_call_output" && item["call_id"] == id)
                .count(),
            1
        );
    }
    assert!(has_message(&retry, "user", "counter value: 2"));
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let feedback = server.requests.recv().await.unwrap();
    assert!(has_message(&feedback, "user", "counter value: 3"));
    assert!(feedback["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["call_id"] == "call_3" && item["output"] == "3"));
    app.shutdown().await.unwrap();
    server.finish().await;
}
