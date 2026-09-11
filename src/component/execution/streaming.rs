//! Application-owned workers for independently parsed streaming contracts.

use std::{
    any::Any,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use futures::{future::join_all, FutureExt};
use tokio::sync::{mpsc, oneshot};

use crate::{
    component::authoring::streaming_attempt::{
        ContractDeclaration, ErasedContract, StreamingToolAbortCause, StreamingToolAttemptId,
        StreamingToolAttemptStart,
    },
    transcript::AssistantPhase,
};

use super::{
    admission::{AdmittedReactionSummary, AdmittedTextFact},
    application::ApplicationFault,
    ProviderOutputKey,
};

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

type WorkerPanic = Box<dyn Any + Send + 'static>;

/// One independent contract's result during Application recovery.
#[derive(Debug, Clone)]
pub struct StreamingToolRecoveryReport {
    pub attempt: StreamingToolAttemptId,
    pub contract: &'static str,
    pub accepted: bool,
    pub settled: bool,
    pub fault: Option<ApplicationFault>,
}

/// Recovery never restarts the provider or replays a completed contract.
#[derive(Debug, Clone)]
pub enum StreamingToolRecoveryStatus {
    NotRequired,
    InFlight {
        attempts: Box<[StreamingToolRecoveryReport]>,
    },
    StillRequired {
        attempts: Box<[StreamingToolRecoveryReport]>,
    },
    Recovered {
        attempts: Box<[StreamingToolRecoveryReport]>,
        reaction_requested: bool,
    },
}

#[derive(Default)]
pub(super) struct StreamingSupervisor {
    workers: Vec<Worker>,
    selected: Option<ProviderOutputKey>,
    sealed: bool,
    pub(super) normal_eof: bool,
    pub(super) saved_fault: Option<ApplicationFault>,
}

impl StreamingSupervisor {
    pub(super) fn pending(&self) -> bool {
        !self.workers.is_empty()
    }

    pub(super) fn start(
        &mut self,
        declarations: Vec<ContractDeclaration>,
    ) -> Result<StreamingReactionLease, ApplicationFault> {
        debug_assert!(!self.pending());
        let mut drivers = Vec::with_capacity(declarations.len());
        for declaration in declarations {
            let id = NEXT_ATTEMPT
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| ApplicationFault::streaming_runtime())?;
            let start = StreamingToolAttemptStart::new(
                StreamingToolAttemptId::new(id),
                declaration.identity(),
                declaration.implementation_version(),
            );
            drivers.push(
                declaration
                    .start(start)
                    .map_err(ApplicationFault::from_streaming)?,
            );
        }
        self.workers = drivers.into_iter().map(Worker::spawn).collect();
        self.selected = None;
        self.sealed = false;
        self.normal_eof = false;
        self.saved_fault = None;
        Ok(StreamingReactionLease {
            workers: self.workers.clone(),
            armed: true,
        })
    }

    pub(super) async fn dispatch(
        &mut self,
        fact: AdmittedTextFact,
    ) -> Result<(), ApplicationFault> {
        if !self.pending() {
            return Ok(());
        }
        let (output, phase, text, seal) = match fact {
            AdmittedTextFact::Delta {
                output,
                phase,
                delta,
            } => (output, phase, delta, false),
            AdmittedTextFact::Sealed {
                output,
                phase,
                text,
            } => (output, phase, text, true),
        };
        if phase == Some(AssistantPhase::Commentary) {
            return Ok(());
        }
        if self.sealed || self.selected.is_some_and(|selected| selected != output) {
            return Err(ApplicationFault::streaming_protocol());
        }
        self.selected = Some(output);
        self.sealed = seal;
        let text: Arc<str> = text.into();
        let results = join_all(self.workers.iter().map(|worker| {
            worker.call(if seal {
                Operation::Seal(text.clone())
            } else {
                Operation::Push(text.clone())
            })
        }))
        .await;
        results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map(|_| ())
    }

    pub(super) async fn finish(
        &mut self,
        summary: AdmittedReactionSummary,
    ) -> Result<(), ApplicationFault> {
        if !self.pending() {
            return Ok(());
        }
        if summary.primary_text != self.selected || (self.selected.is_some() && !self.sealed) {
            return Err(ApplicationFault::streaming_protocol());
        }
        self.normal_eof = true;
        let results = join_all(
            self.workers
                .iter()
                .map(|worker| worker.call(Operation::Finish)),
        )
        .await;
        results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map(|_| ())
    }

    pub(super) async fn abort(&mut self, cause: StreamingToolAbortCause) {
        for worker in &self.workers {
            worker.cancelled.store(true, Ordering::Release);
        }
        let _ = join_all(
            self.workers
                .iter()
                .map(|worker| worker.call(Operation::Abort(cause))),
        )
        .await;
    }

    pub(super) fn clear(&self) -> bool {
        self.workers.iter().all(|worker| {
            let state = worker.state.lock().unwrap_or_else(|p| p.into_inner());
            state.finished && !state.busy && !state.recovery
        })
    }

    pub(super) fn busy(&self) -> bool {
        self.workers
            .iter()
            .any(|worker| worker.state.lock().unwrap_or_else(|p| p.into_inner()).busy)
    }

    pub(super) fn reaction_requested(&self) -> bool {
        self.saved_fault.is_none()
            && !self.workers.iter().any(|worker| {
                worker
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .fault
                    .is_some()
            })
            && self.workers.iter().any(|worker| {
                !worker.cancelled.load(Ordering::Acquire)
                    && worker
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .reaction_requested
            })
    }

    pub(super) fn reports(&self) -> Box<[StreamingToolRecoveryReport]> {
        self.workers
            .iter()
            .map(|worker| {
                let state = worker.state.lock().unwrap_or_else(|p| p.into_inner());
                StreamingToolRecoveryReport {
                    attempt: worker.attempt,
                    contract: worker.identity,
                    accepted: state.accepted,
                    settled: state.finished && !state.busy && !state.recovery,
                    fault: self.saved_fault.or(state.fault),
                }
            })
            .collect()
    }

    pub(super) async fn recover(&mut self) {
        let _ = join_all(
            self.workers
                .iter()
                .filter(|worker| {
                    let state = worker.state.lock().unwrap_or_else(|p| p.into_inner());
                    !state.busy && (state.recovery || !state.finished)
                })
                .map(|worker| worker.call(Operation::Recover)),
        )
        .await;
    }

    /// The worker owns a direct reducer/decoder panic until all contracts have
    /// reached a definitive cleanup state. The outer Application boundary then
    /// resumes its original payload instead of manufacturing an ApplicationFault.
    pub(super) fn take_panic_if_clear(&self) -> Option<WorkerPanic> {
        if !self.clear() {
            return None;
        }
        self.workers.iter().find_map(Worker::take_panic_if_clear)
    }

    pub(super) fn release(&mut self) {
        debug_assert!(self.clear());
        self.workers.clear();
        self.selected = None;
        self.sealed = false;
        self.normal_eof = false;
        self.saved_fault = None;
    }
}

