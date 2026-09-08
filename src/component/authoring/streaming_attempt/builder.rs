//! Typestate builder for strict streaming XML tool contracts.

use std::{error::Error, future::Future, marker::PhantomData, sync::Arc};

use crate::component::authoring::{declaration::ComponentNode, Component};

use super::{
    contract::{
        erase_element, validate_elements, ContractDeclaration, ContractMachine, ErasedContract,
        ErasedContractDeclaration, ErasedElement, FinalReducer, RejectionHandler,
    },
    effects::{
        LiveEffectRuntime, LiveRuntimeFactory, ManagedEffectsFault, NoLiveRuntime, NoPublisher,
        PublisherFactory, StreamingToolAttemptPublisher,
    },
    MissingCompletion, NoStreamingValue, ReadyCompletion, RejectedStreamingToolAttempt,
    StreamingToolAttemptStart, StreamingToolChannels, StreamingToolDeclarationFault,
    StreamingToolDriverFault, StreamingToolRejectionAction, XmlAttemptSummary, XmlElementContract,
    XmlElementHandlers, XmlEnvelope,
};

/// Typestate marker for a required builder choice that has not been made.
pub struct Missing;

/// Typestate marker for a required builder choice that has been made.
pub struct Ready;

/// Typestate marker used before a reducer State type is chosen.
pub struct NoState;

type StateFactory<State> =
    Box<dyn Fn(StreamingToolAttemptStart) -> Result<State, String> + Send + Sync>;
type BuilderMarkers<StateSlot, ElementSlot, FinishSlot, LiveSlot, PublishSlot> =
    PhantomData<fn() -> (StateSlot, ElementSlot, FinishSlot, LiveSlot, PublishSlot)>;

/// A strict streaming XML contract declaration.
///
/// A fresh instance is created through [`XmlStreamingToolCall::new`](crate::component::authoring::XmlStreamingToolCall::new). The
/// builder holds only pure declaration data; its state factory is invoked once
/// for each actual provider reaction before the provider is submitted.
pub struct XmlStreamingAttemptBuilder<
    C: StreamingToolChannels,
    State = NoState,
    StateSlot = Missing,
    ElementSlot = Missing,
    FinishSlot = Missing,
    LiveSlot = Missing,
    PublishSlot = Missing,
    R = NoLiveRuntime,
    P = NoPublisher,
> {
    identity: &'static str,
    implementation_version: &'static str,
    envelope: XmlEnvelope,
    ignore_unknown_elements: bool,
    state_factory: Option<StateFactory<State>>,
    elements: Vec<Box<dyn ErasedElement<C, State>>>,
    finish_reducer: Option<FinalReducer<C, State>>,
    live_factory: Option<LiveRuntimeFactory<R>>,
    publisher_factory: Option<PublisherFactory<P>>,
    rejection_handler: Option<RejectionHandler<C::Diagnostic>>,
    marker: BuilderMarkers<StateSlot, ElementSlot, FinishSlot, LiveSlot, PublishSlot>,
}

impl<C: StreamingToolChannels> XmlStreamingAttemptBuilder<C> {
    pub(crate) fn new(identity: &'static str) -> Self {
        Self {
            identity,
            implementation_version: "v1",
            envelope: XmlEnvelope::strict_fragment(),
            ignore_unknown_elements: false,
            state_factory: None,
            elements: Vec::new(),
            finish_reducer: None,
            live_factory: None,
            publisher_factory: None,
            rejection_handler: None,
            marker: PhantomData,
        }
    }
}

impl<C, State, StateSlot, ElementSlot, FinishSlot, LiveSlot, PublishSlot, R, P>
    XmlStreamingAttemptBuilder<
        C,
        State,
        StateSlot,
        ElementSlot,
        FinishSlot,
        LiveSlot,
        PublishSlot,
        R,
        P,
    >
where
    C: StreamingToolChannels,
{
    /// Attach optional implementation metadata to this contract.
    pub fn version(mut self, version: &'static str) -> Self {
        self.implementation_version = version;
        self
    }

    /// Override the default strict XML envelope policy.
    pub fn envelope(mut self, envelope: XmlEnvelope) -> Self {
        self.envelope = envelope;
        self
    }

    /// Permit one open text element to complete at normal reaction EOF.
    pub fn allow_unclosed_text_at_eof(mut self) -> Self {
        self.envelope = self.envelope.allow_unclosed_text_at_eof();
        self
    }

    /// Let this independently parsed contract ignore top-level elements it did
    /// not declare.
    pub fn ignore_unknown_elements(mut self) -> Self {
        self.ignore_unknown_elements = true;
        self
    }
}

impl<C, Elements, Finish, Live, Publish, R, P>
    XmlStreamingAttemptBuilder<C, NoState, Missing, Elements, Finish, Live, Publish, R, P>
