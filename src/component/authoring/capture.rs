use crate::{
    component::execution::{
        RenderedProjectionDiffMarker, RenderedProjectionError, RenderedProjectionFragment,
        RenderedProjectionItemTemplate,
    },
    pom::{BlockChildren, Document, PomError, ResolvedDocument},
    pom_resolution::{resolve_artifact_document, resolve_system_document, PomResolutionError},
    transcript::{
        CanonicalInputItem, CanonicalTranscript, CanonicalTranscriptError, ConversationRole,
        InstructionAuthority,
    },
};

use super::declaration::Placement;

pub(crate) struct ProjectionRunCapture {
    pub(crate) placement: Placement,
    pub(crate) fragments: Vec<ProjectionFragmentCapture>,
}

pub(crate) enum ProjectionFragmentCapture {
    Complete(BlockChildren),
    Diff {
        structural_path: Vec<usize>,
        slot: &'static str,
        children: BlockChildren,
    },
}

pub(crate) struct BuiltProjectionItems {
    pub(crate) items: Vec<CanonicalInputItem>,
    pub(crate) repeat_items: Vec<usize>,
    pub(crate) ephemeral_items: Vec<usize>,
    pub(crate) diffs: Vec<RenderedProjectionDiffMarker>,
    pub(crate) diff_templates: Vec<RenderedProjectionItemTemplate>,
}

