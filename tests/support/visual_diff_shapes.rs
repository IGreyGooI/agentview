use std::{
    fmt::Write as _,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{
            Application, Frame, ProviderFactStream, ReactionPort, ReactionPortFault, SubmitFault,
            TargetDeclaration,
        },
        prelude::*,
    },
    pom::{ContentNode, ContentRef, Document, ResolvedDocument},
    pom_resolution::resolve_system_document,
    transcript::CanonicalInputItem,
    AgentView as _,
};
use futures::FutureExt;

use super::pom_support::{self, InspectPom};
use super::{
    combine_operation_cleanup, projection_prompt, request_view, responses_provider,
    ResponsesAcceptanceServer,
};

struct CaptureSubmissions<P> {
    inner: P,
    documents: Arc<Mutex<Vec<ResolvedDocument>>>,
}

#[async_trait::async_trait]
impl<P: ReactionPort> ReactionPort for CaptureSubmissions<P> {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.inner.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let document =
            pom_support::combine(frame.submission().projection().items().iter().filter_map(
                |item| match item {
                    CanonicalInputItem::Instruction { pom, .. }
                    | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
                    _ => None,
                },
            ));
        let stream = self.inner.submit(frame).await?;
        self.documents
            .lock()
            .expect("submitted POM capture lock")
            .push(document);
        Ok(stream)
    }

    async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }
}

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
    submitted_pom: Vec<ResolvedDocument>,
    current_component_pom: ResolvedDocument,
}

async fn capture_atomic(mock: &mut ResponsesAcceptanceServer) -> anyhow::Result<DiffShapeCapture> {
    let requests_before = mock.request_count();
    let exported = Arc::new(Mutex::new(None));
    let props = AtomicDiffProps {
        exported: Arc::clone(&exported),
    };
    let (provider, _) = responses_provider(mock.api_base())?;
    let submitted_pom = Arc::new(Mutex::new(Vec::new()));
    let provider = CaptureSubmissions {
        inner: provider,
        documents: Arc::clone(&submitted_pom),
    };
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
            submitted_pom: submitted_pom
                .lock()
                .expect("submitted POM capture lock")
                .clone(),
            current_component_pom: pom_support::projection_document(
                application.current_projection().projection(),
            ),
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
    let submitted_pom = Arc::new(Mutex::new(Vec::new()));
    let provider = CaptureSubmissions {
        inner: provider,
        documents: Arc::clone(&submitted_pom),
    };
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
            submitted_pom: submitted_pom
                .lock()
                .expect("submitted POM capture lock")
                .clone(),
            current_component_pom: pom_support::projection_document(
                application.current_projection().projection(),
            ),
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
    anyhow::ensure!(
        atomic.submitted_pom
            == [
                pom_support::text_element("current_state", "A"),
                pom_support::text_element("current_state", "A+"),
            ]
    );
    anyhow::ensure!(
        atomic.current_component_pom == pom_support::text_element("current_state", "A+")
    );
    write_capture(report, 1, "ATOMIC / SINGLE-FIELD ROOT", &atomic)?;

    let replace = capture_replace(mock).await?;
    let expected_baseline = resolve_system_document(Document::from_xml(
        ReplaceStateView {
            objective: "Keep this field stable.",
            phase: "A".into(),
        }
        .build_root()?,
    ));
    anyhow::ensure!(replace.submitted_pom.first() == Some(&expected_baseline));
    anyhow::ensure!(replace
        .current_component_pom
        .has_field("objective", "Keep this field stable."));
    anyhow::ensure!(replace.current_component_pom.has_field("phase", "B"));

    let current_delta = replace
        .submitted_pom
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("replace diff must submit its changed POM"))?;
    let children = current_delta.children().iter().collect::<Vec<_>>();
    let [ContentRef::Node(ContentNode::Xml(root))] = children.as_slice() else {
        anyhow::bail!("replace diff must submit one structured root");
    };
    anyhow::ensure!(root.name().as_str() == "replace_state");
    anyhow::ensure!(pom_support::attribute(root, "rendering_mode") == Some("delta"));
    anyhow::ensure!(!current_delta.has_element("objective"));
    let fields = root.children().iter().collect::<Vec<_>>();
    let [ContentRef::Node(ContentNode::Xml(phase))] = fields.as_slice() else {
        anyhow::bail!("replace diff must affect exactly one field");
    };
    anyhow::ensure!(phase.name().as_str() == "phase");
    anyhow::ensure!(pom_support::attribute(phase, "rendering_mode") == Some("delta"));
    let expected_operation =
        pom_support::nested_element("replace", &[pom_support::text_element("phase", "B")]);
    anyhow::ensure!(
        phase.children().iter().collect::<Vec<_>>()
            == expected_operation.children().iter().collect::<Vec<_>>()
    );
    write_capture(report, 2, "STRUCTURED FIELD-LOCAL REPLACE", &replace)
}
