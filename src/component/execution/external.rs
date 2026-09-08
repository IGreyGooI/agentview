use std::{
    future::Future,
    num::{NonZeroU64, NonZeroU128},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    vec::IntoIter,
};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    component::authoring::{Component, InternalEventInput as EventInput},
    llm_call::TextTurnEvent,
};

use super::{
    ProviderEvent, ProviderFault,
    application::{Application, ApplicationFault},
    reaction::{
        Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
        ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultReason, SubmitFault, TargetDeclaration,
        TargetEpoch, TargetIdentity,
    },
};

static NEXT_EXTERNAL_TARGET_ID: AtomicU64 = AtomicU64::new(1);

const EXTERNAL_TARGET_ID_DOMAIN: u128 = 3_u128 << 64;
const EXTERNAL_FRAME_QUEUE_CAPACITY: usize = 1;
const EXTERNAL_FACT_QUEUE_CAPACITY: usize = 32;
const EXTERNAL_MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const EXTERNAL_MAX_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_EXTERNAL_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXTERNAL_PROTOCOL_WIRE_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXTERNAL_PROTOCOL_FRAMES: usize = 65_536;

/// Compatibility rendering lineage backed directly by a Frame revision.
///
/// External no longer owns a second string-rendering generation or baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalRenderingGeneration(FrameRevision);

impl ExternalRenderingGeneration {
    pub const fn revision(self) -> FrameRevision {
        self.0
    }
}

/// Reaction-local authority for one external act stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalIngressGeneration(NonZeroU64);

impl ExternalIngressGeneration {
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

/// Whether an exact external Frame is Full or based on an accepted revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExternalObservationKind {
    Full,
    Delta,
}

/// One exact Frame accepted by the external integration boundary.
#[derive(Debug)]
pub struct ExternalObservation {
    frame: Frame,
    ingress: ExternalIngressGeneration,
}

impl ExternalObservation {
    pub fn kind(&self) -> ExternalObservationKind {
        match self.frame.basis() {
            FrameBasis::Full => ExternalObservationKind::Full,
            FrameBasis::DeltaFrom(_) => ExternalObservationKind::Delta,
        }
    }

    pub fn generation(&self) -> ExternalRenderingGeneration {
        ExternalRenderingGeneration(self.frame.revision())
    }

    pub fn base_generation(&self) -> Option<ExternalRenderingGeneration> {
        match self.frame.basis() {
            FrameBasis::Full => None,
            FrameBasis::DeltaFrom(revision) => Some(ExternalRenderingGeneration(revision)),
        }
    }

    pub const fn ingress_generation(&self) -> ExternalIngressGeneration {
        self.ingress
    }

    /// Exact move-only Frame handed off by the private FrameSession.
    pub const fn frame(&self) -> &Frame {
        &self.frame
    }

    pub fn into_frame(self) -> Frame {
        self.frame
    }

    /// Compatibility text view of the exact canonical Frame submission.
    pub fn content(&self) -> &str {
        std::str::from_utf8(self.frame.submission().canonical_bytes())
            .expect("Frame compiler always produces UTF-8 canonical JSON")
    }
}

struct ExternalFrameRequest {
    frame: Frame,
    ingress: ExternalIngressGeneration,
}

struct ExternalIngressState {
    generation: ExternalIngressGeneration,
    sender: Option<mpsc::Sender<Result<ProviderFact, ReactionPortFault>>>,
}

struct ExternalTargetState {
    identity: TargetIdentity,
    epoch: TargetEpoch,
    accepted: Option<FrameRevision>,
    profile: FrameProfile,
    terminal: Option<ReactionPortFault>,
}

impl ExternalTargetState {
    fn declaration(&self) -> Result<TargetDeclaration, ReactionPortFault> {
        if let Some(fault) = self.terminal {
            return Err(fault);
        }
        Ok(self.accepted.map_or_else(
            || TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
            |revision| TargetDeclaration::resume(revision, self.profile.clone()),
        ))
    }

    fn reset_continuity(&mut self) -> Result<(), ReactionPortFault> {
        if let Some(fault) = self.terminal {
            return Err(fault);
        }
        let Some(next) = self
            .epoch
            .get()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
        else {
            let fault = external_terminal_fault(
                ReactionPortFaultCode::Internal,
                ReactionPortFaultReason::Declaration,
            );
            self.terminal = Some(fault);
            return Err(fault);
        };
        self.epoch = TargetEpoch::new(next);
        self.accepted = None;
        Ok(())
    }
}

