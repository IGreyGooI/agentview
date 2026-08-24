use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            ApplicationHost, ApplicationHostFault, EngineObservation, EngineObserver,
            ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort, RenderedProjection,
        },
        prelude::*,
        ComponentHost,
    },
    llm_call::TextTurnEvent,
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::{channel::mpsc, Stream, StreamExt};
use tokio::sync::Notify;

#[derive(Clone)]
struct HostProps {
    exposed: Arc<Mutex<Option<Signal<String>>>>,
}

#[component]
fn host_application(props: HostProps, _events: EventInput<ProviderEvent>) -> Component {
    let state = use_signal(|| String::from("A"));
    *props.exposed.lock().expect("signal exposure lock") = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted Signal read");
    view! {
        #[system_once]
        protocol { "Stable protocol." }
        state { "{value}" }
    }
}

struct ScriptedNoEventPort {
    setups: VecDeque<Result<(), ProviderFault>>,
    projections: Arc<Mutex<Vec<RenderedProjection>>>,
}

#[async_trait]
impl ProviderPort for ScriptedNoEventPort {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.projections
            .lock()
            .expect("projection capture lock")
            .push(projection);

        self.setups.pop_front().unwrap_or(Ok(()))?;
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[derive(Clone)]
struct HandlerProps {
    exposed: Arc<Mutex<Option<Signal<Vec<usize>>>>>,
    renders: Arc<AtomicUsize>,
}

#[component]
fn handler_application(props: HandlerProps, events: EventInput<ProviderEvent>) -> Component {
    props.renders.fetch_add(1, Ordering::SeqCst);
    let values = use_signal(Vec::<usize>::new);
    *props.exposed.lock().expect("handler signal exposure lock") = Some(values.clone());
    let rendered = values
        .with(|values| format!("{values:?}"))
        .expect("mounted handler Signal read");
    let handler_values = values.clone();

    view! {
        state { "{rendered}" }
        {
            EventListener::observe("test.values", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let values = handler_values.clone();
                    async move {
                        let value = event_number(event);
                        values.update(|values| values.push(value)).map(|_| ())
                    }
                })
        }
    }
}

struct ScriptedEventPort {
    scripts: VecDeque<EventScript>,
    execute_calls: Arc<AtomicUsize>,
    projections: Arc<Mutex<Vec<RenderedProjection>>>,
    polls: Option<Arc<AtomicUsize>>,
}

enum EventScript {
    Eof(Vec<usize>),
    FaultAfter(Vec<usize>, ProviderFault),
    PendingAfter {
        events: Vec<usize>,
        pending_polled: Arc<Notify>,
        drops: Arc<AtomicUsize>,
    },
}

enum EventTail {
    Eof,
    Fault(Option<ProviderFault>),
    Pending {
        polled: Arc<Notify>,
        drops: Arc<AtomicUsize>,
    },
}

struct ScriptedEventStream {
    events: VecDeque<usize>,
    tail: EventTail,
    polls: Option<Arc<AtomicUsize>>,
}

impl Stream for ScriptedEventStream {
    type Item = Result<ProviderEvent, ProviderFault>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(polls) = &self.polls {
            polls.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(Some(Ok(number_event(event))));
        }
        match &mut self.tail {
            EventTail::Eof => Poll::Ready(None),
            EventTail::Fault(fault) => Poll::Ready(fault.take().map(Err)),
            EventTail::Pending { polled, .. } => {
                polled.notify_one();
                Poll::Pending
            }
        }
    }
}