where
    C: StreamingToolChannels,
{
    pub fn state_with<State, Factory, InitError>(
        self,
        factory: Factory,
    ) -> XmlStreamingAttemptBuilder<C, State, Ready, Elements, Finish, Live, Publish, R, P>
    where
        State: Send + 'static,
        Factory: Fn(StreamingToolAttemptStart) -> Result<State, InitError> + Send + Sync + 'static,
        InitError: Error + Send + Sync + 'static,
    {
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: Some(Box::new(move |start| {
                factory(start).map_err(|error| error.to_string())
            })),
            elements: Vec::new(),
            finish_reducer: None,
            live_factory: self.live_factory,
            publisher_factory: self.publisher_factory,
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }
}

impl<C, State, Elements, Finish, Live, Publish, R, P>
    XmlStreamingAttemptBuilder<C, State, Ready, Elements, Finish, Live, Publish, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    pub fn element<Head, Value, Configure>(
        mut self,
        contract: XmlElementContract<Head, Value>,
        configure: Configure,
    ) -> XmlStreamingAttemptBuilder<C, State, Ready, Ready, Finish, Live, Publish, R, P>
    where
        Head: Send + Sync + 'static,
        Value: Send + 'static,
        Configure: FnOnce(
            XmlElementHandlers<State, C, Head, Value, MissingCompletion>,
        ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion>,
    {
        self.elements.push(erase_element(
            contract,
            configure(XmlElementHandlers::new()),
        ));
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: self.state_factory,
            elements: self.elements,
            finish_reducer: self.finish_reducer,
            live_factory: self.live_factory,
            publisher_factory: self.publisher_factory,
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }

    pub fn on_rejected<Handler, HandlerFuture, HandlerError>(mut self, handler: Handler) -> Self
    where
        Handler: Fn(RejectedStreamingToolAttempt<C::Diagnostic>) -> HandlerFuture
            + Send
            + Sync
            + 'static,
        HandlerFuture:
            Future<Output = Result<StreamingToolRejectionAction, HandlerError>> + Send + 'static,
        HandlerError: Error + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        self.rejection_handler = Some(Box::new(move |report| {
            let handler = Arc::clone(&handler);
            Box::pin(async move { handler(report).await.map_err(|error| error.to_string()) })
        }));
        self
    }
}

impl<C, State, Live, Publish, R, P>
    XmlStreamingAttemptBuilder<C, State, Ready, Ready, Missing, Live, Publish, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    pub fn finish<Reducer>(
        self,
        reducer: Reducer,
    ) -> XmlStreamingAttemptBuilder<C, State, Ready, Ready, Ready, Live, Publish, R, P>
    where
        Reducer: for<'a> Fn(
                State,
                XmlAttemptSummary<'a, C::Diagnostic>,
            ) -> super::StreamingToolDecision<C>
            + Send
            + Sync
            + 'static,
    {
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: self.state_factory,
            elements: self.elements,
            finish_reducer: Some(Box::new(reducer)),
            live_factory: self.live_factory,
            publisher_factory: self.publisher_factory,
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }
}

impl<C, State, StateSlot, Elements, Finish, Publish, R, P>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Missing, Publish, R, P>
where
    C: StreamingToolChannels,
{
    pub fn live_with<Factory, Runtime, InitError>(
        self,
        factory: Factory,
    ) -> XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Ready, Publish, Runtime, P>
    where
        Factory:
            Fn(StreamingToolAttemptStart) -> Result<Runtime, InitError> + Send + Sync + 'static,
        Runtime: LiveEffectRuntime<C::Live>,
        InitError: Error + Send + Sync + 'static,
    {
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: self.state_factory,
            elements: self.elements,
            finish_reducer: self.finish_reducer,
            live_factory: Some(Box::new(move |start| {
                factory(*start).map_err(|_| ManagedEffectsFault::LiveFactory)
            })),
            publisher_factory: self.publisher_factory,
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }
}

impl<C, State, StateSlot, Elements, Finish, Publish, R, P>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Missing, Publish, R, P>
where
    C: StreamingToolChannels<Live = NoStreamingValue>,
{
    pub fn without_live(
        self,
    ) -> XmlStreamingAttemptBuilder<
        C,
        State,
        StateSlot,
        Elements,
        Finish,
        Ready,
        Publish,
        NoLiveRuntime,
        P,
    > {
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: self.state_factory,
            elements: self.elements,
            finish_reducer: self.finish_reducer,
            live_factory: None,
            publisher_factory: self.publisher_factory,
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }
}