struct ExternalSharedState {
    target: ExternalTargetState,
    ingress: Option<ExternalIngressState>,
    next_ingress: Option<NonZeroU64>,
    control_alive: bool,
}

impl ExternalSharedState {
    fn allocate_ingress(&mut self) -> Result<ExternalIngressGeneration, ReactionPortFault> {
        let current = match self.next_ingress {
            Some(current) => current,
            None => {
                let fault = external_terminal_fault(
                    ReactionPortFaultCode::Internal,
                    ReactionPortFaultReason::Declaration,
                );
                self.target.terminal = Some(fault);
                return Err(fault);
            }
        };
        self.next_ingress = current.get().checked_add(1).and_then(NonZeroU64::new);
        Ok(ExternalIngressGeneration(current))
    }
}

struct ExternalControlInner {
    frames: tokio::sync::Mutex<mpsc::Receiver<ExternalFrameRequest>>,
    shared: Arc<Mutex<ExternalSharedState>>,
}

/// Cloneable, reaction-fenced control plane for one external target.
///
/// It can receive accepted Frames and inject an act for the matching ingress
/// generation. It cannot submit Frames or mutate canonical history.
#[derive(Clone)]
pub struct ExternalControl {
    inner: Arc<ExternalControlInner>,
}

impl Drop for ExternalControlInner {
    fn drop(&mut self) {
        let (sender, terminal) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.control_alive = false;
            let terminal = shared.target.terminal.unwrap_or_else(|| {
                external_terminal_fault(
                    ReactionPortFaultCode::Unavailable,
                    ReactionPortFaultReason::Declaration,
                )
            });
            shared.target.terminal = Some(terminal);
            let sender = shared
                .ingress
                .as_mut()
                .and_then(|ingress| ingress.sender.take());
            (sender, terminal)
        };
        if let Some(sender) = sender {
            let _ = sender.try_send(Err(terminal));
        }
    }
}

impl ExternalControl {
    /// Wait for one exact Frame already accepted by the outbound queue.
    pub async fn next_observation(&self) -> Result<ExternalObservation, ExternalControlFault> {
        let request = self
            .inner
            .frames
            .lock()
            .await
            .recv()
            .await
            .ok_or(ExternalControlFault::ObservationChannelClosed)?;
        Ok(ExternalObservation {
            frame: request.frame,
            ingress: request.ingress,
        })
    }

