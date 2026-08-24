use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    authoring::{
        Component, ComponentAttemptFault, ComponentRenderStage, EventInput, RenderBindings, Signal,
    },
    execution::{ProjectionExecutionScope, ProviderEvent, RenderedProjection},
    signal::{SignalRenderError, SignalRuntime},
};

static NEXT_COMPONENT_HOST_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime-only identity of one mounted Component application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentHostId {
    instance: u64,
}

/// Long-lived owner of root props and retained Component signal slots.
pub struct ComponentHost<Props> {
    id: ComponentHostId,
    mount_generation: u64,
    root: fn(Props, EventInput<ProviderEvent>) -> Component,
    props: Props,
    signals: SignalRuntime,
    next_render_generation: u64,
    current_projection: Option<RenderedProjection>,
}

impl<Props> ComponentHost<Props> {
    pub fn new(root: fn(Props, EventInput<ProviderEvent>) -> Component, props: Props) -> Self {
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
            next_render_generation: 1,
            current_projection: None,
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
        self.current_projection = None;
        self.signals.mark_dirty();
        Ok(self.id)
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

    /// Return the latest complete projection while it still matches current state.
    pub fn current_projection(&self) -> Option<&RenderedProjection> {
        (!self.signals.is_dirty())
            .then_some(())
            .and(self.current_projection.as_ref())
    }

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
        self.signals
            .preflight_render()
            .map_err(ComponentHostFault::signal)?;
        let generation = self.next_render_generation;
        self.next_render_generation = generation
            .checked_add(1)
            .ok_or(ComponentHostFault::RenderGenerationExhausted)?;

        let input = EventInput::new(generation);
        let origin = input.origin();
        let props = self.props.clone();
        let root =
            catch_unwind(AssertUnwindSafe(|| (self.root)(props, input))).map_err(|panic| {
                ComponentHostFault::RootPanicked {
                    message: panic_payload_message(&*panic),
                }
            })?;
        let (mut stage, _) =
            ComponentRenderStage::prepare_complete_root_with_signals(root, origin, &self.signals)?;
        let projection =
            stage
                .projection()
                .clone()
                .with_execution_scope(ProjectionExecutionScope {
                    host_instance: self.id.instance,
                    mount_generation: self.mount_generation,
                });
        stage.set_projection(projection.clone());
        self.current_projection = Some(projection.clone());

        Ok(PreparedRender {
            host_id: self.id,
            generation,
            projection,
            bindings: stage.into_bindings(),
        })
    }
}

/// One complete Provider-facing projection plus its local event bindings.
pub struct PreparedRender {
    host_id: ComponentHostId,
    generation: u64,
    projection: RenderedProjection,
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
    ) -> (RenderedProjection, RenderBindings<ProviderEvent>) {
        (self.projection, self.bindings)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentHostFault {
    #[error("root Component panicked before render: {message}")]
    RootPanicked { message: String },
    #[error("ComponentHost mount generation space is exhausted")]
    MountGenerationExhausted,
    #[error("ComponentHost render generation space is exhausted")]
    RenderGenerationExhausted,
    #[error("signal runtime fault: {message}")]
    Signal { message: String },
    #[error(transparent)]
    Attempt(#[from] ComponentAttemptFault),
}

impl ComponentHostFault {
    fn signal(fault: SignalRenderError) -> Self {
        Self::Signal {
            message: fault.to_string(),
        }
    }
}

fn panic_payload_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("non-string panic payload"))
}
