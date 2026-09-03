#![allow(
    deprecated,
    reason = "these crate-internal binding tests retain ComponentEvents compatibility coverage"
)]

use std::{
    convert::Infallible,
    fmt,
    future::{ready, Ready},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::FutureExt;

use agentview_derive::ComponentEvents;

use crate::{
    component::{
        authoring::{InternalEventInput as EventInput, InternalEventListener as EventListener},
        prelude::*,
        signal::SignalRuntime,
    },
    llm_call::TextTurnEvent,
};

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
    let input = EventInput::<TestEvent>::new(1);
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
fn ordered_provider_handlers(log: Arc<Mutex<Vec<String>>>) -> Component {
    let first_log = Arc::clone(&log);
    let second_log = Arc::clone(&log);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let log = Arc::clone(&first_log);
        async move {
            let TextTurnEvent::TextDelta(text) = event else {
                return Ok::<(), Infallible>(());
            };
            log.lock().unwrap().push(format!("first:{text}"));
            tokio::task::yield_now().await;
            log.lock().unwrap().push(String::from("first:done"));
            Ok::<(), Infallible>(())
        }
    });
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let log = Arc::clone(&second_log);
        async move {
            let TextTurnEvent::TextDelta(text) = event else {
                return Ok::<(), Infallible>(());
            };
            log.lock().unwrap().push(format!("second:{text}"));
            Ok::<(), Infallible>(())
        }
    });
    view! {}
}

fn render_provider_handlers(
    component: Component,
    signals: &SignalRuntime,
) -> RenderBindings<ProviderEvent> {
    let input = EventInput::<ProviderEvent>::new(1);
    ComponentRenderStage::prepare_complete_root_with_signals(component, input.origin(), signals)
        .expect("provider handler render succeeds")
        .0
        .into_bindings()
}

#[tokio::test]
async fn provider_hook_handlers_are_awaited_in_hook_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(ordered_provider_handlers(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "value",
        ))))
        .await
        .unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["first:value", "first:done", "second:value"]
    );
}

#[component]
fn normal_reaction_completion(log: Arc<Mutex<Vec<String>>>) -> Component {
    use_reaction_completion(move || {
        log.lock().unwrap().push(String::from("reaction:complete"));
        ready(Ok::<(), Infallible>(()))
    });
    view! {}
}

#[tokio::test]
async fn normal_finish_invokes_reaction_completion_once() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(normal_reaction_completion(Arc::clone(&log)), &signals);

    bindings.finish_normal().await.unwrap();
    assert!(matches!(
        bindings.finish_normal().await,
        Err(ComponentAttemptFault::AfterStreamFinish)
    ));

    assert_eq!(*log.lock().unwrap(), ["reaction:complete"]);
}

#[component]
fn empty_stream_reaction_completion(log: Arc<Mutex<Vec<String>>>) -> Component {
    let completion_log = Arc::clone(&log);
    use_reaction_completion(move || {
        completion_log
            .lock()
            .unwrap()
            .push(String::from("reaction:complete"));
        ready(Ok::<(), Infallible>(()))
    });
    XmlStreamingToolCall::contract("test.empty-reaction", "v1")
        .empty_element("choice")
        .on_decoded(|| ready(Ok::<(), Infallible>(())))
        .on_invalid(move |diagnostic| {
            log.lock().unwrap().push(format!("invalid:{diagnostic:?}"));
            ready(Ok::<(), Infallible>(()))
        })
}

#[tokio::test]
async fn empty_stream_without_text_complete_reaches_reaction_completion() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(empty_stream_reaction_completion(Arc::clone(&log)), &signals);

    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["reaction:complete"]);
}

