use std::{
    num::{NonZeroU128, NonZeroU64},
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::component::{
    execution::{
        Application, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderFactStream,
        ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch,
        TargetIdentity,
    },
    prelude::*,
};
use async_trait::async_trait;
use futures::{poll, FutureExt};
use tokio::sync::Notify;

#[derive(Default)]
struct Observed {
    declarations: AtomicUsize,
    submissions: AtomicUsize,
}

struct ObservedPort {
    observed: Arc<Observed>,
    declaration: TargetDeclaration,
}

type MountedDemandApplication = (
    Application<ObservedPort>,
    Arc<Observed>,
    Arc<Mutex<Option<ReactionRequest>>>,
    Arc<Mutex<Option<Signal<String>>>>,
);

#[async_trait]
impl ReactionPort for ObservedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.observed.declarations.fetch_add(1, Ordering::SeqCst);
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.observed.submissions.fetch_add(1, Ordering::SeqCst);
        frame.check_handoff_precondition(&self.declaration)?;
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[derive(Clone)]
struct DemandProps {
    request: Arc<Mutex<Option<ReactionRequest>>>,
    signal: Arc<Mutex<Option<Signal<String>>>>,
}

#[component]
fn demand_component(props: DemandProps) -> Component {
    let signal = use_signal(|| String::from("initial"));
    *props.request.lock().unwrap() = Some(use_reaction_request());
    *props.signal.lock().unwrap() = Some(signal.clone());
    let value = signal.with(Clone::clone).unwrap();
    view! { demand { "{value}" } }
}

#[derive(Clone)]
struct PanicDemandProps {
    release: Arc<Notify>,
}

#[component]
fn panicking_demand_component(props: PanicDemandProps) -> Component {
    let release = Arc::clone(&props.release);
    use_future(move || async move {
        release.notified().await;
        std::panic::panic_any(String::from("public demand panic"));
    });
    view! { panic_demand { "mounted" } }
}

fn declaration() -> TargetDeclaration {
    TargetDeclaration::full(
        TargetIdentity::new(NonZeroU128::new(1).unwrap()),
        TargetEpoch::new(NonZeroU64::new(1).unwrap()),
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 4_096,
                max_component_bytes: 1_024,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::NONE,
        ),
    )
}

fn mounted_demand_application() -> MountedDemandApplication {
    let request = Arc::new(Mutex::new(None));
    let signal = Arc::new(Mutex::new(None));
    let request_for_root = Arc::clone(&request);
    let signal_for_root = Arc::clone(&signal);
    let observed = Arc::new(Observed::default());
    let port = ObservedPort {
        observed: Arc::clone(&observed),
        declaration: declaration(),
    };
    let application = Application::mount(
        move || {
            demand_component(DemandProps {
                request: Arc::clone(&request_for_root),
                signal: Arc::clone(&signal_for_root),
            })
        },
        port,
    )
    .unwrap();
    (application, observed, request, signal)
}

#[tokio::test]
async fn public_demand_consumers_preserve_sticky_coalesced_requests_without_rendering_or_submit() {
    let (mut application, observed, request, signal) = mounted_demand_application();
    let request = request.lock().unwrap().clone().unwrap();
    let signal = signal.lock().unwrap().clone().unwrap();
    let revision = application.current_projection().revision();
    assert_eq!(observed.declarations.load(Ordering::SeqCst), 1);
    assert_eq!(observed.submissions.load(Ordering::SeqCst), 0);

    signal.set(String::from("published")).unwrap();
    request.request().unwrap();
    request.request().unwrap();
    request.request().unwrap();

    assert!(application.take_reaction_request().unwrap());
    assert!(!application.take_reaction_request().unwrap());
    assert_eq!(application.current_projection().revision(), revision);
    assert!(application.current_projection().is_dirty());
    assert_eq!(observed.submissions.load(Ordering::SeqCst), 0);

    request.request().unwrap();
    application.wait_for_reaction_request().await.unwrap();
    assert_eq!(application.current_projection().revision(), revision);
    assert!(application.current_projection().is_dirty());
    application.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_waiter_observes_a_request_arriving_after_it_is_pending() {
    let (mut application, _observed, request, _signal) = mounted_demand_application();
    let request = request.lock().unwrap().clone().unwrap();
    let mut waiting = Box::pin(application.wait_for_reaction_request());
    assert!(poll!(waiting.as_mut()).is_pending());

    request.request().unwrap();
    waiting.await.unwrap();
    application.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_public_wait_resumes_the_original_component_task_panic() {
    let release = Arc::new(Notify::new());
    let release_for_root = Arc::clone(&release);
    let port = ObservedPort {
        observed: Arc::new(Observed::default()),
        declaration: declaration(),
    };
    let mut application = Application::mount(
        move || {
            panicking_demand_component(PanicDemandProps {
                release: Arc::clone(&release_for_root),
            })
        },
        port,
    )
    .unwrap();
    let mut waiting = Box::pin(application.wait_for_reaction_request());
    assert!(poll!(waiting.as_mut()).is_pending());

    release.notify_one();
    let panic = tokio::time::timeout(
        Duration::from_secs(1),
        AssertUnwindSafe(waiting).catch_unwind(),
    )
    .await
    .expect("pending public wait must observe the released task panic")
    .expect_err("public demand boundary must resume the task panic");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some("public demand panic")
    );

    assert!(application.shutdown().await.is_err());
}
