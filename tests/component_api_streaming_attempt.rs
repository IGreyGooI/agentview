use std::{
    collections::VecDeque,
    convert::Infallible,
    future::IntoFuture,
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, ExitReason, Frame, FrameCapabilities, FrameConstraints,
            FrameProfile, FrameRevision, ProviderFact, ProviderFactStream, ProviderOutputKey,
            Reaction, ReactionPort, ReactionPortFault, StreamingToolRecoveryReport,
            StreamingToolRecoveryStatus, SubmitFault, TargetDeclaration, TargetEpoch,
            TargetIdentity,
        },
        prelude::*,
    },
    transcript::AssistantPhase,
};
use async_trait::async_trait;

#[derive(Clone, Default)]
struct Probe {
    published: Arc<Mutex<Vec<(String, String)>>>,
    rejected: Arc<Mutex<Vec<String>>>,
    states: Arc<AtomicUsize>,
    submits: Arc<AtomicUsize>,
    handoffs: Arc<AtomicUsize>,
    handoff_revisions: Arc<Mutex<Vec<FrameRevision>>>,
    contexts: Arc<Mutex<Vec<(&'static str, EventContext)>>>,
}

impl Probe {
    fn record(&self, stage: &'static str, context: &EventContext) {
        self.contexts.lock().unwrap().push((stage, context.clone()));
    }
}

async fn recovery_started<P: ReactionPort + 'static>(
    app: &mut Application<P>,
) -> StreamingToolRecoveryStatus {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app.recover_streaming_attempt().await.unwrap() {
                StreamingToolRecoveryStatus::NotRequired => tokio::task::yield_now().await,
                status => break status,
            }
        }
    })
    .await
    .expect("the active reaction must retain its recovery owner")
}

async fn recovery_settled<P: ReactionPort + 'static>(
    app: &mut Application<P>,
) -> Box<[StreamingToolRecoveryReport]> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match recovery_started(app).await {
                StreamingToolRecoveryStatus::Recovered { attempts, .. } => break attempts,
                StreamingToolRecoveryStatus::InFlight { .. }
                | StreamingToolRecoveryStatus::StillRequired { .. } => {
                    tokio::task::yield_now().await
                }
                StreamingToolRecoveryStatus::NotRequired => unreachable!(),
            }
        }
    })
    .await
    .expect("retained effects must settle without replay")
}

fn assert_recovery_origin(report: &StreamingToolRecoveryReport, reaction: &Reaction) {
    assert_eq!(report.context.reaction_id(), Some(reaction.id()));
    assert_eq!(report.context.reaction_kind(), Some(reaction.kind()));
    assert!(report.context.frame_revision().is_some());
}

struct Channels;

impl StreamingToolChannels for Channels {
    type Output = String;
    type Live = NoStreamingValue;
    type Commit = NoStreamingValue;
    type Diagnostic = &'static str;
}

struct PanicChannels;

impl StreamingToolChannels for PanicChannels {
    type Output = NoStreamingValue;
    type Live = NoStreamingValue;
    type Commit = NoStreamingValue;
    type Diagnostic = ();
}

struct Publisher(Probe);

#[async_trait]
impl StreamingToolAttemptPublisher<Channels> for Publisher {
    type Published = ();
    type PublicationOperation = AcceptedStreamingToolAttempt<Channels>;
    type Error = Infallible;

    fn prepare(
        &mut self,
        context: &StreamingPublishContext,
        attempt: Self::PublicationOperation,
    ) -> Result<Self::PublicationOperation, Self::Error> {
        self.0
            .record("prepare_publication", context.event_context());
        self.0.record("accepted", attempt.context.event_context());
        for diagnostic in &attempt.diagnostics {
            self.0.record("diagnostic", &diagnostic.context);
        }
        Ok(attempt)
    }

