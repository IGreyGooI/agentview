#[path = "support/pom.rs"]
mod pom_support;
use pom_support::*;

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
            Application, ContextWindow, Frame, FrameBasis, FrameCapabilities, FrameConstraints,
            FrameProfile, ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall,
            ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch,
            TargetIdentity, ToolDefinition,
        },
        prelude::*,
    },
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Notify;

#[derive(Debug, Clone)]
struct CapturedFrame {
    basis: FrameBasis,
    replay: Vec<CanonicalInputItem>,
    staged_inputs: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    ephemeral: Vec<CanonicalInputItem>,
    tools: Vec<ToolDefinition>,
    canonical_payload: Value,
}

impl CapturedFrame {
    fn ordered_inputs(&self) -> Vec<&CanonicalInputItem> {
        self.replay
            .iter()
            .chain(&self.staged_inputs)
            .chain(&self.projection)
            .chain(&self.ephemeral)
            .collect()
    }
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
                staged_inputs: frame.submission().staged_inputs().to_vec(),
                projection: frame.submission().projection().items().to_vec(),
                ephemeral: frame.submission().ephemeral().to_vec(),
                tools: frame.submission().tools().definitions().to_vec(),
                canonical_payload: serde_json::from_slice(frame.submission().canonical_bytes())
                    .expect("Frame has a canonical JSON payload"),
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
                context_window: ContextWindow::NotApplicable,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        );
        (
            Self {
                declaration: TargetDeclaration::full(
                    TargetIdentity::new(NonZeroU128::new(43).expect("non-zero target")),
                    TargetEpoch::new(NonZeroU64::new(1).expect("non-zero epoch")),
                    profile,
                ),
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
            unreachable!("the test cancels the blocked pre-handoff submit");
        }
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        Ok(Box::pin(futures::stream::iter(
            self.control.take_facts().into_iter().map(Ok),
        )))
    }
}

#[derive(Clone)]
struct StateProps<T> {
    initial: T,
    exposed: Arc<Mutex<Option<Signal<T>>>>,
}

impl<T: Clone> StateProps<T> {
    fn new(initial: T) -> Self {
        Self {
            initial,
            exposed: Arc::new(Mutex::new(None)),
        }
    }

    fn signal(&self) -> Signal<T> {
        self.exposed
            .lock()
            .expect("signal exposure lock")
            .clone()
            .expect("component exposes its mounted signal")
    }
}

fn documents(items: &[CanonicalInputItem]) -> Vec<agentview::pom::ResolvedDocument> {
    item_documents(items)
}

fn assert_ephemeral_not_retained(frames: &[CapturedFrame]) {
    let ephemeral: Vec<_> = frames.iter().flat_map(|frame| &frame.ephemeral).collect();
    for frame in frames {
        for item in frame
            .replay
            .iter()
            .chain(&frame.staged_inputs)
            .chain(&frame.projection)
        {
            assert!(
                !ephemeral.contains(&item),
                "raw ephemeral prompts never become retained input: {item:?}"
            );
        }
    }
}

async fn react(application: &mut Application<CapturingPort>) {
    assert_eq!(
        application.react().await.expect("reaction succeeds"),
        ControlFlow::Continue(())
    );
}

#[component]
fn inherited_grandchild() -> Component {
    view! {
        #[user]
        grandchild { "nested" }
    }
}

#[component]
fn inherited_context() -> Component {
    view! {
        #[user(repeat)]
        child { "scoped" }
        inherited_grandchild()
    }
}

#[component]
fn overridden_ephemeral_child() -> Component {
    view! {
        #[user(ephemeral, repeat)]
        override_value { "ordinary outer placement" }
    }
}

#[component]
fn placement_application() -> Component {
    view! {
        #[user(ephemeral, repeat)]
        transient_prefix { "first" }

        #[developer]
        durable_policy { "ordinary" }

        #[developer(repeat, ephemeral)]
        inherited_context()

        #[user]
        durable_request { "continue" }

        #[user(repeat)]
        persistent_repeat { "retained" }

        #[developer(ephemeral, repeat)]
        transient_suffix { "last parent item" }

        #[developer]
        overridden_ephemeral_child()
    }
}

