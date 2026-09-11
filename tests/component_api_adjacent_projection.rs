use std::{
    collections::VecDeque,
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile,
            ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault,
            SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity, ToolDefinition,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;

#[derive(Debug, Clone)]
struct CapturedFrame {
    basis: FrameBasis,
    replay: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    tools: Vec<ToolDefinition>,
}

#[derive(Clone, Default)]
struct FrameCapture {
    frames: Arc<Mutex<Vec<CapturedFrame>>>,
    pre_handoff_started: Arc<Notify>,
}

impl FrameCapture {
    fn frames(&self) -> Vec<CapturedFrame> {
        self.frames.lock().expect("frame capture lock").clone()
    }

    fn record(&self, frame: &Frame) {
        self.frames
            .lock()
            .expect("frame capture lock")
            .push(CapturedFrame {
                basis: frame.basis(),
                replay: frame.submission().replay().to_vec(),
                projection: frame.submission().projection().items().to_vec(),
                tools: frame.submission().tools().definitions().to_vec(),
            });
    }
}

#[derive(Clone, Default)]
struct SubmitControl {
    block_next_submit: Arc<AtomicBool>,
    force_next_full: Arc<AtomicBool>,
    scripted_facts: Arc<Mutex<VecDeque<Vec<ProviderFact>>>>,
}

impl SubmitControl {
    fn block_next_submit(&self) {
        self.block_next_submit.store(true, Ordering::SeqCst);
    }

    fn force_next_full(&self) {
        self.force_next_full.store(true, Ordering::SeqCst);
    }

    fn enqueue_facts(&self, facts: Vec<ProviderFact>) {
        self.scripted_facts
            .lock()
            .expect("scripted provider facts lock")
            .push_back(facts);
    }

    fn take_facts(&self) -> Vec<ProviderFact> {
        self.scripted_facts
            .lock()
            .expect("scripted provider facts lock")
            .pop_front()
            .unwrap_or_else(|| vec![ProviderFact::ReactionCompleted { primary_text: None }])
    }
}

struct CapturingPort {
    declaration: TargetDeclaration,
    capture: FrameCapture,
    control: SubmitControl,
}

impl CapturingPort {
    fn new() -> (Self, FrameCapture, SubmitControl) {
        let capture = FrameCapture::default();
        let control = SubmitControl::default();
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 1_048_576,
                max_component_bytes: 262_144,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        );
        let identity = TargetIdentity::new(NonZeroU128::new(41).expect("non-zero target"));
        let epoch = TargetEpoch::new(NonZeroU64::new(1).expect("non-zero epoch"));
        (
            Self {
                declaration: TargetDeclaration::full(identity, epoch, profile),
                capture: capture.clone(),
                control: control.clone(),
            },
            capture,
            control,
        )
    }

    fn force_full_if_requested(&mut self) {
        if !self.control.force_next_full.swap(false, Ordering::SeqCst) {
            return;
        }
        let next_epoch = self
            .declaration
            .continuity()
            .epoch()
            .get()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .expect("test epoch can advance");
        self.declaration = TargetDeclaration::full(
            self.declaration.identity(),
            TargetEpoch::new(next_epoch),
            self.declaration.profile().clone(),
        );
    }
}

#[async_trait]
impl ReactionPort for CapturingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.force_full_if_requested();
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.capture.record(&frame);

        if self.control.block_next_submit.swap(false, Ordering::SeqCst) {
            self.capture.pre_handoff_started.notify_one();
            std::future::pending::<()>().await;
            unreachable!("a blocked pre-handoff submit is cancelled by the test");
        }

        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        Ok(Box::pin(futures::stream::iter(
            self.control.take_facts().into_iter().map(Ok),
        )))
    }
}

#[derive(Clone)]
struct StringSnapshotProps {
    initial: String,
    exposed: Arc<Mutex<Option<Signal<String>>>>,
}

