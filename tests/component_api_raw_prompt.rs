use std::{
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{
            Application, Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile,
            ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};
use async_trait::async_trait;

const SYSTEM_RAW: &str = "  # Protocol\r\n\r\n```xml\r\n<fenced attr=\"literal\">keep this raw</fenced>\r\n```\r\n<body><nested>also raw</nested></body>\r\n\ttrailing spaces  \r\n";
const RAW_A: &str = "  user text\r\n<literal_xml id=\"A\">not parsed</literal_xml>\r\n```xml\r\n<fenced>A</fenced>\r\n```\r\n";
const RAW_B: &str = "  user text changed\r\n<literal_xml id=\"B\">not parsed</literal_xml>\r\n```xml\r\n<fenced>B</fenced>\r\n```\r\n";

#[derive(Debug, Clone)]
struct CapturedFrame {
    basis: FrameBasis,
    projection: Vec<CanonicalInputItem>,
}

#[derive(Clone, Default)]
struct FrameCapture {
    frames: Arc<Mutex<Vec<CapturedFrame>>>,
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
                projection: frame.submission().projection().items().to_vec(),
            });
    }
}

struct CapturingPort {
    declaration: TargetDeclaration,
    capture: FrameCapture,
}

impl CapturingPort {
    fn new() -> (Self, FrameCapture) {
        let capture = FrameCapture::default();
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 1_048_576,
                max_component_bytes: 262_144,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        );
        let identity = TargetIdentity::new(NonZeroU128::new(91).expect("non-zero target"));
        let epoch = TargetEpoch::new(NonZeroU64::new(1).expect("non-zero epoch"));
        (
            Self {
                declaration: TargetDeclaration::full(identity, epoch, profile),
                capture: capture.clone(),
            },
            capture,
        )
    }
}

#[async_trait]
impl ReactionPort for CapturingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.capture.record(&frame);
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        Ok(Box::pin(futures::stream::iter(std::iter::once(Ok(
            ProviderFact::ReactionCompleted { primary_text: None },
        )))))
    }
}

#[derive(Clone)]
struct RawStringProps {
    initial: String,
    exposed: Arc<Mutex<Option<Signal<String>>>>,
}

impl RawStringProps {
    fn new(initial: &str) -> Self {
        Self {
            initial: initial.to_owned(),
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<String> {
        self.exposed
            .lock()
            .expect("raw string signal lock")
            .clone()
            .expect("component exposes its raw string signal")
    }
}

#[derive(Clone)]
struct OptionalRawStringProps {
    initial: Option<String>,
    exposed: Arc<Mutex<Option<Signal<Option<String>>>>>,
}

impl OptionalRawStringProps {
    fn new(initial: Option<&str>) -> Self {
        Self {
            initial: initial.map(str::to_owned),
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<Option<String>> {
        self.exposed
            .lock()
            .expect("optional raw string signal lock")
            .clone()
            .expect("component exposes its optional raw string signal")
    }
}

fn developer_raw(value: &str) -> String {
    format!("developer prefix\r\n{value}\t")
}

#[component]
fn raw_system_with_later_placements(props: RawStringProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("raw string signal lock") = Some(state.clone());
    let current = state.with(Clone::clone).expect("mounted signal read");
    let system = String::from(SYSTEM_RAW);
    let borrowed = current.as_str();

    view! {
        #[system_once]
        { system }

        #[user]
        { borrowed }

        #[developer]
        { format!("developer prefix\r\n{current}\t") }
    }
}

#[component]
fn raw_ordinary_user_and_developer(props: OptionalRawStringProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props
        .exposed
        .lock()
        .expect("optional raw string signal lock") = Some(state.clone());

    match state.with(Clone::clone).expect("mounted signal read") {
        Some(value) => {
            let developer = value.clone();
            view! {
                #[user]
                { value }

                #[developer]
                { developer }
            }
        }
        None => view! {},
    }
}

#[component]
fn raw_repeat_and_diff(props: RawStringProps) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("raw string signal lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");
    let repeated_user = value.clone();

    view! {
        #[user(repeat)]
        { repeated_user }

        #[developer(repeat)]
        #[diff(slot = "raw-text")]
        { value }
    }
}

fn rendered(item: &CanonicalInputItem) -> String {
    let pom = match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            pom
        }
        other => panic!("expected a POM projection item, got {other:?}"),
    };
    render_pom_document(pom).expect("raw POM renders")
}

fn projection_texts(frame: &CapturedFrame) -> Vec<String> {
    frame.projection.iter().map(rendered).collect()
}

fn assert_user_developer_raw(frame: &CapturedFrame, raw: &str) {
    assert!(matches!(
        frame.projection.as_slice(),
        [
            CanonicalInputItem::Message {
                role: ConversationRole::User,
                ..
            },
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::Developer,
                ..
            }
        ]
    ));
    assert_eq!(projection_texts(frame), [raw, raw]);
}

