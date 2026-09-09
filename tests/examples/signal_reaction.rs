#[allow(dead_code)]
#[path = "../support/scripted_provider.rs"]
mod scripted_provider;

use agentview::component::execution::FrameBasis;
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
    assert!(frames[0].text.contains("<review_status>pending"));
    assert!(frames[1].text.contains("<review_status>approved"));
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
    assert!(frames[1].text.contains("<review_status>changes-requested"));
    Ok(())
}
