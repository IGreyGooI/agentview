use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "legacy-provider-port")]
use super::authoring::Signal;
use super::{
    authoring::{
        application_exit::ApplicationExitControl, Component, ComponentAttemptFault,
        ComponentRenderStage, FrameRefreshDispatcher, InternalEventInput as EventInput,
        MountTaskStart, PreparationSet, RenderBindings, RetainedModelHistory,
    },
    execution::{DriverDemandHandle, ProjectionExecutionScope, ProviderEvent, RenderedProjection},
    signal::{MountIdentity, SignalMountTransition, SignalRenderError, SignalRuntime, SignalWake},
    task::{MountTaskHandle, MountTaskSupervisorError},
    ApplicationId, ReactionId,
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
    application_id: ApplicationId,
    mount_generation: u64,
    root: ComponentRoot<Props>,
    props: Props,
    signals: SignalRuntime,
    frame_refresh: Option<FrameRefreshDispatcher>,
    driver_demand: Option<DriverDemandHandle>,
    tasks: Option<MountTaskHandle>,
    application_exit: Option<ApplicationExitControl>,
    command_prefix: String,
    native_history_scope: Option<ReactionId>,
    model_history: RetainedModelHistory,
    next_render_generation: u64,
    next_projection_revision: Option<ProjectionRevision>,
    current_projection_revision: Option<ProjectionRevision>,
    current_projection: Option<RenderedProjection>,
    current_projection_prepared: bool,
    current_has_command_wait: bool,
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
            application_id: ApplicationId::fresh(),
            mount_generation: 1,
            root,
            props,
            signals: SignalRuntime::new(),
            frame_refresh: None,
            driver_demand,
            tasks,
            application_exit,
            command_prefix: String::new(),
            native_history_scope: None,
            model_history: RetainedModelHistory::default(),
            next_render_generation: 1,
            next_projection_revision: Some(ProjectionRevision::FIRST),
            current_projection_revision: None,
            current_projection: None,
            current_projection_prepared: false,
            current_has_command_wait: false,
        }
    }

    pub(crate) fn set_frame_refresh_dispatcher(
        &mut self,
        dispatcher: Option<FrameRefreshDispatcher>,
    ) {
        self.frame_refresh = dispatcher;
    }

    pub fn id(&self) -> ComponentHostId {
        self.id
    }

    /// Identity supplied to this host's event callbacks and render hooks.
    pub fn application_id(&self) -> ApplicationId {
        self.application_id
    }

    /// Select protocol records for a main render or one temporary lineage.
    /// Business Signals and mounted identities remain shared across selections.
    pub(crate) fn set_reaction_history_scope(&mut self, lineage: Option<ReactionId>) {
        if self.native_history_scope != lineage {
            self.native_history_scope = lineage;
            self.signals.mark_dirty();
        }
    }

    /// Release only an abandoned temporary protocol lineage from mounted tools.
    pub(crate) fn discard_temporary_history(&mut self, lineage: ReactionId) {
        self.model_history.discard_temporary(lineage);
    }

    pub fn props(&self) -> &Props {
        &self.props
    }

    /// Configure the executable and transport arguments advertised by CLI actions.
    pub(crate) fn set_command_prefix(&mut self, prefix: String) {
        self.command_prefix = prefix;
        self.signals.mark_dirty();
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
        self.model_history = RetainedModelHistory::default();
        self.current_projection = None;
        self.current_projection_prepared = false;
        self.current_has_command_wait = false;
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

    pub(crate) fn signal_wake(&self) -> SignalWake {
        self.signals.wake_handle()
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

    pub(crate) const fn has_command_wait(&self) -> bool {
        self.current_has_command_wait
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
        self.begin_validated_render(None, |_| Ok(()))
    }

    /// Validate a replacement against the live state before retiring any mount.
    /// An error drops the candidate transaction and retains the committed tree.
    pub(crate) fn begin_validated_render(
        &mut self,
        replacement: Option<Component>,
        validate: impl FnOnce(&RenderedProjection) -> Result<(), ComponentHostFault>,
    ) -> Result<CommittedRenderTransition, ComponentHostFault> {
        self.begin_render(replacement, validate, false)
            .map(|render| render.expect("ordinary renders allow mount changes"))
    }

    pub(crate) fn refresh_mounted_projection(&mut self) -> Result<(), ComponentHostFault> {
        if let Some(render) = self.begin_render(None, |_| Ok(()), true)? {
            self.publish_managed_render(render);
        }
        Ok(())
    }

    fn begin_render(
        &mut self,
        replacement: Option<Component>,
        validate: impl FnOnce(&RenderedProjection) -> Result<(), ComponentHostFault>,
        preserve_mounts: bool,
    ) -> Result<Option<CommittedRenderTransition>, ComponentHostFault> {
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

        let input = EventInput::for_application(generation, self.application_id);
        let origin = input.origin();
        let root = replacement.unwrap_or_else(|| {
            let props = self.props.clone();
            match self.root {
                ComponentRoot::Native(root) => root(props),
                ComponentRoot::WithEvents(root) => root(props, input),
            }
        });
        let mut candidate =
            ComponentRenderStage::prepare_complete_root_candidate_with_command_prefix(
                root,
                origin,
                &self.signals,
                self.driver_demand.as_ref(),
                self.frame_refresh.as_ref(),
                self.tasks.as_ref(),
                self.application_exit.as_ref(),
                &self.command_prefix,
                self.native_history_scope,
            )?;
        // An in-flight Frame still owns callbacks from the current mounts.
        // Publishing a different topology could retire those callbacks before
        // the provider delivers their events. Defer that render to the normal
        // Frame boundary and leave the dirty marker pending.
        if preserve_mounts && (!candidate.preserves_mounts() || candidate.has_task_starts()) {
            return Ok(None);
        }
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
        validate(&projection)?;
        let has_command_wait = candidate.stage().has_command_wait();
        let (stage, _, mounts) = candidate.commit_deferred();
        self.current_has_command_wait = has_command_wait;
        let (preparations, bindings, task_starts) = stage.into_execution_parts();
        self.next_projection_revision = projection_revision.checked_next();

        Ok(Some(CommittedRenderTransition {
            host_id: self.id,
            generation,
            projection_revision,
            projection,
            preparations,
            bindings,
            task_starts,
            mounts,
        }))
    }

    /// Install the factory whose output was already validated and committed.
    pub(crate) fn install_rendered_props(&mut self, props: Props) {
        self.props = props;
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
        self.model_history = bindings.retained_model_history();
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
    use std::num::{NonZeroU128, NonZeroU64};

    use crate::component::{authoring::NativeToolCall, EventSource, ReactionKind};
    use agentview_derive::{component, view};

    use super::*;

    fn history_root(_: ()) -> Component {
        NativeToolCall::named("work").on_call(|context, call| async move {
            Ok::<_, std::convert::Infallible>(call.output(format!("{:?}", context.reaction_kind())))
        })
    }

    #[tokio::test]
    async fn shared_mount_renders_only_the_selected_native_history_lineage() {
        use crate::{
            component::{
                execution::{FrameRevision, TargetEpoch, TargetIdentity, ToolCall},
                provenance::EventScope,
            },
            transcript::CanonicalInputItem,
        };

        let mut host = ComponentHost::new_root(history_root, ());
        let frame = FrameRevision::new(
            NonZeroU128::new(1).unwrap(),
            TargetIdentity::new(NonZeroU128::new(1).unwrap()),
            TargetEpoch::new(NonZeroU64::new(1).unwrap()),
            NonZeroU64::new(1).unwrap(),
        );
        let main_id = ReactionId::fresh();
        let branch = ReactionId::fresh();
        let (_, _, mut main) = host.render().unwrap().into_execution_parts();
        let main_scope = EventScope::reaction(host.application_id(), main_id, frame);
        main.set_event_scope(main_scope);
        let (output, _) = main
            .start_native_tool_record_with_context(
                main_scope.event(EventSource::NativeTool {
                    call_id: "shared".to_owned(),
                    output: None,
                    ordinal: None,
                }),
                ToolCall::new("shared", "work", "{}").unwrap(),
            )
            .unwrap();
        output.await.unwrap();

        host.set_reaction_history_scope(Some(branch));
        assert!(host.is_dirty());
        let (projection, _, mut temporary) = host.render().unwrap().into_execution_parts();
        assert!(projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .next()
            .is_none());
        let temporary_scope = EventScope::reaction_with_kind(
            host.application_id(),
            branch,
            ReactionKind::Temporary,
            frame,
        );
        temporary.set_event_scope(temporary_scope);
        let (output, _) = temporary
            .start_native_tool_record_with_context(
                temporary_scope.event(EventSource::NativeTool {
                    call_id: "shared".to_owned(),
                    output: None,
                    ordinal: None,
                }),
                ToolCall::new("shared", "work", "{}").unwrap(),
            )
            .unwrap();
        output.await.unwrap();

        host.set_reaction_history_scope(None);
        let main_projection = host.render().unwrap();
        let main_items = main_projection
            .projection()
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            main_items,
            [
                CanonicalInputItem::tool_call("shared", "work", "{}").unwrap(),
                CanonicalInputItem::tool_result("shared", "Some(Main)").unwrap(),
            ]
        );
        host.set_reaction_history_scope(Some(branch));
        let branch_projection = host.render().unwrap();
        let branch_items = branch_projection
            .projection()
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            branch_items,
            [
                CanonicalInputItem::tool_call("shared", "work", "{}").unwrap(),
                CanonicalInputItem::tool_result("shared", "Some(Temporary)").unwrap(),
            ]
        );
        host.discard_temporary_history(branch);
        let released = host.render().unwrap();
        assert!(released
            .projection()
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .next()
            .is_none());
        host.set_reaction_history_scope(None);
        let retained_main = host.render().unwrap();
        assert_eq!(
            retained_main
                .projection()
                .nodes()
                .iter()
                .flat_map(|node| node.items())
                .cloned()
                .collect::<Vec<_>>(),
            main_items,
        );
    }

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
