use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, Frame, ProviderFact, ProviderFactStream, ReactionOutcome, ReactionPort,
            ReactionPortFault, SubmitFault, TargetDeclaration,
        },
        prelude::*,
    },
    provider::{qwen_realtime::QwenRealtimeProvider, ModelSpec},
};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};

const MODEL: &str = "qwen-audio-3.1-realtime-flash";
const DEADLINE: Duration = Duration::from_secs(10);
type Socket = WebSocketStream<TcpStream>;

#[derive(Clone, Default)]
struct InputState(Arc<Mutex<Option<Signal<u64>>>>);

#[component]
fn text_tree(state: InputState, native_calls: Arc<Mutex<usize>>) -> Component {
    let sequence = use_signal(|| 0_u64);
    *state.0.lock().unwrap() = Some(sequence.clone());
    let sequence = sequence.with(|n| *n).unwrap();
    let input = ComponentDefinition::new("test.retained_text_input", "v1", move |_| {
        Ok(Component::text("  同一原文\n<原样> 😀  ").user())
    })
    .unwrap()
    .instantiate(format!("input-{sequence}"));
    view! {
        #[system_once]
        "Use real native tool receipts before answering."
        { Component::text(format!("Request context {sequence}")).developer_ephemeral_repeat() }
        { input }
        NativeToolCall {
            name: "read_value", description: "Return the value for this test.",
            on_call: move |_| {
                *native_calls.lock().unwrap() += 1;
                Ok::<_, Infallible>(json!({"value":323}))
            },
        }
    }
}

struct TurnPort {
    provider: QwenRealtimeProvider,
    native: Arc<AtomicBool>,
}

#[async_trait]
impl ReactionPort for TurnPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.provider.declare()
    }
    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let native = self.native.clone();
        let stream = self.provider.submit(frame).await?;
        Ok(Box::pin(stream.inspect(move |fact| {
            if matches!(fact, Ok(ProviderFact::ToolCall { .. })) {
                native.store(true, Ordering::Release);
            }
        })))
    }
    async fn next_session_frame(&mut self, pending: bool) -> Result<bool, ReactionPortFault> {
        if self.native.swap(false, Ordering::AcqRel) {
            return Ok(true);
        }
        self.provider.next_session_frame(pending).await
    }
    async fn finish_session(&mut self) {
        self.provider.finish_session().await;
    }
    async fn shutdown(&mut self) {
        self.provider.shutdown().await;
    }
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
async fn receive(socket: &mut Socket) -> Value {
    loop {
        match tokio::time::timeout(DEADLINE, socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(raw) => return serde_json::from_str(raw.as_str()).unwrap(),
            Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.unwrap(),
            other => panic!("unexpected provider frame {other:?}"),
        }
    }
}

async fn accept(listener: &TcpListener, session_id: &str) -> Socket {
    let (stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    send(
        &mut socket,
        json!({"type":"session.created","session":{
            "id":session_id,"model":MODEL,"modalities":["text"],"turn_detection":null,
        }}),
    )
    .await;
    socket
}

async fn frame(
    socket: &mut Socket,
    session_id: &str,
    round: usize,
) -> (Value, Vec<Value>, Vec<String>) {
    let update = receive(socket).await;
    assert_eq!(update["type"], "session.update");
    send(
        socket,
        json!({"type":"session.updated","session":{
            "id":session_id,"model":MODEL,"modalities":["text"],"turn_detection":null,
        }}),
    )
    .await;
    let mut items = Vec::new();
    let mut temporary = Vec::new();
    loop {
        let command = receive(socket).await;
        match command["type"].as_str().unwrap() {
            "conversation.item.create" => {
                let mut item = command["item"].clone();
                let id = format!("input-{round}-{}", items.len());
                item["id"] = json!(id);
                if item["role"] == "system" {
                    temporary.push(id);
                }
                send(
                    socket,
                    json!({"type":"conversation.item.created","item":item}),
                )
                .await;
                items.push(command["item"].clone());
            }
            "response.create" => return (update, items, temporary),
            other => panic!("unexpected frame command {other}"),
        }
    }
}

async fn reply(socket: &mut Socket, response_id: &str, text: &str) {
    send(socket, json!({"type":"response.created","response":{"id":response_id,"status":"in_progress","output":[]}})).await;
    send(socket, json!({"type":"response.done","response":{
        "id":response_id,"status":"completed","output":[{"id":format!("answer-{response_id}"),
        "type":"message","status":"completed","role":"assistant","content":[{"type":"text","text":text}]}],
    }})).await;
}

async fn delete_temporary(socket: &mut Socket, ids: &[String]) {
    for id in ids {
        let delete = receive(socket).await;
        assert_eq!(
            delete,
            json!({"type":"conversation.item.delete","item_id":id})
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), socket.next())
                .await
                .is_err()
        );
        send(
            socket,
            json!({"type":"conversation.item.deleted","item_id":id}),
        )
        .await;
    }
}

