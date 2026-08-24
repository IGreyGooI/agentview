//! Visual acceptance report for Component rendering and Provider-private diff state.

use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use agentview::{
    component::{
        execution::{
            ApplicationHost, DebugPromptCapture, DebugProviderPort, ProviderEvent,
            ProviderIdentity, ProviderPort, RenderedProjection,
        },
        prelude::*,
        ComponentHost,
    },
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
};
use futures::StreamExt;
use serde_json::Value;

#[path = "support/responses_acceptance_server.rs"]
mod responses_acceptance_server;
#[path = "support/visual_diff_shapes.rs"]
mod visual_diff_shapes;

use responses_acceptance_server::ResponsesAcceptanceServer;

const BINDING: &str = "provider-port-visual-acceptance";
const RESPONSE_STATES: [&str; 6] = ["B", "B", "A", "B", "B", "B"];
const POLICY_TITLE: &str = "Visual acceptance policy";
const POLICY_INSTRUCTION: &str =
    "Return the next phase as plain text. This example uses a local deterministic provider.";
const STATE_OBJECTIVE: &str =
    "Keep complete business state in the Component and submit only useful semantic changes.";

#[derive(Clone)]
struct AcceptanceProps {
    renders: Arc<AtomicUsize>,
    completed_handlers: Arc<AtomicUsize>,
}

#[derive(AgentView)]
#[agent_view(document)]
struct AcceptancePolicy {
    #[view(heading = 2)]
    title: &'static str,

    #[view(paragraph)]
    instruction: &'static str,
}

#[derive(AgentView)]
#[agent_view(kind = "agent_state")]
struct AgentStateView {
    #[view(element)]
    objective: &'static str,

    #[view(diff)]
    phase: String,

    #[view(diff(append))]
    observations: Vec<String>,
}

fn agent_state(phase: &str) -> AgentStateView {
    let mut observations = vec![String::from("initialized")];
    if phase == "B" {
        observations.push(String::from("phase B observed"));
    }
    AgentStateView {
        objective: STATE_OBJECTIVE,
        phase: phase.to_owned(),
        observations,
    }
}

#[component]
fn acceptance_policy() -> Component {
    view! {
        #[system_once]
        {
            AcceptancePolicy {
                title: POLICY_TITLE,
                instruction: POLICY_INSTRUCTION,
            }
        }
    }
}

#[component]
fn acceptance_state(
    events: EventInput<ProviderEvent>,
    completed_handlers: Arc<AtomicUsize>,
) -> Component {
    let state = use_signal(|| String::from("A"));
    let current = state.with(Clone::clone).expect("mounted acceptance state");
    let text = events.select(ProviderEvent::TEXT);

    view! {
        #[diff(slot = "agent_state")]
        {agent_state(&current)}

        {
            EventListener::observe("acceptance.state", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let state = state.clone();
                    let completed_handlers = completed_handlers.clone();
                    async move {
                        let TextTurnEvent::TextComplete(next) = event else {
                            return Ok::<(), SignalAccessError>(());
                        };
                        tokio::task::yield_now().await;
                        state.set(next.trim().to_owned())?;
                        completed_handlers.fetch_add(1, Ordering::SeqCst);
                        Ok::<(), SignalAccessError>(())
                    }
                })
        }
    }
}

#[component]
fn acceptance_application(props: AcceptanceProps, events: EventInput<ProviderEvent>) -> Component {
    props.renders.fetch_add(1, Ordering::SeqCst);
    view! {
        acceptance_policy()
        acceptance_state(events, props.completed_handlers)
    }
}

fn responses_provider(api_base: &str) -> anyhow::Result<AsyncOpenAiResponsesProvider> {
    let config = AsyncOpenAiTransportConfig::new(api_base, "visual-acceptance-token")?;
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING)?;
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)?;
    Ok(AsyncOpenAiResponsesProvider::new(
        config,
        identity,
        CodexHttpV1Encoder::new(options),
    ))
}