    async fn publish(
        &mut self,
        context: &StreamingPublishContext,
        operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<(), Infallible> {
        self.0.record("publish", context.event_context());
        let mut outputs = self.0.published.lock().unwrap();
        for entry in &operation.entries {
            self.0.record("staged", &entry.context);
            match &entry.value {
                StagedStreamingToolValue::Output(text) => outputs.push((
                    operation.context.contract_identity().to_owned(),
                    text.clone(),
                )),
                StagedStreamingToolValue::Commit(never) => match *never {},
            }
        }
        StreamingPublishOutcome::Published(())
    }

    async fn resolve(
        &mut self,
        _: &StreamingPublishRecoveryContext,
        _: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<()>, Infallible> {
        Ok(StreamingPublishResolution::Published(()))
    }
}

#[derive(Clone)]
struct ContractProps {
    identity: &'static str,
    tag: &'static str,
    probe: Probe,
    reject: bool,
    ignore_unknown: bool,
}

#[component]
fn text_contract(props: ContractProps) -> Component {
    let ContractProps {
        identity,
        tag,
        probe,
        reject,
        ignore_unknown,
    } = props;
    let state_probe = probe.clone();
    let publisher_probe = probe.clone();
    let open_probe = probe.clone();
    let observer_probe = probe.clone();
    let delta_probe = probe.clone();
    let delta_observer_probe = probe.clone();
    let complete_probe = probe.clone();
    let finish_probe = probe.clone();
    let contract = XmlToolElement::text(tag)
        .occurs(XmlCardinality::exactly(1))
        .decode(|_| Ok(()), |_, text| Ok(text.to_owned()));
    let builder = XmlStreamingToolCall::new::<Channels>(identity).state_with(move |start| {
        state_probe.record("state", start.event_context());
        state_probe.states.fetch_add(1, Ordering::SeqCst);
        Ok::<_, Infallible>(Vec::<String>::new())
    });
    let builder = if ignore_unknown {
        builder.ignore_unknown_elements()
    } else {
        builder
    };
    builder
        .element(contract, move |handlers| {
            handlers
                .on_open(move |_, event| {
                    open_probe.record("open", &event.context);
                    StreamingToolUpdate::none()
                })
                .on_open(move |_, event| {
                    observer_probe.record("open_observer", &event.context);
                    StreamingToolUpdate::none()
                })
                .on_delta(move |_, event| {
                    delta_probe.record("delta", &event.context);
                    StreamingToolUpdate::none()
                })
                .on_delta(move |_, event| {
                    delta_observer_probe.record("delta_observer", &event.context);
                    StreamingToolUpdate::none()
                })
                .on_complete(move |state, event| {
                    complete_probe.record("complete", &event.context);
                    let EventSource::Xml {
                        attempt,
                        occurrence,
                        sequence,
                    } = event.context.source()
                    else {
                        unreachable!()
                    };
                    assert_eq!(*attempt, event.attempt);
                    assert_eq!(*occurrence, Some(event.occurrence));
                    assert_eq!(*sequence, event.sequence);
                    state.push(event.value);
                    StreamingToolUpdate::none()
                })
        })
        .finish(move |values, summary| {
            finish_probe.record("finish", &summary.context);
            if reject || !summary.diagnostics.is_empty() || values.len() != 1 {
                StreamingToolDecision::Reject(StreamingToolRejection::none())
            } else {
                StreamingToolDecision::Accept(StreamingToolUpdate::output(
                    values.into_iter().next().unwrap(),
                ))
            }
        })
        .without_live()
        .publish_with(move |start| {
            publisher_probe.record("publisher_factory", start.event_context());
            Ok::<_, Infallible>(Publisher(publisher_probe.clone()))
        })
        .on_rejected(move |report| {
            let probe = probe.clone();
            async move {
                probe.record("rejected", report.context.event_context());
                for diagnostic in &report.diagnostics {
                    probe.record("diagnostic", &diagnostic.context);
                }
                probe
                    .rejected
                    .lock()
                    .unwrap()
                    .push(report.context.contract_identity().to_owned());
                Ok::<_, Infallible>(StreamingToolRejectionAction::Complete)
            }
        })
        .build()
}

#[component]
fn pair(probe: Probe, reject_first: bool, separate_tags: bool) -> Component {
    view! {
        text_contract(ContractProps { identity: "first", tag: "say", probe: probe.clone(), reject: reject_first, ignore_unknown: separate_tags })
        text_contract(ContractProps { identity: "second", tag: if separate_tags { "think" } else { "say" }, probe, reject: false, ignore_unknown: separate_tags })
    }
}

struct Port {
    declaration: TargetDeclaration,
    scripts: VecDeque<Vec<ProviderFact>>,
    probe: Probe,
    continuity_rejections: usize,
    on_continuity_rejection: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Port {
    fn new(probe: Probe, scripts: Vec<Vec<ProviderFact>>) -> Self {
        Self {
            declaration: TargetDeclaration::full(
                TargetIdentity::new(NonZeroU128::new(101).unwrap()),
                TargetEpoch::new(NonZeroU64::new(1).unwrap()),
                FrameProfile::new(
                    FrameConstraints {
                        max_frame_bytes: 64 * 1024,
                        max_component_bytes: 16 * 1024,
                        context_window: ContextWindow::NotApplicable,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true)
                        .with_native_tool_calls(true)
                        .with_text_delta_output(true),
                ),
            ),
            scripts: scripts.into(),
            probe,
            continuity_rejections: 0,
            on_continuity_rejection: None,
        }
    }

    fn with_continuity_rejection(
        probe: Probe,
        scripts: Vec<Vec<ProviderFact>>,
        on_continuity_rejection: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let mut port = Self::new(probe, scripts);
        port.continuity_rejections = 1;
        port.on_continuity_rejection = Some(on_continuity_rejection);
        port
    }
}

#[async_trait]
impl ReactionPort for Port {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.probe.submits.fetch_add(1, Ordering::SeqCst);
        if self.continuity_rejections > 0 {
            self.continuity_rejections -= 1;
            if let Some(on_rejection) = &self.on_continuity_rejection {
                on_rejection();
            }
            let next_epoch = self
                .declaration
                .continuity()
                .epoch()
                .get()
                .get()
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .expect("test epoch space");
            self.declaration = TargetDeclaration::full(
                self.declaration.identity(),
                TargetEpoch::new(next_epoch),
                self.declaration.profile().clone(),
            );
        }
        frame.check_handoff_precondition(&self.declaration)?;
        self.probe
            .handoff_revisions
            .lock()
            .unwrap()
            .push(frame.revision());
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        self.probe.handoffs.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::iter(
            self.scripts
                .pop_front()
                .expect("scripted reaction")
                .into_iter()
                .map(Ok),
        )))
    }
}

fn assert_continue(flow: impl Into<agentview::component::execution::ReactionOutcome>) {
    assert_eq!(
        flow.into(),
        agentview::component::execution::ReactionOutcome::Completed
    );
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

#[component]
fn panicking_reducer_contract() -> Component {
    XmlStreamingToolCall::new::<PanicChannels>("panic-reducer")
        .state_with(|_| Ok::<_, Infallible>(()))
        .element(
            XmlToolElement::text("say")
                .occurs(XmlCardinality::exactly(1))
                .decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
            |handlers| {
                handlers.on_complete(|_, _| -> StreamingToolUpdate<PanicChannels> {
                    panic!("streaming reducer panic payload")
                })
            },
        )
        .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
        .without_live()
        .without_publication()
        .build()
}

#[tokio::test]
async fn separate_components_independently_parse_the_same_tag_each_reaction() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || pair(root_probe.clone(), false, false),
        Port::new(
            probe.clone(),
            vec![
                text_facts(&["<sa", "y>A &am", "p; B</say>"]),
                text_facts(&["<say>next</say>"]),
            ],
        ),
    )
    .unwrap();
    assert_eq!(probe.states.load(Ordering::SeqCst), 0);
    assert_continue(app.prepare().await.unwrap());
    assert_eq!(probe.states.load(Ordering::SeqCst), 0);
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_continue(app.react(false).unwrap().await.unwrap());
    let mut actual = probe.published.lock().unwrap().clone();
    actual.sort();
    assert_eq!(
        actual,
        vec![
            ("first".into(), "A & B".into()),
            ("first".into(), "next".into()),
            ("second".into(), "A & B".into()),
            ("second".into(), "next".into()),
        ]
    );
    assert_eq!(probe.states.load(Ordering::SeqCst), 4);
    assert_eq!(probe.submits.load(Ordering::SeqCst), 2);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn managed_context_uses_the_successful_handoff_and_multicast_shares_event_identity() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || {
            text_contract(ContractProps {
                identity: "provenance",
                tag: "say",
                probe: root_probe.clone(),
                reject: false,
                ignore_unknown: false,
            })
        },
        Port::with_continuity_rejection(
            probe.clone(),
            vec![text_facts(&["<say>one", " two</say>"])],
            Arc::new(|| {}),
        ),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_eq!(probe.submits.load(Ordering::SeqCst), 2);
    assert_eq!(probe.states.load(Ordering::SeqCst), 1);
    let revision = probe.handoff_revisions.lock().unwrap()[0];
    let contexts = probe.contexts.lock().unwrap();
    let start = &contexts
        .iter()
        .find(|(stage, _)| *stage == "state")
        .unwrap()
        .1;
    assert!(start.reaction_id().is_some());
    for (_, context) in contexts.iter() {
        assert_eq!(context.application_id(), app.application_id());
        assert_eq!(context.reaction_id(), start.reaction_id());
        assert_eq!(context.frame_revision(), Some(revision));
        let EventSource::Xml { attempt, .. } = context.source() else {
            panic!("XML attempt source")
        };
        let EventSource::Xml {
            attempt: expected, ..
        } = start.source()
        else {
            unreachable!()
        };
        assert_eq!(attempt, expected);
    }
    for (stage, context) in contexts.iter() {
        if matches!(*stage, "open" | "delta") {
            let observer = if *stage == "open" {
                "open_observer"
            } else {
                "delta_observer"
            };
            assert!(contexts
                .iter()
                .any(|(other, seen)| *other == observer && seen == context));
            assert_eq!(context.caused_by(), Some(start.event_id()));
        }
        if matches!(
            *stage,
            "publisher_factory" | "prepare_publication" | "accepted" | "publish"
        ) {
            assert_eq!(context.event_id(), start.event_id());
        }
    }
    let finish = &contexts
        .iter()
        .find(|(stage, _)| *stage == "finish")
        .unwrap()
        .1;
    let staged = &contexts
        .iter()
        .find(|(stage, _)| *stage == "staged")
        .unwrap()
        .1;
    assert_eq!(staged, finish);
}

