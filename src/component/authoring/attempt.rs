use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
};

use crate::{
    component::{
        execution::{DriverDemandHandle, RenderedProjection, RenderedProjectionNode},
        signal::{
            SignalMountTransition, SignalRenderError, SignalRenderTransaction, SignalRuntime,
        },
        task::MountTaskHandle,
        ComponentId,
    },
    pom::{BlockChildren, Document, ResolvedDocument},
    pom_resolution::resolve_artifact_document,
};

use super::{
    application_exit::ApplicationExitControl,
    async_task::MountTaskStart,
    capture::{
        build_projection_items, ComponentCaptureError, ProjectionFragmentCapture,
        ProjectionRunCapture,
    },
    declaration::{ComponentNode, Placement, RepeatableRender, ScopeRender},
    event_input::EventInputOrigin,
    event_listener::EventListenerDispatchFault,
    native_tool::{
        await_output, NativeToolCallDeclaration, NativeToolDispatchFault, NativeToolHistory,
        NativeToolRecord,
    },
    preparation::PreparationSet,
    reaction_completion::{ReactionCompletionDeclaration, ReactionCompletionDispatchFault},
    render_context::HookRenderContext,
    streaming_attempt::ContractDeclaration,
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
    preparations: PreparationSet,
    bindings: RenderBindings<Root>,
    task_starts: Vec<MountTaskStart>,
}

/// One complete render candidate whose hook topology is not committed yet.
pub(crate) struct ComponentRenderCandidate<'runtime, Root> {
    stage: ComponentRenderStage<Root>,
    system_candidate: Option<ResolvedDocument>,
    signal_render: SignalRenderTransaction<'runtime>,
}

impl<Root> ComponentRenderCandidate<'_, Root>
where
    Root: Send + Sync + 'static,
{
    pub(crate) fn stage(&self) -> &ComponentRenderStage<Root> {
        &self.stage
    }

    pub(crate) fn stage_mut(&mut self) -> &mut ComponentRenderStage<Root> {
        &mut self.stage
    }

    pub(crate) fn has_task_starts(&self) -> bool {
        !self.stage.task_starts.is_empty()
    }

    pub(crate) fn commit(self) -> (ComponentRenderStage<Root>, Option<ResolvedDocument>) {
        let Self {
            stage,
            system_candidate,
            signal_render,
        } = self;
        signal_render.commit();
        (stage, system_candidate)
    }

    pub(crate) fn commit_deferred(
        self,
    ) -> (
        ComponentRenderStage<Root>,
        Option<ResolvedDocument>,
        SignalMountTransition,
    ) {
        let Self {
            stage,
            system_candidate,
            signal_render,
        } = self;
        let transition = signal_render.commit_deferred();
        (stage, system_candidate, transition)
    }
}

