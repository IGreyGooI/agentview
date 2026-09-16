use std::{
    collections::VecDeque,
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFaultKind, ApplicationFaultReason, Frame, FrameCapabilities,
            FrameConstraints, FrameProfile, ProviderFact, ProviderFactStream, ProviderOutputKey,
            ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch,
            TargetIdentity,
        },
        prelude::*,
    },
    pom_renderer::render_pom_document,
    transcript::{AssistantPhase, CanonicalInputItem},
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Vec<CanonicalInputItem>>>>);

impl Capture {
    fn views(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        CanonicalInputItem::Instruction { pom, .. }
                        | CanonicalInputItem::Message { pom, .. } => {
                            Some(render_pom_document(pom).unwrap())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect()
    }
}

enum Script {
    Finite(Vec<ProviderFact>),
    PendingAfter(Vec<ProviderFact>),
}

struct Port {
    declaration: TargetDeclaration,
    scripts: VecDeque<Script>,
    capture: Capture,
}

impl Port {
    fn new(scripts: impl IntoIterator<Item = Script>) -> (Self, Capture) {
        let capture = Capture::default();
        (
            Self {
                declaration: TargetDeclaration::full(
                    TargetIdentity::new(NonZeroU128::new(907).unwrap()),
                    TargetEpoch::new(NonZeroU64::new(1).unwrap()),
                    FrameProfile::new(
                        FrameConstraints {
                            max_frame_bytes: 1_048_576,
                            max_component_bytes: 262_144,
                            context_window_tokens: None,
                            reserved_output_tokens: None,
                        },
                        FrameCapabilities::new(true),
                    ),
                ),
                scripts: scripts.into_iter().collect(),
                capture: capture.clone(),
            },
            capture,
        )
    }
}

#[async_trait]
impl ReactionPort for Port {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.capture
            .0
            .lock()
            .unwrap()
            .push(frame.submission().projection().items().to_vec());
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        match self.scripts.pop_front().expect("one script per reaction") {
            Script::Finite(facts) => Ok(Box::pin(futures::stream::iter(facts.into_iter().map(Ok)))),
            Script::PendingAfter(facts) => Ok(Box::pin(
                futures::stream::iter(facts.into_iter().map(Ok)).chain(futures::stream::pending()),
            )),
        }
    }
}

fn text_facts(chunks: &[&str]) -> Vec<ProviderFact> {
    let output = ProviderOutputKey::new(1);
    let mut facts: Vec<_> = chunks
        .iter()
        .map(|text| ProviderFact::TextDelta {
            output,
            phase: None,
            delta: (*text).to_owned(),
        })
        .collect();
    facts.push(ProviderFact::TextSealed {
        output,
        phase: None,
        text: chunks.concat(),
    });
    facts.push(ProviderFact::ReactionCompleted {
        primary_text: Some(output),
    });
    facts
}

fn completed() -> Script {
    Script::Finite(vec![ProviderFact::ReactionCompleted { primary_text: None }])
}

#[derive(Clone, Default)]
struct Notebook {
    notes: Vec<String>,
    streamed: String,
    events: Vec<String>,
    diagnostics: Vec<XmlCallbackDiagnostic>,
    confirmations: Vec<usize>,
    show_say: bool,
}

#[derive(Clone, Default)]
struct NotebookProbe(Arc<Mutex<Option<Signal<Notebook>>>>);

impl NotebookProbe {
    fn signal(&self) -> Signal<Notebook> {
        self.0.lock().unwrap().clone().expect("mounted notebook")
    }

    fn snapshot(&self) -> Notebook {
        self.signal().with(Clone::clone).unwrap()
    }
}

