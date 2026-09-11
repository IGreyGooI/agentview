use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::prompt_debug;
use agentview::{
    component::{
        execution::{Application, ProviderIdentity},
        prelude::*,
    },
    pom_renderer::render_pom_document,
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
    transcript::CanonicalInputItem,
};
use serde_json::Value;

#[allow(dead_code)]
#[path = "../support/responses_acceptance_server.rs"]
mod responses_acceptance_server;

use responses_acceptance_server::ResponsesAcceptanceServer;

const TEST_REPLIES: &[&str] = &["done"];
const RETAINED_HISTORY_REPLIES: &[&str] = &[
    "first retained-history reply",
    "second retained-history reply",
];
const FIRST_ROUND_HINT: &str = "history-hint-from-first-component-state";
const FIRST_STATE: &str = "state-A";
const SECOND_STATE: &str = "state-B";

#[component]
fn prompt_debug_test_application() -> Component {
    let context = use_signal(|| String::from("pending"));
    let prepared_context = context.clone();
    use_preparation(move || async move {
        if prepared_context.with(|value| value == "pending")? {
            prepared_context.set("prepared <context> & state".to_owned())?;
        }
        Ok::<(), SignalAccessError>(())
    });
    let context = context.with(Clone::clone).expect("mounted context");
    view! {
        #[system_once]
        { "# Assistant\n\nReturn one short reply." }
        #[developer]
        developer_context { "{context}" }
        #[user]
        { "## Request\n\nReply with done." }
        { NativeToolCall::new(crate::add) }
    }
}

#[derive(Clone)]
struct RetainedHistoryProps {
    completions: Arc<AtomicUsize>,
}

#[component]
fn first_round_history_hint() -> Component {
    view! {
        #[user]
        "history-hint-from-first-component-state"
    }
}

#[component]
fn retained_history_state(value: String) -> Component {
    view! {
        #[user]
        #[diff(slot = "current-state")]
        current_state { "{value}" }
    }
}

#[component]
fn retained_history_application(props: RetainedHistoryProps) -> Component {
    let state = use_signal(|| FIRST_STATE.to_owned());
    let current_state = state
        .with(Clone::clone)
        .expect("mounted retained-history state");
    let next_state = state.clone();
    let completions = Arc::clone(&props.completions);
    use_reaction_completion(move || async move {
        if completions.fetch_add(1, Ordering::SeqCst) == 0 {
            next_state.set(SECOND_STATE.to_owned())?;
        }
        Ok::<(), SignalAccessError>(())
    });

    let initial_history = if current_state == FIRST_STATE {
        first_round_history_hint()
    } else {
        view! {}
    };
    view! {
        { initial_history }
        retained_history_state(current_state)
    }
}

fn test_provider(api_base: &str) -> anyhow::Result<AsyncOpenAiResponsesProvider> {
    let transport = AsyncOpenAiTransportConfig::new(api_base, "test-token")?;
    let identity = ProviderIdentity::new(
        "openai",
        CODEX_HTTP_V1_PROFILE,
        1,
        "debug-prompt-example-test",
    )?;
    let options = CodexHttpV1Options::new("test-model", None, None, None::<String>)?;
    Ok(AsyncOpenAiResponsesProvider::new(
        transport,
        identity,
        CodexHttpV1Encoder::new(options),
    ))
}

async fn next_debug_request(server: &mut ResponsesAcceptanceServer) -> anyhow::Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(1), server.next_request())
        .await
        .map_err(|_| anyhow::anyhow!("mock Responses server did not receive a request in time"))?
}

fn projection_documents(projection: &Value) -> anyhow::Result<Vec<String>> {
    let nodes = projection
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("projection JSON has no nodes array"))?;
    let mut documents = Vec::new();
    for node in nodes {
        let items = node
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("projection node has no items array"))?;
        for item in items {
            let item: CanonicalInputItem = serde_json::from_value(item.clone())?;
            match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => {
                    documents.push(render_pom_document(&pom)?);
                }
                CanonicalInputItem::AssistantText { text, .. } => documents.push(text),
                CanonicalInputItem::ToolCall { .. }
                | CanonicalInputItem::ToolResult { .. }
                | CanonicalInputItem::ProviderExtension(_) => {}
            }
        }
    }
    Ok(documents)
}

