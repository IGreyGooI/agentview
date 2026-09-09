//! A one-turn agent that says Hello World through the OpenAI Responses API.
//!
//! Set `OPENAI_API_KEY`, then run `cargo run --example hello_world`.

use std::convert::Infallible;

use agentview::component::{
    execution::{Application, ReactionPort},
    prelude::*,
};
#[path = "support/live_provider.rs"]
mod live_provider;

#[component]
fn hello_agent() -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, |event| async move {
        if let TextTurnEvent::TextComplete(text) = event {
            println!("{text}");
        }
        Ok::<(), Infallible>(())
    });

    let exit = use_application_exit();
    use_reaction_completion(move || async move { exit.request(ExitReason::Completed) });

    view! {
        greeting_request { "Say exactly: Hello World" }
    }
}

async fn run_agent(provider: impl ReactionPort) -> anyhow::Result<()> {
    let mut application = Application::mount(hello_agent, provider)?;
    let operation = application.run().await.map_err(anyhow::Error::from);
    let shutdown = application.shutdown().await.map_err(anyhow::Error::from);

    match (operation, shutdown) {
        (Ok(ExitReason::Completed), Ok(())) => Ok(()),
        (Ok(reason), Ok(())) => anyhow::bail!("hello agent exited unexpectedly: {reason:?}"),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Err(operation), Err(shutdown)) => {
            Err(operation.context(format!("hello agent shutdown also failed: {shutdown:#}")))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_agent(live_provider::from_env("hello-world")?).await
}
