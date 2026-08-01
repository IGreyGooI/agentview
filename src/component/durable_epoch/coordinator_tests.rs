use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use serde_json::json;
use tokio::sync::Notify;

use super::*;

impl DurableEpochRuntime for &'static str {
    type Error = Infallible;

    fn validate_against_manifest(
        &self,
        _manifest: &EpochContractManifest,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn runtime_descriptor(version: &str) -> RuntimeBindingDescriptor {
    RuntimeBindingDescriptor::new("root/select_intent::xml", "xml", "select_intent", version)
        .unwrap()
}

fn tool_descriptor(description: &str) -> ProviderToolDescriptor {
    let spec = ProviderToolSpec::new(
        "inspect",
        description,
        json!({
            "type": "object",
            "properties": { "subject": { "type": "string" } },
            "required": ["subject"]
        }),
    )
    .unwrap();
    ProviderToolDescriptor::new("root/director_tools", "v1", &spec).unwrap()
}

fn manifest(runtime_version: &str, tool_description: &str) -> EpochContractManifest {
    EpochContractManifest::new(
        EpochContractId::new("player/v1").unwrap(),
        "sha256:player-config-v1",
        "sha256:player-runtime-v1",
        std::num::NonZeroUsize::MIN,
        ProviderAdapterContract::new("test-provider", 1).unwrap(),
        vec![runtime_descriptor(runtime_version)],
        vec![tool_descriptor(tool_description)],
    )
    .unwrap()
}

#[derive(Clone)]
struct TestBinder {
    manifest: EpochContractManifest,
    calls: Arc<AtomicUsize>,
}

impl RuntimeRebindProjection for TestBinder {
    type Runtime = &'static str;
    type Error = Infallible;

    fn rebind(&self, request: RuntimeRebindRequest<'_>) -> Result<Self::Runtime, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.manifest(), &self.manifest);
        assert!(request.fingerprint().as_str().starts_with("sha256:"));
        assert!(request.durable_epoch_id().as_str().ends_with("epoch-7"));
        Ok("rebound-runtime")
    }
}

fn binder(manifest: &EpochContractManifest) -> TestBinder {
    TestBinder {
        manifest: manifest.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
    }
}

#[derive(Debug, thiserror::Error)]
enum TestStoreError {
    #[error("epoch fence mismatch")]
    Fence,

    #[error("invalid test store transition")]
    Transition,
}

#[derive(Clone)]
enum TestEpochState {
    Empty,
    RenderStarted {
        manifest: EpochContractManifest,
        fence: EpochOpenFence,
    },
    Rendered {
        artifact: RenderedEpochArtifact,
        fence: Option<EpochOpenFence>,
    },
    Active(ActiveEpochArtifact),
}

struct TestEpochStore {
    state: Mutex<TestEpochState>,
    next_fence: AtomicUsize,
    now_unix_ms: AtomicU64,
}

impl TestEpochStore {
    fn new() -> Self {
        Self {
            state: Mutex::new(TestEpochState::Empty),
            next_fence: AtomicUsize::new(1),
            now_unix_ms: AtomicU64::new(10_000),
        }
    }

    fn fence(&self, durable_epoch_id: DurableEpochId) -> EpochOpenFence {
        let value = self.next_fence.fetch_add(1, Ordering::SeqCst);
        let issued_at_unix_ms = self.now_unix_ms.load(Ordering::SeqCst);
        let lease = EpochOpenLease::new(issued_at_unix_ms, issued_at_unix_ms + 1_000).unwrap();
        EpochOpenFence::new(durable_epoch_id, format!("fence-{value}"), lease).unwrap()
    }

    fn simulate_owner_loss(&self) {
        let expiry = match &*self.state.lock().unwrap() {
            TestEpochState::RenderStarted { fence, .. } => Some(fence.lease().expires_at_unix_ms()),
            TestEpochState::Rendered { fence, .. } => fence
                .as_ref()
                .map(|fence| fence.lease().expires_at_unix_ms()),
            TestEpochState::Empty | TestEpochState::Active(_) => None,
        };
        if let Some(expiry) = expiry {
            self.now_unix_ms.store(expiry, Ordering::SeqCst);
        }
    }

    fn fence_is_live(&self, fence: &EpochOpenFence) -> bool {
        fence
            .lease()
            .is_live_at(self.now_unix_ms.load(Ordering::SeqCst))
    }
}

#[async_trait::async_trait]
impl DurableEpochStore for TestEpochStore {
    type Error = TestStoreError;

    async fn acquire_epoch(
        &self,
        request: EpochOpenRequest<'_>,
    ) -> Result<EpochOpenAdmission, Self::Error> {
        assert_eq!(request.session_id().as_str(), "forgotten-city/player");
        let mut state = self.state.lock().unwrap();
        match state.clone() {
            TestEpochState::Empty => {
                let durable_epoch_id =
                    DurableEpochId::new("forgotten-city/player/epoch-7").unwrap();
                let fence = self.fence(durable_epoch_id);
                *state = TestEpochState::RenderStarted {
                    manifest: request.manifest().clone(),
                    fence: fence.clone(),
                };
                Ok(EpochOpenAdmission::Create { fence })
            }
            TestEpochState::RenderStarted { manifest, fence } => {
                if manifest != *request.manifest() {
                    return Ok(EpochOpenAdmission::Conflict {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                        existing: manifest,
                    });
                }
                if self.fence_is_live(&fence) {
                    Ok(EpochOpenAdmission::InFlight {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                        lease_expires_at_unix_ms: fence.lease().expires_at_unix_ms(),
                    })
                } else {
                    Ok(EpochOpenAdmission::RecoveryRequired {
                        durable_epoch_id: fence.durable_epoch_id().clone(),
                        phase: EpochOpenRecoveryPhase::RenderStarted,
                    })
                }
            }
            TestEpochState::Rendered { artifact, fence } => {
                if artifact.manifest() != request.manifest() {
                    return Ok(EpochOpenAdmission::Conflict {
                        durable_epoch_id: artifact.durable_epoch_id().clone(),
                        existing: artifact.manifest().clone(),
                    });
                }
                if fence
                    .as_ref()
                    .is_some_and(|fence| self.fence_is_live(fence))
                {
                    return Ok(EpochOpenAdmission::InFlight {
                        durable_epoch_id: artifact.durable_epoch_id().clone(),
                        lease_expires_at_unix_ms: fence
                            .as_ref()
                            .expect("a live rendered fence is present")
                            .lease()
                            .expires_at_unix_ms(),
                    });
                }
                let next_fence = self.fence(artifact.durable_epoch_id().clone());
                *state = TestEpochState::Rendered {
                    artifact: artifact.clone(),
                    fence: Some(next_fence.clone()),
                };
                Ok(EpochOpenAdmission::ResumeAttachment {
                    fence: next_fence,
                    artifact,
                })
            }
            TestEpochState::Active(artifact) => {
                if artifact.rendered().manifest() == request.manifest() {
                    Ok(EpochOpenAdmission::Existing { artifact })
                } else {
                    Ok(EpochOpenAdmission::Conflict {
                        durable_epoch_id: artifact.rendered().durable_epoch_id().clone(),
                        existing: artifact.rendered().manifest().clone(),
                    })
                }
            }
        }
    }

    async fn store_rendered_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().unwrap();
        let TestEpochState::RenderStarted {
            manifest,
            fence: current,
        } = state.clone()
        else {
            return Err(TestStoreError::Transition);
        };
        if current != *fence
            || !self.fence_is_live(fence)
            || artifact.durable_epoch_id() != fence.durable_epoch_id()
            || artifact.manifest() != &manifest
            || artifact.validate().is_err()
        {
            return Err(TestStoreError::Fence);
        }
        *state = TestEpochState::Rendered {
            artifact: artifact.clone(),
            fence: Some(fence.clone()),
        };
        Ok(())
    }

    async fn relinquish_epoch_attachment(
        &self,
        fence: &EpochOpenFence,
        artifact: &RenderedEpochArtifact,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().unwrap();
        let TestEpochState::Rendered {
            artifact: rendered,
            fence: current,
        } = state.clone()
        else {
            return Err(TestStoreError::Transition);
        };
        if current.as_ref() != Some(fence) || &rendered != artifact || artifact.validate().is_err()
        {
            return Err(TestStoreError::Fence);
        }
        *state = TestEpochState::Rendered {
            artifact: rendered,
            fence: None,
        };
        Ok(())
    }

    async fn activate_epoch(
        &self,
        fence: &EpochOpenFence,
        artifact: &ActiveEpochArtifact,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().unwrap();
        let TestEpochState::Rendered {
            artifact: rendered,
            fence: current,
        } = state.clone()
        else {
            return Err(TestStoreError::Transition);
        };
        if current.as_ref() != Some(fence)
            || !self.fence_is_live(fence)
            || artifact.rendered().durable_epoch_id() != fence.durable_epoch_id()
            || artifact.rendered() != &rendered
            || artifact.validate().is_err()
        {
            return Err(TestStoreError::Fence);
        }
        *state = TestEpochState::Active(artifact.clone());
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum TestProviderError {
    #[error("provider attached remotely but its reply was lost")]
    LostAttachReply,
}

struct AttachGate {
    entered: AtomicBool,
    entered_notify: Notify,
    release: Notify,
}

impl AttachGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            entered_notify: Notify::new(),
            release: Notify::new(),
        }
    }

    async fn wait_until_entered(&self) {
        loop {
            let entered = self.entered_notify.notified();
            if self.entered.load(Ordering::SeqCst) {
                return;
            }
            entered.await;
        }
    }
}

