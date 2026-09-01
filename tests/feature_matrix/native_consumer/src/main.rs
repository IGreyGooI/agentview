use std::convert::Infallible;
use std::num::{NonZeroU128, NonZeroU64};

use agentview::component::{
    execution::{
        Application, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderEvent,
        ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault, SubmitFault,
        TargetDeclaration, TargetEpoch, TargetIdentity,
    },
    prelude::{component, use_provider_event_handler, view, Component},
};
use async_trait::async_trait;

struct NativePort {
    declaration: TargetDeclaration,
}

#[async_trait]
impl ReactionPort for NativePort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(
        &'a mut self,
        _frame: Frame,
    ) -> Result<ProviderFactStream<'a>, SubmitFault> {
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderFact::ReactionCompleted { primary_text: None })
        })))
    }
}

#[component]
fn root() -> Component {
    use_provider_event_handler(ProviderEvent::TEXT, |_event| async {
        Ok::<(), Infallible>(())
    });
    view! { native_consumer { "mounted" } }
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
    let port = NativePort {
        declaration: TargetDeclaration::full(identity, epoch, profile),
    };
    let _application = Application::mount(root, port).unwrap();
}
