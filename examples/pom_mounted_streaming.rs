//! Mounted component proof: one System POM/factory bundle creates fresh XML
//! reducer and native-tool state for every provider attempt.
//!
//! This example awaits a host-selected live runtime inside parser callbacks,
//! but it does not integrate the mounted lifecycle with AgentLoop yet.

use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use agentview::{
    component::{
        advanced::{
            experimental::{TurnPublication, TurnPublisher},
            lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
            provider::{
                provider_tool_with_context, ProviderDispatchContext, ProviderDispatchUpdate,
                ProviderDispatcher, ProviderDispatcherCx, ProviderToolCall, ProviderToolResponse,
            },
        },
        system_view, user_view, LiveAbortContext, LiveEffectAbortAck, LiveEffectContext,
        LiveEffectRuntime, TurnBindingCx, UserTurnContext, UserView,
    },
    prelude::*,
};
use serde_json::json;

struct PlayerChannels;

#[derive(Debug)]
enum PlayerOutput {
    Selected(u32),
}

#[derive(Debug)]
enum PlayerLive {
    SelectionOpened(u32),
}

#[derive(Debug)]
enum PlayerCommit {
    AttemptFinished(Option<u32>),
}

struct PlayerLiveRuntime;

#[async_trait::async_trait]
impl LiveEffectRuntime<PlayerLive> for PlayerLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        effect: PlayerLive,
    ) -> Result<(), Self::Error> {
        match effect {
            PlayerLive::SelectionOpened(index) => println!("live selection-opened={index}"),
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

struct DemoPublisher;

#[async_trait::async_trait]
impl TurnPublisher for DemoPublisher {
    type Error = Infallible;

    async fn publish(&mut self, _publication: TurnPublication<'_>) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl TurnChannels for PlayerChannels {
    type Output = PlayerOutput;
    type Live = PlayerLive;
    type Commit = PlayerCommit;
    type Diagnostic = String;
}

struct PlayerMount {
    system_renders: Arc<AtomicUsize>,
    state_initializations: Arc<AtomicUsize>,
}

struct PlayerTurn {
    user_renders: Arc<AtomicUsize>,
    allowed_intents: Arc<[u32]>,
    task: String,
}

struct SelectionState {
    allowed_intents: Arc<[u32]>,
    selected: Option<u32>,
}

#[derive(Debug)]
struct PlayerTools {
    allowed_intents: Arc<[u32]>,
}

#[async_trait::async_trait]
impl ProviderDispatcher<PlayerChannels> for PlayerTools {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<PlayerChannels>, Self::Error> {
        debug_assert_eq!(call.name(), "list_allowed_intents");
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success(json!({
                "allowed_intents": self.allowed_intents.as_ref(),
            })),
            StreamUpdate::new(),
        ))
    }
}

fn select_intent_contract() -> XmlNode {
    let mut contract = XmlNode::new(XmlName::try_from("select_intent").unwrap());
    contract
        .push_attribute(XmlName::try_from("index").unwrap(), "...")
        .expect("the static attribute is unique");
    contract
}

fn player_system(
    cx: SystemMountContext<'_, PlayerMount>,
) -> SystemView<PlayerChannels, PlayerTurn> {
    // Example-only lifecycle probe; production System renderers stay pure.
    cx.props().system_renders.fetch_add(1, Ordering::SeqCst);
    let state_initializations = Arc::clone(&cx.props().state_initializations);
    let list_intents = ProviderToolSpec::new(
        "list_allowed_intents",
        "List the intents allowed for this prepared turn",
        json!({ "type": "object", "properties": {} }),
    )
    .expect("static provider tool contract is valid");
    system_view(component((
        Document::from_xml(XmlNode::new(XmlName::try_from("player_policy").unwrap())),
        StreamingXml::<TurnEmission<PlayerChannels>, String>::new(select_intent_contract())
            .try_state_with(
                move |turn: &TurnBindingCx<'_, PlayerTurn, PlayerChannels>| {
                    // Example-only probe for fresh per-attempt state.
                    state_initializations.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(SelectionState {
                        allowed_intents: Arc::clone(&turn.props().allowed_intents),
                        selected: None,
                    })
                },
            )
            .on_open(|state, element| {
                let Some(index) = element.attr("index") else {
                    return StreamUpdate::from_diagnostic("missing intent index".to_owned());
                };
                let Ok(index) = index.parse::<u32>() else {
                    return StreamUpdate::from_diagnostic(
                        "intent index must be an integer".to_owned(),
                    );
                };
                if !state.allowed_intents.contains(&index) {
                    return StreamUpdate::from_diagnostic("intent is not allowed".to_owned());
                }
                state.selected = Some(index);
                StreamUpdate::from_emission(TurnEmission::Live(PlayerLive::SelectionOpened(index)))
            })
            .on_complete(|state, _| match state.selected {
                Some(index) => {
                    StreamUpdate::from_emission(TurnEmission::Output(PlayerOutput::Selected(index)))
                }
                None => StreamUpdate::from_diagnostic("selection never opened".to_owned()),
            })
            .on_finish(|state| {
                StreamUpdate::from_emission(TurnEmission::Commit(PlayerCommit::AttemptFinished(
                    state.selected,
                )))
            })
            .into_component(),
        provider_tool_with_context(
            "player_tools",
            list_intents,
            |turn: &ProviderDispatcherCx<'_, PlayerTurn, PlayerChannels>| {
                Ok(PlayerTools {
                    allowed_intents: Arc::clone(&turn.props().allowed_intents),
                })
            },
        ),
    )))
}