#[derive(Default)]
struct TestProviderState {
    remote_system_receives: usize,
    attach_calls: usize,
    rehydrate_calls: usize,
    rehydrate_cursors: Vec<Option<ProviderTurnCursor>>,
    sessions: HashMap<DurableEpochId, ProviderEpochReceipt>,
}

struct TestProvider {
    state: Mutex<TestProviderState>,
    fail_attach_reply_once: AtomicBool,
    gate: Option<Arc<AttachGate>>,
}

impl TestProvider {
    fn new() -> Self {
        Self {
            state: Mutex::new(TestProviderState::default()),
            fail_attach_reply_once: AtomicBool::new(false),
            gate: None,
        }
    }

    fn fail_attach_reply_once() -> Self {
        Self {
            fail_attach_reply_once: AtomicBool::new(true),
            ..Self::new()
        }
    }

    fn gated(gate: Arc<AttachGate>) -> Self {
        Self {
            gate: Some(gate),
            ..Self::new()
        }
    }
}

#[async_trait::async_trait]
impl DurableProviderEpoch for TestProvider {
    type Binding = DurableEpochId;
    type Error = TestProviderError;

    async fn attach_epoch(
        &self,
        request: ProviderEpochAttachRequest<'_>,
    ) -> Result<AttachedProviderEpoch<Self::Binding>, Self::Error> {
        let (durable_epoch_id, receipt) = {
            let mut state = self.state.lock().unwrap();
            state.attach_calls += 1;
            let durable_epoch_id = request.durable_epoch_id().clone();
            let receipt = match state.sessions.get(&durable_epoch_id) {
                Some(receipt) => receipt.clone(),
                None => {
                    assert_eq!(request.system(), "stable system");
                    assert_eq!(request.tools().len(), 1);
                    state.remote_system_receives += 1;
                    let receipt = ProviderEpochReceipt::new(
                        "test-provider",
                        1,
                        durable_epoch_id.clone(),
                        request.fingerprint().clone(),
                        json!({ "remote_session": "session-7" }),
                    )
                    .unwrap();
                    state
                        .sessions
                        .insert(durable_epoch_id.clone(), receipt.clone());
                    receipt
                }
            };
            (durable_epoch_id, receipt)
        };

        if let Some(gate) = &self.gate {
            if !gate.entered.swap(true, Ordering::SeqCst) {
                gate.entered_notify.notify_waiters();
                gate.release.notified().await;
            }
        }
        if self.fail_attach_reply_once.swap(false, Ordering::SeqCst) {
            return Err(TestProviderError::LostAttachReply);
        }
        Ok(AttachedProviderEpoch::new(durable_epoch_id, receipt))
    }

