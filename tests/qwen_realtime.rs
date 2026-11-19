#[path = "support/pom.rs"]
mod pom_support;

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        execution::{Application, FrameBasis, ReactionOutcome},
        prelude::*,
    },
    llm_call::TextTurnEvent,
    pom::ResolvedDocument,
    provider::{
        qwen_realtime::{QwenRealtimeAudioEvent, QwenRealtimeProvider, QwenRealtimeResponseStatus},
        ModelSpec,
    },
    transcript::{AssistantTextStatus, CanonicalInputItem},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::{SinkExt, StreamExt};
use pom_support::{projection_document, InspectPom};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
    WebSocketStream,
};

const MODEL: &str = "qwen-audio-3.1-realtime-plus";
const TEST_API_KEY: &str = "local-test-credential";
const TOOL_RESULT: &str = "杭州 25°C sunny";
const FINAL_TEXT: &str = "杭州今天晴，25°C。";
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct InteractionProbe {
    calls: Arc<Mutex<Vec<String>>>,
    text_events: Arc<Mutex<Vec<TextTurnEvent>>>,
    text_received: Arc<Notify>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct LookupInput {
    /// City whose current weather should be retrieved.
    city: String,
}

#[component]
fn weather_application(probe: InteractionProbe) -> Component {
    let status = use_signal(|| "waiting for a weather lookup".to_owned());
    let current = status.with(Clone::clone).unwrap();
    let text_events = probe.text_events.clone();
    let text_received = probe.text_received.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |_context, event| {
        let text_events = text_events.clone();
        let text_received = text_received.clone();
        async move {
            text_events.lock().unwrap().push(event);
            text_received.notify_one();
            Ok::<_, Infallible>(())
        }
    });
    view! {
        #[system_once]
        policy { "Use the available weather tool before answering a weather question." }

        #[developer]
        instruction { "Read the current lookup result before choosing the next action." }

        #[assistant]
        "I can look up the current weather."

        #[user]
        weather {
            city { "杭州" }
            status { "{current}" }
        }

        NativeToolCall {
            name: "lookup_weather",
            description: "Retrieve the current weather for the supplied city.",
            on_call: move |_context, input: LookupInput| {
                probe.calls.lock().unwrap().push(input.city.clone());
                let result = format!("{} 25°C sunny", input.city);
                status.set(result.clone()).unwrap();
                Ok::<_, Infallible>(json!({"weather": result}))
            },
        }
    }
}

#[derive(Clone, Default)]
struct XmlNoteProbe {
    notes: Arc<Mutex<Vec<String>>>,
    text_events: Arc<Mutex<Vec<TextTurnEvent>>>,
}

#[component]
fn voice_notebook(probe: XmlNoteProbe) -> Component {
    let note = use_signal(String::new);
    let current = note.with(Clone::clone).unwrap();
    let text_events = probe.text_events;
    use_provider_event_handler(ProviderEvent::TEXT, move |_context, event| {
        let text_events = text_events.clone();
        async move {
            text_events.lock().unwrap().push(event);
            Ok::<_, Infallible>(())
        }
    });
    let actions = if current.is_empty() {
        view! {
            XmlStreamingToolCall {
                element: XmlToolElement::text("save_note"),
                description: "Save the note text, then read the updated notebook before replying.",
                on_complete: move |_context, text: String| {
                    probe.notes.lock().unwrap().push(text.clone());
                    note.set(text)
                },
            }
        }
    } else {
        view! {}
    };
    view! {
        #[system_once]
        policy { "Save requested notes through the declared action and acknowledge the resulting notebook state." }

        notebook {
            saved_note { "{current}" }
        }
        { actions }
    }
}

#[component]
fn ephemeral_voice_context() -> Component {
    view! {
        #[user]
        request { "Continue the voice conversation." }

        #[developer(ephemeral, repeat)]
        temporary_policy { "Give a short spoken answer." }

        #[user(ephemeral, repeat)]
        temporary_context { "The caller is planning a trip." }
    }
}

struct ObservedFrame {
    reaction_id: ReactionId,
    basis: FrameBasis,
    document: ResolvedDocument,
    replay: Vec<CanonicalInputItem>,
    staged_inputs: Vec<CanonicalInputItem>,
}

struct EphemeralVoiceFrame {
    reaction_id: ReactionId,
    basis: FrameBasis,
    ephemeral: Vec<CanonicalInputItem>,
    replay: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
}

struct Exchange {
    session_update: Value,
    items: Vec<Value>,
}

struct AudioExchange {
    updates: Vec<Value>,
    items: Vec<Value>,
    events: Vec<Value>,
    pcm16: Vec<u8>,
    committed: bool,
}

async fn send_event(socket: &mut WebSocketStream<TcpStream>, event: Value) {
    socket
        .send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
}

async fn next_event(socket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let message = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("client should send the next Realtime event")
            .expect("client connection should remain open")
            .unwrap();
        match message {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.unwrap(),
            other => panic!("expected a JSON Realtime event, received {other:?}"),
        }
    }
}

