use super::*;
use crate::agent::TurnFlow;
use crate::templates::{PromptRenderable, TemplateEngine};

#[derive(Debug, thiserror::Error)]
enum DecisionError {
    #[error("Expected a <{tag}> tag to start {purpose}.")]
    MissingRequiredTag {
        tag: &'static str,
        purpose: &'static str,
    },

    #[error("{message}")]
    FormatReminder { message: &'static str },
}

#[derive(crate::AgentView)]
#[agent_view(kind = "parser_error")]
struct DecisionErrorArtifact {
    #[view(text)]
    error: DecisionError,
}

#[derive(crate::AgentView)]
#[agent_view(kind = "behavior_feedback")]
struct BehaviorFeedbackArtifact {
    #[view(text)]
    message: &'static str,
}

impl BehaviorFeedbackArtifact {
    fn new(message: &'static str) -> Self {
        Self { message }
    }

    fn into_turn_artifact(self) -> TurnArtifact {
        TurnArtifact::try_from_view(&self)
            .expect("the test feedback artifact never contains a DiffSlot")
    }
}

impl DecisionErrorArtifact {
    fn new(error: DecisionError) -> Self {
        Self { error }
    }

    fn into_turn_artifact(self) -> TurnArtifact {
        TurnArtifact::try_from_view(&self)
            .expect("the test decision artifact never contains a DiffSlot")
    }
}

struct NpcParseContext {
    raw_output: String,
    artifacts: Vec<TurnArtifact>,
    speech_started: bool,
    streamed: String,
    completed: Vec<String>,
}

impl NpcParseContext {
    fn new() -> Self {
        Self {
            raw_output: String::new(),
            artifacts: Vec::new(),
            speech_started: false,
            streamed: String::new(),
            completed: Vec::new(),
        }
    }

    fn mark_speech_started(&mut self) {
        self.speech_started = true;
    }

