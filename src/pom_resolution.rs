//! Role-specific resolution of Prompt Object Model documents.

use crate::pom::{
    BlockChildren, BlockContent, ContentNode, ContentRef, Document, HeadingNode, InlineChildren,
    InlineContent, ListItem, ListNode, MarkdownNode, MixedChildren, MixedContent, ParagraphNode,
    ResolvedDocument, StrongNode, XmlNode,
};
use crate::pom_cursor::UserDocumentCursor;
use crate::pom_diff::diff_xml_values;
use std::collections::BTreeSet;

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

    #[error("user document contains duplicate outermost DiffSlot role `{role}`")]
    DuplicateUserDiffSlotRole { role: crate::pom::XmlName },
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

/// Resolves a current user document against the last committed slot baselines.
///
/// Ordinary content is emitted from the current document every turn. Only
/// explicitly marked outermost XML slots participate in the durable cursor.
pub fn resolve_user_document(
    document: Document,
    previous: &UserDocumentCursor,
) -> Result<(ResolvedDocument, UserDocumentCursor), PomResolutionError> {
    validate_unique_outermost_roles(document.children())?;
    let mut resolver = UserResolver {
        previous,
        next: previous.clone(),
    };
    let children = resolver.resolve_block_children(document.children())?;
    Ok((ResolvedDocument::new(children), resolver.next))
}

struct UserResolver<'a> {
    previous: &'a UserDocumentCursor,
    next: UserDocumentCursor,
}