    async fn rehydrate_epoch(
        &self,
        request: ProviderEpochRehydrateRequest<'_>,
    ) -> Result<Self::Binding, Self::Error> {
        let mut state = self.state.lock().unwrap();
        state.rehydrate_calls += 1;
        assert_eq!(request.receipt().adapter(), "test-provider");
        assert_eq!(request.receipt().schema_version(), 1);
        state.rehydrate_cursors.push(request.cursor().cloned());
        Ok(request.durable_epoch_id().clone())
    }
}

fn first_mount(
    manifest: &EpochContractManifest,
    system_calls: &Arc<AtomicUsize>,
) -> FirstEpochMount<&'static str> {
    system_calls.fetch_add(1, Ordering::SeqCst);
    FirstEpochMount::new(manifest.clone(), "stable system", "mounted-runtime")
}

#[tokio::test]
async fn first_open_then_reopen_renders_and_attaches_exactly_once() {
    let store = TestEpochStore::new();
    let provider = TestProvider::new();
    let manifest = manifest("v1", "Inspect one location");
    let binder = binder(&manifest);
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();

    let first = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await
    .unwrap();
    assert_eq!(first.kind(), DurableEpochOpenKind::Created);

    let reopened = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await
    .unwrap();
    assert_eq!(reopened.kind(), DurableEpochOpenKind::Rehydrated);
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    assert_eq!(binder.calls.load(Ordering::SeqCst), 1);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.attach_calls, 1);
    assert_eq!(provider.remote_system_receives, 1);
    assert_eq!(provider.rehydrate_calls, 1);
}