impl Drop for ScriptedEventStream {
    fn drop(&mut self) {
        if let EventTail::Pending { drops, .. } = &self.tail {
            drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl ProviderPort for ScriptedEventPort {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        self.projections
            .lock()
            .expect("projection capture lock")
            .push(projection);
        let script = self
            .scripts
            .pop_front()
            .unwrap_or_else(|| EventScript::Eof(Vec::new()));
        let (events, tail) = match script {
            EventScript::Eof(events) => (events, EventTail::Eof),
            EventScript::FaultAfter(events, fault) => (events, EventTail::Fault(Some(fault))),
            EventScript::PendingAfter {
                events,
                pending_polled,
                drops,
            } => (
                events,
                EventTail::Pending {
                    polled: pending_polled,
                    drops,
                },
            ),
        };
        Ok(Box::pin(ScriptedEventStream {
            events: events.into(),
            tail,
            polls: self.polls.clone(),
        }))
    }
}

#[derive(Clone)]
struct OrderedHandlerProps {
    log: Arc<Mutex<Vec<String>>>,
    first_started: Arc<Notify>,
    first_release: Arc<Notify>,
    renders: Arc<AtomicUsize>,
}

#[component]
fn ordered_handler_application(
    props: OrderedHandlerProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    props.renders.fetch_add(1, Ordering::SeqCst);
    let first_log = Arc::clone(&props.log);
    let second_log = Arc::clone(&props.log);
    let first_started = Arc::clone(&props.first_started);
    let first_release = Arc::clone(&props.first_release);
    let values = events.select(ProviderEvent::TEXT);
    let second_values = values.clone();

    view! {
        {
            EventListener::observe("test.order.first", "v1")
                .listen_to(values)
                .on_event(move |event| {
                    let log = Arc::clone(&first_log);
                    let started = Arc::clone(&first_started);
                    let release = Arc::clone(&first_release);
                    async move {
                        let value = event_number(event);
                        log.lock().unwrap().push(format!("{value}:first:start"));
                        if value == 1 {
                            started.notify_one();
                            release.notified().await;
                        }
                        log.lock().unwrap().push(format!("{value}:first:end"));
                        Ok::<(), Infallible>(())
                    }
                })
        }
        {
            EventListener::observe("test.order.second", "v1")
                .listen_to(second_values)
                .on_event(move |event| {
                    let log = Arc::clone(&second_log);
                    async move {
                        let value = event_number(event);
                        log.lock().unwrap().push(format!("{value}:second"));
                        Ok::<(), Infallible>(())
                    }
                })
        }
    }
}

#[derive(Clone)]
struct StreamingHandlerProps {
    decoded: Arc<Notify>,
    finish_started: Arc<Notify>,
    finish_release: Arc<Notify>,
    finish_count: Arc<AtomicUsize>,
}

#[component]
fn streaming_handler_application(
    props: StreamingHandlerProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let decoded = Arc::clone(&props.decoded);
    let finish_started = Arc::clone(&props.finish_started);
    let finish_release = Arc::clone(&props.finish_release);
    let finish_count = Arc::clone(&props.finish_count);

    XmlStreamingToolCall::contract("test.application-host", "v1")
        .empty_element("value")
        .required_attribute::<usize>("number")
        .exactly_one()
        .listen_to(events.select(ProviderEvent::TEXT))
        .on_decoded(move |_| {
            let decoded = Arc::clone(&decoded);
            async move {
                decoded.notify_one();
                Ok::<(), Infallible>(())
            }
        })
        .on_invalid(|_| async { Ok::<(), Infallible>(()) })
        .on_finish(move || async move {
            finish_count.fetch_add(1, Ordering::SeqCst);
            finish_started.notify_one();
            finish_release.notified().await;
            Ok::<(), Infallible>(())
        })
}

struct StreamingPort {
    events: Option<Vec<ProviderEvent>>,
    eof: Option<mpsc::UnboundedReceiver<Result<ProviderEvent, ProviderFault>>>,
}

#[async_trait]
impl ProviderPort for StreamingPort {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        let events = self.events.take().unwrap_or_default();
        let eof = self.eof.take().unwrap_or_else(|| {
            let (_sender, receiver) = mpsc::unbounded();
            receiver
        });
        Ok(Box::pin(
            futures::stream::iter(events.into_iter().map(Ok)).chain(eof),
        ))
    }
}

struct BindingDropProbe {
    drops: Arc<AtomicUsize>,
}

impl Drop for BindingDropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct BindingDropProps {
    drops: Arc<AtomicUsize>,
    exposed: Arc<Mutex<Option<Signal<usize>>>>,
}

#[component]
fn binding_drop_application(
    props: BindingDropProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let value = use_signal(|| 0_usize);
    *props.exposed.lock().expect("cancellation signal lock") = Some(value.clone());
    let rendered = value.with(|value| *value).expect("mounted Signal read");
    let writer = value.clone();
    let probe = BindingDropProbe {
        drops: Arc::clone(&props.drops),
    };
    view! {
        state { "{rendered}" }
        {
            EventListener::observe("test.binding-drop", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let _keep_binding_alive = Arc::clone(&probe.drops);
                    let value = writer.clone();
                    async move { value.set(event_number(event)) }
                })
        }
    }
}