fn request_messages(request: &Value) -> anyhow::Result<Vec<(String, String)>> {
    let input = request
        .get("input")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Responses request has no input array"))?;
    let mut messages = Vec::new();
    for item in input {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("wire message has no role"))?;
        let content = item
            .get("content")
            .ok_or_else(|| anyhow::anyhow!("wire message has no content"))?;
        let text = match content {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        messages.push((role.to_owned(), text));
    }
    Ok(messages)
}

fn contains_message(messages: &[(String, String)], role: &str, text: &str) -> bool {
    messages
        .iter()
        .any(|(actual_role, actual_text)| actual_role == role && actual_text.contains(text))
}

fn message_position(messages: &[(String, String)], role: &str, text: &str) -> Option<usize> {
    messages
        .iter()
        .position(|(actual_role, actual_text)| actual_role == role && actual_text.contains(text))
}

#[test]
fn readable_prompt_preserves_known_and_unknown_input_items() {
    let body = br#"{
        "model":"test-model",
        "instructions":"system instructions",
        "input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"developer context"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"user request"}]},
            {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"id\":7}"},
            {"type":"function_call_output","call_id":"call_1","output":"{\"status\":\"ready\"}"},
            {"type":"reasoning","encrypted_content":"opaque"}
        ],
        "tools":[{
            "type":"function",
            "name":"lookup",
            "description":"Look up a status.",
            "parameters":{"type":"object","properties":{"id":{"type":"integer"}}},
            "strict":true
        }]
    }"#;

    let prompt = prompt_debug::render_readable_prompt(body, "revision", "Full");

    assert!(prompt.contains("handoff: handed_off"));
    assert!(prompt.contains("## System instructions\n\nsystem instructions"));
    assert!(prompt.contains("## Developer message 1\n\ndeveloper context"));
    assert!(prompt.contains("## User message 2\n\nuser request"));
    assert!(prompt.contains("## Tool call 3: lookup"));
    assert!(prompt.contains("call_id: call_1"));
    assert!(prompt.contains("## Tool result 4"));
    assert!(prompt.contains("{\"status\":\"ready\"}"));
    assert!(prompt.contains("## Input item 5 (unknown type: reasoning)"));
    assert!(prompt.contains("\"encrypted_content\": \"opaque\""));
    assert!(prompt.contains("## Tool 1: lookup"));
    assert!(prompt.contains("Look up a status."));
}