#[tokio::test]
async fn reopen_rehydrates_the_accepted_provider_cursor_without_rerendering_system() {
    let store = TestEpochStore::new();
    let provider = TestProvider::new();
    let manifest = manifest("v1", "Inspect one location");
    let binder = binder(&manifest);
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();

    let first = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await
    .unwrap();
    let cursor = ProviderTurnCursor::new(
        "test-provider",
        1,
        first.artifact().rendered().durable_epoch_id().clone(),
        first.artifact().rendered().fingerprint().clone(),
        json!({ "remote_turn": 4 }),
    )
    .unwrap();

    let reopened = open_durable_epoch(
        &store,
        &binder,
        &provider,
        &session_id,
        &manifest,
        Some(&cursor),
        |_| -> Result<FirstEpochMount<&'static str>, Infallible> {
            panic!("an existing epoch must not render System again")
        },
    )
    .await
    .unwrap();

    assert_eq!(reopened.kind(), DurableEpochOpenKind::Rehydrated);
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    assert_eq!(binder.calls.load(Ordering::SeqCst), 1);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.remote_system_receives, 1);
    assert_eq!(provider.rehydrate_cursors, [Some(cursor)]);
}

#[tokio::test]
async fn wrong_provider_cursor_is_rejected_before_rehydration_or_system_rerender() {
    let store = TestEpochStore::new();
    let provider = TestProvider::new();
    let manifest = manifest("v1", "Inspect one location");
    let binder = binder(&manifest);
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();

    let first = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await
    .unwrap();
    let wrong_cursor = ProviderTurnCursor::new(
        "another-provider",
        1,
        first.artifact().rendered().durable_epoch_id().clone(),
        first.artifact().rendered().fingerprint().clone(),
        json!({ "remote_turn": 4 }),
    )
    .unwrap();

    let result = open_durable_epoch(
        &store,
        &binder,
        &provider,
        &session_id,
        &manifest,
        Some(&wrong_cursor),
        |_| -> Result<FirstEpochMount<&'static str>, Infallible> {
            panic!("an existing epoch must not render System again")
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(DurableEpochCoordinatorError::Phase {
            phase: "provider cursor validation",
            ..
        })
    ));
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.rehydrate_calls, 0);
    assert!(provider.rehydrate_cursors.is_empty());
}

