use crate::StorageString;

use std::fmt;

use super::{ContentNode, MixedBuilder, MixedChildren, PomError};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XmlName {
    value: StorageString,
}

impl XmlName {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, PomError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let is_valid = bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));

        if is_valid {
            Ok(Self { value })
        } else {
            Err(PomError::InvalidXmlName { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for XmlName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for XmlName {
    type Error = PomError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlAttribute {
    name: XmlName,
    value: StorageString,
}

impl XmlAttribute {
    pub fn new(name: XmlName, value: impl Into<StorageString>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }

    pub fn name(&self) -> &XmlName {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

#[derive(Debug, Clone, Default)]
pub struct XmlAttributes(Vec<XmlAttribute>);

impl XmlAttributes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        name: XmlName,
        value: impl Into<StorageString>,
    ) -> Result<(), PomError> {
        if self.0.iter().any(|attribute| attribute.name == name) {
            return Err(PomError::DuplicateXmlAttribute { name });
        }

        self.0.push(XmlAttribute::new(name, value));
        Ok(())
    }

    pub fn try_insert(
        &mut self,
        name: &str,
        value: impl Into<StorageString>,
    ) -> Result<(), PomError> {
        self.insert(XmlName::try_from(name)?, value)
    }

    pub fn get(&self, name: &XmlName) -> Option<&XmlAttribute> {
        self.0.iter().find(|attribute| attribute.name() == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &XmlAttribute> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for XmlAttributes {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .0
                .iter()
                .all(|attribute| other.get(attribute.name()) == Some(attribute))
    }
}

impl Eq for XmlAttributes {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct XmlMetadata {
    collection_kind: Option<IntrinsicCollectionKind>,
    identity: Option<Box<ContentNode>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum IntrinsicCollectionKind {
    Map,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlNode {
    name: XmlName,
    attributes: XmlAttributes,
    children: MixedChildren,
    metadata: XmlMetadata,
}

impl XmlNode {
    pub fn new(name: XmlName) -> Self {
        Self {
            name,
            attributes: XmlAttributes::new(),
            children: MixedChildren::new(),
            metadata: XmlMetadata::default(),
        }
    }

    pub fn build(name: XmlName, build: impl FnOnce(&mut MixedBuilder<'_>)) -> Self {
        let mut node = Self::new(name);
        build(&mut MixedBuilder::new(&mut node.children));
        node
    }

    pub fn try_build(
        raw_name: &str,
        build: impl FnOnce(&mut MixedBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<Self, PomError> {
        let mut node = Self::new(XmlName::try_from(raw_name)?);
        build(&mut MixedBuilder::new(&mut node.children))?;
        Ok(node)
    }

    pub fn name(&self) -> &XmlName {
        &self.name
    }

    pub fn attributes(&self) -> &XmlAttributes {
        &self.attributes
    }

    pub fn children(&self) -> &MixedChildren {
        &self.children
    }

    pub fn push_attribute(
        &mut self,
        name: XmlName,
        value: impl Into<StorageString>,
    ) -> Result<(), PomError> {
        self.attributes.insert(name, value)
    }

    pub fn push(&mut self, content: super::MixedContent) {
        self.children.push(content);
    }
}