#[tokio::test]
async fn ephemeral_subtrees_are_stably_collected_after_ordinary_inputs_on_full_and_delta() {
    let (port, capture, control) = CapturingPort::new();
    let mut application =
        Application::mount(placement_application, port).expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    control.force_next_full();
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].basis, FrameBasis::Full);
    assert!(matches!(frames[1].basis, FrameBasis::DeltaFrom(_)));
    assert_eq!(frames[2].basis, FrameBasis::Full);
    assert_eq!(
        documents(&frames[0].projection),
        [
            text_element("durable_policy", "ordinary"),
            text_element("durable_request", "continue"),
            text_element("persistent_repeat", "retained"),
            text_element("override_value", "ordinary outer placement"),
        ]
    );
    for frame in &frames {
        assert_eq!(
            documents(&frame.ephemeral),
            [
                text_element("transient_prefix", "first"),
                text_element("transient_suffix", "last parent item"),
                text_element("child", "scoped"),
                text_element("grandchild", "nested"),
            ],
            "ephemeral order follows existing parent-grouped projection traversal"
        );
        assert!(matches!(
            frame.ephemeral.as_slice(),
            [
                CanonicalInputItem::Message {
                    role: ConversationRole::User,
                    ..
                },
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::Developer,
                    ..
                },
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::Developer,
                    ..
                },
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::Developer,
                    ..
                },
            ]
        ));
        assert_eq!(
            frame.canonical_payload["component"]["ephemeral"]
                .as_array()
                .expect("nonempty ephemeral section is metered")
                .len(),
            4
        );
    }
    for frame in &frames[1..] {
        assert_eq!(
            documents(&frame.projection),
            [text_element("persistent_repeat", "retained")],
            "outer ordinary placement suppresses the inner ephemeral/repeat settings"
        );
    }
    let retained_repeat = &frames[0].projection[2];
    assert_eq!(
        frames[2]
            .replay
            .iter()
            .filter(|item| *item == retained_repeat)
            .count(),
        2,
        "repeat without ephemeral still retains every submitted occurrence"
    );
    assert_ephemeral_not_retained(&frames);

    application.shutdown().await.expect("shutdown application");
}

#[derive(Clone, AgentView)]
#[agent_view(kind = "temporary_history")]
struct TemporaryHistory {
    #[view(element)]
    objective: &'static str,
    #[view(diff(append))]
    entries: Vec<String>,
}

#[component]
fn ephemeral_diff_application(props: StateProps<Vec<String>>) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let snapshot = TemporaryHistory {
        objective: "Read the complete current scratchpad.",
        entries: state.with(Clone::clone).expect("mounted signal read"),
    };

    view! {
        #[developer(ephemeral, repeat)]
        #[diff(slot = "temporary_history")]
        { snapshot }

        #[user]
        request { "continue" }
    }
}

#[tokio::test]
async fn unchanged_and_changed_ephemeral_diff_content_always_sends_the_complete_current_value() {
    let props = StateProps::new(vec!["first".to_owned()]);
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application =
        Application::mount(move || ephemeral_diff_application(root_props.clone()), port)
            .expect("mount application");

    react(&mut application).await;
    react(&mut application).await;
    props
        .signal()
        .update(|entries| entries.push("second".to_owned()))
        .expect("append ephemeral entry");
    react(&mut application).await;
    control.force_next_full();
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].ephemeral, frames[1].ephemeral);
    assert_eq!(frames[2].ephemeral, frames[3].ephemeral);
    for (index, frame) in frames.iter().enumerate() {
        let values = documents(&frame.ephemeral);
        assert_eq!(values.len(), 1);
        let value = &values[0];
        assert!(value.has_text("first"));
        assert!(!value.has_attribute("rendering_mode", "delta"));
        assert!(!value.has_element("insert"));
        assert_eq!(value.has_text("second"), index >= 2);
        if index > 0 {
            assert!(frame.projection.is_empty());
        }
    }
    assert_ephemeral_not_retained(&frames);

    application.shutdown().await.expect("shutdown application");
}

#[component]
fn optional_ephemeral_application(props: StateProps<Option<String>>) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    match value {
        Some(value) => view! {
            #[user(ephemeral, repeat)]
            temporary_state { "{value}" }
            #[developer]
            context { "fixed" }
        },
        None => view! {
            #[developer]
            context { "fixed" }
        },
    }
}

#[tokio::test]
async fn ephemeral_xml_changes_and_disappearance_emit_no_removal_or_historical_replay() {
    let props = StateProps::new(Some("A".to_owned()));
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application = Application::mount(
        move || optional_ephemeral_application(root_props.clone()),
        port,
    )
    .expect("mount application");

    react(&mut application).await;
    props.signal().set(Some("B".to_owned())).expect("set B");
    react(&mut application).await;
    props.signal().set(None).expect("remove ephemeral context");
    control.force_next_full();
    react(&mut application).await;
    props.signal().set(Some("A".to_owned())).expect("restore A");
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_eq!(
        documents(&frames[0].ephemeral),
        [text_element("temporary_state", "A")]
    );
    assert_eq!(
        documents(&frames[1].ephemeral),
        [text_element("temporary_state", "B")]
    );
    assert!(frames[2].ephemeral.is_empty());
    assert_eq!(
        documents(&frames[3].ephemeral),
        [text_element("temporary_state", "A")]
    );
    assert_eq!(frames[2].basis, FrameBasis::Full);
    assert_eq!(
        documents(&frames[2].replay),
        [text_element("context", "fixed")]
    );
    for frame in &frames[1..] {
        assert!(
            frame.projection.is_empty(),
            "ephemeral changes/removal do not disturb the retained ordinary snapshot"
        );
    }
    assert!(
        frames[2].canonical_payload["component"]
            .get("ephemeral")
            .is_none(),
        "the meter omits an empty ephemeral section"
    );
    assert_ephemeral_not_retained(&frames);

    application.shutdown().await.expect("shutdown application");
}

