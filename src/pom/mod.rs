//! Typed Prompt Object Model for the LLM's view.
//!
//! POM is not XML. Tag-shaped nodes organize content; their rendered text is
//! not a machine-readable serialization format and receives no XML escaping.
//! Code must inspect these typed structures rather than parse rendered prompts.
//! Unit tests verify structures, state, and behavior, not prompt rendering.

mod children;
mod content;
mod diff_slot;
mod document;
mod error;
mod markdown;
mod text;
mod xml;

pub use children::{
    BlockBuilder, BlockChildren, InlineBuilder, InlineChildren, MixedBuilder, MixedChildren,
};
pub use content::{BlockContent, ContentNode, ContentRef, InlineContent, MixedContent};
pub use diff_slot::{DiffSlot, DiffStrategy};
pub use document::{Document, ResolvedDocument};
pub use error::{ContentContext, ContentKind, MarkdownKind, PomError};
pub use markdown::{
    CodeBlockNode, CodeSpanNode, HeadingLevel, HeadingNode, ListBuilder, ListItem, ListKind,
    ListNode, MarkdownNode, ParagraphNode, StrongNode,
};
pub use text::{RawTextNode, TextNode};
pub use xml::{XmlAttribute, XmlAttributes, XmlName, XmlNode};
