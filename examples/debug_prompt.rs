//! Inspect Component projections and exact Responses requests across two tool turns.
//!
//! Run `cargo run --no-default-features --example debug_prompt` with the shared
//! example credentials in `.env` or `OPENAI_API_KEY`.

use std::{
    convert::Infallible,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        execution::{Application, ReactionPort},
        prelude::*,
    },
    transcript::CanonicalInputItem,
};
use anyhow::Context as _;

#[path = "support/live_provider.rs"]
mod live_provider;
#[path = "support/projection_debug.rs"]
mod projection_debug;
#[path = "support/prompt_debug.rs"]
mod prompt_debug;

/// Add two integers for the user.
#[tool]
fn add(a: i32, b: i32) -> Result<i32, ToolError> {
    let result = a
        .checked_add(b)
        .ok_or_else(|| ToolError::new("integer overflow"))?;
    println!("native: add({a}, {b}) = {result}");
    Ok(result)
}

#[component]
fn debug_tool_agent(answer: Arc<Mutex<String>>) -> Component {
    let task_phase = use_signal(|| "awaiting_tool");
    let completed_phase = task_phase.clone();
    use_reaction_completion(move || async move { completed_phase.set("answer_ready") });
    let current_phase = task_phase.with(|phase| *phase).expect("mounted task phase");

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
        #[system_once]
        system_policy {
            "Use the available native tool before answering arithmetic requests."
        }
        #[developer]
        developer_policy {
            "Call add exactly once when asked for this sum. After its result arrives, respond with only the number."
        }
        #[user]
        user_request { "What is 17 plus 25?" }
        #[user]
        task_state { "{current_phase}" }
        { NativeToolCall::new(add) }
    }
}

async fn run_debug_tool(provider: impl ReactionPort, run_dir: &Path) -> anyhow::Result<()> {
    let answer = Arc::new(Mutex::new(String::new()));
    let component_answer = Arc::clone(&answer);
    let mut application = Application::mount(
        move || debug_tool_agent(Arc::clone(&component_answer)),
        provider,
    )?;

    let operation = tokio::time::timeout(Duration::from_secs(120), async {
        println!("reaction 1: request native tool call");
        capture_projection_before_reaction(&mut application, run_dir, 1).await?;
        let _ = application.react().await?;
        let records = native_records(&application);
        let [CanonicalInputItem::ToolCall {
            call_id,
            name,
            raw_arguments,
        }, CanonicalInputItem::ToolResult {
            call_id: result_id,
            content,
        }] = records.as_slice()
        else {
            anyhow::bail!("expected one native call and its result, got {records:?}");
        };
        anyhow::ensure!(
            name == "add" && call_id == result_id && content == "42",
            "unexpected native call/result"
        );
        anyhow::ensure!(
            serde_json::from_str::<serde_json::Value>(raw_arguments)?
                == serde_json::json!({"a": 17, "b": 25}),
            "unexpected tool arguments"
        );
        println!("component: ToolCall + ToolResult, call_id={call_id}, result={content}");

        println!("reaction 2: submit tool result continuation");
        capture_projection_before_reaction(&mut application, run_dir, 2).await?;
        let _ = application.react().await?;
        anyhow::ensure!(
            answer.lock().expect("answer lock").trim() == "42",
            "model did not answer with the tool result"
        );
        anyhow::ensure!(
            native_records(&application) == records,
            "continuation unexpectedly added another tool call"
        );
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("debug prompt example exceeded 120 seconds")
    .and_then(|result| result);
    let shutdown = application.shutdown().await;

    match (operation, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(()), Err(shutdown)) => Err(shutdown.into()),
        (Err(operation), Err(shutdown)) => {
            Err(operation.context(format!("debug prompt shutdown also failed: {shutdown:#}")))
        }
    }
}

async fn capture_projection_before_reaction(
    application: &mut Application<impl ReactionPort>,
    run_dir: &Path,
    reaction: usize,
) -> anyhow::Result<()> {
    // `react()` prepares again; this inspects a checkpoint before that pass.
    anyhow::ensure!(
        application.prepare().await?.is_continue(),
        "application exited while preparing reaction {reaction}"
    );
    projection_debug::write_projection(run_dir, reaction, application.current_projection())
}

fn native_records(application: &Application<impl ReactionPort>) -> Vec<CanonicalInputItem> {
    application
        .current_projection()
        .projection()
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter(|item| {
            matches!(
                item,
                CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
            )
        })
        .cloned()
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = live_provider::from_env("debug-prompt")?;
    let capture = prompt_debug::PromptDebugCapture::new()?;
    eprintln!(
        "[agentview-debug] writing projections and exact request bodies to {}",
        capture.output_dir().display()
    );
    let provider = provider.with_request_observer(capture.observer());

    // `run_debug_tool` owns and drops the provider before `finish` joins its writer.
    let operation = run_debug_tool(provider, capture.output_dir()).await;
    let flushed = capture.finish().await;
    match (operation, flushed) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(()), Err(flushed)) => Err(flushed),
        (Err(operation), Err(flushed)) => {
            Err(operation.context(format!("prompt debug flush also failed: {flushed:#}")))
        }
    }
}

#[cfg(test)]
#[path = "../tests/examples/debug_prompt.rs"]
mod tests;
