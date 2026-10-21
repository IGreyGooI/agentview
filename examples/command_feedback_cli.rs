//! Credential-free stdin application with discoverable Actions and feedback.
//!
//! Run `cargo run --no-default-features --example command_feedback_cli`.
//! Send {"action":"record","input":{"text":"Keep complete action results."}}
//! followed by {"action":"inspect","input":{}} as top-level JSON objects.
//! With stdin open, end the whole batch with a newline (`\n`); 100 ms without
//! new bytes then submits the buffer as one frame. A newline alone
//! does not submit; complete JSON without a final newline stays buffered.
//! EOF submits immediately without requiring a newline. Objects may span lines.
//! Send the whole batch and final newline in one write, wait for its view, then
//! send the next batch. Pausing over 100 ms after any newline can split a batch.
//! JSON is decoded after framing; incomplete or malformed JSON rejects the
//! whole frame without executing its prefix. No end marker is required; input
//! and results have no fixed line-size or transport byte cap.
//! The initial view arrives immediately. Later native Frame deltas retain all
//! outcomes from the current batch without a separate result copy. An idle
//! observation each second delivers changes made by another connected reader.

#[path = "command_feedback/app.rs"]
mod app;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    use agentview::component::execution::{StdinApplication, StdinApplicationOptions};

    StdinApplication::run_with_options(
        app::notebook,
        StdinApplicationOptions {
            idle_refresh_interval: Some(std::time::Duration::from_secs(1)),
        },
    )
    .await
}
