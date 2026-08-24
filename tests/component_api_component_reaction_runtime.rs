use std::{
    collections::VecDeque,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc,
    },
    task::Poll,
    time::Duration,
};

use agentview::component::{
    execution::{
        ComponentReactionOutputError, ComponentReactionProps, ComponentReactionRuntime,
        ComponentReactionRuntimeFault, ProviderEvent, ProviderEventStream, ProviderFault,
        ProviderPort, RenderedProjection,
    },
    prelude::*,
};
use async_trait::async_trait;

#[derive(Clone)]
struct ReactionProps {
    prefix: String,
}

#[component]
fn reaction_component(
    props: ComponentReactionProps<ReactionProps, String>,
    events: EventInput<ProviderEvent>,
) -> Component {
    let state = use_signal(|| String::from("pending"));
    props
        .publish(state.clone())
        .expect("the current Component generation may publish its Signal");

    let prefix = props.value().prefix.clone();
    let writer = state.clone();
    let rendered = state
        .with(Clone::clone)
        .expect("the mounted Signal is readable while rendering");

    view! {
        request { "{prefix}{rendered}" }
        {
            EventListener::observe("test.component-reaction-output", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let prefix = prefix.clone();
                    let writer = writer.clone();
                    async move {
                        let value = match event {
                            TextTurnEvent::TextDelta(value)
                            | TextTurnEvent::TextComplete(value) => value,
                        };
                        writer.set(format!("{prefix}{value}"))
                    }
                })
        }
    }
}

struct ScriptedProvider {
    replies: VecDeque<String>,
    executions: Arc<AtomicUsize>,
}

struct SetupFailureProvider {
    executions: Arc<AtomicUsize>,
}

struct EventScriptProvider {
    scripts: VecDeque<Vec<Result<ProviderEvent, ProviderFault>>>,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderPort for EventScriptProvider {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let script = self.scripts.pop_front().expect("one script per reaction");
        Ok(Box::pin(futures::stream::iter(script)))
    }
}

struct PendingProvider {
    executions: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderPort for PendingProvider {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let polls = Arc::clone(&self.polls);
        Ok(Box::pin(futures::stream::poll_fn(move |_| {
            polls.fetch_add(1, Ordering::SeqCst);
            Poll::Pending
        })))
    }
}

#[async_trait]
impl ProviderPort for SetupFailureProvider {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Err(ProviderFault::retryable_transport("setup failure sentinel"))
    }
}

#[component]
fn render_failure_component(
    props: ComponentReactionProps<ReactionProps, String>,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let state = use_signal(|| String::from("uncommitted"));
    props
        .publish(state)
        .expect("current generation publication");
    panic!("render failure sentinel")
}

#[derive(Clone)]
struct ForeignSourceProps {
    sender: SyncSender<Signal<String>>,
}

#[component]
fn foreign_source_component(
    props: ComponentReactionProps<ForeignSourceProps, String>,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let state = use_signal(|| String::from("foreign"));
    props
        .value()
        .sender
        .send(state.clone())
        .expect("foreign Signal receiver remains open");
    props.publish(state.clone()).unwrap();
    let value = state.with(Clone::clone).unwrap();
    view! { source { "{value}" } }
}

#[derive(Clone)]
struct ForeignTargetProps {
    foreign: Signal<String>,
}

#[component]
fn foreign_target_component(
    props: ComponentReactionProps<ForeignTargetProps, String>,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let _owned = use_signal(|| String::from("owned"));
    props.publish(props.value().foreign.clone()).unwrap();
    view! { target { "must reject foreign state" } }
}

