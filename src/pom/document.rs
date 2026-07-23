use super::{BlockBuilder, BlockChildren, BlockContent, PomError, XmlNode};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Document {
    children: BlockChildren,
}

impl Document {
    pub fn new(children: BlockChildren) -> Self {
        Self { children }
    }

    pub fn children(&self) -> &BlockChildren {
        &self.children
    }

    pub fn from_xml(node: XmlNode) -> Self {
        let mut children = BlockChildren::new();
        children.push(BlockContent::xml(node));
        Self::new(children)
    }

    pub fn build(build: impl FnOnce(&mut BlockBuilder<'_>)) -> Self {
        let mut children = BlockChildren::new();
        build(&mut BlockBuilder::new(&mut children));
        Self::new(children)
    }

    pub fn try_build(
        build: impl FnOnce(&mut BlockBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<Self, PomError> {
        let mut children = BlockChildren::new();
        build(&mut BlockBuilder::new(&mut children))?;
        Ok(Self::new(children))
    }
}

/// A complete prompt document with every diff slot resolved.
///
/// Resolved documents can only be produced by a role-specific resolver. This
/// keeps callers from accidentally treating an unresolved [`Document`] as a
/// complete prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedDocument {
    children: BlockChildren,
}

impl ResolvedDocument {
    pub(crate) fn new(children: BlockChildren) -> Self {
        Self { children }
    }

    pub fn children(&self) -> &BlockChildren {
        &self.children
    }
}
