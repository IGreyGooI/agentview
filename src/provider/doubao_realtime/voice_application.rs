use std::future::Future;

use crate::component::{
    execution::{Application, ApplicationFault, ReactionOutcome},
    prelude::Component,
};
use crate::transcript::CanonicalInputItem;

use crate::provider::qwen_realtime::{
    voice_application::{drive_voice, mount_voice},
    QwenRealtimeMediaEvents, QwenRealtimeVoiceInput,
};

use super::DoubaoRealtimeProvider;

/// An independent voice Application using Doubao's realtime provider.
///
/// Microphone input, media events, interruption controls and Component refresh
/// use the same transport contract as the Qwen voice Application.
pub struct DoubaoRealtimeVoiceApplication(Application<DoubaoRealtimeProvider>);

impl DoubaoRealtimeVoiceApplication {
    /// Mount a voice Component tree and create its audio input and output handles.
    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
        provider: DoubaoRealtimeProvider,
    ) -> Result<(Self, QwenRealtimeVoiceInput, QwenRealtimeMediaEvents), ApplicationFault> {
        Self::mount_with_voice(move |_| root(), provider)
    }

    /// Mount a root that can issue voice controls from its Component callbacks.
    pub fn mount_with_voice(
        root: impl Fn(QwenRealtimeVoiceInput) -> Component + Send + Sync + 'static,
        provider: DoubaoRealtimeProvider,
    ) -> Result<(Self, QwenRealtimeVoiceInput, QwenRealtimeMediaEvents), ApplicationFault> {
        let (provider, provider_input, events, active_response) = provider.with_audio();
        let (app, input) = mount_voice(root, provider, provider_input, active_response)?;
        Ok((Self(app), input, events))
    }

    /// Set request-only prompts loaded at each normal Frame boundary.
    ///
    /// Captured-state updates do not schedule a render or context refresh.
    /// See [`Application::set_frame_ephemeral_loader`].
    pub fn set_frame_ephemeral_loader<F, Fut>(&mut self, loader: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<CanonicalInputItem>> + Send + 'static,
    {
        self.0.set_frame_ephemeral_loader(loader);
    }

    /// Drive the voice session while Component state updates refresh its projection.
    pub async fn run(&mut self) -> Result<ReactionOutcome, ApplicationFault> {
        drive_voice(&mut self.0).await
    }

    /// Close the Application and release its resources.
    pub async fn shutdown(self) -> Result<(), ApplicationFault> {
        self.0.shutdown().await
    }
}
