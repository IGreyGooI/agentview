//! Capture credential-free Full/Delta delivery for the same notebook as the CLI.
//! Run `cargo run --no-default-features --example command_feedback_frames -- OUTPUT_DIR`.
//! Each frame has exact canonical JSON and its actual submitted POM view.

use std::{path::Path, time::Duration};

use agentview::{
    component::execution::{
        Application, CommandCall, CommandInput, CommandOutcome, CommandSender, DebugFrameSnapshot,
        DebugPromptCapture, DebugProviderPort, FrameBasis,
    },
    pom::{ContentNode, ContentRef, XmlNode},
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use anyhow::{ensure, Context as _};
use serde_json::{json, Value};

#[path = "command_feedback/app.rs"]
mod app;

type Notebook = Application<DebugProviderPort>;

async fn action(
    app: &mut Notebook,
    sender: &CommandSender,
    name: &str,
    input: Value,
) -> anyhow::Result<Value> {
    let (prepared, response) =
        tokio::join!(app.prepare(), sender.submit(CommandCall::new(name, input)));
    ensure!(prepared?.is_continue(), "unexpected application exit");
    match response?.outcome {
        CommandOutcome::Output(output) => Ok(output),
        CommandOutcome::Rejected { code, message } => anyhow::bail!("{code}: {message}"),
    }
}

async fn handoff(
    app: &mut Notebook,
    sender: &CommandSender,
    capture: &DebugPromptCapture,
) -> anyhow::Result<DebugFrameSnapshot> {
    // Observation releases the command barrier without adding another result.
    let (reacted, observed) = tokio::join!(app.react(), sender.observe());
    ensure!(reacted?.is_continue(), "application exited before handoff");
    observed?;
    capture
        .latest_frame()
        .context("DebugProviderPort captured no frame")
}

fn xml_count(node: &XmlNode, name: &str) -> usize {
    usize::from(node.name().as_str() == name)
        + node
            .children()
            .iter()
            .map(|child| match child {
                ContentRef::Node(ContentNode::Xml(child)) => xml_count(child, name),
                _ => 0,
            })
            .sum::<usize>()
}

fn save_frame(
    directory: &Path,
    number: usize,
    snapshot: &DebugFrameSnapshot,
) -> anyhow::Result<Value> {
    let payload: Value = serde_json::from_slice(snapshot.canonical_payload())?;
    let items: Vec<CanonicalInputItem> =
        serde_json::from_value(payload["component"]["projection"]["items"].clone())?;
    let mut roots = Vec::new();
    let mut views = Vec::new();
    let mut feedback_records = 0;
    let mut feedback_patch_operations = 0;
    for item in &items {
        let pom = match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => pom,
            _ => anyhow::bail!("unexpected provider item in notebook frame"),
        };
        views.push(render_pom_document(pom)?);
        for child in pom.children().iter() {
            if let ContentRef::Node(ContentNode::Xml(root)) = child {
                roots.push(root.name().as_str().to_owned());
                if root.name().as_str() == "command_feedback" {
                    feedback_records += xml_count(root, "command_result");
                    feedback_patch_operations += ["insert", "remove", "update", "replace"]
                        .iter()
                        .map(|name| xml_count(root, name))
                        .sum::<usize>();
                }
            }
        }
    }
    let view = views.join("\n\n");
    let stem = format!("frame-{number:02}");
    std::fs::write(
        directory.join(format!("{stem}.json")),
        snapshot.canonical_payload(),
    )?;
    std::fs::write(directory.join(format!("{stem}.view.txt")), &view)?;
    Ok(json!({
        "frame": number,
        "basis": match snapshot.basis() { FrameBasis::Full => "full", FrameBasis::DeltaFrom(_) => "delta" },
        "roots": roots,
        "projection_items": items.len(),
        "feedback_records": feedback_records,
        "feedback_patch_operations": feedback_patch_operations,
        "frame_bytes": snapshot.canonical_payload().len(),
        "view_bytes": view.len(),
    }))
}

async fn capture_frames(
    app: &mut Notebook,
    sender: &CommandSender,
    capture: &DebugPromptCapture,
    directory: &Path,
) -> anyhow::Result<()> {
    let first_note = "Keep complete action results.";
    ensure!(
        action(app, sender, "record", json!({"text": first_note})).await?
            == json!({"count": 1, "latest": first_note})
    );
    ensure!(
        action(app, sender, "inspect", json!({})).await?
            == json!({"count": 1, "latest": first_note})
    );
    ensure!(
        capture.frame_snapshots().is_empty(),
        "prepare must not submit a Frame"
    );
    let first = handoff(app, sender, capture).await?;
    let unchanged = handoff(app, sender, capture).await?;
    ensure!(
        action(app, sender, "inspect", json!({})).await?
            == json!({"count": 1, "latest": first_note})
    );
    let new_feedback = handoff(app, sender, capture).await?;
    let second_note = "Keep static context separate from changing state.";
    ensure!(
        action(app, sender, "record", json!({"text": second_note})).await?
            == json!({"count": 2, "latest": second_note})
    );
    let changed_state = handoff(app, sender, capture).await?;

    let reports = [&first, &unchanged, &new_feedback, &changed_state]
        .iter()
        .enumerate()
        .map(|(index, frame)| save_frame(directory, index + 1, frame))
        .collect::<anyhow::Result<Vec<_>>>()?;
    ensure!(reports[0]["basis"] == "full" && reports[0]["feedback_records"] == 2);
    ensure!(reports[1]["basis"] == "delta" && reports[1]["projection_items"] == 0);
    ensure!(
        reports[2]["roots"] == json!(["command_feedback"]) && reports[2]["feedback_records"] == 1
    );
    ensure!(
        reports[3]["roots"] == json!(["notebook_state", "command_feedback"])
            && reports[3]["feedback_records"] == 1
    );
    ensure!(reports
        .iter()
        .all(|report| report["feedback_patch_operations"] == 0));
    let summary = json!({
        "delivery_scope": "One mounted Application and one DebugProviderPort; no model I/O or shared reader cursor.",
        "steps": ["record + inspect, then Full handoff", "unchanged Delta", "inspect, then complete new feedback", "record, then changed state and complete new feedback"],
        "frames": reports,
    });
    std::fs::write(
        directory.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "target/command-feedback-frames".into());
    std::fs::create_dir_all(&directory)?;
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(16);
    let mut app = Application::mount_with_commands(app::notebook, port, input)?;
    let operation = tokio::time::timeout(
        Duration::from_secs(10),
        capture_frames(&mut app, &sender, &capture, &directory),
    )
    .await
    .context("frame example exceeded ten seconds")
    .and_then(|result| result);
    let shutdown = tokio::time::timeout(Duration::from_secs(5), app.shutdown()).await;
    operation?;
    shutdown.context("application shutdown exceeded five seconds")??;
    Ok(())
}
