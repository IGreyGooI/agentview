//! Two externally driven plugins with one live OpenAI Responses parent.
//!
//! Set `OPENAI_API_KEY`, then run `cargo run --example frame_plugin`.

use std::{
    any::Any,
    collections::HashMap,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
};

use agentview::component::{
    execution::{Application, ExternalAct, ExternalApplication, ReactionPort},
    prelude::*,
};
use anyhow::{Context, Result};
use futures::FutureExt;

#[path = "support/live_provider.rs"]
mod live_provider;

#[derive(Clone)]
struct PluginProps {
    parent_id: &'static str,
}

#[component]
fn frame_plugin_component(props: PluginProps) -> Component {
    let response = use_signal(|| None::<String>);
    let received = response.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let received = received.clone();
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                received.set(Some(text))?;
            }
            Ok::<(), SignalAccessError>(())
        }
    });

    let parent_id = props.parent_id;
    let response = response
        .with(Clone::clone)
        .expect("mounted Plugin response")
        .map(|text| view! { parent_response { "{text}" } })
        .unwrap_or_else(|| view! {});
    let prompt = r#"## Plugin request

Provide one concise status update for this plugin."#;
    view! {
        #[developer]
        { prompt }

        #[developer]
        frame_plugin {
            parent { "{parent_id}" }
        }
        { response }
    }
}

struct PluginParent {
    application: ExternalApplication,
}

fn plugin_parent(parent_id: &'static str) -> Result<PluginParent> {
    let props = PluginProps { parent_id };
    let application = ExternalApplication::new_root(move || frame_plugin_component(props.clone()))?;
    Ok(PluginParent { application })
}

type PanicPayload = Box<dyn Any + Send + 'static>;
type BoundaryResult<T> = std::thread::Result<Result<T>>;

struct CleanupOutcome {
    first_error: Option<anyhow::Error>,
    first_panic: Option<PanicPayload>,
}

impl CleanupOutcome {
    fn into_boundary_result(mut self) -> BoundaryResult<()> {
        if let Some(panic) = self.first_panic.take() {
            return Err(panic);
        }
        Ok(match self.first_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        })
    }
}

async fn drain_all<Owner, Owners, Shutdown, ShutdownFuture, ShutdownError>(
    owners: Owners,
    mut shutdown: Shutdown,
) -> CleanupOutcome
where
    Owners: IntoIterator<Item = Owner>,
    Shutdown: FnMut(Owner) -> ShutdownFuture,
    ShutdownFuture: Future<Output = std::result::Result<(), ShutdownError>>,
    ShutdownError: Into<anyhow::Error>,
{
    let mut outcome = CleanupOutcome {
        first_error: None,
        first_panic: None,
    };
    for owner in owners {
        match AssertUnwindSafe(async { shutdown(owner).await })
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) if outcome.first_error.is_none() => {
                outcome.first_error = Some(error.into());
            }
            Ok(Err(_)) => {}
            Err(panic) if outcome.first_panic.is_none() => outcome.first_panic = Some(panic),
            Err(_) => {}
        }
    }
    outcome
}

async fn shutdown_parent(parent: PluginParent) -> Result<()> {
    parent
        .application
        .shutdown()
        .await
        .map_err(anyhow::Error::from)
}

async fn shutdown_registry(registry: HashMap<&'static str, PluginParent>) -> CleanupOutcome {
    drain_all(registry.into_values(), shutdown_parent).await
}

async fn build_plugin_registry_with<Create>(
    mut create: Create,
) -> Result<HashMap<&'static str, PluginParent>>
where
    Create: FnMut(&'static str) -> Result<PluginParent>,
{
    let mut registry = HashMap::new();
    for parent_id in ["parent-a", "parent-b"] {
        match catch_unwind(AssertUnwindSafe(|| create(parent_id))) {
            Ok(Ok(parent)) => {
                registry.insert(parent_id, parent);
            }
            Ok(Err(error)) => {
                let cleanup = shutdown_registry(registry).await.into_boundary_result();
                return resolve_operation_cleanup(
                    Ok(Err(error)),
                    cleanup,
                    "partial Plugin registry cleanup also failed",
                );
            }
            Err(primary_panic) => {
                let cleanup = shutdown_registry(registry).await.into_boundary_result();
                return resolve_operation_cleanup(
                    Err(primary_panic),
                    cleanup,
                    "partial Plugin registry cleanup also failed",
                );
            }
        }
    }
    Ok(registry)
}

async fn build_plugin_registry() -> Result<HashMap<&'static str, PluginParent>> {
    build_plugin_registry_with(plugin_parent).await
}

