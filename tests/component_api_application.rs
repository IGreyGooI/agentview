use std::{
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use agentview::component::{
    execution::{
        Application, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderFact,
        ProviderFactStream, ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration,
        TargetEpoch, TargetIdentity,
    },
    prelude::{component, view, Component},
};
use async_trait::async_trait;

#[derive(Default)]
struct Observed {
    declarations: AtomicUsize,
    submissions: AtomicUsize,
    accepted_frames: AtomicUsize,
}

impl Observed {
    fn declare_count(&self) -> usize {
        self.declarations.load(Ordering::SeqCst)
    }

    fn submit_count(&self) -> usize {
        self.submissions.load(Ordering::SeqCst)
    }

    fn accepted_frames(&self) -> usize {
        self.accepted_frames.load(Ordering::SeqCst)
    }
}

struct ObservedPort {
    declaration: TargetDeclaration,
    observed: Arc<Observed>,
}

#[async_trait]
impl ReactionPort for ObservedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.observed.declarations.fetch_add(1, Ordering::SeqCst);
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.observed.submissions.fetch_add(1, Ordering::SeqCst);
        frame.check_handoff_precondition(&self.declaration)?;
        self.observed.accepted_frames.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderFact::ReactionCompleted { primary_text: None })
        })))
    }
}

#[component]
fn root() -> Component {
    view! { application_lifecycle { "mounted" } }
}

#[tokio::test]
async fn public_application_mounts_reacts_and_shuts_down_through_the_curated_owner_boundary() {
    let observed = Arc::new(Observed::default());
    let identity = TargetIdentity::new(NonZeroU128::new(1).unwrap());
    let epoch = TargetEpoch::new(NonZeroU64::new(1).unwrap());
    let profile = FrameProfile::new(
        FrameConstraints {
            max_frame_bytes: 4_096,
            max_component_bytes: 1_024,
            context_window_tokens: None,
            reserved_output_tokens: None,
        },
        FrameCapabilities::NONE,
    );
    let port = ObservedPort {
        declaration: TargetDeclaration::full(identity, epoch, profile),
        observed: Arc::clone(&observed),
    };

    let mut application = Application::mount(root, port).unwrap();
    assert_eq!(observed.declare_count(), 1);
    assert_eq!(observed.submit_count(), 0);
    let initial_revision = application.current_projection().revision();

    assert_eq!(
        application.react().await.unwrap(),
        ControlFlow::Continue(())
    );

    assert_eq!(observed.submit_count(), 1);
    assert_eq!(observed.accepted_frames(), 1);
    assert!(application.current_projection().revision() >= initial_revision);
    application.shutdown().await.unwrap();
}
