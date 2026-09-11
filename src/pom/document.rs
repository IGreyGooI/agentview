use super::{BlockBuilder, BlockChildren, BlockContent, PomError, RawTextNode, XmlNode};
use crate::StorageString;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    #[doc(hidden)]
    pub fn into_children(self) -> BlockChildren {
        self.children
    }

    pub fn from_xml(node: XmlNode) -> Self {
        let mut children = BlockChildren::new();
        children.push(BlockContent::xml(node));
        Self::new(children)
    }

    pub fn from_raw_text(value: impl Into<StorageString>) -> Self {
        let mut children = BlockChildren::new();
        children.push(BlockContent::raw_text(RawTextNode::new(value)));
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
/// Resolved documents can only be produced by the system, user, or artifact
/// resolution boundaries. This keeps callers from accidentally treating an
/// unresolved [`Document`] as a complete prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedDocument {
    children: BlockChildren,
}

impl<'de> serde::Deserialize<'de> for ResolvedDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let document = Document::deserialize(deserializer)?;
        crate::pom_resolution::resolve_artifact_document(document).map_err(serde::de::Error::custom)
    }
}

impl ResolvedDocument {
    pub(crate) fn new(children: BlockChildren) -> Self {
        Self { children }
    }

    pub fn children(&self) -> &BlockChildren {
        &self.children
    }
}