fn provider(endpoint: &str) -> QwenRealtimeProvider {
    QwenRealtimeProvider::new(
        endpoint,
        "local-dedicated-test-key",
        ModelSpec::new(MODEL, 32768).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn opt_in_text_conversation_keeps_one_handshake_for_equal_inputs_and_native_continuation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let mut socket = accept(&listener, "real-text-session").await;
        for round in 0..3 {
            let (update, items, temporary) = frame(&mut socket, "real-text-session", round).await;
            assert_eq!(temporary.len(), 1);
            assert_eq!(update["session"].get("modalities").is_some(), round == 0);
            assert_eq!(update["session"]["instructions"].is_string(), round == 0);
            let users: Vec<_> = items.iter().filter(|item| item["role"] == "user").collect();
            assert_eq!(users.len(), usize::from(round != 2));
            for user in users {
                assert_eq!(
                    user["content"],
                    json!([{"type":"input_text","text":"  同一原文\n<原样> 😀  "}])
                );
            }
            assert!(items.iter().all(|item| item["role"] != "assistant"));
            if round == 1 {
                let call = json!({"id":"native-item","type":"function_call","status":"completed",
                    "call_id":"native-call","name":"read_value","arguments":"{}"});
                send(&mut socket, json!({"type":"response.created","response":{"id":"native-response","status":"in_progress","output":[]}})).await;
                send(&mut socket, json!({"type":"response.done","response":{"id":"native-response","status":"completed","output":[call]}})).await;
            } else {
                if round == 2 {
                    let receipt = items
                        .iter()
                        .find(|item| item["type"] == "function_call_output")
                        .unwrap();
                    assert_eq!(receipt["call_id"], "native-call");
                    assert_eq!(
                        serde_json::from_str::<Value>(receipt["output"].as_str().unwrap()).unwrap()
                            ["value"],
                        323
                    );
                }
                reply(&mut socket, &format!("reply-{round}"), "323").await;
            }
            delete_temporary(&mut socket, &temporary).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            Message::Close(_)
        ));
        let _ = socket.close(None).await;
    });
    let provider = provider(&endpoint).with_retained_text_conversation();
    let session_id = provider.subscribe_session_id();
    let state = InputState::default();
    let root = state.clone();
    let calls = Arc::new(Mutex::new(0));
    let count = calls.clone();
    let mut app = Application::mount(
        move || text_tree(root.clone(), count.clone()),
        TurnPort {
            provider,
            native: Arc::new(AtomicBool::new(false)),
        },
    )
    .unwrap();
    assert!(app.prepare().await.unwrap().is_continue());
    for round in 0..2 {
        state
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .set(round)
            .unwrap();
        assert_eq!(
            tokio::time::timeout(DEADLINE, app.react(false).unwrap())
                .await
                .unwrap()
                .unwrap(),
            ReactionOutcome::Completed
        );
        assert_eq!(session_id.borrow().as_deref(), Some("real-text-session"));
    }
    assert_eq!(*calls.lock().unwrap(), 1);
    app.shutdown().await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn default_text_requests_with_ephemeral_context_keep_the_existing_full_replay_behavior() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        for round in 0..2 {
            let id = format!("default-session-{round}");
            let mut socket = accept(&listener, &id).await;
            let (update, items, _) = frame(&mut socket, &id, round).await;
            assert_eq!(update["session"]["modalities"], json!(["text"]));
            assert_eq!(
                items.iter().filter(|item| item["role"] == "user").count(),
                round + 1
            );
            assert_eq!(
                items
                    .iter()
                    .filter(|item| item["role"] == "assistant")
                    .count(),
                round
            );
            reply(&mut socket, &format!("default-reply-{round}"), "普通回答").await;
        }
    });
    let provider = provider(&endpoint);
    let session_id = provider.subscribe_session_id();
    let state = InputState::default();
    let root = state.clone();
    let mut app = Application::mount(
        move || text_tree(root.clone(), Arc::new(Mutex::new(0))),
        provider,
    )
    .unwrap();
    assert!(app.prepare().await.unwrap().is_continue());
    for round in 0..2 {
        state
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .set(round)
            .unwrap();
        assert_eq!(
            tokio::time::timeout(DEADLINE, app.react(false).unwrap())
                .await
                .unwrap()
                .unwrap(),
            ReactionOutcome::Completed
        );
        assert_eq!(
            session_id.borrow().as_deref(),
            Some(format!("default-session-{round}").as_str())
        );
    }
    app.shutdown().await.unwrap();
    peer.await.unwrap();
}

