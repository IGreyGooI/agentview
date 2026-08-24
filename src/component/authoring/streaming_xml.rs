use std::{
    any::type_name,
    fmt,
    future::Future,
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    str::FromStr,
    sync::Arc,
};

use async_trait::async_trait;

use crate::{
    llm_call::TextTurnEvent,
    pom::{Document, PomError, XmlName, XmlNode},
};

use super::{
    declaration::{Component, ComponentNode},
    event_input::{EventInputOrigin, EventRouteDescriptor},
    handler::{panic_message, AsyncHandler, AsyncOnceHandler},
    EventInput,
};

mod fault;
mod parser;

pub(crate) use fault::{StreamingXmlDispatchFault, StreamingXmlMountFault};
pub(crate) use parser::{ContractRegistration, MountedStreamingRoute, ParsedContractEvent};

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
    OccurrenceCount {
        contract: &'static str,
        expected: usize,
        observed: usize,
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
    attribute: &'static str,
}

/// Entry point for the strict streaming XML text contract.
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
                attribute,
            },
            marker: PhantomData,
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
    pub fn exactly_one(self) -> XmlStreamingToolCallExactlyOne<Decoded> {
        XmlStreamingToolCallExactlyOne {
            spec: self.spec,
            marker: PhantomData,
        }
    }
}

pub struct XmlStreamingToolCallExactlyOne<Decoded> {
    spec: ContractSpec,
    marker: PhantomData<fn() -> Decoded>,
}

impl<Decoded> XmlStreamingToolCallExactlyOne<Decoded>
where
    Decoded: FromStr + Send + Sync + 'static,
    Decoded::Err: fmt::Display,
{
    pub fn listen_to(
        self,
        input: EventInput<TextTurnEvent>,
    ) -> XmlStreamingToolCallRouted<Decoded> {
        XmlStreamingToolCallRouted {
            spec: self.spec,
            route: input.route_descriptor(),
            marker: PhantomData,
        }
    }
}

pub struct XmlStreamingToolCallRouted<Decoded> {
    spec: ContractSpec,
    route: Arc<EventRouteDescriptor>,
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
    route: Arc<EventRouteDescriptor>,
    decoded: Box<dyn ErasedDecodedHandler>,
}

impl XmlStreamingToolCallDecoded {
    pub fn on_invalid<Handler, HandlerFuture, Error>(
        self,
        handler: Handler,
    ) -> XmlStreamingToolCallInvalid
    where
        Handler: FnMut(XmlContractDiagnostic) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        XmlStreamingToolCallInvalid {
            spec: self.spec,
            route: self.route,
            decoded: self.decoded,
            invalid: AsyncHandler::new(handler),
        }
    }
}

pub struct XmlStreamingToolCallInvalid {
    spec: ContractSpec,
    route: Arc<EventRouteDescriptor>,
    decoded: Box<dyn ErasedDecodedHandler>,
    invalid: AsyncHandler<XmlContractDiagnostic>,
}

impl XmlStreamingToolCallInvalid {
    /// Complete the declaration with a one-shot handler run only for normal EOF.
    pub fn on_finish<Handler, HandlerFuture, Error>(self, handler: Handler) -> Component
    where
        Handler: FnOnce() -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        Component::from_node(ComponentNode::XmlStreamingToolCall(Box::new(
            XmlStreamingToolCallDeclaration {
                spec: self.spec,
                route: self.route,
                decoded: self.decoded,
                invalid: self.invalid,
                terminal: Some(AsyncOnceHandler::new(handler)),
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
        let decoded =
            catch_unwind(AssertUnwindSafe(|| raw.parse::<Decoded>())).map_err(|panic| {
                StreamingXmlDispatchFault::ParserPanicked {
                    contract: spec.identity,
                    message: panic_message(&*panic),
                }
            })?;
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(fault) => {
                return Ok(Some(XmlContractDiagnostic::InvalidAttributeValue {
                    contract: spec.identity,
                    attribute: spec.attribute,
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

pub(crate) struct XmlStreamingToolCallDeclaration {
    spec: ContractSpec,
    route: Arc<EventRouteDescriptor>,
    decoded: Box<dyn ErasedDecodedHandler>,
    invalid: AsyncHandler<XmlContractDiagnostic>,
    terminal: Option<AsyncOnceHandler>,
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
        node.push_attribute(XmlName::new(self.spec.attribute)?, "...")?;
        Ok(Document::from_xml(node))
    }
}

pub(crate) struct MountedXmlStreamingToolCall {
    declaration: XmlStreamingToolCallDeclaration,
}

impl MountedXmlStreamingToolCall {
    pub(crate) fn new(
        declaration: XmlStreamingToolCallDeclaration,
        event_origin: EventInputOrigin,
    ) -> Result<Self, StreamingXmlMountFault> {
        if declaration.route.origin() != event_origin {
            return Err(StreamingXmlMountFault::ForeignEventInput {
                identity: declaration.identity(),
            });
        }
        Ok(Self { declaration })
    }

    pub(crate) fn route_descriptor(&self) -> Arc<EventRouteDescriptor> {
        Arc::clone(&self.declaration.route)
    }

    pub(crate) fn registration(&self, listener_index: usize) -> ContractRegistration {
        ContractRegistration {
            listener_index,
            identity: self.declaration.spec.identity,
            element: self.declaration.spec.element,
            attribute: self.declaration.spec.attribute,
        }
    }

    pub(crate) async fn finish(&mut self) -> Result<(), StreamingXmlDispatchFault> {
        let terminal = self
            .declaration
            .terminal
            .take()
            .ok_or(StreamingXmlDispatchFault::RouteAlreadyFinished)?;
        terminal
            .invoke()
            .await
            .map_err(|source| StreamingXmlDispatchFault::Handler {
                contract: self.declaration.identity(),
                phase: "finish",
                source,
            })
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