fn resolve_operation_cleanup<T>(
    operation: BoundaryResult<T>,
    cleanup: BoundaryResult<()>,
    cleanup_context: &'static str,
) -> Result<T> {
    match (operation, cleanup) {
        (Err(primary_panic), _) => resume_unwind(primary_panic),
        (Ok(_), Err(cleanup_panic)) => resume_unwind(cleanup_panic),
        (Ok(Ok(value)), Ok(Ok(()))) => Ok(value),
        (Ok(Err(operation)), Ok(Ok(()))) => Err(operation),
        (Ok(Ok(_)), Ok(Err(cleanup))) => Err(cleanup),
        (Ok(Err(operation)), Ok(Err(cleanup))) => {
            Err(operation.context(format!("{cleanup_context}: {cleanup}")))
        }
    }
}

#[derive(Clone)]
struct ParentProps {
    delegated_frames: String,
    response: Arc<Mutex<Option<String>>>,
}

#[component]
fn response_parent(props: ParentProps) -> Component {
    let response = Arc::clone(&props.response);
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let response = Arc::clone(&response);
        async move {
            if let TextTurnEvent::TextComplete(text) = event {
                *response
                    .lock()
                    .map_err(|_| anyhow::anyhow!("parent response lock poisoned"))? = Some(text);
            }
            Ok::<(), anyhow::Error>(())
        }
    });

    let exit = use_application_exit();
    use_reaction_completion(move || async move { exit.request(ExitReason::Completed) });

    let delegated_frames = props.delegated_frames;
    let prompt = r#"## Parent instructions

You are the parent agent for two independent plugins. Return one concise status update that is safe for both plugins."#;
    view! {
        #[system_once]
        { prompt }
        delegated_plugin_frames { "{delegated_frames}" }
    }
}

async fn run_parent_agent(provider: impl ReactionPort, delegated_frames: String) -> Result<String> {
    let response = Arc::new(Mutex::new(None));
    let props = ParentProps {
        delegated_frames,
        response: Arc::clone(&response),
    };
    let mut application = Application::mount(move || response_parent(props.clone()), provider)?;

    let operation = AssertUnwindSafe(async {
        application.run().await.map_err(anyhow::Error::from)?;
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    let shutdown =
        AssertUnwindSafe(async { application.shutdown().await.map_err(anyhow::Error::from) })
            .catch_unwind()
            .await;
    resolve_operation_cleanup(
        operation,
        shutdown,
        "parent Application shutdown also failed after the model operation",
    )?;

    let response = response
        .lock()
        .map_err(|_| anyhow::anyhow!("parent response lock poisoned"))?;
    let response = response.clone().context("parent model returned no text")?;
    Ok(response)
}

async fn run_plugin() -> Result<String> {
    let provider = live_provider::from_env("frame-plugin-parent")?;
    let mut registry = build_plugin_registry().await?;

    let operation = AssertUnwindSafe(async {
        let parent_a = registry
            .get_mut("parent-a")
            .context("missing parent-a")?
            .application
            .observe()
            .await?;
        let parent_b = registry
            .get_mut("parent-b")
            .context("missing parent-b")?
            .application
            .observe()
            .await?;
        let delegated_frames = format!(
            "parent-a frame:\n{}\n\nparent-b frame:\n{}",
            parent_a.content(),
            parent_b.content(),
        );
        let response = run_parent_agent(provider, delegated_frames).await?;

        for parent_id in ["parent-a", "parent-b"] {
            let _updated = registry
                .get_mut(parent_id)
                .context("missing plugin parent")?
                .application
                .act(ExternalAct::text(response.clone()))
                .await?;
        }
        Ok::<_, anyhow::Error>(response)
    })
    .catch_unwind()
    .await;
    let shutdown = shutdown_registry(registry).await;
    let response = resolve_operation_cleanup(
        operation,
        shutdown.into_boundary_result(),
        "Plugin registry cleanup also failed after the model operation",
    )?;
    Ok(response)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("{}", run_plugin().await?);
    Ok(())
}

#[cfg(test)]
#[path = "../tests/examples/frame_plugin.rs"]
mod tests;
