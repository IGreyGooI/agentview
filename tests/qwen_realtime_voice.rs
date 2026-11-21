use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        execution::{FrameRevision, ReactionOutcome},
        prelude::*,
    },
    llm_call::TextTurnEvent,
    provider::{
        qwen_realtime::{
            QwenRealtimeAudioEvent, QwenRealtimeProvider, QwenRealtimeResponseStatus,
            QwenRealtimeVoiceApplication,
        },
        ModelSpec,
    },
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{oneshot, Notify},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
    WebSocketStream,
};

const MODEL: &str = "qwen-audio-3.1-realtime-plus";
const CREDENTIAL: &str = "local-voice-test-credential";
const SESSION: &str = "sess_autonomous_voice";
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct VoiceProbe {
    text_frames: Arc<Mutex<Vec<(String, FrameRevision)>>>,
    tool_frames: Arc<Mutex<Vec<FrameRevision>>>,
    completed_frames: Arc<Mutex<Vec<FrameRevision>>>,
    external_state: Arc<Mutex<Option<Signal<u32>>>>,
    rendered_values: Arc<Mutex<Vec<u32>>>,
    rendered: Arc<Notify>,
}

#[derive(Clone, Default)]
struct RefreshProbe {
    state: Arc<Mutex<Option<Signal<u32>>>>,
    calls: Arc<Mutex<Vec<(u32, EventContext)>>>,
    completions: Arc<Mutex<Vec<EventContext>>>,
}

#[component]
fn refreshable_assistant(probe: RefreshProbe) -> Component {
    let state = use_signal(|| 0u32);
    *probe.state.lock().unwrap() = Some(state.clone());
    let generation = state.with(|value| *value).unwrap();
    let completions = probe.completions.clone();
    use_reaction_completion(move |context| async move {
        completions.lock().unwrap().push(context);
        Ok::<_, Infallible>(())
    });
    view! {
        #[system_once]
        "Use the current task state when answering."

        #[developer(ephemeral, repeat)]
        { format!("Current task generation: {generation}") }

        NativeToolCall {
            name: "report_status",
            description: "Read the task state delivered by this Frame.",
            on_call: move |context: EventContext| {
                probe.calls.lock().unwrap().push((generation, context));
                Ok::<_, Infallible>(json!({"generation": generation}))
            },
        }
    }
}

#[component]
fn voice_assistant(probe: VoiceProbe) -> Component {
    let external_state = use_signal(|| 0u32);
    *probe.external_state.lock().unwrap() = Some(external_state.clone());
    probe
        .rendered_values
        .lock()
        .unwrap()
        .push(external_state.with(|value| *value).unwrap());
    probe.rendered.notify_one();
    let completed_frames = probe.completed_frames.clone();
    use_reaction_completion(move |context: EventContext| async move {
        completed_frames
            .lock()
            .unwrap()
            .push(context.frame_revision().unwrap());
        Ok::<_, Infallible>(())
    });
    let completed = use_signal(|| false);
    let queried = use_signal(|| false);
    let completion_state = completed.clone();
    let text_frames = probe.text_frames.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |context, event| {
        let completion_state = completion_state.clone();
        let text_frames = text_frames.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                text_frames
                    .lock()
                    .unwrap()
                    .push((text, context.frame_revision().unwrap()));
                completion_state.set(true).unwrap();
            }
            Ok::<_, Infallible>(())
        }
    });
    let tools = if completed.with(|completed| *completed).unwrap()
        && !queried.with(|queried| *queried).unwrap()
    {
        view! {
            NativeToolCall {
                name: "report_status",
                description: "Report the current voice application status.",
                on_call: move |context: EventContext| {
                    probe.tool_frames.lock().unwrap().push(context.frame_revision().unwrap());
                    queried.set(true).unwrap();
                    Ok::<_, Infallible>(json!({"status": "ready"}))
                },
            }
        }
    } else {
        view! {}
    };
    view! {
        #[system_once]
        "Answer briefly in Chinese."

        #[developer(repeat)]
        "Use the current application state and confirmed tool results when answering."

        { tools }
    }
}

#[component]
fn interruptible_voice_assistant(probe: VoiceProbe) -> Component {
    let completed_frames = probe.completed_frames.clone();
    use_reaction_completion(move |context: EventContext| async move {
        completed_frames
            .lock()
            .unwrap()
            .push(context.frame_revision().unwrap());
        Ok::<_, Infallible>(())
    });
    use_provider_event_handler(ProviderEvent::TEXT, move |context, event| {
        let text_frames = probe.text_frames.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                text_frames
                    .lock()
                    .unwrap()
                    .push((text, context.frame_revision().unwrap()));
            }
            Ok::<_, Infallible>(())
        }
    });
    view! {
        #[system_once]
        "Listen continuously and answer briefly in Chinese."
    }
}

async fn send(socket: &mut WebSocketStream<TcpStream>, event: Value) {
    socket
        .send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
}

async fn receive(socket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let message = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("voice application should send the next event")
            .expect("voice application should keep its socket open")
            .unwrap();
        match message {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("expected a voice protocol event, got {other:?}"),
        }
    }
}

async fn acknowledge_update(socket: &mut WebSocketStream<TcpStream>, event: &Value) {
    let mut session = event["session"].clone();
    session["id"] = json!(SESSION);
    session["model"] = json!(MODEL);
    session["modalities"] = json!(["text", "audio"]);
    session["turn_detection"] = json!({"type": "smart_turn_v2"});
    send(
        socket,
        json!({"type": "session.updated", "session": session}),
    )
    .await;
}

