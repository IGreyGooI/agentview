use std::{any::type_name, fmt, future::Future, marker::PhantomData, str::FromStr, sync::Arc};

use async_trait::async_trait;

use crate::{
    component::execution::ProviderEvent,
    llm_call::TextTurnEvent,
    pom::{Document, PomError, XmlName, XmlNode},
    stream_parser::XmlElement,
};

use super::{
    declaration::{Component, ComponentNode},
    event_input::{EventInputOrigin, EventRouteDescriptor},
    handler::AsyncHandler,
    InternalEventInput as EventInput,
};

mod fault;
mod parser;

pub(crate) use fault::{StreamingXmlDispatchFault, StreamingXmlMountFault};
pub(crate) use parser::{
    ContractRegistration, MountedStreamingRoute, ParsedContractEvent, StreamingRegistrationKind,
    StreamingXmlPhase,
};

/// Strict typed diagnostic emitted by the target streaming XML contract.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum XmlContractDiagnostic {
    MalformedElement {
        contract: &'static str,
        detail: String,
    },
    InvalidAttributeValue {
        contract: &'static str,
        attribute: &'static str,
        value: String,
        expected: &'static str,
        detail: String,
    },
    IncompleteElement {
        contract: &'static str,
        element: &'static str,
    },
}

struct ContractSpec {
    identity: &'static str,
    implementation_version: &'static str,
    element: &'static str,
    attribute: Option<&'static str>,
}

enum ContractRoute {
    ProviderText,
    Explicit(Arc<EventRouteDescriptor>),
}

/// Prompt-free entry point for subscribing to lifecycle events from the mounted Provider XML
/// stream.
///
/// Every declaration on the same event route is aggregated into one mounted parser. A
/// `StreamingXml::tag(...)` declaration contributes only one specific-tag subscription; unlike
/// [`XmlStreamingToolCall`], it does not add instructions or example syntax to the prompt.
pub struct StreamingXml;

impl StreamingXml {
    pub fn tag(tag: &'static str) -> StreamingXmlTag {
        StreamingXmlTag {
            tag,
            route: ContractRoute::ProviderText,
            open: None,
            stream: None,
            complete: None,
            invalid: None,
        }
    }
}

/// One prompt-free, specific-tag subscription to the shared streaming XML parser.
pub struct StreamingXmlTag {
    tag: &'static str,
    route: ContractRoute,
    open: Option<AsyncHandler<XmlElement>>,
    stream: Option<AsyncHandler<XmlElement>>,
    complete: Option<AsyncHandler<XmlElement>>,
    invalid: Option<AsyncHandler<XmlContractDiagnostic>>,
}

impl StreamingXmlTag {
    pub fn on_open<Handler, HandlerFuture, Error>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(XmlElement) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        assert!(
            self.open.is_none(),
            "StreamingXml tag declared on_open twice"
        );
        self.open = Some(AsyncHandler::new(handler));
        self
    }

    /// Receive cumulative content snapshots while the selected element remains open.
    pub fn on_stream<Handler, HandlerFuture, Error>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(XmlElement) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        assert!(
            self.stream.is_none(),
            "StreamingXml tag declared on_stream twice"
        );
        self.stream = Some(AsyncHandler::new(handler));
        self
    }

    pub fn on_complete<Handler, HandlerFuture, Error>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(XmlElement) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        assert!(
            self.complete.is_none(),
            "StreamingXml tag declared on_complete twice"
        );
        self.complete = Some(AsyncHandler::new(handler));
        self
    }

    pub fn on_invalid<Handler, HandlerFuture, Error>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(XmlContractDiagnostic) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        assert!(
            self.invalid.is_none(),
            "StreamingXml tag declared on_invalid twice"
        );
        self.invalid = Some(AsyncHandler::new(handler));
        self
    }

    pub fn into_component(self) -> Component {
        assert!(
            self.open.is_some()
                || self.stream.is_some()
                || self.complete.is_some()
                || self.invalid.is_some(),
            "StreamingXml tag requires at least one lifecycle handler"
        );
        Component::from_node(ComponentNode::StreamingXmlTag(Box::new(
            StreamingXmlTagDeclaration {
                tag: self.tag,
                route: self.route,
                open: self.open,
                stream: self.stream,
                complete: self.complete,
                invalid: self.invalid,
            },
        )))
    }
}

