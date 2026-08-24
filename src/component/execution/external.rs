use std::{pin::Pin, vec::IntoIter};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{component::ComponentHost, llm_call::TextTurnEvent};

use super::{
    prompt_render::render_projection_prompt, ApplicationHost, ApplicationHostFault, ProviderEvent,
    ProviderEventStream, ProviderFault, ProviderPort, RenderedProjection,
};

/// Opaque generation of one externally rendered observation baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalRenderingGeneration(u64);

/// Whether an external observation establishes or advances a rendering baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExternalObservationKind {
    Full,
    Delta,
}

/// One external rendering update produced by [`ExternalApplication`].
///
/// The content encoding is intentionally private to the external adapter. Only
/// the Full/Delta generation relationship is part of this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalObservation {
    kind: ExternalObservationKind,
    generation: ExternalRenderingGeneration,
    base_generation: Option<ExternalRenderingGeneration>,
    content: String,
}

impl ExternalObservation {
    pub fn kind(&self) -> ExternalObservationKind {
        self.kind
    }

    pub fn generation(&self) -> ExternalRenderingGeneration {
        self.generation
    }

    pub fn base_generation(&self) -> Option<ExternalRenderingGeneration> {
        self.base_generation
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    fn full(snapshot: &ExternalRenderingSnapshot) -> Self {
        Self {
            kind: ExternalObservationKind::Full,
            generation: snapshot.generation,
            base_generation: None,
            content: snapshot.content.clone(),
        }
    }

    fn delta(baseline: &ExternalRenderingSnapshot, current: &ExternalRenderingSnapshot) -> Self {
        Self {
            kind: ExternalObservationKind::Delta,
            generation: current.generation,
            base_generation: Some(baseline.generation),
            content: render_external_delta(&baseline.content, &current.content),
        }
    }
}

/// Concrete ProviderPort whose model output is supplied by an external caller.
///
/// `observe` and `act` deliberately do not exist on this type. They belong to
/// [`ExternalApplication`], which owns the current reaction.
pub struct ExternalProviderPort {
    observations: mpsc::Sender<ExternalRequest>,
}

impl ExternalProviderPort {
    fn new(observations: mpsc::Sender<ExternalRequest>) -> Self {
        Self { observations }
    }
}

#[async_trait]
impl ProviderPort for ExternalProviderPort {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        let rendered = render_projection_prompt(&projection)?;
        let (input, pending_input) = oneshot::channel();
        self.observations
            .send(ExternalRequest { rendered, input })
            .await
            .map_err(|_| {
                ProviderFault::retryable_transport(
                    "external observation receiver closed before projection delivery",
                )
            })?;

        match pending_input.await.map_err(|_| {
            ProviderFault::retryable_transport(
                "external reaction owner closed before supplying rollover or model output",
            )
        })? {
            ExternalInput::Rollover => Ok(Box::pin(futures::stream::empty())),
            ExternalInput::TextProtocol(protocol) => Ok(external_provider_events(protocol)),
        }
    }
}

struct ExternalRequest {
    rendered: String,
    input: oneshot::Sender<ExternalInput>,
}

enum ExternalInput {
    Rollover,
    TextProtocol(ExternalAct),
}

type ExternalTextProtocolStream =
    Pin<Box<dyn Stream<Item = Result<TextTurnEvent, ProviderFault>> + Send + 'static>>;

const MAX_EXTERNAL_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXTERNAL_PROTOCOL_WIRE_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXTERNAL_PROTOCOL_FRAMES: usize = 65_536;

/// Opaque payload for one complete external act protocol.
///
/// Concrete protocol adapters create this value below the external Port
/// boundary. Core deliberately exposes no generic Stream or normalized-event
/// constructor.
pub struct ExternalAct {
    protocol: ExternalTextProtocolStream,
}

