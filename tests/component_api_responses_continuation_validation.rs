#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises legacy Responses continuation"
)]

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use agentview::{
    component::execution::{
        ProviderFaultCode, ProviderIdentity, ProviderPort, RenderedProjection,
        RenderedProjectionNode,
    },
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
    transcript::CanonicalInputItem,
};
use axum::{http::header, response::IntoResponse, routing::post, Router};
use futures::StreamExt;
use tokio::sync::oneshot;

const BINDING: &str = "component-api-responses-continuation-validation";

async fn spawn_server() -> (
    String,
    Arc<AtomicUsize>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    async fn completed(
        axum::extract::State(attempts): axum::extract::State<Arc<AtomicUsize>>,
    ) -> impl IntoResponse {
        attempts.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":2,\"delta\":\"done\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_text.done\",\"sequence_number\":3,\"text\":\"done\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"sequence_number\":4,\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\",\"annotations\":[]}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"sequence_number\":5,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\",\"annotations\":[]}]}]}}\n\n"
            ),
        )
    }

    let attempts = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/responses", post(completed))
        .with_state(Arc::clone(&attempts));
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
    (format!("http://{address}"), attempts, shutdown_tx, server)
}

fn provider(api_base: String) -> AsyncOpenAiResponsesProvider {
    let config = AsyncOpenAiTransportConfig::new(api_base, "test-token").unwrap();
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING).unwrap();
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap();
    AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options))
}

fn projection(identity: &str, items: Vec<CanonicalInputItem>) -> RenderedProjection {
    RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(identity, items)]).unwrap()
}

async fn establish_retained_history(
    provider: &mut AsyncOpenAiResponsesProvider,
    items: Vec<CanonicalInputItem>,
) {
    let mut stream = provider
        .execute(projection("retained", items))
        .await
        .expect("the initial causal history dispatches");
    while let Some(event) = stream.next().await {
        event.expect("the loopback response is valid");
    }
}

async fn assert_rejected_before_second_dispatch(
    retained: Vec<CanonicalInputItem>,
    current: RenderedProjection,
) {
    let (api_base, attempts, shutdown, server) = spawn_server().await;
    let mut provider = provider(api_base);
    establish_retained_history(&mut provider, retained).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    let fault = match provider.execute(current).await {
        Ok(_) => panic!("combined duplicate call_id must fail before transport"),
        Err(fault) => fault,
    };

    assert_eq!(fault.code(), ProviderFaultCode::RequestPreparation);
    assert!(!fault.to_string().contains("value"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn changed_retained_call_id_is_rejected_before_transport() {
    assert_rejected_before_second_dispatch(
        vec![
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":1}"#).unwrap(),
            CanonicalInputItem::tool_result("call-1", "first").unwrap(),
        ],
        projection(
            "retained",
            vec![
                CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":2}"#).unwrap(),
                CanonicalInputItem::tool_result("call-1", "first").unwrap(),
            ],
        ),
    )
    .await;
}

#[tokio::test]
async fn duplicate_retained_call_id_in_a_new_node_is_rejected_before_transport() {
    assert_rejected_before_second_dispatch(
        vec![
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":1}"#).unwrap(),
            CanonicalInputItem::tool_result("call-1", "first").unwrap(),
        ],
        projection(
            "new-node",
            vec![
                CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":1}"#).unwrap(),
                CanonicalInputItem::tool_result("call-1", "first").unwrap(),
            ],
        ),
    )
    .await;
}

#[tokio::test]
async fn changed_retained_tool_result_is_rejected_before_transport() {
    let retained_call =
        CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":1}"#).unwrap();
    assert_rejected_before_second_dispatch(
        vec![
            retained_call.clone(),
            CanonicalInputItem::tool_result("call-1", "first").unwrap(),
        ],
        projection(
            "retained",
            vec![
                retained_call,
                CanonicalInputItem::tool_result("call-1", "changed-value").unwrap(),
            ],
        ),
    )
    .await;
}
