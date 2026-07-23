//! Role-specific resolution of Prompt Object Model documents.

use crate::pom::{
    BlockChildren, BlockContent, ContentNode, ContentRef, HeadingNode, InlineChildren,
    InlineContent, ListItem, ListNode, MarkdownNode, MixedChildren, MixedContent, ParagraphNode,
    ResolvedDocument, StrongNode, XmlNode,
};

/// Resolves a system prompt to a complete, diff-slot-free document.
///
/// System prompts do not emit deltas. Present diff slots are therefore
/// expanded recursively and absent diff slots are omitted, irrespective of
/// their role or strategy.
pub fn resolve_system_document(document: crate::pom::Document) -> ResolvedDocument {
    ResolvedDocument::new(resolve_block_children(document.children()))
}

fn resolve_block_children(children: &BlockChildren) -> BlockChildren {
    let mut resolved = BlockChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => resolved.push(resolve_block_node(node)),
            ContentRef::DiffSlot(slot) => {
                if let Some(value) = slot.value() {
                    resolved.push(BlockContent::xml(resolve_xml_node(value)));
                }
            }
        }
    }

    resolved
}

fn resolve_block_node(node: &ContentNode) -> BlockContent {
    match node {
        ContentNode::Markdown(MarkdownNode::Heading(node)) => BlockContent::heading(
            HeadingNode::new(*node.level(), resolve_inline_children(node.children())),
        ),
        ContentNode::Markdown(MarkdownNode::Paragraph(node)) => {
            BlockContent::paragraph(ParagraphNode::new(resolve_inline_children(node.children())))
        }
        ContentNode::Markdown(MarkdownNode::List(node)) => {
            BlockContent::list(resolve_list_node(node))
        }
        ContentNode::Markdown(MarkdownNode::CodeBlock(node)) => {
            BlockContent::code_block(node.clone())
        }
        ContentNode::Markdown(MarkdownNode::ThematicBreak) => BlockContent::thematic_break(),
        ContentNode::Xml(node) => BlockContent::xml(resolve_xml_node(node)),
        ContentNode::Markdown(MarkdownNode::Strong(_) | MarkdownNode::CodeSpan(_))
        | ContentNode::Text(_) => {
            unreachable!("block children only contain block-level content")
        }
    }
}

fn resolve_inline_children(children: &InlineChildren) -> InlineChildren {
    let mut resolved = InlineChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => resolved.push(resolve_inline_node(node)),
            ContentRef::DiffSlot(slot) => {
                if let Some(value) = slot.value() {
                    resolved.push(InlineContent::xml(resolve_xml_node(value)));
                }
            }
        }
    }

    resolved
}

fn resolve_inline_node(node: &ContentNode) -> InlineContent {
    match node {
        ContentNode::Markdown(MarkdownNode::Strong(node)) => {
            InlineContent::strong(StrongNode::new(resolve_inline_children(node.children())))
        }
        ContentNode::Markdown(MarkdownNode::CodeSpan(node)) => {
            InlineContent::code_span(node.clone())
        }
        ContentNode::Xml(node) => InlineContent::xml(resolve_xml_node(node)),
        ContentNode::Text(node) => InlineContent::try_from_node(node.clone().into())
            .expect("existing inline text has already been validated"),
        ContentNode::Markdown(
            MarkdownNode::Heading(_)
            | MarkdownNode::Paragraph(_)
            | MarkdownNode::List(_)
            | MarkdownNode::CodeBlock(_)
            | MarkdownNode::ThematicBreak,
        ) => unreachable!("inline children only contain inline-level content"),
    }
}

fn resolve_list_node(node: &ListNode) -> ListNode {
    let items = node
        .items()
        .iter()
        .map(|item| ListItem::new(resolve_block_children(item.children())))
        .collect();
    ListNode::new(node.kind().clone(), items)
}

fn resolve_mixed_children(children: &MixedChildren) -> MixedChildren {
    let mut resolved = MixedChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => resolved.push(MixedContent::node(resolve_mixed_node(node))),
            ContentRef::DiffSlot(slot) => {
                if let Some(value) = slot.value() {
                    resolved.push(MixedContent::xml(resolve_xml_node(value)));
                }
            }
        }
    }

    resolved
}

fn resolve_mixed_node(node: &ContentNode) -> ContentNode {
    match node {
        ContentNode::Markdown(node) => resolve_markdown_node(node).into(),
        ContentNode::Xml(node) => resolve_xml_node(node).into(),
        ContentNode::Text(node) => node.clone().into(),
    }
}

fn resolve_markdown_node(node: &MarkdownNode) -> MarkdownNode {
    match node {
        MarkdownNode::Heading(node) => MarkdownNode::Heading(HeadingNode::new(
            *node.level(),
            resolve_inline_children(node.children()),
        )),
        MarkdownNode::Paragraph(node) => {
            MarkdownNode::Paragraph(ParagraphNode::new(resolve_inline_children(node.children())))
        }
        MarkdownNode::List(node) => MarkdownNode::List(resolve_list_node(node)),
        MarkdownNode::CodeBlock(node) => MarkdownNode::CodeBlock(node.clone()),
        MarkdownNode::ThematicBreak => MarkdownNode::ThematicBreak,
        MarkdownNode::Strong(node) => {
            MarkdownNode::Strong(StrongNode::new(resolve_inline_children(node.children())))
        }
        MarkdownNode::CodeSpan(node) => MarkdownNode::CodeSpan(node.clone()),
    }
}

fn resolve_xml_node(node: &XmlNode) -> XmlNode {
    node.clone()
        .with_children(resolve_mixed_children(node.children()))
}
