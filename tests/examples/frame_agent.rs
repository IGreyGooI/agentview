#[allow(dead_code)]
#[path = "../support/scripted_provider.rs"]
mod scripted_provider;

use agentview::component::execution::FrameBasis;
use scripted_provider::{ScriptedProvider, ScriptedReaction};

use super::*;

#[tokio::test]
async fn publishes_a_delta_frame_before_normal_exit() -> anyhow::Result<()> {
    let (provider, capture) = ScriptedProvider::new([
        ScriptedReaction::text(["The publication is initial."]),
        ScriptedReaction::text(["The publication is published."]),
    ])?;

    run_agent(provider).await?;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    assert!(frames[0].text.contains("<publication_status>initial"));
    assert!(frames[1].text.contains("<publication_status>published"));
    Ok(())
}