#[tokio::test]
async fn cursor_cannot_cross_create_admission_before_an_active_artifact_exists() {
    let store = TestEpochStore::new();
    let provider = TestProvider::new();
    let manifest = manifest("v1", "Inspect one location");
    let binder = binder(&manifest);
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();
    let stale_cursor = ProviderTurnCursor::new(
        "test-provider",
        1,
        DurableEpochId::new("forgotten-city/another-epoch").unwrap(),
        EpochArtifactFingerprint::from_canonical_bytes(b"another artifact"),
        json!({ "remote_turn": 4 }),
    )
    .unwrap();

    let result = open_durable_epoch(
        &store,
        &binder,
        &provider,
        &session_id,
        &manifest,
        Some(&stale_cursor),
        {
            let system_calls = Arc::clone(&system_calls);
            let manifest = manifest.clone();
            move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(DurableEpochCoordinatorError::Phase {
            phase: "provider cursor validation",
            ..
        })
    ));
    assert_eq!(system_calls.load(Ordering::SeqCst), 0);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.attach_calls, 0);
    assert_eq!(provider.rehydrate_calls, 0);
}

#[tokio::test]
async fn epoch_fence_rejects_manifest_and_rendered_artifact_substitution() {
    let store = TestEpochStore::new();
    let expected_manifest = manifest("v1", "Inspect one location");
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();
    let EpochOpenAdmission::Create { fence } = store
        .acquire_epoch(EpochOpenRequest::new(&session_id, &expected_manifest))
        .await
        .unwrap()
    else {
        panic!("an empty store must issue the first epoch fence");
    };

    let wrong_manifest = manifest("v2", "Inspect one location");
    let wrong_rendered = RenderedEpochArtifact::new(
        fence.durable_epoch_id().clone(),
        wrong_manifest,
        "stable system",
    )
    .unwrap();
    assert!(store
        .store_rendered_epoch(&fence, &wrong_rendered)
        .await
        .is_err());

    let rendered = RenderedEpochArtifact::new(
        fence.durable_epoch_id().clone(),
        expected_manifest.clone(),
        "stable system",
    )
    .unwrap();
    store.store_rendered_epoch(&fence, &rendered).await.unwrap();

    let substituted = RenderedEpochArtifact::new(
        fence.durable_epoch_id().clone(),
        expected_manifest,
        "substituted system",
    )
    .unwrap();
    let wrong_receipt = ProviderEpochReceipt::new(
        "test-provider",
        1,
        fence.durable_epoch_id().clone(),
        substituted.fingerprint().clone(),
        json!({ "remote_session": "session-7" }),
    )
    .unwrap();
    let wrong_active = ActiveEpochArtifact::new(substituted, wrong_receipt).unwrap();
    assert!(store.activate_epoch(&fence, &wrong_active).await.is_err());

    let receipt = ProviderEpochReceipt::new(
        "test-provider",
        1,
        fence.durable_epoch_id().clone(),
        rendered.fingerprint().clone(),
        json!({ "remote_session": "session-7" }),
    )
    .unwrap();
    let active = ActiveEpochArtifact::new(rendered, receipt).unwrap();
    store.activate_epoch(&fence, &active).await.unwrap();
    let state = store.state.lock().unwrap();
    let TestEpochState::Active(stored) = &*state else {
        panic!("the exact persisted artifact should activate");
    };
    assert_eq!(stored, &active);
}

