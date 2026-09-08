use std::{
    collections::VecDeque,
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::{
        execution::{
            Application, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderFact,
            ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity,
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
        _: &StreamingPublishContext,
        attempt: Self::PublicationOperation,
    ) -> Result<Self::PublicationOperation, Self::Error> {
        Ok(attempt)
    }

    async fn publish(
        &mut self,
        _: &StreamingPublishContext,
        operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<(), Infallible> {
        let mut outputs = self.0.published.lock().unwrap();
        for entry in &operation.entries {
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
    let contract = XmlToolElement::text(tag)
        .occurs(XmlCardinality::exactly(1))
        .decode(|_| Ok(()), |_, text| Ok(text.to_owned()));
    let builder = XmlStreamingToolCall::new::<Channels>(identity).state_with(move |_| {
        state_probe.states.fetch_add(1, Ordering::SeqCst);
        Ok::<_, Infallible>(Vec::<String>::new())
    });
    let builder = if ignore_unknown {
        builder.ignore_unknown_elements()
    } else {
        builder
    };
    builder
        .element(contract, |handlers| {
            handlers.on_complete(|state, event| {
                state.push(event.value);
                StreamingToolUpdate::none()
            })
        })
        .finish(move |values, summary| {
            if reject || !summary.diagnostics.is_empty() || values.len() != 1 {
                StreamingToolDecision::Reject(StreamingToolRejection::none())
            } else {
                StreamingToolDecision::Accept(StreamingToolUpdate::output(
                    values.into_iter().next().unwrap(),
                ))
            }
        })
        .without_live()
        .publish_with(move |_| Ok::<_, Infallible>(Publisher(publisher_probe.clone())))
        .on_rejected(move |report| {
            let probe = probe.clone();
            async move {
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
                        context_window_tokens: None,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true),
                ),
            ),
            scripts: scripts.into(),
            probe,
        }
    }
}

#[async_trait]
impl ReactionPort for Port {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.probe.submits.fetch_add(1, Ordering::SeqCst);
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        Ok(Box::pin(futures::stream::iter(
            self.scripts
                .pop_front()
                .expect("scripted reaction")
                .into_iter()
                .map(Ok),
        )))
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
    app.prepare().await.unwrap();
    assert_eq!(probe.states.load(Ordering::SeqCst), 0);
    app.react().await.unwrap();
    app.react().await.unwrap();
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
async fn rejecting_one_contract_does_not_reject_or_rollback_another() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || pair(root_probe.clone(), true, false),
        Port::new(probe.clone(), vec![text_facts(&["<say>accepted</say>"])]),
    )
    .unwrap();
    app.react().await.unwrap();
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
    app.react().await.unwrap();
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
    app.react().await.unwrap();
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
    app.react().await.unwrap();
    let mut rejected = probe.rejected.lock().unwrap().clone();
    rejected.sort();
    assert_eq!(rejected, vec!["first", "second"]);
    assert!(probe.published.lock().unwrap().is_empty());
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

    let panic = std::panic::AssertUnwindSafe(app.react())
        .catch_unwind()
        .await
        .expect_err("the streaming reducer panic must escape Application::react");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"streaming reducer panic payload")
    );

    app.shutdown().await.unwrap();
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
    app.react().await.unwrap();
    assert_eq!(live.confirmed.load(Ordering::SeqCst), 1);
    assert_eq!(live.rolled_back.load(Ordering::SeqCst), 0);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_react_keeps_inflight_apply_owned_then_rolls_back_and_allows_reuse() {
    use agentview::component::execution::{ApplicationFaultKind, StreamingToolRecoveryStatus};
    use std::time::Duration;

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
    let mut reaction = Box::pin(app.react());
    tokio::select! {
        _ = live.started.notified() => {},
        result = &mut reaction => panic!("reaction completed before gated apply: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("apply did not start"),
    }
    drop(reaction);
    assert_eq!(
        app.prepare().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    live.release.notify_one();
    let reports = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app.recover_streaming_attempt().await.unwrap() {
                StreamingToolRecoveryStatus::Recovered { attempts, .. } => break attempts,
                StreamingToolRecoveryStatus::InFlight { .. }
                | StreamingToolRecoveryStatus::StillRequired { .. } => {
                    tokio::task::yield_now().await
                }
                StreamingToolRecoveryStatus::NotRequired => {
                    panic!("cancelled contract lost its worker")
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].fault.unwrap().kind(),
        ApplicationFaultKind::Terminal
    );
    assert!(live.active.lock().unwrap().is_empty());
    assert_eq!(live.rolled_back.load(Ordering::SeqCst), 1);
    app.react().await.unwrap();
    assert_eq!(live.confirmed.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropped_react_preserves_reducer_panic_until_recovery_cleanup_is_definitive() {
    use agentview::component::execution::StreamingToolRecoveryStatus;
    use futures::FutureExt as _;
    use std::time::Duration;

    let live = Arc::new(LiveProbe::default());
    live.rollback_gate.store(true, Ordering::SeqCst);
    let root_live = live.clone();
    let mut app = Application::mount(
        move || panicking_live_contract(root_live.clone()),
        Port::new(Probe::default(), vec![text_facts(&["<say>boom</say>"])]),
    )
    .unwrap();

    let mut reaction = Box::pin(app.react());
    tokio::select! {
        _ = live.rollback_started.notified() => {},
        result = &mut reaction => panic!("reaction completed before panic cleanup blocked: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("panic cleanup did not start rollback"),
    }
    drop(reaction);
    live.rollback_release.notify_one();

    let panic = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match std::panic::AssertUnwindSafe(app.recover_streaming_attempt())
                .catch_unwind()
                .await
            {
                Err(payload) => break payload,
                Ok(Ok(StreamingToolRecoveryStatus::InFlight { .. }))
                | Ok(Ok(StreamingToolRecoveryStatus::StillRequired { .. })) => {
                    tokio::task::yield_now().await;
                }
                Ok(Ok(status)) => {
                    panic!("recovery returned instead of resuming the panic: {status:?}")
                }
                Ok(Err(fault)) => {
                    panic!("recovery returned a fault instead of the panic: {fault:?}")
                }
            }
        }
    })
    .await
    .expect("recovery should observe the retained panic payload");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"detached streaming reducer panic payload")
    );
    assert!(live.active.lock().unwrap().is_empty());
    assert_eq!(live.rolled_back.load(Ordering::SeqCst), 1);

    app.shutdown().await.unwrap();
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
    app.react().await.unwrap();
    assert!(probe.published.lock().unwrap().is_empty());
    assert_eq!(*probe.rejected.lock().unwrap(), vec!["ordered"]);
    assert!(app.take_reaction_request().unwrap());
    assert!(!app.take_reaction_request().unwrap());
    app.react().await.unwrap();
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
    assert_eq!(
        app.react().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
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

    let mut waiter = Box::pin(app.recover_streaming_attempt());
    tokio::select! {
        _ = recovery.started.notified() => {},
        _ = &mut waiter => panic!("recovery completed before resolution"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("resolution did not start"),
    }
    drop(waiter);
    assert!(matches!(
        app.recover_streaming_attempt().await.unwrap(),
        StreamingToolRecoveryStatus::InFlight { .. }
    ));
    recovery.release.notify_one();
    let reports = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app.recover_streaming_attempt().await.unwrap() {
                StreamingToolRecoveryStatus::Recovered {
                    attempts,
                    reaction_requested,
                } => {
                    assert!(!reaction_requested);
                    break attempts;
                }
                StreamingToolRecoveryStatus::InFlight { .. } => tokio::task::yield_now().await,
                status => panic!("unexpected recovery status: {status:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports
        .iter()
        .all(|report| report.accepted && report.settled && report.fault.is_none()));
    assert_eq!(recovery.published.load(Ordering::SeqCst), 1);
    assert_eq!(recovery.resolved.load(Ordering::SeqCst), 1);
    assert_eq!(probe.submits.load(Ordering::SeqCst), 1);
    assert_eq!(probe.published.lock().unwrap().len(), 1);
    app.prepare().await.unwrap();
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
    let fault = app.react().await.unwrap_err();
    assert_eq!(fault.stage(), ApplicationFaultStage::PostReconcile);
    assert_eq!(
        app.take_reaction_request().unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    assert_eq!(app.recover_streaming_attempt().await.unwrap_err(), fault);
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
            let native = NativeToolCall::named("release").on_call(move |call| {
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
    tokio::time::timeout(Duration::from_secs(5), app.react())
        .await
        .expect("native lane must progress during Live apply")
        .unwrap();
    assert_eq!(live.confirmed.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn publication_recovery_not_published_reports_a_terminal_attempt_fault() {
    use agentview::component::execution::{ApplicationFaultKind, StreamingToolRecoveryStatus};

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

    assert_eq!(
        app.react().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    recovery.release.notify_one();
    let attempts = loop {
        match app.recover_streaming_attempt().await.unwrap() {
            StreamingToolRecoveryStatus::Recovered { attempts, .. } => break attempts,
            StreamingToolRecoveryStatus::InFlight { .. }
            | StreamingToolRecoveryStatus::StillRequired { .. } => tokio::task::yield_now().await,
            StreamingToolRecoveryStatus::NotRequired => panic!("publication evidence was lost"),
        }
    };

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
    use agentview::component::execution::{ApplicationFaultKind, StreamingToolRecoveryStatus};

    let live = Arc::new(LiveProbe::default());
    live.indeterminate.store(true, Ordering::SeqCst);
    let root_live = Arc::clone(&live);
    let mut app = Application::mount(
        move || live_contract(Arc::clone(&root_live)),
        Port::new(Probe::default(), vec![text_facts(&["<say>answer</say>"])]),
    )
    .unwrap();

    assert_eq!(
        app.react().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    let attempts = loop {
        match app.recover_streaming_attempt().await.unwrap() {
            StreamingToolRecoveryStatus::Recovered { attempts, .. } => break attempts,
            StreamingToolRecoveryStatus::InFlight { .. }
            | StreamingToolRecoveryStatus::StillRequired { .. } => tokio::task::yield_now().await,
            StreamingToolRecoveryStatus::NotRequired => panic!("live apply evidence was lost"),
        }
    };

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
    use agentview::component::execution::{ApplicationFaultKind, StreamingToolRecoveryStatus};

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

    assert_eq!(
        app.react().await.unwrap_err().kind(),
        ApplicationFaultKind::RecoveryRequired
    );
    let attempts = loop {
        match app.recover_streaming_attempt().await.unwrap() {
            StreamingToolRecoveryStatus::Recovered { attempts, .. } => break attempts,
            StreamingToolRecoveryStatus::InFlight { .. }
            | StreamingToolRecoveryStatus::StillRequired { .. } => tokio::task::yield_now().await,
            StreamingToolRecoveryStatus::NotRequired => panic!("sibling cleanup was lost"),
        }
    };

    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|attempt| attempt.settled));
    assert!(sibling.active.lock().unwrap().is_empty());
    assert_eq!(sibling.rolled_back.load(Ordering::SeqCst), 1);
    app.shutdown().await.unwrap();
}