impl From<StreamingXmlTag> for Component {
    fn from(subscription: StreamingXmlTag) -> Self {
        subscription.into_component()
    }
}

/// Entry point for a prompt-producing, typed streaming XML tool-call declaration.
///
/// The declared element is projected as model-visible example syntax and matching empty elements
/// are decoded before the handler runs. It shares the mounted parser for its event route with
/// [`StreamingXml`] lifecycle subscriptions and other XML tool-call declarations.
pub struct XmlStreamingToolCall;

impl XmlStreamingToolCall {
    pub fn contract(
        identity: &'static str,
        implementation_version: &'static str,
    ) -> XmlStreamingToolCallContract {
        XmlStreamingToolCallContract {
            identity,
            implementation_version,
        }
    }
}

pub struct XmlStreamingToolCallContract {
    identity: &'static str,
    implementation_version: &'static str,
}

impl XmlStreamingToolCallContract {
    pub fn empty_element(self, element: &'static str) -> XmlStreamingToolCallElement {
        XmlStreamingToolCallElement {
            identity: self.identity,
            implementation_version: self.implementation_version,
            element,
        }
    }
}

pub struct XmlStreamingToolCallElement {
    identity: &'static str,
    implementation_version: &'static str,
    element: &'static str,
}

impl XmlStreamingToolCallElement {
    pub fn required_attribute<Decoded>(
        self,
        attribute: &'static str,
    ) -> XmlStreamingToolCallAttribute<Decoded>
    where
        Decoded: FromStr + Send + Sync + 'static,
        Decoded::Err: fmt::Display,
    {
        XmlStreamingToolCallAttribute {
            spec: ContractSpec {
                identity: self.identity,
                implementation_version: self.implementation_version,
                element: self.element,
                attribute: Some(attribute),
            },
            marker: PhantomData,
        }
    }

    /// Dispatch every matching attribute-free empty element from the current Provider stream.
    pub fn on_decoded<Handler, HandlerFuture, Error>(
        self,
        mut handler: Handler,
    ) -> XmlStreamingToolCallDecoded
    where
        Handler: FnMut() -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        XmlStreamingToolCallDecoded {
            spec: ContractSpec {
                identity: self.identity,
                implementation_version: self.implementation_version,
                element: self.element,
                attribute: None,
            },
            route: ContractRoute::ProviderText,
            decoded: Box::new(UnitDecodedHandler {
                handler: AsyncHandler::new(move |()| handler()),
            }),
        }
    }

    pub fn listen_to(self, input: EventInput<TextTurnEvent>) -> XmlStreamingToolCallEmptyRouted {
        XmlStreamingToolCallEmptyRouted {
            spec: ContractSpec {
                identity: self.identity,
                implementation_version: self.implementation_version,
                element: self.element,
                attribute: None,
            },
            route: ContractRoute::Explicit(input.route_descriptor()),
        }
    }
}

pub struct XmlStreamingToolCallAttribute<Decoded> {
    spec: ContractSpec,
    marker: PhantomData<fn() -> Decoded>,
}

