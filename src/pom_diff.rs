//! Semantic diffing for complete POM XML trees.
//!
//! This module compares AST nodes directly. It never renders prompt text.

use crate::pom::{
    BlockChildren, BlockContent, ContentNode, ContentRef, DiffSlot, DiffStrategy, MixedContent,
    ResolvedDocument, XmlName, XmlNode,
};

/// Compares two complete POM documents for an adjacent projection update.
///
/// A stable document made only of uniquely named, same-position XML roots is
/// compared root by root. Each root delegates to [`diff_xml_nodes`]. The
/// explicit-slot behavior of that existing XML path remains unchanged; in a
/// complete ordinary XML document, changes fall back to the complete current
/// root.
///
/// Other document shapes deliberately use a complete-current fallback. Before
/// that fallback, every previous top-level XML root is wrapped in a document
/// removal operation. This makes an XML root replacement or disappearance
/// explicit without inventing document-root identifiers. Text, Markdown, and
/// raw text are never converted into removal operations; a document made only
/// of those values therefore yields no patch when it disappears.
pub(crate) fn diff_resolved_documents(
    current: &ResolvedDocument,
    previous: &ResolvedDocument,
) -> Option<ResolvedDocument> {
    if current == previous {
        return None;
    }

    let current_xml_roots = only_xml_roots(current);
    let previous_xml_roots = only_xml_roots(previous);
    if let (Some(current_xml_roots), Some(previous_xml_roots)) =
        (current_xml_roots, previous_xml_roots)
    {
        if stable_xml_root_layout(&current_xml_roots, &previous_xml_roots) {
            return diff_stable_xml_roots(&current_xml_roots, &previous_xml_roots);
        }
    }

    full_document_fallback(current, xml_roots(previous))
}

/// Produces a complete current-document update for transport fallbacks.
///
/// Unlike [`diff_resolved_documents`], this retains every current child when
/// the caller cannot safely send a sparse delta. A stable all-XML root layout
/// needs no removals because each current root replaces its corresponding
/// prior root. Other shapes first remove every prior XML root, then emit the
/// complete current document. Text, Markdown, and raw text remain atomic and
/// never gain synthetic removal operations.
pub(crate) fn full_document_update(
    current: &ResolvedDocument,
    previous: &ResolvedDocument,
) -> ResolvedDocument {
    if current == previous {
        return current.clone();
    }

    if let (Some(current_xml_roots), Some(previous_xml_roots)) =
        (only_xml_roots(current), only_xml_roots(previous))
    {
        if stable_xml_root_layout(&current_xml_roots, &previous_xml_roots) {
            return current.clone();
        }
    }

    full_document_fallback(current, xml_roots(previous))
        .unwrap_or_else(|| ResolvedDocument::new(BlockChildren::new()))
}

fn diff_stable_xml_roots(current: &[&XmlNode], previous: &[&XmlNode]) -> Option<ResolvedDocument> {
    let mut children = BlockChildren::new();
    for (current, previous) in current.iter().zip(previous) {
        if let Some(patch) = diff_xml_nodes(current, previous) {
            children.push(BlockContent::xml(patch));
        }
    }
    (!children.is_empty()).then(|| ResolvedDocument::new(children))
}

fn stable_xml_root_layout(current: &[&XmlNode], previous: &[&XmlNode]) -> bool {
    current.len() == previous.len()
        && current
            .iter()
            .zip(previous)
            .all(|(current, previous)| current.name() == previous.name())
        && unique_root_names(current)
}

fn unique_root_names(roots: &[&XmlNode]) -> bool {
    roots.iter().enumerate().all(|(index, root)| {
        roots[..index]
            .iter()
            .all(|previous| previous.name() != root.name())
    })
}

fn only_xml_roots(document: &ResolvedDocument) -> Option<Vec<&XmlNode>> {
    document
        .children()
        .iter()
        .map(|child| match child {
            ContentRef::Node(ContentNode::Xml(node)) => Some(node),
            ContentRef::Node(
                ContentNode::Markdown(_) | ContentNode::Text(_) | ContentNode::RawText(_),
            )
            | ContentRef::DiffSlot(_) => None,
        })
        .collect()
}

