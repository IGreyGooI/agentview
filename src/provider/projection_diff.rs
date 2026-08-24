use std::collections::{HashMap, HashSet};

use crate::{
    component::execution::{
        RenderedProjection, RenderedProjectionDiff, RenderedProjectionFragment,
        RenderedProjectionItemTemplate, RenderedProjectionNode,
    },
    pom::{BlockChildren, ContentNode, ContentRef, Document, ResolvedDocument, XmlNode},
    pom_diff::diff_xml_nodes,
    pom_resolution::resolve_system_document,
    transcript::CanonicalInputItem,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectionDiffState {
    slots: HashMap<ProjectionDiffKey, ProjectionDiffBaseline>,
    items: HashMap<ProjectionItemKey, CanonicalInputItem>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProjectionDiffBaseline {
    authored: Document,
    complete: ResolvedDocument,
}

pub(crate) struct PreparedProjectionDiff {
    pub(crate) submission: RenderedProjection,
    pub(crate) candidate: ProjectionDiffState,
    pub(crate) append_policy: ProjectionAppendPolicy,
}

#[derive(Debug, Default)]
pub(crate) struct ProjectionAppendPolicy {
    items: HashSet<(String, usize)>,
}

impl ProjectionAppendPolicy {
    pub(crate) fn requires_append(&self, node_identity: &str, item_index: usize) -> bool {
        self.items.contains(&(node_identity.to_owned(), item_index))
    }
}

impl ProjectionDiffState {
    pub(crate) fn prepare(
        previous: Option<&Self>,
        complete: &RenderedProjection,
    ) -> PreparedProjectionDiff {
        let previous = previous.cloned().unwrap_or_default();
        let mut candidate = previous.clone();
        let mut append_policy = ProjectionAppendPolicy::default();
        let nodes = complete
            .nodes()
            .iter()
            .map(|node| lower_node(node, &previous, &mut candidate, &mut append_policy))
            .collect();
        let submission = RenderedProjection::with_native_tool_names(
            nodes,
            complete.native_tool_names().to_vec(),
        )
        .expect("lowering a validated projection preserves node identities");
        let submission = match complete.execution_scope() {
            Some(scope) => submission.with_execution_scope(scope),
            None => submission,
        };
        PreparedProjectionDiff {
            submission,
            candidate,
            append_policy,
        }
    }
}

fn lower_node(
    node: &RenderedProjectionNode,
    previous: &ProjectionDiffState,
    candidate: &mut ProjectionDiffState,
    append_policy: &mut ProjectionAppendPolicy,
) -> RenderedProjectionNode {
    let templates = node
        .diff_templates()
        .iter()
        .map(|template| (template.item_index(), template))
        .collect::<HashMap<_, _>>();
    let mut items = Vec::with_capacity(node.items().len());
    for (item_index, item) in node.items().iter().enumerate() {
        match templates.get(&item_index) {
            Some(template) => {
                if let Some(item) = lower_template_item(
                    node.identity(),
                    node.diffs(),
                    template,
                    item,
                    previous,
                    candidate,
                ) {
                    append_policy
                        .items
                        .insert((node.identity().to_owned(), items.len()));
                    items.push(item);
                }
            }
            None => items.push(item.clone()),
        }
    }
    RenderedProjectionNode::new(node.identity(), items)
}

fn lower_template_item(
    node_identity: &str,
    diffs: &[RenderedProjectionDiff],
    template: &RenderedProjectionItemTemplate,
    complete_item: &CanonicalInputItem,
    previous: &ProjectionDiffState,
    candidate: &mut ProjectionDiffState,
) -> Option<CanonicalInputItem> {
    let keys = template
        .fragments()
        .iter()
        .filter_map(|fragment| match fragment {
            RenderedProjectionFragment::Complete(_) => None,
            RenderedProjectionFragment::Diff { diff_index, .. } => {
                Some(diff_key(node_identity, &diffs[*diff_index]))
            }
        })
        .collect::<Vec<_>>();
    let item_key = ProjectionItemKey {
        node_identity: node_identity.to_owned(),
        slots: keys.clone(),
    };
    let previous_item = previous.items.get(&item_key);

    let mut missing_baseline = previous_item.is_none();
    let mut emitted_diff = false;
    let mut submission_children = BlockChildren::new();
    for fragment in template.fragments() {
        match fragment {
            RenderedProjectionFragment::Complete(complete) => {
                submission_children.extend(complete.children().clone());
            }
            RenderedProjectionFragment::Diff {
                diff_index,
                authored,
                complete,
            } => {
                let key = diff_key(node_identity, &diffs[*diff_index]);
                let patch = match previous.slots.get(&key) {
                    Some(baseline) => {
                        diff_documents(authored, complete, &baseline.authored, &baseline.complete)
                    }
                    None => {
                        missing_baseline = true;
                        Some(complete.clone())
                    }
                };
                candidate.slots.insert(
                    key,
                    ProjectionDiffBaseline {
                        authored: authored.clone(),
                        complete: complete.clone(),
                    },
                );
                if let Some(patch) = patch {
                    emitted_diff = true;
                    submission_children.extend(patch.children().clone());
                }
            }
        }
    }
    candidate.items.insert(item_key, complete_item.clone());

    if missing_baseline {
        return Some(complete_item.clone());
    }
    if previous_item == Some(complete_item) {
        return None;
    }
    if !emitted_diff {
        return Some(complete_item.clone());
    }
    Some(replace_item_pom(
        complete_item,
        ResolvedDocument::new(submission_children),
    ))
}

fn diff_key(node_identity: &str, diff: &RenderedProjectionDiff) -> ProjectionDiffKey {
    ProjectionDiffKey {
        node_identity: node_identity.to_owned(),
        structural_path: diff.structural_path().to_vec(),
        slot: diff.slot().to_owned(),
    }
}

fn diff_documents(
    current_authored: &Document,
    current_complete: &ResolvedDocument,
    previous_authored: &Document,
    previous_complete: &ResolvedDocument,
) -> Option<ResolvedDocument> {
    if current_authored == previous_authored || current_complete == previous_complete {
        return None;
    }
    let (Some(current_xml), Some(previous_xml)) = (
        single_authored_xml(current_authored),
        single_authored_xml(previous_authored),
    ) else {
        return Some(current_complete.clone());
    };
    if !supports_structured_delta(current_xml) || !supports_structured_delta(previous_xml) {
        return Some(current_complete.clone());
    }
    let patch = diff_xml_nodes(current_xml, previous_xml)?;
    if patch == *current_xml {
        return Some(current_complete.clone());
    }
    Some(resolve_system_document(Document::from_xml(patch)))
}

fn supports_structured_delta(node: &XmlNode) -> bool {
    node.children().len() > 1 && node.diff_slots().next().is_some()
}

fn single_authored_xml(document: &Document) -> Option<&XmlNode> {
    let mut children = document.children().iter();
    let first = children.next()?;
    if children.next().is_some() {
        return None;
    }
    match first {
        ContentRef::Node(ContentNode::Xml(node)) => Some(node),
        ContentRef::Node(_) | ContentRef::DiffSlot(_) => None,
    }
}

fn replace_item_pom(item: &CanonicalInputItem, pom: ResolvedDocument) -> CanonicalInputItem {
    match item {
        CanonicalInputItem::Instruction { authority, .. } => {
            CanonicalInputItem::instruction(*authority, pom)
        }
        CanonicalInputItem::Message { role, .. } => CanonicalInputItem::message(*role, pom),
        CanonicalInputItem::AssistantText { .. }
        | CanonicalInputItem::ToolCall { .. }
        | CanonicalInputItem::ToolResult { .. }
        | CanonicalInputItem::ProviderExtension(_) => item.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectionDiffKey {
    node_identity: String,
    structural_path: Vec<usize>,
    slot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectionItemKey {
    node_identity: String,
    slots: Vec<ProjectionDiffKey>,
}

#[cfg(test)]
mod tests {
    use crate::{
        component::execution::{
            RenderedProjection, RenderedProjectionDiff, RenderedProjectionFragment,
            RenderedProjectionItemTemplate, RenderedProjectionNode,
        },
        pom::{DiffSlot, DiffStrategy, Document, ResolvedDocument, TextNode, XmlNode},
        pom_renderer::render_pom_document,
        pom_resolution::{resolve_artifact_document, resolve_system_document},
        transcript::{CanonicalInputItem, ConversationRole},
    };

    use super::ProjectionDiffState;

    fn document(value: &str) -> (Document, ResolvedDocument) {
        let node = XmlNode::try_build("state", |children| {
            children.text(TextNode::new(value));
            Ok(())
        })
        .unwrap();
        let authored = Document::from_xml(node);
        let complete = resolve_artifact_document(authored.clone()).unwrap();
        (authored, complete)
    }

    fn projection_at(value: &str, structural_path: Vec<usize>) -> RenderedProjection {
        let (authored, complete) = document(value);
        let item = CanonicalInputItem::message(ConversationRole::User, complete.clone());
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item],
            vec![RenderedProjectionDiff::new(0, structural_path, "state")],
            vec![RenderedProjectionItemTemplate::new(
                0,
                vec![RenderedProjectionFragment::Diff {
                    diff_index: 0,
                    authored,
                    complete,
                }],
            )],
        );
        RenderedProjection::from_nodes(vec![node]).unwrap()
    }

    fn projection(value: &str) -> RenderedProjection {
        projection_at(value, vec![0])
    }

    fn text_xml(name: &str, text: &str) -> XmlNode {
        XmlNode::try_build(name, |children| {
            children.text(TextNode::new(text));
            Ok(())
        })
        .unwrap()
    }

    fn semantic_projection(phase: &str, observations: &[&str]) -> RenderedProjection {
        semantic_projection_with_objective("stable", phase, observations)
    }

    fn semantic_projection_with_objective(
        objective: &str,
        phase: &str,
        observations: &[&str],
    ) -> RenderedProjection {
        let observations = XmlNode::try_build("observations", |children| {
            for observation in observations {
                children.xml(text_xml("item", observation));
            }
            Ok(())
        })
        .unwrap();
        let root = XmlNode::try_build("agent_state", |children| {
            children.xml(text_xml("objective", objective));
            children.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                text_xml("phase", phase),
            ));
            children.xml_slot(DiffSlot::present(DiffStrategy::Append, observations));
            Ok(())
        })
        .unwrap();
        let authored = Document::from_xml(root);
        let complete = resolve_system_document(authored.clone());
        let item = CanonicalInputItem::message(ConversationRole::User, complete.clone());
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item],
            vec![RenderedProjectionDiff::new(0, vec![0], "agent_state")],
            vec![RenderedProjectionItemTemplate::new(
                0,
                vec![RenderedProjectionFragment::Diff {
                    diff_index: 0,
                    authored,
                    complete,
                }],
            )],
        );
        RenderedProjection::from_nodes(vec![node]).unwrap()
    }

    fn single_field_projection(value: &str) -> RenderedProjection {
        single_field_projection_with_strategy(value, DiffStrategy::Recursive)
    }

    fn single_field_replace_projection(value: &str) -> RenderedProjection {
        single_field_projection_with_strategy(value, DiffStrategy::Replace)
    }

    fn single_field_projection_with_strategy(
        value: &str,
        strategy: DiffStrategy,
    ) -> RenderedProjection {
        let root = XmlNode::try_build("single_field_state", |children| {
            children.xml_slot(DiffSlot::present(strategy, text_xml("value", value)));
            Ok(())
        })
        .unwrap();
        projection_from_authored_root(root, "single_field_state")
    }

    fn explicit_replace_projection(phase: &str) -> RenderedProjection {
        let root = XmlNode::try_build("agent_state", |children| {
            children.xml(text_xml("objective", "stable"));
            children.xml_slot(DiffSlot::present(
                DiffStrategy::Replace,
                text_xml("phase", phase),
            ));
            Ok(())
        })
        .unwrap();
        projection_from_authored_root(root, "agent_state")
    }

    fn projection_from_authored_root(root: XmlNode, slot: &str) -> RenderedProjection {
        let authored = Document::from_xml(root);
        let complete = resolve_system_document(authored.clone());
        let item = CanonicalInputItem::message(ConversationRole::User, complete.clone());
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item],
            vec![RenderedProjectionDiff::new(0, vec![0], slot)],
            vec![RenderedProjectionItemTemplate::new(
                0,
                vec![RenderedProjectionFragment::Diff {
                    diff_index: 0,
                    authored,
                    complete,
                }],
            )],
        );
        RenderedProjection::from_nodes(vec![node]).unwrap()
    }

    fn rendered_item(projection: &RenderedProjection) -> Option<String> {
        let item = projection.nodes()[0].items().first()?;
        let pom = match item {
            CanonicalInputItem::Message { pom, .. } => pom,
            other => panic!("expected user POM, got {other:?}"),
        };
        Some(render_pom_document(pom).unwrap())
    }

    #[test]
    fn atomic_root_changes_use_full_values_and_same_is_omitted() {
        let first = ProjectionDiffState::prepare(None, &projection("A"));
        assert_eq!(
            rendered_item(&first.submission).as_deref(),
            Some("<state>A</state>")
        );

        let changed = ProjectionDiffState::prepare(Some(&first.candidate), &projection("A+"));
        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<state>A+</state>")
        );

        let unchanged = ProjectionDiffState::prepare(Some(&changed.candidate), &projection("A+"));
        assert!(rendered_item(&unchanged.submission).is_none());

        let later = ProjectionDiffState::prepare(Some(&unchanged.candidate), &projection("A++"));
        assert_eq!(
            rendered_item(&later.submission).as_deref(),
            Some("<state>A++</state>")
        );
    }

    #[test]
    fn single_field_structured_root_changes_use_full_values() {
        let first = ProjectionDiffState::prepare(None, &single_field_projection("A"));
        let changed =
            ProjectionDiffState::prepare(Some(&first.candidate), &single_field_projection("A+"));

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<single_field_state>\n  <value>A+</value>\n</single_field_state>")
        );
    }

    #[test]
    fn single_field_explicit_replace_still_uses_the_complete_current_root() {
        let first = ProjectionDiffState::prepare(None, &single_field_replace_projection("A"));
        let changed = ProjectionDiffState::prepare(
            Some(&first.candidate),
            &single_field_replace_projection("A+"),
        );

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<single_field_state>\n  <value>A+</value>\n</single_field_state>")
        );
    }

    #[test]
    fn authored_slot_metadata_emits_field_patch_and_append_insert() {
        let first = ProjectionDiffState::prepare(None, &semantic_projection("A", &["initialized"]));
        let changed = ProjectionDiffState::prepare(
            Some(&first.candidate),
            &semantic_projection("B", &["initialized", "phase B observed"]),
        );

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some(
                "<agent_state rendering_mode=\"delta\">\n  <phase>B</phase>\n  <observations rendering_mode=\"delta\">\n    <insert>\n      <item>phase B observed</item>\n    </insert>\n  </observations>\n</agent_state>"
            )
        );
    }

    #[test]
    fn explicit_replace_is_scoped_to_a_field_inside_a_structured_root() {
        let first = ProjectionDiffState::prepare(None, &explicit_replace_projection("A"));
        let changed =
            ProjectionDiffState::prepare(Some(&first.candidate), &explicit_replace_projection("B"));

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some(
                "<agent_state rendering_mode=\"delta\">\n  <phase rendering_mode=\"delta\">\n    <replace>\n      <phase>B</phase>\n    </replace>\n  </phase>\n</agent_state>"
            )
        );
    }

    #[test]
    fn changed_unmarked_field_falls_back_to_the_complete_structured_root() {
        let first = ProjectionDiffState::prepare(None, &semantic_projection("A", &["initialized"]));
        let changed = ProjectionDiffState::prepare(
            Some(&first.candidate),
            &semantic_projection_with_objective("changed", "A", &["initialized"]),
        );

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some(
                "<agent_state>\n  <objective>changed</objective>\n  <phase>A</phase>\n  <observations>\n    <item>initialized</item>\n  </observations>\n</agent_state>"
            )
        );
    }

    #[test]
    fn a_new_diff_address_sends_full_and_cannot_be_historical_deduplicated() {
        let first = ProjectionDiffState::prepare(None, &projection_at("A", vec![0]));

        let moved =
            ProjectionDiffState::prepare(Some(&first.candidate), &projection_at("A", vec![1]));

        assert_eq!(
            rendered_item(&moved.submission).as_deref(),
            Some("<state>A</state>")
        );
        assert!(moved.append_policy.requires_append("component", 0));
    }
}
