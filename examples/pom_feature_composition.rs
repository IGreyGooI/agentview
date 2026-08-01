//! Reusable `MountedFeature` composition with a pure turn-props projection.
//!
//! Each feature contributes both retained System/runtime declarations and one
//! fresh User POM fragment. The parent owns the larger per-turn snapshot and
//! projects the fields that each feature needs. The component declares a pure
//! provider capability contract; its dispatcher remains in the explicit host
//! module below. Capture, provider I/O, and persistence are host-owned, so this
//! executable program can prove one durable System attachment, fresh User POM
//! on each turn, and native-tool dispatch through the host binding registry.

use agentview::component::prelude::*;
use serde_json::json;

struct PlayerChannels;

impl TurnChannels for PlayerChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = String;
}

struct SelectIntentProps {
    intent_index: usize,
}

struct TaskProps {
    task: String,
}

struct PlayerTurnProps {
    select_intent: SelectIntentProps,
    task: TaskProps,
}

struct PlayerCall {
    intent_index: usize,
    task: String,
}

fn xml(name: &str) -> Document {
    Document::from_xml(XmlNode::new(
        XmlName::try_from(name).expect("static XML name is valid"),
    ))
}

fn world_lookup_contract() -> ProviderCapabilityContract {
    ProviderCapabilityContract::new(
        "example.world-lookup",
        "v1",
        [ProviderToolSpec::new(
            "lookup_world",
            "Read world state for the selected intent",
            json!({ "type": "object", "properties": {} }),
        )
        .expect("the static tool descriptor is valid")],
    )
    .expect("the static provider capability contract is valid")
}

fn text_xml(name: &str, value: impl AsRef<str>) -> Document {
    let mut node = XmlNode::new(XmlName::try_from(name).expect("static XML name is valid"));
    node.push(MixedContent::text(TextNode::new(value.as_ref())));
    Document::from_xml(node)
}

#[view(component)]
fn select_intent_feature() -> MountedFeature<PlayerChannels, SelectIntentProps> {
    MountedFeature::new(
        durable_system((
            xml("select_intent_rules"),
            StreamingXml::<TurnEmission<PlayerChannels>, String>::new(XmlNode::new(
                XmlName::try_from("select_intent").expect("the static XML route is valid"),
            ))
            .state_with(|| ())
            .into_durable_component(
                RuntimeContract::new("example.select-intent", "v1")
                    .expect("the static runtime contract is valid"),
            ),
            durable_provider_contract::<PlayerChannels, SelectIntentProps>(
                world_lookup_contract(),
                (),
            ),
        )),
        |context| {
            pom_view(text_xml(
                "intent_index",
                context.props().intent_index.to_string(),
            ))
        },
    )
}

#[view(component)]
fn task_feature() -> MountedFeature<PlayerChannels, TaskProps> {
    MountedFeature::new(durable_system(xml("task_policy")), |context| {
        pom_view(text_xml("task", &context.props().task))
    })
}

#[view(component)]
fn player_feature() -> MountedFeature<PlayerChannels, PlayerTurnProps> {
    select_intent_feature()
        .project_props(|player: &PlayerTurnProps| &player.select_intent)
        .compose(task_feature().project_props(|player: &PlayerTurnProps| &player.task))
}

/// The application host binds external I/O after the full feature tree has
/// projected child props into its final `PlayerTurnProps` snapshot. Nothing in
/// this module is part of component authoring or durable POM declaration.
mod host {
    use super::*;
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use agentview::{
        component::{
            advanced::provider::{
                AttachedProviderEpoch, DurableMountedProviderExecutor, FallibleProviderWirePort,
                MountedProviderExecutor, MountedProviderExit, MountedProviderRequest,
                ProviderAdapterContract, ProviderCancellationToken, ProviderDispatchContext,
                ProviderDispatchFailure, ProviderDispatchUpdate, ProviderDispatcher,
                ProviderDispatcherCx, ProviderEpochAttachRequest, ProviderEpochReceipt,
                ProviderEpochRehydrateRequest, ProviderToolCall, ProviderToolResponse,
                ProviderWireAck, ProviderWireEvent, ProviderWireFault,
            },
            host::prelude::*,
        },
        llm_call::{ContextPreparation, ContextPreparationBudget, ExecutorCommit},
        prelude::PromptContext,
    };
    use serde_json::json;

    /// Application I/O belongs here, before the pure component tree receives
    /// its owned per-turn props. This example needs no asynchronous I/O, but
    /// keeps the capture boundary visible for a real world snapshot.
    struct PlayerCapture;

