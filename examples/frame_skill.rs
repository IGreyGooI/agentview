use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use agentview::component::{
    execution::{
        Application, ExternalControl, ExternalObservation, ExternalObservationKind,
        ExternalProviderPort,
    },
    prelude::*,
};
use anyhow::{ensure, Context, Result};
use futures::FutureExt;

#[cfg(test)]
#[path = "support/frame_workflow_golden.rs"]
mod frame_workflow_golden;

struct TaskDropProbe(Arc<AtomicBool>);

impl Drop for TaskDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct SkillProps {
    exported: Arc<Mutex<Option<Signal<String>>>>,
    renders: Arc<AtomicUsize>,
    task_started: Arc<AtomicBool>,
    task_dropped: Arc<AtomicBool>,
}

#[component]
fn frame_skill_component(props: SkillProps) -> Component {
    let state = use_signal(|| String::from("initial"));
    *props.exported.lock().expect("Skill export lock") = Some(state.clone());

    let task_started = Arc::clone(&props.task_started);
    let task_dropped = Arc::clone(&props.task_dropped);
    use_future(move || async move {
        let _drop_probe = TaskDropProbe(task_dropped);
        task_started.store(true, Ordering::Release);
        std::future::pending::<()>().await;
    });

    props.renders.fetch_add(1, Ordering::SeqCst);
    let value = state.with(Clone::clone).expect("mounted Skill state");
    view! { frame_skill { "{value}" } }
}

fn run_typed_command(signal: &Signal<String>, value: &str) -> Result<()> {
    signal.set(value.to_owned())?;
    Ok(())
}

async fn wait_for_task_start(started: &AtomicBool) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("Skill Component task did not start")?;
    Ok(())
}

async fn explicit_exchange(
    application: &mut Application<ExternalProviderPort>,
    control: &ExternalControl,
) -> Result<ExternalObservation> {
    let reaction = application.react();
    tokio::pin!(reaction);
    let observation = tokio::select! {
        observation = control.next_observation() => observation?,
        result = &mut reaction => {
            return Err(anyhow::anyhow!("Skill reaction ended before handoff: {result:?}"));
        }
    };
    control.complete(observation.ingress_generation()).await?;
    ensure!(reaction.await?.is_continue(), "Skill application exited");
    Ok(observation)
}

#[derive(Debug)]
struct SkillEvidence {
    renders_after_mount: usize,
    renders_after_latest: usize,
    renders_after_command: usize,
    latest_submitted: bool,
    command_submitted: bool,
    revision_unchanged_before_exchange: bool,
    command_left_projection_dirty: bool,
    exchange_kind: ExternalObservationKind,
    exchange_saw_updated_state: bool,
    shutdown_completed: bool,
    task_dropped: bool,
}