async fn acknowledge_item(socket: &mut WebSocketStream<TcpStream>, event: &Value) {
    let mut item = event["item"].clone();
    item["id"] = json!("item_application_context");
    send(
        socket,
        json!({"type": "conversation.item.created", "item": item}),
    )
    .await;
}

async fn receive_audio(socket: &mut WebSocketStream<TcpStream>) -> Vec<u8> {
    loop {
        let event = receive(socket).await;
        match event["type"].as_str().unwrap() {
            "conversation.item.create" => acknowledge_item(socket, &event).await,
            "input_audio_buffer.append" => {
                return STANDARD.decode(event["audio"].as_str().unwrap()).unwrap();
            }
            other => panic!("speech must be streamed without client turn commands, got {other}"),
        }
    }
}

async fn receive_repeated_instruction(socket: &mut WebSocketStream<TcpStream>) -> Value {
    let event = receive(socket).await;
    assert_eq!(event["type"], "conversation.item.create");
    let item = &event["item"];
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "system");
    assert_eq!(item["content"].as_array().unwrap().len(), 1);
    assert_eq!(item["content"][0]["type"], "input_text");
    acknowledge_item(socket, &event).await;
    event["item"].clone()
}

async fn receive_context_update(socket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let event = receive(socket).await;
        match event["type"].as_str().unwrap() {
            "session.update" => return event,
            "conversation.item.delete" => {
                send(
                    socket,
                    json!({"type":"conversation.item.deleted", "item_id":event["item_id"]}),
                )
                .await;
            }
            other => panic!("context refresh must not generate a reply, got {other}"),
        }
    }
}

async fn send_input(socket: &mut WebSocketStream<TcpStream>, input_id: &str) {
    send(
        socket,
        json!({
            "type": "input_audio_buffer.speech_started", "item_id": input_id,
            "audio_start_ms": 0,
        }),
    )
    .await;
    send_committed_input(socket, input_id, 100).await;
}

async fn send_committed_input(
    socket: &mut WebSocketStream<TcpStream>,
    input_id: &str,
    audio_end_ms: u64,
) {
    for event in [
        json!({
            "type": "input_audio_buffer.speech_stopped", "item_id": input_id,
            "audio_end_ms": audio_end_ms,
        }),
        json!({"type": "input_audio_buffer.committed", "item_id": input_id}),
        json!({
            "type": "conversation.item.created",
            "item": {
                "id": input_id, "type": "message", "role": "user", "status": "completed",
                "content": [{"type": "input_audio"}],
            },
        }),
        json!({
            "type": "conversation.item.input_audio_transcription.completed",
            "item_id": input_id, "content_index": 0, "transcript": "请回答。",
        }),
    ] {
        send(socket, event).await;
    }
}

async fn accept_session(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = accept_hdr_async(stream, |request: &Request, response: Response| {
        assert_eq!(request.uri().path(), "/api-ws/v1/realtime");
        assert_eq!(
            request.uri().query(),
            Some(format!("model={MODEL}").as_str())
        );
        assert_eq!(
            request.headers()["authorization"],
            format!("Bearer {CREDENTIAL}")
        );
        Ok(response)
    })
    .await
    .unwrap();
    send(
        &mut socket,
        json!({
            "type": "session.created",
            "session": {"id": SESSION, "model": MODEL, "modalities": ["text", "audio"]},
        }),
    )
    .await;
    socket
}

async fn await_session_close(socket: &mut WebSocketStream<TcpStream>) {
    loop {
        let message = tokio::time::timeout(DEADLINE, socket.next())
            .await
            .expect("finishing the application should close the provider socket");
        match message {
            Some(Ok(Message::Text(text))) => {
                let event: Value = serde_json::from_str(text.as_str()).unwrap();
                match event["type"].as_str().unwrap() {
                    "session.update" => acknowledge_update(socket, &event).await,
                    "conversation.item.create" => acknowledge_item(socket, &event).await,
                    "conversation.item.delete" => {
                        send(
                            socket,
                            json!({"type":"conversation.item.deleted", "item_id":event["item_id"]}),
                        )
                        .await
                    }
                    other => panic!("unexpected client turn command after speech, got {other}"),
                }
            }
            Some(Ok(Message::Ping(payload))) => socket.send(Message::Pong(payload)).await.unwrap(),
            Some(Ok(Message::Close(_))) | None => break,
            other => panic!("unexpected shutdown message {other:?}"),
        }
    }
}