#[tokio::test]
async fn rejected_diagnostic_keeps_its_xml_source_after_the_next_reaction() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || {
            text_contract(ContractProps {
                identity: "diagnostics",
                tag: "say",
                probe: root_probe.clone(),
                reject: false,
                ignore_unknown: false,
            })
        },
        Port::new(
            probe.clone(),
            vec![
                text_facts(&["<say>unfinished"]),
                text_facts(&["<say>next</say>"]),
            ],
        ),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    let diagnostic = probe
        .contexts
        .lock()
        .unwrap()
        .iter()
        .find(|(stage, _)| *stage == "diagnostic")
        .expect("incomplete occurrence diagnostic")
        .1
        .clone();
    let rejected = probe
        .contexts
        .lock()
        .unwrap()
        .iter()
        .find(|(stage, _)| *stage == "rejected")
        .expect("rejection report")
        .1
        .clone();
    assert_eq!(diagnostic.application_id(), app.application_id());
    assert_eq!(diagnostic.reaction_id(), rejected.reaction_id());
    assert_eq!(diagnostic.frame_revision(), rejected.frame_revision());
    assert_eq!(diagnostic.caused_by(), Some(rejected.event_id()));
    assert!(matches!(
        diagnostic.source(),
        EventSource::Xml {
            occurrence: Some(_),
            ..
        }
    ));
    assert_continue(app.react(false).unwrap().await.unwrap());
    let contexts = probe.contexts.lock().unwrap();
    let current = &contexts
        .iter()
        .rev()
        .find(|(stage, _)| *stage == "state")
        .unwrap()
        .1;
    assert_ne!(diagnostic.reaction_id(), current.reaction_id());
    assert_ne!(diagnostic.frame_revision(), current.frame_revision());
    assert!(contexts
        .iter()
        .any(|(stage, context)| *stage == "diagnostic" && context == &diagnostic));
}

