#[path = "support/pom.rs"]
mod pom_support;

use std::{
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, Frame, FrameCapabilities, FrameConstraints, FrameProfile,
            NoulAnswer, PromptSource, PromptSourceKind, ProviderFact, ProviderFactStream,
            ProviderOutputKey, ProviderToolCall, QuestionOutcome, ReactionPort, ReactionPortFault,
            RenderedProjection, SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
            ToolDefinition,
        },
        prelude::*,
        ComponentHost,
    },
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use pom_support::{combine, projection_document, InspectPom};
use schemars::JsonSchema;
use serde::Deserialize;

fn component_id(projection: &RenderedProjection, name: &str) -> String {
    projection
        .nodes()
        .iter()
        .find(|node| node.identity().contains(name))
        .unwrap_or_else(|| panic!("missing mounted Component {name}"))
        .identity()
        .to_owned()
}

#[component]
fn profile_note(value: String) -> Component {
    view! { note { "{value}" } }
}

#[component]
fn profile_nested() -> Component {
    view! {
        introduction { "Inspect each note independently." }
        profile_note("你好🙂".to_owned())
        profile_note("A longer second note with its own Component identity.".to_owned())
        next_action { "Compare the notes before choosing an action." }
    }
}

#[test]
fn nested_instances_keep_exclusive_sources_and_additive_inclusive_totals() {
    let mut host = ComponentHost::new_root(|_| profile_nested(), ());
    let projection = host.render().unwrap().projection().clone();
    let prompt = projection.render_prompt_with_sources().unwrap();
    let profile = prompt.profile_bytes();
    let parent_id = component_id(&projection, "profile_nested");
    let notes = profile
        .components()
        .iter()
        .filter(|component| component.identity().contains("profile_note"))
        .collect::<Vec<_>>();

    assert_eq!(notes.len(), 2);
    assert_ne!(notes[0].identity(), notes[1].identity());
    assert_eq!(notes[0].parent(), Some(parent_id.as_str()));
    assert_eq!(notes[1].parent(), Some(parent_id.as_str()));
    assert!(notes[0].self_bytes() > 0);
    assert!(notes[1].self_bytes() > notes[0].self_bytes());
    assert_eq!(notes[0].self_bytes(), notes[0].inclusive_bytes());
    assert_eq!(notes[1].self_bytes(), notes[1].inclusive_bytes());

    let parent = profile
        .components()
        .iter()
        .find(|component| component.identity() == parent_id)
        .unwrap();
    assert!(parent.self_bytes() > 0);
    assert_eq!(
        parent.inclusive_bytes(),
        parent.self_bytes() + notes.iter().map(|note| note.self_bytes()).sum::<usize>()
    );
    let component_total = profile
        .components()
        .iter()
        .map(|component| component.self_bytes())
        .sum::<usize>();
    assert_eq!(
        profile.total_bytes(),
        component_total + profile.framework_bytes() + profile.unattributed_bytes()
    );
    assert_eq!(profile.unattributed_bytes(), 0);
    assert!(profile.framework_bytes() > 0);
    let unicode_source_bytes = prompt
        .source_spans()
        .iter()
        .filter(|span| span.source().component_identity() == Some(notes[0].identity()))
        .map(|span| span.byte_range().len())
        .sum::<usize>();
    assert_eq!(notes[0].self_bytes(), unicode_source_bytes);
    assert!(unicode_source_bytes >= "你好🙂".len());
    assert_eq!(
        profile.total_bytes(),
        prompt
            .source_spans()
            .iter()
            .map(|span| span.byte_range().len())
            .sum::<usize>()
    );
    assert!(projection_document(&projection).has_field("note", "你好🙂"));
}

#[component]
fn profile_policy() -> Component {
    view! {
        "Use the current view and declared actions."
        policy { "Record actual action outcomes in the next observation." }
    }
}

