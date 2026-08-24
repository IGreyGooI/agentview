use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};

use crate::component::{prelude::*, ComponentHost};
use futures::StreamExt;
use tokio::sync::Notify;

use super::{
    super::{ApplicationHostFault, ProviderEvent, ProviderFault},
    external_provider_events, ExternalAct, ExternalApplication, ExternalApplicationFault,
    ExternalObservationKind, ExternalReaction, ExternalReactionState,
    MAX_EXTERNAL_PROTOCOL_WIRE_BYTES, MAX_EXTERNAL_TEXT_BYTES,
};

#[derive(Clone)]
struct ExternalProps {
    values: Arc<Mutex<Vec<String>>>,
    log: Arc<Mutex<Vec<String>>>,
    renders: Arc<AtomicUsize>,
    pending_drops: Arc<AtomicUsize>,
    first_started: Arc<Notify>,
    first_release: Arc<Notify>,
    block_next_render: Arc<AtomicBool>,
    render_started: Arc<Notify>,
    render_release: Arc<(Mutex<bool>, Condvar)>,
}

impl ExternalProps {
    fn new() -> Self {
        Self {
            values: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(Mutex::new(Vec::new())),
            renders: Arc::new(AtomicUsize::new(0)),
            pending_drops: Arc::new(AtomicUsize::new(0)),
            first_started: Arc::new(Notify::new()),
            first_release: Arc::new(Notify::new()),
            block_next_render: Arc::new(AtomicBool::new(false)),
            render_started: Arc::new(Notify::new()),
            render_release: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }
}

struct PendingHandlerDrop {
    drops: Arc<AtomicUsize>,
}

impl Drop for PendingHandlerDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[component]
fn external_application(props: ExternalProps, events: EventInput<ProviderEvent>) -> Component {
    props.renders.fetch_add(1, Ordering::SeqCst);
    if props.block_next_render.swap(false, Ordering::SeqCst) {
        props.render_started.notify_one();
        let (released, release) = &*props.render_release;
        let mut released = released.lock().unwrap();
        while !*released {
            released = release.wait(released).unwrap();
        }
    }
    let rendered_values = format!("{:?}", *props.values.lock().unwrap());
    let first_log = Arc::clone(&props.log);
    let first_values = Arc::clone(&props.values);
    let pending_drops = Arc::clone(&props.pending_drops);
    let first_started = Arc::clone(&props.first_started);
    let first_release = Arc::clone(&props.first_release);
    let second_log = Arc::clone(&props.log);
    let text = events.select(ProviderEvent::TEXT);
    let second_text = text.clone();

    view! {
        #[system_once]
        protocol { "External protocol." }
        state { "{rendered_values}" }
        {
            EventListener::observe("test.external.first", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let log = Arc::clone(&first_log);
                    let values = Arc::clone(&first_values);
                    let pending_drops = Arc::clone(&pending_drops);
                    let started = Arc::clone(&first_started);
                    let release = Arc::clone(&first_release);
                    async move {
                        let (kind, value) = event_parts(event);
                        log.lock()
                            .unwrap()
                            .push(format!("{kind}:{value}:first:start"));
                        let _pending_drop = PendingHandlerDrop {
                            drops: pending_drops,
                        };
                        if value == "blocked" {
                            started.notify_one();
                            release.notified().await;
                        }
                        values.lock().unwrap().push(value.clone());
                        log.lock()
                            .unwrap()
                            .push(format!("{kind}:{value}:first:end"));
                        Ok::<(), std::convert::Infallible>(())
                    }
                })
        }
        {
            EventListener::observe("test.external.second", "v1")
                .listen_to(second_text)
                .on_event(move |event| {
                    let log = Arc::clone(&second_log);
                    async move {
                        let (kind, value) = event_parts(event);
                        log.lock()
                            .unwrap()
                            .push(format!("{kind}:{value}:second"));
                        Ok::<(), std::convert::Infallible>(())
                    }
                })
        }
    }
}

fn event_parts(event: TextTurnEvent) -> (&'static str, String) {
    match event {
        TextTurnEvent::TextDelta(text) => ("delta", text),
        TextTurnEvent::TextComplete(text) => ("complete", text),
    }
}

fn text_protocol(events: Vec<TextTurnEvent>) -> ExternalAct {
    ExternalAct::from_text_protocol(futures::stream::iter(events.into_iter().map(Ok)))
}

fn completed_text(text: impl Into<String>) -> ExternalAct {
    text_protocol(vec![TextTurnEvent::TextComplete(text.into())])
}

