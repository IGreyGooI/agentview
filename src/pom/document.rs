use super::BlockChildren;

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
}