fn xml_roots(document: &ResolvedDocument) -> Vec<&XmlNode> {
    document
        .children()
        .iter()
        .filter_map(|child| match child {
            ContentRef::Node(ContentNode::Xml(node)) => Some(node),
            ContentRef::Node(
                ContentNode::Markdown(_) | ContentNode::Text(_) | ContentNode::RawText(_),
            )
            | ContentRef::DiffSlot(_) => None,
        })
        .collect()
}

fn full_document_fallback(
    current: &ResolvedDocument,
    previous_xml_roots: Vec<&XmlNode>,
) -> Option<ResolvedDocument> {
    let mut children = BlockChildren::new();
    for previous in previous_xml_roots {
        children.push(BlockContent::xml(document_removal(previous.clone())));
    }
    children.extend(current.children().clone());
    (!children.is_empty()).then(|| ResolvedDocument::new(children))
}

fn document_removal(previous: XmlNode) -> XmlNode {
    operation_node("remove", previous)
}

pub(crate) fn diff_xml_nodes(current: &XmlNode, previous: &XmlNode) -> Option<XmlNode> {
    if current == previous {
        return None;
    }
    if !same_unmarked_shape(current, previous) {
        return Some(current.clone());
    }

    let mut delta = delta_node(current.name().clone());
    if let Some(kind) = attribute(current, "kind") {
        delta
            .push_attribute(xml_name("kind"), kind)
            .expect("a fresh delta node has no duplicate kind attribute");
    }

    for (current_slot, previous_slot) in current.diff_slots().zip(previous.diff_slots()) {
        if let Some(patch) = diff_slot(current_slot, previous_slot) {
            delta.push(MixedContent::xml(patch));
        }
    }

    (!delta.children().is_empty()).then_some(delta)
}

fn same_unmarked_shape(current: &XmlNode, previous: &XmlNode) -> bool {
    current.name() == previous.name()
        && current.attributes() == previous.attributes()
        && current.metadata() == previous.metadata()
        && current.children().len() == previous.children().len()
        && current
            .children()
            .iter()
            .zip(previous.children().iter())
            .all(|(current, previous)| match (current, previous) {
                (ContentRef::Node(current), ContentRef::Node(previous)) => current == previous,
                (ContentRef::DiffSlot(current), ContentRef::DiffSlot(previous)) => {
                    current.role() == previous.role() && current.strategy() == previous.strategy()
                }
                _ => false,
            })
}

fn diff_slot(current: &DiffSlot, previous: &DiffSlot) -> Option<XmlNode> {
    match (current.value(), previous.value()) {
        (None, None) => None,
        (Some(value), None) => Some(value.clone()),
        (None, Some(_)) => Some(deletion_node(current.role().clone())),
        (Some(current_value), Some(previous_value)) => {
            diff_xml_values(current_value, previous_value, current.strategy())
        }
    }
}

pub(crate) fn diff_xml_values(
    current: &XmlNode,
    previous: &XmlNode,
    strategy: &DiffStrategy,
) -> Option<XmlNode> {
    match strategy {
        DiffStrategy::Recursive if current.is_map() && previous.is_map() => {
            diff_map(current, previous)
        }
        DiffStrategy::Recursive => diff_xml_nodes(current, previous),
        DiffStrategy::Replace => (current != previous).then(|| replace_node(current.clone())),
        DiffStrategy::Append => diff_append(current, previous),
        DiffStrategy::Sequence => diff_sequence(current, previous),
        DiffStrategy::Set => diff_set(current, previous),
        DiffStrategy::Keyed(key) => diff_keyed(current, previous, key),
    }
}

