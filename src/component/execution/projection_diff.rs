//! Adjacent projection reconciliation and semantic XML lowering.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    component::execution::{
        RenderedProjection, RenderedProjectionDiffMarker, RenderedProjectionFragment,
        RenderedProjectionItemTemplate, RenderedProjectionNode,
    },
    pom::{BlockChildren, ContentNode, ContentRef, Document, ResolvedDocument, XmlNode},
    pom_diff::{diff_resolved_documents, diff_xml_nodes, full_document_update},
    pom_resolution::resolve_system_document,
    transcript::{AssistantPhase, CanonicalInputItem, ConversationRole, InstructionAuthority},
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

/// Provider occurrences and submitted tool records already represented by a
/// Component. Other authored state is compared with the complete checkpoint.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProjectionReconciliationState {
    provider_claims: Vec<ProjectionLedgerNode>,
    /// Provider-derived, staged, or snapshot tool items tracked in the current
    /// execution scope. They become ambiguous after crossing a scope fence.
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
    source_indexes: HashMap<(String, usize), usize>,
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
        let mut candidate = Self::default();
        let mut append_policy = ProjectionAppendPolicy::default();
        let mut source_indexes = HashMap::new();
        let nodes = complete
            .nodes()
            .iter()
            .map(|node| {
                lower_node(
                    node,
                    &previous,
                    &mut candidate,
                    &mut append_policy,
                    &mut source_indexes,
                )
            })
            .collect();
        let submission =
            RenderedProjection::with_native_tools(nodes, complete.native_tools().to_vec())
                .expect("lowering a validated projection preserves node identities");
        let submission = match complete.execution_scope() {
            Some(scope) => submission.with_execution_scope(scope),
            None => submission,
        };
        PreparedProjectionDiff {
            submission,
            candidate,
            append_policy,
            source_indexes,
        }
    }
}