impl<C, State, StateSlot, Elements, Finish, Live, R, P>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Missing, R, P>
where
    C: StreamingToolChannels,
{
    pub fn publish_with<Factory, Publisher, InitError>(
        self,
        factory: Factory,
    ) -> XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Ready, R, Publisher>
    where
        Factory:
            Fn(StreamingToolAttemptStart) -> Result<Publisher, InitError> + Send + Sync + 'static,
        Publisher: StreamingToolAttemptPublisher<C>,
        InitError: Error + Send + Sync + 'static,
    {
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: self.state_factory,
            elements: self.elements,
            finish_reducer: self.finish_reducer,
            live_factory: self.live_factory,
            publisher_factory: Some(Box::new(move |start| {
                factory(*start).map_err(|_| ManagedEffectsFault::PublisherFactory)
            })),
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }
}

impl<C, State, StateSlot, Elements, Finish, Live, R, P>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Missing, R, P>
where
    C: StreamingToolChannels<Output = NoStreamingValue, Commit = NoStreamingValue>,
{
    pub fn without_publication(
        self,
    ) -> XmlStreamingAttemptBuilder<
        C,
        State,
        StateSlot,
        Elements,
        Finish,
        Live,
        Ready,
        R,
        NoPublisher,
    > {
        XmlStreamingAttemptBuilder {
            identity: self.identity,
            implementation_version: self.implementation_version,
            envelope: self.envelope,
            ignore_unknown_elements: self.ignore_unknown_elements,
            state_factory: self.state_factory,
            elements: self.elements,
            finish_reducer: self.finish_reducer,
            live_factory: self.live_factory,
            publisher_factory: None,
            rejection_handler: self.rejection_handler,
            marker: PhantomData,
        }
    }
}

impl<C, State, R, P> XmlStreamingAttemptBuilder<C, State, Ready, Ready, Ready, Ready, Ready, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    pub fn build(self) -> Component {
        Component::from_node(ComponentNode::StreamingAttempt(Box::new(
            ContractDeclaration::new(BuiltContractDeclaration {
                identity: self.identity,
                implementation_version: self.implementation_version,
                envelope: self.envelope,
                ignore_unknown_elements: self.ignore_unknown_elements,
                state_factory: self.state_factory,
                elements: self.elements,
                finish_reducer: self.finish_reducer,
                live_factory: self.live_factory,
                publisher_factory: self.publisher_factory,
                rejection_handler: self.rejection_handler,
            }),
        )))
    }
}

struct BuiltContractDeclaration<C, State, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    identity: &'static str,
    implementation_version: &'static str,
    envelope: XmlEnvelope,
    ignore_unknown_elements: bool,
    state_factory: Option<StateFactory<State>>,
    elements: Vec<Box<dyn ErasedElement<C, State>>>,
    finish_reducer: Option<FinalReducer<C, State>>,
    live_factory: Option<LiveRuntimeFactory<R>>,
    publisher_factory: Option<PublisherFactory<P>>,
    rejection_handler: Option<RejectionHandler<C::Diagnostic>>,
}

impl<C, State, R, P> ErasedContractDeclaration for BuiltContractDeclaration<C, State, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    fn identity(&self) -> &'static str {
        self.identity
    }

    fn implementation_version(&self) -> &'static str {
        self.implementation_version
    }

    fn validate_and_prompt(&self) -> Result<crate::pom::Document, StreamingToolDeclarationFault> {
        validate_elements(self.identity, &self.elements)
    }

    fn start(
        self: Box<Self>,
        start: StreamingToolAttemptStart,
    ) -> Result<Box<dyn ErasedContract>, StreamingToolDriverFault> {
        let Self {
            identity,
            implementation_version: _,
            envelope,
            ignore_unknown_elements,
            state_factory,
            elements,
            finish_reducer,
            live_factory,
            publisher_factory,
            rejection_handler,
        } = *self;
        // `start` is public to the Application layer as well as being reached
        // after render-time prompt validation. Keep this check here so state
        // construction can never precede declaration validation.
        validate_elements(identity, &elements).map_err(StreamingToolDriverFault::Declaration)?;
        let state_factory = state_factory.ok_or_else(|| {
            StreamingToolDriverFault::Declaration(StreamingToolDeclarationFault::Prompt {
                contract: identity,
                message: "state factory was not configured".to_owned(),
            })
        })?;
        let state = state_factory(start).map_err(|message| StreamingToolDriverFault::Adapter {
            contract: identity,
            message,
        })?;
        let finish_reducer = finish_reducer.ok_or_else(|| {
            StreamingToolDriverFault::Declaration(StreamingToolDeclarationFault::Prompt {
                contract: identity,
                message: "final reducer was not configured".to_owned(),
            })
        })?;
        Ok(Box::new(ContractMachine::new(
            start,
            envelope,
            ignore_unknown_elements,
            state,
            elements,
            finish_reducer,
            rejection_handler,
            live_factory,
            publisher_factory,
        )?))
    }
}
