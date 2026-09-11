//! A live model calls a Rust function, then answers using its tool result.
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
    transcript::CanonicalInputItem,
};
use anyhow::Context as _;

#[path = "support/live_provider.rs"]
mod live_provider;

/// Add two integers.
#[tool]
fn add(a: i32, b: i32) -> Result<i32, ToolError> {
    let result = a
        .checked_add(b)
        .ok_or_else(|| ToolError::new("integer overflow"))?;
    println!("native: add({a}, {b}) = {result}");
    Ok(result)
}

#[component]
fn calculator(answer: Arc<Mutex<String>>) -> Component {
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

    let request = r#"## Request

- Call `add` exactly once with `a=17` and `b=25`.
- Do not answer before using the tool.
- After receiving the tool result, reply with only the resulting number."#;
    view! {
        { request }
        { NativeToolCall::new(add) }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = live_provider::from_env("native-tool")?.with_response_usage_observer(|usage| {
        if let Some(tokens) = usage.input_tokens() {
            println!("provider input tokens: {tokens}");
        }
    });
    let answer = Arc::new(Mutex::new(String::new()));
    let component_answer = Arc::clone(&answer);
    let mut application =
        Application::mount(move || calculator(Arc::clone(&component_answer)), provider)?;

    let operation = tokio::time::timeout(Duration::from_secs(120), async {
        println!("reaction 1: request native tool call");
        let _ = application.react().await?;
        let records: Vec<_> = application
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
            .collect();
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

        println!("reaction 2: submit tool result");
        let _ = application.react().await?;
        anyhow::ensure!(
            answer.lock().expect("answer lock").trim() == "42",
            "model did not answer with the tool result"
        );
        let current_records: Vec<_> = application
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
            .collect();
        anyhow::ensure!(
            current_records == records,
            "continuation unexpectedly added another tool call"
        );
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("native tool example exceeded 120 seconds")
    .and_then(|result| result);
    let shutdown = application.shutdown().await;
    operation?;
    shutdown?;
    println!("verified: native call, component records, result handoff, and model answer");
    Ok(())
}
