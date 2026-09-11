#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises the Chat ProviderPort adapter"
)]

use std::convert::Infallible;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use super::super::AsyncOpenAiTransportConfig;
use super::{
    AsyncOpenAiChatCompletionsProvider, ChatCompletionsConfigError, OpenAiChatCompletionsOptions,
    OPENAI_CHAT_COMPLETIONS_PROFILE,
};
use crate::{
    component::{
        execution::{
            ApplicationHost, ProviderEvent, ProviderFault, ProviderFaultCode, ProviderFaultKind,
            ProviderIdentity, ProviderPort, RenderedProjection, RenderedProjectionNode,
        },
        prelude::*,
        ComponentHost,
    },
    llm_call::TextTurnEvent,
    pom::{Document, ResolvedDocument, TextNode, XmlNode},
    pom_resolution::resolve_artifact_document,
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

const BINDING: &str = "chat-completions-test-binding";
const AMBIENT_HEADERS_CHILD_ENV: &str = "AGENTVIEW_CHAT_AMBIENT_HEADERS_TEST_CHILD";

#[derive(Clone)]
struct DiffPromptProps {
    value: String,
}

#[derive(Clone)]
struct ChatInputGateProps {
    system: String,
    state: String,
}

#[derive(Clone)]
struct ChatNativeToolGateProps {
    state: String,
    declare_tool: bool,
}

#[derive(Clone)]
struct ChatCaptureProps {
    completed: Arc<Mutex<Vec<String>>>,
}

#[component]
fn chat_capture_application(
    props: ChatCaptureProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let completed = props.completed;
    let text = events.select(ProviderEvent::TEXT);
    view! {
        #[system_once]
        capture_system { "Return one concise result." }

        capture_request { "Provide the result." }

        {
            EventListener::observe("chat.capture", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let completed = Arc::clone(&completed);
                    async move {
                        if let TextTurnEvent::TextComplete(text) = event {
                            completed.lock().unwrap().push(text);
                        }
                        Ok::<(), Infallible>(())
                    }
                })
        }
    }
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

#[component]
fn chat_input_gate_application(
    props: ChatInputGateProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let system = props.system;
    let state = props.state;
    view! {
        #[system_once]
        gate_system { "{system}" }

        #[diff(slot = "state")]
        gate_state { "{state}" }
    }
}

#[component]
fn chat_native_tool_gate_application(
    props: ChatNativeToolGateProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let state = props.state;
    if props.declare_tool {
        view! {
            #[diff(slot = "state")]
            native_gate_state { "{state}" }
            {
                NativeToolCall::named("native-gate-secret-tool")
                    .on_call(|call| async move {
                        Ok::<_, String>(call.output("unused-native-tool-output"))
                    })
            }
        }
    } else {
        view! {
            #[diff(slot = "state")]
            native_gate_state { "{state}" }
        }
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
enum ServerReply {
    Completed(&'static [&'static str]),
    Raw(&'static str),
    HttpStatus(StatusCode),
    NonSse,
}

#[derive(Clone)]
struct ServerState {
    attempts: Arc<AtomicUsize>,
    bodies: mpsc::UnboundedSender<Vec<u8>>,
    replies: Arc<Vec<ServerReply>>,
}

fn completed_response(chunks: &[&str]) -> Response {
    let mut body = String::new();
    let first = serde_json::json!({
        "id": "chatcmpl_test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-chat-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": ""},
            "finish_reason": null,
        }],
    });
    body.push_str(&format!("data: {first}\n\n"));
    for chunk in chunks {
        let frame = serde_json::json!({
            "id": "chatcmpl_test",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "test-chat-model",
            "choices": [{
                "index": 0,
                "delta": {"content": chunk},
                "finish_reason": null,
            }],
        });
        body.push_str(&format!("data: {frame}\n\n"));
    }
    let terminal = serde_json::json!({
        "id": "chatcmpl_test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-chat-model",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
        }],
    });
    body.push_str(&format!("data: {terminal}\n\n"));
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn chat_endpoint(State(state): State<ServerState>, body: Bytes) -> Response {
    let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
    state.bodies.send(body.to_vec()).unwrap();
    match state
        .replies
        .get(attempt)
        .or_else(|| state.replies.last())
        .expect("test server has at least one response")
    {
        ServerReply::Completed(chunks) => completed_response(chunks),
        ServerReply::Raw(body) => {
            ([(header::CONTENT_TYPE, "text/event-stream")], *body).into_response()
        }
        ServerReply::HttpStatus(status) => (*status, "upstream-secret").into_response(),
        ServerReply::NonSse => (
            [(header::CONTENT_TYPE, "application/json")],
            "non-sse-secret",
        )
            .into_response(),
    }
}

