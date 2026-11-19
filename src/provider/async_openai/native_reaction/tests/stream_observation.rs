//! Live wire diagnostics exercise actual loopback HTTP and stalled SSE.

use super::*;
use crate::provider::async_openai::{
    OpenAiResponsesCallEvent, OpenAiResponsesCallOutcome, OpenAiResponsesStreamEvent,
    OpenAiResponsesStreamEventKind,
};
use std::time::Duration;

type Events = Arc<Mutex<Vec<OpenAiResponsesStreamEvent>>>;

fn observed(provider: AsyncOpenAiResponsesProvider) -> (AsyncOpenAiResponsesProvider, Events) {
    let events = Events::default();
    let sink = Arc::clone(&events);
    (
        provider.with_stream_observer(move |event| sink.lock().unwrap().push(event)),
        events,
    )
}

#[tokio::test]
async fn live_stream_observation_exposes_request_headers_chunks_and_sse_before_stalled_completion()
{
    type Requests = Arc<Mutex<Vec<Vec<u8>>>>;
    async fn reply(State(requests): State<Requests>, body: axum::body::Bytes) -> impl IntoResponse {
        requests.lock().unwrap().push(body.to_vec());
        let prefix = encode_sse([
            json!({"type":"response.created", "response":{
                "id":"response_live", "status":"in_progress", "output":[],
            }}),
            json!({"type":"response.output_item.added", "output_index":0, "item":{
                "id":"message_live", "type":"message", "role":"assistant", "status":"in_progress", "content":[],
            }}),
            json!({"type":"response.output_text.delta", "item_id":"message_live", "output_index":0, "content_index":0, "delta":"partial"}),
        ]);
        let stream = futures::stream::once(async move {
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(prefix))
        })
        .chain(futures::stream::pending());
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header("x-request-id", "live_request")
            .header("x-private-header", "never_capture_this_header")
            .body(Body::from_stream(stream))
            .unwrap()
    }
    let requests = Requests::default();
    let app = Router::new()
        .route("/responses", post(reply))
        .with_state(Arc::clone(&requests));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut provider, events) = observed(provider(&base, None)); // No call observer required.
    let frame = full_frame(&provider.declare().unwrap());
    let revision = frame.revision();
    let mut stream = provider.submit(frame).await.unwrap();
    assert!(matches!(
        events.lock().unwrap()[0].event,
        OpenAiResponsesStreamEventKind::Request { .. }
    ));
    let fact = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(fact, ProviderFact::TextDelta { .. }));
    {
        let events = events.lock().unwrap();
        assert!(events.iter().all(|event| event.frame_revision == revision));
        assert!(events
            .iter()
            .all(|event| event.call_id == events[0].call_id));
        assert!(events
            .windows(2)
            .all(|events| events[0].elapsed_ms <= events[1].elapsed_ms));
        assert!(events.iter().all(|event| event.timestamp_ms > 0));
        let OpenAiResponsesStreamEventKind::Request { body } = &events[0].event else {
            unreachable!()
        };
        assert_eq!(body.as_ref(), requests.lock().unwrap()[0].as_slice());
        assert!(events.iter().any(|event| matches!(&event.event,
            OpenAiResponsesStreamEventKind::Headers { status:200, request_id:Some(id) } if id == "live_request")));
        assert!(events.iter().any(|event| matches!(event.event, OpenAiResponsesStreamEventKind::BodyChunk { bytes } if bytes > 0)));
        let types = events
            .iter()
            .filter_map(|event| match &event.event {
                OpenAiResponsesStreamEventKind::Sse { data, .. } => Some(
                    serde_json::from_str::<Value>(data).unwrap()["type"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "response.created",
                "response.output_item.added",
                "response.output_text.delta"
            ]
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event.event, OpenAiResponsesStreamEventKind::Finished { .. })));
        assert!(!format!("{events:?}").contains("never_capture_this_header"));
    }
    drop(stream);
    let terminals = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event.event {
            OpenAiResponsesStreamEventKind::Finished { outcome } => Some(outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals, [OpenAiResponsesCallOutcome::Cancelled]);
    provider.shutdown().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn live_stream_observation_correlates_concurrent_fresh_sessions_with_call_accounting() {
    let body = encode_sse([json!({"type":"response.completed", "response":{
        "id":"response_complete", "status":"completed", "output":[],
    }})]);
    let (base, stop, server) = spawn_sse_server(body).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&calls);
    let (mut first, events) = observed(
        provider(&base, None).with_call_observer(move |call| sink.lock().unwrap().push(call)),
    );
    let mut second = first.fresh_session().unwrap();
    let a = full_frame(&first.declare().unwrap());
    let b = full_frame(&second.declare().unwrap());
    let (a_result, b_result) = tokio::join!(
        async { first.submit(a).await.unwrap().collect::<Vec<_>>().await },
        async { second.submit(b).await.unwrap().collect::<Vec<_>>().await }
    );
    assert!(a_result.iter().chain(&b_result).all(Result::is_ok));
    let events = events.lock().unwrap().clone();
    let requests = events
        .iter()
        .filter(|event| matches!(event.event, OpenAiResponsesStreamEventKind::Request { .. }))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].call_id, requests[1].call_id);
    assert_ne!(requests[0].frame_revision, requests[1].frame_revision);
    let calls = calls.lock().unwrap();
    for request in requests {
        assert!(calls.iter().any(|call| matches!(call, OpenAiResponsesCallEvent::Started(call) if call.call_id == request.call_id && call.frame_revision == request.frame_revision)));
        let terminals = events
            .iter()
            .filter(|event| {
                event.call_id == request.call_id
                    && matches!(
                        event.event,
                        OpenAiResponsesStreamEventKind::Finished {
                            outcome: OpenAiResponsesCallOutcome::Completed
                        }
                    )
            })
            .count();
        assert_eq!(terminals, 1);
    }
    drop(calls);
    first.shutdown().await;
    second.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn live_stream_observation_preserves_http_errors_and_ignores_preparation_rejection() {
    let response_body = json!({"error":{"code":"rate_limit"}}).to_string();
    let (base, stop, server) = spawn_response_server(
        StatusCode::TOO_MANY_REQUESTS,
        "application/json",
        response_body,
    )
    .await;
    let (mut provider, events) = observed(provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let facts = provider
        .submit(frame)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(facts.iter().any(Result::is_err));
    {
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event.event,
            OpenAiResponsesStreamEventKind::Headers { status: 429, .. }
        )));
        assert!(events.iter().any(|event| matches!(&event.event, OpenAiResponsesStreamEventKind::ResponseBody {body} if serde_json::from_slice::<Value>(body).unwrap()["error"]["code"] == "rate_limit")));
        assert!(matches!(
            events.last().unwrap().event,
            OpenAiResponsesStreamEventKind::Finished {
                outcome: OpenAiResponsesCallOutcome::Failed
            }
        ));
    }
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();

    let (mut provider, events) = observed(super::provider("http://127.0.0.1:1", Some(1)));
    let frame = full_frame(&provider.declare().unwrap());
    assert!(provider.submit(frame).await.is_err());
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn live_stream_observation_callback_panics_cannot_change_completion() {
    let body = encode_sse([json!({"type":"response.completed", "response":{
        "id":"response_complete", "status":"completed", "output":[],
    }})]);
    let (base, stop, server) = spawn_sse_server(body).await;
    let mut provider =
        provider(&base, None).with_stream_observer(|_| panic!("live diagnostic sink"));
    let frame = full_frame(&provider.declare().unwrap());
    let facts = provider
        .submit(frame)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(facts.iter().all(Result::is_ok));
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();
}