    fn speech_started(&self) -> bool {
        self.speech_started
    }
}

impl ParseContext for NpcParseContext {
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

#[derive(crate::AgentView)]
#[agent_view(kind = "speak")]
struct SpeakTool {}

#[test]
fn streaming_tool_is_object_safe_and_its_contract_is_derived() {
    let tool = SpeakTool {};
    let prompt: &dyn StreamingTool<NpcParseContext> = &tool;
    let node = prompt.build_root().unwrap();

    assert_eq!(node.name().as_str(), "speak");
}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for SpeakTool {
    async fn on_open(
        &mut self,
        _elem: &XmlElement,
        ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        if ctx.speech_started() {
            return Err(StreamingToolError::Rejected {
                tag: "speak",
                reason: "duplicate speech tag".into(),
            });
        }

        ctx.mark_speech_started();
        Ok(())
    }

    async fn on_stream(
        &mut self,
        elem: &XmlElement,
        ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        ctx.streamed = elem.content.clone();
        Ok(())
    }

    async fn on_complete(
        &mut self,
        elem: &XmlElement,
        ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        if elem.content.trim().is_empty() {
            return Err(StreamingToolError::InvalidContent {
                tag: "speak",
                reason: "speech content cannot be empty".into(),
            });
        }

        ctx.completed.push(elem.content.clone());
        Ok(())
    }
}

#[derive(crate::AgentView)]
#[agent_view(kind = "tool")]
struct ContractDrivenTool {
    #[view(attr, name = "name")]
    contract_name: &'static str,
}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for ContractDrivenTool {
    async fn on_open(
        &mut self,
        _elem: &XmlElement,
        ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        ctx.mark_speech_started();
        Ok(())
    }
}

#[tokio::test]
async fn streaming_tool_registration_uses_the_derived_contract_identity() {
    let mut runner =
        StreamingToolRunner::new(NpcParseContext::new()).with_tool(ContractDrivenTool {
            contract_name: "contract_driven",
        });

    runner.feed("<contract_driven/>").await;

    assert!(runner.with_context(NpcParseContext::speech_started).await);
}

#[derive(crate::AgentView)]
#[agent_view(kind = "tool")]
struct NamelessTool {}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for NamelessTool {}

#[test]
fn generic_tool_contract_requires_an_explicit_name_attribute() {
    let result = StreamingToolRunner::new(NpcParseContext::new()).try_with_tool(NamelessTool {});

    assert!(matches!(
        result,
        Err(StreamingToolRegistrationError::MissingToolName)
    ));
}

#[derive(crate::AgentView)]
#[agent_view(kind = "named_action")]
struct RootNamedActionTool {
    name: &'static str,
}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for RootNamedActionTool {
    async fn on_open(
        &mut self,
        _elem: &XmlElement,
        ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        ctx.mark_speech_started();
        Ok(())
    }
}

#[tokio::test]
async fn non_tool_contract_uses_its_root_even_when_it_has_a_name_attribute() {
    let mut runner =
        StreamingToolRunner::new(NpcParseContext::new()).with_tool(RootNamedActionTool {
            name: "payload_name",
        });

    runner.feed(r#"<named_action name="payload_name"/>"#).await;

    assert!(runner.with_context(NpcParseContext::speech_started).await);
}

#[derive(crate::AgentView)]
#[agent_view(kind = "update_relationship")]
struct RelationTool {}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for RelationTool {
    async fn on_open(
        &mut self,
        elem: &XmlElement,
        _ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        let Some(raw_delta) = elem.attr("trust_delta") else {
            return Err(StreamingToolError::InvalidAttribute {
                tag: "update_relationship",
                attr: "trust_delta",
                reason: "missing trust delta".into(),
            });
        };

        raw_delta
            .parse::<i32>()
            .map(|_| ())
            .map_err(|_| StreamingToolError::InvalidAttribute {
                tag: "update_relationship",
                attr: "trust_delta",
                reason: "expected integer".into(),
            })
    }
}

#[derive(crate::AgentView)]
#[agent_view(kind = "emit_event")]
struct FailingTool {}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for FailingTool {
    async fn on_complete(
        &mut self,
        _elem: &XmlElement,
        _ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        Err(StreamingToolError::Execution {
            tag: "emit_event",
            source: anyhow::anyhow!("event channel closed"),
        })
    }
}

#[derive(crate::AgentView)]
#[agent_view(kind = "thought")]
struct ThoughtTool {}

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for ThoughtTool {
    async fn on_open(
        &mut self,
        _elem: &XmlElement,
        ctx: &mut NpcParseContext,
    ) -> Result<(), StreamingToolError> {
        if ctx.speech_started() {
            ctx.add_artifact(
                DecisionErrorArtifact::new(DecisionError::FormatReminder {
                    message: "<thought> appeared after <speak>. Put private reasoning before visible speech next time.",
                })
                .into_turn_artifact(),
            );
        }

        Ok(())
    }
}

struct TestTurnUpdate {
    flow: TurnFlow,
    artifacts: Vec<TurnArtifact>,
    task: Option<String>,
}

fn drain_npc_turn_update(ctx: &mut NpcParseContext) -> TestTurnUpdate {
    if ctx.speech_started() {
        TestTurnUpdate {
            flow: TurnFlow::Wait,
            artifacts: Vec::new(),
            task: None,
        }
    } else {
        TestTurnUpdate {
            flow: TurnFlow::Continue,
            artifacts: vec![
                DecisionErrorArtifact::new(DecisionError::MissingRequiredTag {
                    tag: "speak",
                    purpose: "speech",
                })
                .into_turn_artifact(),
            ],
            task: Some(
                "Your previous response did not start speech. Return valid Hermes XML with a <speak> tag."
                    .to_owned(),
            ),
        }
    }
}

#[tokio::test]
async fn streaming_tool_updates_concrete_parse_context_in_parser_order() {
    let mut runner = StreamingToolRunner::new(NpcParseContext::new()).with_tool(SpeakTool {});

    runner.feed("<thought>短想法</thought><speak>你好").await;
    assert!(runner.with_context(|ctx| ctx.speech_started()).await);
    assert_eq!(
        runner.with_context(|ctx| ctx.streamed.clone()).await,
        "你好"
    );

    runner.feed("，陌生人。</speak>").await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert!(ctx.speech_started());
    assert_eq!(ctx.streamed, "你好");
    assert_eq!(ctx.completed, vec!["你好，陌生人。"]);
}

#[tokio::test]
async fn decision_function_returns_retry_artifact_when_required_tool_missing() {
    let mut ctx = NpcParseContext::new();
    let engine = TemplateEngine::new();

    let update = drain_npc_turn_update(&mut ctx);

    assert_eq!(update.flow, TurnFlow::Continue);
    assert_eq!(
        update.task.as_deref(),
        Some(
            "Your previous response did not start speech. Return valid Hermes XML with a <speak> tag."
        )
    );
    assert_eq!(update.artifacts.len(), 1);
    assert_eq!(update.artifacts[0].kind(), "parser_error");
    assert_eq!(
        update.artifacts[0]
            .render_full(&engine)
            .await
            .unwrap()
            .into_string(),
        "<parser_error>Expected a &lt;speak&gt; tag to start speech.</parser_error>"
    );
}

#[tokio::test]
async fn sleep_feedback_uses_behavior_feedback_kind_for_next_awake() {
    let engine = TemplateEngine::new();
    let update = TestTurnUpdate {
        flow: TurnFlow::Wait,
        artifacts: vec![
            BehaviorFeedbackArtifact::new("Use <thought> before <speak> next time.")
                .into_turn_artifact(),
        ],
        task: Some("Remember the previous format reminder.".into()),
    };

    assert_eq!(update.flow, TurnFlow::Wait);
    assert_eq!(
        update.task.as_deref(),
        Some("Remember the previous format reminder.")
    );
    assert_eq!(update.artifacts.len(), 1);
    assert_eq!(update.artifacts[0].kind(), "behavior_feedback");
    assert_eq!(
        update.artifacts[0]
            .render_full(&engine)
            .await
            .unwrap()
            .into_string(),
        "<behavior_feedback>Use &lt;thought&gt; before &lt;speak&gt; next time.</behavior_feedback>"
    );
}

#[tokio::test]
async fn tool_checks_parse_context_before_committing_side_effect() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new()).with_tool(SpeakTool {});