async fn send_audio_reply(
    socket: &mut WebSocketStream<TcpStream>,
    response_id: &str,
    item_id: &str,
    transcript: &str,
    pcm16: &[u8],
    mut local_render: Option<oneshot::Receiver<()>>,
) {
    let item = json!({
        "id": item_id, "object": "realtime.item", "type": "message",
        "status": "completed", "role": "assistant",
        "content": [{"type": "audio", "transcript": transcript}],
    });
    for event in [
        json!({
            "type": "response.created",
            "response": {"id": response_id, "status": "in_progress", "output": []},
        }),
        json!({
            "type": "response.output_item.added", "response_id": response_id, "output_index": 0,
            "item": {
                "id": item_id, "object": "realtime.item", "type": "message",
                "status": "in_progress", "role": "assistant", "content": [],
            },
        }),
        json!({
            "type": "response.content_part.added", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "part": {"type": "audio", "text": ""},
        }),
        json!({
            "type": "response.audio_transcript.delta", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0, "delta": transcript,
        }),
        json!({
            "type": "response.audio.delta", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "delta": STANDARD.encode(pcm16),
        }),
        json!({
            "type": "response.audio_transcript.done", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0, "transcript": transcript,
        }),
        json!({
            "type": "response.audio.done", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
        }),
        json!({
            "type": "response.content_part.done", "response_id": response_id,
            "item_id": item_id, "output_index": 0, "content_index": 0,
            "part": {"type": "audio", "text": transcript},
        }),
        json!({
            "type": "response.output_item.done", "response_id": response_id,
            "output_index": 0, "item": item,
        }),
        json!({
            "type": "response.done",
            "response": {
                "id": response_id, "object": "realtime.response", "status": "completed",
                "modalities": ["text", "audio"], "output": [item],
            },
        }),
    ] {
        if event["type"] == "response.done" {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), socket.next())
                    .await
                    .is_err(),
                "rendering changed Components must wait for response.done before handing off a Frame"
            );
        }
        let started = event["type"] == "response.created";
        send(socket, event).await;
        if started {
            if let Some(rendered) = local_render.take() {
                tokio::time::timeout(DEADLINE, rendered)
                    .await
                    .expect("Signal writes should render while the response is active")
                    .unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), socket.next())
                        .await
                        .is_err(),
                    "a local render must preserve the response.done Frame boundary"
                );
            }
        }
    }
}

async fn send_tool_reply(socket: &mut WebSocketStream<TcpStream>) {
    let item = json!({
        "id": "item_status", "object": "realtime.item", "type": "function_call",
        "status": "completed", "call_id": "call_status", "name": "report_status", "arguments": "{}",
    });
    let mut added = item.clone();
    added["status"] = json!("in_progress");
    added["arguments"] = json!("");
    for event in [
        json!({
            "type": "response.created",
            "response": {"id": "response_status", "status": "in_progress", "output": []},
        }),
        json!({
            "type": "response.output_item.added", "response_id": "response_status",
            "output_index": 0, "item": added,
        }),
        json!({
            "type": "response.function_call_arguments.done", "response_id": "response_status",
            "item_id": "item_status", "output_index": 0, "call_id": "call_status",
            "name": "report_status", "arguments": "{}",
        }),
        json!({
            "type": "response.output_item.done", "response_id": "response_status",
            "output_index": 0, "item": item,
        }),
        json!({
            "type": "response.done",
            "response": {
                "id": "response_status", "object": "realtime.response", "status": "completed",
                "modalities": ["text", "audio"], "output": [item],
            },
        }),
    ] {
        send(socket, event).await;
    }
}

