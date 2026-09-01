//! Fixed-port owner for the frame-driven application runtime.

#![allow(dead_code)]

use std::{
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

use futures::{stream::FuturesUnordered, StreamExt};

use super::{
    admission::{
        ReactionAdmissionFault, ReactionAdmissionGuard, ReactionAdmissionReason, ToolLaneTicket,
        ToolOutputStagingFault, ToolOutputStagingReason,
    },
    driver_demand::{DriverDemand, DriverDemandFault},
    frame::{FrameSession, FrameSessionFault, InvalidFrameProfileFault, PreparedFrame},
    port::{ProviderEvent, RenderedProjection, ToolOutput},
    projection_diff::ProjectionReconciliationFault,
    reaction::{
        ProviderFactStream, ReactionPort, ReactionPortFault, ReactionPortFaultCode,
        ReactionPortFaultKind, ReactionPortFaultReason, SubmitFault, TargetDeclaration,
        TargetDeclarationInvariantFault,
    },
};
use crate::component::{
    authoring::{
        Component, ComponentAttemptFault, InternalEventInput as EventInput, MountTaskStart,
        RenderBindings, SpawnError,
    },
    host::CommittedRenderTransition,
    host::ComponentHost,
    task::{
        MountTaskScope, MountTaskSupervisor, MountTaskSupervisorError, TaskPanicMonitor,
        TaskRetirement, TaskSupervisorStatus,
    },
    ComponentHostFault, PreparedRender,
};

type RootFactory = Arc<dyn Fn(EventInput<ProviderEvent>) -> Component + Send + Sync + 'static>;

fn render_root(root: RootFactory, events: EventInput<ProviderEvent>) -> Component {
    root(events)
}

const APPLICATION_READY: u8 = 0;
const APPLICATION_TERMINATED_AFTER_CANCELLATION: u8 = 1;
const APPLICATION_TERMINATED_AFTER_TASK_PANIC: u8 = 2;

fn consume_supervised_task_panic(application_state: &Arc<AtomicU8>, monitor: &TaskPanicMonitor) {
    if application_state.swap(APPLICATION_TERMINATED_AFTER_TASK_PANIC, Ordering::AcqRel)
        == APPLICATION_TERMINATED_AFTER_TASK_PANIC
    {
        return;
    }
    if let Some(payload) = monitor.take_payload() {
        resume_unwind(payload);
    }
}

fn task_panic_terminal_fault(stage: ApplicationFaultStage) -> ApplicationFault {
    ApplicationFault::terminal(
        stage,
        ApplicationFaultCode::Unavailable,
        ApplicationFaultReason::ComponentRuntime,
    )
}

fn application_terminal_state_fault(
    stage: ApplicationFaultStage,
    state: u8,
) -> Option<ApplicationFault> {
    match state {
        APPLICATION_READY => None,
        APPLICATION_TERMINATED_AFTER_CANCELLATION => Some(ApplicationFault::terminal(
            stage,
            ApplicationFaultCode::Unavailable,
            ApplicationFaultReason::CancelledAfterHandoff,
        )),
        APPLICATION_TERMINATED_AFTER_TASK_PANIC => Some(task_panic_terminal_fault(stage)),
        _ => Some(ApplicationFault::terminal(
            stage,
            ApplicationFaultCode::Internal,
            ApplicationFaultReason::ComponentRuntime,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OuterDriverBoundaryFault {
    ApplicationState(u8),
    TaskSupervisorClosed,
}

fn begin_outer_driver_boundary(
    application_state: &Arc<AtomicU8>,
    monitor: &TaskPanicMonitor,
) -> Result<(), OuterDriverBoundaryFault> {
    if monitor.status() == TaskSupervisorStatus::Panicked {
        consume_supervised_task_panic(application_state, monitor);
    }

    let state = application_state.load(Ordering::Acquire);
    if state != APPLICATION_READY {
        // A task can panic after the first observation but before this existing
        // terminal classification is returned. Give that fresh payload priority.
        if monitor.status() == TaskSupervisorStatus::Panicked {
            consume_supervised_task_panic(application_state, monitor);
        }
        return Err(OuterDriverBoundaryFault::ApplicationState(
            application_state.load(Ordering::Acquire),
        ));
    }

    match monitor.status() {
        TaskSupervisorStatus::Healthy => Ok(()),
        TaskSupervisorStatus::Panicked => {
            consume_supervised_task_panic(application_state, monitor);
            Err(OuterDriverBoundaryFault::ApplicationState(
                application_state.load(Ordering::Acquire),
            ))
        }
        TaskSupervisorStatus::Closed => Err(OuterDriverBoundaryFault::TaskSupervisorClosed),
    }
}

fn application_fault_from_outer_driver_boundary(
    stage: ApplicationFaultStage,
    fault: OuterDriverBoundaryFault,
) -> ApplicationFault {
    match fault {
        OuterDriverBoundaryFault::ApplicationState(state) => {
            application_terminal_state_fault(stage, state)
                .expect("outer boundary only returns a non-ready Application state")
        }
        OuterDriverBoundaryFault::TaskSupervisorClosed => {
            ApplicationFault::from_task_supervisor(stage, MountTaskSupervisorError::Closed)
        }
    }
}

fn drop_reaction_before_panic_arbitration<T>(reaction: T, monitor: &TaskPanicMonitor) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(reaction))) {
        if monitor.status() != TaskSupervisorStatus::Panicked {
            resume_unwind(payload);
        }
    }
}

type ToolLane = Pin<
    Box<dyn Future<Output = Result<CompletedToolLane, ComponentAttemptFault>> + Send + 'static>,
>;

struct CompletedToolLane {
    ticket: ToolLaneTicket,
    output: ToolOutput,
}

enum SubmissionAttempt {
    Completed,
    ContinuityChanged,
}

struct ReactionCancellationGuard {
    application_state: Arc<AtomicU8>,
    handed_off: bool,
    returned: bool,
}

impl ReactionCancellationGuard {
    fn new(application_state: Arc<AtomicU8>) -> Self {
        Self {
            application_state,
            handed_off: false,
            returned: false,
        }
    }

    fn mark_handoff(&mut self) {
        self.handed_off = true;
    }

    fn mark_returned(&mut self) {
        self.returned = true;
    }
}

impl Drop for ReactionCancellationGuard {
    fn drop(&mut self) {
        if self.handed_off && !self.returned {
            let _ = self.application_state.compare_exchange(
                APPLICATION_READY,
                APPLICATION_TERMINATED_AFTER_CANCELLATION,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

/// Owns one mounted Component runtime, one private session, and one fixed port.
///
/// The port and session remain private. Applications are mounted, driven with
/// explicit reactions, observed through snapshots, then consumed by shutdown.
pub struct Application<P: ReactionPort> {
    components: ComponentHost<RootFactory>,
    pending_render: Option<PendingApplicationRender>,
    driver_demand: DriverDemand,
    tasks: MountTaskSupervisor,
    session: FrameSession,
    declaration: TargetDeclaration,
    port: P,
    state: Arc<AtomicU8>,
}

struct PendingApplicationRender {
    committed: CommittedRenderTransition,
    retirement: TaskRetirement,
}

/// One read-only view of the latest successfully committed Component DOM.
///
/// A dirty snapshot remains complete: `dirty` only reports that newer
/// Component state is waiting for a later explicit reconcile.
pub struct ProjectionSnapshot<'a> {
    projection: &'a RenderedProjection,
    revision: u64,
    dirty: bool,
}

impl ProjectionSnapshot<'_> {
    pub const fn projection(&self) -> &RenderedProjection {
        self.projection
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }
}

impl<P: ReactionPort> Application<P> {
    fn arbitrate_task_panic(&self, stage: ApplicationFaultStage) -> ApplicationFault {
        let monitor = self.tasks.panic_monitor();
        consume_supervised_task_panic(&self.state, &monitor);
        task_panic_terminal_fault(stage)
    }

    fn check_task_panic(&self, stage: ApplicationFaultStage) -> Result<(), ApplicationFault> {
        if self.tasks.panic_monitor().status() == TaskSupervisorStatus::Panicked {
            return Err(self.arbitrate_task_panic(stage));
        }
        Ok(())
    }

    /// Declare one fixed target and bootstrap its complete Component projection.
    ///
    /// Mount does not create or submit a Frame. The declaration and profile
    /// checks happen before the root is invoked, so an invalid target contract
    /// cannot establish partial Component state.
    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
        port: P,
    ) -> Result<Self, ApplicationFault> {
        Self::mount_with_event_root(move |_events| root(), port)
    }

    /// Temporary bridge for integrations that still receive provider events
    /// through the retained Component authoring surface.
    pub(crate) fn mount_with_events(
        root: impl Fn(EventInput<ProviderEvent>) -> Component + Send + Sync + 'static,
        port: P,
    ) -> Result<Self, ApplicationFault> {
        Self::mount_with_event_root(root, port)
    }

    fn mount_with_event_root(
        root: impl Fn(EventInput<ProviderEvent>) -> Component + Send + Sync + 'static,
        mut port: P,
    ) -> Result<Self, ApplicationFault> {
        let declaration = port.declare().map_err(|source| {
            ApplicationFault::from_port(ApplicationFaultStage::Declaration, source)
        })?;
        declaration
            .validate()
            .map_err(ApplicationFault::invalid_declaration)?;
        declaration
            .profile()
            .validate()
            .map_err(ApplicationFault::invalid_profile)?;

        let (driver_demand, demand_handle) = DriverDemand::new();
        let tasks = MountTaskSupervisor::new();
        let task_handle = tasks.handle();
        let root: RootFactory = Arc::new(root);
        let mut application = Self {
            components: ComponentHost::new_with_application_capabilities(
                render_root,
                root,
                demand_handle,
                task_handle,
            ),
            pending_render: None,
            driver_demand,
            tasks,
            session: FrameSession::new(&declaration).map_err(|source| {
                ApplicationFault::from_session(ApplicationFaultStage::Declaration, source)
            })?,
            declaration,
            port,
            state: Arc::new(AtomicU8::new(APPLICATION_READY)),
        };
        let committed = application
            .components
            .begin_managed_render()
            .map_err(|source| {
                ApplicationFault::from_component(ApplicationFaultStage::Bootstrap, source)
            })?;
        debug_assert!(committed.retired_mounts().is_empty());
        let published = application.components.publish_managed_render(committed);
        let (_rendered, task_starts) = published.into_parts();
        application.start_task_batch(ApplicationFaultStage::Bootstrap, task_starts)?;
        application.check_task_panic(ApplicationFaultStage::Bootstrap)?;
        Ok(application)
    }

    /// Wait for and consume one coalesced Component request for a later reaction.
    ///
    /// A request is sticky until one driver observation consumes it. This call
    /// does not render Components or submit a Frame.
    pub async fn wait_for_reaction_request(&mut self) -> Result<(), ApplicationFault> {
        let stage = ApplicationFaultStage::Reaction;
        let application_state = Arc::clone(&self.state);
        let monitor = self.tasks.panic_monitor();
        begin_outer_driver_boundary(&application_state, &monitor)
            .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
        let mut waiting = Box::pin(self.driver_demand.wait());
        let result = tokio::select! {
            biased;
            task = monitor.wait() => {
                drop(waiting);
                return match task {
                    Ok(()) => {
                        consume_supervised_task_panic(&application_state, &monitor);
                        Err(task_panic_terminal_fault(stage))
                    }
                    Err(source) => Err(ApplicationFault::from_task_supervisor(stage, source)),
                };
            }
            demand = &mut waiting => demand,
        };
        drop(waiting);
        match monitor.status() {
            TaskSupervisorStatus::Healthy => {
                result.map_err(|source| ApplicationFault::from_driver_demand(stage, source))
            }
            TaskSupervisorStatus::Panicked => {
                consume_supervised_task_panic(&application_state, &monitor);
                Err(task_panic_terminal_fault(stage))
            }
            TaskSupervisorStatus::Closed => Err(ApplicationFault::from_task_supervisor(
                stage,
                MountTaskSupervisorError::Closed,
            )),
        }
    }

    /// Consume a pending Component request without waiting.
    ///
    /// Repeated Component requests coalesce, so this returns `true` at most
    /// once before another request arrives. It does not render or submit.
    pub fn take_reaction_request(&self) -> Result<bool, ApplicationFault> {
        let stage = ApplicationFaultStage::Reaction;
        let monitor = self.tasks.panic_monitor();
        begin_outer_driver_boundary(&self.state, &monitor)
            .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
        let result = self
            .driver_demand
            .take()
            .map_err(|source| ApplicationFault::from_driver_demand(stage, source));
        begin_outer_driver_boundary(&self.state, &monitor)
            .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
        result
    }

    /// Internal compatibility bridge for pre-public demand tests.
    pub(crate) async fn wait_for_driver_demand(&mut self) -> Result<(), DriverDemandFault> {
        self.wait_for_reaction_request()
            .await
            .map_err(|_| DriverDemandFault::StaleMount)
    }

    /// Internal compatibility bridge for pre-public demand tests.
    pub(crate) fn take_driver_demand(&self) -> Result<bool, DriverDemandFault> {
        self.take_reaction_request()
            .map_err(|_| DriverDemandFault::StaleMount)
    }

    /// Fence the mounted tree, then abort and await every Component-owned task.
    pub async fn shutdown(mut self) -> Result<(), ApplicationFault> {
        let application_state = Arc::clone(&self.state);
        let monitor = self.tasks.panic_monitor();
        let panic_already_consumed =
            application_state.load(Ordering::Acquire) == APPLICATION_TERMINATED_AFTER_TASK_PANIC;
        if !panic_already_consumed && monitor.status() == TaskSupervisorStatus::Panicked {
            consume_supervised_task_panic(&application_state, &monitor);
            return Err(task_panic_terminal_fault(ApplicationFaultStage::Reaction));
        }

        let mount_fence = self.components.fence_all_mounts().map_err(|source| {
            ApplicationFault::from_component(ApplicationFaultStage::Reaction, source)
        });
        self.pending_render.take();

        if panic_already_consumed {
            let _ = self.tasks.shutdown().await;
            mount_fence?;
            return Err(task_panic_terminal_fault(ApplicationFaultStage::Reaction));
        }

        let mut shutdown = Box::pin(self.tasks.shutdown());
        let result = tokio::select! {
            biased;
            task = monitor.wait() => {
                match task {
                    Ok(()) => {
                        drop(shutdown);
                        consume_supervised_task_panic(&application_state, &monitor);
                        return Err(task_panic_terminal_fault(ApplicationFaultStage::Reaction));
                    }
                    Err(_) => shutdown.as_mut().await,
                }
            }
            result = &mut shutdown => result,
        };
        drop(shutdown);
        if monitor.status() == TaskSupervisorStatus::Panicked {
            consume_supervised_task_panic(&application_state, &monitor);
            return Err(task_panic_terminal_fault(ApplicationFaultStage::Reaction));
        }
        mount_fence?;
        result.map_err(|source| {
            ApplicationFault::from_task_supervisor(ApplicationFaultStage::Reaction, source)
        })
    }

    /// Return the latest committed complete projection without reconciling.
    pub fn current_projection(&self) -> ProjectionSnapshot<'_> {
        let projection = self
            .components
            .current_projection()
            .expect("mounted Application always has a committed projection");
        let revision = self
            .components
            .current_projection_revision()
            .expect("committed Application projection always has a revision")
            .get();
        ProjectionSnapshot {
            projection,
            revision,
            dirty: self.components.is_dirty() || self.pending_render.is_some(),
        }
    }

    /// Reconcile and complete exactly one externally requested reaction.
    ///
    /// Dirty Component state never calls this method implicitly. A successful
    /// handoff commits before the returned fact stream is observed; later
    /// stream, binding, or lane faults retain that committed Frame.
    pub async fn react(&mut self) -> Result<(), ApplicationFault> {
        let application_state = Arc::clone(&self.state);
        let monitor = self.tasks.panic_monitor();
        begin_outer_driver_boundary(&application_state, &monitor).map_err(|fault| {
            application_fault_from_outer_driver_boundary(ApplicationFaultStage::Reaction, fault)
        })?;
        let mut reaction = Box::pin(self.react_with_cancellation_guard());
        let result = tokio::select! {
            biased;
            task = monitor.wait() => {
                drop_reaction_before_panic_arbitration(reaction, &monitor);
                return match task {
                    Ok(()) => {
                        consume_supervised_task_panic(&application_state, &monitor);
                        Err(task_panic_terminal_fault(ApplicationFaultStage::Reaction))
                    }
                    Err(source) => Err(ApplicationFault::from_task_supervisor(
                        ApplicationFaultStage::Reaction,
                        source,
                    )),
                };
            }
            result = &mut reaction => result,
        };
        drop_reaction_before_panic_arbitration(reaction, &monitor);
        match monitor.status() {
            TaskSupervisorStatus::Healthy => result,
            TaskSupervisorStatus::Panicked => {
                consume_supervised_task_panic(&application_state, &monitor);
                Err(task_panic_terminal_fault(ApplicationFaultStage::Reaction))
            }
            TaskSupervisorStatus::Closed => Err(ApplicationFault::from_task_supervisor(
                ApplicationFaultStage::Reaction,
                MountTaskSupervisorError::Closed,
            )),
        }
    }

    async fn react_with_cancellation_guard(&mut self) -> Result<(), ApplicationFault> {
        if let Some(fault) = application_terminal_state_fault(
            ApplicationFaultStage::Reaction,
            self.state.load(Ordering::Acquire),
        ) {
            return Err(fault);
        }
        let mut cancellation = ReactionCancellationGuard::new(Arc::clone(&self.state));
        let result = self.react_inner(&mut cancellation).await;
        cancellation.mark_returned();
        result
    }

    async fn react_inner(
        &mut self,
        cancellation: &mut ReactionCancellationGuard,
    ) -> Result<(), ApplicationFault> {
        let declaration = self.refresh_declaration()?;
        self.check_task_panic(ApplicationFaultStage::Declaration)?;
        let rendered = self
            .reconcile_components(ApplicationFaultStage::Reconcile)
            .await?;
        let (projection, mut bindings) = rendered.into_execution_parts();
        let prepared = self
            .session
            .prepare(&declaration, &projection)
            .map_err(|source| {
                ApplicationFault::from_session(ApplicationFaultStage::FramePrepare, source)
            })?;
        self.check_task_panic(ApplicationFaultStage::FramePrepare)?;

        match run_submission_attempt(
            &mut self.port,
            &mut self.session,
            &mut bindings,
            prepared,
            cancellation,
        )
        .await?
        {
            SubmissionAttempt::Completed => {}
            SubmissionAttempt::ContinuityChanged => {
                let retry_declaration = self.refresh_declaration()?;
                let retry = self
                    .session
                    .prepare(&retry_declaration, &projection)
                    .map_err(|source| {
                        ApplicationFault::from_session(ApplicationFaultStage::FramePrepare, source)
                    })?;
                match run_submission_attempt(
                    &mut self.port,
                    &mut self.session,
                    &mut bindings,
                    retry,
                    cancellation,
                )
                .await?
                {
                    SubmissionAttempt::Completed => {}
                    SubmissionAttempt::ContinuityChanged => {
                        return Err(ApplicationFault::terminal(
                            ApplicationFaultStage::Submit,
                            ApplicationFaultCode::Protocol,
                            ApplicationFaultReason::UnstableContinuity,
                        ))
                    }
                }
            }
        }
        if self.components.is_dirty() {
            self.reconcile_components(ApplicationFaultStage::PostReconcile)
                .await?;
        }
        Ok(())
    }

    async fn reconcile_components(
        &mut self,
        stage: ApplicationFaultStage,
    ) -> Result<PreparedRender, ApplicationFault> {
        if self.pending_render.is_none() {
            let committed = match self.components.begin_managed_render() {
                Ok(committed) => committed,
                Err(ComponentHostFault::TaskPanicked) => {
                    return Err(self.arbitrate_task_panic(stage));
                }
                Err(source) => return Err(ApplicationFault::from_component(stage, source)),
            };
            self.check_task_panic(stage)?;
            let retired = committed
                .retired_mounts()
                .iter()
                .map(|mount| MountTaskScope::new(mount.component().clone(), mount.generation()))
                .collect::<Vec<_>>();
            let retirement = match self.tasks.retire(retired) {
                Ok(retirement) => retirement,
                Err(MountTaskSupervisorError::Panicked) => {
                    return Err(self.arbitrate_task_panic(stage));
                }
                Err(source) => {
                    return Err(ApplicationFault::from_task_supervisor(stage, source));
                }
            };
            self.pending_render = Some(PendingApplicationRender {
                committed,
                retirement,
            });
        }

        let retirement = self
            .pending_render
            .as_ref()
            .expect("a managed render transition was installed")
            .retirement
            .clone();
        match retirement.wait().await {
            Ok(()) => {}
            Err(MountTaskSupervisorError::Panicked) => {
                return Err(self.arbitrate_task_panic(stage));
            }
            Err(source) => return Err(ApplicationFault::from_task_supervisor(stage, source)),
        }
        self.check_task_panic(stage)?;
        let pending = self
            .pending_render
            .take()
            .expect("a completed managed render transition remains installed");
        let published = self.components.publish_managed_render(pending.committed);
        let (rendered, task_starts) = published.into_parts();
        self.start_task_batch(stage, task_starts)?;
        self.check_task_panic(stage)?;
        Ok(rendered)
    }

    fn start_task_batch(
        &self,
        stage: ApplicationFaultStage,
        starts: Vec<MountTaskStart>,
    ) -> Result<(), ApplicationFault> {
        for start in starts {
            match start.start() {
                Ok(()) => {}
                Err(SpawnError::RuntimePanicked) => {
                    return Err(self.arbitrate_task_panic(stage));
                }
                Err(source) => return Err(ApplicationFault::from_task_start(stage, source)),
            }
        }
        Ok(())
    }

    fn refresh_declaration(&mut self) -> Result<TargetDeclaration, ApplicationFault> {
        let declaration = self.port.declare().map_err(|source| {
            ApplicationFault::from_port(ApplicationFaultStage::Declaration, source)
        })?;
        declaration
            .validate()
            .map_err(ApplicationFault::invalid_declaration)?;
        self.session
            .observe_declaration(&declaration)
            .map_err(|source| {
                ApplicationFault::from_session(ApplicationFaultStage::Declaration, source)
            })?;
        self.declaration = declaration.clone();
        Ok(declaration)
    }
}

/// Submit one prepared Frame and linearize its private commit at handoff.
///
/// Only the port borrow is carried by the returned stream. The session borrow
/// ends after the synchronous commit, leaving the reaction pump free to mutate
/// shared history and Component state while it consumes port-owned facts.
async fn submit_prepared_frame<'port, P: ReactionPort>(
    port: &'port mut P,
    session: &mut FrameSession,
    prepared: PreparedFrame,
) -> Result<ProviderFactStream<'port>, SubmitFault> {
    let (frame, commit) = prepared.into_parts();
    let stream = port.submit(frame).await?;
    session.commit(commit);
    Ok(stream)
}

