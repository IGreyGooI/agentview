use std::{collections::HashSet, convert::Infallible, future::Future, pin::Pin};

use futures::{StreamExt, stream::FuturesUnordered};

use crate::component::{
    ComponentHost, ComponentHostFault, ComponentHostId,
    authoring::{ComponentAttemptFault, RenderBindings},
};

#[allow(
    deprecated,
    reason = "this feature-gated host implements the retained ProviderPort compatibility runtime"
)]
use super::{ProviderEvent, ProviderFault, ProviderPort, RenderedProjection};

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineObservation {
    InputSubmitted,
    WireSnapshotAccepted,
    WireSnapshotBlocked {
        reason: &'static str,
    },
    ProviderEvent {
        ordinal: u64,
    },
    PartialCommitted,
    ToolCallPending {
        call_id: String,
    },
    HandlerStarted {
        call_id: String,
    },
    HandlerCompleted {
        call_id: String,
    },
    ToolCallClosed {
        call_id: String,
    },
    Terminal {
        stage: &'static str,
        reason: &'static str,
    },
    Cleanup {
        outcome: &'static str,
    },
}

pub trait EngineObserver: Send {
    fn observe(&mut self, observation: &EngineObservation);
}

struct CompletedLane {
    call: super::ToolCall,
    output: super::ToolOutput,
}

type ReactionLane =
    Pin<Box<dyn Future<Output = Result<CompletedLane, ApplicationHostFault>> + Send>>;

/// One generation-exact legacy Component render ready for Provider dispatch.
struct PreparedComponentReaction {
    projection: RenderedProjection,
    bindings: RenderBindings<ProviderEvent>,
}

/// Owns the terminal observation and deliberately outlives all reaction-local
/// resources. During panic unwind its Drop does not re-enter user observer
/// code, so the original payload propagates after later resources are dropped.
struct ReactionLifecycle<'a> {
    observer: &'a mut Option<Box<dyn EngineObserver>>,
    stage: &'static str,
    reason: &'static str,
    terminal_set: bool,
}

impl<'a> ReactionLifecycle<'a> {
    fn new(observer: &'a mut Option<Box<dyn EngineObserver>>) -> Self {
        Self {
            observer,
            stage: "cancellation",
            reason: "reaction_dropped",
            terminal_set: false,
        }
    }

    fn observe(&mut self, observation: EngineObservation) {
        if let Some(observer) = self.observer.as_mut() {
            observer.observe(&observation);
        }
    }

    fn terminal(&mut self, stage: &'static str, reason: &'static str) {
        if !self.terminal_set {
            self.stage = stage;
            self.reason = reason;
            self.terminal_set = true;
        }
    }
}

impl Drop for ReactionLifecycle<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        self.observe(EngineObservation::Terminal {
            stage: self.stage,
            reason: self.reason,
        });
        self.observe(EngineObservation::Cleanup {
            outcome: "complete",
        });
    }
}

/// Coordinates one retained Component application with one model backend.
///
/// This compatibility runtime does not execute `use_preparation` declarations.
/// It returns [`ApplicationHostFault::ComponentPreparationsUnsupported`]
/// before invoking [`ProviderPort::execute`] when a Component declares one.
/// Use [`Application`](super::Application) with a [`ReactionPort`](super::ReactionPort)
/// for preparation preparation.
#[deprecated(note = "use `Application<P>` as the mounted runtime owner")]
pub struct ApplicationHost<P> {
    provider: P,
    bound_host: Option<ComponentHostId>,
    observer: Option<Box<dyn EngineObserver>>,
}

#[allow(
    deprecated,
    reason = "methods implement the deprecated ApplicationHost compatibility type"
)]
impl<P> ApplicationHost<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            bound_host: None,
            observer: None,
        }
    }

    pub fn with_observer(mut self, observer: impl EngineObserver + 'static) -> Self {
        self.observer = Some(Box::new(observer));
        self
    }
}