#[tokio::test]
async fn explicit_context_refresh_is_silent_and_keeps_a_newer_signal_write_during_ack() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let (initialized_tx, initialized_rx) = oneshot::channel();
    let (refresh_started_tx, refresh_started_rx) = oneshot::channel();
    let (newer_queued_tx, newer_queued_rx) = oneshot::channel();
    let (context_ready_tx, context_ready_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let initial = receive(&mut socket).await;
        acknowledge_update(&mut socket, &initial).await;
        receive_repeated_instruction(&mut socket).await;
        initialized_tx.send(()).unwrap();
        assert_eq!(receive_audio(&mut socket).await, [11, 0, 12, 0]);

        let first_refresh = receive_context_update(&mut socket).await;
        assert!(first_refresh["session"].get("turn_detection").is_none());
        refresh_started_tx.send(()).unwrap();
        newer_queued_rx.await.unwrap();
        acknowledge_update(&mut socket, &first_refresh).await;
        receive_repeated_instruction(&mut socket).await;

        let second_refresh = receive_context_update(&mut socket).await;
        acknowledge_update(&mut socket, &second_refresh).await;
        receive_repeated_instruction(&mut socket).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err(),
            "context-only Frames must leave Smart Turn waiting for real input"
        );
        context_ready_tx.send(()).unwrap();
        assert_eq!(receive_audio(&mut socket).await, [21, 0, 22, 0]);
        send_input(&mut socket, "input-refresh").await;
        send_tool_reply(&mut socket).await;

        let continuation = receive_context_update(&mut socket).await;
        acknowledge_update(&mut socket, &continuation).await;
        let result = receive(&mut socket).await;
        assert_eq!(result["item"]["type"], "function_call_output");
        assert_eq!(result["item"]["call_id"], "call_status");
        let output: Value =
            serde_json::from_str(result["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!({"generation":2}));
        acknowledge_item(&mut socket, &result).await;
        receive_repeated_instruction(&mut socket).await;
        assert_eq!(receive(&mut socket).await["type"], "response.create");
        send_audio_reply(
            &mut socket,
            "refresh-answer",
            "refresh-answer-item",
            "结果已更新。",
            &[1, 0],
            None,
        )
        .await;
        await_session_close(&mut socket).await;
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = RefreshProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = QwenRealtimeVoiceApplication::mount(
        move || refreshable_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    let operation = async {
        initialized_rx.await.unwrap();
        let state = probe.state.lock().unwrap().clone().unwrap();
        input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
        state.set(1).unwrap();
        input.refresh_context().await.unwrap();
        refresh_started_rx.await.unwrap();
        state.set(2).unwrap();
        input.refresh_context().await.unwrap();
        newer_queued_tx.send(()).unwrap();
        context_ready_rx.await.unwrap();
        assert!(
            probe.completions.lock().unwrap().is_empty(),
            "silent refresh is not a reaction completion"
        );
        input.append_pcm16(&[21, 0, 22, 0]).await.unwrap();
        loop {
            if matches!(events.next_event().await.unwrap(), QwenRealtimeAudioEvent::ResponseEnded { response_id, .. } if response_id == "refresh-answer")
            {
                break;
            }
        }
        input.finish().await.unwrap();
    };
    let (outcome, ()) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .unwrap();
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    server.await.unwrap();
    let calls = probe.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, 2);
    let completions = probe.completions.lock().unwrap();
    assert!(completions
        .iter()
        .all(|context| context.reaction_id() == calls[0].1.reaction_id()));
    assert_eq!(completions[0].frame_revision(), calls[0].1.frame_revision());
}

#[tokio::test]
async fn response_racing_a_context_update_keeps_the_prior_frames_native_bindings() {
    exercise_context_update_race(true).await;
    exercise_context_update_race(false).await;
}

async fn exercise_context_update_race(before_update_ack: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let (initialized_tx, initialized_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let initial = receive(&mut socket).await;
        acknowledge_update(&mut socket, &initial).await;
        receive_repeated_instruction(&mut socket).await;
        initialized_tx.send(()).unwrap();
        assert_eq!(receive_audio(&mut socket).await, [11, 0, 12, 0]);

        let refresh = receive_context_update(&mut socket).await;
        // Previously forwarded audio starts a response before the update ACK.
        // Its function call must execute the old closure, even though the new
        // Signal value has already been rendered and handed off locally.
        if before_update_ack {
            send_input(&mut socket, "input-race").await;
            send_tool_reply(&mut socket).await;
            acknowledge_update(&mut socket, &refresh).await;
            receive_repeated_instruction(&mut socket).await;
        } else {
            acknowledge_update(&mut socket, &refresh).await;
            let context = receive(&mut socket).await;
            assert_eq!(context["item"]["role"], "system");
            send_input(&mut socket, "input-race").await;
            send_tool_reply(&mut socket).await;
            acknowledge_item(&mut socket, &context).await;
        }

        let continuation = receive_context_update(&mut socket).await;
        acknowledge_update(&mut socket, &continuation).await;
        let result = receive(&mut socket).await;
        assert_eq!(result["item"]["type"], "function_call_output");
        assert_eq!(result["item"]["call_id"], "call_status");
        let output: Value =
            serde_json::from_str(result["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!({"generation":0}));
        acknowledge_item(&mut socket, &result).await;
        receive_repeated_instruction(&mut socket).await;
        assert_eq!(receive(&mut socket).await["type"], "response.create");
        send_audio_reply(
            &mut socket,
            "race-answer",
            "race-answer-item",
            "已取得结果。",
            &[1, 0],
            None,
        )
        .await;
        await_session_close(&mut socket).await;
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = RefreshProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = QwenRealtimeVoiceApplication::mount(
        move || refreshable_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    let operation = async {
        initialized_rx.await.unwrap();
        input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
        probe
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .set(1)
            .unwrap();
        input.refresh_context().await.unwrap();
        loop {
            if matches!(events.next_event().await.unwrap(), QwenRealtimeAudioEvent::ResponseEnded { response_id, .. } if response_id == "race-answer")
            {
                break;
            }
        }
        input.finish().await.unwrap();
    };
    let (outcome, ()) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .unwrap();
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    server.await.unwrap();
    let calls = probe.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, 0);
    let completions = probe.completions.lock().unwrap();
    assert_eq!(completions[0].frame_revision(), calls[0].1.frame_revision());
    assert_ne!(completions[1].frame_revision(), calls[0].1.frame_revision());
    assert!(completions
        .iter()
        .all(|context| context.reaction_id() == calls[0].1.reaction_id()));
}

#[tokio::test]
async fn refused_optional_context_commands_keep_voice_and_native_tool_results_alive() {
    exercise_refused_context(true).await;
    exercise_refused_context(false).await;
}

async fn exercise_refused_context(reject_update: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let (initialized_tx, initialized_rx) = oneshot::channel();
    let (refused_tx, refused_rx) = oneshot::channel();
    let expected_generation = if reject_update { 0 } else { 1 };
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let initial = receive(&mut socket).await;
        acknowledge_update(&mut socket, &initial).await;
        receive_repeated_instruction(&mut socket).await;
        initialized_tx.send(()).unwrap();
        let refresh = receive_context_update(&mut socket).await;
        let command = if reject_update {
            "session.update"
        } else {
            acknowledge_update(&mut socket, &refresh).await;
            let context = receive(&mut socket).await;
            assert_eq!(context["item"]["role"], "system");
            "conversation.item.create"
        };
        send(
            &mut socket,
            json!({"type":"error", "error": {
                "type":"invalid_request_error", "code":"invalid_value", "param":command,
                "message":"Optional context refused by test provider"
            }}),
        )
        .await;
        let unexpected = tokio::time::timeout(Duration::from_millis(50), socket.next()).await;
        assert!(unexpected.is_err(),
            "a refused optional context command must not generate, retry, or replace the voice session: {unexpected:?}");
        refused_tx.send(()).unwrap();
        assert_eq!(receive_audio(&mut socket).await, [31, 0, 32, 0]);
        send_input(&mut socket, "input-after-refusal").await;
        send_tool_reply(&mut socket).await;
        let continuation = receive_context_update(&mut socket).await;
        acknowledge_update(&mut socket, &continuation).await;
        let result = receive(&mut socket).await;
        assert_eq!(result["item"]["type"], "function_call_output");
        let output: Value =
            serde_json::from_str(result["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!({"generation":expected_generation}));
        acknowledge_item(&mut socket, &result).await;
        receive_repeated_instruction(&mut socket).await;
        assert_eq!(receive(&mut socket).await["type"], "response.create");
        send_audio_reply(
            &mut socket,
            "refusal-answer",
            "refusal-answer-item",
            "语音仍可继续。",
            &[1, 0],
            None,
        )
        .await;
        await_session_close(&mut socket).await;
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = RefreshProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = QwenRealtimeVoiceApplication::mount(
        move || refreshable_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    let operation = async {
        initialized_rx.await.unwrap();
        probe
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .set(1)
            .unwrap();
        input.refresh_context().await.unwrap();
        refused_rx.await.unwrap();
        assert!(probe.completions.lock().unwrap().is_empty());
        input.append_pcm16(&[31, 0, 32, 0]).await.unwrap();
        loop {
            if matches!(events.next_event().await.unwrap(), QwenRealtimeAudioEvent::ResponseEnded { response_id, .. } if response_id == "refusal-answer")
            {
                break;
            }
        }
        input.finish().await.unwrap();
    };
    let (outcome, ()) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .unwrap();
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    server.await.unwrap();
    assert_eq!(probe.calls.lock().unwrap()[0].0, expected_generation);
}

#[tokio::test]
async fn smart_turn_voice_streams_two_rounds_and_refreshes_the_frame_after_response_done() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let (audio_queued_tx, audio_queued_rx) = oneshot::channel();
    let (local_render_tx, local_render_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;

        let initial = receive(&mut socket).await;
        assert_eq!(initial["type"], "session.update");
        assert_eq!(
            initial["session"]["turn_detection"]["type"],
            "smart_turn_v2"
        );
        assert_ne!(
            initial["session"]["turn_detection"]["create_response"],
            false
        );
        assert_eq!(initial["session"]["tools"], json!([]));
        acknowledge_update(&mut socket, &initial).await;
        let first_instruction = receive_repeated_instruction(&mut socket).await;
        assert_eq!(receive_audio(&mut socket).await, [11, 0, 12, 0]);
        send_input(&mut socket, "input_1").await;
        send_audio_reply(
            &mut socket,
            "response_1",
            "answer_1",
            "第一句。",
            &[1, 0, 2, 0],
            Some(local_render_rx),
        )
        .await;

        let next = receive(&mut socket).await;
        assert_eq!(next["type"], "session.update");
        assert_eq!(
            next["session"]["tools"][0]["function"]["name"],
            "report_status"
        );
        audio_queued_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err()
        );
        acknowledge_update(&mut socket, &next).await;
        let second_instruction = receive_repeated_instruction(&mut socket).await;
        assert_eq!(receive_audio(&mut socket).await, [21, 0, 22, 0]);
        send_input(&mut socket, "input_2").await;
        send_tool_reply(&mut socket).await;
        let feedback_frame = receive(&mut socket).await;
        assert_eq!(feedback_frame["type"], "session.update");
        assert_eq!(feedback_frame["session"]["tools"], json!([]));
        acknowledge_update(&mut socket, &feedback_frame).await;
        let result = receive(&mut socket).await;
        assert_eq!(result["type"], "conversation.item.create");
        assert_eq!(result["item"]["type"], "function_call_output");
        assert_eq!(result["item"]["call_id"], "call_status");
        let status: Value =
            serde_json::from_str(result["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(status, json!({"status": "ready"}));
        acknowledge_item(&mut socket, &result).await;
        let third_instruction = receive_repeated_instruction(&mut socket).await;
        assert_eq!(first_instruction, second_instruction);
        assert_eq!(second_instruction, third_instruction);
        let continuation = receive(&mut socket).await;
        assert_eq!(continuation["type"], "response.create");
        send_audio_reply(
            &mut socket,
            "response_2",
            "answer_2",
            "第二句。",
            &[3, 0, 4, 0],
            None,
        )
        .await;

        await_session_close(&mut socket).await;
    });

    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = VoiceProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) =
        QwenRealtimeVoiceApplication::mount(move || voice_assistant(root_probe.clone()), provider)
            .unwrap();
    let operation = async {
        let mut audio_queued_tx = Some(audio_queued_tx);
        let mut local_render_tx = Some(local_render_tx);
        input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
        let mut received = Vec::new();
        for response_id in ["response_1", "response_2"] {
            loop {
                let event = tokio::time::timeout(DEADLINE, events.next_event())
                    .await
                    .expect("autonomous speech response should arrive")
                    .unwrap();
                let done = matches!(&event, QwenRealtimeAudioEvent::ResponseEnded {
                    response_id: id, status: QwenRealtimeResponseStatus::Completed, ..
                } if id == response_id);
                if matches!(&event, QwenRealtimeAudioEvent::ResponseStarted { response_id }
                    if response_id == "response_1")
                {
                    let state = probe.external_state.lock().unwrap().clone().unwrap();
                    state.set(1).unwrap();
                    tokio::time::timeout(DEADLINE, async {
                        loop {
                            let rendered = probe.rendered.notified();
                            tokio::pin!(rendered);
                            rendered.as_mut().enable();
                            if probe.rendered_values.lock().unwrap().contains(&1) {
                                break;
                            }
                            rendered.await;
                        }
                    })
                    .await
                    .expect("external Signal change should trigger a local render");
                    local_render_tx.take().unwrap().send(()).unwrap();
                }
                received.push(event);
                if done {
                    break;
                }
            }
            if response_id == "response_1" {
                input.append_pcm16(&[21, 0, 22, 0]).await.unwrap();
                audio_queued_tx.take().unwrap().send(()).unwrap();
            }
        }
        input.finish().await.unwrap();
        received
    };
    let (outcome, received) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .expect("the voice application should finish after its input closes");
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap();
    for (response_id, expected_pcm) in [
        ("response_1", vec![1, 0, 2, 0]),
        ("response_2", vec![3, 0, 4, 0]),
    ] {
        assert!(received.iter().any(|event| matches!(event,
            QwenRealtimeAudioEvent::AudioDelta { response_id: id, pcm16, .. }
                if id == response_id && pcm16 == &expected_pcm
        )));
    }
    let texts = probe.text_frames.lock().unwrap();
    let tools = probe.tool_frames.lock().unwrap();
    let completed = probe.completed_frames.lock().unwrap();
    assert_eq!(texts.len(), 2);
    assert_eq!(tools.len(), 1);
    assert_eq!(texts[0].0, "第一句。");
    assert_eq!(texts[1].0, "第二句。");
    assert_ne!(texts[0].1, tools[0]);
    assert_ne!(tools[0], texts[1].1);
    assert_ne!(texts[0].1, texts[1].1);
    assert!(completed.len() >= 3);
    assert_eq!(texts[0].1, completed[0]);
    assert_eq!(tools[0], completed[1]);
    assert_eq!(texts[1].1, completed[2]);
    assert!(probe.rendered_values.lock().unwrap().contains(&1));
}

#[tokio::test]
async fn spoken_interruption_keeps_streaming_audio_and_continues_on_the_same_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let initial = receive(&mut socket).await;
        assert_eq!(initial["type"], "session.update");
        assert_eq!(
            initial["session"]["turn_detection"]["type"],
            "smart_turn_v2"
        );
        acknowledge_update(&mut socket, &initial).await;
        assert_eq!(receive_audio(&mut socket).await, [11, 0, 12, 0]);
        send_input(&mut socket, "input_before_interrupt").await;
        for event in [
            json!({
                "type": "response.created",
                "response": {"id": "response_interrupted", "status": "in_progress", "output": []},
            }),
            json!({
                "type": "response.output_item.added", "response_id": "response_interrupted",
                "output_index": 0,
                "item": {
                    "id": "answer_interrupted", "object": "realtime.item", "type": "message",
                    "status": "in_progress", "role": "assistant", "content": [],
                },
            }),
            json!({
                "type": "response.content_part.added", "response_id": "response_interrupted",
                "item_id": "answer_interrupted", "output_index": 0, "content_index": 0,
                "part": {"type": "audio", "text": ""},
            }),
            json!({
                "type": "response.audio_transcript.delta", "response_id": "response_interrupted",
                "item_id": "answer_interrupted", "output_index": 0, "content_index": 0,
                "delta": "被打断",
            }),
            json!({
                "type": "response.audio.delta", "response_id": "response_interrupted",
                "item_id": "answer_interrupted", "output_index": 0, "content_index": 0,
                "delta": STANDARD.encode([5, 0, 6, 0]),
            }),
        ] {
            send(&mut socket, event).await;
        }

        let cancel = receive(&mut socket).await;
        assert_eq!(cancel["type"], "response.cancel");
        send(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error", "code": "invalid_parameter",
                    "param": "response.cancel", "message": "Cannot cancel this automatic response.",
                },
            }),
        )
        .await;

        // The mock has not ended the response. Receiving this append proves
        // microphone audio reaches Qwen while its previous reply is active.
        assert_eq!(receive_audio(&mut socket).await, [31, 0, 32, 0]);
        send(
            &mut socket,
            json!({
                "type": "input_audio_buffer.speech_started", "item_id": "input_interrupt",
                "audio_start_ms": 200,
            }),
        )
        .await;
        send(
            &mut socket,
            json!({
                "type": "response.done",
                "response": {
                    "id": "response_interrupted", "object": "realtime.response", "status": "cancelled",
                    "status_details": {"type": "cancelled", "reason": "turn_detected"},
                    "modalities": ["text", "audio"],
                    "output": [{
                        "id": "answer_interrupted", "object": "realtime.item", "type": "message",
                        "status": "incomplete", "role": "assistant",
                        "content": [{"type": "audio", "transcript": "被打断"}],
                    }],
                },
            }),
        )
        .await;

        let refreshed = receive(&mut socket).await;
        assert_eq!(refreshed["type"], "session.update");
        acknowledge_update(&mut socket, &refreshed).await;
        send_committed_input(&mut socket, "input_interrupt", 300).await;
        send_audio_reply(
            &mut socket,
            "response_after_interrupt",
            "answer_after_interrupt",
            "收到，请继续。",
            &[7, 0, 8, 0],
            None,
        )
        .await;
        await_session_close(&mut socket).await;
    });

    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = VoiceProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = QwenRealtimeVoiceApplication::mount(
        move || interruptible_voice_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    let operation = async {
        input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
        let mut received = Vec::new();
        let mut interruption_sent = false;
        loop {
            let event = tokio::time::timeout(DEADLINE, events.next_event())
                .await
                .expect("spoken interruption should preserve the voice event stream")
                .unwrap();
            if !interruption_sent
                && matches!(&event, QwenRealtimeAudioEvent::AudioDelta { response_id, .. }
                    if response_id == "response_interrupted")
            {
                input
                    .interrupt("response_interrupted".to_owned())
                    .await
                    .unwrap_err();
                input.append_pcm16(&[31, 0, 32, 0]).await.unwrap();
                interruption_sent = true;
            }
            let done = matches!(&event, QwenRealtimeAudioEvent::ResponseEnded {
                response_id, status: QwenRealtimeResponseStatus::Completed, ..
            } if response_id == "response_after_interrupt");
            received.push(event);
            if done {
                break;
            }
        }
        assert!(interruption_sent);
        input.finish().await.unwrap();
        received
    };
    let (outcome, received) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .expect("interrupted voice session should settle after input closes");
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap();
    assert!(received.iter().any(|event| matches!(event,
        QwenRealtimeAudioEvent::ResponseEnded {
            response_id, status: QwenRealtimeResponseStatus::Cancelled, error: None,
        } if response_id == "response_interrupted"
    )));
    assert!(received.iter().any(|event| matches!(event,
        QwenRealtimeAudioEvent::AudioDelta { response_id, pcm16, .. }
            if response_id == "response_after_interrupt" && pcm16 == &[7, 0, 8, 0]
    )));
    let texts = probe.text_frames.lock().unwrap();
    let completed = probe.completed_frames.lock().unwrap();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, "收到，请继续。");
    assert!(completed.len() >= 2);
    assert_ne!(completed[0], completed[1]);
    assert_eq!(texts[0].1, completed[1]);
}

