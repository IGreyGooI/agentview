use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "legacy-provider-port")]
use super::authoring::Signal;
use super::{
    authoring::{
        application_exit::ApplicationExitControl, Component, ComponentAttemptFault,
        ComponentRenderStage, InternalEventInput as EventInput, MountTaskStart, PreparationSet,
        RenderBindings,
    },
    execution::{DriverDemandHandle, ProjectionExecutionScope, ProviderEvent, RenderedProjection},
    signal::{MountIdentity, SignalMountTransition, SignalRenderError, SignalRuntime},
    task::{MountTaskHandle, MountTaskSupervisorError},
};

static NEXT_COMPONENT_HOST_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime-only identity of one mounted Component application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentHostId {
    instance: u64,
}

/// Host-local identity of one successfully committed complete projection.
///
/// This is rendering metadata, not a Provider-facing `FrameRevision`. It is
/// intentionally crate-internal so the application runtime can detect a new
/// render without making ComponentHost's attempt generation part of that
/// protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProjectionRevision(u64);

impl ProjectionRevision {
    const FIRST: Self = Self(1);

    #[allow(dead_code)] // Read by the application snapshot API introduced after this host layer.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Long-lived owner of root props and retained Component signal slots.
pub struct ComponentHost<Props> {
    id: ComponentHostId,
    mount_generation: u64,
    root: ComponentRoot<Props>,
    props: Props,
    signals: SignalRuntime,
    driver_demand: Option<DriverDemandHandle>,
    tasks: Option<MountTaskHandle>,
    application_exit: Option<ApplicationExitControl>,
    next_render_generation: u64,
    next_projection_revision: Option<ProjectionRevision>,
    current_projection_revision: Option<ProjectionRevision>,
    current_projection: Option<RenderedProjection>,
    current_projection_prepared: bool,
}

enum ComponentRoot<Props> {
    Native(fn(Props) -> Component),
    WithEvents(fn(Props, EventInput<ProviderEvent>) -> Component),
}

impl<Props> ComponentHost<Props> {
    pub fn new_root(root: fn(Props) -> Component, props: Props) -> Self {
        Self::new_with_optional_capabilities(ComponentRoot::Native(root), props, None, None, None)
    }

    #[cfg(feature = "legacy-provider-port")]
    #[deprecated(note = "use `ComponentHost::new_root` with `use_provider_event_handler`")]
    pub fn new(root: fn(Props, EventInput<ProviderEvent>) -> Component, props: Props) -> Self {
        Self::new_with_event_input(root, props)
    }

    #[cfg_attr(
        not(feature = "legacy-provider-port"),
        allow(dead_code, reason = "retained for crate-internal event-routing tests")
    )]
    pub(crate) fn new_with_event_input(
        root: fn(Props, EventInput<ProviderEvent>) -> Component,
        props: Props,
    ) -> Self {
        Self::new_with_optional_capabilities(
            ComponentRoot::WithEvents(root),
            props,
            None,
            None,
            None,
        )
    }

    pub(crate) fn new_with_application_capabilities(
        root: fn(Props, EventInput<ProviderEvent>) -> Component,
        props: Props,
        driver_demand: DriverDemandHandle,
        tasks: MountTaskHandle,
        application_exit: Option<ApplicationExitControl>,
    ) -> Self {
        Self::new_with_optional_capabilities(
            ComponentRoot::WithEvents(root),
            props,
            Some(driver_demand),
            Some(tasks),
            application_exit,
        )
    }

    fn new_with_optional_capabilities(
        root: ComponentRoot<Props>,
        props: Props,
        driver_demand: Option<DriverDemandHandle>,
        tasks: Option<MountTaskHandle>,
        application_exit: Option<ApplicationExitControl>,
    ) -> Self {
        let instance = NEXT_COMPONENT_HOST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("ComponentHost identity space exhausted"));
        Self {
            id: ComponentHostId { instance },
            mount_generation: 1,
            root,
            props,
            signals: SignalRuntime::new(),
            driver_demand,
            tasks,
            application_exit,
            next_render_generation: 1,
            next_projection_revision: Some(ProjectionRevision::FIRST),
            current_projection_revision: None,
            current_projection: None,
            current_projection_prepared: false,
        }
    }

    pub fn id(&self) -> ComponentHostId {
        self.id
    }

    pub fn props(&self) -> &Props {
        &self.props
    }

    /// Replace root props without rendering or invoking a Provider.
    pub fn set_props(&mut self, next: Props) {
        self.props = next;
        self.signals.mark_dirty();
    }

    /// Start a fresh mount of the same root and invalidate every old Signal.
    pub fn remount(&mut self, next: Props) -> Result<ComponentHostId, ComponentHostFault> {
        let next_mount_generation = self
            .mount_generation
            .checked_add(1)
            .ok_or(ComponentHostFault::MountGenerationExhausted)?;
        self.signals
            .invalidate_all()
            .map_err(ComponentHostFault::signal)?;
        self.mount_generation = next_mount_generation;
        self.props = next;
        self.current_projection_revision = None;
        self.current_projection = None;
        self.current_projection_prepared = false;
        self.signals.mark_dirty();
        Ok(self.id)
    }

    /// Fence every capability issued by the currently mounted tree.
    pub(crate) fn fence_all_mounts(&mut self) -> Result<Vec<MountIdentity>, ComponentHostFault> {
        self.signals
            .invalidate_all()
            .map_err(ComponentHostFault::signal)
    }

    pub fn is_dirty(&self) -> bool {
        self.signals.is_dirty()
    }

    pub fn wake_revision(&self) -> u64 {
        self.signals.wake_revision()
    }

    pub async fn wait_for_wake_after(&self, observed: u64) -> u64 {
        self.signals.wait_for_wake_after(observed).await
    }

    /// Return the latest successfully committed complete projection.
    ///
    /// A dirty host retains this snapshot until a later render succeeds. Call
    /// [`is_dirty`](Self::is_dirty) separately to determine whether the
    /// authoritative Component state is waiting to be rendered.
    pub fn current_projection(&self) -> Option<&RenderedProjection> {
        self.current_projection.as_ref()
    }

    #[allow(dead_code)] // Read by the application snapshot API introduced after this host layer.
    pub(crate) fn current_projection_revision(&self) -> Option<ProjectionRevision> {
        self.current_projection_revision
    }

    pub(crate) const fn current_projection_is_prepared(&self) -> bool {
        self.current_projection_prepared
    }

    pub(crate) fn mark_current_projection_prepared(&mut self) {
        debug_assert!(self.current_projection.is_some());
        self.current_projection_prepared = true;
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn owns_signal<T>(&self, signal: &Signal<T>) -> bool {
        self.signals.owns(signal)
    }
}