#[tokio::test]
async fn prepared_projection_and_exact_requests_are_captured_separately() -> anyhow::Result<()> {
    let mut server = ResponsesAcceptanceServer::start(TEST_REPLIES).await?;
    let capture = prompt_debug::PromptDebugCapture::new()?;
    let run_dir = capture.output_dir().to_path_buf();
    let provider = test_provider(server.api_base())?.with_request_observer(capture.observer());
    let mut application = Application::mount(prompt_debug_test_application, provider)?;

    crate::capture_projection_before_reaction(&mut application, &run_dir, 1).await?;
    assert_eq!(
        server.request_count(),
        0,
        "projection capture must not submit"
    );
    let captured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("0001/projection.json"))?)?;
    assert_eq!(captured["prepared"], true);
    assert_eq!(captured["dirty"], false);
    assert_eq!(captured["native_tools"][0]["name"], "add");
    let nodes = captured["nodes"].as_array().expect("projection nodes");
    let current = application.current_projection();
    assert_eq!(nodes.len(), current.projection().nodes().len());
    for (captured_node, current_node) in nodes.iter().zip(current.projection().nodes()) {
        assert_eq!(captured_node["identity"], current_node.identity());
        assert_eq!(
            captured_node["items"],
            serde_json::to_value(current_node.items())?
        );
    }
    let projection = std::fs::read_to_string(run_dir.join("0001/projection.txt"))?;
    assert!(projection.contains("# Assistant\n\nReturn one short reply."));
    assert!(projection
        .contains("<developer_context>prepared &lt;context&gt; &amp; state</developer_context>"));
    assert!(!projection.contains("pending"));
    assert!(!projection.contains("context_management"));

    let reaction = application.react().await;
    let server_body = server.next_request().await?;
    crate::capture_projection_before_reaction(&mut application, &run_dir, 2).await?;
    assert_eq!(server.request_count(), 1);
    let second_projection: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("0002/projection.json"))?)?;
    assert_eq!(second_projection["nodes"], captured["nodes"]);
    let second_reaction = application.react().await;
    let second_body = server.next_request().await?;
    let shutdown = application.shutdown().await;
    let flushed = capture.finish().await;
    server.shutdown().await?;
    let _ = reaction?;
    let _ = second_reaction?;
    shutdown?;
    flushed?;

    let request_path = run_dir.join("0001/request.json");
    assert_eq!(std::fs::read(&request_path)?, server_body);
    let request: Value = serde_json::from_slice(&server_body)?;
    assert_eq!(
        request["instructions"],
        "# Assistant\n\nReturn one short reply."
    );
    assert_eq!(
        std::fs::read(run_dir.join("0002/request.json"))?,
        second_body
    );
    let second_request: serde_json::Value = serde_json::from_slice(&second_body)?;
    assert!(second_request["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| { item["role"] == "assistant" && item["content"][0]["text"] == "done" }));
    let prompt = std::fs::read_to_string(run_dir.join("0001/prompt.txt"))?;
    assert!(prompt.contains("## System instructions"));
    assert!(prompt.contains("## Developer message 1"));
    assert!(prompt.contains("## User message 2"));
    Ok(())
}

#[tokio::test]
async fn current_component_projection_excludes_retained_history_while_request_keeps_it(
) -> anyhow::Result<()> {
    let mut server = ResponsesAcceptanceServer::start(RETAINED_HISTORY_REPLIES).await?;
    let capture = prompt_debug::PromptDebugCapture::new()?;
    let run_dir = capture.output_dir().to_path_buf();
    let completions = Arc::new(AtomicUsize::new(0));
    let props = RetainedHistoryProps {
        completions: Arc::clone(&completions),
    };
    let root_props = props.clone();
    let provider = test_provider(server.api_base())?.with_request_observer(capture.observer());
    let mut application = Application::mount(
        move || retained_history_application(root_props.clone()),
        provider,
    )?;

    let operation = async {
        for _ in 0..2 {
            anyhow::ensure!(
                application.prepare().await?.is_continue(),
                "initial no-op preparation exited"
            );
        }
        anyhow::ensure!(
            completions.load(Ordering::SeqCst) == 0,
            "preparation ran a reaction completion before the first handoff"
        );

        crate::capture_projection_before_reaction(&mut application, &run_dir, 1).await?;
        anyhow::ensure!(
            server.request_count() == 0,
            "projection capture submitted a request before the first reaction"
        );
        let first_projection: Value =
            serde_json::from_slice(&std::fs::read(run_dir.join("0001/projection.json"))?)?;
        assert_eq!(first_projection["scope"], "current_component_requirements");
        let first_documents = projection_documents(&first_projection)?;
        assert!(
            first_documents
                .iter()
                .any(|document| document == FIRST_ROUND_HINT),
            "first projection did not expose the one-time plain user hint: {first_documents:#?}"
        );
        assert!(
            first_documents
                .iter()
                .any(|document| document.contains(FIRST_STATE)),
            "first projection did not expose state A: {first_documents:#?}"
        );

        let _ = application.react().await?;
        let first_body = next_debug_request(&mut server).await?;
        anyhow::ensure!(
            completions.load(Ordering::SeqCst) == 1,
            "first reaction completion did not update the state exactly once"
        );

        for _ in 0..2 {
            anyhow::ensure!(
                application.prepare().await?.is_continue(),
                "post-completion no-op preparation exited"
            );
        }
        anyhow::ensure!(
            completions.load(Ordering::SeqCst) == 1,
            "no-op preparation consumed the reaction completion again"
        );

        crate::capture_projection_before_reaction(&mut application, &run_dir, 2).await?;
        let second_projection: Value =
            serde_json::from_slice(&std::fs::read(run_dir.join("0002/projection.json"))?)?;
        assert_eq!(second_projection["scope"], "current_component_requirements");
        let second_documents = projection_documents(&second_projection)?;
        assert!(
            second_documents
                .iter()
                .any(|document| document.contains(SECOND_STATE)),
            "second projection did not expose state B: {second_documents:#?}"
        );
        assert!(
            !second_documents
                .iter()
                .any(|document| document.contains(FIRST_ROUND_HINT)),
            "second projection retained the first-round hint: {second_documents:#?}"
        );
        assert!(
            !second_documents
                .iter()
                .any(|document| document.contains(FIRST_STATE)),
            "second projection retained state A: {second_documents:#?}"
        );
        let second_projection_text = std::fs::read_to_string(run_dir.join("0002/projection.txt"))?;
        assert!(second_projection_text.contains(SECOND_STATE));
        assert!(!second_projection_text.contains(FIRST_ROUND_HINT));
        assert!(!second_projection_text.contains(FIRST_STATE));

        let _ = application.react().await?;
        let second_body = next_debug_request(&mut server).await?;
        anyhow::ensure!(
            completions.load(Ordering::SeqCst) == 2,
            "each completed response should run one reaction completion"
        );

        Ok::<_, anyhow::Error>((first_body, second_body))
    }
    .await;
    let shutdown = application.shutdown().await;
    let flushed = capture.finish().await;
    let server_shutdown = server.shutdown().await;

    let (first_body, second_body) = operation?;
    shutdown?;
    flushed?;
    server_shutdown?;

    assert_eq!(
        std::fs::read(run_dir.join("0001/request.json"))?,
        first_body,
        "captured first request must equal the mock server body"
    );
    assert_eq!(
        std::fs::read(run_dir.join("0002/request.json"))?,
        second_body,
        "captured second request must equal the mock server body"
    );

    let second_request: Value = serde_json::from_slice(&second_body)?;
    let second_messages = request_messages(&second_request)?;
    assert!(
        contains_message(&second_messages, "user", FIRST_ROUND_HINT),
        "second request lost the first-round user hint: {second_messages:#?}"
    );
    assert!(
        contains_message(&second_messages, "assistant", RETAINED_HISTORY_REPLIES[0]),
        "second request lost the first assistant reply: {second_messages:#?}"
    );
    assert!(
        contains_message(&second_messages, "user", FIRST_STATE),
        "second request lost state A from retained history: {second_messages:#?}"
    );
    assert!(
        contains_message(&second_messages, "user", SECOND_STATE),
        "second request omitted the current state B: {second_messages:#?}"
    );
    let first_state_position = message_position(&second_messages, "user", FIRST_STATE)
        .expect("state A presence was asserted above");
    let second_state_position = message_position(&second_messages, "user", SECOND_STATE)
        .expect("state B presence was asserted above");
    assert!(
        first_state_position < second_state_position,
        "retained state A must precede current state B: {second_messages:#?}"
    );

    let second_prompt = std::fs::read_to_string(run_dir.join("0002/prompt.txt"))?;
    assert!(second_prompt.contains(FIRST_ROUND_HINT));
    assert!(second_prompt.contains(RETAINED_HISTORY_REPLIES[0]));
    assert!(second_prompt.contains(FIRST_STATE));
    assert!(second_prompt.contains(SECOND_STATE));
    Ok(())
}
