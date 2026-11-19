//! Two spoken turns and a Component tool on one Qwen Realtime connection.

use std::{collections::HashMap, io::Write, path::Path, time::Duration};

use agentview::{
    component::{execution::ReactionOutcome, prelude::*},
    provider::{
        qwen_realtime::{
            QwenRealtimeAudioEvent, QwenRealtimeMediaEvents, QwenRealtimeProvider,
            QwenRealtimeResponseStatus, QwenRealtimeServerEvents, QwenRealtimeVoiceApplication,
            QwenRealtimeVoiceInput, QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ,
        },
        ModelSpec,
    },
};
use anyhow::Context;

const LOOKUP: &[u8] = include_bytes!("data/qwen_realtime/lookup-widget.pcm");
const FOLLOW_UP: &[u8] = include_bytes!("data/qwen_realtime/stock-plus-one.pcm");
const OUTPUT: &str = "target/qwen-realtime";

#[component]
fn stock_assistant() -> Component {
    let stock = use_signal(|| None::<u32>);
    let result = stock
        .with(|stock| match stock {
            Some(quantity) => format!("{quantity} widgets in stock"),
            None => "Stock has not been checked yet".to_owned(),
        })
        .expect("mounted stock state");

    view! {
        #[system_once]
        "You are an inventory assistant. When asked for stock, call get_stock before answering.          Use its result to answer in Chinese. Use the confirmed number in follow-up questions."

        inventory { "{result}" }

        NativeToolCall {
            name: "get_stock",
            description: "Look up the available quantity of widgets.",
            on_call: move |_context| stock.update(|stock| {
                let quantity = 7;
                *stock = Some(quantity);
                println!("get_stock returned {quantity}");
                quantity
            }),
        }
    }
}

async fn send_utterance(input: &QwenRealtimeVoiceInput, pcm: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !pcm.is_empty() && pcm.len() % 2 == 0,
        "input must be 16 kHz mono PCM16"
    );
    let mut pacing = tokio::time::interval(Duration::from_millis(100));
    pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let silence = [0; 3_200];
    // A real microphone continues streaming during the silence after speech.
    for chunk in pcm
        .chunks(3_200)
        .chain(std::iter::repeat_n(&silence[..], 40))
    {
        pacing.tick().await;
        input.append_pcm16(chunk).await?;
    }
    Ok(())
}

#[derive(Default)]
struct SpokenReply {
    pcm: Vec<u8>,
    transcript: String,
    completed: bool,
}

async fn receive_reply(
    events: &mut QwenRealtimeMediaEvents,
    server: &mut QwenRealtimeServerEvents,
) -> anyhow::Result<SpokenReply> {
    let mut replies = HashMap::<String, SpokenReply>::new();
    let mut answer_id = None::<String>;
    let mut user_transcribed = false;

    loop {
        if let Some(id) = &answer_id {
            if user_transcribed && replies.get(id).is_some_and(|reply| reply.completed) {
                let reply = replies.remove(id).expect("completed reply");
                anyhow::ensure!(!reply.pcm.is_empty(), "response contained no audio");
                return Ok(reply);
            }
        }

        tokio::select! {
            event = events.next_event() => match event? {
                QwenRealtimeAudioEvent::InputTranscriptDone { transcript, .. } => {
                    println!("You: {transcript}");
                    user_transcribed = true;
                }
                QwenRealtimeAudioEvent::InputTranscriptFailed { error, .. } => {
                    return Err(error.into());
                }
                QwenRealtimeAudioEvent::AudioDelta { response_id, pcm16, .. } => {
                    replies.entry(response_id).or_default().pcm.extend(pcm16);
                }
                QwenRealtimeAudioEvent::OutputTranscriptDone { response_id, transcript, .. } => {
                    replies.entry(response_id).or_default().transcript = transcript;
                }
                QwenRealtimeAudioEvent::ResponseEnded { response_id, status, error } => {
                    if let Some(error) = error {
                        return Err(error.into());
                    }
                    anyhow::ensure!(status == QwenRealtimeResponseStatus::Completed, "response ended with {status:?}");
                    replies.entry(response_id).or_default().completed = true;
                }
                _ => {}
            },
            event = server.next_event(), if answer_id.is_none() => {
                let event = event?;
                // A response containing a tool call will get another response after its result.
                if event["type"] == "response.done" && event["response"]["output"].as_array().is_some_and(|items| {
                    items.iter().any(|item| item["type"] == "message")
                        && items.iter().all(|item| item["type"] != "function_call")
                }) {
                    answer_id = Some(event["response"]["id"].as_str().context("response ID is missing")?.to_owned());
                }
            }
        }
    }
}

fn write_wav(path: &Path, pcm: &[u8]) -> anyhow::Result<()> {
    let data_size = u32::try_from(pcm.len())?;
    let riff_size = data_size.checked_add(36).context("WAV is too large")?;
    let rate = QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ;
    let mut file = std::fs::File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&[1, 0, 1, 0])?; // PCM, one channel
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * 2).to_le_bytes())?;
    file.write_all(&[2, 0, 16, 0])?; // Two bytes per frame, 16 bits per sample
    file.write_all(b"data")?;
    file.write_all(&data_size.to_le_bytes())?;
    file.write_all(pcm)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let key = std::env::var("QWEN_REALTIME_API_KEY").context("Set QWEN_REALTIME_API_KEY")?;
    let endpoint = std::env::var("QWEN_REALTIME_URL")
        .unwrap_or_else(|_| "wss://dashscope.aliyuncs.com/api-ws/v1/realtime".to_owned());
    let tokens = std::env::var("AGENTVIEW_CONTEXT_WINDOW_TOKENS")
        .map_or(Ok(262_144), |value| value.parse())
        .context("AGENTVIEW_CONTEXT_WINDOW_TOKENS must be a positive integer")?;
    let model = ModelSpec::new("qwen-audio-3.1-realtime-plus", tokens)?;
    let provider = QwenRealtimeProvider::new(&endpoint, &key, model)?;
    let (mut app, input, mut events) =
        QwenRealtimeVoiceApplication::mount(stock_assistant, provider)?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let utterances = match args.as_slice() {
        [] => [LOOKUP.to_vec(), FOLLOW_UP.to_vec()],
        [first, second] => [
            tokio::fs::read(first).await?,
            tokio::fs::read(second).await?,
        ],
        _ => anyhow::bail!("usage: qwen_realtime_live [first.pcm second.pcm]"),
    };

    std::fs::create_dir_all(OUTPUT)?;
    let mut server = events.subscribe_server_events();
    let conversation = async {
        for (index, pcm) in utterances.iter().enumerate() {
            let (_, reply) = tokio::try_join!(
                send_utterance(&input, pcm),
                receive_reply(&mut events, &mut server)
            )?;

            let path = Path::new(OUTPUT).join(format!("reply-{}.wav", index + 1));
            write_wav(&path, &reply.pcm)?;
            println!("Assistant: {}", reply.transcript);
            println!("Audio saved to {}", path.display());
        }
        input.finish().await?;
        Ok::<_, anyhow::Error>(())
    };
    let drive = async {
        anyhow::ensure!(
            app.run().await? == ReactionOutcome::Completed,
            "Realtime session ended before input drained"
        );
        Ok::<_, anyhow::Error>(())
    };
    let operation = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::try_join!(drive, conversation)
    })
    .await;
    let shutdown = app.shutdown().await;
    operation.context("Timed out waiting for Realtime replies")??;
    shutdown?;
    Ok(())
}
