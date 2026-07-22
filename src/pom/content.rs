use super::{
    CodeBlockNode, CodeSpanNode, HeadingNode, ListNode, MarkdownNode, ParagraphNode, PomError,
    StrongNode, TextNode, XmlNode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentNode {
    Markdown(MarkdownNode),
    Xml(XmlNode),
    Text(TextNode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContentEdge {
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

    pub(crate) fn as_ref(&self) -> ContentRef<'_> {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineContent(ContentEdge);

impl InlineContent {
    fn markdown(node: MarkdownNode) -> Self {
        debug_assert!(node.is_inline());
        Self(ContentEdge::Node(ContentNode::Markdown(node)))
    }

    pub fn try_text(node: TextNode) -> Result<Self, PomError> {
        Ok(Self(ContentEdge::Node(ContentNode::Text(node))))
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

    pub(crate) fn as_ref(&self) -> ContentRef<'_> {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedContent(ContentEdge);

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