async fn open_session(listener: &TcpListener, session_id: &str) -> WebSocketStream<TcpStream> {
    let (stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
        .await
        .expect("provider should open the Realtime connection")
        .unwrap();
    let mut socket = accept_hdr_async(stream, |request: &Request, response: Response| {
        assert_eq!(request.uri().path(), "/api-ws/v1/realtime");
        assert_eq!(
            request.uri().query(),
            Some(format!("model={MODEL}").as_str())
        );
        assert_eq!(
            request.headers()["authorization"],
            format!("Bearer {TEST_API_KEY}")
        );
        Ok(response)
    })
    .await
    .unwrap();
    send_event(
        &mut socket,
        json!({
            "type": "session.created",
            "session": {
                "id": session_id,
                "object": "realtime.session",
                "model": MODEL,
                "modalities": ["text", "audio"]
            }
        }),
    )
    .await;
    socket
}

async fn receive_frame(
    socket: &mut WebSocketStream<TcpStream>,
    session_id: &str,
    round: usize,
) -> Exchange {
    let session_update = next_event(socket).await;
    assert_eq!(session_update["type"], "session.update");
    send_event(
        socket,
        json!({
            "type": "session.updated",
            "session": {
                "id": session_id,
                "object": "realtime.session",
                "model": MODEL,
                "modalities": ["text"],
                "turn_detection": null
            }
        }),
    )
    .await;
    let mut items = Vec::new();
    loop {
        let event = next_event(socket).await;
        if event["type"] == "response.create" {
            break;
        }
        assert_eq!(event["type"], "conversation.item.create");
        let mut item = event["item"].clone();
        if item.get("id").is_none() {
            item["id"] = json!(format!("item_input_{round}_{}", items.len()));
        }
        send_event(
            socket,
            json!({"type": "conversation.item.created", "item": item}),
        )
        .await;
        items.push(event["item"].clone());
    }
    Exchange {
        session_update,
        items,
    }
}

async fn receive_audio_exchange(
    socket: &mut WebSocketStream<TcpStream>,
    input_item_id: &str,
) -> AudioExchange {
    let mut exchange = AudioExchange {
        updates: Vec::new(),
        items: Vec::new(),
        events: Vec::new(),
        pcm16: Vec::new(),
        committed: false,
    };
    loop {
        let event = next_event(socket).await;
        exchange.events.push(event.clone());
        match event["type"].as_str().unwrap() {
            "session.update" => {
                send_event(
                    socket,
                    json!({
                        "type": "session.updated",
                        "session": {
                            "id": "sess_voice_application", "model": MODEL,
                            "modalities": ["text", "audio"],
                            "input_audio_format": "pcm", "output_audio_format": "pcm",
                            "turn_detection": null
                        }
                    }),
                )
                .await;
                exchange.updates.push(event["session"].clone());
            }
            "conversation.item.create" => {
                let mut item = event["item"].clone();
                if item.get("id").is_none() {
                    item["id"] = json!(format!("item_{input_item_id}_{}", exchange.items.len()));
                }
                send_event(
                    socket,
                    json!({"type": "conversation.item.created", "item": item}),
                )
                .await;
                exchange.items.push(event["item"].clone());
            }
            "input_audio_buffer.append" => {
                exchange
                    .pcm16
                    .extend(STANDARD.decode(event["audio"].as_str().unwrap()).unwrap());
            }
            "input_audio_buffer.commit" => {
                assert!(!exchange.committed);
                exchange.committed = true;
                send_event(
                    socket,
                    json!({"type": "input_audio_buffer.committed", "item_id": input_item_id}),
                )
                .await;
                send_event(
                    socket,
                    json!({
                        "type": "conversation.item.created",
                        "item": {
                            "id": input_item_id, "type": "message", "role": "user",
                            "status": "completed", "content": [{"type": "input_audio"}]
                        }
                    }),
                )
                .await;
            }
            "response.create" => return exchange,
            other => panic!("unexpected audio application event {other}"),
        }
    }
}

async fn spawn_server(
    replies: Vec<Vec<Value>>,
) -> (String, tokio::task::JoinHandle<Vec<Exchange>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api-ws/v1/realtime?model={MODEL}",
        listener.local_addr().unwrap()
    );
    let task = tokio::spawn(async move {
        let session_id = "sess_local";
        let mut socket = open_session(&listener, session_id).await;
        let mut exchanges = Vec::new();
        for (round, reply) in replies.into_iter().enumerate() {
            exchanges.push(receive_frame(&mut socket, session_id, round).await);
            for event in reply {
                send_event(&mut socket, event).await;
            }
        }
        let _ = socket.close(None).await;
        exchanges
    });
    (endpoint, task)
}

fn tool_reply() -> Vec<Value> {
    let item = json!({
        "id": "item_lookup",
        "object": "realtime.item",
        "type": "function_call",
        "status": "completed",
        "call_id": "call_lookup",
        "name": "lookup_weather",
        "arguments": r#"{"city":"杭州"}"#
    });
    let mut added = item.clone();
    added["status"] = json!("in_progress");
    added["arguments"] = json!("");
    vec![
        json!({
            "type": "response.created",
            "response": {"id": "resp_lookup", "status": "in_progress", "output": []}
        }),
        json!({
            "type": "response.output_item.added",
            "response_id": "resp_lookup", "output_index": 0, "item": added
        }),
        json!({
            "type": "conversation.item.created", "item": added
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "response_id": "resp_lookup", "item_id": "item_lookup", "output_index": 0,
            "call_id": "call_lookup", "delta": "{\"city\":\"杭"
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "response_id": "resp_lookup", "item_id": "item_lookup", "output_index": 0,
            "call_id": "call_lookup", "delta": "州\"}"
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "response_id": "resp_lookup", "item_id": "item_lookup", "output_index": 0,
            "call_id": "call_lookup", "name": "lookup_weather", "arguments": item["arguments"]
        }),
        json!({
            "type": "response.output_item.done",
            "response_id": "resp_lookup", "output_index": 0, "item": item
        }),
        json!({
            "type": "response.done",
            "response": {
                "id": "resp_lookup", "object": "realtime.response", "status": "completed",
                "modalities": ["text"], "output": [item]
            }
        }),
    ]
}

