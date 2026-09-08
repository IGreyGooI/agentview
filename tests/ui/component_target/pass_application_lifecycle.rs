use std::{
    num::{NonZeroU128, NonZeroU64},
    ops::ControlFlow,
};

use agentview::component::{
    execution::{
        Application, ApplicationFault, ApplicationFaultCode, ApplicationFaultKind,
        ApplicationFaultReason, ApplicationFaultStage, Frame, FrameCapabilities, FrameConstraints,
        FrameProfile, ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault,
        SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
    },
    prelude::{component, view, Component},
};
use async_trait::async_trait;

struct BorrowingPort {
    declaration: TargetDeclaration,
    facts: Vec<Result<ProviderFact, ReactionPortFault>>,
}

#[async_trait]
impl ReactionPort for BorrowingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        Ok(Box::pin(futures::stream::iter(self.facts.iter().cloned())))
    }
}

#[component]
fn root() -> Component {
    view! { lifecycle { "mounted" } }
}

fn main() {
    futures::executor::block_on(async {
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
        let port = BorrowingPort {
            declaration: TargetDeclaration::full(identity, epoch, profile),
            facts: vec![Ok(ProviderFact::ReactionCompleted { primary_text: None })],
        };
        let mut application = Application::mount(root, port).unwrap();
        let snapshot = application.current_projection();
        let _ = (
            snapshot.projection(),
            snapshot.revision(),
            snapshot.is_dirty(),
        );
        let _ = application.take_reaction_request().unwrap();
        let wait = application.wait_for_reaction_request();
        drop(wait);
        match application.react().await {
            Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(_)) => {}
            Err(fault) => inspect_fault(fault),
        }
        application.shutdown().await.unwrap();
    });
}

fn inspect_fault(fault: ApplicationFault) {
    let _: (
        ApplicationFaultKind,
        ApplicationFaultCode,
        ApplicationFaultStage,
        ApplicationFaultReason,
    ) = (fault.kind(), fault.code(), fault.stage(), fault.reason());
}
