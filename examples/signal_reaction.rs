//! Provider facts update a retained Signal for the next explicit reaction.

use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{Application, RenderedProjection},
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};

#[path = "support/scripted_provider.rs"]
mod scripted_provider;

use scripted_provider::{AcceptedFrame, ScriptedProvider, ScriptedReaction};

#[derive(Clone)]
struct ReviewProps {
    document_id: String,
    delta_statuses: Arc<Mutex<Vec<ReviewStatus>>>,
    completed_handlers: Arc<AtomicUsize>,
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewStatus {
    Pending,
    Approved,
    ChangesRequested,
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("pending"),
            Self::Approved => formatter.write_str("approved"),
            Self::ChangesRequested => formatter.write_str("changes-requested"),
        }
    }
}

fn completed_status(text: &str) -> ReviewStatus {
    match text.trim() {
        "approved" => ReviewStatus::Approved,
        _ => ReviewStatus::ChangesRequested,
    }
}

struct TaskDropProbe(Arc<AtomicBool>);

impl Drop for TaskDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[component]
fn review_policy() -> Component {
    view! {
        #[system_once]
        review_policy { "Return either approved or changes-requested." }
    }
}

#[component]
fn document_request(document_id: String) -> Component {
    view! {
        document_request { document_id { "{document_id}" } }
    }
}

#[component]
fn review_state(status: Signal<ReviewStatus>) -> Component {
    let current = status.with(|value| *value).expect("mounted review Signal");
    view! {
        review_status { "{current}" }
    }
}

#[component]
fn review_turn(props: ReviewProps) -> Component {
    let status = use_signal(|| ReviewStatus::Pending);
    let handler_status = status.clone();
    let delta_statuses = Arc::clone(&props.delta_statuses);
    let completed_handlers = Arc::clone(&props.completed_handlers);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let status = handler_status.clone();
        let delta_statuses = Arc::clone(&delta_statuses);
        let completed_handlers = Arc::clone(&completed_handlers);
        async move {
            match event {
                TextTurnEvent::TextDelta(_) => {
                    let current = status.with(|value| *value)?;
                    delta_statuses
                        .lock()
                        .expect("delta status capture lock")
                        .push(current);
                }
                TextTurnEvent::TextComplete(text) => {
                    tokio::task::yield_now().await;
                    status.set(completed_status(&text))?;
                    completed_handlers.fetch_add(1, Ordering::SeqCst);
                }
            }
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
        review_policy()
        document_request(props.document_id)
        review_state(status)
    }
}

#[component]
fn review_application(props: ReviewProps) -> Component {
    view! {
        review_turn(props)
    }
}

fn projection_text(projection: &RenderedProjection) -> anyhow::Result<String> {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom)),
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|documents| documents.join("\n"))
        .map_err(Into::into)
}

struct ReactionEvidence {
    submissions_at_mount: usize,
    frames: Vec<AcceptedFrame>,
    first_handler_completed_before_return: bool,
    delta_statuses: Vec<ReviewStatus>,
    current_projection_text: String,
    shutdown_completed: bool,
    component_task_dropped: bool,
}

#[cfg(test)]
struct OperationErrorEvidence {
    operation_failed: bool,
    operation_reason: Option<agentview::component::execution::ApplicationFaultReason>,
    shutdown_completed: bool,
    component_task_dropped: bool,
}

