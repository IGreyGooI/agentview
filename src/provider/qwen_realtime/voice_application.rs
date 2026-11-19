use std::{
    future::IntoFuture,
    sync::{Arc, Mutex},
};

use tokio::sync::{mpsc, oneshot, watch};

use crate::component::{
    execution::{Application, ApplicationFault, ReactionOutcome, ReactionPort, ReactionPortFault},
    prelude::{component, Component},
};

use super::{
    audio::encode_pcm16,
    media::{closed, Input, Interrupt, Media},
    Mode, QwenRealtimeMediaEvents, QwenRealtimeMediaInput, QwenRealtimeProvider,
};

type VoiceReceiver = Arc<Mutex<Option<mpsc::Receiver<Input>>>>;

/// A dedicated voice Application for Qwen Realtime with Smart Turn v2.
///
/// It mounts its own Component tree and retains one WebSocket. Qwen decides
/// when to commit microphone input and reply. After each response, AgentView
/// handles native tools and submits the next Frame before forwarding queued
/// microphone audio after an interruption. Microphone input continues during
/// playback so Qwen can detect barge-in. Playback consumers discard queued
/// audio for the cancelled response ID when they receive `ResponseEnded`.
pub struct QwenRealtimeVoiceApplication(Application<QwenRealtimeProvider>);

impl QwenRealtimeVoiceApplication {
    /// Mount a voice-only Component tree and create its audio input and output handles.
    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
        provider: QwenRealtimeProvider,
    ) -> Result<(Self, QwenRealtimeVoiceInput, QwenRealtimeMediaEvents), ApplicationFault> {
        Self::mount_with_voice(move |_| root(), provider)
    }

    /// Mount a root that can issue voice controls from its Component callbacks.
    pub fn mount_with_voice(
        root: impl Fn(QwenRealtimeVoiceInput) -> Component + Send + Sync + 'static,
        mut provider: QwenRealtimeProvider,
    ) -> Result<(Self, QwenRealtimeVoiceInput, QwenRealtimeMediaEvents), ApplicationFault> {
        let (media, provider_input, events) = Media::new(Mode::SmartTurnV2);
        let active_response = media.subscribe_active_response();
        provider.mode = Mode::SmartTurnV2;
        provider.media = Some(media);
        provider.profile.capabilities = provider
            .profile
            .capabilities
            .with_realtime_audio(true)
            .with_text_delta_output(false);
        let (app, input) = mount_voice(root, provider, provider_input, active_response)?;
        Ok((Self(app), input, events))
    }

    /// Drive one voice session until its input finishes or its Component exits.
    pub async fn run(&mut self) -> Result<ReactionOutcome, ApplicationFault> {
        drive_voice(&mut self.0).await
    }

    /// Close the Application and release its resources.
    pub async fn shutdown(self) -> Result<(), ApplicationFault> {
        self.0.shutdown().await
    }
}

pub(crate) fn mount_voice<P: ReactionPort + 'static>(
    root: impl Fn(QwenRealtimeVoiceInput) -> Component + Send + Sync + 'static,
    provider: P,
    provider_input: QwenRealtimeMediaInput,
    active_response: watch::Receiver<Option<String>>,
) -> Result<(Application<P>, QwenRealtimeVoiceInput), ApplicationFault> {
    let (sender, receiver) = mpsc::channel(8);
    let receiver = Arc::new(Mutex::new(Some(receiver)));
    let input = QwenRealtimeVoiceInput {
        sender,
        active_response,
    };
    let component_input = input.clone();
    let app = Application::mount(
        move || {
            Component::fragment([
                voice_input(receiver.clone(), provider_input.clone()),
                root(component_input.clone()),
            ])
        },
        provider,
    )?;
    Ok((app, input))
}

pub(crate) async fn drive_voice<P: ReactionPort + 'static>(
    application: &mut Application<P>,
) -> Result<ReactionOutcome, ApplicationFault> {
    let reaction = application.react(false)?.into_future();
    tokio::pin!(reaction);
    tokio::select! {
        biased;
        result = &mut reaction => result,
        () = application.refresh_projection_on_signal_writes() => reaction.await,
    }
}

#[component]
fn voice_input(receiver: VoiceReceiver, provider: QwenRealtimeMediaInput) -> Component {
    use_future(move || async move {
        let receiver = receiver.lock().expect("voice input receiver mutex").take();
        let Some(mut receiver) = receiver else { return };
        while let Some(command) = receiver.recv().await {
            if provider.send(command).await.is_err() {
                break;
            }
        }
    });
    Component::realtime_audio()
}

/// The bounded microphone input for an independent voice Application.
#[derive(Clone)]
pub struct QwenRealtimeVoiceInput {
    sender: mpsc::Sender<Input>,
    active_response: watch::Receiver<Option<String>>,
}

impl QwenRealtimeVoiceInput {
    /// Stream 16 kHz mono signed little-endian PCM16, including silence.
    pub async fn append_pcm16(&self, pcm16: &[u8]) -> Result<(), ReactionPortFault> {
        self.send(Input::Append(encode_pcm16(pcm16)?)).await
    }

    /// Tell a provider with microphone pause support that PCM delivery is paused or resumed.
    pub async fn set_input_muted(&self, muted: bool) -> Result<(), ReactionPortFault> {
        self.send(Input::SetMuted(muted)).await
    }

    /// Close the microphone input when the conversation has finished.
    pub async fn finish(&self) -> Result<(), ReactionPortFault> {
        self.send(Input::Finish).await
    }

    /// Request a provider cancellation for a response reported by `ResponseStarted`.
    /// Returns `Ok(false)` if that response has already ended. A provider rejection
    /// is returned to the caller while the voice session continues.
    pub async fn interrupt(&self, response_id: String) -> Result<bool, ReactionPortFault> {
        // Media clears this snapshot before completion reaches Component
        // callbacks. Return immediately for an ended reply, including in
        // callbacks that must settle before Media can process another command.
        if self.active_response.borrow().as_deref() != Some(response_id.as_str()) {
            return Ok(false);
        }
        let (result, receiver) = oneshot::channel();
        self.send(Input::Interrupt(Interrupt {
            response_id,
            result,
        }))
        .await?;
        receiver.await.map_err(|_| closed())?
    }

    async fn send(&self, input: Input) -> Result<(), ReactionPortFault> {
        self.sender.send(input).await.map_err(|_| closed())
    }
}