    /// Inject exactly one ordered external text protocol into an active reaction.
    pub async fn act(
        &self,
        generation: ExternalIngressGeneration,
        mut act: ExternalAct,
    ) -> Result<(), ExternalControlFault> {
        let sender = self.claim_ingress(generation)?;
        let output = ProviderOutputKey::new(0);
        let mut accumulated = String::new();

        while let Some(event) = act.protocol.next().await {
            match event {
                Ok(TextTurnEvent::TextDelta(delta)) => {
                    if exceeds_external_text_limit(accumulated.len(), delta.len()) {
                        return send_external_fault(sender, external_output_limit_fault()).await;
                    }
                    accumulated.push_str(&delta);
                    send_external_fact(
                        &sender,
                        ProviderFact::TextDelta {
                            output,
                            phase: None,
                            delta,
                        },
                    )
                    .await?;
                }
                Ok(TextTurnEvent::TextComplete(text)) => {
                    if text.len() > MAX_EXTERNAL_TEXT_BYTES {
                        return send_external_fault(sender, external_output_limit_fault()).await;
                    }
                    send_external_fact(
                        &sender,
                        ProviderFact::TextSealed {
                            output,
                            phase: None,
                            text,
                        },
                    )
                    .await?;
                    send_external_fact(
                        &sender,
                        ProviderFact::ReactionCompleted {
                            primary_text: Some(output),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                Err(_) => {
                    return send_external_fault(
                        sender,
                        external_retryable_fault(
                            ReactionPortFaultCode::Protocol,
                            ReactionPortFaultReason::StreamTransport,
                        ),
                    )
                    .await;
                }
            }
        }

        send_external_fact(
            &sender,
            ProviderFact::TextSealed {
                output,
                phase: None,
                text: accumulated,
            },
        )
        .await?;
        send_external_fact(
            &sender,
            ProviderFact::ReactionCompleted {
                primary_text: Some(output),
            },
        )
        .await
    }

    /// Complete an active reaction without producing a primary text output.
    ///
    /// The matching ingress generation can be claimed exactly once. Repeated
    /// or late completion attempts return [`ExternalControlFault::StaleIngress`].
    pub async fn complete(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<(), ExternalControlFault> {
        let sender = self.claim_ingress(generation)?;
        send_external_fact(
            &sender,
            ProviderFact::ReactionCompleted { primary_text: None },
        )
        .await
    }

    fn claim_ingress(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<mpsc::Sender<Result<ProviderFact, ReactionPortFault>>, ExternalControlFault> {
        let mut shared = self
            .inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(ingress) = shared.ingress.as_mut() else {
            return Err(ExternalControlFault::StaleIngress);
        };
        if ingress.generation != generation {
            return Err(ExternalControlFault::StaleIngress);
        }
        ingress
            .sender
            .take()
            .ok_or(ExternalControlFault::StaleIngress)
    }

    fn is_active(&self, generation: ExternalIngressGeneration) -> bool {
        self.inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ingress
            .as_ref()
            .is_some_and(|ingress| ingress.generation == generation)
    }

    fn reset_continuity(&self) -> Result<(), ExternalControlFault> {
        let mut shared = self
            .inner
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shared.ingress.is_some() {
            return Err(ExternalControlFault::ActiveIngress);
        }
        shared
            .target
            .reset_continuity()
            .map_err(|_| ExternalControlFault::ContinuityUnavailable)
    }
}

async fn send_external_fact(
    sender: &mpsc::Sender<Result<ProviderFact, ReactionPortFault>>,
    fact: ProviderFact,
) -> Result<(), ExternalControlFault> {
    sender
        .send(Ok(fact))
        .await
        .map_err(|_| ExternalControlFault::StaleIngress)
}

async fn send_external_fault(
    sender: mpsc::Sender<Result<ProviderFact, ReactionPortFault>>,
    fault: ReactionPortFault,
) -> Result<(), ExternalControlFault> {
    sender
        .send(Err(fault))
        .await
        .map_err(|_| ExternalControlFault::StaleIngress)
}

/// Frame-native external target. Create its control handle before moving the
/// port into an Application.
pub struct ExternalProviderPort {
    frames: mpsc::Sender<ExternalFrameRequest>,
    shared: Arc<Mutex<ExternalSharedState>>,
    #[cfg(test)]
    submit_barrier: Option<Arc<ExternalSubmitBarrier>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ExternalSubmitBarrier {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
impl ExternalSubmitBarrier {
    async fn hold_after_reserve(&self) {
        self.reached.notify_one();
        self.release.notified().await;
    }
}

impl ExternalProviderPort {
    pub fn new() -> Result<(Self, ExternalControl), ReactionPortFault> {
        let instance = NEXT_EXTERNAL_TARGET_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                external_terminal_fault(
                    ReactionPortFaultCode::Internal,
                    ReactionPortFaultReason::Declaration,
                )
            })?;
        let identity = TargetIdentity::new(
            NonZeroU128::new(EXTERNAL_TARGET_ID_DOMAIN | u128::from(instance))
                .expect("External target domain is non-zero"),
        );
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: EXTERNAL_MAX_FRAME_BYTES,
                max_component_bytes: EXTERNAL_MAX_COMPONENT_BYTES,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        );
        let shared = Arc::new(Mutex::new(ExternalSharedState {
            target: ExternalTargetState {
                identity,
                epoch: TargetEpoch::new(NonZeroU64::MIN),
                accepted: None,
                profile,
                terminal: None,
            },
            ingress: None,
            next_ingress: Some(NonZeroU64::MIN),
            control_alive: true,
        }));
        let (frames, pending_frames) = mpsc::channel(EXTERNAL_FRAME_QUEUE_CAPACITY);
        let control = ExternalControl {
            inner: Arc::new(ExternalControlInner {
                frames: tokio::sync::Mutex::new(pending_frames),
                shared: Arc::clone(&shared),
            }),
        };
        Ok((
            Self {
                frames,
                shared,
                #[cfg(test)]
                submit_barrier: None,
            },
            control,
        ))
    }
}

#[async_trait]
impl ReactionPort for ExternalProviderPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target
            .declaration()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let permit = match self.frames.clone().reserve_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                let shared = self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let fault = shared.target.declaration().err().unwrap_or_else(|| {
                    external_terminal_fault(
                        ReactionPortFaultCode::Unavailable,
                        ReactionPortFaultReason::Declaration,
                    )
                });
                return Err(SubmitFault::Rejected(fault));
            }
        };

        #[cfg(test)]
        if let Some(barrier) = self.submit_barrier.as_ref() {
            barrier.hold_after_reserve().await;
        }

        let (ingress, facts) = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !shared.control_alive {
                let fault = shared.target.declaration().err().unwrap_or_else(|| {
                    external_terminal_fault(
                        ReactionPortFaultCode::Unavailable,
                        ReactionPortFaultReason::Declaration,
                    )
                });
                return Err(SubmitFault::Rejected(fault));
            }
            let declaration = shared.target.declaration().map_err(SubmitFault::Rejected)?;
            frame.check_handoff_precondition(&declaration)?;
            if shared.ingress.is_some() {
                return Err(SubmitFault::Rejected(external_terminal_fault(
                    ReactionPortFaultCode::Internal,
                    ReactionPortFaultReason::Declaration,
                )));
            }
            let ingress = shared.allocate_ingress().map_err(SubmitFault::Rejected)?;
            let (facts, pending_facts) = mpsc::channel(EXTERNAL_FACT_QUEUE_CAPACITY);
            shared.target.accepted = Some(frame.revision());
            shared.ingress = Some(ExternalIngressState {
                generation: ingress,
                sender: Some(facts),
            });
            (ingress, pending_facts)
        };