impl UserResolver<'_> {
    fn resolve_block_children(
        &mut self,
        children: &BlockChildren,
    ) -> Result<BlockChildren, PomResolutionError> {
        let mut resolved = BlockChildren::new();
        for child in children.iter() {
            match child {
                ContentRef::Node(node) => {
                    if let Some(node) = self.resolve_block_node(node)? {
                        resolved.push(node);
                    }
                }
                ContentRef::DiffSlot(slot) => {
                    if let Some(node) = self.resolve_slot(slot)? {
                        resolved.push(BlockContent::xml(node));
                    }
                }
            }
        }
        Ok(resolved)
    }

    fn resolve_block_node(
        &mut self,
        node: &ContentNode,
    ) -> Result<Option<BlockContent>, PomResolutionError> {
        Ok(match node {
            ContentNode::Markdown(MarkdownNode::Heading(node)) => {
                let children = self.resolve_inline_children(node.children())?;
                if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                    None
                } else {
                    Some(BlockContent::heading(HeadingNode::new(
                        *node.level(),
                        children,
                    )))
                }
            }
            ContentNode::Markdown(MarkdownNode::Paragraph(node)) => {
                let children = self.resolve_inline_children(node.children())?;
                if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                    None
                } else {
                    Some(BlockContent::paragraph(ParagraphNode::new(children)))
                }
            }
            ContentNode::Markdown(MarkdownNode::List(node)) => {
                self.resolve_list_node(node)?.map(BlockContent::list)
            }
            ContentNode::Markdown(MarkdownNode::CodeBlock(node)) => {
                Some(BlockContent::code_block(node.clone()))
            }
            ContentNode::Markdown(MarkdownNode::ThematicBreak) => {
                Some(BlockContent::thematic_break())
            }
            ContentNode::Xml(node) => Some(BlockContent::xml(self.resolve_ordinary_xml(node)?)),
            ContentNode::Markdown(MarkdownNode::Strong(_) | MarkdownNode::CodeSpan(_))
            | ContentNode::Text(_) => {
                unreachable!("block children only contain block-level content")
            }
        })
    }

    fn resolve_inline_children(
        &mut self,
        children: &InlineChildren,
    ) -> Result<InlineChildren, PomResolutionError> {
        let mut resolved = InlineChildren::new();
        for child in children.iter() {
            match child {
                ContentRef::Node(node) => {
                    if let Some(node) = self.resolve_inline_node(node)? {
                        resolved.push(node);
                    }
                }
                ContentRef::DiffSlot(slot) => {
                    if let Some(node) = self.resolve_slot(slot)? {
                        resolved.push(InlineContent::xml(node));
                    }
                }
            }
        }
        Ok(resolved)
    }

    fn resolve_inline_node(
        &mut self,
        node: &ContentNode,
    ) -> Result<Option<InlineContent>, PomResolutionError> {
        Ok(match node {
            ContentNode::Markdown(MarkdownNode::Strong(node)) => {
                let children = self.resolve_inline_children(node.children())?;
                if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                    None
                } else {
                    Some(InlineContent::strong(StrongNode::new(children)))
                }
            }
            ContentNode::Markdown(MarkdownNode::CodeSpan(node)) => {
                Some(InlineContent::code_span(node.clone()))
            }
            ContentNode::Xml(node) => Some(InlineContent::xml(self.resolve_ordinary_xml(node)?)),
            ContentNode::Text(node) => Some(
                InlineContent::try_from_node(node.clone().into())
                    .expect("existing inline text has already been validated"),
            ),
            ContentNode::Markdown(
                MarkdownNode::Heading(_)
                | MarkdownNode::Paragraph(_)
                | MarkdownNode::List(_)
                | MarkdownNode::CodeBlock(_)
                | MarkdownNode::ThematicBreak,
            ) => unreachable!("inline children only contain inline-level content"),
        })
    }

    fn resolve_list_node(
        &mut self,
        node: &ListNode,
    ) -> Result<Option<ListNode>, PomResolutionError> {
        let mut items = Vec::with_capacity(node.items().len());
        for item in node.items() {
            let children = self.resolve_block_children(item.children())?;
            if !emptied_by_resolution(item.children().is_empty(), children.is_empty()) {
                items.push(ListItem::new(children));
            }
        }
        if emptied_by_resolution(node.items().is_empty(), items.is_empty()) {
            Ok(None)
        } else {
            Ok(Some(ListNode::new(node.kind().clone(), items)))
        }
    }

    fn resolve_mixed_children(
        &mut self,
        children: &MixedChildren,
    ) -> Result<MixedChildren, PomResolutionError> {
        let mut resolved = MixedChildren::new();
        for child in children.iter() {
            match child {
                ContentRef::Node(node) => {
                    if let Some(node) = self.resolve_mixed_node(node)? {
                        resolved.push(MixedContent::node(node));
                    }
                }
                ContentRef::DiffSlot(slot) => {
                    if let Some(node) = self.resolve_slot(slot)? {
                        resolved.push(MixedContent::xml(node));
                    }
                }
            }
        }
        Ok(resolved)
    }

    fn resolve_mixed_node(
        &mut self,
        node: &ContentNode,
    ) -> Result<Option<ContentNode>, PomResolutionError> {
        Ok(match node {
            ContentNode::Markdown(node) => self.resolve_markdown_node(node)?.map(Into::into),
            ContentNode::Xml(node) => Some(self.resolve_ordinary_xml(node)?.into()),
            ContentNode::Text(node) => Some(node.clone().into()),
        })
    }

    fn resolve_markdown_node(
        &mut self,
        node: &MarkdownNode,
    ) -> Result<Option<MarkdownNode>, PomResolutionError> {
        Ok(match node {
            MarkdownNode::Heading(node) => {
                let children = self.resolve_inline_children(node.children())?;
                if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                    None
                } else {
                    Some(MarkdownNode::Heading(HeadingNode::new(
                        *node.level(),
                        children,
                    )))
                }
            }
            MarkdownNode::Paragraph(node) => {
                let children = self.resolve_inline_children(node.children())?;
                if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                    None
                } else {
                    Some(MarkdownNode::Paragraph(ParagraphNode::new(children)))
                }
            }
            MarkdownNode::List(node) => self.resolve_list_node(node)?.map(MarkdownNode::List),
            MarkdownNode::CodeBlock(node) => Some(MarkdownNode::CodeBlock(node.clone())),
            MarkdownNode::ThematicBreak => Some(MarkdownNode::ThematicBreak),
            MarkdownNode::Strong(node) => {
                let children = self.resolve_inline_children(node.children())?;
                if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                    None
                } else {
                    Some(MarkdownNode::Strong(StrongNode::new(children)))
                }
            }
            MarkdownNode::CodeSpan(node) => Some(MarkdownNode::CodeSpan(node.clone())),
        })
    }

    fn resolve_ordinary_xml(&mut self, node: &XmlNode) -> Result<XmlNode, PomResolutionError> {
        Ok(node
            .clone()
            .with_children(self.resolve_mixed_children(node.children())?))
    }

    fn resolve_slot(
        &mut self,
        slot: &crate::pom::DiffSlot,
    ) -> Result<Option<XmlNode>, PomResolutionError> {
        let role = slot.role().clone();
        let Some(current) = slot.value() else {
            if !self.next.remove(&role) {
                return Ok(None);
            }
            return Ok(Some(materialize_xml_patch(deletion_patch(role))?));
        };

        let output = match self.previous.baseline(&role) {
            Some((strategy, previous)) if strategy == slot.strategy() => {
                diff_xml_values(current, previous, slot.strategy())
            }
            Some(_) | None => Some(current.clone()),
        };
        self.next
            .insert(role, slot.strategy().clone(), current.clone());

        output.map(materialize_xml_patch).transpose()
    }
}