async fn capture_complete_prompt(
    debug: &mut DebugProviderPort,
    capture: &DebugPromptCapture,
    projection: RenderedProjection,
) -> anyhow::Result<String> {
    let mut events = debug.execute(projection).await?;
    while let Some(event) = events.next().await {
        event?;
    }
    capture
        .latest()
        .ok_or_else(|| anyhow::anyhow!("DebugProviderPort did not capture the projection"))
}

fn component_nodes(projection: &RenderedProjection) -> String {
    let mut output = String::new();
    for (index, node) in projection.nodes().iter().enumerate() {
        writeln!(
            output,
            "  [{index}] {} | items={} | diff_fragments={}",
            component_name(node.identity()),
            node.items().len(),
            node.diffs().len()
        )
        .expect("writing to String");
        for diff in node.diffs() {
            writeln!(
                output,
                "      diff slot={} item={} path={:?}",
                diff.slot(),
                diff.item_index(),
                diff.structural_path()
            )
            .expect("writing to String");
        }
    }
    output
}

fn expected_component_nodes() -> &'static str {
    concat!(
        "  [0] root | items=1 | diff_fragments=0\n",
        "  [1] acceptance_application | items=0 | diff_fragments=0\n",
        "  [2] acceptance_policy | items=0 | diff_fragments=0\n",
        "  [3] acceptance_state | items=1 | diff_fragments=1\n",
        "      diff slot=agent_state item=0 path=[0]\n",
    )
}

fn component_name(identity: &str) -> &str {
    let before_location = identity
        .rsplit_once('@')
        .map(|(function, _)| function)
        .unwrap_or(identity);
    let function = before_location
        .rsplit('/')
        .next()
        .unwrap_or(before_location);
    function.rsplit("::").next().unwrap_or(function)
}

