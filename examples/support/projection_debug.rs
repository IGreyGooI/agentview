//! Inspection of the Component's current content requirements before a reaction.

use agentview::{
    component::execution::ProjectionSnapshot, pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use anyhow::Context as _;
use serde_json::{json, Value};
use std::{fmt::Write as _, fs, path::Path};
/// Writes the Component's complete current declaration after preparation.
/// Previously delivered content can remain in provider history after leaving this snapshot.
pub fn write_projection(
    run_dir: &Path,
    reaction: usize,
    snapshot: ProjectionSnapshot<'_>,
) -> anyhow::Result<()> {
    let reaction_dir = run_dir.join(format!("{reaction:04}"));
    fs::create_dir_all(&reaction_dir)
        .with_context(|| format!("create projection directory {}", reaction_dir.display()))?;
    let projection_json = projection_json(&snapshot)?;
    let json_text = serde_json::to_string_pretty(&projection_json)
        .context("serialize component projection JSON")?;
    let projection_text = render_projection_text(reaction, &snapshot)?;
    let json_path = reaction_dir.join("projection.json");
    let text_path = reaction_dir.join("projection.txt");
    fs::write(&json_path, json_text)
        .with_context(|| format!("write projection JSON {}", json_path.display()))?;
    fs::write(&text_path, &projection_text)
        .with_context(|| format!("write projection text {}", text_path.display()))?;

    eprintln!(
        "[agentview-debug] component projection before reaction {reaction:04}\n  projection.json: {}\n  projection.txt: {}\n\n{projection_text}",
        json_path.display(),
        text_path.display(),
    );
    Ok(())
}
fn projection_json(snapshot: &ProjectionSnapshot<'_>) -> anyhow::Result<Value> {
    let projection = snapshot.projection();
    let nodes = projection
        .nodes()
        .iter()
        .map(|node| {
            let items = serde_json::to_value(node.items())
                .with_context(|| format!("serialize items for node {}", node.identity()))?;
            let diffs = node
                .diffs()
                .iter()
                .map(|marker| {
                    json!({
                        "item_index": marker.item_index(),
                        "structural_path": marker.structural_path(),
                        "slot": marker.slot(),
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "identity": node.identity(),
                "items": items,
                "diffs": diffs,
            }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let native_tools = projection
        .native_tools()
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.input_schema(),
                "strict": tool.strict(),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "scope": "current_component_requirements",
        "revision": snapshot.revision(),
        "prepared": snapshot.is_prepared(),
        "dirty": snapshot.is_dirty(),
        "nodes": nodes,
        "native_tools": native_tools,
    }))
}
fn render_projection_text(
    reaction: usize,
    snapshot: &ProjectionSnapshot<'_>,
) -> anyhow::Result<String> {
    let mut output = String::new();
    let _ = writeln!(&mut output, "# AgentView Component Projection");
    let _ = writeln!(
        &mut output,
        "capture: component projection / before reaction"
    );
    let _ = writeln!(&mut output, "reaction: {reaction}");
    let _ = writeln!(&mut output, "revision: {}", snapshot.revision());
    let _ = writeln!(&mut output, "prepared: {}", snapshot.is_prepared());
    let _ = writeln!(&mut output, "dirty: {}", snapshot.is_dirty());
    let _ = writeln!(
        &mut output,
        "scope: complete current Component requirements\ncontext: previously delivered content may remain in retained history after leaving this projection\n"
    );
    for (node_index, node) in snapshot.projection().nodes().iter().enumerate() {
        let _ = writeln!(
            &mut output,
            "## Node {}: {}\n",
            node_index + 1,
            node.identity()
        );
        if node.items().is_empty() {
            output.push_str("(no input items)\n\n");
        }
        for (item_index, item) in node.items().iter().enumerate() {
            render_item(&mut output, node.identity(), item_index + 1, item)?;
        }
        if !node.diffs().is_empty() {
            output.push_str("### Diff markers\n\n");
            for marker in node.diffs() {
                let _ = writeln!(
                    &mut output,
                    "item_index: {}; structural_path: {:?}; slot: {}",
                    marker.item_index(),
                    marker.structural_path(),
                    marker.slot(),
                );
            }
            output.push('\n');
        }
    }
    output.push_str("## Native tools\n\n");
    if snapshot.projection().native_tools().is_empty() {
        output.push_str("(none)\n");
    }
    for (tool_index, tool) in snapshot.projection().native_tools().iter().enumerate() {
        let schema = serde_json::to_string_pretty(tool.input_schema())
            .context("serialize native tool input schema")?;
        let _ = writeln!(
            &mut output,
            "### Tool {}: {}\n",
            tool_index + 1,
            tool.name()
        );
        let _ = writeln!(&mut output, "description:\n{}\n", tool.description());
        let _ = writeln!(&mut output, "strict: {}\n", tool.strict());
        let _ = writeln!(&mut output, "input_schema:\n{schema}\n");
    }
    Ok(output)
}
fn render_item(
    output: &mut String,
    node_identity: &str,
    index: usize,
    item: &CanonicalInputItem,
) -> anyhow::Result<()> {
    match item {
        CanonicalInputItem::Instruction { authority, pom } => {
            let _ = writeln!(output, "### Item {index}: instruction\n");
            let _ = writeln!(output, "authority: {authority:?}\n");
            append_content(
                output,
                "rendered_pom",
                &render_pom_document(pom).with_context(|| {
                    format!("render POM for node {node_identity}, item {index}")
                })?,
            );
        }
        CanonicalInputItem::Message { role, pom } => {
            let _ = writeln!(output, "### Item {index}: message\n");
            let _ = writeln!(output, "role: {role:?}\n");
            append_content(
                output,
                "rendered_pom",
                &render_pom_document(pom).with_context(|| {
                    format!("render POM for node {node_identity}, item {index}")
                })?,
            );
        }
        CanonicalInputItem::AssistantText {
            text,
            phase,
            status,
        } => {
            let _ = writeln!(output, "### Item {index}: assistant_text\n");
            let _ = writeln!(output, "phase: {phase:?}");
            let _ = writeln!(output, "status: {status:?}\n");
            append_content(output, "text", text);
        }
        CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        } => {
            let _ = writeln!(output, "### Item {index}: tool_call\n");
            let _ = writeln!(output, "call_id: {call_id}");
            let _ = writeln!(output, "name: {name}\n");
            append_content(output, "raw_arguments", raw_arguments);
        }
        CanonicalInputItem::ToolResult { call_id, content } => {
            let _ = writeln!(output, "### Item {index}: tool_result\n");
            let _ = writeln!(output, "call_id: {call_id}\n");
            append_content(output, "content", content);
        }
        CanonicalInputItem::ProviderExtension(_) => {
            let _ = writeln!(output, "### Item {index}: provider_extension\n");
            let raw = serde_json::to_string_pretty(item)
                .context("serialize provider extension projection item")?;
            append_content(output, "canonical_item", &raw);
        }
    }
    Ok(())
}
fn append_content(output: &mut String, label: &str, content: &str) {
    let _ = writeln!(output, "{label}:");
    if content.is_empty() {
        output.push_str("(empty)\n\n");
    } else {
        output.push_str(content);
        if !content.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
    }
}