fn materialize_xml_patch(node: XmlNode) -> Result<XmlNode, PomResolutionError> {
    resolve_xml_node(&node, SlotPolicy::Materialize)
}

fn deletion_patch(role: crate::pom::XmlName) -> XmlNode {
    let mut deletion = XmlNode::new(role);
    deletion
        .push_attribute(
            crate::pom::XmlName::try_from("rendering_mode")
                .expect("internal POM vocabulary is valid"),
            "delta",
        )
        .expect("a fresh deletion node has no attributes");
    deletion.push(MixedContent::xml(XmlNode::new(
        crate::pom::XmlName::try_from("none").expect("internal POM vocabulary is valid"),
    )));
    deletion
}

fn validate_unique_outermost_roles(children: &BlockChildren) -> Result<(), PomResolutionError> {
    let mut roles = BTreeSet::new();
    scan_block_children(children, &mut roles)
}

fn record_role(
    role: &crate::pom::XmlName,
    roles: &mut BTreeSet<crate::pom::XmlName>,
) -> Result<(), PomResolutionError> {
    if roles.insert(role.clone()) {
        Ok(())
    } else {
        Err(PomResolutionError::DuplicateUserDiffSlotRole { role: role.clone() })
    }
}

fn scan_block_children(
    children: &BlockChildren,
    roles: &mut BTreeSet<crate::pom::XmlName>,
) -> Result<(), PomResolutionError> {
    for child in children.iter() {
        match child {
            ContentRef::DiffSlot(slot) => record_role(slot.role(), roles)?,
            ContentRef::Node(node) => scan_content_node(node, roles)?,
        }
    }
    Ok(())
}

fn scan_inline_children(
    children: &InlineChildren,
    roles: &mut BTreeSet<crate::pom::XmlName>,
) -> Result<(), PomResolutionError> {
    for child in children.iter() {
        match child {
            ContentRef::DiffSlot(slot) => record_role(slot.role(), roles)?,
            ContentRef::Node(node) => scan_content_node(node, roles)?,
        }
    }
    Ok(())
}

fn scan_mixed_children(
    children: &MixedChildren,
    roles: &mut BTreeSet<crate::pom::XmlName>,
) -> Result<(), PomResolutionError> {
    for child in children.iter() {
        match child {
            ContentRef::DiffSlot(slot) => record_role(slot.role(), roles)?,
            ContentRef::Node(node) => scan_content_node(node, roles)?,
        }
    }
    Ok(())
}