impl<Decoded> XmlStreamingToolCallAttribute<Decoded>
where
    Decoded: FromStr + Send + Sync + 'static,
    Decoded::Err: fmt::Display,
{
    /// Dispatch every matching empty element from the current Provider stream.
    pub fn on_decoded<Handler, HandlerFuture, Error>(
        self,
        handler: Handler,
    ) -> XmlStreamingToolCallDecoded
    where
        Handler: FnMut(Decoded) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
        Decoded::Err: Send + 'static,
    {
        XmlStreamingToolCallDecoded {
            spec: self.spec,
            route: ContractRoute::ProviderText,
            decoded: Box::new(TypedDecodedHandler {
                handler: AsyncHandler::new(handler),
            }),
        }
    }

    pub fn listen_to(
        self,
        input: EventInput<TextTurnEvent>,
    ) -> XmlStreamingToolCallRouted<Decoded> {
        XmlStreamingToolCallRouted {
            spec: self.spec,
            route: ContractRoute::Explicit(input.route_descriptor()),
            marker: PhantomData,
        }
    }
}

pub struct XmlStreamingToolCallEmptyRouted {
    spec: ContractSpec,
    route: ContractRoute,
}

impl XmlStreamingToolCallEmptyRouted {
    pub fn on_decoded<Handler, HandlerFuture, Error>(
        self,
        mut handler: Handler,
    ) -> XmlStreamingToolCallDecoded
    where
        Handler: FnMut() -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        XmlStreamingToolCallDecoded {
            spec: self.spec,
            route: self.route,
            decoded: Box::new(UnitDecodedHandler {
                handler: AsyncHandler::new(move |()| handler()),
            }),
        }
    }
}

pub struct XmlStreamingToolCallRouted<Decoded> {
    spec: ContractSpec,
    route: ContractRoute,
    marker: PhantomData<fn() -> Decoded>,
}

impl<Decoded> XmlStreamingToolCallRouted<Decoded>
where
    Decoded: FromStr + Send + Sync + 'static,
    Decoded::Err: fmt::Display + Send + 'static,
{
    pub fn on_decoded<Handler, HandlerFuture, Error>(
        self,
        handler: Handler,
    ) -> XmlStreamingToolCallDecoded
    where
        Handler: FnMut(Decoded) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        XmlStreamingToolCallDecoded {
            spec: self.spec,
            route: self.route,
            decoded: Box::new(TypedDecodedHandler {
                handler: AsyncHandler::new(handler),
            }),
        }
    }
}

pub struct XmlStreamingToolCallDecoded {
    spec: ContractSpec,
    route: ContractRoute,
    decoded: Box<dyn ErasedDecodedHandler>,
}

impl XmlStreamingToolCallDecoded {
    pub fn on_invalid<Handler, HandlerFuture, Error>(self, handler: Handler) -> Component
    where
        Handler: FnMut(XmlContractDiagnostic) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Component::from_node(ComponentNode::XmlStreamingToolCall(Box::new(
            XmlStreamingToolCallDeclaration {
                spec: self.spec,
                route: self.route,
                decoded: self.decoded,
                invalid: AsyncHandler::new(handler),
            },
        )))
    }
}

#[async_trait]
trait ErasedDecodedHandler: Send {
    async fn decode_and_invoke(
        &mut self,
        spec: &ContractSpec,
        raw: &str,
    ) -> Result<Option<XmlContractDiagnostic>, StreamingXmlDispatchFault>;
}

struct TypedDecodedHandler<Decoded> {
    handler: AsyncHandler<Decoded>,
}

struct UnitDecodedHandler {
    handler: AsyncHandler<()>,
}

#[async_trait]
impl<Decoded> ErasedDecodedHandler for TypedDecodedHandler<Decoded>
where
    Decoded: FromStr + Send + Sync + 'static,
    Decoded::Err: fmt::Display + Send + 'static,
{
    async fn decode_and_invoke(
        &mut self,
        spec: &ContractSpec,
        raw: &str,
    ) -> Result<Option<XmlContractDiagnostic>, StreamingXmlDispatchFault> {
        let decoded = match raw.parse::<Decoded>() {
            Ok(decoded) => decoded,
            Err(fault) => {
                return Ok(Some(XmlContractDiagnostic::InvalidAttributeValue {
                    contract: spec.identity,
                    attribute: spec
                        .attribute
                        .expect("typed XML decoding requires one attribute"),
                    value: raw.to_owned(),
                    expected: type_name::<Decoded>(),
                    detail: fault.to_string(),
                }));
            }
        };
        self.handler.invoke(decoded).await.map_err(|source| {
            StreamingXmlDispatchFault::Handler {
                contract: spec.identity,
                phase: "decoded",
                source,
            }
        })?;
        Ok(None)
    }
}