fn text_reply() -> Vec<Value> {
    let item = json!({
        "id": "item_answer", "object": "realtime.item", "type": "message",
        "status": "completed", "role": "assistant",
        "content": [{"type": "text", "text": FINAL_TEXT}]
    });
    vec![
        json!({
            "type": "response.created",
            "response": {"id": "resp_answer", "status": "in_progress", "output": []}
        }),
        json!({
            "type": "response.output_item.added", "response_id": "resp_answer", "output_index": 0,
            "item": {
                "id": "item_answer", "object": "realtime.item", "type": "message",
                "status": "in_progress", "role": "assistant", "content": []
            }
        }),
        json!({
            "type": "response.content_part.added", "response_id": "resp_answer",
            "item_id": "item_answer", "output_index": 0, "content_index": 0,
            "part": {"type": "text", "text": ""}
        }),
        json!({
            "type": "response.text.delta", "response_id": "resp_answer",
            "item_id": "item_answer", "output_index": 0, "content_index": 0,
            "delta": "杭州今天晴，"
        }),
        json!({
            "type": "response.text.delta", "response_id": "resp_answer",
            "item_id": "item_answer", "output_index": 0, "content_index": 0,
            "delta": "25°C。"
        }),
        json!({
            "type": "response.text.done", "response_id": "resp_answer",
            "item_id": "item_answer", "output_index": 0, "content_index": 0,
            "text": FINAL_TEXT
        }),
        json!({
            "type": "response.content_part.done", "response_id": "resp_answer",
            "item_id": "item_answer", "output_index": 0, "content_index": 0,
            "part": {"type": "text", "text": FINAL_TEXT}
        }),
        json!({
            "type": "response.output_item.done", "response_id": "resp_answer",
            "output_index": 0, "item": item
        }),
        json!({
            "type": "response.done",
            "response": {
                "id": "resp_answer", "object": "realtime.response", "status": "completed",
                "modalities": ["text"], "output": [item]
            }
        }),
    ]
}

fn audio_reply(response_id: &str, item_id: &str, text: &str, pcm16: &[u8]) -> Vec<Value> {
    let item = json!({
        "id": item_id, "object": "realtime.item", "type": "message",
        "status": "completed", "role": "assistant",
        "content": [{"type": "audio", "transcript": text}]
    });
    vec![
        json!({
            "type": "response.created",
            "response": {"id": response_id, "status": "in_progress", "output": []}
        }),
        json!({
            "type": "response.output_item.added", "response_id": response_id, "output_index": 0,
            "item": {
                "id": item_id, "object": "realtime.item", "type": "message",
                "status": "in_progress", "role": "assistant", "content": []
            }
        }),
        json!({
            "type": "response.content_part.added", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "part": {"type": "audio", "text": ""}
        }),
        json!({
            "type": "response.audio_transcript.delta", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "delta": text
        }),
        json!({
            "type": "response.audio.delta", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "delta": STANDARD.encode(pcm16)
        }),
        json!({
            "type": "response.audio_transcript.done", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "transcript": text
        }),
        json!({
            "type": "response.audio.done", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0
        }),
        json!({
            "type": "response.content_part.done", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "part": {"type": "audio", "text": text}
        }),
        json!({
            "type": "response.output_item.done", "response_id": response_id,
            "output_index": 0, "item": item
        }),
        json!({
            "type": "response.done",
            "response": {
                "id": response_id, "object": "realtime.response", "status": "completed",
                "modalities": ["text", "audio"], "output": [item]
            }
        }),
    ]
}

