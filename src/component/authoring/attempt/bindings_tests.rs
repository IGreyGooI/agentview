use std::{
    convert::Infallible,
    fmt,
    future::{ready, Ready},
    sync::{Arc, Mutex},
};

use crate::{component::prelude::*, component::signal::SignalRuntime, llm_call::TextTurnEvent};

use super::{ComponentAttemptFault, ComponentRenderStage, RenderBindings};

#[derive(ComponentEvents)]
enum TestEvent {
    Tick(u32),
    Text(TextTurnEvent),
}

type TestRoot = fn(Arc<Mutex<Vec<String>>>, EventInput<TestEvent>) -> Component;

fn mount(
    root: TestRoot,
    log: Arc<Mutex<Vec<String>>>,
) -> (SignalRuntime, RenderBindings<TestEvent>) {
    let signals = SignalRuntime::new();
    let bindings = render_with_signals(root, log, &signals);
    (signals, bindings)
}

fn render_with_signals(
    root: TestRoot,
    log: Arc<Mutex<Vec<String>>>,
    signals: &SignalRuntime,
) -> RenderBindings<TestEvent> {
    let input = EventInput::new(1);
    let origin = input.origin();
    ComponentRenderStage::prepare_complete_root_with_signals(root(log, input), origin, signals)
        .expect("render staging succeeds")
        .0
        .into_bindings()
}

#[component]
fn ordered_handlers(log: Arc<Mutex<Vec<String>>>, events: EventInput<TestEvent>) -> Component {
    let first_log = Arc::clone(&log);
    let second_log = Arc::clone(&log);
    let ticks = events.select(TestEvent::TICK);
    view! {
        {
            EventListener::observe("test.order.first", "v1")
                .listen_to(ticks.clone())
                .on_event(move |tick| {
                    let log = Arc::clone(&first_log);
                    async move {
                        log.lock().unwrap().push(format!("first:{tick}"));
                        tokio::task::yield_now().await;
                        log.lock().unwrap().push(String::from("first:done"));
                        Ok::<(), Infallible>(())
                    }
                })
        }
        {
            EventListener::observe("test.order.second", "v1")
                .listen_to(ticks)
                .on_event(move |tick| {
                    let log = Arc::clone(&second_log);
                    async move {
                        log.lock().unwrap().push(format!("second:{tick}"));
                        Ok::<(), Infallible>(())
                    }
                })
        }
    }
}

#[tokio::test]
async fn dispatch_awaits_matching_handlers_in_structural_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(ordered_handlers, Arc::clone(&log));

    bindings.dispatch(TestEvent::Tick(7)).await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["first:7", "first:done", "second:7"]);
}

#[component]
fn signal_handler(log: Arc<Mutex<Vec<String>>>, events: EventInput<TestEvent>) -> Component {
    let render_count = log.lock().unwrap().len();
    log.lock().unwrap().push(format!("render:{render_count}"));
    let value = use_signal(|| 0_u32);
    let observed = value.clone();
    let handler_value = value.clone();
    let handler_log = Arc::clone(&log);
    let rendered = observed.with(|value| *value).unwrap();
    view! {
        signal_value { "{rendered}" }
        {
            EventListener::observe("test.signal", "v1")
                .listen_to(events.select(TestEvent::TICK))
                .on_event(move |next| {
                    let value = handler_value.clone();
                    let log = Arc::clone(&handler_log);
                    async move {
                        tokio::task::yield_now().await;
                        value.set(next)?;
                        log.lock().unwrap().push(format!("set:{next}"));
                        Ok::<(), SignalAccessError>(())
                    }
                })
        }
    }
}

#[tokio::test]
async fn async_signal_work_does_not_rerender_during_dispatch() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(signal_handler, Arc::clone(&log));

    bindings.dispatch(TestEvent::Tick(9)).await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["render:0", "set:9"]);
}

#[component]
fn streaming_handlers(log: Arc<Mutex<Vec<String>>>, events: EventInput<TestEvent>) -> Component {
    let decoded_log = Arc::clone(&log);
    let invalid_log = Arc::clone(&log);
    let finish_log = Arc::clone(&log);
    XmlStreamingToolCall::contract("test.streaming", "v1")
        .empty_element("choice")
        .required_attribute::<u32>("value")
        .exactly_one()
        .listen_to(events.select(TestEvent::TEXT))
        .on_decoded(move |value| {
            let log = Arc::clone(&decoded_log);
            async move {
                tokio::task::yield_now().await;
                log.lock().unwrap().push(format!("decoded:{value}"));
                Ok::<(), Infallible>(())
            }
        })
        .on_invalid(move |diagnostic| {
            let log = Arc::clone(&invalid_log);
            async move {
                log.lock().unwrap().push(format!("invalid:{diagnostic:?}"));
                Ok::<(), Infallible>(())
            }
        })
        .on_finish(move || async move {
            tokio::task::yield_now().await;
            finish_log.lock().unwrap().push(String::from("finish"));
            Ok::<(), Infallible>(())
        })
}