async fn run_submission_attempt<P: ReactionPort>(
    port: &mut P,
    session: &mut FrameSession,
    bindings: &mut RenderBindings<ProviderEvent>,
    prepared: PreparedFrame,
    cancellation: &mut ReactionCancellationGuard,
) -> Result<SubmissionAttempt, ApplicationFault> {
    let submission = submit_prepared_frame(port, session, prepared).await;
    match submission {
        Ok(facts) => {
            cancellation.mark_handoff();
            pump_provider_facts(session, bindings, facts).await?;
            Ok(SubmissionAttempt::Completed)
        }
        Err(SubmitFault::ContinuityChanged) => Ok(SubmissionAttempt::ContinuityChanged),
        Err(fault) => Err(ApplicationFault::from_submit(fault)),
    }
}

async fn pump_provider_facts(
    session: &mut FrameSession,
    bindings: &mut RenderBindings<ProviderEvent>,
    mut facts: ProviderFactStream<'_>,
) -> Result<(), ApplicationFault> {
    let budget = session.full_reserve_budget();
    let mut admission = ReactionAdmissionGuard::with_budget(
        &mut session.canonical_history.transcript,
        &mut session.target_delivery.tool_outputs,
        budget,
    )?;
    let mut lanes = FuturesUnordered::<ToolLane>::new();

    let fact_stream_fault = loop {
        enum Next {
            Fact(Option<Result<super::reaction::ProviderFact, ReactionPortFault>>),
            Lane(Option<Result<CompletedToolLane, ComponentAttemptFault>>),
        }

        let next = if lanes.is_empty() {
            Next::Fact(facts.next().await)
        } else {
            tokio::select! {
                fact = facts.next() => Next::Fact(fact),
                lane = lanes.next() => Next::Lane(lane),
            }
        };

        match next {
            Next::Lane(lane) => finish_tool_lane(lane, &mut admission)?,
            Next::Fact(None) => break None,
            Next::Fact(Some(Err(source))) => {
                break Some(ApplicationFault::from_port(
                    ApplicationFaultStage::FactStream,
                    source,
                ))
            }
            Next::Fact(Some(Ok(fact))) => {
                let (event, ticket) = admission.admit(fact)?.into_parts();
                match (event, ticket) {
                    (Some(ProviderEvent::ToolCall(call)), Some(ticket)) => {
                        let future = bindings.start_native_tool(call).map_err(|source| {
                            ApplicationFault::from_attempt(ApplicationFaultStage::Binding, source)
                        })?;
                        lanes.push(Box::pin(async move {
                            let output =
                                future.await.map_err(ComponentAttemptFault::native_tool)?;
                            Ok(CompletedToolLane { ticket, output })
                        }));
                    }
                    (Some(event), None) => {
                        await_binding_with_lanes(
                            bindings.dispatch(event),
                            &mut lanes,
                            &mut admission,
                        )
                        .await?;
                    }
                    (None, None) => {}
                    _ => {
                        return Err(ApplicationFault::terminal(
                            ApplicationFaultStage::Admission,
                            ApplicationFaultCode::Internal,
                            ApplicationFaultReason::FactProjectionInvariant,
                        ))
                    }
                }
            }
        }
    };

    drop(facts);
    while !lanes.is_empty() {
        finish_tool_lane(lanes.next().await, &mut admission)?;
    }
    if let Some(fault) = fact_stream_fault {
        return Err(fault);
    }
    admission.finish_normal()?;
    bindings
        .finish_normal()
        .await
        .map_err(|source| ApplicationFault::from_attempt(ApplicationFaultStage::Binding, source))?;
    Ok(())
}

fn finish_tool_lane(
    lane: Option<Result<CompletedToolLane, ComponentAttemptFault>>,
    admission: &mut ReactionAdmissionGuard<'_>,
) -> Result<(), ApplicationFault> {
    match lane {
        Some(Ok(completed)) => admission
            .stage_tool_output(completed.ticket, completed.output)
            .map_err(|source| {
                ApplicationFault::from_tool_output(ApplicationFaultStage::ToolOutput, source)
            }),
        Some(Err(source)) => Err(ApplicationFault::from_attempt(
            ApplicationFaultStage::ToolOutput,
            source,
        )),
        None => Ok(()),
    }
}

async fn await_binding_with_lanes<T, F>(
    future: F,
    lanes: &mut FuturesUnordered<ToolLane>,
    admission: &mut ReactionAdmissionGuard<'_>,
) -> Result<T, ApplicationFault>
where
    F: Future<Output = Result<T, ComponentAttemptFault>>,
{
    let mut future = Box::pin(future);
    loop {
        if lanes.is_empty() {
            return future.await.map_err(|source| {
                ApplicationFault::from_attempt(ApplicationFaultStage::Binding, source)
            });
        }
        tokio::select! {
            result = &mut future => return result.map_err(|source| {
                ApplicationFault::from_attempt(ApplicationFaultStage::Binding, source)
            }),
            lane = lanes.next() => finish_tool_lane(lane, admission)?,
        }
    }
}

