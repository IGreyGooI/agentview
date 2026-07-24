//! Semantic diffing for complete POM XML trees.
//!
//! This module compares AST nodes directly. It never renders prompt text.

use crate::pom::{ContentNode, ContentRef, DiffSlot, DiffStrategy, MixedContent, XmlName, XmlNode};

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
