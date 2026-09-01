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
    view! { private_state { "mounted" } }
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
    let port = Port {
        declaration: TargetDeclaration::full(identity, epoch, profile),
    };
    let mut application: Application<Port> = Application::mount(root, port).unwrap();
    let _ = application.port();
    let _ = application.port_mut();
    let _ = application.session();
    let _ = application.session_mut();
}
