use crate::StorageString;

use super::{
    CodeBlockNode, CodeSpanNode, ContentContext, ContentKind, HeadingNode, ListNode, MarkdownKind,
    MarkdownNode, ParagraphNode, PomError, StrongNode, TextNode, XmlNode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentNode {
    Markdown(MarkdownNode),
    Xml(XmlNode),
    Text(TextNode),
}

impl ContentNode {
    pub fn kind(&self) -> ContentKind {
        match self {
            Self::Markdown(node) => ContentKind::Markdown(node.kind()),
            Self::Xml(_) => ContentKind::Xml,
            Self::Text(_) => ContentKind::Text,
        }
    }
}

impl MarkdownNode {
    pub fn kind(&self) -> MarkdownKind {
        match self {
            Self::Heading(_) => MarkdownKind::Heading,
            Self::Paragraph(_) => MarkdownKind::Paragraph,
            Self::List(_) => MarkdownKind::List,
            Self::CodeBlock(_) => MarkdownKind::CodeBlock,
            Self::ThematicBreak => MarkdownKind::ThematicBreak,
            Self::Strong(_) => MarkdownKind::Strong,
            Self::CodeSpan(_) => MarkdownKind::CodeSpan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ContentEdge {
    Node(ContentNode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRef<'a> {
    Node(&'a ContentNode),
}

impl ContentEdge {
    fn as_ref(&self) -> ContentRef<'_> {
        match self {
            Self::Node(node) => ContentRef::Node(node),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockContent(ContentEdge);

impl BlockContent {
    fn markdown(node: MarkdownNode) -> Self {
        debug_assert!(node.is_block());
        Self(ContentEdge::Node(ContentNode::Markdown(node)))
    }

    pub fn heading(node: HeadingNode) -> Self {
        Self::markdown(MarkdownNode::Heading(node))
    }

    pub fn paragraph(node: ParagraphNode) -> Self {
        Self::markdown(MarkdownNode::Paragraph(node))
    }

    pub fn list(node: ListNode) -> Self {
        Self::markdown(MarkdownNode::List(node))
    }

    pub fn code_block(node: CodeBlockNode) -> Self {
        Self::markdown(MarkdownNode::CodeBlock(node))
    }

    pub fn thematic_break() -> Self {
        Self::markdown(MarkdownNode::ThematicBreak)
    }

    pub fn xml(node: XmlNode) -> Self {
        Self(ContentEdge::Node(ContentNode::Xml(node)))
    }

    pub fn try_from_node(node: ContentNode) -> Result<Self, PomError> {
        match node.kind() {
            ContentKind::Markdown(
                MarkdownKind::Heading
                | MarkdownKind::Paragraph
                | MarkdownKind::List
                | MarkdownKind::CodeBlock
                | MarkdownKind::ThematicBreak,
            )
            | ContentKind::Xml => Ok(Self(ContentEdge::Node(node))),
            ContentKind::Markdown(MarkdownKind::Strong | MarkdownKind::CodeSpan)
            | ContentKind::Text => Err(PomError::WrongContentContext {
                expected: ContentContext::Block,
                actual: node.kind(),
            }),
        }
    }

    pub(crate) fn as_ref(&self) -> ContentRef<'_> {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineContent(pub(super) ContentEdge);

impl InlineContent {
    fn markdown(node: MarkdownNode) -> Self {
        debug_assert!(node.is_inline());
        Self(ContentEdge::Node(ContentNode::Markdown(node)))
    }

    pub fn try_text(value: impl Into<StorageString>) -> Result<Self, PomError> {
        Self::try_text_node(TextNode::new(value))
    }

    pub fn strong(node: StrongNode) -> Self {
        Self::markdown(MarkdownNode::Strong(node))
    }

    pub fn code_span(node: CodeSpanNode) -> Self {
        Self::markdown(MarkdownNode::CodeSpan(node))
    }

    pub fn xml(node: XmlNode) -> Self {
        Self(ContentEdge::Node(ContentNode::Xml(node)))
    }

    pub fn try_from_node(node: ContentNode) -> Result<Self, PomError> {
        match node {
            ContentNode::Text(node) => Self::try_text_node(node),
            ContentNode::Xml(node) => Ok(Self::xml(node)),
            ContentNode::Markdown(node) => match node.kind() {
                MarkdownKind::Strong | MarkdownKind::CodeSpan => Ok(Self::markdown(node)),
                kind @ (MarkdownKind::Heading
                | MarkdownKind::Paragraph
                | MarkdownKind::List
                | MarkdownKind::CodeBlock
                | MarkdownKind::ThematicBreak) => Err(PomError::WrongContentContext {
                    expected: ContentContext::Inline,
                    actual: ContentKind::Markdown(kind),
                }),
            },
        }
    }

    fn try_text_node(node: TextNode) -> Result<Self, PomError> {
        if node.value().contains('\n') || node.value().contains('\r') {
            return Err(PomError::InvalidInlineNewline {
                value: node.value().into(),
            });
        }

        Ok(Self(ContentEdge::Node(ContentNode::Text(node))))
    }

    pub(crate) fn as_ref(&self) -> ContentRef<'_> {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedContent(pub(super) ContentEdge);

impl MixedContent {
    pub fn node(node: ContentNode) -> Self {
        Self(ContentEdge::Node(node))
    }

    pub fn text(node: TextNode) -> Self {
        Self::node(node.into())
    }

    pub fn markdown(node: MarkdownNode) -> Self {
        Self::node(node.into())
    }

    pub fn xml(node: XmlNode) -> Self {
        Self::node(node.into())
    }

    pub(crate) fn as_ref(&self) -> ContentRef<'_> {
        self.0.as_ref()
    }
}

impl From<MarkdownNode> for ContentNode {
    fn from(node: MarkdownNode) -> Self {
        Self::Markdown(node)
    }
}

impl From<XmlNode> for ContentNode {
    fn from(node: XmlNode) -> Self {
        Self::Xml(node)
    }
}

impl From<TextNode> for ContentNode {
    fn from(node: TextNode) -> Self {
        Self::Text(node)
    }
}
