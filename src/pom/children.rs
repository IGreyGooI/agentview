use super::{BlockContent, ContentRef, InlineContent, MixedContent};

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
        self.0.push(content);
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
        self.0.push(content);
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
