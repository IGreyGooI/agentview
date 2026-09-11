//! Regression coverage for Responses request observation at transport handoff.

use std::{
    ops::ControlFlow,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{Application, FrameBasis, ProviderIdentity},
        prelude::*,
    },
    provider::{
        async_openai::{
            AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig,
            OpenAiResponsesRequestSnapshot,
        },
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
};
use axum::http::StatusCode;
use serde_json::Value;

#[allow(dead_code)]
#[path = "support/responses_acceptance_server.rs"]
mod responses_acceptance_server;

use responses_acceptance_server::ResponsesAcceptanceServer;

const BINDING: &str = "component-api-request-observer";
const TEXT_REPLIES: &[&str] = &["first assistant reply", "second assistant reply"];

#[derive(Clone)]
struct TwoTurnProps {
    completed_turns: Arc<AtomicUsize>,
}

#[component]
fn two_turn_application(props: TwoTurnProps) -> Component {
    let turn = use_signal(|| String::from("first-turn"));
    let current_turn = turn.with(Clone::clone).expect("mounted request turn");
    let next_turn = turn.clone();
    let completed_turns = Arc::clone(&props.completed_turns);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let next_turn = next_turn.clone();
        let completed_turns = Arc::clone(&completed_turns);
        async move {
            if let TextTurnEvent::TextComplete(_) = event {
                let next = if completed_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                    "second-turn"
                } else {
                    "later-turn"
                };
                next_turn.set(next.to_owned())?;
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    view! {
        #[system_once]
        request_observer_policy { "Return the supplied state as plain text." }
        #[diff(slot = "turn")]
        request_observer_turn { "{current_turn}" }
    }
}

fn responses_provider(
    api_base: &str,
    serialized_body_limit: Option<usize>,
) -> anyhow::Result<AsyncOpenAiResponsesProvider> {
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token")?;
    let config = match serialized_body_limit {
        Some(limit) => config.with_responses_serialized_request_body_limit(limit)?,
        None => config,
    };
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING)?;
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)?;
    Ok(AsyncOpenAiResponsesProvider::new(
        config,
        identity,
        CodexHttpV1Encoder::new(options),
    ))
}

async fn next_request(server: &mut ResponsesAcceptanceServer) -> anyhow::Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(1), server.next_request())
        .await
        .map_err(|_| anyhow::anyhow!("mock Responses server did not receive a request in time"))?
}

async fn assert_no_request(server: &mut ResponsesAcceptanceServer) -> anyhow::Result<()> {
    match tokio::time::timeout(Duration::from_millis(100), server.next_request()).await {
        Err(_) => Ok(()),
        Ok(Ok(body)) => anyhow::bail!(
            "pre-handoff preparation unexpectedly reached the mock server with a {} byte body",
            body.len()
        ),
        Ok(Err(error)) => Err(error),
    }
}

fn message_texts(body: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let request: Value = serde_json::from_slice(body)?;
    let input = request
        .get("input")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Responses request has no input array"))?;
    input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .map(|item| {
            let role = item
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("wire message has no role"))?;
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .and_then(|parts| {
                    parts
                        .iter()
                        .find_map(|part| part.get("text").and_then(Value::as_str))
                })
                .ok_or_else(|| anyhow::anyhow!("wire message has no text content"))?;
            Ok((role.to_owned(), text.to_owned()))
        })
        .collect()
}

fn contains_message(messages: &[(String, String)], role: &str, text: &str) -> bool {
    messages
        .iter()
        .any(|(actual_role, actual_text)| actual_role == role && actual_text.contains(text))
}

