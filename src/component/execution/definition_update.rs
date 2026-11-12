//! Definition replacement at an idle application boundary.

use std::sync::{atomic::Ordering, Arc};

use futures::FutureExt;

use super::{Application, Maintenance, Shared};
use crate::component::{
    authoring::{ComponentAttemptFault, ComponentDefinition},
    execution::{
        ApplicationFault, ApplicationFaultKind, ApplicationFaultStage, CommandInput, ReactionPort,
    },
    task::MountTaskScope,
    ComponentHostFault,
};

/// Application-local revision of the installed root definition. Starts at one;
/// only an accepted replacement advances it. It is separate from FrameRevision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionVersion(u64);

impl DefinitionVersion {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A replacement has published its complete view and finished mount retirement.
/// Preparation and model delivery still require an explicit operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefinitionUpdate {
    previous_version: DefinitionVersion,
    effective_version: DefinitionVersion,
}

impl DefinitionUpdate {
    pub const fn previous_version(&self) -> DefinitionVersion {
        self.previous_version
    }

    pub const fn effective_version(&self) -> DefinitionVersion {
        self.effective_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DefinitionUpdateErrorKind {
    /// A reaction, another maintenance operation, or unfinished reconcile owns
    /// the boundary. Retry explicitly after it settles.
    Busy,
    /// Candidate rendering, hook compatibility, or Frame validation failed.
    /// The installed definition, mounts and projection are unchanged.
    Rejected,
    /// Application/target/task infrastructure is unavailable. A failure after
    /// retirement begins is terminal, not a recoverable definition rejection.
    Runtime,
    VersionExhausted,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("definition update {kind:?} at version {effective_version:?}: {message}")]
pub struct DefinitionUpdateError {
    kind: DefinitionUpdateErrorKind,
    effective_version: DefinitionVersion,
    message: String,
}

impl DefinitionUpdateError {
    pub const fn kind(&self) -> DefinitionUpdateErrorKind {
        self.kind
    }

    /// The installed revision, including when a terminal lifecycle fault occurs
    /// after a candidate has been accepted. User panics unwind as usual.
    pub const fn effective_version(&self) -> DefinitionVersion {
        self.effective_version
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl<P: ReactionPort + 'static> Application<P> {
    /// Mount an externally authored root without Rust component/view macros.
    pub fn mount_definition(
        definition: ComponentDefinition,
        port: P,
    ) -> Result<Self, ApplicationFault> {
        Self::mount(move || definition.instantiate("application"), port)
    }

    pub fn mount_definition_with_commands(
        definition: ComponentDefinition,
        port: P,
        input: CommandInput,
    ) -> Result<Self, ApplicationFault> {
        Self::mount_with_commands(move || definition.instantiate("application"), port, input)
    }

    pub fn definition_version(&self) -> DefinitionVersion {
        self.shared.definition_version()
    }

    /// Replace the complete root declaration at an idle boundary. Active Main
    /// and Temporary reactions cause Busy; they keep their original callbacks
    /// and EventContext until settlement. No reaction or preparation is started.
    ///
    /// Compatible keyed instances retain state and their original mount tasks.
    /// Removed/incompatible instances are fenced, cancelled and joined before
    /// publication. A rejected candidate retains the previous definition.
    /// Once admitted, dropping this waiter does not cancel the owned update.
    pub async fn update_definition(
        &mut self,
        definition: ComponentDefinition,
    ) -> Result<DefinitionUpdate, DefinitionUpdateError> {
        let shared = self.shared.clone();
        shared.check().map_err(|error| {
            shared.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
        })?;
        let maintenance = shared.reserve_maintenance().map_err(|error| {
            let kind = if error.kind() == ApplicationFaultKind::Terminal {
                DefinitionUpdateErrorKind::Runtime
            } else {
                DefinitionUpdateErrorKind::Busy
            };
            shared.update_error(kind, error.to_string())
        })?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            shared.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
        })?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let worker = shared.clone();
        let task = runtime.spawn(async move {
            let result = std::panic::AssertUnwindSafe(async {
                let result = worker.install_definition(definition, maintenance).await;
                worker.check().map_err(|error| {
                    worker.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
                })?;
                result
            })
            .catch_unwind()
            .await;
            let result = match result {
                Ok(result) => result,
                Err(payload) => {
                    // Match the owned-reaction panic boundary: retain the
                    // original payload, terminalize, and unwind at the caller.
                    worker.fail(payload);
                    Err(worker.update_error(
                        DefinitionUpdateErrorKind::Runtime,
                        "definition update panicked",
                    ))
                }
            };
            let _ = send.send(result);
        });
        {
            let mut admission = shared.admission.lock().unwrap_or_else(|p| p.into_inner());
            admission.tasks.retain(|task| !task.is_finished());
            admission.tasks.push(task);
        }
        let result = receive.await.map_err(|_| {
            shared.update_error(DefinitionUpdateErrorKind::Runtime, "update owner closed")
        })?;
        shared.check().map_err(|error| {
            shared.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
        })?;
        result
    }
}

impl<P: ReactionPort + 'static> Shared<P> {
    fn definition_version(&self) -> DefinitionVersion {
        DefinitionVersion(self.definition_version.load(Ordering::Acquire))
    }