#[derive(Debug)]
struct HandlerFailure(&'static str);

impl fmt::Display for HandlerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Clone)]
struct FaultingHandlerProps {
    exposed: Arc<Mutex<Option<Signal<usize>>>>,
}

#[component]
fn faulting_handler_application(
    props: FaultingHandlerProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let value = use_signal(|| 0_usize);
    *props.exposed.lock().expect("faulting signal exposure lock") = Some(value.clone());
    let rendered = value.with(|value| *value).expect("mounted Signal read");
    let writer = value.clone();
    let first_route = events.select(ProviderEvent::TEXT);
    let second_route = first_route.clone();

    view! {
        state { "{rendered}" }
        {
            EventListener::observe("test.fault.write", "v1")
                .listen_to(first_route)
                .on_event(move |event| {
                    let value = writer.clone();
                    async move { value.set(event_number(event)) }
                })
        }
        {
            EventListener::observe("test.fault.fail", "v1")
                .listen_to(second_route)
                .on_event(|_| async {
                    Err::<(), _>(HandlerFailure("handler failure sentinel"))
                })
        }
    }
}

fn projection_text(projection: &RenderedProjection) -> String {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom).unwrap()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn number_event(value: usize) -> ProviderEvent {
    ProviderEvent::Text(TextTurnEvent::TextDelta(value.to_string()))
}

fn event_number(event: TextTurnEvent) -> usize {
    match event {
        TextTurnEvent::TextDelta(value) | TextTurnEvent::TextComplete(value) => {
            value.parse().expect("scripted event contains a number")
        }
    }
}

fn exposed_value<T>(exposed: &Arc<Mutex<Option<Signal<T>>>>) -> T
where
    T: Clone + Send + Sync + 'static,
{
    exposed
        .lock()
        .unwrap()
        .clone()
        .expect("Signal exposed")
        .with(Clone::clone)
        .unwrap()
}

fn scripted_no_event_port(
    setups: impl IntoIterator<Item = Result<(), ProviderFault>>,
) -> (ScriptedNoEventPort, Arc<Mutex<Vec<RenderedProjection>>>) {
    let projections = Arc::new(Mutex::new(Vec::new()));
    (
        ScriptedNoEventPort {
            setups: setups.into_iter().collect(),
            projections: Arc::clone(&projections),
        },
        projections,
    )
}