fn json_lines_protocol(lines: &[&str]) -> ExternalAct {
    ExternalAct::__from_cli_json_lines(lines.join("\n"))
}

fn wrapper(props: &ExternalProps) -> ExternalApplication<ExternalProps> {
    ExternalApplication::new(ComponentHost::new(external_application, props.clone()))
}

#[derive(Clone)]
struct TerminalProps {
    renders: Arc<AtomicUsize>,
    finishes: Arc<AtomicUsize>,
}

#[component]
fn terminal_application(props: TerminalProps, events: EventInput<ProviderEvent>) -> Component {
    props.renders.fetch_add(1, Ordering::SeqCst);
    let finishes = Arc::clone(&props.finishes);

    view! {
        terminal_protocol { "Return one value element." }
        {
            XmlStreamingToolCall::contract("test.external.terminal", "v1")
                .empty_element("value")
                .required_attribute::<usize>("number")
                .exactly_one()
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_decoded(|_| async { Ok::<(), std::convert::Infallible>(()) })
                .on_invalid(|_| async { Ok::<(), std::convert::Infallible>(()) })
                .on_finish(move || {
                    let finishes = Arc::clone(&finishes);
                    async move {
                        finishes.fetch_add(1, Ordering::SeqCst);
                        Ok::<(), std::convert::Infallible>(())
                    }
                })
        }
    }
}

#[tokio::test]
async fn reaction_task_failure_is_consumed_before_reporting_owner_loss() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let owner = external.owner.take().unwrap();
    let (cancellation, _cancelled) = tokio::sync::oneshot::channel();
    external.reaction = Some(ExternalReaction {
        state: ExternalReactionState::AwaitingObservation,
        cancellation: Some(cancellation),
        task: tokio::spawn(async move {
            let _owner = owner;
            panic!("external reaction task panic")
        }),
    });

    let first = external.observe().await.unwrap_err();
    assert!(matches!(
        first,
        ExternalApplicationFault::ReactionTaskFailed { .. }
    ));

    let second = external.observe().await.unwrap_err();
    assert!(matches!(second, ExternalApplicationFault::OwnerUnavailable));
}

#[tokio::test]
async fn observe_starts_one_reaction_and_returns_its_rendered_prompt() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);

    let observation = external.observe().await.unwrap();

    assert_eq!(props.renders.load(Ordering::SeqCst), 1);
    assert_eq!(observation.kind(), ExternalObservationKind::Full);
    assert_eq!(observation.base_generation(), None);
    let protocol = observation.content().find("External protocol.").unwrap();
    let state = observation.content().find("<state>").unwrap();
    assert!(protocol < state, "projection order must survive rendering");
    assert!(props.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn observe_rolls_over_the_current_reaction_before_returning_the_next_observation() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);

    let first = external.observe().await.unwrap();
    let second = external.observe().await.unwrap();

    assert_eq!(props.renders.load(Ordering::SeqCst), 2);
    assert_eq!(first.kind(), ExternalObservationKind::Full);
    assert_eq!(second.kind(), ExternalObservationKind::Delta);
    assert_eq!(second.base_generation(), Some(first.generation()));
    assert_ne!(second.generation(), first.generation());
    assert!(props.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn act_dispatches_the_current_output_then_observes_the_updated_application() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let first = external.observe().await.unwrap();

    let next = external.act(completed_text("one")).await.unwrap();

    assert_eq!(props.renders.load(Ordering::SeqCst), 2);
    assert_eq!(next.kind(), ExternalObservationKind::Delta);
    assert_eq!(next.base_generation(), Some(first.generation()));
    assert!(next.content().contains("one"));
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "complete:one:first:start",
            "complete:one:first:end",
            "complete:one:second"
        ]
    );
}

