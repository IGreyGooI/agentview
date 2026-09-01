use agentview::component::prelude::*;

/// Shared root input used to pin the first canonical Frame across examples.
#[component]
pub(crate) fn shared_frame_workflow_root() -> Component {
    view! { frame_workflow_golden { "same root input" } }
}

/// Exact canonical FrameSubmissionV1 bytes for `shared_frame_workflow_root`.
pub(crate) const EXACT_FIRST_FRAME: &[u8] = br#"{"component":{"projection":{"items":[{"kind":"message","payload":{"pom":{"children":[{"Node":{"Xml":{"attributes":[],"children":[{"Node":{"Text":{"value":"same root input"}}}],"metadata":{"collection_kind":null,"identity":null},"name":"frame_workflow_golden"}}}]},"role":"user"}}]},"tools":[],"version":1},"replay":[],"staged_inputs":[],"version":1}"#;

pub(crate) fn assert_exact_first_frame(actual: &[u8]) {
    assert_eq!(
        actual,
        EXACT_FIRST_FRAME,
        "actual canonical Frame: {}",
        String::from_utf8_lossy(actual)
    );
}