#[allow(
    deprecated,
    reason = "dispatch preserves the feature-gated ProviderPort compatibility contract"
)]
impl<P> ApplicationHost<P>
where
    P: ProviderPort,
{
    /// Render and complete exactly one Provider reaction.
    ///
    /// The returned future resolves only after Provider EOF, owned tool work,
    /// and generation-local EOF diagnostics. Dropping it cancels the reaction by
    /// dropping its Provider stream and local bindings; successful Component
    /// updates remain.
    pub async fn dispatch_llm_reaction<Props>(
        &mut self,
        components: &mut ComponentHost<Props>,
    ) -> Result<(), ApplicationHostFault>
    where
        Props: Clone + Send + 'static,
    {
        match self
            .dispatch_llm_reaction_with_pre_reconcile_capture(
                components,
                || Ok::<_, Infallible>(()),
            )
            .await?
        {
            Ok(()) => Ok(()),
            Err(never) => match never {},
        }
    }

    /// Dispatch one reaction and capture compatibility state immediately before
    /// the successful post-reconcile render.
    ///
    /// New frame-driven orchestration does not need this seam. The retained
    /// `ComponentReactionRuntime` uses it to freeze its typed output selection
    /// after handlers finish, while still allowing render-time publication to
    /// remain valid during post-reconcile.
    pub(crate) async fn dispatch_llm_reaction_with_pre_reconcile_capture<Props, T, E>(
        &mut self,
        components: &mut ComponentHost<Props>,
        capture: impl FnOnce() -> Result<T, E>,
    ) -> Result<Result<T, E>, ApplicationHostFault>
    where
        Props: Clone + Send + 'static,
    {
        // Declared before every reaction resource so its Drop observes only
        // after streams, lanes, and bindings have synchronously dropped.
        let mut lifecycle = ReactionLifecycle::new(&mut self.observer);
        if let Err(fault) = Self::bind_or_validate(&mut self.bound_host, components.id()) {
            lifecycle.terminal("host", "component_host_mismatch");
            return Err(fault);
        }

        let prepared = match prepare_component_reaction(components) {
            Ok(prepared) => prepared,
            Err(fault) => {
                lifecycle.terminal("render", "component_render");
                return Err(fault);
            }
        };

        dispatch_prepared_reaction(&mut self.provider, prepared, &mut lifecycle).await?;
        let captured = match capture() {
            Ok(captured) => captured,
            Err(fault) => {
                lifecycle.terminal("output_capture", "compatibility");
                return Ok(Err(fault));
            }
        };

        if components.is_dirty() {
            if let Err(fault) = components.render() {
                lifecycle.terminal("post_reconcile", "component_render");
                return Err(fault.into());
            }
        }
        lifecycle.terminal("provider_eof", "normal_eof");
        Ok(Ok(captured))
    }

    fn bind_or_validate(
        bound_host: &mut Option<ComponentHostId>,
        observed: ComponentHostId,
    ) -> Result<(), ApplicationHostFault> {
        match *bound_host {
            Some(expected) if expected != observed => {
                Err(ApplicationHostFault::ComponentHostMismatch { expected, observed })
            }
            Some(_) => Ok(()),
            None => {
                *bound_host = Some(observed);
                Ok(())
            }
        }
    }
}

fn prepare_component_reaction<Props>(
    components: &mut ComponentHost<Props>,
) -> Result<PreparedComponentReaction, ApplicationHostFault>
where
    Props: Clone + Send + 'static,
{
    let prepared = components.render()?;
    let (projection, preparations, bindings) = prepared.into_execution_parts();
    if !preparations.is_empty() {
        return Err(ApplicationHostFault::ComponentPreparationsUnsupported);
    }
    Ok(PreparedComponentReaction {
        projection,
        bindings,
    })
}