pub(super) struct StreamingReactionLease {
    workers: Vec<Worker>,
    armed: bool,
}

impl StreamingReactionLease {
    pub(super) fn complete(&mut self) {
        self.armed = false;
    }
}

impl Drop for StreamingReactionLease {
    fn drop(&mut self) {
        if self.armed {
            for worker in &self.workers {
                worker.cancelled.store(true, Ordering::Release);
                let _ = worker.enqueue(Command {
                    operation: Operation::Abort(StreamingToolAbortCause::Cancelled),
                    reply: None,
                });
            }
        }
    }
}

#[derive(Default)]
struct WorkerState {
    busy: bool,
    queued: usize,
    finished: bool,
    recovery: bool,
    accepted: bool,
    reaction_requested: bool,
    fault: Option<ApplicationFault>,
    panic: Option<WorkerPanic>,
}

#[derive(Clone)]
struct Worker {
    identity: &'static str,
    attempt: StreamingToolAttemptId,
    commands: mpsc::UnboundedSender<Command>,
    state: Arc<Mutex<WorkerState>>,
    cancelled: Arc<AtomicBool>,
}

enum Operation {
    Push(Arc<str>),
    Seal(Arc<str>),
    Finish,
    Abort(StreamingToolAbortCause),
    Recover,
}

struct Command {
    operation: Operation,
    reply: Option<oneshot::Sender<Result<(), ApplicationFault>>>,
}