#[component]
fn ordered_reaction_completion(log: Arc<Mutex<Vec<String>>>) -> Component {
    let raw_log = Arc::clone(&log);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let log = Arc::clone(&raw_log);
        async move {
            if matches!(event, TextTurnEvent::TextComplete(_)) {
                log.lock().unwrap().push(String::from("raw"));
            }
            Ok::<(), Infallible>(())
        }
    });

    let first_completion_log = Arc::clone(&log);
    use_reaction_completion(move || async move {
        first_completion_log
            .lock()
            .unwrap()
            .push(String::from("completion:first:start"));
        tokio::task::yield_now().await;
        first_completion_log
            .lock()
            .unwrap()
            .push(String::from("completion:first:end"));
        Ok::<(), Infallible>(())
    });
    let second_completion_log = Arc::clone(&log);
    use_reaction_completion(move || {
        second_completion_log
            .lock()
            .unwrap()
            .push(String::from("completion:second"));
        ready(Ok::<(), Infallible>(()))
    });

    let decoded_log = Arc::clone(&log);
    let invalid_log = Arc::clone(&log);
    view! {
        {
            XmlStreamingToolCall::contract("test.completion-decoded", "v1")
                .empty_element("choice")
                .on_decoded(move || {
                    decoded_log.lock().unwrap().push(String::from("decoded"));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(|_| ready(Ok::<(), Infallible>(())))
        }
        {
            XmlStreamingToolCall::contract("test.completion-eof", "v1")
                .empty_element("unfinished")
                .on_decoded(|| ready(Ok::<(), Infallible>(())))
                .on_invalid(move |_| {
                    invalid_log.lock().unwrap().push(String::from("eof:invalid"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

#[tokio::test]
async fn reaction_completion_runs_after_raw_derived_and_eof_handlers_in_hook_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(ordered_reaction_completion(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<choice /><unfinished"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "raw",
            "decoded",
            "eof:invalid",
            "completion:first:start",
            "completion:first:end",
            "completion:second",
        ]
    );
}

#[tokio::test]
async fn nonempty_delta_without_text_complete_stays_a_protocol_fault() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(empty_stream_reaction_completion(Arc::clone(&log)), &signals);
    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "partial",
        ))))
        .await
        .unwrap();

    let fault = bindings.finish_normal().await.unwrap_err();

    assert!(matches!(
        fault,
        ComponentAttemptFault::StreamingInput { message }
            if message.contains("finished without TextComplete")
    ));
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_delta_without_text_complete_stays_a_protocol_fault() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(empty_stream_reaction_completion(Arc::clone(&log)), &signals);
    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::new())))
        .await
        .unwrap();

    let fault = bindings.finish_normal().await.unwrap_err();

    assert!(matches!(
        fault,
        ComponentAttemptFault::StreamingInput { message }
            if message.contains("finished without TextComplete")
    ));
    assert!(log.lock().unwrap().is_empty());
}

#[component]
fn failing_reaction_completion(log: Arc<Mutex<Vec<String>>>) -> Component {
    use_reaction_completion(|| {
        ready(Err::<(), _>(HandlerError(
            "reaction completion returned error",
        )))
    });
    use_reaction_completion(move || {
        log.lock().unwrap().push(String::from("must-not-run"));
        ready(Ok::<(), Infallible>(()))
    });
    view! {}
}

#[tokio::test]
async fn reaction_completion_error_stops_later_callbacks_and_faults_bindings_closed() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(failing_reaction_completion(Arc::clone(&log)), &signals);

    let fault = bindings.finish_normal().await.unwrap_err();

    assert!(matches!(
        fault,
        ComponentAttemptFault::ReactionCompletion { message }
            if message.contains("reaction completion returned error")
    ));
    assert!(matches!(
        bindings.finish_normal().await,
        Err(ComponentAttemptFault::AttemptInactive)
    ));
    assert!(log.lock().unwrap().is_empty());
}

#[component]
fn provider_order_leaf(label: String, log: Arc<Mutex<Vec<String>>>) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let label = label.clone();
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(label);
            Ok::<(), Infallible>(())
        }
    });
    view! {}
}

#[component]
fn provider_order_branch(log: Arc<Mutex<Vec<String>>>) -> Component {
    let branch_log = Arc::clone(&log);
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let log = Arc::clone(&branch_log);
        async move {
            log.lock().unwrap().push(String::from("branch"));
            Ok::<(), Infallible>(())
        }
    });
    view! {
        { provider_order_leaf(String::from("nested"), log) }
    }
}

#[component]
fn provider_order_root(log: Arc<Mutex<Vec<String>>>) -> Component {
    let parent_log = Arc::clone(&log);
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let log = Arc::clone(&parent_log);
        async move {
            log.lock().unwrap().push(String::from("parent"));
            Ok::<(), Infallible>(())
        }
    });
    view! {
        { provider_order_leaf(String::from("first"), Arc::clone(&log)) }
        { provider_order_branch(Arc::clone(&log)) }
        { provider_order_leaf(String::from("last"), log) }
    }
}

#[tokio::test]
async fn provider_handlers_follow_parent_sibling_and_nested_component_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings = render_provider_handlers(provider_order_root(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("done"),
        )))
        .await
        .unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["parent", "first", "branch", "nested", "last"]
    );
}

#[component]
fn synchronous_provider_signal_write() -> Component {
    let value = use_signal(|| 0_u32);
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        value.set(1).unwrap();
        ready(Ok::<(), Infallible>(()))
    });
    view! {}
}

#[tokio::test]
async fn provider_handler_start_does_not_hold_the_mount_fence_during_user_code() {
    let signals = SignalRuntime::new();
    let mut bindings = render_provider_handlers(synchronous_provider_signal_write(), &signals);

    tokio::time::timeout(
        Duration::from_secs(1),
        bindings.dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("done"),
        ))),
    )
    .await
    .expect("synchronous callback invocation must not deadlock")
    .unwrap();

    assert!(signals.is_dirty());
}