struct ResponsesRequestView {
    instructions: String,
    wire_items: Vec<WireItemView>,
    user_inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WireItemView {
    role: String,
    text: String,
}

fn request_view(body: &[u8]) -> anyhow::Result<ResponsesRequestView> {
    let request: Value = serde_json::from_slice(body)?;
    let instructions = request
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input = request
        .get("input")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Responses request has no input array"))?;
    let wire_items = input
        .iter()
        .map(|item| {
            anyhow::ensure!(
                item.get("type").and_then(Value::as_str) == Some("message"),
                "acceptance request contains a non-message wire item"
            );
            let role = item
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("wire message has no role"))?;
            anyhow::ensure!(
                matches!(role, "user" | "assistant"),
                "acceptance request contains unsupported role {role}"
            );
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .and_then(|content| content.first())
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("wire message has no text content"))?;
            Ok(WireItemView {
                role: role.to_owned(),
                text,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let user_inputs = wire_items
        .iter()
        .filter(|item| item.role == "user")
        .map(|item| item.text.clone())
        .collect();
    Ok(ResponsesRequestView {
        instructions,
        wire_items,
        user_inputs,
    })
}

#[derive(Clone, Copy)]
enum SubmissionKind {
    Full,
    Delta,
    Omit,
    AppendFallback,
}

struct Step {
    title: &'static str,
    expected_state: &'static str,
    submission: SubmissionKind,
}

fn policy_prompt() -> String {
    format!("## {POLICY_TITLE}\n\n{POLICY_INSTRUCTION}")
}

fn complete_agent_state(phase: &str) -> String {
    let observation = if phase == "B" {
        "\n    <item>phase B observed</item>"
    } else {
        ""
    };
    format!(
        "<agent_state>\n  <objective>{STATE_OBJECTIVE}</objective>\n  <phase>{phase}</phase>\n  <observations>\n    <item>initialized</item>{observation}\n  </observations>\n</agent_state>"
    )
}

fn delta_to_b() -> String {
    String::from(
        "<agent_state rendering_mode=\"delta\">\n  <phase>B</phase>\n  <observations rendering_mode=\"delta\">\n    <insert>\n      <item>phase B observed</item>\n    </insert>\n  </observations>\n</agent_state>",
    )
}

fn append_fallback_to_a() -> String {
    String::from(
        "<agent_state rendering_mode=\"delta\">\n  <phase>A</phase>\n  <observations>\n    <item>initialized</item>\n  </observations>\n</agent_state>",
    )
}

fn expected_submission(step: &Step) -> Vec<String> {
    match step.submission {
        SubmissionKind::Full => vec![complete_agent_state(step.expected_state)],
        SubmissionKind::Delta => vec![delta_to_b()],
        SubmissionKind::Omit => Vec::new(),
        SubmissionKind::AppendFallback => vec![append_fallback_to_a()],
    }
}

fn complete_prompt(phase: &str) -> String {
    format!(
        "## System\n\n{}\n\n## User\n\n{}",
        policy_prompt(),
        complete_agent_state(phase)
    )
}

fn wire_item(role: &str, text: impl Into<String>) -> WireItemView {
    WireItemView {
        role: role.to_owned(),
        text: text.into(),
    }
}

struct AcceptanceHarness {
    components: ComponentHost<AcceptanceProps>,
    debug: DebugProviderPort,
    debug_capture: DebugPromptCapture,
    application: ApplicationHost<AsyncOpenAiResponsesProvider>,
    mock: ResponsesAcceptanceServer,
    renders: Arc<AtomicUsize>,
    completed_handlers: Arc<AtomicUsize>,
    expected_wire_items: Vec<WireItemView>,
}

impl AcceptanceHarness {
    async fn start() -> anyhow::Result<Self> {
        let mock = ResponsesAcceptanceServer::start(&RESPONSE_STATES).await?;
        let application = ApplicationHost::new(responses_provider(mock.api_base())?);
        let (debug, debug_capture) = DebugProviderPort::new();
        let renders = Arc::new(AtomicUsize::new(0));
        let completed_handlers = Arc::new(AtomicUsize::new(0));
        Ok(Self {
            components: ComponentHost::new(
                acceptance_application,
                AcceptanceProps {
                    renders: renders.clone(),
                    completed_handlers: completed_handlers.clone(),
                },
            ),
            debug,
            debug_capture,
            application,
            mock,
            renders,
            completed_handlers,
            expected_wire_items: Vec::new(),
        })
    }

    fn replace_provider(&mut self) -> anyhow::Result<()> {
        self.application = ApplicationHost::new(responses_provider(self.mock.api_base())?);
        self.expected_wire_items.clear();
        Ok(())
    }

    async fn render_and_dispatch(
        &mut self,
        report: &mut String,
        step_number: usize,
        step: &Step,
    ) -> anyhow::Result<Vec<String>> {
        let renders_before = self.renders.load(Ordering::SeqCst);
        let projection = self.components.render()?.projection().clone();
        let nodes = component_nodes(&projection);
        let full_prompt =
            capture_complete_prompt(&mut self.debug, &self.debug_capture, projection).await?;
        anyhow::ensure!(
            self.renders.load(Ordering::SeqCst) == renders_before + 1,
            "the explicit preview must perform exactly one Component render"
        );
        anyhow::ensure!(
            nodes == expected_component_nodes(),
            "Component node ownership or order drifted:\n{nodes}"
        );
        anyhow::ensure!(
            full_prompt == complete_prompt(step.expected_state),
            "{} complete Component prompt drifted",
            step.title
        );

        self.application
            .dispatch_llm_reaction(&mut self.components)
            .await?;
        anyhow::ensure!(
            self.renders.load(Ordering::SeqCst) == renders_before + 2,
            "one reaction must render once before Provider execution and never rerender after events"
        );
        anyhow::ensure!(
            self.completed_handlers.load(Ordering::SeqCst) == step_number,
            "reaction returned before its suspended event handler completed"
        );
        anyhow::ensure!(
            self.mock.request_count() == step_number,
            "one reaction must issue exactly one Provider request"
        );
        let body = self.mock.next_request().await?;
        let request = request_view(&body)?;
        anyhow::ensure!(
            request.instructions == policy_prompt(),
            "Responses instructions drifted from the complete policy POM"
        );
        let expected_new_items = expected_submission(step);
        let previous_user_count = self
            .expected_wire_items
            .iter()
            .filter(|item| item.role == "user")
            .count();
        anyhow::ensure!(
            request.user_inputs.len() >= previous_user_count,
            "Provider user history moved backwards"
        );
        let new_items = request.user_inputs[previous_user_count..].to_vec();
        anyhow::ensure!(
            new_items == expected_new_items,
            "{} submission mismatch:\nexpected {expected_new_items:#?}\nreceived {new_items:#?}",
            step.title
        );
        let mut expected_wire_items = self.expected_wire_items.clone();
        expected_wire_items.extend(
            expected_new_items
                .iter()
                .cloned()
                .map(|item| wire_item("user", item)),
        );
        anyhow::ensure!(
            request.wire_items == expected_wire_items,
            "{} Provider-private wire history drifted:\nexpected {expected_wire_items:#?}\nreceived {:#?}",
            step.title,
            request.wire_items
        );

        writeln!(
            report,
            "\n============================================================"
        )?;
        writeln!(report, "REACTION {step_number}: {}", step.title)?;
        writeln!(
            report,
            "============================================================"
        )?;
        writeln!(report, "ORDERED COMPONENT NODES")?;
        write!(report, "{nodes}")?;
        writeln!(report, "\nCOMPONENT FULL PROMPT")?;
        writeln!(report, "{full_prompt}")?;
        writeln!(
            report,
            "\nRESPONSES INSTRUCTIONS (provider-specific wire field)"
        )?;
        writeln!(report, "{}", request.instructions)?;
        writeln!(report, "\nPROVIDER SUBMISSION: {}", step.title)?;
        if new_items.is_empty() {
            writeln!(
                report,
                "  (no new user item; unchanged diff slot was omitted)"
            )?;
        } else {
            for item in &new_items {
                writeln!(report, "{item}")?;
            }
        }
        writeln!(
            report,
            "\nPROVIDER PRIVATE WIRE WINDOW: {} ordered item(s)",
            request.wire_items.len()
        )?;
        for (index, item) in request.wire_items.iter().enumerate() {
            writeln!(report, "  [{index}] {}", item.role.to_uppercase())?;
            for line in item.text.lines() {
                writeln!(report, "      {line}")?;
            }
        }
        let next_state = RESPONSE_STATES
            .get(step_number - 1)
            .copied()
            .unwrap_or(RESPONSE_STATES[RESPONSE_STATES.len() - 1]);
        writeln!(
            report,
            "SIGNAL UPDATE: suspended TextComplete(\"{next_state}\") handler completed before dispatch returned; no event-time rerender occurred."
        )?;

        self.expected_wire_items = expected_wire_items;
        self.expected_wire_items
            .push(wire_item("assistant", next_state));
        Ok(new_items)
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        self.mock.shutdown().await
    }
}

async fn run_acceptance() -> anyhow::Result<String> {
    let mut harness = AcceptanceHarness::start().await?;
    let mut report = String::from(
        "PROVIDER PORT / COMPONENT HOST VISUAL ACCEPTANCE\n\
         Component is complete business truth; Provider history and diff are private optimizations.\n\
         Every displayed Provider submission below is captured from an actual local Responses request.\n",
    );
    let steps = [
        Step {
            title: "FULL",
            expected_state: "A",
            submission: SubmissionKind::Full,
        },
        Step {
            title: "FIELD PATCH + APPEND INSERT",
            expected_state: "B",
            submission: SubmissionKind::Delta,
        },
        Step {
            title: "OMIT",
            expected_state: "B",
            submission: SubmissionKind::Omit,
        },
        Step {
            title: "APPEND FALLBACK FULL",
            expected_state: "A",
            submission: SubmissionKind::AppendFallback,
        },
        Step {
            title: "REPEATED DELTA",
            expected_state: "B",
            submission: SubmissionKind::Delta,
        },
    ];

    let mut first_delta = None;
    for (index, step) in steps.iter().enumerate() {
        let new_items = harness
            .render_and_dispatch(&mut report, index + 1, step)
            .await?;
        match step.title {
            "FIELD PATCH + APPEND INSERT" => first_delta = new_items.first().cloned(),
            "REPEATED DELTA" => anyhow::ensure!(
                new_items.first() == first_delta.as_ref(),
                "the repeated B delta must be appended even when its text matches history"
            ),
            _ => {}
        }
    }

    harness.replace_provider()?;
    let fresh = Step {
        title: "FRESH FULL",
        expected_state: "B",
        submission: SubmissionKind::Full,
    };
    let new_items = harness
        .render_and_dispatch(&mut report, steps.len() + 1, &fresh)
        .await?;
    anyhow::ensure!(
        new_items[0] == complete_agent_state("B"),
        "a fresh Provider must receive the complete current state"
    );
    anyhow::ensure!(
        harness.debug_capture.snapshots().len() == steps.len() + 1,
        "DebugProviderPort must capture every displayed projection"
    );

    visual_diff_shapes::append_to_report(&mut report, &mut harness.mock).await?;

    writeln!(
        report,
        "\n============================================================"
    )?;
    writeln!(report, "ACCEPTANCE PASSED")?;
    writeln!(
        report,
        "============================================================"
    )?;
    writeln!(
        report,
        "- Component prompts remained complete on every reaction."
    )?;
    writeln!(
        report,
        "- Structured agent_state deltas omitted the stable objective and used field-patch/append semantics."
    )?;
    writeln!(
        report,
        "- Atomic/single-field roots submitted the complete current value without a root delta/replace wrapper."
    )?;
    writeln!(
        report,
        "- Explicit replace remained local to its changed field inside a structured root."
    )?;
    writeln!(
        report,
        "- Repeated delta was appended instead of historical-deduplicated."
    )?;
    writeln!(
        report,
        "- A new Provider recovered from the current full projection."
    )?;

    harness.shutdown().await?;
    Ok(report)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    print!("{}", run_acceptance().await?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn report_exposes_the_frozen_component_and_provider_boundaries() {
        let report = run_acceptance().await.expect("acceptance run succeeds");

        for expected in [
            "## Visual acceptance policy",
            "COMPONENT FULL PROMPT",
            "RESPONSES INSTRUCTIONS (provider-specific wire field)",
            "PROVIDER SUBMISSION: FULL",
            "PROVIDER SUBMISSION: FIELD PATCH + APPEND INSERT",
            "PROVIDER SUBMISSION: OMIT",
            "PROVIDER SUBMISSION: APPEND FALLBACK FULL",
            "PROVIDER SUBMISSION: REPEATED DELTA",
            "PROVIDER SUBMISSION: FRESH FULL",
            "<phase>B</phase>",
            "<observations rendering_mode=\"delta\">",
            "DIFF SHAPE 1: ATOMIC / SINGLE-FIELD ROOT",
            "CURRENT PROVIDER SUBMISSION\n<current_state>A+</current_state>",
            "DIFF SHAPE 2: STRUCTURED FIELD-LOCAL REPLACE",
            "<phase rendering_mode=\"delta\">\n    <replace>\n      <phase>B</phase>",
            "Structured agent_state deltas omitted the stable objective",
            "SIGNAL UPDATE: suspended TextComplete",
            "ACCEPTANCE PASSED",
        ] {
            assert!(
                report.contains(expected),
                "missing report section {expected}"
            );
        }
        assert!(
            !report.contains("Separate exact Responses/Chat tests verify"),
            "the visual acceptance must show real Provider submissions instead of delegating proof"
        );
    }
}