#[tokio::test]
async fn explicit_reactions_use_complete_projection_and_enforce_host_identity() {
    let (port, projections) = scripted_no_event_port([Ok(()), Ok(()), Ok(())]);
    let exposed = Arc::new(Mutex::new(None));
    let mut components = ComponentHost::new(
        host_application,
        HostProps {
            exposed: Arc::clone(&exposed),
        },
    );
    let mut host = ApplicationHost::new(port);

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("first reaction reaches Provider EOF");

    exposed
        .lock()
        .unwrap()
        .clone()
        .expect("Signal exposed")
        .set(String::from("B"))
        .unwrap();
    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("second reaction reaches Provider EOF");

    components
        .remount(HostProps {
            exposed: Arc::new(Mutex::new(None)),
        })
        .expect("same ComponentHost remounts");
    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("remount remains bound to the same ApplicationHost");
    let mut foreign_components = ComponentHost::new(
        host_application,
        HostProps {
            exposed: Arc::new(Mutex::new(None)),
        },
    );
    let fault = host
        .dispatch_llm_reaction(&mut foreign_components)
        .await
        .unwrap_err();

    assert!(matches!(
        fault,
        ApplicationHostFault::ComponentHostMismatch { .. }
    ));
    let captured = projections.lock().unwrap();
    assert_eq!(captured.len(), 3, "foreign Host must not execute the Port");
    let first_items = captured[0]
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .collect::<Vec<_>>();
    let second_items = captured[1]
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .collect::<Vec<_>>();
    assert_eq!(first_items.len(), 2);
    assert_eq!(second_items.len(), 2);
    assert_eq!(first_items[0], second_items[0]);
    let first = projection_text(&captured[0]);
    let second = projection_text(&captured[1]);
    assert!(first.contains("<protocol>Stable protocol.</protocol>"));
    assert!(first.contains("<state>A</state>"));
    assert!(second.contains("<protocol>Stable protocol.</protocol>"));
    assert!(second.contains("<state>B</state>"));
    assert!(!second.contains("<state>A</state>"));
}

struct ObservationLog(Arc<Mutex<Vec<EngineObservation>>>);

impl EngineObserver for ObservationLog {
    fn observe(&mut self, observation: &EngineObservation) {
        self.0.lock().unwrap().push(observation.clone());
    }
}

#[tokio::test]
async fn observer_sees_submission_partial_commit_and_terminal_in_causal_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let port = ScriptedEventPort {
        scripts: VecDeque::from([EventScript::Eof(vec![7])]),
        execute_calls: Arc::new(AtomicUsize::new(0)),
        projections: Arc::new(Mutex::new(Vec::new())),
        polls: None,
    };
    let exposed = Arc::new(Mutex::new(None));
    let mut components = ComponentHost::new(
        handler_application,
        HandlerProps {
            exposed,
            renders: Arc::new(AtomicUsize::new(0)),
        },
    );
    let mut host = ApplicationHost::new(port).with_observer(ObservationLog(Arc::clone(&log)));

    host.dispatch_llm_reaction(&mut components).await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        vec![
            EngineObservation::InputSubmitted,
            EngineObservation::WireSnapshotAccepted,
            EngineObservation::ProviderEvent { ordinal: 1 },
            EngineObservation::PartialCommitted,
            EngineObservation::Terminal {
                stage: "provider_eof",
                reason: "normal_eof"
            },
            EngineObservation::Cleanup {
                outcome: "complete"
            },
        ]
    );
}

