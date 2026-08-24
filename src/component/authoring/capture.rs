use crate::{
    component::execution::{
        RenderedProjectionDiff, RenderedProjectionError, RenderedProjectionFragment,
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
    pub(crate) diffs: Vec<RenderedProjectionDiff>,
    pub(crate) diff_templates: Vec<RenderedProjectionItemTemplate>,
}

pub(crate) fn build_projection_items(
    sealed_system: Option<&ResolvedDocument>,
    runs: Vec<ProjectionRunCapture>,
) -> Result<BuiltProjectionItems, ComponentCaptureError> {
    let mut transcript = CanonicalTranscript::new();
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
                diffs.push(RenderedProjectionDiff::new(
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
            Placement::Developer => {
                CanonicalInputItem::instruction(InstructionAuthority::Developer, pom)
            }
            Placement::User => CanonicalInputItem::message(ConversationRole::User, pom),
            Placement::SystemOnce => unreachable!("late System is rejected during capture"),
        };
        transcript = transcript.appended(item)?;
        if has_diff {
            diff_templates.push(RenderedProjectionItemTemplate::new(
                item_index,
                rendered_fragments,
            ));
        }
    }
    Ok(BuiltProjectionItems {
        items: transcript.items().to_vec(),
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
