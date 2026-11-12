//! Per-HTTP-call accounting uses real loopback transport, never model APIs.

use super::*;
use crate::provider::async_openai::{
    OpenAiResponsesCallEvent, OpenAiResponsesCallKind, OpenAiResponsesCallOutcome,
    OpenAiResponsesCallSnapshot,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

type Events = Arc<Mutex<Vec<OpenAiResponsesCallEvent>>>;

fn observed(provider: AsyncOpenAiResponsesProvider) -> (AsyncOpenAiResponsesProvider, Events) {
    let events = Events::default();
    let sink = Arc::clone(&events);
    (
        provider.with_call_observer(move |event| sink.lock().unwrap().push(event)),
        events,
    )
}

fn finished(events: &Events) -> Vec<OpenAiResponsesCallSnapshot> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            OpenAiResponsesCallEvent::Finished(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn assert_pairs(events: &Events, count: usize) {
    let events = events.lock().unwrap();
    assert_eq!(events.len(), count * 2);
    let mut starts = BTreeMap::new();
    let mut finishes = BTreeMap::new();
    for event in events.iter() {
        match event {
            OpenAiResponsesCallEvent::Started(call) => {
                assert!(call.outcome.is_none());
                assert!(call.finished_at_unix_ms.is_none());
                assert!(call.usage.is_none());
                assert!(starts.insert(call.call_id.clone(), call).is_none());
            }
            OpenAiResponsesCallEvent::Finished(call) => {
                assert!(call.outcome.is_some());
                assert!(call.finished_at_unix_ms.is_some());
                assert!(call.elapsed_ms.is_some());
                let start = starts.get(&call.call_id).expect("start precedes terminal");
                assert_eq!(start.target, call.target);
                assert_eq!(start.frame_revision, call.frame_revision);
                assert_eq!(start.frame_basis, call.frame_basis);
                assert_eq!(start.model, call.model);
                assert!(finishes.insert(call.call_id.clone(), call).is_none());
            }
        }
    }
    assert_eq!(starts.len(), count);
    assert_eq!(finishes.len(), count);
}

fn complete(usage: Option<Value>) -> String {
    let mut response =
        json!({"id":"resp_accounting", "status":"completed", "model":"billed-model", "output":[]});
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    encode_sse([json!({"type":"response.completed", "response":response})])
}

fn reported_usage() -> Value {
    json!({
        "input_tokens":100,
        "input_tokens_details":{"cached_tokens":40,"cache_creation_tokens":15,"cache_storage_tokens":7},
        "output_tokens":20,
        "output_tokens_details":{"reasoning_tokens":11},
        "total_tokens":120,
        "cost":0.125,
        "cost_details":{"upstream_inference_cost":0.1},
    })
}

#[tokio::test]
async fn call_accounting_preserves_raw_usage_response_model_and_whitelisted_request_id() {
    async fn reply(State(count): State<Arc<AtomicUsize>>) -> impl IntoResponse {
        count.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header("x-request-id", "request_accounting")
            .header("x-private-header", "secret-never-in-accounting")
            .body(Body::from(complete(Some(reported_usage()))))
            .unwrap()
    }
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/responses", post(reply))
        .with_state(Arc::clone(&count));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut provider, events) = observed(provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let target = frame.target();
    let revision = frame.revision();
    let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
    assert!(matches!(
        facts.as_slice(),
        [Ok(ProviderFact::ReactionCompleted { .. })]
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_pairs(&events, 1);
    let call = finished(&events).pop().unwrap();
    assert_eq!(call.kind, OpenAiResponsesCallKind::Foreground);
    assert_eq!(call.target, target);
    assert_eq!(call.frame_revision, revision);
    assert_eq!(call.model, "test-model");
    assert_eq!(call.response_model.as_deref(), Some("billed-model"));
    assert_eq!(call.response_id.as_deref(), Some("resp_accounting"));
    assert_eq!(call.request_id.as_deref(), Some("request_accounting"));
    assert_eq!(call.http_status, Some(200));
    assert_eq!(call.usage, Some(reported_usage()));
    assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Completed));
    assert!(!format!("{call:?}").contains("secret-never-in-accounting"));
    provider.shutdown().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn call_accounting_keeps_unknown_usage_and_incomplete_or_failed_reported_usage() {
    for (body, outcome, usage) in [
        (complete(None), OpenAiResponsesCallOutcome::Completed, None),
        (
            encode_sse([json!({"type":"response.incomplete", "response":{
                "id":"resp_partial", "status":"incomplete", "usage":reported_usage(),
            }})]),
            OpenAiResponsesCallOutcome::Incomplete,
            Some(reported_usage()),
        ),
        (
            encode_sse([json!({"type":"response.failed", "response":{
                "id":"resp_failed", "status":"failed", "usage":reported_usage(),
            }})]),
            OpenAiResponsesCallOutcome::Failed,
            Some(reported_usage()),
        ),
    ] {
        let (base, stop, server) = spawn_sse_server(body).await;
        let (mut provider, events) = observed(provider(&base, None));
        let frame = full_frame(&provider.declare().unwrap());
        let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
        assert_eq!(
            facts.iter().all(Result::is_ok),
            outcome == OpenAiResponsesCallOutcome::Completed
        );
        assert_pairs(&events, 1);
        let call = finished(&events).pop().unwrap();
        assert_eq!(call.outcome, Some(outcome));
        assert_eq!(call.usage, usage);
        provider.shutdown().await;
        stop.send(()).unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn call_accounting_retains_error_http_usage_and_counts_transport_failure() {
    let body = json!({"id":"resp_error", "request_id":"body_request", "usage":reported_usage(), "error":{"code":"rate_limit"}}).to_string();
    let (base, stop, server) =
        spawn_response_server(StatusCode::TOO_MANY_REQUESTS, "application/json", body).await;
    let (mut provider, events) = observed(provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
    assert!(facts.iter().any(Result::is_err));
    assert_pairs(&events, 1);
    let call = finished(&events).pop().unwrap();
    assert_eq!(call.http_status, Some(429));
    assert_eq!(call.request_id.as_deref(), Some("body_request"));
    assert_eq!(call.usage, Some(reported_usage()));
    assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Failed));
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (mut provider, events) = observed(super::provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
    assert!(facts.iter().any(Result::is_err));
    assert_pairs(&events, 1);
    let call = finished(&events).pop().unwrap();
    assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Failed));
    assert!(call.http_status.is_none());
    assert!(call.usage.is_none());
}

#[tokio::test]
async fn call_accounting_has_no_calls_for_rejected_or_continuity_retried_preparation() {
    let (mut provider, events) = observed(provider("http://127.0.0.1:1", Some(1)));
    let frame = full_frame(&provider.declare().unwrap());
    assert!(provider.submit(frame).await.is_err());
    assert!(events.lock().unwrap().is_empty());

    let (base, stop, server) = spawn_sse_server(complete(None)).await;
    let (mut provider, events) = observed(super::provider(&base, None));
    let stale = full_frame(&provider.declare().unwrap());
    provider.reaction_target.lose_continuity();
    assert!(matches!(
        provider.submit(stale).await,
        Err(SubmitFault::ContinuityChanged)
    ));
    assert!(events.lock().unwrap().is_empty());
    let frame = full_frame(&provider.declare().unwrap());
    let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
    assert!(facts.iter().all(Result::is_ok));
    assert_pairs(&events, 1);
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn call_accounting_closes_dropped_transport_and_stream_once_with_known_metadata() {
    let (base, stop, server) = spawn_sse_server(complete(Some(reported_usage()))).await;
    let (mut provider, events) = observed(provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let stream = provider.submit(frame).await.unwrap();
    drop(stream); // The returned stream was never polled after HTTP handoff.
    assert_pairs(&events, 1);
    assert_eq!(
        finished(&events)[0].outcome,
        Some(OpenAiResponsesCallOutcome::Cancelled)
    );
    assert!(finished(&events)[0].usage.is_none());
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();

    let body = encode_sse([json!({"type":"response.created", "response":{
        "id":"resp_started", "status":"in_progress", "model":"partial-model", "output":[],
    }})]);
    let (base, stop, server) = spawn_sse_server(body).await;
    let (mut provider, events) = observed(super::provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
    assert!(facts.iter().any(Result::is_err));
    assert_pairs(&events, 1);
    let call = finished(&events).pop().unwrap();
    assert_eq!(call.response_id.as_deref(), Some("resp_started"));
    assert_eq!(call.response_model.as_deref(), Some("partial-model"));
    assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Failed));
    assert!(call.usage.is_none());
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn call_accounting_midstream_cancellation_retains_usage_already_received() {
    async fn reply() -> impl IntoResponse {
        let prefix = encode_sse([
            json!({"type":"response.created", "response":{
                "id":"resp_cancelled", "status":"in_progress", "model":"partial-model", "output":[], "usage":reported_usage(),
            }}),
            json!({"type":"response.output_item.added", "output_index":0, "item":{
                "id":"msg_cancelled", "type":"message", "role":"assistant", "status":"in_progress", "content":[],
            }}),
            json!({"type":"response.output_text.delta", "item_id":"msg_cancelled", "output_index":0, "content_index":0, "delta":"partial"}),
        ]);
        let stream = futures::stream::once(async move {
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(prefix))
        })
        .chain(futures::stream::pending());
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header("x-tt-logid", "ark_request_id")
            .body(Body::from_stream(stream))
            .unwrap()
    }
    let app = Router::new().route("/responses", post(reply));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut provider, events) = observed(provider(&base, None));
    let frame = full_frame(&provider.declare().unwrap());
    let mut stream = provider.submit(frame).await.unwrap();
    let fact = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(fact, ProviderFact::TextDelta { .. }));
    assert!(finished(&events).is_empty());
    drop(stream);
    assert_pairs(&events, 1);
    let call = finished(&events).pop().unwrap();
    assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Cancelled));
    assert_eq!(call.usage, Some(reported_usage()));
    assert_eq!(call.request_id.as_deref(), Some("ark_request_id"));
    assert_eq!(call.response_id.as_deref(), Some("resp_cancelled"));
    provider.shutdown().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn call_accounting_temporary_forks_keep_distinct_targets_and_calls() {
    let (base, stop, server) = spawn_sse_server(complete(Some(reported_usage()))).await;
    let (mut provider, events) = observed(provider(&base, None));
    let declaration = provider.declare().unwrap();
    let facts: Vec<_> = provider
        .submit(full_frame(&declaration))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(facts.iter().all(Result::is_ok));
    let mut temporary = provider.fork_temporary().unwrap();
    let child = temporary.declare().unwrap();
    let facts: Vec<_> = temporary
        .submit(full_frame(&child))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(facts.iter().all(Result::is_ok));
    assert_pairs(&events, 2);
    let calls = finished(&events);
    assert_ne!(calls[0].call_id, calls[1].call_id);
    assert_eq!(calls[0].target, declaration.identity());
    assert_eq!(calls[1].target, child.identity());
    assert_ne!(calls[0].target, calls[1].target);
    assert_eq!(calls[1].usage, Some(reported_usage()));
    temporary.shutdown().await;
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn call_accounting_panics_cannot_change_http_outcome_or_reaction_continuity() {
    let (base, stop, server) = spawn_sse_server(complete(None)).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&calls);
    let mut provider = provider(&base, None).with_call_observer(move |_| {
        sink.fetch_add(1, Ordering::SeqCst);
        panic!("accounting sink probe");
    });
    let frame = full_frame(&provider.declare().unwrap());
    let facts: Vec<_> = provider.submit(frame).await.unwrap().collect().await;
    assert!(matches!(
        facts.as_slice(),
        [Ok(ProviderFact::ReactionCompleted { .. })]
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        provider.declare().unwrap().continuity(),
        TargetContinuity::Accepted { .. }
    ));
    provider.shutdown().await;
    stop.send(()).unwrap();
    server.await.unwrap();
}
