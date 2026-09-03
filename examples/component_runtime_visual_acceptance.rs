//! Component runtime acceptance for complete DOM and private provider history.

use std::{
    fmt::Write as _,
    panic::{resume_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ProviderIdentity, ReactionPort, RenderedProjection, TargetIdentity,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
    transcript::CanonicalInputItem,
};
use axum::http::StatusCode;
use futures::FutureExt;
use serde_json::Value;

#[path = "support/responses_acceptance_server.rs"]
mod responses_acceptance_server;
#[path = "support/visual_diff_shapes.rs"]
mod visual_diff_shapes;

use responses_acceptance_server::ResponsesAcceptanceServer;

const BINDING: &str = "component-runtime-visual-acceptance";
const RESPONSE_STATES: [&str; 6] = ["B", "B", "A", "B", "B", "B"];
const POLICY_TITLE: &str = "Visual acceptance policy";
const POLICY_INSTRUCTION: &str =
    "Return the next phase as plain text. This example uses a local deterministic provider.";
const STATE_OBJECTIVE: &str =
    "Keep complete business state in the Component and submit only useful semantic changes.";

#[derive(Clone)]
struct AcceptanceProps {
    initial_phase: String,
    completed_handlers: Arc<AtomicUsize>,
    exported_state: Arc<Mutex<Option<Signal<String>>>>,
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
}

struct TaskDropProbe(Arc<AtomicBool>);

impl Drop for TaskDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
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
fn acceptance_state(props: AcceptanceProps) -> Component {
    let initial_phase = props.initial_phase;
    let state = use_signal(move || initial_phase);
    *props
        .exported_state
        .lock()
        .expect("acceptance Signal export lock") = Some(state.clone());
    let current = state.with(Clone::clone).expect("mounted acceptance state");
    let completed_handlers = Arc::clone(&props.completed_handlers);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let state = state.clone();
        let completed_handlers = Arc::clone(&completed_handlers);
        async move {
            let TextTurnEvent::TextComplete(next) = event else {
                return Ok::<(), SignalAccessError>(());
            };
            tokio::task::yield_now().await;
            state.set(next.trim().to_owned())?;
            completed_handlers.fetch_add(1, Ordering::SeqCst);
            Ok::<(), SignalAccessError>(())
        }
    });

    let task_started = Arc::clone(&props.task_started);
    let task_dropped = Arc::clone(&props.task_dropped);
    use_future(move || async move {
        let _drop_probe = TaskDropProbe(task_dropped);
        task_started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
    });

    view! {
        #[diff(slot = "agent_state")]
        {agent_state(&current)}
    }
}

#[component]
fn acceptance_application(props: AcceptanceProps) -> Component {
    view! {
        acceptance_policy()
        acceptance_state(props)
    }
}

fn responses_provider(
    api_base: &str,
) -> anyhow::Result<(AsyncOpenAiResponsesProvider, TargetIdentity)> {
    let config = AsyncOpenAiTransportConfig::new(api_base, "visual-acceptance-token")?;
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING)?;
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>)?;
    let mut provider =
        AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options));
    let target = ReactionPort::declare(&mut provider)?.identity();
    Ok((provider, target))
}

fn projection_prompt(projection: &RenderedProjection) -> anyhow::Result<String> {
    let mut system = Vec::new();
    let mut user = Vec::new();
    for item in projection.nodes().iter().flat_map(|node| node.items()) {
        match item {
            CanonicalInputItem::Instruction { pom, .. } => {
                system.push(render_pom_document(pom)?);
            }
            CanonicalInputItem::Message { pom, .. } => {
                user.push(render_pom_document(pom)?);
            }
            _ => {}
        }
    }
    let mut sections = Vec::new();
    if !system.is_empty() {
        sections.push(format!("## System\n\n{}", system.join("\n\n")));
    }
    if !user.is_empty() {
        sections.push(format!("## User\n\n{}", user.join("\n\n")));
    }
    Ok(sections.join("\n\n"))
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

async fn wait_for_task_start(started: &AtomicBool) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Component task did not start"))
}

type ResponsesApplication = Application<AsyncOpenAiResponsesProvider>;

