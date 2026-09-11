use crate::StorageString;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextNode {
    value: StorageString,
}

impl TextNode {
    pub fn new(value: impl Into<StorageString>) -> Self {
        Self {
            value: value.into(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn append(&mut self, value: &str) {
        self.value.push_str(value);
    }
}

/// One opaque source-text block.
///
/// Unlike [`TextNode`], raw text is not Markdown content. At a document root
/// it is rendered exactly as supplied; when embedded in XML it is escaped as
/// XML character data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RawTextNode {
    value: StorageString,
}

impl RawTextNode {
    pub fn new(value: impl Into<StorageString>) -> Self {
        Self {
            value: value.into(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}
