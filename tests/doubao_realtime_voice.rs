use std::{
    collections::BTreeMap,
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
        doubao_realtime::{DoubaoRealtimeProvider, DoubaoRealtimeVoiceApplication},
        qwen_realtime::{QwenRealtimeAudioEvent, QwenRealtimeResponseStatus},
        ModelSpec,
    },
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
    WebSocketStream,
};

const MODEL: &str = "1.2.6.1";
const CREDENTIAL: &str = "local-seeduplex-test-credential";
const VOICE: &str = "zh_female_vv_jupiter_bigtts";
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct Probe {
    tool_frames: Arc<Mutex<Vec<(String, FrameRevision)>>>,
    text_frames: Arc<Mutex<Vec<(String, FrameRevision)>>>,
    completed_frames: Arc<Mutex<Vec<FrameRevision>>>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WeatherInput {
    city: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct StockInput {
    stock_code: String,
}

#[component]
fn notification_assistant(probe: Probe) -> Component {
    let count = use_signal(|| 0u32);
    let text_frames = probe.text_frames.clone();
    let completion_count = count.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |context, event| {
        let text_frames = text_frames.clone();
        let completion_count = completion_count.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                text_frames
                    .lock()
                    .unwrap()
                    .push((text, context.frame_revision().unwrap()));
                completion_count.update(|count| *count += 1).unwrap();
            }
            Ok::<_, Infallible>(())
        }
    });
    let count = count.with(|count| *count).unwrap();
    view! {
        #[system_once]
        "Listen continuously. Read notifications when idle."
        #[developer]
        status { "{count}" }
    }
}

#[component]
fn voice_assistant(probe: Probe) -> Component {
    let calls_completed = use_signal(|| 0u8);
    let text_frames = probe.text_frames.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |context, event| {
        let text_frames = text_frames.clone();
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
    let completed_frames = probe.completed_frames.clone();
    use_reaction_completion(move |context: EventContext| {
        let completed_frames = completed_frames.clone();
        async move {
            completed_frames
                .lock()
                .unwrap()
                .push(context.frame_revision().unwrap());
            Ok::<_, Infallible>(())
        }
    });
    let tools = if calls_completed.with(|count| *count).unwrap() < 2 {
        let weather_count = calls_completed.clone();
        let weather_frames = probe.tool_frames.clone();
        let stock_frames = probe.tool_frames.clone();
        view! {
            NativeToolCall {
                name: "get_weather",
                description: "Read the current city weather.",
                on_call: move |context: EventContext, input: WeatherInput| {
                    weather_frames.lock().unwrap().push((
                        "weather".to_owned(), context.frame_revision().unwrap(),
                    ));
                    weather_count.update(|count| *count += 1).unwrap();
                    Ok::<_, Infallible>(json!({"city": input.city, "temperature": 25}))
                },
            }
            NativeToolCall {
                name: "get_stock",
                description: "Read the current stock price.",
                on_call: move |context: EventContext, input: StockInput| {
                    stock_frames.lock().unwrap().push((
                        "stock".to_owned(), context.frame_revision().unwrap(),
                    ));
                    calls_completed.update(|count| *count += 1).unwrap();
                    Ok::<_, Infallible>(json!({"stock_code": input.stock_code, "price": 33.25}))
                },
            }
        }
    } else {
        Component::fragment([])
    };
    Component::fragment([
        view! {
            #[system_once]
            "Listen continuously and answer briefly."
        },
        tools,
    ])
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
            .expect("voice application must send the next protocol event")
            .expect("voice application must keep the session open")
            .unwrap();
        match message {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("expected a Seeduplex JSON event, got {other:?}"),
        }
    }
}

async fn accept_session(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
        .await
        .unwrap()
        .unwrap();
    accept_hdr_async(stream, |request: &Request, response: Response| {
        assert_eq!(request.uri().path(), "/api/v3/realtime/dialogue");
        assert_eq!(request.headers()["x-api-key"], CREDENTIAL);
        assert!(request.headers().get("authorization").is_none());
        Ok(response)
    })
    .await
    .unwrap()
}

