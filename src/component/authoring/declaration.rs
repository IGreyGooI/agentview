use crate::{
    agent_view::{AgentView, IntoBlockChildren},
    pom::{
        CodeSpanNode, Document, InlineChildren, InlineContent, MixedContent, ParagraphNode,
        PomError, TextNode, XmlName, XmlNode,
    },
};

#[cfg(any(feature = "legacy-provider-port", test))]
use super::event_listener::EventListenerDeclaration;
use super::render_context::HookRenderContext;
use super::streaming_xml::{
    StreamingXmlTag, StreamingXmlTagDeclaration, XmlStreamingToolCallDeclaration,
};

pub struct Component {
    pub(crate) node: ComponentNode,
}

impl Component {
    pub(crate) fn from_node(node: ComponentNode) -> Self {
        Self { node }
    }
}

pub(crate) enum ComponentNode {
    Fragment(Vec<Component>),
    Pom(PomFragment),
    Scope {
        function: &'static str,
        render: ScopeRender,
    },
    Placement {
        placement: Placement,
        child: Box<Component>,
    },
    Diff {
        slot: &'static str,
        child: Box<Component>,
    },
    #[cfg(any(feature = "legacy-provider-port", test))]
    EventListener(EventListenerDeclaration),
    StreamingXmlTag(Box<StreamingXmlTagDeclaration>),
    XmlStreamingToolCall(Box<XmlStreamingToolCallDeclaration>),
    StreamingAttempt(Box<super::streaming_attempt::ContractDeclaration>),
    NativeToolCall(Box<super::native_tool::NativeToolCallDeclaration>),
}

pub(crate) type RepeatableRender =
    Box<dyn for<'render> Fn(&mut HookRenderContext<'render>) -> Component + Send + 'static>;

pub(crate) enum ScopeRender {
    Fresh(Box<dyn FnOnce() -> Component + Send + 'static>),
    Repeatable(RepeatableRender),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    SystemOnce,
    Developer,
    DeveloperRepeat,
    User,
    UserRepeat,
    Assistant,
}

#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum MacroPlacement {
    SystemOnce,
    Developer,
    DeveloperRepeat,
    User,
    UserRepeat,
    Assistant,
}

/// One prompt-facing text template retained as static segments and typed slots.
#[doc(hidden)]
pub struct TextTemplate {
    parts: Vec<TextTemplatePart>,
}

impl TextTemplate {
    pub(crate) fn new(parts: Vec<TextTemplatePart>) -> Self {
        Self { parts }
    }

    fn materialize(self) -> String {
        let capacity = self.parts.iter().map(TextTemplatePart::rendered_len).sum();
        let mut text = String::with_capacity(capacity);
        for part in self.parts {
            match part {
                TextTemplatePart::Static(value) => text.push_str(value),
                TextTemplatePart::Slot(slot) => {
                    debug_assert!(!slot.value_type.is_empty());
                    text.push_str(&slot.rendered);
                }
            }
        }
        text
    }
}