#[tokio::test]
async fn normal_eof_finishes_parser_then_awaits_terminal_handler() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(streaming_handlers, Arc::clone(&log));

    bindings
        .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::from(
            "<choice value=\"42\" />",
        ))))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["decoded:42", "finish"]);
}

#[tokio::test]
async fn normal_eof_dispatches_parser_diagnostic_before_terminal_handler() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(streaming_handlers, Arc::clone(&log));

    bindings
        .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::new())))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    assert!(log[0].starts_with("invalid:OccurrenceCount"), "{log:?}");
    assert_eq!(log[1], "finish");
}

#[derive(Debug)]
struct HandlerError(&'static str);

impl fmt::Display for HandlerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[component]
fn invocation_panic_handler(
    _log: Arc<Mutex<Vec<String>>>,
    events: EventInput<TestEvent>,
) -> Component {
    EventListener::observe("test.panic.invocation", "v1")
        .listen_to(events.select(TestEvent::TICK))
        .on_event(|_| -> Ready<Result<(), HandlerError>> { panic!("invocation panic sentinel") })
}

#[component]
fn future_panic_handler(_log: Arc<Mutex<Vec<String>>>, events: EventInput<TestEvent>) -> Component {
    EventListener::observe("test.panic.future", "v1")
        .listen_to(events.select(TestEvent::TICK))
        .on_event(|_| async {
            panic!("future panic sentinel");
            #[allow(unreachable_code)]
            Ok::<(), HandlerError>(())
        })
}

#[component]
fn returned_error_handler(
    _log: Arc<Mutex<Vec<String>>>,
    events: EventInput<TestEvent>,
) -> Component {
    EventListener::observe("test.returned.error", "v1")
        .listen_to(events.select(TestEvent::TICK))
        .on_event(|_| ready(Err::<(), _>(HandlerError("returned error sentinel"))))
}

#[component]
fn signal_write_before_later_failure(
    log: Arc<Mutex<Vec<String>>>,
    events: EventInput<TestEvent>,
) -> Component {
    let value = use_signal(|| 0_u32);
    let rendered = value.with(|value| *value).unwrap();
    log.lock().unwrap().push(format!("rendered:{rendered}"));

    let writer_value = value.clone();
    let ticks = events.select(TestEvent::TICK);
    view! {
        {
            EventListener::observe("test.signal.write", "v1")
                .listen_to(ticks.clone())
                .on_event(move |next| {
                    let value = writer_value.clone();
                    async move {
                        value.set(next)?;
                        Ok::<(), SignalAccessError>(())
                    }
                })
        }
        {
            EventListener::observe("test.signal.later-failure", "v1")
                .listen_to(ticks)
                .on_event(|next| {
                    if next == 2 {
                        panic!("later invocation panic sentinel");
                    }
                    ready(Err::<(), _>(HandlerError("later returned error sentinel")))
                })
        }
    }
}

#[tokio::test]
async fn successful_signal_write_survives_a_later_handler_failure() {
    for (next, expected) in [
        (1, "later returned error sentinel"),
        (2, "later invocation panic sentinel"),
    ] {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (signals, mut bindings) = mount(signal_write_before_later_failure, Arc::clone(&log));

        let fault = bindings.dispatch(TestEvent::Tick(next)).await.unwrap_err();
        assert!(fault.to_string().contains(expected), "{fault:?}");
        assert!(matches!(
            bindings.dispatch(TestEvent::Tick(99)).await,
            Err(ComponentAttemptFault::AttemptInactive)
        ));
        drop(bindings);

        let rerendered = render_with_signals(
            signal_write_before_later_failure,
            Arc::clone(&log),
            &signals,
        );
        drop(rerendered);
        let expected_render = format!("rendered:{next}");
        assert_eq!(
            log.lock().unwrap().last().map(String::as_str),
            Some(expected_render.as_str())
        );
    }
}

#[component]
fn terminal_error_handler(
    _log: Arc<Mutex<Vec<String>>>,
    events: EventInput<TestEvent>,
) -> Component {
    XmlStreamingToolCall::contract("test.terminal.error", "v1")
        .empty_element("choice")
        .required_attribute::<u32>("value")
        .exactly_one()
        .listen_to(events.select(TestEvent::TEXT))
        .on_decoded(|_| ready(Ok::<(), HandlerError>(())))
        .on_invalid(|_| ready(Ok::<(), HandlerError>(())))
        .on_finish(|| ready(Err::<(), _>(HandlerError("terminal error sentinel"))))
}

#[component]
fn terminal_invocation_panic_handler(
    _log: Arc<Mutex<Vec<String>>>,
    events: EventInput<TestEvent>,
) -> Component {
    XmlStreamingToolCall::contract("test.terminal.invocation-panic", "v1")
        .empty_element("choice")
        .required_attribute::<u32>("value")
        .exactly_one()
        .listen_to(events.select(TestEvent::TEXT))
        .on_decoded(|_| ready(Ok::<(), HandlerError>(())))
        .on_invalid(|_| ready(Ok::<(), HandlerError>(())))
        .on_finish(|| -> Ready<Result<(), HandlerError>> {
            panic!("terminal invocation panic sentinel")
        })
}

#[component]
fn terminal_future_panic_handler(
    _log: Arc<Mutex<Vec<String>>>,
    events: EventInput<TestEvent>,
) -> Component {
    XmlStreamingToolCall::contract("test.terminal.future-panic", "v1")
        .empty_element("choice")
        .required_attribute::<u32>("value")
        .exactly_one()
        .listen_to(events.select(TestEvent::TEXT))
        .on_decoded(|_| ready(Ok::<(), HandlerError>(())))
        .on_invalid(|_| ready(Ok::<(), HandlerError>(())))
        .on_finish(|| async {
            panic!("terminal future panic sentinel");
            #[allow(unreachable_code)]
            Ok::<(), HandlerError>(())
        })
}

#[component]
fn terminal_drop_probe(log: Arc<Mutex<Vec<String>>>, events: EventInput<TestEvent>) -> Component {
    let terminal_log = Arc::clone(&log);
    view! {
        {
            EventListener::observe("test.drop.fault", "v1")
                .listen_to(events.select(TestEvent::TICK))
                .on_event(|_| ready(Err::<(), _>(HandlerError("drop fault sentinel"))))
        }
        {
            XmlStreamingToolCall::contract("test.drop.terminal", "v1")
                .empty_element("choice")
                .required_attribute::<u32>("value")
                .exactly_one()
                .listen_to(events.select(TestEvent::TEXT))
                .on_decoded(|_| ready(Ok::<(), HandlerError>(())))
                .on_invalid(|_| ready(Ok::<(), HandlerError>(())))
                .on_finish(move || {
                    terminal_log.lock().unwrap().push(String::from("finish"));
                    ready(Ok::<(), HandlerError>(()))
                })
        }
    }
}

#[tokio::test]
async fn callback_panics_and_errors_fail_the_generation_closed() {
    for (root, expected) in [
        (
            invocation_panic_handler
                as fn(Arc<Mutex<Vec<String>>>, EventInput<TestEvent>) -> Component,
            "invocation panic sentinel",
        ),
        (future_panic_handler, "future panic sentinel"),
        (returned_error_handler, "returned error sentinel"),
    ] {
        let (_signals, mut bindings) = mount(root, Arc::new(Mutex::new(Vec::new())));
        let fault = bindings.dispatch(TestEvent::Tick(1)).await.unwrap_err();
        assert!(fault.to_string().contains(expected), "{fault:?}");
        assert!(matches!(
            bindings.dispatch(TestEvent::Tick(2)).await,
            Err(ComponentAttemptFault::AttemptInactive)
        ));
    }
}

#[tokio::test]
async fn terminal_handler_error_fails_the_generation_closed() {
    let (_signals, mut bindings) = mount(terminal_error_handler, Arc::new(Mutex::new(Vec::new())));
    bindings
        .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::from(
            "<choice value=\"1\" />",
        ))))
        .await
        .unwrap();

    let fault = bindings.finish_normal().await.unwrap_err();
    assert!(fault.to_string().contains("terminal error sentinel"));
    assert!(matches!(
        bindings.dispatch(TestEvent::Tick(2)).await,
        Err(ComponentAttemptFault::AttemptInactive)
    ));
}