#[tokio::test]
async fn native_weather_lookup_delivers_its_result_and_updated_view_before_the_next_response() {
    let (endpoint, server) = spawn_server(vec![tool_reply(), text_reply()]).await;
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        TEST_API_KEY,
        ModelSpec::new(MODEL, 32_768).unwrap(),
    )
    .unwrap();
    let probe = InteractionProbe::default();
    let root_probe = probe.clone();
    let observed = Arc::new(Mutex::new(Vec::<ObservedFrame>::new()));
    let observed_frames = observed.clone();
    let mut app = Application::mount(move || weather_application(root_probe.clone()), provider)
        .unwrap()
        .with_frame_observer(move |event| {
            observed_frames.lock().unwrap().push(ObservedFrame {
                reaction_id: event.reaction_id(),
                basis: event.frame().basis(),
                document: projection_document(event.projection()),
                replay: event.frame().submission().replay().to_vec(),
                staged_inputs: event.frame().submission().staged_inputs().to_vec(),
            });
        });

    let first = tokio::time::timeout(DEADLINE, app.react(false).unwrap())
        .await
        .expect("tool reaction should settle")
        .unwrap();
    assert_eq!(first, ReactionOutcome::Completed);
    assert_eq!(*probe.calls.lock().unwrap(), ["杭州"]);

    let second = tokio::time::timeout(DEADLINE, app.react(false).unwrap())
        .await
        .expect("answer reaction should settle")
        .unwrap();
    assert_eq!(second, ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    let exchanges = tokio::time::timeout(DEADLINE, server)
        .await
        .expect("test server should finish")
        .unwrap();

    let frames = observed.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert!(frames[0]
        .document
        .has_field("status", "waiting for a weather lookup"));
    assert!(frames[1].document.has_field("status", TOOL_RESULT));
    assert_eq!(
        frames[1].staged_inputs,
        [CanonicalInputItem::tool_result(
            "call_lookup",
            json!({"weather": TOOL_RESULT}).to_string()
        )
        .unwrap()]
    );

    assert_eq!(exchanges.len(), 2);
    let initial_session = &exchanges[0].session_update["session"];
    assert_eq!(initial_session["modalities"], json!(["text"]));
    assert!(initial_session["turn_detection"].is_null());
    let continued_session = exchanges[1].session_update["session"].as_object().unwrap();
    assert!(!continued_session.contains_key("modalities"));
    assert!(!continued_session.contains_key("turn_detection"));
    for exchange in &exchanges {
        let session = &exchange.session_update["session"];
        let tools = session["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "lookup_weather");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["city"]["type"],
            "string"
        );
    }
    assert!(exchanges[0]
        .items
        .iter()
        .any(|item| item["role"] == "system"));
    assert!(exchanges[0].items.iter().any(|item| {
        item["role"] == "assistant" && item["content"][0]["type"] == "output_text"
    }));
    assert!(!exchanges[0]
        .items
        .iter()
        .any(|item| item["type"] == "function_call_output"));
    assert!(!exchanges[1]
        .items
        .iter()
        .any(|item| item["type"] == "function_call"));
    let outputs: Vec<_> = exchanges[1]
        .items
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["call_id"], "call_lookup");
    assert_eq!(
        serde_json::from_str::<Value>(outputs[0]["output"].as_str().unwrap()).unwrap(),
        json!({"weather": TOOL_RESULT})
    );
    assert_eq!(
        probe
            .text_events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                TextTurnEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        FINAL_TEXT
    );
    assert_eq!(
        probe
            .text_events
            .lock()
            .unwrap()
            .iter()
            .filter(
                |event| matches!(event, TextTurnEvent::TextComplete(text) if text == FINAL_TEXT)
            )
            .count(),
        1
    );
    assert_eq!(*probe.calls.lock().unwrap(), ["杭州"]);
}

#[tokio::test]
async fn interrupted_response_reconnects_and_replays_the_partial_text_as_interrupted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api-ws/v1/realtime?model={MODEL}",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut first_socket = open_session(&listener, "sess_interrupted").await;
        let first = receive_frame(&mut first_socket, "sess_interrupted", 0).await;
        // Stop after one text delta, leaving both the item and response open.
        for event in text_reply().into_iter().take(4) {
            send_event(&mut first_socket, event).await;
        }
        let closed = tokio::time::timeout(DEADLINE, first_socket.next())
            .await
            .expect("interrupt should close the old session");
        assert!(matches!(
            closed,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));

        let mut next_socket = open_session(&listener, "sess_recovered").await;
        let second = receive_frame(&mut next_socket, "sess_recovered", 1).await;
        for event in text_reply() {
            send_event(&mut next_socket, event).await;
        }
        let _ = next_socket.close(None).await;
        vec![first, second]
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        TEST_API_KEY,
        ModelSpec::new(MODEL, 32_768).unwrap(),
    )
    .unwrap();
    let probe = InteractionProbe::default();
    let root_probe = probe.clone();
    let observed = Arc::new(Mutex::new(Vec::<ObservedFrame>::new()));
    let observed_frames = observed.clone();
    let mut app = Application::mount(move || weather_application(root_probe.clone()), provider)
        .unwrap()
        .with_frame_observer(move |event| {
            observed_frames.lock().unwrap().push(ObservedFrame {
                reaction_id: event.reaction_id(),
                basis: event.frame().basis(),
                document: projection_document(event.projection()),
                replay: event.frame().submission().replay().to_vec(),
                staged_inputs: event.frame().submission().staged_inputs().to_vec(),
            });
        });
    let reaction = app.react(false).unwrap();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let notified = probe.text_received.notified();
            if probe
                .text_events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, TextTurnEvent::TextDelta(_)))
            {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("the model should observe the partial output before interruption");
    reaction.interrupt().unwrap();
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction)
            .await
            .expect("interruption should settle")
            .unwrap(),
        ReactionOutcome::Interrupted
    );
    assert_eq!(
        tokio::time::timeout(DEADLINE, app.react(false).unwrap())
            .await
            .expect("the explicit continuation should reconnect")
            .unwrap(),
        ReactionOutcome::Completed
    );
    app.shutdown().await.unwrap();
    let exchanges = tokio::time::timeout(DEADLINE, server)
        .await
        .expect("the reconnect server should finish")
        .unwrap();

    let frames = observed.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].basis, FrameBasis::Full);
    assert!(frames[1].replay.iter().any(|item| matches!(
        item,
        CanonicalInputItem::AssistantText {
            text,
            status: AssistantTextStatus::Interrupted,
            ..
        } if text == "杭州今天晴，"
    )));
    let replay_index = exchanges[1]
        .items
        .iter()
        .position(|item| {
            item["role"] == "assistant"
                && item["content"][0]["type"] == "output_text"
                && item["content"][0]["text"] == "杭州今天晴，"
        })
        .expect("the fresh session should receive the actual partial model output");
    let interruption = &exchanges[1].items[replay_index + 1];
    assert_eq!(interruption["role"], "user");
    assert_eq!(interruption["content"][0]["type"], "input_text");
    assert!(probe.calls.lock().unwrap().is_empty());
    assert_eq!(
        probe
            .text_events
            .lock()
            .unwrap()
            .iter()
            .filter(
                |event| matches!(event, TextTurnEvent::TextComplete(text) if text == FINAL_TEXT)
            )
            .count(),
        1
    );
}

