//! Shared application: business state and typed Actions, with framework feedback.

use agentview::component::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecordInput {
    /// The complete note to save, verbatim.
    text: String,
}

fn snapshot(notes: &[String]) -> Value {
    json!({"count": notes.len(), "latest": notes.last()})
}

#[component]
pub fn notebook() -> Component {
    let notes = use_signal(Vec::<String>::new);
    // The root owns the one command barrier. Actions may also live in children.
    use_wait_for_command();
    let (count, latest) = notes
        .with(|notes| (notes.len(), notes.last().cloned().unwrap_or_default()))
        .expect("mounted notebook");
    let record_notes = notes.clone();
    let inspect_notes = notes;

    view! {
        // Separate stable XML roots let ordinary projection comparison omit
        // unchanged context when the current notebook state changes.
        notebook_context {
            "Use record to save a note and inspect to read the current count and latest note. Notes last until this application shuts down. Action results are complete JSON; an error object describes a business rejection."
        }
        notebook_state {
            count { "{count}" }
            latest { "{latest}" }
        }
        Action {
            name: "record",
            description: "Save a nonempty note and return the current count and complete latest note.",
            on_call: move |input: RecordInput| -> anyhow::Result<Value> {
                if input.text.trim().is_empty() {
                    return Ok(json!({"error": {"code": "empty_note", "message": "Supply a nonempty text value."}}));
                }
                Ok(record_notes.update(|notes| {
                    notes.push(input.text);
                    snapshot(notes)
                })?)
            },
        }
        Action {
            name: "inspect",
            description: "Read the current count and complete latest note without changing notebook state. Send an empty input object.",
            on_call: move || inspect_notes.with(|notes| snapshot(notes)),
        }
        // No application-owned outcome log: the command barrier mounts the
        // framework's ordinary command_feedback Component automatically.
    }
}
