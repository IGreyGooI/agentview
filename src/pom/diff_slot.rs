use super::{XmlName, XmlNode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffStrategy {
    Recursive,
    Replace,
    Append,
    Sequence,
    Set,
    Keyed(XmlName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSlot {
    role: XmlName,
    strategy: DiffStrategy,
    value: Option<XmlNode>,
}

impl DiffSlot {
    pub fn present(strategy: DiffStrategy, value: XmlNode) -> Self {
        Self {
            role: value.name().clone(),
            strategy,
            value: Some(value),
        }
    }

    pub fn absent(role: XmlName, strategy: DiffStrategy) -> Self {
        Self {
            role,
            strategy,
            value: None,
        }
    }

    pub fn role(&self) -> &XmlName {
        &self.role
    }

    pub fn strategy(&self) -> &DiffStrategy {
        &self.strategy
    }

    pub fn value(&self) -> Option<&XmlNode> {
        self.value.as_ref()
    }

    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }
}