#[tokio::test]
async fn request_observer_captures_handed_off_bodies_and_frame_context() -> anyhow::Result<()> {
    let mut server = ResponsesAcceptanceServer::start(TEXT_REPLIES).await?;
    let snapshots = Arc::new(Mutex::new(Vec::<OpenAiResponsesRequestSnapshot>::new()));
    let observer_snapshots = Arc::clone(&snapshots);
    let provider =
        responses_provider(server.api_base(), None)?.with_request_observer(move |snapshot| {
            observer_snapshots
                .lock()
                .expect("request observer snapshot lock")
                .push(snapshot);
        });
    let props = TwoTurnProps {
        completed_turns: Arc::new(AtomicUsize::new(0)),
    };
    let root_props = props.clone();
    let mut application =
        Application::mount(move || two_turn_application(root_props.clone()), provider)?;

    assert!(matches!(
        application.react().await?,
        ControlFlow::Continue(())
    ));
    let first_body = next_request(&mut server).await?;
    assert!(matches!(
        application.react().await?,
        ControlFlow::Continue(())
    ));
    let second_body = next_request(&mut server).await?;

    let snapshots = snapshots
        .lock()
        .expect("request observer snapshot lock")
        .clone();
    assert_eq!(snapshots.len(), 2, "one observer event per handoff");
    assert_eq!(snapshots[0].body(), first_body.as_slice());
    assert_eq!(snapshots[1].body(), second_body.as_slice());
    assert_eq!(snapshots[0].frame_basis(), FrameBasis::Full);
    assert_eq!(
        snapshots[1].frame_basis(),
        FrameBasis::DeltaFrom(snapshots[0].frame_revision())
    );

    let second_messages = message_texts(&second_body)?;
    assert!(
        contains_message(&second_messages, "user", "first-turn"),
        "second request lost the first user input: {second_messages:#?}"
    );
    assert!(
        contains_message(&second_messages, "assistant", "first assistant reply"),
        "second request lost the first assistant reply: {second_messages:#?}"
    );
    assert!(
        contains_message(&second_messages, "user", "second-turn"),
        "second request omitted its current user input: {second_messages:#?}"
    );
    assert_eq!(props.completed_turns.load(Ordering::SeqCst), 2);

    application.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn request_observer_does_not_publish_pre_handoff_preparation_failures() -> anyhow::Result<()>
{
    let mut server = ResponsesAcceptanceServer::start(TEXT_REPLIES).await?;
    let snapshots = Arc::new(Mutex::new(Vec::<OpenAiResponsesRequestSnapshot>::new()));
    let observer_snapshots = Arc::clone(&snapshots);
    let provider =
        responses_provider(server.api_base(), Some(1))?.with_request_observer(move |snapshot| {
            observer_snapshots
                .lock()
                .expect("request observer snapshot lock")
                .push(snapshot);
        });
    let props = TwoTurnProps {
        completed_turns: Arc::new(AtomicUsize::new(0)),
    };
    let root_props = props.clone();
    let mut application =
        Application::mount(move || two_turn_application(root_props.clone()), provider)?;

    let _fault = application
        .react()
        .await
        .expect_err("one-byte request body limit must reject before handoff");
    assert!(
        snapshots
            .lock()
            .expect("request observer snapshot lock")
            .is_empty(),
        "failed preparation must not publish an observer event"
    );
    assert_eq!(server.request_count(), 0);
    assert_no_request(&mut server).await?;

    application.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn request_observer_captures_handed_off_requests_before_http_failure() -> anyhow::Result<()> {
    let mut server =
        ResponsesAcceptanceServer::start_failure(StatusCode::SERVICE_UNAVAILABLE).await?;
    let snapshots = Arc::new(Mutex::new(Vec::<OpenAiResponsesRequestSnapshot>::new()));
    let observer_snapshots = Arc::clone(&snapshots);
    let provider =
        responses_provider(server.api_base(), None)?.with_request_observer(move |snapshot| {
            observer_snapshots
                .lock()
                .expect("request observer snapshot lock")
                .push(snapshot);
        });
    let props = TwoTurnProps {
        completed_turns: Arc::new(AtomicUsize::new(0)),
    };
    let root_props = props.clone();
    let mut application =
        Application::mount(move || two_turn_application(root_props.clone()), provider)?;

    let _fault = application
        .react()
        .await
        .expect_err("service failure follows a completed transport handoff");
    let body = next_request(&mut server).await?;
    let snapshots = snapshots
        .lock()
        .expect("request observer snapshot lock")
        .clone();
    assert_eq!(server.request_count(), 1);
    assert_eq!(
        snapshots.len(),
        1,
        "handoff must publish before HTTP status handling"
    );
    assert_eq!(snapshots[0].body(), body.as_slice());
    assert_eq!(snapshots[0].frame_basis(), FrameBasis::Full);

    application.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn request_observer_panics_do_not_break_reactions_or_continuity() -> anyhow::Result<()> {
    let mut server = ResponsesAcceptanceServer::start(TEXT_REPLIES).await?;
    let snapshots = Arc::new(Mutex::new(Vec::<OpenAiResponsesRequestSnapshot>::new()));
    let observer_snapshots = Arc::clone(&snapshots);
    let panic_calls = Arc::new(AtomicUsize::new(0));
    let observer_panic_calls = Arc::clone(&panic_calls);
    let provider =
        responses_provider(server.api_base(), None)?.with_request_observer(move |snapshot| {
            observer_snapshots
                .lock()
                .expect("request observer snapshot lock")
                .push(snapshot);
            observer_panic_calls.fetch_add(1, Ordering::SeqCst);
            panic!("request observer panic probe");
        });
    let props = TwoTurnProps {
        completed_turns: Arc::new(AtomicUsize::new(0)),
    };
    let root_props = props.clone();
    let mut application =
        Application::mount(move || two_turn_application(root_props.clone()), provider)?;

    assert!(matches!(
        application.react().await?,
        ControlFlow::Continue(())
    ));
    let first_body = next_request(&mut server).await?;
    assert!(matches!(
        application.react().await?,
        ControlFlow::Continue(())
    ));
    let second_body = next_request(&mut server).await?;

    let snapshots = snapshots
        .lock()
        .expect("request observer snapshot lock")
        .clone();
    assert_eq!(panic_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        snapshots.len(),
        2,
        "panic observer remains invoked per handoff"
    );
    assert_eq!(snapshots[0].body(), first_body.as_slice());
    assert_eq!(snapshots[1].body(), second_body.as_slice());
    assert_eq!(snapshots[0].frame_basis(), FrameBasis::Full);
    assert_eq!(
        snapshots[1].frame_basis(),
        FrameBasis::DeltaFrom(snapshots[0].frame_revision())
    );
    assert!(
        contains_message(
            &message_texts(&second_body)?,
            "assistant",
            "first assistant reply"
        ),
        "second reaction must retain continuity after an observer panic"
    );
    assert_eq!(props.completed_turns.load(Ordering::SeqCst), 2);

    application.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}
