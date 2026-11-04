#[path = "support/pom.rs"]
mod pom_support;

use std::{
    num::{NonZeroU128, NonZeroU64},
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, Frame, FrameCapabilities, FrameConstraints, FrameProfile,
            FrameRevision, ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort,
            ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
        ApplicationId, ComponentHost, EventContext, EventSource,
    },
    llm_call::TextTurnEvent,
    pom::ResolvedDocument,
};
use async_trait::async_trait;
use futures::StreamExt;
use pom_support::{combine, item_documents, projection_document, InspectPom};
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct Probe {
    events: Arc<Mutex<Vec<(&'static str, EventContext)>>>,
    ids: Arc<Mutex<Vec<ApplicationId>>>,
    delivered: Arc<Notify>,
    frames: Arc<Mutex<Vec<FrameRevision>>>,
    views: Arc<Mutex<Vec<ResolvedDocument>>>,
}

#[component]
fn identity_child(probe: Probe) -> Component {
    probe.ids.lock().unwrap().push(use_application_id());
    view! { child { "Ready." } }
}

#[component]
fn identity_root(probe: Probe) -> Component {
    let id = agentview::component::prelude::use_application_id();
    probe.ids.lock().unwrap().push(id);
    view! { { identity_child(probe) } }
}

#[test]
fn application_identity_is_shared_by_components_and_stable_across_render_and_remount() {
    let probe = Probe::default();
    let mut host = ComponentHost::new_root(identity_root, probe.clone());
    let application_id = host.application_id();
    host.render().unwrap();
    host.render().unwrap();
    host.remount(probe.clone()).unwrap();
    host.render().unwrap();
    let ids = probe.ids.lock().unwrap();
    assert_eq!(ids.len(), 6);
    assert!(ids.iter().all(|id| *id == application_id));

    let other = ComponentHost::new_root(identity_root, Probe::default());
    assert_ne!(application_id, other.application_id());
}

#[component]
fn observed_reaction(probe: Probe) -> Component {
    probe.ids.lock().unwrap().push(use_application_id());
    let feedback = use_signal(|| String::from("waiting"));
    let first = probe.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |ctx, event| {
        let probe = first.clone();
        async move {
            tokio::task::yield_now().await;
            let label = match event {
                TextTurnEvent::TextDelta(_) => "first:delta",
                TextTurnEvent::TextComplete(_) => "first:complete",
            };
            probe.events.lock().unwrap().push((label, ctx));
            Ok::<(), std::convert::Infallible>(())
        }
    });
    let second = probe.clone();
    use_provider_event_handler(ProviderEvent::TEXT, move |ctx, event| {
        let probe = second.clone();
        async move {
            tokio::task::yield_now().await;
            let label = match event {
                TextTurnEvent::TextDelta(_) => "second:delta",
                TextTurnEvent::TextComplete(_) => "second:complete",
            };
            probe.events.lock().unwrap().push((label, ctx));
            probe.delivered.notify_one();
            Ok::<(), std::convert::Infallible>(())
        }
    });
    let completed = probe.clone();
    let completed_feedback = feedback.clone();
    use_reaction_completion(move |ctx| async move {
        tokio::task::yield_now().await;
        completed
            .events
            .lock()
            .unwrap()
            .push(("completion:first", ctx));
        completed_feedback.set(String::from("completed"))
    });
    use_reaction_completion(move |ctx| async move {
        tokio::task::yield_now().await;
        probe
            .events
            .lock()
            .unwrap()
            .push(("completion:second", ctx));
        Ok::<(), std::convert::Infallible>(())
    });
    let current = feedback.with(Clone::clone).unwrap();
    view! { feedback { "{current}" } }
}

struct ScriptedPort {
    declaration: TargetDeclaration,
    probe: Probe,
    stay_pending: bool,
}

impl ScriptedPort {
    fn new(probe: Probe, stay_pending: bool) -> Self {
        Self {
            declaration: TargetDeclaration::full(
                TargetIdentity::new(NonZeroU128::new(719).unwrap()),
                TargetEpoch::new(NonZeroU64::MIN),
                FrameProfile::new(
                    FrameConstraints {
                        max_frame_bytes: 65_536,
                        max_component_bytes: 8_192,
                        context_window: ContextWindow::NotApplicable,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true),
                ),
            ),
            probe,
            stay_pending,
        }
    }
}

