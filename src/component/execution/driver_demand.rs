//! Internal Component-to-driver reaction demand primitive.
//!
//! The primitive carries no scheduling policy. One pending bit means that a
//! later driver turn is requested; repeated requests coalesce until the sole
//! driver consumes that bit.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Debug)]
struct DriverDemandState {
    active: bool,
    pending: bool,
}

#[derive(Debug)]
struct DriverDemandCore {
    state: Mutex<DriverDemandState>,
    changed: Notify,
}

/// Cloneable, mount-fenced capability that can request a later driver turn.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Phase 7 seam; the public Component hook is introduced in Phase 8.
pub(crate) struct DriverDemandHandle {
    core: Arc<DriverDemandCore>,
}

impl DriverDemandHandle {
    /// Record at least one outstanding demand without rendering or reentry.
    #[allow(dead_code)] // Invoked through the Phase 7 internal mount bridge and its tests.
    pub(crate) fn request(&self) -> Result<(), DriverDemandFault> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active {
            return Err(DriverDemandFault::StaleMount);
        }
        state.pending = true;
        drop(state);
        self.core.changed.notify_waiters();
        Ok(())
    }
}

/// Application-owned receiver for one mount's reaction demand.
#[derive(Debug)]
pub(crate) struct DriverDemand {
    core: Arc<DriverDemandCore>,
}

impl DriverDemand {
    pub(crate) fn new() -> (Self, DriverDemandHandle) {
        let core = Arc::new(DriverDemandCore {
            state: Mutex::new(DriverDemandState {
                active: true,
                pending: false,
            }),
            changed: Notify::new(),
        });
        (
            Self {
                core: Arc::clone(&core),
            },
            DriverDemandHandle { core },
        )
    }

    /// Consume the current coalesced demand without waiting.
    pub(crate) fn take(&self) -> Result<bool, DriverDemandFault> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active {
            return Err(DriverDemandFault::StaleMount);
        }
        Ok(std::mem::take(&mut state.pending))
    }

    /// Wait for and consume one coalesced demand.
    pub(crate) async fn wait(&self) -> Result<(), DriverDemandFault> {
        loop {
            let changed = self.core.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            {
                let mut state = self
                    .core
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !state.active {
                    return Err(DriverDemandFault::StaleMount);
                }
                if std::mem::take(&mut state.pending) {
                    return Ok(());
                }
            }

            changed.await;
        }
    }
}

impl Drop for DriverDemand {
    fn drop(&mut self) {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active = false;
        state.pending = false;
        drop(state);
        self.core.changed.notify_waiters();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DriverDemandFault {
    #[error("reaction demand belongs to a stale Component mount")]
    StaleMount,
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use futures::poll;

    use super::{DriverDemand, DriverDemandFault};

    #[tokio::test]
    async fn request_before_wait_is_sticky() {
        let (demand, handle) = DriverDemand::new();

        handle.request().unwrap();

        demand.wait().await.unwrap();
        assert!(!demand.take().unwrap());
    }

    #[tokio::test]
    async fn request_after_wait_wakes_the_registered_driver() {
        let (demand, handle) = DriverDemand::new();
        let mut waiting = Box::pin(demand.wait());
        assert!(matches!(poll!(waiting.as_mut()), Poll::Pending));

        handle.request().unwrap();

        waiting.await.unwrap();
        assert!(!demand.take().unwrap());
    }

    #[tokio::test]
    async fn pending_requests_coalesce_into_one_later_turn() {
        let (demand, handle) = DriverDemand::new();

        handle.request().unwrap();
        handle.request().unwrap();
        handle.request().unwrap();

        demand.wait().await.unwrap();
        assert!(!demand.take().unwrap());
    }

    #[tokio::test]
    async fn request_during_a_turn_remains_for_the_next_wait() {
        let (demand, handle) = DriverDemand::new();
        handle.request().unwrap();
        demand.wait().await.unwrap();

        handle.request().unwrap();

        demand.wait().await.unwrap();
        assert!(!demand.take().unwrap());
    }

    #[tokio::test]
    async fn cancelling_a_wait_does_not_consume_a_later_request() {
        let (demand, handle) = DriverDemand::new();
        let mut cancelled = Box::pin(demand.wait());
        assert!(matches!(poll!(cancelled.as_mut()), Poll::Pending));
        drop(cancelled);

        handle.request().unwrap();

        demand.wait().await.unwrap();
        assert!(!demand.take().unwrap());
    }

    #[tokio::test]
    async fn dropping_mount_fences_handles_and_releases_waiters() {
        let (demand, handle) = DriverDemand::new();
        let waiting_core = std::sync::Arc::clone(&demand.core);
        let waiting = tokio::spawn(async move { DriverDemand { core: waiting_core }.wait().await });
        tokio::task::yield_now().await;

        drop(demand);

        assert_eq!(handle.request(), Err(DriverDemandFault::StaleMount));
        assert_eq!(waiting.await.unwrap(), Err(DriverDemandFault::StaleMount));
    }
}
