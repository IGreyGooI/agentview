//! Role-specific resolution of Prompt Object Model documents.

use crate::pom::{
    BlockChildren, BlockContent, ContentNode, ContentRef, Document, HeadingNode, InlineChildren,
    InlineContent, ListItem, ListNode, MarkdownNode, MixedChildren, MixedContent, ParagraphNode,
    ResolvedDocument, StrongNode, XmlNode,
};

#[derive(Debug, Clone, Copy)]
enum SlotPolicy {
    Materialize,
    Reject,
}

/// Errors raised when a document contains a diff slot in a slot-free boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PomResolutionError {
    #[error("turn artifacts must be complete POM documents and cannot contain DiffSlot edges")]
    ArtifactContainsDiffSlot,
}

/// Resolves a system prompt to a complete, diff-slot-free document.
///
/// System prompts do not emit deltas. Present diff slots are therefore
/// expanded recursively and absent diff slots are omitted, irrespective of
/// their role or strategy.
pub fn resolve_system_document(document: Document) -> ResolvedDocument {
    resolve_document(document, SlotPolicy::Materialize)
        .expect("materializing system slots cannot reject a diff slot")
}

/// Validates and resolves an ephemeral turn artifact.
///
/// Artifacts do not own a stateful cursor, so accepting a `DiffSlot` here would
/// silently invent the wrong resolution semantics. Any slot at any depth is
/// rejected instead.
pub fn resolve_artifact_document(
    document: Document,
) -> Result<ResolvedDocument, PomResolutionError> {
    resolve_document(document, SlotPolicy::Reject)
}

fn resolve_document(
    document: Document,
    policy: SlotPolicy,
) -> Result<ResolvedDocument, PomResolutionError> {
    Ok(ResolvedDocument::new(resolve_block_children(
        document.children(),
        policy,
    )?))
}

fn resolve_block_children(
    children: &BlockChildren,
    policy: SlotPolicy,
) -> Result<BlockChildren, PomResolutionError> {
    let mut resolved = BlockChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => resolved.push(resolve_block_node(node, policy)?),
            ContentRef::DiffSlot(slot) => match policy {
                SlotPolicy::Materialize => {
                    if let Some(value) = slot.value() {
                        resolved.push(BlockContent::xml(resolve_xml_node(value, policy)?));
                    }
                }
                SlotPolicy::Reject => return Err(PomResolutionError::ArtifactContainsDiffSlot),
            },
        }
    }

    Ok(resolved)
}

fn resolve_block_node(
    node: &ContentNode,
    policy: SlotPolicy,
) -> Result<BlockContent, PomResolutionError> {
    let resolved = match node {
        ContentNode::Markdown(MarkdownNode::Heading(node)) => {
            BlockContent::heading(HeadingNode::new(
                *node.level(),
                resolve_inline_children(node.children(), policy)?,
            ))
        }
        ContentNode::Markdown(MarkdownNode::Paragraph(node)) => BlockContent::paragraph(
            ParagraphNode::new(resolve_inline_children(node.children(), policy)?),
        ),
        ContentNode::Markdown(MarkdownNode::List(node)) => {
            BlockContent::list(resolve_list_node(node, policy)?)
        }
        ContentNode::Markdown(MarkdownNode::CodeBlock(node)) => {
            BlockContent::code_block(node.clone())
        }
        ContentNode::Markdown(MarkdownNode::ThematicBreak) => BlockContent::thematic_break(),
        ContentNode::Xml(node) => BlockContent::xml(resolve_xml_node(node, policy)?),
        ContentNode::Markdown(MarkdownNode::Strong(_) | MarkdownNode::CodeSpan(_))
        | ContentNode::Text(_) => {
            unreachable!("block children only contain block-level content")
        }
    };

    Ok(resolved)
}

fn resolve_inline_children(
    children: &InlineChildren,
    policy: SlotPolicy,
) -> Result<InlineChildren, PomResolutionError> {
    let mut resolved = InlineChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => resolved.push(resolve_inline_node(node, policy)?),
            ContentRef::DiffSlot(slot) => match policy {
                SlotPolicy::Materialize => {
                    if let Some(value) = slot.value() {
                        resolved.push(InlineContent::xml(resolve_xml_node(value, policy)?));
                    }
                }
                SlotPolicy::Reject => return Err(PomResolutionError::ArtifactContainsDiffSlot),
            },
        }
    }

    Ok(resolved)
}