#[component]
fn replacing_output_component(
    props: ComponentReactionProps<(), String>,
    events: EventInput<ProviderEvent>,
) -> Component {
    let primary = use_signal(|| String::from("primary"));
    let secondary = use_signal(|| String::from("secondary"));
    props.publish(primary.clone()).unwrap();
    let publisher = props.clone();
    let secondary_writer = secondary.clone();

    view! {
        selection { "output selection" }
        {
            EventListener::observe("test.replace-component-output", "v1")
                .listen_to(events.select(ProviderEvent::TEXT))
                .on_event(move |event| {
                    let publisher = publisher.clone();
                    let secondary = secondary_writer.clone();
                    async move {
                        let value = match event {
                            TextTurnEvent::TextDelta(value)
                            | TextTurnEvent::TextComplete(value) => value,
                        };
                        secondary
                            .set(value)
                            .map_err(ComponentReactionOutputError::Signal)?;
                        publisher.publish(secondary)
                    }
                })
        }
    }
}

#[derive(Debug)]
struct BindingFailure;

impl fmt::Display for BindingFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("binding failure sentinel")
    }
}

#[component]
fn binding_failure_component(
    props: ComponentReactionProps<(), String>,
    events: EventInput<ProviderEvent>,
) -> Component {
    let state = use_signal(|| String::from("pending"));
    props.publish(state.clone()).unwrap();
    let writer = state.clone();
    let first = events.select(ProviderEvent::TEXT);
    let second = first.clone();

    view! {
        {
            EventListener::observe("test.partial-output-write", "v1")
                .listen_to(first)
                .on_event(move |_| {
                    let writer = writer.clone();
                    async move { writer.set(String::from("partial")) }
                })
        }
        {
            EventListener::observe("test.partial-output-failure", "v1")
                .listen_to(second)
                .on_event(|_| async { Err::<(), _>(BindingFailure) })
        }
    }
}

struct DestructionProbe {
    prior: Option<agentview::component::execution::ComponentReactionOutput<DestructionProbe>>,
    drop_blocked: Arc<AtomicBool>,
    drop_checks: Arc<AtomicUsize>,
}

impl Drop for DestructionProbe {
    fn drop(&mut self) {
        let Some(prior) = self.prior.clone() else {
            return;
        };
        self.drop_checks.fetch_add(1, Ordering::SeqCst);
        let (started_tx, started_rx) = sync_channel(0);
        let (done_tx, done_rx) = sync_channel(0);
        std::thread::spawn(move || {
            let _ = started_tx.send(());
            let _ = prior.with(|_| ());
            let _ = done_tx.send(());
        });
        started_rx.recv().unwrap();
        if done_rx.recv_timeout(Duration::from_millis(250)).is_err() {
            self.drop_blocked.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Clone)]
struct DestructionProps {
    prior: Option<agentview::component::execution::ComponentReactionOutput<DestructionProbe>>,
    drop_blocked: Arc<AtomicBool>,
    drop_checks: Arc<AtomicUsize>,
    fail_render: bool,
}

#[component]
fn destruction_component(
    props: ComponentReactionProps<DestructionProps, DestructionProbe>,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let prior = props.value().prior.clone();
    let drop_blocked = Arc::clone(&props.value().drop_blocked);
    let drop_checks = Arc::clone(&props.value().drop_checks);
    let state = use_signal(move || DestructionProbe {
        prior,
        drop_blocked,
        drop_checks,
    });
    props.publish(state).unwrap();
    assert!(!props.value().fail_render, "destruction render sentinel");
    view! { destruction_probe { "lock retirement probe" } }
}

fn text_event(value: &str) -> ProviderEvent {
    ProviderEvent::Text(TextTurnEvent::TextComplete(value.to_owned()))
}

fn event_provider(
    scripts: impl IntoIterator<Item = Vec<Result<ProviderEvent, ProviderFault>>>,
    executions: &Arc<AtomicUsize>,
) -> EventScriptProvider {
    EventScriptProvider {
        scripts: scripts.into_iter().collect(),
        executions: Arc::clone(executions),
    }
}

#[async_trait]
impl ProviderPort for ScriptedProvider {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let reply = self.replies.pop_front().expect("one reply per reaction");
        Ok(Box::pin(futures::stream::iter([Ok(ProviderEvent::Text(
            TextTurnEvent::TextComplete(reply),
        ))])))
    }
}

