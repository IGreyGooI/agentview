//! Executable provider-native tool-group proof outside AgentLoop.

use std::convert::Infallible;

use agentview::{
    component::{
        advanced::{
            experimental::{TurnPublication, TurnPublisher},
            lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
            provider::{
                provider_tools_with_context, ProviderDispatchContext, ProviderDispatchUpdate,
                ProviderDispatcher, ProviderDispatcherCx, ProviderToolCall, ProviderToolResponse,
            },
        },
        *,
    },
    prelude::*,
};
use serde_json::json;

struct DirectorChannels;

impl TurnChannels for DirectorChannels {
    type Output = String;
    type Live = String;
    type Commit = String;
    type Diagnostic = String;
}

struct DirectorTurn {
    step: u32,
    task: String,
}

#[derive(Debug)]
struct DirectorTools {
    step: u32,
    calls: Vec<String>,
}

#[async_trait::async_trait]
impl ProviderDispatcher<DirectorChannels> for DirectorTools {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<DirectorChannels>, Self::Error> {
        self.calls
            .push(context.invocation_key().invocation_id().to_owned());
        let response = match call.name() {
            "inspect_board" => ProviderToolResponse::success(json!({
                "step": self.step,
                "summary": "north gate is open",
            })),
            "edit_plan" if call.arguments()["text"].as_str().is_none() => {
                ProviderToolResponse::error("invalid_arguments", "edit_plan requires `text`")
            }
            "edit_plan" => ProviderToolResponse::success("plan updated"),
            _ => unreachable!("the mounted runtime routes only declared tools"),
        };
        let label = format!("{}#{}", call.name(), context.sequence());
        Ok(ProviderDispatchUpdate::new(
            response,
            StreamUpdate::from_emission(TurnEmission::Output(label.clone()))
                .with_emission(TurnEmission::Live(format!("started:{label}")))
                .with_emission(TurnEmission::Commit(format!("record:{label}"))),
        ))
    }
}

fn tool_spec(name: &str, description: &str) -> ProviderToolSpec {
    ProviderToolSpec::new(
        name,
        description,
        json!({ "type": "object", "properties": {} }),
    )
    .expect("static tool spec is valid")
}

fn director_system(_: SystemMountContext<'_, ()>) -> SystemView<DirectorChannels, DirectorTurn> {
    system_view(component((
        Document::from_xml(XmlNode::new(XmlName::try_from("director_policy").unwrap())),
        provider_tools_with_context(
            "director-tools",
            [
                tool_spec("inspect_board", "Inspect the current board"),
                tool_spec("edit_plan", "Edit the current plan"),
            ],
            |cx: &ProviderDispatcherCx<'_, DirectorTurn, DirectorChannels>| {
                Ok(DirectorTools {
                    step: cx.props().step,
                    calls: Vec::new(),
                })
            },
        ),
    )))
}

fn director_user(cx: UserTurnContext<'_, DirectorTurn>) -> UserView {
    let mut task = XmlNode::new(XmlName::try_from("task").unwrap());
    task.push_attribute(
        XmlName::try_from("step").unwrap(),
        cx.props().step.to_string(),
    )
    .unwrap();
    task.push(MixedContent::text(TextNode::new(&cx.props().task)));
    user_view(Document::from_xml(task))
}

struct DemoLiveRuntime;

#[async_trait::async_trait]
impl LiveEffectRuntime<String> for DemoLiveRuntime {
    type Error = Infallible;

    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        effect: String,
    ) -> Result<(), Self::Error> {
        println!("LIVE {} {effect}", context.sequence());
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

    async fn publish(&mut self, publication: TurnPublication<'_>) -> Result<(), Self::Error> {
        println!("PUBLISH results={}", publication.provider_results().len());
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let epoch = mount_system_epoch(&(), director_system)?;
    println!("SYSTEM\n{}", epoch.rendered_system());

    let props = DirectorTurn {
        step: 4,
        task: "Inspect the gate, then update the plan".to_owned(),
    };
    let turn = epoch.begin_turn("director");
    let prepared = turn.prepare_user(&props, director_user)?;
    let (user, _) = resolve_user_document(
        prepared.user_document().clone(),
        &UserDocumentCursor::default(),
    )?;
    println!("\nUSER\n{}", render_pom_document(&user)?);

    let mut attempt = prepared.start_provider_attempt(DemoLiveRuntime)?;
    for call in [
        ProviderToolCall::new("call-1", "inspect_board", json!({}))
            .with_result_correlation_id("provider-call-1"),
        ProviderToolCall::new("call-2", "edit_plan", json!({ "text": "hold gate" })),
    ] {
        let outcome = match attempt.call_tool(call).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                let _report = attempt.abort(BindingAbortReason::ProviderFailure).await;
                return Err(anyhow::anyhow!(message));
            }
        };
        println!(
            "RESULT invocation={} correlation={} tool={} error={} response={:?}",
            outcome.result().invocation_id().unwrap_or("<missing>"),
            outcome.result().result_correlation_id().unwrap_or("<none>"),
            outcome.result().name(),
            outcome.result().is_error(),
            outcome.result().response(),
        );
        for emission in outcome.update().emissions() {
            if let TurnEmission::Output(output) = emission {
                println!("OUTPUT {output}");
            }
        }
    }

    let finished = match attempt.finish().await {
        Ok(finished) => finished,
        Err(failure) => {
            let message = failure.error().to_string();
            let _report = failure.abort(BindingAbortReason::ProviderFailure).await;
            return Err(anyhow::anyhow!(message));
        }
    };
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
        println!("COMMIT {commit}");
    }
    Ok(())
}