async fn spawn_server(
    replies: Vec<ServerReply>,
) -> (
    String,
    mpsc::UnboundedReceiver<Vec<u8>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    assert!(!replies.is_empty());
    let (body_tx, body_rx) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/chat/completions", post(chat_endpoint))
        .with_state(ServerState {
            attempts: Arc::new(AtomicUsize::new(0)),
            bodies: body_tx,
            replies: Arc::new(replies),
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
    (format!("http://{address}"), body_rx, shutdown_tx, server)
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
        completed_response(&["ok"])
    }

    let (header_tx, header_rx) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/chat/completions", post(capture_headers))
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

fn xml_document(name: &str, text: &str) -> ResolvedDocument {
    let node = XmlNode::try_build(name, |children| {
        children.text(TextNode::new(text));
        Ok(())
    })
    .unwrap();
    resolve_artifact_document(Document::from_xml(node)).unwrap()
}

fn projection(items: Vec<CanonicalInputItem>) -> RenderedProjection {
    RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", items)])
        .expect("the test root projection identity is valid")
}

fn provider(api_base: String) -> AsyncOpenAiChatCompletionsProvider {
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token").unwrap();
    provider_with_config(config)
}

fn provider_with_config(config: AsyncOpenAiTransportConfig) -> AsyncOpenAiChatCompletionsProvider {
    let identity =
        ProviderIdentity::new("openai", OPENAI_CHAT_COMPLETIONS_PROFILE, 1, BINDING).unwrap();
    let options = OpenAiChatCompletionsOptions::new("test-chat-model").unwrap();
    AsyncOpenAiChatCompletionsProvider::new(config, identity, options)
}

async fn trace_provider(
    provider: &mut AsyncOpenAiChatCompletionsProvider,
    projection: RenderedProjection,
) -> (Vec<ProviderEvent>, Option<ProviderFault>) {
    let mut stream = match provider.execute(projection).await {
        Ok(stream) => stream,
        Err(fault) => return (Vec::new(), Some(fault)),
    };
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(fault) => return (events, Some(fault)),
        }
    }
    (events, None)
}

async fn gate_fault(
    provider: &mut AsyncOpenAiChatCompletionsProvider,
    projection: RenderedProjection,
) -> ProviderFault {
    match provider.execute(projection).await {
        Err(fault) => fault,
        Ok(_) => panic!("Chat Input Gate failure was deferred until after handoff"),
    }
}

async fn trace_handed_off_provider(
    provider: &mut AsyncOpenAiChatCompletionsProvider,
    projection: RenderedProjection,
) -> (Vec<ProviderEvent>, Option<ProviderFault>) {
    let mut stream = match provider.execute(projection).await {
        Ok(stream) => stream,
        Err(_) => panic!("post-handoff Chat fault was returned directly from execute"),
    };
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(fault) => return (events, Some(fault)),
        }
    }
    (events, None)
}

fn text_trace(events: Vec<ProviderEvent>) -> Vec<(&'static str, String)> {
    events
        .into_iter()
        .map(|event| match event {
            ProviderEvent::Text(TextTurnEvent::TextDelta(text)) => ("delta", text),
            ProviderEvent::Text(TextTurnEvent::TextComplete(text)) => ("complete", text),
            _ => panic!("Chat Completions emitted a non-text event"),
        })
        .collect()
}

fn assert_chat_request_body_limit_fault(fault: &ProviderFault, excluded: &str) {
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
    assert_eq!(fault.code(), ProviderFaultCode::RequestPreparation);
    assert_eq!(
        fault.message(),
        "OpenAI Chat Completions serialized outbound request body exceeded configured limit"
    );
    assert!(!format!("{fault:?}\n{fault}").contains(excluded));
}

