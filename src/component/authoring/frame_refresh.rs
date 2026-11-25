use std::sync::Arc;

use tokio::sync::watch;

use crate::component::{execution::ReactionPortFault, signal::HookMount};

pub(crate) enum FrameRefreshRequest {
    Silent,
    Respond {
        correlation: String,
        instruction: String,
    },
    Replace {
        response_id: String,
        instruction: String,
    },
}

type Dispatch = dyn Fn(FrameRefreshRequest) -> Result<(), ReactionPortFault> + Send + Sync;

#[derive(Clone)]
pub(crate) struct FrameRefreshDispatcher {
    active_response: watch::Receiver<Option<String>>,
    dispatch: Arc<Dispatch>,
}

impl FrameRefreshDispatcher {
    pub(crate) fn new(
        active_response: watch::Receiver<Option<String>>,
        dispatch: impl Fn(FrameRefreshRequest) -> Result<(), ReactionPortFault> + Send + Sync + 'static,
    ) -> Self {
        Self {
            active_response,
            dispatch: Arc::new(dispatch),
        }
    }
}

/// Explicit context delivery for the provider session owned by this Component's application.
#[derive(Clone)]
pub struct FrameRefresh {
    mount: HookMount,
    dispatcher: Option<FrameRefreshDispatcher>,
}

impl FrameRefresh {
    pub(crate) fn new(mount: HookMount, dispatcher: Option<FrameRefreshDispatcher>) -> Self {
        Self { mount, dispatcher }
    }

    /// Submit the current Component context at the provider's next safe boundary.
    pub async fn request(&self) -> Result<(), FrameRefreshError> {
        self.dispatch(FrameRefreshRequest::Silent)
    }

    /// Submit context and then request a reply using a trusted, one-time instruction.
    /// The correlation identifies this delivery in provider observations.
    pub async fn request_and_respond(
        &self,
        correlation: impl Into<String>,
        instruction: impl Into<String>,
    ) -> Result<(), FrameRefreshError> {
        self.dispatch(FrameRefreshRequest::Respond {
            correlation: correlation.into(),
            instruction: instruction.into(),
        })
    }

    /// Capture the current provider response ID for a later targeted replacement.
    pub fn active_response_id(&self) -> Result<Option<String>, FrameRefreshError> {
        self.with_dispatcher(|dispatcher| Ok(dispatcher.active_response.borrow().clone()))
    }

    /// Cancel the specified active provider response, submit context, and request a new reply.
    /// The provider admits the replacement only while that response still owns the turn.
    pub async fn interrupt_and_respond(
        &self,
        response_id: impl Into<String>,
        instruction: impl Into<String>,
    ) -> Result<(), FrameRefreshError> {
        self.dispatch(FrameRefreshRequest::Replace {
            response_id: response_id.into(),
            instruction: instruction.into(),
        })
    }

    fn dispatch(&self, request: FrameRefreshRequest) -> Result<(), FrameRefreshError> {
        self.with_dispatcher(|dispatcher| {
            (dispatcher.dispatch)(request).map_err(FrameRefreshError::Provider)
        })
    }

    fn with_dispatcher<T>(
        &self,
        operation: impl FnOnce(&FrameRefreshDispatcher) -> Result<T, FrameRefreshError>,
    ) -> Result<T, FrameRefreshError> {
        // A callback may outlive its Component. Use the mount fence for the
        // immediate enqueue so retirement cannot let an obsolete request enter
        // the session while a newer mount owns the view.
        self.mount
            .with_active(|| {
                let dispatcher = self
                    .dispatcher
                    .as_ref()
                    .ok_or(FrameRefreshError::Unsupported)?;
                operation(dispatcher)
            })
            .ok_or(FrameRefreshError::StaleMount)?
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameRefreshError {
    #[error("the Frame refresh belongs to a stale Component mount")]
    StaleMount,
    #[error("this application does not provide Component Frame refresh")]
    Unsupported,
    #[error("the provider could not admit the Frame refresh: {0}")]
    Provider(#[source] ReactionPortFault),
}

/// Retain a handle for explicit context delivery to the current provider session.
/// Call inside a `#[component]` function and use the handle from async work.
/// Update Component Signals before requesting delivery. Success reports admission.
#[track_caller]
pub fn use_frame_refresh() -> FrameRefresh {
    panic!("use_frame_refresh must be called directly inside a #[component] function")
}

#[cfg(test)]
#[path = "frame_refresh_tests.rs"]
mod tests;