#[component]
fn say_action(state: Signal<Notebook>, observed: usize) -> Component {
    let opens = state.clone();
    let deltas = state.clone();
    let invalid = state.clone();
    view! {
        XmlStreamingToolCall {
            element: XmlToolElement::text("say"),
            description: "Save one note. Its decoded text becomes the note.",
            on_open: move |_: Arc<()>| {
                opens.update(|book| book.events.push(format!("open:{observed}")))
            },
            on_delta: move |text: String| {
                let state = deltas.clone();
                async move {
                    tokio::task::yield_now().await;
                    state.update(|book| {
                        book.events.push(format!("delta:{text}"));
                        book.streamed.push_str(&text);
                    })
                }
            },
            on_complete: move |text: String| {
                state.update(|book| {
                    book.events.push(format!("complete:{observed}:{text}"));
                    book.notes.push(text);
                })
            },
            on_invalid: move |diagnostic: XmlCallbackDiagnostic| {
                invalid.update(|book| {
                    book.events.push(format!("invalid:{}", diagnostic.code));
                    book.diagnostics.push(diagnostic);
                })
            },
        }
    }
}

struct Confirmation {
    count: usize,
}

#[component]
fn notebook(probe: NotebookProbe) -> Component {
    let state = use_signal(|| Notebook {
        show_say: true,
        ..Notebook::default()
    });
    *probe.0.lock().unwrap() = Some(state.clone());
    let current = state.with(Clone::clone).unwrap();
    let count = current.notes.len();
    let confirmed = current.confirmations.len();
    let feedback = current
        .diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let note_action = if current.show_say {
        say_action(state.clone(), count)
    } else {
        view! {}
    };
    let confirm_open = state.clone();
    let confirm_invalid = state.clone();
    view! {
        notebook_state {
            saved_count { "{count}" }
            confirmations { "{confirmed}" }
            feedback { "{feedback}" }
            next_action { "Use confirm with the saved_count from this view." }
        }
        { note_action }
        XmlStreamingToolCall {
            element: XmlToolElement::self_closing("confirm")
                .required_attribute::<usize>("count", "2")
                .decode(
                    |mut attributes| attributes.take_required::<usize>("count"),
                    |count, _| Ok(Confirmation { count: *count }),
                ),
            description: "Confirm the number of notes currently saved.",
            on_open: move |count: Arc<usize>| {
                confirm_open.update(|book| book.events.push(format!("confirm-open:{count}")))
            },
            on_complete: move |value: Confirmation| {
                state.update(|book| {
                    book.events.push(format!("confirm:{}", value.count));
                    if value.count == book.notes.len() {
                        book.confirmations.push(value.count);
                    }
                })
            },
            on_invalid: move |diagnostic: XmlCallbackDiagnostic| {
                confirm_invalid.update(|book| {
                    book.events.push(format!("invalid:{}", diagnostic.code));
                    book.diagnostics.push(diagnostic);
                })
            },
        }
    }
}

