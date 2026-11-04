//! Fresh-provider branches share only the caller's explicit business snapshot.

use super::*;
use crate::component::execution::reaction::TargetContinuity;

impl<P: ReactionPort> Application<P> {
    /// Mount an independent application on a fresh provider without model history.
    ///
    /// Capture an owned business snapshot in `root`. The child has fresh hooks,
    /// tasks, command input, and session state; its first submission is Full.
    /// The target can have a different provider type, model, and Frame profile.
    /// Supply a fresh target with a distinct identity and FullRequired continuity.
    /// Neither this method nor the child reads or changes the source provider.
    ///
    /// Requires a healthy, nonexiting source and an active Tokio runtime. Source
    /// commands, staged tool results, and recovery work remain with the source.
    /// The caller owns snapshot consistency and drives and shuts down each
    /// application independently. No reaction is submitted by this method.
    pub async fn fork_to<Q: ReactionPort + 'static>(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        target: Q,
    ) -> Result<Application<Q>, ApplicationFault> {
        self.fork_to_with_input(root, target, None).await
    }

    /// Mount a history-free branch with a new, independently owned command inbox.
    pub async fn fork_to_with_commands<Q: ReactionPort + 'static>(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        target: Q,
        input: CommandInput,
    ) -> Result<Application<Q>, ApplicationFault> {
        self.fork_to_with_input(root, target, Some(input)).await
    }

    async fn fork_to_with_input<Q: ReactionPort + 'static>(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        mut target: Q,
        input: Option<CommandInput>,
    ) -> Result<Application<Q>, ApplicationFault> {
        let stage = ApplicationFaultStage::Fork;
        let state = Arc::clone(&self.state);
        let monitor = self.tasks.panic_monitor();
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                target.shutdown().await;
                begin_outer_driver_boundary(&state, &monitor)
                    .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
                return Err(ApplicationFault::fork_not_ready());
            }
        };
        let exit = self.application_exit.clone();
        let source_identity = self.declaration.identity();
        let task_handle = self.tasks.handle();
        // Own the supplied target before checking the source or evaluating user
        // code. A rejected or panicking mount must still shut that target down.
        let mut candidate = ForkCandidate {
            resource: Some(ForkResource::Port(target)),
            cleanups: &mut self.fork_cleanups,
            runtime,
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            begin_outer_driver_boundary(&state, &monitor)
                .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
            task_handle
                .preflight()
                .map_err(|fault| ApplicationFault::from_task_supervisor(stage, fault))?;
            if exit.reason().is_some() {
                return Err(ApplicationFault::fork_not_ready());
            }
            let declaration = candidate
                .port_mut()
                .declare()
                .map_err(|fault| ApplicationFault::from_port(stage, fault))?;
            declaration.validate().map_err(|fault| ApplicationFault {
                stage,
                ..ApplicationFault::invalid_declaration(fault)
            })?;
            declaration
                .profile()
                .validate()
                .map_err(|fault| ApplicationFault {
                    stage,
                    ..ApplicationFault::invalid_profile(fault)
                })?;
            if declaration.identity() == source_identity
                || !matches!(
                    declaration.continuity(),
                    TargetContinuity::FullRequired { .. }
                )
            {
                return Err(ApplicationFault::from_session(
                    stage,
                    FrameSessionFault::ForkSnapshotIncompatible,
                ));
            }
            let session = FrameSession::new(&declaration)
                .map_err(|fault| ApplicationFault::from_session(stage, fault))?;
            begin_outer_driver_boundary(&state, &monitor)
                .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
            if exit.reason().is_some() {
                return Err(ApplicationFault::fork_not_ready());
            }
            let root: RootFactory = Arc::new(move |_events| root());
            let child =
                Application::unmounted(root, candidate.take_port(), input, declaration, session);
            candidate.resource = Some(ForkResource::Application(Box::new(child)));
            candidate.application_mut().bootstrap(|child, projection| {
                // Check against the destination's own budget before tasks start.
                // Preparing here does not commit or submit the initial Frame.
                child
                    .session
                    .prepare(&child.declaration, projection)
                    .map_err(|fault| ApplicationFault::from_session(stage, fault))?;
                begin_outer_driver_boundary(&state, &monitor)
                    .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
                if exit.reason().is_some() {
                    return Err(ApplicationFault::fork_not_ready());
                }
                Ok(())
            })?;
            begin_outer_driver_boundary(&state, &monitor)
                .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
            if exit.reason().is_some() {
                return Err(ApplicationFault::fork_not_ready());
            }
            Ok(())
        }));
        let fault = match outcome {
            Ok(Ok(())) => return Ok(candidate.take_application()),
            Ok(Err(fault)) => fault,
            Err(payload) => {
                drop(candidate);
                if monitor.status() == TaskSupervisorStatus::Panicked {
                    consume_supervised_task_panic(&state, &monitor);
                }
                if state.load(Ordering::Acquire) != APPLICATION_TERMINATED_AFTER_TASK_PANIC {
                    state.store(APPLICATION_TERMINATED_AFTER_FORK_PANIC, Ordering::Release);
                }
                resume_unwind(payload);
            }
        };
        drop(candidate);

        let mut cleanup = Box::pin(AssertUnwindSafe(self.complete_fork_cleanups()).catch_unwind());
        let outcome = tokio::select! {
            biased;
            task = monitor.wait() => {
                drop_driver_future_before_panic_arbitration(cleanup, &monitor);
                return match task {
                    Ok(()) => {
                        consume_supervised_task_panic(&state, &monitor);
                        Err(task_panic_terminal_fault(stage))
                    }
                    Err(fault) => Err(ApplicationFault::from_task_supervisor(stage, fault)),
                };
            }
            result = &mut cleanup => result,
        };
        drop_driver_future_before_panic_arbitration(cleanup, &monitor);
        let cleanup_result = match outcome {
            Ok(result) => result,
            Err(payload) => {
                if monitor.status() == TaskSupervisorStatus::Panicked {
                    consume_supervised_task_panic(&state, &monitor);
                }
                if state.load(Ordering::Acquire) != APPLICATION_TERMINATED_AFTER_TASK_PANIC {
                    state.store(APPLICATION_TERMINATED_AFTER_FORK_PANIC, Ordering::Release);
                }
                resume_unwind(payload);
            }
        };
        begin_outer_driver_boundary(&state, &monitor)
            .map_err(|fault| application_fault_from_outer_driver_boundary(stage, fault))?;
        cleanup_result?;
        Err(fault)
    }
}
