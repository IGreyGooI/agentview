//! Private semantic projection compiler for `#[diff]` metadata.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    component::execution::{
        RenderedProjection, RenderedProjectionDiffMarker, RenderedProjectionFragment,
        RenderedProjectionItemTemplate, RenderedProjectionNode,
    },
    pom::{BlockChildren, ContentNode, ContentRef, Document, ResolvedDocument, XmlNode},
    pom_diff::diff_xml_nodes,
    pom_resolution::resolve_system_document,
    transcript::{AssistantPhase, CanonicalInputItem, InstructionAuthority},
};

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProjectionDiffState {
    slots: HashMap<ProjectionDiffKey, ProjectionDiffBaseline>,
    items: HashMap<ProjectionItemKey, CanonicalInputItem>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProjectionDiffBaseline {
    authored: Document,
    complete: ResolvedDocument,
}

/// Cumulative, provider-neutral ownership ledger for Component projection
/// items. Nodes survive temporary omission so remount-stable identities retain
/// their occurrence counts across complete projection checkpoints.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProjectionReconciliationState {
    nodes: Vec<ProjectionLedgerNode>,
    /// Provider-derived or staged items tracked in the current execution
    /// scope. They become ambiguous after crossing an execution-scope fence.
    provider_outputs: Vec<CanonicalInputItem>,
    unclaimed_provider_outputs: Vec<CanonicalInputItem>,
    ambiguous_provider_outputs: Vec<CanonicalInputItem>,
}

#[derive(Debug, Clone, PartialEq)]
struct ProjectionLedgerNode {
    identity: String,
    items: Vec<CanonicalInputItem>,
}

pub(crate) struct PreparedProjectionDiff {
    pub(crate) submission: RenderedProjection,
    pub(crate) candidate: ProjectionDiffState,
    pub(crate) append_policy: ProjectionAppendPolicy,
}

