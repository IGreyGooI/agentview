//! Agent streaming-tool layer above [`crate::stream_parser::HermesParser`].
//!
//! `HermesParser` remains the low-level XML scanner. This module adds the
//! reusable agent-facing layer from `plans/llm-agent-context-prompt-streaming-tool-design.md`:
//! tools are registered by tag, receive ordered open/stream/complete callbacks,
//! and write parse facts/artifacts into a concrete [`ParseContext`].
//!
//! These are "streaming tools" only because the model emits them as text/XML.
//! They are part of the [`TurnSink`](crate::llm_call::TurnSink) layer: parse the
//! assistant text, update per-turn state, and let the agent loop decide whether
//! to continue or sleep. They are not provider-native tools like `rig::Tool`;
//! native tool execution belongs in the application [`LLMExecutor`](crate::llm_call::LLMExecutor).

use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;

use crate::llm_call::{TextTurnEvent, TurnSink};
use crate::stream_parser::{HermesParser, XmlElement};
use crate::templates::{PromptRenderable, TemplateEngine, TurnArtifact};

// ── Errors / artifacts ────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum StreamingToolError {
    #[error("<{tag}> invalid attribute `{attr}`: {reason}")]
    InvalidAttribute {
        tag: &'static str,
        attr: &'static str,
        reason: String,
    },

    #[error("<{tag}> invalid content: {reason}")]
    InvalidContent { tag: &'static str, reason: String },

    #[error("<{tag}> rejected: {reason}")]
    Rejected { tag: &'static str, reason: String },

    #[error("<{tag}> execution failed: {source}")]
    Execution {
        tag: &'static str,
        #[source]
        source: anyhow::Error,
    },
}

pub struct ToolErrorArtifact {
    pub error: StreamingToolError,
}

impl ToolErrorArtifact {
    pub fn new(error: StreamingToolError) -> Self {
        Self { error }
    }

    pub fn into_turn_artifact(self) -> TurnArtifact {
        TurnArtifact {
            kind: "parser_error".to_owned(),
            payload: Box::new(self),
        }
    }
}

#[async_trait::async_trait]
impl PromptRenderable for ToolErrorArtifact {
    async fn render_full<'a>(&'a self, _engine: &'a TemplateEngine) -> anyhow::Result<String> {
        Ok(format!("<parser_error>{}</parser_error>", self.error))
    }
}

// ── ParseContext ──────────────────────────────────────────────────────────────

/// Generic surface shared by all agent-specific parse contexts.
///
/// Concrete contexts add domain facts and helpers, such as `speech_started` for
/// NPC response parsing or selected intent drafts for player intent parsing.
pub trait ParseContext {
    fn raw_output(&self) -> &str;
    fn set_raw_output(&mut self, output: String);

    fn add_artifact(&mut self, artifact: TurnArtifact);
    fn artifacts(&self) -> &[TurnArtifact];
}

// ── StreamingTool ─────────────────────────────────────────────────────────────

/// Per-tag handler for XML function-call style streaming output.
///
/// Tools own validation and commit boundaries for their tag. They should update
/// the concrete parse context rather than deciding the whole agent loop.
#[async_trait::async_trait]
pub trait StreamingTool<C: ParseContext>: Send {
    fn tag(&self) -> &'static str;

    async fn on_open(
        &mut self,
        _elem: &XmlElement,
        _ctx: &mut C,
    ) -> Result<(), StreamingToolError> {
        Ok(())
    }

    async fn on_stream(
        &mut self,
        _elem: &XmlElement,
        _ctx: &mut C,
    ) -> Result<(), StreamingToolError> {
        Ok(())
    }

    async fn on_complete(
        &mut self,
        _elem: &XmlElement,
        _ctx: &mut C,
    ) -> Result<(), StreamingToolError> {
        Ok(())
    }
}

type SharedTool<C> = Arc<TokioMutex<Box<dyn StreamingTool<C>>>>;

/// Owns a parse context plus the tools registered for a single streamed response.
pub struct StreamingToolRunner<C: ParseContext> {
    ctx: Arc<TokioMutex<C>>,
    parser: HermesParser,
}