#[test]
fn serialized_chat_request_body_limit_must_be_nonzero() {
    let config = AsyncOpenAiTransportConfig::new("http://127.0.0.1:1", "test-token").unwrap();

    let error = match config.with_chat_completions_serialized_request_body_limit(0) {
        Ok(_) => panic!("a zero Chat serialized request-body limit was accepted"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        ChatCompletionsConfigError::InvalidSerializedRequestBodyLimit
    );
    assert_eq!(
        error.to_string(),
        "OpenAI Chat Completions serialized outbound request body limit must be non-zero"
    );
}

#[tokio::test]
async fn explicit_api_base_ignores_ambient_openai_routing_headers() {
    if std::env::var_os(AMBIENT_HEADERS_CHILD_ENV).is_none() {
        let test_name = concat!(
            module_path!(),
            "::explicit_api_base_ignores_ambient_openai_routing_headers"
        )
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .expect("the test module belongs to the current crate");
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg(test_name)
            .arg("--exact")
            .arg("--nocapture")
            .env(AMBIENT_HEADERS_CHILD_ENV, "1")
            .env("OPENAI_API_KEY", "ambient-token")
            .env("OPENAI_ORG_ID", "invalid\norganization")
            .env("OPENAI_PROJECT_ID", "invalid\nproject")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "isolated ambient-header regression child failed"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("test {test_name} ... ok")),
            "isolated ambient-header regression child did not execute the test"
        );
        return;
    }

    let (api_base, mut headers, shutdown, server) = spawn_header_capture_server().await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;
    assert!(fault.is_none(), "explicit API configuration must complete");

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
async fn complete_projection_maps_instruction_authority_and_streams_text() {
    let (api_base, mut bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Completed(&["hello ", "world"])]).await;
    let mut provider = provider(api_base);
    let current = projection(vec![
        CanonicalInputItem::instruction(
            InstructionAuthority::System,
            xml_document("rules", "system"),
        ),
        CanonicalInputItem::instruction(
            InstructionAuthority::Developer,
            xml_document("policy", "developer"),
        ),
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "user")),
    ]);

    let (events, fault) = trace_provider(&mut provider, current).await;

    assert!(fault.is_none());
    assert_eq!(
        text_trace(events),
        [
            ("delta", "hello ".to_owned()),
            ("delta", "world".to_owned()),
            ("complete", "hello world".to_owned()),
        ]
    );
    let request: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(request["model"], "test-chat-model");
    assert_eq!(request["stream"], true);
    assert_eq!(request["tool_choice"], "none");
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["messages"][1]["role"], "developer");
    assert_eq!(request["messages"][2]["role"], "user");

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn serialized_chat_request_body_limit_uses_exact_json_bytes_before_dispatch() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["calibration"]),
        ServerReply::Completed(&["exact"]),
    ])
    .await;
    let raw_bytes = 512;
    let exact_text = "p".repeat(raw_bytes);
    let exact_projection = projection(vec![CanonicalInputItem::message(
        ConversationRole::User,
        xml_document("payload", &exact_text),
    )]);
    let mut calibration = provider(api_base.clone());

    let (_, calibration_fault) = trace_provider(&mut calibration, exact_projection.clone()).await;
    assert!(calibration_fault.is_none());
    let exact_body = bodies.recv().await.unwrap();
    let exact_limit = exact_body.len();
    assert!(
        exact_limit > raw_bytes,
        "the serialized envelope has structure"
    );

    let exact_config = AsyncOpenAiTransportConfig::new(api_base.clone(), "test-token")
        .unwrap()
        .with_chat_completions_serialized_request_body_limit(exact_limit)
        .unwrap();
    let mut exact_provider = provider_with_config(exact_config);
    let (_, exact_fault) = trace_provider(&mut exact_provider, exact_projection).await;
    assert!(
        exact_fault.is_none(),
        "the exact byte limit must be inclusive"
    );
    assert_eq!(bodies.recv().await.unwrap(), exact_body);

    let plus_one_sentinel = format!("{exact_text}x");
    let plus_one_config = AsyncOpenAiTransportConfig::new(api_base.clone(), "test-token")
        .unwrap()
        .with_chat_completions_serialized_request_body_limit(exact_limit)
        .unwrap();
    let mut plus_one_provider = provider_with_config(plus_one_config);
    let plus_one_fault = gate_fault(
        &mut plus_one_provider,
        projection(vec![CanonicalInputItem::message(
            ConversationRole::User,
            xml_document("payload", &plus_one_sentinel),
        )]),
    )
    .await;
    assert_chat_request_body_limit_fault(&plus_one_fault, &plus_one_sentinel);
    assert!(bodies.try_recv().is_err(), "limit plus one reached HTTP");

    let escaped_sentinel = "\"".repeat(raw_bytes);
    let escaped_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_chat_completions_serialized_request_body_limit(exact_limit)
        .unwrap();
    let mut escaped_provider = provider_with_config(escaped_config);
    let escaped_fault = gate_fault(
        &mut escaped_provider,
        projection(vec![CanonicalInputItem::message(
            ConversationRole::User,
            xml_document("payload", &escaped_sentinel),
        )]),
    )
    .await;
    assert_chat_request_body_limit_fault(&escaped_fault, &escaped_sentinel);
    assert!(bodies.try_recv().is_err(), "escaped overflow reached HTTP");

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn serialized_chat_request_body_limit_failure_preserves_history_and_diff_memo() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["retained-answer"]),
        ServerReply::Completed(&["retained-answer"]),
        ServerReply::Completed(&["retained-answer"]),
        ServerReply::Completed(&["retained-answer"]),
    ])
    .await;
    let initial = ChatInputGateProps {
        system: "stable-limit-system".to_owned(),
        state: "state-a".to_owned(),
    };
    let valid_retry = ChatInputGateProps {
        system: "stable-limit-system".to_owned(),
        state: "state-c".to_owned(),
    };

    let mut calibration = provider(api_base.clone());
    let mut calibration_host = ComponentHost::new(chat_input_gate_application, initial.clone());
    let first = calibration_host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut calibration, first).await.1.is_none());
    let _calibration_first = bodies.recv().await.unwrap();
    calibration_host.set_props(valid_retry.clone());
    let retry = calibration_host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut calibration, retry).await.1.is_none());
    let calibrated_retry_body = bodies.recv().await.unwrap();

    let limited_config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_chat_completions_serialized_request_body_limit(calibrated_retry_body.len())
        .unwrap();
    let mut limited = provider_with_config(limited_config);
    let mut limited_host = ComponentHost::new(chat_input_gate_application, initial);
    let first = limited_host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut limited, first).await.1.is_none());
    let _limited_first = bodies.recv().await.unwrap();

    let oversized_sentinel = "oversized-gate-secret".repeat(calibrated_retry_body.len());
    limited_host.set_props(ChatInputGateProps {
        system: "stable-limit-system".to_owned(),
        state: oversized_sentinel.clone(),
    });
    let oversized = limited_host.render().unwrap().projection().clone();
    let first_fault = gate_fault(&mut limited, oversized.clone()).await;
    assert_chat_request_body_limit_fault(&first_fault, &oversized_sentinel);
    let retry_fault = gate_fault(&mut limited, oversized).await;
    assert_chat_request_body_limit_fault(&retry_fault, &oversized_sentinel);
    assert!(bodies.try_recv().is_err(), "a rejected retry reached HTTP");

    limited_host.set_props(valid_retry);
    let valid = limited_host.render().unwrap().projection().clone();
    let (_, valid_fault) = trace_provider(&mut limited, valid).await;
    assert!(valid_fault.is_none());
    let valid_body = bodies.recv().await.unwrap();
    assert_eq!(valid_body, calibrated_retry_body);
    assert!(!String::from_utf8(valid_body)
        .unwrap()
        .contains(&oversized_sentinel));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn private_assistant_history_is_reused_without_duplicate_projection_submission() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["first answer"]),
        ServerReply::Completed(&["second answer"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let first_user =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "first"));
    let second_user =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "second"));

    let (_, first_fault) =
        trace_provider(&mut provider, projection(vec![first_user.clone()])).await;
    assert!(first_fault.is_none());
    let _first_request = bodies.recv().await.unwrap();

    let (_, second_fault) = trace_provider(
        &mut provider,
        projection(vec![
            first_user,
            CanonicalInputItem::assistant_text("first answer", None),
            second_user,
        ]),
    )
    .await;
    assert!(second_fault.is_none());
    let request: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "first answer");
    assert_eq!(messages[2]["role"], "user");

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn diff_marked_component_state_is_lowered_against_the_accepted_chat_baseline() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["first answer"]),
        ServerReply::Completed(&["second answer"]),
        ServerReply::Completed(&["third answer"]),
        ServerReply::Completed(&["fourth answer"]),
    ])
    .await;
    let mut provider = provider(api_base);

    let (_, first_fault) = trace_provider(&mut provider, diff_projection("A")).await;
    assert!(first_fault.is_none());
    let first: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_eq!(
        first["messages"][0]["content"],
        "<current_state>A</current_state>"
    );

    let (_, second_fault) = trace_provider(&mut provider, diff_projection("A+")).await;
    assert!(second_fault.is_none());
    let second: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let submitted = second["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert_eq!(submitted, "<current_state>A+</current_state>");

    let (_, third_fault) = trace_provider(&mut provider, diff_projection("A")).await;
    assert!(third_fault.is_none());
    let third: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let submitted = third["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert_eq!(submitted, "<current_state>A</current_state>");

    let (_, fourth_fault) = trace_provider(&mut provider, diff_projection("A+")).await;
    assert!(fourth_fault.is_none());
    let fourth: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let submitted = fourth["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert_eq!(submitted, "<current_state>A+</current_state>");

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn changed_system_is_rejected_without_advancing_history_or_diff_memo() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["first answer"]),
        ServerReply::Completed(&["retry answer"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let mut host = ComponentHost::new(
        chat_input_gate_application,
        ChatInputGateProps {
            system: "system-authority-a".to_owned(),
            state: "state-a".to_owned(),
        },
    );

    let first = host.render().unwrap().projection().clone();
    let (_, first_fault) = trace_provider(&mut provider, first).await;
    assert!(first_fault.is_none());
    let _first_body = bodies.recv().await.unwrap();

    host.set_props(ChatInputGateProps {
        system: "system-authority-b-secret".to_owned(),
        state: "state-b".to_owned(),
    });
    let changed = host.render().unwrap().projection().clone();
    let changed_fault = gate_fault(&mut provider, changed).await;
    assert_eq!(changed_fault.kind(), ProviderFaultKind::ModelRejected);
    assert_eq!(changed_fault.code(), ProviderFaultCode::RequestPreparation);
    assert_eq!(
        changed_fault.message(),
        "Chat Completions System instruction changed after the first submission"
    );
    assert!(!format!("{changed_fault:?}\n{changed_fault}").contains("system-authority-b-secret"));
    assert!(
        bodies.try_recv().is_err(),
        "the rejected candidate reached HTTP"
    );

    host.set_props(ChatInputGateProps {
        system: "system-authority-a".to_owned(),
        state: "state-b".to_owned(),
    });
    let retry = host.render().unwrap().projection().clone();
    let (_, retry_fault) = trace_provider(&mut provider, retry).await;
    assert!(
        retry_fault.is_none(),
        "the unchanged System must remain legal"
    );
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let messages = retry["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[0]["content"],
        "<gate_system>system-authority-a</gate_system>"
    );
    assert_eq!(messages[1]["content"], "<gate_state>state-a</gate_state>");
    assert_eq!(messages[2]["content"], "first answer");
    assert_eq!(messages[3]["content"], "<gate_state>state-b</gate_state>");
    assert!(!retry.to_string().contains("system-authority-b-secret"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn identical_system_remains_legal_after_projection_scope_reset() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["first answer"]),
        ServerReply::Completed(&["remount answer"]),
        ServerReply::Completed(&["final answer"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let mut first_host = ComponentHost::new(
        chat_input_gate_application,
        ChatInputGateProps {
            system: "stable-remount-system".to_owned(),
            state: "state-a".to_owned(),
        },
    );
    let first = first_host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut provider, first).await.1.is_none());
    let _first_body = bodies.recv().await.unwrap();

    let mut remounted_host = ComponentHost::new(
        chat_input_gate_application,
        ChatInputGateProps {
            system: "stable-remount-system".to_owned(),
            state: "state-b".to_owned(),
        },
    );
    let remounted = remounted_host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut provider, remounted).await.1.is_none());
    let _remounted_body = bodies.recv().await.unwrap();

    remounted_host.set_props(ChatInputGateProps {
        system: "stable-remount-system".to_owned(),
        state: "state-c".to_owned(),
    });
    let final_projection = remounted_host.render().unwrap().projection().clone();
    let (_, final_fault) = trace_provider(&mut provider, final_projection).await;
    assert!(
        final_fault.is_none(),
        "an unchanged System must remain legal after a scope reset"
    );
    let final_body = String::from_utf8(bodies.recv().await.unwrap()).unwrap();
    assert!(final_body.contains("state-c"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn native_tool_declarations_are_rejected_without_advancing_history_or_diff_memo() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["retained answer"]),
        ServerReply::Completed(&["retry answer"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let mut host = ComponentHost::new(
        chat_native_tool_gate_application,
        ChatNativeToolGateProps {
            state: "native-state-a".to_owned(),
            declare_tool: false,
        },
    );

    let first = host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut provider, first).await.1.is_none());
    let _first_body = bodies.recv().await.unwrap();

    host.set_props(ChatNativeToolGateProps {
        state: "native-state-b".to_owned(),
        declare_tool: true,
    });
    let rejected = host.render().unwrap().projection().clone();
    for projection in [rejected.clone(), rejected] {
        let fault = gate_fault(&mut provider, projection).await;
        assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
        assert_eq!(fault.code(), ProviderFaultCode::RequestPreparation);
        assert_eq!(
            fault.message(),
            "OpenAI Chat Completions does not support native tool declarations"
        );
        assert!(!format!("{fault:?}\n{fault}").contains("native-gate-secret-tool"));
    }
    assert!(
        bodies.try_recv().is_err(),
        "a rejected declaration reached HTTP"
    );

    host.set_props(ChatNativeToolGateProps {
        state: "native-state-b".to_owned(),
        declare_tool: false,
    });
    let retry = host.render().unwrap().projection().clone();
    assert!(trace_provider(&mut provider, retry).await.1.is_none());
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let messages = retry["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0]["content"],
        "<native_gate_state>native-state-a</native_gate_state>"
    );
    assert_eq!(messages[1]["content"], "retained answer");
    assert_eq!(
        messages[2]["content"],
        "<native_gate_state>native-state-b</native_gate_state>"
    );
    assert!(!retry.to_string().contains("native-gate-secret-tool"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn faulted_diff_submission_replays_causal_input_and_retained_partial() {
    let incomplete = concat!(
        "data: {\"id\":\"chatcmpl_diff_fault\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"test-chat-model\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"content\":\"discarded\"},\"finish_reason\":null}]}\n\n"
    );
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["accepted"]),
        ServerReply::Raw(incomplete),
        ServerReply::Completed(&["recovered"]),
    ])
    .await;
    let mut provider = provider(api_base);

    let (_, first_fault) = trace_provider(&mut provider, diff_projection("A")).await;
    assert!(first_fault.is_none());
    let (_, faulted) = trace_provider(&mut provider, diff_projection("B")).await;
    assert!(faulted.is_some());
    let (_, retry_fault) = trace_provider(&mut provider, diff_projection("B")).await;
    assert!(retry_fault.is_none());

    let _accepted = bodies.recv().await.unwrap();
    let faulted: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let retry: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    assert_ne!(retry, faulted);
    let contents = retry["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        contents,
        [
            "<current_state>A</current_state>",
            "accepted",
            "<current_state>B</current_state>",
            "discarded",
            "[agentview: assistant output interrupted before completion]",
            "<current_state>B</current_state>",
        ]
    );

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn provider_private_output_bridges_a_projection_that_has_not_claimed_it_yet() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["first answer"]),
        ServerReply::Completed(&["second answer"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let first_user =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "first"));
    let second_user =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "second"));

    let _ = trace_provider(&mut provider, projection(vec![first_user.clone()])).await;
    let _ = bodies.recv().await.unwrap();
    let _ = trace_provider(&mut provider, projection(vec![first_user, second_user])).await;
    let request: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "first answer");

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn eof_without_done_is_a_retryable_stream_fault() {
    let incomplete = concat!(
        "data: {\"id\":\"chatcmpl_test\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"test-chat-model\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"content\":\"partial-secret\"},\"finish_reason\":null}]}\n\n"
    );
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Raw(incomplete)]).await;
    let mut provider = provider(api_base);

    let (events, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    assert_eq!(text_trace(events), [("delta", "partial-secret".to_owned())]);
    let fault = fault.expect("unterminated Chat stream must fail");
    assert_eq!(fault.kind(), ProviderFaultKind::RetryableTransport);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseProtocol);
    assert!(!fault.message().contains("partial-secret"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn dropping_after_text_complete_retains_validated_history_before_public_eof() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["uncommitted answer"]),
        ServerReply::Completed(&["next answer"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let first = projection(vec![CanonicalInputItem::message(
        ConversationRole::User,
        xml_document("request", "first"),
    )]);
    let mut stream = provider.execute(first).await.unwrap();
    while let Some(event) = stream.next().await {
        if matches!(
            event.unwrap(),
            ProviderEvent::Text(TextTurnEvent::TextComplete(_))
        ) {
            break;
        }
    }
    drop(stream);
    let _first_request = bodies.recv().await.unwrap();

    let next = projection(vec![CanonicalInputItem::message(
        ConversationRole::User,
        xml_document("request", "next"),
    )]);
    let (_, fault) = trace_provider(&mut provider, next).await;
    assert!(fault.is_none());
    let request: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"], "<request>first</request>");
    assert_eq!(messages[1]["content"], "uncommitted answer");
    assert_eq!(messages[2]["content"], "<request>next</request>");

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn faulted_submission_retains_causal_input_and_interrupted_partial_history() {
    let incomplete = concat!(
        "data: {\"id\":\"chatcmpl_fault\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"test-chat-model\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"content\":\"discarded\"},\"finish_reason\":null}]}\n\n"
    );
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["confirmed"]),
        ServerReply::Raw(incomplete),
        ServerReply::Completed(&["recovered"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let first =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "first"));
    let second =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "second"));
    let third =
        CanonicalInputItem::message(ConversationRole::User, xml_document("request", "third"));

    let (_, first_fault) = trace_provider(&mut provider, projection(vec![first.clone()])).await;
    assert!(first_fault.is_none());
    let _ = bodies.recv().await.unwrap();
    let (_, second_fault) = trace_provider(
        &mut provider,
        projection(vec![first.clone(), second.clone()]),
    )
    .await;
    assert!(second_fault.is_some());
    let _ = bodies.recv().await.unwrap();

    let (_, third_fault) = trace_provider(&mut provider, projection(vec![first, third])).await;
    assert!(third_fault.is_none());
    let request: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let contents = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        contents,
        [
            "<request>first</request>",
            "confirmed",
            "<request>second</request>",
            "discarded",
            "[agentview: assistant output interrupted before completion]",
            "<request>third</request>"
        ]
    );
    assert!(request.to_string().contains("discarded"));
    assert!(request.to_string().contains("second"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn compatible_history_appends_new_items_in_current_tree_order() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![
        ServerReply::Completed(&["provider one"]),
        ServerReply::Completed(&["provider two"]),
    ])
    .await;
    let mut provider = provider(api_base);
    let item = |text: &str| CanonicalInputItem::assistant_text(text, None);
    let first = RenderedProjection::from_nodes(vec![
        RenderedProjectionNode::new("left", vec![item("A"), item("B")]),
        RenderedProjectionNode::new("right", vec![item("O")]),
    ])
    .unwrap();
    let second = RenderedProjection::from_nodes(vec![
        RenderedProjectionNode::new("right", vec![item("O"), item("P")]),
        RenderedProjectionNode::new("left", vec![item("A"), item("B"), item("C")]),
    ])
    .unwrap();

    let (_, first_fault) = trace_provider(&mut provider, first).await;
    assert!(first_fault.is_none());
    let first_request: serde_json::Value =
        serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let first_contents = first_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(first_contents, ["A", "B", "O"]);
    let (_, second_fault) = trace_provider(&mut provider, second).await;
    assert!(second_fault.is_none());
    let request: serde_json::Value = serde_json::from_slice(&bodies.recv().await.unwrap()).unwrap();
    let contents = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(contents, ["A", "B", "O", "provider one", "P", "C"]);

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn tool_call_output_fails_closed_without_exposing_wire_content() {
    let tool_call = concat!(
        "data: {\"id\":\"chatcmpl_tool\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"test-chat-model\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"secret-call\",",
        "\"type\":\"function\",\"function\":{\"name\":\"secret-tool\",",
        "\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n"
    );
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Raw(tool_call)]).await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    let fault = fault.expect("native tool output is deferred for B5");
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
    assert_eq!(fault.code(), ProviderFaultCode::ModelRejected);
    assert!(!fault.message().contains("secret-call"));
    assert!(!fault.message().contains("secret-tool"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn output_limit_fails_before_emitting_the_overflowing_delta() {
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Completed(&["1234", "overflow-secret"])]).await;
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")
        .unwrap()
        .with_response_limits(1024 * 1024, 64 * 1024, 4)
        .unwrap();
    let mut provider = provider_with_config(config);

    let (events, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    assert_eq!(text_trace(events), [("delta", "1234".to_owned())]);
    let fault = fault.expect("overflowing output must fail");
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
    assert_eq!(fault.code(), ProviderFaultCode::OutputLimit);
    assert!(!fault.message().contains("overflow-secret"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[test]
fn chat_model_must_be_nonempty() {
    let error = OpenAiChatCompletionsOptions::new("").unwrap_err();
    assert!(error.to_string().contains("must be non-empty"));
}

#[tokio::test]
async fn done_without_stop_is_an_incomplete_transport_fault() {
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Raw("data: [DONE]\n\n")]).await;
    let mut provider = provider(api_base);

    let (events, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    assert!(events.is_empty());
    let fault = fault.expect("[DONE] without a terminal choice must fail");
    assert_eq!(fault.kind(), ProviderFaultKind::RetryableTransport);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseProtocol);

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn truncated_finish_reason_is_model_rejected() {
    let length = concat!(
        "data: {\"id\":\"chatcmpl_length\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"test-chat-model\",\"choices\":[{\"index\":0,",
        "\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (api_base, _bodies, shutdown, server) = spawn_server(vec![ServerReply::Raw(length)]).await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    let fault = fault.expect("a truncated completion is not a normal text completion");
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn multiple_choices_are_rejected_by_the_single_text_reaction() {
    let multiple = concat!(
        "data: {\"id\":\"chatcmpl_multiple\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"test-chat-model\",\"choices\":[",
        "{\"index\":0,\"delta\":{\"content\":\"first-secret\"},\"finish_reason\":null},",
        "{\"index\":1,\"delta\":{\"content\":\"second-secret\"},\"finish_reason\":null}]}\n\n"
    );
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Raw(multiple)]).await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    let fault = fault.expect("one reaction cannot silently select among choices");
    assert_eq!(fault.kind(), ProviderFaultKind::ModelRejected);
    assert!(!fault.message().contains("first-secret"));
    assert!(!fault.message().contains("second-secret"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn malformed_json_has_a_protocol_code_without_exposing_the_frame() {
    let malformed = "data: {malformed-json-secret}\n\n";
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Raw(malformed)]).await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    let fault = fault.expect("malformed JSON must fail");
    assert_eq!(fault.code(), ProviderFaultCode::ResponseProtocol);
    assert!(!fault.message().contains("malformed-json-secret"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn successful_non_sse_response_is_rejected_before_parsing() {
    let (api_base, _bodies, shutdown, server) = spawn_server(vec![ServerReply::NonSse]).await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    let fault = fault.expect("Chat streaming requires an SSE response");
    assert_eq!(fault.kind(), ProviderFaultKind::RetryableTransport);
    assert_eq!(fault.code(), ProviderFaultCode::ResponseProtocol);
    assert!(!fault.message().contains("non-sse-secret"));

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn http_faults_have_structural_sanitized_codes() {
    let cases = [
        (
            "authentication",
            StatusCode::UNAUTHORIZED,
            ProviderFaultCode::Authentication,
        ),
        (
            "authorization",
            StatusCode::FORBIDDEN,
            ProviderFaultCode::Authorization,
        ),
        (
            "rate-limit",
            StatusCode::TOO_MANY_REQUESTS,
            ProviderFaultCode::RateLimited,
        ),
        (
            "upstream-status",
            StatusCode::SERVICE_UNAVAILABLE,
            ProviderFaultCode::UpstreamStatus,
        ),
    ];

    for (name, status, expected) in cases {
        let (api_base, mut bodies, shutdown, server) =
            spawn_server(vec![ServerReply::HttpStatus(status)]).await;
        let mut provider = provider(api_base);

        let (_, fault) = trace_handed_off_provider(&mut provider, projection(Vec::new())).await;

        let fault = fault.expect("HTTP status must be surfaced");
        assert_eq!(fault.code(), expected, "case {name}");
        assert!(!fault.message().contains("upstream-secret"), "case {name}");
        let _first_request = bodies.recv().await.unwrap();
        assert!(bodies.try_recv().is_err(), "case {name}");
        let _ = shutdown.send(());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn transient_status_is_redacted_and_has_no_hidden_retry() {
    let (api_base, mut bodies, shutdown, server) = spawn_server(vec![ServerReply::HttpStatus(
        StatusCode::SERVICE_UNAVAILABLE,
    )])
    .await;
    let mut provider = provider(api_base);

    let (_, fault) = trace_provider(&mut provider, projection(Vec::new())).await;

    let fault = fault.expect("503 must be surfaced to the Host");
    assert_eq!(fault.kind(), ProviderFaultKind::RetryableTransport);
    assert_eq!(fault.code(), ProviderFaultCode::UpstreamStatus);
    assert!(!fault.message().contains("upstream-secret"));
    let _first_request = bodies.recv().await.unwrap();
    assert!(
        bodies.try_recv().is_err(),
        "the adapter must not retry itself"
    );

    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn application_host_dispatches_chat_events_through_the_existing_consumer_path() {
    let (api_base, _bodies, shutdown, server) =
        spawn_server(vec![ServerReply::Completed(&["accepted"])]).await;
    let completed = Arc::new(Mutex::new(Vec::new()));
    let mut components = ComponentHost::new(
        chat_capture_application,
        ChatCaptureProps {
            completed: Arc::clone(&completed),
        },
    );
    let mut application = ApplicationHost::new(provider(api_base));

    application
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap();

    assert_eq!(&*completed.lock().unwrap(), &["accepted"]);

    let _ = shutdown.send(());
    server.await.unwrap();
}