fn props(prefix: &str) -> ReactionProps {
    ReactionProps {
        prefix: prefix.to_owned(),
    }
}

#[tokio::test]
async fn runtime_owns_one_host_pair_and_returns_only_current_generation_output() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider {
        replies: VecDeque::from([String::from("A"), String::from("B")]),
        executions: Arc::clone(&executions),
    };
    let mut runtime = ComponentReactionRuntime::new(provider, reaction_component, props("first:"));
    let component_host_id = runtime.component_host_id();

    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));

    let first = runtime
        .dispatch_llm_reaction()
        .await
        .expect("the first reaction returns its typed output");
    assert_eq!(first.cloned().unwrap(), "first:A");
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    assert_eq!(
        runtime.remount(props("second:")).unwrap(),
        component_host_id,
        "remount must preserve the ComponentHost identity"
    );
    assert_eq!(runtime.component_host_id(), component_host_id);
    assert!(matches!(
        first.cloned(),
        Err(ComponentReactionOutputError::StaleGeneration { .. })
    ));
    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::StaleGeneration { .. })
    ));

    let second = runtime
        .dispatch_llm_reaction()
        .await
        .expect("the remounted reaction returns fresh typed output");
    assert_eq!(second.cloned().unwrap(), "second:B");
    assert_ne!(first.generation(), second.generation());
    assert_eq!(runtime.component_host_id(), component_host_id);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "each runtime call must execute the Provider exactly once"
    );
}

#[tokio::test]
async fn retained_props_update_starts_a_new_reaction_without_remounting_the_component_host() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider {
        replies: VecDeque::from([String::from("A"), String::from("B")]),
        executions: Arc::clone(&executions),
    };
    let mut runtime = ComponentReactionRuntime::new(provider, reaction_component, props("first:"));
    let component_host_id = runtime.component_host_id();

    let first = runtime.dispatch_llm_reaction().await.unwrap();
    assert_eq!(first.cloned().unwrap(), "first:A");

    assert_eq!(
        runtime.set_props(props("second:")).unwrap(),
        component_host_id
    );
    assert!(matches!(
        first.cloned(),
        Err(ComponentReactionOutputError::StaleGeneration { .. })
    ));

    let second = runtime.dispatch_llm_reaction().await.unwrap();
    assert_eq!(second.cloned().unwrap(), "second:B");
    assert_ne!(first.generation(), second.generation());
    assert_eq!(runtime.component_host_id(), component_host_id);
    assert_eq!(executions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn provider_failure_does_not_expose_render_time_publication() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = SetupFailureProvider {
        executions: Arc::clone(&executions),
    };
    let mut runtime = ComponentReactionRuntime::new(provider, reaction_component, props("failed:"));

    assert!(runtime.dispatch_llm_reaction().await.is_err());
    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_render_discards_its_uncommitted_publication() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider {
        replies: VecDeque::new(),
        executions: Arc::clone(&executions),
    };
    let mut runtime =
        ComponentReactionRuntime::new(provider, render_failure_component, props("failed:"));

    assert!(runtime.dispatch_llm_reaction().await.is_err());
    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "a failed render must not reach the Provider"
    );
}