    #[async_trait::async_trait]
    impl MountedTurnCapture for PlayerCapture {
        type Transcript = String;
        type ContextState = ();
        type CallProps = PlayerCall;
        type TurnProps = PlayerTurnProps;
        type Source = ();
        type Error = Infallible;

        async fn capture_turn_props(
            &self,
            context: TurnCaptureContext<'_, String, (), PlayerCall, ()>,
        ) -> Result<Self::TurnProps, Self::Error> {
            Ok(PlayerTurnProps {
                select_intent: SelectIntentProps {
                    intent_index: context.call_props().intent_index,
                },
                task: TaskProps {
                    task: context.call_props().task.clone(),
                },
            })
        }
    }

    #[derive(Debug)]
    struct WorldLookupDispatcher {
        intent_index: usize,
    }

    #[async_trait::async_trait]
    impl ProviderDispatcher<PlayerChannels> for WorldLookupDispatcher {
        type Error = Infallible;

        async fn dispatch(
            &mut self,
            _context: &ProviderDispatchContext,
            call: ProviderToolCall,
        ) -> Result<ProviderDispatchUpdate<PlayerChannels>, Self::Error> {
            debug_assert_eq!(call.name(), "lookup_world");
            Ok(ProviderDispatchUpdate::new(
                ProviderToolResponse::success(json!({
                    "intent_index": self.intent_index,
                    "location": "forum",
                })),
                StreamUpdate::new(),
            ))
        }
    }