#[component]
fn profile_ephemeral() -> Component {
    view! { "Current request: review the displayed notes." }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[component]
fn profile_catalogs() -> Component {
    view! {
        #[developer(ephemeral, repeat)]
        profile_ephemeral()

        #[system_once]
        profile_policy()

        observation { "Both notes are available for review." }
        NativeToolCall {
            name: "inspect_notes",
            description: "Inspect the current notes before deciding.",
            on_call: |_context, _input: NoArguments| Ok::<_, Infallible>("Notes inspected."),
        }
        Decision {
            name: "notes_ready",
            question: Noul::new("Are the displayed notes ready for review?"),
            on_result: |_context, _result: QuestionOutcome<NoulAnswer>| Ok::<_, Infallible>(()),
        }
    }
}

fn source_owner(source: &PromptSource) -> Option<(&str, PromptSourceKind)> {
    match source {
        PromptSource::Component { identity, kind } => Some((identity, *kind)),
        _ => None,
    }
}

#[test]
fn merged_system_catalogs_and_relocated_ephemeral_content_retain_their_owners() {
    let mut host = ComponentHost::new_root(|_| profile_catalogs(), ());
    let projection = host.render().unwrap().projection().clone();
    let prompt = projection.render_prompt_with_sources().unwrap();
    let policy = component_id(&projection, "profile_policy");
    let declaring = component_id(&projection, "profile_catalogs");
    let decision = component_id(&projection, "decision_component");
    let tool = component_id(&projection, "NativeToolCall(inspect_notes)");
    let ephemeral = component_id(&projection, "profile_ephemeral");

    for (owner, kind) in [
        (&policy, PromptSourceKind::System),
        (&tool, PromptSourceKind::NativeTool),
        (&decision, PromptSourceKind::Decision),
        (&ephemeral, PromptSourceKind::Content),
    ] {
        assert!(prompt
            .source_spans()
            .iter()
            .any(|span| { source_owner(span.source()) == Some((owner.as_str(), kind)) }));
    }

    let owned_spans = prompt
        .source_spans()
        .iter()
        .filter_map(|span| source_owner(span.source()).map(|(owner, _)| (owner, span.byte_range())))
        .collect::<Vec<_>>();
    let ephemeral_start = owned_spans
        .iter()
        .filter(|(owner, _)| *owner == ephemeral)
        .map(|(_, range)| range.start)
        .min()
        .unwrap();
    assert!(owned_spans
        .iter()
        .filter(|(owner, _)| *owner != ephemeral)
        .all(|(_, range)| range.end <= ephemeral_start));

    let profile = prompt.profile_bytes();
    assert_eq!(
        profile
            .components()
            .iter()
            .find(|component| component.identity() == decision)
            .unwrap()
            .parent(),
        Some(declaring.as_str())
    );
    assert_eq!(profile.unattributed_bytes(), 0);
    for owner in [policy, declaring, decision, tool, ephemeral] {
        assert!(
            profile
                .components()
                .iter()
                .find(|component| component.identity() == owner)
                .unwrap()
                .self_bytes()
                > 0
        );
    }
}

#[test]
fn externally_supplied_tool_definitions_remain_explicitly_unattributed() {
    let projection = RenderedProjection::with_native_tools(
        Vec::new(),
        vec![ToolDefinition::new(
            "external_action",
            "A declaration supplied without a mounted Component.",
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap()],
    )
    .unwrap();
    let prompt = projection.render_prompt_with_sources().unwrap();
    let profile = prompt.profile_bytes();

    assert!(profile.components().is_empty());
    assert!(profile.unattributed_bytes() > 0);
    assert!(prompt.source_spans().iter().any(|span| matches!(
        span.source(),
        PromptSource::Unattributed {
            kind: PromptSourceKind::NativeTool
        }
    )));
    assert_eq!(
        profile.total_bytes(),
        profile.framework_bytes() + profile.unattributed_bytes()
    );
}

const ACTION_FEEDBACK: &str =
    "Inspection completed: both notes are available, their owners are identified, and review can continue.";

#[component]
fn profile_interaction() -> Component {
    let feedback = use_signal(|| String::from("pending"));
    let current = feedback.with(Clone::clone).unwrap();
    view! {
        inspection_status { "{current}" }
        next_action { "Call inspect, then inspect its result before continuing." }
        NativeToolCall {
            name: "inspect",
            description: "Inspect the notes and retain the actual result in the current view.",
            on_call: move |_context, _input: NoArguments| {
                feedback.set(String::from(ACTION_FEEDBACK))?;
                Ok::<_, SignalAccessError>(ACTION_FEEDBACK)
            },
        }
    }
}

#[derive(Clone)]
struct SubmittedObservation {
    items: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    tools: Vec<String>,
}

struct InspectPort {
    declaration: TargetDeclaration,
    frames: Arc<Mutex<Vec<SubmittedObservation>>>,
}

impl InspectPort {
    fn new() -> (Self, Arc<Mutex<Vec<SubmittedObservation>>>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                declaration: TargetDeclaration::full(
                    TargetIdentity::new(NonZeroU128::new(0x701).unwrap()),
                    TargetEpoch::new(NonZeroU64::MIN),
                    FrameProfile::new(
                        FrameConstraints {
                            max_frame_bytes: 262_144,
                            max_component_bytes: 65_536,
                            context_window: ContextWindow::NotApplicable,
                            reserved_output_tokens: None,
                        },
                        FrameCapabilities::new(true).with_native_tool_calls(true),
                    ),
                ),
                frames: frames.clone(),
            },
            frames,
        )
    }
}