impl<Root> ComponentRenderStage<Root>
where
    Root: Send + Sync + 'static,
{
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "retained for crate-internal render staging tests without a production caller"
        )
    )]
    pub(crate) fn prepare_complete_root_with_signals(
        root: Component,
        event_origin: EventInputOrigin,
        signals: &SignalRuntime,
    ) -> Result<(Self, Option<ResolvedDocument>), ComponentAttemptFault> {
        Self::prepare_complete_root_with_capabilities(root, event_origin, signals, None)
    }

    pub(crate) fn prepare_complete_root_with_capabilities(
        root: Component,
        event_origin: EventInputOrigin,
        signals: &SignalRuntime,
        driver_demand: Option<&DriverDemandHandle>,
    ) -> Result<(Self, Option<ResolvedDocument>), ComponentAttemptFault> {
        Ok(Self::prepare_complete_root_candidate_with_capabilities(
            root,
            event_origin,
            signals,
            driver_demand,
            None,
            None,
        )?
        .commit())
    }

    pub(crate) fn prepare_complete_root_candidate_with_capabilities<'runtime>(
        root: Component,
        event_origin: EventInputOrigin,
        signals: &'runtime SignalRuntime,
        driver_demand: Option<&DriverDemandHandle>,
        tasks: Option<&MountTaskHandle>,
        application_exit: Option<&ApplicationExitControl>,
    ) -> Result<ComponentRenderCandidate<'runtime, Root>, ComponentAttemptFault> {
        let mut signal_render = signals
            .begin_render()
            .map_err(ComponentAttemptFault::signal)?;
        let (stage, system_candidate) = Self::mount(
            root,
            event_origin,
            &mut signal_render,
            driver_demand,
            tasks,
            application_exit,
        )?;
        Ok(ComponentRenderCandidate {
            stage,
            system_candidate,
            signal_render,
        })
    }

    fn mount(
        root: Component,
        event_origin: EventInputOrigin,
        signal_render: &mut SignalRenderTransaction<'_>,
        driver_demand: Option<&DriverDemandHandle>,
        tasks: Option<&MountTaskHandle>,
        application_exit: Option<&ApplicationExitControl>,
    ) -> Result<(Self, Option<ResolvedDocument>), ComponentAttemptFault> {
        let mut listeners = Vec::new();
        let mut reaction_completions = Vec::new();
        let mut preparations = PreparationSet::default();
        let mut native_tools = Vec::new();
        let mut task_starts = Vec::new();
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
            driver_demand,
            tasks,
            application_exit,
            signal_render,
            &mut listeners,
            &mut reaction_completions,
            &mut preparations,
            &mut native_tools,
            &mut task_starts,
            &mut capture,
        );
        rendered?;

        let streaming_routes = build_streaming_routes(&listeners)?;
        let streaming_contracts = std::mem::take(&mut capture.streaming_contracts);
        let system_candidate = capture.resolve_system_candidate()?;
        let projection = capture.into_projection(system_candidate.as_ref())?;
        let projection = RenderedProjection::with_native_tools(
            projection.nodes().to_vec(),
            native_tools
                .iter()
                .map(|tool| {
                    tool.definition()
                        .expect("tool definitions were validated during render")
                        .clone()
                })
                .collect(),
        )
        .map_err(|error| ComponentAttemptFault::RuntimeInvariant {
            message: error.to_string(),
        })?;
        Ok((
            Self {
                projection,
                preparations,
                bindings: RenderBindings {
                    listeners,
                    reaction_completions,
                    native_tools,
                    streaming_routes,
                    streaming_contracts,
                    finished: false,
                    faulted: false,
                    marker: std::marker::PhantomData,
                },
                task_starts,
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

    pub(crate) fn into_execution_parts(
        self,
    ) -> (PreparationSet, RenderBindings<Root>, Vec<MountTaskStart>) {
        (self.preparations, self.bindings, self.task_starts)
    }

    #[cfg(test)]
    pub(crate) fn into_bindings(self) -> RenderBindings<Root> {
        debug_assert!(self.task_starts.is_empty());
        self.bindings
    }
}

/// Generation-local event routes, streaming parsers, and async handlers.
///
/// Dropping this value abandons parser accumulators. Only
/// [`finish_normal`](Self::finish_normal) performs semantic stream completion
/// and dispatches diagnostics discovered at EOF.
pub(crate) struct RenderBindings<Root> {
    listeners: Vec<MountedListener>,
    reaction_completions: Vec<ReactionCompletionDeclaration>,
    native_tools: Vec<NativeToolCallDeclaration>,
    streaming_routes: Vec<MountedStreamingRoute>,
    streaming_contracts: Vec<ContractDeclaration>,
    finished: bool,
    faulted: bool,
    marker: std::marker::PhantomData<fn(Root)>,
}

impl<Root> RenderBindings<Root>
where
    Root: Send + Sync + 'static,
{
    pub(crate) fn take_streaming_contracts(&mut self) -> Vec<ContractDeclaration> {
        std::mem::take(&mut self.streaming_contracts)
    }

    /// Dispatch one immutable root event without rendering.
    pub(crate) async fn dispatch(&mut self, event: Root) -> Result<(), ComponentAttemptFault> {
        self.ensure_open()?;
        match self.dispatch_inner(event).await {
            Ok(()) => Ok(()),
            Err(fault) => self.abort(fault),
        }
    }

    #[cfg(feature = "legacy-provider-port")]
    pub(crate) fn start_native_tool(
        &mut self,
        call: crate::component::execution::ToolCall,
    ) -> Result<NativeToolFuture, ComponentAttemptFault> {
        self.start_native_tool_record(call)
            .map(|(future, _)| future)
    }

    pub(crate) fn start_native_tool_record(
        &mut self,
        call: crate::component::execution::ToolCall,
    ) -> Result<(NativeToolFuture, NativeToolRecord), ComponentAttemptFault> {
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
        let record = tool
            .append_call(&call)
            .map_err(ComponentAttemptFault::native_tool)?;
        let future = tool
            .start(call)
            .map_err(ComponentAttemptFault::native_tool)?;
        Ok((
            Box::pin(await_output(call_id, future, record.clone())),
            record,
        ))
    }

    async fn dispatch_inner(&mut self, event: Root) -> Result<(), ComponentAttemptFault> {
        let mut parsed_events = Vec::new();
        for route in &mut self.streaming_routes {
            parsed_events.extend(
                route
                    .dispatch_root(&event)
                    .map_err(ComponentAttemptFault::streaming_input)?,
            );
        }

        // Raw observers see the provider event before handlers for events derived from it.
        for listener in &mut self.listeners {
            listener.dispatch_root(&event).await?;
        }
        for parsed in parsed_events {
            dispatch_parsed_at(&mut self.listeners, parsed).await?;
        }
        Ok(())
    }

    /// Finish parsers and await diagnostics discovered at normal Provider EOF.
    pub(crate) async fn finish_normal(&mut self) -> Result<(), ComponentAttemptFault> {
        self.ensure_open()?;
        match self.finish_normal_inner().await {
            Ok(()) => {
                self.finished = true;
                Ok(())
            }
            Err(fault) => self.abort(fault),
        }
    }

    async fn finish_normal_inner(&mut self) -> Result<(), ComponentAttemptFault> {
        let mut parsed_events = Vec::new();
        for route in &mut self.streaming_routes {
            parsed_events.extend(
                route
                    .finish()
                    .map_err(ComponentAttemptFault::streaming_input)?,
            );
        }

        for parsed in parsed_events {
            dispatch_parsed_at(&mut self.listeners, parsed).await?;
        }
        for completion in &mut self.reaction_completions {
            completion
                .dispatch()
                .await
                .map_err(ComponentAttemptFault::reaction_completion)?;
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

async fn dispatch_parsed_at(
    listeners: &mut [MountedListener],
    event: ParsedContractEvent,
) -> Result<(), ComponentAttemptFault> {
    let listener_index = match &event {
        ParsedContractEvent::Decoded { listener_index, .. }
        | ParsedContractEvent::Invalid { listener_index, .. }
        | ParsedContractEvent::Lifecycle { listener_index, .. } => *listener_index,
    };
    let listener = listeners.get_mut(listener_index).ok_or_else(|| {
        ComponentAttemptFault::RuntimeInvariant {
            message: format!("streaming parser targeted missing listener {listener_index}"),
        }
    })?;
    dispatch_parsed(listener, event).await
}

async fn dispatch_parsed(
    listener: &mut MountedListener,
    event: ParsedContractEvent,
) -> Result<(), ComponentAttemptFault> {
    match event {
        ParsedContractEvent::Decoded { value, .. } => {
            listener.dispatch_streaming_decoded(Some(value), None).await
        }
        ParsedContractEvent::Invalid { diagnostic, .. } => {
            listener.dispatch_streaming_invalid(diagnostic).await
        }
        ParsedContractEvent::Lifecycle { phase, element, .. } => {
            listener.dispatch_streaming_phase(phase, element).await
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
    driver_demand: Option<&DriverDemandHandle>,
    tasks: Option<&MountTaskHandle>,
    application_exit: Option<&ApplicationExitControl>,
    signal_render: &mut SignalRenderTransaction<'_>,
    listeners: &mut Vec<MountedListener>,
    reaction_completions: &mut Vec<ReactionCompletionDeclaration>,
    preparations: &mut PreparationSet,
    native_tools: &mut Vec<NativeToolCallDeclaration>,
    task_starts: &mut Vec<MountTaskStart>,
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
                    driver_demand,
                    tasks,
                    application_exit,
                    signal_render,
                    listeners,
                    reaction_completions,
                    preparations,
                    native_tools,
                    task_starts,
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
                ScopeRender::Fresh(render) => invoke_fresh(render),
                ScopeRender::Repeatable(renderer) => invoke_repeatable(
                    signal_render,
                    component_id.clone(),
                    &renderer,
                    forced != Some(Placement::SystemOnce),
                    HookInvocationContext {
                        event_origin,
                        driver_demand,
                        tasks,
                        application_exit,
                        listeners,
                        reaction_completions,
                        preparations,
                        task_starts,
                    },
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
                driver_demand,
                tasks,
                application_exit,
                signal_render,
                listeners,
                reaction_completions,
                preparations,
                native_tools,
                task_starts,
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
                    invoke_fresh(render)
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
                driver_demand,
                tasks,
                application_exit,
                signal_render,
                listeners,
                reaction_completions,
                preparations,
                native_tools,
                task_starts,
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
                    forced.unwrap_or(Placement::User),
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
                    driver_demand,
                    tasks,
                    application_exit,
                    signal_render,
                    listeners,
                    reaction_completions,
                    preparations,
                    native_tools,
                    task_starts,
                    capture,
                )?;
            }
        }
        #[cfg(any(feature = "legacy-provider-port", test))]
        ComponentNode::EventListener(declaration) => {
            if forced == Some(Placement::SystemOnce) {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "EventListener/EventInput",
                });
            }
            listeners.push(MountedListener::new_event(declaration, event_origin)?);
        }
        ComponentNode::StreamingXmlTag(declaration) => {
            if forced == Some(Placement::SystemOnce) {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "StreamingXml/EventInput",
                });
            }
            listeners.push(MountedListener::new_streaming_tag(
                *declaration,
                event_origin,
            )?);
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
        ComponentNode::StreamingAttempt(declaration) => {
            let placement = forced.unwrap_or(Placement::User);
            if placement == Placement::SystemOnce {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "XmlStreamingToolCall/attempt",
                });
            }
            let document = declaration.validate_and_prompt()?;
            capture.push(owner, placement, document.into_children(), None)?;
            capture.streaming_contracts.push(*declaration);
        }
        ComponentNode::NativeToolCall(mut declaration) => {
            if forced == Some(Placement::SystemOnce) {
                return Err(ComponentAttemptFault::SystemAttemptLocal {
                    component: owner.to_string(),
                    capability: "NativeToolCall",
                });
            }
            declaration
                .definition()
                .map_err(|error| ComponentAttemptFault::RuntimeInvariant {
                    message: error.to_string(),
                })?;
            let position = *scope_cursor;
            *scope_cursor = scope_cursor.saturating_add(1);
            let component_id = owner.child(
                &format!("agentview::NativeToolCall({})", declaration.name()),
                position,
            );
            let history = signal_render
                .render_component(component_id.clone(), |scope| {
                    scope.use_signal_at(0, NativeToolHistory::default)
                })
                .map_err(ComponentAttemptFault::signal)?;
            let items = history
                .with(|history| history.items().cloned().collect())
                .map_err(|error| ComponentAttemptFault::RuntimeInvariant {
                    message: error.to_string(),
                })?;
            capture.register_component(component_id.clone())?;
            let index = capture.node_indexes[&component_id];
            capture.nodes[index].native_items = items;
            declaration.mount(history);
            native_tools.push(*declaration);
        }
    }
    Ok(())
}

