use std::num::{NonZeroU128, NonZeroU64};

use agentview::component::{
    execution::{
        Application, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderFactStream,
        ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch,
        TargetIdentity,
    },
    prelude::{component, view, Component},
};
use async_trait::async_trait;

struct Port {
    declaration: TargetDeclaration,
}

#[async_trait]
impl ReactionPort for Port {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[component]
fn root() -> Component {
    view! { shutdown_owner { "mounted" } }
}

async fn shutdown_and_reuse(application: Application<Port>) {
    application.shutdown().await.unwrap();
    let _ = application.current_projection();
}

fn main() {
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
    let application = Application::mount(
        root,
        Port {
            declaration: TargetDeclaration::full(identity, epoch, profile),
        },
    )
    .unwrap();
    let _ = shutdown_and_reuse(application);
}