#[component]
fn cancellable_ephemeral_application(props: StateProps<String>) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted signal read");

    view! {
        #[user(ephemeral, repeat)]
        transient { "{value}" }

        #[developer]
        ordinary { "{value}" }
    }
}

#[tokio::test]
async fn preparation_and_pre_handoff_cancellation_do_not_consume_ephemeral_or_ordinary_input() {
    let props = StateProps::new("A".to_owned());
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application = Application::mount(
        move || cancellable_ephemeral_application(root_props.clone()),
        port,
    )
    .expect("mount application");

    assert_eq!(
        application.prepare().await.expect("prepare succeeds"),
        ControlFlow::Continue(())
    );
    assert!(capture.frames().is_empty());
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
    .expect("submission reaches the blocking port");
    drop(cancelled);

    props.signal().set("A".to_owned()).expect("restore A");
    react(&mut application).await;
    control.force_next_full();
    react(&mut application).await;

    let frames = capture.frames();
    assert_eq!(frames.len(), 4);
    assert_eq!(
        documents(&frames[0].ephemeral),
        [text_element("transient", "A")]
    );
    assert_eq!(
        documents(&frames[1].ephemeral),
        [text_element("transient", "B")]
    );
    for frame in &frames[2..] {
        assert_eq!(
            documents(&frame.ephemeral),
            [text_element("transient", "A")]
        );
        assert!(
            frame.projection.is_empty(),
            "a cancelled B frame never advances the ordinary A snapshot"
        );
    }
    assert_eq!(
        documents(&frames[3].replay),
        [text_element("ordinary", "A")]
    );
    assert_ephemeral_not_retained(&frames);

    application.shutdown().await.expect("shutdown application");
}

#[derive(Clone)]
struct BalanceState {
    balance: u32,
    feedback: &'static str,
    next_action: &'static str,
}

const ADJUSTMENT_FEEDBACK: &str =
    "Adjustment saved: <balance>80</balance> & `balance_total` [confirmed].\r\nKeep &gt; literal.";

#[component]
fn balance_context(props: StateProps<BalanceState>) -> Component {
    let initial = props.initial.clone();
    let state = use_signal(move || initial);
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let snapshot = state.with(Clone::clone).expect("mounted signal read");
    let balance = snapshot.balance;
    let feedback = snapshot.feedback;
    let next_action = snapshot.next_action;
    let adjust_state = state.clone();
    let confirm_state = state.clone();

    view! {
        #[user]
        balance_view {
            balance { "{balance}" }
            feedback { "{feedback}" }
            next_action { "{next_action}" }
        }
        {
            NativeToolCall::named("adjust_balance").on_call(move |call| {
                let state = adjust_state.clone();
                async move {
                    state.set(BalanceState {
                        balance: 80,
                        feedback: ADJUSTMENT_FEEDBACK,
                        next_action: "confirm_balance",
                    }).expect("save actual adjustment result");
                    Ok::<_, Infallible>(call.output("Balance updated to 80."))
                }
            })
        }
        {
            NativeToolCall::named("confirm_balance").on_call(move |call| {
                let state = confirm_state.clone();
                async move {
                    state.update(|state| {
                        assert_eq!(state.balance, 80);
                        assert_eq!(state.feedback, ADJUSTMENT_FEEDBACK);
                        assert_eq!(state.next_action, "confirm_balance");
                        state.feedback = "Balance confirmed.";
                        state.next_action = "finish";
                    }).expect("record actual confirmation result");
                    Ok::<_, Infallible>(call.output("Confirmed balance 80."))
                }
            })
        }
    }
}

#[component]
fn interactive_ephemeral_application(props: StateProps<BalanceState>) -> Component {
    view! {
        #[developer(ephemeral, repeat)]
        balance_context(props)

        #[user]
        request { "Adjust the balance, inspect the result, and confirm it." }
    }
}

