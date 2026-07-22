use crate::StorageString;

use super::XmlName;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ContentContext {
    Block,
    Inline,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ContentKind {
    Markdown(MarkdownKind),
    Xml,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum MarkdownKind {
    Heading,
    Paragraph,
    List,
    CodeBlock,
    ThematicBreak,
    Strong,
    CodeSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, thiserror::Error)]
pub enum PomError {
    #[error("invalid XML name: {value}")]
    InvalidXmlName { value: StorageString },
    #[error("duplicate XML attribute: {name}")]
    DuplicateXmlAttribute { name: XmlName },
    #[error("invalid Markdown heading level: {value}")]
    InvalidHeadingLevel { value: u8 },
    #[error("inline text cannot contain a newline: {value:?}")]
    InvalidInlineNewline { value: StorageString },
    #[error("wrong content context: expected {expected:?}, got {actual:?}")]
    WrongContentContext {
        expected: ContentContext,
        actual: ContentKind,
    },
}
