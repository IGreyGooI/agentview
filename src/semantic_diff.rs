use crate::semantic_view::{
    AgentView, IntrinsicDiffStrategy, SemanticChild, SemanticDiffSlot, SemanticDiffStrategy,
    SemanticFragment, SemanticNode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SemanticPatch {
    Unchanged,
    ReplaceRoot(SemanticFragment),
    ReplaceField {
        field_name: &'static str,
        value: SemanticFragment,
    },
    RemoveField {
        field_name: &'static str,
    },
    DeltaNode(SemanticNode),
}

pub(crate) fn diff_agent_views<T: AgentView>(
    current: &T,
    previous: &T,
) -> Option<SemanticFragment> {
    match diff_fragment(&current.render_root(), &previous.render_root()) {
        SemanticPatch::Unchanged => None,
        SemanticPatch::ReplaceRoot(fragment) => Some(fragment),
        SemanticPatch::DeltaNode(node) => Some(SemanticFragment::Node(node)),
        SemanticPatch::ReplaceField { .. } | SemanticPatch::RemoveField { .. } => {
            unreachable!("field patches are consumed by their parent diff slot")
        }
    }
}

fn diff_fragment(current: &SemanticFragment, previous: &SemanticFragment) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }

    match (current, previous) {
        (SemanticFragment::Node(current), SemanticFragment::Node(previous)) => {
            diff_node(current, previous)
        }
        _ => SemanticPatch::ReplaceRoot(current.clone()),
    }
}

fn diff_node(current: &SemanticNode, previous: &SemanticNode) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }
    if !same_unmarked_shape(current, previous) {
        return SemanticPatch::ReplaceRoot(SemanticFragment::Node(current.clone()));
    }

    let mut delta = SemanticNode::new(current.tag());
    delta.push_attr("rendering_mode", "delta");
    if let Some(kind) = current.attr("kind") {
        delta.push_attr("kind", kind);
    }

    let mut changed = false;
    for (current_slot, previous_slot) in current.diff_slots().zip(previous.diff_slots()) {
        let patch = diff_slot(current_slot, previous_slot);
        changed |= push_field_patch(&mut delta, patch);
    }

    if changed {
        SemanticPatch::DeltaNode(delta)
    } else {
        SemanticPatch::Unchanged
    }
}

fn same_unmarked_shape(current: &SemanticNode, previous: &SemanticNode) -> bool {
    if current.tag() != previous.tag()
        || current.attrs() != previous.attrs()
        || current.intrinsic_diff_strategy() != previous.intrinsic_diff_strategy()
        || !current
            .ordinary_fragments()
            .eq(previous.ordinary_fragments())
    {
        return false;
    }

    let current_slots = current.diff_slots().collect::<Vec<_>>();
    let previous_slots = previous.diff_slots().collect::<Vec<_>>();
    current_slots.len() == previous_slots.len()
        && current_slots
            .iter()
            .zip(previous_slots)
            .all(|(current, previous)| {
                current.field_name == previous.field_name && current.strategy == previous.strategy
            })
}

fn diff_slot(current: &SemanticDiffSlot, previous: &SemanticDiffSlot) -> SemanticPatch {
    match (&current.value, &previous.value) {
        (None, None) => SemanticPatch::Unchanged,
        (Some(value), None) => SemanticPatch::ReplaceField {
            field_name: current.field_name,
            value: value.clone(),
        },
        (None, Some(_)) => SemanticPatch::RemoveField {
            field_name: current.field_name,
        },
        (Some(current_value), Some(previous_value)) => {
            diff_present_slot(current, current_value, previous_value)
        }
    }
}

fn diff_present_slot(
    slot: &SemanticDiffSlot,
    current: &SemanticFragment,
    previous: &SemanticFragment,
) -> SemanticPatch {
    match &slot.strategy {
        SemanticDiffStrategy::Recursive => diff_recursive_value(slot.field_name, current, previous),
        SemanticDiffStrategy::Replace => diff_replace_value(slot.field_name, current, previous),
        SemanticDiffStrategy::Append => diff_append_list(slot.field_name, current, previous),
        SemanticDiffStrategy::Sequence => diff_sequence_list(slot.field_name, current, previous),
        SemanticDiffStrategy::Set => diff_set_list(slot.field_name, current, previous),
        SemanticDiffStrategy::Keyed(key) => {
            diff_keyed_list(slot.field_name, current, previous, key)
        }
    }
}

fn diff_recursive_value(
    field_name: &'static str,
    current: &SemanticFragment,
    previous: &SemanticFragment,
) -> SemanticPatch {
    let patch = match (current, previous) {
        (SemanticFragment::Node(current_node), SemanticFragment::Node(previous_node))
            if current_node.intrinsic_diff_strategy() == Some(&IntrinsicDiffStrategy::Map)
                && previous_node.intrinsic_diff_strategy() == Some(&IntrinsicDiffStrategy::Map) =>
        {
            diff_map(current_node, previous_node)
        }
        _ => diff_fragment(current, previous),
    };

    replace_root_with_field(patch, field_name)
}

