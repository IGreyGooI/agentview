//! Awake runtime for view-model driven loops.
//!
//! This module does not inspect rendered VM state and does not classify why an
//! application continued. The owner of an
//! [`AgentViewModel`](crate::agent::AgentViewModel) source calls
//! [`ViewAwakeHandle::awake`] when a loop waiting on that source may continue.

use tokio::sync::watch;

/// Monotonic awake epoch for a view source.
pub type ViewEpoch = u64;

/// Runtime side used by daemon/session code to wait for awake signals.
#[derive(Debug, Clone)]
pub struct ViewAwake {
    tx: watch::Sender<ViewEpoch>,
}

impl Default for ViewAwake {
    fn default() -> Self {
        Self::new().0
    }
}

impl ViewAwake {
    /// Create a new awake runtime and its application-side handle.
    pub fn new() -> (Self, ViewAwakeHandle) {
        let (tx, _rx) = watch::channel(0);
        let awake = Self { tx };
        let handle = awake.handle();
        (awake, handle)
    }

    /// Handle to pass to the application code that owns the view source.
    pub fn handle(&self) -> ViewAwakeHandle {
        ViewAwakeHandle {
            tx: self.tx.clone(),
        }
    }

    /// Current awake epoch.
    pub fn current_epoch(&self) -> ViewEpoch {
        *self.tx.borrow()
    }

    /// Subscribe a long-running loop to awake signals from this source.
    pub fn subscribe(&self) -> ViewAwakeSubscription {
        let rx = self.tx.subscribe();
        let seen_epoch = *rx.borrow();
        ViewAwakeSubscription { rx, seen_epoch }
    }

    /// Wait until the owner awakes the source after `epoch`.
    ///
    /// If the current epoch is already newer, this returns immediately. The
    /// returned value is the newest observed epoch, not necessarily every
    /// intermediate awake signal.
    pub async fn wait_after(&self, epoch: ViewEpoch) -> ViewEpoch {
        self.subscribe().wait_after(epoch).await
    }
}

/// Subscription side used by long-running loops such as autonomous agents.
#[derive(Debug, Clone)]
pub struct ViewAwakeSubscription {
    rx: watch::Receiver<ViewEpoch>,
    seen_epoch: ViewEpoch,
}

impl ViewAwakeSubscription {
    /// Latest epoch observed by this subscription.
    pub fn current_epoch(&self) -> ViewEpoch {
        self.seen_epoch
    }

    /// Wait until the source is awoken after `epoch`.
    pub async fn wait_after(&mut self, epoch: ViewEpoch) -> ViewEpoch {
        loop {
            let current = *self.rx.borrow_and_update();
            if current > epoch {
                self.seen_epoch = current;
                return current;
            }

            if self.rx.changed().await.is_err() {
                self.seen_epoch = current;
                return current;
            }
        }
    }

    /// Wait for the next awake after this subscription's current epoch.
    pub async fn wait_next(&mut self) -> ViewEpoch {
        self.wait_after(self.seen_epoch).await
    }
}

/// Application-owned handle used to awake loops waiting on the view source.
#[derive(Debug, Clone)]
pub struct ViewAwakeHandle {
    tx: watch::Sender<ViewEpoch>,
}

impl ViewAwakeHandle {
    /// Awake loops waiting on the associated view source.
    ///
    /// This is the only signal. The caller decides when awake is needed;
    /// `agentview` only publishes the new epoch to waiters.
    pub fn awake(&self) -> ViewEpoch {
        let mut next = 0;
        self.tx.send_modify(|epoch| {
            *epoch = epoch.checked_add(1).expect("ViewEpoch overflowed");
            next = *epoch;
        });
        next
    }

    /// Current awake epoch.
    pub fn current_epoch(&self) -> ViewEpoch {
        *self.tx.borrow()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::sync::Barrier;
    use tokio::time::timeout;

    use super::ViewAwake;

    #[tokio::test]
    async fn wait_after_returns_immediately_for_newer_epoch() {
        let (awake, handle) = ViewAwake::new();

        let epoch = handle.awake();

        assert_eq!(awake.wait_after(epoch - 1).await, epoch);
    }

    #[tokio::test]
    async fn wait_after_blocks_until_invalidated() {
        let (awake, handle) = ViewAwake::new();
        let waiter = tokio::spawn({
            let awake = awake.clone();
            async move { awake.wait_after(0).await }
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        let epoch = handle.awake();

        assert_eq!(
            timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap(),
            epoch
        );
    }

    #[tokio::test]
    async fn awake_epochs_are_monotonic() {
        let (awake, handle) = ViewAwake::new();

        assert_eq!(handle.awake(), 1);
        assert_eq!(handle.awake(), 2);
        assert_eq!(awake.current_epoch(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_awake_calls_publish_the_latest_epoch() {
        let workers = 64;
        let calls_per_worker = 100;
        let (awake, handle) = ViewAwake::new();
        let barrier = Arc::new(Barrier::new(workers));

        let mut tasks = Vec::with_capacity(workers);
        for _ in 0..workers {
            let barrier = Arc::clone(&barrier);
            let handle = handle.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                for _ in 0..calls_per_worker {
                    handle.awake();
                    tokio::task::yield_now().await;
                }
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        let expected = (workers * calls_per_worker) as u64;
        assert_eq!(awake.subscribe().current_epoch(), expected);
    }

    #[tokio::test]
    async fn subscription_wait_next_waits_after_current_epoch() {
        let (awake, handle) = ViewAwake::new();
        handle.awake();

        let mut subscription = awake.subscribe();
        let waiter = tokio::spawn(async move { subscription.wait_next().await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        let epoch = handle.awake();

        assert_eq!(
            timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap(),
            epoch
        );
    }

    #[tokio::test]
    async fn subscription_wait_next_does_not_miss_awake_before_waiting() {
        let (awake, handle) = ViewAwake::new();
        let mut subscription = awake.subscribe();

        let epoch = handle.awake();

        assert_eq!(subscription.wait_next().await, epoch);
    }
}