async fn react(application: &mut Application<CapturingPort>) {
    assert_eq!(
        application.react().await.expect("reaction succeeds"),
        ControlFlow::Continue(())
    );
}

#[tokio::test]
async fn raw_system_text_is_verbatim_and_keeps_later_placements_independent() {
    let props = RawStringProps::new(RAW_A);
    let root_props = props.clone();
    let (port, capture) = CapturingPort::new();
    let mut application = Application::mount(
        move || raw_system_with_later_placements(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    props
        .signal()
        .set(RAW_B.to_owned())
        .expect("update raw text");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(matches!(
        frames[0].projection.as_slice(),
        [
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::System,
                ..
            },
            CanonicalInputItem::Message {
                role: ConversationRole::User,
                ..
            },
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::Developer,
                ..
            }
        ]
    ));
    assert_eq!(
        projection_texts(&frames[0]),
        vec![
            SYSTEM_RAW.to_owned(),
            RAW_A.to_owned(),
            developer_raw(RAW_A),
        ],
        "Markdown, fenced XML, body XML, CRLF, indentation, and trailing whitespace remain raw text"
    );
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    assert_user_developer_raw_with_developer(&frames[1], RAW_B, &developer_raw(RAW_B));

    application.shutdown().await.expect("shutdown application");
}

fn assert_user_developer_raw_with_developer(frame: &CapturedFrame, user: &str, developer: &str) {
    assert!(matches!(
        frame.projection.as_slice(),
        [
            CanonicalInputItem::Message {
                role: ConversationRole::User,
                ..
            },
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::Developer,
                ..
            }
        ]
    ));
    assert_eq!(projection_texts(frame), [user, developer]);
}

#[tokio::test]
async fn raw_user_and_developer_values_compare_as_whole_documents_and_disappear_silently() {
    let props = OptionalRawStringProps::new(Some(RAW_A));
    let root_props = props.clone();
    let (port, capture) = CapturingPort::new();
    let mut application = Application::mount(
        move || raw_ordinary_user_and_developer(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    props.signal().set(Some(RAW_B.to_owned())).expect("set B");
    react(&mut application).await;
    props
        .signal()
        .set(Some(RAW_A.to_owned()))
        .expect("restore A");
    react(&mut application).await;
    props.signal().set(None).expect("remove raw text");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 5);
    assert_user_developer_raw(&frames[0], RAW_A);
    assert!(
        frames[1].projection.is_empty(),
        "unchanged raw strings are omitted as whole values"
    );
    assert_user_developer_raw(&frames[2], RAW_B);
    assert_user_developer_raw(&frames[3], RAW_A);
    assert!(
        frames[4].projection.is_empty(),
        "removing raw text does not synthesize an XML remove even when the text contains XML-looking content"
    );

    application.shutdown().await.expect("shutdown application");
}

#[tokio::test]
async fn raw_repeat_and_diff_items_submit_complete_text_on_every_reaction() {
    let props = RawStringProps::new(RAW_A);
    let root_props = props.clone();
    let (port, capture) = CapturingPort::new();
    let mut application = Application::mount(move || raw_repeat_and_diff(root_props.clone()), port)
        .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    props.signal().set(RAW_B.to_owned()).expect("set B");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_user_developer_raw(&frames[0], RAW_A);
    assert_user_developer_raw(&frames[1], RAW_A);
    assert_user_developer_raw(&frames[2], RAW_B);
    for frame in &frames {
        for raw in projection_texts(frame) {
            assert!(!raw.contains("rendering_mode=\"delta\""));
            assert!(!raw.contains("<insert>"));
            assert!(!raw.contains("<remove>"));
        }
    }

    application.shutdown().await.expect("shutdown application");
}
