use std::{
    fmt::Write as _,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use agentview::component::{execution::Application, prelude::*};
use futures::FutureExt;

use super::{
    combine_operation_cleanup, projection_prompt, request_view, responses_provider,
    ResponsesAcceptanceServer,
};

#[derive(Clone)]
struct AtomicDiffProps {
    exported: Arc<Mutex<Option<Signal<String>>>>,
}

#[component]
fn atomic_diff_application(props: AtomicDiffProps) -> Component {
    let value = use_signal(|| String::from("A"));
    *props.exported.lock().expect("atomic Signal export lock") = Some(value.clone());
    let current = value.with(Clone::clone).expect("mounted atomic Signal");
    view! {
        #[diff(slot = "current_state")]
        current_state { "{current}" }
    }
}

#[derive(AgentView)]
#[agent_view(kind = "replace_state")]
struct ReplaceStateView {
    #[view(element)]
    objective: &'static str,

    #[view(diff(replace))]
    phase: String,
}

#[derive(Clone)]
struct ReplaceDiffProps {
    exported: Arc<Mutex<Option<Signal<String>>>>,
}

#[component]
fn replace_diff_application(props: ReplaceDiffProps) -> Component {
    let phase = use_signal(|| String::from("A"));
    *props.exported.lock().expect("replace Signal export lock") = Some(phase.clone());
    let current = phase.with(Clone::clone).expect("mounted replace Signal");
    view! {
        #[diff(slot = "replace_state")]
        {
            ReplaceStateView {
                objective: "Keep this field stable.",
                phase: current,
            }
        }
    }
}

struct DiffShapeCapture {
    baseline_submission: String,
    current_component_prompt: String,
    current_submission: String,
}

async fn capture_atomic(mock: &mut ResponsesAcceptanceServer) -> anyhow::Result<DiffShapeCapture> {
    let requests_before = mock.request_count();
    let exported = Arc::new(Mutex::new(None));
    let props = AtomicDiffProps {
        exported: Arc::clone(&exported),
    };
    let (provider, _) = responses_provider(mock.api_base())?;
    let mut application =
        Application::mount(move || atomic_diff_application(props.clone()), provider)?;
    let operation_result = AssertUnwindSafe(async {
        anyhow::ensure!(application.react().await?.is_continue());
        let baseline = request_view(&mock.next_request().await?)?;
        let signal = exported
            .lock()
            .map_err(|_| anyhow::anyhow!("atomic Signal export lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("atomic Signal was not exported"))?;
        signal.set(String::from("A+"))?;
        anyhow::ensure!(application.react().await?.is_continue());
        let current_component_prompt =
            projection_prompt(application.current_projection().projection())?;
        let current = request_view(&mock.next_request().await?)?;
        anyhow::ensure!(
            baseline.user_inputs.len() == 1 && current.user_inputs.len() == 2,
            "atomic diff trace must submit one baseline and one current item"
        );
        anyhow::ensure!(mock.request_count() == requests_before + 2);
        Ok::<_, anyhow::Error>(DiffShapeCapture {
            baseline_submission: baseline.user_inputs[0].clone(),
            current_component_prompt,
            current_submission: current.user_inputs[1].clone(),
        })
    })
    .catch_unwind()
    .await;
    let shutdown_result = AssertUnwindSafe(application.shutdown())
        .catch_unwind()
        .await
        .map(|result| result.map_err(Into::into));
    combine_operation_cleanup(operation_result, shutdown_result)
}

async fn capture_replace(mock: &mut ResponsesAcceptanceServer) -> anyhow::Result<DiffShapeCapture> {
    let requests_before = mock.request_count();
    let exported = Arc::new(Mutex::new(None));
    let props = ReplaceDiffProps {
        exported: Arc::clone(&exported),
    };
    let (provider, _) = responses_provider(mock.api_base())?;
    let mut application =
        Application::mount(move || replace_diff_application(props.clone()), provider)?;
    let operation_result = AssertUnwindSafe(async {
        anyhow::ensure!(application.react().await?.is_continue());
        let baseline = request_view(&mock.next_request().await?)?;
        let signal = exported
            .lock()
            .map_err(|_| anyhow::anyhow!("replace Signal export lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("replace Signal was not exported"))?;
        signal.set(String::from("B"))?;
        anyhow::ensure!(application.react().await?.is_continue());
        let current_component_prompt =
            projection_prompt(application.current_projection().projection())?;
        let current = request_view(&mock.next_request().await?)?;
        anyhow::ensure!(
            baseline.user_inputs.len() == 1 && current.user_inputs.len() == 2,
            "replace diff trace must submit one baseline and one current item"
        );
        anyhow::ensure!(mock.request_count() == requests_before + 2);
        Ok::<_, anyhow::Error>(DiffShapeCapture {
            baseline_submission: baseline.user_inputs[0].clone(),
            current_component_prompt,
            current_submission: current.user_inputs[1].clone(),
        })
    })
    .catch_unwind()
    .await;
    let shutdown_result = AssertUnwindSafe(application.shutdown())
        .catch_unwind()
        .await
        .map(|result| result.map_err(Into::into));
    combine_operation_cleanup(operation_result, shutdown_result)
}

fn write_capture(
    report: &mut String,
    number: usize,
    title: &str,
    capture: &DiffShapeCapture,
) -> anyhow::Result<()> {
    writeln!(
        report,
        "\n============================================================"
    )?;
    writeln!(report, "DIFF SHAPE {number}: {title}")?;
    writeln!(
        report,
        "============================================================"
    )?;
    writeln!(
        report,
        "BASELINE PROVIDER SUBMISSION\n{}",
        capture.baseline_submission
    )?;
    writeln!(
        report,
        "\nCURRENT COMPONENT FULL PROMPT\n{}",
        capture.current_component_prompt
    )?;
    writeln!(
        report,
        "\nCURRENT PROVIDER SUBMISSION\n{}",
        capture.current_submission
    )?;
    Ok(())
}

pub(super) async fn append_to_report(
    report: &mut String,
    mock: &mut ResponsesAcceptanceServer,
) -> anyhow::Result<()> {
    let atomic = capture_atomic(mock).await?;
    anyhow::ensure!(atomic.baseline_submission == "<current_state>A</current_state>");
    anyhow::ensure!(
        atomic.current_component_prompt == "## User\n\n<current_state>A+</current_state>"
            && atomic.current_submission == "<current_state>A+</current_state>"
    );
    write_capture(report, 1, "ATOMIC / SINGLE-FIELD ROOT", &atomic)?;

    let replace = capture_replace(mock).await?;
    anyhow::ensure!(
        replace.baseline_submission
            == "<replace_state>\n  <objective>Keep this field stable.</objective>\n  <phase>A</phase>\n</replace_state>"
    );
    anyhow::ensure!(
        replace.current_component_prompt
            == "## User\n\n<replace_state>\n  <objective>Keep this field stable.</objective>\n  <phase>B</phase>\n</replace_state>"
    );
    anyhow::ensure!(
        replace.current_submission
            == "<replace_state rendering_mode=\"delta\">\n  <phase rendering_mode=\"delta\">\n    <replace>\n      <phase>B</phase>\n    </replace>\n  </phase>\n</replace_state>"
    );
    write_capture(report, 2, "STRUCTURED FIELD-LOCAL REPLACE", &replace)
}
