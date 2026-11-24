use std::{
    future::IntoFuture,
    sync::{Arc, Mutex},
};

use tokio::sync::{mpsc, oneshot, watch, Semaphore};

use crate::component::{
    execution::{
        Application, ApplicationFault, ReactionOutcome, ReactionPort, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultReason,
    },
    prelude::{component, Component},
};

use super::{
    audio::encode_pcm16,
    media::{closed, Input, Interrupt, Media, SpokenText},
    text::{TextCorrelations, TextRequest},
    Mode, QwenRealtimeMediaEvents, QwenRealtimeMediaInput, QwenRealtimeProvider,
    QwenRealtimeTextTurn,
};

type VoiceReceiver = Arc<Mutex<Option<mpsc::Receiver<Input>>>>;
type ControlReceiver = Arc<Mutex<Option<mpsc::Receiver<VoiceControl>>>>;

struct VoiceControl {
    input: Option<Input>,
    forwarded: oneshot::Sender<Result<(), ReactionPortFault>>,
}
const PENDING_SPEECH_CAPACITY: usize = 8;
const MAX_SPOKEN_TEXT_BYTES: usize = 64 * 1024;

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
    /// Mount a realtime Component tree and create its input and output handles.
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
    let context_refresh_supported = provider.supports_context_refresh();
    let (text_correlation, text_correlations) = provider_input.text_correlations();
    let (sender, receiver) = mpsc::channel(8);
    let receiver = Arc::new(Mutex::new(Some(receiver)));
    let (controls, control_receiver) = mpsc::channel(32);
    let control_receiver = Arc::new(Mutex::new(Some(control_receiver)));
    let input = QwenRealtimeVoiceInput {
        sender,
        controls,
        active_response,
        pending_speech: Arc::new(Semaphore::new(PENDING_SPEECH_CAPACITY)),
        context_refresh_supported,
        pending_text: Arc::new(Semaphore::new(32)),
        text_correlation,
        text_correlations,
    };
    let component_input = input.clone();
    let app = Application::mount(
        move || {
            Component::fragment([
                voice_input(
                    receiver.clone(),
                    control_receiver.clone(),
                    provider_input.clone(),
                ),
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
fn voice_input(
    receiver: VoiceReceiver,
    controls: ControlReceiver,
    provider: QwenRealtimeMediaInput,
) -> Component {
    use_future(move || async move {
        let receiver = receiver.lock().expect("voice input receiver mutex").take();
        let Some(mut receiver) = receiver else { return };
        let controls = controls
            .lock()
            .expect("voice control receiver mutex")
            .take();
        let Some(mut controls) = controls else { return };
        let microphone = provider.clone();
        // A paused microphone queue can be full while the provider waits for
        // an old turn's terminal. Keep this Component's text/control pump live
        // independently, so that text can send the cancellation ending the wait.
        tokio::join!(
            async move {
                while let Some(command) = receiver.recv().await {
                    if microphone.send(command).await.is_err() {
                        break;
                    }
                }
            },
            async move {
                while let Some(command) = controls.recv().await {
                    let result = match command.input {
                        Some(input) => provider.send(input).await,
                        None => Ok(()),
                    };
                    let _ = command.forwarded.send(result);
                    if result.is_err() {
                        break;
                    }
                }
            }
        );
    });
    Component::realtime_audio()
}

/// The bounded microphone, text and control input for a voice Application.
#[derive(Clone)]
pub struct QwenRealtimeVoiceInput {
    sender: mpsc::Sender<Input>,
    controls: mpsc::Sender<VoiceControl>,
    active_response: watch::Receiver<Option<String>>,
    pending_speech: Arc<Semaphore>,
    context_refresh_supported: bool,
    pending_text: Arc<Semaphore>,
    text_correlation: watch::Receiver<Option<String>>,
    text_correlations: TextCorrelations,
}

impl QwenRealtimeVoiceInput {
    /// Send an ordinary user message on the mounted Smart Turn session.
    /// The receipt waits for the user item's ACK and its first response identity.
    /// A newer message cancels the preceding generated response, then waits for
    /// that response's terminal before sending this message. Dropping a waiter
    /// never retracts a message already accepted by the input queue.
    pub async fn submit_text(
        &self,
        text: impl Into<String>,
    ) -> Result<QwenRealtimeTextTurn, ReactionPortFault> {
        self.submit_user_text(None, text.into()).await
    }

    /// Retain an opaque application correlation outside model-visible content.
    pub async fn submit_text_with_correlation(
        &self,
        correlation: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<QwenRealtimeTextTurn, ReactionPortFault> {
        self.submit_user_text(Some(correlation.into()), text.into())
            .await
    }

    async fn submit_user_text(
        &self,
        correlation: Option<String>,
        text: String,
    ) -> Result<QwenRealtimeTextTurn, ReactionPortFault> {
        let permit = self.text_permit(&text)?;
        let (request, admission) = TextRequest::acknowledged(text, correlation, true, permit);
        self.send(Input::UserText(request)).await?;
        admission.await.map_err(|_| closed())?
    }

    /// Queue one trusted completion cue. Every cue retains its own FIFO slot
    /// while speech is busy. Raw context_response_started/completed/failed events
    /// carry this correlation and the provider's actual response identity.
    pub async fn queue_context_response(
        &self,
        correlation: impl Into<String>,
        instruction: impl Into<String>,
    ) -> Result<(), ReactionPortFault> {
        let instruction = instruction.into();
        let permit = self.text_permit(&instruction)?;
        let mut request = TextRequest::queued(instruction, permit);
        request.correlation = Some(correlation.into());
        self.send(Input::RefreshContextAndRespond(request)).await
    }

    /// Queue a service-authored reply and receive its acknowledged final result.
    pub async fn respond_with_correlation(
        &self,
        correlation: impl Into<String>,
        instruction: impl Into<String>,
    ) -> Result<QwenRealtimeTextTurn, ReactionPortFault> {
        let instruction = instruction.into();
        let permit = self.text_permit(&instruction)?;
        let (request, admission) =
            TextRequest::acknowledged(instruction, Some(correlation.into()), false, permit);
        self.send(Input::RefreshContextAndRespond(request)).await?;
        admission.await.map_err(|_| closed())?
    }

    /// The formal text selected for the next Frame, available to preparations.
    /// Use text_correlation_for_call for authorization of an actual tool call.
    pub fn current_text_correlation(&self) -> Option<String> {
        self.text_correlation.borrow().clone()
    }

    /// Look up the immutable input owner of an admitted native tool call.
    pub fn text_correlation_for_call(&self, call_id: &str) -> Option<String> {
        self.text_correlations.get(call_id)
    }

    /// Look up the immutable input owner of a provider response, including tools.
    pub fn text_correlation_for_response(&self, response_id: &str) -> Option<String> {
        self.text_correlations.response(response_id)
    }

    fn text_permit(
        &self,
        text: &str,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ReactionPortFault> {
        // The API caller owns each new text. Reject only an unusable command or
        // excess queue entry, preserving all earlier inputs and live microphone.
        if !self.context_refresh_supported
            || text.trim().is_empty()
            || text.len() > MAX_SPOKEN_TEXT_BYTES
        {
            return Err(ReactionPortFault::retryable(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::RequestPreparation,
            ));
        }
        self.pending_text.clone().try_acquire_owned().map_err(|_| {
            ReactionPortFault::retryable(
                ReactionPortFaultCode::Limit,
                ReactionPortFaultReason::RequestPreparation,
            )
        })
    }
    /// Queue a context-only Frame refresh after changing Component Signals.
    ///
    /// Qwen Smart Turn v2 re-renders the mounted tree and submits its current
    /// context through the normal Frame handoff in the same running reaction.
    /// This does not request speech or call `response.create`. A response or
    /// user utterance already underway retains its submitted callbacks; its
    /// normal next Frame carries the update. Success means queued, not remotely
    /// acknowledged. Optional client refusals are observed and omitted without
    /// cancelling speech. Repeated ephemeral developer snapshots are suitable
    /// for asynchronous task state. Other voice providers return a local
    /// unsupported fault.
    pub async fn refresh_context(&self) -> Result<(), ReactionPortFault> {
        self.request_context_refresh(None).await
    }

    /// Queue fresh Component context and one response after its remote ACKs.
    ///
    /// This explicitly opts into generation on Qwen Smart Turn v2, retaining
    /// the current reaction and socket. Active speech finishes first. A user
    /// turn or automatic response that wins the context-installation race
    /// takes priority and skips this optional generation. Native tool feedback
    /// and this intent share a single `response.create` when both are ready.
    /// Success means queued; raw media delivery observations report the result.
    pub async fn refresh_context_and_respond(&self) -> Result<(), ReactionPortFault> {
        self.refresh_context_and_respond_with_instruction(
            "这是一次由服务端主动触发的回复。请依据刚更新的上下文完成本次答复；本指令仅对这一次回复有效。若有新的用户请求，优先回应用户。"
        ).await
    }

    /// Request one reply with a trusted instruction scoped to that delivery.
    /// Qwen receives a service-authored text item after the Frame context,
    /// then acknowledges it before generation. The provider retires the item
    /// when its response ends or if a user turn wins insertion.
    /// Empty or over-64-KiB instructions are refused locally. Mark any
    /// externally sourced content as quoted data within the authored cue.
    pub async fn refresh_context_and_respond_with_instruction(
        &self,
        instruction: impl Into<String>,
    ) -> Result<(), ReactionPortFault> {
        self.request_context_refresh(Some(instruction.into())).await
    }

    async fn request_context_refresh(
        &self,
        instruction: Option<String>,
    ) -> Result<(), ReactionPortFault> {
        // This input type is shared by providers with different session
        // protocols. Reject only the unsupported optional refresh, rather than
        // turning it into speech or disturbing their microphone stream.
        if !self.context_refresh_supported {
            return Err(ReactionPortFault::retryable(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::RequestPreparation,
            ));
        }
        // The application owns this optional cue. Refuse only an unusable new
        // instruction before it can occupy the bounded microphone/control queue.
        if instruction
            .as_ref()
            .is_some_and(|text| text.trim().is_empty() || text.len() > MAX_SPOKEN_TEXT_BYTES)
        {
            return Err(ReactionPortFault::retryable(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::RequestPreparation,
            ));
        }
        self.send(match instruction {
            Some(instruction) => {
                let permit = self.text_permit(&instruction)?;
                Input::RefreshContextAndRespond(TextRequest::queued(instruction, permit))
            }
            None => Input::RefreshContext,
        })
        .await
    }

    /// Stream 16 kHz mono signed little-endian PCM16, including silence.
    pub async fn append_pcm16(&self, pcm16: &[u8]) -> Result<(), ReactionPortFault> {
        self.send(Input::Append(encode_pcm16(pcm16)?)).await
    }

    /// Tell a provider with microphone pause support that PCM delivery is paused or resumed.
    pub async fn set_input_muted(&self, muted: bool) -> Result<(), ReactionPortFault> {
        self.send(Input::SetMuted(muted)).await
    }

    /// Queue a trusted server-authored greeting or notification for spoken output.
    ///
    /// The provider waits for the current reply and any user utterance to
    /// finish. A queued notification is forwarded only after its exchange's
    /// normal Frame setup, without cancelling speech or stopping the microphone.
    /// The returned success means queued, not spoken or acknowledged upstream.
    ///
    /// Seeduplex synthesizes this text directly. Qwen receives an instruction
    /// to read it and generates a response, so verbatim wording is not guaranteed.
    /// Qwen waits for the matching text-item acknowledgement before generation;
    /// a rejected or unacknowledged optional notice is skipped without closing
    /// the voice session. User speech that wins this insertion race takes priority.
    /// This method is for trusted application text, not external user commands.
    /// Empty text, text over 64 KiB, or eight already queued notifications return
    /// a local fault without affecting the running session.
    pub async fn speak_text(&self, text: impl Into<String>) -> Result<(), ReactionPortFault> {
        let text = text.into();
        // The application owns the notification. Reject only an unusable or
        // oversized new request before it could stall the microphone queue.
        if text.trim().is_empty() || text.len() > MAX_SPOKEN_TEXT_BYTES {
            return Err(ReactionPortFault::retryable(
                ReactionPortFaultCode::Rejected,
                ReactionPortFaultReason::RequestPreparation,
            ));
        }
        // Each accepted request retains one permit while queued across provider
        // responses. Refuse only this excess notification, leaving PCM and the
        // already accepted notifications free to advance.
        let permit = self
            .pending_speech
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                ReactionPortFault::retryable(
                    ReactionPortFaultCode::Limit,
                    ReactionPortFaultReason::RequestPreparation,
                )
            })?;
        self.send(Input::Speak(SpokenText::new(text, permit))).await
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
        if matches!(
            input,
            Input::UserText(_) | Input::RefreshContextAndRespond(_)
        ) {
            return self.send_control(Some(input)).await;
        }
        // Finish stays behind all prior microphone packets. First fence the
        // independent control pump, so no accepted text can arrive after the
        // provider decides that its finished input has no pending deliveries.
        if matches!(input, Input::Finish) {
            self.send_control(None).await?;
        }
        self.sender.send(input).await.map_err(|_| closed())
    }

    async fn send_control(&self, input: Option<Input>) -> Result<(), ReactionPortFault> {
        let (forwarded, receipt) = oneshot::channel();
        self.controls
            .send(VoiceControl { input, forwarded })
            .await
            .map_err(|_| closed())?;
        receipt.await.map_err(|_| closed())?
    }
}
