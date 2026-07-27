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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, AgentView)]
#[agent_view(kind = "demo_context")]
struct DemoContextView {
    #[view(element)]
    agent_id: String,

    #[view(diff)]
    scene: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DemoContextBuilder {
    agent_id: String,
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

#[derive(Debug, Clone, Default)]
struct DemoStreamingTools {
    verify_intent_budget: VerifyIntentBudgetTool,
    select: SelectTool,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct DemoWorkflowStepView<T>
where
    T: AgentView<Root = XmlNode>,
{
    #[view(text)]
    before: &'static str,

    #[view(xml)]
    call: T,

    #[view(text)]
    after: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct DemoTaskView {
    #[view(text)]
    before: &'static str,

    #[view(text)]
    selection_range: SelectionRange,

    #[view(text)]
    after: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "response_contract")]
struct DemoResponseContractView {
    #[view(attr)]
    transport: &'static str,

    #[view(root)]
    verify_intent_budget: VerifyIntentBudgetTool,

    #[view(root)]
    select: SelectTool,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct DemoSystemPromptView {
    #[view(heading = 1)]
    title: &'static str,

    #[view(block)]
    task: DemoTaskView,

    #[view(heading = 2)]
    workflow_title: &'static str,

    #[view(block)]
    verify_workflow: DemoWorkflowStepView<VerifyIntentBudgetTool>,

    #[view(block)]
    select_workflow: DemoWorkflowStepView<SelectTool>,

    #[view(xml)]
    response_contract: DemoResponseContractView,
}

impl From<DemoStreamingTools> for DemoSystemPromptView {
    /// Projects runtime tool configuration into prompt-facing fields.
    ///
    /// This only assembles typed view data; `#[derive(AgentView)]` builds the
    /// POM AST and the shared renderer owns all Markdown/XML serialization.
    fn from(tools: DemoStreamingTools) -> Self {
        let DemoStreamingTools {
            verify_intent_budget,
            select,
        } = tools;
        let selection_range = select.selection_range.clone();

        Self {
            title: "Demo intent selector",
            task: DemoTaskView {
                before: "Select ",
                selection_range,
                after: " currently valid demo intents from the current context.",
            },
            workflow_title: "Required workflow",
            verify_workflow: DemoWorkflowStepView {
                before: "First, call ",
                call: verify_intent_budget.clone(),
                after: ".",
            },
            select_workflow: DemoWorkflowStepView {
                before: "Then, after verification, return only ",
                call: select.clone(),
                after: " elements.",
            },
            response_contract: DemoResponseContractView {
                transport: "xml",
                verify_intent_budget,
                select,
            },
        }
    }
}

impl Default for DemoSystemPromptView {
    fn default() -> Self {
        Self::from(DemoStreamingTools::default())
    }
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct DemoPromptTextView {
    #[view(text)]
    text: StorageString,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct DemoUserPromptView {
    #[view(name = "agent_context", diff)]
    context: DemoContextView,

    #[view(block)]
    artifacts: Vec<TurnArtifact>,

    #[view(block)]
    feedback: Option<DemoPromptTextView>,

    #[view(block)]
    task: Option<DemoPromptTextView>,
}

#[derive(Clone)]
struct DemoViewModel {
    context_builder: DemoContextBuilder,
    system_prompt: DemoSystemPromptView,
}

impl DemoViewModel {
    fn new(context_builder: DemoContextBuilder, system_prompt: DemoSystemPromptView) -> Self {
        Self {
            context_builder,
            system_prompt,
        }
    }
}

#[async_trait::async_trait]
impl AgentViewModel<Turn, DemoParseContext> for DemoViewModel {
    type Source = DemoSource;
    type View = DemoContextView;
    type ContextState = agentview::agent::DefaultContextState;

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        Ok(self.system_prompt.build_root()?)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        self.context_builder.capture(source).await
    }

    async fn build_user_document(
        &self,
        ctx: &PromptContext<Turn, Self::ContextState>,
        _call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(DemoUserPromptView {
            context: current_view.clone(),
            artifacts: ctx.context_state().feedback.artifacts.clone(),
            feedback: ctx
                .context_state()
                .feedback
                .task
                .clone()
                .map(|text| DemoPromptTextView { text: text.into() }),
            task: (!task.is_empty()).then(|| DemoPromptTextView { text: task }),
        }
        .build_root()?)
    }

    async fn commit_turn(
        &self,
        ctx: &mut PromptContext<Turn, Self::ContextState>,
        request: &AgentTurnRequest<Turn>,
        executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut DemoParseContext,
    ) -> anyhow::Result<TurnFlow> {
        ctx.push_history(Turn::user(request.user.clone()));
        ctx.extend_history(executor_commit.append);
        ctx.context_state_mut().feedback = agentview::agent::DefaultAgentFeedback::default();
        Ok(TurnFlow::Wait)
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

const VERIFY_INTENT_BUDGET_TAG: &str = "verify_intent_budget";
const SELECT_TAG: &str = "select";
const SELECT_LOCAL_ID_ATTRIBUTE: &str = "local_id";

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "verify_intent_budget")]
struct VerifyIntentBudgetTool {
    #[view(attr)]
    scope: &'static str,

    #[view(attr)]
    required: &'static str,
}

impl Default for VerifyIntentBudgetTool {
    fn default() -> Self {
        Self {
            scope: "demo",
            required: "first",
        }
    }
}

#[async_trait::async_trait]
impl StreamingTool<DemoParseContext> for VerifyIntentBudgetTool {
    async fn on_open(
        &mut self,
        elem: &XmlElement,
        ctx: &mut DemoParseContext,
    ) -> Result<(), StreamingToolError> {
        if elem.attr("scope") != Some(self.scope) {
            return Err(StreamingToolError::InvalidAttribute {
                tag: VERIFY_INTENT_BUDGET_TAG,
                attr: "scope",
                reason: format!("expected scope=\"{}\"", self.scope),
            });
        }

        let attempt = INTENT_BUDGET_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt == 1 {
            return Err(StreamingToolError::Rejected {
                tag: VERIFY_INTENT_BUDGET_TAG,
                reason: "intent budget changed during validation; verify the budget again before selecting intents".to_string(),
            });
        }

        ctx.intent_budget_verified = true;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct SelectionRange {
    min: usize,
    max: usize,
}

impl std::fmt::Display for SelectionRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}-{}", self.min, self.max)
    }
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "select")]
struct SelectTool {
    #[view(attr)]
    local_id: &'static str,

    #[view(attr, name = "cardinality")]
    selection_range: SelectionRange,
}

impl Default for SelectTool {
    fn default() -> Self {
        Self {
            local_id: "...",
            selection_range: SelectionRange { min: 1, max: 3 },
        }
    }
}

#[async_trait::async_trait]
impl StreamingTool<DemoParseContext> for SelectTool {
    async fn on_open(
        &mut self,
        elem: &XmlElement,
        ctx: &mut DemoParseContext,
    ) -> Result<(), StreamingToolError> {
        let Some(local_id) = elem.attr(SELECT_LOCAL_ID_ATTRIBUTE) else {
            return Err(StreamingToolError::InvalidAttribute {
                tag: SELECT_TAG,
                attr: SELECT_LOCAL_ID_ATTRIBUTE,
                reason: "missing selected intent id".to_string(),
            });
        };

        if ctx.selected.len() >= self.selection_range.max {
            return Err(StreamingToolError::Rejected {
                tag: SELECT_TAG,
                reason: format!(
                    "expected at most {} selected intents",
                    self.selection_range.max
                ),
            });
        }

        ctx.selected.push(local_id.to_string());
        Ok(())
    }
}

#[derive(AgentView)]
#[agent_view(kind = "parser_error")]
struct ParserErrorArtifact {
    #[view(text)]
    message: String,
}

impl ParserErrorArtifact {
    fn turn_artifact(message: impl Into<String>) -> TurnArtifact {
        let artifact = Self {
            message: message.into(),
        };
        TurnArtifact::try_from_view(&artifact)
            .expect("the demo parser-error POM never contains a DiffSlot")
    }
}

struct DemoLoopUpdate {
    flow: TurnFlow,
    artifacts: Vec<TurnArtifact>,
    task: Option<String>,
}

fn drain_demo_loop_update(
    ctx: &mut DemoParseContext,
    tools: &DemoStreamingTools,
) -> DemoLoopUpdate {
    if !ctx.intent_budget_verified {
        return DemoLoopUpdate {
            flow: TurnFlow::Continue,
            artifacts: vec![ParserErrorArtifact::turn_artifact(
                "Intent budget verification failed because the budget changed during validation.",
            )],
            task: Some(format!(
                "Retry the required workflow and return {} selections in one response.",
                tools.select.selection_range,
            )),
        };
    }

    if ctx.selected.len() < tools.select.selection_range.min {
        DemoLoopUpdate {
            flow: TurnFlow::Continue,
            artifacts: vec![ParserErrorArtifact::turn_artifact(format!(
                "Expected at least {} selected intents.",
                tools.select.selection_range.min
            ))],
            task: Some(format!(
                "Return at least {} selections after verifying the intent budget.",
                tools.select.selection_range.min
            )),
        }
    } else {
        DemoLoopUpdate {
            flow: TurnFlow::Wait,
            artifacts: Vec::new(),
            task: Some("Next awake: keep using the response contract.".to_owned()),
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
    let tools = DemoStreamingTools::default();
    let system_prompt = DemoSystemPromptView::from(tools.clone());
    let agent: Agent<DemoViewModel, RigExampleExecutor, Turn, TextTurnEvent, DemoParseContext> =
        Agent::with_view(
            DemoViewModel::new(
                DemoContextBuilder {
                    agent_id: "demo_agent".to_string(),
                },
                system_prompt,
            ),
            std::env::var("AGENT_EXAMPLE_MODEL")
                .unwrap_or_else(|_| "deepseek/deepseek-v3.2".to_string()),
            128,
            PromptContext::<Turn, agentview::agent::DefaultContextState>::without_system(),
        )
        .with_observer(Arc::new(executor.clone()));

    let source = DemoSource {
        scene: "demo scene captured from a tiny app source".to_string(),
    };

    let mut task = format!(
        "Select {} currently valid demo intents using the required workflow.",
        tools.select.selection_range
    );
    for _ in 0..3 {
        let mut parse_ctx = agent
            .call("DemoSelect")
            .with_user(task)
            .execute_with_sink(
                &source,
                &executor,
                StreamingToolRunner::new(DemoParseContext::default())
                    .with_tool(tools.verify_intent_budget.clone())
                    .with_tool(tools.select.clone()),
            )
            .await?;

        let update = drain_demo_loop_update(&mut parse_ctx, &tools);
        if !update.artifacts.is_empty() || update.task.is_some() {
            agent.session_mut().await.context_state_mut().feedback =
                agentview::agent::DefaultAgentFeedback {
                    artifacts: update.artifacts,
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
            .session()
            .await
            .context()
            .context_state()
            .feedback
            .task
            .as_deref()
            .unwrap_or("<none>")
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_system_prompt_is_a_derived_document_view() {
        let prompt = DemoSystemPromptView::default();
        let document = prompt.build_root().unwrap();

        assert!(!document.children().is_empty());
    }

    #[test]
    fn streaming_tools_contribute_their_own_pom_contract_nodes() {
        let tools = DemoStreamingTools::default();

        let verify = tools.verify_intent_budget.build_root().unwrap();
        let select = tools.select.build_root().unwrap();

        assert_eq!(verify.name().as_str(), "verify_intent_budget");
        assert_eq!(select.name().as_str(), "select");
        assert_eq!(
            verify
                .attributes()
                .get(&XmlName::try_from("scope").unwrap())
                .unwrap()
                .value(),
            tools.verify_intent_budget.scope
        );
        assert_eq!(
            select
                .attributes()
                .get(&XmlName::try_from("local_id").unwrap())
                .unwrap()
                .value(),
            tools.select.local_id
        );
        assert_eq!(
            select
                .attributes()
                .get(&XmlName::try_from("cardinality").unwrap())
                .unwrap()
                .value(),
            format!(
                "{}-{}",
                tools.select.selection_range.min, tools.select.selection_range.max
            )
        );
    }

    #[test]
    fn system_document_reads_dynamic_tool_configuration() {
        let mut tools = DemoStreamingTools::default();
        tools.verify_intent_budget.scope = "sandbox";
        tools.select.selection_range.max = 4;
        let prompt = DemoSystemPromptView::from(tools);

        let rendered =
            render_pom_document(&resolve_system_document(prompt.build_root().unwrap())).unwrap();

        assert!(rendered.contains("Select 1-4 currently valid demo intents"));
        assert!(rendered.contains("scope=\"sandbox\""));
        assert!(rendered.contains("cardinality=\"1-4\""));
    }

    #[test]
    fn demo_system_prompt_is_authored_and_rendered_as_pom() {
        let rendered = render_pom_document(&resolve_system_document(
            DemoSystemPromptView::default().build_root().unwrap(),
        ))
        .unwrap();

        assert_eq!(
            rendered,
            concat!(
                "# Demo intent selector\n\n",
                "Select 1-3 currently valid demo intents from the current context.\n\n",
                "## Required workflow\n\n",
                "First, call <verify_intent_budget scope=\"demo\" required=\"first\" />.\n\n",
                "Then, after verification, return only <select local_id=\"...\" cardinality=\"1-3\" /> elements.\n\n",
                "<response_contract transport=\"xml\">\n",
                "  <verify_intent_budget scope=\"demo\" required=\"first\" />\n",
                "  <select local_id=\"...\" cardinality=\"1-3\" />\n",
                "</response_contract>"
            )
        );
    }

    #[test]
    fn demo_user_document_resolves_the_derived_context_slot_and_typed_turn_blocks() {
        let previous = DemoContextView {
            agent_id: "demo_agent".to_owned(),
            scene: "old scene".to_owned(),
        };
        let current = DemoContextView {
            agent_id: "demo_agent".to_owned(),
            scene: "new scene".to_owned(),
        };
        let first_document = DemoUserPromptView {
            context: previous,
            artifacts: Vec::new(),
            feedback: None,
            task: Some(DemoPromptTextView {
                text: "Choose one intent.".into(),
            }),
        }
        .build_root()
        .unwrap();
        let (first, cursor) = agentview::pom_resolution::resolve_user_document(
            first_document,
            &UserDocumentCursor::default(),
        )
        .unwrap();
        let first = render_pom_document(&first).unwrap();

        assert!(first.contains("<agent_context kind=\"demo_context\">"));
        assert!(first.contains("<scene>old scene</scene>"));
        assert!(first.ends_with("Choose one intent."));

        let artifact = ParserErrorArtifact::turn_artifact("The selection was rejected.");
        let second_document = DemoUserPromptView {
            context: current.clone(),
            artifacts: vec![artifact],
            feedback: Some(DemoPromptTextView {
                text: "Retry the required workflow.".into(),
            }),
            task: Some(DemoPromptTextView {
                text: "Choose one intent.".into(),
            }),
        }
        .build_root()
        .unwrap();
        let (second, cursor) =
            agentview::pom_resolution::resolve_user_document(second_document, &cursor).unwrap();
        let second = render_pom_document(&second).unwrap();

        let context_index = second
            .find("<agent_context rendering_mode=\"delta\"")
            .expect("the changed derived context must render as a delta");
        let artifact_index = second
            .find("<parser_error>The selection was rejected.</parser_error>")
            .expect("the typed artifact must remain a POM XML block");
        let feedback_index = second
            .find("Retry the required workflow.")
            .expect("feedback must remain a separate Markdown block");
        let task_index = second
            .find("Choose one intent.")
            .expect("the task must remain a separate Markdown block");

        assert!(second.contains("<scene>new scene</scene>"));
        assert!(context_index < artifact_index);
        assert!(artifact_index < feedback_index);
        assert!(feedback_index < task_index);

        let unchanged_document = DemoUserPromptView {
            context: current,
            artifacts: Vec::new(),
            feedback: None,
            task: Some(DemoPromptTextView {
                text: "Continue with the current scene.".into(),
            }),
        }
        .build_root()
        .unwrap();
        let (unchanged, _) =
            agentview::pom_resolution::resolve_user_document(unchanged_document, &cursor).unwrap();

        assert_eq!(
            render_pom_document(&unchanged).unwrap(),
            "Continue with the current scene."
        );
    }
}
