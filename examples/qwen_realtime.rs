//! Interactive text using an Alibaba Cloud Realtime session.

use std::io::Write;

use agentview::{
    component::{
        execution::{Application, ReactionOutcome},
        prelude::*,
    },
    provider::{qwen_realtime::QwenRealtimeProvider, ModelSpec},
};
use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

#[component]
fn conversation(input: watch::Receiver<String>) -> Component {
    let message = use_signal(String::new);
    let prepared = message.clone();
    use_preparation(move || async move { prepared.set(input.borrow().clone()) });

    use_provider_event_handler(ProviderEvent::TEXT_DELTA, |_, delta| async move {
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "{delta}")?;
        stdout.flush()?;
        Ok::<(), std::io::Error>(())
    });
    let message = message
        .with(Clone::clone)
        .expect("active conversation input");

    view! {
        #[system_once]
        "Answer in the same language as the user."

        #[user(repeat)]
        { message }
    }
}

async fn text_chat(provider: QwenRealtimeProvider) -> anyhow::Result<()> {
    let (input, root_input) = watch::channel(String::new());
    let mut application = Application::mount(move || conversation(root_input.clone()), provider)?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let result = async {
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            input.send_replace(line);
            anyhow::ensure!(
                application.react(false)?.await? == ReactionOutcome::Completed,
                "Realtime reaction ended early"
            );
            println!();
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let shutdown = application.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key =
        std::env::var("QWEN_REALTIME_API_KEY").context("QWEN_REALTIME_API_KEY must be set")?;
    let endpoint = std::env::var("QWEN_REALTIME_URL").context("QWEN_REALTIME_URL must be set")?;
    let context_tokens: u64 = std::env::var("AGENTVIEW_CONTEXT_WINDOW_TOKENS")?
        .parse()
        .context("AGENTVIEW_CONTEXT_WINDOW_TOKENS must be a positive integer")?;
    let model = ModelSpec::new("qwen-audio-3.1-realtime-plus", context_tokens)?;
    let provider = QwenRealtimeProvider::new(&endpoint, &api_key, model)?;

    text_chat(provider).await
}
