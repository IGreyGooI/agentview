//! Live example for `agentview` Agent + streaming XML tools.
//!
//! Run with:
//! `OPENROUTER_API_KEY=... cargo run -p agentview --example agent_streaming_tool_loop`
//!
//! Or with an OpenAI-compatible endpoint:
//! `OHMYGPT_API_KEY=... OHMYGPT_BASE_URL=... cargo run -p agentview --example agent_streaming_tool_loop`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentview::prelude::*;
use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::prelude::*;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use serde::{Deserialize, Serialize};

#[cfg(feature = "openrouter")]
type DemoLlmAgent = rig::agent::Agent<rig::providers::openrouter::completion::CompletionModel>;

#[cfg(not(feature = "openrouter"))]
type DemoLlmAgent = rig::agent::Agent<rig::providers::openai::completion::CompletionModel>;

#[cfg(feature = "openrouter")]
fn make_agent(model: &str, preamble: &str, max_tokens: u64) -> anyhow::Result<DemoLlmAgent> {
    let key = std::env::var("OPENROUTER_API_KEY")?;
    Ok(rig::providers::openrouter::Client::new(&key)?
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .build())
}

#[cfg(not(feature = "openrouter"))]
fn make_agent(model: &str, preamble: &str, max_tokens: u64) -> anyhow::Result<DemoLlmAgent> {
    let key = std::env::var("OHMYGPT_API_KEY")?;
    let base =
        std::env::var("OHMYGPT_BASE_URL").unwrap_or_else(|_| "https://api.ohmygpt.com/v1".into());
    Ok(rig::providers::openai::CompletionsClient::builder()
        .api_key(&key)
        .base_url(&base)
        .build()?
        .agent(model)
        .preamble(preamble)
        .max_tokens(max_tokens)
        .build())
}

#[derive(Debug, Clone)]
struct RigExampleExecutor;

fn turns_to_messages(turns: &[Turn]) -> Vec<rig::message::Message> {
    turns
        .iter()
        .map(|turn| match turn.role {
            Role::User => rig::message::Message::user(turn.text.as_ref()),
            Role::Assistant => rig::message::Message::assistant(turn.text.as_ref()),
        })
        .collect()
}

#[async_trait::async_trait]
impl LLMExecutor for RigExampleExecutor {
    async fn execute_llm<S>(
        &self,
        request: AgentTurnRequest,
        sink: &mut S,
        side_sinks: &mut Vec<Box<dyn TurnSink<Output = ()>>>,
    ) -> anyhow::Result<ExecutorCommit>
    where
        S: TurnSink + Send,
    {
        let agent = make_agent(&request.model, &request.system, request.max_tokens)?;
        let history = turns_to_messages(&request.history);
        let prompt = agent.stream_prompt(request.user.as_str());
        let inner = if history.is_empty() {
            prompt.await
        } else {
            prompt.with_history(history).await
        };

        let mut full_text = String::new();
        let mut stream = Box::pin(inner);
        while let Some(item) = stream.next().await {
            match item? {
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t)) => {
                    full_text.push_str(&t.text);
                    sink.on_event(TextTurnEvent::TextDelta(t.text.clone()))
                        .await;
                    for side_sink in side_sinks.iter_mut() {
                        side_sink
                            .on_event(TextTurnEvent::TextDelta(t.text.clone()))
                            .await;
                    }
                }
                other => {
                    let item = match other {
                        MultiTurnStreamItem::FinalResponse(_) => "final_response".to_string(),
                        MultiTurnStreamItem::StreamAssistantItem(_) => {
                            "non-text assistant stream item".to_string()
                        }
                        _ => "unknown stream item".to_string(),
                    };
                    self.on_agent_turn_event(AgentTurnEvent::IgnoredStreamItem {
                        call_id: request.call_id.clone(),
                        item,
                    })
                    .await;
                }
            }
        }

        sink.on_event(TextTurnEvent::TextComplete(full_text.clone()))
            .await;
        for side_sink in side_sinks.iter_mut() {
            side_sink
                .on_event(TextTurnEvent::TextComplete(full_text.clone()))
                .await;
        }

        Ok(ExecutorCommit::text(full_text))
    }
}

#[async_trait::async_trait]
impl AgentTurnObserver for RigExampleExecutor {
    async fn on_agent_turn_event(&self, event: AgentTurnEvent) {
        println!("\n--- AGENT TURN EVENT ---\n{event:?}");
    }
}