struct AcceptanceHarness {
    application: Option<ResponsesApplication>,
    target: TargetIdentity,
    completed_handlers: Arc<AtomicUsize>,
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
    expected_wire_items: Vec<WireItemView>,
    requests_per_reaction: Vec<usize>,
    requests_at_mount: usize,
    mount_was_passive: bool,
    handlers_completed_before_return: bool,
}

impl AcceptanceHarness {
    fn mount(mock: &ResponsesAcceptanceServer, phase: &str) -> anyhow::Result<Self> {
        let completed_handlers = Arc::new(AtomicUsize::new(0));
        let exported_state = Arc::new(Mutex::new(None));
        let task_started = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::new(AtomicBool::new(false));
        let (provider, target) = responses_provider(mock.api_base())?;
        let props = AcceptanceProps {
            initial_phase: phase.to_owned(),
            completed_handlers: Arc::clone(&completed_handlers),
            exported_state,
            task_started: Arc::clone(&task_started),
            task_dropped: Arc::clone(&task_dropped),
        };
        let requests_at_mount = mock.request_count();
        let application =
            Application::mount(move || acceptance_application(props.clone()), provider)?;
        Ok(Self {
            application: Some(application),
            target,
            completed_handlers,
            task_started,
            task_dropped,
            expected_wire_items: Vec::new(),
            requests_per_reaction: Vec::new(),
            requests_at_mount,
            mount_was_passive: true,
            handlers_completed_before_return: true,
        })
    }

    fn validate_current_mount(
        &mut self,
        mock: &ResponsesAcceptanceServer,
        phase: &str,
    ) -> anyhow::Result<()> {
        let application = self
            .application
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Responses Application was already consumed"))?;
        self.mount_was_passive &= mock.request_count() == self.requests_at_mount
            && projection_prompt(application.current_projection().projection())?
                == complete_prompt(phase);
        anyhow::ensure!(
            self.mount_was_passive,
            "mounted Application was not passive"
        );
        Ok(())
    }

