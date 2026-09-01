#![cfg(feature = "legacy-provider-port")]

//! Code-shape guards for the retained Component and minimal Provider boundary.
//!
//! Behavioral details belong in focused runtime tests. These checks prevent
//! removed Harness/checkpoint/RecordLog concepts from returning through a
//! compatibility facade.

const TARGET: &str = include_str!("target_api/minimal_component.rs");
const PROVIDER_PORT: &str = include_str!("../src/component/execution/port.rs");
const APPLICATION_HOST: &str = include_str!("../src/component/execution/application_host.rs");
const EXTERNAL_PROVIDER: &str = include_str!("../src/component/execution/external.rs");
const COMPONENT_HOST: &str = include_str!("../src/component/host.rs");
const SIGNAL: &str = include_str!("../src/component/signal.rs");

fn executable_definition_count(source: &str, signature: &str) -> usize {
    source
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("//") && line.contains(signature)
        })
        .count()
}

#[test]
fn target_executes_one_domain_neutral_application_root() {
    assert_eq!(
        executable_definition_count(TARGET, "fn application_root("),
        1
    );
    for expected in [
        "#[component]",
        "view! {",
        "#[system_once]",
        "#[diff(slot = \"request\")]",
        "events.select(ProviderEvent::TEXT)",
        "EventListener::observe(\"target.response\", \"v1\")",
        "ComponentHost::new(",
        "ApplicationHost::new(provider)",
        "dispatch_llm_reaction(&mut components)",
    ] {
        assert!(
            TARGET.contains(expected),
            "domain-neutral target must retain {expected}"
        );
    }

    for removed in ["ApplicationHarness", "checkpoint", "RecordLog"] {
        assert!(
            !TARGET.contains(removed),
            "domain-neutral target must not contain {removed}"
        );
    }
}

#[test]
fn signal_is_the_only_retained_state_handle() {
    assert!(SIGNAL.contains("pub struct Signal<T>"));
    assert!(!SIGNAL.contains("SignalSetter"));
    assert!(!SIGNAL.contains("pub fn setter("));
}

#[test]
fn provider_port_has_one_full_projection_method() {
    let provider_trait = PROVIDER_PORT
        .split_once("pub trait ProviderPort: Send {")
        .expect("ProviderPort must exist")
        .1
        .split_once("\n}")
        .expect("ProviderPort trait must close")
        .0;
    let methods = provider_trait
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("async fn ")
                .or_else(|| line.trim_start().strip_prefix("fn "))
                .and_then(|signature| signature.split(['<', '(']).next())
        })
        .collect::<Vec<_>>();

    assert_eq!(methods, ["execute"]);
    assert!(provider_trait.contains("projection: RenderedProjection"));
    assert!(provider_trait.contains("ProviderEventStream<'a>"));
    assert!(!provider_trait.contains("Events"));
    for removed in [
        "fn act",
        "type Action",
        "ProviderOperation",
        "ProviderFrame",
    ] {
        assert!(!provider_trait.contains(removed));
    }
}

#[test]
fn external_protocol_stream_stays_below_the_public_wrapper() {
    assert!(EXTERNAL_PROVIDER.contains("pub async fn act("));
    assert!(EXTERNAL_PROVIDER.contains("act: ExternalAct"));
    assert!(EXTERNAL_PROVIDER.contains("type ExternalTextProtocolStream ="));
    assert!(!EXTERNAL_PROVIDER.contains("pub async fn act<S>"));
    assert!(!EXTERNAL_PROVIDER.contains("pub type ExternalTextProtocolStream"));
    assert!(!EXTERNAL_PROVIDER.contains("pub fn from_text_protocol"));
    assert!(!EXTERNAL_PROVIDER.contains("pub fn from_json_lines"));
    assert!(EXTERNAL_PROVIDER.contains("#[doc(hidden)]\n    pub fn __from_cli_json_lines"));

    let provider_impl = EXTERNAL_PROVIDER
        .split_once("impl ReactionPort for ExternalProviderPort {")
        .expect("ExternalProviderPort must implement ReactionPort")
        .1
        .split_once("\n}")
        .expect("ExternalProviderPort implementation must close")
        .0;
    assert!(provider_impl.contains("async fn submit<'a>("));
    assert!(provider_impl.contains("frame: Frame"));
    assert!(provider_impl.contains("reserve_owned()"));
    assert!(!provider_impl.contains("render_projection_prompt"));
    assert!(!provider_impl.contains("fn observe"));
    assert!(!provider_impl.contains("fn act"));
}

