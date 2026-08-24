pub use crate::component::signal::Signal;

/// Declare one retained state slot in the owning `#[component]` render.
///
/// The attribute macro rewrites a direct call to the mounted hook context. A
/// call from a plain helper has no Component scope and fails closed.
#[track_caller]
pub fn use_signal<T>(_initialize: impl FnOnce() -> T) -> Signal<T>
where
    T: Send + Sync + 'static,
{
    panic!("use_signal must be called directly inside a #[component] function")
}
