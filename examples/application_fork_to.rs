//! Start a child on a different port type from an explicit business snapshot.
//! Debug ports record Frames without model I/O or model-generated actions.
//! Run `cargo run --no-default-features --example application_fork_to`.

use agentview::component::{
    execution::{
        Application, DebugProviderPort, Frame, FrameBasis, ProviderFactStream, ReactionPort,
        ReactionPortFault, SubmitFault, TargetDeclaration,
    },
    prelude::*,
};
use anyhow::{ensure, Context as _};
use async_trait::async_trait;

#[derive(Clone)]
struct BusinessSnapshot {
    total: i64,
}

#[component]
fn counter(snapshot: BusinessSnapshot) -> Component {
    let state = use_signal(move || snapshot);
    let total = state.with(|state| state.total).expect("mounted counter");
    view! {
        #[system_once]
        "Observe the current counter."
        counter { value: total, }
    }
}

// The generic child has a different Rust port type. Its inner Debug port has
// its own fresh target identity and records the child's exact Frames.
struct OtherPort(DebugProviderPort);

#[async_trait]
impl ReactionPort for OtherPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.0.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.0.submit(frame).await
    }

    async fn shutdown(&mut self) {
        self.0.shutdown().await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The business owner supplies the snapshot. No actions mutate it in this example.
    let business = BusinessSnapshot { total: 7 };
    let parent_snapshot = business.clone();
    let (source_port, source_capture) = DebugProviderPort::new();
    let mut parent = Application::mount(move || counter(parent_snapshot.clone()), source_port)?;

    let result = async {
        ensure!(parent.react(false)?.await?.is_continue(), "source exited");

        let snapshot = business.clone();
        let (target_port, child_capture) = DebugProviderPort::new();
        let mut child = parent
            .fork_to(move || counter(snapshot.clone()), OtherPort(target_port))
            .await?;
        let submitted_during_fork = !child_capture.frame_snapshots().is_empty();

        let child_turn = child.react(false)?.await;
        let child_cleanup = child.shutdown().await;
        ensure!(child_turn?.is_continue(), "child exited");
        child_cleanup?;

        ensure!(!submitted_during_fork, "fork_to submitted a Frame");
        let first = child_capture
            .latest_frame()
            .context("child Frame was not recorded")?;
        let payload: serde_json::Value = serde_json::from_slice(first.canonical_payload())?;
        let replay = payload["replay"]
            .as_array()
            .context("missing replay array")?;
        ensure!(
            first.basis() == FrameBasis::Full,
            "child did not start with Full"
        );
        ensure!(replay.is_empty(), "child inherited source history");
        ensure!(
            source_capture.frame_snapshots().len() == 1,
            "source was driven during fork_to"
        );

        println!(
            "Source delivered one counter view (total = {}).",
            business.total
        );
        println!(
            "Child on OtherPort delivered its first Full; replay items = {}.",
            replay.len()
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let parent_cleanup = parent.shutdown().await;
    result?;
    parent_cleanup?;
    Ok(())
}