    fn provider_dispatchers() -> ProviderDispatcherRegistry<PlayerChannels, PlayerTurnProps> {
        let mut registry = ProviderDispatcherRegistry::new();
        registry
            .register_with_context(
                world_lookup_contract(),
                "example.world-lookup-host/v1",
                |context: &ProviderDispatcherCx<'_, PlayerTurnProps, PlayerChannels>| {
                    Ok::<_, ProviderDispatchFailure>(WorldLookupDispatcher {
                        intent_index: context.props().select_intent.intent_index,
                    })
                },
            )
            .expect("the host registry matches the static provider contract");
        registry
    }

    #[derive(Debug, thiserror::Error)]
    enum ExampleProviderError {
        #[error("example provider wire failed: {0}")]
        Wire(#[from] ProviderWireFault),
        #[error("example provider expected a native-tool result acknowledgement")]
        MissingToolResult,
    }

    #[derive(Clone)]
    struct ExampleProviderEpoch(ProviderEpochReceipt);

    #[derive(Default)]
    struct ExampleProvider {
        systems: Mutex<Vec<String>>,
        users: Mutex<Vec<String>>,
        tool_results: Mutex<Vec<ProviderToolResponse>>,
    }

    impl ExampleProvider {
        fn counts(&self) -> (usize, usize, usize) {
            (
                self.systems
                    .lock()
                    .expect("example system log is available")
                    .len(),
                self.users
                    .lock()
                    .expect("example user log is available")
                    .len(),
                self.tool_results
                    .lock()
                    .expect("example tool result log is available")
                    .len(),
            )
        }
    }

    #[async_trait::async_trait]
    impl MountedProviderExecutor<String> for ExampleProvider {
        type Error = ExampleProviderError;
        type Epoch = ExampleProviderEpoch;

        async fn prepare_context(
            &self,
            _epoch: &Self::Epoch,
            _request: &MountedProviderRequest<String>,
            _budget: ContextPreparationBudget,
        ) -> Result<ContextPreparation<String>, Self::Error> {
            Ok(ContextPreparation::Ready)
        }

        async fn execute(
            &self,
            epoch: &Self::Epoch,
            request: MountedProviderRequest<String>,
            wire: &mut dyn FallibleProviderWirePort,
            _cancellation: ProviderCancellationToken,
        ) -> Result<MountedProviderExit<String>, Self::Error> {
            debug_assert_eq!(epoch.0.adapter(), "example.pom-feature-provider");
            self.users
                .lock()
                .expect("example user log is available")
                .push(request.user().to_owned());

            let acknowledgement = wire
                .submit(ProviderWireEvent::Tool(
                    ProviderToolCall::new(
                        "example-world-lookup",
                        "lookup_world",
                        json!({ "scope": "current_turn" }),
                    )
                    .with_result_correlation_id("example-world-result"),
                ))
                .await?;
            let ProviderWireAck::ToolResult { result, .. } = acknowledgement else {
                return Err(ExampleProviderError::MissingToolResult);
            };
            self.tool_results
                .lock()
                .expect("example tool result log is available")
                .push(result.response().clone());

            Ok(MountedProviderExit::completed(ExecutorCommit::empty()))
        }
    }

    #[async_trait::async_trait]
    impl DurableMountedProviderExecutor<String> for ExampleProvider {
        fn durable_provider_adapter_contract(&self) -> ProviderAdapterContract {
            ProviderAdapterContract::new("example.pom-feature-provider", 1)
                .expect("the example provider adapter contract is valid")
        }

        async fn attach_durable_epoch(
            &self,
            request: ProviderEpochAttachRequest<'_>,
        ) -> Result<AttachedProviderEpoch<Self::Epoch>, Self::Error> {
            self.systems
                .lock()
                .expect("example system log is available")
                .push(request.system().to_owned());
            let receipt = ProviderEpochReceipt::new(
                "example.pom-feature-provider",
                1,
                request.durable_epoch_id().clone(),
                request.fingerprint().clone(),
                json!({ "native_tool_count": request.tools().len() }),
            )
            .expect("the example provider receipt is valid");
            Ok(AttachedProviderEpoch::new(
                ExampleProviderEpoch(receipt.clone()),
                receipt,
            ))
        }

        async fn rehydrate_durable_epoch(
            &self,
            request: ProviderEpochRehydrateRequest<'_>,
        ) -> Result<Self::Epoch, Self::Error> {
            Ok(ExampleProviderEpoch(request.receipt().clone()))
        }
    }

    #[derive(Default)]
    struct WaitReducer;

    impl SessionReducer<String, (), PlayerChannels> for WaitReducer {
        type Error = Infallible;

        fn reduce(
            &self,
            _session: &mut AgentSession<String, ()>,
            _context: SessionReduceContext<'_, String, PlayerChannels>,
            _executor_commit: ExecutorCommit<String>,
        ) -> Result<TurnFlow, Self::Error> {
            Ok(TurnFlow::Wait)
        }
    }

    fn no_live_effects() -> NoLiveEffects {
        NoLiveEffects
    }

    pub(super) async fn run(
        definition: MountedHarnessDefinition<PlayerChannels, PlayerTurnProps>,
    ) -> anyhow::Result<()> {
        let provider = Arc::new(ExampleProvider::default());
        let factory = InMemoryMountedAgentFactory::<
            PlayerChannels,
            String,
            (),
            ExampleProvider,
            WaitReducer,
            fn() -> NoLiveEffects,
        >::new(
            DurableSessionId::new("example/player-feature/session")?,
            AgentSession::new(PromptContext::<String, ()>::without_system()),
            Arc::clone(&provider),
            "example/player-feature-model",
            128,
            WaitReducer,
            no_live_effects as fn() -> NoLiveEffects,
        )?;
        let agent = factory
            .open_with_bindings(
                definition.with_capture(PlayerCapture),
                MountedHostBindings::with_provider_dispatchers(provider_dispatchers()),
            )
            .await?;

        for (turn, intent_index, task) in [
            (1, 2, "Inspect the first intent."),
            (2, 5, "Inspect the next intent."),
        ] {
            let call_id = format!("example/player-feature/call-{turn}");
            let mut call = agent
                .start(MountedCallInput::new(
                    DurableCallId::new(&call_id)?,
                    DurableCallInputId::new(format!("{call_id}/input-v1"))?,
                    format!("player-feature-turn-{turn}"),
                    Arc::new(PlayerCall {
                        intent_index,
                        task: task.to_owned(),
                    }),
                    Arc::new(()),
                )?)
                .await?;
            let outcome = call.wait().await?;
            anyhow::ensure!(
                matches!(&outcome, MountedCallOutcome::Executed { records, .. } if records.len() == 1),
                "the example provider must execute exactly one turn"
            );
        }

        let (system_count, user_count, tool_result_count) = provider.counts();
        anyhow::ensure!(
            system_count == 1,
            "expected exactly one System attachment, got {system_count}"
        );
        anyhow::ensure!(
            user_count == 2,
            "expected one fresh User POM per turn, got {user_count}"
        );
        anyhow::ensure!(
            tool_result_count == 2,
            "expected one native-tool result per turn, got {tool_result_count}"
        );
        println!(
            "SYSTEM attached once; USER prompts={user_count}; native-tool results={tool_result_count}"
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let definition =
        player_feature().into_harness(EpochContractId::new("example/player-feature/v1")?);
    host::run(definition).await
}
