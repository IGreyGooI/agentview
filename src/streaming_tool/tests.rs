use super::*;
use crate::templates::TemplateEngine;

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

struct DecisionErrorArtifact {
    error: DecisionError,
}

struct BehaviorFeedbackArtifact {
    message: &'static str,
}

impl BehaviorFeedbackArtifact {
    fn new(message: &'static str) -> Self {
        Self { message }
    }

    fn into_turn_artifact(self) -> TurnArtifact {
        TurnArtifact {
            kind: "behavior_feedback".to_owned(),
            payload: Box::new(self),
        }
    }
}

impl DecisionErrorArtifact {
    fn new(error: DecisionError) -> Self {
        Self { error }
    }

    fn into_turn_artifact(self) -> TurnArtifact {
        TurnArtifact {
            kind: "parser_error".to_owned(),
            payload: Box::new(self),
        }
    }
}

#[async_trait::async_trait]
impl PromptRenderable for DecisionErrorArtifact {
    async fn render_full<'a>(&'a self, _engine: &'a TemplateEngine) -> anyhow::Result<String> {
        Ok(format!("<parser_error>{}</parser_error>", self.error))
    }
}

#[async_trait::async_trait]
impl PromptRenderable for BehaviorFeedbackArtifact {
    async fn render_full<'a>(&'a self, _engine: &'a TemplateEngine) -> anyhow::Result<String> {
        Ok(format!(
            "<behavior_feedback>{}</behavior_feedback>",
            self.message
        ))
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

struct SpeakTool;

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for SpeakTool {
    fn tag(&self) -> &'static str {
        "speak"
    }

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

struct RelationTool;

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for RelationTool {
    fn tag(&self) -> &'static str {
        "update_relationship"
    }

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

struct FailingTool;

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for FailingTool {
    fn tag(&self) -> &'static str {
        "emit_event"
    }

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

struct ThoughtTool;

#[async_trait::async_trait]
impl StreamingTool<NpcParseContext> for ThoughtTool {
    fn tag(&self) -> &'static str {
        "thought"
    }

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

fn decide_npc_speak(ctx: &NpcParseContext) -> AgentTurnControl {
    if ctx.speech_started() {
        AgentTurnControl::sleep()
    } else {
        AgentTurnControl::continue_with(
            "Your previous response did not start speech. Return valid Hermes XML with a <speak> tag.",
            vec![
                DecisionErrorArtifact::new(DecisionError::MissingRequiredTag {
                    tag: "speak",
                    purpose: "speech",
                })
                .into_turn_artifact(),
            ],
        )
    }
}

#[tokio::test]
async fn streaming_tool_updates_concrete_parse_context_in_parser_order() {
    let mut runner = StreamingToolRunner::new(NpcParseContext::new()).with_tool(SpeakTool);

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
    let ctx = NpcParseContext::new();
    let engine = TemplateEngine::new();

    let control = decide_npc_speak(&ctx);

    assert_eq!(control.return_policy, ReturnPolicy::Continue);
    assert_eq!(
        control.feedback.task.as_deref(),
        Some(
            "Your previous response did not start speech. Return valid Hermes XML with a <speak> tag."
        )
    );
    assert_eq!(control.feedback.artifacts.len(), 1);
    assert_eq!(control.feedback.artifacts[0].kind, "parser_error");
    assert_eq!(
        control.feedback.artifacts[0]
            .render_full(&engine)
            .await
            .unwrap(),
        "<parser_error>Expected a <speak> tag to start speech.</parser_error>"
    );
}

#[tokio::test]
async fn sleep_feedback_uses_behavior_feedback_kind_for_next_awake() {
    let engine = TemplateEngine::new();
    let control = AgentTurnControl::sleep_with_feedback(AgentFeedback::new(
        vec![
            BehaviorFeedbackArtifact::new("Use <thought> before <speak> next time.")
                .into_turn_artifact(),
        ],
        Some("Remember the previous format reminder.".into()),
    ));

    assert_eq!(control.return_policy, ReturnPolicy::Sleep);
    assert_eq!(
        control.feedback.task.as_deref(),
        Some("Remember the previous format reminder.")
    );
    assert_eq!(control.feedback.artifacts.len(), 1);
    assert_eq!(control.feedback.artifacts[0].kind, "behavior_feedback");
    assert_eq!(
        control.feedback.artifacts[0]
            .render_full(&engine)
            .await
            .unwrap(),
        "<behavior_feedback>Use <thought> before <speak> next time.</behavior_feedback>"
    );
}

#[tokio::test]
async fn tool_checks_parse_context_before_committing_side_effect() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new()).with_tool(SpeakTool);

    runner.feed("<speak></speak><speak>second</speak>").await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert!(ctx.speech_started());
    assert_eq!(ctx.completed, vec!["second"]);
    assert_eq!(ctx.artifacts.len(), 2);
    assert_eq!(ctx.artifacts[0].kind, "parser_error");
    assert_eq!(ctx.artifacts[1].kind, "parser_error");
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap(),
        "<parser_error><speak> invalid content: speech content cannot be empty</parser_error>"
    );
    assert_eq!(
        ctx.artifacts[1].render_full(&engine).await.unwrap(),
        "<parser_error><speak> rejected: duplicate speech tag</parser_error>"
    );
}

#[tokio::test]
async fn invalid_attribute_becomes_parser_error_artifact() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new()).with_tool(RelationTool);

    runner
        .feed(r#"<update_relationship target="player" trust_delta="high"/>"#)
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.artifacts.len(), 1);
    assert_eq!(ctx.artifacts[0].kind, "parser_error");
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap(),
        "<parser_error><update_relationship> invalid attribute `trust_delta`: expected integer</parser_error>"
    );
}

#[tokio::test]
async fn execution_error_becomes_parser_error_and_stream_continues() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new())
        .with_tool(FailingTool)
        .with_tool(SpeakTool);

    runner
        .feed("<emit_event>bad side effect</emit_event><speak>still speaks</speak>")
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.completed, vec!["still speaks"]);
    assert_eq!(ctx.artifacts.len(), 1);
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap(),
        "<parser_error><emit_event> execution failed: event channel closed</parser_error>"
    );
}

#[tokio::test]
async fn multiple_tool_errors_accumulate_without_short_circuiting() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new())
        .with_tool(RelationTool)
        .with_tool(FailingTool)
        .with_tool(SpeakTool);

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
        ctx.artifacts[0].render_full(&engine).await.unwrap(),
        "<parser_error><update_relationship> invalid attribute `trust_delta`: missing trust delta</parser_error>"
    );
    assert_eq!(
        ctx.artifacts[1].render_full(&engine).await.unwrap(),
        "<parser_error><emit_event> execution failed: event channel closed</parser_error>"
    );
}

#[tokio::test]
async fn thought_after_speech_is_feedback_but_does_not_rollback_speech() {
    let engine = TemplateEngine::new();
    let mut runner = StreamingToolRunner::new(NpcParseContext::new())
        .with_tool(SpeakTool)
        .with_tool(ThoughtTool);

    runner
        .feed("<speak>visible first</speak><thought>private after speech</thought>")
        .await;
    runner.finalize().await;

    let ctx = runner.into_context().await;
    assert_eq!(ctx.completed, vec!["visible first"]);
    assert_eq!(ctx.artifacts.len(), 1);
    assert_eq!(ctx.artifacts[0].kind, "parser_error");
    assert_eq!(
        ctx.artifacts[0].render_full(&engine).await.unwrap(),
        "<parser_error><thought> appeared after <speak>. Put private reasoning before visible speech next time.</parser_error>"
    );
}