#[tokio::test]
async fn consecutive_act_calls_each_target_the_wrapper_current_reaction() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let after_one = external.act(completed_text("one")).await.unwrap();
    let after_two = external.act(completed_text("two")).await.unwrap();

    assert!(after_one.content().contains("one"));
    assert!(!after_one.content().contains("two"));
    assert!(after_two.content().contains("one"));
    assert!(after_two.content().contains("two"));
    assert_eq!(after_two.base_generation(), Some(after_one.generation()));
    assert_eq!(props.renders.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn one_act_dispatches_ordered_deltas_and_stops_after_text_complete() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let observed_polls = Arc::clone(&polls);
    let mut events = std::collections::VecDeque::from([
        Ok(TextTurnEvent::TextDelta(String::from("a"))),
        Ok(TextTurnEvent::TextDelta(String::from("b"))),
        Ok(TextTurnEvent::TextComplete(String::from("ab"))),
        Ok(TextTurnEvent::TextDelta(String::from("must-not-poll"))),
    ]);
    let protocol = futures::stream::poll_fn(move |_| {
        observed_polls.fetch_add(1, Ordering::SeqCst);
        std::task::Poll::Ready(events.pop_front())
    });

    let next = external
        .act(ExternalAct::from_text_protocol(protocol))
        .await
        .unwrap();

    assert!(next.content().contains("ab"));
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:a:first:start",
            "delta:a:first:end",
            "delta:a:second",
            "delta:b:first:start",
            "delta:b:first:end",
            "delta:b:second",
            "complete:ab:first:start",
            "complete:ab:first:end",
            "complete:ab:second",
        ]
    );
}

#[tokio::test]
async fn input_eof_ends_one_act_after_all_ordered_deltas() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let next = external
        .act(text_protocol(vec![
            TextTurnEvent::TextDelta(String::from("left")),
            TextTurnEvent::TextDelta(String::from("right")),
        ]))
        .await
        .unwrap();

    assert!(next.content().contains("left"));
    assert!(next.content().contains("right"));
    assert_eq!(props.renders.load(Ordering::SeqCst), 2);
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:left:first:start",
            "delta:left:first:end",
            "delta:left:second",
            "delta:right:first:start",
            "delta:right:first:end",
            "delta:right:second",
            "complete:leftright:first:start",
            "complete:leftright:first:end",
            "complete:leftright:second",
        ]
    );
}

#[tokio::test]
async fn abnormal_protocol_disconnect_remains_a_stream_error() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let fault = external
        .act(ExternalAct::from_text_protocol(futures::stream::iter([
            Ok(TextTurnEvent::TextDelta(String::from("partial"))),
            Err(ProviderFault::retryable_transport(
                "external disconnect sentinel",
            )),
        ])))
        .await
        .unwrap_err();

    let ExternalApplicationFault::Application(ApplicationHostFault::ProviderExecution(fault)) =
        fault
    else {
        panic!("unexpected external stream fault: {fault:?}");
    };
    assert_eq!(fault.message(), "external disconnect sentinel");
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:partial:first:start",
            "delta:partial:first:end",
            "delta:partial:second"
        ]
    );
    assert_eq!(props.renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn production_json_lines_adapter_preserves_order_and_complete_cutoff() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let next = external
        .act(json_lines_protocol(&[
            r#"{"type":"text_delta","text":"left"}"#,
            r#"{"type":"text_delta","text":"right"}"#,
            r#"{"type":"text_complete","text":"leftright"}"#,
            r#"{"type":"disconnect"}"#,
        ]))
        .await
        .unwrap();

    assert!(next.content().contains("leftright"));
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:left:first:start",
            "delta:left:first:end",
            "delta:left:second",
            "delta:right:first:start",
            "delta:right:first:end",
            "delta:right:second",
            "complete:leftright:first:start",
            "complete:leftright:first:end",
            "complete:leftright:second",
        ]
    );
}

#[tokio::test]
async fn production_json_lines_adapter_uses_input_eof_for_completion_synthesis() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    external
        .act(json_lines_protocol(&[
            r#"{"type":"text_delta","text":"left"}"#,
            r#"{"type":"text_delta","text":"right"}"#,
        ]))
        .await
        .unwrap();

    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:left:first:start",
            "delta:left:first:end",
            "delta:left:second",
            "delta:right:first:start",
            "delta:right:first:end",
            "delta:right:second",
            "complete:leftright:first:start",
            "complete:leftright:first:end",
            "complete:leftright:second",
        ]
    );
}

#[tokio::test]
async fn production_json_lines_adapter_keeps_disconnect_abnormal() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let fault = external
        .act(json_lines_protocol(&[
            r#"{"type":"text_delta","text":"partial"}"#,
            r#"{"type":"disconnect"}"#,
        ]))
        .await
        .unwrap_err();

    let ExternalApplicationFault::Application(ApplicationHostFault::ProviderExecution(fault)) =
        fault
    else {
        panic!("unexpected external stream fault: {fault:?}");
    };
    assert_eq!(
        fault.message(),
        "external text protocol disconnected abnormally"
    );
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:partial:first:start",
            "delta:partial:first:end",
            "delta:partial:second",
        ]
    );
}

