use std::fmt::Write as _;

use agentview::{
    component::{
        execution::{DebugProviderPort, ProviderEvent, ProviderPort, RenderedProjection},
        prelude::*,
        ComponentHost,
    },
    provider::async_openai::AsyncOpenAiResponsesProvider,
};
use futures::StreamExt;

use super::{capture_complete_prompt, request_view, responses_provider, ResponsesAcceptanceServer};

#[derive(Clone)]
struct AtomicDiffProps {
    value: &'static str,
}

#[component]
fn atomic_diff_application(
    props: AtomicDiffProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let value = props.value;
    view! {
        #[diff(slot = "current_state")]
        current_state { "{value}" }
    }
}

#[derive(AgentView)]
#[agent_view(kind = "replace_state")]
struct ReplaceStateView {
    #[view(element)]
    objective: &'static str,

    #[view(diff(replace))]
    phase: &'static str,
}

#[derive(Clone)]
struct ReplaceDiffProps {
    phase: &'static str,
}

#[component]
fn replace_diff_application(
    props: ReplaceDiffProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    view! {
        #[diff(slot = "replace_state")]
        {
            ReplaceStateView {
                objective: "Keep this field stable.",
                phase: props.phase,
            }
        }
    }
}

struct DiffShapeCapture {
    baseline_submission: String,
    current_component_prompt: String,
    current_submission: String,
}

async fn execute_and_capture_request(
    provider: &mut AsyncOpenAiResponsesProvider,
    mock: &mut ResponsesAcceptanceServer,
    projection: RenderedProjection,
) -> anyhow::Result<super::ResponsesRequestView> {
    let mut events = provider.execute(projection).await?;
    while let Some(event) = events.next().await {
        event?;
    }
    request_view(&mock.next_request().await?)
}

async fn capture<Props>(
    mock: &mut ResponsesAcceptanceServer,
    root: fn(Props, EventInput<ProviderEvent>) -> Component,
    baseline: Props,
    current: Props,
) -> anyhow::Result<DiffShapeCapture>
where
    Props: Clone + Send + 'static,
{
    let request_count = mock.request_count();
    let mut provider = responses_provider(mock.api_base())?;
    let (mut debug, debug_capture) = DebugProviderPort::new();
    let mut components = ComponentHost::new(root, baseline);
    let baseline = execute_and_capture_request(
        &mut provider,
        mock,
        components.render()?.projection().clone(),
    )
    .await?;
    components.set_props(current);
    let projection = components.render()?.projection().clone();
    let current_component_prompt =
        capture_complete_prompt(&mut debug, &debug_capture, projection.clone()).await?;
    let current = execute_and_capture_request(&mut provider, mock, projection).await?;
    anyhow::ensure!(
        baseline.user_inputs.len() == 1 && current.user_inputs.len() == 2,
        "a two-step diff trace must submit one baseline and one current user item"
    );
    anyhow::ensure!(mock.request_count() == request_count + 2);
    Ok(DiffShapeCapture {
        baseline_submission: baseline.user_inputs[0].clone(),
        current_component_prompt,
        current_submission: current.user_inputs[1].clone(),
    })
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
    let atomic = capture(
        mock,
        atomic_diff_application,
        AtomicDiffProps { value: "A" },
        AtomicDiffProps { value: "A+" },
    )
    .await?;
    anyhow::ensure!(atomic.baseline_submission == "<current_state>A</current_state>");
    anyhow::ensure!(
        atomic.current_component_prompt == "## User\n\n<current_state>A+</current_state>"
            && atomic.current_submission == "<current_state>A+</current_state>"
    );
    write_capture(report, 1, "ATOMIC / SINGLE-FIELD ROOT", &atomic)?;

    let replace = capture(
        mock,
        replace_diff_application,
        ReplaceDiffProps { phase: "A" },
        ReplaceDiffProps { phase: "B" },
    )
    .await?;
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