fn diff_append(current: &XmlNode, previous: &XmlNode) -> Option<XmlNode> {
    if current == previous {
        return None;
    }
    let Some((current_items, previous_items)) = collection_parts(current, previous) else {
        return Some(current.clone());
    };
    if current_items.len() < previous_items.len()
        || current_items
            .iter()
            .zip(&previous_items)
            .any(|(current, previous)| current != previous)
    {
        return Some(current.clone());
    }

    let operations = current_items[previous_items.len()..]
        .iter()
        .cloned()
        .map(|item| operation_node("insert", item))
        .collect::<Vec<_>>();
    collection_delta(current, operations)
}

fn diff_sequence(current: &XmlNode, previous: &XmlNode) -> Option<XmlNode> {
    if current == previous {
        return None;
    }
    let Some((current_items, previous_items)) = collection_parts(current, previous) else {
        return Some(current.clone());
    };
    let common_prefix_len = current_items
        .iter()
        .zip(&previous_items)
        .take_while(|(current, previous)| current == previous)
        .count();
    if common_prefix_len < current_items.len().min(previous_items.len()) {
        return Some(current.clone());
    }

    let mut operations = Vec::new();
    operations.extend(
        current_items[common_prefix_len..]
            .iter()
            .cloned()
            .map(|item| operation_node("insert", item)),
    );
    operations.extend(
        previous_items[common_prefix_len..]
            .iter()
            .cloned()
            .map(|item| operation_node("remove", item)),
    );
    collection_delta(current, operations)
}

fn diff_set(current: &XmlNode, previous: &XmlNode) -> Option<XmlNode> {
    if current == previous {
        return None;
    }
    let Some((current_items, previous_items)) = collection_parts(current, previous) else {
        return Some(current.clone());
    };

    let mut operations = Vec::new();
    operations.extend(
        current_items
            .iter()
            .filter(|item| !previous_items.contains(item))
            .cloned()
            .map(|item| operation_node("insert", item)),
    );
    operations.extend(
        previous_items
            .iter()
            .filter(|item| !current_items.contains(item))
            .cloned()
            .map(|item| operation_node("remove", item)),
    );
    collection_delta(current, operations)
}

fn diff_keyed(current: &XmlNode, previous: &XmlNode, key: &XmlName) -> Option<XmlNode> {
    if current == previous {
        return None;
    }
    let Some((current_items, previous_items)) = collection_parts(current, previous) else {
        return Some(current.clone());
    };
    let Some(current_items) = keyed_nodes(current_items, key) else {
        return Some(current.clone());
    };
    let Some(previous_items) = keyed_nodes(previous_items, key) else {
        return Some(current.clone());
    };

    let mut operations = Vec::new();
    operations.extend(
        current_items
            .iter()
            .filter(|(key, _)| {
                !previous_items
                    .iter()
                    .any(|(previous_key, _)| previous_key == key)
            })
            .map(|(_, item)| operation_node("insert", item.clone())),
    );
    operations.extend(
        previous_items
            .iter()
            .filter(|(key, _)| {
                !current_items
                    .iter()
                    .any(|(current_key, _)| current_key == key)
            })
            .map(|(_, item)| operation_node("remove", item.clone())),
    );
    operations.extend(current_items.iter().filter_map(|(key, item)| {
        previous_items
            .iter()
            .find(|(previous_key, _)| previous_key == key)
            .filter(|(_, previous_item)| previous_item != item)
            .map(|_| operation_node("update", item.clone()))
    }));
    collection_delta(current, operations)
}