#[async_trait]
impl ErasedDecodedHandler for UnitDecodedHandler {
    async fn decode_and_invoke(
        &mut self,
        spec: &ContractSpec,
        raw: &str,
    ) -> Result<Option<XmlContractDiagnostic>, StreamingXmlDispatchFault> {
        debug_assert!(spec.attribute.is_none());
        debug_assert!(raw.is_empty());
        self.handler
            .invoke(())
            .await
            .map_err(|source| StreamingXmlDispatchFault::Handler {
                contract: spec.identity,
                phase: "decoded",
                source,
            })?;
        Ok(None)
    }
}

pub(crate) struct StreamingXmlTagDeclaration {
    tag: &'static str,
    route: ContractRoute,
    open: Option<AsyncHandler<XmlElement>>,
    stream: Option<AsyncHandler<XmlElement>>,
    complete: Option<AsyncHandler<XmlElement>>,
    invalid: Option<AsyncHandler<XmlContractDiagnostic>>,
}

impl StreamingXmlTagDeclaration {
    pub(crate) fn identity(&self) -> &'static str {
        self.tag
    }
}

pub(crate) struct MountedStreamingXmlTag {
    declaration: StreamingXmlTagDeclaration,
    route: Arc<EventRouteDescriptor>,
}

impl MountedStreamingXmlTag {
    pub(crate) fn new(
        declaration: StreamingXmlTagDeclaration,
        event_origin: EventInputOrigin,
    ) -> Result<Self, StreamingXmlMountFault> {
        XmlName::new(declaration.tag).map_err(|fault| StreamingXmlMountFault::InvalidTag {
            tag: declaration.tag,
            detail: fault.to_string(),
        })?;
        let route = resolve_route(declaration.identity(), &declaration.route, event_origin)?;
        Ok(Self { declaration, route })
    }

    pub(crate) fn route_descriptor(&self) -> Arc<EventRouteDescriptor> {
        Arc::clone(&self.route)
    }

    pub(crate) fn registration(&self, listener_index: usize) -> ContractRegistration {
        ContractRegistration {
            listener_index,
            identity: self.declaration.identity(),
            element: self.declaration.tag,
            kind: StreamingRegistrationKind::Lifecycle {
                open: self.declaration.open.is_some(),
                stream: self.declaration.stream.is_some(),
                complete: self.declaration.complete.is_some(),
                invalid: self.declaration.invalid.is_some(),
            },
        }
    }

    pub(crate) async fn dispatch_phase(
        &mut self,
        phase: StreamingXmlPhase,
        element: XmlElement,
    ) -> Result<(), StreamingXmlDispatchFault> {
        let handler = match phase {
            StreamingXmlPhase::Open => self.declaration.open.as_mut(),
            StreamingXmlPhase::Stream => self.declaration.stream.as_mut(),
            StreamingXmlPhase::Complete => self.declaration.complete.as_mut(),
        };
        let Some(handler) = handler else {
            return Ok(());
        };
        handler
            .invoke(element)
            .await
            .map_err(|source| StreamingXmlDispatchFault::Handler {
                contract: self.declaration.identity(),
                phase: phase.as_str(),
                source,
            })
    }

    pub(crate) async fn dispatch_invalid(
        &mut self,
        diagnostic: XmlContractDiagnostic,
    ) -> Result<(), StreamingXmlDispatchFault> {
        let Some(handler) = self.declaration.invalid.as_mut() else {
            return Ok(());
        };
        handler
            .invoke(diagnostic)
            .await
            .map_err(|source| StreamingXmlDispatchFault::Handler {
                contract: self.declaration.identity(),
                phase: "invalid",
                source,
            })
    }
}

