use std::{
    collections::{HashMap, HashSet},
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
};

use futures::FutureExt;

use crate::{
    component::{
        execution::{RenderedProjection, RenderedProjectionNode},
        signal::{SignalRenderError, SignalRenderTransaction, SignalRuntime},
        ComponentId,
    },
    pom::{BlockChildren, Document, ResolvedDocument},
    pom_resolution::resolve_artifact_document,
};

use super::{
    capture::{
        build_projection_items, ComponentCaptureError, ProjectionFragmentCapture,
        ProjectionRunCapture,
    },
    declaration::{ComponentNode, Placement, RepeatableRender, ScopeRender},
    event_input::EventInputOrigin,
    event_listener::EventListenerDispatchFault,
    handler::panic_message,
    native_tool::{await_output, NativeToolCallDeclaration, NativeToolDispatchFault},
    render_context::{HookRenderAbort, HookRenderContext},
    streaming_xml::{
        MountedStreamingRoute, ParsedContractEvent, StreamingXmlDispatchFault,
        StreamingXmlMountFault,
    },
    Component,
};

#[cfg(test)]
mod bindings_tests;
mod listener;

use listener::{build_streaming_routes, MountedListener};

const MAX_DECLARATION_DEPTH: usize = 64;
type NativeToolFuture = Pin<
    Box<
        dyn futures::Future<
                Output = Result<crate::component::execution::ToolOutput, NativeToolDispatchFault>,
            > + Send,
    >,
>;

/// One completed render before its projection and local bindings are separated.
/// The stage is consumed after the Provider-facing transcript is copied out.
pub(crate) struct ComponentRenderStage<Root> {
    projection: RenderedProjection,
    bindings: RenderBindings<Root>,
}

impl<Root> ComponentRenderStage<Root>
where
    Root: Send + Sync + 'static,
{
    pub(crate) fn prepare_complete_root_with_signals(
        root: Component,
        event_origin: EventInputOrigin,
        signals: &SignalRuntime,
    ) -> Result<(Self, Option<ResolvedDocument>), ComponentAttemptFault> {
        let mut signal_render = signals
            .begin_render()
            .map_err(ComponentAttemptFault::signal)?;
        let prepared = Self::mount(root, event_origin, &mut signal_render)?;
        signal_render.commit();
        Ok(prepared)
    }

    fn mount(
        root: Component,
        event_origin: EventInputOrigin,
        signal_render: &mut SignalRenderTransaction<'_>,
    ) -> Result<(Self, Option<ResolvedDocument>), ComponentAttemptFault> {
        let mut listeners = Vec::new();
        let mut native_tools = Vec::new();
        let root_id = ComponentId::root();
        let mut capture = RenderCapture::new(root_id.clone());
        let mut scope_cursor = 0;
        let mut system_scope_cursor = 0;
        let rendered = visit_render(
            root,
            &root_id,
            &mut scope_cursor,
            &mut system_scope_cursor,
            &mut Vec::new(),
            None,
            0,
            event_origin,
            signal_render,
            &mut listeners,
            &mut native_tools,
            &mut capture,
        );
        rendered?;

        let streaming_routes = build_streaming_routes(&listeners)?;
        let system_candidate = capture.resolve_system_candidate()?;
        let projection = capture.into_projection(system_candidate.as_ref())?;
        let projection = RenderedProjection::with_native_tool_names(
            projection.nodes().to_vec(),
            native_tools
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
        )
        .map_err(|error| ComponentAttemptFault::RuntimeInvariant {
            message: error.to_string(),
        })?;
        Ok((
            Self {
                projection,
                bindings: RenderBindings {
                    listeners,
                    native_tools,
                    streaming_routes,
                    finished: false,
                    faulted: false,
                    marker: std::marker::PhantomData,
                },
            },
            system_candidate,
        ))
    }

    pub(crate) fn projection(&self) -> &RenderedProjection {
        &self.projection
    }

    pub(crate) fn set_projection(&mut self, projection: RenderedProjection) {
        self.projection = projection;
    }

    pub(crate) fn into_bindings(self) -> RenderBindings<Root> {
        self.bindings
    }
}

