use super::{BlockBuilder, BlockChildren, PomError};

#[derive(Debug, Clone, PartialEq, Eq)]
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