    runner.feed("<speak></speak><speak>second</speak>").await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert!(ctx.speech_started());
    assert_eq!(ctx.completed, vec!["second"]);
    assert_eq!(ctx.artifacts.len(), 2);
    assert_eq!(ctx.artifacts[0].kind(), "parser_error");
    assert_eq!(ctx.artifacts[1].kind(), "parser_error");
    assert_eq!(
        ctx.artifacts[0]
            .render_full(&engine)
            .await
            .unwrap()
            .into_string(),
        "<parser_error>&lt;speak&gt; invalid content: speech content cannot be empty</parser_error>"
    );
    assert_eq!(
        ctx.artifacts[1]
            .render_full(&engine)
            .await
            .unwrap()
            .into_string(),
        "<parser_error>&lt;speak&gt; rejected: duplicate speech tag</parser_error>"
    );
}

#[tokio::test]
async fn invalid_attribute_becomes_parser_error_artifact() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new()).with_tool(RelationTool {});

    runner
        .feed(r#"<update_relationship target="player" trust_delta="high"/>"#)
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.artifacts.len(), 1);
    assert_eq!(ctx.artifacts[0].kind(), "parser_error");
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap().into_string(),
        "<parser_error>&lt;update\\_relationship&gt; invalid attribute \\`trust\\_delta\\`: expected integer</parser_error>"
    );
}

#[tokio::test]
async fn tool_error_artifact_escapes_dynamic_text_through_pom() {
    let artifact = ToolErrorArtifact::new(StreamingToolError::InvalidContent {
        tag: "select",
        reason: "<select>&".to_owned(),
    })
    .into_turn_artifact();

    assert_eq!(
        artifact
            .render_full(&TemplateEngine::new())
            .await
            .unwrap()
            .as_str(),
        "<parser_error>&lt;select&gt; invalid content: &lt;select&gt;&amp;</parser_error>"
    );
}

#[tokio::test]
async fn execution_error_becomes_parser_error_and_stream_continues() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new())
        .with_tool(FailingTool {})
        .with_tool(SpeakTool {});

    runner
        .feed("<emit_event>bad side effect</emit_event><speak>still speaks</speak>")
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.completed, vec!["still speaks"]);
    assert_eq!(ctx.artifacts.len(), 1);
    assert_eq!(
        ctx.artifacts[0]
            .render_full(&engine)
            .await
            .unwrap()
            .into_string(),
        "<parser_error>&lt;emit\\_event&gt; execution failed: event channel closed</parser_error>"
    );
}

#[tokio::test]
async fn multiple_tool_errors_accumulate_without_short_circuiting() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new())
        .with_tool(RelationTool {})
        .with_tool(FailingTool {})
        .with_tool(SpeakTool {});

    runner
        .feed(
            r#"<update_relationship target="player"/><emit_event>x</emit_event><speak>ok</speak>"#,
        )
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.completed, vec!["ok"]);
    assert_eq!(ctx.artifacts.len(), 2);
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap().into_string(),
        "<parser_error>&lt;update\\_relationship&gt; invalid attribute \\`trust\\_delta\\`: missing trust delta</parser_error>"
    );
    assert_eq!(
        ctx.artifacts[1]
            .render_full(&engine)
            .await
            .unwrap()
            .into_string(),
        "<parser_error>&lt;emit\\_event&gt; execution failed: event channel closed</parser_error>"
    );
}

#[tokio::test]
async fn thought_after_speech_is_feedback_but_does_not_rollback_speech() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new())
        .with_tool(SpeakTool {})
        .with_tool(ThoughtTool {});

    runner
        .feed("<speak>visible first</speak><thought>private after speech</thought>")
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.completed, vec!["visible first"]);
    assert_eq!(ctx.artifacts.len(), 1);
    assert_eq!(ctx.artifacts[0].kind(), "parser_error");
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap().into_string(),
        "<parser_error>&lt;thought&gt; appeared after &lt;speak&gt;. Put private reasoning before visible speech next time.</parser_error>"
    );
}
