//! A two-turn agent that publishes a Signal update as the second Frame.

use std::convert::Infallible;

use agentview::component::{
    execution::{Application, ReactionPort},
    prelude::*,
};
#[path = "support/live_provider.rs"]
mod live_provider;

#[component]
fn frame_agent_component() -> Component {
    let state = use_signal(|| String::from("initial"));
    let application_exit = use_application_exit();

    use_provider_event_handler(ProviderEvent::TEXT, |event| async move {
        if let TextTurnEvent::TextComplete(text) = event {
            println!("{text}");
        }
        Ok::<(), Infallible>(())
    });

    let completed_state = state.clone();
    use_reaction_completion(move || async move {
        if completed_state.with(|value| value == "initial")? {
            completed_state.set(String::from("published"))?;
        } else {
            application_exit.request(ExitReason::Completed)?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let value = state.with(Clone::clone).expect("mounted Signal read");
    let prompt = r#"## Publication policy

State the publication status in one concise sentence."#;
    view! {
        #[system_once]
        { prompt }
        publication_status { "{value}" }
    }
}

async fn run_agent(provider: impl ReactionPort) -> anyhow::Result<()> {
    let mut application = Application::mount(frame_agent_component, provider)?;
    let operation = application.run().await.map_err(anyhow::Error::from);
    let shutdown = application.shutdown().await.map_err(anyhow::Error::from);

    match (operation, shutdown) {
        (Ok(ExitReason::Completed), Ok(())) => Ok(()),
        (Ok(reason), Ok(())) => anyhow::bail!("frame agent exited unexpectedly: {reason:?}"),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Err(operation), Err(shutdown)) => {
            Err(operation.context(format!("frame agent shutdown also failed: {shutdown:#}")))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_agent(live_provider::from_env("frame-agent")?).await
}

#[cfg(test)]
#[path = "../tests/examples/frame_agent.rs"]
mod tests;
