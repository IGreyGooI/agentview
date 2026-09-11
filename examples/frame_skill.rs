//! A typed skill whose parent is a live OpenAI Responses agent.
//!
//! Set `OPENAI_API_KEY`, then run `cargo run --example frame_skill`.

use std::sync::{Arc, Mutex};

use agentview::component::{
    execution::{Application, ExternalAct, ExternalApplication, ReactionPort},
    prelude::*,
};
use anyhow::{Context, Result};

#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Clone)]
struct SkillProps {
    exported: Arc<Mutex<Option<Signal<String>>>>,
}

#[component]
fn frame_skill_component(props: SkillProps) -> Component {
    let request = use_signal(|| String::from("No parent request has been assigned yet."));
    *props.exported.lock().expect("Skill export lock") = Some(request.clone());

    let response = use_signal(|| None::<String>);
    let received = response.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let received = received.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                received.set(Some(text))?;
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    let request = request.with(Clone::clone).expect("mounted Skill request");
    let response = response
        .with(Clone::clone)
        .expect("mounted Skill response")
        .map(|text| view! { skill_response { "{text}" } })
        .unwrap_or_else(|| view! {});
    let prompt = format!("## Skill request\n\n{request}");
    view! {
        #[developer]
        { prompt }
        { response }
    }
}

fn run_typed_command(signal: &Signal<String>, value: &str) -> Result<()> {
    signal.set(value.to_owned())?;
    Ok(())
}

#[derive(Clone)]
struct ParentProps {
    delegated_frame: String,
    response: Arc<Mutex<Option<String>>>,
}

#[component]
fn response_parent(props: ParentProps) -> Component {
    let response = Arc::clone(&props.response);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let response = Arc::clone(&response);
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                *response
                    .lock()
                    .map_err(|_| anyhow::anyhow!("parent response lock poisoned"))? = Some(text);
            }
            Ok::<(), anyhow::Error>(())
        }
    });

    let exit = use_application_exit();
    use_reaction_completion(move || async move { exit.request(ExitReason::Completed) });

    let delegated_frame = props.delegated_frame;
    let prompt = r#"## Parent instructions

You are the parent agent for a delegated frame skill. Answer the skill request concisely."#;
    view! {
        #[system_once]
        { prompt }
        delegated_skill_frame { "{delegated_frame}" }
    }
}

async fn run_parent_agent(provider: impl ReactionPort, delegated_frame: String) -> Result<String> {
    let response = Arc::new(Mutex::new(None));
    let props = ParentProps {
        delegated_frame,
        response: Arc::clone(&response),
    };
    let mut application = Application::mount(move || response_parent(props.clone()), provider)?;

    let result = application.run().await;
    let shutdown = application.shutdown().await;
    result.map_err(|operation| match &shutdown {
        Ok(()) => anyhow::Error::from(operation),
        Err(shutdown) => anyhow::Error::from(operation)
            .context(format!("parent agent shutdown also failed: {shutdown}")),
    })?;
    shutdown?;
    let response = response
        .lock()
        .map_err(|_| anyhow::anyhow!("parent response lock poisoned"))?;
    let response = response.clone().context("parent model returned no text")?;
    Ok(response)
}

async fn run_skill() -> Result<String> {
    let provider = live_provider::from_env("frame-skill-parent")?;
    let exported = Arc::new(Mutex::new(None));
    let props = SkillProps {
        exported: Arc::clone(&exported),
    };
    let mut skill = ExternalApplication::new_root(move || frame_skill_component(props.clone()))?;

    let operation = async {
        let command = exported
            .lock()
            .map_err(|_| anyhow::anyhow!("Skill export lock poisoned"))?
            .clone()
            .context("Skill did not export its typed command state")?;
        run_typed_command(&command, "Say hello from the frame skill.")?;

        let observation = skill.observe().await?;
        let response = run_parent_agent(provider, observation.content().to_owned()).await?;
        let _updated = skill.act(ExternalAct::text(response.clone())).await?;
        Ok::<_, anyhow::Error>(response)
    }
    .await;
    let shutdown = skill.shutdown().await;
    let response = operation.map_err(|operation| match &shutdown {
        Ok(()) => operation,
        Err(shutdown) => operation.context(format!("Skill shutdown also failed: {shutdown}")),
    })?;
    shutdown?;
    Ok(response)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("{}", run_skill().await?);
    Ok(())
}