#[tokio::test]
async fn one_reaction_runs_two_voice_exchanges_and_delivers_tool_feedback_on_the_same_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api-ws/v1/realtime?model={MODEL}",
        listener.local_addr().unwrap()
    );
    let (release_asr, wait_for_asr) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = open_session(&listener, "sess_voice_application").await;
        let first = receive_audio_exchange(&mut socket, "input_voice_1").await;
        for mut event in tool_reply() {
            if event["type"] == "response.done" {
                event["response"]["modalities"] = json!(["text", "audio"]);
            }
            send_event(&mut socket, event).await;
        }
        send_event(
            &mut socket,
            json!({
                "type": "conversation.item.input_audio_transcription.completed",
                "item_id": "input_voice_1", "content_index": 0,
                "transcript": "杭州天气怎么样？"
            }),
        )
        .await;
        let feedback = receive_audio_exchange(&mut socket, "input_voice_1").await;
        for event in audio_reply("resp_voice_1", "item_voice_1", FINAL_TEXT, &[1, 0, 2, 0]) {
            send_event(&mut socket, event).await;
        }

        let second = receive_audio_exchange(&mut socket, "input_voice_2").await;
        for event in audio_reply(
            "resp_voice_2",
            "item_voice_2",
            "祝你出行愉快。",
            &[3, 0, 4, 0],
        ) {
            send_event(&mut socket, event).await;
        }
        wait_for_asr.await.unwrap();
        send_event(
            &mut socket,
            json!({
                "type": "conversation.item.input_audio_transcription.completed",
                "item_id": "input_voice_2", "content_index": 0,
                "transcript": "谢谢。"
            }),
        )
        .await;
        let closed = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("finished media input should close the session after final ASR");
        assert!(matches!(
            closed,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));
        [first, feedback, second]
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        TEST_API_KEY,
        ModelSpec::new(MODEL, 32_768).unwrap(),
    )
    .unwrap();
    let (provider, input, mut events) = provider.with_audio();
    let probe = InteractionProbe::default();
    let root_probe = probe.clone();
    let observed = Arc::new(Mutex::new(Vec::<ObservedFrame>::new()));
    let observed_frames = observed.clone();
    let mut app = Application::mount(move || weather_application(root_probe.clone()), provider)
        .unwrap()
        .with_frame_observer(move |event| {
            observed_frames.lock().unwrap().push(ObservedFrame {
                reaction_id: event.reaction_id(),
                basis: event.frame().basis(),
                document: projection_document(event.projection()),
                replay: event.frame().submission().replay().to_vec(),
                staged_inputs: event.frame().submission().staged_inputs().to_vec(),
            });
        });
    let reaction = app.react(false).unwrap();
    let reaction_id = reaction.id();
    input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
    input.commit_input().await.unwrap();

    let mut media_events = Vec::new();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_event())
            .await
            .expect("first voice exchange should advance")
            .unwrap();
        let finished = matches!(
            &event,
            QwenRealtimeAudioEvent::ResponseEnded { response_id, status, .. }
                if response_id == "resp_voice_1" && *status == QwenRealtimeResponseStatus::Completed
        );
        media_events.push(event);
        if finished {
            break;
        }
    }
    assert_eq!(*probe.calls.lock().unwrap(), ["杭州"]);
    input.append_pcm16(&[21, 0, 22, 0]).await.unwrap();
    input.commit_input().await.unwrap();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_event())
            .await
            .expect("second voice exchange should advance")
            .unwrap();
        let finished = matches!(
            &event,
            QwenRealtimeAudioEvent::ResponseEnded { response_id, status, .. }
                if response_id == "resp_voice_2" && *status == QwenRealtimeResponseStatus::Completed
        );
        media_events.push(event);
        if finished {
            break;
        }
    }
    input.finish().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), reaction.clone())
            .await
            .is_err()
    );
    release_asr.send(()).unwrap();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_event())
            .await
            .expect("final input transcript should survive response completion")
            .unwrap();
        let finished = matches!(
            &event,
            QwenRealtimeAudioEvent::InputTranscriptDone { item_id, .. }
                if item_id == "input_voice_2"
        );
        media_events.push(event);
        if finished {
            break;
        }
    }
    let mut server_events = Vec::new();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_server_event())
            .await
            .expect("raw server events should remain available alongside typed media events")
            .unwrap();
        let finished = event["type"] == "conversation.item.input_audio_transcription.completed"
            && event["item_id"] == "input_voice_2";
        server_events.push(event);
        if finished {
            break;
        }
    }
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction)
            .await
            .expect("finished media session should settle its owning reaction")
            .unwrap(),
        ReactionOutcome::Completed
    );
    app.shutdown().await.unwrap();
    let [first, feedback, second] = tokio::time::timeout(DEADLINE, server)
        .await
        .expect("voice server should finish")
        .unwrap();

    assert_eq!(first.pcm16, [11, 0, 12, 0]);
    assert!(first.committed);
    assert!(feedback.pcm16.is_empty());
    assert!(!feedback.committed);
    assert_eq!(second.pcm16, [21, 0, 22, 0]);
    assert!(second.committed);
    let initial_config = &first.updates[0];
    assert_eq!(initial_config["modalities"], json!(["text", "audio"]));
    assert!(initial_config["turn_detection"].is_null());
    let outputs: Vec<_> = feedback
        .items
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["call_id"], "call_lookup");
    assert_eq!(
        serde_json::from_str::<Value>(outputs[0]["output"].as_str().unwrap()).unwrap(),
        json!({"weather": TOOL_RESULT})
    );
    let frames = observed.lock().unwrap();
    assert_eq!(frames.len(), 3);
    assert!(frames.iter().all(|frame| frame.reaction_id == reaction_id));
    assert!(frames[0]
        .document
        .has_field("status", "waiting for a weather lookup"));
    assert!(frames.iter().skip(1).any(|frame| {
        frame.document.has_field("status", TOOL_RESULT)
            && frame.staged_inputs.iter().any(|item| {
                matches!(item, CanonicalInputItem::ToolResult { call_id, .. } if call_id == "call_lookup")
            })
    }));
    let audio: Vec<_> = media_events
        .iter()
        .filter_map(|event| match event {
            QwenRealtimeAudioEvent::AudioDelta {
                pcm16,
                sample_rate_hz,
                ..
            } => {
                assert_eq!(*sample_rate_hz, 24_000);
                Some(pcm16.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(audio, [vec![1, 0, 2, 0], vec![3, 0, 4, 0]]);
    let input_transcripts: Vec<_> = media_events
        .iter()
        .filter_map(|event| match event {
            QwenRealtimeAudioEvent::InputTranscriptDone { transcript, .. } => {
                Some(transcript.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(input_transcripts, ["杭州天气怎么样？", "谢谢。"]);
    assert!(server_events
        .iter()
        .any(|event| event["type"] == "response.function_call_arguments.done"));
    for (response_id, input_item_id) in [
        ("resp_lookup", "input_voice_1"),
        ("resp_voice_2", "input_voice_2"),
    ] {
        let response_done = server_events
            .iter()
            .position(|event| {
                event["type"] == "response.done" && event["response"]["id"] == response_id
            })
            .unwrap();
        let transcript_done = server_events
            .iter()
            .position(|event| {
                event["type"] == "conversation.item.input_audio_transcription.completed"
                    && event["item_id"] == input_item_id
            })
            .unwrap();
        assert!(response_done < transcript_done);
    }
    let completed: Vec<_> = probe
        .text_events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            TextTurnEvent::TextComplete(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completed, [FINAL_TEXT, "祝你出行愉快。"]);
}

enum IdleSessionStop {
    Interrupt,
    Exit,
}

async fn stop_idle_voice_session(stop: IdleSessionStop) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api-ws/v1/realtime?model={MODEL}",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut socket = open_session(&listener, "sess_voice_application").await;
        let exchange = receive_audio_exchange(&mut socket, "input_idle").await;
        for event in audio_reply("resp_idle", "item_idle", FINAL_TEXT, &[1, 0]) {
            send_event(&mut socket, event).await;
        }
        send_event(
            &mut socket,
            json!({
                "type": "conversation.item.input_audio_transcription.completed",
                "item_id": "input_idle", "content_index": 0, "transcript": "你好。"
            }),
        )
        .await;
        let closed = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("shutdown should close a session waiting for the next utterance");
        assert!(matches!(
            closed,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));
        exchange
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        TEST_API_KEY,
        ModelSpec::new(MODEL, 32_768).unwrap(),
    )
    .unwrap();
    let (provider, input, mut events) = provider.with_audio();
    let probe = InteractionProbe::default();
    let root_probe = probe.clone();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let submitted = frames.clone();
    let mut app = Application::mount(move || weather_application(root_probe.clone()), provider)
        .unwrap()
        .with_frame_observer(move |event| submitted.lock().unwrap().push(event.frame().revision()));
    let reaction = app.react(false).unwrap();
    input.append_pcm16(&[11, 0]).await.unwrap();
    input.commit_input().await.unwrap();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_event())
            .await
            .expect("first utterance should complete before testing an idle session")
            .unwrap();
        if matches!(event, QwenRealtimeAudioEvent::InputTranscriptDone { .. }) {
            break;
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), reaction.clone())
            .await
            .is_err()
    );
    let expected = match stop {
        IdleSessionStop::Interrupt => {
            reaction.interrupt().unwrap();
            ReactionOutcome::Interrupted
        }
        IdleSessionStop::Exit => {
            app.exit_handle().request(ExitReason::Requested).unwrap();
            ReactionOutcome::Exited(ExitReason::Requested)
        }
    };
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction)
            .await
            .expect("session input wait should observe interrupt or normal exit")
            .unwrap(),
        expected
    );
    tokio::time::timeout(DEADLINE, app.shutdown())
        .await
        .expect("shutdown should drain the session after its wait is cancelled")
        .unwrap();
    let exchange = tokio::time::timeout(DEADLINE, server)
        .await
        .expect("idle session server should observe connection cleanup")
        .unwrap();
    assert_eq!(exchange.pcm16, [11, 0]);
    assert!(exchange.committed);
    assert_eq!(frames.lock().unwrap().len(), 1);
    assert!(probe.calls.lock().unwrap().is_empty());
    assert_eq!(
        probe
            .text_events
            .lock()
            .unwrap()
            .iter()
            .filter(
                |event| matches!(event, TextTurnEvent::TextComplete(text) if text == FINAL_TEXT)
            )
            .count(),
        1
    );
}

