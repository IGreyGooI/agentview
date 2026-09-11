//! A support agent whose Responses input is composed by several business Components.

use std::convert::Infallible;

use agentview::component::{
    execution::{Application, ReactionPort},
    prelude::*,
};
#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Clone)]
struct SupportCase {
    account_id: String,
    plan: String,
    request: String,
}

#[component]
fn support_policy() -> Component {
    let prompt = r#"## Support policy

Resolve the request using only the supplied account context."#;
    view! {
        #[system_once]
        { prompt }
    }
}

#[component]
fn account_context(account_id: String, plan: String) -> Component {
    view! {
        #[developer]
        account_context {
            account_id { "{account_id}" }
            plan { "{plan}" }
        }
    }
}

#[component]
fn customer_request(request: String) -> Component {
    let prompt = format!("## Customer request\n\n{request}");
    view! {
        { prompt }
    }
}

#[component]
fn response_requirements() -> Component {
    let prompt = r#"## Response requirements

Return a concise answer and the next action."#;
    view! {
        { prompt }
    }
}

#[component]
fn support_application(props: SupportCase) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, |event| async move {
        if let TextTurnEvent::TextComplete(text) = event {
            println!("{text}");
        }
        Ok::<(), Infallible>(())
    });

    let exit = use_application_exit();
    use_reaction_completion(move || async move { exit.request(ExitReason::Completed) });

    view! {
        support_policy()
        account_context(props.account_id, props.plan)
        customer_request(props.request)
        response_requirements()
    }
}

fn support_case() -> SupportCase {
    SupportCase {
        account_id: "acct-1042".to_owned(),
        plan: "team".to_owned(),
        request: "Explain why yesterday's export is unavailable.".to_owned(),
    }
}

async fn run_support_agent(provider: impl ReactionPort) -> anyhow::Result<()> {
    let props = support_case();
    let mut application = Application::mount(move || support_application(props.clone()), provider)?;
    let operation = application.run().await.map_err(anyhow::Error::from);
    let shutdown = application.shutdown().await.map_err(anyhow::Error::from);

    match (operation, shutdown) {
        (Ok(ExitReason::Completed), Ok(())) => Ok(()),
        (Ok(reason), Ok(())) => anyhow::bail!("support agent exited unexpectedly: {reason:?}"),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Err(operation), Err(shutdown)) => {
            Err(operation.context(format!("support agent shutdown also failed: {shutdown:#}")))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_support_agent(live_provider::from_env("component-composition")?).await
}