#[tokio::test]
async fn production_json_lines_adapter_rejects_malformed_frames_without_echoing_input() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let fault = external
        .act(json_lines_protocol(&[
            r#"{"type":"unknown","secret":"do-not-echo"}"#,
        ]))
        .await
        .unwrap_err();

    let ExternalApplicationFault::Application(ApplicationHostFault::ProviderExecution(fault)) =
        fault
    else {
        panic!("unexpected external stream fault: {fault:?}");
    };
    assert_eq!(fault.message(), "external text protocol frame was invalid");
    assert!(!fault.message().contains("do-not-echo"));
    assert!(props.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn production_json_lines_adapter_bounds_wire_before_frame_decoding() {
    let protocol = " ".repeat(MAX_EXTERNAL_PROTOCOL_WIRE_BYTES + 1);
    let mut events = external_provider_events(ExternalAct::__from_cli_json_lines(protocol));

    let fault = events.next().await.unwrap().unwrap_err();

    assert_eq!(
        fault.message(),
        "external text protocol exceeded configured wire limit"
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn production_json_lines_adapter_bounds_protocol_frame_count() {
    const MAX_PROTOCOL_FRAMES: usize = 65_536;
    let frame = r#"{"type":"text_delta","text":""}"#;
    let protocol = std::iter::repeat_n(frame, MAX_PROTOCOL_FRAMES + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let mut events = external_provider_events(ExternalAct::__from_cli_json_lines(protocol));

    let fault = events.next().await.unwrap().unwrap_err();

    assert_eq!(
        fault.message(),
        "external text protocol exceeded configured frame limit"
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn production_json_lines_adapter_rejects_excess_empty_lines_before_decoding() {
    const MAX_PROTOCOL_FRAMES: usize = 65_536;
    let protocol = "\n".repeat(MAX_PROTOCOL_FRAMES + 1);
    let mut events = external_provider_events(ExternalAct::__from_cli_json_lines(protocol));

    let fault = events.next().await.unwrap().unwrap_err();

    assert_eq!(
        fault.message(),
        "external text protocol exceeded configured frame limit"
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn cumulative_external_text_is_bounded_below_the_port() {
    let first = "a".repeat(MAX_EXTERNAL_TEXT_BYTES / 2 + 1);
    let second = "b".repeat(MAX_EXTERNAL_TEXT_BYTES / 2 + 1);
    let mut events = external_provider_events(text_protocol(vec![
        TextTurnEvent::TextDelta(first.clone()),
        TextTurnEvent::TextDelta(second),
    ]));

    assert_eq!(
        events.next().await.unwrap().unwrap(),
        ProviderEvent::Text(TextTurnEvent::TextDelta(first))
    );
    let fault = events.next().await.unwrap().unwrap_err();
    assert_eq!(
        fault.message(),
        "external output text exceeded configured output limit"
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn explicit_external_completion_is_bounded_below_the_port() {
    let complete = "x".repeat(MAX_EXTERNAL_TEXT_BYTES + 1);
    let mut events =
        external_provider_events(text_protocol(vec![TextTurnEvent::TextComplete(complete)]));

    let fault = events.next().await.unwrap().unwrap_err();
    assert_eq!(
        fault.message(),
        "external output text exceeded configured output limit"
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn full_re_render_reuses_the_current_generation_and_pending_reaction() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    let first = external.observe().await.unwrap();

    let full = external.full_re_render().unwrap();

    assert_eq!(full.kind(), ExternalObservationKind::Full);
    assert_eq!(full.generation(), first.generation());
    assert_eq!(full.base_generation(), None);
    assert_eq!(full.content(), first.content());
    assert_eq!(props.renders.load(Ordering::SeqCst), 1);
    assert!(props.log.lock().unwrap().is_empty());

    let next = external.act(completed_text("same-reaction")).await.unwrap();
    assert_eq!(next.base_generation(), Some(full.generation()));
    assert_eq!(props.renders.load(Ordering::SeqCst), 2);
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "complete:same-reaction:first:start",
            "complete:same-reaction:first:end",
            "complete:same-reaction:second"
        ]
    );
}

#[tokio::test]
async fn full_re_render_rebuilds_the_baseline_after_a_delta() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();
    let delta = external.act(completed_text("one")).await.unwrap();
    assert_eq!(delta.kind(), ExternalObservationKind::Delta);

    let full = external.full_re_render().unwrap();

    assert_eq!(full.kind(), ExternalObservationKind::Full);
    assert_eq!(full.generation(), delta.generation());
    assert_eq!(full.base_generation(), None);
    assert!(full.content().contains("External protocol."));
    assert!(full.content().contains("one"));
    assert_eq!(props.renders.load(Ordering::SeqCst), 2);

    let next = external.act(completed_text("two")).await.unwrap();
    assert_eq!(next.base_generation(), Some(full.generation()));
    assert_eq!(props.renders.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn full_re_render_does_not_run_terminal_handlers() {
    let renders = Arc::new(AtomicUsize::new(0));
    let finishes = Arc::new(AtomicUsize::new(0));
    let mut external = ExternalApplication::new(ComponentHost::new(
        terminal_application,
        TerminalProps {
            renders: Arc::clone(&renders),
            finishes: Arc::clone(&finishes),
        },
    ));
    let first = external.observe().await.unwrap();

    let full = external.full_re_render().unwrap();

    assert_eq!(full.generation(), first.generation());
    assert_eq!(renders.load(Ordering::SeqCst), 1);
    assert_eq!(finishes.load(Ordering::SeqCst), 0);

    let complete = String::from(r#"<value number="1" />"#);
    external
        .act(text_protocol(vec![TextTurnEvent::TextDelta(complete)]))
        .await
        .unwrap();
    assert_eq!(renders.load(Ordering::SeqCst), 2);
    assert_eq!(finishes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn act_waits_for_normal_serial_handler_dispatch_before_rendering_the_next_observation() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let mut act = Box::pin(external.act(completed_text("blocked")));
    tokio::select! {
        () = props.first_started.notified() => {}
        result = act.as_mut() => panic!("act completed before its first handler blocked: {result:?}"),
    }
    assert_eq!(*props.log.lock().unwrap(), ["complete:blocked:first:start"]);

    props.first_release.notify_one();
    let next = act.await.unwrap();
    assert!(next.content().contains("blocked"));
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "complete:blocked:first:start",
            "complete:blocked:first:end",
            "complete:blocked:second"
        ]
    );
}

#[tokio::test]
async fn act_without_a_current_external_reaction_fails_closed() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);

    assert!(matches!(
        external.act(completed_text("orphaned")).await,
        Err(ExternalApplicationFault::NoActiveReaction)
    ));
    assert_eq!(props.renders.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelling_act_cancels_pending_dispatch_and_preserves_the_external_owner() {
    let props = ExternalProps::new();
    let mut external = wrapper(&props);
    external.observe().await.unwrap();

    let mut act = Box::pin(external.act(text_protocol(vec![
        TextTurnEvent::TextDelta(String::from("retained")),
        TextTurnEvent::TextComplete(String::from("blocked")),
    ])));
    tokio::select! {
        () = props.first_started.notified() => {}
        result = act.as_mut() => panic!("act completed before its handler blocked: {result:?}"),
    }
    let completed_handler_drops = props.pending_drops.load(Ordering::SeqCst);

    drop(act);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while props.pending_drops.load(Ordering::SeqCst) == completed_handler_drops {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelling act must drop the pending handler future");

    let recovered = external.observe().await.unwrap();
    assert!(recovered.content().contains("retained"));
    assert!(!recovered.content().contains("blocked"));
    assert_eq!(
        *props.log.lock().unwrap(),
        [
            "delta:retained:first:start",
            "delta:retained:first:end",
            "delta:retained:second",
            "complete:blocked:first:start",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_observe_while_starting_a_reaction_preserves_the_external_owner() {
    let props = ExternalProps::new();
    props.block_next_render.store(true, Ordering::SeqCst);
    let mut external = wrapper(&props);

    let mut observe = Box::pin(external.observe());
    tokio::select! {
        () = props.render_started.notified() => {}
        result = observe.as_mut() => panic!("observe completed before its render blocked: {result:?}"),
    }
    drop(observe);
    let (released, release) = &*props.render_release;
    *released.lock().unwrap() = true;
    release.notify_all();

    let recovered = tokio::time::timeout(std::time::Duration::from_secs(1), external.observe())
        .await
        .expect("a later observe must recover the external owner")
        .unwrap();
    assert!(recovered.content().contains("External protocol."));
    assert_eq!(props.renders.load(Ordering::SeqCst), 2);

    let after_act = external.act(completed_text("after-cancel")).await.unwrap();
    assert!(after_act.content().contains("after-cancel"));
}
