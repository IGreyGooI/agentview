use std::panic::panic_any;

use crate::component::signal as signal_kernel;

use super::Signal;

/// Runtime hook authority passed only to repeatable Component renderers.
#[doc(hidden)]
pub struct HookRenderContext<'render> {
    signals: &'render mut signal_kernel::SignalRenderScope,
    attempt_local_allowed: bool,
}

impl<'render> HookRenderContext<'render> {
    pub(crate) fn new(signals: &'render mut signal_kernel::SignalRenderScope) -> Self {
        Self {
            signals,
            attempt_local_allowed: true,
        }
    }

    pub(crate) fn for_system(signals: &'render mut signal_kernel::SignalRenderScope) -> Self {
        Self {
            signals,
            attempt_local_allowed: false,
        }
    }

    #[doc(hidden)]
    pub fn use_signal_at<T>(&mut self, site: u32, initialize: impl FnOnce() -> T) -> Signal<T>
    where
        T: Send + Sync + 'static,
    {
        if !self.attempt_local_allowed {
            panic_any(HookRenderAbort::SystemAttemptLocal {
                capability: "signal",
            });
        }
        self.signals
            .use_signal_at(site, initialize)
            .unwrap_or_else(|fault| panic_any(HookRenderAbort::Signal(fault)))
    }
}

#[derive(Debug)]
pub(crate) enum HookRenderAbort {
    Signal(signal_kernel::SignalRenderError),
    SystemAttemptLocal { capability: &'static str },
}