#[allow(
    deprecated,
    reason = "dispatch executes the retained ProviderPort compatibility stream"
)]
async fn dispatch_prepared_reaction<P>(
    provider: &mut P,
    prepared: PreparedComponentReaction,
    lifecycle: &mut ReactionLifecycle<'_>,
) -> Result<(), ApplicationHostFault>
where
    P: ProviderPort,
{
    let PreparedComponentReaction {
        projection,
        mut bindings,
    } = prepared;
    let mut events = match provider.execute(projection).await {
        Ok(events) => events,
        Err(fault) => {
            lifecycle.observe(EngineObservation::WireSnapshotBlocked {
                reason: "provider_setup",
            });
            lifecycle.terminal("provider_setup", "execute");
            return Err(ApplicationHostFault::ProviderSetup(fault));
        }
    };
    lifecycle.observe(EngineObservation::InputSubmitted);
    lifecycle.observe(EngineObservation::WireSnapshotAccepted);
    let mut lanes = FuturesUnordered::<ReactionLane>::new();
    let mut tool_call_ids = HashSet::new();
    let mut event_ordinal = 0_u64;

    loop {
        enum Next {
            Event(Option<Result<ProviderEvent, ProviderFault>>),
            Lane(Option<Result<CompletedLane, ApplicationHostFault>>),
        }
        let next = if lanes.is_empty() {
            Next::Event(events.next().await)
        } else {
            tokio::select! {
                event = events.next() => Next::Event(event),
                lane = lanes.next() => Next::Lane(lane),
            }
        };
        let event = match next {
            Next::Lane(lane) => match finish_lane(lane, lifecycle) {
                Ok(()) => continue,
                Err(fault) => return Err(fault),
            },
            Next::Event(None) => break,
            Next::Event(Some(Err(fault))) => {
                lifecycle.terminal("provider_stream", "stream_fault");
                return Err(ApplicationHostFault::ProviderExecution(fault));
            }
            Next::Event(Some(Ok(event))) => event,
        };
        event_ordinal = event_ordinal.saturating_add(1);
        lifecycle.observe(EngineObservation::ProviderEvent {
            ordinal: event_ordinal,
        });
        match event {
            ProviderEvent::Text(event) => {
                if matches!(event, crate::llm_call::TextTurnEvent::TextDelta(_)) {
                    lifecycle.observe(EngineObservation::PartialCommitted);
                }
                if let Err(fault) = await_bindings_with_lanes(
                    bindings.dispatch(ProviderEvent::Text(event)),
                    &mut lanes,
                    lifecycle,
                )
                .await
                {
                    lifecycle.terminal("binding", "event_handler");
                    return Err(fault);
                }
            }
            ProviderEvent::ToolCall(call) => {
                if !tool_call_ids.insert(call.call_id().to_owned()) {
                    lifecycle.terminal("provider_event", "duplicate_tool_call");
                    return Err(ApplicationHostFault::DuplicateToolCall {
                        call_id: call.call_id().to_owned(),
                    });
                }
                lifecycle.observe(EngineObservation::ToolCallPending {
                    call_id: call.call_id().to_owned(),
                });
                let lane_call = call.clone();
                let lane_call_id = lane_call.call_id().to_owned();
                let future = match bindings.start_native_tool(call) {
                    Ok(future) => future,
                    Err(fault) => {
                        lifecycle.terminal("tool_binding", "start");
                        return Err(ApplicationHostFault::Bindings(fault));
                    }
                };
                lifecycle.observe(EngineObservation::HandlerStarted {
                    call_id: lane_call_id.clone(),
                });
                lanes.push(Box::pin(async move {
                    let output = future.await.map_err(|fault| {
                        ApplicationHostFault::Bindings(ComponentAttemptFault::native_tool(fault))
                    })?;
                    Ok::<_, ApplicationHostFault>(CompletedLane {
                        call: lane_call,
                        output,
                    })
                }));
            }
        }
    }

    drop(events);
    while !lanes.is_empty() {
        finish_lane(lanes.next().await, lifecycle)?;
    }
    if let Err(fault) =
        await_bindings_with_lanes(bindings.finish_normal(), &mut lanes, lifecycle).await
    {
        lifecycle.terminal("terminal_handler", "finish");
        return Err(fault);
    }
    Ok(())
}