pub(crate) struct XmlStreamingToolCallDeclaration {
    spec: ContractSpec,
    route: ContractRoute,
    decoded: Box<dyn ErasedDecodedHandler>,
    invalid: AsyncHandler<XmlContractDiagnostic>,
}

impl XmlStreamingToolCallDeclaration {
    pub(crate) fn identity(&self) -> &'static str {
        self.spec.identity
    }

    pub(crate) fn implementation_version(&self) -> &'static str {
        self.spec.implementation_version
    }

    pub(crate) fn prompt_document(&self) -> Result<Document, PomError> {
        let mut node = XmlNode::new(XmlName::new(self.spec.element)?);
        if let Some(attribute) = self.spec.attribute {
            node.push_attribute(XmlName::new(attribute)?, "...")?;
        }
        Ok(Document::from_xml(node))
    }
}

pub(crate) struct MountedXmlStreamingToolCall {
    declaration: XmlStreamingToolCallDeclaration,
    route: Arc<EventRouteDescriptor>,
}

impl MountedXmlStreamingToolCall {
    pub(crate) fn new(
        declaration: XmlStreamingToolCallDeclaration,
        event_origin: EventInputOrigin,
    ) -> Result<Self, StreamingXmlMountFault> {
        let route = match &declaration.route {
            ContractRoute::ProviderText => EventInput::<ProviderEvent>::from_origin(event_origin)
                .select(ProviderEvent::TEXT)
                .route_descriptor(),
            ContractRoute::Explicit(route) => {
                if route.origin() != event_origin {
                    return Err(StreamingXmlMountFault::ForeignEventInput {
                        identity: declaration.identity(),
                    });
                }
                Arc::clone(route)
            }
        };
        Ok(Self { declaration, route })
    }

    pub(crate) fn route_descriptor(&self) -> Arc<EventRouteDescriptor> {
        Arc::clone(&self.route)
    }

    pub(crate) fn registration(&self, listener_index: usize) -> ContractRegistration {
        ContractRegistration {
            listener_index,
            identity: self.declaration.spec.identity,
            element: self.declaration.spec.element,
            kind: StreamingRegistrationKind::EmptyToolCall {
                attribute: self.declaration.spec.attribute,
            },
        }
    }

    pub(crate) async fn dispatch_decoded(
        &mut self,
        raw: &str,
    ) -> Result<(), StreamingXmlDispatchFault> {
        let diagnostic = self
            .declaration
            .decoded
            .decode_and_invoke(&self.declaration.spec, raw)
            .await?;
        if let Some(diagnostic) = diagnostic {
            self.dispatch_invalid(diagnostic).await?;
        }
        Ok(())
    }

    pub(crate) async fn dispatch_invalid(
        &mut self,
        diagnostic: XmlContractDiagnostic,
    ) -> Result<(), StreamingXmlDispatchFault> {
        self.declaration
            .invalid
            .invoke(diagnostic)
            .await
            .map_err(|source| StreamingXmlDispatchFault::Handler {
                contract: self.declaration.identity(),
                phase: "invalid",
                source,
            })
    }
}

fn resolve_route(
    identity: &'static str,
    route: &ContractRoute,
    event_origin: EventInputOrigin,
) -> Result<Arc<EventRouteDescriptor>, StreamingXmlMountFault> {
    match route {
        ContractRoute::ProviderText => Ok(EventInput::<ProviderEvent>::from_origin(event_origin)
            .select(ProviderEvent::TEXT)
            .route_descriptor()),
        ContractRoute::Explicit(route) => {
            if route.origin() != event_origin {
                return Err(StreamingXmlMountFault::ForeignEventInput { identity });
            }
            Ok(Arc::clone(route))
        }
    }
}