    async fn react_and_capture(
        &mut self,
        mock: &mut ResponsesAcceptanceServer,
        report: &mut String,
        step_number: usize,
        step: &Step,
    ) -> anyhow::Result<Vec<String>> {
        let application = self
            .application
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Responses Application was already consumed"))?;
        let snapshot = application.current_projection();
        let nodes = component_nodes(snapshot.projection());
        let full_prompt = projection_prompt(snapshot.projection())?;
        anyhow::ensure!(
            !snapshot.is_dirty(),
            "current projection must be complete and clean"
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

        let requests_before = mock.request_count();
        let handlers_before = self.completed_handlers.load(Ordering::SeqCst);
        application.react().await?;
        let request_delta = mock.request_count() - requests_before;
        self.requests_per_reaction.push(request_delta);
        self.handlers_completed_before_return &=
            self.completed_handlers.load(Ordering::SeqCst) == handlers_before + 1;
        anyhow::ensure!(
            request_delta == 1,
            "one reaction must issue one mock request"
        );
        anyhow::ensure!(
            self.handlers_completed_before_return,
            "reaction returned before its suspended fact handler completed"
        );
        let body = mock.next_request().await?;
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
        let next_state = RESPONSE_STATES[step_number - 1];
        let current_after_handler =
            projection_prompt(application.current_projection().projection())?;
        anyhow::ensure!(
            current_after_handler == complete_prompt(next_state),
            "handler-published state was not reconciled before react returned"
        );
        writeln!(
            report,
            "SIGNAL UPDATE: suspended TextComplete(\"{next_state}\") handler completed before react returned; no event-time reaction occurred."
        )?;

        self.expected_wire_items = expected_wire_items;
        self.expected_wire_items
            .push(wire_item("assistant", next_state));
        Ok(new_items)
    }

    async fn shutdown_current(&mut self) -> anyhow::Result<()> {
        let application = self
            .application
            .take()
            .ok_or_else(|| anyhow::anyhow!("Responses Application was already consumed"))?;
        application.shutdown().await?;
        Ok(())
    }
}

struct SuccessEvidence {
    report: String,
    requests_per_reaction: Vec<usize>,
    mount_was_passive: bool,
    handlers_completed_before_return: bool,
    fresh_target_was_distinct: bool,
    old_task_dropped: bool,
    fresh_task_dropped: bool,
}

async fn run_success(mock: &mut ResponsesAcceptanceServer) -> anyhow::Result<SuccessEvidence> {
    let mut old = AcceptanceHarness::mount(mock, "A")?;
    let old_target = old.target;
    let old_operation = AssertUnwindSafe(async {
        wait_for_task_start(&old.task_started).await?;
        old.validate_current_mount(mock, "A")?;
        let mut report = String::from(
            "FRAME-NATIVE RESPONSES / APPLICATION VISUAL ACCEPTANCE\n\
             Component DOM is complete business truth; target wire history and diff are private optimizations.\n\
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
            let new_items = old
                .react_and_capture(mock, &mut report, index + 1, step)
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
        Ok::<_, anyhow::Error>(report)
    })
    .catch_unwind()
    .await;
    let old_shutdown = AssertUnwindSafe(old.shutdown_current())
        .catch_unwind()
        .await;
    let old_task_dropped = old.task_dropped.load(Ordering::Acquire);
    let old_result = combine_operation_cleanup(old_operation, old_shutdown);
    anyhow::ensure!(
        old_task_dropped,
        "old Responses Application task was not dropped by shutdown"
    );
    let mut report = old_result?;

    let mut fresh = AcceptanceHarness::mount(mock, "B")?;
    let fresh_operation = AssertUnwindSafe(async {
        wait_for_task_start(&fresh.task_started).await?;
        fresh.validate_current_mount(mock, "B")?;
        let fresh_target_was_distinct = old_target != fresh.target;
        anyhow::ensure!(
            fresh_target_was_distinct,
            "fresh Responses Application must own a distinct target identity/session"
        );
        writeln!(
            report,
            "\nFRESH TARGET: distinct target/session mounted from explicit phase B snapshot after old Application shutdown."
        )?;
        let step = Step {
            title: "FRESH FULL",
            expected_state: "B",
            submission: SubmissionKind::Full,
        };
        let new_items = fresh
            .react_and_capture(mock, &mut report, 6, &step)
            .await?;
        anyhow::ensure!(
            new_items == [complete_agent_state("B")],
            "fresh target must receive the complete explicit phase B snapshot"
        );
        Ok::<_, anyhow::Error>(fresh_target_was_distinct)
    })
    .catch_unwind()
    .await;
    let fresh_shutdown = AssertUnwindSafe(fresh.shutdown_current())
        .catch_unwind()
        .await;
    let fresh_task_dropped = fresh.task_dropped.load(Ordering::Acquire);
    let fresh_result = combine_operation_cleanup(fresh_operation, fresh_shutdown);
    anyhow::ensure!(
        fresh_task_dropped,
        "fresh Responses Application task was not dropped by shutdown"
    );
    let fresh_target_was_distinct = fresh_result?;

    visual_diff_shapes::append_to_report(&mut report, mock).await?;

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
    writeln!(report, "- Structured agent_state deltas omitted the stable objective and used field-patch/append semantics.")?;
    writeln!(report, "- Atomic/single-field roots submitted the complete current value without a root delta/replace wrapper.")?;
    writeln!(
        report,
        "- Explicit replace remained local to its changed field inside a structured root."
    )?;
    writeln!(
        report,
        "- Repeated delta was appended instead of historical-deduplicated."
    )?;
    writeln!(report, "- The old Application was consumed before a distinct target mounted from explicit phase B state.")?;

    let mut requests_per_reaction = old.requests_per_reaction;
    requests_per_reaction.extend(fresh.requests_per_reaction);
    Ok(SuccessEvidence {
        report,
        requests_per_reaction,
        mount_was_passive: old.mount_was_passive && fresh.mount_was_passive,
        handlers_completed_before_return: old.handlers_completed_before_return
            && fresh.handlers_completed_before_return,
        fresh_target_was_distinct,
        old_task_dropped,
        fresh_task_dropped,
    })
}

type PanicPayload = Box<dyn std::any::Any + Send + 'static>;
type BoundaryResult<T> = Result<anyhow::Result<T>, PanicPayload>;

fn combine_operation_cleanup<T>(
    operation: BoundaryResult<T>,
    cleanup: BoundaryResult<()>,
) -> anyhow::Result<T> {
    match (operation, cleanup) {
        (Err(original), _) => resume_unwind(original),
        (Ok(_), Err(cleanup_panic)) => resume_unwind(cleanup_panic),
        (Ok(Ok(value)), Ok(Ok(()))) => Ok(value),
        (Ok(Err(operation)), Ok(Ok(()))) => Err(operation),
        (Ok(Ok(_)), Ok(Err(cleanup))) => Err(cleanup),
        (Ok(Err(operation)), Ok(Err(cleanup))) => Err(operation.context(format!(
            "cleanup also failed after the operation error: {cleanup}"
        ))),
    }
}

struct ErrorCleanupEvidence {
    operation_error_and_request_observed: bool,
    task_dropped: bool,
    server_joined: bool,
}

async fn run_error_application(
    mock: &mut ResponsesAcceptanceServer,
) -> anyhow::Result<ErrorCleanupEvidence> {
    let task_started = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::new(AtomicBool::new(false));
    let (provider, _) = responses_provider(mock.api_base())?;
    let props = AcceptanceProps {
        initial_phase: String::from("A"),
        completed_handlers: Arc::new(AtomicUsize::new(0)),
        exported_state: Arc::new(Mutex::new(None)),
        task_started: Arc::clone(&task_started),
        task_dropped: Arc::clone(&task_dropped),
    };
    let mut application =
        Application::mount(move || acceptance_application(props.clone()), provider)?;
    let application_operation = AssertUnwindSafe(async {
        wait_for_task_start(&task_started).await?;
        let operation_failed = application.react().await.is_err();
        let request_received = if mock.request_count() == 1 {
            mock.next_request().await.is_ok()
        } else {
            false
        };
        Ok::<_, anyhow::Error>(operation_failed && request_received)
    })
    .catch_unwind()
    .await;
    let application_cleanup = AssertUnwindSafe(application.shutdown())
        .catch_unwind()
        .await
        .map(|result| result.map_err(Into::into));
    let task_dropped = task_dropped.load(Ordering::Acquire);
    let application_result = combine_operation_cleanup(application_operation, application_cleanup);
    anyhow::ensure!(
        task_dropped,
        "error Responses Application task was not dropped by shutdown"
    );
    let operation_error_and_request_observed = application_result?;
    Ok(ErrorCleanupEvidence {
        operation_error_and_request_observed,
        task_dropped,
        server_joined: false,
    })
}

async fn run_error_cleanup() -> anyhow::Result<ErrorCleanupEvidence> {
    let mut mock =
        ResponsesAcceptanceServer::start_failure(StatusCode::SERVICE_UNAVAILABLE).await?;
    let operation_result = AssertUnwindSafe(run_error_application(&mut mock))
        .catch_unwind()
        .await;
    let server_cleanup = AssertUnwindSafe(mock.shutdown()).catch_unwind().await;
    let server_joined = matches!(&server_cleanup, Ok(Ok(())));
    let mut evidence = combine_operation_cleanup(operation_result, server_cleanup)?;
    evidence.server_joined = server_joined;
    Ok(evidence)
}

struct AcceptanceEvidence {
    report: String,
    requests_per_reaction: Vec<usize>,
    mount_was_passive: bool,
    handlers_completed_before_return: bool,
    fresh_target_was_distinct: bool,
    old_task_dropped: bool,
    fresh_task_dropped: bool,
    success_server_joined: bool,
    error_task_dropped: bool,
    error_server_joined: bool,
    operation_error_and_request_observed: bool,
    success_cleanup_completed: bool,
    error_cleanup_completed: bool,
}

async fn run_acceptance() -> anyhow::Result<AcceptanceEvidence> {
    let mut mock = ResponsesAcceptanceServer::start(&RESPONSE_STATES).await?;
    let operation_result = AssertUnwindSafe(run_success(&mut mock))
        .catch_unwind()
        .await;
    let server_cleanup = AssertUnwindSafe(mock.shutdown()).catch_unwind().await;
    let success_server_joined = matches!(&server_cleanup, Ok(Ok(())));
    let success = combine_operation_cleanup(operation_result, server_cleanup)?;
    let error = run_error_cleanup().await?;
    let success_cleanup_completed =
        success.old_task_dropped && success.fresh_task_dropped && success_server_joined;
    let error_cleanup_completed =
        error.operation_error_and_request_observed && error.task_dropped && error.server_joined;
    Ok(AcceptanceEvidence {
        report: success.report,
        requests_per_reaction: success.requests_per_reaction,
        mount_was_passive: success.mount_was_passive,
        handlers_completed_before_return: success.handlers_completed_before_return,
        fresh_target_was_distinct: success.fresh_target_was_distinct,
        old_task_dropped: success.old_task_dropped,
        fresh_task_dropped: success.fresh_task_dropped,
        success_server_joined,
        error_task_dropped: error.task_dropped,
        error_server_joined: error.server_joined,
        operation_error_and_request_observed: error.operation_error_and_request_observed,
        success_cleanup_completed,
        error_cleanup_completed,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let acceptance = run_acceptance().await?;
    anyhow::ensure!(acceptance.requests_per_reaction == vec![1; 6]);
    anyhow::ensure!(acceptance.mount_was_passive);
    anyhow::ensure!(acceptance.handlers_completed_before_return);
    anyhow::ensure!(acceptance.fresh_target_was_distinct);
    anyhow::ensure!(acceptance.old_task_dropped);
    anyhow::ensure!(acceptance.fresh_task_dropped);
    anyhow::ensure!(acceptance.success_server_joined);
    anyhow::ensure!(acceptance.error_task_dropped);
    anyhow::ensure!(acceptance.error_server_joined);
    anyhow::ensure!(acceptance.operation_error_and_request_observed);
    anyhow::ensure!(acceptance.success_cleanup_completed);
    anyhow::ensure!(acceptance.error_cleanup_completed);
    print!("{}", acceptance.report);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    fn panic_payload(message: &'static str) -> PanicPayload {
        Box::new(message)
    }

    fn panic_message(payload: PanicPayload) -> &'static str {
        *payload
            .downcast::<&'static str>()
            .expect("test panic payload remains a static string")
    }

    #[test]
    fn cleanup_boundary_preserves_the_original_operation_panic() {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            combine_operation_cleanup::<()>(
                Err(panic_payload("operation panic")),
                Err(panic_payload("cleanup panic")),
            )
        }))
        .expect_err("the original operation panic must resume");

        assert_eq!(panic_message(panic), "operation panic");
    }