fn player_user(cx: UserTurnContext<'_, PlayerTurn>) -> UserView {
    // Example-only lifecycle probe; production User renderers stay pure.
    cx.props().user_renders.fetch_add(1, Ordering::SeqCst);
    let mut task = XmlNode::new(XmlName::try_from("task").expect("static tag is valid"));
    task.push(MixedContent::text(TextNode::new(&cx.props().task)));
    user_view(Document::from_xml(task))
}

fn print_update(update: &StreamUpdate<TurnEmission<PlayerChannels>, String>) {
    for emission in update.emissions() {
        match emission {
            TurnEmission::Output(PlayerOutput::Selected(index)) => {
                println!("output selected={index}")
            }
            TurnEmission::Live(PlayerLive::SelectionOpened(index)) => {
                println!("live selection-opened={index}")
            }
            TurnEmission::Commit(PlayerCommit::AttemptFinished(selected)) => {
                println!("commit attempt-finished={selected:?}")
            }
        }
    }
    for diagnostic in update.diagnostics() {
        println!("diagnostic={diagnostic}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let system_renders = Arc::new(AtomicUsize::new(0));
    let state_initializations = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &PlayerMount {
            system_renders: Arc::clone(&system_renders),
            state_initializations: Arc::clone(&state_initializations),
        },
        player_system,
    )?;
    println!("SYSTEM\n{}", epoch.rendered_system());
    println!("state initializations after mount=0");

    let user_renders = Arc::new(AtomicUsize::new(0));
    let mut cursor = UserDocumentCursor::default();
    for (task, index) in [("inspect-square", 2), ("continue-game", 4)] {
        let props = PlayerTurn {
            user_renders: Arc::clone(&user_renders),
            allowed_intents: Arc::from([index, index + 1]),
            task: task.to_owned(),
        };
        let turn = epoch.begin_turn(task);
        let user = turn.prepare_user(&props, player_user)?;
        let (resolved, next_cursor) = resolve_user_document(user.user_document().clone(), &cursor)?;
        println!("\nUSER\n{}", render_pom_document(&resolved)?);

        let mut attempt = user.start_streaming_attempt(PlayerLiveRuntime)?;
        let tool = match attempt
            .call_tool(ProviderToolCall::new(
                format!("allowed-intents-{index}"),
                "list_allowed_intents",
                json!({}),
            ))
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                let _report = attempt.abort(BindingAbortReason::ProviderFailure).await;
                return Err(anyhow::anyhow!(message));
            }
        };
        println!("tool response={:?}", tool.result().response());
        let update = match attempt
            .on_event(TextTurnEvent::TextDelta(format!(
                "<select_intent index=\"{index}\" />"
            )))
            .await
        {
            Ok(update) => update,
            Err(error) => {
                let message = error.to_string();
                let _report = attempt.abort(BindingAbortReason::ParserFailure).await;
                return Err(anyhow::anyhow!(message));
            }
        };
        print_update(&update);
        let finished = match attempt.finish_stream().await {
            Ok(finished) => finished,
            Err(failure) => {
                let message = failure.error().to_string();
                let _report = failure.abort(BindingAbortReason::ParserFailure).await;
                return Err(anyhow::anyhow!(message));
            }
        };
        print_update(finished.update());

        let mut publisher = DemoPublisher;
        let mut published = match finished.publish_with(&mut publisher).await {
            Ok(published) => published,
            Err(failure) => {
                let message = failure.to_string();
                let _report = failure.abort(BindingAbortReason::PublishFailure).await;
                return Err(anyhow::anyhow!(message));
            }
        };
        // The demo host explicitly takes ownership of every released Commit.
        for commit in published.take_pending_commits() {
            match commit {
                PlayerCommit::AttemptFinished(selected) => {
                    println!("commit attempt-finished={selected:?}")
                }
            }
        }
        cursor = next_cursor;
    }

    println!(
        "\nCOUNTS system={} user={} states={}",
        system_renders.load(Ordering::SeqCst),
        user_renders.load(Ordering::SeqCst),
        state_initializations.load(Ordering::SeqCst),
    );
    Ok(())
}
