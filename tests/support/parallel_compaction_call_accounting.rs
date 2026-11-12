//! Call accounting must include summary workers outside the foreground task.

use super::*;
use agentview::provider::async_openai::{
    OpenAiResponsesCallEvent, OpenAiResponsesCallKind, OpenAiResponsesCallOutcome,
    OpenAiResponsesCallSnapshot,
};

type Events = Arc<Mutex<Vec<OpenAiResponsesCallEvent>>>;

fn accounting(provider: AsyncOpenAiResponsesProvider) -> (AsyncOpenAiResponsesProvider, Events) {
    let events = Events::default();
    let sink = Arc::clone(&events);
    (
        provider.with_call_observer(move |event| sink.lock().unwrap().push(event)),
        events,
    )
}

fn calls(events: &Events) -> Vec<OpenAiResponsesCallSnapshot> {
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

fn summary_usage() -> Value {
    json!({
        "input_tokens":1000, "input_tokens_details":{"cached_tokens":900,"cache_creation_tokens":33},
        "output_tokens":40, "output_tokens_details":{"reasoning_tokens":12}, "total_tokens":1040,
        "cost":0.01,
    })
}

fn reply_with_usage() -> Value {
    let mut reply = summary_reply(SUMMARY_TEXT);
    reply["model"] = json!("summary-billed-model");
    reply["request_id"] = json!("summary-request-id");
    reply["usage"] = summary_usage();
    reply
}

fn assert_complete_pairs(events: &Events, expected: usize) {
    let events = events.lock().unwrap();
    let mut started = std::collections::BTreeMap::new();
    let mut terminal = std::collections::BTreeSet::new();
    for event in events.iter() {
        match event {
            OpenAiResponsesCallEvent::Started(call) => {
                assert!(started.insert(call.call_id.clone(), call).is_none());
            }
            OpenAiResponsesCallEvent::Finished(call) => {
                let start = started
                    .get(&call.call_id)
                    .expect("call start precedes its terminal");
                assert_eq!(start.target, call.target);
                assert_eq!(start.frame_revision, call.frame_revision);
                assert_eq!(start.kind, call.kind);
                assert!(terminal.insert(&call.call_id));
            }
        }
    }
    assert_eq!(started.len(), expected);
    assert_eq!(terminal.len(), expected);
}

#[tokio::test]
async fn background_call_keeps_its_original_frame_after_newer_foreground_calls() {
    let mut server = Server::start(reply_with_usage()).await;
    let (provider, events) = accounting(provider(&server.base, ContextRatioPolicy::default()));
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let text = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(Arc::clone(&text)), provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.compactions.recv().await.unwrap();
    let trigger = calls(&events)
        .into_iter()
        .rfind(|call| call.kind == OpenAiResponsesCallKind::Foreground)
        .unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    let newest = calls(&events)
        .into_iter()
        .rfind(|call| call.kind == OpenAiResponsesCallKind::Foreground)
        .unwrap();
    assert_ne!(trigger.frame_revision, newest.frame_revision);
    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Ready).await;
    let summary = calls(&events)
        .into_iter()
        .find(|call| call.kind == OpenAiResponsesCallKind::BackgroundCompaction)
        .unwrap();
    assert_eq!(summary.target, trigger.target);
    assert_eq!(summary.frame_revision, trigger.frame_revision);
    assert_eq!(summary.frame_basis, trigger.frame_basis);
    assert_eq!(summary.outcome, Some(OpenAiResponsesCallOutcome::Completed));
    assert_eq!(summary.model, "test-model");
    assert_eq!(
        summary.response_model.as_deref(),
        Some("summary-billed-model")
    );
    assert_eq!(summary.response_id.as_deref(), Some("summary_response"));
    assert_eq!(summary.request_id.as_deref(), Some("summary-request-id"));
    assert_eq!(summary.usage, Some(summary_usage()));
    assert_complete_pairs(&events, 4);
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn forced_call_is_accounted_before_its_foreground_is_handed_off() {
    let mut server = Server::start(reply_with_usage()).await;
    let (provider, events) = accounting(forced_provider(
        &server.base,
        20_000,
        Duration::from_secs(5),
    ));
    let mut app = Application::mount(replacing_large_context, provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.requests.recv().await.unwrap();
    let second = std::future::IntoFuture::into_future(app.react(false).unwrap());
    tokio::pin!(second);
    tokio::select! {
        request = server.compactions.recv() => { request.unwrap(); }
        result = &mut second => panic!("foreground ended before its summary: {result:?}"),
    }
    assert!(server.requests.try_recv().is_err());
    assert_eq!(calls(&events).len(), 1);
    server.release.add_permits(1);
    assert!(tokio::time::timeout(Duration::from_secs(5), &mut second)
        .await
        .unwrap()
        .unwrap()
        .is_continue());
    let all = calls(&events);
    let summary = all
        .iter()
        .find(|call| call.kind == OpenAiResponsesCallKind::ForcedCompaction)
        .unwrap();
    let foreground = all
        .iter()
        .rfind(|call| call.kind == OpenAiResponsesCallKind::Foreground)
        .unwrap();
    assert_eq!(summary.frame_revision, foreground.frame_revision);
    assert_eq!(summary.target, foreground.target);
    assert_eq!(summary.usage, Some(summary_usage()));
    assert_eq!(summary.outcome, Some(OpenAiResponsesCallOutcome::Completed));
    assert_ne!(summary.call_id, foreground.call_id);
    assert_complete_pairs(&events, 3);
    app.shutdown().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn summary_http_failure_preserves_usage_and_shutdown_closes_pending_call_as_unknown() {
    let mut server =
        Server::start_with_status(reply_with_usage(), StatusCode::SERVICE_UNAVAILABLE).await;
    let (provider, events) = accounting(provider(&server.base, ContextRatioPolicy::default()));
    let mut monitor = provider.parallel_compaction_monitor().unwrap();
    let text = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(Arc::clone(&text)), provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.compactions.recv().await.unwrap();
    server.release.add_permits(1);
    until_phase(&mut monitor, ParallelCompactionPhase::Failed).await;
    let summary = calls(&events)
        .into_iter()
        .find(|call| call.kind == OpenAiResponsesCallKind::BackgroundCompaction)
        .unwrap();
    assert_eq!(summary.outcome, Some(OpenAiResponsesCallOutcome::Failed));
    assert_eq!(summary.http_status, Some(503));
    assert_eq!(summary.usage, Some(summary_usage()));
    assert_complete_pairs(&events, 3);
    app.shutdown().await.unwrap();
    server.finish().await;

    let mut server = Server::start(reply_with_usage()).await;
    let (provider, events) =
        accounting(super::provider(&server.base, ContextRatioPolicy::default()));
    let text = Arc::new(AtomicUsize::new(0));
    let mut app = Application::mount(move || counter(Arc::clone(&text)), provider).unwrap();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    server.compactions.recv().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), app.shutdown())
        .await
        .unwrap()
        .unwrap();
    let summary = calls(&events)
        .into_iter()
        .find(|call| call.kind == OpenAiResponsesCallKind::BackgroundCompaction)
        .unwrap();
    assert_eq!(summary.outcome, Some(OpenAiResponsesCallOutcome::Cancelled));
    assert!(
        summary.usage.is_none(),
        "aborted HTTP usage is unknown, never zero"
    );
    assert_complete_pairs(&events, 3);
    server.finish().await;
}