fn diff_map(current: &XmlNode, previous: &XmlNode) -> Option<XmlNode> {
    if current == previous {
        return None;
    }
    if current.name() != previous.name()
        || current.attributes() != previous.attributes()
        || !current.is_map()
        || !previous.is_map()
        || current.diff_slots().next().is_some()
        || previous.diff_slots().next().is_some()
    {
        return Some(current.clone());
    }
    let Some(current_entries) = map_entries(current) else {
        return Some(current.clone());
    };
    let Some(previous_entries) = map_entries(previous) else {
        return Some(current.clone());
    };

    let mut operations = Vec::new();
    operations.extend(
        current_entries
            .iter()
            .filter(|(identity, _)| {
                !previous_entries
                    .iter()
                    .any(|(previous_identity, _)| previous_identity == identity)
            })
            .map(|(_, entry)| operation_node("insert", entry.clone())),
    );
    operations.extend(
        previous_entries
            .iter()
            .filter(|(identity, _)| {
                !current_entries
                    .iter()
                    .any(|(current_identity, _)| current_identity == identity)
            })
            .map(|(_, entry)| operation_node("remove", entry.clone())),
    );
    operations.extend(current_entries.iter().filter_map(|(identity, entry)| {
        previous_entries
            .iter()
            .find(|(previous_identity, _)| previous_identity == identity)
            .filter(|(_, previous_entry)| previous_entry != entry)
            .map(|_| operation_node("update", entry.clone()))
    }));
    collection_delta(current, operations)
}

fn collection_parts(current: &XmlNode, previous: &XmlNode) -> Option<(Vec<XmlNode>, Vec<XmlNode>)> {
    if current.name() != previous.name()
        || current.attributes() != previous.attributes()
        || current.metadata() != previous.metadata()
    {
        return None;
    }
    Some((visible_child_nodes(current), visible_child_nodes(previous)))
}

fn visible_child_nodes(node: &XmlNode) -> Vec<XmlNode> {
    node.children()
        .iter()
        .filter_map(|child| match child {
            ContentRef::Node(node) => Some(content_as_node(node.clone())),
            ContentRef::DiffSlot(slot) => slot.value().cloned(),
        })
        .collect()
}

fn content_as_node(content: ContentNode) -> XmlNode {
    match content {
        ContentNode::Xml(node) => node,
        ContentNode::Text(text) => {
            let mut item = XmlNode::new(xml_name("item"));
            item.push(MixedContent::text(text));
            item
        }
        ContentNode::RawText(text) => {
            let mut item = XmlNode::new(xml_name("item"));
            item.push(MixedContent::raw_text(text));
            item
        }
        ContentNode::Markdown(markdown) => {
            let mut item = XmlNode::new(xml_name("item"));
            item.push(MixedContent::markdown(markdown));
            item
        }
    }
}

fn keyed_nodes(items: Vec<XmlNode>, key: &XmlName) -> Option<Vec<(String, XmlNode)>> {
    let mut keyed = Vec::with_capacity(items.len());
    for item in items {
        let value = item.attributes().get(key)?.value().to_owned();
        if keyed
            .iter()
            .any(|(existing, _): &(String, XmlNode)| existing == &value)
        {
            return None;
        }
        keyed.push((value, item));
    }
    keyed.sort_by(|(left, _), (right, _)| left.cmp(right));
    Some(keyed)
}

fn map_entries(node: &XmlNode) -> Option<Vec<(ContentNode, XmlNode)>> {
    let mut entries = Vec::new();
    for entry in visible_child_nodes(node) {
        let identity = entry.identity()?.clone();
        if entries
            .iter()
            .any(|(existing, _): &(ContentNode, XmlNode)| existing == &identity)
        {
            return None;
        }
        entries.push((identity, entry));
    }
    Some(entries)
}

fn collection_delta(current: &XmlNode, operations: Vec<XmlNode>) -> Option<XmlNode> {
    if operations.is_empty() {
        return None;
    }
    let mut delta = delta_node(current.name().clone());
    for operation in operations {
        delta.push(MixedContent::xml(operation));
    }
    Some(delta)
}

fn operation_node(name: &str, value: XmlNode) -> XmlNode {
    let mut operation = XmlNode::new(xml_name(name));
    operation.push(MixedContent::xml(value));
    operation
}

fn replace_node(current: XmlNode) -> XmlNode {
    let mut field = delta_node(current.name().clone());
    let mut replace = XmlNode::new(xml_name("replace"));
    replace.push(MixedContent::xml(current));
    field.push(MixedContent::xml(replace));
    field
}

fn deletion_node(role: XmlName) -> XmlNode {
    let mut field = delta_node(role);
    field.push(MixedContent::xml(XmlNode::new(xml_name("none"))));
    field
}