impl StringSnapshotProps {
    fn new(initial: &str) -> Self {
        Self {
            initial: initial.to_owned(),
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<String> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component exposes its state signal")
    }
}

#[component]
fn ordinary_xml_application(props: StringSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    view! {
        state { "{value}" }
        {
            NativeToolCall::named("lookup").on_call(|call| async move {
                Ok::<_, Infallible>(call.output("unused"))
            })
        }
    }
}

#[component]
fn ordinary_assistant_application(props: StringSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    view! {
        #[assistant]
        state { "{value}" }
    }
}

#[derive(Clone)]
struct SemanticDiffProps {
    exposed: Arc<Mutex<Option<Signal<Vec<String>>>>>,
}

impl SemanticDiffProps {
    fn new() -> Self {
        Self {
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<Vec<String>> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component exposes its entries signal")
    }
}

#[derive(AgentView, Clone)]
#[agent_view(kind = "adjacent_history")]
struct AdjacentHistoryView {
    #[view(element)]
    format: &'static str,
    #[view(diff(seq))]
    entries: Vec<String>,
}

#[component]
fn semantic_diff_application(props: SemanticDiffProps) -> Component {
    let entries = use_signal(|| vec!["first".to_owned()]);
    *props.exposed.lock().expect("signal exposure lock") = Some(entries.clone());
    let snapshot = AdjacentHistoryView {
        format: "v1",
        entries: entries.with(Clone::clone).expect("mounted signal read"),
    };
    view! {
        #[developer]
        #[diff(slot = "history")]
        { snapshot }
    }
}

#[component]
fn repeat_diff_and_adjacent_normal_application(props: SemanticDiffProps) -> Component {
    let entries = use_signal(|| vec!["first".to_owned()]);
    *props.exposed.lock().expect("signal exposure lock") = Some(entries.clone());
    let snapshot = AdjacentHistoryView {
        format: "v1",
        entries: entries.with(Clone::clone).expect("mounted signal read"),
    };

    view! {
        #[developer]
        normal_context { "ordinary" }

        #[developer(repeat)]
        repeat_context { "always" }

        #[developer(repeat)]
        #[diff(slot = "history")]
        { snapshot }
    }
}

#[component]
fn repeat_and_normal_state_application(props: StringSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    view! {
        #[developer]
        normal_state { "{value}" }

        #[developer(repeat)]
        repeat_state { "{value}" }
    }
}

#[derive(Clone)]
struct AssistantDiffProps {
    exposed: Arc<Mutex<Option<Signal<Option<Vec<String>>>>>>,
}

impl AssistantDiffProps {
    fn new() -> Self {
        Self {
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<Option<Vec<String>>> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component exposes its assistant diff signal")
    }
}

#[component]
fn assistant_semantic_diff_application(props: AssistantDiffProps) -> Component {
    let entries = use_signal(|| Some(vec!["first".to_owned()]));
    *props.exposed.lock().expect("signal exposure lock") = Some(entries.clone());

    match entries.with(Clone::clone).expect("mounted signal read") {
        Some(entries) => {
            let snapshot = AdjacentHistoryView {
                format: "v1",
                entries,
            };
            view! {
                #[assistant]
                #[diff(slot = "history")]
                { snapshot }
            }
        }
        None => view! {},
    }
}

#[derive(AgentView, Clone)]
#[agent_view(kind = "policy")]
struct MixedDocumentPolicyView {
    #[view(element)]
    access: &'static str,
}

#[derive(AgentView, Clone)]
#[agent_view(document)]
struct MixedDocumentView {
    #[view(paragraph)]
    note: String,