#[derive(Debug, Clone)]
struct DemoSource {
    scene: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DemoContextView {
    agent_id: String,
    scene: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DemoContextBuilder {
    agent_id: String,
}

#[async_trait::async_trait]
impl PromptRenderable for DemoContextView {
    async fn render_full<'a>(
        &'a self,
        engine: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        Ok(engine.render_template(
            "demo_context_full",
            r#"<demo_context><agent_id>{{ agent_id }}</agent_id><scene>{{ scene }}</scene></demo_context>"#,
            minijinja::context! {
                agent_id => self.agent_id.as_str(),
                scene => self.scene.as_str(),
            },
        )?.into())
    }
}

#[async_trait::async_trait]
impl ContextView for DemoContextView {
    async fn render_delta<'a>(
        &'a self,
        prev: &'a Self,
        engine: &'a TemplateEngine,
    ) -> anyhow::Result<Option<PromptFragment>> {
        if self == prev {
            return Ok(None);
        }

        Ok(Some(
            engine
                .render_template(
                    "demo_context_delta",
                    r#"<demo_delta><scene>{{ scene }}</scene></demo_delta>"#,
                    minijinja::context! {
                        scene => self.scene.as_str(),
                    },
                )?
                .into(),
        ))
    }
}

#[async_trait::async_trait]
impl ContextViewBuilder for DemoContextBuilder {
    type Source = DemoSource;
    type View = DemoContextView;

    async fn capture(&self, source: &DemoSource) -> Self::View {
        DemoContextView {
            agent_id: self.agent_id.clone(),
            scene: source.scene.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DemoPromptLayout;

impl PromptLayout for DemoPromptLayout {
    fn system_template(&self) -> &'static str {
        r#"{{ instructions }}
{% if output_schema %}{{ output_schema }}{% endif %}"#
    }

    fn user_template(&self) -> &'static str {
        r#"{% if context_block %}{{ context_block }}
{% endif %}{% for artifact in artifacts %}{{ artifact.rendered }}
{% endfor %}<task>{{ task }}</task>"#
    }
}

#[derive(Default)]
struct DemoParseContext {
    raw_output: String,
    artifacts: Vec<TurnArtifact>,
    intent_budget_verified: bool,
    selected: Vec<String>,
}

impl ParseContext for DemoParseContext {
    fn raw_output(&self) -> &str {
        &self.raw_output
    }

    fn set_raw_output(&mut self, output: String) {
        self.raw_output = output;
    }

    fn add_artifact(&mut self, artifact: TurnArtifact) {
        self.artifacts.push(artifact);
    }

    fn artifacts(&self) -> &[TurnArtifact] {
        &self.artifacts
    }
}

static INTENT_BUDGET_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

struct VerifyIntentBudgetTool;

#[async_trait::async_trait]
impl StreamingTool<DemoParseContext> for VerifyIntentBudgetTool {
    fn tag(&self) -> &'static str {
        "verify_intent_budget"
    }

    async fn on_open(
        &mut self,
        elem: &XmlElement,
        ctx: &mut DemoParseContext,
    ) -> Result<(), StreamingToolError> {
        if elem.attr("scope") != Some("demo") {
            return Err(StreamingToolError::InvalidAttribute {
                tag: "verify_intent_budget",
                attr: "scope",
                reason: "expected scope=\"demo\"".to_string(),
            });
        }

        let attempt = INTENT_BUDGET_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
            return Err(StreamingToolError::Rejected {
                tag: "verify_intent_budget",
                reason: "intent budget changed during validation; verify the budget again before selecting intents".to_string(),
            });
        }

        ctx.intent_budget_verified = true;
        Ok(())
    }
}

struct SelectTool;

#[async_trait::async_trait]
impl StreamingTool<DemoParseContext> for SelectTool {
    fn tag(&self) -> &'static str {
        "select"
    }

    async fn on_open(
        &mut self,
        elem: &XmlElement,
        ctx: &mut DemoParseContext,
    ) -> Result<(), StreamingToolError> {
        let Some(local_id) = elem.attr("local_id") else {
            return Err(StreamingToolError::InvalidAttribute {
                tag: "select",
                attr: "local_id",
                reason: "missing selected intent id".to_string(),
            });
        };

        ctx.selected.push(local_id.to_string());
        Ok(())
    }
}

struct ParserErrorArtifact {
    message: String,
}

