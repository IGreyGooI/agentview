//! Application-wide, mount-fenced normal-exit capability.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::component::signal::HookMount;

/// The normal lifecycle result recorded for an [`Application`](crate::component::execution::Application).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The application completed its declared business work.
    Completed,
    /// An owning host or external control requested normal termination.
    Requested,
}

/// Failure to request normal application termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApplicationExitError {
    #[error("the application exit handle belongs to a stale Component mount")]
    StaleMount,
    #[error("the application is already closed")]
    Closed,
}

#[derive(Debug)]
struct ExitState {
    reason: Option<ExitReason>,
    closed: bool,
}

#[derive(Debug)]
struct ApplicationExitCore {
    state: Mutex<ExitState>,
    changed: Notify,
}

/// Cloneable normal-exit capability issued to either a Component or its host.
#[derive(Clone)]
pub struct ApplicationExitHandle {
    control: ApplicationExitControl,
    mount: Option<HookMount>,
}

impl ApplicationExitHandle {
    pub(crate) fn for_mount(mount: HookMount, control: ApplicationExitControl) -> Self {
        Self {
            control,
            mount: Some(mount),
        }
    }

    /// Record a normal application exit reason.
    ///
    /// The first accepted reason is retained. Later requests are successful
    /// no-ops until the owning application is closed.
    pub fn request(&self, reason: ExitReason) -> Result<(), ApplicationExitError> {
        match &self.mount {
            Some(mount) => mount
                .with_active(|| self.control.request(reason))
                .ok_or(ApplicationExitError::StaleMount)?,
            None => self.control.request(reason),
        }
    }
}

/// Host-owned state shared by every application-exit handle.
#[derive(Clone, Debug)]
pub(crate) struct ApplicationExitControl {
    core: Arc<ApplicationExitCore>,
}

impl ApplicationExitControl {
    pub(crate) fn new() -> Self {
        Self {
            core: Arc::new(ApplicationExitCore {
                state: Mutex::new(ExitState {
                    reason: None,
                    closed: false,
                }),
                changed: Notify::new(),
            }),
        }
    }

    pub(crate) fn handle(&self) -> ApplicationExitHandle {
        ApplicationExitHandle {
            control: self.clone(),
            mount: None,
        }
    }

    pub(crate) fn reason(&self) -> Option<ExitReason> {
        self.core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reason
    }

    /// Wait for a recorded normal exit without losing a concurrent wakeup.
    pub(crate) async fn wait(&self) -> ExitReason {
        loop {
            let changed = self.core.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            if let Some(reason) = self.reason() {
                return reason;
            }

            changed.await;
        }
    }

    /// Atomically observe whether a Provider submission may begin.
    ///
    /// A request that wins this mutex after this method returns is intentionally
    /// allowed to take effect after the in-flight submission boundary.
    pub(crate) fn try_begin_submission(&self) -> Result<(), ExitReason> {
        let state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.reason {
            Some(reason) => Err(reason),
            None if state.closed => Err(ExitReason::Requested),
            None => Ok(()),
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        state.reason.get_or_insert(ExitReason::Requested);
        drop(state);
        self.core.changed.notify_waiters();
    }

    fn request(&self, reason: ExitReason) -> Result<(), ApplicationExitError> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(ApplicationExitError::Closed);
        }
        let changed = state.reason.is_none();
        state.reason.get_or_insert(reason);
        drop(state);
        if changed {
            self.core.changed.notify_waiters();
        }
        Ok(())
    }
}

/// Retain a mount-scoped capability for ending the owning application normally.
///
/// This hook must be called directly inside a `#[component]` function so the
/// macro can bind it to the Component's lexical hook slot.
#[track_caller]
pub fn use_application_exit() -> ApplicationExitHandle {
    panic!("use_application_exit must be called directly inside a #[component] function")
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use futures::poll;

    use super::{ApplicationExitControl, ApplicationExitError, ExitReason};
    use crate::component::{
        signal::{HookKind, SignalRuntime},
        ComponentId,
    };

    fn active_mount(signals: &SignalRuntime) -> crate::component::signal::HookMount {
        let mut render = signals.begin_render().unwrap();
        let mount = render
            .render_component(ComponentId::root(), |scope| {
                scope.use_marker_at(0, HookKind::ApplicationExit)
            })
            .unwrap();
        render.commit();
        mount
    }

    #[test]
    fn first_exit_reason_is_sticky_across_handle_clones() {
        let control = ApplicationExitControl::new();
        let first = control.handle();
        let second = first.clone();

        first.request(ExitReason::Completed).unwrap();
        second.request(ExitReason::Requested).unwrap();
        control.close();

        assert_eq!(control.reason(), Some(ExitReason::Completed));
    }

    #[tokio::test]
    async fn wait_observes_an_exit_recorded_before_and_after_registration() {
        let control = ApplicationExitControl::new();
        control.handle().request(ExitReason::Completed).unwrap();
        assert_eq!(control.wait().await, ExitReason::Completed);

        let control = ApplicationExitControl::new();
        let mut waiting = Box::pin(control.wait());
        assert!(matches!(poll!(waiting.as_mut()), Poll::Pending));
        control.handle().request(ExitReason::Requested).unwrap();
        assert_eq!(waiting.await, ExitReason::Requested);
    }

    #[test]
    fn submission_boundary_observes_an_already_recorded_exit() {
        let control = ApplicationExitControl::new();
        assert_eq!(control.try_begin_submission(), Ok(()));
        control.handle().request(ExitReason::Requested).unwrap();
        assert_eq!(control.try_begin_submission(), Err(ExitReason::Requested));
    }

    #[tokio::test]
    async fn close_wakes_waiters_and_rejects_new_requests() {
        let control = ApplicationExitControl::new();
        let mut waiting = Box::pin(control.wait());
        assert!(matches!(poll!(waiting.as_mut()), Poll::Pending));

        control.close();

        assert_eq!(waiting.await, ExitReason::Requested);
        assert_eq!(
            control.handle().request(ExitReason::Completed),
            Err(ApplicationExitError::Closed)
        );
        assert_eq!(control.try_begin_submission(), Err(ExitReason::Requested));
    }

    #[test]
    fn component_handles_are_mount_fenced_but_host_handles_are_not() {
        let control = ApplicationExitControl::new();
        let signals = SignalRuntime::new();
        let component =
            super::ApplicationExitHandle::for_mount(active_mount(&signals), control.clone());
        let host = control.handle();

        signals.invalidate_all().unwrap();

        assert_eq!(
            component.request(ExitReason::Completed),
            Err(ApplicationExitError::StaleMount)
        );
        host.request(ExitReason::Requested).unwrap();
        assert_eq!(control.reason(), Some(ExitReason::Requested));
    }
}