#[tokio::test]
async fn concurrent_open_is_fenced_before_a_second_system_render() {
    let store = Arc::new(TestEpochStore::new());
    let gate = Arc::new(AttachGate::new());
    let provider = Arc::new(TestProvider::gated(Arc::clone(&gate)));
    let manifest = Arc::new(manifest("v1", "Inspect one location"));
    let binder = Arc::new(binder(&manifest));
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = Arc::new(DurableSessionId::new("forgotten-city/player").unwrap());

    let first = tokio::spawn({
        let store = Arc::clone(&store);
        let provider = Arc::clone(&provider);
        let manifest = Arc::clone(&manifest);
        let binder = Arc::clone(&binder);
        let system_calls = Arc::clone(&system_calls);
        let session_id = Arc::clone(&session_id);
        async move {
            open_durable_epoch(
                store.as_ref(),
                binder.as_ref(),
                provider.as_ref(),
                &session_id,
                &manifest,
                None,
                {
                    let system_calls = Arc::clone(&system_calls);
                    let manifest = Arc::clone(&manifest);
                    move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
                },
            )
            .await
        }
    });
    gate.wait_until_entered().await;

    let second = open_durable_epoch(
        store.as_ref(),
        binder.as_ref(),
        provider.as_ref(),
        &session_id,
        &manifest,
        None,
        {
            let system_calls = Arc::clone(&system_calls);
            let manifest = Arc::clone(&manifest);
            move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
        },
    )
    .await;
    assert!(matches!(
        second,
        Err(DurableEpochCoordinatorError::InFlight {
            lease_expires_at_unix_ms: 11_000,
            ..
        })
    ));
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    // `notify_one` retains a permit when the attachment task has observed the
    // gate but has not registered its waiter yet. `notify_waiters` would lose
    // that release and make this concurrency proof occasionally hang.
    gate.release.notify_one();
    assert_eq!(
        first.await.unwrap().unwrap().kind(),
        DurableEpochOpenKind::Created
    );
    assert_eq!(provider.state.lock().unwrap().attach_calls, 1);
}

#[tokio::test]
async fn cancelled_attachment_reopens_from_rendered_artifact_without_rerendering_system() {
    let store = Arc::new(TestEpochStore::new());
    let gate = Arc::new(AttachGate::new());
    let provider = Arc::new(TestProvider::gated(Arc::clone(&gate)));
    let manifest = Arc::new(manifest("v1", "Inspect one location"));
    let binder = Arc::new(binder(&manifest));
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = Arc::new(DurableSessionId::new("forgotten-city/player").unwrap());

    let cancelled = tokio::spawn({
        let store = Arc::clone(&store);
        let provider = Arc::clone(&provider);
        let manifest = Arc::clone(&manifest);
        let binder = Arc::clone(&binder);
        let system_calls = Arc::clone(&system_calls);
        let session_id = Arc::clone(&session_id);
        async move {
            open_durable_epoch(
                store.as_ref(),
                binder.as_ref(),
                provider.as_ref(),
                &session_id,
                &manifest,
                None,
                {
                    let system_calls = Arc::clone(&system_calls);
                    let manifest = Arc::clone(&manifest);
                    move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
                },
            )
            .await
        }
    });
    gate.wait_until_entered().await;
    cancelled.abort();
    match cancelled.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the gated epoch opener should have been cancelled"),
    }
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    {
        let provider = provider.state.lock().unwrap();
        assert_eq!(provider.attach_calls, 1);
        assert_eq!(provider.remote_system_receives, 1);
    }

    store.simulate_owner_loss();
    let reopened = open_durable_epoch(
        store.as_ref(),
        binder.as_ref(),
        provider.as_ref(),
        &session_id,
        &manifest,
        None,
        {
            let system_calls = Arc::clone(&system_calls);
            let manifest = Arc::clone(&manifest);
            move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
        },
    )
    .await
    .unwrap();

    assert_eq!(reopened.kind(), DurableEpochOpenKind::AttachmentResumed);
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    assert_eq!(binder.calls.load(Ordering::SeqCst), 1);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.attach_calls, 2);
    assert_eq!(provider.remote_system_receives, 1);
}