async fn receive_audio(socket: &mut WebSocketStream<TcpStream>) -> Vec<u8> {
    let event = receive(socket).await;
    assert_eq!(event["type"], "input_audio_buffer.append");
    STANDARD.decode(event["audio"].as_str().unwrap()).unwrap()
}

async fn send_input(socket: &mut WebSocketStream<TcpStream>, item: &str, transcript: &str) {
    for event in [
        json!({
            "type": "conversation.item.input_audio_transcription.started", "item_id": "",
        }),
        json!({
            "type": "conversation.item.input_audio_transcription.delta",
            "item_id": item, "content_index": 0, "delta": transcript,
        }),
        json!({
            "type": "conversation.item.input_audio_transcription.completed",
            "item_id": item, "content_index": 0, "transcript": transcript,
        }),
        json!({"type": "input_audio_buffer.committed"}),
    ] {
        send(socket, event).await;
    }
}

async fn send_reply(
    socket: &mut WebSocketStream<TcpStream>,
    question: &str,
    response: &str,
    text: &str,
    pcm16: &[u8],
) {
    for event in [
        json!({
            "type": "response.output_text.delta", "question_id": question,
            "response_id": response, "delta": text,
        }),
        json!({
            "type": "response.output_text.done", "question_id": question,
            "response_id": response, "text": text,
        }),
        json!({
            "type": "response.output_audio.delta", "question_id": question,
            "response_id": response, "delta": STANDARD.encode(pcm16),
        }),
        json!({
            "type": "response.output_audio.done", "question_id": question,
            "response_id": response,
        }),
        json!({
            "type": "response.done", "usage": {"input_tokens": 8, "output_tokens": 4},
        }),
    ] {
        send(socket, event).await;
    }
}

async fn await_close(socket: &mut WebSocketStream<TcpStream>) {
    assert_eq!(receive(socket).await["type"], "session.close");
    send(socket, json!({"type": "session.closed"})).await;
    loop {
        match tokio::time::timeout(DEADLINE, socket.next()).await.unwrap() {
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(Message::Ping(payload))) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("unexpected traffic after session.close: {other:?}"),
        }
    }
}