/// Generation-local event routes, streaming parsers, and async handlers.
///
/// Dropping this value abandons parser accumulators and terminal handlers. Only
/// [`finish_normal`](Self::finish_normal) performs semantic stream completion.
pub(crate) struct RenderBindings<Root> {
    listeners: Vec<MountedListener>,
    native_tools: Vec<NativeToolCallDeclaration>,
    streaming_routes: Vec<MountedStreamingRoute>,
    finished: bool,
    faulted: bool,
    marker: std::marker::PhantomData<fn(Root)>,
}

impl<Root> RenderBindings<Root>
where
    Root: Send + Sync + 'static,
{
    /// Dispatch one immutable root event without rendering.
    pub(crate) async fn dispatch(&mut self, event: Root) -> Result<(), ComponentAttemptFault> {
        self.ensure_open()?;
        let result = AssertUnwindSafe(self.dispatch_inner(event))
            .catch_unwind()
            .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(fault)) => self.abort(fault),
            Err(panic) => self.abort(ComponentAttemptFault::AttemptPanicked {
                phase: "dispatch",
                message: panic_message(&*panic),
            }),
        }
    }

    pub(crate) fn start_native_tool(
        &mut self,
        call: crate::component::execution::ToolCall,
    ) -> Result<NativeToolFuture, ComponentAttemptFault> {
        self.ensure_open()?;
        let call_id = call.call_id().to_owned();
        let mut matching = self
            .native_tools
            .iter_mut()
            .filter(|tool| tool.name() == call.name());
        let Some(tool) = matching.next() else {
            return Err(ComponentAttemptFault::NativeToolBinding {
                call_id,
                name: call.name().to_owned(),
                bindings: 0,
            });
        };
        if matching.next().is_some() {
            return Err(ComponentAttemptFault::NativeToolBinding {
                call_id,
                name: call.name().to_owned(),
                bindings: 2,
            });
        }
        let future = tool
            .start(call)
            .map_err(ComponentAttemptFault::native_tool)?;
        Ok(Box::pin(await_output(call_id, future)))
    }

    async fn dispatch_inner(&mut self, event: Root) -> Result<(), ComponentAttemptFault> {
        let mut parsed_by_listener = (0..self.listeners.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        for route in &mut self.streaming_routes {
            let parsed = route
                .dispatch_root(&event)
                .map_err(ComponentAttemptFault::streaming_input)?;
            queue_parsed_events(parsed, &mut parsed_by_listener)?;
        }

        for (listener_index, parsed) in parsed_by_listener.into_iter().enumerate() {
            let listener = self.listeners.get_mut(listener_index).ok_or_else(|| {
                ComponentAttemptFault::RuntimeInvariant {
                    message: format!("missing listener at index {listener_index}"),
                }
            })?;
            listener.dispatch_root(&event).await?;
            for parsed in parsed {
                dispatch_parsed(listener, parsed).await?;
            }
        }
        Ok(())
    }

    /// Finish parsers and await terminal handlers after normal Provider EOF.
    pub(crate) async fn finish_normal(&mut self) -> Result<(), ComponentAttemptFault> {
        self.ensure_open()?;
        let result = AssertUnwindSafe(self.finish_normal_inner())
            .catch_unwind()
            .await;
        match result {
            Ok(Ok(())) => {
                self.finished = true;
                Ok(())
            }
            Ok(Err(fault)) => self.abort(fault),
            Err(panic) => self.abort(ComponentAttemptFault::AttemptPanicked {
                phase: "finish",
                message: panic_message(&*panic),
            }),
        }
    }

    async fn finish_normal_inner(&mut self) -> Result<(), ComponentAttemptFault> {
        let mut parsed_by_listener = (0..self.listeners.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        for route in &mut self.streaming_routes {
            let parsed = route
                .finish()
                .map_err(ComponentAttemptFault::streaming_input)?;
            queue_parsed_events(parsed, &mut parsed_by_listener)?;
        }

        for (listener_index, parsed) in parsed_by_listener.into_iter().enumerate() {
            let listener = self.listeners.get_mut(listener_index).ok_or_else(|| {
                ComponentAttemptFault::RuntimeInvariant {
                    message: format!("missing listener at index {listener_index}"),
                }
            })?;
            for parsed in parsed {
                dispatch_parsed(listener, parsed).await?;
            }
            listener.finish().await?;
        }
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), ComponentAttemptFault> {
        if self.finished {
            Err(ComponentAttemptFault::AfterStreamFinish)
        } else if self.faulted {
            Err(ComponentAttemptFault::AttemptInactive)
        } else {
            Ok(())
        }
    }

    fn abort<T>(&mut self, fault: ComponentAttemptFault) -> Result<T, ComponentAttemptFault> {
        self.faulted = true;
        Err(fault)
    }
}

fn queue_parsed_events(
    events: Vec<ParsedContractEvent>,
    by_listener: &mut [Vec<ParsedContractEvent>],
) -> Result<(), ComponentAttemptFault> {
    for event in events {
        let listener_index = match &event {
            ParsedContractEvent::Decoded { listener_index, .. }
            | ParsedContractEvent::Invalid { listener_index, .. } => *listener_index,
        };
        let queue = by_listener.get_mut(listener_index).ok_or_else(|| {
            ComponentAttemptFault::RuntimeInvariant {
                message: format!("streaming parser targeted missing listener {listener_index}"),
            }
        })?;
        queue.push(event);
    }
    Ok(())
}

async fn dispatch_parsed(
    listener: &mut MountedListener,
    event: ParsedContractEvent,
) -> Result<(), ComponentAttemptFault> {
    match event {
        ParsedContractEvent::Decoded { value, .. } => {
            listener.dispatch_streaming(Some(value), None).await
        }
        ParsedContractEvent::Invalid { diagnostic, .. } => {
            listener.dispatch_streaming(None, Some(diagnostic)).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn visit_render(
    component: Component,
    owner: &ComponentId,
    scope_cursor: &mut usize,
    system_scope_cursor: &mut usize,
    structural_path: &mut Vec<usize>,
    forced: Option<Placement>,
    depth: usize,
    event_origin: EventInputOrigin,
    signal_render: &mut SignalRenderTransaction<'_>,
    listeners: &mut Vec<MountedListener>,
    native_tools: &mut Vec<NativeToolCallDeclaration>,
    capture: &mut RenderCapture,
) -> Result<(), ComponentAttemptFault> {
    ensure_depth(depth)?;
    match component.node {
        ComponentNode::Fragment(children) => {
            for (position, child) in children.into_iter().enumerate() {
                structural_path.push(position);
                visit_render(
                    child,
                    owner,
                    scope_cursor,
                    system_scope_cursor,
                    structural_path,
                    forced,
                    depth + 1,
                    event_origin,
                    signal_render,
                    listeners,
                    native_tools,
                    capture,
                )?;
                structural_path.pop();
            }
        }
        ComponentNode::Pom(fragment) => {
            let placement = forced.unwrap_or(Placement::User);
            let document = fragment
                .into_document()
                .map_err(ComponentCaptureError::from)?;
            if placement == Placement::SystemOnce {
                capture.system.extend(document.into_children());
            } else {
                capture.push(owner, placement, document.into_children(), None)?;
            }
        }
        ComponentNode::Scope { function, render } => {
            let is_system_scope = forced == Some(Placement::SystemOnce);
            let scope_cursor = if is_system_scope {
                system_scope_cursor
            } else {
                scope_cursor
            };
            let position = *scope_cursor;
            *scope_cursor = scope_cursor.saturating_add(1);
            let component_id = if is_system_scope {
                owner.system_child(function, position)
            } else {
                owner.child(function, position)
            };
            capture.register_component(component_id.clone())?;
            let rendered = match render {
                ScopeRender::Fresh(render) => invoke_fresh(function, render)?,
                ScopeRender::Repeatable(renderer) => invoke_repeatable(
                    signal_render,
                    component_id.clone(),
                    &renderer,
                    forced != Some(Placement::SystemOnce),
                )?,
            };
            let mut child_cursor = 0;
            let mut child_system_scope_cursor = 0;
            visit_render(
                rendered,
                &component_id,
                &mut child_cursor,
                &mut child_system_scope_cursor,
                &mut Vec::new(),
                forced,
                depth + 1,
                event_origin,
                signal_render,
                listeners,
                native_tools,
                capture,
            )?;
        }
        ComponentNode::Placement { placement, child } => {
            let resolved = forced.or(Some(placement));
            let child = match child.node {
                ComponentNode::Scope {
                    function,
                    render: ScopeRender::Fresh(render),
                } if crate::component::authoring::__private::is_system_once_boundary(function) => {
                    invoke_fresh(function, render)?
                }
                node => Component::from_node(node),
            };
            visit_render(
                child,
                owner,
                scope_cursor,
                system_scope_cursor,
                structural_path,
                resolved,
                depth + 1,
                event_origin,
                signal_render,
                listeners,
                native_tools,
                capture,
            )?;
        }
        ComponentNode::Diff { slot, child } => {
            let address = capture.register_diff(owner, structural_path, slot, forced)?;
            if let Some(address) = address {
                let ComponentNode::Pom(fragment) = child.node else {
                    return Err(ComponentAttemptFault::RuntimeInvariant {
                        message: "a diff boundary must contain exactly one POM root".to_owned(),
                    });
                };
                let document = fragment
                    .into_document()
                    .map_err(ComponentCaptureError::from)?;
                capture.push(
                    owner,
                    Placement::User,
                    document.into_children(),
                    Some((address.structural_path, address.slot)),
                )?;
            } else {
                visit_render(
                    *child,
                    owner,
                    scope_cursor,
                    system_scope_cursor,
                    structural_path,
                    forced,
                    depth + 1,
                    event_origin,
                    signal_render,
                    listeners,
                    native_tools,
                    capture,
                )?;
            }
        }
        ComponentNode::EventListener(declaration) => {
            if forced == Some(Placement::SystemOnce) {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "EventListener/EventInput",
                });
            }
            listeners.push(MountedListener::new_event(declaration, event_origin)?);
        }
        ComponentNode::XmlStreamingToolCall(declaration) => {
            let placement = forced.unwrap_or(Placement::User);
            if placement == Placement::SystemOnce {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "XmlStreamingToolCall/EventInput",
                });
            }
            let document = declaration
                .prompt_document()
                .map_err(ComponentCaptureError::from)?;
            capture.push(owner, placement, document.into_children(), None)?;
            listeners.push(MountedListener::new_streaming(*declaration, event_origin)?);
        }
        ComponentNode::NativeToolCall(declaration) => {
            if forced == Some(Placement::SystemOnce) {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "NativeToolCall",
                });
            }
            native_tools.push(*declaration);
        }
    }
    Ok(())
}