impl ParserErrorArtifact {
    fn turn_artifact(message: impl Into<String>) -> TurnArtifact {
        TurnArtifact {
            kind: "parser_error".to_string(),
            payload: Box::new(Self {
                message: message.into(),
            }),
        }
    }
}

#[async_trait::async_trait]
impl PromptRenderable for ParserErrorArtifact {
    async fn render_full<'a>(
        &'a self,
        engine: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        Ok(engine
            .render_template(
                "parser_error_artifact",
                r#"<parser_error>{{ message }}</parser_error>"#,
                minijinja::context! {
                    message => self.message.as_str(),
                },
            )?
            .into())
    }
}

struct DemoLoopUpdate {
    flow: TurnFlow,
    artifacts: Vec<TurnArtifact>,
    task: Option<String>,
}

fn drain_demo_loop_update(ctx: &mut DemoParseContext) -> DemoLoopUpdate {
    if !ctx.intent_budget_verified {
        return DemoLoopUpdate {
            flow: TurnFlow::Continue,
            artifacts: vec![ParserErrorArtifact::turn_artifact(
                "<verify_intent_budget scope=\"demo\"/> failed: intent budget changed during validation. Verify again before selecting intents.",
            )],
            task: Some("The intent budget verification failed because the budget changed during validation. In one response, call <verify_intent_budget scope=\"demo\"/> again and also return 1-3 <select local_id=\"...\"/> tags.".to_owned()),
        };
    }

    if ctx.selected.is_empty() {
        DemoLoopUpdate {
            flow: TurnFlow::Continue,
            artifacts: vec![ParserErrorArtifact::turn_artifact(
                "Expected at least one <select> tag.",
            )],
            task: Some(
                "Return at least one <select local_id=\"...\"/> tag after verifying the intent budget."
                    .to_owned(),
            ),
        }
    } else {
        DemoLoopUpdate {
            flow: TurnFlow::Wait,
            artifacts: Vec::new(),
            task: Some("Next awake: keep using <select local_id=\"...\"/> tags.".to_owned()),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    INTENT_BUDGET_ATTEMPTS.store(0, Ordering::SeqCst);

    let executor = RigExampleExecutor;
    let agent: TextAgent<
        DemoContextBuilder,
        RigExampleExecutor,
        IdentityTransform,
        TextTurnEvent,
        DemoParseContext,
    > = Agent::new(
        DemoContextBuilder {
            agent_id: "demo_agent".to_string(),
        },
        DemoPromptLayout,
        PromptSystemVars {
            instructions: "You are a demo intent selector agent.".to_string(),
            output_schema: Some(
                "First call <verify_intent_budget scope=\"demo\"/>. After the budget is verified, return only <select local_id=\"...\"/> tags."
                    .to_string(),
            ),
        },
        std::env::var("AGENT_EXAMPLE_MODEL")
            .unwrap_or_else(|_| "deepseek/deepseek-v3.2".to_string()),
        128,
        IdentityTransform,
    )
    .with_observer(Arc::new(executor.clone()));

    let source = DemoSource {
        scene: "demo scene captured from a tiny app source".to_string(),
    };

    let mut task = "Select 1-3 currently valid demo intents. First call <verify_intent_budget scope=\"demo\"/>, then return <select local_id=\"...\"/> tags.".to_owned();
    for _ in 0..3 {
        let mut parse_ctx = agent
            .call("DemoSelect")
            .with_user(task)
            .execute_with_sink(
                &source,
                &executor,
                StreamingToolRunner::new(DemoParseContext::default())
                    .with_tool(VerifyIntentBudgetTool)
                    .with_tool(SelectTool),
            )
            .await?;

        let update = drain_demo_loop_update(&mut parse_ctx);
        let rendered_artifacts = agent
            .config
            .view
            .render_turn_artifacts(update.artifacts)
            .await?;
        if !rendered_artifacts.is_empty() || update.task.is_some() {
            agent.ctx.write().await.context_state_mut().feedback =
                agentview::agent::DefaultAgentFeedback {
                    artifacts: rendered_artifacts,
                    task: update.task,
                };
        }
        match update.flow {
            TurnFlow::Wait => break,
            TurnFlow::Continue => task = String::new(),
        }
    }

    println!(
        "\n--- PENDING NEXT-AWAKE FEEDBACK ---\n{}",
        agent
            .ctx
            .read()
            .await
            .context_state()
            .feedback
            .task
            .as_deref()
            .unwrap_or("<none>")
    );

    Ok(())
}
