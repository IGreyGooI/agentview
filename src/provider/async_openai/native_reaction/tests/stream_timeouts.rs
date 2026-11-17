//! Raw HTTP activity, including incomplete SSE events, owns the idle deadline.

use super::*;
use std::{convert::Infallible, time::Duration};

struct TestServer(tokio::task::JoinHandle<()>);

impl Drop for TestServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn streaming_server(chunks: Vec<(Duration, String)>) -> (String, TestServer) {
    let chunks = Arc::new(chunks);
    let app = Router::new().route(
        "/responses",
        post(move || {
            let chunks = Arc::clone(&chunks);
            async move {
                let body = futures::stream::iter(chunks.as_ref().clone()).then(
                    |(delay, bytes)| async move {
                        tokio::time::sleep(delay).await;
                        Ok::<_, Infallible>(bytes)
                    },
                );
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(body))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, TestServer(task))
}

fn streaming_provider(base: &str, idle: Duration) -> AsyncOpenAiResponsesProvider {
    let config = AsyncOpenAiTransportConfig::new(base, "test-token")
        .unwrap()
        .with_timeouts(Duration::from_secs(1), Duration::from_millis(250), idle)
        .unwrap()
        .with_stream_timeouts(Duration::from_secs(1), idle)
        .unwrap();
    let identity = ProviderIdentity::new("openai", "stream-timeouts", 1, "test").unwrap();
    let options = CodexHttpV1Options::new(
        crate::provider::ModelSpec::new("test-model", 272_000).unwrap(),
        None,
        None,
        None::<String>,
    )
    .unwrap();
    AsyncOpenAiResponsesProvider::try_new(config, identity, CodexHttpV1Encoder::new(options))
        .unwrap()
}

async fn facts(
    provider: &mut AsyncOpenAiResponsesProvider,
) -> Vec<Result<ProviderFact, crate::component::execution::ReactionPortFault>> {
    let frame = full_frame(&provider.declare().unwrap());
    tokio::time::timeout(Duration::from_secs(3), async {
        provider.submit(frame).await.unwrap().collect().await
    })
    .await
    .expect("loopback response must complete or hit its idle budget")
}

fn assert_complete_hello(
    facts: &[Result<ProviderFact, crate::component::execution::ReactionPortFault>],
) {
    assert!(
        facts.iter().all(Result::is_ok),
        "unexpected fault: {facts:?}"
    );
    assert!(facts.iter().any(|fact| matches!(fact,
        Ok(ProviderFact::TextSealed { text, .. }) if text == "hello"
    )));
    assert!(matches!(
        facts.last(),
        Some(Ok(ProviderFact::ReactionCompleted { .. }))
    ));
}

#[tokio::test]
async fn stream_idle_resets_for_fragments_before_one_sse_event_is_complete() {
    let body = completed_text_sse();
    let event_end = body.find("\n\n").unwrap() + 2;
    let first_event = &body[..event_end];
    let piece_bytes = first_event.len().div_ceil(8);
    let mut chunks = first_event
        .as_bytes()
        .chunks(piece_bytes)
        .enumerate()
        .map(|(index, bytes)| {
            (
                if index == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(50)
                },
                String::from_utf8(bytes.to_vec()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    chunks.push((Duration::ZERO, body[event_end..].to_owned()));
    let (base, _server) = streaming_server(chunks).await;
    let mut provider = streaming_provider(&base, Duration::from_millis(200));

    assert_complete_hello(&facts(&mut provider).await);
    provider.shutdown().await;
}

#[tokio::test]
async fn stream_idle_resets_for_heartbeats_without_a_semantic_sse_event() {
    let mut chunks = vec![(Duration::ZERO, ": keepalive\n\n".to_owned())];
    chunks.extend((0..5).map(|_| (Duration::from_millis(75), ": keepalive\n\n".to_owned())));
    chunks.push((Duration::ZERO, completed_text_sse()));
    let (base, _server) = streaming_server(chunks).await;
    let mut provider = streaming_provider(&base, Duration::from_millis(200));

    assert_complete_hello(&facts(&mut provider).await);
    provider.shutdown().await;
}

#[tokio::test]
async fn stream_idle_still_fails_after_real_network_silence() {
    let chunks = vec![
        (Duration::ZERO, ": keepalive\n\n".to_owned()),
        (Duration::from_millis(600), completed_text_sse()),
    ];
    let (base, _server) = streaming_server(chunks).await;
    let mut provider = streaming_provider(&base, Duration::from_millis(150));

    let result = facts(&mut provider).await;
    assert_eq!(result.len(), 1);
    assert!(matches!(&result[0], Err(fault)
        if fault.code() == ReactionPortFaultCode::Unavailable
            && matches!(fault.reason(), ReactionPortFaultReason::StreamTransport | ReactionPortFaultReason::StreamTimeout)
    ));
    provider.shutdown().await;
}
