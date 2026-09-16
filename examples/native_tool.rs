//! A live model changes retained state, sees the result, then undoes its change.
//!
//! Run `cargo run --no-default-features --example native_tool` with the shared
//! example credentials in `.env` or `OPENAI_API_KEY`.

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{execution::Application, prelude::*},
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use anyhow::Context as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Adjustment {
    /// The amount to add, from -100 through 100.
    amount: i32,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Undo {
    /// The current value shown in the latest counter view.
    expected_value: i32,
}

#[derive(Clone, Serialize)]
struct Counter {
    value: i32,
    previous: Option<i32>,
    operations: u8,
    feedback: &'static str,
}

#[component]
fn counter(answer: Arc<Mutex<String>>) -> Component {
    let state = use_signal(|| Counter {
        value: 17,
        previous: None,
        operations: 0,
        feedback: "Ready to adjust.",
    });
    let current = state.with(Clone::clone).expect("mounted counter");
    let value = current.value;
    let feedback = current.feedback;
    let next_action = match current.operations {
        0 => "Call adjust exactly once with amount=25.",
        1 => "Call undo exactly once, passing the current counter value as expected_value.",
        _ => {
            "The undo is complete. Reply with only the current counter value, without using tools."
        }
    };
    let adjust_state = state.clone();

    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let answer = Arc::clone(&answer);
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                println!("assistant: {text}");
                *answer.lock().expect("answer lock") = text.to_string();
            }
            Ok::<(), Infallible>(())
        }
    });

    view! {
        instructions {
            "Follow the latest counter view. Make at most one tool call per response, then wait \
             for its result and the updated view before choosing your next action."
        }
        counter_state {
            value { "{value}" }
            feedback { "{feedback}" }
            next_action { "{next_action}" }
        }
        NativeToolCall {
            name: "adjust",
            description: "Add an amount from -100 through 100. Available before the first adjustment.",
            on_call: move |input: Adjustment| {
                adjust_state.update(|counter| {
                    if counter.operations != 0 || !(-100..=100).contains(&input.amount) {
                        counter.feedback = "Ignored: adjust requires a fresh counter and an amount from -100 through 100.";
                    } else {
                        counter.previous = Some(counter.value);
                        counter.value += input.amount;
                        counter.operations += 1;
                        counter.feedback = "Adjustment applied. Undo is now available.";
                    }
                    counter.clone()
                })
            },
        }
        NativeToolCall {
            name: "undo",
            description: "Undo the adjustment. Pass the current counter value to confirm which state you observed.",
            on_call: move |input: Undo| {
                state.update(|counter| {
                    if counter.operations != 1 || input.expected_value != counter.value {
                        counter.feedback = "Ignored: undo requires an applied adjustment and the current counter value.";
                    } else {
                        counter.value = counter.previous.take().expect("applied adjustment");
                        counter.operations += 1;
                        counter.feedback = "Undo applied. The original value is restored.";
                    }
                    counter.clone()
                })
            },
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = live_provider::from_env("native-tool")?;
    let answer = Arc::new(Mutex::new(String::new()));
    let component_answer = Arc::clone(&answer);
    let mut application =
        Application::mount(move || counter(Arc::clone(&component_answer)), provider)?;

    let operation =
        tokio::time::timeout(Duration::from_secs(120), async {
            for (step, tool, expected_value, expected_arguments) in [
                (1, "adjust", 42, serde_json::json!({"amount": 25})),
                (2, "undo", 17, serde_json::json!({"expected_value": 42})),
            ] {
                println!("reaction {step}: observe the counter, then call {tool}");
                let _ = application.react().await?;
                let current_projection = application.current_projection();
                let items: Vec<_> = current_projection
                    .projection()
                    .nodes()
                    .iter()
                    .flat_map(|node| node.items())
                    .collect();
                let calls: Vec<_> = items
                    .iter()
                    .filter_map(|item| match item {
                        CanonicalInputItem::ToolCall {
                            call_id,
                            name,
                            raw_arguments,
                        } => Some((call_id, name, raw_arguments)),
                        _ => None,
                    })
                    .collect();
                anyhow::ensure!(calls.len() == step, "expected one action per reaction");
                let (call_id, _, arguments) = calls
                    .into_iter()
                    .find(|(_, name, _)| *name == tool)
                    .context("model did not select the advertised next action")?;
                anyhow::ensure!(
                    serde_json::from_str::<serde_json::Value>(arguments)? == expected_arguments,
                    "model did not use the observed counter state"
                );
                let result = items
                    .iter()
                    .find_map(|item| match item {
                        CanonicalInputItem::ToolResult {
                            call_id: result_id,
                            content,
                        } if result_id == call_id => Some(content),
                        _ => None,
                    })
                    .context("action result missing from the component")?;
                let result: serde_json::Value = serde_json::from_str(result)?;
                anyhow::ensure!(result["value"] == expected_value && result["operations"] == step);
                let view = items
                    .iter()
                    .filter_map(|item| match item {
                        CanonicalInputItem::Instruction { pom, .. }
                        | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom)),
                        _ => None,
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("\n");
                anyhow::ensure!(view.contains(&format!("<value>{expected_value}</value>")));
                anyhow::ensure!(
                    view.contains(result["feedback"].as_str().context("feedback missing")?)
                );
                println!("counter: {result}");
            }

            println!("reaction 3: deliver the undo result and restored state");
            let _ = application.react().await?;
            let call_count = application
                .current_projection()
                .projection()
                .nodes()
                .iter()
                .flat_map(|node| node.items())
                .filter(|item| matches!(item, CanonicalInputItem::ToolCall { .. }))
                .count();
            anyhow::ensure!(
                call_count == 2,
                "model acted after completing the interaction"
            );
            anyhow::ensure!(
                answer.lock().expect("answer lock").trim() == "17",
                "model did not answer using the restored counter"
            );
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("native tool example exceeded 120 seconds")
        .and_then(|result| result);
    let shutdown = application.shutdown().await;
    operation?;
    shutdown?;
    println!("verified: observe -> adjust -> feedback -> undo -> restored-state answer");
    Ok(())
}