#[tokio::test]
async fn callbacks_preserve_source_order_skip_typos_and_deliver_feedback_for_the_next_action() {
    let probe = NotebookProbe::default();
    let root_probe = probe.clone();
    let (port, capture) = Port::new([
        Script::Finite(text_facts(&[
            "<say>fir",
            "st &am",
            "p; &#x4E2D;</say><saya><say>hidden</say></saya>",
            "<confirm count=\"1\"/><say>last</say>",
        ])),
        Script::Finite(text_facts(&["<confirm count=\"2\"/>"])),
        completed(),
    ]);
    let mut app = Application::mount(move || notebook(root_probe.clone()), port).unwrap();
    let _ = app.react().await.unwrap();
    let first = probe.snapshot();
    assert_eq!(first.notes, ["first & 中", "last"]);
    assert_eq!(first.streamed, "first & 中last");
    assert_eq!(first.confirmations, [1]);
    assert_eq!(
        first.diagnostics.len(),
        1,
        "registered siblings are valid actions"
    );
    assert_eq!(first.diagnostics[0].code, "unknown_element");
    assert_eq!(
        first
            .events
            .iter()
            .filter(|event| !event.starts_with("delta:"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            "open:0",
            "complete:0:first & 中",
            "invalid:unknown_element",
            "confirm-open:1",
            "confirm:1",
            "open:0",
            "complete:0:last"
        ],
        "callbacks across declared components execute in XML source order"
    );

    let _ = app.react().await.unwrap();
    let _ = app.react().await.unwrap();
    assert_eq!(probe.snapshot().confirmations, [1, 2]);
    let views = capture.views();
    assert!(views[0].contains("Save one note."));
    assert!(views[0].contains("confirm"));
    assert!(views[0].contains("count"));
    assert!(views[1].contains("<saved_count>2</saved_count>"));
    assert!(
        views[1].contains("saya"),
        "the next submitted Frame exposes the skipped action"
    );
    assert!(views[2].contains("<confirmations>2</confirmations>"));
}

#[tokio::test]
async fn typed_attribute_errors_skip_callbacks_and_valid_following_actions_continue() {
    let probe = NotebookProbe::default();
    let root_probe = probe.clone();
    let (port, _) = Port::new([Script::Finite(text_facts(&[
        "<confirm count=\"wrong\"/><confirm/><confirm count=\"0\" extra=\"x\"/>",
        "<confirm count=\"0\"/><say>still works</say>",
    ]))]);
    let mut app = Application::mount(move || notebook(root_probe.clone()), port).unwrap();
    let _ = app.react().await.unwrap();
    let state = probe.snapshot();
    assert_eq!(state.notes, ["still works"]);
    assert_eq!(state.confirmations, [0]);
    assert_eq!(state.diagnostics.len(), 3);
    assert_eq!(
        state
            .events
            .iter()
            .filter(|event| event.starts_with("confirm-open:"))
            .count(),
        1
    );
    assert!(state
        .diagnostics
        .iter()
        .all(|diagnostic| diagnostic.element.as_deref() == Some("confirm")));
}

#[tokio::test]
async fn rerender_refreshes_captures_and_unmount_removes_the_action() {
    let probe = NotebookProbe::default();
    let root_probe = probe.clone();
    let (port, capture) = Port::new([
        Script::Finite(text_facts(&["<say>one</say>"])),
        Script::Finite(text_facts(&["<say>two</say>"])),
        Script::Finite(text_facts(&["<say>unmounted</say><confirm count=\"2\"/>"])),
    ]);
    let mut app = Application::mount(move || notebook(root_probe.clone()), port).unwrap();
    let _ = app.react().await.unwrap();
    let _ = app.react().await.unwrap();
    let state = probe.snapshot();
    assert!(state.events.contains(&"complete:0:one".to_owned()));
    assert!(state.events.contains(&"complete:1:two".to_owned()));
    probe.signal().update(|book| book.show_say = false).unwrap();
    let _ = app.react().await.unwrap();
    let state = probe.snapshot();
    assert_eq!(state.notes, ["one", "two"]);
    assert_eq!(state.confirmations, [2]);
    assert_eq!(state.diagnostics.len(), 1);
    assert_eq!(state.diagnostics[0].code, "unknown_element");
    assert_eq!(capture.views().len(), 3);
    let current = app.current_projection();
    let current_text = current
        .projection()
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom).unwrap()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!current_text.contains("Save one note."));
}

#[tokio::test]
async fn commentary_is_not_executed_and_eof_reports_incomplete_actions() {
    let probe = NotebookProbe::default();
    let root_probe = probe.clone();
    let commentary = ProviderOutputKey::new(2);
    let mut facts = vec![
        ProviderFact::TextDelta {
            output: commentary,
            phase: Some(AssistantPhase::Commentary),
            delta: "<say>commentary</say>".to_owned(),
        },
        ProviderFact::TextSealed {
            output: commentary,
            phase: Some(AssistantPhase::Commentary),
            text: "<say>commentary</say>".to_owned(),
        },
    ];
    facts.extend(text_facts(&["<say>kept</say><say>unfinished"]));
    let (port, capture) = Port::new([Script::Finite(facts), completed()]);
    let mut app = Application::mount(move || notebook(root_probe.clone()), port).unwrap();
    let _ = app.react().await.unwrap();
    let state = probe.snapshot();
    assert_eq!(state.notes, ["kept"]);
    assert_eq!(state.streamed, "keptunfinished");
    assert_eq!(state.diagnostics.len(), 1);
    assert_eq!(state.diagnostics[0].element.as_deref(), Some("say"));
    let _ = app.react().await.unwrap();
    assert!(capture.views()[1].contains("<saved_count>1</saved_count>"));
    assert!(capture.views()[1].contains("unfinished") || capture.views()[1].contains("incomplete"));
}