impl ReconciledProjectionPlan {
    /// A transport Full resets semantic patch eligibility, but may still have
    /// a compatible complete snapshot represented in its canonical replay.
    pub(crate) fn prepare(
        previous_complete: Option<&RenderedProjection>,
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
            previous_complete,
            complete,
            &prepared,
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
    previous_complete: Option<&RenderedProjection>,
    complete: &RenderedProjection,
    prepared: &PreparedProjectionDiff,
) -> Result<(RenderedProjection, ProjectionReconciliationState), ProjectionReconciliationFault> {
    let mut candidate = previous.clone();
    for node in complete.nodes() {
        candidate.ensure_node(node.identity());
    }

    let node_indexes = candidate
        .provider_claims
        .iter()
        .enumerate()
        .map(|(index, node)| (node.identity.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut claimed_indexes = previous
        .provider_claims
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
    let previous_nodes = previous_complete
        .map(RenderedProjection::nodes)
        .unwrap_or_default();
    let current_identities = complete
        .nodes()
        .iter()
        .map(|node| node.identity())
        .collect::<HashSet<_>>();
    let node_prefix = previous_nodes
        .iter()
        .zip(complete.nodes())
        .take_while(|(previous, current)| previous.identity() == current.identity())
        .count();
    let rebuild_nodes = node_prefix < previous_nodes.len() && node_prefix < complete.nodes().len();

    let mut nodes = Vec::with_capacity(complete.nodes().len() + previous_nodes.len());
    for old_node in previous_nodes {
        if !current_identities.contains(old_node.identity()) {
            let owned = previous
                .provider_claims
                .iter()
                .find(|claim| claim.identity == old_node.identity());
            let mut claimed = ItemOccurrenceIndex::new(
                owned
                    .map(|claim| claim.items.as_slice())
                    .unwrap_or_default(),
            );
            let items = old_node
                .items()
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    if !has_diff_template(old_node, index)
                        && !old_node.repeats_item(index)
                        && claimed.claim(item).is_some()
                    {
                        None
                    } else {
                        remove_projection_item(item)
                    }
                })
                .collect();
            nodes.push(RenderedProjectionNode::new(old_node.identity(), items));
        }
    }

    for (node_offset, node) in complete.nodes().iter().enumerate() {
        let refresh_node = rebuild_nodes && node_offset >= node_prefix;
        let ledger_node_index = *node_indexes
            .get(node.identity())
            .expect("every current projection node has a ledger entry");
        let old_node = previous_nodes
            .iter()
            .find(|old| old.identity() == node.identity());
        let old_items = old_node
            .map(RenderedProjectionNode::items)
            .unwrap_or_default()
            .iter()
            .filter(|item| !is_system_instruction(item))
            .collect::<Vec<_>>();
        let current_items = node
            .items()
            .iter()
            .enumerate()
            .filter(|(_, item)| !is_system_instruction(item))
            .collect::<Vec<_>>();
        let prefix = old_items
            .iter()
            .zip(&current_items)
            .take_while(|(old, (_, current))| {
                submission_item_key(old) == submission_item_key(current)
            })
            .count();
        let paired_change = old_items.len() == current_items.len()
            && prefix < old_items.len()
            && same_item_role(old_items[prefix], current_items[prefix].1)
            && old_items[prefix + 1..]
                .iter()
                .zip(&current_items[prefix + 1..])
                .all(|(old, (_, current))| {
                    submission_item_key(old) == submission_item_key(current)
                });
        let rebuild_tail =
            prefix < old_items.len() && prefix < current_items.len() && !paired_change;
        let old_claims = previous
            .provider_claims
            .iter()
            .find(|claim| claim.identity == node.identity());
        let mut old_claimed = ItemOccurrenceIndex::new(
            old_claims
                .map(|claim| claim.items.as_slice())
                .unwrap_or_default(),
        );
        let old_owned = old_node
            .map(RenderedProjectionNode::items)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter(|(_, item)| !is_system_instruction(item))
            .map(|(index, item)| {
                !has_diff_template(old_node.expect("an old item has an old node"), index)
                    && !old_node
                        .expect("an old item has an old node")
                        .repeats_item(index)
                    && old_claimed.claim(item).is_some()
            })
            .collect::<Vec<_>>();
        let mut items = Vec::new();

        // Remove a changed suffix before inserting its replacement so moving
        // equal XML roots cannot remove a root just inserted by this Frame.
        if refresh_node || !paired_change {
            let removal_start = if refresh_node { 0 } else { prefix };
            for (offset, old) in old_items.iter().enumerate().skip(removal_start) {
                if !old_owned[offset] {
                    if let Some(removal) = remove_projection_item(old) {
                        items.push(removal);
                    }
                }
            }
        }

        for (offset, (source_index, item)) in current_items.iter().enumerate() {
            let template = has_diff_template(node, *source_index);
            let repeat = node.repeats_item(*source_index);
            let old = (offset < prefix || paired_change)
                .then(|| old_items.get(offset).copied())
                .flatten();
            let unchanged =
                old.is_some_and(|old| submission_item_key(old) == submission_item_key(item));
            if !template && !repeat {
                if claimed_indexes
                    .get_mut(node.identity())
                    .and_then(|index| index.claim(item))
                    .is_some()
                {
                    continue;
                }
                if !unchanged {
                    if let Some(index) = provider_outputs.claim(item) {
                        claimed_provider_outputs[index] = true;
                        candidate.provider_claims[ledger_node_index]
                            .items
                            .push((*item).clone());
                        continue;
                    }
                    if ambiguous_provider_outputs.contains(item) {
                        return Err(ProjectionReconciliationFault::AmbiguousProjectionProvenance);
                    }
                }
            }

            let refresh = refresh_node || (rebuild_tail && offset >= prefix);
            let output = if repeat {
                Some(match old {
                    Some(old) if !refresh && !old_owned.get(offset).copied().unwrap_or(false) => {
                        full_projection_item(item, old)
                    }
                    _ => (*item).clone(),
                })
            } else if template {
                if refresh || old.is_none() {
                    Some((*item).clone())
                } else {
                    prepared
                        .source_indexes
                        .get(&(node.identity().to_owned(), *source_index))
                        .filter(|index| {
                            prepared
                                .append_policy
                                .requires_append(node.identity(), **index)
                        })
                        .and_then(|index| {
                            prepared
                                .submission
                                .nodes()
                                .iter()
                                .find(|lowered| lowered.identity() == node.identity())
                                .and_then(|lowered| lowered.items().get(*index))
                        })
                        .cloned()
                        .map(|output| {
                            if output == **item && !old_owned.get(offset).copied().unwrap_or(false)
                            {
                                full_projection_item(item, old.expect("a prior item exists here"))
                            } else {
                                output
                            }
                        })
                }
            } else if refresh {
                Some((*item).clone())
            } else if unchanged {
                None
            } else if let Some(old) = old {
                if old_owned.get(offset).copied().unwrap_or(false) {
                    Some((*item).clone())
                } else {
                    diff_projection_item(item, old)
                }
            } else {
                Some((*item).clone())
            };
            if let Some(output) = output {
                if matches!(
                    output,
                    CanonicalInputItem::ToolCall { .. } | CanonicalInputItem::ToolResult { .. }
                ) {
                    // A reset submits retained tool records from the snapshot.
                    // Keep their ownership when that snapshot's window rolls.
                    candidate.provider_claims[ledger_node_index]
                        .items
                        .push(output.clone());
                    candidate.provider_outputs.push(output.clone());
                }
                items.push(output);
            }
        }
        nodes.push(RenderedProjectionNode::new(node.identity(), items));
    }
    candidate
        .provider_claims
        .retain(|node| !node.items.is_empty());
    candidate.unclaimed_provider_outputs = previous
        .unclaimed_provider_outputs
        .iter()
        .cloned()
        .zip(claimed_provider_outputs)
        .filter_map(|(output, claimed)| (!claimed).then_some(output))
        .collect();
    let submission = RenderedProjection::with_native_tools(nodes, complete.native_tools().to_vec())
        .expect("reconciling a validated projection preserves its identities");
    let submission = match complete.execution_scope() {
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
            provider_claims: Vec::new(),
            provider_outputs: Vec::new(),
            unclaimed_provider_outputs: Vec::new(),
            ambiguous_provider_outputs,
        }
    }