impl<C: ParseContext + Send + 'static> StreamingToolRunner<C> {
    pub fn new(ctx: C) -> Self {
        Self {
            ctx: Arc::new(TokioMutex::new(ctx)),
            parser: HermesParser::new(),
        }
    }

    pub fn with_tool<T>(mut self, tool: T) -> Self
    where
        T: StreamingTool<C> + 'static,
    {
        self.register_tool(tool);
        self
    }

    pub fn register_tool<T>(&mut self, tool: T)
    where
        T: StreamingTool<C> + 'static,
    {
        let tag = tool.tag();
        let tool: SharedTool<C> = Arc::new(TokioMutex::new(Box::new(tool)));

        let open_tool = Arc::clone(&tool);
        let open_ctx = Arc::clone(&self.ctx);
        self.parser.on_open(tag, move |elem| {
            let open_tool = Arc::clone(&open_tool);
            let open_ctx = Arc::clone(&open_ctx);
            Box::pin(async move {
                let result = {
                    let mut tool = open_tool.lock().await;
                    let mut ctx = open_ctx.lock().await;
                    tool.on_open(&elem, &mut ctx).await
                };
                if let Err(error) = result {
                    open_ctx
                        .lock()
                        .await
                        .add_artifact(ToolErrorArtifact::new(error).into_turn_artifact());
                }
            })
        });

        let stream_tool = Arc::clone(&tool);
        let stream_ctx = Arc::clone(&self.ctx);
        self.parser.on_stream(tag, move |elem| {
            let stream_tool = Arc::clone(&stream_tool);
            let stream_ctx = Arc::clone(&stream_ctx);
            Box::pin(async move {
                let result = {
                    let mut tool = stream_tool.lock().await;
                    let mut ctx = stream_ctx.lock().await;
                    tool.on_stream(&elem, &mut ctx).await
                };
                if let Err(error) = result {
                    stream_ctx
                        .lock()
                        .await
                        .add_artifact(ToolErrorArtifact::new(error).into_turn_artifact());
                }
            })
        });

        let complete_tool = Arc::clone(&tool);
        let complete_ctx = Arc::clone(&self.ctx);
        self.parser.on_complete(tag, move |elem| {
            let complete_tool = Arc::clone(&complete_tool);
            let complete_ctx = Arc::clone(&complete_ctx);
            Box::pin(async move {
                let result = {
                    let mut tool = complete_tool.lock().await;
                    let mut ctx = complete_ctx.lock().await;
                    tool.on_complete(&elem, &mut ctx).await
                };
                if let Err(error) = result {
                    complete_ctx
                        .lock()
                        .await
                        .add_artifact(ToolErrorArtifact::new(error).into_turn_artifact());
                }
            })
        });
    }

    pub async fn feed(&mut self, chunk: &str) {
        self.parser.feed(chunk).await;
    }

    pub async fn finalize(&mut self) {
        self.parser.finalize().await;
    }

    pub async fn with_context<R>(&self, f: impl FnOnce(&C) -> R) -> R {
        let ctx = self.ctx.lock().await;
        f(&ctx)
    }

    pub async fn with_context_mut<R>(&self, f: impl FnOnce(&mut C) -> R) -> R {
        let mut ctx = self.ctx.lock().await;
        f(&mut ctx)
    }

    pub async fn into_context(self) -> C {
        let Self { ctx, parser } = self;
        drop(parser);
        Arc::try_unwrap(ctx)
            .unwrap_or_else(|_| panic!("StreamingToolRunner context still has active references"))
            .into_inner()
    }
}

#[async_trait::async_trait]
impl<C: ParseContext + Send + 'static> TurnSink for StreamingToolRunner<C> {
    type Output = C;

    async fn on_event(&mut self, event: TextTurnEvent) {
        match event {
            TextTurnEvent::TextDelta(chunk) => self.feed(&chunk).await,
            TextTurnEvent::TextComplete(full_text) => {
                self.ctx.lock().await.set_raw_output(full_text);
            }
        }
    }

    async fn finish(mut self: Box<Self>) -> Self::Output {
        self.finalize().await;
        let runner = *self;
        runner.into_context().await
    }
}

#[cfg(test)]
mod tests;