#[tokio::test]
async fn cleanup_refusal_or_unconfirmed_delete_keeps_answer_and_replays_on_next_input() {
    for failure in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let mut first = accept(&listener, "before-cleanup-failure").await;
            let (_, _, temporary) = frame(&mut first, "before-cleanup-failure", 0).await;
            reply(&mut first, "first-answer", "已完成第一条").await;
            let delete = receive(&mut first).await;
            assert_eq!(delete["item_id"], temporary[0]);
            if failure == 0 {
                send(&mut first,json!({"type":"error","error":{"type":"invalid_request_error","code":"invalid_state","param":"conversation.item.delete"}})).await;
            } else if failure == 1 {
                send(
                    &mut first,
                    json!({"type":"conversation.item.deleted","item_id":"a-different-item"}),
                )
                .await;
            } else {
                first.close(None).await.unwrap();
            }
            let mut second = accept(&listener, "after-cleanup-failure").await;
            let (update, items, temporary) = frame(&mut second, "after-cleanup-failure", 1).await;
            assert_eq!(update["session"]["modalities"], json!(["text"]));
            assert_eq!(items.iter().filter(|i| i["role"] == "user").count(), 2);
            assert_eq!(items.iter().filter(|i| i["role"] == "assistant").count(), 1);
            assert_eq!(
                items.iter().find(|i| i["role"] == "assistant").unwrap()["content"][0]["text"],
                "已完成第一条"
            );
            reply(&mut second, "second-answer", "第二条也完成").await;
            delete_temporary(&mut second, &temporary).await;
            assert!(matches!(
                second.next().await.unwrap().unwrap(),
                Message::Close(_)
            ));
            let _ = second.close(None).await;
        });
        let provider = provider(&endpoint).with_retained_text_conversation();
        let session_id = provider.subscribe_session_id();
        let state = InputState::default();
        let root = state.clone();
        let mut app = Application::mount(
            move || text_tree(root.clone(), Arc::new(Mutex::new(0))),
            provider,
        )
        .unwrap();
        assert!(app.prepare().await.unwrap().is_continue());
        for (round, id) in ["before-cleanup-failure", "after-cleanup-failure"]
            .into_iter()
            .enumerate()
        {
            state
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .set(round as u64)
                .unwrap();
            assert_eq!(
                tokio::time::timeout(DEADLINE, app.react(false).unwrap())
                    .await
                    .unwrap()
                    .unwrap(),
                ReactionOutcome::Completed
            );
            assert_eq!(session_id.borrow().as_deref(), Some(id));
        }
        app.shutdown().await.unwrap();
        peer.await.unwrap();
    }
}
