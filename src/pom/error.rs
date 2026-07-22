use crate::StorageString;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PomError {
    #[error("invalid XML name: {value}")]
    InvalidXmlName { value: StorageString },
    #[error("invalid Markdown heading level: {value}")]
    InvalidHeadingLevel { value: u8 },
}