impl ExternalAct {
    /// Decode the JSON-lines text protocol used by the external CLI adapter.
    ///
    /// Each line is one `text_delta`, `text_complete`, or abnormal `disconnect`
    /// frame. End of input is normal protocol EOF. Frames are decoded lazily so
    /// an explicit completion prevents any later frame from being polled.
    #[doc(hidden)]
    pub fn __from_cli_json_lines(protocol: impl Into<String>) -> Self {
        let protocol = protocol.into();
        if protocol.len() > MAX_EXTERNAL_PROTOCOL_WIRE_BYTES {
            return Self {
                protocol: Box::pin(futures::stream::once(async {
                    Err(ProviderFault::retryable_transport(
                        "external text protocol exceeded configured wire limit",
                    ))
                })),
            };
        }
        if protocol
            .lines()
            .take(MAX_EXTERNAL_PROTOCOL_FRAMES + 1)
            .count()
            > MAX_EXTERNAL_PROTOCOL_FRAMES
        {
            return Self {
                protocol: Box::pin(futures::stream::once(async {
                    Err(ProviderFault::retryable_transport(
                        "external text protocol exceeded configured frame limit",
                    ))
                })),
            };
        }
        let lines = protocol
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .into_iter();
        Self {
            protocol: Box::pin(external_json_lines_protocol(lines)),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_text_protocol<S>(protocol: S) -> Self
    where
        S: Stream<Item = Result<TextTurnEvent, ProviderFault>> + Send + 'static,
    {
        Self {
            protocol: Box::pin(protocol),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ExternalJsonLine {
    TextDelta { text: String },
    TextComplete { text: String },
    Disconnect,
}

fn external_json_lines_protocol(
    lines: IntoIter<String>,
) -> impl Stream<Item = Result<TextTurnEvent, ProviderFault>> + Send + 'static {
    futures::stream::unfold((lines, 0_usize), |(mut lines, frames)| async move {
        let line = lines.next()?;
        if frames >= MAX_EXTERNAL_PROTOCOL_FRAMES {
            return Some((
                Err(ProviderFault::retryable_transport(
                    "external text protocol exceeded configured frame limit",
                )),
                (Vec::new().into_iter(), frames),
            ));
        }
        let event = match serde_json::from_str::<ExternalJsonLine>(&line) {
            Ok(ExternalJsonLine::TextDelta { text }) => Ok(TextTurnEvent::TextDelta(text)),
            Ok(ExternalJsonLine::TextComplete { text }) => Ok(TextTurnEvent::TextComplete(text)),
            Ok(ExternalJsonLine::Disconnect) => Err(ProviderFault::retryable_transport(
                "external text protocol disconnected abnormally",
            )),
            Err(_) => Err(ProviderFault::retryable_transport(
                "external text protocol frame was invalid",
            )),
        };
        Some((event, (lines, frames + 1)))
    })
}

struct ExternalProtocolState {
    protocol: ExternalTextProtocolStream,
    accumulated: String,
    complete: bool,
}

fn external_provider_events(act: ExternalAct) -> ProviderEventStream<'static> {
    Box::pin(futures::stream::unfold(
        ExternalProtocolState {
            protocol: act.protocol,
            accumulated: String::new(),
            complete: false,
        },
        |mut state| async move {
            if state.complete {
                return None;
            }

            let event = match state.protocol.next().await {
                Some(Ok(TextTurnEvent::TextDelta(delta))) => {
                    if exceeds_external_text_limit(state.accumulated.len(), delta.len()) {
                        state.complete = true;
                        Err(external_text_limit_fault())
                    } else {
                        state.accumulated.push_str(&delta);
                        Ok(ProviderEvent::Text(TextTurnEvent::TextDelta(delta)))
                    }
                }
                Some(Ok(TextTurnEvent::TextComplete(complete))) => {
                    state.complete = true;
                    if complete.len() > MAX_EXTERNAL_TEXT_BYTES {
                        Err(external_text_limit_fault())
                    } else {
                        Ok(ProviderEvent::Text(TextTurnEvent::TextComplete(complete)))
                    }
                }
                Some(Err(fault)) => {
                    state.complete = true;
                    Err(fault)
                }
                None => {
                    state.complete = true;
                    Ok(ProviderEvent::Text(TextTurnEvent::TextComplete(
                        std::mem::take(&mut state.accumulated),
                    )))
                }
            };
            Some((event, state))
        },
    ))
}

fn exceeds_external_text_limit(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .is_none_or(|total| total > MAX_EXTERNAL_TEXT_BYTES)
}

fn external_text_limit_fault() -> ProviderFault {
    ProviderFault::retryable_transport("external output text exceeded configured output limit")
}

#[derive(Clone)]
struct ExternalRenderingSnapshot {
    generation: ExternalRenderingGeneration,
    content: String,
}

struct ExternalOwner<Props> {
    host: ApplicationHost<ExternalProviderPort>,
    components: ComponentHost<Props>,
}

enum ExternalReactionState {
    AwaitingObservation,
    AwaitingInput(oneshot::Sender<ExternalInput>),
    Finishing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExternalReactionPhase {
    AwaitingObservation,
    AwaitingInput,
    Finishing,
}

impl ExternalReactionState {
    fn phase(&self) -> ExternalReactionPhase {
        match self {
            Self::AwaitingObservation => ExternalReactionPhase::AwaitingObservation,
            Self::AwaitingInput(_) => ExternalReactionPhase::AwaitingInput,
            Self::Finishing => ExternalReactionPhase::Finishing,
        }
    }
}

struct ExternalReaction<Props> {
    state: ExternalReactionState,
    cancellation: Option<oneshot::Sender<()>>,
    task: JoinHandle<ExternalReactionCompletion<Props>>,
}

impl<Props> Drop for ExternalReaction<Props> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ExternalReactionCompletion<Props> {
    owner: ExternalOwner<Props>,
    outcome: ExternalReactionOutcome,
}

enum ExternalReactionOutcome {
    Finished(Result<(), ApplicationHostFault>),
    Cancelled,
}

enum ExternalReactionProgress<Props> {
    Request(Option<ExternalRequest>),
    Completion(Result<ExternalReactionCompletion<Props>, tokio::task::JoinError>),
}

struct ExternalReactionCancellation {
    cancellation: Option<oneshot::Sender<()>>,
}

impl ExternalReactionCancellation {
    fn new(cancellation: oneshot::Sender<()>) -> Self {
        Self {
            cancellation: Some(cancellation),
        }
    }

    fn disarm(&mut self) {
        self.cancellation = None;
    }

    fn take(&mut self) -> Option<oneshot::Sender<()>> {
        self.cancellation.take()
    }
}

impl Drop for ExternalReactionCancellation {
    fn drop(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            let _ = cancellation.send(());
        }
    }
}

/// External-only wrapper that owns one current Provider reaction.
///
/// Ordinary `observe` and `act` roll over through normal Provider EOF. `act`
/// first supplies one complete text protocol stream. Full re-render only
/// rebuilds the external rendering baseline and leaves the reaction pending.
pub struct ExternalApplication<Props> {
    observations: mpsc::Receiver<ExternalRequest>,
    owner: Option<ExternalOwner<Props>>,
    reaction: Option<ExternalReaction<Props>>,
    baseline: Option<ExternalRenderingSnapshot>,
    current_rendering: Option<ExternalRenderingSnapshot>,
    next_generation: u64,
}

impl<Props> ExternalApplication<Props>
where
    Props: Clone + Send + 'static,
{
    pub fn new(components: ComponentHost<Props>) -> Self {
        let (observations, pending_observations) = mpsc::channel(1);
        let provider = ExternalProviderPort::new(observations);
        Self {
            observations: pending_observations,
            owner: Some(ExternalOwner {
                host: ApplicationHost::new(provider),
                components,
            }),
            reaction: None,
            baseline: None,
            current_rendering: None,
            next_generation: 1,
        }
    }

    /// Finish the current reaction, if any, and return the next observation.
    pub async fn observe(&mut self) -> Result<ExternalObservation, ExternalApplicationFault> {
        match self.current_reaction_phase() {
            Some(ExternalReactionPhase::AwaitingInput) => {
                self.finish_current(ExternalInput::Rollover).await?;
            }
            Some(ExternalReactionPhase::AwaitingObservation | ExternalReactionPhase::Finishing) => {
                self.recover_cancelled_reaction().await?;
            }
            None => {}
        }
        self.start_next().await
    }

    /// Supply one complete external text protocol stream, then observe next.
    pub async fn act(
        &mut self,
        act: ExternalAct,
    ) -> Result<ExternalObservation, ExternalApplicationFault> {
        if self.current_reaction_phase() != Some(ExternalReactionPhase::AwaitingInput) {
            return Err(ExternalApplicationFault::NoActiveReaction);
        }
        self.finish_current(ExternalInput::TextProtocol(act))
            .await?;
        self.start_next().await
    }

    /// Re-emit a Full rendering for the current reaction without rolling it over.
    pub fn full_re_render(&mut self) -> Result<ExternalObservation, ExternalApplicationFault> {
        if self.current_reaction_phase() != Some(ExternalReactionPhase::AwaitingInput) {
            return Err(ExternalApplicationFault::NoActiveReaction);
        }
        let current = self
            .current_rendering
            .clone()
            .ok_or(ExternalApplicationFault::SynchronizationClosed)?;
        self.baseline = Some(current.clone());
        Ok(ExternalObservation::full(&current))
    }

    async fn finish_current(
        &mut self,
        input: ExternalInput,
    ) -> Result<(), ExternalApplicationFault> {
        let (input_sender, cancellation) = {
            let reaction = self
                .reaction
                .as_mut()
                .ok_or(ExternalApplicationFault::NoActiveReaction)?;
            let input_sender =
                match std::mem::replace(&mut reaction.state, ExternalReactionState::Finishing) {
                    ExternalReactionState::AwaitingInput(input) => input,
                    state => {
                        reaction.state = state;
                        return Err(ExternalApplicationFault::NoActiveReaction);
                    }
                };
            let cancellation = reaction
                .cancellation
                .take()
                .ok_or(ExternalApplicationFault::SynchronizationClosed)?;
            (input_sender, cancellation)
        };
        self.current_rendering = None;
        let input_delivered = input_sender.send(input).is_ok();
        let mut cancellation = ExternalReactionCancellation::new(cancellation);
        let completion = self.await_current_completion().await;
        cancellation.disarm();
        let completion = match completion {
            Ok(completion) => completion,
            Err(fault) => return Err(self.consume_failed_reaction(fault)),
        };
        match self.restore_completed_reaction(completion)? {
            ExternalReactionOutcome::Finished(result) => result?,
            ExternalReactionOutcome::Cancelled => {}
        }
        if !input_delivered {
            return Err(ExternalApplicationFault::SynchronizationClosed);
        }
        Ok(())
    }

    async fn start_next(&mut self) -> Result<ExternalObservation, ExternalApplicationFault> {
        let owner = self
            .owner
            .take()
            .ok_or(ExternalApplicationFault::OwnerUnavailable)?;
        let (cancellation, cancelled) = oneshot::channel();
        self.reaction = Some(ExternalReaction {
            state: ExternalReactionState::AwaitingObservation,
            cancellation: Some(cancellation),
            task: tokio::spawn(run_reaction(owner, cancelled)),
        });
        self.await_next_observation().await
    }

    async fn await_next_observation(
        &mut self,
    ) -> Result<ExternalObservation, ExternalApplicationFault> {
        let cancellation = self
            .reaction
            .as_mut()
            .and_then(|reaction| reaction.cancellation.take())
            .ok_or(ExternalApplicationFault::SynchronizationClosed)?;
        let mut cancellation = ExternalReactionCancellation::new(cancellation);

        loop {
            let progress = {
                let observations = &mut self.observations;
                let reaction = self
                    .reaction
                    .as_mut()
                    .ok_or(ExternalApplicationFault::NoActiveReaction)?;
                tokio::select! {
                    request = observations.recv() => ExternalReactionProgress::Request(request),
                    completion = &mut reaction.task => {
                        ExternalReactionProgress::Completion(completion)
                    }
                }
            };

            match progress {
                ExternalReactionProgress::Request(Some(request)) => {
                    if request.input.is_closed() {
                        continue;
                    }
                    let observation = self.publish_rendering(request.rendered)?;
                    let cancellation = cancellation
                        .take()
                        .ok_or(ExternalApplicationFault::SynchronizationClosed)?;
                    let reaction = self
                        .reaction
                        .as_mut()
                        .ok_or(ExternalApplicationFault::NoActiveReaction)?;
                    reaction.state = ExternalReactionState::AwaitingInput(request.input);
                    reaction.cancellation = Some(cancellation);
                    return Ok(observation);
                }
                ExternalReactionProgress::Request(None) => {
                    return Err(ExternalApplicationFault::SynchronizationClosed);
                }
                ExternalReactionProgress::Completion(completion) => {
                    cancellation.disarm();
                    let completion = match completion {
                        Ok(completion) => completion,
                        Err(fault) => return Err(self.consume_failed_reaction(fault)),
                    };
                    return match self.restore_completed_reaction(completion)? {
                        ExternalReactionOutcome::Finished(Ok(()))
                        | ExternalReactionOutcome::Cancelled => {
                            Err(ExternalApplicationFault::ReactionEndedBeforeObservation)
                        }
                        ExternalReactionOutcome::Finished(Err(fault)) => Err(fault.into()),
                    };
                }
            }
        }
    }

    async fn recover_cancelled_reaction(&mut self) -> Result<(), ExternalApplicationFault> {
        let phase = self
            .current_reaction_phase()
            .ok_or(ExternalApplicationFault::NoActiveReaction)?;
        let completion = match self.await_current_completion().await {
            Ok(completion) => completion,
            Err(fault) => return Err(self.consume_failed_reaction(fault)),
        };
        match self.restore_completed_reaction(completion)? {
            ExternalReactionOutcome::Cancelled => Ok(()),
            ExternalReactionOutcome::Finished(result)
                if phase == ExternalReactionPhase::Finishing =>
            {
                result.map_err(Into::into)
            }
            ExternalReactionOutcome::Finished(Ok(())) => {
                Err(ExternalApplicationFault::ReactionEndedBeforeObservation)
            }
            ExternalReactionOutcome::Finished(Err(fault)) => Err(fault.into()),
        }
    }

    async fn await_current_completion(
        &mut self,
    ) -> Result<ExternalReactionCompletion<Props>, tokio::task::JoinError> {
        let reaction = self
            .reaction
            .as_mut()
            .expect("current reaction was checked before awaiting completion");
        (&mut reaction.task).await
    }

    fn restore_completed_reaction(
        &mut self,
        completion: ExternalReactionCompletion<Props>,
    ) -> Result<ExternalReactionOutcome, ExternalApplicationFault> {
        let finished_reaction = self
            .reaction
            .take()
            .ok_or(ExternalApplicationFault::NoActiveReaction)?;
        drop(finished_reaction);
        self.owner = Some(completion.owner);
        Ok(completion.outcome)
    }

    fn current_reaction_phase(&self) -> Option<ExternalReactionPhase> {
        self.reaction
            .as_ref()
            .map(|reaction| reaction.state.phase())
    }

    fn consume_failed_reaction(
        &mut self,
        fault: tokio::task::JoinError,
    ) -> ExternalApplicationFault {
        drop(self.reaction.take());
        ExternalApplicationFault::reaction_task(fault)
    }

    fn publish_rendering(
        &mut self,
        content: String,
    ) -> Result<ExternalObservation, ExternalApplicationFault> {
        let generation = ExternalRenderingGeneration(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(ExternalApplicationFault::GenerationExhausted)?;
        let current = ExternalRenderingSnapshot {
            generation,
            content,
        };
        let observation = self.baseline.as_ref().map_or_else(
            || ExternalObservation::full(&current),
            |baseline| ExternalObservation::delta(baseline, &current),
        );
        self.baseline = Some(current.clone());
        self.current_rendering = Some(current);
        Ok(observation)
    }
}

async fn run_reaction<Props>(
    mut owner: ExternalOwner<Props>,
    cancellation: oneshot::Receiver<()>,
) -> ExternalReactionCompletion<Props>
where
    Props: Clone + Send + 'static,
{
    let outcome = {
        let reaction = owner.host.dispatch_llm_reaction(&mut owner.components);
        tokio::pin!(reaction);
        tokio::select! {
            result = &mut reaction => ExternalReactionOutcome::Finished(result),
            cancelled = cancellation => {
                match cancelled {
                    Ok(()) => ExternalReactionOutcome::Cancelled,
                    Err(_) => ExternalReactionOutcome::Finished(reaction.await),
                }
            }
        }
    };
    ExternalReactionCompletion { owner, outcome }
}

fn render_external_delta(baseline: &str, current: &str) -> String {
    if baseline == current {
        String::new()
    } else {
        current.to_owned()
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExternalApplicationFault {
    #[error("external command requires a current reaction; call observe first")]
    NoActiveReaction,
    #[error("external reaction synchronization closed unexpectedly")]
    SynchronizationClosed,
    #[error("external reaction ended before publishing an observation")]
    ReactionEndedBeforeObservation,
    #[error("external application owner is unavailable after an internal task failure")]
    OwnerUnavailable,
    #[error("external reaction task failed: {message}")]
    ReactionTaskFailed { message: String },
    #[error("external rendering generation space is exhausted")]
    GenerationExhausted,
    #[error(transparent)]
    Application(#[from] ApplicationHostFault),
}

impl ExternalApplicationFault {
    fn reaction_task(fault: tokio::task::JoinError) -> Self {
        Self::ReactionTaskFailed {
            message: fault.to_string(),
        }
    }
}

#[cfg(test)]
mod tests;
