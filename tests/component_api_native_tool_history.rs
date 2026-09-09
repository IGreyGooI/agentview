use std::{
    collections::VecDeque,
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile,
            ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall, ReactionPort,
            ReactionPortFault, RenderedProjection, ResettableReactionPort, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
    },
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::{oneshot, Notify};

const CANCELLED_TOOL_OUTPUT: &str = "Tool execution was cancelled; its outcome is unknown.";

enum Script {
    Finite(Vec<ProviderFact>),
    PendingAfter(Vec<ProviderFact>),
}

impl Script {
    fn completed() -> Self {
        Self::Finite(vec![ProviderFact::ReactionCompleted { primary_text: None }])
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CapturedFrame {
    basis: FrameBasis,
    replay: Vec<CanonicalInputItem>,
    staged_inputs: Vec<CanonicalInputItem>,
    projection: Vec<CanonicalInputItem>,
    tools: Vec<String>,
}

#[derive(Clone, Default)]
struct FrameCapture {
    frames: Arc<Mutex<Vec<CapturedFrame>>>,
}

impl FrameCapture {
    fn frames(&self) -> Vec<CapturedFrame> {
        self.frames.lock().expect("frame capture lock").clone()
    }
}

struct ScriptedPort {
    declaration: TargetDeclaration,
    scripts: VecDeque<Script>,
    capture: FrameCapture,
    force_next_full: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl ScriptedPort {
    fn new(scripts: impl IntoIterator<Item = Script>) -> (Self, FrameCapture) {
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
        let identity = TargetIdentity::new(NonZeroU128::new(7).expect("non-zero identity"));
        let epoch = TargetEpoch::new(NonZeroU64::new(1).expect("non-zero epoch"));
        (
            Self {
                declaration: TargetDeclaration::full(identity, epoch, profile),
                scripts: scripts.into_iter().collect(),
                capture: capture.clone(),
                force_next_full: None,
            },
            capture,
        )
    }

    fn new_with_forced_full(
        scripts: impl IntoIterator<Item = Script>,
    ) -> (Self, FrameCapture, Arc<std::sync::atomic::AtomicBool>) {
        let (mut port, capture) = Self::new(scripts);
        let trigger = Arc::new(std::sync::atomic::AtomicBool::new(false));
        port.force_next_full = Some(Arc::clone(&trigger));
        (port, capture, trigger)
    }

    fn force_full_if_requested(&mut self) {
        let Some(trigger) = &self.force_next_full else {
            return;
        };
        if !trigger.swap(false, Ordering::SeqCst) {
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
impl ReactionPort for ScriptedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.force_full_if_requested();
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.capture
            .frames
            .lock()
            .expect("frame capture lock")
            .push(CapturedFrame {
                basis: frame.basis(),
                replay: frame.submission().replay().to_vec(),
                staged_inputs: frame.submission().staged_inputs().to_vec(),
                projection: frame.submission().projection().items().to_vec(),
                tools: frame.submission().tools().names().to_vec(),
            });

        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let script = self
            .scripts
            .pop_front()
            .expect("test supplied one script per submitted frame");
        let facts: ProviderFactStream<'a> = match script {
            Script::Finite(facts) => Box::pin(futures::stream::iter(facts.into_iter().map(Ok))),
            Script::PendingAfter(facts) => {
                Box::pin(futures::stream::iter(facts.into_iter().map(Ok)).chain(
                    futures::stream::pending::<Result<ProviderFact, ReactionPortFault>>(),
                ))
            }
        };
        Ok(facts)
    }
}

impl ResettableReactionPort for ScriptedPort {
    fn reset_model_context(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
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
        Ok(self.declaration.clone())
    }
}

fn tool_fact(output: u64, ordinal: u64, call_id: &str, name: &str) -> ProviderFact {
    tool_fact_with_arguments(output, ordinal, call_id, name, "{}")
}

fn tool_fact_with_arguments(
    output: u64,
    ordinal: u64,
    call_id: &str,
    name: &str,
    raw_arguments: &str,
) -> ProviderFact {
    ProviderFact::ToolCall {
        output: ProviderOutputKey::new(output),
        ordinal,
        call: ProviderToolCall::new(call_id, name, raw_arguments).expect("valid test tool call"),
    }
}

fn tool_call(call_id: &str, name: &str) -> CanonicalInputItem {
    tool_call_with_arguments(call_id, name, "{}")
}

fn tool_call_with_arguments(call_id: &str, name: &str, raw_arguments: &str) -> CanonicalInputItem {
    CanonicalInputItem::tool_call(call_id, name, raw_arguments).expect("valid test tool call")
}

fn tool_result(call_id: &str, content: &str) -> CanonicalInputItem {
    CanonicalInputItem::tool_result(call_id, content).expect("valid test tool result")
}

fn occurrences(items: &[CanonicalInputItem], expected: &CanonicalInputItem) -> usize {
    items.iter().filter(|item| *item == expected).count()
}

fn submission_occurrences(frame: &CapturedFrame, expected: &CanonicalInputItem) -> usize {
    occurrences(&frame.replay, expected)
        + occurrences(&frame.staged_inputs, expected)
        + occurrences(&frame.projection, expected)
}

fn native_items(items: &[CanonicalInputItem]) -> Vec<CanonicalInputItem> {
    items
        .iter()
        .filter(|item| {
            matches!(
                item,
                CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
            )
        })
        .cloned()
        .collect()
}

fn native_log(projection: &RenderedProjection, tool_name: &str) -> Vec<CanonicalInputItem> {
    let identity = format!("agentview::NativeToolCall({tool_name})");
    let nodes = projection
        .nodes()
        .iter()
        .filter(|node| node.identity().contains(&identity))
        .collect::<Vec<_>>();
    assert_eq!(
        nodes.len(),
        1,
        "each mounted native tool owns one independent projection node"
    );
    native_items(nodes[0].items())
}

fn assert_native_group(items: &[CanonicalInputItem], call_id: &str, name: &str, content: &str) {
    let call = tool_call(call_id, name);
    let result = tool_result(call_id, content);
    assert!(items.contains(&call), "native log retains call {call_id}");
    assert!(
        items.contains(&result),
        "native log retains result {call_id}"
    );
}

fn assert_call_is_replayed_and_result_is_staged(
    frame: &CapturedFrame,
    call_id: &str,
    name: &str,
    content: &str,
) {
    let call = tool_call(call_id, name);
    let result = tool_result(call_id, content);
    assert_eq!(occurrences(&frame.replay, &call), 1);
    assert_eq!(occurrences(&frame.staged_inputs, &result), 1);
    assert_eq!(occurrences(&frame.projection, &call), 0);
    assert_eq!(occurrences(&frame.projection, &result), 0);
    assert_eq!(submission_occurrences(frame, &call), 1);
    assert_eq!(submission_occurrences(frame, &result), 1);
}

#[derive(Clone)]
struct RepeatedToolProps {
    calls: Arc<AtomicUsize>,
    rerender: Arc<Mutex<Option<Signal<u8>>>>,
}

#[component]
fn repeated_tool_application(props: RepeatedToolProps) -> Component {
    let redraw = use_signal(|| 0_u8);
    *props.rerender.lock().expect("rerender signal lock") = Some(redraw.clone());
    let redraw_value = redraw.with(|value| *value).expect("mounted signal read");
    let calls = Arc::clone(&props.calls);

    view! {
        redraw_state { "{redraw_value}" }
        {
            NativeToolCall::named("add").on_call(move |call| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    let content = format!("result:{}", call.call_id());
                    Ok::<_, Infallible>(call.output(content))
                }
            })
        }
    }
}

#[tokio::test]
async fn native_tool_component_keeps_each_same_argument_call_once_across_rerenders() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rerender = Arc::new(Mutex::new(None));
    let props = RepeatedToolProps {
        calls: Arc::clone(&calls),
        rerender: Arc::clone(&rerender),
    };
    let (port, capture) = ScriptedPort::new([
        Script::Finite(vec![
            tool_fact(1, 1, "call-add-one", "add"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::Finite(vec![
            tool_fact(2, 1, "call-add-two", "add"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
    ]);
    let root_props = props.clone();
    let mut application =
        Application::mount(move || repeated_tool_application(root_props.clone()), port)
            .expect("mount native tool application");

    assert!(matches!(
        application.react().await.expect("first tool reaction"),
        ControlFlow::Continue(())
    ));
    assert!(matches!(
        application.react().await.expect("second tool reaction"),
        ControlFlow::Continue(())
    ));

    rerender
        .lock()
        .expect("rerender signal lock")
        .clone()
        .expect("rerender signal is exposed")
        .set(1)
        .expect("mounted signal accepts a write");
    assert!(matches!(
        application
            .react()
            .await
            .expect("explicit rerender reaction"),
        ControlFlow::Continue(())
    ));

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let expected_log = vec![
        tool_call("call-add-one", "add"),
        tool_result("call-add-one", "result:call-add-one"),
        tool_call("call-add-two", "add"),
        tool_result("call-add-two", "result:call-add-two"),
    ];
    assert_eq!(
        native_log(application.current_projection().projection(), "add"),
        expected_log,
        "same arguments with distinct call IDs execute independently and append once"
    );

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_call_is_replayed_and_result_is_staged(
        &frames[1],
        "call-add-one",
        "add",
        "result:call-add-one",
    );
    assert_call_is_replayed_and_result_is_staged(
        &frames[2],
        "call-add-two",
        "add",
        "result:call-add-two",
    );
    assert!(native_items(&frames[2].projection).is_empty());
}

#[derive(Clone)]
struct RoundHistoryProps {
    rerender: Arc<Mutex<Option<Signal<u8>>>>,
}

#[component]
fn round_history_application(props: RoundHistoryProps) -> Component {
    let redraw = use_signal(|| 0_u8);
    *props.rerender.lock().expect("round history signal lock") = Some(redraw.clone());
    let redraw_value = redraw.with(|value| *value).expect("mounted signal read");

    view! {
        round_history_state { "{redraw_value}" }
        {
            NativeToolCall::named("rounds").on_call(|call| async move {
                let content = format!("result:{}", call.call_id());
                Ok::<_, Infallible>(call.output(content))
            })
        }
    }
}

#[tokio::test]
async fn native_tool_component_retains_only_the_latest_two_provider_response_rounds() {
    let rerender = Arc::new(Mutex::new(None));
    let props = RoundHistoryProps {
        rerender: Arc::clone(&rerender),
    };
    let (port, capture) = ScriptedPort::new([
        Script::Finite(vec![
            tool_fact(1, 1, "round-one-a", "rounds"),
            tool_fact(2, 2, "round-one-b", "rounds"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
        Script::Finite(vec![
            tool_fact(3, 1, "round-two", "rounds"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::Finite(vec![
            tool_fact(4, 1, "round-three", "rounds"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
    ]);
    let root_props = props.clone();
    let mut application =
        Application::mount(move || round_history_application(root_props.clone()), port)
            .expect("mount round history application");

    assert!(matches!(
        application.react().await.expect("first provider response"),
        ControlFlow::Continue(())
    ));
    let after_first_round = native_log(application.current_projection().projection(), "rounds");
    assert_eq!(after_first_round.len(), 4);
    assert_native_group(
        &after_first_round,
        "round-one-a",
        "rounds",
        "result:round-one-a",
    );
    assert_native_group(
        &after_first_round,
        "round-one-b",
        "rounds",
        "result:round-one-b",
    );

    assert!(matches!(
        application
            .prepare()
            .await
            .expect("prepare does not submit"),
        ControlFlow::Continue(())
    ));
    assert_eq!(capture.frames().len(), 1, "prepare does not submit a frame");
    assert_eq!(
        native_log(application.current_projection().projection(), "rounds"),
        after_first_round,
        "a preparation-only render does not create a native tool round"
    );

    rerender
        .lock()
        .expect("round history signal lock")
        .clone()
        .expect("round history signal is exposed")
        .set(1)
        .expect("mounted signal accepts a write");
    assert!(matches!(
        application
            .react()
            .await
            .expect("tool-free provider response"),
        ControlFlow::Continue(())
    ));
    let after_tool_free_response =
        native_log(application.current_projection().projection(), "rounds");
    assert_eq!(after_tool_free_response, after_first_round);

    assert!(matches!(
        application.react().await.expect("second provider response"),
        ControlFlow::Continue(())
    ));
    let after_second_round = native_log(application.current_projection().projection(), "rounds");
    assert_eq!(after_second_round.len(), 6);
    assert_native_group(
        &after_second_round,
        "round-one-a",
        "rounds",
        "result:round-one-a",
    );
    assert_native_group(
        &after_second_round,
        "round-one-b",
        "rounds",
        "result:round-one-b",
    );
    assert_native_group(
        &after_second_round,
        "round-two",
        "rounds",
        "result:round-two",
    );

    assert!(matches!(
        application.react().await.expect("third provider response"),
        ControlFlow::Continue(())
    ));
    let after_third_round = native_log(application.current_projection().projection(), "rounds");
    assert_eq!(after_third_round.len(), 4);
    assert_native_group(
        &after_third_round,
        "round-two",
        "rounds",
        "result:round-two",
    );
    assert_native_group(
        &after_third_round,
        "round-three",
        "rounds",
        "result:round-three",
    );
    for call_id in ["round-one-a", "round-one-b"] {
        assert!(
            !after_third_round.contains(&tool_call(call_id, "rounds"))
                && !after_third_round.contains(&tool_result(call_id, &format!("result:{call_id}"))),
            "the first response is evicted as one complete multi-call round"
        );
    }

    assert!(matches!(
        application.react().await.expect("post-prune continuation"),
        ControlFlow::Continue(())
    ));
    let frames = capture.frames();
    assert_eq!(frames.len(), 5);
    assert!(matches!(frames[4].basis, FrameBasis::DeltaFrom(_)));
    assert!(
        frames[4].projection.is_empty(),
        "pruning Component-owned native history does not emit projection removal records"
    );
    assert_eq!(
        native_items(&frames[4].staged_inputs),
        vec![tool_result("round-three", "result:round-three")],
        "the final continuation hands off only the latest completed tool result"
    );
}

#[tokio::test]
async fn reset_model_context_replays_the_retained_native_tool_component_rounds_in_a_full_frame() {
    let rerender = Arc::new(Mutex::new(None));
    let props = RoundHistoryProps {
        rerender: Arc::clone(&rerender),
    };
    let (port, capture) = ScriptedPort::new([
        Script::Finite(vec![
            tool_fact(1, 1, "reset-round-one", "rounds"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::Finite(vec![
            tool_fact(2, 1, "reset-round-two", "rounds"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::Finite(vec![
            tool_fact(3, 1, "reset-round-three", "rounds"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
        Script::completed(),
    ]);
    let root_props = props.clone();
    let mut application =
        Application::mount(move || round_history_application(root_props.clone()), port)
            .expect("mount reset history application");

    for description in [
        "first completed tool round",
        "second completed tool round",
        "third completed tool round",
        "handoff third completed tool result",
    ] {
        assert!(matches!(
            application.react().await.expect(description),
            ControlFlow::Continue(())
        ));
    }

    application
        .reset_model_context()
        .expect("completed native tool rounds allow a context reset");
    assert!(matches!(
        application.react().await.expect("full reset handoff"),
        ControlFlow::Continue(())
    ));

    let frames = capture.frames();
    assert_eq!(frames.len(), 5);
    assert_eq!(frames[4].basis, FrameBasis::Full);
    assert!(
        frames[4].replay.is_empty(),
        "the reset discards canonical session history"
    );
    let reset_projection = native_items(&frames[4].projection);
    assert_eq!(reset_projection.len(), 4);
    assert_native_group(
        &reset_projection,
        "reset-round-two",
        "rounds",
        "result:reset-round-two",
    );
    assert_native_group(
        &reset_projection,
        "reset-round-three",
        "rounds",
        "result:reset-round-three",
    );
    assert!(
        !reset_projection.contains(&tool_call("reset-round-one", "rounds"))
            && !reset_projection
                .contains(&tool_result("reset-round-one", "result:reset-round-one")),
        "the reset Full is sourced from the two retained Component rounds, not all prior history"
    );
}

#[derive(Clone)]
struct ParallelToolProps {
    first_calls: Arc<AtomicUsize>,
    second_calls: Arc<AtomicUsize>,
    first_started: Arc<Notify>,
    second_started: Arc<Notify>,
    release_first: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    release_second: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    second_finished: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

#[component]
fn parallel_tool_application(props: ParallelToolProps) -> Component {
    let second_calls = Arc::clone(&props.second_calls);
    let second_started = Arc::clone(&props.second_started);
    let release_second = Arc::clone(&props.release_second);
    let second_finished = Arc::clone(&props.second_finished);
    let first_calls = Arc::clone(&props.first_calls);
    let first_started = Arc::clone(&props.first_started);
    let release_first = Arc::clone(&props.release_first);

    view! {
        parallel_context { "component order is intentionally second then first" }
        {
            NativeToolCall::named("second").on_call(move |call| {
                second_calls.fetch_add(1, Ordering::SeqCst);
                let started = Arc::clone(&second_started);
                let release = release_second
                    .lock()
                    .expect("second release lock")
                    .take()
                    .expect("second tool has one release receiver");
                let finished = second_finished
                    .lock()
                    .expect("second finish lock")
                    .take()
                    .expect("second tool has one finish sender");
                async move {
                    started.notify_one();
                    release.await.expect("second lane release sender remains open");
                    finished.send(()).expect("second finish receiver remains open");
                    Ok::<_, Infallible>(call.output("result-second"))
                }
            })
        }
        {
            NativeToolCall::named("first").on_call(move |call| {
                first_calls.fetch_add(1, Ordering::SeqCst);
                let started = Arc::clone(&first_started);
                let release = release_first
                    .lock()
                    .expect("first release lock")
                    .take()
                    .expect("first tool has one release receiver");
                async move {
                    started.notify_one();
                    release.await.expect("first lane release sender remains open");
                    Ok::<_, Infallible>(call.output("result-first"))
                }
            })
        }
    }
}

#[tokio::test]
async fn parallel_native_tool_results_follow_provider_ordinals_not_tree_or_completion_order() {
    let (release_first, release_first_rx) = oneshot::channel();
    let (release_second, release_second_rx) = oneshot::channel();
    let (second_finished, mut second_finished_rx) = oneshot::channel();
    let props = ParallelToolProps {
        first_calls: Arc::new(AtomicUsize::new(0)),
        second_calls: Arc::new(AtomicUsize::new(0)),
        first_started: Arc::new(Notify::new()),
        second_started: Arc::new(Notify::new()),
        release_first: Arc::new(Mutex::new(Some(release_first_rx))),
        release_second: Arc::new(Mutex::new(Some(release_second_rx))),
        second_finished: Arc::new(Mutex::new(Some(second_finished))),
    };
    let (port, capture) = ScriptedPort::new([
        Script::Finite(vec![
            tool_fact(10, 1, "call-first", "first"),
            tool_fact(20, 2, "call-second", "second"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
    ]);
    let root_props = props.clone();
    let mut application =
        Application::mount(move || parallel_tool_application(root_props.clone()), port)
            .expect("mount parallel native tools");

    let first_started = props.first_started.notified();
    let second_started = props.second_started.notified();
    tokio::pin!(first_started);
    tokio::pin!(second_started);
    let mut reaction = Box::pin(application.react());
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            result = &mut reaction => panic!("reaction completed before both tool lanes started: {result:?}"),
            _ = &mut first_started => {}
        }
    })
    .await
    .expect("first tool lane starts");
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            result = &mut reaction => panic!("reaction completed before both tool lanes started: {result:?}"),
            _ = &mut second_started => {}
        }
    })
    .await
    .expect("second tool lane starts");

    release_second
        .send(())
        .expect("second lane receiver remains open");
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            result = &mut reaction => panic!("reaction completed before first lane was released: {result:?}"),
            result = &mut second_finished_rx => result.expect("second finish sender remains open"),
        }
    })
    .await
    .expect("second tool finishes before first is released");
    release_first
        .send(())
        .expect("first lane receiver remains open");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut reaction)
            .await
            .expect("parallel reaction settles")
            .expect("parallel reaction succeeds"),
        ControlFlow::Continue(())
    ));
    drop(reaction);

    assert_eq!(props.first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(props.second_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        application.react().await.expect("continuation reaction"),
        ControlFlow::Continue(())
    ));

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        native_items(&frames[1].replay),
        vec![
            tool_call("call-first", "first"),
            tool_call("call-second", "second")
        ],
        "ToolCall replay follows provider ordinal although declaration tree is reversed"
    );
    assert_eq!(
        native_items(&frames[1].staged_inputs),
        vec![
            tool_result("call-first", "result-first"),
            tool_result("call-second", "result-second"),
        ],
        "ToolResult staging follows provider ordinal although second completed first"
    );
    assert!(native_items(&frames[1].projection).is_empty());
}

#[derive(Clone)]
struct ConditionalToolProps {
    calls: Arc<AtomicUsize>,
    mounted: Arc<Mutex<Option<Signal<bool>>>>,
}

#[component]
fn conditional_tool_application(props: ConditionalToolProps) -> Component {
    let mounted = use_signal(|| true);
    *props.mounted.lock().expect("mounted signal lock") = Some(mounted.clone());
    let is_mounted = mounted.with(|value| *value).expect("mounted signal read");
    let calls = Arc::clone(&props.calls);

    if is_mounted {
        view! {
            conditional_context { "tool mounted" }
            {
                NativeToolCall::named("vanishing").on_call(move |call| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move { Ok::<_, Infallible>(call.output("vanishing-result")) }
                })
            }
        }
    } else {
        view! {
            conditional_context { "tool unmounted" }
        }
    }
}

#[tokio::test]
async fn unmounting_a_native_tool_keeps_accepted_history_in_later_replay() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mounted = Arc::new(Mutex::new(None));
    let props = ConditionalToolProps {
        calls: Arc::clone(&calls),
        mounted: Arc::clone(&mounted),
    };
    let (port, capture, force_full) = ScriptedPort::new_with_forced_full([
        Script::Finite(vec![
            tool_fact(1, 1, "call-vanishing", "vanishing"),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
        Script::completed(),
    ]);
    let root_props = props.clone();
    let mut application = Application::mount(
        move || conditional_tool_application(root_props.clone()),
        port,
    )
    .expect("mount conditional native tool");

    let _ = application.react().await.expect("tool reaction");
    let _ = application.react().await.expect("tool output continuation");
    let normal_delta = capture.frames();
    assert_call_is_replayed_and_result_is_staged(
        &normal_delta[1],
        "call-vanishing",
        "vanishing",
        "vanishing-result",
    );
    mounted
        .lock()
        .expect("mounted signal lock")
        .clone()
        .expect("mounted signal is exposed")
        .set(false)
        .expect("mounted signal accepts a write");
    force_full.store(true, Ordering::SeqCst);
    let _ = application.react().await.expect("unmounted continuation");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(application
        .current_projection()
        .projection()
        .native_tools()
        .iter()
        .all(|tool| tool.name() != "vanishing"));

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[2].tools, Vec::<String>::new());
    assert_eq!(
        native_items(&frames[2].replay),
        vec![
            tool_call("call-vanishing", "vanishing"),
            tool_result("call-vanishing", "vanishing-result"),
        ],
        "accepted call/result history survives after its declaring component is removed"
    );
    assert!(native_items(&frames[2].staged_inputs).is_empty());
    assert!(native_items(&frames[2].projection).is_empty());
}

#[derive(Clone)]
struct CancelledToolProps {
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
}

#[component]
fn cancelled_tool_application(props: CancelledToolProps) -> Component {
    let calls = Arc::clone(&props.calls);
    let started = Arc::clone(&props.started);
    view! {
        cancelled_context { "pending tool" }
        {
            NativeToolCall::named("pending").on_call(move |call| {
                calls.fetch_add(1, Ordering::SeqCst);
                let started = Arc::clone(&started);
                async move {
                    started.notify_one();
                    std::future::pending::<()>().await;
                    Ok::<_, Infallible>(call.output("unreachable"))
                }
            })
        }
    }
}

#[tokio::test]
async fn cancelled_native_tool_appends_fallback_to_its_component_log_before_reconcile() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let props = CancelledToolProps {
        calls: Arc::clone(&calls),
        started: Arc::clone(&started),
    };
    let (port, capture) = ScriptedPort::new([
        Script::PendingAfter(vec![tool_fact(1, 1, "call-pending", "pending")]),
        Script::completed(),
    ]);
    let root_props = props.clone();
    let mut application =
        Application::mount(move || cancelled_tool_application(root_props.clone()), port)
            .expect("mount cancelled native tool");

    let started_wait = started.notified();
    tokio::pin!(started_wait);
    let mut cancelled = Box::pin(application.react());
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            result = &mut cancelled => panic!("reaction completed before pending tool started: {result:?}"),
            _ = &mut started_wait => {}
        }
    })
    .await
    .expect("pending native tool starts");
    drop(cancelled);

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(application.current_projection().is_dirty());
    assert!(matches!(
        application.react().await.expect("fallback continuation"),
        ControlFlow::Continue(())
    ));

    let frames = capture.frames();
    assert_eq!(frames.len(), 2);
    assert_call_is_replayed_and_result_is_staged(
        &frames[1],
        "call-pending",
        "pending",
        CANCELLED_TOOL_OUTPUT,
    );
    assert_eq!(
        native_log(application.current_projection().projection(), "pending"),
        vec![
            tool_call("call-pending", "pending"),
            tool_result("call-pending", CANCELLED_TOOL_OUTPUT),
        ],
        "cancellation fallback is part of the owning component history on the next reconcile"
    );
}

/// Adds two integers.
#[tool]
fn add(left: i64, right: i64) -> Result<i64, ToolError> {
    Ok(left + right)
}

#[component]
fn typed_add_application() -> Component {
    NativeToolCall::new(add)
}

#[tokio::test]
async fn typed_native_tool_component_records_successful_and_invalid_argument_calls() {
    let valid_arguments = r#"{"left":2,"right":5}"#;
    let invalid_arguments = r#"{"left":"bad","right":5}"#;
    let (port, capture) = ScriptedPort::new([
        Script::Finite(vec![
            tool_fact_with_arguments(1, 1, "typed-valid", "add", valid_arguments),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::Finite(vec![
            tool_fact_with_arguments(2, 1, "typed-invalid", "add", invalid_arguments),
            ProviderFact::ReactionCompleted { primary_text: None },
        ]),
        Script::completed(),
    ]);
    let mut application =
        Application::mount(typed_add_application, port).expect("mount typed native tool");

    assert!(matches!(
        application.react().await.expect("valid typed tool call"),
        ControlFlow::Continue(())
    ));
    assert!(matches!(
        application.react().await.expect("invalid typed tool call"),
        ControlFlow::Continue(())
    ));
    assert!(matches!(
        application.react().await.expect("typed tool continuation"),
        ControlFlow::Continue(())
    ));

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].tools, ["add"]);
    assert_eq!(
        native_items(&frames[1].staged_inputs),
        vec![tool_result("typed-valid", "7")]
    );

    let invalid_output = native_items(&frames[2].staged_inputs)
        .into_iter()
        .find_map(|item| match item {
            CanonicalInputItem::ToolResult { call_id, content } if call_id == "typed-invalid" => {
                Some(content)
            }
            _ => None,
        })
        .expect("invalid typed call produces a staged result");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&invalid_output)
            .expect("invalid argument result is JSON"),
        serde_json::json!({
            "error": {
                "code": "invalid_arguments",
                "message": "Arguments do not match this tool's input schema.",
            }
        })
    );

    let log = native_log(application.current_projection().projection(), "add");
    assert_eq!(
        log,
        vec![
            tool_call_with_arguments("typed-valid", "add", valid_arguments),
            tool_result("typed-valid", "7"),
            tool_call_with_arguments("typed-invalid", "add", invalid_arguments),
            tool_result("typed-invalid", &invalid_output),
        ],
        "each typed tool dispatch appends one call and one result in response order"
    );
}