fn diff_replace_value(
    field_name: &'static str,
    current: &SemanticFragment,
    previous: &SemanticFragment,
) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }

    let mut delta = SemanticNode::new(field_name);
    delta.push_attr("rendering_mode", "delta");
    delta.push_child(operation_node("replace", fragment_as_node(current.clone())));
    SemanticPatch::DeltaNode(delta)
}

fn diff_append_list(
    field_name: &'static str,
    current: &SemanticFragment,
    previous: &SemanticFragment,
) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }

    let Some((current_node, current_items, previous_items)) = collection_parts(current, previous)
    else {
        return replace_field(field_name, current);
    };

    if current_items.len() < previous_items.len()
        || current_items
            .iter()
            .zip(&previous_items)
            .any(|(current, previous)| current != previous)
    {
        return replace_field(field_name, current);
    }

    let mut delta = collection_delta_node(current_node);
    for item in &current_items[previous_items.len()..] {
        delta.push_child(operation_node("insert", item.clone()));
    }
    delta_or_unchanged(delta)
}

fn diff_sequence_list(
    field_name: &'static str,
    current: &SemanticFragment,
    previous: &SemanticFragment,
) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }

    let Some((current_node, current_items, previous_items)) = collection_parts(current, previous)
    else {
        return replace_field(field_name, current);
    };

    let common_prefix_len = current_items
        .iter()
        .zip(&previous_items)
        .take_while(|(current, previous)| current == previous)
        .count();
    if common_prefix_len < current_items.len().min(previous_items.len()) {
        return replace_field(field_name, current);
    }

    let mut delta = collection_delta_node(current_node);
    for item in &current_items[common_prefix_len..] {
        delta.push_child(operation_node("insert", item.clone()));
    }
    for item in &previous_items[common_prefix_len..] {
        delta.push_child(operation_node("remove", item.clone()));
    }
    delta_or_unchanged(delta)
}

fn diff_set_list(
    field_name: &'static str,
    current: &SemanticFragment,
    previous: &SemanticFragment,
) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }

    let Some((current_node, current_items, previous_items)) = collection_parts(current, previous)
    else {
        return replace_field(field_name, current);
    };

    let mut delta = collection_delta_node(current_node);
    for item in &current_items {
        if !previous_items.contains(item) {
            delta.push_child(operation_node("insert", item.clone()));
        }
    }
    for item in &previous_items {
        if !current_items.contains(item) {
            delta.push_child(operation_node("remove", item.clone()));
        }
    }
    delta_or_unchanged(delta)
}

fn diff_keyed_list(
    field_name: &'static str,
    current: &SemanticFragment,
    previous: &SemanticFragment,
    key_attr: &str,
) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }

    let Some((current_node, current_items, previous_items)) = collection_parts(current, previous)
    else {
        return replace_field(field_name, current);
    };
    let Some(current_items) = keyed_nodes(current_items, key_attr) else {
        return replace_field(field_name, current);
    };
    let Some(previous_items) = keyed_nodes(previous_items, key_attr) else {
        return replace_field(field_name, current);
    };

    let mut delta = collection_delta_node(current_node);
    for (key, item) in &current_items {
        if !previous_items
            .iter()
            .any(|(previous_key, _)| previous_key == key)
        {
            delta.push_child(operation_node("insert", item.clone()));
        }
    }
    for (key, item) in &previous_items {
        if !current_items
            .iter()
            .any(|(current_key, _)| current_key == key)
        {
            delta.push_child(operation_node("remove", item.clone()));
        }
    }
    for (key, item) in &current_items {
        if let Some((_, previous_item)) = previous_items
            .iter()
            .find(|(previous_key, _)| previous_key == key)
        {
            if item != previous_item {
                delta.push_child(operation_node("update", item.clone()));
            }
        }
    }
    delta_or_unchanged(delta)
}

fn diff_map(current: &SemanticNode, previous: &SemanticNode) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }
    if current.tag() != previous.tag()
        || current.attrs() != previous.attrs()
        || current.intrinsic_diff_strategy() != previous.intrinsic_diff_strategy()
        || current.diff_slots().next().is_some()
        || previous.diff_slots().next().is_some()
    {
        return SemanticPatch::ReplaceRoot(SemanticFragment::Node(current.clone()));
    }

    let Some(current_entries) = map_entries(current) else {
        return SemanticPatch::ReplaceRoot(SemanticFragment::Node(current.clone()));
    };
    let Some(previous_entries) = map_entries(previous) else {
        return SemanticPatch::ReplaceRoot(SemanticFragment::Node(current.clone()));
    };

    let mut delta = collection_delta_node(current);
    for (identity, entry) in &current_entries {
        if !previous_entries
            .iter()
            .any(|(previous_identity, _)| previous_identity == identity)
        {
            delta.push_child(operation_node("insert", entry.clone()));
        }
    }
    for (identity, entry) in &previous_entries {
        if !current_entries
            .iter()
            .any(|(current_identity, _)| current_identity == identity)
        {
            delta.push_child(operation_node("remove", entry.clone()));
        }
    }
    for (identity, entry) in &current_entries {
        if let Some((_, previous_entry)) = previous_entries
            .iter()
            .find(|(previous_identity, _)| previous_identity == identity)
        {
            if entry != previous_entry {
                delta.push_child(operation_node("update", entry.clone()));
            }
        }
    }
    delta_or_unchanged(delta)
}

