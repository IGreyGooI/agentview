use crate::StorageString;

use super::{
    content::ContentEdge, BlockContent, CodeBlockNode, CodeSpanNode, ContentNode, ContentRef,
    DiffSlot, HeadingLevel, HeadingNode, InlineContent, ListBuilder, ListKind, ListNode,
    MarkdownNode, MixedContent, ParagraphNode, PomError, StrongNode, TextNode, XmlNode,
};

fn normalize_text_edge(
    previous: Option<&mut ContentEdge>,
    incoming: ContentEdge,
) -> Option<ContentEdge> {
    match incoming {
        ContentEdge::Node(ContentNode::Text(text)) if text.is_empty() => None,
        ContentEdge::Node(ContentNode::Text(text)) => match previous {
            Some(ContentEdge::Node(ContentNode::Text(previous))) => {
                previous.append(text.value());
                None
            }
            Some(ContentEdge::Node(ContentNode::Markdown(_)))
            | Some(ContentEdge::Node(ContentNode::Xml(_)))
            | Some(ContentEdge::Diff(_))
            | None => Some(ContentEdge::Node(ContentNode::Text(text))),
        },
        ContentEdge::Node(ContentNode::Markdown(node)) => {
            Some(ContentEdge::Node(ContentNode::Markdown(node)))
        }
        ContentEdge::Node(ContentNode::Xml(node)) => {
            Some(ContentEdge::Node(ContentNode::Xml(node)))
        }
        ContentEdge::Diff(slot) => Some(ContentEdge::Diff(slot)),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BlockChildren(Vec<BlockContent>);

impl BlockChildren {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, content: BlockContent) {
        self.0.push(content);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ContentRef<'_>> {
        self.0.iter().map(BlockContent::as_ref)
    }
}

pub struct BlockBuilder<'a> {
    children: &'a mut BlockChildren,
}

impl<'a> BlockBuilder<'a> {
    pub(crate) fn new(children: &'a mut BlockChildren) -> Self {
        Self { children }
    }

    pub fn push(&mut self, content: BlockContent) {
        self.children.push(content);
    }

    pub fn heading(&mut self, level: HeadingLevel, build: impl FnOnce(&mut InlineBuilder<'_>)) {
        let mut children = InlineChildren::new();
        build(&mut InlineBuilder::new(&mut children));
        self.push(BlockContent::heading(HeadingNode::new(level, children)));
    }

    pub fn try_heading(
        &mut self,
        raw_level: u8,
        build: impl FnOnce(&mut InlineBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<(), PomError> {
        let level = HeadingLevel::try_from(raw_level)?;
        let mut children = InlineChildren::new();
        build(&mut InlineBuilder::new(&mut children))?;
        self.push(BlockContent::heading(HeadingNode::new(level, children)));
        Ok(())
    }

    pub fn paragraph(&mut self, build: impl FnOnce(&mut InlineBuilder<'_>)) {
        let mut children = InlineChildren::new();
        build(&mut InlineBuilder::new(&mut children));
        self.push(BlockContent::paragraph(ParagraphNode::new(children)));
    }

    pub fn try_paragraph(
        &mut self,
        build: impl FnOnce(&mut InlineBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<(), PomError> {
        let mut children = InlineChildren::new();
        build(&mut InlineBuilder::new(&mut children))?;
        self.push(BlockContent::paragraph(ParagraphNode::new(children)));
        Ok(())
    }

    pub fn list(&mut self, kind: ListKind, build: impl FnOnce(&mut ListBuilder)) {
        let mut builder = ListBuilder::new();
        build(&mut builder);
        self.push(BlockContent::list(ListNode::new(kind, builder.finish())));
    }

    pub fn try_list(
        &mut self,
        kind: ListKind,
        build: impl FnOnce(&mut ListBuilder) -> Result<(), PomError>,
    ) -> Result<(), PomError> {
        let mut builder = ListBuilder::new();
        build(&mut builder)?;
        self.push(BlockContent::list(ListNode::new(kind, builder.finish())));
        Ok(())
    }

    pub fn code_block(&mut self, language: Option<StorageString>, body: TextNode) {
        self.push(BlockContent::code_block(CodeBlockNode::new(language, body)));
    }

    pub fn thematic_break(&mut self) {
        self.push(BlockContent::thematic_break());
    }

    pub fn xml(&mut self, node: XmlNode) {
        self.push(BlockContent::xml(node));
    }

    pub fn xml_slot(&mut self, slot: DiffSlot) {
        self.push(BlockContent::xml_slot(slot));
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct InlineChildren(Vec<InlineContent>);

impl InlineChildren {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, content: InlineContent) {
        if let Some(edge) =
            normalize_text_edge(self.0.last_mut().map(|previous| &mut previous.0), content.0)
        {
            self.0.push(InlineContent(edge));
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ContentRef<'_>> {
        self.0.iter().map(InlineContent::as_ref)
    }
}

pub struct InlineBuilder<'a> {
    children: &'a mut InlineChildren,
}

impl<'a> InlineBuilder<'a> {
    fn new(children: &'a mut InlineChildren) -> Self {
        Self { children }
    }

    pub fn push(&mut self, content: InlineContent) {
        self.children.push(content);
    }

    pub fn try_text(&mut self, value: impl Into<StorageString>) -> Result<(), PomError> {
        self.push(InlineContent::try_text(value)?);
        Ok(())
    }

    pub fn strong(&mut self, build: impl FnOnce(&mut InlineBuilder<'_>)) {
        let mut children = InlineChildren::new();
        build(&mut InlineBuilder::new(&mut children));
        self.push(InlineContent::strong(StrongNode::new(children)));
    }

    pub fn try_strong(
        &mut self,
        build: impl FnOnce(&mut InlineBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<(), PomError> {
        let mut children = InlineChildren::new();
        build(&mut InlineBuilder::new(&mut children))?;
        self.push(InlineContent::strong(StrongNode::new(children)));
        Ok(())
    }

    pub fn code_span(&mut self, body: TextNode) {
        self.push(InlineContent::code_span(CodeSpanNode::new(body)));
    }

    pub fn xml(&mut self, node: XmlNode) {
        self.push(InlineContent::xml(node));
    }

    pub fn xml_slot(&mut self, slot: DiffSlot) {
        self.push(InlineContent::xml_slot(slot));
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MixedChildren(Vec<MixedContent>);

impl MixedChildren {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, content: MixedContent) {
        if let Some(edge) =
            normalize_text_edge(self.0.last_mut().map(|previous| &mut previous.0), content.0)
        {
            self.0.push(MixedContent(edge));
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ContentRef<'_>> {
        self.0.iter().map(MixedContent::as_ref)
    }
}

pub struct MixedBuilder<'a> {
    children: &'a mut MixedChildren,
}

impl<'a> MixedBuilder<'a> {
    pub(crate) fn new(children: &'a mut MixedChildren) -> Self {
        Self { children }
    }

    pub fn push(&mut self, content: MixedContent) {
        self.children.push(content);
    }

    pub fn text(&mut self, node: TextNode) {
        self.push(MixedContent::text(node));
    }

    pub fn markdown(&mut self, node: MarkdownNode) {
        self.push(MixedContent::markdown(node));
    }

    pub fn xml(&mut self, node: XmlNode) {
        self.push(MixedContent::xml(node));
    }

    pub fn xml_slot(&mut self, slot: DiffSlot) {
        self.push(MixedContent::xml_slot(slot));
    }
}