    #[view(xml)]
    policy: MixedDocumentPolicyView,
}

#[component]
fn mixed_document_application(props: StringSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let note = state.with(Clone::clone).expect("mounted signal read");
    let document = MixedDocumentView {
        note,
        policy: MixedDocumentPolicyView {
            access: "allow-read",
        },
    };

    view! {
        #[developer]
        { document }
    }
}

#[component]
fn mixed_template_application(props: SemanticDiffProps) -> Component {
    let entries = use_signal(|| vec!["first".to_owned()]);
    *props.exposed.lock().expect("signal exposure lock") = Some(entries.clone());
    let snapshot = AdjacentHistoryView {
        format: "v1",
        entries: entries.with(Clone::clone).expect("mounted signal read"),
    };

    view! {
        #[developer]
        prefix { "fixed" }
        #[developer]
        #[diff(slot = "history")]
        { snapshot }
        #[developer]
        suffix { "fixed" }
    }
}

#[derive(Clone)]
struct OptionalSnapshotProps {
    initial: Option<String>,
    exposed: Arc<Mutex<Option<Signal<Option<String>>>>>,
}

impl OptionalSnapshotProps {
    fn new(initial: Option<&str>) -> Self {
        Self {
            initial: initial.map(str::to_owned),
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<Option<String>> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component exposes its state signal")
    }
}

#[component]
fn optional_developer_policy(props: OptionalSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    match value {
        Some(value) => view! {
            workspace { "agentview" }
            #[developer]
            policy { "{value}" }
        },
        None => view! { workspace { "agentview" } },
    }
}

#[component]
fn optional_repeat_user_policy(props: OptionalSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    match value {
        Some(value) => view! {
            #[user(repeat)]
            policy { "{value}" }
        },
        None => view! {},
    }
}

#[component]
fn nested_repeat_user_child() -> Component {
    view! {
        #[user(repeat)]
        inherited_value { "child" }
    }
}

#[component]
fn outer_placement_override_and_repeat_application() -> Component {
    view! {
        #[developer]
        nested_repeat_user_child()

        #[user(repeat)]
        nested_repeat_user_child()
    }
}

#[component]
fn optional_plain_text(props: OptionalSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    match value {
        Some(value) => view! { "{value}" },
        None => view! {},
    }
}

#[component]
fn optional_assistant_state(props: OptionalSnapshotProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());