/// Macro-only text-template part.
#[doc(hidden)]
pub enum TextTemplatePart {
    Static(&'static str),
    Slot(TextTemplateSlot),
}

impl TextTemplatePart {
    fn rendered_len(&self) -> usize {
        match self {
            Self::Static(value) => value.len(),
            Self::Slot(slot) => slot.rendered.len(),
        }
    }
}

/// A formatted value that retains its Rust type separately from template text.
#[doc(hidden)]
pub struct TextTemplateSlot {
    value_type: &'static str,
    rendered: String,
}

impl TextTemplateSlot {
    pub(crate) fn new(value_type: &'static str, rendered: String) -> Self {
        Self {
            value_type,
            rendered,
        }
    }
}

impl From<MacroPlacement> for Placement {
    fn from(value: MacroPlacement) -> Self {
        match value {
            MacroPlacement::SystemOnce => Self::SystemOnce,
            MacroPlacement::Developer => Self::Developer,
            MacroPlacement::DeveloperRepeat => Self::DeveloperRepeat,
            MacroPlacement::User => Self::User,
            MacroPlacement::UserRepeat => Self::UserRepeat,
            MacroPlacement::Assistant => Self::Assistant,
        }
    }
}

pub(crate) enum PomFragment {
    Paragraph(TextTemplate),
    MarkdownParagraph(Vec<MarkdownInlineTemplate>),
    Xml(XmlTemplate),
    Document(Result<Document, PomError>),
}

impl PomFragment {
    pub(crate) fn into_document(self) -> Result<Document, PomError> {
        match self {
            Self::Paragraph(text) => Document::try_build(|blocks| {
                blocks.try_paragraph(|inline| inline.try_text(text.materialize()))
            }),
            Self::MarkdownParagraph(children) => {
                let mut inline = InlineChildren::new();
                for child in children {
                    inline.push(child.into_inline_content()?);
                }
                Document::try_build(|blocks| {
                    blocks.push(crate::pom::BlockContent::paragraph(ParagraphNode::new(
                        inline,
                    )));
                    Ok(())
                })
            }
            Self::Xml(template) => template.into_node().map(Document::from_xml),
            Self::Document(document) => document,
        }
    }
}

#[doc(hidden)]
pub enum MarkdownInlineTemplate {
    Text(TextTemplate),
    CodeSpan(TextTemplate),
}

impl MarkdownInlineTemplate {
    pub(crate) fn into_inline_content(self) -> Result<InlineContent, PomError> {
        match self {
            Self::Text(text) => InlineContent::try_text(text.materialize()),
            Self::CodeSpan(text) => Ok(InlineContent::code_span(CodeSpanNode::new(TextNode::new(
                text.materialize(),
            )))),
        }
    }
}

/// Sealed conversion for a dynamic `view!` root expression.
///
/// This is intentionally narrower than `Display`: `String`, `&String`, and
/// `&str` become opaque source-text document roots, while ordinary domain
/// scalars remain rejected until the author uses a quoted template or
/// `format!(...)`.
#[doc(hidden)]
pub trait IntoViewRoot: private::Sealed {
    fn into_view_root(self) -> Component;
}

/// Sealed conversion for a typed POM value used directly under `#[diff]`.
///
/// Unlike [`IntoViewRoot`], this deliberately excludes `Component`: a diff
/// boundary owns exactly one POM root, never an arbitrary Component subtree.
#[doc(hidden)]
pub trait IntoDiffViewRoot: private::Sealed {
    fn into_diff_view_root(self) -> Component;
}

impl IntoViewRoot for Component {
    fn into_view_root(self) -> Component {
        self
    }
}

impl IntoViewRoot for StreamingXmlTag {
    fn into_view_root(self) -> Component {
        self.into_component()
    }
}

fn raw_text_root(value: impl Into<crate::StorageString>) -> Component {
    Component::from_node(ComponentNode::Pom(PomFragment::Document(Ok(
        Document::from_raw_text(value),
    ))))
}

impl private::Sealed for String {}

impl IntoViewRoot for String {
    fn into_view_root(self) -> Component {
        raw_text_root(self)
    }
}

impl IntoDiffViewRoot for String {
    fn into_diff_view_root(self) -> Component {
        raw_text_root(self)
    }
}

impl<'a> private::Sealed for &'a str {}

impl<'a> IntoViewRoot for &'a str {
    fn into_view_root(self) -> Component {
        raw_text_root(self)
    }
}

impl<'a> IntoDiffViewRoot for &'a str {
    fn into_diff_view_root(self) -> Component {
        raw_text_root(self)
    }
}

impl<'a> private::Sealed for &'a String {}

impl<'a> IntoViewRoot for &'a String {
    fn into_view_root(self) -> Component {
        raw_text_root(self.as_str())
    }
}

impl<'a> IntoDiffViewRoot for &'a String {
    fn into_diff_view_root(self) -> Component {
        raw_text_root(self.as_str())
    }
}

impl<T> private::Sealed for T
where
    T: AgentView,
    T::Root: IntoBlockChildren,
{
}

impl<T> IntoViewRoot for T
where
    T: AgentView,
    T::Root: IntoBlockChildren,
{
    fn into_view_root(self) -> Component {
        let document = self
            .build_root()
            .map(|root| Document::new(root.into_block_children()));
        Component::from_node(ComponentNode::Pom(PomFragment::Document(document)))
    }
}

impl<T> IntoDiffViewRoot for T
where
    T: AgentView,
    T::Root: IntoBlockChildren,
{
    fn into_diff_view_root(self) -> Component {
        let document = self
            .build_root()
            .map(|root| Document::new(root.into_block_children()));
        Component::from_node(ComponentNode::Pom(PomFragment::Document(document)))
    }
}

impl private::Sealed for Component {}
impl private::Sealed for StreamingXmlTag {}

mod private {
    pub trait Sealed {}
}

#[doc(hidden)]
pub struct XmlTemplate {
    name: &'static str,
    attributes: Vec<(&'static str, TextTemplate)>,
    children: Vec<XmlTemplateChild>,
}

impl XmlTemplate {
    pub(crate) fn new(
        name: &'static str,
        attributes: Vec<(&'static str, TextTemplate)>,
        children: Vec<XmlTemplateChild>,
    ) -> Self {
        Self {
            name,
            attributes,
            children,
        }
    }

    fn into_node(self) -> Result<XmlNode, PomError> {
        let mut node = XmlNode::new(XmlName::new(self.name)?);
        for (name, value) in self.attributes {
            node.push_attribute(XmlName::new(name)?, value.materialize())?;
        }
        for child in self.children {
            match child {
                XmlTemplateChild::Text(text) => {
                    node.push(MixedContent::text(TextNode::new(text.materialize())));
                }
                XmlTemplateChild::Element(child) => {
                    node.push(MixedContent::xml(child.into_node()?));
                }
            }
        }
        Ok(node)
    }
}

#[doc(hidden)]
pub enum XmlTemplateChild {
    Text(TextTemplate),
    Element(XmlTemplate),
}

#[cfg(test)]
mod tests {
    use super::{TextTemplate, TextTemplatePart, TextTemplateSlot};

    #[test]
    fn text_template_retains_static_segments_and_typed_slots_until_materialization() {
        let slot = TextTemplateSlot::new(std::any::type_name::<u8>(), String::from("03"));
        assert_eq!(slot.value_type, "u8");

        let template = TextTemplate::new(vec![
            TextTemplatePart::Static("attempt="),
            TextTemplatePart::Slot(slot),
            TextTemplatePart::Static("."),
        ]);
        assert_eq!(template.materialize(), "attempt=03.");
    }
}
