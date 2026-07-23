//! Shared template engine and prompt rendering traits.
//!
//! Application-specific system frames and instruction blocks should live in the
//! application crate. This crate only owns the reusable rendering surface.

// ── Minijinja-backed engine ───────────────────────────────────────────────────

use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::pom::{Document, PomError, ResolvedDocument, XmlName};
use crate::pom_renderer::render_pom_document;
use crate::pom_resolution::{resolve_artifact_document, PomResolutionError};
use crate::semantic_view::{render_agent_view_diff_xml, render_agent_view_xml, AgentViewRoot};
use crate::StorageString;

pub const AGENT_SYSTEM_LAYOUT_TEMPLATE: &str = "agent_system_layout";
pub const AGENT_USER_LAYOUT_TEMPLATE: &str = "agent_user_layout";

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("template engine lock poisoned")]
    LockPoisoned,

    #[error("failed to compile template `{name}`")]
    Compile {
        name: String,
        #[source]
        source: minijinja::Error,
    },

    #[error("failed to render template `{name}`")]
    Render {
        name: String,
        #[source]
        source: minijinja::Error,
    },
}

/// Minijinja renderer shared by prompt renderables.
///
/// The caller supplies the template source at render time. This keeps SQL,
/// hardcoded, and future persisted prompt sources on the same path.
#[derive(Clone)]
pub struct TemplateEngine {
    inner: Arc<RwLock<minijinja::Environment<'static>>>,
}

/// Rendered prompt text with room for non-provider metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFragment {
    text: String,
    meta: PromptFragmentMeta,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFragmentMeta {
    pub memo: Option<String>,
    pub indent_depth: usize,
    pub attrs: Vec<(String, String)>,
}

impl PromptFragment {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            meta: PromptFragmentMeta::default(),
        }
    }

    pub fn with_memo(mut self, memo: impl Into<String>) -> Self {
        self.meta.memo = Some(memo.into());
        self
    }

    pub fn with_indent_depth(mut self, indent_depth: usize) -> Self {
        self.meta.indent_depth = indent_depth;
        self
    }

    pub fn with_attr(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.meta.attrs.push((key.into(), value.into()));
        self
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn memo(&self) -> Option<&str> {
        self.meta.memo.as_deref()
    }

    pub fn indent_depth(&self) -> usize {
        self.meta.indent_depth
    }

    pub fn meta(&self) -> &PromptFragmentMeta {
        &self.meta
    }

    pub fn into_string(self) -> String {
        self.text
    }
}

