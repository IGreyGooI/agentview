//! External-consumer proof for durable streaming authoring from the narrow
//! component prelude. `author` intentionally imports no host, provider,
//! persistence, lifecycle, or raw component APIs.

mod author {
    use agentview::component::prelude::*;

    pub(super) struct ChoiceChannels;

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum ChoiceOutput {
        Selected(u32),
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum ChoiceLive {
        Opened(u32),
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum ChoiceDiagnostic {
        MissingIndex,
        InvalidIndex(String),
    }

    impl TurnChannels for ChoiceChannels {
        type Output = ChoiceOutput;
        type Live = ChoiceLive;
        type Commit = Never;
        type Diagnostic = ChoiceDiagnostic;
    }

    #[derive(Clone)]
    pub(super) struct ChoiceTurn {
        pub(super) task: String,
    }

    #[derive(Clone, AgentView)]
    #[agent_view(document)]
    struct ChoiceSystem {
        #[view(paragraph)]
        policy: &'static str,
    }

    #[derive(Clone, AgentView)]
    #[agent_view(document)]
    struct ChoiceUser {
        #[view(paragraph)]
        task: String,
    }

    fn choose_contract() -> XmlNode {
        let mut contract = XmlNode::new(XmlName::try_from("choose").unwrap());
        contract
            .push_attribute(XmlName::try_from("index").unwrap(), "...")
            .unwrap();
        contract
    }

    #[view(component)]
    fn choose() -> DurableComponent<ChoiceChannels, ChoiceTurn> {
        StreamingXml::<TurnEmission<ChoiceChannels>, ChoiceDiagnostic>::new(choose_contract())
            .state_with(|| None::<u32>)
            .on_open(|selected, element| {
                let Some(value) = element.attr("index") else {
                    return StreamUpdate::from_diagnostic(ChoiceDiagnostic::MissingIndex);
                };
                let index = match value.parse::<u32>() {
                    Ok(index) => index,
                    Err(_) => {
                        return StreamUpdate::from_diagnostic(ChoiceDiagnostic::InvalidIndex(
                            value.to_owned(),
                        ));
                    }
                };
                *selected = Some(index);
                StreamUpdate::from_emission(TurnEmission::Live(ChoiceLive::Opened(index)))
            })
            .on_complete(|selected, _| match *selected {
                Some(index) => {
                    StreamUpdate::from_emission(TurnEmission::Output(ChoiceOutput::Selected(index)))
                }
                None => StreamUpdate::from_diagnostic(ChoiceDiagnostic::MissingIndex),
            })
            .into_durable_component(RuntimeContract::new("review.choose", "v1")?)
    }

    pub(super) fn choice_system() -> DurableSystem<ChoiceChannels, ChoiceTurn> {
        durable_system((
            ChoiceSystem {
                policy: "Emit one choose element with an allowed index.",
            }
            .build_root()
            .expect("the static System POM is valid"),
            choose(),
        ))
    }

    fn choice_user_document(props: &ChoiceTurn) -> Result<Document, PomError> {
        ChoiceUser {
            task: props.task.clone(),
        }
        .build_root()
    }

    pub(super) fn choice_user(context: UserTurnContext<'_, ChoiceTurn>) -> UserView {
        user_view(choice_user_document(context.props()).expect("the typed User POM is valid"))
    }

    #[view(component)]
    pub(super) fn choice_agent() -> MountedFeature<ChoiceChannels, ChoiceTurn> {
        MountedFeature::try_new(choice_system(), |turn| choice_user_document(turn.props()))
    }
}

mod host {
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use agentview::{
        component::{
            advanced::lifecycle::{
                mount_system_epoch_with_contract, SystemMountContext, SystemView,
            },
            system_view, LiveAbortContext, LiveEffectAbortAck, LiveEffectContext,
            LiveEffectRuntime,
        },
        llm_call::TextTurnEvent,
    };

    use super::author::{
        self, ChoiceChannels, ChoiceDiagnostic, ChoiceLive, ChoiceOutput, ChoiceTurn,
    };

    struct RecordingLive(Arc<Mutex<Vec<u32>>>);

    #[async_trait::async_trait]
    impl LiveEffectRuntime<ChoiceLive> for RecordingLive {
        type Error = Infallible;

        async fn apply(
            &mut self,
            _context: &LiveEffectContext,
            effect: ChoiceLive,
        ) -> Result<(), Self::Error> {
            match effect {
                ChoiceLive::Opened(index) => self.0.lock().unwrap().push(index),
            }
            Ok(())
        }

        async fn abort(
            &mut self,
            context: &LiveAbortContext,
        ) -> Result<LiveEffectAbortAck, Self::Error> {
            Ok(if context.applied_effects() == 0 {
                LiveEffectAbortAck::NoEffectsApplied
            } else {
                LiveEffectAbortAck::CompensationCompleted
            })
        }
    }

    fn mount(_: SystemMountContext<'_, ()>) -> SystemView<ChoiceChannels, ChoiceTurn> {
        system_view(author::choice_system().into_one_shot_component())
    }

    pub(super) async fn exercise_author_component() -> anyhow::Result<()> {
        let epoch = mount_system_epoch_with_contract(
            agentview::component::EpochContractId::new("test/authoring-streaming-prelude/v1")?,
            &(),
            mount,
        )?;
        assert!(epoch.rendered_system().contains("choose"));

        let live = Arc::new(Mutex::new(Vec::new()));
        let props = ChoiceTurn {
            task: "Choose an intent for the current turn.".to_owned(),
        };
        let prepared = epoch
            .begin_turn("authoring-streaming")
            .prepare_user(&props, author::choice_user)?;
        let mut attempt = prepared.start_streaming_attempt(RecordingLive(Arc::clone(&live)))?;

        let update = attempt
            .on_event(TextTurnEvent::TextDelta(
                "<choose index=\"7\" />".to_owned(),
            ))
            .await?;
        assert_eq!(*live.lock().unwrap(), vec![7]);
        assert_eq!(
            update.emissions(),
            &[agentview::component::TurnEmission::Output(
                ChoiceOutput::Selected(7)
            )]
        );
        assert!(update.diagnostics().is_empty());

        let finished = attempt
            .finish_stream()
            .await
            .expect("the well-formed streaming contract should finish");
        assert!(finished.update().emissions().is_empty());
        assert!(finished.update().diagnostics().is_empty());

        let invalid = epoch
            .begin_turn("authoring-streaming-invalid")
            .prepare_user(&props, author::choice_user)?;
        let mut invalid_attempt =
            invalid.start_streaming_attempt(RecordingLive(Arc::clone(&live)))?;
        let invalid_update = invalid_attempt
            .on_event(TextTurnEvent::TextDelta("<choose />".to_owned()))
            .await?;
        assert!(invalid_update.emissions().is_empty());
        assert_eq!(
            invalid_update.diagnostics(),
            &[
                ChoiceDiagnostic::MissingIndex,
                ChoiceDiagnostic::MissingIndex
            ]
        );
        let _finished = invalid_attempt
            .finish_stream()
            .await
            .expect("a typed validation diagnostic should not fail the stream");

        Ok(())
    }
}

#[test]
fn narrow_prelude_builds_typed_system_user_and_streaming_feature() {
    let definition = author::choice_agent().into_harness(
        agentview::component::EpochContractId::new("test/authoring-streaming-prelude/v1").unwrap(),
    );
    let _ = definition;
}

#[tokio::test]
async fn narrow_prelude_streaming_reducer_runs_under_a_host_owned_live_runtime(
) -> anyhow::Result<()> {
    host::exercise_author_component().await
}