#[tokio::test]
async fn rejecting_one_contract_does_not_reject_or_rollback_another() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || pair(root_probe.clone(), true, false),
        Port::new(probe.clone(), vec![text_facts(&["<say>accepted</say>"])]),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_eq!(
        *probe.published.lock().unwrap(),
        vec![("second".into(), "accepted".into())]
    );
    assert_eq!(*probe.rejected.lock().unwrap(), vec!["first"]);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_element_policy_is_local_to_each_contract() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || pair(root_probe.clone(), false, true),
        Port::new(
            probe.clone(),
            vec![text_facts(&["<think>plan</think><say>hello</say>"])],
        ),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    let mut actual = probe.published.lock().unwrap().clone();
    actual.sort();
    assert_eq!(
        actual,
        vec![
            ("first".into(), "hello".into()),
            ("second".into(), "plan".into())
        ]
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn commentary_is_not_parsed_and_sealed_only_text_is_still_consumed() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let commentary = ProviderOutputKey::new(1);
    let primary = ProviderOutputKey::new(2);
    let facts = vec![
        ProviderFact::TextDelta {
            output: commentary,
            phase: Some(AssistantPhase::Commentary),
            delta: "<say>not an action</say>".into(),
        },
        ProviderFact::TextSealed {
            output: commentary,
            phase: Some(AssistantPhase::Commentary),
            text: "<say>not an action</say>".into(),
        },
        ProviderFact::TextSealed {
            output: primary,
            phase: None,
            text: "<say>answer</say>".into(),
        },
        ProviderFact::ReactionCompleted {
            primary_text: Some(primary),
        },
    ];
    let mut app = Application::mount(
        move || {
            text_contract(ContractProps {
                identity: "answer",
                tag: "say",
                probe: root_probe.clone(),
                reject: false,
                ignore_unknown: false,
            })
        },
        Port::new(probe.clone(), vec![facts]),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_eq!(
        *probe.published.lock().unwrap(),
        vec![("answer".into(), "answer".into())]
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_reactions_finalize_each_contract_without_fake_text_completion() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || pair(root_probe.clone(), false, false),
        Port::new(
            probe.clone(),
            vec![vec![ProviderFact::ReactionCompleted { primary_text: None }]],
        ),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    let mut rejected = probe.rejected.lock().unwrap().clone();
    rejected.sort();
    assert_eq!(rejected, vec!["first", "second"]);
    assert!(probe.published.lock().unwrap().is_empty());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_from_a_continuity_rejection_prevents_retry_submission() {
    let probe = Probe::default();
    let exit = Arc::new(Mutex::new(None::<ApplicationExitHandle>));
    let exit_on_rejection = Arc::clone(&exit);
    let port = Port::with_continuity_rejection(
        probe.clone(),
        vec![text_facts(&["<say>must-not-run</say>"])],
        Arc::new(move || {
            exit_on_rejection
                .lock()
                .unwrap()
                .as_ref()
                .expect("host exit handle before provider submit")
                .request(ExitReason::Requested)
                .expect("live host exit handle");
        }),
    );
    let mut app = Application::mount(
        || {
            text_contract(ContractProps {
                identity: "continuity-exit",
                tag: "say",
                probe: Probe::default(),
                reject: false,
                ignore_unknown: false,
            })
        },
        port,
    )
    .unwrap();
    *exit.lock().unwrap() = Some(app.exit_handle());

    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    assert_eq!(probe.submits.load(Ordering::SeqCst), 1);
    assert_eq!(probe.handoffs.load(Ordering::SeqCst), 0);
    assert!(probe.published.lock().unwrap().is_empty());
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    assert_eq!(probe.submits.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn reducer_panic_preserves_its_original_payload_at_the_react_boundary() {
    use futures::FutureExt as _;

    let mut app = Application::mount(
        panicking_reducer_contract,
        Port::new(Probe::default(), vec![text_facts(&["<say>boom</say>"])]),
    )
    .unwrap();

    let panic = std::panic::AssertUnwindSafe(std::future::IntoFuture::into_future(
        app.react(false).unwrap(),
    ))
    .catch_unwind()
    .await
    .expect_err("the streaming reducer panic must escape Application::react");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"streaming reducer panic payload")
    );

    assert_eq!(
        app.shutdown().await.unwrap_err().kind(),
        agentview::component::execution::ApplicationFaultKind::Terminal
    );
}

#[derive(Default)]
struct LiveProbe {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    gate: std::sync::atomic::AtomicBool,
    rollback_started: tokio::sync::Notify,
    rollback_release: tokio::sync::Notify,
    rollback_gate: std::sync::atomic::AtomicBool,
    indeterminate: AtomicBool,
    active: Mutex<Vec<u64>>,
    confirmed: AtomicUsize,
    rolled_back: AtomicUsize,
}

struct LiveChannels;
impl StreamingToolChannels for LiveChannels {
    type Output = NoStreamingValue;
    type Live = u64;
    type Commit = NoStreamingValue;
    type Diagnostic = ();
}

struct LiveRuntime(Arc<LiveProbe>);

#[async_trait]
impl LiveEffectRuntime<u64> for LiveRuntime {
    type Receipt = u64;
    type ApplyOperation = u64;
    type SettlementOperation = ();
    type Error = Infallible;

    fn prepare_apply(&mut self, _: &LiveEffectContext, value: u64) -> Result<u64, Infallible> {
        Ok(value)
    }

    async fn apply(
        &mut self,
        _: &LiveEffectContext,
        value: &u64,
    ) -> LiveApplyOutcome<u64, Infallible> {
        if self.0.indeterminate.swap(false, Ordering::SeqCst) {
            return LiveApplyOutcome::Indeterminate;
        }
        if self.0.gate.swap(false, Ordering::SeqCst) {
            self.0.started.notify_one();
            self.0.release.notified().await;
        }
        self.0.active.lock().unwrap().push(*value);
        LiveApplyOutcome::Applied(*value)
    }

    async fn resolve_apply(
        &mut self,
        _: &LiveRecoveryContext,
        value: &u64,
    ) -> Result<LiveApplyResolution<u64>, Infallible> {
        Ok(if self.0.active.lock().unwrap().contains(value) {
            LiveApplyResolution::Applied(*value)
        } else {
            LiveApplyResolution::NotApplied
        })
    }

    fn prepare_settlement(&mut self, _: &LiveSettlementContext, _: &u64) -> Result<(), Infallible> {
        Ok(())
    }

    async fn confirm(
        &mut self,
        _: &LiveConfirmContext,
        _: &u64,
        _: &(),
    ) -> LiveSettleOutcome<Infallible> {
        self.0.confirmed.fetch_add(1, Ordering::SeqCst);
        LiveSettleOutcome::Settled
    }

    async fn rollback(
        &mut self,
        _: &LiveRollbackContext,
        value: &u64,
        _: &(),
    ) -> LiveSettleOutcome<Infallible> {
        if self.0.rollback_gate.swap(false, Ordering::SeqCst) {
            self.0.rollback_started.notify_one();
            self.0.rollback_release.notified().await;
        }
        let mut active = self.0.active.lock().unwrap();
        let index = active
            .iter()
            .rposition(|candidate| candidate == value)
            .expect("applied receipt");
        active.remove(index);
        self.0.rolled_back.fetch_add(1, Ordering::SeqCst);
        LiveSettleOutcome::Settled
    }

    async fn resolve_settlement(
        &mut self,
        _: &LiveRecoveryContext,
        _: &u64,
        _: &(),
    ) -> Result<LiveSettleResolution, Infallible> {
        Ok(LiveSettleResolution::Settled)
    }
}

fn live_contract(probe: Arc<LiveProbe>) -> Component {
    live_contract_with_identity("live", probe)
}

fn live_contract_with_identity(identity: &'static str, probe: Arc<LiveProbe>) -> Component {
    XmlStreamingToolCall::new::<LiveChannels>(identity)
        .state_with(|_| Ok::<_, Infallible>(()))
        .element(
            XmlToolElement::text("say")
                .occurs(XmlCardinality::exactly(1))
                .decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
            |handlers| {
                handlers
                    .on_open(|_, _| StreamingToolUpdate::live(7))
                    .on_complete(|_, _| StreamingToolUpdate::none())
            },
        )
        .finish(|_, summary| {
            if summary.diagnostics.is_empty() {
                StreamingToolDecision::Accept(StreamingToolUpdate::none())
            } else {
                StreamingToolDecision::Reject(StreamingToolRejection::none())
            }
        })
        .live_with(move |_| Ok::<_, Infallible>(LiveRuntime(probe.clone())))
        .without_publication()
        .build()
}

fn panicking_live_contract(probe: Arc<LiveProbe>) -> Component {
    XmlStreamingToolCall::new::<LiveChannels>("panic-live")
        .state_with(|_| Ok::<_, Infallible>(()))
        .element(
            XmlToolElement::text("say")
                .occurs(XmlCardinality::exactly(1))
                .decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
            |handlers| {
                handlers
                    .on_open(|_, _| StreamingToolUpdate::live(17))
                    .on_complete(|_, _| -> StreamingToolUpdate<LiveChannels> {
                        panic!("detached streaming reducer panic payload")
                    })
            },
        )
        .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
        .live_with(move |_| Ok::<_, Infallible>(LiveRuntime(probe.clone())))
        .without_publication()
        .build()
}

#[tokio::test]
async fn accept_without_publication_still_confirms_live_receipts() {
    let live = Arc::new(LiveProbe::default());
    let root_live = live.clone();
    let mut app = Application::mount(
        move || live_contract(root_live.clone()),
        Port::new(Probe::default(), vec![text_facts(&["<say>hello</say>"])]),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_eq!(live.confirmed.load(Ordering::SeqCst), 1);
    assert_eq!(live.rolled_back.load(Ordering::SeqCst), 0);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupt_keeps_inflight_apply_owned_then_rolls_back_and_allows_reuse() {
    use agentview::component::execution::{ApplicationFaultKind, ReactionOutcome};

    let live = Arc::new(LiveProbe::default());
    live.gate.store(true, Ordering::SeqCst);
    let root_live = live.clone();
    let mut app = Application::mount(
        move || live_contract(root_live.clone()),
        Port::new(
            Probe::default(),
            vec![
                text_facts(&["<say>cancelled</say>"]),
                text_facts(&["<say>next</say>"]),
            ],
        ),
    )
    .unwrap();
    let control = app.react(false).unwrap();
    let mut waiter = Box::pin(control.clone().into_future());
    tokio::select! {
        _ = live.started.notified() => {},
        result = &mut waiter => panic!("reaction completed before gated apply: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("apply did not start"),
    }
    drop(waiter);
    control.interrupt().unwrap();
    let status = recovery_started(&mut app).await;
    let StreamingToolRecoveryStatus::InFlight { attempts } = status else {
        panic!("the pending apply must still be owned");
    };
    assert_eq!(attempts.len(), 1);
    assert_recovery_origin(&attempts[0], &control);
    assert_eq!(
        app.prepare().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    live.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), control.into_future())
            .await
            .expect("interrupt cleanup must finish")
            .unwrap(),
        ReactionOutcome::Interrupted
    );
    assert!(matches!(
        app.recover_streaming_attempt().await.unwrap(),
        StreamingToolRecoveryStatus::NotRequired
    ));
    assert!(live.active.lock().unwrap().is_empty());
    assert_eq!(live.rolled_back.load(Ordering::SeqCst), 1);
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_eq!(live.confirmed.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropped_waiter_preserves_reducer_panic_until_cleanup_is_definitive() {
    use futures::FutureExt as _;

    let live = Arc::new(LiveProbe::default());
    live.rollback_gate.store(true, Ordering::SeqCst);
    let root_live = live.clone();
    let mut app = Application::mount(
        move || panicking_live_contract(root_live.clone()),
        Port::new(Probe::default(), vec![text_facts(&["<say>boom</say>"])]),
    )
    .unwrap();
    let control = app.react(false).unwrap();
    let mut waiter = Box::pin(control.clone().into_future());
    tokio::select! {
        _ = live.rollback_started.notified() => {},
        result = &mut waiter => panic!("reaction completed before panic cleanup blocked: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("panic cleanup did not start rollback"),
    }
    drop(waiter);
    assert!(control.clone().into_future().now_or_never().is_none());
    live.rollback_release.notify_one();
    let panic = tokio::time::timeout(
        Duration::from_secs(5),
        std::panic::AssertUnwindSafe(control.into_future()).catch_unwind(),
    )
    .await
    .expect("owned cleanup must finish")
    .unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"detached streaming reducer panic payload")
    );
    assert!(live.active.lock().unwrap().is_empty());
    assert_eq!(live.rolled_back.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.shutdown().await.unwrap_err().kind(),
        agentview::component::execution::ApplicationFaultKind::Terminal
    );
}

fn ordered_contract(probe: Probe) -> Component {
    let publish_probe = probe.clone();
    XmlStreamingToolCall::new::<Channels>("ordered")
        .state_with(|_| Ok::<_, Infallible>((false, None::<String>)))
        .element(
            XmlToolElement::text("think")
                .occurs(XmlCardinality::exactly(1))
                .decode(|_| Ok(()), |_, _| Ok(())),
            |handlers| {
                handlers.on_complete(|state, _| {
                    state.0 = true;
                    StreamingToolUpdate::none()
                })
            },
        )
        .element(
            XmlToolElement::text("say")
                .occurs(XmlCardinality::exactly(1))
                .decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
            |handlers| {
                handlers
                    .on_open(|state, _| {
                        if state.0 {
                            StreamingToolUpdate::none()
                        } else {
                            StreamingToolUpdate::diagnostic("say_before_think")
                        }
                    })
                    .on_complete(|state, event| {
                        state.1 = Some(event.value);
                        StreamingToolUpdate::none()
                    })
            },
        )
        .finish(|state, summary| {
            if summary.diagnostics.is_empty() {
                StreamingToolDecision::Accept(StreamingToolUpdate::output(state.1.unwrap()))
            } else {
                StreamingToolDecision::Reject(StreamingToolRejection::none())
            }
        })
        .without_live()
        .publish_with(move |_| Ok::<_, Infallible>(Publisher(publish_probe.clone())))
        .on_rejected(move |_| {
            let probe = probe.clone();
            async move {
                probe.rejected.lock().unwrap().push("ordered".into());
                Ok::<_, Infallible>(StreamingToolRejectionAction::RequestReaction)
            }
        })
        .build()
}

#[tokio::test]
async fn shared_state_enforces_think_before_say_and_coalesces_retry_after_cleanup() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || ordered_contract(root_probe.clone()),
        Port::new(
            probe.clone(),
            vec![
                text_facts(&["<say>too early</say><think>plan</think>"]),
                text_facts(&["<think>plan</think><say>answer</say>"]),
            ],
        ),
    )
    .unwrap();
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert!(probe.published.lock().unwrap().is_empty());
    assert_eq!(*probe.rejected.lock().unwrap(), vec!["ordered"]);
    assert!(app.take_reaction_request().unwrap());
    assert!(!app.take_reaction_request().unwrap());
    assert_continue(app.react(false).unwrap().await.unwrap());
    assert_eq!(
        *probe.published.lock().unwrap(),
        vec![("ordered".into(), "answer".into())]
    );
    assert!(!app.take_reaction_request().unwrap());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_elements_fail_before_state_factory_or_provider_submission() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let result = Application::mount(
        move || {
            let state_probe = root_probe.clone();
            XmlStreamingToolCall::new::<LiveChannels>("duplicate")
                .state_with(move |_| {
                    state_probe.states.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(())
                })
                .element(
                    XmlToolElement::text("same").decode(|_| Ok(()), |_, _| Ok(())),
                    |handlers| handlers.on_complete(|_, _| StreamingToolUpdate::none()),
                )
                .element(
                    XmlToolElement::text("same").decode(|_| Ok(()), |_, _| Ok(())),
                    |handlers| handlers.on_complete(|_, _| StreamingToolUpdate::none()),
                )
                .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
                .live_with(|_| Ok::<_, Infallible>(LiveRuntime(Arc::new(LiveProbe::default()))))
                .without_publication()
                .build()
        },
        Port::new(probe.clone(), vec![]),
    );
    assert!(result.is_err());
    assert_eq!(probe.states.load(Ordering::SeqCst), 0);
    assert_eq!(probe.submits.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct RecoveryProbe {
    published: AtomicUsize,
    resolved: AtomicUsize,
    not_published: AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct UncertainPublisher(Arc<RecoveryProbe>);

#[async_trait]
impl StreamingToolAttemptPublisher<Channels> for UncertainPublisher {
    type Published = ();
    type PublicationOperation = AcceptedStreamingToolAttempt<Channels>;
    type Error = std::io::Error;

    fn prepare(
        &mut self,
        _: &StreamingPublishContext,
        attempt: Self::PublicationOperation,
    ) -> Result<Self::PublicationOperation, Self::Error> {
        Ok(attempt)
    }

    async fn publish(
        &mut self,
        _: &StreamingPublishContext,
        _: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<(), Self::Error> {
        self.0.published.fetch_add(1, Ordering::SeqCst);
        StreamingPublishOutcome::Indeterminate
    }

    async fn resolve(
        &mut self,
        _: &StreamingPublishRecoveryContext,
        _: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<()>, Self::Error> {
        self.0.resolved.fetch_add(1, Ordering::SeqCst);
        self.0.started.notify_one();
        self.0.release.notified().await;
        Ok(if self.0.not_published.load(Ordering::SeqCst) {
            StreamingPublishResolution::NotPublished
        } else {
            StreamingPublishResolution::Published(())
        })
    }
}

#[component]
fn uncertain_with_completion(
    recovery: Arc<RecoveryProbe>,
    completions: Arc<Mutex<Vec<EventContext>>>,
) -> Component {
    use_reaction_completion(move |context| async move {
        completions.lock().unwrap().push(context);
        Ok::<_, Infallible>(())
    });
    XmlStreamingToolCall::new::<Channels>("completion-recovery")
        .state_with(|_| Ok::<_, Infallible>(()))
        .element(
            XmlToolElement::text("say").decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
            |handlers| handlers.on_complete(|_, event| StreamingToolUpdate::output(event.value)),
        )
        .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
        .without_live()
        .publish_with(move |_| Ok::<_, Infallible>(UncertainPublisher(recovery.clone())))
        .build()
}

#[tokio::test]
async fn managed_completion_waits_for_publication_recovery_and_requires_success() {
    use agentview::component::execution::ApplicationFaultKind;

    for not_published in [false, true] {
        let recovery = Arc::new(RecoveryProbe::default());
        recovery
            .not_published
            .store(not_published, Ordering::SeqCst);
        let completions = Arc::new(Mutex::new(Vec::new()));
        let root_recovery = recovery.clone();
        let root_completions = completions.clone();
        let mut app = Application::mount(
            move || uncertain_with_completion(root_recovery.clone(), root_completions.clone()),
            Port::new(Probe::default(), vec![text_facts(&["<say>answer</say>"])]),
        )
        .unwrap();
        let reaction = app.react(false).unwrap();
        let mut recovery_waiter = Box::pin(recovery_started(&mut app));
        tokio::select! {
            _ = recovery.started.notified() => {},
            result = &mut recovery_waiter => panic!("publication recovery ended before resolution: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("publication recovery did not start"),
        }
        assert!(
            completions.lock().unwrap().is_empty(),
            "normal completion must wait for managed effects"
        );
        recovery.release.notify_one();
        let StreamingToolRecoveryStatus::Recovered { attempts, .. } = recovery_waiter.await else {
            panic!("publication recovery must become definitive");
        };
        assert_recovery_origin(&attempts[0], &reaction);
        assert_eq!(
            reaction.clone().await.unwrap_err().kind(),
            ApplicationFaultKind::RecoveryRequired
        );
        let contexts = completions.lock().unwrap().clone();
        assert_eq!(
            contexts.len(),
            usize::from(!not_published),
            "failed publication must not publish a successful completion callback"
        );
        if let Some(context) = contexts.first() {
            assert_eq!(context.reaction_id(), Some(reaction.id()));
            assert_eq!(context.reaction_kind(), Some(ReactionKind::Main));
            assert_eq!(
                context.frame_revision(),
                attempts[0].context.frame_revision()
            );
        }
        app.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn dropped_recovery_waiter_preserves_publication_and_settled_sibling() {
    use agentview::component::execution::{ApplicationFaultKind, StreamingToolRecoveryStatus};
    use std::time::Duration;

    let probe = Probe::default();
    let recovery = Arc::new(RecoveryProbe::default());
    let root_probe = probe.clone();
    let root_recovery = recovery.clone();
    let mut app = Application::mount(
        move || {
            let publisher_probe = root_recovery.clone();
            let uncertain = XmlStreamingToolCall::new::<Channels>("uncertain")
                .state_with(|_| Ok::<_, Infallible>(()))
                .element(
                    XmlToolElement::text("say").decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
                    |handlers| {
                        handlers.on_complete(|_, event| StreamingToolUpdate::output(event.value))
                    },
                )
                .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
                .without_live()
                .publish_with(move |_| {
                    Ok::<_, Infallible>(UncertainPublisher(publisher_probe.clone()))
                })
                .build();
            let sibling = text_contract(ContractProps {
                identity: "settled",
                tag: "say",
                probe: root_probe.clone(),
                reject: false,
                ignore_unknown: false,
            });
            view! { {uncertain} {sibling} }
        },
        Port::new(probe.clone(), vec![text_facts(&["<say>answer</say>"])]),
    )
    .unwrap();
    let reaction = app.react(false).unwrap();
    let mut reaction_waiter = Box::pin(reaction.clone().into_future());
    let mut waiter = Box::pin(recovery_started(&mut app));
    tokio::select! {
        _ = recovery.started.notified() => {},
        result = &mut reaction_waiter => panic!("reaction completed before publication resolution: {result:?}"),
        _ = &mut waiter => panic!("recovery completed before resolution"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("resolution did not start"),
    }
    drop(waiter);
    assert_eq!(
        *probe.published.lock().unwrap(),
        vec![("settled".into(), "answer".into())]
    );
    assert_eq!(
        app.prepare().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    assert_eq!(
        app.take_reaction_request().unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    let StreamingToolRecoveryStatus::InFlight { attempts } =
        app.recover_streaming_attempt().await.unwrap()
    else {
        panic!("publication resolution must remain owned");
    };
    assert_eq!(attempts.len(), 2);
    for report in &attempts {
        assert_recovery_origin(report, &reaction);
    }
    drop(reaction_waiter);
    recovery.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), reaction.into_future())
            .await
            .expect("owned recovery must finish without its waiter")
            .unwrap_err()
            .kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    assert!(matches!(
        app.recover_streaming_attempt().await.unwrap(),
        StreamingToolRecoveryStatus::NotRequired
    ));
    assert_eq!(recovery.published.load(Ordering::SeqCst), 1);
    assert_eq!(recovery.resolved.load(Ordering::SeqCst), 1);
    assert_eq!(probe.submits.load(Ordering::SeqCst), 1);
    assert_eq!(probe.published.lock().unwrap().len(), 1);
    assert_continue(app.prepare().await.unwrap());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_waits_for_indeterminate_publication_recovery_before_breaking() {
    use agentview::component::execution::{ApplicationFaultKind, StreamingToolRecoveryStatus};
    use std::time::Duration;

    let probe = Probe::default();
    let recovery = Arc::new(RecoveryProbe::default());
    let root_recovery = Arc::clone(&recovery);
    let mut app = Application::mount(
        move || {
            let publisher_probe = Arc::clone(&root_recovery);
            XmlStreamingToolCall::new::<Channels>("exit-recovery")
                .state_with(|_| Ok::<_, Infallible>(()))
                .element(
                    XmlToolElement::text("say").decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
                    |handlers| {
                        handlers.on_complete(|_, event| StreamingToolUpdate::output(event.value))
                    },
                )
                .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
                .without_live()
                .publish_with(move |_| {
                    Ok::<_, Infallible>(UncertainPublisher(Arc::clone(&publisher_probe)))
                })
                .build()
        },
        Port::new(probe.clone(), vec![text_facts(&["<say>answer</say>"])]),
    )
    .unwrap();
    let exit = app.exit_handle();

    let reaction = app.react(false).unwrap();
    let mut reaction_waiter = Box::pin(reaction.clone().into_future());
    let mut recovery_waiter = Box::pin(recovery_started(&mut app));
    tokio::select! {
        _ = recovery.started.notified() => {},
        result = &mut reaction_waiter => panic!("reaction completed before publisher resolution: {result:?}"),
        result = &mut recovery_waiter => panic!("recovery settled before publisher resolution: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("publisher recovery did not begin"),
    }
    drop(recovery_waiter);
    exit.request(ExitReason::Requested).unwrap();
    assert!(matches!(
        app.recover_streaming_attempt().await.unwrap(),
        StreamingToolRecoveryStatus::InFlight { .. }
    ));
    assert!(futures::poll!(reaction_waiter.as_mut()).is_pending());
    recovery.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), reaction_waiter)
            .await
            .expect("exit must wait for owned recovery")
            .unwrap_err()
            .kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    assert!(matches!(
        app.recover_streaming_attempt().await.unwrap(),
        StreamingToolRecoveryStatus::NotRequired
    ));
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ControlFlow::Break(ExitReason::Requested)
    );
    assert_eq!(probe.submits.load(Ordering::SeqCst), 1);
    assert_eq!(probe.handoffs.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct ReconcileProbe {
    broken: Arc<Mutex<Option<Signal<bool>>>>,
    rejections: Arc<AtomicUsize>,
}

#[component]
fn rejection_with_failed_reconcile(probe: ReconcileProbe) -> Component {
    let broken = use_signal(|| false);
    *probe.broken.lock().unwrap() = Some(broken.clone());
    let second_tag = if broken.with(|value| *value).unwrap() {
        "say"
    } else {
        "think"
    };
    XmlStreamingToolCall::new::<Channels>("reconcile")
        .state_with(|_| Ok::<_, Infallible>(()))
        .element(
            XmlToolElement::text("say").decode(|_| Ok(()), |_, _| Ok(())),
            |handlers| handlers.on_complete(|_, _| StreamingToolUpdate::none()),
        )
        .element(
            XmlToolElement::text(second_tag).decode(|_| Ok(()), |_, _| Ok(())),
            |handlers| handlers.on_complete(|_, _| StreamingToolUpdate::none()),
        )
        .finish(|_, _| StreamingToolDecision::Reject(StreamingToolRejection::none()))
        .without_live()
        .publish_with(|_| Ok::<_, Infallible>(Publisher(Probe::default())))
        .on_rejected(move |_| {
            let broken = broken.clone();
            let probe = probe.clone();
            async move {
                probe.rejections.fetch_add(1, Ordering::SeqCst);
                broken.set(true).unwrap();
                Ok::<_, Infallible>(StreamingToolRejectionAction::RequestReaction)
            }
        })
        .build()
}

#[tokio::test]
async fn failed_post_reconcile_retains_fence_and_never_replays_rejection() {
    use agentview::component::execution::{
        ApplicationFaultKind, ApplicationFaultStage, StreamingToolRecoveryStatus,
    };
    let probe = ReconcileProbe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || rejection_with_failed_reconcile(root_probe.clone()),
        Port::new(Probe::default(), vec![text_facts(&["<say>rejected</say>"])]),
    )
    .unwrap();
    let reaction = app.react(false).unwrap();
    let fault = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app.recover_streaming_attempt().await {
                Err(fault) => break fault,
                Ok(StreamingToolRecoveryStatus::StillRequired { attempts }) => {
                    assert_eq!(attempts.len(), 1);
                    assert_recovery_origin(&attempts[0], &reaction);
                    break attempts[0]
                        .fault
                        .expect("failed post-reconcile must retain its fault");
                }
                Ok(
                    StreamingToolRecoveryStatus::NotRequired
                    | StreamingToolRecoveryStatus::InFlight { .. },
                ) => tokio::task::yield_now().await,
                status => panic!("failed reconcile lost its cleanup fence: {status:?}"),
            }
        }
    })
    .await
    .expect("failed post-reconcile must become recoverable");
    assert_eq!(fault.stage(), ApplicationFaultStage::PostReconcile);
    assert_eq!(
        app.take_reaction_request().unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    let StreamingToolRecoveryStatus::StillRequired { attempts } =
        app.recover_streaming_attempt().await.unwrap()
    else {
        panic!("failed view must retain its cleanup fence");
    };
    assert_eq!(attempts[0].fault, Some(fault));
    probe
        .broken
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set(false)
        .unwrap();
    let StreamingToolRecoveryStatus::Recovered {
        attempts,
        reaction_requested,
    } = app.recover_streaming_attempt().await.unwrap()
    else {
        panic!("expected completed cleanup");
    };
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].fault, Some(fault));
    assert_recovery_origin(&attempts[0], &reaction);
    assert_eq!(reaction.await.unwrap_err(), fault);
    assert!(!reaction_requested);
    assert!(!app.take_reaction_request().unwrap());
    assert_eq!(probe.rejections.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_tool_lanes_keep_running_while_streaming_live_apply_waits() {
    use agentview::component::execution::ProviderToolCall;
    use std::time::Duration;

    let live = Arc::new(LiveProbe::default());
    live.gate.store(true, Ordering::SeqCst);
    let root_live = live.clone();
    let mut facts = text_facts(&["<say>with native tool</say>"]);
    facts.insert(
        0,
        ProviderFact::ToolCall {
            output: ProviderOutputKey::new(9),
            ordinal: 1,
            call: ProviderToolCall::new("call-1", "release", "{}").unwrap(),
        },
    );
    let mut app = Application::mount(
        move || {
            let live_probe = root_live.clone();
            let native = NativeToolCall::named("release").on_call(move |_ctx, call| {
                let live_probe = live_probe.clone();
                async move {
                    live_probe.started.notified().await;
                    live_probe.release.notify_one();
                    Ok::<_, Infallible>(call.output("released"))
                }
            });
            let streaming = live_contract(root_live.clone());
            view! { {native} {streaming} }
        },
        Port::new(Probe::default(), vec![facts]),
    )
    .unwrap();
    let flow = tokio::time::timeout(
        Duration::from_secs(5),
        app.react(false).unwrap().into_future(),
    )
    .await
    .expect("native lane must progress during Live apply")
    .unwrap();
    assert_continue(flow);
    assert_eq!(live.confirmed.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn publication_recovery_not_published_reports_a_terminal_attempt_fault() {
    use agentview::component::execution::ApplicationFaultKind;

    let recovery = Arc::new(RecoveryProbe::default());
    recovery.not_published.store(true, Ordering::SeqCst);
    let root_recovery = Arc::clone(&recovery);
    let mut app = Application::mount(
        move || {
            let publisher_probe = Arc::clone(&root_recovery);
            XmlStreamingToolCall::new::<Channels>("not-published")
                .state_with(|_| Ok::<_, Infallible>(()))
                .element(
                    XmlToolElement::text("say").decode(|_| Ok(()), |_, text| Ok(text.to_owned())),
                    |handlers| {
                        handlers.on_complete(|_, event| StreamingToolUpdate::output(event.value))
                    },
                )
                .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()))
                .without_live()
                .publish_with(move |_| {
                    Ok::<_, Infallible>(UncertainPublisher(Arc::clone(&publisher_probe)))
                })
                .build()
        },
        Port::new(Probe::default(), vec![text_facts(&["<say>answer</say>"])]),
    )
    .unwrap();

    let reaction = app.react(false).unwrap();
    recovery.release.notify_one();
    let attempts = recovery_settled(&mut app).await;
    assert_recovery_origin(&attempts[0], &reaction);
    assert_eq!(
        reaction.await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );

    assert_eq!(attempts.len(), 1);
    assert!(!attempts[0].accepted);
    assert!(attempts[0].settled);
    assert_eq!(
        attempts[0].fault.unwrap().kind(),
        ApplicationFaultKind::Terminal
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn indeterminate_live_apply_recovery_reports_a_terminal_attempt_fault() {
    use agentview::component::execution::ApplicationFaultKind;

    let live = Arc::new(LiveProbe::default());
    live.indeterminate.store(true, Ordering::SeqCst);
    let root_live = Arc::clone(&live);
    let mut app = Application::mount(
        move || live_contract(Arc::clone(&root_live)),
        Port::new(Probe::default(), vec![text_facts(&["<say>answer</say>"])]),
    )
    .unwrap();

    let reaction = app.react(false).unwrap();
    let attempts = recovery_settled(&mut app).await;
    assert_recovery_origin(&attempts[0], &reaction);
    assert_eq!(
        reaction.await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );

    assert_eq!(attempts.len(), 1);
    assert!(!attempts[0].accepted);
    assert!(attempts[0].settled);
    assert_eq!(
        attempts[0].fault.unwrap().kind(),
        ApplicationFaultKind::Terminal
    );
    assert!(live.active.lock().unwrap().is_empty());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn midstream_recovery_rolls_back_live_receipts_in_sibling_contracts() {
    use agentview::component::execution::ApplicationFaultKind;

    let uncertain = Arc::new(LiveProbe::default());
    uncertain.indeterminate.store(true, Ordering::SeqCst);
    let sibling = Arc::new(LiveProbe::default());
    let root_uncertain = Arc::clone(&uncertain);
    let root_sibling = Arc::clone(&sibling);
    let mut app = Application::mount(
        move || {
            let uncertain = live_contract_with_identity("uncertain", Arc::clone(&root_uncertain));
            let sibling = live_contract_with_identity("sibling", Arc::clone(&root_sibling));
            view! { {uncertain} {sibling} }
        },
        Port::new(Probe::default(), vec![text_facts(&["<say>answer</say>"])]),
    )
    .unwrap();

    let reaction = app.react(false).unwrap();
    let attempts = recovery_settled(&mut app).await;
    assert_recovery_origin(&attempts[0], &reaction);
    assert_eq!(
        reaction.await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );

    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|attempt| attempt.settled));
    assert!(sibling.active.lock().unwrap().is_empty());
    assert_eq!(sibling.rolled_back.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}
