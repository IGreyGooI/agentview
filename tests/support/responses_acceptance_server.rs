use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
struct ServerState {
    attempts: Arc<AtomicUsize>,
    bodies: mpsc::UnboundedSender<Vec<u8>>,
    replies: &'static [&'static str],
    failure: Option<StatusCode>,
}

pub struct ResponsesAcceptanceServer {
    api_base: String,
    attempts: Arc<AtomicUsize>,
    bodies: mpsc::UnboundedReceiver<Vec<u8>>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl ResponsesAcceptanceServer {
    pub async fn start(replies: &'static [&'static str]) -> anyhow::Result<Self> {
        anyhow::ensure!(!replies.is_empty(), "mock Responses server needs a reply");
        Self::start_with_mode(replies, None).await
    }

    pub async fn start_failure(status: StatusCode) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !status.is_success(),
            "failure fixture needs a non-success status"
        );
        Self::start_with_mode(&["unused"], Some(status)).await
    }

    async fn start_with_mode(
        replies: &'static [&'static str],
        failure: Option<StatusCode>,
    ) -> anyhow::Result<Self> {
        let (body_tx, body_rx) = mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/responses", post(responses_endpoint))
            .with_state(ServerState {
                attempts: attempts.clone(),
                bodies: body_tx,
                replies,
                failure,
            });
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("visual acceptance server");
        });
        Ok(Self {
            api_base: format!("http://{address}"),
            attempts,
            bodies: body_rx,
            shutdown: shutdown_tx,
            server,
        })
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub fn request_count(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    pub async fn next_request(&mut self) -> anyhow::Result<Vec<u8>> {
        self.bodies
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("mock provider did not receive request"))
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(());
        self.server.await?;
        Ok(())
    }
}

async fn responses_endpoint(State(state): State<ServerState>, body: Bytes) -> Response {
    let attempt = state.attempts.fetch_add(1, Ordering::SeqCst);
    if state.bodies.send(body.to_vec()).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some(status) = state.failure {
        return status.into_response();
    }
    let next = state
        .replies
        .get(attempt)
        .copied()
        .unwrap_or(state.replies[state.replies.len() - 1]);
    completed_response(attempt + 1, next)
}

fn completed_response(sequence: usize, text: &str) -> Response {
    let message_id = format!("msg_{sequence}");
    let response_id = format!("resp_{sequence}");
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {
            "id": message_id,
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": []
        }
    });
    let done = json!({
        "id": message_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    });
    let events = [
        added,
        json!({
            "type": "response.output_text.delta",
            "sequence_number": 2,
            "delta": text,
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0
        }),
        json!({
            "type": "response.output_text.done",
            "sequence_number": 3,
            "text": text,
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 4,
            "output_index": 0,
            "item": done.clone()
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 5,
            "response": {"id": response_id, "status": "completed", "output": [done]}
        }),
    ];
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}
