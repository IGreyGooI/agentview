//! Start asynchronous reactions over one mounted tree, with temporary history.
//! Run `cargo run --no-default-features --example parallel_reactions`.

#[path = "support/parallel_fixture.rs"]
mod fixture;

use std::sync::{Arc, Mutex};

use agentview::{
    component::{
        execution::{Application, ReactionKind, ReactionOutcome},
        prelude::*,
        EventContext,
    },
    transcript::CanonicalInputItem,
};
use anyhow::{ensure, Context as _};
use fixture::{Port, Script};

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Adjustment {
    /// Signed amount to add to the shared total.
    amount: i64,
}

#[derive(Clone, Default)]
struct Observed {
    total: Arc<Mutex<Option<Signal<i64>>>>,
    actions: Arc<Mutex<Vec<EventContext>>>,
}

#[component]
fn counter(observed: Observed) -> Component {
    let total = use_signal(|| 0_i64);
    *observed.total.lock().unwrap() = Some(total.clone());
    let current = total.with(|value| *value).expect("mounted counter");
    view! {
        counter {
            total { "{current}" }
            instruction { "Adjust the shared total, observe the result, then choose the next amount." }
        }
        NativeToolCall {
            name: "adjust",
            description: "Add the signed amount and return the actual new total.",
            on_call: move |context, input: Adjustment| {
                observed.actions.lock().unwrap().push(context);
                total.update(|value| { *value += input.amount; *value })
            },
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (port, capture) = Port::scripted([
        Script::Completed, // establish a settled checkpoint
        Script::Pending,   // Main keeps streaming while later events start branches
        Script::Adjust(10),
        Script::Adjust(-3),
        Script::Completed, // deliver the temporary's final tool result
        Script::Completed, // Main observes shared business state
    ]);
    let observed = Observed::default();
    let root_observed = observed.clone();
    let mut app = Application::mount(move || counter(root_observed.clone()), port)?;
    let application_id = app.application_id();

    let result = async {
        ensure!(app.react(false)?.await? == ReactionOutcome::Completed);
        let main = app.react(false)?;
        capture.wait_for_frames(2).await;

        // A later business event starts this operation immediately. Both handles
        // refer to the same mounted counter and carry their own ReactionId.
        let branch = app.react(true)?;
        ensure!(main.kind() == ReactionKind::Main);
        ensure!(branch.kind() == ReactionKind::Temporary);
        let branch_id = branch.id();
        ensure!(branch.clone().await? == ReactionOutcome::Completed);

        let next = branch.react()?;
        ensure!(next.kind() == ReactionKind::Temporary);
        ensure!(next.id() != branch_id);
        ensure!(next.clone().await? == ReactionOutcome::Completed);
        ensure!(capture.staged_results(3)
            == [CanonicalInputItem::tool_result("adjust-10", "10")?]);

        ensure!(next.react()?.await? == ReactionOutcome::Completed);
        ensure!(capture.staged_results(4)
            == [CanonicalInputItem::tool_result("adjust--3", "7")?]);
        main.interrupt()?;
        ensure!(main.await? == ReactionOutcome::Interrupted);

        ensure!(app.react(false)?.await? == ReactionOutcome::Completed);
        let current = observed.total.lock().unwrap().clone().context("counter Signal")?;
        ensure!(current.with(|value| *value)? == 7);
        let actions = observed.actions.lock().unwrap();
        ensure!(actions.len() == 2);
        ensure!(actions.iter().all(|context| {
            context.application_id() == application_id
                && context.reaction_kind() == Some(ReactionKind::Temporary)
        }));
        let frames = capture.frames.lock().unwrap();
        let main_followup = &frames[5];
        ensure!(main_followup.submission().staged_inputs().is_empty());
        ensure!(main_followup
            .submission()
            .replay()
            .iter()
            .chain(main_followup.submission().projection().items())
            .all(|item| !matches!(item, CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. })));
        println!("Same Application: {application_id}; temporary reaction: {branch_id}.");
        println!("Temporary observed 10, adjusted again, then received 7.");
        println!("Main was interrupted; its next Frame sees shared total 7 with no temporary protocol history.");
        Ok::<(), anyhow::Error>(())
    }.await;

    let cleanup = app.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}