    fn ensure_node(&mut self, identity: &str) {
        if self
            .provider_claims
            .iter()
            .any(|node| node.identity == identity)
        {
            return;
        }
        self.provider_claims.push(ProjectionLedgerNode {
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

fn has_diff_template(node: &RenderedProjectionNode, item_index: usize) -> bool {
    node.diff_templates()
        .iter()
        .any(|template| template.item_index() == item_index)
}

fn same_item_role(left: &CanonicalInputItem, right: &CanonicalInputItem) -> bool {
    match (left, right) {
        (
            CanonicalInputItem::Instruction {
                authority: left, ..
            },
            CanonicalInputItem::Instruction {
                authority: right, ..
            },
        ) => left == right,
        (
            CanonicalInputItem::Message { role: left, .. },
            CanonicalInputItem::Message { role: right, .. },
        ) => left == right,
        (CanonicalInputItem::AssistantText { .. }, CanonicalInputItem::AssistantText { .. }) => {
            true
        }
        _ => false,
    }
}

fn item_pom(item: &CanonicalInputItem) -> Option<&ResolvedDocument> {
    match item {
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            pom,
        }
        | CanonicalInputItem::Message {
            role: ConversationRole::User,
            pom,
        } => Some(pom),
        _ => None,
    }
}

fn remove_projection_item(item: &CanonicalInputItem) -> Option<CanonicalInputItem> {
    let previous = item_pom(item)?;
    let empty = ResolvedDocument::new(BlockChildren::new());
    diff_resolved_documents(&empty, previous).map(|patch| replace_item_pom(item, patch))
}

fn diff_projection_item(
    current: &CanonicalInputItem,
    previous: &CanonicalInputItem,
) -> Option<CanonicalInputItem> {
    match (item_pom(current), item_pom(previous)) {
        (Some(current_pom), Some(previous_pom)) if same_item_role(current, previous) => {
            diff_resolved_documents(current_pom, previous_pom)
                .map(|patch| replace_item_pom(current, patch))
        }
        _ => Some(current.clone()),
    }
}

/// Keeps the complete current item for a transport fallback while preserving
/// document-level XML removals when the POM root layout cannot be updated
/// positionally.
fn full_projection_item(
    current: &CanonicalInputItem,
    previous: &CanonicalInputItem,
) -> CanonicalInputItem {
    match (item_pom(current), item_pom(previous)) {
        (Some(current_pom), Some(previous_pom)) if same_item_role(current, previous) => {
            replace_item_pom(current, full_document_update(current_pom, previous_pom))
        }
        _ => current.clone(),
    }
}

fn lower_node(
    node: &RenderedProjectionNode,
    previous: &ProjectionDiffState,
    candidate: &mut ProjectionDiffState,
    append_policy: &mut ProjectionAppendPolicy,
    source_indexes: &mut HashMap<(String, usize), usize>,
) -> RenderedProjectionNode {
    let templates = node
        .diff_templates()
        .iter()
        .map(|template| (template.item_index(), template))
        .collect::<HashMap<_, _>>();
    let mut items = Vec::with_capacity(node.items().len());
    for (item_index, item) in node.items().iter().enumerate() {
        let template = templates.get(&item_index);
        let lowered = match template {
            Some(template) => lower_template_item(
                node.identity(),
                node.diffs(),
                template,
                item,
                previous,
                candidate,
            ),
            None => Some(item.clone()),
        };
        let repeat = node.repeats_item(item_index);
        let output = if repeat { Some(item.clone()) } else { lowered };
        if let Some(output) = output {
            if repeat || template.is_some() {
                append_policy
                    .items
                    .insert((node.identity().to_owned(), items.len()));
            }
            source_indexes.insert((node.identity().to_owned(), item_index), items.len());
            items.push(output);
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
    let previous_item_matches_template = previous_item_matches_template(
        node_identity,
        diffs,
        template,
        complete_item,
        previous_item,
        previous,
    );

    let mut missing_baseline = previous_item.is_none();
    let mut requires_full_document_fallback =
        previous_item.is_some() && !previous_item_matches_template;
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
                let diff = match previous.slots.get(&key) {
                    Some(baseline) => {
                        diff_documents(authored, complete, &baseline.authored, &baseline.complete)
                    }
                    None => {
                        missing_baseline = true;
                        TemplateDocumentDiff::Full
                    }
                };
                candidate.slots.insert(
                    key,
                    ProjectionDiffBaseline {
                        authored: authored.clone(),
                        complete: complete.clone(),
                    },
                );
                match diff {
                    TemplateDocumentDiff::Omit => {}
                    TemplateDocumentDiff::Patch(patch) => {
                        emitted_diff = true;
                        submission_children.extend(patch.children().clone());
                    }
                    TemplateDocumentDiff::Full => {
                        requires_full_document_fallback = true;
                    }
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
    let previous_item = previous_item.expect("a missing baseline returned above");
    if requires_full_document_fallback {
        return Some(full_projection_item(complete_item, previous_item));
    }
    if !emitted_diff {
        return diff_projection_item(complete_item, previous_item);
    }
    Some(replace_item_pom(
        complete_item,
        ResolvedDocument::new(submission_children),
    ))
}

/// Checks that the current template's non-diff fragments still reconstruct
/// the prior complete item when its diff fragments are replaced by their
/// committed baselines. If they do not, a field delta cannot describe the
/// entire item safely and the caller must use a document-level fallback.
fn previous_item_matches_template(
    node_identity: &str,
    diffs: &[RenderedProjectionDiffMarker],
    template: &RenderedProjectionItemTemplate,
    complete_item: &CanonicalInputItem,
    previous_item: Option<&CanonicalInputItem>,
    previous: &ProjectionDiffState,
) -> bool {
    let Some(previous_item) = previous_item else {
        return false;
    };
    let mut expected_children = BlockChildren::new();
    for fragment in template.fragments() {
        match fragment {
            RenderedProjectionFragment::Complete(complete) => {
                expected_children.extend(complete.children().clone());
            }
            RenderedProjectionFragment::Diff { diff_index, .. } => {
                let key = diff_key(node_identity, &diffs[*diff_index]);
                let Some(baseline) = previous.slots.get(&key) else {
                    return false;
                };
                expected_children.extend(baseline.complete.children().clone());
            }
        }
    }
    replace_item_pom(complete_item, ResolvedDocument::new(expected_children)) == *previous_item
}

fn diff_key(node_identity: &str, diff: &RenderedProjectionDiffMarker) -> ProjectionDiffKey {
    ProjectionDiffKey {
        node_identity: node_identity.to_owned(),
        structural_path: diff.structural_path().to_vec(),
        slot: diff.slot().to_owned(),
    }
}

enum TemplateDocumentDiff {
    Omit,
    Patch(ResolvedDocument),
    /// The fragment cannot safely be represented by an authored field delta.
    /// Its enclosing item must use document-level comparison instead.
    Full,
}

fn diff_documents(
    current_authored: &Document,
    current_complete: &ResolvedDocument,
    previous_authored: &Document,
    previous_complete: &ResolvedDocument,
) -> TemplateDocumentDiff {
    if current_authored == previous_authored || current_complete == previous_complete {
        return TemplateDocumentDiff::Omit;
    }
    let (Some(current_xml), Some(previous_xml)) = (
        single_authored_xml(current_authored),
        single_authored_xml(previous_authored),
    ) else {
        return TemplateDocumentDiff::Full;
    };
    if !supports_structured_delta(current_xml) || !supports_structured_delta(previous_xml) {
        return TemplateDocumentDiff::Full;
    }
    let Some(patch) = diff_xml_nodes(current_xml, previous_xml) else {
        return TemplateDocumentDiff::Omit;
    };
    if patch == *current_xml {
        return TemplateDocumentDiff::Full;
    }
    TemplateDocumentDiff::Patch(resolve_system_document(Document::from_xml(patch)))
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
        pom::{
            BlockChildren, DiffSlot, DiffStrategy, Document, ResolvedDocument, TextNode, XmlNode,
        },
        pom_renderer::render_pom_document,
        pom_resolution::{resolve_artifact_document, resolve_system_document},
        transcript::{AssistantPhase, CanonicalInputItem, ConversationRole, InstructionAuthority},
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

    fn empty_component_projection() -> RenderedProjection {
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("component", Vec::new())])
            .unwrap()
    }

    fn two_diff_projection(first: &str, second: &str) -> RenderedProjection {
        let (_, context) = document("context");
        let (first_authored, first_complete) = document(first);
        let (second_authored, second_complete) = document(second);
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![
                CanonicalInputItem::message(ConversationRole::User, context),
                CanonicalInputItem::message(ConversationRole::User, first_complete.clone()),
                CanonicalInputItem::message(ConversationRole::User, second_complete.clone()),
            ],
            vec![
                RenderedProjectionDiffMarker::new(1, vec![1], "first"),
                RenderedProjectionDiffMarker::new(2, vec![2], "second"),
            ],
            vec![
                RenderedProjectionItemTemplate::new(
                    1,
                    vec![RenderedProjectionFragment::Diff {
                        diff_index: 0,
                        authored: first_authored,
                        complete: first_complete,
                    }],
                ),
                RenderedProjectionItemTemplate::new(
                    2,
                    vec![RenderedProjectionFragment::Diff {
                        diff_index: 1,
                        authored: second_authored,
                        complete: second_complete,
                    }],
                ),
            ],
        );
        RenderedProjection::from_nodes(vec![node]).unwrap()
    }

    fn named_root_diff_projection(name: &str, value: &str) -> RenderedProjection {
        projection_from_authored_root(text_xml(name, value), "state")
    }

    fn empty_diff_projection() -> RenderedProjection {
        let authored = Document::new(BlockChildren::new());
        let complete = resolve_system_document(authored.clone());
        let item = CanonicalInputItem::message(ConversationRole::User, complete.clone());
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
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

    fn mixed_template_projection(context: Option<XmlNode>, state: &str) -> RenderedProjection {
        let state_authored = Document::from_xml(text_xml("state", state));
        let state_complete = resolve_system_document(state_authored.clone());
        let mut complete_children = BlockChildren::new();
        let mut fragments = Vec::new();
        if let Some(context) = context {
            let context = resolve_artifact_document(Document::from_xml(context)).unwrap();
            complete_children.extend(context.children().clone());
            fragments.push(RenderedProjectionFragment::Complete(context));
        }
        complete_children.extend(state_complete.children().clone());
        fragments.push(RenderedProjectionFragment::Diff {
            diff_index: 0,
            authored: state_authored,
            complete: state_complete,
        });
        let item = CanonicalInputItem::message(
            ConversationRole::User,
            ResolvedDocument::new(complete_children),
        );
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![RenderedProjectionItemTemplate::new(0, fragments)],
        );
        RenderedProjection::from_nodes(vec![node]).unwrap()
    }

    fn mixed_structured_template_projection(policy: &str, phase: &str) -> RenderedProjection {
        let policy =
            resolve_artifact_document(Document::from_xml(text_xml("policy", policy))).unwrap();
        let root = XmlNode::try_build("agent_state", |children| {
            children.xml(text_xml("objective", "stable"));
            children.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                text_xml("phase", phase),
            ));
            Ok(())
        })
        .unwrap();
        let authored = Document::from_xml(root);
        let complete = resolve_system_document(authored.clone());
        let mut children = BlockChildren::new();
        children.extend(policy.children().clone());
        children.extend(complete.children().clone());
        let item =
            CanonicalInputItem::message(ConversationRole::User, ResolvedDocument::new(children));
        let node = RenderedProjectionNode::with_diff_templates(
            "component",
            vec![item],
            vec![RenderedProjectionDiffMarker::new(0, vec![0], "state")],
            vec![RenderedProjectionItemTemplate::new(
                0,
                vec![
                    RenderedProjectionFragment::Complete(policy),
                    RenderedProjectionFragment::Diff {
                        diff_index: 0,
                        authored,
                        complete,
                    },
                ],
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

    fn raw_user_xml_projection(values: &[&str]) -> RenderedProjection {
        let items = values
            .iter()
            .map(|value| {
                CanonicalInputItem::message(
                    ConversationRole::User,
                    resolve_artifact_document(Document::from_xml(text_xml("state", value)))
                        .unwrap(),
                )
            })
            .collect();
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", items)]).unwrap()
    }

    fn rendered_pom_items(projection: &RenderedProjection) -> Vec<String> {
        projection
            .nodes()
            .iter()
            .flat_map(RenderedProjectionNode::items)
            .map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => render_pom_document(pom).unwrap(),
                other => panic!("expected a POM projection item, got {other:?}"),
            })
            .collect()
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
    fn reconciliation_uses_the_adjacent_complete_snapshot() {
        let first_complete = text_projection(&[("left", &["A", "A"]), ("right", &["O"])]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();
        assert_eq!(text_projection_items(&first.submission), ["A", "A", "O"]);

        let second_complete =
            text_projection(&[("right", &["O", "P"]), ("left", &["A", "A", "A"])]);
        let second = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &second_complete,
        )
        .unwrap();
        assert_eq!(
            text_projection_items(&second.submission),
            ["O", "P", "A", "A", "A"]
        );

        let omitted_complete = text_projection(&[("right", &["O", "P"])]);
        let omitted = ReconciledProjectionPlan::prepare(
            Some(&second_complete),
            Some(&second.diff_baseline),
            Some(&second.reconciliation),
            &[],
            &omitted_complete,
        )
        .unwrap();
        assert!(text_projection_items(&omitted.submission).is_empty());

        let reappeared_complete = text_projection(&[("left", &["A", "A", "A", "B"])]);
        let reappeared = ReconciledProjectionPlan::prepare(
            Some(&omitted_complete),
            Some(&omitted.diff_baseline),
            Some(&omitted.reconciliation),
            &[],
            &reappeared_complete,
        )
        .unwrap();
        assert_eq!(
            text_projection_items(&reappeared.submission),
            ["A", "A", "A", "B"]
        );

        let replacement_complete = text_projection(&[("replacement", &["A"])]);
        let replacement = ReconciledProjectionPlan::prepare(
            Some(&reappeared_complete),
            Some(&reappeared.diff_baseline),
            Some(&reappeared.reconciliation),
            &[],
            &replacement_complete,
        )
        .unwrap();
        assert_eq!(text_projection_items(&replacement.submission), ["A"]);
    }

    #[test]
    fn ordinary_item_reappearing_after_replacement_is_submitted_again() {
        let first_complete = text_projection(&[("root", &["A"])]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let second_complete = text_projection(&[("root", &["B"])]);
        let second = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &second_complete,
        )
        .unwrap();

        let third_complete = text_projection(&[("root", &["A"])]);
        let third = ReconciledProjectionPlan::prepare(
            Some(&second_complete),
            Some(&second.diff_baseline),
            Some(&second.reconciliation),
            &[],
            &third_complete,
        )
        .unwrap();

        assert_eq!(text_projection_items(&first.submission), ["A"]);
        assert_eq!(text_projection_items(&second.submission), ["B"]);
        assert_eq!(text_projection_items(&third.submission), ["A"]);
    }

    #[test]
    fn omitted_items_emit_xml_removals_only_for_developer_and_user_pom() {
        let pom = |name, value| {
            resolve_artifact_document(Document::from_xml(text_xml(name, value))).unwrap()
        };
        let previous_complete = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "root",
            vec![
                CanonicalInputItem::instruction(
                    InstructionAuthority::Developer,
                    pom("developer", "policy"),
                ),
                CanonicalInputItem::message(ConversationRole::User, pom("user", "request")),
                CanonicalInputItem::instruction(
                    InstructionAuthority::System,
                    pom("system", "baseline"),
                ),
                CanonicalInputItem::message(
                    ConversationRole::Assistant,
                    pom("assistant_message", "reply"),
                ),
                CanonicalInputItem::assistant_text("assistant text", None),
                CanonicalInputItem::tool_call("call-1", "lookup", "{}").expect("valid tool call"),
                CanonicalInputItem::tool_result("call-1", "tool result")
                    .expect("valid tool result"),
            ],
        )])
        .unwrap();
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &previous_complete).unwrap();

        let current =
            RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", Vec::new())])
                .unwrap();
        let omitted = ReconciledProjectionPlan::prepare(
            Some(&previous_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &current,
        )
        .unwrap();

        let items = omitted.submission.nodes()[0].items();
        assert!(matches!(
            items,
            [
                CanonicalInputItem::Instruction {
                    authority: InstructionAuthority::Developer,
                    ..
                },
                CanonicalInputItem::Message {
                    role: ConversationRole::User,
                    ..
                }
            ]
        ));
        let rendered = items
            .iter()
            .map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => render_pom_document(pom).unwrap(),
                other => panic!("only POM removals are expected, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            [
                "<remove>\n  <developer>policy</developer>\n</remove>",
                "<remove>\n  <user>request</user>\n</remove>",
            ]
        );
    }

