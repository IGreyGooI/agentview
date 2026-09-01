use crate::component::{
    execution::{DriverDemandFault, DriverDemandHandle},
    signal::HookMount,
};

/// Mount-scoped capability for requesting at least one later reaction turn.
///
/// Requests are sticky and coalesce until the owning application driver
/// consumes them. Requesting does not render, enter a reaction, or submit a
/// Frame.
#[derive(Clone)]
pub struct ReactionRequest {
    mount: HookMount,
    demand: DriverDemandHandle,
}

impl ReactionRequest {
    pub(crate) fn new(mount: HookMount, demand: DriverDemandHandle) -> Self {
        Self { mount, demand }
    }

    pub fn request(&self) -> Result<(), ReactionRequestError> {
        self.mount
            .with_active(|| self.demand.request())
            .ok_or(ReactionRequestError::StaleMount)?
            .map_err(ReactionRequestError::from)
    }
}

impl From<DriverDemandFault> for ReactionRequestError {
    fn from(_fault: DriverDemandFault) -> Self {
        Self::StaleMount
    }
}

/// Failure to request a later reaction from a Component capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReactionRequestError {
    #[error("the reaction request belongs to a stale Component mount")]
    StaleMount,
}

/// Retain a mount-scoped handle that can request a later driver turn.
///
/// This hook must be called directly inside a `#[component]` function so the
/// macro can bind it to the Component's lexical hook slot.
#[track_caller]
pub fn use_reaction_request() -> ReactionRequest {
    panic!("use_reaction_request must be called directly inside a #[component] function")
}