fn invoke_fresh(
    function: &'static str,
    renderer: Box<dyn FnOnce() -> Component + Send + 'static>,
) -> Result<Component, ComponentAttemptFault> {
    catch_unwind(AssertUnwindSafe(renderer)).map_err(|panic| render_panic(function, &*panic))
}

fn invoke_repeatable(
    signals: &mut SignalRenderTransaction<'_>,
    component: ComponentId,
    renderer: &RepeatableRender,
    attempt_local_allowed: bool,
) -> Result<Component, ComponentAttemptFault> {
    let rendered = catch_unwind(AssertUnwindSafe(|| {
        signals.render_component(component.clone(), |signal_scope| {
            let mut hooks = if attempt_local_allowed {
                HookRenderContext::new(signal_scope)
            } else {
                HookRenderContext::for_system(signal_scope)
            };
            Ok(renderer(&mut hooks))
        })
    }));
    match rendered {
        Ok(Ok(component)) => Ok(component),
        Ok(Err(fault)) => Err(ComponentAttemptFault::signal(fault)),
        Err(panic) => Err(render_panic(component.as_str(), &*panic)),
    }
}

fn render_panic(component: &str, panic: &(dyn std::any::Any + Send)) -> ComponentAttemptFault {
    if let Some(HookRenderAbort::Signal(fault)) = panic.downcast_ref::<HookRenderAbort>() {
        return ComponentAttemptFault::signal(fault.clone());
    }
    if let Some(HookRenderAbort::SystemAttemptLocal { capability }) =
        panic.downcast_ref::<HookRenderAbort>()
    {
        return ComponentAttemptFault::SystemAttemptLocal {
            component: component.to_owned(),
            capability,
        };
    }
    ComponentAttemptFault::RenderPanicked {
        component: component.to_owned(),
        message: panic_message(panic),
    }
}