fn review_props() -> ReviewProps {
    ReviewProps {
        document_id: "proposal-7".to_owned(),
        delta_statuses: Arc::new(Mutex::new(Vec::new())),
        completed_handlers: Arc::new(AtomicUsize::new(0)),
        task_started: Arc::new(AtomicBool::new(false)),
        task_dropped: Arc::new(AtomicBool::new(false)),
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

async fn run_reactions_with(first_reaction: ScriptedReaction) -> anyhow::Result<ReactionEvidence> {
    let props = review_props();
    let delta_statuses = Arc::clone(&props.delta_statuses);
    let completed_handlers = Arc::clone(&props.completed_handlers);
    let task_started = Arc::clone(&props.task_started);
    let (provider, capture) = ScriptedProvider::new([first_reaction, ScriptedReaction::empty()])?;
    let root_props = props.clone();
    let mut application =
        Application::mount(move || review_application(root_props.clone()), provider)?;
    let operation_result = async {
        let submissions_at_mount = capture.submission_count();
        wait_for_task_start(&task_started).await?;
        application.react().await?;
        let first_handler_completed_before_return = completed_handlers.load(Ordering::SeqCst) == 1;
        application.react().await?;
        let current_projection_text =
            projection_text(application.current_projection().projection())?;
        let delta_statuses = delta_statuses
            .lock()
            .map_err(|_| anyhow::anyhow!("delta status capture lock poisoned"))?
            .clone();
        Ok::<_, anyhow::Error>((
            submissions_at_mount,
            first_handler_completed_before_return,
            capture.frames(),
            delta_statuses,
            current_projection_text,
        ))
    }
    .await;
    let shutdown_result = application.shutdown().await;
    let component_task_dropped = props.task_dropped.load(Ordering::Acquire);
    let (
        submissions_at_mount,
        first_handler_completed_before_return,
        frames,
        delta_statuses,
        current_projection_text,
    ) = resolve_after_shutdown(operation_result, shutdown_result)?;

    Ok(ReactionEvidence {
        submissions_at_mount,
        frames,
        first_handler_completed_before_return,
        delta_statuses,
        current_projection_text,
        shutdown_completed: true,
        component_task_dropped,
    })
}

fn resolve_after_shutdown<T>(
    operation_result: anyhow::Result<T>,
    shutdown_result: Result<(), agentview::component::execution::ApplicationFault>,
) -> anyhow::Result<T> {
    match (operation_result, shutdown_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(shutdown)) => Err(shutdown.into()),
        (Err(operation), Err(shutdown)) => Err(operation.context(format!(
            "Application shutdown also failed after the operation error: {shutdown}"
        ))),
    }
}

async fn run_reactions() -> anyhow::Result<ReactionEvidence> {
    run_reactions_with(ScriptedReaction::text(["app", "roved"])).await
}

#[cfg(test)]
async fn run_operation_error() -> anyhow::Result<OperationErrorEvidence> {
    let props = review_props();
    let task_started = Arc::clone(&props.task_started);
    let task_dropped = Arc::clone(&props.task_dropped);
    let (provider, _) = ScriptedProvider::new([ScriptedReaction::mismatched_text_seal(
        ["app", "roved"],
        "changes-requested",
    )])?;
    let root_props = props.clone();
    let mut application =
        Application::mount(move || review_application(root_props.clone()), provider)?;
    let operation_result = async {
        wait_for_task_start(&task_started).await?;
        let operation = application.react().await;
        Ok::<_, anyhow::Error>((
            operation.is_err(),
            operation.err().map(|fault| fault.reason()),
        ))
    }
    .await;
    let shutdown_result = application.shutdown().await;
    let (operation_failed, operation_reason) =
        resolve_after_shutdown(operation_result, shutdown_result)?;
    Ok(OperationErrorEvidence {
        operation_failed,
        operation_reason,
        shutdown_completed: true,
        component_task_dropped: task_dropped.load(Ordering::Acquire),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let evidence = run_reactions().await?;
    println!(
        "reactions={} mount_submissions={} first_handler_complete={} delta_observations={} second_frame_approved={} current_approved={} shutdown={} task_dropped={} bases={:?}",
        evidence.frames.len(),
        evidence.submissions_at_mount,
        evidence.first_handler_completed_before_return,
        evidence.delta_statuses.len(),
        evidence.frames[1]
            .text
            .contains("<review_status>approved</review_status>"),
        evidence.current_projection_text.contains("<review_status>approved</review_status>"),
        evidence.shutdown_completed,
        evidence.component_task_dropped,
        evidence
            .frames
            .iter()
            .map(|frame| frame.basis)
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use agentview::component::execution::{
        ApplicationFaultReason, FrameBasis, ReactionAdmissionReason,
    };

    use super::*;

    #[tokio::test]
    async fn mount_is_passive_and_two_explicit_reactions_are_full_then_delta() {
        let evidence = run_reactions().await.expect("scripted reactions complete");

        assert_eq!(evidence.submissions_at_mount, 0);
        assert_eq!(evidence.frames.len(), 2);
        assert_eq!(evidence.frames[0].basis, FrameBasis::Full);
        assert!(matches!(evidence.frames[1].basis, FrameBasis::DeltaFrom(_)));
        assert!(evidence.first_handler_completed_before_return);
        assert!(evidence.frames[0]
            .text
            .contains("<review_status>pending</review_status>"));
        assert!(evidence.frames[1]
            .text
            .contains("<review_status>approved</review_status>"));
        assert!(evidence
            .current_projection_text
            .contains("<review_status>approved</review_status>"));
        assert!(evidence.shutdown_completed);
        assert!(evidence.component_task_dropped);
    }

    #[tokio::test]
    async fn deltas_do_not_publish_a_terminal_review_decision() {
        let evidence = run_reactions_with(ScriptedReaction::text(["app", "roved"]))
            .await
            .expect("streamed review reaction completes");

        assert_eq!(evidence.delta_statuses, vec![ReviewStatus::Pending; 2]);
        assert!(evidence.frames[1]
            .text
            .contains("<review_status>approved</review_status>"));
    }

    #[tokio::test]
    async fn invalid_completed_decision_requests_changes() {
        let evidence = run_reactions_with(ScriptedReaction::sealed("undecided"))
            .await
            .expect("invalid completed review reaction completes");

        assert!(evidence.frames[1]
            .text
            .contains("<review_status>changes-requested</review_status>"));
    }

    #[tokio::test]
    async fn operation_error_still_consumes_application_and_cleans_up_tasks() {
        let evidence = run_operation_error()
            .await
            .expect("operation-error fixture cleans up");

        assert!(evidence.operation_failed);
        assert_eq!(
            evidence.operation_reason,
            Some(ApplicationFaultReason::Admission(
                ReactionAdmissionReason::TextSealMismatch
            ))
        );
        assert!(evidence.shutdown_completed);
        assert!(evidence.component_task_dropped);
    }
}