#[component]
fn captured_provider_handler(label: String, log: Arc<Mutex<Vec<String>>>) -> Component {
    let handler_label = label.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let log = Arc::clone(&log);
        let label = handler_label.clone();
        async move {
            log.lock().unwrap().push(label);
            Ok::<(), Infallible>(())
        }
    });
    view! { capture { "{label}" } }
}

#[tokio::test]
async fn successful_rerender_publishes_the_new_provider_handler_capture() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let _first = render_provider_handlers(
        captured_provider_handler(String::from("first"), Arc::clone(&log)),
        &signals,
    );
    let mut second = render_provider_handlers(
        captured_provider_handler(String::from("second"), Arc::clone(&log)),
        &signals,
    );

    second
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("done"),
        )))
        .await
        .unwrap();

    assert_eq!(*log.lock().unwrap(), ["second"]);
}

#[component]
fn optionally_failing_descendant(fail: bool) -> Component {
    assert!(!fail, "candidate descendant failure");
    view! {}
}

#[component]
fn provider_handler_before_failing_descendant(
    label: String,
    fail: bool,
    log: Arc<Mutex<Vec<String>>>,
) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        let label = label.clone();
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(label);
            Ok::<(), Infallible>(())
        }
    });
    view! {
        { optionally_failing_descendant(fail) }
    }
}

#[tokio::test]
async fn failed_render_discards_candidate_handler_and_preserves_committed_binding() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut committed = render_provider_handlers(
        provider_handler_before_failing_descendant(
            String::from("committed"),
            false,
            Arc::clone(&log),
        ),
        &signals,
    );

    let input = EventInput::<ProviderEvent>::new(2);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
            provider_handler_before_failing_descendant(
                String::from("candidate"),
                true,
                Arc::clone(&log),
            ),
            input.origin(),
            &signals,
        )
    }));
    assert!(failed.is_err());

    committed
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("old"),
        )))
        .await
        .unwrap();
    let mut recovered = render_provider_handlers(
        provider_handler_before_failing_descendant(
            String::from("recovered"),
            false,
            Arc::clone(&log),
        ),
        &signals,
    );
    recovered
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("new"),
        )))
        .await
        .unwrap();

    assert_eq!(*log.lock().unwrap(), ["committed", "recovered"]);
}

#[derive(Clone, Copy)]
enum ProviderHandlerFailure {
    InvocationPanic,
    FuturePanic,
    ReturnedError,
}

#[component]
fn failing_provider_handler(failure: ProviderHandlerFailure) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
        if matches!(failure, ProviderHandlerFailure::InvocationPanic) {
            panic!("provider hook invocation panic");
        }
        async move {
            match failure {
                ProviderHandlerFailure::FuturePanic => panic!("provider hook future panic"),
                ProviderHandlerFailure::ReturnedError => {
                    Err(HandlerError("provider hook returned error"))
                }
                ProviderHandlerFailure::InvocationPanic => Ok(()),
            }
        }
    });
    view! {}
}

#[tokio::test]
async fn provider_hook_panics_propagate() {
    for failure in [
        ProviderHandlerFailure::InvocationPanic,
        ProviderHandlerFailure::FuturePanic,
    ] {
        let signals = SignalRuntime::new();
        let mut bindings = render_provider_handlers(failing_provider_handler(failure), &signals);
        let panic = std::panic::AssertUnwindSafe(bindings.dispatch(ProviderEvent::Text(
            TextTurnEvent::TextComplete(String::from("done")),
        )))
        .catch_unwind()
        .await;
        assert!(panic.is_err());
    }
}

#[tokio::test]
async fn provider_hook_returned_error_fails_dispatch_closed() {
    let signals = SignalRuntime::new();
    let mut bindings = render_provider_handlers(
        failing_provider_handler(ProviderHandlerFailure::ReturnedError),
        &signals,
    );
    let fault = bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("done"),
        )))
        .await
        .unwrap_err();
    assert!(matches!(
        fault,
        ComponentAttemptFault::ListenerDispatch { message }
            if message.contains("provider hook returned error")
    ));
}

#[component]
fn system_provider_handler() -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, |_event| {
        ready(Ok::<(), Infallible>(()))
    });
    view! {}
}

#[component]
fn provider_handler_in_system_scope() -> Component {
    view! {
        #[system_once]
        { system_provider_handler() }
    }
}

#[test]
fn provider_handler_is_rejected_inside_system_once() {
    let signals = SignalRuntime::new();
    let input = EventInput::<ProviderEvent>::new(1);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
            provider_handler_in_system_scope(),
            input.origin(),
            &signals,
        )
    }));

    assert!(panic.is_err());
}