#[tokio::test]
async fn interrupt_settles_a_voice_reaction_waiting_for_the_next_utterance() {
    stop_idle_voice_session(IdleSessionStop::Interrupt).await;
}

#[tokio::test]
async fn normal_exit_settles_a_voice_reaction_waiting_for_the_next_utterance() {
    stop_idle_voice_session(IdleSessionStop::Exit).await;
}

#[tokio::test]
async fn xml_voice_action_delivers_updated_view_and_continues_without_another_utterance() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api-ws/v1/realtime?model={MODEL}",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut socket = open_session(&listener, "sess_voice_application").await;
        let first = receive_audio_exchange(&mut socket, "input_note").await;
        for event in audio_reply(
            "resp_save_note",
            "item_save_note",
            "<save_note>remember umbrella</save_note>",
            &[1, 0, 2, 0],
        ) {
            send_event(&mut socket, event).await;
        }
        send_event(
            &mut socket,
            json!({
                "type": "conversation.item.input_audio_transcription.completed",
                "item_id": "input_note", "content_index": 0,
                "transcript": "Remind me to take an umbrella."
            }),
        )
        .await;
        let feedback = receive_audio_exchange(&mut socket, "input_note").await;
        for event in audio_reply(
            "resp_note_ack",
            "item_note_ack",
            "Saved your umbrella reminder.",
            &[3, 0, 4, 0],
        ) {
            send_event(&mut socket, event).await;
        }
        let closed = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("finished XML voice session should close");
        assert!(matches!(
            closed,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));
        [first, feedback]
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        TEST_API_KEY,
        ModelSpec::new(MODEL, 32_768).unwrap(),
    )
    .unwrap();
    let (provider, input, mut events) = provider.with_audio();
    let probe = XmlNoteProbe::default();
    let root_probe = probe.clone();
    let observed = Arc::new(Mutex::new(Vec::<ObservedFrame>::new()));
    let observed_frames = observed.clone();
    let mut app = Application::mount(move || voice_notebook(root_probe.clone()), provider)
        .unwrap()
        .with_frame_observer(move |event| {
            observed_frames.lock().unwrap().push(ObservedFrame {
                reaction_id: event.reaction_id(),
                basis: event.frame().basis(),
                document: projection_document(event.projection()),
                replay: event.frame().submission().replay().to_vec(),
                staged_inputs: event.frame().submission().staged_inputs().to_vec(),
            });
        });
    let reaction = app.react(false).unwrap();
    let reaction_id = reaction.id();
    input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
    input.commit_input().await.unwrap();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_event())
            .await
            .expect("XML action feedback should trigger another response")
            .unwrap();
        if matches!(
            event,
            QwenRealtimeAudioEvent::ResponseEnded { response_id, status, .. }
                if response_id == "resp_note_ack" && status == QwenRealtimeResponseStatus::Completed
        ) {
            break;
        }
    }
    input.finish().await.unwrap();
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction)
            .await
            .expect("XML action and acknowledgement should settle one reaction")
            .unwrap(),
        ReactionOutcome::Completed
    );
    app.shutdown().await.unwrap();
    let [first, feedback] = tokio::time::timeout(DEADLINE, server)
        .await
        .expect("XML voice server should finish")
        .unwrap();

    assert_eq!(first.pcm16, [11, 0, 12, 0]);
    assert!(first.committed);
    assert!(feedback.pcm16.is_empty());
    assert!(!feedback.committed);
    assert!(feedback
        .items
        .iter()
        .all(|item| { item["type"] != "function_call" && item["type"] != "function_call_output" }));
    assert_eq!(*probe.notes.lock().unwrap(), ["remember umbrella"]);
    let frames = observed.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|frame| frame.reaction_id == reaction_id));
    assert!(frames[0].document.has_field("saved_note", ""));
    assert!(frames[1]
        .document
        .has_field("saved_note", "remember umbrella"));
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    let completed: Vec<_> = probe
        .text_events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            TextTurnEvent::TextComplete(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        [
            "<save_note>remember umbrella</save_note>",
            "Saved your umbrella reminder."
        ]
    );
}