#[test]
fn application_host_owns_provider_binding_and_observer_state() {
    let host_state = APPLICATION_HOST
        .split_once("pub struct ApplicationHost<P> {")
        .expect("ApplicationHost must exist")
        .1
        .split_once("\n}")
        .expect("ApplicationHost fields must close")
        .0;
    let field_names = host_state
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_suffix(',')
                .and_then(|field| field.split_once(':'))
                .map(|(name, _)| name)
        })
        .collect::<Vec<_>>();

    assert_eq!(field_names, ["provider", "bound_host", "observer"]);
    assert!(APPLICATION_HOST.contains("pub async fn dispatch_llm_reaction<Props>("));
    assert!(!APPLICATION_HOST.contains("pub async fn observe<Props>("));
    assert!(APPLICATION_HOST.contains("pub fn with_observer("));
    assert!(APPLICATION_HOST.contains("ReactionLifecycle::new(&mut self.observer)"));
    assert!(!COMPONENT_HOST.contains("observer"));
    assert!(
        APPLICATION_HOST.contains("let prepared = match prepare_component_reaction(components)")
    );
    assert!(APPLICATION_HOST.contains("lifecycle.terminal(\"render\", \"component_render\")"));
    assert!(APPLICATION_HOST.contains(".execute(projection)"));
    assert!(APPLICATION_HOST.contains("await_bindings_with_lanes("));
    assert!(APPLICATION_HOST.contains("bindings.dispatch(ProviderEvent::Text(event))"));
    assert!(APPLICATION_HOST.contains("bindings.finish_normal()"));

    for removed in [
        "baseline",
        "RecordLog",
        "checkpoint",
        "accept_checkpoint",
        "ApplicationSnapshot",
        "ProviderOperation",
    ] {
        assert!(
            !APPLICATION_HOST.contains(removed),
            "ApplicationHost must not restore {removed}"
        );
    }
}

#[test]
fn prepared_render_transfers_scoped_projection_and_local_bindings() {
    assert!(COMPONENT_HOST.contains("let committed = self.begin_managed_render()?;"));
    assert!(COMPONENT_HOST.contains("self.publish_managed_render(committed)"));
    assert!(COMPONENT_HOST
        .contains("ComponentRenderStage::prepare_complete_root_candidate_with_capabilities("));
    assert!(COMPONENT_HOST.contains(".with_execution_scope(ProjectionExecutionScope {"));
    assert!(COMPONENT_HOST.contains("candidate.stage_mut().set_projection(projection.clone());"));
    assert!(COMPONENT_HOST.contains("let (stage, _, mounts) = candidate.commit_deferred();"));
    assert!(COMPONENT_HOST.contains("let (bindings, task_starts) = stage.into_execution_parts();"));
    assert!(COMPONENT_HOST.contains("pub(crate) struct CommittedRenderTransition"));
    assert!(COMPONENT_HOST.contains("pub(crate) fn retired_mounts(&self)"));
    assert!(COMPONENT_HOST.contains("mounts.activate();"));
    assert!(COMPONENT_HOST.contains("bindings: RenderBindings<ProviderEvent>"));
    assert!(COMPONENT_HOST.contains("pub(crate) fn into_execution_parts("));
    assert!(COMPONENT_HOST.contains("(RenderedProjection, RenderBindings<ProviderEvent>)"));
    assert!(COMPONENT_HOST.contains("(self.projection, self.bindings)"));
    assert!(APPLICATION_HOST.contains("fn prepare_component_reaction<Props>("));
    assert!(
        APPLICATION_HOST.contains("let (projection, bindings) = prepared.into_execution_parts();")
    );
    assert!(APPLICATION_HOST.contains("async fn dispatch_prepared_reaction<P>("));
    assert!(!COMPONENT_HOST.contains("ComponentHost<Props, Events>"));

    for removed in ["RecordLog", "baseline", "ProviderContext", "checkpoint"] {
        assert!(!COMPONENT_HOST.contains(removed));
    }
}
