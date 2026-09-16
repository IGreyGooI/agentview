//! A model saves notes, observes the saved count, then confirms that count.
//!
//! Run `cargo run --no-default-features --example streaming_callbacks` with the
//! shared example credentials in `.env` or `OPENAI_API_KEY`.

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

#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Clone, Default)]
struct Notebook {
    notes: Vec<String>,
    opens: usize,
    streamed_text: String,
    feedback: String,
    confirmed: bool,
}

#[component]
fn notebook(answer: Arc<Mutex<String>>) -> Component {
    let state = use_signal(Notebook::default);
    let current = state.with(Clone::clone).expect("mounted notebook");
    let count = current.notes.len();
    let opens = current.opens;
    let streamed_text = current.streamed_text;
    let feedback = current.feedback;
    let confirmed = current.confirmed;
    let notes = current.notes.join(", ");
    let next_action = if confirmed {
        "The confirmation succeeded. Reply with exactly CONFIRMED 2 and do not send more actions."
    } else if count == 0 {
        "Respond with exactly <note>alpha</note><note>beta</note>, then wait for the updated view."
    } else {
        "Read saved_count below and respond only with <confirm count=\"...\"/> using that count."
    };
    let open_state = state.clone();
    let delta_state = state.clone();
    let complete_state = state.clone();
    let invalid_state = state.clone();
    let confirm_invalid_state = state.clone();

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

    let actions = if confirmed {
        view! {}
    } else if count == 0 {
        view! {
            XmlStreamingToolCall {
                element: XmlToolElement::text("note"),
                description: "Save a note. Write its text between <note> and </note>.",
                on_open: move |_: Arc<()>| open_state.update(|book| book.opens += 1),
                on_delta: move |text: String| {
                    delta_state.update(|book| book.streamed_text.push_str(&text))
                },
                on_complete: move |text: String| {
                    complete_state.update(|book| {
                        if count != 0 || text.trim().is_empty() {
                            book.feedback = "Ignored: add nonempty notes before confirming the notebook.".to_owned();
                        } else {
                            book.notes.push(text);
                            book.feedback = format!("Saved {} notes. Read saved_count to confirm them.", book.notes.len());
                        }
                    })
                },
                on_invalid: move |diagnostic: XmlCallbackDiagnostic| {
                    invalid_state.update(|book| book.feedback = diagnostic.to_string())
                },
            }
        }
    } else {
        view! {
            XmlStreamingToolCall {
                element: XmlToolElement::self_closing("confirm")
                    .required_attribute::<usize>("count", "2")
                    .decode(
                        |mut attributes| attributes.take_required::<usize>("count"),
                        |count, _| Ok(*count),
                    ),
                description: "Confirm the saved notes using the saved_count from the latest notebook view.",
                on_complete: move |count: usize| {
                    state.update(|book| {
                        if book.notes.is_empty() || count != book.notes.len() {
                            book.feedback = "Ignored: confirm must match the current saved_count.".to_owned();
                        } else {
                            book.confirmed = true;
                            book.feedback = format!("Confirmed {count} saved notes.");
                        }
                    })
                },
                on_invalid: move |diagnostic: XmlCallbackDiagnostic| {
                    confirm_invalid_state.update(|book| book.feedback = diagnostic.to_string())
                },
            }
        }
    };

    view! {
        "Follow the latest notebook view. {next_action}"
        notebook_state {
            saved_count { "{count}" }
            notes { "{notes}" }
            feedback { "{feedback}" }
            confirmed { "{confirmed}" }
            opened_notes { "{opens}" }
            streamed_text { "{streamed_text}" }
        }
        { actions }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = live_provider::from_env("streaming-callbacks")?;
    let answer = Arc::new(Mutex::new(String::new()));
    let component_answer = Arc::clone(&answer);
    let mut application =
        Application::mount(move || notebook(Arc::clone(&component_answer)), provider)?;
    let operation = tokio::time::timeout(Duration::from_secs(120), async {
        for step in 1..=3 {
            let _ = application.react().await?;
            let mut view = String::new();
            for item in application
                .current_projection()
                .projection()
                .nodes()
                .iter()
                .flat_map(|node| node.items())
            {
                if let CanonicalInputItem::Message { pom, .. } = item {
                    view.push_str(&render_pom_document(pom)?);
                }
            }
            println!("reaction {step}:\n{view}");
            anyhow::ensure!(
                view.contains("<saved_count>2</saved_count>"),
                "two notes were not saved"
            );
            anyhow::ensure!(
                view.contains("<notes>alpha, beta</notes>"),
                "unexpected notes"
            );
            anyhow::ensure!(
                view.contains("<opened_notes>2</opened_notes>"),
                "open callbacks were not delivered"
            );
            anyhow::ensure!(
                view.contains("<streamed_text>alphabeta</streamed_text>"),
                "delta callbacks lost text"
            );
            if step >= 2 {
                anyhow::ensure!(
                    view.contains("<confirmed>true</confirmed>"),
                    "model did not act on the updated view"
                );
            }
        }
        anyhow::ensure!(
            answer.lock().expect("answer lock").trim() == "CONFIRMED 2",
            "model did not acknowledge the confirmation feedback"
        );
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("streaming callback example exceeded 120 seconds")
    .and_then(|result| result);
    let shutdown = application.shutdown().await;
    operation?;
    shutdown?;
    println!("verified: streamed callbacks -> saved notes -> observed count -> confirmation -> acknowledgement");
    Ok(())
}
