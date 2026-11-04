use crate::component::ApplicationId;

/// Read the stable identity of the application that owns this Component.
///
/// The identity is unchanged across renders and remounts of the same host.
/// A forked application has its own identity. Call this hook directly inside
/// a `#[component]` function.
#[track_caller]
pub fn use_application_id() -> ApplicationId {
    panic!("use_application_id must be called directly inside a #[component] function")
}