#[tokio::test]
async fn terminal_invocation_and_future_panics_fail_the_generation_closed() {
    for (root, expected) in [
        (
            terminal_invocation_panic_handler
                as fn(Arc<Mutex<Vec<String>>>, EventInput<TestEvent>) -> Component,
            "terminal invocation panic sentinel",
        ),
        (
            terminal_future_panic_handler,
            "terminal future panic sentinel",
        ),
    ] {
        let (_signals, mut bindings) = mount(root, Arc::new(Mutex::new(Vec::new())));
        bindings
            .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::from(
                "<choice value=\"1\" />",
            ))))
            .await
            .unwrap();

        let fault = bindings.finish_normal().await.unwrap_err();
        assert!(fault.to_string().contains(expected), "{fault:?}");
        assert!(matches!(
            bindings.dispatch(TestEvent::Tick(2)).await,
            Err(ComponentAttemptFault::AttemptInactive)
        ));
    }
}

#[tokio::test]
async fn dropping_open_or_faulted_bindings_does_not_invoke_terminal_handlers() {
    let normal_log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut normal) = mount(terminal_drop_probe, Arc::clone(&normal_log));
    normal
        .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::from(
            "<choice value=\"1\" />",
        ))))
        .await
        .unwrap();
    normal.finish_normal().await.unwrap();
    assert_eq!(*normal_log.lock().unwrap(), ["finish"]);

    let superseded_log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, superseded) = mount(terminal_drop_probe, Arc::clone(&superseded_log));
    drop(superseded);
    assert!(superseded_log.lock().unwrap().is_empty());

    let faulted_log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut faulted) = mount(terminal_drop_probe, Arc::clone(&faulted_log));
    let fault = faulted.dispatch(TestEvent::Tick(1)).await.unwrap_err();
    assert!(
        fault.to_string().contains("drop fault sentinel"),
        "{fault:?}"
    );
    drop(faulted);
    assert!(faulted_log.lock().unwrap().is_empty());
}