#[component]
fn aliased_provider_handler_is_not_a_hook() -> Component {
    use crate::component::authoring::use_provider_event_handler as install_handler;

    install_handler(ProviderEvent::TEXT, |_event| {
        ready(Ok::<(), Infallible>(()))
    });
    view! {}
}

#[test]
fn unsupported_provider_handler_alias_panics() {
    let signals = SignalRuntime::new();
    let input = EventInput::<ProviderEvent>::new(1);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
            aliased_provider_handler_is_not_a_hook(),
            input.origin(),
            &signals,
        )
    }));

    assert!(panic.is_err());
}

#[component]
fn public_mixed_hook_order(alternate: bool) -> Component {
    if alternate {
        use_provider_event_handler(ProviderEvent::TEXT, |_event| {
            ready(Ok::<(), Infallible>(()))
        });
        let _value = use_signal(|| 1_u32);
    } else {
        let _value = use_signal(|| 2_u32);
        use_provider_event_handler(ProviderEvent::TEXT, |_event| {
            ready(Ok::<(), Infallible>(()))
        });
    }
    view! {}
}

#[test]
fn public_mixed_signal_and_handler_order_drift_panics() {
    let signals = SignalRuntime::new();
    let input = EventInput::<ProviderEvent>::new(1);
    ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
        public_mixed_hook_order(false),
        input.origin(),
        &signals,
    )
    .unwrap();

    let input = EventInput::<ProviderEvent>::new(2);
    let drifted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
            public_mixed_hook_order(true),
            input.origin(),
            &signals,
        )
    }));

    assert!(drifted.is_err());
}

#[component]
fn reaction_request_without_application() -> Component {
    let _request = use_reaction_request();
    view! {}
}

#[test]
fn reaction_request_without_orchestration_capability_panics() {
    let signals = SignalRuntime::new();
    let input = EventInput::<ProviderEvent>::new(1);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
            reaction_request_without_application(),
            input.origin(),
            &signals,
        )
    }));

    assert!(panic.is_err());
}

#[component]
fn system_reaction_request() -> Component {
    let _request = use_reaction_request();
    view! {}
}

#[component]
fn reaction_request_in_system_scope() -> Component {
    view! {
        #[system_once]
        { system_reaction_request() }
    }
}

#[test]
fn reaction_request_is_rejected_inside_system_once() {
    let signals = SignalRuntime::new();
    let input = EventInput::<ProviderEvent>::new(1);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
            reaction_request_in_system_scope(),
            input.origin(),
            &signals,
        )
    }));

    assert!(panic.is_err());
}

#[tokio::test]
async fn provider_handler_binding_is_stale_after_its_component_unmounts() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings = render_provider_handlers(
        captured_provider_handler(String::from("stale"), Arc::clone(&log)),
        &signals,
    );
    signals.invalidate_all().unwrap();

    let fault = bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("must-not-dispatch"),
        )))
        .await
        .unwrap_err();

    assert!(matches!(
        fault,
        ComponentAttemptFault::ListenerDispatch { message }
            if message.contains("stale Component mount")
    ));
    assert!(log.lock().unwrap().is_empty());
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
    XmlStreamingToolCall::contract("test.streaming", "v1")
        .empty_element("choice")
        .required_attribute::<u32>("value")
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
}

#[tokio::test]
async fn normal_eof_dispatches_the_decoded_value() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(streaming_handlers, Arc::clone(&log));

    bindings
        .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::from(
            "<choice value=\"42\" />",
        ))))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["decoded:42"]);
}

#[tokio::test]
async fn normal_eof_without_a_match_has_no_component_diagnostic() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(streaming_handlers, Arc::clone(&log));

    bindings
        .dispatch(TestEvent::Text(TextTurnEvent::TextComplete(String::new())))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert!(log.lock().unwrap().is_empty());
}