    #[test]
    fn raw_user_xml_reorder_and_duplicate_omission_emit_ordered_removals() {
        let cases: [(&str, &[&str], &[&str], &[&str]); 2] = [
            (
                "reorder",
                &["a", "b"],
                &["b", "a"],
                &[
                    "<remove>\n  <state>a</state>\n</remove>",
                    "<remove>\n  <state>b</state>\n</remove>",
                    "<state>b</state>",
                    "<state>a</state>",
                ],
            ),
            (
                "one repeated item omitted",
                &["a", "a"],
                &["a"],
                &["<remove>\n  <state>a</state>\n</remove>"],
            ),
        ];

        for (name, previous_values, current_values, expected) in cases {
            let previous_complete = raw_user_xml_projection(previous_values);
            let first =
                ReconciledProjectionPlan::prepare(None, None, None, &[], &previous_complete)
                    .unwrap();
            let current = raw_user_xml_projection(current_values);
            let changed = ReconciledProjectionPlan::prepare(
                Some(&previous_complete),
                Some(&first.diff_baseline),
                Some(&first.reconciliation),
                &[],
                &current,
            )
            .unwrap();

            assert_eq!(rendered_pom_items(&changed.submission), expected, "{name}");
        }
    }

    #[test]
    fn prepending_a_node_refreshes_the_affected_node_suffix() {
        let first_complete = text_projection(&[("left", &["A"])]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let current = text_projection(&[("new", &["B"]), ("left", &["A"])]);
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &current,
        )
        .unwrap();

        assert_eq!(text_projection_items(&changed.submission), ["B", "A"]);
    }

