use std::{
    convert::Infallible,
    future::ready,
    panic::{catch_unwind, AssertUnwindSafe},
};

use agentview::component::{prelude::*, ComponentHost};

#[derive(Clone)]
struct DriftProps {
    alternate: bool,
}

#[component]
fn public_hook_kind_drift(props: DriftProps) -> Component {
    if props.alternate {
        use_provider_event_handler(ProviderEvent::TEXT, |_event| {
            ready(Ok::<(), Infallible>(()))
        });
        let _signal = use_signal(|| 2_u64);
    } else {
        let _signal = use_signal(|| 1_u64);
        use_provider_event_handler(ProviderEvent::TEXT, |_event| {
            ready(Ok::<(), Infallible>(()))
        });
    }

    view! { hook_topology { "stable" } }
}

#[component]
fn system_future() -> Component {
    use_future(|| async {});
    view! { system_future { "invalid" } }
}

#[component]
fn system_future_root(_props: ()) -> Component {
    view! {
        #[system_once]
        { system_future() }
    }
}

#[component]
fn system_coroutine() -> Component {
    let _service: Coroutine<()> = use_coroutine(1, |_inbox| async {});
    view! { system_coroutine { "invalid" } }
}

#[component]
fn system_coroutine_root(_props: ()) -> Component {
    view! {
        #[system_once]
        { system_coroutine() }
    }
}

#[test]
fn public_mixed_hook_kind_drift_panics_without_publishing_candidate() {
    let mut host = ComponentHost::new_root(public_hook_kind_drift, DriftProps { alternate: false });
    host.render().expect("initial topology commits");
    let committed = host.current_projection().unwrap().clone();

    host.set_props(DriftProps { alternate: true });
    let panic = catch_unwind(AssertUnwindSafe(|| host.render()));
    assert!(panic.is_err());
    assert_eq!(host.current_projection(), Some(&committed));
}

#[test]
fn future_inside_system_scope_panics() {
    let mut host = ComponentHost::new_root(system_future_root, ());
    let panic = catch_unwind(AssertUnwindSafe(|| host.render()));
    assert!(panic.is_err());
}

#[test]
fn coroutine_inside_system_scope_panics() {
    let mut host = ComponentHost::new_root(system_coroutine_root, ());
    let panic = catch_unwind(AssertUnwindSafe(|| host.render()));
    assert!(panic.is_err());
}

#[test]
fn spawn_without_component_task_context_returns_a_typed_error() {
    assert_eq!(spawn(async {}), Err(SpawnError::ContextUnavailable));
}