#[tokio::test]
async fn render_started_crash_requires_explicit_recovery_without_rerender() {
    let store = TestEpochStore::new();
    let provider = TestProvider::new();
    let manifest = manifest("v1", "Inspect one location");
    let binder = binder(&manifest);
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();

    let admission = store
        .acquire_epoch(EpochOpenRequest::new(&session_id, &manifest))
        .await
        .unwrap();
    assert!(matches!(admission, EpochOpenAdmission::Create { .. }));
    store.simulate_owner_loss();

    let reopen = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await;
    assert!(matches!(
        reopen,
        Err(DurableEpochCoordinatorError::RecoveryRequired {
            phase: EpochOpenRecoveryPhase::RenderStarted,
            ..
        })
    ));
    assert_eq!(system_calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.state.lock().unwrap().attach_calls, 0);
}

#[tokio::test]
async fn lost_attach_reply_retries_same_epoch_without_second_remote_system() {
    let store = TestEpochStore::new();
    let provider = TestProvider::fail_attach_reply_once();
    let manifest = manifest("v1", "Inspect one location");
    let binder = binder(&manifest);
    let system_calls = Arc::new(AtomicUsize::new(0));
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();

    let first = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await;
    assert!(matches!(
        first,
        Err(DurableEpochCoordinatorError::Phase {
            phase: "provider attachment",
            ..
        })
    ));
    let resumed = open_durable_epoch(&store, &binder, &provider, &session_id, &manifest, None, {
        let system_calls = Arc::clone(&system_calls);
        let manifest = manifest.clone();
        move |_| Ok::<_, Infallible>(first_mount(&manifest, &system_calls))
    })
    .await
    .unwrap();
    assert_eq!(resumed.kind(), DurableEpochOpenKind::AttachmentResumed);
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    assert_eq!(binder.calls.load(Ordering::SeqCst), 1);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.attach_calls, 2);
    assert_eq!(provider.remote_system_receives, 1);
}

#[tokio::test]
async fn changed_tool_manifest_is_rejected_before_render_or_provider_work() {
    let store = TestEpochStore::new();
    let provider = TestProvider::new();
    let initial = manifest("v1", "Inspect one location");
    let initial_binder = binder(&initial);
    let session_id = DurableSessionId::new("forgotten-city/player").unwrap();
    let system_calls = Arc::new(AtomicUsize::new(0));
    open_durable_epoch(
        &store,
        &initial_binder,
        &provider,
        &session_id,
        &initial,
        None,
        {
            let system_calls = Arc::clone(&system_calls);
            let initial = initial.clone();
            move |_| Ok::<_, Infallible>(first_mount(&initial, &system_calls))
        },
    )
    .await
    .unwrap();

    let changed = manifest("v1", "Inspect the whole district");
    let changed_binder = binder(&changed);
    let reopen = open_durable_epoch(
        &store,
        &changed_binder,
        &provider,
        &session_id,
        &changed,
        None,
        {
            let system_calls = Arc::clone(&system_calls);
            let changed = changed.clone();
            move |_| Ok::<_, Infallible>(first_mount(&changed, &system_calls))
        },
    )
    .await;
    assert!(matches!(
        reopen,
        Err(DurableEpochCoordinatorError::Conflict { .. })
    ));
    assert_eq!(system_calls.load(Ordering::SeqCst), 1);
    let provider = provider.state.lock().unwrap();
    assert_eq!(provider.attach_calls, 1);
    assert_eq!(provider.rehydrate_calls, 0);
}