/// One generation-exact projection submission paired with the complete
/// baseline that becomes authoritative only after Frame handoff.
pub(crate) struct ReconciledProjectionPlan {
    pub(crate) submission: RenderedProjection,
    pub(crate) diff_baseline: ProjectionDiffState,
    pub(crate) reconciliation: ProjectionReconciliationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProjectionReconciliationFault {
    #[error("projection item provenance is ambiguous across execution scopes")]
    AmbiguousProjectionProvenance,
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

impl ReconciledProjectionPlan {
    /// Compile a self-contained Full (`previous_diff == None`) or an
    /// append-compatible Delta. Reconciliation state survives a Full reset,
    /// while the semantic diff baseline deliberately does not. Ordinary items
    /// claim per-node history and provider output occurrences; emitted
    /// `#[diff]` items remain exact ordered submissions.
    pub(crate) fn prepare(
        previous_diff: Option<&ProjectionDiffState>,
        previous_reconciliation: Option<&ProjectionReconciliationState>,
        newly_unclaimed_outputs: &[CanonicalInputItem],
        complete: &RenderedProjection,
    ) -> Result<Self, ProjectionReconciliationFault> {
        let prepared = ProjectionDiffState::prepare(previous_diff, complete);
        let mut previous_reconciliation = previous_reconciliation.cloned().unwrap_or_default();
        previous_reconciliation
            .provider_outputs
            .extend_from_slice(newly_unclaimed_outputs);
        previous_reconciliation
            .unclaimed_provider_outputs
            .extend_from_slice(newly_unclaimed_outputs);
        let (submission, reconciliation) = reconcile_projection_submission(
            &previous_reconciliation,
            prepared.submission,
            &prepared.append_policy,
        )?;
        Ok(Self {
            submission,
            diff_baseline: prepared.candidate,
            reconciliation,
        })
    }
}

fn reconcile_projection_submission(
    previous: &ProjectionReconciliationState,
    lowered: RenderedProjection,
    append_policy: &ProjectionAppendPolicy,
) -> Result<(RenderedProjection, ProjectionReconciliationState), ProjectionReconciliationFault> {
    let mut candidate = previous.clone();
    for node in lowered.nodes() {
        candidate.ensure_node(node.identity());
    }

    let node_indexes = candidate
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.identity.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut submitted_indexes = previous
        .nodes
        .iter()
        .map(|node| {
            (
                node.identity.as_str(),
                ItemOccurrenceIndex::new(&node.items),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut provider_outputs = ItemOccurrenceIndex::new(&previous.unclaimed_provider_outputs);
    let mut claimed_provider_outputs = vec![false; previous.unclaimed_provider_outputs.len()];
    let ambiguous_provider_outputs = ItemOccurrenceIndex::new(&previous.ambiguous_provider_outputs);

    let mut nodes = Vec::with_capacity(lowered.nodes().len());
    for node in lowered.nodes() {
        let ledger_node_index = *node_indexes
            .get(node.identity())
            .expect("every current projection node has a ledger entry");
        let mut items = Vec::with_capacity(node.items().len());
        for (item_index, item) in node.items().iter().enumerate() {
            if is_system_instruction(item) {
                continue;
            }
            if append_policy.requires_append(node.identity(), item_index) {
                candidate.nodes[ledger_node_index].items.push(item.clone());
                items.push(item.clone());
                continue;
            }
            if submitted_indexes
                .get_mut(node.identity())
                .and_then(|index| index.claim(item))
                .is_some()
            {
                continue;
            }
            if let Some(index) = provider_outputs.claim(item) {
                claimed_provider_outputs[index] = true;
                candidate.nodes[ledger_node_index].items.push(item.clone());
                continue;
            }
            if ambiguous_provider_outputs.contains(item) {
                return Err(ProjectionReconciliationFault::AmbiguousProjectionProvenance);
            }
            candidate.nodes[ledger_node_index].items.push(item.clone());
            items.push(item.clone());
        }
        nodes.push(RenderedProjectionNode::new(node.identity(), items));
    }
    candidate.unclaimed_provider_outputs = previous
        .unclaimed_provider_outputs
        .iter()
        .cloned()
        .zip(claimed_provider_outputs)
        .filter_map(|(output, claimed)| (!claimed).then_some(output))
        .collect();
    let submission =
        RenderedProjection::with_native_tool_names(nodes, lowered.native_tool_names().to_vec())
            .expect("reconciling a validated projection preserves its identities");
    let submission = match lowered.execution_scope() {
        Some(scope) => submission.with_execution_scope(scope),
        None => submission,
    };
    Ok((submission, candidate))
}

impl ProjectionReconciliationState {
    /// Reset authored occurrences for a new execution scope. Provider outputs
    /// from earlier scopes remain fenced because value-only projections cannot
    /// prove whether a matching new occurrence is authored or provider-owned.
    pub(crate) fn reset_authored_for_scope(
        &self,
        newly_ambiguous_outputs: &[CanonicalInputItem],
    ) -> Self {
        let mut ambiguous_provider_outputs = self.ambiguous_provider_outputs.clone();
        ambiguous_provider_outputs.extend(self.provider_outputs.iter().cloned());
        ambiguous_provider_outputs.extend_from_slice(newly_ambiguous_outputs);
        Self {
            nodes: Vec::new(),
            provider_outputs: Vec::new(),
            unclaimed_provider_outputs: Vec::new(),
            ambiguous_provider_outputs,
        }
    }

    fn ensure_node(&mut self, identity: &str) {
        if self.nodes.iter().any(|node| node.identity == identity) {
            return;
        }
        self.nodes.push(ProjectionLedgerNode {
            identity: identity.to_owned(),
            items: Vec::new(),
        });
    }
}

struct ItemOccurrenceIndex {
    occurrences: HashMap<Vec<u8>, VecDeque<usize>>,
}

impl ItemOccurrenceIndex {
    fn new(items: &[CanonicalInputItem]) -> Self {
        let mut occurrences = HashMap::<_, VecDeque<_>>::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            occurrences
                .entry(submission_item_key(item))
                .or_default()
                .push_back(index);
        }
        Self { occurrences }
    }

    fn claim(&mut self, item: &CanonicalInputItem) -> Option<usize> {
        self.occurrences
            .get_mut(&submission_item_key(item))
            .and_then(VecDeque::pop_front)
    }

    fn contains(&self, item: &CanonicalInputItem) -> bool {
        self.occurrences
            .get(&submission_item_key(item))
            .is_some_and(|occurrences| !occurrences.is_empty())
    }
}

fn submission_item_key(item: &CanonicalInputItem) -> Vec<u8> {
    match item {
        CanonicalInputItem::AssistantText {
            text,
            phase,
            status,
        } => serde_json::to_vec(&(0_u8, text, normalized_final_phase(*phase), status)),
        _ => serde_json::to_vec(&(1_u8, item)),
    }
    .expect("canonical input items always have a JSON representation")
}

fn normalized_final_phase(phase: Option<AssistantPhase>) -> Option<AssistantPhase> {
    match phase {
        None | Some(AssistantPhase::FinalAnswer) => Some(AssistantPhase::FinalAnswer),
        commentary => commentary,
    }
}

fn is_system_instruction(item: &CanonicalInputItem) -> bool {
    matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    )
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
    diffs: &[RenderedProjectionDiffMarker],
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

fn diff_key(node_identity: &str, diff: &RenderedProjectionDiffMarker) -> ProjectionDiffKey {
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
            RenderedProjection, RenderedProjectionDiffMarker, RenderedProjectionFragment,
            RenderedProjectionItemTemplate, RenderedProjectionNode,
        },
        pom::{DiffSlot, DiffStrategy, Document, ResolvedDocument, TextNode, XmlNode},
        pom_renderer::render_pom_document,
        pom_resolution::{resolve_artifact_document, resolve_system_document},
        transcript::{AssistantPhase, CanonicalInputItem, ConversationRole},
    };

    use super::{ProjectionDiffState, ProjectionReconciliationFault, ReconciledProjectionPlan};

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
            vec![RenderedProjectionDiffMarker::new(
                0,
                structural_path,
                "state",
            )],
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
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "agent_state")],
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
            vec![RenderedProjectionDiffMarker::new(0, vec![0], slot)],
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