    #[test]
    fn reordering_nodes_refreshes_the_reordered_sequence() {
        let first_complete = text_projection(&[("left", &["A"]), ("right", &["B"])]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let current = text_projection(&[("right", &["B"]), ("left", &["A"])]);
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &current,
        )
        .unwrap();

        assert_eq!(text_projection_items(&changed.submission), ["B", "A"]);
    }

    #[test]
    fn omitted_diff_boundary_drops_its_baseline_before_reappearing() {
        let first_complete = semantic_projection("A", &["initialized"]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let omitted_complete = empty_component_projection();
        let omitted = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &omitted_complete,
        )
        .unwrap();
        assert_eq!(omitted.diff_baseline, ProjectionDiffState::default());

        let reappeared = ReconciledProjectionPlan::prepare(
            Some(&omitted_complete),
            Some(&omitted.diff_baseline),
            Some(&omitted.reconciliation),
            &[],
            &first_complete,
        )
        .unwrap();
        assert_eq!(
            rendered_item(&reappeared.submission).as_deref(),
            Some(
                "<agent_state>\n  <objective>stable</objective>\n  <phase>A</phase>\n  <observations>\n    <item>initialized</item>\n  </observations>\n</agent_state>"
            )
        );
    }

    #[test]
    fn later_diff_template_uses_lowered_source_index_after_prior_template_is_omitted() {
        let first_complete = two_diff_projection("A", "X");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = two_diff_projection("A", "Y");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();

        assert_eq!(changed.submission.nodes()[0].items().len(), 1);
        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<state>Y</state>")
        );
    }

    #[test]
    fn diff_template_root_replacement_removes_the_old_root_before_emitting_the_new_one() {
        let first_complete = named_root_diff_projection("policy", "old");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = named_root_diff_projection("context", "new");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<remove>\n  <policy>old</policy>\n</remove>\n\n<context>new</context>")
        );
    }

    #[test]
    fn repeat_diff_root_replacement_removes_once_then_repeats_the_current_document() {
        let repeat_projection = |name, value| {
            let projection = named_root_diff_projection(name, value);
            let node = projection.nodes()[0].clone().with_repeat_items(vec![0]);
            RenderedProjection::from_nodes(vec![node]).unwrap()
        };
        let first_complete = repeat_projection("policy", "old");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = repeat_projection("context", "new");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();
        assert_ne!(changed.diff_baseline, first.diff_baseline);
        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<remove>\n  <policy>old</policy>\n</remove>\n\n<context>new</context>")
        );

        let unchanged = ReconciledProjectionPlan::prepare(
            Some(&changed_complete),
            Some(&changed.diff_baseline),
            Some(&changed.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();
        assert_eq!(
            rendered_item(&unchanged.submission).as_deref(),
            Some("<context>new</context>")
        );
    }

    #[test]
    fn empty_diff_template_emits_the_previous_root_removal() {
        let first_complete = named_root_diff_projection("policy", "old");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let empty_complete = empty_diff_projection();
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &empty_complete,
        )
        .unwrap();

        assert_eq!(changed.submission.nodes()[0].items().len(), 1);
        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<remove>\n  <policy>old</policy>\n</remove>")
        );
    }

    #[test]
    fn atomic_diff_fallback_retains_unchanged_complete_template_siblings() {
        let first_complete = mixed_template_projection(Some(text_xml("policy", "stable")), "A");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = mixed_template_projection(Some(text_xml("policy", "stable")), "B");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some("<policy>stable</policy>\n\n<state>B</state>")
        );
    }

    #[test]
    fn unchanged_complete_template_sibling_preserves_a_structured_field_delta() {
        let first_complete = mixed_structured_template_projection("stable", "A");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = mixed_structured_template_projection("stable", "B");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some(
                "<policy>stable</policy>\n\n<agent_state rendering_mode=\"delta\">\n  <phase>B</phase>\n</agent_state>"
            )
        );
    }

    #[test]
    fn changed_complete_template_sibling_falls_back_to_the_complete_document_diff() {
        let first_complete = mixed_template_projection(Some(text_xml("policy", "old")), "A");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = mixed_template_projection(Some(text_xml("context", "new")), "A");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some(
                "<remove>\n  <policy>old</policy>\n</remove>\n\n<remove>\n  <state>A</state>\n</remove>\n\n<context>new</context>\n\n<state>A</state>"
            )
        );
    }

    #[test]
    fn removed_complete_template_sibling_falls_back_to_the_complete_document_diff() {
        let first_complete = mixed_template_projection(Some(text_xml("policy", "old")), "A");
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();

        let changed_complete = mixed_template_projection(None, "A");
        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            Some(&first.diff_baseline),
            Some(&first.reconciliation),
            &[],
            &changed_complete,
        )
        .unwrap();

        assert_eq!(
            rendered_item(&changed.submission).as_deref(),
            Some(
                "<remove>\n  <policy>old</policy>\n</remove>\n\n<remove>\n  <state>A</state>\n</remove>\n\n<state>A</state>"
            )
        );
    }

    #[test]
    fn provider_outputs_are_claimed_by_occurrence_after_authored_items() {
        let first_complete = text_projection(&[("root", &["authored"])]);
        let first = ReconciledProjectionPlan::prepare(
            None,
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
            Some(&first_complete),
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
            Some(&first_claim_complete),
            Some(&first_claim.diff_baseline),
            Some(&first_claim.reconciliation),
            &[],
            &second_claim_complete,
        )
        .unwrap();
        assert!(text_projection_items(&second_claim.submission).is_empty());
        assert!(second_claim
            .reconciliation
            .unclaimed_provider_outputs
            .is_empty());
    }

    #[test]
    fn full_reuses_the_complete_snapshot_but_resets_the_diff_baseline() {
        let first_complete = text_projection(&[("root", &["authored"])]);
        let first = ReconciledProjectionPlan::prepare(
            None,
            None,
            None,
            &[CanonicalInputItem::assistant_text("provider", None)],
            &first_complete,
        )
        .unwrap();

        let current = text_projection(&[("root", &["authored", "provider", "new"])]);
        let full = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
            None,
            Some(&first.reconciliation),
            &[],
            &current,
        )
        .unwrap();

        assert_eq!(text_projection_items(&full.submission), ["new"]);
        assert!(full.reconciliation.unclaimed_provider_outputs.is_empty());

        let first_diff_complete = semantic_projection("A", &["initialized"]);
        let first_diff =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_diff_complete).unwrap();
        let reset = ReconciledProjectionPlan::prepare(
            Some(&first_diff_complete),
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

        let plan =
            ReconciledProjectionPlan::prepare(None, None, None, &[output], &complete).unwrap();

        assert!(text_projection_items(&plan.submission).is_empty());
        assert!(plan.reconciliation.unclaimed_provider_outputs.is_empty());
    }

    #[test]
    fn snapshot_tool_claims_do_not_hide_changed_or_duplicate_records() {
        let call = CanonicalInputItem::tool_call("call-1", "lookup", "{}").unwrap();
        let result = CanonicalInputItem::tool_result("call-1", "original").unwrap();
        let projection = |items| {
            RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("tool", items)])
                .unwrap()
        };
        let first_complete = projection(vec![call.clone(), result.clone()]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();
        let changed_result = CanonicalInputItem::tool_result("call-1", "changed").unwrap();
        let changed_call =
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"key":"changed"}"#).unwrap();

        for (items, expected) in [
            (
                vec![call.clone(), changed_result.clone()],
                vec![changed_result],
            ),
            (
                vec![changed_call.clone(), result.clone()],
                vec![changed_call],
            ),
            (
                vec![call.clone(), result.clone(), call.clone(), result.clone()],
                vec![call, result],
            ),
        ] {
            let changed = ReconciledProjectionPlan::prepare(
                Some(&first_complete),
                Some(&first.diff_baseline),
                Some(&first.reconciliation),
                &[],
                &projection(items),
            )
            .unwrap();
            assert_eq!(changed.submission.nodes()[0].items(), expected);
            let mut history = first_complete.nodes()[0].items().to_vec();
            history.extend(expected);
            assert!(crate::transcript::CanonicalTranscript::try_from_items(history).is_err());
        }
    }

    #[test]
    fn snapshot_tool_claims_remain_fenced_after_an_execution_scope_change() {
        let complete = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "tool",
            vec![
                CanonicalInputItem::tool_call("call-1", "lookup", "{}").unwrap(),
                CanonicalInputItem::tool_result("call-1", "result").unwrap(),
            ],
        )])
        .unwrap();
        let first = ReconciledProjectionPlan::prepare(None, None, None, &[], &complete).unwrap();
        let reset = first.reconciliation.reset_authored_for_scope(&[]);

        assert!(matches!(
            ReconciledProjectionPlan::prepare(None, None, Some(&reset), &[], &complete),
            Err(ProjectionReconciliationFault::AmbiguousProjectionProvenance)
        ));
    }

    #[test]
    fn emitted_diff_patch_is_not_suppressed_by_an_equal_provider_output() {
        let first_complete = semantic_projection("A", &["initialized"]);
        let first =
            ReconciledProjectionPlan::prepare(None, None, None, &[], &first_complete).unwrap();
        let changed_complete = semantic_projection("B", &["initialized"]);
        let preview = ProjectionDiffState::prepare(Some(&first.diff_baseline), &changed_complete);
        let patch = preview.submission.nodes()[0].items()[0].clone();

        let changed = ReconciledProjectionPlan::prepare(
            Some(&first_complete),
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
            None,
            &[collision.clone(), collision.clone()],
            &empty,
        )
        .unwrap();
        let reset = previous.reconciliation.reset_authored_for_scope(&[]);

        let once = text_projection(&[("root", &["collision"])]);
        let claimed_current_scope =
            ReconciledProjectionPlan::prepare(None, None, Some(&reset), &[collision], &once)
                .unwrap();
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
                Some(&once),
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
        let previous =
            ReconciledProjectionPlan::prepare(None, None, None, &[collision], &empty).unwrap();
        let reset = previous.reconciliation.reset_authored_for_scope(&[]);

        let unrelated = text_projection(&[("root", &["new-authored"])]);
        let successful =
            ReconciledProjectionPlan::prepare(None, None, Some(&reset), &[], &unrelated).unwrap();
        assert_eq!(
            text_projection_items(&successful.submission),
            ["new-authored"]
        );

        let later_collision = text_projection(&[("root", &["new-authored", "collision"])]);
        assert!(matches!(
            ReconciledProjectionPlan::prepare(
                Some(&unrelated),
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
        let previous =
            ReconciledProjectionPlan::prepare(None, None, None, &[diff_item], &empty).unwrap();
        let reset = previous.reconciliation.reset_authored_for_scope(&[]);

        let plan = ReconciledProjectionPlan::prepare(None, None, Some(&reset), &[], &diff_complete)
            .unwrap();

        assert!(rendered_item(&plan.submission).is_some());
        assert_eq!(plan.reconciliation.ambiguous_provider_outputs.len(), 1);
    }
}