impl Worker {
    fn spawn(mut driver: Box<dyn ErasedContract>) -> Self {
        let identity = driver.identity();
        let attempt = driver.attempt();
        let (commands, mut receiver) = mpsc::unbounded_channel::<Command>();
        let state = Arc::new(Mutex::new(WorkerState::default()));
        let shared = state.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = cancelled.clone();
        driver.set_cancellation(cancellation.clone());
        tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                shared.lock().unwrap_or_else(|p| p.into_inner()).busy = true;
                let operation = if cancellation.load(Ordering::Acquire)
                    && matches!(
                        command.operation,
                        Operation::Push(_) | Operation::Seal(_) | Operation::Finish
                    ) {
                    Operation::Abort(StreamingToolAbortCause::Cancelled)
                } else {
                    command.operation
                };
                let aborting = matches!(operation, Operation::Abort(_));
                let result = std::panic::AssertUnwindSafe(async {
                    match operation {
                        Operation::Push(text) => driver.push(&text).await,
                        Operation::Seal(text) => driver.seal(&text).await,
                        Operation::Finish => driver.finish().await,
                        Operation::Abort(cause) => driver.abort(cause).await,
                        Operation::Recover => driver.recover().await,
                    }
                })
                .catch_unwind()
                .await;
                let (result, panic) = match result {
                    Ok(result) => (result.map_err(ApplicationFault::from_streaming), None),
                    Err(payload) => (Err(ApplicationFault::streaming_runtime()), Some(payload)),
                };
                let panicked = panic.is_some();
                let failed = result.is_err();
                if failed && !driver.needs_recovery() && !aborting {
                    let _ = std::panic::AssertUnwindSafe(
                        driver.abort(StreamingToolAbortCause::RuntimeFault),
                    )
                    .catch_unwind()
                    .await;
                }
                if cancellation.load(Ordering::Acquire) && !aborting {
                    let _ = std::panic::AssertUnwindSafe(
                        driver.abort(StreamingToolAbortCause::Cancelled),
                    )
                    .catch_unwind()
                    .await;
                }
                let terminal_fault = driver
                    .terminal_fault()
                    .map(ApplicationFault::from_streaming);
                {
                    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
                    state.queued = state.queued.saturating_sub(1);
                    state.busy = state.queued != 0;
                    state.finished = driver.completed();
                    state.recovery = driver.needs_recovery();
                    state.accepted = driver.accepted();
                    state.reaction_requested =
                        driver.reaction_requested() && !cancellation.load(Ordering::Acquire);
                    let has_panic = panicked || state.panic.is_some();
                    if let Some(payload) = panic {
                        state.panic.get_or_insert(payload);
                    }
                    if let Err(fault) = &result {
                        if !has_panic
                            && fault.kind() != super::ApplicationFaultKind::RecoveryRequired
                        {
                            state.fault.get_or_insert(*fault);
                        }
                    }
                    if let Some(fault) = terminal_fault {
                        if !has_panic {
                            state.fault.get_or_insert(fault);
                        }
                    }
                }
                if let Some(reply) = command.reply {
                    let _ = reply.send(result);
                }
            }
        });
        Self {
            identity,
            attempt,
            commands,
            state,
            cancelled,
        }
    }

    async fn call(&self, operation: Operation) -> Result<(), ApplicationFault> {
        let (reply, result) = oneshot::channel();
        self.enqueue(Command {
            operation,
            reply: Some(reply),
        })?;
        let result = result
            .await
            .map_err(|_| ApplicationFault::streaming_runtime())?;
        if let Some(payload) = self.take_panic_if_clear() {
            std::panic::resume_unwind(payload);
        }
        result
    }

    fn take_panic_if_clear(&self) -> Option<WorkerPanic> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (state.finished && !state.busy && !state.recovery)
            .then(|| state.panic.take())
            .flatten()
    }

    fn enqueue(&self, command: Command) -> Result<(), ApplicationFault> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.queued += 1;
        state.busy = true;
        if self.commands.send(command).is_err() {
            state.queued -= 1;
            state.busy = state.queued != 0;
            state.recovery = true;
            return Err(ApplicationFault::streaming_runtime());
        }
        Ok(())
    }
}