/// Retry policy for a failure at the Application orchestration boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplicationFaultKind {
    Retryable,
    Terminal,
}

/// Payload-free structural category for an Application failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplicationFaultCode {
    Unavailable,
    Rejected,
    Protocol,
    InvalidConfiguration,
    Limit,
    Exhausted,
    Component,
    Internal,
}

/// The pipeline stage that observed an Application failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplicationFaultStage {
    Reaction,
    Declaration,
    Bootstrap,
    Reconcile,
    FramePrepare,
    Submit,
    FactStream,
    Admission,
    Binding,
    ToolOutput,
    PostReconcile,
}

/// Closed, payload-free cause for an Application failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplicationFaultReason {
    CancelledAfterHandoff,
    Port(ReactionPortFaultReason),
    InvalidDeclaration,
    InvalidFrameProfile,
    NamespaceExhausted,
    TargetIdentityChanged,
    FrameProfileChanged,
    EpochRegressed,
    AcceptedRevisionInNewEpoch,
    ContinuityResetWithoutEpochAdvance,
    ReplayReplacementUnsupported,
    AmbiguousProjectionProvenance,
    RevisionExhausted,
    PendingToolCall,
    CanonicalInvariant,
    FrameBudget,
    FrameInvariant,
    UnstableContinuity,
    ProfileChangedBeforeHandoff,
    ComponentRuntime,
    ComponentContract,
    ComponentInvariant,
    BindingLifecycle,
    EventHandler,
    ToolBinding,
    ToolLane,
    Admission(ReactionAdmissionReason),
    ToolOutput(ToolOutputStagingReason),
    FactProjectionInvariant,
}

/// Sanitized failure from the fixed-port Application pipeline.
///
/// Provider output, Component-authored values, tool names and arbitrary error
/// sources are classified and dropped before this boundary. User panics do not
/// enter this fault type; they unwind through the caller boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("application {stage:?} {kind:?}: {code:?}/{reason:?}")]
pub struct ApplicationFault {
    stage: ApplicationFaultStage,
    kind: ApplicationFaultKind,
    code: ApplicationFaultCode,
    reason: ApplicationFaultReason,
}

impl ApplicationFault {
    const fn terminal(
        stage: ApplicationFaultStage,
        code: ApplicationFaultCode,
        reason: ApplicationFaultReason,
    ) -> Self {
        Self {
            stage,
            kind: ApplicationFaultKind::Terminal,
            code,
            reason,
        }
    }

    fn from_port(stage: ApplicationFaultStage, fault: ReactionPortFault) -> Self {
        let kind = match fault.kind() {
            ReactionPortFaultKind::Retryable => ApplicationFaultKind::Retryable,
            ReactionPortFaultKind::Terminal => ApplicationFaultKind::Terminal,
        };
        let code = match fault.code() {
            ReactionPortFaultCode::Unavailable => ApplicationFaultCode::Unavailable,
            ReactionPortFaultCode::Rejected => ApplicationFaultCode::Rejected,
            ReactionPortFaultCode::Protocol => ApplicationFaultCode::Protocol,
            ReactionPortFaultCode::Limit => ApplicationFaultCode::Limit,
            ReactionPortFaultCode::Internal => ApplicationFaultCode::Internal,
        };
        Self {
            stage,
            kind,
            code,
            reason: ApplicationFaultReason::Port(fault.reason()),
        }
    }

    fn invalid_declaration(_fault: TargetDeclarationInvariantFault) -> Self {
        Self::terminal(
            ApplicationFaultStage::Declaration,
            ApplicationFaultCode::Protocol,
            ApplicationFaultReason::InvalidDeclaration,
        )
    }

    fn invalid_profile(_fault: InvalidFrameProfileFault) -> Self {
        Self::terminal(
            ApplicationFaultStage::Declaration,
            ApplicationFaultCode::InvalidConfiguration,
            ApplicationFaultReason::InvalidFrameProfile,
        )
    }

    fn from_session(stage: ApplicationFaultStage, fault: FrameSessionFault) -> Self {
        let (code, reason) = match fault {
            FrameSessionFault::NamespaceExhausted => (
                ApplicationFaultCode::Exhausted,
                ApplicationFaultReason::NamespaceExhausted,
            ),
            FrameSessionFault::TargetIdentityChanged => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::TargetIdentityChanged,
            ),
            FrameSessionFault::FrameProfileChanged => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::FrameProfileChanged,
            ),
            FrameSessionFault::EpochRegressed { .. } => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::EpochRegressed,
            ),
            FrameSessionFault::AcceptedRevisionInNewEpoch { .. } => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::AcceptedRevisionInNewEpoch,
            ),
            FrameSessionFault::ContinuityResetWithoutEpochAdvance { .. } => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::ContinuityResetWithoutEpochAdvance,
            ),
            FrameSessionFault::ReplayReplacementUnsupported => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::ReplayReplacementUnsupported,
            ),
            FrameSessionFault::ProjectionReconciliation(
                ProjectionReconciliationFault::AmbiguousProjectionProvenance,
            ) => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::AmbiguousProjectionProvenance,
            ),
            FrameSessionFault::RevisionExhausted => (
                ApplicationFaultCode::Exhausted,
                ApplicationFaultReason::RevisionExhausted,
            ),
            FrameSessionFault::PendingToolCall { .. } => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::PendingToolCall,
            ),
            FrameSessionFault::ToolOutput(fault) => return Self::from_tool_output(stage, fault),
            FrameSessionFault::Canonical(_) => (
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::CanonicalInvariant,
            ),
            FrameSessionFault::Budget(_) => (
                ApplicationFaultCode::Limit,
                ApplicationFaultReason::FrameBudget,
            ),
            FrameSessionFault::Invariant(_) => (
                ApplicationFaultCode::Internal,
                ApplicationFaultReason::FrameInvariant,
            ),
        };
        Self::terminal(stage, code, reason)
    }

    fn from_submit(fault: SubmitFault) -> Self {
        match fault {
            SubmitFault::ContinuityChanged => Self::terminal(
                ApplicationFaultStage::Submit,
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::UnstableContinuity,
            ),
            SubmitFault::ProfileChanged => Self::terminal(
                ApplicationFaultStage::Submit,
                ApplicationFaultCode::Protocol,
                ApplicationFaultReason::ProfileChangedBeforeHandoff,
            ),
            SubmitFault::Rejected(fault) => Self::from_port(ApplicationFaultStage::Submit, fault),
        }
    }

    fn from_component(stage: ApplicationFaultStage, fault: ComponentHostFault) -> Self {
        match fault {
            ComponentHostFault::MountGenerationExhausted
            | ComponentHostFault::RenderGenerationExhausted
            | ComponentHostFault::ProjectionRevisionExhausted => Self::terminal(
                stage,
                ApplicationFaultCode::Exhausted,
                ApplicationFaultReason::ComponentRuntime,
            ),
            ComponentHostFault::Signal { .. } => Self::terminal(
                stage,
                ApplicationFaultCode::Component,
                ApplicationFaultReason::ComponentRuntime,
            ),
            ComponentHostFault::TaskRuntime { .. } | ComponentHostFault::TaskPanicked => {
                Self::terminal(
                    stage,
                    ApplicationFaultCode::Internal,
                    ApplicationFaultReason::ComponentRuntime,
                )
            }
            ComponentHostFault::Attempt(fault) => Self::from_attempt(stage, fault),
        }
    }

    fn from_task_supervisor(
        stage: ApplicationFaultStage,
        _fault: MountTaskSupervisorError,
    ) -> Self {
        Self::terminal(
            stage,
            ApplicationFaultCode::Internal,
            ApplicationFaultReason::ComponentRuntime,
        )
    }

    fn from_task_start(stage: ApplicationFaultStage, _fault: SpawnError) -> Self {
        Self::terminal(
            stage,
            ApplicationFaultCode::Internal,
            ApplicationFaultReason::ComponentRuntime,
        )
    }

    fn from_driver_demand(stage: ApplicationFaultStage, _fault: DriverDemandFault) -> Self {
        Self::terminal(
            stage,
            ApplicationFaultCode::Unavailable,
            ApplicationFaultReason::ComponentRuntime,
        )
    }

    fn from_attempt(stage: ApplicationFaultStage, fault: ComponentAttemptFault) -> Self {
        let reason = match fault {
            ComponentAttemptFault::AfterStreamFinish | ComponentAttemptFault::AttemptInactive => {
                ApplicationFaultReason::BindingLifecycle
            }
            ComponentAttemptFault::InvalidListenerToken { .. }
            | ComponentAttemptFault::ForeignEventInput { .. }
            | ComponentAttemptFault::EventSelectorCollision { .. }
            | ComponentAttemptFault::SystemAttemptLocal { .. }
            | ComponentAttemptFault::HookCapabilityUnavailable { .. }
            | ComponentAttemptFault::StreamingMount { .. } => {
                ApplicationFaultReason::ComponentContract
            }
            ComponentAttemptFault::Signal { .. } => ApplicationFaultReason::ComponentRuntime,
            ComponentAttemptFault::ListenerDispatch { .. }
            | ComponentAttemptFault::StreamingInput { .. } => ApplicationFaultReason::EventHandler,
            ComponentAttemptFault::NativeToolBinding { .. } => ApplicationFaultReason::ToolBinding,
            ComponentAttemptFault::NativeToolLane { .. } => ApplicationFaultReason::ToolLane,
            ComponentAttemptFault::RuntimeInvariant { .. } | ComponentAttemptFault::Capture(_) => {
                ApplicationFaultReason::ComponentInvariant
            }
        };
        Self::terminal(stage, ApplicationFaultCode::Component, reason)
    }

    fn from_admission(fault: ReactionAdmissionFault) -> Self {
        if let ReactionAdmissionFault::ToolOutput(fault) = fault {
            return Self::from_tool_output(ApplicationFaultStage::Admission, fault);
        }
        let reason = fault.reason();
        let code = match reason {
            ReactionAdmissionReason::Budget => ApplicationFaultCode::Limit,
            ReactionAdmissionReason::InternalOutputOrder
            | ReactionAdmissionReason::BudgetStateMismatch => ApplicationFaultCode::Internal,
            _ => ApplicationFaultCode::Protocol,
        };
        Self::terminal(
            ApplicationFaultStage::Admission,
            code,
            ApplicationFaultReason::Admission(reason),
        )
    }

    fn from_tool_output(stage: ApplicationFaultStage, fault: ToolOutputStagingFault) -> Self {
        let reason = fault.reason();
        let code = match reason {
            ToolOutputStagingReason::RegistrationIdentityExhausted => {
                ApplicationFaultCode::Exhausted
            }
            ToolOutputStagingReason::Budget => ApplicationFaultCode::Limit,
            ToolOutputStagingReason::CanonicalInvariant => ApplicationFaultCode::Protocol,
            _ => ApplicationFaultCode::Protocol,
        };
        Self::terminal(stage, code, ApplicationFaultReason::ToolOutput(reason))
    }

    pub const fn stage(&self) -> ApplicationFaultStage {
        self.stage
    }

    pub const fn kind(&self) -> ApplicationFaultKind {
        self.kind
    }

    pub const fn code(&self) -> ApplicationFaultCode {
        self.code
    }

    pub const fn reason(&self) -> ApplicationFaultReason {
        self.reason
    }
}

impl From<ReactionAdmissionFault> for ApplicationFault {
    fn from(fault: ReactionAdmissionFault) -> Self {
        Self::from_admission(fault)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        error::Error,
        future::{poll_fn, Future},
        num::{NonZeroU128, NonZeroU64},
        panic::AssertUnwindSafe,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        task::{Context, Poll},
        time::Duration,
    };

    use agentview_derive::{component, view};
    use async_trait::async_trait;
    use futures::{stream, task::noop_waker, StreamExt};
    use tokio::sync::Notify;

    use super::*;
    use crate::component::execution::reaction::{
        Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
        ProviderFact, ProviderOutputKey, ProviderToolCall, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultReason, TargetContinuity, TargetDeclaration,
        TargetDeclarationInvariantFault, TargetEpoch, TargetIdentity,
    };
    use crate::component::{authoring::__private, prelude::*};
    use crate::transcript::{AssistantTextStatus, CanonicalInputItem};

    #[derive(Default)]
    struct HandoffProbe {
        declarations: AtomicUsize,
        polls: AtomicUsize,
        handoffs: AtomicUsize,
    }

    struct ProbePort {
        marker: usize,
        pending_polls: usize,
        reject: bool,
        probe: Arc<HandoffProbe>,
        declaration: Result<TargetDeclaration, ReactionPortFault>,
        order: Option<Arc<Mutex<Vec<&'static str>>>>,
    }

    struct EpochRegressionPort {
        identity: TargetIdentity,
        profile: FrameProfile,
        probe: Arc<HandoffProbe>,
    }