        // The permit makes this send infallible and non-suspending. This is the
        // crossing poll: queue ownership and Ready(Ok(stream)) are linearized.
        permit.send(ExternalFrameRequest { frame, ingress });
        Ok(external_fact_stream(
            facts,
            Arc::clone(&self.shared),
            ingress,
        ))
    }
}

struct ExternalFactStreamGuard {
    shared: Arc<Mutex<ExternalSharedState>>,
    generation: ExternalIngressGeneration,
    clean_eof: bool,
    continuity_invalidated: bool,
}

impl ExternalFactStreamGuard {
    fn invalidate_continuity(&mut self) -> Option<ReactionPortFault> {
        if self.continuity_invalidated {
            return self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .target
                .terminal;
        }
        self.continuity_invalidated = true;
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(fault) = shared.target.terminal {
            return Some(fault);
        }
        let _ = shared.target.reset_continuity();
        shared.target.terminal
    }
}

impl Drop for ExternalFactStreamGuard {
    fn drop(&mut self) {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shared
            .ingress
            .as_ref()
            .is_some_and(|ingress| ingress.generation == self.generation)
        {
            shared.ingress = None;
        }
        if !self.clean_eof && !self.continuity_invalidated {
            let _ = shared.target.reset_continuity();
        }
    }
}

struct ExternalFactStreamState {
    facts: mpsc::Receiver<Result<ProviderFact, ReactionPortFault>>,
    terminal_seen: bool,
    fault_seen: bool,
    guard: ExternalFactStreamGuard,
}

fn external_fact_stream(
    facts: mpsc::Receiver<Result<ProviderFact, ReactionPortFault>>,
    shared: Arc<Mutex<ExternalSharedState>>,
    generation: ExternalIngressGeneration,
) -> ProviderFactStream<'static> {
    Box::pin(futures::stream::unfold(
        ExternalFactStreamState {
            facts,
            terminal_seen: false,
            fault_seen: false,
            guard: ExternalFactStreamGuard {
                shared,
                generation,
                clean_eof: false,
                continuity_invalidated: false,
            },
        },
        |mut state| async move {
            match state.facts.recv().await {
                Some(Ok(fact)) => {
                    if matches!(fact, ProviderFact::ReactionCompleted { .. }) {
                        state.terminal_seen = true;
                    }
                    Some((Ok(fact), state))
                }
                Some(Err(fault)) => {
                    state.fault_seen = true;
                    let _ = state.guard.invalidate_continuity();
                    Some((Err(fault), state))
                }
                None => {
                    if state.terminal_seen {
                        state.guard.clean_eof = true;
                        drop(state);
                        None
                    } else if state.fault_seen {
                        drop(state);
                        None
                    } else {
                        let terminal = state.guard.invalidate_continuity();
                        state.fault_seen = true;
                        Some((
                            Err(terminal.unwrap_or_else(|| {
                                external_retryable_fault(
                                    ReactionPortFaultCode::Unavailable,
                                    ReactionPortFaultReason::StreamTransport,
                                )
                            })),
                            state,
                        ))
                    }
                }
            }
        },
    ))
}

type ExternalTextProtocolStream =
    Pin<Box<dyn Stream<Item = Result<TextTurnEvent, ProviderFault>> + Send + 'static>>;

/// Opaque payload for one complete external text protocol.
pub struct ExternalAct {
    protocol: ExternalTextProtocolStream,
}