fn ensure_depth(depth: usize) -> Result<(), ComponentAttemptFault> {
    if depth <= MAX_DECLARATION_DEPTH {
        Ok(())
    } else {
        Err(ComponentCaptureError::DeclarationDepthExceeded {
            maximum: MAX_DECLARATION_DEPTH,
        }
        .into())
    }
}

struct RenderCapture {
    system: BlockChildren,
    nodes: Vec<ProjectionNodeCapture>,
    last_run: Option<ProjectionRunAddress>,
    node_indexes: HashMap<ComponentId, usize>,
    diff_addresses: HashSet<DiffAddress>,
}

impl RenderCapture {
    fn new(root: ComponentId) -> Self {
        let mut node_indexes = HashMap::new();
        node_indexes.insert(root.clone(), 0);
        Self {
            system: BlockChildren::new(),
            nodes: vec![ProjectionNodeCapture {
                identity: root,
                runs: Vec::new(),
            }],
            last_run: None,
            node_indexes,
            diff_addresses: HashSet::new(),
        }
    }

    fn register_component(&mut self, identity: ComponentId) -> Result<(), ComponentAttemptFault> {
        if self.node_indexes.contains_key(&identity) {
            return Err(ComponentAttemptFault::RuntimeInvariant {
                message: format!("duplicate mounted Component identity `{identity}`"),
            });
        }
        let index = self.nodes.len();
        self.nodes.push(ProjectionNodeCapture {
            identity: identity.clone(),
            runs: Vec::new(),
        });
        self.node_indexes.insert(identity, index);
        Ok(())
    }