impl<Props> ComponentHost<Props>
where
    Props: Clone + Send + 'static,
{
    /// Render one complete projection from the latest authoritative state.
    pub fn render(&mut self) -> Result<PreparedRender, ComponentHostFault> {
        let committed = self.begin_managed_render()?;
        let published = self.publish_managed_render(committed);
        debug_assert!(published.task_starts.is_empty());
        Ok(published.rendered)
    }

    /// Commit one candidate topology without exposing its projection/bindings.
    pub(crate) fn begin_managed_render(
        &mut self,
    ) -> Result<CommittedRenderTransition, ComponentHostFault> {
        self.signals
            .preflight_render()
            .map_err(ComponentHostFault::signal)?;
        let projection_revision = self
            .next_projection_revision
            .ok_or(ComponentHostFault::ProjectionRevisionExhausted)?;
        let generation = self.next_render_generation;
        self.next_render_generation = generation
            .checked_add(1)
            .ok_or(ComponentHostFault::RenderGenerationExhausted)?;

        let input = EventInput::new(generation);
        let origin = input.origin();
        let props = self.props.clone();
        let root = match self.root {
            ComponentRoot::Native(root) => root(props),
            ComponentRoot::WithEvents(root) => root(props, input),
        };
        let mut candidate =
            ComponentRenderStage::prepare_complete_root_candidate_with_capabilities(
                root,
                origin,
                &self.signals,
                self.driver_demand.as_ref(),
                self.tasks.as_ref(),
                self.application_exit.as_ref(),
            )?;
        if candidate.has_task_starts() {
            self.tasks
                .as_ref()
                .expect("task hooks require a task capability")
                .preflight()
                .map_err(ComponentHostFault::task_runtime)?;
        }
        let projection =
            candidate
                .stage()
                .projection()
                .clone()
                .with_execution_scope(ProjectionExecutionScope {
                    host_instance: self.id.instance,
                    mount_generation: self.mount_generation,
                });
        candidate.stage_mut().set_projection(projection.clone());
        let (stage, _, mounts) = candidate.commit_deferred();
        let (preparations, bindings, task_starts) = stage.into_execution_parts();
        self.next_projection_revision = projection_revision.checked_next();

        Ok(CommittedRenderTransition {
            host_id: self.id,
            generation,
            projection_revision,
            projection,
            preparations,
            bindings,
            task_starts,
            mounts,
        })
    }

    /// Activate a committed topology and atomically publish its render output.
    pub(crate) fn publish_managed_render(
        &mut self,
        committed: CommittedRenderTransition,
    ) -> PublishedManagedRender {
        let CommittedRenderTransition {
            host_id,
            generation,
            projection_revision,
            projection,
            preparations,
            bindings,
            task_starts,
            mounts,
        } = committed;
        debug_assert_eq!(host_id, self.id);
        let projection_prepared = preparations.is_empty();
        mounts.activate();
        self.current_projection_revision = Some(projection_revision);
        self.current_projection = Some(projection.clone());
        self.current_projection_prepared = projection_prepared;

        PublishedManagedRender {
            rendered: PreparedRender {
                host_id,
                generation,
                projection,
                preparations,
                bindings,
            },
            task_starts,
        }
    }
}

