use std::num::{NonZeroU128, NonZeroU64};

use agentview::component::execution::reaction::{
    Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderFact, ProviderFactStream,
    ReactionPort, ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason, SubmitFault,
    TargetDeclaration, TargetEpoch, TargetIdentity,
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
        let _structured_payload = (
            frame.prepared_profile(),
            frame.submission().replay(),
            frame.submission().staged_inputs(),
            frame.submission().projection().items(),
            frame.submission().tools().names(),
            frame.submission().canonical_bytes(),
        );
        Ok(Box::pin(futures::stream::iter(self.facts.iter().cloned())))
    }
}

fn assert_port<T: ReactionPort>() {}

fn main() {
    assert_port::<BorrowingPort>();

    let identity = TargetIdentity::new(NonZeroU128::new(1).unwrap());
    let epoch = TargetEpoch::new(NonZeroU64::new(1).unwrap());
    let declaration = TargetDeclaration::full(
        identity,
        epoch,
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 4_096,
                max_component_bytes: 1_024,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::NONE,
        ),
    );
    let mut port = BorrowingPort {
        declaration,
        facts: vec![Ok(ProviderFact::ReactionCompleted { primary_text: None })],
    };
    let _ = port.declare().unwrap();
    let _ = ReactionPortFault::terminal(
        ReactionPortFaultCode::Internal,
        ReactionPortFaultReason::Other,
    );
}
