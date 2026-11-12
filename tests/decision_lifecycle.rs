use std::{
    num::{NonZeroU128, NonZeroU64},
    sync::Arc,
    time::Duration,
};

use agentview::component::execution::{
    ContextWindow, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderDecision,
    ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault,
    SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
};
use agentview::component::prelude::Noul;
use async_trait::async_trait;
use tokio::sync::Notify;

#[derive(Default)]
struct Lifecycle {
    stream_polled: Notify,
    shutdown_started: Notify,
    allow_cleanup: Notify,
    cleanup_done: Notify,
}

struct LifecyclePort {
    declaration: TargetDeclaration,
    lifecycle: Arc<Lifecycle>,
    complete: bool,
}

impl LifecyclePort {
    fn new(lifecycle: Arc<Lifecycle>, complete: bool) -> Self {
        Self {
            declaration: TargetDeclaration::full(
                TargetIdentity::new(NonZeroU128::new(76_005).unwrap()),
                TargetEpoch::new(NonZeroU64::MIN),
                FrameProfile::new(
                    FrameConstraints {
                        max_frame_bytes: 1_048_576,
                        max_component_bytes: 262_144,
                        context_window: ContextWindow::NotApplicable,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true).with_decision(true),
                ),
            ),
            lifecycle,
            complete,
        }
    }
}

#[async_trait]
impl ReactionPort for LifecyclePort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        let batch_id = frame.submission().decision().unwrap().batch_id().clone();
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        if self.complete {
            Ok(Box::pin(futures::stream::iter([
                Ok(ProviderFact::Decision {
                    output: ProviderOutputKey::new(0),
                    decision: ProviderDecision::json_envelope(
                        batch_id,
                        serde_json::json!({"answers":{"judgment":{"type":"noul","noul":0.9}}})
                            .to_string(),
                    ),
                }),
                Ok(ProviderFact::ReactionCompleted { primary_text: None }),
            ])))
        } else {
            let lifecycle = self.lifecycle.clone();
            Ok(Box::pin(futures::stream::once(async move {
                lifecycle.stream_polled.notify_one();
                std::future::pending::<Result<ProviderFact, ReactionPortFault>>().await
            })))
        }
    }

    async fn shutdown(&mut self) {
        self.lifecycle.shutdown_started.notify_one();
        self.lifecycle.allow_cleanup.notified().await;
        self.lifecycle.cleanup_done.notify_one();
    }
}

async fn bounded(event: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), event.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_judgment_waiter_keeps_consuming_cleanup_owned() {
    let lifecycle = Arc::new(Lifecycle::default());
    let port = LifecyclePort::new(lifecycle.clone(), false);
    let waiter = tokio::spawn(agentview::system1::judge(
        port,
        serde_json::json!({"permission":"granted"}),
        Noul::new("Does the displayed state permit opening the gate?"),
    ));
    bounded(&lifecycle.stream_polled).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    bounded(&lifecycle.shutdown_started).await;
    lifecycle.allow_cleanup.notify_one();
    bounded(&lifecycle.cleanup_done).await;
}

#[tokio::test]
async fn judgment_success_waits_for_consuming_cleanup() {
    let lifecycle = Arc::new(Lifecycle::default());
    let port = LifecyclePort::new(lifecycle.clone(), true);
    let waiter = tokio::spawn(agentview::system1::judge(
        port,
        serde_json::json!({"permission":"granted"}),
        Noul::new("Does the displayed state permit opening the gate?"),
    ));
    bounded(&lifecycle.shutdown_started).await;
    assert!(
        !waiter.is_finished(),
        "a staged value must wait for cleanup"
    );
    lifecycle.allow_cleanup.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.value.noul, 0.9);
    assert!(result.origin.frame_revision.is_some());
    bounded(&lifecycle.cleanup_done).await;
}