/// A committed render whose retired mount cleanup has not completed yet.
pub(crate) struct CommittedRenderTransition {
    host_id: ComponentHostId,
    generation: u64,
    projection_revision: ProjectionRevision,
    projection: RenderedProjection,
    preparations: PreparationSet,
    bindings: RenderBindings<ProviderEvent>,
    task_starts: Vec<MountTaskStart>,
    mounts: SignalMountTransition,
}

pub(crate) struct PublishedManagedRender {
    rendered: PreparedRender,
    task_starts: Vec<MountTaskStart>,
}

impl PublishedManagedRender {
    pub(crate) fn into_parts(self) -> (PreparedRender, Vec<MountTaskStart>) {
        (self.rendered, self.task_starts)
    }
}

impl CommittedRenderTransition {
    pub(crate) fn retired_mounts(&self) -> &[MountIdentity] {
        self.mounts.retired()
    }
}

/// One complete Provider-facing projection plus its local event bindings.
pub struct PreparedRender {
    host_id: ComponentHostId,
    generation: u64,
    projection: RenderedProjection,
    preparations: PreparationSet,
    bindings: RenderBindings<ProviderEvent>,
}

impl PreparedRender {
    pub fn component_host_id(&self) -> ComponentHostId {
        self.host_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn projection(&self) -> &RenderedProjection {
        &self.projection
    }

    pub(crate) fn into_execution_parts(
        self,
    ) -> (
        RenderedProjection,
        PreparationSet,
        RenderBindings<ProviderEvent>,
    ) {
        (self.projection, self.preparations, self.bindings)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentHostFault {
    #[error("ComponentHost mount generation space is exhausted")]
    MountGenerationExhausted,
    #[error("ComponentHost render generation space is exhausted")]
    RenderGenerationExhausted,
    #[error("ComponentHost projection revision space is exhausted")]
    ProjectionRevisionExhausted,
    #[error("signal runtime fault: {message}")]
    Signal { message: String },
    #[error("Component task runtime fault: {message}")]
    TaskRuntime { message: String },
    #[doc(hidden)]
    #[error("Component task runtime observed a task panic")]
    TaskPanicked,
    #[error(transparent)]
    Attempt(#[from] ComponentAttemptFault),
}

impl ComponentHostFault {
    fn signal(fault: SignalRenderError) -> Self {
        Self::Signal {
            message: fault.to_string(),
        }
    }

    fn task_runtime(fault: MountTaskSupervisorError) -> Self {
        match fault {
            MountTaskSupervisorError::Panicked => Self::TaskPanicked,
            fault => Self::TaskRuntime {
                message: fault.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use agentview_derive::{component, view};

    use super::*;

    #[derive(Clone)]
    struct RevisionProps {
        label: &'static str,
        panic: bool,
    }

    #[component]
    fn revision_root(props: RevisionProps, _events: EventInput<ProviderEvent>) -> Component {
        assert!(!props.panic, "candidate render panic");
        let label = props.label;
        view! { revision { "{label}" } }
    }

    #[test]
    fn projection_revision_tracks_only_successful_commits_and_is_not_reused() {
        let mut host = ComponentHost::new_with_event_input(
            revision_root,
            RevisionProps {
                label: "first",
                panic: false,
            },
        );
        assert_eq!(host.current_projection_revision(), None);

        host.render().expect("first render");
        let first = host
            .current_projection_revision()
            .expect("first projection revision");
        assert_eq!(first.get(), 1);

        host.set_props(RevisionProps {
            label: "second",
            panic: false,
        });
        assert!(host.is_dirty());
        assert_eq!(host.current_projection_revision(), Some(first));
        host.render().expect("second render");
        let second = host
            .current_projection_revision()
            .expect("second projection revision");
        assert!(second > first);
        assert!(!host.is_dirty());

        let committed = host
            .current_projection()
            .expect("committed projection")
            .clone();
        host.set_props(RevisionProps {
            label: "failed",
            panic: true,
        });
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.render()));
        assert!(panic.is_err(), "candidate render panic must propagate");
        assert_eq!(host.current_projection_revision(), Some(second));
        assert_eq!(host.current_projection(), Some(&committed));
        assert!(host.is_dirty());

        host.remount(RevisionProps {
            label: "remounted",
            panic: false,
        })
        .expect("remount");
        assert_eq!(host.current_projection_revision(), None);
        assert!(host.current_projection().is_none());

        host.render().expect("render remounted root");
        let remounted = host
            .current_projection_revision()
            .expect("remounted projection revision");
        assert!(remounted > second, "a remount must not reuse a revision");
    }
}
