use crate::component::{ApplicationId, ReactionId, ReactionKind};

use super::{Frame, RenderedProjection};

/// Read-only diagnostics for one prepared submission attempt.
///
/// The projection and its revision are frozen by this reaction's normal
/// preparation. They remain paired with this Frame even when another reaction
/// prepares newer shared state. A callback does not imply HTTP handoff: a port
/// may reject the Frame, and continuity recovery may prepare another attempt.
pub struct FrameObservation<'a> {
    pub(crate) application_id: ApplicationId,
    pub(crate) reaction_id: ReactionId,
    pub(crate) reaction_kind: ReactionKind,
    pub(crate) projection_revision: u64,
    pub(crate) projection: &'a RenderedProjection,
    pub(crate) frame: &'a Frame,
}

impl FrameObservation<'_> {
    pub fn application_id(&self) -> ApplicationId {
        self.application_id
    }

    pub fn reaction_id(&self) -> ReactionId {
        self.reaction_id
    }

    pub fn reaction_kind(&self) -> ReactionKind {
        self.reaction_kind
    }

    pub fn projection_revision(&self) -> u64 {
        self.projection_revision
    }

    pub fn projection(&self) -> &RenderedProjection {
        self.projection
    }

    pub fn frame(&self) -> &Frame {
        self.frame
    }
}