#[tokio::test]
async fn reaction_awaits_events_and_handlers_in_provider_and_structural_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let first_started = Arc::new(Notify::new());
    let first_release = Arc::new(Notify::new());
    let renders = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let port = ScriptedEventPort {
        scripts: VecDeque::from([EventScript::Eof(vec![1, 2])]),
        execute_calls: Arc::clone(&execute_calls),
        projections: Arc::new(Mutex::new(Vec::new())),
        polls: Some(Arc::clone(&polls)),
    };
    let mut components = ComponentHost::new(
        ordered_handler_application,
        OrderedHandlerProps {
            log: Arc::clone(&log),
            first_started: Arc::clone(&first_started),
            first_release: Arc::clone(&first_release),
            renders: Arc::clone(&renders),
        },
    );
    let mut host = ApplicationHost::new(port);

    let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = first_started.notified() => {}
            result = reaction.as_mut() => panic!("reaction completed before the first handler blocked: {result:?}"),
        }
    })
    .await
    .expect("first handler starts");
    assert_eq!(*log.lock().unwrap(), ["1:first:start"]);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), reaction.as_mut())
            .await
            .is_err()
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);

    first_release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), reaction)
        .await
        .expect("reaction resumes after the first handler")
        .expect("reaction reaches EOF after every handler");

    assert_eq!(
        *log.lock().unwrap(),
        [
            "1:first:start",
            "1:first:end",
            "1:second",
            "2:first:start",
            "2:first:end",
            "2:second",
        ]
    );
    assert_eq!(renders.load(Ordering::SeqCst), 1);
    assert_eq!(execute_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_eof_awaits_the_terminal_handler() {
    let decoded = Arc::new(Notify::new());
    let finish_started = Arc::new(Notify::new());
    let finish_release = Arc::new(Notify::new());
    let finish_count = Arc::new(AtomicUsize::new(0));
    let (eof_sender, eof) = mpsc::unbounded();
    let port = StreamingPort {
        events: Some(vec![ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<value number=\"7\" />"),
        ))]),
        eof: Some(eof),
    };
    let mut components = ComponentHost::new(
        streaming_handler_application,
        StreamingHandlerProps {
            decoded: Arc::clone(&decoded),
            finish_started: Arc::clone(&finish_started),
            finish_release: Arc::clone(&finish_release),
            finish_count: Arc::clone(&finish_count),
        },
    );
    let mut host = ApplicationHost::new(port);

    let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = decoded.notified() => {}
            result = reaction.as_mut() => panic!("reaction completed before controlled EOF: {result:?}"),
        }
    })
    .await
    .expect("TextComplete is handled before EOF");
    assert!(
        tokio::time::timeout(Duration::from_millis(40), reaction.as_mut())
            .await
            .is_err()
    );
    assert_eq!(finish_count.load(Ordering::SeqCst), 0);

    drop(eof_sender);
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = finish_started.notified() => {}
            result = reaction.as_mut() => panic!("reaction completed before terminal handler blocked: {result:?}"),
        }
    })
    .await
    .expect("Provider EOF starts the terminal handler");
    assert_eq!(finish_count.load(Ordering::SeqCst), 1);

    finish_release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), reaction)
        .await
        .expect("reaction resumes after terminal handler completion")
        .expect("reaction succeeds after Provider EOF and terminal completion");
}

#[tokio::test]
async fn dropping_a_pending_stream_reaction_drops_stream_and_bindings_and_reuses_port() {
    let stream_polled = Arc::new(Notify::new());
    let stream_drops = Arc::new(AtomicUsize::new(0));
    let binding_drops = Arc::new(AtomicUsize::new(0));
    let exposed = Arc::new(Mutex::new(None));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let projections = Arc::new(Mutex::new(Vec::new()));
    let port = ScriptedEventPort {
        scripts: VecDeque::from([
            EventScript::PendingAfter {
                events: vec![7],
                pending_polled: Arc::clone(&stream_polled),
                drops: Arc::clone(&stream_drops),
            },
            EventScript::Eof(Vec::new()),
        ]),
        execute_calls: Arc::clone(&execute_calls),
        projections: Arc::clone(&projections),
        polls: None,
    };
    let mut components = ComponentHost::new(
        binding_drop_application,
        BindingDropProps {
            drops: Arc::clone(&binding_drops),
            exposed: Arc::clone(&exposed),
        },
    );
    let mut host = ApplicationHost::new(port);

    let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = stream_polled.notified() => {}
            result = reaction.as_mut() => panic!("pending stream unexpectedly completed: {result:?}"),
        }
    })
    .await
    .expect("stream reaches its pending tail");
    assert_eq!(stream_drops.load(Ordering::SeqCst), 0);
    assert_eq!(binding_drops.load(Ordering::SeqCst), 0);
    assert_eq!(exposed_value(&exposed), 7);

    drop(reaction);
    assert_eq!(stream_drops.load(Ordering::SeqCst), 1);
    assert_eq!(binding_drops.load(Ordering::SeqCst), 1);

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Port remains reusable after command cancellation");
    assert_eq!(execute_calls.load(Ordering::SeqCst), 2);
    assert_eq!(binding_drops.load(Ordering::SeqCst), 2);
    let second_projection = projection_text(&projections.lock().unwrap()[1]);
    assert!(second_projection.contains("<state>7</state>"));
}