#[tokio::test]
async fn batched_native_results_continue_the_current_response_before_the_next_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api/v3/realtime/dialogue",
        listener.local_addr().unwrap(),
    );
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let create = receive(&mut socket).await;
        assert_eq!(create["type"], "session.create");
        assert_eq!(create["session"]["model"], MODEL);
        assert_eq!(
            create["session"]["audio"]["output"]["format"]["type"],
            "pcm_s16le"
        );
        assert_eq!(create["session"]["audio"]["output"]["voice"], VOICE);
        let mut tool_names: Vec<_> = create["session"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        tool_names.sort_unstable();
        assert_eq!(tool_names, vec!["get_stock", "get_weather"]);
        send(
            &mut socket,
            json!({"type": "session.created", "session": {"id": "dialog_1"}}),
        )
        .await;
        assert_eq!(receive_audio(&mut socket).await, [11, 0, 12, 0]);
        send_input(&mut socket, "input_1", "Read the weather and stock price.").await;
        send(
            &mut socket,
            json!({
                "type": "response.output_audio.started", "question_id": "question",
                "response_id": "", "tts_type": "chat_tts_text",
            }),
        )
        .await;
        send(
            &mut socket,
            json!({
                "type": "response.function_call_arguments.done",
                "items": [
                    {"id": "tool_1", "type": "function_call", "call_id": "call_weather",
                     "name": "get_weather", "arguments": "{\"city\":\"Beijing\"}"},
                    {"id": "tool_2", "type": "function_call", "call_id": "call_stock",
                     "name": "get_stock", "arguments": "{\"stock_code\":\"600036\"}"},
                ],
            }),
        )
        .await;

        // The real protocol waits here: no response.done is sent until both
        // admitted calls have returned their actual, correlated results.
        let mut results = BTreeMap::new();
        while results.len() < 2 {
            let event = receive(&mut socket).await;
            assert_eq!(event["type"], "conversation.item.create");
            for item in event["items"].as_array().unwrap() {
                assert_eq!(item["role"], "tool");
                let call = item["call_id"].as_str().unwrap().to_owned();
                let result: Value =
                    serde_json::from_str(item["content"][0]["text"].as_str().unwrap()).unwrap();
                assert!(results.insert(call, result).is_none());
            }
        }
        assert_eq!(
            results["call_weather"],
            json!({"city": "Beijing", "temperature": 25})
        );
        assert_eq!(
            results["call_stock"],
            json!({"stock_code": "600036", "price": 33.25})
        );
        send_reply(
            &mut socket,
            "question",
            "wire_response_1",
            "Both lookups are complete.",
            &[1, 0, 2, 0],
        )
        .await;

        let update = receive(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        assert_eq!(update["session"]["tools"], json!([]));
        send(
            &mut socket,
            json!({"type": "session.updated", "session": {"id": "dialog_1"}}),
        )
        .await;
        // Seeing audio next also proves the next Frame did not resend the tool
        // results that already continued the preceding provider response.
        assert_eq!(receive_audio(&mut socket).await, [21, 0, 22, 0]);
        send_input(&mut socket, "input_2", "Say the second answer.").await;
        send(
            &mut socket,
            json!({
                "type": "response.output_audio.started", "question_id": "question_2",
                "response_id": "wire_response_2", "tts_type": "chat_tts_text",
            }),
        )
        .await;
        send_reply(
            &mut socket,
            "question_2",
            "wire_response_2",
            "Second answer.",
            &[3, 0, 4, 0],
        )
        .await;
        await_close(&mut socket).await;
    });

    let provider = DoubaoRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 32_768).unwrap(),
        VOICE,
    )
    .unwrap();
    let probe = Probe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = DoubaoRealtimeVoiceApplication::mount(
        move || voice_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    let mut raw = events.subscribe_server_events();
    let operation = async {
        input.append_pcm16(&[11, 0, 12, 0]).await.unwrap();
        let mut rounds = Vec::new();
        for expected_pcm in [vec![1, 0, 2, 0], vec![3, 0, 4, 0]] {
            let mut round = Vec::new();
            loop {
                let event = events.next_event().await.unwrap();
                let done = matches!(
                    &event,
                    QwenRealtimeAudioEvent::ResponseEnded {
                        status: QwenRealtimeResponseStatus::Completed,
                        ..
                    }
                );
                round.push(event);
                if done {
                    break;
                }
            }
            let ids: Vec<_> = round
                .iter()
                .filter_map(|event| match event {
                    QwenRealtimeAudioEvent::ResponseStarted { response_id }
                    | QwenRealtimeAudioEvent::AudioDelta { response_id, .. }
                    | QwenRealtimeAudioEvent::AudioDone { response_id, .. }
                    | QwenRealtimeAudioEvent::OutputTranscriptDelta { response_id, .. }
                    | QwenRealtimeAudioEvent::OutputTranscriptDone { response_id, .. }
                    | QwenRealtimeAudioEvent::OutputTextDelta { response_id, .. }
                    | QwenRealtimeAudioEvent::OutputTextDone { response_id, .. }
                    | QwenRealtimeAudioEvent::ResponseEnded { response_id, .. } => {
                        Some(response_id)
                    }
                    _ => None,
                })
                .collect();
            assert!(!ids.is_empty());
            assert!(!ids[0].is_empty());
            assert!(ids.iter().all(|id| *id == ids[0]));
            assert_eq!(
                round
                    .iter()
                    .filter(|event| matches!(event, QwenRealtimeAudioEvent::ResponseStarted { .. }))
                    .count(),
                1
            );
            assert!(round.iter().any(|event| matches!(event,
                QwenRealtimeAudioEvent::AudioDelta { pcm16, sample_rate_hz: 24_000, .. }
                    if pcm16 == &expected_pcm
            )));
            assert!(round.iter().any(|event| matches!(event,
                QwenRealtimeAudioEvent::InputTranscriptDone { transcript, .. } if !transcript.is_empty()
            )));
            rounds.push(round);
            if rounds.len() == 1 {
                input.append_pcm16(&[21, 0, 22, 0]).await.unwrap();
            }
        }
        input.finish().await.unwrap();
        rounds
    };
    let (outcome, rounds) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .expect("Seeduplex session should handle tools and two spoken rounds");
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    assert_eq!(rounds.len(), 2);
    app.shutdown().await.unwrap();
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap();

    let mut saw_native_batch = false;
    while let Ok(event) = raw.next_event().await {
        saw_native_batch |= event["type"] == "response.function_call_arguments.done";
    }
    assert!(saw_native_batch);
    let tools = probe.tool_frames.lock().unwrap();
    let texts = probe.text_frames.lock().unwrap();
    let completed = probe.completed_frames.lock().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(texts.len(), 2);
    assert_eq!(tools[0].1, tools[1].1);
    assert_eq!(tools[0].1, texts[0].1);
    assert_ne!(texts[0].1, texts[1].1);
    assert_eq!(&completed[..2], &[texts[0].1, texts[1].1]);
}

