//! Provider text completion updates a Signal that is sent in the next Frame.

use std::fmt;

use agentview::component::{
    execution::{Application, ReactionPort},
    prelude::*,
};
#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Clone)]
struct ReviewProps {
    document_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewStatus {
    Pending,
    Approved,
    ChangesRequested,
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("pending"),
            Self::Approved => formatter.write_str("approved"),
            Self::ChangesRequested => formatter.write_str("changes-requested"),
        }
    }
}

fn completed_status(text: &str) -> ReviewStatus {
    if text.trim().eq_ignore_ascii_case("approved") {
        ReviewStatus::Approved
    } else {
        ReviewStatus::ChangesRequested
    }
}

#[component]
fn review_policy() -> Component {
    let prompt = r#"## Review policy

Return exactly one token: approved or changes-requested."#;
    view! {
        #[system_once]
        { prompt }
    }
}

#[component]
fn document_request(document_id: String) -> Component {
    view! {
        document_request { document_id { "{document_id}" } }
    }
}

#[component]
fn review_state(status: Signal<ReviewStatus>) -> Component {
    let current = status.with(|value| *value).expect("mounted review Signal");
    view! {
        review_status { "{current}" }
    }
}

#[component]
fn review_turn(props: ReviewProps) -> Component {
    let status = use_signal(|| ReviewStatus::Pending);
    let application_exit = use_application_exit();
    let submitted_status = status
        .with(|status| *status)
        .expect("mounted review status");
    let handler_status = status.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let status = handler_status.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                println!("{text}");
                status.set(completed_status(&text))?;
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    use_reaction_completion(move || async move {
        if submitted_status != ReviewStatus::Pending {
            application_exit.request(ExitReason::Completed)?;
        }
        Ok::<(), anyhow::Error>(())
    });

    view! {
        review_policy()
        document_request(props.document_id)
        review_state(status)
    }
}

#[component]
fn review_application(props: ReviewProps) -> Component {
    view! {
        review_turn(props)
    }
}

async fn run_review(provider: impl ReactionPort) -> anyhow::Result<()> {
    let props = ReviewProps {
        document_id: "proposal-7".to_owned(),
    };
    let mut application = Application::mount(move || review_application(props.clone()), provider)?;
    let operation = application.run().await.map_err(anyhow::Error::from);
    let shutdown = application.shutdown().await.map_err(anyhow::Error::from);

    match (operation, shutdown) {
        (Ok(ExitReason::Completed), Ok(())) => Ok(()),
        (Ok(reason), Ok(())) => anyhow::bail!("review agent exited unexpectedly: {reason:?}"),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Err(operation), Err(shutdown)) => {
            Err(operation.context(format!("review agent shutdown also failed: {shutdown:#}")))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_review(live_provider::from_env("signal-reaction")?).await
}

#[cfg(test)]
#[path = "../tests/examples/signal_reaction.rs"]
mod tests;