    #[async_trait]
    impl ReactionPort for EpochRegressionPort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            let declaration_index = self.probe.declarations.fetch_add(1, Ordering::Relaxed);
            let epoch = if declaration_index == 1 { 2 } else { 1 };
            Ok(TargetDeclaration::full(
                self.identity,
                TargetEpoch::new(NonZeroU64::new(epoch).unwrap()),
                self.profile.clone(),
            ))
        }

        async fn submit<'a>(
            &'a mut self,
            _frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            self.probe.polls.fetch_add(1, Ordering::Relaxed);
            Err(ReactionPortFault::retryable(
                ReactionPortFaultCode::Unavailable,
                ReactionPortFaultReason::Transport,
            )
            .into())
        }
    }

    #[async_trait]
    impl ReactionPort for ProbePort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            self.probe.declarations.fetch_add(1, Ordering::Relaxed);
            if let Some(order) = &self.order {
                order.lock().unwrap().push("declare");
            }
            self.declaration.clone()
        }

        async fn submit<'a>(
            &'a mut self,
            frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            let pending_polls = self.pending_polls;
            let reject = self.reject;
            let probe = Arc::clone(&self.probe);
            poll_fn(move |cx| {
                let poll = probe.polls.fetch_add(1, Ordering::Relaxed);
                if poll < pending_polls {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }

                Poll::Ready(())
            })
            .await;
            let declaration = self.declaration.clone().map_err(SubmitFault::Rejected)?;
            frame.check_handoff_precondition(&declaration)?;
            if reject {
                return Err(ReactionPortFault::terminal(
                    ReactionPortFaultCode::Rejected,
                    ReactionPortFaultReason::UpstreamRejected,
                )
                .into());
            }
            self.probe.handoffs.fetch_add(1, Ordering::Relaxed);
            Ok(Box::pin(stream::empty()))
        }
    }

    fn probe_port(pending_polls: usize, reject: bool) -> (ProbePort, Arc<HandoffProbe>) {
        let probe = Arc::new(HandoffProbe::default());
        (
            ProbePort {
                marker: 17,
                pending_polls,
                reject,
                probe: Arc::clone(&probe),
                declaration: Ok(target_declaration(valid_profile())),
                order: None,
            },
            probe,
        )
    }

    enum FactScript {
        Finite(Vec<Result<ProviderFact, ReactionPortFault>>),
        PendingAfter(Vec<Result<ProviderFact, ReactionPortFault>>),
        PanicOnDropPending,
    }

    struct PanicOnDropFactStream;

    impl futures::Stream for PanicOnDropFactStream {
        type Item = Result<ProviderFact, ReactionPortFault>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for PanicOnDropFactStream {
        fn drop(&mut self) {
            std::panic::panic_any("provider fact stream drop panic");
        }
    }

    #[derive(Default)]
    struct ScriptProbe {
        declarations: AtomicUsize,
        submissions: AtomicUsize,
        handoffs: AtomicUsize,
        bases: Mutex<Vec<FrameBasis>>,
        canonical_frames: Mutex<Vec<Vec<u8>>>,
        staged_input_counts: Mutex<Vec<usize>>,
        task_drop_observed_at_submit: Mutex<Vec<bool>>,
    }

    struct ScriptedPort {
        declaration: TargetDeclaration,
        scripts: VecDeque<FactScript>,
        continuity_rejections: usize,
        probe: Arc<ScriptProbe>,
        observed_task_drop: Option<Arc<std::sync::atomic::AtomicBool>>,
        before_stream: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    #[async_trait]
    impl ReactionPort for ScriptedPort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            self.probe.declarations.fetch_add(1, Ordering::Relaxed);
            Ok(self.declaration.clone())
        }

        async fn submit<'a>(
            &'a mut self,
            frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            self.probe.submissions.fetch_add(1, Ordering::Relaxed);
            if let Some(dropped) = &self.observed_task_drop {
                self.probe
                    .task_drop_observed_at_submit
                    .lock()
                    .unwrap()
                    .push(dropped.load(Ordering::Acquire));
            }
            if self.continuity_rejections > 0 {
                self.continuity_rejections -= 1;
                let next_epoch = self
                    .declaration
                    .continuity()
                    .epoch()
                    .get()
                    .get()
                    .checked_add(1)
                    .and_then(NonZeroU64::new)
                    .expect("test epoch space");
                self.declaration = TargetDeclaration::full(
                    self.declaration.identity(),
                    TargetEpoch::new(next_epoch),
                    self.declaration.profile().clone(),
                );
                frame.check_handoff_precondition(&self.declaration)?;
                unreachable!("continuity reset must reject the prepared Frame");
            }

            frame.check_handoff_precondition(&self.declaration)?;
            self.probe.bases.lock().unwrap().push(frame.basis());
            self.probe
                .canonical_frames
                .lock()
                .unwrap()
                .push(frame.submission().canonical_bytes().to_vec());
            self.probe
                .staged_input_counts
                .lock()
                .unwrap()
                .push(frame.submission().staged_inputs().len());
            let revision = frame.revision();
            let profile = frame.prepared_profile().clone();
            self.declaration = TargetDeclaration::resume(revision, profile);
            self.probe.handoffs.fetch_add(1, Ordering::Relaxed);

            if let Some(before_stream) = &self.before_stream {
                before_stream();
            }

            let script = self.scripts.pop_front().expect("test fact script");
            let facts: ProviderFactStream<'a> = match script {
                FactScript::Finite(facts) => Box::pin(stream::iter(facts)),
                FactScript::PendingAfter(facts) => Box::pin(
                    stream::iter(facts)
                        .chain(stream::pending::<Result<ProviderFact, ReactionPortFault>>()),
                ),
                FactScript::PanicOnDropPending => Box::pin(PanicOnDropFactStream),
            };
            Ok(facts)
        }
    }

    fn scripted_port(
        scripts: Vec<FactScript>,
        continuity_rejections: usize,
    ) -> (ScriptedPort, Arc<ScriptProbe>) {
        let probe = Arc::new(ScriptProbe::default());
        let profile = FrameProfile::new(valid_profile().constraints, FrameCapabilities::new(true));
        (
            ScriptedPort {
                declaration: target_declaration(profile),
                scripts: scripts.into(),
                continuity_rejections,
                probe: Arc::clone(&probe),
                observed_task_drop: None,
                before_stream: None,
            },
            probe,
        )
    }

    fn completed_reaction() -> FactScript {
        FactScript::Finite(vec![Ok(ProviderFact::ReactionCompleted {
            primary_text: None,
        })])
    }

    fn tool_fact(output: u64, ordinal: u64, call_id: &str, name: &str) -> ProviderFact {
        ProviderFact::ToolCall {
            output: ProviderOutputKey::new(output),
            ordinal,
            call: ProviderToolCall::new(call_id, name, "{}").unwrap(),
        }
    }

    #[component]
    fn stateful_tool_component() -> Component {
        let state = use_signal(|| String::from("before"));
        let value = state.with(Clone::clone).expect("mounted state");
        let update = state.clone();
        view! {
            state { "{value}" }
            {
                NativeToolCall::named("change").on_call(move |call| {
                    let update = update.clone();
                    async move {
                        update.set(String::from("after")).map_err(|fault| fault.to_string())?;
                        Ok::<_, String>(call.output("changed"))
                    }
                })
            }
        }
    }

    #[component]
    fn stateful_provider_handler(events: Arc<Mutex<Vec<&'static str>>>) -> Component {
        let state = use_signal(|| String::from("before"));
        let rendered = state.with(Clone::clone).expect("mounted state");
        let update = state.clone();
        use_provider_event_handler(ProviderEvent::TEXT, move |event| {
            let update = update.clone();
            let events = Arc::clone(&events);
            async move {
                match event {
                    crate::llm_call::TextTurnEvent::TextDelta(_) => {
                        events.lock().unwrap().push("delta");
                    }
                    crate::llm_call::TextTurnEvent::TextComplete(text) => {
                        events.lock().unwrap().push("complete");
                        update.set(text)?;
                    }
                }
                Ok::<(), SignalAccessError>(())
            }
        });
        view! { state { "{rendered}" } }
    }

    #[derive(Clone)]
    struct DemandComponentProps {
        exported_demand: Arc<Mutex<Option<ReactionRequest>>>,
        exported_signal: Arc<Mutex<Option<Signal<String>>>>,
    }

    #[component]
    fn demand_component(props: DemandComponentProps) -> Component {
        let state = use_signal(|| String::from("before"));
        let demand = use_reaction_request();
        *props.exported_demand.lock().unwrap() = Some(demand);
        *props.exported_signal.lock().unwrap() = Some(state.clone());
        let value = state.with(Clone::clone).expect("mounted demand state");
        view! { demand_state { "{value}" } }
    }

    #[derive(Clone)]
    struct FutureLifecycleProps {
        starts: Arc<AtomicUsize>,
        release: Arc<Notify>,
    }

    #[component]
    fn future_lifecycle_component(props: FutureLifecycleProps) -> Component {
        let starts = Arc::clone(&props.starts);
        let release = Arc::clone(&props.release);
        use_future(move || async move {
            starts.fetch_add(1, Ordering::AcqRel);
            release.notified().await;
        });
        view! { future_lifecycle { "mounted" } }
    }

    #[derive(Clone)]
    struct RuntimeBoundaryProps {
        starts: Arc<AtomicUsize>,
        release: Arc<Notify>,
        exported_demand: Arc<Mutex<Option<ReactionRequest>>>,
    }

    #[component]
    fn runtime_boundary_component(props: RuntimeBoundaryProps) -> Component {
        *props.exported_demand.lock().unwrap() = Some(use_reaction_request());
        let starts = Arc::clone(&props.starts);
        let release = Arc::clone(&props.release);
        use_future(move || async move {
            starts.fetch_add(1, Ordering::AcqRel);
            release.notified().await;
        });
        view! { runtime_boundary { "mounted" } }
    }

    #[derive(Clone)]
    struct CoroutineLifecycleProps {
        exported: std::sync::mpsc::Sender<Coroutine<u32>>,
        received: tokio::sync::mpsc::UnboundedSender<[u32; 2]>,
    }

    #[component]
    fn coroutine_lifecycle_component(props: CoroutineLifecycleProps) -> Component {
        let received = props.received.clone();
        let coroutine = use_coroutine(1, move |mut inbox| async move {
            let first = inbox.recv().await.expect("first coroutine message");
            let second = inbox.recv().await.expect("second coroutine message");
            let _ = received.send([first, second]);
        });
        props
            .exported
            .send(coroutine)
            .expect("test coroutine receiver remains mounted");
        view! { coroutine_lifecycle { "mounted" } }
    }

    struct TaskDropProbe {
        dropped: Arc<std::sync::atomic::AtomicBool>,
        gate: Option<Arc<TaskDropGate>>,
        panic: Option<&'static str>,
    }

    struct TaskDropGate {
        entered: std::sync::atomic::AtomicBool,
        released: Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl TaskDropGate {
        fn new() -> Self {
            Self {
                entered: std::sync::atomic::AtomicBool::new(false),
                released: Mutex::new(false),
                changed: std::sync::Condvar::new(),
            }
        }

        fn block_drop(&self) {
            self.entered.store(true, Ordering::Release);
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.changed.wait(released).unwrap();
            }
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    impl Drop for TaskDropProbe {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.block_drop();
            }
            self.dropped.store(true, Ordering::Release);
            if let Some(payload) = self.panic {
                std::panic::panic_any(payload);
            }
        }
    }

    #[derive(Clone)]
    struct PanickingFutureProps {
        starts: Arc<AtomicUsize>,
        release: Arc<Notify>,
        sibling_dropped: Arc<std::sync::atomic::AtomicBool>,
        sibling_drop_gate: Option<Arc<TaskDropGate>>,
    }

    #[component]
    fn panicking_future_component(props: PanickingFutureProps) -> Component {
        let primary_starts = Arc::clone(&props.starts);
        let release = Arc::clone(&props.release);
        use_future(move || async move {
            primary_starts.fetch_add(1, Ordering::AcqRel);
            release.notified().await;
            panic!("component task panic payload");
        });

        let sibling_starts = Arc::clone(&props.starts);
        let sibling_dropped = Arc::clone(&props.sibling_dropped);
        let sibling_drop_gate = props.sibling_drop_gate.clone();
        use_future(move || async move {
            let _drop_probe = TaskDropProbe {
                dropped: sibling_dropped,
                gate: sibling_drop_gate,
                panic: None,
            };
            sibling_starts.fetch_add(1, Ordering::AcqRel);
            std::future::pending::<()>().await;
        });
        view! { panicking_future { "mounted" } }
    }

    #[derive(Clone)]
    struct SpawnHandlerProps {
        observed: tokio::sync::mpsc::UnboundedSender<&'static str>,
    }

    #[component]
    fn spawn_handler_component(props: SpawnHandlerProps) -> Component {
        use_provider_event_handler(ProviderEvent::TEXT, move |_event| {
            let invocation_sender = props.observed.clone();
            let invocation = spawn(async move {
                let _ = invocation_sender.send("invocation");
            });
            let future_sender = props.observed.clone();
            async move {
                invocation.map_err(|fault| fault.to_string())?;
                spawn(async move {
                    let _ = future_sender.send("future");
                })
                .map_err(|fault| fault.to_string())?;
                Ok::<(), String>(())
            }
        });
        view! { spawn_handler { "mounted" } }
    }

    #[derive(Clone)]
    struct RetirementChildProps {
        started: Arc<std::sync::atomic::AtomicBool>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
        drop_gate: Option<Arc<TaskDropGate>>,
        panic_on_drop: Option<&'static str>,
    }

    #[component]
    fn retirement_child(props: RetirementChildProps) -> Component {
        let started = Arc::clone(&props.started);
        let dropped = Arc::clone(&props.dropped);
        let gate = props.drop_gate.clone();
        let panic = props.panic_on_drop;
        use_future(move || async move {
            let _drop_probe = TaskDropProbe {
                dropped,
                gate,
                panic,
            };
            started.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        view! { retiring_child { "mounted" } }
    }

    #[derive(Clone)]
    struct RetirementRootProps {
        exported: Arc<Mutex<Option<Signal<bool>>>>,
        started: Arc<std::sync::atomic::AtomicBool>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
        drop_gate: Option<Arc<TaskDropGate>>,
        panic_on_drop: Option<&'static str>,
    }

    #[component]
    fn retirement_root(props: RetirementRootProps) -> Component {
        let visible = use_signal(|| true);
        *props.exported.lock().unwrap() = Some(visible.clone());
        let child = if visible.with(|visible| *visible).unwrap() {
            retirement_child(RetirementChildProps {
                started: Arc::clone(&props.started),
                dropped: Arc::clone(&props.dropped),
                drop_gate: props.drop_gate.clone(),
                panic_on_drop: props.panic_on_drop,
            })
        } else {
            __private::fragment(Vec::new())
        };
        view! {
            retirement_root { "mounted" }
            {child}
        }
    }

    #[derive(Clone)]
    struct ReconcilePanicProps {
        exported: Arc<Mutex<Option<Signal<bool>>>>,
        task_started: Arc<std::sync::atomic::AtomicBool>,
        panic_release: Arc<Notify>,
        render_gate: Arc<TaskDropGate>,
        child_started: Arc<std::sync::atomic::AtomicBool>,
        child_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[component]
    fn reconcile_panic_root(props: ReconcilePanicProps) -> Component {
        let visible = use_signal(|| true);
        *props.exported.lock().unwrap() = Some(visible.clone());

        let task_started = Arc::clone(&props.task_started);
        let panic_release = Arc::clone(&props.panic_release);
        use_future(move || async move {
            task_started.store(true, Ordering::Release);
            panic_release.notified().await;
            panic!("panic during reconcile");
        });

        let visible = visible.with(|visible| *visible).unwrap();
        if !visible {
            props.render_gate.block_drop();
        }
        let child = if visible {
            retirement_child(RetirementChildProps {
                started: Arc::clone(&props.child_started),
                dropped: Arc::clone(&props.child_dropped),
                drop_gate: None,
                panic_on_drop: None,
            })
        } else {
            __private::fragment(Vec::new())
        };
        view! {
            reconcile_panic_root { "mounted" }
            {child}
        }
    }

    #[derive(Clone)]
    struct DeferredPanicProps {
        started: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<Notify>,
    }

    #[component]
    fn deferred_panic_component(props: DeferredPanicProps) -> Component {
        use_future(move || async move {
            props.started.store(true, Ordering::Release);
            props.release.notified().await;
            panic!("deferred bootstrap task panic");
        });
        view! { deferred_panic { "mounted" } }
    }

    #[component]
    fn immediate_panic_component() -> Component {
        use_future(|| async {
            panic!("immediate bootstrap task panic");
        });
        view! { immediate_panic { "mounted" } }
    }

    fn rendered_text(projection: &super::super::RenderedProjection) -> String {
        projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => {
                    Some(crate::pom_renderer::render_pom_document(pom).unwrap())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn valid_profile() -> FrameProfile {
        FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 4_096,
                max_component_bytes: 1_024,
                context_window_tokens: Some(8_192),
                reserved_output_tokens: Some(1_024),
            },
            FrameCapabilities::NONE,
        )
    }

    fn target_declaration(profile: FrameProfile) -> TargetDeclaration {
        TargetDeclaration::full(
            TargetIdentity::new(NonZeroU128::new(1).unwrap()),
            TargetEpoch::new(NonZeroU64::new(1).unwrap()),
            profile,
        )
    }

    fn prepared_frame(application: &mut Application<ProbePort>) -> PreparedFrame {
        let projection = application.components.current_projection().unwrap().clone();
        application
            .session
            .prepare(&application.declaration, &projection)
            .unwrap()
    }

    fn assert_session_uncommitted(application: &Application<ProbePort>) {
        assert!(application
            .session
            .canonical_history
            .transcript
            .items()
            .is_empty());
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            0
        );
        assert_eq!(application.session.target_delivery.committed_revision, None);
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        future.poll(&mut cx)
    }

    #[test]
    fn mount_declares_before_render_and_retains_complete_projection_and_target() {
        let renders = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&renders);
        let order = Arc::new(Mutex::new(Vec::new()));
        let observed_order = Arc::clone(&order);
        let (mut port, probe) = probe_port(0, false);
        port.order = Some(Arc::clone(&order));
        let expected_declaration = port.declaration.as_ref().unwrap().clone();

        let application = Application::mount(
            move || {
                observed_order.lock().unwrap().push("render");
                observed_renders.fetch_add(1, Ordering::Relaxed);
                __private::fragment(Vec::new())
            },
            port,
        )
        .unwrap();

        assert_eq!(application.port.marker, 17);
        assert_eq!(*order.lock().unwrap(), ["declare", "render"]);
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 1);
        assert_eq!(probe.polls.load(Ordering::Relaxed), 0);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_eq!(renders.load(Ordering::Relaxed), 1);
        let projection = application.components.current_projection().unwrap();
        projection.to_transcript().unwrap();
        assert!(!application.components.is_dirty());
        assert_eq!(application.declaration, expected_declaration);
        assert_eq!(application.declaration.identity().get().get(), 1);
        assert_eq!(application.declaration.continuity().epoch().get().get(), 1);
        assert_eq!(application.declaration.profile(), &valid_profile());
        let _session = &application.session;
    }

    #[tokio::test]
    async fn projection_snapshot_is_read_only_and_reports_pending_reconcile() {
        let (port, probe) = scripted_port(vec![completed_reaction()], 0);
        let mut application =
            Application::mount(|| view! { latest { "committed" } }, port).unwrap();
        let committed = {
            let snapshot = application.current_projection();
            assert_eq!(snapshot.revision(), 1);
            assert!(!snapshot.is_dirty());
            snapshot.projection().clone()
        };

        let root = Arc::clone(application.components.props());
        application.components.set_props(root);

        let dirty = application.current_projection();
        assert_eq!(dirty.revision(), 1);
        assert!(dirty.is_dirty());
        assert_eq!(dirty.projection(), &committed);
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 1);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 0);

        application.react().await.unwrap();

        let reconciled = application.current_projection();
        assert_eq!(reconciled.revision(), 2);
        assert!(!reconciled.is_dirty());
        assert_eq!(reconciled.projection(), &committed);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn declaration_failure_prevents_profile_validation_render_and_submit() {
        let renders = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&renders);
        let (mut port, probe) = probe_port(0, false);
        port.declaration = Err(ReactionPortFault::retryable(
            ReactionPortFaultCode::Unavailable,
            ReactionPortFaultReason::Declaration,
        ));

        let fault = match Application::mount(
            move || {
                observed_renders.fetch_add(1, Ordering::Relaxed);
                __private::fragment(Vec::new())
            },
            port,
        ) {
            Ok(_) => panic!("declaration failure unexpectedly mounted"),
            Err(fault) => fault,
        };

        assert_eq!(fault.stage(), ApplicationFaultStage::Declaration);
        assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
        assert_eq!(fault.code(), ApplicationFaultCode::Unavailable);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::Port(ReactionPortFaultReason::Declaration)
        );
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 1);
        assert_eq!(probe.polls.load(Ordering::Relaxed), 0);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_eq!(renders.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn contradictory_resume_declarations_prevent_root_render() {
        let identity = TargetIdentity::new(NonZeroU128::new(1).unwrap());
        let other_identity = TargetIdentity::new(NonZeroU128::new(2).unwrap());
        let epoch = TargetEpoch::new(NonZeroU64::new(1).unwrap());
        let other_epoch = TargetEpoch::new(NonZeroU64::new(2).unwrap());
        let cases = [
            (
                TargetDeclaration::from_raw(
                    identity,
                    TargetContinuity::Accepted {
                        epoch,
                        revision: FrameRevision::new(
                            NonZeroU128::new(91).unwrap(),
                            other_identity,
                            epoch,
                            NonZeroU64::new(7).unwrap(),
                        ),
                    },
                    valid_profile(),
                ),
                TargetDeclarationInvariantFault::AcceptedRevisionTargetMismatch,
            ),
            (
                TargetDeclaration::from_raw(
                    identity,
                    TargetContinuity::Accepted {
                        epoch: other_epoch,
                        revision: FrameRevision::new(
                            NonZeroU128::new(92).unwrap(),
                            identity,
                            epoch,
                            NonZeroU64::new(8).unwrap(),
                        ),
                    },
                    valid_profile(),
                ),
                TargetDeclarationInvariantFault::AcceptedRevisionEpochMismatch,
            ),
        ];

        for (declaration, _expected) in cases {
            let renders = Arc::new(AtomicUsize::new(0));
            let observed_renders = Arc::clone(&renders);
            let (mut port, probe) = probe_port(0, false);
            port.declaration = Ok(declaration);

            let fault = match Application::mount(
                move || {
                    observed_renders.fetch_add(1, Ordering::Relaxed);
                    __private::fragment(Vec::new())
                },
                port,
            ) {
                Ok(_) => panic!("contradictory declaration unexpectedly mounted"),
                Err(fault) => fault,
            };

            assert_eq!(fault.stage(), ApplicationFaultStage::Declaration);
            assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
            assert_eq!(fault.code(), ApplicationFaultCode::Protocol);
            assert_eq!(fault.reason(), ApplicationFaultReason::InvalidDeclaration);
            assert_eq!(probe.declarations.load(Ordering::Relaxed), 1);
            assert_eq!(probe.polls.load(Ordering::Relaxed), 0);
            assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
            assert_eq!(renders.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn invalid_profile_prevents_bootstrap_render_and_submit() {
        let renders = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&renders);
        let (mut port, probe) = probe_port(0, false);
        port.declaration = Ok(target_declaration(FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 128,
                max_component_bytes: 100,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::NONE,
        )));

        let fault = match Application::mount(
            move || {
                observed_renders.fetch_add(1, Ordering::Relaxed);
                __private::fragment(Vec::new())
            },
            port,
        ) {
            Ok(_) => panic!("invalid profile unexpectedly mounted"),
            Err(fault) => fault,
        };

        assert_eq!(fault.stage(), ApplicationFaultStage::Declaration);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::InvalidConfiguration);
        assert_eq!(fault.reason(), ApplicationFaultReason::InvalidFrameProfile);
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 1);
        assert_eq!(probe.polls.load(Ordering::Relaxed), 0);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_eq!(renders.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn bootstrap_render_panic_propagates_without_submit() {
        let (port, probe) = probe_port(0, false);

        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            Application::mount(|| -> Component { panic!("bootstrap root failed") }, port)
        }));

        assert!(panic.is_err());
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 1);
        assert_eq!(probe.polls.load(Ordering::Relaxed), 0);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn application_fault_drops_authored_and_provider_payloads() {
        const SENTINEL: &str = "sensitive-authored-or-provider-payload";
        let faults = [
            ApplicationFault::from_attempt(
                ApplicationFaultStage::Binding,
                ComponentAttemptFault::ListenerDispatch {
                    message: SENTINEL.to_owned(),
                },
            ),
            ApplicationFault::from_session(
                ApplicationFaultStage::FramePrepare,
                FrameSessionFault::PendingToolCall {
                    call_id: SENTINEL.to_owned(),
                },
            ),
        ];

        for fault in faults {
            assert!(!fault.to_string().contains(SENTINEL));
            assert!(!format!("{fault:?}").contains(SENTINEL));
            assert!(fault.source().is_none());
        }
    }

    #[test]
    fn nested_admission_tool_output_fault_keeps_its_structural_classification() {
        let fault = ApplicationFault::from_admission(ReactionAdmissionFault::ToolOutput(
            ToolOutputStagingFault::RegistrationIdentityExhausted,
        ));

        assert_eq!(fault.stage(), ApplicationFaultStage::Admission);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::Exhausted);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::ToolOutput(
                ToolOutputStagingReason::RegistrationIdentityExhausted
            )
        );
    }

    #[test]
    fn replay_replacement_fault_keeps_its_structural_classification() {
        let fault = ApplicationFault::from_session(
            ApplicationFaultStage::FramePrepare,
            FrameSessionFault::ReplayReplacementUnsupported,
        );

        assert_eq!(fault.stage(), ApplicationFaultStage::FramePrepare);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::Protocol);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::ReplayReplacementUnsupported
        );
    }

    #[test]
    fn ambiguous_projection_provenance_fault_keeps_its_structural_classification() {
        let fault = ApplicationFault::from_session(
            ApplicationFaultStage::FramePrepare,
            FrameSessionFault::ProjectionReconciliation(
                ProjectionReconciliationFault::AmbiguousProjectionProvenance,
            ),
        );

        assert_eq!(fault.stage(), ApplicationFaultStage::FramePrepare);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::Protocol);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::AmbiguousProjectionProvenance
        );
    }

    #[test]
    fn frame_session_keeps_history_and_delivery_as_distinct_owners() {
        let declaration = target_declaration(valid_profile());
        let session = FrameSession::new(&declaration).unwrap();
        let FrameSession {
            canonical_history,
            target_delivery,
            ..
        } = session;

        assert!(canonical_history.transcript.items().is_empty());
        assert_eq!(target_delivery.committed_revision, None);
    }

    #[test]
    fn every_pending_cancellation_point_preserves_session_state() {
        for pending_polls in 1..=3 {
            let (port, probe) = probe_port(pending_polls, false);
            let mut application =
                Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
            let prepared = prepared_frame(&mut application);
            let mut submit = Box::pin(submit_prepared_frame(
                &mut application.port,
                &mut application.session,
                prepared,
            ));

            for _ in 0..pending_polls {
                assert!(poll_once(submit.as_mut()).is_pending());
            }
            drop(submit);

            assert_eq!(probe.polls.load(Ordering::Relaxed), pending_polls);
            assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
            assert_session_uncommitted(&application);
        }
    }

    #[tokio::test]
    async fn pre_handoff_react_cancellation_keeps_application_reusable() {
        let (port, probe) = probe_port(1, false);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let mut reaction = Box::pin(application.react());

        assert!(poll_once(reaction.as_mut()).is_pending());
        drop(reaction);

        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_session_uncommitted(&application);

        let fault = application.react().await.unwrap_err();
        assert_eq!(fault.stage(), ApplicationFaultStage::Admission);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::Admission(ReactionAdmissionReason::MissingCompletion)
        );
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn observed_higher_epoch_survives_pre_handoff_rejection() {
        let probe = Arc::new(HandoffProbe::default());
        let renders = Arc::new(AtomicUsize::new(0));
        let observed_renders = Arc::clone(&renders);
        let mut application = Application::mount(
            move || {
                observed_renders.fetch_add(1, Ordering::Relaxed);
                __private::fragment(Vec::new())
            },
            EpochRegressionPort {
                identity: TargetIdentity::new(NonZeroU128::new(77).unwrap()),
                profile: valid_profile(),
                probe: Arc::clone(&probe),
            },
        )
        .unwrap();

        let first = application.react().await.unwrap_err();
        assert_eq!(first.stage(), ApplicationFaultStage::Submit);
        assert_eq!(first.kind(), ApplicationFaultKind::Retryable);
        let renders_after_rejection = renders.load(Ordering::Relaxed);

        let regressed = application.react().await.unwrap_err();
        assert_eq!(regressed.stage(), ApplicationFaultStage::Declaration);
        assert_eq!(regressed.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(regressed.reason(), ApplicationFaultReason::EpochRegressed);
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 3);
        assert_eq!(probe.polls.load(Ordering::Relaxed), 1);
        assert_eq!(renders.load(Ordering::Relaxed), renders_after_rejection);
    }

    #[test]
    fn ready_error_preserves_session_state() {
        let (port, probe) = probe_port(0, true);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let prepared = prepared_frame(&mut application);
        let mut submit = Box::pin(submit_prepared_frame(
            &mut application.port,
            &mut application.session,
            prepared,
        ));

        assert!(matches!(poll_once(submit.as_mut()), Poll::Ready(Err(_))));
        drop(submit);

        assert_eq!(probe.polls.load(Ordering::Relaxed), 1);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_session_uncommitted(&application);
    }

    #[test]
    fn pending_then_ready_error_preserves_session_state() {
        let (port, probe) = probe_port(2, true);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let prepared = prepared_frame(&mut application);
        let mut submit = Box::pin(submit_prepared_frame(
            &mut application.port,
            &mut application.session,
            prepared,
        ));

        assert!(poll_once(submit.as_mut()).is_pending());
        assert!(poll_once(submit.as_mut()).is_pending());
        assert!(matches!(poll_once(submit.as_mut()), Poll::Ready(Err(_))));
        drop(submit);

        assert_eq!(probe.polls.load(Ordering::Relaxed), 3);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_session_uncommitted(&application);
    }

    #[test]
    fn same_future_transitions_from_pending_to_crossing_ready() {
        let (port, probe) = probe_port(1, false);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let prepared = prepared_frame(&mut application);
        let mut submit = Box::pin(submit_prepared_frame(
            &mut application.port,
            &mut application.session,
            prepared,
        ));

        assert!(poll_once(submit.as_mut()).is_pending());
        let stream = match poll_once(submit.as_mut()) {
            Poll::Ready(Ok(stream)) => stream,
            Poll::Ready(Err(fault)) => panic!("unexpected pre-handoff fault: {fault}"),
            Poll::Pending => panic!("crossing poll returned Pending"),
        };
        drop(submit);

        assert_eq!(probe.polls.load(Ordering::Relaxed), 2);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert!(application
            .session
            .target_delivery
            .committed_revision
            .is_some());
        drop(stream);
    }

    #[test]
    fn repeated_identical_declaration_keeps_prepared_frame_valid() {
        let (port, probe) = probe_port(0, false);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let prepared = prepared_frame(&mut application);

        let repeated = application.port.declare().unwrap();
        assert_eq!(repeated, application.declaration);
        let mut submit = Box::pin(submit_prepared_frame(
            &mut application.port,
            &mut application.session,
            prepared,
        ));
        let stream = match poll_once(submit.as_mut()) {
            Poll::Ready(Ok(stream)) => stream,
            Poll::Ready(Err(fault)) => panic!("idempotent declaration invalidated Frame: {fault}"),
            Poll::Pending => panic!("immediate crossing unexpectedly returned Pending"),
        };
        drop(submit);

        assert_eq!(probe.declarations.load(Ordering::Relaxed), 2);
        assert_eq!(probe.polls.load(Ordering::Relaxed), 1);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert!(application
            .session
            .target_delivery
            .committed_revision
            .is_some());
        drop(stream);
    }

    #[test]
    fn profile_only_change_rejects_before_handoff_or_commit() {
        let (port, probe) = probe_port(0, false);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let prepared = prepared_frame(&mut application);
        let mut changed_profile = valid_profile();
        changed_profile.constraints.context_window_tokens = Some(16_384);
        application.port.declaration = Ok(target_declaration(changed_profile));
        let mut submit = Box::pin(submit_prepared_frame(
            &mut application.port,
            &mut application.session,
            prepared,
        ));

        assert!(matches!(
            poll_once(submit.as_mut()),
            Poll::Ready(Err(SubmitFault::ProfileChanged))
        ));
        drop(submit);

        assert_eq!(probe.polls.load(Ordering::Relaxed), 1);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert_session_uncommitted(&application);
    }

    #[test]
    fn crossing_poll_commits_before_returning_stream() {
        let (port, probe) = probe_port(0, false);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();
        let prepared = prepared_frame(&mut application);
        let mut submit = Box::pin(submit_prepared_frame(
            &mut application.port,
            &mut application.session,
            prepared,
        ));

        let stream = match poll_once(submit.as_mut()) {
            Poll::Ready(Ok(stream)) => stream,
            Poll::Ready(Err(fault)) => panic!("unexpected pre-handoff fault: {fault}"),
            Poll::Pending => panic!("crossing poll returned Pending"),
        };
        assert_eq!(probe.polls.load(Ordering::Relaxed), 1);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        drop(submit);

        assert!(application
            .session
            .canonical_history
            .transcript
            .items()
            .is_empty());
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            0
        );
        assert!(application
            .session
            .target_delivery
            .committed_revision
            .is_some());
        assert!(application.components.current_projection().is_some());
        drop(stream);
    }

    #[tokio::test]
    async fn react_uses_full_then_matching_head_delta() {
        let (port, probe) = scripted_port(vec![completed_reaction(), completed_reaction()], 0);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();

        application.react().await.unwrap();
        application.react().await.unwrap();

        let bases = probe.bases.lock().unwrap();
        assert_eq!(bases.len(), 2);
        assert_eq!(bases[0], FrameBasis::Full);
        assert!(matches!(bases[1], FrameBasis::DeltaFrom(_)));
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 3);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 2);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn continuity_race_redeclares_and_reprepares_exactly_once() {
        let (port, probe) = scripted_port(vec![completed_reaction()], 1);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();

        application.react().await.unwrap();

        assert_eq!(probe.declarations.load(Ordering::Relaxed), 3);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 2);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert_eq!(*probe.bases.lock().unwrap(), [FrameBasis::Full]);
        assert!(application
            .session
            .target_delivery
            .committed_revision
            .is_some());
    }

    #[tokio::test]
    async fn a_second_continuity_race_fails_without_handoff_or_commit() {
        let (port, probe) = scripted_port(vec![completed_reaction()], 2);
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();

        let fault = application.react().await.unwrap_err();

        assert_eq!(fault.stage(), ApplicationFaultStage::Submit);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::Protocol);
        assert_eq!(fault.reason(), ApplicationFaultReason::UnstableContinuity);
        assert_eq!(probe.declarations.load(Ordering::Relaxed), 3);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 2);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 0);
        assert!(probe.bases.lock().unwrap().is_empty());
        assert!(application
            .session
            .canonical_history
            .transcript
            .items()
            .is_empty());
        assert_eq!(application.session.target_delivery.committed_revision, None);
    }

    #[tokio::test]
    async fn post_handoff_stream_fault_keeps_frame_and_interrupts_partial_text() {
        let stream_fault = ReactionPortFault::terminal(
            ReactionPortFaultCode::Protocol,
            ReactionPortFaultReason::StreamTransport,
        );
        let (port, probe) = scripted_port(
            vec![FactScript::Finite(vec![
                Ok(ProviderFact::TextDelta {
                    output: ProviderOutputKey::new(7),
                    phase: None,
                    delta: String::from("partial"),
                }),
                Err(stream_fault),
            ])],
            0,
        );
        let mut application = Application::mount(|| __private::fragment(Vec::new()), port).unwrap();

        let fault = application.react().await.unwrap_err();

        assert_eq!(fault.stage(), ApplicationFaultStage::FactStream);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::Protocol);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::Port(ReactionPortFaultReason::StreamTransport)
        );
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert!(application
            .session
            .target_delivery
            .committed_revision
            .is_some());
        assert!(application
            .session
            .canonical_history
            .transcript
            .items()
            .iter()
            .any(|item| matches!(
                item,
                CanonicalInputItem::AssistantText {
                    text,
                    status: AssistantTextStatus::Interrupted,
                    ..
                } if text == "partial"
            )));
    }

    #[tokio::test]
    async fn stream_fault_drains_started_tool_lanes_before_returning() {
        let lane_started = Arc::new(Notify::new());
        let release_lane = Arc::new(Notify::new());
        let observed_start = Arc::clone(&lane_started);
        let observed_release = Arc::clone(&release_lane);
        let stream_fault = ReactionPortFault::retryable(
            ReactionPortFaultCode::Unavailable,
            ReactionPortFaultReason::StreamTransport,
        );
        let (port, probe) = scripted_port(
            vec![
                FactScript::Finite(vec![
                    Ok(tool_fact(9, 1, "call-after-stream-fault", "delayed")),
                    Err(stream_fault),
                ]),
                completed_reaction(),
            ],
            0,
        );
        let mut application = Application::mount(
            move || {
                let lane_started = Arc::clone(&observed_start);
                let release_lane = Arc::clone(&observed_release);
                NativeToolCall::named("delayed").on_call(move |call| {
                    let lane_started = Arc::clone(&lane_started);
                    let release_lane = Arc::clone(&release_lane);
                    async move {
                        lane_started.notify_one();
                        release_lane.notified().await;
                        Ok::<_, String>(call.output("completed-after-stream-fault"))
                    }
                })
            },
            port,
        )
        .unwrap();
        let started = lane_started.notified();
        tokio::pin!(started);
        let mut reaction = Box::pin(application.react());

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut reaction => panic!("reaction returned before lane cleanup: {result:?}"),
                _ = &mut started => {}
            }
        })
        .await
        .expect("tool lane start");
        release_lane.notify_one();

        let fault = tokio::time::timeout(Duration::from_secs(1), reaction)
            .await
            .expect("reaction cleanup")
            .unwrap_err();
        assert_eq!(fault.stage(), ApplicationFaultStage::FactStream);
        assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            1
        );

        application.react().await.unwrap();

        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 2);
        assert_eq!(*probe.staged_input_counts.lock().unwrap(), [0, 1]);
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn tool_lane_progresses_while_fact_stream_is_pending() {
        let completed = Arc::new(Notify::new());
        let observed_completion = Arc::clone(&completed);
        let (port, probe) = scripted_port(
            vec![FactScript::PendingAfter(vec![Ok(tool_fact(
                3,
                1,
                "call-pending",
                "wait",
            ))])],
            0,
        );
        let mut application = Application::mount(
            move || {
                let completed = Arc::clone(&observed_completion);
                NativeToolCall::named("wait").on_call(move |call| {
                    let completed = Arc::clone(&completed);
                    async move {
                        completed.notify_one();
                        Ok::<_, String>(call.output("ready"))
                    }
                })
            },
            port,
        )
        .unwrap();
        let notified = completed.notified();
        tokio::pin!(notified);
        let mut reaction = Box::pin(application.react());

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut reaction => panic!("pending stream reaction ended: {result:?}"),
                _ = &mut notified => {}
            }
        })
        .await
        .expect("tool lane completion");
        drop(reaction);

        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert!(application
            .session
            .canonical_history
            .transcript
            .items()
            .iter()
            .any(|item| matches!(
                item,
                CanonicalInputItem::ToolCall { call_id, .. } if call_id == "call-pending"
            )));
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn post_handoff_cancellation_terminates_application_before_next_declaration() {
        let lane_started = Arc::new(Notify::new());
        let observed_start = Arc::clone(&lane_started);
        let (port, probe) = scripted_port(
            vec![FactScript::PendingAfter(vec![Ok(tool_fact(
                11,
                1,
                "call-cancelled",
                "never-finishes",
            ))])],
            0,
        );
        let mut application = Application::mount(
            move || {
                let lane_started = Arc::clone(&observed_start);
                NativeToolCall::named("never-finishes").on_call(move |call| {
                    let lane_started = Arc::clone(&lane_started);
                    async move {
                        lane_started.notify_one();
                        std::future::pending::<()>().await;
                        Ok::<_, String>(call.output("unreachable"))
                    }
                })
            },
            port,
        )
        .unwrap();
        let started = lane_started.notified();
        tokio::pin!(started);
        let mut reaction = Box::pin(application.react());

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut reaction => panic!("pending reaction ended: {result:?}"),
                _ = &mut started => {}
            }
        })
        .await
        .expect("tool lane start");
        drop(reaction);

        let declarations = probe.declarations.load(Ordering::Relaxed);
        let submissions = probe.submissions.load(Ordering::Relaxed);
        let fault = application.react().await.unwrap_err();

        assert_eq!(fault.stage(), ApplicationFaultStage::Reaction);
        assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(fault.code(), ApplicationFaultCode::Unavailable);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::CancelledAfterHandoff
        );
        assert_eq!(probe.declarations.load(Ordering::Relaxed), declarations);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), submissions);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn normal_tool_reaction_post_reconciles_dirty_component_state() {
        let (port, probe) = scripted_port(
            vec![
                FactScript::Finite(vec![
                    Ok(tool_fact(5, 1, "call-change", "change")),
                    Ok(ProviderFact::ReactionCompleted { primary_text: None }),
                ]),
                completed_reaction(),
            ],
            0,
        );
        let mut application = Application::mount(stateful_tool_component, port).unwrap();
        assert!(
            rendered_text(application.components.current_projection().unwrap())
                .contains("<state>before</state>")
        );

        application.react().await.unwrap();

        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert!(!application.components.is_dirty());
        assert!(
            rendered_text(application.components.current_projection().unwrap())
                .contains("<state>after</state>")
        );
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            1
        );

        application.react().await.unwrap();

        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 2);
        assert_eq!(*probe.staged_input_counts.lock().unwrap(), [0, 1]);
        assert_eq!(
            application
                .session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn provider_hook_updates_state_before_post_reconcile_without_implicit_submit() {
        let output = ProviderOutputKey::new(7);
        let (port, probe) = scripted_port(
            vec![FactScript::Finite(vec![
                Ok(ProviderFact::TextDelta {
                    output,
                    phase: None,
                    delta: String::from("after"),
                }),
                Ok(ProviderFact::TextSealed {
                    output,
                    phase: None,
                    text: String::from("after"),
                }),
                Ok(ProviderFact::ReactionCompleted {
                    primary_text: Some(output),
                }),
            ])],
            0,
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let handler_events = Arc::clone(&events);
        let mut application = Application::mount(
            move || stateful_provider_handler(Arc::clone(&handler_events)),
            port,
        )
        .unwrap();
        assert!(
            rendered_text(application.components.current_projection().unwrap())
                .contains("<state>before</state>")
        );

        application.react().await.unwrap();

        assert_eq!(*events.lock().unwrap(), ["delta", "complete"]);
        assert_eq!(probe.handoffs.load(Ordering::Relaxed), 1);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 1);
        assert!(!application.components.is_dirty());
        assert!(
            rendered_text(application.components.current_projection().unwrap())
                .contains("<state>after</state>")
        );
        assert!(matches!(
            application.session.canonical_history.transcript.items().last(),
            Some(CanonicalInputItem::AssistantText { text, .. }) if text == "after"
        ));
    }

    #[tokio::test]
    async fn component_demand_is_sticky_without_rendering_and_next_frame_sees_prior_signal_write() {
        let exported_demand = Arc::new(Mutex::new(None));
        let exported_signal = Arc::new(Mutex::new(None));
        let demand_slot = Arc::clone(&exported_demand);
        let signal_slot = Arc::clone(&exported_signal);
        let (port, probe) = scripted_port(vec![completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                demand_component(DemandComponentProps {
                    exported_demand: Arc::clone(&demand_slot),
                    exported_signal: Arc::clone(&signal_slot),
                })
            },
            port,
        )
        .unwrap();
        let demand = exported_demand.lock().unwrap().clone().unwrap();
        let signal = exported_signal.lock().unwrap().clone().unwrap();

        assert!(!application.take_driver_demand().unwrap());
        let initial_revision = application.current_projection().revision();
        demand.request().unwrap();
        let unchanged = application.current_projection();
        assert!(!unchanged.is_dirty());
        assert_eq!(unchanged.revision(), initial_revision);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 0);
        assert!(application.take_driver_demand().unwrap());

        signal.set(String::from("after")).unwrap();
        demand.request().unwrap();

        let stale = application.current_projection();
        assert!(stale.is_dirty());
        assert!(rendered_text(stale.projection()).contains("before"));
        application.wait_for_driver_demand().await.unwrap();
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 0);
        assert!(!application.take_driver_demand().unwrap());

        application.react().await.unwrap();

        let frames = probe.canonical_frames.lock().unwrap();
        let frame = std::str::from_utf8(&frames[0]).unwrap();
        assert!(frame.contains("after"));
        assert!(!frame.contains("before"));
        drop(frames);
        assert_eq!(probe.submissions.load(Ordering::Relaxed), 1);

        drop(application);
        assert_eq!(demand.request(), Err(ReactionRequestError::StaleMount));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn use_future_starts_once_per_mount_without_implicit_demand_or_dirtying() {
        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let (port, _) = scripted_port(vec![completed_reaction(), completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                future_lifecycle_component(FutureLifecycleProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                })
            },
            port,
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while starts.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        application.react().await.unwrap();
        application.react().await.unwrap();
        assert_eq!(starts.load(Ordering::Acquire), 1);
        release.notify_waiters();
        tokio::task::yield_now().await;
        assert!(!application.current_projection().is_dirty());
        assert!(!application.take_driver_demand().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn use_coroutine_retains_one_bounded_fifo_across_rerender() {
        let (exported_tx, exported_rx) = std::sync::mpsc::channel();
        let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel();
        let (port, _) = scripted_port(vec![completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                coroutine_lifecycle_component(CoroutineLifecycleProps {
                    exported: exported_tx.clone(),
                    received: received_tx.clone(),
                })
            },
            port,
        )
        .unwrap();

        let first = exported_rx.recv().unwrap();
        first.send(1).await.unwrap();
        application.react().await.unwrap();
        let rerendered = exported_rx.recv().unwrap();
        rerendered.send(2).await.unwrap();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), received_rx.recv())
                .await
                .unwrap(),
            Some([1, 2])
        );
        assert!(!application.current_projection().is_dirty());
        assert!(!application.take_driver_demand().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_is_available_during_handler_invocation_and_future() {
        let output = ProviderOutputKey::new(11);
        let (port, _) = scripted_port(
            vec![FactScript::Finite(vec![
                Ok(ProviderFact::TextDelta {
                    output,
                    phase: None,
                    delta: String::from("answer"),
                }),
                Ok(ProviderFact::TextSealed {
                    output,
                    phase: None,
                    text: String::from("answer"),
                }),
                Ok(ProviderFact::ReactionCompleted {
                    primary_text: Some(output),
                }),
            ])],
            0,
        );
        let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut application = Application::mount(
            move || {
                spawn_handler_component(SpawnHandlerProps {
                    observed: observed_tx.clone(),
                })
            },
            port,
        )
        .unwrap();

        application.react().await.unwrap();
        let mut observed = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            while observed.len() != 4 {
                observed.push(observed_rx.recv().await.expect("spawned task result"));
            }
        })
        .await
        .unwrap();
        observed.sort_unstable();
        assert_eq!(observed, ["future", "future", "invocation", "invocation"]);
        assert!(!application.current_projection().is_dirty());
        assert!(!application.take_driver_demand().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unmount_awaits_task_drop_before_replacement_frame_submit() {
        let exported = Arc::new(Mutex::new(None));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_exported = Arc::clone(&exported);
        let component_started = Arc::clone(&started);
        let component_dropped = Arc::clone(&dropped);
        let (port, probe) = scripted_port(vec![completed_reaction(), completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                retirement_root(RetirementRootProps {
                    exported: Arc::clone(&component_exported),
                    started: Arc::clone(&component_started),
                    dropped: Arc::clone(&component_dropped),
                    drop_gate: None,
                    panic_on_drop: None,
                })
            },
            port,
        )
        .unwrap();
        let visible = exported.lock().unwrap().clone().unwrap();

        application.react().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        application.port.observed_task_drop = Some(Arc::clone(&dropped));
        visible.set(false).unwrap();
        application.react().await.unwrap();

        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(*probe.task_drop_observed_at_submit.lock().unwrap(), [true]);
        assert!(
            !rendered_text(application.current_projection().projection())
                .contains("retiring_child")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_retirement_resumes_without_replacing_the_pending_transition() {
        let exported = Arc::new(Mutex::new(None));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drop_gate = Arc::new(TaskDropGate::new());
        let component_exported = Arc::clone(&exported);
        let component_started = Arc::clone(&started);
        let component_dropped = Arc::clone(&dropped);
        let component_gate = Arc::clone(&drop_gate);
        let (port, probe) = scripted_port(vec![completed_reaction(), completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                retirement_root(RetirementRootProps {
                    exported: Arc::clone(&component_exported),
                    started: Arc::clone(&component_started),
                    dropped: Arc::clone(&component_dropped),
                    drop_gate: Some(Arc::clone(&component_gate)),
                    panic_on_drop: None,
                })
            },
            port,
        )
        .unwrap();
        let visible = exported.lock().unwrap().clone().unwrap();

        application.react().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let previous_revision = application.current_projection().revision();
        visible.set(false).unwrap();

        let mut reaction = Box::pin(application.react());
        let entered = tokio::select! {
            result = &mut reaction => panic!("retirement unexpectedly completed: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(1), async {
                while !drop_gate.entered.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            }) => entered,
        };
        entered.expect("task drop must enter the retirement gate");
        drop(reaction);
        drop_gate.release();

        let pending = application.current_projection();
        assert!(pending.is_dirty());
        assert_eq!(pending.revision(), previous_revision);
        assert!(rendered_text(pending.projection()).contains("retiring_child"));
        assert_eq!(probe.submissions.load(Ordering::Acquire), 1);

        application.react().await.unwrap();
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(
            application.current_projection().revision(),
            previous_revision + 1
        );
        assert_eq!(probe.submissions.load(Ordering::Acquire), 2);
        assert!(
            !rendered_text(application.current_projection().projection())
                .contains("retiring_child")
        );
    }

    async fn assert_retirement_panic_overrides(next_reaction: FactScript) {
        use futures::FutureExt as _;

        let exported = Arc::new(Mutex::new(None));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_exported = Arc::clone(&exported);
        let component_started = Arc::clone(&started);
        let component_dropped = Arc::clone(&dropped);
        let (port, probe) = scripted_port(vec![completed_reaction(), next_reaction], 0);
        let mut application = Application::mount(
            move || {
                retirement_root(RetirementRootProps {
                    exported: Arc::clone(&component_exported),
                    started: Arc::clone(&component_started),
                    dropped: Arc::clone(&component_dropped),
                    drop_gate: None,
                    panic_on_drop: Some("retirement task panic"),
                })
            },
            port,
        )
        .unwrap();
        let visible = exported.lock().unwrap().clone().unwrap();

        application.react().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        visible.set(false).unwrap();

        let panic = AssertUnwindSafe(application.react())
            .catch_unwind()
            .await
            .expect_err("retirement task panic must escape the reaction");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"retirement task panic"));
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(probe.submissions.load(Ordering::Acquire), 1);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retirement_task_panic_overrides_a_ready_normal_reaction() {
        assert_retirement_panic_overrides(completed_reaction()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retirement_task_panic_overrides_a_ready_reaction_fault() {
        assert_retirement_panic_overrides(FactScript::Finite(vec![Err(
            ReactionPortFault::retryable(
                ReactionPortFaultCode::Unavailable,
                ReactionPortFaultReason::StreamTransport,
            ),
        )]))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_fences_mount_capabilities_and_awaits_task_drop() {
        let exported = Arc::new(Mutex::new(None));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drop_gate = Arc::new(TaskDropGate::new());
        let component_exported = Arc::clone(&exported);
        let component_started = Arc::clone(&started);
        let component_dropped = Arc::clone(&dropped);
        let component_gate = Arc::clone(&drop_gate);
        let (port, _) = scripted_port(vec![completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                retirement_root(RetirementRootProps {
                    exported: Arc::clone(&component_exported),
                    started: Arc::clone(&component_started),
                    dropped: Arc::clone(&component_dropped),
                    drop_gate: Some(Arc::clone(&component_gate)),
                    panic_on_drop: None,
                })
            },
            port,
        )
        .unwrap();
        let visible = exported.lock().unwrap().clone().unwrap();

        application.react().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut shutdown = Box::pin(application.shutdown());
        let entered = tokio::select! {
            result = &mut shutdown => panic!("shutdown completed before task drop: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(1), async {
                while !drop_gate.entered.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            }) => entered,
        };
        entered.expect("task drop must enter the shutdown gate");

        assert!(matches!(visible.set(false), Err(SignalAccessError::Stale)));
        assert!(!dropped.load(Ordering::Acquire));
        drop_gate.release();
        shutdown.await.unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn owning_runtime_shutdown_fails_all_application_driver_boundaries_before_work() {
        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let exported_demand = Arc::new(Mutex::new(None));
        let renders = Arc::new(AtomicUsize::new(0));
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let component_demand = Arc::clone(&exported_demand);
        let observed_renders = Arc::clone(&renders);
        let (port, probe) = scripted_port(Vec::new(), 0);

        let owning_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut application = owning_runtime.block_on(async {
            let application = Application::mount(
                move || {
                    observed_renders.fetch_add(1, Ordering::AcqRel);
                    runtime_boundary_component(RuntimeBoundaryProps {
                        starts: Arc::clone(&component_starts),
                        release: Arc::clone(&component_release),
                        exported_demand: Arc::clone(&component_demand),
                    })
                },
                port,
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while starts.load(Ordering::Acquire) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the Component task must start on runtime A");
            exported_demand
                .lock()
                .unwrap()
                .as_ref()
                .expect("the mounted Component exports its demand handle")
                .request()
                .unwrap();
            application
        });

        assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
        assert_eq!(renders.load(Ordering::Acquire), 1);
        assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
        drop(owning_runtime);

        let replacement_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        replacement_runtime.block_on(async {
            let fault = tokio::time::timeout(Duration::from_secs(1), application.react())
                .await
                .expect("react must fail instead of waiting on the closed supervisor")
                .unwrap_err();
            assert_eq!(fault.stage(), ApplicationFaultStage::Reaction);
            assert_eq!(fault.kind(), ApplicationFaultKind::Terminal);
            assert_eq!(fault.code(), ApplicationFaultCode::Internal);
            assert_eq!(fault.reason(), ApplicationFaultReason::ComponentRuntime);
            assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
            assert_eq!(renders.load(Ordering::Acquire), 1);
            assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
            assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), application.wait_for_driver_demand(),)
                    .await
                    .expect("blocking demand must fail instead of hanging"),
                Err(DriverDemandFault::StaleMount)
            );
            assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
            assert_eq!(renders.load(Ordering::Acquire), 1);
            assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
            assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);

            assert_eq!(
                application.take_driver_demand(),
                Err(DriverDemandFault::StaleMount)
            );
            assert_eq!(probe.declarations.load(Ordering::Acquire), 1);
            assert_eq!(renders.load(Ordering::Acquire), 1);
            assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
            assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_resumes_a_polled_task_panic_with_its_original_payload() {
        use futures::FutureExt as _;

        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let component_sibling = Arc::clone(&sibling_dropped);
        let (port, _) = scripted_port(vec![completed_reaction()], 0);
        let mut application = Application::mount(
            move || {
                panicking_future_component(PanickingFutureProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                    sibling_dropped: Arc::clone(&component_sibling),
                    sibling_drop_gate: None,
                })
            },
            port,
        )
        .unwrap();

        application.react().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while starts.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();

        let panic = AssertUnwindSafe(application.shutdown())
            .catch_unwind()
            .await
            .expect_err("shutdown must resume the Component task panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"component task panic payload")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn task_panic_interrupts_demand_wait() {
        use futures::FutureExt as _;

        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let component_sibling = Arc::clone(&sibling_dropped);
        let (port, _) = scripted_port(Vec::new(), 0);
        let mut application = Application::mount(
            move || {
                panicking_future_component(PanickingFutureProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                    sibling_dropped: Arc::clone(&component_sibling),
                    sibling_drop_gate: None,
                })
            },
            port,
        )
        .unwrap();

        let trigger_starts = Arc::clone(&starts);
        let trigger_release = Arc::clone(&release);
        let trigger = tokio::spawn(async move {
            while trigger_starts.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
            trigger_release.notify_one();
        });
        let panic = AssertUnwindSafe(application.wait_for_driver_demand())
            .catch_unwind()
            .await
            .expect_err("a Component task panic must escape the driver wait");
        trigger.await.unwrap();

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"component task panic payload")
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !sibling_dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn task_panic_unwinds_before_a_blocking_sibling_destructor_finishes() {
        use futures::FutureExt as _;

        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drop_gate = Arc::new(TaskDropGate::new());
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let component_sibling = Arc::clone(&sibling_dropped);
        let component_gate = Arc::clone(&drop_gate);
        let (port, _) = scripted_port(Vec::new(), 0);
        let mut application = Application::mount(
            move || {
                panicking_future_component(PanickingFutureProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                    sibling_dropped: Arc::clone(&component_sibling),
                    sibling_drop_gate: Some(Arc::clone(&component_gate)),
                })
            },
            port,
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while starts.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();

        let unwind = tokio::time::timeout(
            Duration::from_secs(1),
            AssertUnwindSafe(application.wait_for_driver_demand()).catch_unwind(),
        )
        .await;
        let panic = match unwind {
            Ok(Err(panic)) => panic,
            Ok(Ok(result)) => {
                drop_gate.release();
                panic!("task panic returned instead of unwinding: {result:?}");
            }
            Err(_) => {
                drop_gate.release();
                panic!("blocking sibling cleanup delayed task panic unwind");
            }
        };
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"component task panic payload")
        );

        let entered = tokio::time::timeout(Duration::from_secs(1), async {
            while !drop_gate.entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if entered.is_err() {
            drop_gate.release();
            panic!("task panic did not synchronously initiate sibling abort");
        }
        assert!(!sibling_dropped.load(Ordering::Acquire));
        assert_eq!(
            application.state.load(Ordering::Acquire),
            APPLICATION_TERMINATED_AFTER_TASK_PANIC
        );
        assert_eq!(
            application.react().await.unwrap_err().reason(),
            ApplicationFaultReason::ComponentRuntime
        );

        drop_gate.release();
        let shutdown = application.shutdown().await.unwrap_err();
        assert_eq!(shutdown.reason(), ApplicationFaultReason::ComponentRuntime);
        assert!(sibling_dropped.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn task_panic_during_reconcile_uses_the_original_payload_arbiter() {
        use futures::FutureExt as _;

        let exported = Arc::new(Mutex::new(None));
        let task_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let panic_release = Arc::new(Notify::new());
        let render_gate = Arc::new(TaskDropGate::new());
        let child_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_exported = Arc::clone(&exported);
        let component_task_started = Arc::clone(&task_started);
        let component_panic_release = Arc::clone(&panic_release);
        let component_render_gate = Arc::clone(&render_gate);
        let component_child_started = Arc::clone(&child_started);
        let component_child_dropped = Arc::clone(&child_dropped);
        let (port, probe) = scripted_port(Vec::new(), 0);
        let mut application = Application::mount(
            move || {
                reconcile_panic_root(ReconcilePanicProps {
                    exported: Arc::clone(&component_exported),
                    task_started: Arc::clone(&component_task_started),
                    panic_release: Arc::clone(&component_panic_release),
                    render_gate: Arc::clone(&component_render_gate),
                    child_started: Arc::clone(&component_child_started),
                    child_dropped: Arc::clone(&component_child_dropped),
                })
            },
            port,
        )
        .unwrap();
        let visible = exported.lock().unwrap().clone().unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !task_started.load(Ordering::Acquire) || !child_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        visible.set(false).unwrap();
        let monitor = application.tasks.panic_monitor();
        let trigger_gate = Arc::clone(&render_gate);
        let trigger_release = Arc::clone(&panic_release);
        let trigger = tokio::spawn(async move {
            while !trigger_gate.entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            trigger_release.notify_one();
            monitor.wait().await.unwrap();
            trigger_gate.release();
        });

        let panic = AssertUnwindSafe(application.react())
            .catch_unwind()
            .await
            .expect_err("panic latched during reconcile must unwind");
        trigger.await.unwrap();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"panic during reconcile")
        );
        assert_eq!(probe.submissions.load(Ordering::Acquire), 0);
        assert_eq!(
            application.state.load(Ordering::Acquire),
            APPLICATION_TERMINATED_AFTER_TASK_PANIC
        );
        assert_eq!(
            application.react().await.unwrap_err().reason(),
            ApplicationFaultReason::ComponentRuntime
        );
        let shutdown = application.shutdown().await.unwrap_err();
        assert_eq!(shutdown.reason(), ApplicationFaultReason::ComponentRuntime);
        assert!(child_dropped.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bootstrap_task_panic_is_independent_of_initial_poll_order() {
        let (port, _) = scripted_port(Vec::new(), 0);
        let mounted = std::panic::catch_unwind(AssertUnwindSafe(|| {
            Application::mount(immediate_panic_component, port)
        }));

        match mounted {
            Err(panic) => assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"immediate bootstrap task panic")
            ),
            Ok(Err(fault)) => panic!("bootstrap task panic became a fault: {fault:?}"),
            Ok(Ok(application)) => {
                let monitor = application.tasks.panic_monitor();
                tokio::time::timeout(Duration::from_secs(1), monitor.wait())
                    .await
                    .unwrap()
                    .unwrap();
                let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let _ = application.take_driver_demand();
                }))
                .expect_err("the first driver boundary must resume bootstrap panic");
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"immediate bootstrap task panic")
                );
                let shutdown = application.shutdown().await.unwrap_err();
                assert_eq!(shutdown.reason(), ApplicationFaultReason::ComponentRuntime);
            }
        }
    }

    async fn cancelled_application_with_latched_task_panic(
    ) -> (Application<ScriptedPort>, Arc<ScriptProbe>) {
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let component_started = Arc::clone(&started);
        let component_release = Arc::clone(&release);
        let (port, probe) = scripted_port(vec![FactScript::PendingAfter(Vec::new())], 0);
        let mut application = Application::mount(
            move || {
                deferred_panic_component(DeferredPanicProps {
                    started: Arc::clone(&component_started),
                    release: Arc::clone(&component_release),
                })
            },
            port,
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bootstrap task must start");

        let mut reaction = Box::pin(application.react());
        assert!(poll_once(reaction.as_mut()).is_pending());
        drop(reaction);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
        assert_eq!(
            application.state.load(Ordering::Acquire),
            APPLICATION_TERMINATED_AFTER_CANCELLATION
        );

        let monitor = application.tasks.panic_monitor();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), monitor.wait())
            .await
            .expect("late task panic must be observed")
            .expect("task supervisor remains observable");
        assert_eq!(monitor.status(), TaskSupervisorStatus::Panicked);
        (application, probe)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fresh_task_panic_overrides_post_handoff_cancellation_at_react_boundary() {
        use futures::FutureExt as _;

        let (mut application, probe) = cancelled_application_with_latched_task_panic().await;
        let declarations = probe.declarations.load(Ordering::Acquire);
        let submissions = probe.submissions.load(Ordering::Acquire);

        let panic = AssertUnwindSafe(application.react())
            .catch_unwind()
            .await
            .expect_err("fresh task panic must override the cancellation terminal fault");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"deferred bootstrap task panic")
        );

        let fault = application.react().await.unwrap_err();
        assert_eq!(fault.reason(), ApplicationFaultReason::ComponentRuntime);
        assert_eq!(probe.declarations.load(Ordering::Acquire), declarations);
        assert_eq!(probe.submissions.load(Ordering::Acquire), submissions);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fresh_task_panic_overrides_post_handoff_cancellation_at_demand_wait_boundary() {
        use futures::FutureExt as _;

        let (mut application, _) = cancelled_application_with_latched_task_panic().await;
        let panic = AssertUnwindSafe(application.wait_for_driver_demand())
            .catch_unwind()
            .await
            .expect_err("fresh task panic must override the stale demand terminal fault");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"deferred bootstrap task panic")
        );
        assert_eq!(
            application.wait_for_driver_demand().await,
            Err(DriverDemandFault::StaleMount)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fresh_task_panic_overrides_post_handoff_cancellation_at_demand_take_boundary() {
        let (application, _) = cancelled_application_with_latched_task_panic().await;
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| application.take_driver_demand()))
            .expect_err("fresh task panic must override the stale demand terminal fault");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"deferred bootstrap task panic")
        );
        assert_eq!(
            application.take_driver_demand(),
            Err(DriverDemandFault::StaleMount)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn task_panic_without_an_active_driver_unwinds_at_the_next_boundary() {
        use futures::FutureExt as _;

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let component_started = Arc::clone(&started);
        let component_release = Arc::clone(&release);
        let (port, _) = scripted_port(Vec::new(), 0);
        let mut application = Application::mount(
            move || {
                deferred_panic_component(DeferredPanicProps {
                    started: Arc::clone(&component_started),
                    release: Arc::clone(&component_release),
                })
            },
            port,
        )
        .unwrap();
        let monitor = application.tasks.panic_monitor();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();
        monitor.wait().await.unwrap();

        let panic = AssertUnwindSafe(application.wait_for_driver_demand())
            .catch_unwind()
            .await
            .expect_err("a latched panic must interrupt the driver wait");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"deferred bootstrap task panic")
        );

        let fault = application.react().await.unwrap_err();
        assert_eq!(fault.reason(), ApplicationFaultReason::ComponentRuntime);
        assert_eq!(
            application.state.load(Ordering::Acquire),
            APPLICATION_TERMINATED_AFTER_TASK_PANIC
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn task_panic_wins_over_provider_stream_destructor_panic() {
        use futures::FutureExt as _;

        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let component_sibling = Arc::clone(&sibling_dropped);
        let (port, _) = scripted_port(vec![FactScript::PanicOnDropPending], 0);
        let mut application = Application::mount(
            move || {
                panicking_future_component(PanickingFutureProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                    sibling_dropped: Arc::clone(&component_sibling),
                    sibling_drop_gate: None,
                })
            },
            port,
        )
        .unwrap();

        let trigger_starts = Arc::clone(&starts);
        let trigger_release = Arc::clone(&release);
        let trigger = tokio::spawn(async move {
            while trigger_starts.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
            trigger_release.notify_one();
        });
        let panic = AssertUnwindSafe(application.react())
            .catch_unwind()
            .await
            .expect_err("the Component task panic must win destructor arbitration");
        trigger.await.unwrap();

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"component task panic payload")
        );
        assert!(sibling_dropped.load(Ordering::Acquire));
    }

    async fn assert_same_poll_task_panic_overrides_reaction_result(script: FactScript) {
        use futures::FutureExt as _;

        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let component_sibling = Arc::clone(&sibling_dropped);
        let (port, probe) = scripted_port(vec![script], 0);
        let mut application = Application::mount(
            move || {
                panicking_future_component(PanickingFutureProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                    sibling_dropped: Arc::clone(&component_sibling),
                    sibling_drop_gate: None,
                })
            },
            port,
        )
        .unwrap();
        let monitor = application.tasks.panic_monitor();
        application.port.before_stream = Some(Arc::new(move || {
            release.notify_one();
            while monitor.status() != TaskSupervisorStatus::Panicked {
                std::thread::yield_now();
            }
        }));

        let panic = AssertUnwindSafe(application.react())
            .catch_unwind()
            .await
            .expect_err("the same-poll Component task panic must win");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"component task panic payload")
        );
        assert!(starts.load(Ordering::Acquire) >= 1);
        assert!(sibling_dropped.load(Ordering::Acquire));
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_poll_task_panic_overrides_a_normal_reaction_result() {
        assert_same_poll_task_panic_overrides_reaction_result(completed_reaction()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_poll_task_panic_overrides_a_reaction_fault() {
        assert_same_poll_task_panic_overrides_reaction_result(FactScript::Finite(vec![Err(
            ReactionPortFault::retryable(
                ReactionPortFaultCode::Unavailable,
                ReactionPortFaultReason::StreamTransport,
            ),
        )]))
        .await;
    }

    #[test]
    fn task_hook_without_tokio_runtime_fails_before_bootstrap_commit() {
        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let component_starts = Arc::clone(&starts);
        let component_release = Arc::clone(&release);
        let (port, probe) = scripted_port(Vec::new(), 0);
        let fault = match Application::mount(
            move || {
                future_lifecycle_component(FutureLifecycleProps {
                    starts: Arc::clone(&component_starts),
                    release: Arc::clone(&component_release),
                })
            },
            port,
        ) {
            Ok(_) => panic!("task hook mount outside Tokio must fail"),
            Err(fault) => fault,
        };

        assert_eq!(fault.stage(), ApplicationFaultStage::Bootstrap);
        assert_eq!(fault.reason(), ApplicationFaultReason::ComponentRuntime);
        assert_eq!(starts.load(Ordering::Acquire), 0);
        assert_eq!(probe.handoffs.load(Ordering::Acquire), 0);
    }
}
