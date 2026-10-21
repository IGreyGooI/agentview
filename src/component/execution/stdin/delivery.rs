//! One native Frame cursor per frontend connection, independent of application state.

use std::{
    num::{NonZeroU128, NonZeroU64},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{ensure, Context, Result};

use super::{render_items, StdinProjectionResponse, StdinResponse};
use crate::component::execution::{
    frame::{FrameCommitCandidate, FrameSession},
    reaction::{
        ContextWindow, FrameCapabilities, FrameConstraints, FrameProfile, TargetDeclaration,
        TargetEpoch, TargetIdentity,
    },
};
use crate::transcript::{CanonicalInputItem, InstructionAuthority};

static NEXT_STDIN_TARGET: AtomicU64 = AtomicU64::new(1);
const STDIN_TARGET_DOMAIN: u128 = 4_u128 << 64;

pub(super) struct StdinDelivery {
    session: FrameSession,
    declaration: TargetDeclaration,
    system_present: bool,
}

pub(super) struct StdinDeliveryCommit {
    native: FrameCommitCandidate,
    system_present: bool,
}

impl StdinDelivery {
    pub(super) fn new() -> Result<Self> {
        let instance = NEXT_STDIN_TARGET
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok()
            .context("stdin target identity exhausted")?;
        // stdin has no model context window or fixed message-size cap. Leave
        // room for the compiler's Full reserve without imposing a small I/O cap.
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: usize::MAX,
                max_component_bytes: usize::MAX / 2,
                context_window: ContextWindow::NotApplicable,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        );
        let declaration = TargetDeclaration::full(
            TargetIdentity::new(
                NonZeroU128::new(STDIN_TARGET_DOMAIN | u128::from(instance))
                    .expect("stdin target domain is nonzero"),
            ),
            TargetEpoch::new(NonZeroU64::MIN),
            profile,
        );
        Ok(Self {
            session: FrameSession::new(&declaration)?,
            declaration,
            system_present: false,
        })
    }

    pub(super) fn prepare(
        &self,
        output: StdinProjectionResponse,
    ) -> Result<(StdinResponse, StdinDeliveryCommit)> {
        ensure!(
            output.projection.native_tools().is_empty(),
            "stdin views cannot contain native provider tools"
        );
        let system_present = output
            .projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .any(|item| {
                matches!(item,
                    CanonicalInputItem::Instruction { authority: InstructionAuthority::System, pom }
                        if !pom.children().is_empty()
                )
            });
        let (frame, candidate) = self
            .session
            .prepare(&self.declaration, &output.projection)?
            .into_parts();
        let submission = frame.submission();
        let mut sections = vec![render_items(
            submission.replay().iter().chain(submission.staged_inputs()),
        )?];
        if self.system_present && !system_present {
            // Native Full encodes a removed System snapshot by omission. stdout
            // needs an explicit replacement because its earlier text still exists.
            sections.push("<system_snapshot />\n".to_owned());
        }
        for item in submission
            .projection()
            .items()
            .iter()
            .chain(submission.ephemeral())
        {
            let text = render_items(std::iter::once(item))?;
            sections.push(if is_system(item) {
                format!("<system_snapshot>\n{text}</system_snapshot>\n")
            } else {
                text
            });
        }
        let view = sections
            .into_iter()
            .filter(|section| !section.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Ok((
            StdinResponse {
                ok: output.ok,
                result: output.result,
                view,
            },
            StdinDeliveryCommit {
                native: candidate,
                system_present,
            },
        ))
    }

    /// A failed encode/write discards its candidate and closes the connection.
    pub(super) fn commit(&mut self, candidate: StdinDeliveryCommit) {
        self.session.commit(candidate.native);
        self.system_present = candidate.system_present;
        self.declaration = TargetDeclaration::resume(
            self.session
                .target_delivery
                .committed_revision
                .expect("committed stdin Frame"),
            self.declaration.profile().clone(),
        );
    }
}

fn is_system(item: &CanonicalInputItem) -> bool {
    matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        component::execution::{RenderedProjection, RenderedProjectionNode},
        pom::{Document, TextNode, XmlNode},
        pom_resolution::resolve_system_document,
    };

    fn output(system: Option<&str>) -> StdinProjectionResponse {
        let items = system
            .into_iter()
            .map(|value| {
                let node = XmlNode::try_build("policy", |children| {
                    children.text(TextNode::new(value));
                    Ok(())
                })
                .unwrap();
                CanonicalInputItem::instruction(
                    InstructionAuthority::System,
                    resolve_system_document(Document::from_xml(node)),
                )
            })
            .collect();
        StdinProjectionResponse {
            ok: true,
            result: None,
            projection: RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
                "system", items,
            )])
            .unwrap(),
        }
    }

    #[test]
    fn discarded_delivery_keeps_the_native_baseline_uncommitted() {
        let mut delivery = StdinDelivery::new().unwrap();
        let (_, discarded) = delivery.prepare(output(Some("A"))).unwrap();
        drop(discarded);
        assert!(delivery
            .session
            .target_delivery
            .committed_revision
            .is_none());
        assert!(!delivery.system_present);
        let (_, candidate) = delivery.prepare(output(Some("A"))).unwrap();
        assert!(candidate.system_present);
        delivery.commit(candidate);
        assert!(delivery
            .session
            .target_delivery
            .committed_revision
            .is_some());
        let (frame, _) = delivery
            .session
            .prepare(&delivery.declaration, &output(Some("A")).projection)
            .unwrap()
            .into_parts();
        assert!(frame.submission().projection().items().is_empty());
    }

    #[test]
    fn system_presence_advances_only_when_delivery_is_committed() {
        let mut delivery = StdinDelivery::new().unwrap();
        let (_, candidate) = delivery.prepare(output(Some("A"))).unwrap();
        assert!(candidate.system_present);
        assert!(!delivery.system_present);
        delivery.commit(candidate);
        let (frame, _) = delivery
            .session
            .prepare(&delivery.declaration, &output(Some("A")).projection)
            .unwrap()
            .into_parts();
        assert!(frame.submission().projection().items().is_empty());

        let (_, candidate) = delivery.prepare(output(Some("B"))).unwrap();
        assert!(candidate.system_present);
        delivery.commit(candidate);
        let (_, candidate) = delivery.prepare(output(None)).unwrap();
        assert!(!candidate.system_present);
        assert!(delivery.system_present);
        delivery.commit(candidate);
        assert!(!delivery.system_present);
        let (frame, _) = delivery
            .session
            .prepare(&delivery.declaration, &output(None).projection)
            .unwrap()
            .into_parts();
        assert!(frame.submission().projection().items().is_empty());
    }

    #[test]
    fn an_empty_system_instruction_clears_the_normalized_snapshot() {
        let mut delivery = StdinDelivery::new().unwrap();
        let (_, candidate) = delivery.prepare(output(Some("A"))).unwrap();
        delivery.commit(candidate);
        let mut empty = output(None);
        empty.projection = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "system",
            vec![CanonicalInputItem::instruction(
                InstructionAuthority::System,
                resolve_system_document(Document::build(|_| {})),
            )],
        )])
        .unwrap();
        let (_, candidate) = delivery.prepare(empty).unwrap();
        assert!(!candidate.system_present);
        delivery.commit(candidate);
        assert!(!delivery.system_present);
    }
}