#[tokio::test]
async fn proactive_speech_waits_for_the_reply_and_keeps_the_microphone_and_frame_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api/v3/realtime/dialogue",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        let create = receive(&mut socket).await;
        assert_eq!(create["type"], "session.create");
        send(
            &mut socket,
            json!({"type":"session.created", "session":{"id":"notification-session"}}),
        )
        .await;
        let greeting = receive(&mut socket).await;
        assert_eq!(greeting["type"], "speech_text_buffer.commit");
        assert_eq!(greeting["text"], "你好，我已经准备好了。");
        send(
            &mut socket,
            json!({
                "type":"response.output_audio.started", "response_id":"greeting-response",
            }),
        )
        .await;
        send(
            &mut socket,
            json!({
                "type":"response.output_audio.delta", "response_id":"greeting-response",
                "delta":STANDARD.encode([1, 0, 2, 0]),
            }),
        )
        .await;
        assert_eq!(receive_audio(&mut socket).await, [31, 0, 32, 0]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err(),
            "queued notifications must not cancel or overlap an active reply"
        );
        send_reply(
            &mut socket,
            "",
            "greeting-response",
            "你好，我已经准备好了。",
            &[3, 0, 4, 0],
        )
        .await;

        let update = receive(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        assert_ne!(
            update["session"]["instructions"],
            create["session"]["instructions"]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), socket.next())
                .await
                .is_err(),
            "the notification must wait for the following Frame's configuration ACK"
        );
        send(
            &mut socket,
            json!({"type":"session.updated", "session":{"id":"notification-session"}}),
        )
        .await;
        let notification = receive(&mut socket).await;
        assert_eq!(notification["type"], "speech_text_buffer.commit");
        assert_eq!(notification["text"], "你的任务已经完成。");
        send_reply(
            &mut socket,
            "",
            "notification-response",
            "你的任务已经完成。",
            &[5, 0, 6, 0],
        )
        .await;
        // The updated status is delivered through the ordinary third Frame.
        let update = receive(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        send(
            &mut socket,
            json!({"type":"session.updated", "session":{"id":"notification-session"}}),
        )
        .await;
        await_close(&mut socket).await;
    });
    let provider = DoubaoRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 32_768).unwrap(),
        VOICE,
    )
    .unwrap();
    let probe = Probe::default();
    let root_probe = probe.clone();
    let (mut app, input, mut events) = DoubaoRealtimeVoiceApplication::mount(
        move || notification_assistant(root_probe.clone()),
        provider,
    )
    .unwrap();
    assert!(input.speak_text(" ").await.is_err());
    assert!(input.speak_text("x".repeat(65_537)).await.is_err());
    input.speak_text("你好，我已经准备好了。").await.unwrap();
    let operation = async {
        let mut queued = false;
        let mut ended = 0;
        while ended < 2 {
            match events.next_event().await.unwrap() {
                QwenRealtimeAudioEvent::ResponseStarted { response_id }
                    if response_id == "greeting-response" =>
                {
                    input.speak_text("你的任务已经完成。").await.unwrap();
                    input.append_pcm16(&[31, 0, 32, 0]).await.unwrap();
                    queued = true;
                }
                QwenRealtimeAudioEvent::ResponseEnded { status, .. } => {
                    assert_eq!(status, QwenRealtimeResponseStatus::Completed);
                    ended += 1;
                }
                _ => {}
            }
        }
        assert!(queued);
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
    let frames = probe.text_frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_ne!(frames[0].1, frames[1].1);
}