#[tokio::test]
async fn voice_ephemeral_context_follows_audio_and_is_deleted_before_the_next_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api-ws/v1/realtime?model={MODEL}",
        listener.local_addr().unwrap()
    );
    let (delete_started, wait_for_delete) = oneshot::channel();
    let (release_deletion, wait_for_release) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = open_session(&listener, "sess_voice_application").await;
        let mut delete_started = Some(delete_started);
        let mut wait_for_release = Some(wait_for_release);
        let mut exchanges = Vec::new();
        let mut deleted_ids = Vec::new();
        for round in 0..2 {
            let input_id = format!("input_ephemeral_{round}");
            let exchange = receive_audio_exchange(&mut socket, &input_id).await;
            let ephemeral_start = exchange.items.len() - 2;
            assert_eq!(exchange.items[ephemeral_start]["role"], "system");
            assert_eq!(exchange.items[ephemeral_start + 1]["role"], "user");
            let commit_position = exchange
                .events
                .iter()
                .position(|event| event["type"] == "input_audio_buffer.commit")
                .unwrap();
            let created_positions: Vec<_> = exchange
                .events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    (event["type"] == "conversation.item.create").then_some(index)
                })
                .collect();
            assert!(commit_position < created_positions[ephemeral_start]);
            assert_eq!(
                created_positions[ephemeral_start + 1] + 1,
                exchange.events.len() - 1
            );
            for event in audio_reply(
                &format!("resp_ephemeral_{round}"),
                &format!("item_ephemeral_{round}"),
                "Have a pleasant trip.",
                &[1, 0, 2, 0],
            ) {
                send_event(&mut socket, event).await;
            }
            send_event(
                &mut socket,
                json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "item_id": input_id, "content_index": 0, "transcript": "Hello."
                }),
            )
            .await;
            for index in ephemeral_start..exchange.items.len() {
                let deletion = next_event(&mut socket).await;
                let acknowledged_id = exchange.items[index]["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("item_{input_id}_{index}"));
                assert_eq!(deletion["type"], "conversation.item.delete");
                assert_eq!(deletion["item_id"], acknowledged_id);
                if let Some(started) = delete_started.take() {
                    started.send(()).unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(50), socket.next())
                            .await
                            .is_err()
                    );
                    wait_for_release.take().unwrap().await.unwrap();
                }
                send_event(
                    &mut socket,
                    json!({"type": "conversation.item.deleted", "item_id": acknowledged_id}),
                )
                .await;
                deleted_ids.push(acknowledged_id);
            }
            exchanges.push(exchange);
        }
        let closed = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("finished ephemeral voice session should close");
        assert!(matches!(
            closed,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));
        (exchanges, deleted_ids)
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        TEST_API_KEY,
        ModelSpec::new(MODEL, 32_768).unwrap(),
    )
    .unwrap();
    let (provider, input, mut events) = provider.with_audio();
    let observed = Arc::new(Mutex::new(Vec::<EphemeralVoiceFrame>::new()));
    let observed_frames = observed.clone();
    let mut app = Application::mount(ephemeral_voice_context, provider)
        .unwrap()
        .with_frame_observer(move |event| {
            let submission = event.frame().submission();
            observed_frames.lock().unwrap().push(EphemeralVoiceFrame {
                reaction_id: event.reaction_id(),
                basis: event.frame().basis(),
                ephemeral: submission.ephemeral().to_vec(),
                replay: submission.replay().to_vec(),
                projection: submission.projection().items().to_vec(),
            });
        });
    let reaction = app.react(false).unwrap();
    let reaction_id = reaction.id();
    input.append_pcm16(&[11, 0]).await.unwrap();
    input.commit_input().await.unwrap();
    tokio::time::timeout(DEADLINE, wait_for_delete)
        .await
        .expect("first response should start deleting its temporary context")
        .unwrap();
    input.append_pcm16(&[21, 0]).await.unwrap();
    input.commit_input().await.unwrap();
    release_deletion.send(()).unwrap();
    loop {
        let event = tokio::time::timeout(DEADLINE, events.next_event())
            .await
            .expect("second utterance should reuse the session after deleting temporary context")
            .unwrap();
        if matches!(
            event,
            QwenRealtimeAudioEvent::ResponseEnded { response_id, .. }
                if response_id == "resp_ephemeral_1"
        ) {
            break;
        }
    }
    input.finish().await.unwrap();
    assert_eq!(
        tokio::time::timeout(DEADLINE, reaction)
            .await
            .expect("ephemeral voice reaction should finish after acknowledged cleanup")
            .unwrap(),
        ReactionOutcome::Completed
    );
    app.shutdown().await.unwrap();
    let (exchanges, deleted_ids) = tokio::time::timeout(DEADLINE, server)
        .await
        .expect("ephemeral voice server should finish")
        .unwrap();
    assert_eq!(exchanges.len(), 2);
    assert_eq!(deleted_ids.len(), 4);
    assert_eq!(exchanges[0].pcm16, [11, 0]);
    assert_eq!(exchanges[1].pcm16, [21, 0]);
    let frames = observed.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    for frame in frames.iter() {
        assert_eq!(frame.reaction_id, reaction_id);
        assert_eq!(frame.ephemeral.len(), 2);
        let ephemeral =
            pom_support::combine(frame.ephemeral.iter().map(pom_support::item_document));
        assert!(ephemeral.has_field("temporary_policy", "Give a short spoken answer."));
        assert!(ephemeral.has_field("temporary_context", "The caller is planning a trip."));
        assert!(frame
            .replay
            .iter()
            .chain(&frame.projection)
            .all(|item| !frame.ephemeral.contains(item)));
    }
}
