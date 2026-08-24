//! Durable baselines for stateful user-document diff slots.

use std::collections::BTreeMap;

use crate::pom::{DiffStrategy, XmlName, XmlNode};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UserDocumentCursor {
    slots: BTreeMap<XmlName, SlotBaseline>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct SlotBaseline {
    strategy: DiffStrategy,
    value: XmlNode,
}

impl UserDocumentCursor {
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn contains(&self, role: &XmlName) -> bool {
        self.slots.contains_key(role)
    }

    pub fn strategy(&self, role: &XmlName) -> Option<&DiffStrategy> {
        self.slots.get(role).map(|baseline| &baseline.strategy)
    }

    pub fn value(&self, role: &XmlName) -> Option<&XmlNode> {
        self.slots.get(role).map(|baseline| &baseline.value)
    }

    pub fn remove(&mut self, role: &XmlName) -> bool {
        self.slots.remove(role).is_some()
    }

    pub fn clear(&mut self) {
        self.slots.clear();
    }

    pub(crate) fn baseline(&self, role: &XmlName) -> Option<(&DiffStrategy, &XmlNode)> {
        self.slots
            .get(role)
            .map(|baseline| (&baseline.strategy, &baseline.value))
    }

    pub(crate) fn insert(&mut self, role: XmlName, strategy: DiffStrategy, value: XmlNode) {
        self.slots.insert(role, SlotBaseline { strategy, value });
    }
}