#[tokio::test]
async fn provider_setup_fault_is_reported_and_the_provider_remains_reusable() {
    let (port, projections) = scripted_no_event_port([
        Err(ProviderFault::retryable_transport("setup fault sentinel")),
        Ok(()),
    ]);
    let mut components = ComponentHost::new(
        host_application,
        HostProps {
            exposed: Arc::new(Mutex::new(None)),
        },
    );
    let mut host = ApplicationHost::new(port);

    let fault = host
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap_err();
    let ApplicationHostFault::ProviderSetup(provider_fault) = fault else {
        panic!("unexpected setup fault: {fault:?}");
    };
    assert_eq!(provider_fault.message(), "setup fault sentinel");

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Port remains reusable after a setup fault");
    assert_eq!(projections.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn provider_stream_fault_is_reported_and_the_provider_remains_reusable() {
    let exposed = Arc::new(Mutex::new(None));
    let renders = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let projections = Arc::new(Mutex::new(Vec::new()));
    let port = ScriptedEventPort {
        scripts: VecDeque::from([
            EventScript::FaultAfter(
                vec![7],
                ProviderFault::model_rejected("stream fault sentinel"),
            ),
            EventScript::Eof(Vec::new()),
        ]),
        execute_calls: Arc::clone(&execute_calls),
        projections: Arc::clone(&projections),
        polls: None,
    };
    let mut components = ComponentHost::new(
        handler_application,
        HandlerProps {
            exposed: Arc::clone(&exposed),
            renders,
        },
    );
    let mut host = ApplicationHost::new(port);

    let fault = host
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap_err();
    let ApplicationHostFault::ProviderExecution(provider_fault) = fault else {
        panic!("unexpected stream fault: {fault:?}");
    };
    assert_eq!(provider_fault.message(), "stream fault sentinel");
    assert_eq!(exposed_value(&exposed), vec![7]);

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Port remains reusable after a stream fault");
    assert_eq!(execute_calls.load(Ordering::SeqCst), 2);
    let second_projection = projection_text(&projections.lock().unwrap()[1]);
    assert!(second_projection.contains("<state>\\[7\\]</state>"));
}

#[tokio::test]
async fn binding_fault_preserves_prior_signal_updates_and_the_provider_remains_reusable() {
    let exposed = Arc::new(Mutex::new(None));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let projections = Arc::new(Mutex::new(Vec::new()));
    let port = ScriptedEventPort {
        scripts: VecDeque::from([EventScript::Eof(vec![7]), EventScript::Eof(Vec::new())]),
        execute_calls: Arc::clone(&execute_calls),
        projections: Arc::clone(&projections),
        polls: None,
    };
    let mut components = ComponentHost::new(
        faulting_handler_application,
        FaultingHandlerProps {
            exposed: Arc::clone(&exposed),
        },
    );
    let mut host = ApplicationHost::new(port);

    let fault = host
        .dispatch_llm_reaction(&mut components)
        .await
        .unwrap_err();
    let ApplicationHostFault::Bindings(binding_fault) = fault else {
        panic!("unexpected binding fault: {fault:?}");
    };
    assert!(
        binding_fault
            .to_string()
            .contains("handler failure sentinel"),
        "{binding_fault:?}"
    );
    assert_eq!(exposed_value(&exposed), 7);

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Port remains reusable after a binding fault");
    assert_eq!(execute_calls.load(Ordering::SeqCst), 2);
    let second_projection = projection_text(&projections.lock().unwrap()[1]);
    assert!(
        second_projection.contains("<state>7</state>"),
        "unexpected second projection: {second_projection}"
    );
}