impl ExternalAct {
    /// Construct one normal, complete external text response.
    ///
    /// Injection uses the same output-size limit, one-shot ingress claim, and
    /// sealed-text terminal grammar as every other external text act.
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            protocol: Box::pin(futures::stream::once(async move {
                Ok::<_, ProviderFault>(TextTurnEvent::TextComplete(text))
            })),
        }
    }

    /// Decode the JSON-lines protocol used by the external CLI adapter.
    #[doc(hidden)]
    pub fn __from_cli_json_lines(protocol: impl Into<String>) -> Self {
        let protocol = protocol.into();
        if protocol.len() > MAX_EXTERNAL_PROTOCOL_WIRE_BYTES {
            return Self::fault(ProviderFault::retryable_transport(
                "external text protocol exceeded configured wire limit",
            ));
        }
        if protocol
            .lines()
            .take(MAX_EXTERNAL_PROTOCOL_FRAMES + 1)
            .count()
            > MAX_EXTERNAL_PROTOCOL_FRAMES
        {
            return Self::fault(ProviderFault::retryable_transport(
                "external text protocol exceeded configured frame limit",
            ));
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

    fn fault(fault: ProviderFault) -> Self {
        Self {
            protocol: Box::pin(futures::stream::once(async move { Err(fault) })),
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

fn exceeds_external_text_limit(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .is_none_or(|total| total > MAX_EXTERNAL_TEXT_BYTES)
}

fn external_output_limit_fault() -> ReactionPortFault {
    external_retryable_fault(
        ReactionPortFaultCode::Limit,
        ReactionPortFaultReason::OutputLimit,
    )
}

fn external_retryable_fault(
    code: ReactionPortFaultCode,
    reason: ReactionPortFaultReason,
) -> ReactionPortFault {
    ReactionPortFault::retryable(code, reason)
}

fn external_terminal_fault(
    code: ReactionPortFaultCode,
    reason: ReactionPortFaultReason,
) -> ReactionPortFault {
    ReactionPortFault::terminal(code, reason)
}

struct ExternalOwner {
    application: Application<ExternalProviderPort>,
}

enum ExternalReactionState {
    AwaitingObservation,
    AwaitingInput(ExternalIngressGeneration),
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

struct ExternalReaction {
    state: ExternalReactionState,
    cancellation: Option<oneshot::Sender<()>>,
    task: JoinHandle<ExternalReactionCompletion>,
}

impl Drop for ExternalReaction {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ExternalReactionCompletion {
    owner: ExternalOwner,
    outcome: ExternalReactionOutcome,
}

enum ExternalReactionOutcome {
    Finished(Result<(), ApplicationFault>),
    Cancelled,
}

enum ExternalReactionProgress {
    Request(Box<Result<ExternalObservation, ExternalControlFault>>),
    Completion(Box<Result<ExternalReactionCompletion, tokio::task::JoinError>>),
}

enum ExternalReactionFinishProgress {
    Injection(Result<(), ExternalControlFault>),
    Completion(Box<Result<ExternalReactionCompletion, tokio::task::JoinError>>),
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

/// Compatibility scheduler around the private Frame-driven Application.
///
/// Each `observe` or `act` completes at most one prior ingress and explicitly
/// starts one next reaction. Rendering dirtiness never starts a reaction.
pub struct ExternalApplication {
    control: ExternalControl,
    owner: Option<ExternalOwner>,
    reaction: Option<ExternalReaction>,
}

impl ExternalApplication {
    pub fn new_root(
        root: impl Fn() -> Component + Send + Sync + 'static,
    ) -> Result<Self, ExternalApplicationFault> {
        let (port, control) =
            ExternalProviderPort::new().map_err(|_| ExternalApplicationFault::TargetUnavailable)?;
        let application =
            Application::mount(root, port).map_err(ExternalApplicationFault::application)?;
        Ok(Self {
            control,
            owner: Some(ExternalOwner { application }),
            reaction: None,
        })
    }

    #[cfg(feature = "legacy-provider-port")]
    #[deprecated(note = "use `ExternalApplication::new_root` with `use_provider_event_handler`")]
    pub fn new(
        root: impl Fn(EventInput<ProviderEvent>) -> Component + Send + Sync + 'static,
    ) -> Result<Self, ExternalApplicationFault> {
        Self::new_with_event_input(root)
    }

    #[cfg_attr(
        not(feature = "legacy-provider-port"),
        allow(dead_code, reason = "retained for crate-internal event-routing tests")
    )]
    fn new_with_event_input(
        root: impl Fn(EventInput<ProviderEvent>) -> Component + Send + Sync + 'static,
    ) -> Result<Self, ExternalApplicationFault> {
        let (port, control) =
            ExternalProviderPort::new().map_err(|_| ExternalApplicationFault::TargetUnavailable)?;
        let application = Application::mount_with_events(root, port)
            .map_err(ExternalApplicationFault::application)?;
        Ok(Self {
            control,
            owner: Some(ExternalOwner { application }),
            reaction: None,
        })
    }

    /// Consume this application and finish all Component-owned async work.
    ///
    /// Cleanup starts when this method is called, before the returned future is
    /// polled. Dropping that waiter does not cancel cleanup: the dedicated task
    /// retains ownership until the active reaction has returned the Application
    /// and the Application has fenced, aborted, and joined all mount tasks.
    pub fn shutdown(
        self,
    ) -> impl Future<Output = Result<(), ExternalApplicationFault>> + Send + 'static {
        let cleanup_runtime = Handle::current().id();
        let mut cleanup = tokio::spawn(self.shutdown_owned());
        std::future::poll_fn(move |context| {
            if !Handle::try_current().is_ok_and(|runtime| runtime.id() == cleanup_runtime) {
                return Poll::Ready(Err(ExternalApplicationFault::ShutdownTaskFailed));
            }
            match Pin::new(&mut cleanup).poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(result)) => Poll::Ready(result),
                Poll::Ready(Err(fault)) if fault.is_panic() => {
                    std::panic::resume_unwind(fault.into_panic())
                }
                Poll::Ready(Err(_)) => {
                    Poll::Ready(Err(ExternalApplicationFault::ShutdownTaskFailed))
                }
            }
        })
    }

    /// Finish any pending reaction without output, then return the next Frame.
    pub async fn observe(&mut self) -> Result<ExternalObservation, ExternalApplicationFault> {
        self.finish_or_recover_current().await?;
        self.start_next().await
    }

    /// Finish the current reaction with one act, then return the next Frame.
    pub async fn act(
        &mut self,
        act: ExternalAct,
    ) -> Result<ExternalObservation, ExternalApplicationFault> {
        if self.current_reaction_phase() != Some(ExternalReactionPhase::AwaitingInput) {
            return Err(ExternalApplicationFault::NoActiveReaction);
        }
        let generation = match self.reaction.as_ref().map(|reaction| &reaction.state) {
            Some(ExternalReactionState::AwaitingInput(generation)) => *generation,
            _ => return Err(ExternalApplicationFault::NoActiveReaction),
        };
        self.finish_current(ExternalCompletion::Act(generation, act))
            .await?;
        self.start_next().await
    }

    /// End the pending ingress, reset target continuity, and explicitly react.
    ///
    /// Unlike the removed same-revision re-render path, this always creates a
    /// new Full Frame in a later epoch and a new ingress generation.
    pub async fn observe_full(&mut self) -> Result<ExternalObservation, ExternalApplicationFault> {
        self.finish_or_recover_current().await?;
        self.control.reset_continuity()?;
        self.start_next().await
    }

    async fn finish_or_recover_current(&mut self) -> Result<(), ExternalApplicationFault> {
        if self
            .reaction
            .as_ref()
            .is_some_and(|reaction| reaction.task.is_finished())
        {
            return self.recover_finished_reaction().await;
        }
        match self.current_reaction_phase() {
            Some(ExternalReactionPhase::AwaitingInput) => {
                let generation = match self.reaction.as_ref().map(|reaction| &reaction.state) {
                    Some(ExternalReactionState::AwaitingInput(generation)) => *generation,
                    _ => return Err(ExternalApplicationFault::NoActiveReaction),
                };
                self.finish_current(ExternalCompletion::Empty(generation))
                    .await
            }
            Some(ExternalReactionPhase::AwaitingObservation | ExternalReactionPhase::Finishing) => {
                self.recover_finished_reaction().await
            }
            None => Ok(()),
        }
    }

    async fn shutdown_owned(mut self) -> Result<(), ExternalApplicationFault> {
        let reaction_result = if let Some(reaction) = self.reaction.as_mut() {
            reaction.state = ExternalReactionState::Finishing;
            if let Some(cancellation) = reaction.cancellation.take() {
                let _ = cancellation.send(());
            }
            let completion = self.await_current_completion().await;
            let outcome = self.restore_joined_reaction(completion)?;
            Self::finish_reaction_outcome(outcome)
        } else {
            Ok(())
        };

        let owner = self
            .owner
            .take()
            .ok_or(ExternalApplicationFault::OwnerUnavailable)?;
        let shutdown_result = owner
            .application
            .shutdown()
            .await
            .map_err(ExternalApplicationFault::application);

        reaction_result?;
        shutdown_result
    }

    async fn finish_current(
        &mut self,
        completion: ExternalCompletion,
    ) -> Result<(), ExternalApplicationFault> {
        let stale_ingress_means_already_completed =
            matches!(&completion, ExternalCompletion::Empty(_));
        let cancellation = {
            let reaction = self
                .reaction
                .as_mut()
                .ok_or(ExternalApplicationFault::NoActiveReaction)?;
            reaction.state = ExternalReactionState::Finishing;
            reaction
                .cancellation
                .take()
                .ok_or(ExternalApplicationFault::SynchronizationClosed)?
        };
        let mut cancellation = ExternalReactionCancellation::new(cancellation);
        let control = self.control.clone();
        let injection = async move {
            match completion {
                ExternalCompletion::Empty(generation) => control.complete(generation).await,
                ExternalCompletion::Act(generation, act) => control.act(generation, act).await,
            }
        };
        tokio::pin!(injection);
        let progress = {
            let reaction = self
                .reaction
                .as_mut()
                .ok_or(ExternalApplicationFault::NoActiveReaction)?;
            tokio::select! {
                biased;
                completion = &mut reaction.task => {
                    ExternalReactionFinishProgress::Completion(Box::new(completion))
                },
                injection = &mut injection => {
                    ExternalReactionFinishProgress::Injection(injection)
                },
            }
        };
        let injection = match progress {
            ExternalReactionFinishProgress::Completion(completion) => {
                cancellation.disarm();
                let outcome = self.restore_joined_reaction(*completion)?;
                return Self::finish_reaction_outcome(outcome);
            }
            ExternalReactionFinishProgress::Injection(injection) => injection,
        };

        let completion = self.await_current_completion().await;
        cancellation.disarm();
        let outcome = self.restore_joined_reaction(completion)?;
        Self::finish_reaction_outcome(outcome)?;
        match injection {
            Ok(()) => {}
            Err(ExternalControlFault::StaleIngress) if stale_ingress_means_already_completed => {}
            Err(fault) => return Err(fault.into()),
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
        let control = self.control.clone();

        loop {
            let progress = {
                let reaction = self
                    .reaction
                    .as_mut()
                    .ok_or(ExternalApplicationFault::NoActiveReaction)?;
                tokio::select! {
                    biased;
                    completion = &mut reaction.task => {
                        ExternalReactionProgress::Completion(Box::new(completion))
                    },
                    request = control.next_observation() => {
                        ExternalReactionProgress::Request(Box::new(request))
                    },
                }
            };
            match progress {
                ExternalReactionProgress::Request(request) => match *request {
                    Ok(observation) => {
                        if !self.control.is_active(observation.ingress_generation()) {
                            continue;
                        }
                        let cancellation = cancellation
                            .take()
                            .ok_or(ExternalApplicationFault::SynchronizationClosed)?;
                        let reaction = self
                            .reaction
                            .as_mut()
                            .ok_or(ExternalApplicationFault::NoActiveReaction)?;
                        reaction.state =
                            ExternalReactionState::AwaitingInput(observation.ingress_generation());
                        reaction.cancellation = Some(cancellation);
                        return Ok(observation);
                    }
                    Err(ExternalControlFault::ObservationChannelClosed) => {
                        // The only persistent frame sender belongs to the
                        // Application inside `run_reaction`; submit-time clones
                        // cannot outlive `submit()`. Closure therefore proves
                        // that producer ownership has been destroyed. The
                        // receiver is never closed independently. Join the task
                        // to arbitrate its terminal result instead of racing
                        // Tokio's completion publication.
                        let completion = self.await_current_completion().await;
                        cancellation.disarm();
                        return self.finish_before_observation(completion);
                    }
                    Err(fault) => return Err(fault.into()),
                },
                ExternalReactionProgress::Completion(completion) => {
                    cancellation.disarm();
                    return self.finish_before_observation(*completion);
                }
            }
        }
    }

    fn finish_before_observation(
        &mut self,
        completion: Result<ExternalReactionCompletion, tokio::task::JoinError>,
    ) -> Result<ExternalObservation, ExternalApplicationFault> {
        let completion = match completion {
            Ok(completion) => completion,
            Err(fault) => return Err(self.consume_failed_reaction(fault)),
        };
        match self.restore_completed_reaction(completion)? {
            ExternalReactionOutcome::Finished(Ok(())) | ExternalReactionOutcome::Cancelled => {
                Err(ExternalApplicationFault::ReactionEndedBeforeObservation)
            }
            ExternalReactionOutcome::Finished(Err(fault)) => {
                Err(ExternalApplicationFault::application(fault))
            }
        }
    }

    async fn recover_finished_reaction(&mut self) -> Result<(), ExternalApplicationFault> {
        let completion = match self.await_current_completion().await {
            Ok(completion) => completion,
            Err(fault) => return Err(self.consume_failed_reaction(fault)),
        };
        match self.restore_completed_reaction(completion)? {
            ExternalReactionOutcome::Cancelled => Ok(()),
            ExternalReactionOutcome::Finished(result) => {
                result.map_err(ExternalApplicationFault::application)
            }
        }
    }

    async fn await_current_completion(
        &mut self,
    ) -> Result<ExternalReactionCompletion, tokio::task::JoinError> {
        let reaction = self
            .reaction
            .as_mut()
            .expect("current reaction was checked before awaiting completion");
        (&mut reaction.task).await
    }

    fn restore_completed_reaction(
        &mut self,
        completion: ExternalReactionCompletion,
    ) -> Result<ExternalReactionOutcome, ExternalApplicationFault> {
        let finished = self
            .reaction
            .take()
            .ok_or(ExternalApplicationFault::NoActiveReaction)?;
        drop(finished);
        self.owner = Some(completion.owner);
        Ok(completion.outcome)
    }

    fn restore_joined_reaction(
        &mut self,
        completion: Result<ExternalReactionCompletion, tokio::task::JoinError>,
    ) -> Result<ExternalReactionOutcome, ExternalApplicationFault> {
        match completion {
            Ok(completion) => self.restore_completed_reaction(completion),
            Err(fault) => Err(self.consume_failed_reaction(fault)),
        }
    }

    fn finish_reaction_outcome(
        outcome: ExternalReactionOutcome,
    ) -> Result<(), ExternalApplicationFault> {
        match outcome {
            ExternalReactionOutcome::Finished(result) => {
                result.map_err(ExternalApplicationFault::application)
            }
            ExternalReactionOutcome::Cancelled => Ok(()),
        }
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
        if fault.is_panic() {
            std::panic::resume_unwind(fault.into_panic());
        }
        ExternalApplicationFault::ReactionTaskFailed
    }

    #[cfg(test)]
    fn control(&self) -> ExternalControl {
        self.control.clone()
    }
}

enum ExternalCompletion {
    Empty(ExternalIngressGeneration),
    Act(ExternalIngressGeneration, ExternalAct),
}

async fn run_reaction(
    mut owner: ExternalOwner,
    cancellation: oneshot::Receiver<()>,
) -> ExternalReactionCompletion {
    let outcome = await_reaction_or_cancellation(owner.application.react(), cancellation).await;
    ExternalReactionCompletion { owner, outcome }
}

async fn await_reaction_or_cancellation(
    reaction: impl std::future::Future<Output = Result<(), ApplicationFault>>,
    cancellation: oneshot::Receiver<()>,
) -> ExternalReactionOutcome {
    tokio::pin!(reaction);
    tokio::pin!(cancellation);
    tokio::select! {
        biased;
        result = &mut reaction => ExternalReactionOutcome::Finished(result),
        cancelled = &mut cancellation => {
            match cancelled {
                Ok(()) => ExternalReactionOutcome::Cancelled,
                Err(_) => ExternalReactionOutcome::Finished(reaction.await),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExternalControlFault {
    #[error("external observation channel is closed")]
    ObservationChannelClosed,
    #[error("external act ingress is stale or was already consumed")]
    StaleIngress,
    #[error("external continuity cannot reset while an ingress is active")]
    ActiveIngress,
    #[error("external target continuity is unavailable")]
    ContinuityUnavailable,
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
    #[error("external reaction task failed")]
    ReactionTaskFailed,
    #[error("external application cleanup task failed")]
    ShutdownTaskFailed,
    #[error("external target could not be created")]
    TargetUnavailable,
    #[error("external frame-driven application failed")]
    Application,
    #[error(transparent)]
    Control(#[from] ExternalControlFault),
}

impl ExternalApplicationFault {
    fn application(_fault: ApplicationFault) -> Self {
        Self::Application
    }
}

#[cfg(test)]
mod tests;