impl From<String> for PromptFragment {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for PromptFragment {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

// ── Prompt layout primitives ─────────────────────────────────────────────────

/// Ephemeral information for one LLM turn that is not part of persistent context.
///
/// The payload is a resolved POM document, so artifact authors cannot inject
/// trusted raw prompt markup or carry stateful diff slots into a turn.
#[derive(Debug, Clone, Serialize)]
pub struct TurnArtifact {
    kind: XmlName,
    document: ResolvedDocument,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnArtifactError {
    #[error("invalid turn artifact kind `{kind}`")]
    InvalidKind {
        kind: StorageString,
        #[source]
        source: PomError,
    },

    #[error(transparent)]
    Resolution(#[from] PomResolutionError),
}

impl TurnArtifact {
    pub fn try_from_document(
        kind: impl Into<StorageString>,
        document: Document,
    ) -> Result<Self, TurnArtifactError> {
        let kind = kind.into();
        let kind = XmlName::new(kind.clone())
            .map_err(|source| TurnArtifactError::InvalidKind { kind, source })?;
        Ok(Self {
            kind,
            document: resolve_artifact_document(document)?,
        })
    }

    pub fn kind(&self) -> &str {
        self.kind.as_str()
    }

    pub fn document(&self) -> &ResolvedDocument {
        &self.document
    }
}

#[derive(Debug, Clone, Serialize)]
/// Internal compatibility DTO for the legacy Minijinja user envelope.
pub struct RenderedTurnArtifact {
    kind: StorageString,
    rendered: String,
}

impl RenderedTurnArtifact {
    pub(crate) fn new(kind: impl Into<StorageString>, rendered: String) -> Self {
        Self {
            kind: kind.into(),
            rendered,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBlockKind {
    Full,
    Delta,
    Empty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSystemVars {
    pub instructions: String,
    pub output_schema: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptUserVars {
    pub context_kind: ContextBlockKind,
    pub context_block: String,
    #[serde(default, skip_deserializing)]
    pub artifacts: Vec<RenderedTurnArtifact>,
    pub task: String,
}

/// Agent-specific prompt envelope templates.
pub trait PromptLayout {
    fn system_template(&self) -> &'static str;
    fn user_template(&self) -> &'static str;
}

impl TemplateEngine {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(minijinja::Environment::new())),
        }
    }

    pub fn render_template(
        &self,
        name: &str,
        source: &str,
        ctx: minijinja::Value,
    ) -> Result<String, TemplateError> {
        let mut env = self
            .inner
            .write()
            .map_err(|_| TemplateError::LockPoisoned)?;
        env.remove_template(name);
        env.add_template_owned(name.to_owned(), source.to_owned())
            .map_err(|source| TemplateError::Compile {
                name: name.to_owned(),
                source,
            })?;
        env.get_template(name)
            .and_then(|t| t.render(ctx))
            .map_err(|source| TemplateError::Render {
                name: name.to_owned(),
                source,
            })
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Prompt / context view traits ──────────────────────────────────────────────

/// Anything that can render itself as a prompt block.
///
/// This is intentionally smaller than [`ContextView`]. It can be used by root
/// context views, leaf views, and future turn artifacts.
#[async_trait::async_trait]
pub trait PromptRenderable: Send + Sync {
    async fn render_full<'a>(&'a self, templates: &'a TemplateEngine) -> Result<PromptFragment>;
}

#[async_trait::async_trait]
impl PromptRenderable for TurnArtifact {
    async fn render_full<'a>(&'a self, _templates: &'a TemplateEngine) -> Result<PromptFragment> {
        Ok(render_pom_document(&self.document)?.into())
    }
}

#[async_trait::async_trait]
impl PromptRenderable for String {
    async fn render_full<'a>(&'a self, _templates: &'a TemplateEngine) -> Result<PromptFragment> {
        Ok(self.clone().into())
    }
}

#[async_trait::async_trait]
impl PromptRenderable for ResolvedDocument {
    async fn render_full<'a>(&'a self, _templates: &'a TemplateEngine) -> Result<PromptFragment> {
        Ok(render_pom_document(self)?.into())
    }
}

/// Renderable semantic context view.
///
/// Self-contained context is a root prompt-context invariant. Nested views may
/// render stable handles to content introduced by sibling root sections or by
/// earlier committed prompt context; they do not need to locally hydrate every
/// reference they print.
#[async_trait::async_trait]
pub trait ContextView: PromptRenderable + Sized {
    /// Render only what changed since `prev` (warm path / subsequent calls).
    ///
    /// Returns `None` if nothing has changed — caller skips the delta block.
    async fn render_delta<'a>(
        &'a self,
        prev: &'a Self,
        templates: &'a TemplateEngine,
    ) -> Result<Option<PromptFragment>>;
}

#[async_trait::async_trait]
impl<T> PromptRenderable for T
where
    T: AgentViewRoot + Send + Sync,
{
    async fn render_full<'a>(&'a self, _templates: &'a TemplateEngine) -> Result<PromptFragment> {
        Ok(render_agent_view_xml(self).into())
    }
}

#[async_trait::async_trait]
impl<T> ContextView for T
where
    T: AgentViewRoot + Send + Sync,
{
    async fn render_delta<'a>(
        &'a self,
        prev: &'a Self,
        _templates: &'a TemplateEngine,
    ) -> Result<Option<PromptFragment>> {
        Ok(render_agent_view_diff_xml(self, prev).map(Into::into))
    }
}

#[async_trait::async_trait]
impl ContextView for String {
    async fn render_delta<'a>(
        &'a self,
        prev: &'a Self,
        _templates: &'a TemplateEngine,
    ) -> Result<Option<PromptFragment>> {
        if self == prev {
            Ok(None)
        } else {
            Ok(Some(self.clone().into()))
        }
    }
}

/// Builds a fresh root context view from an application-specific source/runtime.
///
/// Scope/identity belongs on the builder, not on generic [`crate::agent::Agent`].
#[async_trait::async_trait]
pub trait ContextViewBuilder: Send + Sync {
    type Source: Sync;
    type View: ContextView;

    async fn capture(&self, source: &Self::Source) -> Self::View;
}

#[cfg(test)]
mod tests {
    use super::{ContextView, PromptRenderable, TemplateEngine};

    #[tokio::test]
    async fn string_prompt_renders_as_plain_text() {
        let fragment = "hello <world>"
            .to_owned()
            .render_full(&TemplateEngine::new())
            .await
            .unwrap();

        assert_eq!(fragment.as_str(), "hello <world>");
    }

    #[tokio::test]
    async fn string_context_delta_is_empty_when_unchanged() {
        let current = "hello".to_owned();
        let previous = "hello".to_owned();

        let delta = current
            .render_delta(&previous, &TemplateEngine::new())
            .await
            .unwrap();

        assert!(delta.is_none());
    }

    #[tokio::test]
    async fn string_context_delta_renders_new_text_when_changed() {
        let current = "hello world".to_owned();
        let previous = "hello".to_owned();

        let delta = current
            .render_delta(&previous, &TemplateEngine::new())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(delta.as_str(), "hello world");
    }
}