#[tokio::test]
async fn proactive_notifications_wait_for_the_next_frame_and_keep_streaming_microphone_audio() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let update = receive(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        acknowledge_update(&mut socket, &update).await;
        let first_notice = receive(&mut socket).await;
        assert_eq!(first_notice["type"], "conversation.item.create");
        assert_eq!(first_notice["item"]["role"], "user");
        let first_item = first_notice["item"]["id"].as_str().unwrap().to_owned();
        assert_eq!(receive_audio(&mut socket).await, [21, 0, 22, 0]);
        send(
            &mut socket,
            json!({"type":"conversation.item.created",
            "item":{"id":"unrelated-item", "type":"message", "role":"user", "content":[]}}),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err(),
            "the microphone remains active, but generation waits for this notice's matching ACK"
        );
        send(
            &mut socket,
            json!({"type":"conversation.item.created", "item":first_notice["item"]}),
        )
        .await;
        assert_eq!(receive(&mut socket).await["type"], "response.create");
        let answer = json!({
            "id":"greeting-item", "object":"realtime.item", "type":"message",
            "status":"completed", "role":"assistant",
            "content":[{"type":"audio", "transcript":"你好，我已经准备好了。"}],
        });
        for event in [
            json!({"type":"response.created", "response":{"id":"greeting-response", "status":"in_progress", "output":[]}}),
            json!({
                "type":"response.output_item.added", "response_id":"greeting-response", "output_index":0,
                "item":{"id":"greeting-item", "object":"realtime.item", "type":"message", "status":"in_progress", "role":"assistant", "content":[]},
            }),
            json!({
                "type":"response.content_part.added", "response_id":"greeting-response", "item_id":"greeting-item",
                "output_index":0, "content_index":0, "part":{"type":"audio", "text":""},
            }),
            json!({
                "type":"response.audio.delta", "response_id":"greeting-response", "item_id":"greeting-item",
                "output_index":0, "content_index":0, "delta":STANDARD.encode([1, 0, 2, 0]),
            }),
        ] {
            send(&mut socket, event).await;
        }
        assert_eq!(receive_audio(&mut socket).await, [31, 0, 32, 0]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err(),
            "a queued notification must not cancel or overlap the current reply"
        );
        for event in [
            json!({
                "type":"response.audio_transcript.done", "response_id":"greeting-response", "item_id":"greeting-item",
                "output_index":0, "content_index":0, "transcript":"你好，我已经准备好了。",
            }),
            json!({
                "type":"response.audio.done", "response_id":"greeting-response", "item_id":"greeting-item",
                "output_index":0, "content_index":0,
            }),
            json!({
                "type":"response.content_part.done", "response_id":"greeting-response", "item_id":"greeting-item",
                "output_index":0, "content_index":0, "part":{"type":"audio", "text":"你好，我已经准备好了。"},
            }),
            json!({"type":"response.output_item.done", "response_id":"greeting-response", "output_index":0, "item":answer}),
            json!({
                "type":"response.done", "response":{"id":"greeting-response", "object":"realtime.response", "status":"completed",
                    "modalities":["text","audio"], "output":[answer]},
            }),
        ] {
            send(&mut socket, event).await;
        }
        let delete = receive(&mut socket).await;
        assert_eq!(delete["type"], "conversation.item.delete");
        assert_eq!(delete["item_id"], first_item);
        send(
            &mut socket,
            json!({"type":"conversation.item.deleted", "item_id":first_item}),
        )
        .await;
        let update = receive(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err(),
            "the next notification must wait until the new Frame is configured"
        );
        acknowledge_update(&mut socket, &update).await;
        let notice = receive(&mut socket).await;
        assert_eq!(notice["type"], "conversation.item.create");
        assert_eq!(notice["item"]["role"], "user");
        let item_id = notice["item"]["id"].as_str().unwrap().to_owned();
        send(
            &mut socket,
            json!({"type":"conversation.item.created", "item":notice["item"]}),
        )
        .await;
        assert_eq!(receive(&mut socket).await["type"], "response.create");
        send_audio_reply(
            &mut socket,
            "notification-response",
            "notification-item",
            "你的任务已经完成。",
            &[3, 0, 4, 0],
            None,
        )
        .await;
        let delete = receive(&mut socket).await;
        assert_eq!(delete["type"], "conversation.item.delete");
        assert_eq!(delete["item_id"], item_id);
        send(
            &mut socket,
            json!({"type":"conversation.item.deleted", "item_id":item_id}),
        )
        .await;
        let update = receive(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        acknowledge_update(&mut socket, &update).await;
        await_session_close(&mut socket).await;
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = VoiceProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = QwenRealtimeVoiceApplication::mount(
        move || interruptible_voice_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    input.speak_text("你好，我已经准备好了。").await.unwrap();
    input.append_pcm16(&[21, 0, 22, 0]).await.unwrap();
    let operation = async {
        let mut ended = 0;
        while ended < 2 {
            match events.next_event().await.unwrap() {
                QwenRealtimeAudioEvent::ResponseStarted { response_id }
                    if response_id == "greeting-response" =>
                {
                    input.speak_text("你的任务已经完成。").await.unwrap();
                    input.append_pcm16(&[31, 0, 32, 0]).await.unwrap();
                }
                QwenRealtimeAudioEvent::ResponseEnded { status, .. } => {
                    assert_eq!(status, QwenRealtimeResponseStatus::Completed);
                    ended += 1;
                }
                _ => {}
            }
        }
        input.finish().await.unwrap();
    };
    let (outcome, ()) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .unwrap();
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap();
    let texts = probe.text_frames.lock().unwrap();
    assert_eq!(texts.len(), 2);
    assert_ne!(texts[0].1, texts[1].1);
}

#[tokio::test]
async fn expired_notice_acks_do_not_replace_frame_item_acks_or_interrupt_user_replies() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/api-ws/v1/realtime", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let update = receive(&mut socket).await;
        acknowledge_update(&mut socket, &update).await;
        receive_repeated_instruction(&mut socket).await;
        let notice = receive(&mut socket).await;
        assert_eq!(notice["type"], "conversation.item.create");
        assert_eq!(notice["item"]["role"], "user");
        let notice_id = notice["item"]["id"].as_str().unwrap().to_owned();
        assert_eq!(receive_audio(&mut socket).await, [21, 0, 22, 0]);
        assert!(
            tokio::time::timeout(Duration::from_millis(5_100), socket.next())
                .await
                .is_err(),
            "an unacknowledged optional notice expires without generating or retrying"
        );
        send_input(&mut socket, "user_after_notice_timeout").await;
        send_audio_reply(
            &mut socket,
            "user_reply_1",
            "user_answer_1",
            "先回答用户。",
            &[1, 0, 2, 0],
            None,
        )
        .await;
        let first_delete = receive(&mut socket).await;
        assert_eq!(first_delete["type"], "conversation.item.delete");
        assert_eq!(first_delete["item_id"], "item_application_context");
        send(
            &mut socket,
            json!({"type":"conversation.item.deleted", "item_id":first_delete["item_id"]}),
        )
        .await;
        let update = receive(&mut socket).await;
        acknowledge_update(&mut socket, &update).await;
        let context = receive(&mut socket).await;
        assert_eq!(context["type"], "conversation.item.create");
        assert_eq!(context["item"]["role"], "system");
        // This expired notice ACK arrives inside the next Frame's context ACK
        // wait. It must trigger only notice cleanup, never stand in for context.
        send(
            &mut socket,
            json!({"type":"conversation.item.created", "item":notice["item"]}),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err()
        );
        let mut context_item = context["item"].clone();
        context_item["id"] = json!("frame_context_2");
        send(
            &mut socket,
            json!({"type":"conversation.item.created", "item":context_item}),
        )
        .await;
        let optional_delete = receive(&mut socket).await;
        assert_eq!(optional_delete["type"], "conversation.item.delete");
        assert_eq!(optional_delete["item_id"], notice_id);
        assert_eq!(receive_audio(&mut socket).await, [31, 0, 32, 0]);
        send_input(&mut socket, "second_user_input").await;
        send_audio_reply(
            &mut socket,
            "user_reply_2",
            "user_answer_2",
            "继续回答用户。",
            &[3, 0, 4, 0],
            None,
        )
        .await;
        let context_delete = receive(&mut socket).await;
        assert_eq!(context_delete["type"], "conversation.item.delete");
        assert_eq!(context_delete["item_id"], "frame_context_2");
        // The optional delete ACK is also independent of this Frame's cleanup.
        send(
            &mut socket,
            json!({"type":"conversation.item.deleted", "item_id":notice_id}),
        )
        .await;
        send(
            &mut socket,
            json!({"type":"conversation.item.deleted", "item_id":"frame_context_2"}),
        )
        .await;
        await_session_close(&mut socket).await;
    });
    let provider = QwenRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 262_144).unwrap(),
    )
    .unwrap();
    let probe = VoiceProbe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = QwenRealtimeVoiceApplication::mount(
        move || {
            view! {
                #[developer(ephemeral, repeat)]
                "Use the current application state when answering."
                { interruptible_voice_assistant(root_probe.clone()) }
            }
        },
        provider,
    )
    .unwrap();
    input.speak_text("这条可选通知会超时。").await.unwrap();
    input.append_pcm16(&[21, 0, 22, 0]).await.unwrap();
    let operation = async {
        let mut ended = 0;
        while ended < 2 {
            if let QwenRealtimeAudioEvent::ResponseEnded { status, .. } =
                events.next_event().await.unwrap()
            {
                assert_eq!(status, QwenRealtimeResponseStatus::Completed);
                ended += 1;
                if ended == 1 {
                    input.append_pcm16(&[31, 0, 32, 0]).await.unwrap();
                }
            }
        }
        input.finish().await.unwrap();
    };
    let (outcome, ()) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .unwrap();
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap();
    let texts = probe.text_frames.lock().unwrap();
    assert_eq!(texts.len(), 2);
    assert_ne!(texts[0].1, texts[1].1);
}
