use crate::StorageString;

use super::{BlockBuilder, BlockChildren, InlineChildren, PomError, TextNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HeadingLevel {
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
}

impl HeadingLevel {
    pub fn number(self) -> u8 {
        match self {
            Self::H1 => 1,
            Self::H2 => 2,
            Self::H3 => 3,
            Self::H4 => 4,
            Self::H5 => 5,
            Self::H6 => 6,
        }
    }
}

impl TryFrom<u8> for HeadingLevel {
    type Error = PomError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::H1),
            2 => Ok(Self::H2),
            3 => Ok(Self::H3),
            4 => Ok(Self::H4),
            5 => Ok(Self::H5),
            6 => Ok(Self::H6),
            value => Err(PomError::InvalidHeadingLevel { value }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MarkdownNode {
    Heading(HeadingNode),
    Paragraph(ParagraphNode),
    List(ListNode),
    CodeBlock(CodeBlockNode),
    ThematicBreak,
    Strong(StrongNode),
    CodeSpan(CodeSpanNode),
}

impl MarkdownNode {
    pub(crate) fn is_block(&self) -> bool {
        match self {
            Self::Heading(_)
            | Self::Paragraph(_)
            | Self::List(_)
            | Self::CodeBlock(_)
            | Self::ThematicBreak => true,
            Self::Strong(_) | Self::CodeSpan(_) => false,
        }
    }

    pub(crate) fn is_inline(&self) -> bool {
        match self {
            Self::Heading(_)
            | Self::Paragraph(_)
            | Self::List(_)
            | Self::CodeBlock(_)
            | Self::ThematicBreak => false,
            Self::Strong(_) | Self::CodeSpan(_) => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeadingNode {
    level: HeadingLevel,
    children: InlineChildren,
}

impl HeadingNode {
    pub fn new(level: HeadingLevel, children: InlineChildren) -> Self {
        Self { level, children }
    }

    pub fn level(&self) -> &HeadingLevel {
        &self.level
    }

    pub fn children(&self) -> &InlineChildren {
        &self.children
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParagraphNode {
    children: InlineChildren,
}

impl ParagraphNode {
    pub fn new(children: InlineChildren) -> Self {
        Self { children }
    }

    pub fn children(&self) -> &InlineChildren {
        &self.children
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StrongNode {
    children: InlineChildren,
}

impl StrongNode {
    pub fn new(children: InlineChildren) -> Self {
        Self { children }
    }

    pub fn children(&self) -> &InlineChildren {
        &self.children
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListNode {
    kind: ListKind,
    items: Vec<ListItem>,
}

impl ListNode {
    pub fn new(kind: ListKind, items: Vec<ListItem>) -> Self {
        Self { kind, items }
    }

    pub fn kind(&self) -> &ListKind {
        &self.kind
    }

    pub fn items(&self) -> &[ListItem] {
        &self.items
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ListKind {
    Unordered,
    Ordered { start: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListItem {
    children: BlockChildren,
}

impl ListItem {
    pub fn new(children: BlockChildren) -> Self {
        Self { children }
    }

    pub fn children(&self) -> &BlockChildren {
        &self.children
    }
}

pub struct ListBuilder {
    items: Vec<ListItem>,
}

impl ListBuilder {
    pub(crate) fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn item(&mut self, build: impl FnOnce(&mut BlockBuilder<'_>)) {
        let mut children = BlockChildren::new();
        build(&mut BlockBuilder::new(&mut children));
        self.items.push(ListItem::new(children));
    }

    pub fn try_item(
        &mut self,
        build: impl FnOnce(&mut BlockBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<(), PomError> {
        let mut children = BlockChildren::new();
        build(&mut BlockBuilder::new(&mut children))?;
        self.items.push(ListItem::new(children));
        Ok(())
    }

    pub(crate) fn finish(self) -> Vec<ListItem> {
        self.items
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodeBlockNode {
    language: Option<StorageString>,
    body: TextNode,
}

impl CodeBlockNode {
    pub fn new(language: Option<StorageString>, body: TextNode) -> Self {
        Self { language, body }
    }

    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    pub fn body(&self) -> &TextNode {
        &self.body
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodeSpanNode {
    body: TextNode,
}

impl CodeSpanNode {
    pub fn new(body: TextNode) -> Self {
        Self { body }
    }

    pub fn body(&self) -> &TextNode {
        &self.body
    }
}