#[component]
fn native_streaming_alternatives(log: Arc<Mutex<Vec<String>>>) -> Component {
    let choice_decoded = Arc::clone(&log);
    let choice_invalid = Arc::clone(&log);
    let resign_decoded = Arc::clone(&log);
    let resign_invalid = Arc::clone(&log);

    view! {
        {
            XmlStreamingToolCall::contract("test.native.choice", "v1")
                .empty_element("choice")
                .required_attribute::<u32>("value")
                .on_decoded(move |value| {
                    choice_decoded.lock().unwrap().push(format!("choice:{value}"));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    choice_invalid
                        .lock()
                        .unwrap()
                        .push(format!("choice:invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        {
            XmlStreamingToolCall::contract("test.native.resign", "v1")
                .empty_element("resign")
                .on_decoded(move || {
                    resign_decoded
                        .lock()
                        .unwrap()
                        .push(String::from("resign"));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    resign_invalid
                        .lock()
                        .unwrap()
                        .push(format!("resign:invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

#[component]
fn native_optional_attribute_free(log: Arc<Mutex<Vec<String>>>) -> Component {
    let decoded_log = Arc::clone(&log);
    let invalid_log = Arc::clone(&log);

    XmlStreamingToolCall::contract("test.native.optional-empty", "v1")
        .empty_element("resign")
        .on_decoded(move || {
            decoded_log.lock().unwrap().push(String::from("decoded"));
            ready(Ok::<(), Infallible>(()))
        })
        .on_invalid(move |diagnostic| {
            invalid_log
                .lock()
                .unwrap()
                .push(format!("invalid:{diagnostic:?}"));
            ready(Ok::<(), Infallible>(()))
        })
}

#[component]
fn native_interleaved_streaming(log: Arc<Mutex<Vec<String>>>) -> Component {
    let a_decoded = Arc::clone(&log);
    let a_invalid = Arc::clone(&log);
    let b_decoded = Arc::clone(&log);
    let b_invalid = log;

    view! {
        {
            XmlStreamingToolCall::contract("test.native.interleaved-a", "v1")
                .empty_element("a")
                .required_attribute::<u32>("value")
                .on_decoded(move |value| {
                    let log = Arc::clone(&a_decoded);
                    async move {
                        log.lock().unwrap().push(format!("a:{value}:start"));
                        tokio::task::yield_now().await;
                        log.lock().unwrap().push(format!("a:{value}:end"));
                        Ok::<(), Infallible>(())
                    }
                })
                .on_invalid(move |diagnostic| {
                    a_invalid.lock().unwrap().push(format!("a:invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        {
            XmlStreamingToolCall::contract("test.native.interleaved-b", "v1")
                .empty_element("b")
                .on_decoded(move || {
                    b_decoded.lock().unwrap().push(String::from("b"));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    b_invalid.lock().unwrap().push(format!("b:invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

#[component]
fn native_streaming_lifecycle(log: Arc<Mutex<Vec<String>>>) -> Component {
    let open_log = Arc::clone(&log);
    let stream_log = Arc::clone(&log);
    let complete_log = Arc::clone(&log);
    let invalid_log = log;
    view! {
        {
            StreamingXml::tag("speak")
                .on_open(move |element| {
                    open_log.lock().unwrap().push(format!(
                        "open:{}:{}",
                        element.attr("mood").unwrap_or_default(),
                        element.content
                    ));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_stream(move |element| {
                    stream_log
                        .lock()
                        .unwrap()
                        .push(format!("stream:{}", element.content));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_complete(move |element| {
                    complete_log
                        .lock()
                        .unwrap()
                        .push(format!("complete:{}", element.content));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    invalid_log
                        .lock()
                        .unwrap()
                        .push(format!("invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

fn phase_logger(tag: &'static str, phase: &'static str, log: Arc<Mutex<Vec<String>>>) {
    log.lock().unwrap().push(format!("{tag}:{phase}"));
}

#[component]
fn native_empty_lifecycle_order(log: Arc<Mutex<Vec<String>>>) -> Component {
    let a_open = Arc::clone(&log);
    let a_complete = Arc::clone(&log);
    let b_open = Arc::clone(&log);
    let b_complete = log;
    view! {
        {
            StreamingXml::tag("a")
                .on_open(move |_| {
                    phase_logger("a", "open", Arc::clone(&a_open));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_complete(move |_| {
                    phase_logger("a", "complete", Arc::clone(&a_complete));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        {
            StreamingXml::tag("b")
                .on_open(move |_| {
                    phase_logger("b", "open", Arc::clone(&b_open));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_complete(move |_| {
                    phase_logger("b", "complete", Arc::clone(&b_complete));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

#[component]
fn native_unbounded_occurrences(log: Arc<Mutex<Vec<String>>>) -> Component {
    StreamingXml::tag("a")
        .on_complete(move |_| {
            log.lock().unwrap().push(String::from("a"));
            ready(Ok::<(), Infallible>(()))
        })
        .into()
}

fn invalid_prefix_subscription(
    tag: &'static str,
    label: &'static str,
    log: Arc<Mutex<Vec<String>>>,
) -> Component {
    StreamingXml::tag(tag)
        .on_invalid(move |_| {
            log.lock().unwrap().push(label.to_owned());
            ready(Ok::<(), Infallible>(()))
        })
        .into()
}

#[component]
fn native_incomplete_prefix_order(log: Arc<Mutex<Vec<String>>>, reverse: bool) -> Component {
    if reverse {
        view! {
            { invalid_prefix_subscription("speak", "speak", Arc::clone(&log)) }
            { invalid_prefix_subscription("say", "say", log) }
        }
    } else {
        view! {
            { invalid_prefix_subscription("say", "say", Arc::clone(&log)) }
            { invalid_prefix_subscription("speak", "speak", log) }
        }
    }
}

#[component]
fn native_same_tag_fanout(log: Arc<Mutex<Vec<String>>>) -> Component {
    let first = Arc::clone(&log);
    let second = log;
    view! {
        {
            StreamingXml::tag("a").on_complete(move |_| {
                let first = Arc::clone(&first);
                async move {
                    first.lock().unwrap().push(String::from("first:start"));
                    tokio::task::yield_now().await;
                    first.lock().unwrap().push(String::from("first:end"));
                    Ok::<(), Infallible>(())
                }
            })
        }
        {
            StreamingXml::tag("a").on_complete(move |_| {
                second.lock().unwrap().push(String::from("second"));
                ready(Ok::<(), Infallible>(()))
            })
        }
    }
}

#[component]
fn native_shared_tool_call_and_lifecycle(log: Arc<Mutex<Vec<String>>>) -> Component {
    let decoded = Arc::clone(&log);
    let invalid = Arc::clone(&log);
    let open = Arc::clone(&log);
    let complete = log;
    view! {
        {
            XmlStreamingToolCall::contract("test.native.shared", "v1")
                .empty_element("choice")
                .required_attribute::<u32>("value")
                .on_decoded(move |value| {
                    decoded.lock().unwrap().push(format!("decoded:{value}"));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    invalid
                        .lock()
                        .unwrap()
                        .push(format!("invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        {
            StreamingXml::tag("choice")
                .on_open(move |element| {
                    open.lock().unwrap().push(format!(
                        "open:{}",
                        element.attr("value").unwrap_or_default()
                    ));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_complete(move |element| {
                    complete
                        .lock()
                        .unwrap()
                        .push(format!("complete:{}", element.content));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

#[component]
fn native_failing_lifecycle_order(log: Arc<Mutex<Vec<String>>>) -> Component {
    let a = Arc::clone(&log);
    let b = log;
    view! {
        {
            StreamingXml::tag("a").on_complete(move |_| {
                a.lock().unwrap().push(String::from("a"));
                ready(Err::<(), _>(HandlerError("lifecycle failure sentinel")))
            })
        }
        {
            StreamingXml::tag("b").on_complete(move |_| {
                b.lock().unwrap().push(String::from("b"));
                ready(Ok::<(), Infallible>(()))
            })
        }
    }
}

#[component]
fn invalid_streaming_tag() -> Component {
    StreamingXml::tag("not valid")
        .on_complete(|_| ready(Ok::<(), Infallible>(())))
        .into()
}

#[component]
fn native_nested_lifecycle(log: Arc<Mutex<Vec<String>>>) -> Component {
    let outer_open = Arc::clone(&log);
    let outer_complete = Arc::clone(&log);
    let inner_open = Arc::clone(&log);
    let inner_complete = log;
    view! {
        {
            StreamingXml::tag("outer")
                .on_open(move |_| {
                    phase_logger("outer", "open", Arc::clone(&outer_open));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_complete(move |element| {
                    outer_complete
                        .lock()
                        .unwrap()
                        .push(format!("outer:complete:{}", element.content));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        {
            StreamingXml::tag("inner")
                .on_open(move |_| {
                    phase_logger("inner", "open", Arc::clone(&inner_open));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_complete(move |element| {
                    inner_complete
                        .lock()
                        .unwrap()
                        .push(format!("inner:complete:{}", element.content));
                    ready(Ok::<(), Infallible>(()))
                })
        }
    }
}

#[component]
fn raw_provider_log(log: Arc<Mutex<Vec<String>>>) -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, move |_| {
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(String::from("raw"));
            Ok::<(), Infallible>(())
        }
    });
    view! {}
}

#[component]
fn native_streaming_before_raw(log: Arc<Mutex<Vec<String>>>) -> Component {
    let decoded = Arc::clone(&log);
    let invalid = Arc::clone(&log);
    view! {
        {
            XmlStreamingToolCall::contract("test.native.phase", "v1")
                .empty_element("a")
                .on_decoded(move || {
                    decoded.lock().unwrap().push(String::from("xml"));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    invalid.lock().unwrap().push(format!("invalid:{diagnostic:?}"));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        { raw_provider_log(log) }
    }
}

#[tokio::test]
async fn native_streaming_declarations_share_provider_text_and_decode_no_attributes() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_alternatives(Arc::clone(&log)), &signals);
    assert_eq!(bindings.streaming_routes.len(), 1);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<resign /><resign />"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["resign", "resign"]);
}

#[tokio::test]
async fn native_streaming_dispatches_every_same_target_occurrence() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_alternatives(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<choice value=\"1\" /><choice value=\"2\" />"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["choice:1", "choice:2"]);
}

#[tokio::test]
async fn native_siblings_dispatch_in_source_order_and_await_each_handler() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_interleaved_streaming(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<a value=\"1\" /><b /><a value=\"2\" />"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["a:1:start", "a:1:end", "b", "a:2:start", "a:2:end"]
    );
}

#[tokio::test]
async fn native_streaming_lifecycle_keeps_cumulative_snapshots_across_chunks() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_lifecycle(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "<speak mood=\"calm\">hello",
        ))))
        .await
        .unwrap();
    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            " world",
        ))))
        .await
        .unwrap();
    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<speak mood=\"calm\">hello world!</speak>"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "open:calm:",
            "stream:hello",
            "stream:hello world",
            "complete:hello world!",
        ]
    );
}

#[tokio::test]
async fn native_streaming_waits_for_a_split_tag_name_and_attributes_before_opening() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_lifecycle(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "<spe",
        ))))
        .await
        .unwrap();
    assert!(log.lock().unwrap().is_empty());

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "ak mood=\"ca",
        ))))
        .await
        .unwrap();
    assert!(log.lock().unwrap().is_empty());

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "lm\">hello",
        ))))
        .await
        .unwrap();
    assert_eq!(*log.lock().unwrap(), ["open:calm:", "stream:hello"]);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<speak mood=\"calm\">hello</speak>"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["open:calm:", "stream:hello", "complete:hello"]
    );
}

#[tokio::test]
async fn native_streaming_excludes_an_incomplete_closing_fragment_from_content() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_lifecycle(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "<speak mood=\"calm\">hello",
        ))))
        .await
        .unwrap();
    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "</spe",
        ))))
        .await
        .unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["open:calm:", "stream:hello", "stream:hello"]
    );

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<speak mood=\"calm\">hello</speak>"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "open:calm:",
            "stream:hello",
            "stream:hello",
            "complete:hello",
        ]
    );
}

#[tokio::test]
async fn native_streaming_excludes_closing_tag_whitespace_from_content() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_lifecycle(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "<speak mood=\"calm\">hello",
        ))))
        .await
        .unwrap();
    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextDelta(String::from(
            "</speak ",
        ))))
        .await
        .unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["open:calm:", "stream:hello", "stream:hello"]
    );

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<speak mood=\"calm\">hello</speak >"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "open:calm:",
            "stream:hello",
            "stream:hello",
            "complete:hello",
        ]
    );
}

#[tokio::test]
async fn native_empty_lifecycle_events_follow_source_order_in_one_complete_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_empty_lifecycle_order(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<a/><b/><a/>"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "a:open",
            "a:complete",
            "b:open",
            "b:complete",
            "a:open",
            "a:complete",
        ]
    );
}

#[tokio::test]
async fn native_streaming_has_no_occurrence_cardinality_policy() {
    const OCCURRENCES: usize = 4_097;

    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_unbounded_occurrences(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            "<a/>".repeat(OCCURRENCES),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(log.lock().unwrap().len(), OCCURRENCES);
}

#[tokio::test]
async fn incomplete_closing_prefix_is_independent_of_subscription_mount_order() {
    for reverse in [false, true] {
        let log = Arc::new(Mutex::new(Vec::new()));
        let signals = SignalRuntime::new();
        let mut bindings = render_provider_handlers(
            native_incomplete_prefix_order(Arc::clone(&log), reverse),
            &signals,
        );

        bindings
            .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
                String::from("<speak>x</s"),
            )))
            .await
            .unwrap();
        bindings.finish_normal().await.unwrap();

        assert_eq!(*log.lock().unwrap(), ["speak"], "reverse={reverse}");
    }
}

#[tokio::test]
async fn native_tool_call_and_lifecycle_observer_share_the_same_tag_and_route() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings = render_provider_handlers(
        native_shared_tool_call_and_lifecycle(Arc::clone(&log)),
        &signals,
    );
    assert_eq!(bindings.streaming_routes.len(), 1);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<choice value=\"7\" />"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["open:7", "decoded:7", "complete:"]);
}

#[tokio::test]
async fn lifecycle_handler_error_stops_later_xml_occurrences() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_failing_lifecycle_order(Arc::clone(&log)), &signals);

    let fault = bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<a/><b/><a/>"),
        )))
        .await
        .unwrap_err();

    assert!(matches!(
        fault,
        ComponentAttemptFault::StreamingInput { message }
            if message.contains("lifecycle failure sentinel")
    ));
    assert_eq!(*log.lock().unwrap(), ["a"]);
}

#[tokio::test]
async fn native_same_tag_subscriptions_fan_out_in_mount_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings = render_provider_handlers(native_same_tag_fanout(Arc::clone(&log)), &signals);
    assert_eq!(bindings.streaming_routes.len(), 1);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<a/><a/>"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "first:start",
            "first:end",
            "second",
            "first:start",
            "first:end",
            "second",
        ]
    );
}

#[tokio::test]
async fn native_incomplete_lifecycle_emits_invalid_after_the_last_stream_snapshot() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_lifecycle(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<speak mood=\"calm\">unfinished"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    let log = log.lock().unwrap();
    assert_eq!(&log[..2], ["open:calm:", "stream:unfinished"]);
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(log[2].starts_with("invalid:IncompleteElement"), "{log:?}");
}

#[tokio::test]
async fn native_nested_targets_publish_phases_without_unknown_parent_consumption() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_nested_lifecycle(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<wrapper><outer>x<inner>y</inner>z</outer></wrapper>"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "outer:open",
            "inner:open",
            "inner:complete:y",
            "outer:complete:x<inner>y</inner>z",
        ]
    );
}