    match state.with(Clone::clone).expect("mounted signal read") {
        Some(value) => view! {
            #[assistant]
            state { "{value}" }
        },
        None => view! {},
    }
}

#[derive(Clone)]
struct ConditionalChildProps {
    exposed: Arc<Mutex<Option<Signal<bool>>>>,
}

impl ConditionalChildProps {
    fn new() -> Self {
        Self {
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<bool> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component exposes its mounted signal")
    }
}

#[component]
fn removable_policy_child() -> Component {
    view! {
        #[developer]
        policy { "child-policy" }
    }
}

#[component]
fn conditional_policy_child(props: ConditionalChildProps) -> Component {
    let mounted = use_signal(|| true);
    *props.exposed.lock().expect("signal exposure lock") = Some(mounted.clone());

    if mounted.with(|value| *value).expect("mounted signal read") {
        view! { removable_policy_child() }
    } else {
        view! {}
    }
}

fn rendered(item: &CanonicalInputItem) -> String {
    let pom = match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            pom
        }
        other => panic!("expected a POM projection item, got {other:?}"),
    };
    render_pom_document(pom).expect("projection POM renders")
}

fn projection_texts(frame: &CapturedFrame) -> Vec<String> {
    frame.projection.iter().map(rendered).collect()
}

async fn react(application: &mut Application<CapturingPort>) {
    assert_eq!(
        application.react().await.expect("reaction succeeds"),
        ControlFlow::Continue(())
    );
}

fn assert_lookup_metadata(frame: &CapturedFrame) {
    assert_eq!(frame.tools.len(), 1);
    let tool = &frame.tools[0];
    assert_eq!(tool.name(), "lookup");
    assert_eq!(tool.description(), "AgentView native tool");
    assert_eq!(
        tool.input_schema(),
        &json!({"type": "object", "additionalProperties": true})
    );
    assert!(!tool.strict());
}

fn assert_assistant_message(frame: &CapturedFrame) {
    assert!(matches!(
        frame.projection.as_slice(),
        [CanonicalInputItem::Message {
            role: ConversationRole::Assistant,
            ..
        }]
    ));
}

#[tokio::test]
async fn ordinary_items_compare_against_only_the_last_committed_complete_snapshot() {
    let props = StringSnapshotProps::new("A");
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || ordinary_xml_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props.signal().set("B".to_owned()).expect("set B");
    react(&mut application).await;
    props.signal().set("A".to_owned()).expect("restore A");
    react(&mut application).await;
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    assert!(matches!(frames[2].basis, FrameBasis::DeltaFrom(_)));
    assert!(matches!(frames[3].basis, FrameBasis::DeltaFrom(_)));
    assert_eq!(projection_texts(&frames[0]), ["<state>A</state>"]);
    assert_eq!(projection_texts(&frames[1]), ["<state>B</state>"]);
    assert_eq!(
        projection_texts(&frames[2]),
        ["<state>A</state>"],
        "returning to an earlier value compares with B, not cumulative history"
    );
    assert!(projection_texts(&frames[3]).is_empty());
    for frame in &frames {
        assert_lookup_metadata(frame);
    }

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn assistant_items_are_messages_and_compare_against_the_adjacent_snapshot() {
    let props = StringSnapshotProps::new("A");
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application = Application::mount(
        move || ordinary_assistant_application(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    props.signal().set("B".to_owned()).expect("set B");
    react(&mut application).await;
    props.signal().set("A".to_owned()).expect("restore A");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert_assistant_message(&frames[0]);
    assert_eq!(projection_texts(&frames[0]), ["<state>A</state>"]);
    assert!(frames[1].projection.is_empty());
    assert_assistant_message(&frames[2]);
    assert_eq!(projection_texts(&frames[2]), ["<state>B</state>"]);
    assert_assistant_message(&frames[3]);
    assert_eq!(projection_texts(&frames[3]), ["<state>A</state>"]);

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn assistant_item_disappearance_is_silent_and_reappearance_is_submitted() {
    let props = OptionalSnapshotProps::new(Some("A"));
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || optional_assistant_state(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props.signal().set(None).expect("remove assistant item");
    react(&mut application).await;
    props
        .signal()
        .set(Some("A".to_owned()))
        .expect("restore assistant item");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_assistant_message(&frames[0]);
    assert_eq!(projection_texts(&frames[0]), ["<state>A</state>"]);
    assert!(
        frames[1].projection.is_empty(),
        "assistant item disappearance does not synthesize an XML remove operation"
    );
    assert_assistant_message(&frames[2]);
    assert_eq!(projection_texts(&frames[2]), ["<state>A</state>"]);

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn assistant_diff_delta_keeps_its_message_role_and_disappearance_is_silent() {
    let props = AssistantDiffProps::new();
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application = Application::mount(
        move || assistant_semantic_diff_application(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .update(|entries| {
            entries
                .as_mut()
                .expect("assistant diff item remains mounted")
                .push("second".to_owned())
        })
        .expect("append assistant diff entry");
    react(&mut application).await;
    props
        .signal()
        .set(None)
        .expect("remove assistant diff item");
    react(&mut application).await;
    props
        .signal()
        .set(Some(vec!["first".to_owned()]))
        .expect("restore assistant diff item");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_assistant_message(&frames[1]);
    let delta = projection_texts(&frames[1])
        .into_iter()
        .next()
        .expect("assistant semantic delta is submitted");
    assert!(delta.contains("rendering_mode=\"delta\""));
    assert!(delta.contains("<insert>"));
    assert!(delta.contains("second"));
    assert!(
        frames[2].projection.is_empty(),
        "removing an assistant diff item emits no XML remove operation"
    );
    assert_assistant_message(&frames[3]);
    let restored = projection_texts(&frames[3])
        .into_iter()
        .next()
        .expect("restored assistant diff item is submitted in full");
    assert!(restored.contains("first"));
    assert!(!restored.contains("rendering_mode=\"delta\""));

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn provider_assistant_text_and_authored_assistant_message_do_not_claim_each_other() {
    let props = OptionalSnapshotProps::new(None);
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let output = ProviderOutputKey::new(71);
    control.enqueue_facts(vec![
        ProviderFact::TextSealed {
            output,
            phase: None,
            text: "<state>A</state>".to_owned(),
        },
        ProviderFact::ReactionCompleted {
            primary_text: Some(output),
        },
    ]);
    let mut application =
        Application::mount(move || optional_assistant_state(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .set(Some("A".to_owned()))
        .expect("mount authored assistant item");
    react(&mut application).await;
    control.force_next_full();
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(frames[0].projection.is_empty());
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    assert_eq!(
        frames[1].replay,
        [CanonicalInputItem::assistant_text("<state>A</state>", None)],
        "the provider assistant output remains in replay before the Component authors identical XML"
    );
    assert_assistant_message(&frames[1]);
    assert_eq!(projection_texts(&frames[1]), ["<state>A</state>"]);

    assert_eq!(frames[2].basis, FrameBasis::Full);
    assert_eq!(frames[2].replay.len(), 2);
    assert_eq!(
        frames[2].replay[0],
        CanonicalInputItem::assistant_text("<state>A</state>", None)
    );
    match &frames[2].replay[1] {
        CanonicalInputItem::Message {
            role: ConversationRole::Assistant,
            pom,
        } => assert_eq!(
            render_pom_document(pom).expect("replayed authored assistant POM renders"),
            "<state>A</state>"
        ),
        other => panic!("expected an authored assistant message in replay, got {other:?}"),
    }
    assert!(
        frames[2].projection.is_empty(),
        "the unchanged authored assistant message is independently omitted after Full recovery"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn full_transport_keeps_the_adjacent_complete_snapshot_for_ordinary_items() {
    let props = StringSnapshotProps::new("A");
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application =
        Application::mount(move || ordinary_xml_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    control.force_next_full();
    react(&mut application).await;
    props.signal().set("B".to_owned()).expect("set B");
    react(&mut application).await;
    props.signal().set("A".to_owned()).expect("restore A");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert_eq!(frames[1].basis, FrameBasis::Full);
    assert!(projection_texts(&frames[1]).is_empty());
    assert!(matches!(frames[2].basis, FrameBasis::DeltaFrom(_)));
    assert!(matches!(frames[3].basis, FrameBasis::DeltaFrom(_)));
    assert_eq!(projection_texts(&frames[2]), ["<state>B</state>"]);
    assert_eq!(projection_texts(&frames[3]), ["<state>A</state>"]);
    for frame in &frames {
        assert_lookup_metadata(frame);
    }

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn explicit_diff_items_keep_their_structured_delta_semantics() {
    let props = SemanticDiffProps::new();
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || semantic_diff_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .update(|entries| entries.push("second".to_owned()))
        .expect("append history entry");
    react(&mut application).await;
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    let delta = projection_texts(&frames[1])
        .into_iter()
        .next()
        .expect("changed diff item is submitted");
    assert!(delta.contains("rendering_mode=\"delta\""));
    assert!(delta.contains("<insert>"));
    assert!(delta.contains("second"));
    assert!(!delta.contains("first"));
    assert!(projection_texts(&frames[2]).is_empty());

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn repeat_items_submit_complete_current_values_without_affecting_adjacent_normal_items() {
    let props = SemanticDiffProps::new();
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application = Application::mount(
        move || repeat_diff_and_adjacent_normal_application(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    props
        .signal()
        .update(|entries| entries.push("second".to_owned()))
        .expect("append repeat history entry");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    let initial = projection_texts(&frames[0]);
    assert_eq!(initial.len(), 2);
    assert!(initial[0].contains("<normal_context>ordinary</normal_context>"));
    assert!(initial[1].contains("<repeat_context>always</repeat_context>"));
    assert!(initial[1].contains("first"));

    let unchanged = projection_texts(&frames[1]);
    assert_eq!(unchanged.len(), 1);
    assert!(unchanged[0].contains("<repeat_context>always</repeat_context>"));
    assert!(unchanged[0].contains("first"));
    assert!(!unchanged[0].contains("normal_context"));
    assert!(
        !unchanged[0].contains("rendering_mode=\"delta\""),
        "a repeat item is submitted as complete current POM even when it has #[diff] metadata"
    );

    let changed = projection_texts(&frames[2]);
    assert_eq!(changed.len(), 1);
    assert!(changed[0].contains("<repeat_context>always</repeat_context>"));
    assert!(changed[0].contains("first"));
    assert!(changed[0].contains("second"));
    assert!(!changed[0].contains("rendering_mode=\"delta\""));
    assert!(!changed[0].contains("<insert>"));

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn mixed_document_text_changes_keep_text_atomic_and_refresh_its_xml_root() {
    let props = StringSnapshotProps::new("Initial note.");
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || mixed_document_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .set("Updated note.".to_owned())
        .expect("update mixed document text");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    let patch = projection_texts(&frames[1])
        .into_iter()
        .next()
        .expect("changed mixed document is submitted");
    assert!(patch.starts_with("<remove>"));
    assert!(patch.contains("<policy>\n  <access>allow-read</access>\n</policy>"));
    assert!(patch.contains("Updated note."));
    assert_eq!(
        patch.matches("<policy>").count(),
        2,
        "mixed document fallback removes the previous XML root before replaying the complete current document"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn mixed_item_template_keeps_complete_fragments_around_the_structured_delta() {
    let props = SemanticDiffProps::new();
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || mixed_template_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .update(|entries| entries.push("second".to_owned()))
        .expect("append mixed-template history entry");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    let submitted = projection_texts(&frames[1]);
    assert_eq!(
        submitted.len(),
        1,
        "mixed template submission: {submitted:?}"
    );
    let delta = &submitted[0];
    assert!(
        delta.contains("<prefix>fixed</prefix>"),
        "mixed template delta: {delta}"
    );
    assert!(delta.contains("<suffix>fixed</suffix>"));
    assert!(delta.contains("rendering_mode=\"delta\""));
    assert!(delta.contains("<insert>"));
    assert!(delta.contains("second"));

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn disappearing_xml_item_emits_a_developer_remove_patch_and_can_reappear() {
    let props = OptionalSnapshotProps::new(Some("allow-read"));
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || optional_developer_policy(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props.signal().set(None).expect("remove policy");
    react(&mut application).await;
    props
        .signal()
        .set(Some("allow-read".to_owned()))
        .expect("restore policy");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(
        projection_texts(&frames[0]),
        [
            "<workspace>agentview</workspace>",
            "<policy>allow-read</policy>"
        ]
    );
    assert_eq!(
        projection_texts(&frames[1]),
        ["<remove>\n  <policy>allow-read</policy>\n</remove>"],
        "the vanished XML document is compared with an empty document"
    );
    assert!(matches!(
        frames[1].projection.as_slice(),
        [CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            ..
        }]
    ));
    assert_eq!(
        projection_texts(&frames[2]),
        ["<policy>allow-read</policy>"],
        "the complete snapshot keeps workspace but no longer contains policy"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn disappearing_repeat_user_item_emits_the_normal_xml_remove_patch() {
    let props = OptionalSnapshotProps::new(Some("allow-read"));
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application = Application::mount(
        move || optional_repeat_user_policy(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    props.signal().set(None).expect("remove repeat policy");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert!(matches!(
        frames[0].projection.as_slice(),
        [CanonicalInputItem::Message {
            role: ConversationRole::User,
            ..
        }]
    ));
    assert_eq!(
        projection_texts(&frames[0]),
        ["<policy>allow-read</policy>"]
    );
    assert!(matches!(
        frames[1].projection.as_slice(),
        [CanonicalInputItem::Message {
            role: ConversationRole::User,
            ..
        }]
    ));
    assert_eq!(
        projection_texts(&frames[1]),
        ["<policy>allow-read</policy>"],
        "an unchanged #[user(repeat)] root is still submitted on the next reaction"
    );
    assert!(matches!(
        frames[2].projection.as_slice(),
        [CanonicalInputItem::Message {
            role: ConversationRole::User,
            ..
        }]
    ));
    assert_eq!(
        projection_texts(&frames[2]),
        ["<remove>\n  <policy>allow-read</policy>\n</remove>"],
        "when a repeat item is absent, its former user POM is removed instead of being repeated"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn outer_placement_overrides_inner_repeat_and_repeat_scope_survives_later_reactions() {
    let (port, capture, _) = CapturingPort::new();
    let mut application = Application::mount(outer_placement_override_and_repeat_application, port)
        .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert!(matches!(
        frames[0].projection.as_slice(),
        [
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::Developer,
                ..
            },
            CanonicalInputItem::Message {
                role: ConversationRole::User,
                ..
            }
        ]
    ));
    assert_eq!(
        projection_texts(&frames[0]),
        [
            "<inherited_value>child</inherited_value>",
            "<inherited_value>child</inherited_value>"
        ]
    );
    for frame in &frames[1..] {
        assert!(matches!(
            frame.projection.as_slice(),
            [CanonicalInputItem::Message {
                role: ConversationRole::User,
                ..
            }]
        ));
        assert_eq!(
            projection_texts(frame),
            ["<inherited_value>child</inherited_value>"],
            "the outer #[user(repeat)] subtree remains repeatable across reactions while the outer ordinary developer subtree is omitted"
        );
    }

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn disappearing_component_node_emits_the_xml_remove_patch() {
    let props = ConditionalChildProps::new();
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application =
        Application::mount(move || conditional_policy_child(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    props.signal().set(false).expect("unmount child");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        projection_texts(&frames[0]),
        ["<policy>child-policy</policy>"]
    );
    assert_eq!(
        projection_texts(&frames[1]),
        ["<remove>\n  <policy>child-policy</policy>\n</remove>"]
    );
    assert!(matches!(
        frames[1].projection.as_slice(),
        [CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            ..
        }]
    ));

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn plain_text_changes_are_full_and_disappearance_has_no_remove_patch() {
    let props = OptionalSnapshotProps::new(Some("first note"));
    let root_props = props.clone();
    let (port, capture, _) = CapturingPort::new();
    let mut application = Application::mount(move || optional_plain_text(root_props.clone()), port)
        .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .set(Some("second note".to_owned()))
        .expect("update text");
    react(&mut application).await;
    props.signal().set(None).expect("remove text");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(projection_texts(&frames[0]), ["first note"]);
    assert_eq!(
        projection_texts(&frames[1]),
        ["second note"],
        "plain text changes are sent as complete current text"
    );
    assert!(
        projection_texts(&frames[2]).is_empty(),
        "plain text disappearance does not synthesize an XML remove operation"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn cancelled_pre_handoff_submission_does_not_advance_the_complete_snapshot() {
    let props = StringSnapshotProps::new("A");
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application =
        Application::mount(move || ordinary_xml_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    control.block_next_submit();
    props.signal().set("B".to_owned()).expect("set B");

    let notified = capture.pre_handoff_started.notified();
    let mut cancelled = Box::pin(application.react());
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = notified => {}
            result = &mut cancelled => panic!("reaction ended before pre-handoff block: {result:?}"),
        }
    })
    .await
    .expect("pre-handoff submission reaches the blocking port");
    drop(cancelled);

    props.signal().set("A".to_owned()).expect("restore A");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(projection_texts(&frames[0]), ["<state>A</state>"]);
    assert_eq!(
        projection_texts(&frames[1]),
        ["<state>B</state>"],
        "the blocked frame was prepared but never handed off"
    );
    assert!(
        projection_texts(&frames[2]).is_empty(),
        "the successful retry still compares with the last handed-off A snapshot"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn repeat_items_are_delivered_only_by_react_and_cancelled_frames_do_not_advance() {
    let props = StringSnapshotProps::new("A");
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application = Application::mount(
        move || repeat_and_normal_state_application(root_props.clone()),
        port,
    )
    .expect("mount application");

    assert_eq!(
        application.prepare().await.expect("prepare succeeds"),
        ControlFlow::Continue(())
    );
    assert!(
        capture.frames().is_empty(),
        "preparation alone does not hand a repeat item to the provider"
    );

    react(&mut application).await;
    control.block_next_submit();
    props.signal().set("B".to_owned()).expect("set B");

    let notified = capture.pre_handoff_started.notified();
    let mut cancelled = Box::pin(application.react());
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = notified => {}
            result = &mut cancelled => panic!("reaction ended before pre-handoff block: {result:?}"),
        }
    })
    .await
    .expect("pre-handoff submission reaches the blocking port");
    drop(cancelled);

    props.signal().set("A".to_owned()).expect("restore A");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(
        projection_texts(&frames[0]),
        [
            "<normal_state>A</normal_state>",
            "<repeat_state>A</repeat_state>"
        ]
    );
    assert!(
        projection_texts(&frames[1])
            .iter()
            .any(|item| item.contains("<repeat_state>B</repeat_state>")),
        "the cancelled attempt prepared B but never committed it"
    );
    assert_eq!(
        projection_texts(&frames[2]),
        ["<repeat_state>A</repeat_state>"],
        "the retry compares normal state with the last handed-off A while delivering the repeat item"
    );

    application.shutdown().await.expect("shutdown application");
}