#[tokio::test]
async fn active_interrupt_uses_the_same_response_and_stale_controls_do_not_cancel_again() {
    exercise_interruption(true).await;
}

#[tokio::test]
async fn provider_detected_barge_in_stops_playback_and_drains_until_done() {
    exercise_interruption(false).await;
}

async fn exercise_interruption(explicit: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/api/v3/realtime/dialogue",
        listener.local_addr().unwrap(),
    );
    let server = tokio::spawn(async move {
        let mut socket = accept_session(&listener).await;
        assert_eq!(receive(&mut socket).await["type"], "session.create");
        send(
            &mut socket,
            json!({"type": "session.created", "session": {"id": "dialog_interrupt"}}),
        )
        .await;
        assert_eq!(receive_audio(&mut socket).await, [31, 0, 32, 0]);
        send_input(&mut socket, "input_interrupt", "Please count slowly.").await;
        send(
            &mut socket,
            json!({
                "type": "response.output_audio.started", "question_id": "question_interrupt",
                "response_id": "", "tts_type": "chat_tts_text",
            }),
        )
        .await;
        send(
            &mut socket,
            json!({
                "type": "response.output_audio.delta", "question_id": "question_interrupt",
                "response_id": "wire_interrupt", "delta": STANDARD.encode([5, 0, 6, 0]),
            }),
        )
        .await;
        if explicit {
            assert_eq!(receive(&mut socket).await["type"], "response.cancel");
            send(&mut socket, json!({"type": "response.canceled"})).await;
        } else {
            send(
                &mut socket,
                json!({
                    "type": "conversation.item.input_audio_transcription.started",
                    "item_id": "next-question",
                }),
            )
            .await;
        }
        send(
            &mut socket,
            json!({
                "type": "response.output_audio.delta",
                "delta": STANDARD.encode([99, 0, 99, 0]),
            }),
        )
        .await;
        send(&mut socket, json!({"type": "response.done"})).await;
        // Unchanged configuration needs no session.update on the following
        // Frame; an ended-response interrupt must emit no second cancellation.
        assert_eq!(receive_audio(&mut socket).await, [41, 0, 42, 0]);
        send_reply(
            &mut socket,
            "next",
            "next-response",
            "New reply.",
            &[7, 0, 8, 0],
        )
        .await;
        await_close(&mut socket).await;
    });

    let provider = DoubaoRealtimeProvider::new(
        &endpoint,
        CREDENTIAL,
        ModelSpec::new(MODEL, 32_768).unwrap(),
        VOICE,
    )
    .unwrap();
    let (mut app, input, mut events) = DoubaoRealtimeVoiceApplication::mount(
        || {
            view! {
                #[system_once]
                "Listen continuously and stop speaking when interrupted."
            }
        },
        provider,
    )
    .unwrap();
    let operation = async {
        input.append_pcm16(&[31, 0, 32, 0]).await.unwrap();
        let id = loop {
            if let QwenRealtimeAudioEvent::ResponseStarted { response_id } =
                events.next_event().await.unwrap()
            {
                break response_id;
            }
        };
        assert!(!id.is_empty());
        if explicit {
            assert!(input.interrupt(id.clone()).await.unwrap());
        }
        loop {
            if let QwenRealtimeAudioEvent::ResponseEnded {
                response_id,
                status,
                ..
            } = events.next_event().await.unwrap()
            {
                assert_eq!(response_id, id);
                assert_eq!(status, QwenRealtimeResponseStatus::Cancelled);
                break;
            }
        }
        assert!(!input.interrupt(id).await.unwrap());
        input.append_pcm16(&[41, 0, 42, 0]).await.unwrap();
        loop {
            if let QwenRealtimeAudioEvent::AudioDelta { pcm16, .. } =
                events.next_event().await.unwrap()
            {
                assert_eq!(pcm16, [7, 0, 8, 0]);
                break;
            }
        }
        input.finish().await.unwrap();
    };
    let (outcome, ()) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(app.run(), operation) })
            .await
            .expect("active cancellation should keep the voice session reusable");
    assert_eq!(outcome.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    tokio::time::timeout(DEADLINE, server)
        .await
        .unwrap()
        .unwrap();
}
