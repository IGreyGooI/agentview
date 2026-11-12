//! Inspect Component origins and byte sizes of the complete current prompt.
//!
//! Run `cargo run --no-default-features --example prompt_profile`; no credentials are needed.

use agentview::component::{prelude::*, ComponentHost};
use schemars::JsonSchema;
use serde::Deserialize;

#[component]
fn task_context(title: String, details: String) -> Component {
    view! {
        task {
            title { "{title}" }
            details { "{details}" }
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecordChoice {
    title: String,
}

#[component]
fn task_picker() -> Component {
    let chosen = use_signal(|| String::from("pending"));
    let current = chosen.with(Clone::clone).expect("mounted choice");
    view! {
        #[system_once]
        "Choose one displayed task using record_choice, then inspect its actual result."

        task_context("Review".to_owned(), "Inspect the changed Components and their test coverage.".to_owned())
        task_context("Document".to_owned(), "Explain the new behavior and provide a runnable example. 中文内容也会保留来源。".to_owned())

        selection { "{current}" }
        NativeToolCall {
            name: "record_choice",
            description: "Record the selected task title and expose it in the next observation.",
            on_call: move |_context, input: RecordChoice| chosen.set(input.title),
        }

        #[developer(ephemeral, repeat)]
        "Choose the task that most directly advances the current request."
    }
}

fn main() -> anyhow::Result<()> {
    let mut host = ComponentHost::new_root(|_| task_picker(), ());
    let prompt = host.render()?.projection().render_prompt_with_sources()?;
    let profile = prompt.profile_bytes();

    println!(
        "{} bytes, {} framework bytes, {} unattributed bytes",
        profile.total_bytes(),
        profile.framework_bytes(),
        profile.unattributed_bytes(),
    );
    println!("\n  self  subtree  Component");
    for component in profile.components() {
        println!(
            "{:>6} {:>8}  {}",
            component.self_bytes(),
            component.inclusive_bytes(),
            component.identity(),
        );
    }

    println!("\nSource byte ranges:");
    for span in prompt.source_spans() {
        println!("{:?}: {:?}", span.byte_range(), span.source());
    }
    println!(
        "\nSelf counts include each Component's own UTF-8 bytes; subtree includes descendants."
    );
    println!("These sizes describe the complete current prompt; provider requests can include additional context.");
    Ok(())
}