fn scan_content_node(
    node: &ContentNode,
    roles: &mut BTreeSet<crate::pom::XmlName>,
) -> Result<(), PomResolutionError> {
    match node {
        ContentNode::Xml(node) => scan_mixed_children(node.children(), roles),
        ContentNode::Markdown(MarkdownNode::Heading(node)) => {
            scan_inline_children(node.children(), roles)
        }
        ContentNode::Markdown(MarkdownNode::Paragraph(node)) => {
            scan_inline_children(node.children(), roles)
        }
        ContentNode::Markdown(MarkdownNode::List(node)) => {
            for item in node.items() {
                scan_block_children(item.children(), roles)?;
            }
            Ok(())
        }
        ContentNode::Markdown(MarkdownNode::Strong(node)) => {
            scan_inline_children(node.children(), roles)
        }
        ContentNode::Markdown(
            MarkdownNode::CodeBlock(_) | MarkdownNode::CodeSpan(_) | MarkdownNode::ThematicBreak,
        )
        | ContentNode::Text(_) => Ok(()),
    }
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
            ContentRef::Node(node) => {
                if let Some(node) = resolve_block_node(node, policy)? {
                    resolved.push(node);
                }
            }
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
) -> Result<Option<BlockContent>, PomResolutionError> {
    let resolved = match node {
        ContentNode::Markdown(MarkdownNode::Heading(node)) => {
            let children = resolve_inline_children(node.children(), policy)?;
            if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                None
            } else {
                Some(BlockContent::heading(HeadingNode::new(
                    *node.level(),
                    children,
                )))
            }
        }
        ContentNode::Markdown(MarkdownNode::Paragraph(node)) => {
            let children = resolve_inline_children(node.children(), policy)?;
            if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                None
            } else {
                Some(BlockContent::paragraph(ParagraphNode::new(children)))
            }
        }
        ContentNode::Markdown(MarkdownNode::List(node)) => {
            resolve_list_node(node, policy)?.map(BlockContent::list)
        }
        ContentNode::Markdown(MarkdownNode::CodeBlock(node)) => {
            Some(BlockContent::code_block(node.clone()))
        }
        ContentNode::Markdown(MarkdownNode::ThematicBreak) => Some(BlockContent::thematic_break()),
        ContentNode::Xml(node) => Some(BlockContent::xml(resolve_xml_node(node, policy)?)),
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
            ContentRef::Node(node) => {
                if let Some(node) = resolve_inline_node(node, policy)? {
                    resolved.push(node);
                }
            }
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
) -> Result<Option<InlineContent>, PomResolutionError> {
    let resolved = match node {
        ContentNode::Markdown(MarkdownNode::Strong(node)) => {
            let children = resolve_inline_children(node.children(), policy)?;
            if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                None
            } else {
                Some(InlineContent::strong(StrongNode::new(children)))
            }
        }
        ContentNode::Markdown(MarkdownNode::CodeSpan(node)) => {
            Some(InlineContent::code_span(node.clone()))
        }
        ContentNode::Xml(node) => Some(InlineContent::xml(resolve_xml_node(node, policy)?)),
        ContentNode::Text(node) => Some(
            InlineContent::try_from_node(node.clone().into())
                .expect("existing inline text has already been validated"),
        ),
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

fn resolve_list_node(
    node: &ListNode,
    policy: SlotPolicy,
) -> Result<Option<ListNode>, PomResolutionError> {
    let mut items = Vec::with_capacity(node.items().len());
    for item in node.items() {
        let children = resolve_block_children(item.children(), policy)?;
        if !emptied_by_resolution(item.children().is_empty(), children.is_empty()) {
            items.push(ListItem::new(children));
        }
    }
    if emptied_by_resolution(node.items().is_empty(), items.is_empty()) {
        Ok(None)
    } else {
        Ok(Some(ListNode::new(node.kind().clone(), items)))
    }
}

fn resolve_mixed_children(
    children: &MixedChildren,
    policy: SlotPolicy,
) -> Result<MixedChildren, PomResolutionError> {
    let mut resolved = MixedChildren::new();

    for child in children.iter() {
        match child {
            ContentRef::Node(node) => {
                if let Some(node) = resolve_mixed_node(node, policy)? {
                    resolved.push(MixedContent::node(node));
                }
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
) -> Result<Option<ContentNode>, PomResolutionError> {
    let resolved = match node {
        ContentNode::Markdown(node) => resolve_markdown_node(node, policy)?.map(Into::into),
        ContentNode::Xml(node) => Some(resolve_xml_node(node, policy)?.into()),
        ContentNode::Text(node) => Some(node.clone().into()),
    };
    Ok(resolved)
}

fn resolve_markdown_node(
    node: &MarkdownNode,
    policy: SlotPolicy,
) -> Result<Option<MarkdownNode>, PomResolutionError> {
    let resolved = match node {
        MarkdownNode::Heading(node) => {
            let children = resolve_inline_children(node.children(), policy)?;
            if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                None
            } else {
                Some(MarkdownNode::Heading(HeadingNode::new(
                    *node.level(),
                    children,
                )))
            }
        }
        MarkdownNode::Paragraph(node) => {
            let children = resolve_inline_children(node.children(), policy)?;
            if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                None
            } else {
                Some(MarkdownNode::Paragraph(ParagraphNode::new(children)))
            }
        }
        MarkdownNode::List(node) => resolve_list_node(node, policy)?.map(MarkdownNode::List),
        MarkdownNode::CodeBlock(node) => Some(MarkdownNode::CodeBlock(node.clone())),
        MarkdownNode::ThematicBreak => Some(MarkdownNode::ThematicBreak),
        MarkdownNode::Strong(node) => {
            let children = resolve_inline_children(node.children(), policy)?;
            if emptied_by_resolution(node.children().is_empty(), children.is_empty()) {
                None
            } else {
                Some(MarkdownNode::Strong(StrongNode::new(children)))
            }
        }
        MarkdownNode::CodeSpan(node) => Some(MarkdownNode::CodeSpan(node.clone())),
    };
    Ok(resolved)
}

fn resolve_xml_node(node: &XmlNode, policy: SlotPolicy) -> Result<XmlNode, PomResolutionError> {
    Ok(node
        .clone()
        .with_children(resolve_mixed_children(node.children(), policy)?))
}

fn emptied_by_resolution(original_was_empty: bool, resolved_is_empty: bool) -> bool {
    !original_was_empty && resolved_is_empty
}