    fn push(
        &mut self,
        owner: &ComponentId,
        placement: Placement,
        children: BlockChildren,
        diff: Option<(Vec<usize>, &'static str)>,
    ) -> Result<(), ComponentAttemptFault> {
        if children.is_empty() {
            return Ok(());
        }
        let fragment = match diff {
            Some((structural_path, slot)) => ProjectionFragmentCapture::Diff {
                structural_path,
                slot,
                children,
            },
            None => ProjectionFragmentCapture::Complete(children),
        };
        let index = self.node_indexes.get(owner).copied().ok_or_else(|| {
            ComponentAttemptFault::RuntimeInvariant {
                message: format!("missing projection node for Component `{owner}`"),
            }
        })?;
        let node =
            self.nodes
                .get_mut(index)
                .ok_or_else(|| ComponentAttemptFault::RuntimeInvariant {
                    message: format!("missing projection node index {index}"),
                })?;
        if let Some(address) = self.last_run {
            if address.node_index == index {
                let previous = node.runs.get_mut(address.run_index).ok_or_else(|| {
                    ComponentAttemptFault::RuntimeInvariant {
                        message: format!(
                            "missing projection run {} for Component `{owner}`",
                            address.run_index
                        ),
                    }
                })?;
                if previous.placement != placement {
                    let run_index = node.runs.len();
                    node.runs.push(ProjectionRunCapture {
                        placement,
                        fragments: vec![fragment],
                    });
                    self.last_run = Some(ProjectionRunAddress {
                        node_index: index,
                        run_index,
                    });
                    return Ok(());
                }
                previous.fragments.push(fragment);
                return Ok(());
            }
        }
        let run_index = node.runs.len();
        node.runs.push(ProjectionRunCapture {
            placement,
            fragments: vec![fragment],
        });
        self.last_run = Some(ProjectionRunAddress {
            node_index: index,
            run_index,
        });
        Ok(())
    }

    fn resolve_system_candidate(
        &mut self,
    ) -> Result<Option<ResolvedDocument>, ComponentAttemptFault> {
        if self.system.is_empty() {
            return Ok(None);
        }
        resolve_artifact_document(Document::new(std::mem::take(&mut self.system)))
            .map(Some)
            .map_err(ComponentCaptureError::from)
            .map_err(ComponentAttemptFault::from)
    }

    fn into_projection(
        self,
        system: Option<&ResolvedDocument>,
    ) -> Result<RenderedProjection, ComponentAttemptFault> {
        let nodes = self
            .nodes
            .into_iter()
            .enumerate()
            .map(|(index, node)| {
                let node_system = if index == 0 { system } else { None };
                let built = build_projection_items(node_system, node.runs)?;
                Ok(RenderedProjectionNode::with_diff_templates(
                    node.identity.to_string(),
                    built.items,
                    built.diffs,
                    built.diff_templates,
                ))
            })
            .collect::<Result<Vec<_>, ComponentCaptureError>>()?;
        RenderedProjection::from_nodes(nodes)
            .map_err(ComponentCaptureError::from)
            .map_err(ComponentAttemptFault::from)
    }

    fn register_diff(
        &mut self,
        component: &ComponentId,
        structural_path: &[usize],
        slot: &'static str,
        forced: Option<Placement>,
    ) -> Result<Option<DiffAddress>, ComponentAttemptFault> {
        validate_diff_slot(slot)?;
        if forced.is_some_and(|placement| placement != Placement::User) {
            return Ok(None);
        }
        let address = DiffAddress {
            component: component.clone(),
            structural_path: structural_path.to_vec(),
            slot,
        };
        if !self.diff_addresses.insert(address.clone()) {
            return Err(ComponentCaptureError::DuplicateDiffAddress {
                address: format!(
                    "{}:{:?}:{}",
                    address.component, address.structural_path, address.slot
                ),
            }
            .into());
        }
        Ok(Some(address))
    }
}

struct ProjectionNodeCapture {
    identity: ComponentId,
    runs: Vec<ProjectionRunCapture>,
}

#[derive(Clone, Copy)]
struct ProjectionRunAddress {
    node_index: usize,
    run_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiffAddress {
    component: ComponentId,
    structural_path: Vec<usize>,
    slot: &'static str,
}

fn validate_diff_slot(slot: &'static str) -> Result<(), ComponentAttemptFault> {
    if !slot.is_empty()
        && slot
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(ComponentCaptureError::InvalidDiffSlot { slot }.into())
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComponentAttemptFault {
    #[error("provider stream lifecycle has already finished")]
    AfterStreamFinish,
    #[error("render bindings are no longer active")]
    AttemptInactive,
    #[error("invalid listener {kind} `{value}`")]
    InvalidListenerToken {
        kind: &'static str,
        value: &'static str,
    },
    #[error("listener `{identity}` uses an EventInput from another render generation")]
    ForeignEventInput { identity: &'static str },
    #[error("event selector metadata collision on route `{route}`")]
    EventSelectorCollision { route: &'static str },
    #[error("System component `{component}` declared generation-local {capability}")]
    SystemAttemptLocal {
        component: String,
        capability: &'static str,
    },
    #[error("component `{component}` panicked during render: {message}")]
    RenderPanicked { component: String, message: String },
    #[error("render bindings panicked during {phase}: {message}")]
    AttemptPanicked {
        phase: &'static str,
        message: String,
    },
    #[error("signal runtime fault: {message}")]
    Signal { message: String },
    #[error("event listener dispatch fault: {message}")]
    ListenerDispatch { message: String },
    #[error("streaming input fault: {message}")]
    StreamingInput { message: String },
    #[error("streaming contract mount fault: {message}")]
    StreamingMount { message: String },
    #[error("native tool binding for `{name}` call `{call_id}` has {bindings} handlers")]
    NativeToolBinding {
        call_id: String,
        name: String,
        bindings: usize,
    },
    #[error("native tool lane fault: {message}")]
    NativeToolLane { message: String },
    #[error("component render invariant failed: {message}")]
    RuntimeInvariant { message: String },
    #[error(transparent)]
    Capture(#[from] ComponentCaptureError),
}

impl ComponentAttemptFault {
    fn signal(fault: SignalRenderError) -> Self {
        Self::Signal {
            message: fault.to_string(),
        }
    }

    fn listener_dispatch(fault: EventListenerDispatchFault) -> Self {
        Self::ListenerDispatch {
            message: fault.to_string(),
        }
    }

    fn streaming_mount(fault: StreamingXmlMountFault) -> Self {
        match fault {
            StreamingXmlMountFault::ForeignEventInput { identity } => {
                Self::ForeignEventInput { identity }
            }
            fault @ StreamingXmlMountFault::DuplicateTarget { .. } => Self::StreamingMount {
                message: fault.to_string(),
            },
        }
    }

    fn streaming_input(fault: StreamingXmlDispatchFault) -> Self {
        Self::StreamingInput {
            message: fault.to_string(),
        }
    }

    pub(crate) fn native_tool(fault: NativeToolDispatchFault) -> Self {
        Self::NativeToolLane {
            message: fault.to_string(),
        }
    }
}