    fn text_projection(nodes: &[(&str, &[&str])]) -> RenderedProjection {
        RenderedProjection::from_nodes(
            nodes
                .iter()
                .map(|(identity, items)| {
                    RenderedProjectionNode::new(
                        *identity,
                        items
                            .iter()
                            .map(|text| CanonicalInputItem::assistant_text(*text, None))
                            .collect(),
                    )
                })
                .collect(),
        )
        .unwrap()
    }

    fn text_projection_items(projection: &RenderedProjection) -> Vec<&str> {
        projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .map(|item| match item {
                CanonicalInputItem::AssistantText { text, .. } => text.as_str(),
                other => panic!("expected assistant text, got {other:?}"),
            })
            .collect()
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

    #[test]
    fn reconciliation_counts_occurrences_per_node_in_current_render_order() {
        let first_complete = text_projection(&[("left", &["A", "A"]), ("right", &["O"])]);
        let first = ReconciledProjectionPlan::prepare(None, None, &[], &first_complete).unwrap();
        assert_eq!(text_projection_items(&first.submission), ["A", "A", "O"]);

        let second_complete =
            text_projection(&[("right", &["O", "P"]), ("left", &["A", "A", "A"])]);
        let second = ReconciledProjectionPlan::prepare(
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &second_complete,
        )
        .unwrap();
        assert_eq!(text_projection_items(&second.submission), ["P", "A"]);

        let omitted_complete = text_projection(&[("right", &["O", "P"])]);
        let omitted = ReconciledProjectionPlan::prepare(
            Some(&second.diff_baseline),
            Some(&second.reconciliation),
            &[],
            &omitted_complete,
        )
        .unwrap();
        assert!(text_projection_items(&omitted.submission).is_empty());

        let reappeared_complete = text_projection(&[("left", &["A", "A", "A", "B"])]);
        let reappeared = ReconciledProjectionPlan::prepare(
            Some(&omitted.diff_baseline),
            Some(&omitted.reconciliation),
            &[],
            &reappeared_complete,
        )
        .unwrap();
        assert_eq!(text_projection_items(&reappeared.submission), ["B"]);

        let replacement_complete = text_projection(&[("replacement", &["A"])]);
        let replacement = ReconciledProjectionPlan::prepare(
            Some(&reappeared.diff_baseline),
            Some(&reappeared.reconciliation),
            &[],
            &replacement_complete,
        )
        .unwrap();
        assert_eq!(text_projection_items(&replacement.submission), ["A"]);
    }

    #[test]
    fn provider_outputs_are_claimed_by_occurrence_after_authored_items() {
        let first_complete = text_projection(&[("root", &["authored"])]);
        let first = ReconciledProjectionPlan::prepare(
            None,
            None,
            &[
                CanonicalInputItem::assistant_text("provider", None),
                CanonicalInputItem::assistant_text("provider", None),
            ],
            &first_complete,
        )
        .unwrap();

        let first_claim_complete =
            RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
                "root",
                vec![
                    CanonicalInputItem::assistant_text("authored", None),
                    CanonicalInputItem::assistant_text(
                        "provider",
                        Some(AssistantPhase::FinalAnswer),
                    ),
                ],
            )])
            .unwrap();
        let first_claim = ReconciledProjectionPlan::prepare(
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &first_claim_complete,
        )
        .unwrap();
        assert!(text_projection_items(&first_claim.submission).is_empty());
        assert_eq!(
            first_claim.reconciliation.unclaimed_provider_outputs.len(),
            1
        );

        let second_claim_complete =
            text_projection(&[("root", &["authored", "provider", "provider"])]);
        let second_claim = ReconciledProjectionPlan::prepare(
            Some(&first_claim.diff_baseline),
            Some(&first_claim.reconciliation),
            &[],
            &second_claim_complete,
        )
        .unwrap();
        assert!(text_projection_items(&second_claim.submission).is_empty());
        assert!(
            second_claim
                .reconciliation
                .unclaimed_provider_outputs
                .is_empty()
        );
    }

    #[test]
    fn full_reset_reuses_the_ledger_but_resets_the_diff_baseline() {
        let first_complete = text_projection(&[("root", &["authored"])]);
        let first = ReconciledProjectionPlan::prepare(
            None,
            None,
            &[CanonicalInputItem::assistant_text("provider", None)],
            &first_complete,
        )
        .unwrap();

        let current = text_projection(&[("root", &["authored", "provider", "new"])]);
        let full =
            ReconciledProjectionPlan::prepare(None, Some(&first.reconciliation), &[], &current)
                .unwrap();

        assert_eq!(text_projection_items(&full.submission), ["new"]);
        assert!(full.reconciliation.unclaimed_provider_outputs.is_empty());

        let first_diff_complete = semantic_projection("A", &["initialized"]);
        let first_diff =
            ReconciledProjectionPlan::prepare(None, None, &[], &first_diff_complete).unwrap();
        let reset = ReconciledProjectionPlan::prepare(
            None,
            Some(&first_diff.reconciliation),
            &[],
            &first_diff_complete,
        )
        .unwrap();
        assert_eq!(
            rendered_item(&reset.submission).as_deref(),
            Some(
                "<agent_state>\n  <objective>stable</objective>\n  <phase>A</phase>\n  <observations>\n    <item>initialized</item>\n  </observations>\n</agent_state>"
            )
        );
    }

    #[test]
    fn staged_output_is_claimable_in_the_same_frame() {
        let output = CanonicalInputItem::assistant_text("staged", None);
        let complete = text_projection(&[("root", &["staged"])]);

        let plan = ReconciledProjectionPlan::prepare(None, None, &[output], &complete).unwrap();

        assert!(text_projection_items(&plan.submission).is_empty());
        assert!(plan.reconciliation.unclaimed_provider_outputs.is_empty());
    }

    #[test]
    fn emitted_diff_patch_is_not_suppressed_by_an_equal_provider_output() {
        let first_complete = semantic_projection("A", &["initialized"]);
        let first = ReconciledProjectionPlan::prepare(None, None, &[], &first_complete).unwrap();
        let changed_complete = semantic_projection("B", &["initialized"]);
        let preview = ProjectionDiffState::prepare(Some(&first.diff_baseline), &changed_complete);
        let patch = preview.submission.nodes()[0].items()[0].clone();

        let changed = ReconciledProjectionPlan::prepare(
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[patch],
            &changed_complete,
        )
        .unwrap();

        assert!(rendered_item(&changed.submission).is_some());
        assert_eq!(changed.reconciliation.unclaimed_provider_outputs.len(), 1);
    }

    #[test]
    fn current_scope_provider_occurrences_precede_ambiguous_cross_scope_occurrences() {
        let collision = CanonicalInputItem::assistant_text("collision", None);
        let empty = text_projection(&[("root", &[])]);
        let previous = ReconciledProjectionPlan::prepare(
            None,
            None,
            &[collision.clone(), collision.clone()],
            &empty,
        )
        .unwrap();
        let reset = previous.reconciliation.reset_authored_for_scope(&[]);

        let once = text_projection(&[("root", &["collision"])]);
        let claimed_current_scope =
            ReconciledProjectionPlan::prepare(None, Some(&reset), &[collision], &once).unwrap();
        assert!(text_projection_items(&claimed_current_scope.submission).is_empty());
        assert_eq!(
            claimed_current_scope
                .reconciliation
                .ambiguous_provider_outputs
                .len(),
            2
        );

        let twice = text_projection(&[("root", &["collision", "collision"])]);
        assert!(matches!(
            ReconciledProjectionPlan::prepare(
                Some(&claimed_current_scope.diff_baseline),
                Some(&claimed_current_scope.reconciliation),
                &[],
                &twice,
            ),
            Err(ProjectionReconciliationFault::AmbiguousProjectionProvenance)
        ));
    }

    #[test]
    fn cross_scope_ambiguity_survives_successful_later_frames() {
        let collision = CanonicalInputItem::assistant_text("collision", None);
        let empty = text_projection(&[("root", &[])]);
        let previous = ReconciledProjectionPlan::prepare(None, None, &[collision], &empty).unwrap();
        let reset = previous.reconciliation.reset_authored_for_scope(&[]);

        let unrelated = text_projection(&[("root", &["new-authored"])]);
        let successful =
            ReconciledProjectionPlan::prepare(None, Some(&reset), &[], &unrelated).unwrap();
        assert_eq!(
            text_projection_items(&successful.submission),
            ["new-authored"]
        );

        let later_collision = text_projection(&[("root", &["new-authored", "collision"])]);
        assert!(matches!(
            ReconciledProjectionPlan::prepare(
                Some(&successful.diff_baseline),
                Some(&successful.reconciliation),
                &[],
                &later_collision,
            ),
            Err(ProjectionReconciliationFault::AmbiguousProjectionProvenance)
        ));
    }

    #[test]
    fn emitted_diff_item_bypasses_cross_scope_ambiguity() {
        let diff_complete = semantic_projection("B", &["initialized"]);
        let diff_item = ProjectionDiffState::prepare(None, &diff_complete)
            .submission
            .nodes()[0]
            .items()[0]
            .clone();
        let empty = text_projection(&[("root", &[])]);
        let previous = ReconciledProjectionPlan::prepare(None, None, &[diff_item], &empty).unwrap();
        let reset = previous.reconciliation.reset_authored_for_scope(&[]);

        let plan =
            ReconciledProjectionPlan::prepare(None, Some(&reset), &[], &diff_complete).unwrap();

        assert!(rendered_item(&plan.submission).is_some());
        assert_eq!(plan.reconciliation.ambiguous_provider_outputs.len(), 1);
    }
}