    #[test]
    fn cleanup_boundary_resumes_a_cleanup_only_panic() {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            combine_operation_cleanup::<()>(Ok(Ok(())), Err(panic_payload("cleanup panic")))
        }))
        .expect_err("a cleanup-only panic must resume");

        assert_eq!(panic_message(panic), "cleanup panic");
    }

    #[test]
    fn cleanup_boundary_propagates_an_ordinary_operation_error_after_cleanup() {
        let error = combine_operation_cleanup::<()>(
            Ok(Err(anyhow::anyhow!("operation error"))),
            Ok(Ok(())),
        )
        .expect_err("ordinary operation error must propagate");

        assert_eq!(error.to_string(), "operation error");
    }

    #[tokio::test]
    async fn native_responses_application_exposes_component_and_wire_boundaries() {
        let acceptance = run_acceptance().await.expect("acceptance run succeeds");

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
            "FRESH TARGET: distinct target/session",
            "ACCEPTANCE PASSED",
        ] {
            assert!(
                acceptance.report.contains(expected),
                "missing report section {expected}"
            );
        }
        assert_eq!(acceptance.requests_per_reaction, vec![1; 6]);
        assert!(acceptance.mount_was_passive);
        assert!(acceptance.handlers_completed_before_return);
        assert!(acceptance.fresh_target_was_distinct);
        assert!(acceptance.old_task_dropped);
        assert!(acceptance.fresh_task_dropped);
        assert!(acceptance.success_server_joined);
        assert!(acceptance.error_task_dropped);
        assert!(acceptance.error_server_joined);
        assert!(acceptance.operation_error_and_request_observed);
        assert!(acceptance.success_cleanup_completed);
        assert!(acceptance.error_cleanup_completed);
    }
}
