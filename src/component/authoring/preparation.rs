use std::{collections::HashSet, fmt, future::Future, pin::Pin};

use crate::component::signal::{HookMount, MountIdentity};

type PreparationFuture =
    Pin<Box<dyn Future<Output = Result<(), PreparationFault>> + Send + 'static>>;

#[derive(PartialEq, Eq, Hash)]
struct PreparationIdentity {
    mount: MountIdentity,
    slot: usize,
}

#[derive(Default)]
pub(crate) struct PreparationRun {
    completed: HashSet<PreparationIdentity>,
}

pub(crate) struct PreparationDeclaration {
    identity: PreparationIdentity,
    mount: HookMount,
    prepare: Box<dyn FnOnce() -> PreparationFuture + Send + 'static>,
}

impl PreparationDeclaration {
    pub(crate) fn new<Loader, LoaderFuture, Error>(loader: Loader, mount: HookMount) -> Self
    where
        Loader: FnOnce() -> LoaderFuture + Send + 'static,
        LoaderFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Self {
            identity: PreparationIdentity {
                mount: MountIdentity::new(mount.component.clone(), mount.generation),
                slot: mount.slot,
            },
            mount,
            prepare: Box::new(move || {
                Box::pin(async move { loader().await.map_err(PreparationFault::loader) })
            }),
        }
    }

    async fn prepare(self, run: &mut PreparationRun) -> Result<(), PreparationFault> {
        let Self {
            identity,
            mount,
            prepare,
        } = self;
        if run.completed.contains(&identity) {
            return Ok(());
        }
        let Some(_permit) = mount.authorize() else {
            return Ok(());
        };
        prepare().await?;
        mount.with_active(|| run.completed.insert(identity));
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct PreparationSet {
    declarations: Vec<PreparationDeclaration>,
}

impl PreparationSet {
    pub(crate) fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }

    pub(crate) fn has_pending(&self, run: &PreparationRun) -> bool {
        self.declarations
            .iter()
            .any(|declaration| !run.completed.contains(&declaration.identity))
    }

    pub(crate) fn push(&mut self, declaration: PreparationDeclaration) {
        self.declarations.push(declaration);
    }

    pub(crate) async fn prepare(self, run: &mut PreparationRun) -> Result<(), PreparationFault> {
        for declaration in self.declarations {
            declaration.prepare(run).await?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PreparationFault {
    #[error("preparation loader failed: {message}")]
    Loader { message: String },
}

impl PreparationFault {
    fn loader(error: impl fmt::Display) -> Self {
        Self::Loader {
            message: error.to_string(),
        }
    }
}

/// Declare asynchronous preparation required before Provider handoff.
///
/// Each explicit `Application::prepare()` or `Application::react()` call runs
/// this hook once per Component mount generation. Signal writes are reconciled
/// before handoff, and preparation hooks on newly mounted Components also run.
/// Rerendering an already prepared mount does not rerun its hook within that
/// operation, even if another hook changes its inputs. Sequence dependent work
/// in one loader or express it through newly mounted child Components.
///
/// No completion is cached across operations: a subsequent call runs every
/// active preparation hook again, including after an ordinary error or dropped
/// operation. Loaders must tolerate retries and cancellation. Completed Signal
/// writes and external effects are not rolled back; external effects require
/// their own business idempotency and deduplication policy.
///
/// The loader factory and returned future execute outside the mount fence as
/// part of the caller's operation. Only the returned future is awaited, not
/// detached work. Panics unwind directly through `prepare()` or `react()`.
///
/// This hook must be called directly inside a `#[component]` function so it can
/// be bound to that lexical slot. It requires the current
/// [`Application`](crate::component::execution::Application) and
/// [`ReactionPort`](crate::component::execution::ReactionPort) runtime; the
/// deprecated `ApplicationHost` compatibility runtime rejects preparation hooks
/// before invoking its Provider.
#[track_caller]
pub fn use_preparation<Loader, LoaderFuture, Error>(_loader: Loader)
where
    Loader: FnOnce() -> LoaderFuture + Send + 'static,
    LoaderFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: fmt::Display + Send + 'static,
{
    panic!("use_preparation must be called directly inside a #[component] function")
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;
    use crate::component::{
        signal::{HookKind, SignalRuntime},
        ComponentId,
    };

    fn active_mount(signals: &SignalRuntime) -> HookMount {
        let mut render = signals.begin_render().unwrap();
        let mount = render
            .render_component(ComponentId::root(), |scope| {
                scope.use_marker_at(0, HookKind::Preparation)
            })
            .unwrap();
        render.commit();
        mount
    }

    #[tokio::test]
    async fn same_run_prepares_a_remounted_generation_but_not_the_same_slot_twice() {
        let signals = SignalRuntime::new();
        let mount = active_mount(&signals);
        let calls = Arc::new(AtomicUsize::new(0));
        let declaration = |mount| {
            let calls = Arc::clone(&calls);
            PreparationDeclaration::new(
                move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok::<(), &'static str>(()))
                },
                mount,
            )
        };
        let mut run = PreparationRun::default();

        declaration(mount.clone()).prepare(&mut run).await.unwrap();
        declaration(mount).prepare(&mut run).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        signals.remount_component(&ComponentId::root()).unwrap();
        let replacement = active_mount(&signals);
        declaration(replacement).prepare(&mut run).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_mount_does_not_invoke_preparation_factory() {
        let signals = SignalRuntime::new();
        let mount = active_mount(&signals);
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let declaration = PreparationDeclaration::new(
            move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok::<(), &'static str>(()))
            },
            mount,
        );
        signals.remount_component(&ComponentId::root()).unwrap();
        let mut run = PreparationRun::default();

        declaration.prepare(&mut run).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(run.completed.is_empty());
    }

    #[tokio::test]
    async fn retirement_before_completion_does_not_record_preparation_success() {
        let signals = Arc::new(SignalRuntime::new());
        let mount = active_mount(&signals);
        let declaration = PreparationDeclaration::new(
            move || async move {
                signals.remount_component(&ComponentId::root()).unwrap();
                Ok::<(), &'static str>(())
            },
            mount,
        );
        let mut run = PreparationRun::default();

        declaration.prepare(&mut run).await.unwrap();

        assert!(run.completed.is_empty());
    }
}
