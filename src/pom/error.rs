use crate::StorageString;

use super::XmlName;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PomError {
    #[error("invalid XML name: {value}")]
    InvalidXmlName { value: StorageString },
    #[error("duplicate XML attribute: {name}")]
    DuplicateXmlAttribute { name: XmlName },
    #[error("invalid Markdown heading level: {value}")]
    InvalidHeadingLevel { value: u8 },
}