fn collection_parts<'a>(
    current: &'a SemanticFragment,
    previous: &SemanticFragment,
) -> Option<(&'a SemanticNode, Vec<SemanticNode>, Vec<SemanticNode>)> {
    let (SemanticFragment::Node(current_node), SemanticFragment::Node(previous_node)) =
        (current, previous)
    else {
        return None;
    };
    if current_node.tag() != previous_node.tag()
        || current_node.attrs() != previous_node.attrs()
        || current_node.intrinsic_diff_strategy() != previous_node.intrinsic_diff_strategy()
    {
        return None;
    }

    Some((
        current_node,
        visible_child_nodes(current_node),
        visible_child_nodes(previous_node),
    ))
}

fn visible_child_nodes(node: &SemanticNode) -> Vec<SemanticNode> {
    node.children()
        .iter()
        .filter_map(|child| match child {
            SemanticChild::Fragment(fragment) => Some(fragment_as_node(fragment.clone())),
            SemanticChild::DiffSlot(slot) => slot.value.clone().map(fragment_as_node),
        })
        .collect()
}

fn keyed_nodes(items: Vec<SemanticNode>, key_attr: &str) -> Option<Vec<(String, SemanticNode)>> {
    let mut keyed = Vec::with_capacity(items.len());
    for item in items {
        let key = item.attr(key_attr)?.to_owned();
        if keyed.iter().any(|(existing_key, _)| existing_key == &key) {
            return None;
        }
        keyed.push((key, item));
    }
    keyed.sort_by(|(current, _), (previous, _)| current.cmp(previous));
    Some(keyed)
}

fn map_entries(node: &SemanticNode) -> Option<Vec<(SemanticFragment, SemanticNode)>> {
    let mut entries = Vec::new();
    for entry in visible_child_nodes(node) {
        let identity = entry.identity()?.clone();
        if entries
            .iter()
            .any(|(existing_identity, _)| existing_identity == &identity)
        {
            return None;
        }
        entries.push((identity, entry));
    }
    Some(entries)
}

fn replace_root_with_field(patch: SemanticPatch, field_name: &'static str) -> SemanticPatch {
    match patch {
        SemanticPatch::ReplaceRoot(value) => SemanticPatch::ReplaceField { field_name, value },
        patch => patch,
    }
}

fn replace_field(field_name: &'static str, current: &SemanticFragment) -> SemanticPatch {
    SemanticPatch::ReplaceField {
        field_name,
        value: current.clone(),
    }
}

fn push_field_patch(parent: &mut SemanticNode, patch: SemanticPatch) -> bool {
    match patch {
        SemanticPatch::Unchanged => false,
        SemanticPatch::ReplaceField {
            field_name: _,
            value,
        } => {
            parent.push_fragment(value);
            true
        }
        SemanticPatch::RemoveField { field_name } => {
            parent.push_child(none_field_node(field_name));
            true
        }
        SemanticPatch::DeltaNode(node) => {
            parent.push_child(node);
            true
        }
        SemanticPatch::ReplaceRoot(_) => {
            unreachable!("root replacements are converted before embedding a field patch")
        }
    }
}

fn delta_or_unchanged(node: SemanticNode) -> SemanticPatch {
    if node.has_rendered_children() {
        SemanticPatch::DeltaNode(node)
    } else {
        SemanticPatch::Unchanged
    }
}

fn collection_delta_node(current: &SemanticNode) -> SemanticNode {
    let mut node = SemanticNode::new(current.tag());
    node.push_attr("rendering_mode", "delta");
    node
}

fn operation_node(tag: &'static str, child: SemanticNode) -> SemanticNode {
    let mut node = SemanticNode::new(tag);
    node.push_child(child);
    node
}

fn none_field_node(field_name: &'static str) -> SemanticNode {
    let mut node = SemanticNode::new(field_name);
    node.push_child(SemanticNode::new("none"));
    node
}

fn fragment_as_node(fragment: SemanticFragment) -> SemanticNode {
    match fragment {
        SemanticFragment::Node(node) => node,
        SemanticFragment::Text(text) => SemanticNode::element("item", text),
        SemanticFragment::Comment(comment) => {
            let mut node = SemanticNode::new("item");
            node.push_comment(comment);
            node
        }
    }
}