#[component]
fn failing_callback(events: Arc<Mutex<Vec<String>>>) -> Component {
    let completion = Arc::clone(&events);
    let invalid = Arc::clone(&events);
    view! {
        XmlStreamingToolCall {
            element: XmlToolElement::text("say"),
            on_delta: move |text: String| {
                events.lock().unwrap().push(format!("delta:{text}"));
                Err::<(), _>("application callback failed")
            },
            on_complete: move |text: String| {
                completion.lock().unwrap().push(format!("complete:{text}"));
                Ok::<(), Infallible>(())
            },
            on_invalid: move |_: XmlCallbackDiagnostic| {
                invalid.lock().unwrap().push("invalid".to_owned());
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[tokio::test]
async fn callback_failure_stops_later_buffered_events_and_remains_a_runtime_fault() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let root_events = Arc::clone(&events);
    let (port, _) = Port::new([Script::Finite(text_facts(&[
        "<say>one</say><say>two</say>",
    ]))]);
    let mut app =
        Application::mount(move || failing_callback(Arc::clone(&root_events)), port).unwrap();
    let error = app
        .react()
        .await
        .expect_err("callback error is a reaction fault");
    assert_eq!(error.kind(), ApplicationFaultKind::Terminal);
    assert_eq!(error.reason(), ApplicationFaultReason::StreamingContract);
    assert_eq!(*events.lock().unwrap(), ["delta:one"]);
}

#[derive(Clone, Default)]
struct PendingProbe {
    started: Arc<Notify>,
    dropped: Arc<AtomicBool>,
    state: Arc<Mutex<Option<Signal<Vec<String>>>>>,
}

struct PendingGuard(Arc<AtomicBool>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[component]
fn pending_callback(probe: PendingProbe) -> Component {
    let state = use_signal(Vec::<String>::new);
    *probe.state.lock().unwrap() = Some(state.clone());
    let text = state.with(|events| events.join(",")).unwrap();
    let delta_state = state.clone();
    view! {
        pending_state { "{text}" }
        XmlStreamingToolCall {
            element: XmlToolElement::text("say"),
            on_delta: move |text: String| {
                let state = delta_state.clone();
                let probe = probe.clone();
                async move {
                    state.update(|events| events.push(format!("delta:{text}")))?;
                    if text == "pause" {
                        let _guard = PendingGuard(Arc::clone(&probe.dropped));
                        probe.started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok::<(), SignalAccessError>(())
                }
            },
            on_complete: move |text: String| {
                state.update(|events| events.push(format!("complete:{text}")))
            },
        }
    }
}

#[tokio::test]
async fn cancellation_drops_callback_future_and_does_not_replay_buffered_completion() {
    let probe = PendingProbe::default();
    let root_probe = probe.clone();
    let (port, capture) = Port::new([
        Script::PendingAfter(vec![ProviderFact::TextDelta {
            output: ProviderOutputKey::new(1),
            phase: None,
            delta: "<say>pause</say><say>never</say>".to_owned(),
        }]),
        Script::Finite(text_facts(&["<say>next</say>"])),
    ]);
    let mut app = Application::mount(move || pending_callback(root_probe.clone()), port).unwrap();
    let mut reaction = Box::pin(app.react());
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            result = &mut reaction => panic!("pending callback completed: {result:?}"),
            _ = probe.started.notified() => {},
        }
    })
    .await
    .expect("callback starts before the buffered completion");
    drop(reaction);
    assert!(probe.dropped.load(Ordering::SeqCst));
    let state = probe.state.lock().unwrap().clone().unwrap();
    assert_eq!(state.with(Clone::clone).unwrap(), ["delta:pause"]);
    let _ = app.react().await.unwrap();
    assert_eq!(
        state.with(Clone::clone).unwrap(),
        ["delta:pause", "delta:next", "complete:next"]
    );
    assert!(
        capture.views()[1].contains("delta:pause"),
        "completed state writes survive cancellation"
    );
}

#[component]
fn ping_action(pings: Arc<Mutex<usize>>) -> Component {
    view! {
        XmlStreamingToolCall {
            element: XmlToolElement::self_closing("ping"),
            description: "Record one ping without arguments.",
            on_complete: move |(): ()| {
                *pings.lock().unwrap() += 1;
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[tokio::test]
async fn self_closing_defaults_to_unit_and_invalid_input_has_feedback_without_a_handler() {
    let pings = Arc::new(Mutex::new(0));
    let root_pings = Arc::clone(&pings);
    let (port, capture) = Port::new([Script::Finite(text_facts(&["<pign/><ping/>"])), completed()]);
    let mut app = Application::mount(move || ping_action(Arc::clone(&root_pings)), port).unwrap();
    let _ = app.react().await.unwrap();
    let _ = app.react().await.unwrap();
    assert_eq!(*pings.lock().unwrap(), 1);
    assert!(capture.views()[1].contains("pign"));
    assert!(capture.views()[1].contains("unknown_element"));
}

#[derive(Clone, Default)]
struct SpawnProbe {
    visible: Arc<Mutex<Option<Signal<bool>>>>,
    local: Arc<Mutex<Option<Signal<u8>>>>,
    sync_started: Arc<Notify>,
    async_started: Arc<Notify>,
    sync_dropped: Arc<AtomicBool>,
    async_dropped: Arc<AtomicBool>,
}

#[component]
fn spawning_action(probe: SpawnProbe) -> Component {
    let local = use_signal(|| 0_u8);
    *probe.local.lock().unwrap() = Some(local.clone());
    let sync_probe = probe.clone();
    view! {
        XmlStreamingToolCall {
            element: XmlToolElement::self_closing("start"),
            on_open: move |_: Arc<()>| {
                let probe = sync_probe.clone();
                spawn(async move {
                    let _guard = PendingGuard(probe.sync_dropped);
                    probe.sync_started.notify_one();
                    std::future::pending::<()>().await;
                })
            },
            on_complete: move |(): ()| {
                let probe = probe.clone();
                let local = local.clone();
                async move {
                    tokio::task::yield_now().await;
                    local.set(1).map_err(|error| error.to_string())?;
                    spawn(async move {
                        let _guard = PendingGuard(probe.async_dropped);
                        probe.async_started.notify_one();
                        std::future::pending::<()>().await;
                    })
                    .map_err(|error| error.to_string())
                }
            },
        }
    }
}

#[component]
fn spawning_root(probe: SpawnProbe) -> Component {
    let visible = use_signal(|| true);
    *probe.visible.lock().unwrap() = Some(visible.clone());
    if visible.with(|value| *value).unwrap() {
        view! { spawning_action(probe) }
    } else {
        view! { state { "action unmounted" } }
    }
}

#[tokio::test]
async fn callback_invocation_and_async_poll_have_mount_context_and_unmount_retires_spawned_tasks() {
    let probe = SpawnProbe::default();
    let root_probe = probe.clone();
    let (port, _) = Port::new([Script::Finite(text_facts(&["<start/>"])), completed()]);
    let mut app = Application::mount(move || spawning_root(root_probe.clone()), port).unwrap();
    let _ = app.react().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        probe.sync_started.notified().await;
        probe.async_started.notified().await;
    })
    .await
    .expect("both callback contexts can start a Component task");
    let local = probe.local.lock().unwrap().clone().unwrap();
    assert_eq!(local.with(|value| *value).unwrap(), 1);
    probe
        .visible
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .set(false)
        .unwrap();
    let _ = app.react().await.unwrap();
    assert!(probe.sync_dropped.load(Ordering::SeqCst));
    assert!(probe.async_dropped.load(Ordering::SeqCst));
    assert!(
        local.with(|value| *value).is_err(),
        "unmounted component Signals are stale"
    );
}

struct ManagedChannels;

impl StreamingToolChannels for ManagedChannels {
    type Output = String;
    type Live = NoStreamingValue;
    type Commit = NoStreamingValue;
    type Diagnostic = &'static str;
}

struct ManagedPublisher(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl StreamingToolAttemptPublisher<ManagedChannels> for ManagedPublisher {
    type Published = ();
    type PublicationOperation = Vec<String>;
    type Error = Infallible;

    fn prepare(
        &mut self,
        _: &StreamingPublishContext,
        attempt: AcceptedStreamingToolAttempt<ManagedChannels>,
    ) -> Result<Self::PublicationOperation, Self::Error> {
        Ok(attempt
            .entries
            .into_iter()
            .map(|entry| match entry.value {
                StagedStreamingToolValue::Output(value) => value,
                StagedStreamingToolValue::Commit(never) => match never {},
            })
            .collect())
    }

    async fn publish(
        &mut self,
        _: &StreamingPublishContext,
        operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<(), Self::Error> {
        self.0.lock().unwrap().extend(operation.iter().cloned());
        StreamingPublishOutcome::Published(())
    }

    async fn resolve(
        &mut self,
        _: &StreamingPublishRecoveryContext,
        _: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<()>, Self::Error> {
        Ok(StreamingPublishResolution::Published(()))
    }
}

#[derive(Clone, Default)]
struct MixedProbe {
    picks: Arc<Mutex<Vec<u32>>>,
    published: Arc<Mutex<Vec<String>>>,
    diagnostics: Arc<Mutex<Vec<XmlCallbackDiagnostic>>>,
}

#[component]
fn mixed_callbacks(probe: MixedProbe, duplicate: bool) -> Component {
    let publisher = Arc::clone(&probe.published);
    let managed = XmlStreamingToolCall::new::<ManagedChannels>("managed.tags")
        .ignore_unknown_elements()
        .state_with(|_| Ok::<_, Infallible>(()))
        .element(
            XmlToolElement::text(if duplicate { "pick" } else { "tag" })
                .decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
            |handlers| handlers.on_complete(|_, value| StreamingToolUpdate::output(value.value)),
        )
        .finish(|_, summary| {
            assert!(
                summary.diagnostics.is_empty(),
                "managed parser ignores direct sibling actions"
            );
            StreamingToolDecision::Accept(StreamingToolUpdate::none())
        })
        .without_live()
        .publish_with(move |_| Ok::<_, Infallible>(ManagedPublisher(Arc::clone(&publisher))))
        .build();
    view! {
        XmlStreamingToolCall {
            element: XmlToolElement::self_closing("pick")
                .required_attribute::<u32>("value", "7")
                .decode(|mut attrs| attrs.take_required::<u32>("value"), |value, _| Ok(*value)),
            on_complete: move |value: u32| {
                probe.picks.lock().unwrap().push(value);
                Ok::<(), Infallible>(())
            },
            on_invalid: move |diagnostic: XmlCallbackDiagnostic| {
                probe.diagnostics.lock().unwrap().push(diagnostic);
                Ok::<(), Infallible>(())
            },
        }
        { managed }
    }
}

#[tokio::test]
async fn simple_and_managed_xml_actions_coexist_without_sibling_unknown_diagnostics() {
    let probe = MixedProbe::default();
    let root_probe = probe.clone();
    let (port, _) = Port::new([Script::Finite(text_facts(&[
        "<pick value=\"7\"/><tag>managed value</tag>",
    ]))]);
    let mut app =
        Application::mount(move || mixed_callbacks(root_probe.clone(), false), port).unwrap();
    let _ = app.react().await.unwrap();
    assert_eq!(*probe.picks.lock().unwrap(), [7]);
    assert_eq!(*probe.published.lock().unwrap(), ["managed value"]);
    assert!(probe.diagnostics.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_names_between_simple_and_managed_actions_fail_before_submission() {
    let probe = MixedProbe::default();
    let (port, capture) = Port::new([completed()]);
    let result = match Application::mount(move || mixed_callbacks(probe.clone(), true), port) {
        Ok(mut application) => application.react().await.map(|_| ()),
        Err(error) => Err(error),
    };
    let error = result.expect_err("one action name cannot select direct and managed handlers");
    assert_eq!(error.kind(), ApplicationFaultKind::Terminal);
    assert!(
        capture.views().is_empty(),
        "an ambiguous action catalog must never be handed off"
    );
}
