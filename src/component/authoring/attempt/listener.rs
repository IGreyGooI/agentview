use std::collections::HashMap;

use crate::component::authoring::{
    event_input::{EventInputOrigin, EventRouteTopology},
    event_listener::EventListenerDeclaration,
    streaming_xml::{
        MountedStreamingRoute, MountedXmlStreamingToolCall, XmlContractDiagnostic,
        XmlStreamingToolCallDeclaration,
    },
};

use super::ComponentAttemptFault;

pub(super) struct MountedListener {
    declaration: MountedListenerDeclaration,
}

impl MountedListener {
    pub(super) fn new_event(
        declaration: EventListenerDeclaration,
        event_origin: EventInputOrigin,
    ) -> Result<Self, ComponentAttemptFault> {
        validate_listener_token("identity", declaration.identity())?;
        validate_listener_token("version", declaration.implementation_version())?;
        if declaration.route().origin() != event_origin {
            return Err(ComponentAttemptFault::ForeignEventInput {
                identity: declaration.identity(),
            });
        }
        Ok(Self {
            declaration: MountedListenerDeclaration::Event(declaration),
        })
    }

    pub(super) fn new_streaming(
        declaration: XmlStreamingToolCallDeclaration,
        event_origin: EventInputOrigin,
    ) -> Result<Self, ComponentAttemptFault> {
        validate_listener_token("identity", declaration.identity())?;
        validate_listener_token("version", declaration.implementation_version())?;
        let mounted = MountedXmlStreamingToolCall::new(declaration, event_origin)
            .map_err(ComponentAttemptFault::streaming_mount)?;
        Ok(Self {
            declaration: MountedListenerDeclaration::Streaming(Box::new(mounted)),
        })
    }

    pub(super) async fn dispatch_root<Root>(
        &mut self,
        root: &Root,
    ) -> Result<bool, ComponentAttemptFault>
    where
        Root: Send + Sync + 'static,
    {
        match &mut self.declaration {
            MountedListenerDeclaration::Event(declaration) => declaration
                .dispatch_root(root)
                .await
                .map_err(ComponentAttemptFault::listener_dispatch),
            MountedListenerDeclaration::Streaming(_) => Ok(false),
        }
    }

    pub(super) async fn dispatch_streaming(
        &mut self,
        decoded: Option<String>,
        invalid: Option<XmlContractDiagnostic>,
    ) -> Result<(), ComponentAttemptFault> {
        let MountedListenerDeclaration::Streaming(streaming) = &mut self.declaration else {
            return Err(ComponentAttemptFault::RuntimeInvariant {
                message: String::from("streaming parser targeted a non-streaming listener"),
            });
        };
        match (decoded, invalid) {
            (Some(value), None) => streaming
                .dispatch_decoded(&value)
                .await
                .map_err(ComponentAttemptFault::streaming_input),
            (None, Some(diagnostic)) => streaming
                .dispatch_invalid(diagnostic)
                .await
                .map_err(ComponentAttemptFault::streaming_input),
            _ => Err(ComponentAttemptFault::RuntimeInvariant {
                message: String::from("streaming parser produced an invalid event shape"),
            }),
        }
    }

    pub(super) async fn finish(&mut self) -> Result<(), ComponentAttemptFault> {
        match &mut self.declaration {
            MountedListenerDeclaration::Event(_) => Ok(()),
            MountedListenerDeclaration::Streaming(streaming) => streaming
                .finish()
                .await
                .map_err(ComponentAttemptFault::streaming_input),
        }
    }

    fn route_topology(&self) -> EventRouteTopology {
        match &self.declaration {
            MountedListenerDeclaration::Event(declaration) => declaration.topology(),
            MountedListenerDeclaration::Streaming(streaming) => {
                streaming.route_descriptor().topology()
            }
        }
    }
}

enum MountedListenerDeclaration {
    Event(EventListenerDeclaration),
    Streaming(Box<MountedXmlStreamingToolCall>),
}

pub(super) fn build_streaming_routes(
    listeners: &[MountedListener],
) -> Result<Vec<MountedStreamingRoute>, ComponentAttemptFault> {
    let mut listener_topologies = Vec::<EventRouteTopology>::with_capacity(listeners.len());
    for listener in listeners {
        let topology = listener.route_topology();
        if let Some(existing) = listener_topologies
            .iter()
            .find(|existing| existing.has_projector_collision(&topology))
        {
            let route = topology
                .selectors
                .last()
                .or_else(|| existing.selectors.last())
                .map_or("root", |segment| segment.selector_identity);
            return Err(ComponentAttemptFault::EventSelectorCollision { route });
        }
        listener_topologies.push(topology);
    }

    let mut routes = Vec::<MountedStreamingRoute>::new();
    let mut by_topology = HashMap::<EventRouteTopology, usize>::new();
    for (listener_index, listener) in listeners.iter().enumerate() {
        let MountedListenerDeclaration::Streaming(streaming) = &listener.declaration else {
            continue;
        };
        let descriptor = streaming.route_descriptor();
        let topology = descriptor.topology();
        let route_index = if let Some(route_index) = by_topology.get(&topology).copied() {
            route_index
        } else {
            let route_index = routes.len();
            routes.push(MountedStreamingRoute::new(descriptor));
            by_topology.insert(topology, route_index);
            route_index
        };
        routes[route_index]
            .register(streaming.registration(listener_index))
            .map_err(ComponentAttemptFault::streaming_mount)?;
    }
    Ok(routes)
}

pub(super) fn validate_listener_token(
    kind: &'static str,
    value: &'static str,
) -> Result<(), ComponentAttemptFault> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Ok(());
    }
    Err(ComponentAttemptFault::InvalidListenerToken { kind, value })
}