fn resolve_inline_node(
    node: &ContentNode,
    policy: SlotPolicy,
) -> Result<InlineContent, PomResolutionError> {
    let resolved = match node {
        ContentNode::Markdown(MarkdownNode::Strong(node)) => InlineContent::strong(
            StrongNode::new(resolve_inline_children(node.children(), policy)?),
        ),
        ContentNode::Markdown(MarkdownNode::CodeSpan(node)) => {
            InlineContent::code_span(node.clone())
        }
        ContentNode::Xml(node) => InlineContent::xml(resolve_xml_node(node, policy)?),
        ContentNode::Text(node) => InlineContent::try_from_node(node.clone().into())
            .expect("existing inline text has already been validated"),
        ContentNode::Markdown(
            MarkdownNode::Heading(_)
            | MarkdownNode::Paragraph(_)
            | MarkdownNode::List(_)
            | MarkdownNode::CodeBlock(_)
            | MarkdownNode::ThematicBreak,
        ) => unreachable!("inline children only contain inline-level content"),
    };

    Ok(resolved)
}

fn resolve_list_node(node: &ListNode, policy: SlotPolicy) -> Result<ListNode, PomResolutionError> {
    let items = node
        .items()
        .iter()
        .map(|item| resolve_block_children(item.children(), policy).map(ListItem::new))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ListNode::new(node.kind().clone(), items))
}

fn resolve_mixed_children(
    children: &MixedChildren,
    policy: SlotPolicy,
) -> Result<MixedChildren, PomResolutionError> {
    let mut resolved = MixedChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => {
                resolved.push(MixedContent::node(resolve_mixed_node(node, policy)?))
            }
            ContentRef::DiffSlot(slot) => match policy {
                SlotPolicy::Materialize => {
                    if let Some(value) = slot.value() {
                        resolved.push(MixedContent::xml(resolve_xml_node(value, policy)?));
                    }
                }
                SlotPolicy::Reject => return Err(PomResolutionError::ArtifactContainsDiffSlot),
            },
        }
    }

    Ok(resolved)
}

fn resolve_mixed_node(
    node: &ContentNode,
    policy: SlotPolicy,
) -> Result<ContentNode, PomResolutionError> {
    let resolved = match node {
        ContentNode::Markdown(node) => resolve_markdown_node(node, policy)?.into(),
        ContentNode::Xml(node) => resolve_xml_node(node, policy)?.into(),
        ContentNode::Text(node) => node.clone().into(),
    };
    Ok(resolved)
}

fn resolve_markdown_node(
    node: &MarkdownNode,
    policy: SlotPolicy,
) -> Result<MarkdownNode, PomResolutionError> {
    let resolved = match node {
        MarkdownNode::Heading(node) => MarkdownNode::Heading(HeadingNode::new(
            *node.level(),
            resolve_inline_children(node.children(), policy)?,
        )),
        MarkdownNode::Paragraph(node) => MarkdownNode::Paragraph(ParagraphNode::new(
            resolve_inline_children(node.children(), policy)?,
        )),
        MarkdownNode::List(node) => MarkdownNode::List(resolve_list_node(node, policy)?),
        MarkdownNode::CodeBlock(node) => MarkdownNode::CodeBlock(node.clone()),
        MarkdownNode::ThematicBreak => MarkdownNode::ThematicBreak,
        MarkdownNode::Strong(node) => MarkdownNode::Strong(StrongNode::new(
            resolve_inline_children(node.children(), policy)?,
        )),
        MarkdownNode::CodeSpan(node) => MarkdownNode::CodeSpan(node.clone()),
    };
    Ok(resolved)
}

fn resolve_xml_node(node: &XmlNode, policy: SlotPolicy) -> Result<XmlNode, PomResolutionError> {
    Ok(node
        .clone()
        .with_children(resolve_mixed_children(node.children(), policy)?))
}