#[tokio::test]
async fn raw_provider_handlers_run_before_derived_xml_handlers() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_streaming_before_raw(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("<a />"),
        )))
        .await
        .unwrap();
    bindings.finish_normal().await.unwrap();

    assert_eq!(*log.lock().unwrap(), ["raw", "xml"]);
}

#[tokio::test]
async fn native_attribute_free_closing_tag_is_invalid() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_optional_attribute_free(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("</resign>"),
        )))
        .await
        .unwrap();
    {
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("invalid:MalformedElement"), "{log:?}");
    }
    bindings.finish_normal().await.unwrap();

    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("invalid:MalformedElement"), "{log:?}");
}

#[tokio::test]
async fn native_attribute_free_incomplete_closing_tag_is_invalid_at_eof() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let signals = SignalRuntime::new();
    let mut bindings =
        render_provider_handlers(native_optional_attribute_free(Arc::clone(&log)), &signals);

    bindings
        .dispatch(ProviderEvent::Text(TextTurnEvent::TextComplete(
            String::from("</resign"),
        )))
        .await
        .unwrap();
    assert!(log.lock().unwrap().is_empty());
    bindings.finish_normal().await.unwrap();

    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("invalid:IncompleteElement"), "{log:?}");
}

#[test]
fn invalid_streaming_tag_is_rejected_during_mount() {
    let signals = SignalRuntime::new();
    let input = EventInput::<ProviderEvent>::new(1);
    let result = ComponentRenderStage::<ProviderEvent>::prepare_complete_root_with_signals(
        invalid_streaming_tag(),
        input.origin(),
        &signals,
    );
    let Err(fault) = result else {
        panic!("invalid streaming XML tag unexpectedly mounted");
    };

    assert!(matches!(
        fault,
        ComponentAttemptFault::StreamingMount { message }
            if message.contains("streaming XML tag `not valid` is not a valid XML name")
    ));
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
    let log = Arc::new(Mutex::new(Vec::new()));
    let (signals, mut bindings) = mount(signal_write_before_later_failure, Arc::clone(&log));

    let fault = bindings.dispatch(TestEvent::Tick(1)).await.unwrap_err();
    assert!(fault.to_string().contains("later returned error sentinel"));
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
    assert_eq!(
        log.lock().unwrap().last().map(String::as_str),
        Some("rendered:1")
    );
}

