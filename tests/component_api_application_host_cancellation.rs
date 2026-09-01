#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises ApplicationHost cancellation"
)]

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            ApplicationHost, ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort,
            RenderedProjection,
        },
        prelude::*,
        ComponentHost,
    },
    llm_call::TextTurnEvent,
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use tokio::sync::Notify;

#[derive(Clone)]
struct CancellationProps {
    exposed: Arc<Mutex<Option<Signal<Vec<usize>>>>>,
    log: Arc<Mutex<Vec<String>>>,
    pending_started: Arc<Notify>,
    pending_drops: Arc<AtomicUsize>,
}

struct PendingFutureDropProbe {
    drops: Arc<AtomicUsize>,
}

impl Drop for PendingFutureDropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[component]
fn cancellation_application(
    props: CancellationProps,
    events: EventInput<ProviderEvent>,
) -> Component {
    let values = use_signal(Vec::<usize>::new);
    *props.exposed.lock().expect("Signal exposure lock") = Some(values.clone());
    let rendered = values
        .with(|values| format!("{values:?}"))
        .expect("mounted Signal read");
    let first_values = values.clone();
    let selected = events.select(ProviderEvent::TEXT);
    let pending_selected = selected.clone();
    let final_selected = selected.clone();
    let pending_log = Arc::clone(&props.log);
    let final_log = Arc::clone(&props.log);
    let pending_started = Arc::clone(&props.pending_started);
    let pending_drops = Arc::clone(&props.pending_drops);

    view! {
        state { "{rendered}" }
        {
            EventListener::observe("test.cancel.write", "v1")
                .listen_to(selected)
                .on_event(move |event| {
                    let values = first_values.clone();
                    async move {
                        let value = event_number(event);
                        values.update(|values| values.push(value)).map(|_| ())
                    }
                })
        }
        {
            EventListener::observe("test.cancel.pending", "v1")
                .listen_to(pending_selected)
                .on_event(move |event| {
                    let log = Arc::clone(&pending_log);
                    let started = Arc::clone(&pending_started);
                    let drops = Arc::clone(&pending_drops);
                    async move {
                        let value = event_number(event);
                        log.lock().unwrap().push(format!("{value}:pending:start"));
                        if value == 1 {
                            let _drop_probe = PendingFutureDropProbe { drops };
                            started.notify_one();
                            std::future::pending::<()>().await;
                        }
                        log.lock().unwrap().push(format!("{value}:pending:end"));
                        Ok::<(), Infallible>(())
                    }
                })
        }
        {
            EventListener::observe("test.cancel.after", "v1")
                .listen_to(final_selected)
                .on_event(move |event| {
                    let values = values.clone();
                    let log = Arc::clone(&final_log);
                    async move {
                        let value = event_number(event);
                        log.lock().unwrap().push(format!("{value}:after"));
                        values.update(|values| values.push(value + 100)).map(|_| ())
                    }
                })
        }
    }
}

struct ScriptedPort {
    scripts: VecDeque<Vec<usize>>,
    execute_calls: Arc<AtomicUsize>,
    projections: Arc<Mutex<Vec<RenderedProjection>>>,
}

#[async_trait]
impl ProviderPort for ScriptedPort {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        self.projections.lock().unwrap().push(projection);
        let events = self.scripts.pop_front().unwrap_or_default();
        Ok(Box::pin(futures::stream::iter(
            events.into_iter().map(|value| Ok(number_event(value))),
        )))
    }
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

fn exposed_values(exposed: &Arc<Mutex<Option<Signal<Vec<usize>>>>>) -> Vec<usize> {
    exposed
        .lock()
        .unwrap()
        .clone()
        .expect("Signal exposed")
        .with(Clone::clone)
        .unwrap()
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

#[tokio::test]
async fn cancelling_a_pending_handler_keeps_prior_signal_writes_and_reuses_the_port() {
    let exposed = Arc::new(Mutex::new(None));
    let log = Arc::new(Mutex::new(Vec::new()));
    let pending_started = Arc::new(Notify::new());
    let pending_drops = Arc::new(AtomicUsize::new(0));
    let execute_calls = Arc::new(AtomicUsize::new(0));
    let projections = Arc::new(Mutex::new(Vec::new()));
    let port = ScriptedPort {
        scripts: VecDeque::from([vec![1, 99], vec![2]]),
        execute_calls: Arc::clone(&execute_calls),
        projections: Arc::clone(&projections),
    };
    let mut components = ComponentHost::new(
        cancellation_application,
        CancellationProps {
            exposed: Arc::clone(&exposed),
            log: Arc::clone(&log),
            pending_started: Arc::clone(&pending_started),
            pending_drops: Arc::clone(&pending_drops),
        },
    );
    let mut host = ApplicationHost::new(port);

    let mut reaction = Box::pin(host.dispatch_llm_reaction(&mut components));
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = pending_started.notified() => {}
            result = reaction.as_mut() => panic!("reaction completed before its handler blocked: {result:?}"),
        }
    })
    .await
    .expect("pending handler starts");
    assert_eq!(exposed_values(&exposed), vec![1]);
    assert_eq!(*log.lock().unwrap(), ["1:pending:start"]);

    drop(reaction);
    assert_eq!(pending_drops.load(Ordering::SeqCst), 1);
    assert_eq!(*log.lock().unwrap(), ["1:pending:start"]);
    assert!(components.is_dirty());
    let committed = components
        .current_projection()
        .expect("cancellation retains the pre-reaction projection");
    assert!(projection_text(committed).contains("<state>\\[\\]</state>"));

    host.dispatch_llm_reaction(&mut components)
        .await
        .expect("Port remains reusable after handler cancellation");
    assert!(!components.is_dirty());
    assert_eq!(execute_calls.load(Ordering::SeqCst), 2);
    assert_eq!(pending_drops.load(Ordering::SeqCst), 1);
    assert_eq!(exposed_values(&exposed), vec![1, 2, 102]);
    assert_eq!(
        *log.lock().unwrap(),
        [
            "1:pending:start",
            "2:pending:start",
            "2:pending:end",
            "2:after"
        ]
    );
    let projections = projections.lock().unwrap();
    assert_eq!(projections.len(), 2);
    assert!(projection_text(&projections[1]).contains("<state>\\[1\\]</state>"));
    let committed = components
        .current_projection()
        .expect("successful retry commits its post-reconciled projection");
    assert!(projection_text(committed).contains("<state>\\[1, 2, 102\\]</state>"));
}
