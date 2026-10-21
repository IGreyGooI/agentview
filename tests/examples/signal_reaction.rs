#[allow(dead_code)]
#[path = "../support/scripted_provider.rs"]
mod scripted_provider;

use agentview::component::execution::FrameBasis;
use agentview::transcript::{CanonicalInputItem, ConversationRole};
use scripted_provider::pom::{self, InspectPom};
use scripted_provider::{ScriptedProvider, ScriptedReaction};

use super::*;

#[tokio::test]
async fn completed_review_is_published_in_the_next_frame() -> anyhow::Result<()> {
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text(["app", "roved"]),
        ScriptedReaction::empty(),
    ])?;

    run_review(provider).await?;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    assert!(frames[0].document().has_field("review_status", "pending"));
    assert!(frames[0].document().has_element("document_request"));
    assert!(matches!(
        frames[1].projection.as_slice(),
        [CanonicalInputItem::Message {
            role: ConversationRole::User,
            pom,
        }] if pom == &pom::text_element("review_status", "approved")
    ));
    assert_eq!(
        frames[1].replay,
        [CanonicalInputItem::assistant_text("approved", None)],
        "the model reply belongs to history, separate from the current review state"
    );
    Ok(())
}

#[tokio::test]
async fn non_approved_completion_publishes_changes_requested() -> anyhow::Result<()> {
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::sealed("needs more evidence"),
        ScriptedReaction::empty(),
    ])?;

    run_review(provider).await?;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert!(frames[1]
        .document()
        .has_field("review_status", "changes-requested"));
    Ok(())
}
