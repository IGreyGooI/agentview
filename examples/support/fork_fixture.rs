//! Deterministic local Responses fixture for the Application fork example.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::{
    body::Body,
    extract::State,
    http::{header, Response},
    Json,
};
use serde_json::{json, Value};

pub const COMMON_HISTORY: &str = "I will compare both plans before choosing the cheaper one.";

#[derive(Default)]
pub struct Fixture {
    pub requests: Mutex<Vec<(bool, Value)>>,
    parent_calls: AtomicUsize,
    worker_calls: AtomicUsize,
}

pub async fn respond(
    State(fixture): State<Arc<Fixture>>,
    Json(request): Json<Value>,
) -> Response<Body> {
    // Route using the native tool catalog, never the rendered Component view.
    let worker = request["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "compare_plans"));
    fixture.requests.lock().unwrap().push((worker, request));
    let step = if worker {
        fixture.worker_calls.fetch_add(1, Ordering::Relaxed)
    } else {
        fixture.parent_calls.fetch_add(1, Ordering::Relaxed)
    };
    let (tool, text) = match (worker, step) {
        (false, 0) => (None, COMMON_HISTORY),
        (false, 1) => (
            None,
            "The comparison is running; I will wait for its result.",
        ),
        (false, 2) => (Some(("accept_plan", json!({"expected_cost": 42}))), ""),
        (true, 0) => (Some(("compare_plans", json!({}))), ""),
        (true, _) => (None, "The completed comparison has been delivered."),
        _ => (None, "The cheaper plan was accepted."),
    };
    let mut events = Vec::new();
    let done = if let Some((name, arguments)) = tool {
        let arguments = arguments.to_string();
        let call_id = format!("{name}-{step}");
        events.extend([
            json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"function_call", "id":"call", "status":"in_progress", "call_id":call_id, "name":name, "arguments":""}}),
            json!({"type":"response.function_call_arguments.delta", "output_index":0, "item_id":"call", "delta":arguments}),
            json!({"type":"response.function_call_arguments.done", "output_index":0, "item_id":"call", "arguments":arguments}),
        ]);
        json!({"type":"function_call", "id":"call", "status":"completed", "call_id":call_id, "name":name, "arguments":arguments})
    } else {
        events.extend([
            json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"message", "id":"message", "status":"in_progress", "role":"assistant", "content":[]}}),
            json!({"type":"response.output_text.delta", "output_index":0, "item_id":"message", "content_index":0, "delta":text}),
            json!({"type":"response.output_text.done", "output_index":0, "item_id":"message", "content_index":0, "text":text}),
        ]);
        json!({"type":"message", "id":"message", "status":"completed", "role":"assistant",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    };
    events.push(json!({"type":"response.output_item.done", "output_index":0, "item":done}));
    events.push(json!({"type":"response.completed", "response":{"id":"response", "status":"completed", "output":[done]}}));
    let body: String = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}