pub(crate) fn build_projection_items(
    sealed_system: Option<&ResolvedDocument>,
    runs: Vec<ProjectionRunCapture>,
) -> Result<BuiltProjectionItems, ComponentCaptureError> {
    let mut transcript = CanonicalTranscript::new();
    let mut repeat_items = Vec::new();
    let mut ephemeral_items = Vec::new();
    let mut diffs = Vec::new();
    let mut diff_templates = Vec::new();
    if let Some(system) = sealed_system {
        transcript = transcript.appended(CanonicalInputItem::instruction(
            InstructionAuthority::System,
            system.clone(),
        ))?;
    }
    for run in runs {
        if run.fragments.is_empty() {
            continue;
        }
        let item_index = transcript.items().len();
        let mut complete_children = BlockChildren::new();
        let mut rendered_fragments = Vec::with_capacity(run.fragments.len());
        let mut has_diff = false;
        for fragment in run.fragments {
            let (children, address) = match fragment {
                ProjectionFragmentCapture::Complete(children) => (children, None),
                ProjectionFragmentCapture::Diff {
                    structural_path,
                    slot,
                    children,
                } => (children, Some((structural_path, slot))),
            };
            let authored = Document::new(children);
            // The outer #[diff] owns history; nested slots describe fields in this complete value.
            let complete = if address.is_some() {
                resolve_system_document(authored.clone())
            } else {
                resolve_artifact_document(authored.clone())?
            };
            complete_children.extend(complete.children().clone());
            if let Some((structural_path, slot)) = address {
                has_diff = true;
                let diff_index = diffs.len();
                diffs.push(RenderedProjectionDiffMarker::new(
                    item_index,
                    structural_path,
                    slot,
                ));
                rendered_fragments.push(RenderedProjectionFragment::Diff {
                    diff_index,
                    authored,
                    complete,
                });
            } else {
                rendered_fragments.push(RenderedProjectionFragment::Complete(complete));
            }
        }
        let pom = ResolvedDocument::new(complete_children);
        let item = match run.placement {
            Placement::Developer
            | Placement::DeveloperRepeat
            | Placement::DeveloperEphemeralRepeat => {
                CanonicalInputItem::instruction(InstructionAuthority::Developer, pom)
            }
            Placement::User | Placement::UserRepeat | Placement::UserEphemeralRepeat => {
                CanonicalInputItem::message(ConversationRole::User, pom)
            }
            Placement::Assistant => CanonicalInputItem::message(ConversationRole::Assistant, pom),
            Placement::SystemOnce => unreachable!("late System is rejected during capture"),
        };
        transcript = transcript.appended(item)?;
        if matches!(
            run.placement,
            Placement::DeveloperRepeat
                | Placement::UserRepeat
                | Placement::DeveloperEphemeralRepeat
                | Placement::UserEphemeralRepeat
        ) {
            repeat_items.push(item_index);
        }
        if matches!(
            run.placement,
            Placement::DeveloperEphemeralRepeat | Placement::UserEphemeralRepeat
        ) {
            ephemeral_items.push(item_index);
        }
        if has_diff {
            diff_templates.push(RenderedProjectionItemTemplate::new(
                item_index,
                rendered_fragments,
            ));
        }
    }
    Ok(BuiltProjectionItems {
        items: transcript.items().to_vec(),
        repeat_items,
        ephemeral_items,
        diffs,
        diff_templates,
    })
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentCaptureError {
    #[error("System placement changed during one mounted Component attempt in {component}")]
    SystemTopologyDrift { component: &'static str },
    #[error("invalid diff slot `{slot}`")]
    InvalidDiffSlot { slot: &'static str },
    #[error("duplicate diff address `{address}`")]
    DuplicateDiffAddress { address: String },
    #[error("Component declaration exceeded maximum depth {maximum}")]
    DeclarationDepthExceeded { maximum: usize },
    #[error(transparent)]
    Pom(#[from] PomError),
    #[error(transparent)]
    Resolution(#[from] PomResolutionError),
    #[error(transparent)]
    Transcript(#[from] CanonicalTranscriptError),
    #[error(transparent)]
    Projection(#[from] RenderedProjectionError),
}

#[cfg(test)]
mod tests {
    use crate::{
        component::{prelude::*, ComponentHost},
        pom::Document,
        pom_resolution::resolve_system_document,
        transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
    };

    #[component]
    fn mixed_prompt_modes(_props: ()) -> Component {
        view! {
            #[developer]
            "Ordinary policy."

            #[developer(repeat)]
            "Repeated policy."

            #[developer(ephemeral, repeat)]
            "Current context."

            #[developer(repeat, ephemeral)]
            "More current context."

            #[developer(repeat)]
            "Another repeated policy."

            #[user(ephemeral, repeat)]
            #[diff(slot = "workspace")]
            workspace { "Current state." }

            #[user]
            "Ordinary task."
        }
    }

    #[component]
    fn deepest_context() -> Component {
        view! {
            #[user(ephemeral, repeat)]
            "Deepest context."
        }
    }

    #[component]
    fn nested_context() -> Component {
        view! {
            #[developer(repeat)]
            "Policy."

            #[developer(ephemeral, repeat)]
            "Current context."

            #[assistant]
            "Example."

            deepest_context()
        }
    }

    #[component]
    fn inherited_prompt_modes(_props: ()) -> Component {
        view! {
            #[user(ephemeral, repeat)]
            nested_context()

            #[developer]
            nested_context()

            "Ordinary sibling."
        }
    }

    #[test]
    fn capture_separates_prompt_modes_and_merges_contiguous_ephemeral_content() {
        let mut host = ComponentHost::new_root(mixed_prompt_modes, ());
        let rendered = host.render().unwrap();
        let node = rendered
            .projection()
            .nodes()
            .iter()
            .find(|node| node.identity().contains("mixed_prompt_modes"))
            .unwrap();

        assert_eq!(node.items().len(), 6);
        for index in 0..6 {
            assert_eq!(node.repeats_item(index), matches!(index, 1..=4));
            assert_eq!(node.is_ephemeral_item(index), matches!(index, 2 | 4));
        }
        let CanonicalInputItem::Instruction { pom, .. } = &node.items()[2] else {
            panic!("ephemeral developer content retains its role");
        };
        assert_eq!(
            pom,
            &resolve_system_document(Document::build(|blocks| {
                blocks.paragraph(|inline| inline.try_text("Current context.").unwrap());
                blocks.paragraph(|inline| inline.try_text("More current context.").unwrap());
            }))
        );
        assert_eq!(node.diffs().len(), 1);
        assert_eq!(node.diffs()[0].item_index(), 4);
    }

    #[test]
    fn outer_placement_overrides_inner_ephemeral_modes_across_component_subtrees() {
        let mut host = ComponentHost::new_root(inherited_prompt_modes, ());
        let rendered = host.render().unwrap();
        let nodes = rendered.projection().nodes();
        let context_nodes = nodes
            .iter()
            .filter(|node| node.identity().contains("nested_context") && !node.items().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(context_nodes.len(), 4);
        for (index, node) in context_nodes.into_iter().enumerate() {
            assert_eq!(node.items().len(), 1);
            assert_eq!(node.is_ephemeral_item(0), index < 2);
            assert_eq!(node.repeats_item(0), index < 2);
            if index < 2 {
                assert!(matches!(
                    node.items()[0],
                    CanonicalInputItem::Message {
                        role: ConversationRole::User,
                        ..
                    }
                ));
            } else {
                assert!(matches!(
                    node.items()[0],
                    CanonicalInputItem::Instruction {
                        authority: InstructionAuthority::Developer,
                        ..
                    }
                ));
            }
        }
        let parent = nodes
            .iter()
            .find(|node| node.identity().contains("inherited_prompt_modes"))
            .unwrap();
        assert_eq!(parent.items().len(), 1);
        assert!(!parent.is_ephemeral_item(0));
        assert!(!parent.repeats_item(0));
    }
}