#[async_trait]
impl ReactionPort for ScriptedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.probe.frames.lock().unwrap().push(frame.revision());
        self.probe
            .views
            .lock()
            .unwrap()
            .push(combine(item_documents(
                frame.submission().projection().items(),
            )));
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let output = ProviderOutputKey::new(8);
        let mut facts = vec![Ok(ProviderFact::TextDelta {
            output,
            phase: None,
            delta: String::from("result"),
        })];
        if !self.stay_pending {
            facts.extend([
                Ok(ProviderFact::TextSealed {
                    output,
                    phase: None,
                    text: String::from("result"),
                }),
                Ok(ProviderFact::ReactionCompleted {
                    primary_text: Some(output),
                }),
            ]);
        }
        let tail = futures::stream::pending().take(usize::from(self.stay_pending));
        Ok(Box::pin(futures::stream::iter(facts).chain(tail)))
    }
}

#[tokio::test]
async fn provider_multicast_and_completion_keep_frame_identity_and_order_across_reactions() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut application = Application::mount(
        move || observed_reaction(root_probe.clone()),
        ScriptedPort::new(probe.clone(), false),
    )
    .unwrap();
    let application_id = application.application_id();
    for _ in 0..2 {
        assert!(application.react().unwrap().await.unwrap().is_continue());
        assert!(
            projection_document(application.current_projection().projection())
                .has_field("feedback", "completed")
        );
    }
    let frames = probe.frames.lock().unwrap().clone();
    let events = probe.events.lock().unwrap().clone();
    let views = probe.views.lock().unwrap().clone();
    assert_eq!(frames.len(), 2);
    assert!(views[0].has_field("feedback", "waiting"));
    assert!(views[1].has_field("feedback", "completed"));
    assert_eq!(events.len(), 12);
    for (round, chunk) in events.chunks_exact(6).enumerate() {
        assert_eq!(
            chunk.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
            vec![
                "first:delta",
                "second:delta",
                "first:complete",
                "second:complete",
                "completion:first",
                "completion:second"
            ]
        );
        let reaction_id = chunk[0].1.reaction_id().expect("reaction provenance");
        for (_, context) in chunk {
            assert_eq!(context.application_id(), application_id);
            assert_eq!(context.reaction_id(), Some(reaction_id));
            assert_eq!(context.frame_revision(), Some(frames[round]));
        }
        assert_eq!(chunk[0].1, chunk[1].1);
        assert_eq!(chunk[2].1, chunk[3].1);
        assert_eq!(chunk[4].1, chunk[5].1);
        assert_ne!(chunk[0].1.event_id(), chunk[2].1.event_id());
        assert_ne!(chunk[2].1.event_id(), chunk[4].1.event_id());
        assert_eq!(
            chunk[0].1.source(),
            &EventSource::Provider {
                output: Some(ProviderOutputKey::new(8))
            }
        );
        assert_eq!(chunk[4].1.source(), &EventSource::ReactionCompleted);
    }
    assert_ne!(events[0].1.reaction_id(), events[6].1.reaction_id());
    assert!(probe
        .ids
        .lock()
        .unwrap()
        .iter()
        .all(|id| *id == application_id));
    application.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_reaction_keeps_delivered_context_without_dispatching_completion() {
    let probe = Probe::default();
    let root_probe = probe.clone();
    let mut application = Application::mount(
        move || observed_reaction(root_probe.clone()),
        ScriptedPort::new(probe.clone(), true),
    )
    .unwrap();
    let application_id = application.application_id();
    let reaction_control = application.react().unwrap();
    let mut reaction = Box::pin(std::future::IntoFuture::into_future(
        reaction_control.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            _ = probe.delivered.notified() => {}
            result = reaction.as_mut() => panic!("reaction completed unexpectedly: {result:?}"),
        }
    })
    .await
    .unwrap();
    reaction_control.interrupt().unwrap();
    drop(reaction);
    assert_eq!(
        reaction_control.await.unwrap(),
        agentview::component::execution::ReactionOutcome::Interrupted
    );
    let events = probe.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "first:delta");
    assert_eq!(events[1].0, "second:delta");
    assert_eq!(events[0].1, events[1].1);
    assert_eq!(events[0].1.application_id(), application_id);
    assert_eq!(
        events[0].1.frame_revision(),
        Some(probe.frames.lock().unwrap()[0])
    );
    application.shutdown().await.unwrap();
    assert_eq!(*probe.events.lock().unwrap(), events);
}
