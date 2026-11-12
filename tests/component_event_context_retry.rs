use std::{
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{Arc, Mutex},
};

use agentview::component::{
    execution::{
        Application, ContextWindow, Frame, FrameCapabilities, FrameConstraints, FrameProfile,
        FrameRevision, ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort,
        ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
    },
    prelude::*,
};
use async_trait::async_trait;

#[derive(Default)]
struct Observations {
    rejected: Vec<FrameRevision>,
    accepted: Vec<FrameRevision>,
    callbacks: Vec<EventContext>,
}

struct RetryPort {
    declaration: TargetDeclaration,
    observations: Arc<Mutex<Observations>>,
    retry: bool,
}

#[async_trait]
impl ReactionPort for RetryPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        if self.retry {
            self.retry = false;
            self.observations
                .lock()
                .unwrap()
                .rejected
                .push(frame.revision());
            self.declaration = TargetDeclaration::full(
                self.declaration.identity(),
                TargetEpoch::new(NonZeroU64::new(2).unwrap()),
                self.declaration.profile().clone(),
            );
            return Err(SubmitFault::ContinuityChanged);
        }
        self.observations
            .lock()
            .unwrap()
            .accepted
            .push(frame.revision());
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let output = ProviderOutputKey::new(1);
        Ok(Box::pin(futures::stream::iter([
            Ok(ProviderFact::TextDelta {
                output,
                phase: None,
                delta: "observed".to_owned(),
            }),
            Ok(ProviderFact::TextSealed {
                output,
                phase: None,
                text: "observed".to_owned(),
            }),
            Ok(ProviderFact::ReactionCompleted {
                primary_text: Some(output),
            }),
        ])))
    }
}

#[component]
fn collect_sources(observations: Arc<Mutex<Observations>>) -> Component {
    let events = Arc::clone(&observations);
    use_provider_event_handler(ProviderEvent::TEXT, move |context, _event| {
        let events = Arc::clone(&events);
        async move {
            events.lock().unwrap().callbacks.push(context);
            Ok::<_, Infallible>(())
        }
    });
    use_reaction_completion(move |context| async move {
        observations.lock().unwrap().callbacks.push(context);
        Ok::<_, Infallible>(())
    });
    view! { request { "Observe the current state." } }
}

#[tokio::test]
async fn continuity_retry_attributes_callbacks_only_to_the_successful_handoff() {
    let observations = Arc::new(Mutex::new(Observations::default()));
    let port = RetryPort {
        declaration: TargetDeclaration::full(
            TargetIdentity::new(NonZeroU128::new(901).unwrap()),
            TargetEpoch::new(NonZeroU64::new(1).unwrap()),
            FrameProfile::new(
                FrameConstraints {
                    max_frame_bytes: 1_048_576,
                    max_component_bytes: 262_144,
                    context_window: ContextWindow::NotApplicable,
                    reserved_output_tokens: None,
                },
                FrameCapabilities::NONE
                    .with_native_tool_calls(true)
                    .with_text_delta_output(true),
            ),
        ),
        observations: Arc::clone(&observations),
        retry: true,
    };
    let state = Arc::clone(&observations);
    let prepared = Arc::new(Mutex::new(Vec::new()));
    let sink = prepared.clone();
    let mut app = Application::mount(move || collect_sources(Arc::clone(&state)), port)
        .unwrap()
        .with_frame_observer(move |event| {
            sink.lock().unwrap().push((
                event.reaction_id(),
                event.projection_revision(),
                event.frame().revision(),
            ))
        });
    let application_id = app.application_id();
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    assert!(app.react(false).unwrap().await.unwrap().is_continue());
    {
        let observed = observations.lock().unwrap();
        assert_eq!(observed.rejected.len(), 1);
        assert_eq!(observed.accepted.len(), 2);
        assert_eq!(observed.callbacks.len(), 6);
        let attempts = prepared.lock().unwrap();
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[0].0, attempts[1].0);
        assert_eq!(attempts[0].1, attempts[1].1);
        assert_eq!(attempts[0].2, observed.rejected[0]);
        assert_eq!(attempts[1].2, observed.accepted[0]);
        assert_eq!(attempts[2].2, observed.accepted[1]);
        for (reaction, contexts) in observed.callbacks.chunks_exact(3).enumerate() {
            assert!(contexts[0].reaction_id().is_some());
            for context in contexts {
                assert_eq!(context.application_id(), application_id);
                assert_eq!(context.reaction_id(), contexts[0].reaction_id());
                assert_eq!(context.frame_revision(), Some(observed.accepted[reaction]));
                assert_ne!(context.frame_revision(), Some(observed.rejected[0]));
            }
            assert_ne!(contexts[0].event_id(), contexts[1].event_id());
            assert_ne!(contexts[1].event_id(), contexts[2].event_id());
            assert_eq!(
                contexts[0].source(),
                &EventSource::Provider {
                    output: Some(ProviderOutputKey::new(1))
                }
            );
            assert_eq!(contexts[1].source(), contexts[0].source());
            assert_eq!(contexts[2].source(), &EventSource::ReactionCompleted);
        }
        assert_ne!(
            observed.callbacks[0].reaction_id(),
            observed.callbacks[3].reaction_id()
        );
    }
    app.shutdown().await.unwrap();
}