#[tokio::test]
async fn foreign_host_signal_cannot_become_authoritative_output() {
    let source_executions = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = sync_channel(1);
    let mut source = ComponentReactionRuntime::new(
        event_provider([Vec::new()], &source_executions),
        foreign_source_component,
        ForeignSourceProps { sender },
    );
    source.dispatch_llm_reaction().await.unwrap();
    let foreign = receiver.recv().unwrap();

    let target_executions = Arc::new(AtomicUsize::new(0));
    let mut target = ComponentReactionRuntime::new(
        event_provider([Vec::new()], &target_executions),
        foreign_target_component,
        ForeignTargetProps { foreign },
    );

    assert!(matches!(
        target.dispatch_llm_reaction().await,
        Err(ComponentReactionRuntimeFault::Output(
            ComponentReactionOutputError::ForeignSignal { .. }
        ))
    ));
    assert!(matches!(
        target.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));
    assert_eq!(target_executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_mount_generation_allows_exactly_one_reaction_attempt() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = event_provider(
        [vec![Ok(text_event("secondary-current"))], Vec::new()],
        &executions,
    );
    let mut runtime = ComponentReactionRuntime::new(provider, replacing_output_component, ());

    let first = runtime.dispatch_llm_reaction().await.unwrap();
    assert_eq!(first.cloned().unwrap(), "secondary-current");

    let fault = match runtime.dispatch_llm_reaction().await {
        Ok(_) => panic!("a second reaction in one mount must fail"),
        Err(fault) => fault,
    };
    assert!(matches!(
        fault,
        ComponentReactionRuntimeFault::ReactionAlreadyDispatched { .. }
    ));
    assert_eq!(first.cloned().unwrap(), "secondary-current");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_failure_discards_partially_updated_output() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = event_provider(
        [
            vec![
                Ok(text_event("partial")),
                Err(ProviderFault::retryable_transport(
                    "stream failure sentinel",
                )),
            ],
            Vec::new(),
        ],
        &executions,
    );
    let mut runtime = ComponentReactionRuntime::new(provider, reaction_component, props("stream:"));

    assert!(runtime.dispatch_llm_reaction().await.is_err());
    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    let repeat = runtime.dispatch_llm_reaction().await;
    assert!(matches!(
        repeat,
        Err(ComponentReactionRuntimeFault::ReactionAlreadyDispatched { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    runtime.remount(props("recovered:")).unwrap();
    let recovered = runtime.dispatch_llm_reaction().await.unwrap();
    assert_eq!(recovered.cloned().unwrap(), "pending");
    assert_eq!(executions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn binding_failure_discards_partially_updated_output() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = event_provider([vec![Ok(text_event("partial"))]], &executions);
    let mut runtime = ComponentReactionRuntime::new(provider, binding_failure_component, ());

    assert!(runtime.dispatch_llm_reaction().await.is_err());
    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_reaction_discards_pending_output() {
    let executions = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let provider = PendingProvider {
        executions: Arc::clone(&executions),
        polls: Arc::clone(&polls),
    };
    let mut runtime =
        ComponentReactionRuntime::new(provider, reaction_component, props("cancelled:"));

    let mut reaction = Box::pin(runtime.dispatch_llm_reaction());
    assert!(matches!(futures::poll!(reaction.as_mut()), Poll::Pending));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(polls.load(Ordering::SeqCst) >= 1);
    drop(reaction);

    assert!(matches!(
        runtime.current_output(),
        Err(ComponentReactionOutputError::Absent { .. })
    ));
    let repeat = runtime.dispatch_llm_reaction().await;
    assert!(matches!(
        repeat,
        Err(ComponentReactionRuntimeFault::ReactionAlreadyDispatched { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn output_destruction_runs_after_the_publication_lock_is_released() {
    let executions = Arc::new(AtomicUsize::new(0));
    let drop_blocked = Arc::new(AtomicBool::new(false));
    let drop_checks = Arc::new(AtomicUsize::new(0));
    let initial = DestructionProps {
        prior: None,
        drop_blocked: Arc::clone(&drop_blocked),
        drop_checks: Arc::clone(&drop_checks),
        fail_render: false,
    };
    let mut runtime = ComponentReactionRuntime::new(
        event_provider([Vec::new()], &executions),
        destruction_component,
        initial,
    );
    let first = runtime.dispatch_llm_reaction().await.unwrap();

    runtime
        .remount(DestructionProps {
            prior: Some(first),
            drop_blocked: Arc::clone(&drop_blocked),
            drop_checks: Arc::clone(&drop_checks),
            fail_render: true,
        })
        .unwrap();
    assert!(runtime.dispatch_llm_reaction().await.is_err());

    assert_eq!(drop_checks.load(Ordering::SeqCst), 1);
    assert!(
        !drop_blocked.load(Ordering::SeqCst),
        "caller-defined Output::drop ran while the publication state was locked"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}