fn finish_lane(
    lane: Option<Result<CompletedLane, ApplicationHostFault>>,
    lifecycle: &mut ReactionLifecycle<'_>,
) -> Result<(), ApplicationHostFault> {
    match lane {
        Some(Ok(completed)) => {
            let call_id = completed.call.call_id().to_owned();
            lifecycle.observe(EngineObservation::HandlerCompleted {
                call_id: call_id.clone(),
            });
            completed
                .call
                .publish_output(completed.output)
                .map_err(|_| {
                    lifecycle.terminal("tool_sink", "accept");
                    ApplicationHostFault::ToolOutputSinkClosed {
                        call_id: call_id.clone(),
                    }
                })?;
            lifecycle.observe(EngineObservation::ToolCallClosed { call_id });
            Ok(())
        }
        Some(Err(fault)) => {
            lifecycle.terminal("tool_lane", "handler");
            Err(fault)
        }
        None => Ok(()),
    }
}

async fn await_bindings_with_lanes<T, F>(
    future: F,
    lanes: &mut FuturesUnordered<ReactionLane>,
    lifecycle: &mut ReactionLifecycle<'_>,
) -> Result<T, ApplicationHostFault>
where
    F: Future<Output = Result<T, ComponentAttemptFault>>,
{
    let mut future = Box::pin(future);
    loop {
        if lanes.is_empty() {
            return future.await.map_err(ApplicationHostFault::Bindings);
        }
        tokio::select! {
            result = &mut future => return result.map_err(ApplicationHostFault::Bindings),
            lane = lanes.next() => finish_lane(lane, lifecycle)?,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApplicationHostFault {
    #[error("ApplicationHost is bound to {expected:?}, not {observed:?}")]
    ComponentHostMismatch {
        expected: ComponentHostId,
        observed: ComponentHostId,
    },
    #[error(
        "Component preparations require Application<P> with ReactionPort and are unsupported by ApplicationHost"
    )]
    ComponentPreparationsUnsupported,
    #[error(transparent)]
    Component(#[from] ComponentHostFault),
    #[error("ProviderPort execute setup failed: {0}")]
    ProviderSetup(ProviderFault),
    #[error("Provider execution failed: {0}")]
    ProviderExecution(ProviderFault),
    #[error("Component bindings failed: {0}")]
    Bindings(#[from] ComponentAttemptFault),
    #[error("tool output sink closed for call `{call_id}`")]
    ToolOutputSinkClosed { call_id: String },
    #[error("provider emitted duplicate tool call `{call_id}`")]
    DuplicateToolCall { call_id: String },
}

#[cfg(test)]
#[allow(
    deprecated,
    reason = "these tests exercise the feature-gated ApplicationHost and ProviderPort compatibility runtime"
)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        future::pending,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use futures::stream::FuturesUnordered;
    use tokio::sync::Notify;

    use crate::{
        component::{
            ComponentHost,
            execution::{
                ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort, ToolCall,
                ToolOutput, ToolOutputSink,
            },
            prelude::*,
        },
        llm_call::TextTurnEvent,
    };

    use super::{
        ApplicationHost, ApplicationHostFault, CompletedLane, EngineObservation, EngineObserver,
        ReactionLane, ReactionLifecycle, await_bindings_with_lanes, finish_lane,
    };

    #[derive(Debug, Default)]
    struct RecordingSink {
        accepted: Mutex<Vec<(u64, ToolOutput)>>,
    }

    impl ToolOutputSink for RecordingSink {
        fn register(&self, _ordinal: u64, _call_id: &str) -> Result<(), ()> {
            Ok(())
        }

        fn accept(&self, ordinal: u64, output: ToolOutput) -> Result<(), ()> {
            self.accepted.lock().unwrap().push((ordinal, output));
            Ok(())
        }
    }

    struct RecordingObserver(Arc<Mutex<Vec<EngineObservation>>>);

    impl EngineObserver for RecordingObserver {
        fn observe(&mut self, observation: &EngineObservation) {
            self.0.lock().unwrap().push(observation.clone());
        }
    }

    struct PanickingObserver {
        calls: Arc<AtomicUsize>,
    }

    impl EngineObserver for PanickingObserver {
        fn observe(&mut self, _observation: &EngineObservation) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            panic!("observer panic sentinel");
        }
    }

    #[derive(Clone)]
    struct NativeLaneProps {
        started: Arc<AtomicUsize>,
        started_signal: Arc<Notify>,
        release: Arc<Notify>,
        fail_release: Arc<Notify>,
        fail_call_id: Option<String>,
        dropped: Arc<AtomicUsize>,
        text_seen: Arc<Notify>,
        terminal_runs: Arc<AtomicUsize>,
    }

    struct LaneDropProbe(Arc<AtomicUsize>);

    impl Drop for LaneDropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct LaneFailure;

    impl std::fmt::Display for LaneFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("lane failure sentinel")
        }
    }

    #[component]
    fn owned_lane_application(
        props: NativeLaneProps,
        events: EventInput<ProviderEvent>,
    ) -> Component {
        let started = Arc::clone(&props.started);
        let started_signal = Arc::clone(&props.started_signal);
        let release = Arc::clone(&props.release);
        let fail_release = Arc::clone(&props.fail_release);
        let fail_call_id = props.fail_call_id.clone();
        let dropped = Arc::clone(&props.dropped);
        let text_seen = Arc::clone(&props.text_seen);
        let terminal_runs = Arc::clone(&props.terminal_runs);
        let text = events.select(ProviderEvent::TEXT);
        let xml = events.select(ProviderEvent::TEXT);

        view! {
            runtime { "owned lanes" }
            {
                NativeToolCall::named("wait")
                    .on_call(move |call| {
                        let started = Arc::clone(&started);
                        let started_signal = Arc::clone(&started_signal);
                        let release = Arc::clone(&release);
                        let fail_release = Arc::clone(&fail_release);
                        let fail_call_id = fail_call_id.clone();
                        let dropped = Arc::clone(&dropped);
                        async move {
                            let _drop_probe = LaneDropProbe(dropped);
                            started.fetch_add(1, Ordering::SeqCst);
                            started_signal.notify_one();
                            if fail_call_id.as_deref() == Some(call.call_id()) {
                                fail_release.notified().await;
                                return Err::<ToolOutput, _>(LaneFailure);
                            }
                            release.notified().await;
                            Ok::<_, LaneFailure>(call.output("owned-lane-result"))
                        }
                    })
            }
            {
                EventListener::observe("test.owned-lane.text", "v1")
                    .listen_to(text)
                    .on_event(move |_| {
                        let text_seen = Arc::clone(&text_seen);
                        async move {
                            text_seen.notify_one();
                            Ok::<(), Infallible>(())
                        }
                    })
            }
            {
                XmlStreamingToolCall::contract("test.owned-lane.finish", "v1")
                    .empty_element("done")
                    .required_attribute::<usize>("value")
                    .listen_to(xml)
                    .on_decoded(|_| async { Ok::<(), Infallible>(()) })
                    .on_invalid(move |_| {
                        let terminal_runs = Arc::clone(&terminal_runs);
                        async move {
                            terminal_runs.fetch_add(1, Ordering::SeqCst);
                            Ok::<(), Infallible>(())
                        }
                    })
            }
        }
    }

    struct OwnedLanePort {
        events: VecDeque<Result<ProviderEvent, ProviderFault>>,
        keep_open: bool,
        drops: Option<Arc<AtomicUsize>>,
    }

    struct OwnedLaneStream {
        events: VecDeque<Result<ProviderEvent, ProviderFault>>,
        keep_open: bool,
        drops: Option<Arc<AtomicUsize>>,
    }

    impl futures::Stream for OwnedLaneStream {
        type Item = Result<ProviderEvent, ProviderFault>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if let Some(event) = self.events.pop_front() {
                std::task::Poll::Ready(Some(event))
            } else if self.keep_open {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(None)
            }
        }
    }

    impl Drop for OwnedLaneStream {
        fn drop(&mut self) {
            if let Some(drops) = &self.drops {
                drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[async_trait]
    impl ProviderPort for OwnedLanePort {
        async fn execute<'a>(
            &'a mut self,
            _projection: crate::component::execution::RenderedProjection,
        ) -> Result<ProviderEventStream<'a>, ProviderFault> {
            Ok(Box::pin(OwnedLaneStream {
                events: std::mem::take(&mut self.events),
                keep_open: self.keep_open,
                drops: self.drops.clone(),
            }))
        }
    }

    async fn wait_for_lanes(started: &AtomicUsize, signal: &Notify, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < expected {
                signal.notified().await;
            }
        })
        .await
        .expect("owned lanes start");
    }

    #[test]
    fn lane_result_is_staged_by_the_main_sequencer_before_close_observation() {
        let sink = Arc::new(RecordingSink::default());
        let sink_handle: Arc<dyn ToolOutputSink> = sink.clone();
        let call = ToolCall::new("call-1", "lookup", "{}")
            .unwrap()
            .with_output_sink(4, sink_handle)
            .unwrap();
        let output = call.output("result");
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut observer: Option<Box<dyn EngineObserver>> =
            Some(Box::new(RecordingObserver(Arc::clone(&observations))));
        let mut lifecycle = ReactionLifecycle::new(&mut observer);

        finish_lane(Some(Ok(CompletedLane { call, output })), &mut lifecycle).unwrap();

        assert_eq!(sink.accepted.lock().unwrap().len(), 1);
        assert_eq!(
            *observations.lock().unwrap(),
            vec![
                EngineObservation::HandlerCompleted {
                    call_id: String::from("call-1"),
                },
                EngineObservation::ToolCallClosed {
                    call_id: String::from("call-1"),
                },
            ]
        );
    }

    #[test]
    fn observer_panic_propagates_without_reentering_the_observer_during_unwind() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut panicking: Option<Box<dyn EngineObserver>> = Some(Box::new(PanickingObserver {
            calls: Arc::clone(&calls),
        }));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut lifecycle = ReactionLifecycle::new(&mut panicking);
            lifecycle.observe(EngineObservation::InputSubmitted);
        }));

        assert!(panic.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn lane_fault_remains_terminal_cause_when_binding_dispatch_unwinds() {
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut observer: Option<Box<dyn EngineObserver>> =
            Some(Box::new(RecordingObserver(Arc::clone(&observations))));
        {
            let mut lifecycle = ReactionLifecycle::new(&mut observer);
            let mut lanes = FuturesUnordered::<ReactionLane>::new();
            lanes.push(Box::pin(async {
                Err(ApplicationHostFault::ToolOutputSinkClosed {
                    call_id: String::from("call-fault"),
                })
            }));

            let fault = await_bindings_with_lanes(
                pending::<Result<(), crate::component::authoring::ComponentAttemptFault>>(),
                &mut lanes,
                &mut lifecycle,
            )
            .await
            .expect_err("lane failure wins over a blocked binding handler");
            assert!(matches!(
                fault,
                ApplicationHostFault::ToolOutputSinkClosed { ref call_id } if call_id == "call-fault"
            ));

            // This is the caller's generic dispatch classification. The lane
            // cause must remain the observable terminal cause.
            lifecycle.terminal("binding", "event_handler");
        }

        assert_eq!(
            *observations.lock().unwrap(),
            vec![
                EngineObservation::Terminal {
                    stage: "tool_lane",
                    reason: "handler",
                },
                EngineObservation::Cleanup {
                    outcome: "complete",
                },
            ]
        );
    }

    #[tokio::test]
    async fn owned_lanes_run_concurrently_and_eof_waits_before_parser_diagnostics() {
        let sink = Arc::new(RecordingSink::default());
        let sink_handle: Arc<dyn ToolOutputSink> = sink.clone();
        let first = ToolCall::new("call-first", "wait", "{}")
            .unwrap()
            .with_output_sink(3, Arc::clone(&sink_handle))
            .unwrap();
        let second = ToolCall::new("call-second", "wait", "{}")
            .unwrap()
            .with_output_sink(1, sink_handle)
            .unwrap();
        let props = NativeLaneProps {
            started: Arc::new(AtomicUsize::new(0)),
            started_signal: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            fail_release: Arc::new(Notify::new()),
            fail_call_id: None,
            dropped: Arc::new(AtomicUsize::new(0)),
            text_seen: Arc::new(Notify::new()),
            terminal_runs: Arc::new(AtomicUsize::new(0)),
        };
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut components = ComponentHost::new(owned_lane_application, props.clone());
        let port = OwnedLanePort {
            events: VecDeque::from([
                Ok(ProviderEvent::ToolCall(first)),
                Ok(ProviderEvent::ToolCall(second)),
                Ok(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
                    "<done value=\"1\"",
                )))),
                Ok(ProviderEvent::Text(TextTurnEvent::TextComplete(
                    String::from("<done value=\"1\""),
                ))),
            ]),
            keep_open: false,
            drops: None,
        };
        let mut host =
            ApplicationHost::new(port).with_observer(RecordingObserver(Arc::clone(&observations)));
        let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                () = props.text_seen.notified() => {},
                result = reaction.as_mut() => panic!("reaction ended before its later text event: {result:?}"),
            }
        })
        .await
        .expect("provider continues after scheduling lanes");
        wait_for_lanes(&props.started, &props.started_signal, 2).await;
        assert_eq!(props.terminal_runs.load(Ordering::SeqCst), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(40), reaction.as_mut())
                .await
                .is_err(),
            "Provider EOF must wait for all owned lane futures"
        );

        props.release.notify_waiters();
        reaction
            .await
            .expect("both lane outputs stage before normal EOF diagnostics");

        assert_eq!(sink.accepted.lock().unwrap().len(), 2);
        assert_eq!(props.terminal_runs.load(Ordering::SeqCst), 1);
        assert_eq!(props.dropped.load(Ordering::SeqCst), 2);
        let observations = observations.lock().unwrap();
        assert_eq!(
            &observations[..10],
            [
                EngineObservation::InputSubmitted,
                EngineObservation::WireSnapshotAccepted,
                EngineObservation::ProviderEvent { ordinal: 1 },
                EngineObservation::ToolCallPending {
                    call_id: String::from("call-first"),
                },
                EngineObservation::HandlerStarted {
                    call_id: String::from("call-first"),
                },
                EngineObservation::ProviderEvent { ordinal: 2 },
                EngineObservation::ToolCallPending {
                    call_id: String::from("call-second"),
                },
                EngineObservation::HandlerStarted {
                    call_id: String::from("call-second"),
                },
                EngineObservation::ProviderEvent { ordinal: 3 },
                EngineObservation::PartialCommitted,
            ]
        );
        for call_id in ["call-first", "call-second"] {
            let completed = observations
                .iter()
                .position(|observation| {
                    matches!(
                        observation,
                        EngineObservation::HandlerCompleted { call_id: observed } if observed == call_id
                    )
                })
                .expect("handler completion observed");
            let closed = observations
                .iter()
                .position(|observation| {
                    matches!(
                        observation,
                        EngineObservation::ToolCallClosed { call_id: observed } if observed == call_id
                    )
                })
                .expect("staged output close observed");
            assert!(completed < closed, "output stages after handler completion");
        }
    }

    #[tokio::test]
    async fn dropping_reaction_cancels_owned_lanes_before_cancellation_cleanup() {
        let sink: Arc<dyn ToolOutputSink> = Arc::new(RecordingSink::default());
        let call = ToolCall::new("call-cancel", "wait", "{}")
            .unwrap()
            .with_output_sink(0, sink)
            .unwrap();
        let props = NativeLaneProps {
            started: Arc::new(AtomicUsize::new(0)),
            started_signal: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            fail_release: Arc::new(Notify::new()),
            fail_call_id: None,
            dropped: Arc::new(AtomicUsize::new(0)),
            text_seen: Arc::new(Notify::new()),
            terminal_runs: Arc::new(AtomicUsize::new(0)),
        };
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut components = ComponentHost::new(owned_lane_application, props.clone());
        let port = OwnedLanePort {
            events: VecDeque::from([Ok(ProviderEvent::ToolCall(call))]),
            keep_open: false,
            drops: None,
        };
        let mut host =
            ApplicationHost::new(port).with_observer(RecordingObserver(Arc::clone(&observations)));
        let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));

        tokio::time::timeout(Duration::from_secs(1), async {
            while props.started.load(Ordering::SeqCst) < 1 {
                tokio::select! {
                    () = props.started_signal.notified() => {},
                    result = reaction.as_mut() => panic!("reaction ended before its lane started: {result:?}"),
                }
            }
        })
        .await
        .expect("owned lane starts before cancellation");
        drop(reaction);

        assert_eq!(props.dropped.load(Ordering::SeqCst), 1);
        let observations = observations.lock().unwrap();
        assert_eq!(
            &observations[observations.len() - 2..],
            [
                EngineObservation::Terminal {
                    stage: "cancellation",
                    reason: "reaction_dropped",
                },
                EngineObservation::Cleanup {
                    outcome: "complete",
                },
            ]
        );
    }

    #[tokio::test]
    async fn lane_failure_while_provider_is_open_drops_sibling_lane_and_stream() {
        let sink: Arc<dyn ToolOutputSink> = Arc::new(RecordingSink::default());
        let failing = ToolCall::new("call-fail", "wait", "{}")
            .unwrap()
            .with_output_sink(0, Arc::clone(&sink))
            .unwrap();
        let sibling = ToolCall::new("call-sibling", "wait", "{}")
            .unwrap()
            .with_output_sink(1, sink)
            .unwrap();
        let stream_drops = Arc::new(AtomicUsize::new(0));
        let props = NativeLaneProps {
            started: Arc::new(AtomicUsize::new(0)),
            started_signal: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            fail_release: Arc::new(Notify::new()),
            fail_call_id: Some(String::from("call-fail")),
            dropped: Arc::new(AtomicUsize::new(0)),
            text_seen: Arc::new(Notify::new()),
            terminal_runs: Arc::new(AtomicUsize::new(0)),
        };
        let mut components = ComponentHost::new(owned_lane_application, props.clone());
        let port = OwnedLanePort {
            events: VecDeque::from([
                Ok(ProviderEvent::ToolCall(failing)),
                Ok(ProviderEvent::ToolCall(sibling)),
            ]),
            keep_open: true,
            drops: Some(Arc::clone(&stream_drops)),
        };
        let mut host = ApplicationHost::new(port);
        let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));

        tokio::time::timeout(Duration::from_secs(1), async {
            while props.started.load(Ordering::SeqCst) < 2 {
                tokio::select! {
                    () = props.started_signal.notified() => {},
                    result = reaction.as_mut() => panic!("reaction ended before both lanes started: {result:?}"),
                }
            }
        })
        .await
        .expect("both lanes start while the provider stream remains open");

        props.fail_release.notify_one();
        let fault = tokio::time::timeout(Duration::from_secs(1), reaction)
            .await
            .expect("lane failure returns without waiting on the open provider stream")
            .expect_err("failing lane aborts the reaction");
        assert!(matches!(fault, ApplicationHostFault::Bindings(_)));
        assert_eq!(props.dropped.load(Ordering::SeqCst), 2);
        assert_eq!(stream_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_outcome_reports_completed_cleanup_after_fault() {
        let observations = Arc::new(Mutex::new(Vec::new()));
        let mut observer: Option<Box<dyn EngineObserver>> =
            Some(Box::new(RecordingObserver(Arc::clone(&observations))));
        {
            let mut lifecycle = ReactionLifecycle::new(&mut observer);
            lifecycle.terminal("provider_stream", "stream_fault");
        }
        assert_eq!(
            *observations.lock().unwrap(),
            vec![
                EngineObservation::Terminal {
                    stage: "provider_stream",
                    reason: "stream_fault",
                },
                EngineObservation::Cleanup {
                    outcome: "complete",
                },
            ]
        );
    }
}