    fn update_error(
        &self,
        kind: DefinitionUpdateErrorKind,
        message: impl Into<String>,
    ) -> DefinitionUpdateError {
        DefinitionUpdateError {
            kind,
            effective_version: self.definition_version(),
            message: message.into(),
        }
    }

    async fn install_definition(
        &self,
        definition: ComponentDefinition,
        _maintenance: Maintenance<P>,
    ) -> Result<DefinitionUpdate, DefinitionUpdateError> {
        let mut guard = self.core.lock().await;
        self.check().map_err(|error| {
            self.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
        })?;
        let core = guard.as_mut().ok_or_else(|| {
            self.update_error(DefinitionUpdateErrorKind::Runtime, "application closed")
        })?;
        if core.pending_render.is_some()
            || core.streaming.pending()
            || core
                .command_input
                .as_ref()
                .is_some_and(CommandInput::has_pending)
        {
            return Err(self.update_error(
                DefinitionUpdateErrorKind::Busy,
                "an earlier reconcile, recovery or command must finish first",
            ));
        }
        if core.application_exit.reason().is_some() {
            return Err(self.update_error(
                DefinitionUpdateErrorKind::Runtime,
                "application exit has been requested",
            ));
        }
        let previous_version = self.definition_version();
        let next = previous_version.0.checked_add(1).ok_or_else(|| {
            self.update_error(
                DefinitionUpdateErrorKind::VersionExhausted,
                "revision exhausted",
            )
        })?;
        core.tasks.handle().preflight().map_err(|error| {
            self.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
        })?;
        let declaration = core.refresh_declaration().map_err(|error| {
            self.update_error(DefinitionUpdateErrorKind::Runtime, error.to_string())
        })?;
        let session = &core.session;
        let committed = core
            .components
            .begin_validated_render(Some(definition.instantiate("application")), |projection| {
                self.check()
                    .map_err(|error| ComponentHostFault::TaskRuntime {
                        message: error.to_string(),
                    })?;
                session
                    .prepare(&declaration, projection)
                    .map(|_| ())
                    .map_err(|error| {
                        ComponentHostFault::Attempt(ComponentAttemptFault::Definition {
                            definition: "Frame validation".to_owned(),
                            message: error.to_string(),
                        })
                    })
            })
            .map_err(|error| {
                let kind = match &error {
                    ComponentHostFault::Attempt(_) => DefinitionUpdateErrorKind::Rejected,
                    _ => DefinitionUpdateErrorKind::Runtime,
                };
                self.update_error(kind, error.to_string())
            })?;

        // No fallible user validation remains. This is the irreversible mount
        // boundary; lifecycle infrastructure faults after it remain terminal.
        let root: super::super::RootFactory =
            Arc::new(move |_| definition.instantiate("application"));
        core.components.install_rendered_props(root);
        self.definition_version.store(next, Ordering::Release);
        let retired = committed
            .retired_mounts()
            .iter()
            .map(|mount| MountTaskScope::new(mount.component().clone(), mount.generation()))
            .collect::<Vec<_>>();
        let retirement = core
            .tasks
            .retire(retired)
            .map_err(|error| self.terminal_update_error(error.to_string()))?;
        core.pending_render = Some(super::super::PendingApplicationRender {
            committed,
            retirement,
        });
        let result = core
            .reconcile_components(ApplicationFaultStage::Reconcile)
            .await;
        self.publish(core);
        result.map_err(|error| self.terminal_update_error(error.to_string()))?;
        Ok(DefinitionUpdate {
            previous_version,
            effective_version: DefinitionVersion(next),
        })
    }

    fn terminal_update_error(&self, message: String) -> DefinitionUpdateError {
        // A mount was already retired; replaying the old definition cannot
        // restore its capabilities or external task effects.
        let _ = self.state.compare_exchange(
            super::super::APPLICATION_READY,
            super::super::APPLICATION_TERMINATED_AFTER_DEFINITION_UPDATE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.update_error(DefinitionUpdateErrorKind::Runtime, message)
    }
}