async fn run_skill() -> Result<SkillEvidence> {
    let exported = Arc::new(Mutex::new(None));
    let renders = Arc::new(AtomicUsize::new(0));
    let task_started = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::new(AtomicBool::new(false));
    let props = SkillProps {
        exported: Arc::clone(&exported),
        renders: Arc::clone(&renders),
        task_started: Arc::clone(&task_started),
        task_dropped: Arc::clone(&task_dropped),
    };
    let (port, control) = ExternalProviderPort::new()?;
    let mut application = Application::mount(move || frame_skill_component(props.clone()), port)?;

    let operation = async {
        wait_for_task_start(&task_started).await?;
        let renders_after_mount = renders.load(Ordering::SeqCst);
        let latest = application.current_projection();
        let initial_revision = latest.revision();
        ensure!(
            !latest.is_dirty(),
            "mounted Skill projection must be current"
        );
        let latest_submitted = control.next_observation().now_or_never().is_some();
        let renders_after_latest = renders.load(Ordering::SeqCst);

        let signal = exported
            .lock()
            .map_err(|_| anyhow::anyhow!("Skill export lock poisoned"))?
            .clone()
            .context("Skill did not export its typed state")?;
        run_typed_command(&signal, "updated")?;

        let stale = application.current_projection();
        let revision_unchanged_before_exchange = stale.revision() == initial_revision;
        let command_left_projection_dirty = stale.is_dirty();
        let command_submitted = control.next_observation().now_or_never().is_some();
        let renders_after_command = renders.load(Ordering::SeqCst);

        let observation = explicit_exchange(&mut application, &control).await?;
        let exchange_kind = observation.kind();
        let exchange_saw_updated_state = observation.content().contains("updated");

        ensure!(
            renders_after_mount == renders_after_latest
                && renders_after_latest == renders_after_command,
            "latest and typed command must not render"
        );
        ensure!(
            !latest_submitted && !command_submitted,
            "latest and typed command must not submit"
        );
        ensure!(
            revision_unchanged_before_exchange && command_left_projection_dirty,
            "typed command must leave the committed projection stale until exchange"
        );
        ensure!(
            exchange_kind == ExternalObservationKind::Full && exchange_saw_updated_state,
            "explicit Skill exchange must receive the updated Full Frame"
        );

        Ok::<_, anyhow::Error>(SkillEvidence {
            renders_after_mount,
            renders_after_latest,
            renders_after_command,
            latest_submitted,
            command_submitted,
            revision_unchanged_before_exchange,
            command_left_projection_dirty,
            exchange_kind,
            exchange_saw_updated_state,
            shutdown_completed: false,
            task_dropped: false,
        })
    }
    .await;

    let shutdown = application.shutdown().await;
    let task_was_dropped = task_dropped.load(Ordering::Acquire);
    let mut evidence = match (operation, shutdown) {
        (Ok(evidence), Ok(())) => evidence,
        (Err(operation), Ok(())) => return Err(operation),
        (Ok(_), Err(shutdown)) => return Err(shutdown.into()),
        (Err(operation), Err(shutdown)) => {
            return Err(operation.context(format!("Skill shutdown also failed: {shutdown}")));
        }
    };
    ensure!(
        task_was_dropped,
        "Skill shutdown must drain its Component task"
    );
    evidence.shutdown_completed = true;
    evidence.task_dropped = true;
    Ok(evidence)
}

#[tokio::main]
async fn main() -> Result<()> {
    let evidence = run_skill().await?;
    println!(
        "frame_skill latest_submitted={} command_submitted={} renders={}/{}/{} revision_unchanged={} dirty_before_exchange={} exchange={:?} updated={} shutdown={} task_dropped={}",
        evidence.latest_submitted,
        evidence.command_submitted,
        evidence.renders_after_mount,
        evidence.renders_after_latest,
        evidence.renders_after_command,
        evidence.revision_unchanged_before_exchange,
        evidence.command_left_projection_dirty,
        evidence.exchange_kind,
        evidence.exchange_saw_updated_state,
        evidence.shutdown_completed,
        evidence.task_dropped,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_skill_driver_matches_the_shared_exact_first_frame_golden() {
        let (port, control) = ExternalProviderPort::new().unwrap();
        let mut application =
            Application::mount(frame_workflow_golden::shared_frame_workflow_root, port).unwrap();

        let operation = explicit_exchange(&mut application, &control).await;
        let shutdown = application.shutdown().await;
        let observation = operation.unwrap();
        shutdown.unwrap();

        assert_eq!(observation.kind(), ExternalObservationKind::Full);
        frame_workflow_golden::assert_exact_first_frame(
            observation.frame().submission().canonical_bytes(),
        );
    }

    #[tokio::test]
    async fn latest_and_typed_command_are_passive_until_explicit_exchange() {
        let evidence = run_skill().await.unwrap();

        assert_eq!(evidence.renders_after_mount, 1);
        assert_eq!(evidence.renders_after_latest, 1);
        assert_eq!(evidence.renders_after_command, 1);
        assert!(!evidence.latest_submitted);
        assert!(!evidence.command_submitted);
        assert!(evidence.revision_unchanged_before_exchange);
        assert!(evidence.command_left_projection_dirty);
        assert_eq!(evidence.exchange_kind, ExternalObservationKind::Full);
        assert!(evidence.exchange_saw_updated_state);
        assert!(evidence.shutdown_completed);
        assert!(evidence.task_dropped);
    }
}