fn delta_node(name: XmlName) -> XmlNode {
    let mut node = XmlNode::new(name);
    node.push_attribute(xml_name("rendering_mode"), "delta")
        .expect("a fresh delta node has no attributes");
    node
}

fn attribute<'a>(node: &'a XmlNode, name: &str) -> Option<&'a str> {
    node.attributes()
        .get(&xml_name(name))
        .map(|attribute| attribute.value())
}

fn xml_name(value: &str) -> XmlName {
    XmlName::try_from(value).expect("internal POM vocabulary is a valid XML name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pom::{InlineChildren, InlineContent, ParagraphNode, RawTextNode, TextNode},
        pom_renderer::render_pom_document,
    };

    fn xml(name: &str, text: &str) -> XmlNode {
        let mut node = XmlNode::new(xml_name(name));
        node.push(MixedContent::text(TextNode::new(text)));
        node
    }

    fn xml_with_attribute(name: &str, attribute: (&str, &str), text: &str) -> XmlNode {
        let mut node = xml(name, text);
        node.push_attribute(xml_name(attribute.0), attribute.1)
            .expect("fresh XML node has no attributes");
        node
    }

    fn xml_document(nodes: impl IntoIterator<Item = XmlNode>) -> ResolvedDocument {
        let mut children = BlockChildren::new();
        for node in nodes {
            children.push(BlockContent::xml(node));
        }
        ResolvedDocument::new(children)
    }

    fn text_document(text: &str) -> ResolvedDocument {
        let mut inline = InlineChildren::new();
        inline.push(InlineContent::try_text(text).expect("test text is valid"));
        let mut children = BlockChildren::new();
        children.push(BlockContent::paragraph(ParagraphNode::new(inline)));
        ResolvedDocument::new(children)
    }

    fn raw_document(text: &str) -> ResolvedDocument {
        let mut children = BlockChildren::new();
        children.push(BlockContent::raw_text(RawTextNode::new(text)));
        ResolvedDocument::new(children)
    }

    fn empty_document() -> ResolvedDocument {
        ResolvedDocument::new(BlockChildren::new())
    }

    fn remove(node: XmlNode) -> XmlNode {
        let mut operation = XmlNode::new(xml_name("remove"));
        operation.push(MixedContent::xml(node));
        operation
    }

    fn rendered(document: &ResolvedDocument) -> String {
        render_pom_document(document).expect("test POM renders")
    }

    fn apply_removals(previous: &ResolvedDocument, patch: &ResolvedDocument) -> Vec<XmlNode> {
        let mut roots = xml_roots(previous).into_iter().cloned().collect::<Vec<_>>();
        for operation in xml_roots(patch) {
            assert_eq!(operation.name().as_str(), "remove");
            let mut children = operation.children().iter();
            let target = match children.next() {
                Some(ContentRef::Node(ContentNode::Xml(node))) => node,
                child => panic!("expected one XML removal target, got {child:?}"),
            };
            assert!(children.next().is_none());
            let position = roots
                .iter()
                .position(|root| root == target)
                .expect("remove target existed in the prior document");
            roots.remove(position);
        }
        roots
    }

    #[test]
    fn disappearing_xml_root_emits_an_applicable_remove_operation() {
        let previous = xml_document([xml("policy", "read only")]);
        let current = empty_document();

        let patch = diff_resolved_documents(&current, &previous).expect("XML removal patch");

        assert_eq!(patch, xml_document([remove(xml("policy", "read only"))]));
        assert_eq!(
            rendered(&patch),
            "<remove>\n  <policy>read only</policy>\n</remove>"
        );
        assert!(apply_removals(&previous, &patch).is_empty());
    }

    #[test]
    fn plain_xml_text_and_attribute_changes_replace_the_complete_root() {
        let previous = xml_document([xml_with_attribute("status", ("priority", "low"), "running")]);
        let current = xml_document([xml_with_attribute("status", ("priority", "high"), "done")]);

        let patch = diff_resolved_documents(&current, &previous).expect("changed root patch");

        assert_eq!(patch, current);
        assert_eq!(rendered(&patch), "<status priority=\"high\">done</status>");
    }

    #[test]
    fn full_document_update_retains_unchanged_stable_roots() {
        let previous = xml_document([xml("policy", "old"), xml("state", "unchanged")]);
        let current = xml_document([xml("policy", "new"), xml("state", "unchanged")]);

        assert_eq!(full_document_update(&current, &previous), current);
    }

    #[test]
    fn changed_root_name_removes_the_old_xml_before_emitting_the_new_root() {
        let previous = xml_document([xml("policy", "old")]);
        let current = xml_document([xml("context", "new")]);

        let patch = diff_resolved_documents(&current, &previous).expect("replacement patch");

        assert_eq!(
            patch,
            xml_document([remove(xml("policy", "old")), xml("context", "new")])
        );
        assert_eq!(
            rendered(&patch),
            "<remove>\n  <policy>old</policy>\n</remove>\n\n<context>new</context>"
        );
    }

    #[test]
    fn reordered_xml_roots_fall_back_to_removals_before_the_complete_current_document() {
        let previous = xml_document([xml("alpha", "A"), xml("beta", "B")]);
        let current = xml_document([xml("beta", "B"), xml("alpha", "A")]);

        let patch = diff_resolved_documents(&current, &previous).expect("reorder fallback");

        assert_eq!(
            patch,
            xml_document([
                remove(xml("alpha", "A")),
                remove(xml("beta", "B")),
                xml("beta", "B"),
                xml("alpha", "A"),
            ])
        );
    }

    #[test]
    fn duplicate_xml_root_names_use_the_same_conservative_fallback() {
        let previous = xml_document([xml("entry", "first"), xml("entry", "second")]);
        let current = xml_document([xml("entry", "changed"), xml("entry", "second")]);

        let patch = diff_resolved_documents(&current, &previous).expect("duplicate root fallback");

        assert_eq!(
            patch,
            xml_document([
                remove(xml("entry", "first")),
                remove(xml("entry", "second")),
                xml("entry", "changed"),
                xml("entry", "second"),
            ])
        );
    }

    #[test]
    fn text_and_xml_transitions_keep_text_atomic_and_xml_removals_explicit() {
        let text = text_document("plain context");
        let xml_document_value = xml_document([xml("context", "structured")]);

        assert_eq!(
            diff_resolved_documents(&xml_document_value, &text),
            Some(xml_document_value.clone()),
            "text to XML is a full-current fallback"
        );

        let patch =
            diff_resolved_documents(&text, &xml_document_value).expect("XML-to-text fallback");
        assert_eq!(patch, {
            let mut children = BlockChildren::new();
            children.push(BlockContent::xml(remove(xml("context", "structured"))));
            children.extend(text.children().clone());
            ResolvedDocument::new(children)
        });
        assert_eq!(
            rendered(&patch),
            "<remove>\n  <context>structured</context>\n</remove>\n\nplain context"
        );
    }

    #[test]
    fn standalone_text_change_is_full_current_and_text_disappearance_is_silent() {
        let previous = text_document("old note");
        let current = text_document("new note");

        assert_eq!(diff_resolved_documents(&current, &previous), Some(current));
        assert_eq!(
            diff_resolved_documents(&empty_document(), &previous),
            None,
            "text has no synthetic delete operation"
        );
    }

    #[test]
    fn raw_text_is_an_atomic_document_value_and_never_emits_xml_removals() {
        let previous = raw_document("<policy>old</policy>");
        let current = raw_document("<context>new</context>");

        let patch = diff_resolved_documents(&current, &previous)
            .expect("changed raw text produces the complete current document");
        assert_eq!(patch, current);
        assert_eq!(rendered(&patch), "<context>new</context>");
        assert!(!rendered(&patch).contains("<remove>"));

        assert_eq!(
            diff_resolved_documents(&empty_document(), &previous),
            None,
            "raw text disappearance has no synthetic XML deletion"
        );
    }
}