fn invoke_fresh(renderer: Box<dyn FnOnce() -> Component + Send + 'static>) -> Component {
    renderer()
}

struct HookInvocationContext<'render> {
    event_origin: EventInputOrigin,
    driver_demand: Option<&'render DriverDemandHandle>,
    tasks: Option<&'render MountTaskHandle>,
    application_exit: Option<&'render ApplicationExitControl>,
    listeners: &'render mut Vec<MountedListener>,
    reaction_completions: &'render mut Vec<ReactionCompletionDeclaration>,
    preparations: &'render mut PreparationSet,
    task_starts: &'render mut Vec<MountTaskStart>,
}

fn invoke_repeatable(
    signals: &mut SignalRenderTransaction<'_>,
    component: ComponentId,
    renderer: &RepeatableRender,
    attempt_local_allowed: bool,
    context: HookInvocationContext<'_>,
) -> Result<Component, ComponentAttemptFault> {
    let HookInvocationContext {
        event_origin,
        driver_demand,
        tasks,
        application_exit,
        listeners,
        reaction_completions,
        preparations,
        task_starts,
    } = context;
    let mut provider_handlers = Vec::new();
    let mut local_reaction_completions = Vec::new();
    let rendered = signals
        .render_component(component, |signal_scope| {
            let mut hooks = if attempt_local_allowed {
                HookRenderContext::new(
                    signal_scope,
                    event_origin,
                    &mut provider_handlers,
                    &mut local_reaction_completions,
                    preparations,
                    task_starts,
                    driver_demand,
                    tasks,
                    application_exit,
                )
            } else {
                HookRenderContext::for_system(
                    signal_scope,
                    event_origin,
                    &mut provider_handlers,
                    &mut local_reaction_completions,
                    preparations,
                    task_starts,
                    driver_demand,
                    tasks,
                    application_exit,
                )
            };
            Ok(renderer(&mut hooks))
        })
        .map_err(ComponentAttemptFault::signal)?;
    for declaration in provider_handlers {
        listeners.push(MountedListener::new_event(declaration, event_origin)?);
    }
    reaction_completions.extend(local_reaction_completions);
    Ok(rendered)
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
    streaming_contracts: Vec<ContractDeclaration>,
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
            streaming_contracts: Vec::new(),
            system: BlockChildren::new(),
            nodes: vec![ProjectionNodeCapture {
                identity: root,
                runs: Vec::new(),
                native_items: Vec::new(),
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
            native_items: Vec::new(),
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
                let mut built = build_projection_items(node_system, node.runs)?;
                built.items.extend(node.native_items);
                Ok(RenderedProjectionNode::with_diff_templates(
                    node.identity.to_string(),
                    built.items,
                    built.diffs,
                    built.diff_templates,
                )
                .with_repeat_items(built.repeat_items))
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
        if forced == Some(Placement::SystemOnce) {
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
    native_items: Vec<crate::transcript::CanonicalInputItem>,
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
    #[error("invalid streaming element declaration in contract `{contract}`")]
    InvalidStreamingToolElementName {
        contract: &'static str,
        element: &'static str,
        detail: String,
    },
    #[error("invalid streaming attribute declaration in contract `{contract}`")]
    InvalidStreamingToolAttributeName {
        contract: &'static str,
        element: &'static str,
        attribute: &'static str,
        detail: String,
    },
    #[error("duplicate streaming element `{element}` in contract `{contract}`")]
    DuplicateStreamingToolElement {
        contract: &'static str,
        element: &'static str,
    },
    #[error("duplicate streaming attribute `{attribute}` in contract `{contract}`")]
    DuplicateStreamingToolAttribute {
        contract: &'static str,
        element: &'static str,
        attribute: &'static str,
    },
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
    #[error("component `{component}` requires unavailable runtime capability `{capability}`")]
    HookCapabilityUnavailable {
        component: String,
        capability: &'static str,
    },
    #[error("signal runtime fault: {message}")]
    Signal { message: String },
    #[error("event listener dispatch fault: {message}")]
    ListenerDispatch { message: String },
    #[error("reaction completion dispatch fault: {message}")]
    ReactionCompletion { message: String },
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

    fn reaction_completion(fault: ReactionCompletionDispatchFault) -> Self {
        Self::ReactionCompletion {
            message: fault.to_string(),
        }
    }

    fn streaming_mount(fault: StreamingXmlMountFault) -> Self {
        match fault {
            StreamingXmlMountFault::ForeignEventInput { identity } => {
                Self::ForeignEventInput { identity }
            }
            fault @ StreamingXmlMountFault::InvalidTag { .. } => Self::StreamingMount {
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
