//! Provider events update a retained Signal for the next explicit reaction.

use std::fmt;

use agentview::component::{
    execution::{ApplicationHost, ProviderEvent, RenderedProjection},
    prelude::*,
    ComponentHost,
};

#[path = "support/scripted_provider.rs"]
mod scripted_provider;

use scripted_provider::ScriptedProvider;

#[derive(Clone)]
struct ReviewProps {
    document_id: String,
}

#[derive(Clone, Copy)]
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

fn completed_status(event: TextTurnEvent) -> Option<ReviewStatus> {
    let TextTurnEvent::TextComplete(text) = event else {
        return None;
    };
    Some(match text.trim() {
        "approved" => ReviewStatus::Approved,
        _ => ReviewStatus::ChangesRequested,
    })
}

#[component]
fn review_policy() -> Component {
    view! {
        #[system_once]
        review_policy { "Return either approved or changes-requested." }
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
fn review_response_consumer(
    status: Signal<ReviewStatus>,
    text: EventInput<TextTurnEvent>,
) -> Component {
    view! {
        {
            EventListener::observe("example.review.status", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let status = status.clone();
                    async move {
                        let Some(completed) = completed_status(event) else {
                            return Ok(());
                        };
                        status.set(completed)
                    }
                })
        }
    }
}

#[component]
fn review_turn(props: ReviewProps, events: EventInput<ProviderEvent>) -> Component {
    let status = use_signal(|| ReviewStatus::Pending);
    let text = events.select(ProviderEvent::TEXT);

    view! {
        review_policy()
        document_request(props.document_id)
        review_state(status.clone())
        review_response_consumer(status, text)
    }
}

#[component]
fn review_application(props: ReviewProps, events: EventInput<ProviderEvent>) -> Component {
    view! {
        review_turn(props, events)
    }
}

async fn run_reactions_with(
    first_reaction: Vec<ProviderEvent>,
) -> anyhow::Result<Vec<RenderedProjection>> {
    let (provider, capture) = ScriptedProvider::new([first_reaction, Vec::new()]);
    let mut components = ComponentHost::new(
        review_application,
        ReviewProps {
            document_id: "proposal-7".to_owned(),
        },
    );
    let mut application = ApplicationHost::new(provider);

    application.dispatch_llm_reaction(&mut components).await?;
    application.dispatch_llm_reaction(&mut components).await?;
    Ok(capture.snapshots())
}

async fn run_reactions() -> anyhow::Result<Vec<RenderedProjection>> {
    run_reactions_with(vec![ProviderEvent::Text(TextTurnEvent::TextComplete(
        "approved".to_owned(),
    ))])
    .await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let projections = run_reactions().await?;
    println!("reactions={}", projections.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use agentview::{pom_renderer::render_pom_document, transcript::CanonicalInputItem};

    use super::*;

    fn projection_text(projection: &RenderedProjection) -> String {
        projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => {
                    Some(render_pom_document(pom).expect("POM renders"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn provider_feedback_appears_only_after_the_next_explicit_reaction() {
        let projections = run_reactions().await.expect("scripted reactions complete");

        assert_eq!(projections.len(), 2);
        assert!(projection_text(&projections[0]).contains("<review_status>pending</review_status>"));
        assert!(
            projection_text(&projections[1]).contains("<review_status>approved</review_status>")
        );
    }

    #[tokio::test]
    async fn deltas_do_not_publish_a_terminal_review_decision() {
        let projections = run_reactions_with(vec![
            ProviderEvent::Text(TextTurnEvent::TextDelta("changes-".to_owned())),
            ProviderEvent::Text(TextTurnEvent::TextComplete("approved".to_owned())),
        ])
        .await
        .expect("streamed review reaction completes");

        assert!(
            projection_text(&projections[1]).contains("<review_status>approved</review_status>")
        );
    }

    #[tokio::test]
    async fn invalid_completed_decision_requests_changes() {
        let projections = run_reactions_with(vec![ProviderEvent::Text(
            TextTurnEvent::TextComplete("undecided".to_owned()),
        )])
        .await
        .expect("invalid completed review reaction completes");

        assert!(projection_text(&projections[1])
            .contains("<review_status>changes-requested</review_status>"));
    }
}
