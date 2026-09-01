use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ExternalAct, ExternalControlFault, ExternalObservationKind,
            ExternalProviderPort, FrameBasis,
        },
        prelude::*,
    },
    llm_call::TextTurnEvent,
};

struct TaskDropProbe(Arc<AtomicBool>);

impl Drop for TaskDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct PublicExchangeProps {
    effects: Arc<Mutex<Vec<String>>>,
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
}

#[component]
fn public_external_exchange(props: PublicExchangeProps) -> Component {
    let state = use_signal(|| String::from("pending"));
    let handler_state = state.clone();
    let effects = Arc::clone(&props.effects);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let state = handler_state.clone();
        let effects = Arc::clone(&effects);
        async move {
            match event {
                TextTurnEvent::TextDelta(text) => {
                    effects.lock().unwrap().push(format!("delta:{text}"));
                }
                TextTurnEvent::TextComplete(text) => {
                    effects
                        .lock()
                        .unwrap()
                        .push(format!("complete:{text}:start"));
                    tokio::task::yield_now().await;
                    state.set(format!("handled:{text}"))?;
                    effects.lock().unwrap().push(format!("complete:{text}:end"));
                }
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    let task_started = Arc::clone(&props.task_started);
    let task_dropped = Arc::clone(&props.task_dropped);
    use_future(move || async move {
        let _drop_probe = TaskDropProbe(task_dropped);
        task_started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
    });

    let rendered = state.with(Clone::clone).expect("mounted state");
    view! { public_external_state { "{rendered}" } }
}

async fn wait_for_task_start(started: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Component task must start");
}

#[tokio::test]
async fn public_text_and_empty_completion_are_ordered_stale_fenced_and_consumingly_cleaned_up() {
    let effects = Arc::new(Mutex::new(Vec::new()));
    let task_started = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::new(AtomicBool::new(false));
    let props = PublicExchangeProps {
        effects: Arc::clone(&effects),
        task_started: Arc::clone(&task_started),
        task_dropped: Arc::clone(&task_dropped),
    };
    let (port, control) = ExternalProviderPort::new().unwrap();
    let mut application =
        Application::mount(move || public_external_exchange(props.clone()), port).unwrap();
    wait_for_task_start(&task_started).await;

    let first_ingress = {
        let first_reaction = application.react();
        tokio::pin!(first_reaction);
        let first = tokio::select! {
            observation = control.next_observation() => observation.unwrap(),
            result = &mut first_reaction => {
                panic!("text reaction ended before handoff: {result:?}")
            }
        };
        assert_eq!(first.kind(), ExternalObservationKind::Full);
        let first_ingress = first.ingress_generation();
        control
            .act(first_ingress, ExternalAct::text("approved"))
            .await
            .unwrap();
        assert_eq!(
            control
                .act(first_ingress, ExternalAct::text("repeated"))
                .await,
            Err(ExternalControlFault::StaleIngress)
        );
        first_reaction.await.unwrap();
        first_ingress
    };

    let second_ingress = {
        let second_reaction = application.react();
        tokio::pin!(second_reaction);
        let second = tokio::select! {
            observation = control.next_observation() => observation.unwrap(),
            result = &mut second_reaction => {
                panic!("empty reaction ended before handoff: {result:?}")
            }
        };
        assert_eq!(second.kind(), ExternalObservationKind::Delta);
        assert!(matches!(second.frame().basis(), FrameBasis::DeltaFrom(_)));
        assert!(second.content().contains("approved"));
        assert!(second.content().contains("handled:approved"));
        assert_eq!(
            control.act(first_ingress, ExternalAct::text("late")).await,
            Err(ExternalControlFault::StaleIngress)
        );
        let second_ingress = second.ingress_generation();
        control.complete(second_ingress).await.unwrap();
        assert_eq!(
            control.complete(second_ingress).await,
            Err(ExternalControlFault::StaleIngress)
        );
        second_reaction.await.unwrap();
        second_ingress
    };

    {
        let third_reaction = application.react();
        tokio::pin!(third_reaction);
        let third = tokio::select! {
            observation = control.next_observation() => observation.unwrap(),
            result = &mut third_reaction => {
                panic!("third reaction ended before handoff: {result:?}")
            }
        };
        assert_eq!(
            control.complete(second_ingress).await,
            Err(ExternalControlFault::StaleIngress)
        );
        control.complete(third.ingress_generation()).await.unwrap();
        third_reaction.await.unwrap();
    }

    assert_eq!(
        *effects.lock().unwrap(),
        ["complete:approved:start", "complete:approved:end"]
    );
    assert!(!task_dropped.load(Ordering::Acquire));
    application.shutdown().await.unwrap();
    assert!(task_dropped.load(Ordering::Acquire));
}