fn tool_fact(output: u64, call_id: &str, name: &str) -> ProviderFact {
    ProviderFact::ToolCall {
        output: ProviderOutputKey::new(output),
        ordinal: 1,
        call: ProviderToolCall::new(call_id, name, "{}").expect("valid tool call"),
    }
}

#[tokio::test]
async fn ephemeral_component_delivers_tool_feedback_and_next_action_while_retaining_native_history()
{
    let props = StateProps::new(BalanceState {
        balance: 100,
        feedback: "No adjustment has run.",
        next_action: "adjust_balance",
    });
    let root_props = props.clone();
    let (port, capture, control) = CapturingPort::new();
    let mut application = Application::mount(
        move || interactive_ephemeral_application(root_props.clone()),
        port,
    )
    .expect("mount application");
    let reply_key = ProviderOutputKey::new(1);
    let reply = "The balance is 100; I will adjust it.";
    control.enqueue_facts(vec![
        ProviderFact::TextDelta {
            output: reply_key,
            phase: None,
            delta: reply.to_owned(),
        },
        ProviderFact::TextSealed {
            output: reply_key,
            phase: None,
            text: reply.to_owned(),
        },
        tool_fact(2, "adjust-1", "adjust_balance"),
        ProviderFact::ReactionCompleted {
            primary_text: Some(reply_key),
        },
    ]);

    react(&mut application).await;
    let initial = capture.frames().remove(0);
    let initial_view = combine(documents(&initial.ephemeral));
    assert!(initial_view.has_field("balance", "100"));
    assert!(initial_view.has_text("No adjustment has run."));
    assert!(
        initial_view.has_field("next_action", "adjust_balance"),
        "initial ephemeral view: {initial_view:?}"
    );
    assert_eq!(
        initial
            .tools
            .iter()
            .map(ToolDefinition::name)
            .collect::<Vec<_>>(),
        ["adjust_balance", "confirm_balance"],
        "ephemeral subtree tools are still discoverable"
    );
    assert_eq!(props.signal().with(|state| state.balance).unwrap(), 80);

    control.enqueue_facts(vec![
        tool_fact(3, "confirm-1", "confirm_balance"),
        ProviderFact::ReactionCompleted { primary_text: None },
    ]);
    react(&mut application).await;
    let updated = capture.frames().remove(1);
    let updated_view = combine(documents(&updated.ephemeral));
    assert!(updated_view.has_field("feedback", ADJUSTMENT_FEEDBACK));
    assert!(updated_view.has_field("balance", "80"));

    assert!(updated_view.has_field("next_action", "confirm_balance"));
    assert!(!updated_view.has_field("balance", "100"));
    let adjust_call =
        CanonicalInputItem::tool_call("adjust-1", "adjust_balance", "{}").expect("valid tool call");
    let adjust_result = CanonicalInputItem::tool_result("adjust-1", "Balance updated to 80.")
        .expect("valid tool result");
    assert!(updated.replay.contains(&adjust_call));
    assert!(updated.staged_inputs.contains(&adjust_result));
    assert!(updated
        .replay
        .contains(&CanonicalInputItem::assistant_text(reply, None)));
    let ordered = updated.ordered_inputs();
    assert_eq!(ordered.last().copied(), updated.ephemeral.last());
    assert!(
        ordered
            .iter()
            .position(|item| **item == adjust_result)
            .unwrap()
            < ordered
                .iter()
                .position(|item| **item == updated.ephemeral[0])
                .unwrap()
    );

    control.force_next_full();
    react(&mut application).await;
    let frames = capture.frames();
    let confirmed = &frames[2];
    assert_eq!(confirmed.basis, FrameBasis::Full);
    let confirmed_view = combine(documents(&confirmed.ephemeral));
    assert!(confirmed_view.has_text("Balance confirmed."));
    assert!(confirmed_view.has_field("next_action", "finish"));
    assert!(confirmed.replay.contains(&adjust_call));
    assert!(confirmed.replay.contains(&adjust_result));
    assert!(confirmed.replay.contains(
        &CanonicalInputItem::tool_call("confirm-1", "confirm_balance", "{}")
            .expect("valid tool call")
    ));
    assert!(confirmed.staged_inputs.contains(
        &CanonicalInputItem::tool_result("confirm-1", "Confirmed balance 80.")
            .expect("valid tool result")
    ));
    for frame in &frames {
        assert!(frame.ephemeral.iter().all(|item| matches!(
            item,
            CanonicalInputItem::Instruction {
                authority: InstructionAuthority::Developer,
                ..
            }
        )));
        assert!(frame.projection.iter().all(|item| !matches!(
            item,
            CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
        )));
    }
    assert_ephemeral_not_retained(&frames);

    application.shutdown().await.expect("shutdown application");
}