#[tokio::test]
async fn panic_after_a_signal_write_propagates_without_runtime_recovery() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_signals, mut bindings) = mount(signal_write_before_later_failure, log);

    let panic = std::panic::AssertUnwindSafe(bindings.dispatch(TestEvent::Tick(2)))
        .catch_unwind()
        .await;

    assert!(panic.is_err());
}

#[tokio::test]
async fn callback_panics_propagate() {
    for root in [
        (invocation_panic_handler
            as fn(Arc<Mutex<Vec<String>>>, EventInput<TestEvent>) -> Component),
        future_panic_handler,
    ] {
        let (_signals, mut bindings) = mount(root, Arc::new(Mutex::new(Vec::new())));
        let panic = std::panic::AssertUnwindSafe(bindings.dispatch(TestEvent::Tick(1)))
            .catch_unwind()
            .await;
        assert!(panic.is_err());
    }
}

#[tokio::test]
async fn callback_returned_error_fails_the_generation_closed() {
    let (_signals, mut bindings) = mount(returned_error_handler, Arc::new(Mutex::new(Vec::new())));
    let fault = bindings.dispatch(TestEvent::Tick(1)).await.unwrap_err();
    assert!(fault.to_string().contains("returned error sentinel"));
    assert!(matches!(
        bindings.dispatch(TestEvent::Tick(2)).await,
        Err(ComponentAttemptFault::AttemptInactive)
    ));
}
