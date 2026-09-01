use super::super::{event_input::EventRouteProjectionFault, handler::HandlerFault};

#[derive(Debug, thiserror::Error)]
pub(crate) enum StreamingXmlMountFault {
    #[error("streaming contract `{identity}` uses an EventInput from another render generation")]
    ForeignEventInput { identity: &'static str },
    #[error("streaming XML tag `{tag}` is not a valid XML name: {detail}")]
    InvalidTag { tag: &'static str, detail: String },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum StreamingXmlDispatchFault {
    #[error(transparent)]
    Route(#[from] EventRouteProjectionFault),
    #[error("streaming contract `{contract}` received the wrong routed event type")]
    EventTypeMismatch { contract: &'static str },
    #[error("streaming XML route received text after completion")]
    RouteTextAfterCompletion,
    #[error(
        "streaming XML route TextComplete ({completion_bytes} bytes) does not extend {delta_bytes} delta bytes"
    )]
    RouteCompletionMismatch {
        delta_bytes: usize,
        completion_bytes: usize,
    },
    #[error("streaming XML route finished without TextComplete")]
    RouteMissingTextComplete,
    #[error("streaming XML route was already finished")]
    RouteAlreadyFinished,
    #[error("streaming XML input exceeded {maximum} bytes (observed {observed})")]
    InputLimitExceeded { maximum: usize, observed: usize },
    #[error("streaming XML element depth exceeded {maximum} (observed {observed})")]
    ElementDepthLimitExceeded { maximum: usize, observed: usize },
    #[error("streaming XML element exceeded {maximum} attributes (observed {observed})")]
    AttributeLimitExceeded { maximum: usize, observed: usize },
    #[error("streaming contract `{contract}` {phase} handler failed: {source}")]
    Handler {
        contract: &'static str,
        phase: &'static str,
        #[source]
        source: HandlerFault,
    },
}
