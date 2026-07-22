use super::{
    content::ContentEdge, BlockContent, ContentNode, ContentRef, InlineContent, MixedContent,
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
            | None => Some(ContentEdge::Node(ContentNode::Text(text))),
        },
        ContentEdge::Node(ContentNode::Markdown(node)) => {
            Some(ContentEdge::Node(ContentNode::Markdown(node)))
        }
        ContentEdge::Node(ContentNode::Xml(node)) => {
            Some(ContentEdge::Node(ContentNode::Xml(node)))
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