#[async_trait]
impl ReactionPort for InspectPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        let is_first = {
            let mut frames = self.frames.lock().unwrap();
            let is_first = frames.is_empty();
            frames.push(SubmittedObservation {
                items: frame.submission().projection().items().to_vec(),
                staged: frame.submission().staged_inputs().to_vec(),
                tools: frame
                    .submission()
                    .tools()
                    .definitions()
                    .iter()
                    .map(|tool| tool.name().to_owned())
                    .collect(),
            });
            is_first
        };
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let mut facts = Vec::new();
        if is_first {
            facts.push(ProviderFact::ToolCall {
                output: ProviderOutputKey::new(0),
                ordinal: 0,
                call: ProviderToolCall::new("inspect-call", "inspect", "{}").unwrap(),
            });
        }
        facts.push(ProviderFact::ReactionCompleted { primary_text: None });
        Ok(Box::pin(futures::stream::iter(facts.into_iter().map(Ok))))
    }
}

fn observation_document(observation: &SubmittedObservation) -> agentview::pom::ResolvedDocument {
    combine(observation.items.iter().filter_map(|item| match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            Some(pom.clone())
        }
        _ => None,
    }))
}

#[tokio::test]
async fn native_action_feedback_refreshes_component_costs_and_reaches_the_next_frame() {
    let (port, frames) = InspectPort::new();
    let mut application = Application::mount(profile_interaction, port).unwrap();
    let initial_snapshot = application.current_projection();
    let owner = component_id(initial_snapshot.projection(), "profile_interaction");
    let initial = initial_snapshot
        .render_prompt_with_sources()
        .unwrap()
        .profile_bytes();
    let initial_owner = initial
        .components()
        .iter()
        .find(|component| component.identity() == owner)
        .unwrap();

    application.react(false).unwrap().await.unwrap();
    let updated_snapshot = application.current_projection();
    assert!(projection_document(updated_snapshot.projection())
        .has_field("inspection_status", ACTION_FEEDBACK));
    let updated = updated_snapshot
        .render_prompt_with_sources()
        .unwrap()
        .profile_bytes();
    let updated_owner = updated
        .components()
        .iter()
        .find(|component| component.identity() == owner)
        .unwrap();
    assert!(updated_owner.self_bytes() > initial_owner.self_bytes());

    application.react(false).unwrap().await.unwrap();
    let observations = frames.lock().unwrap().clone();
    assert_eq!(observations.len(), 2);
    assert!(observation_document(&observations[0]).has_field("inspection_status", "pending"));
    assert!(observation_document(&observations[1]).has_field("inspection_status", ACTION_FEEDBACK));
    assert_eq!(observations[1].tools, ["inspect"]);
    assert!(observations[1].staged.iter().any(|item| matches!(
        item,
        CanonicalInputItem::ToolResult { call_id, .. } if call_id == "inspect-call"
    )));
    application.shutdown().await.unwrap();
}
