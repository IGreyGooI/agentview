//! Direct, reaction-owned callbacks over the strict streaming XML grammar.
//!
//! These callbacks are ordinary application handlers. Earlier side effects are
//! not rolled back when later input is invalid or the reaction is cancelled.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
};

use crate::{
    component::{
        authoring::{
            async_task::ComponentTaskContext, declaration::ComponentNode, Component,
            ComponentAttemptFault, Signal,
        },
        signal::HookMount,
    },
    pom::{
        BlockChildren, BlockContent, Document, MixedContent, RawTextNode, TextNode, XmlName,
        XmlNode,
    },
};

use super::{
    contract::{
        erase_element, map_declaration_fault, map_invalid_kind, map_parser_declaration_fault,
        ElementCompleteDispatch, ElementOpenDispatch, ErasedElement,
    },
    parser::{
        CompletionForm, Contract, ElementForm, ElementSpec, Parser, ParserEvent, ParserFault,
    },
    NoStreamingValue, SelfClosing, StreamingToolAttemptId, StreamingToolChannels,
    StreamingToolDeclarationFault, StreamingToolDriverFault, StreamingToolEmission,
    StreamingToolUpdate, TextContent, XmlCardinality, XmlContractViolationKind, XmlDecodeViolation,
    XmlElementContract, XmlElementDraft, XmlElementForm, XmlElementHandlers, XmlOccurrenceId,
    XmlSourceSpan,
};

const CALLBACK_CONTRACT: &str = "agentview.xml-callbacks";
const MAX_RETAINED_DIAGNOSTICS: usize = 32;

type CallbackFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
type Callback<Input> = Box<dyn FnMut(Input) -> CallbackFuture + Send + 'static>;

/// Feedback for a model-authored XML action that could not be dispatched.
///
/// Ordinary invalid input does not stop later valid independent actions. The
/// Component also retains this diagnostic in its next model-visible projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XmlCallbackDiagnostic {
    pub element: Option<String>,
    pub code: &'static str,
    pub message: String,
    pub span: Option<XmlSourceSpan>,
}

impl fmt::Display for XmlCallbackDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(element) = &self.element {
            write!(formatter, "{element}: ")?;
        }
        formatter.write_str(&self.message)
    }
}

/// Conversion for synchronous or asynchronous XML callback results.
#[doc(hidden)]
pub trait XmlCallbackReturn<Mode>: Send + 'static {
    type Output: Send + 'static;
    type Error: fmt::Display + Send + 'static;
    type Future: Future<Output = Result<Self::Output, Self::Error>> + Send + 'static;

    fn into_callback_future(self) -> Self::Future;
}

#[doc(hidden)]
pub enum SyncXmlCallback {}
#[doc(hidden)]
pub enum AsyncXmlCallback {}

impl<Output, Error> XmlCallbackReturn<SyncXmlCallback> for Result<Output, Error>
where
    Output: Send + 'static,
    Error: fmt::Display + Send + 'static,
{
    type Output = Output;
    type Error = Error;
    type Future = std::future::Ready<Self>;

    fn into_callback_future(self) -> Self::Future {
        std::future::ready(self)
    }
}

impl<HandlerFuture, Output, Error> XmlCallbackReturn<AsyncXmlCallback> for HandlerFuture
where
    HandlerFuture: Future<Output = Result<Output, Error>> + Send + 'static,
    Output: Send + 'static,
    Error: fmt::Display + Send + 'static,
{
    type Output = Output;
    type Error = Error;
    type Future = Self;

    fn into_callback_future(self) -> Self::Future {
        self
    }
}

fn callback<Input, Handler, Return, Mode>(mut handler: Handler) -> Callback<Input>
where
    Handler: FnMut(Input) -> Return + Send + 'static,
    Return: XmlCallbackReturn<Mode>,
{
    Box::new(move |input| {
        let future = handler(input).into_callback_future();
        Box::pin(async move { future.await.map(|_| ()).map_err(|error| error.to_string()) })
    })
}

/// An element declaration accepted by the direct callback API.
///
/// Text drafts default to `String` content and self-closing drafts to `()`.
/// A fully decoded contract retains its own `Head` and `Value` types.
pub trait IntoXmlCallbackElement {
    type Head: Send + Sync + 'static;
    type Value: Send + 'static;

    fn into_callback_element(self) -> XmlElementContract<Self::Head, Self::Value>;
}

impl IntoXmlCallbackElement for XmlElementDraft<TextContent> {
    type Head = ();
    type Value = String;

    fn into_callback_element(self) -> XmlElementContract<(), String> {
        self.decode(|_| Ok(()), |_, text| Ok(text.to_owned()))
    }
}

impl IntoXmlCallbackElement for XmlElementDraft<SelfClosing> {
    type Head = ();
    type Value = ();

    fn into_callback_element(self) -> XmlElementContract<(), ()> {
        self.decode(|_| Ok(()), |_, _| Ok(()))
    }
}

impl<Head, Value> IntoXmlCallbackElement for XmlElementContract<Head, Value>
where
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    type Head = Head;
    type Value = Value;

    fn into_callback_element(self) -> Self {
        self
    }
}

/// Properties for a streaming XML action whose callbacks capture application state.
pub struct XmlStreamingToolCallProps<Element = ()> {
    element: Element,
    description: String,
    invalid: Option<Callback<XmlCallbackDiagnostic>>,
}

/// Typed callback properties after an element grammar has been chosen.
#[doc(hidden)]
pub struct XmlCallbackElement<Head, Value> {
    contract: XmlElementContract<Head, Value>,
    open: Option<Callback<Arc<Head>>>,
    delta: Option<Callback<String>>,
    complete: Option<Callback<Value>>,
}

impl XmlStreamingToolCallProps {
    pub(crate) fn new() -> Self {
        Self {
            element: (),
            description: String::new(),
            invalid: None,
        }
    }

    pub fn element<Element: IntoXmlCallbackElement>(
        self,
        element: Element,
    ) -> XmlStreamingToolCallProps<XmlCallbackElement<Element::Head, Element::Value>> {
        XmlStreamingToolCallProps {
            element: XmlCallbackElement {
                contract: element.into_callback_element(),
                open: None,
                delta: None,
                complete: None,
            },
            description: self.description,
            invalid: self.invalid,
        }
    }
}

impl<Element> XmlStreamingToolCallProps<Element> {
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Handle an ordinary model-input diagnostic after it has been retained for
    /// the next view. Unattributed input errors go to the first declared action.
    pub fn on_invalid<Handler, Return, Mode>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(XmlCallbackDiagnostic) -> Return + Send + 'static,
        Return: XmlCallbackReturn<Mode>,
    {
        self.invalid = Some(callback(handler));
        self
    }
}

impl<Head, Value> XmlStreamingToolCallProps<XmlCallbackElement<Head, Value>>
where
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    pub fn on_open<Handler, Return, Mode>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(Arc<Head>) -> Return + Send + 'static,
        Return: XmlCallbackReturn<Mode>,
    {
        self.element.open = Some(callback(handler));
        self
    }

    /// Receive newly decoded text, in source order. Earlier writes are not
    /// undone if this occurrence later turns out to be incomplete or invalid.
    pub fn on_delta<Handler, Return, Mode>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(String) -> Return + Send + 'static,
        Return: XmlCallbackReturn<Mode>,
    {
        self.element.delta = Some(callback(handler));
        self
    }

    /// Receive one fully decoded valid element as soon as it closes.
    pub fn on_complete<Handler, Return, Mode>(mut self, handler: Handler) -> Self
    where
        Handler: FnMut(Value) -> Return + Send + 'static,
        Return: XmlCallbackReturn<Mode>,
    {
        self.element.complete = Some(callback(handler));
        self
    }

    pub fn build(self) -> Component {
        let handlers = XmlElementHandlers::new()
            .on_open(|_, event| StreamingToolUpdate::output(CallbackEvent::Open(event.head)))
            .on_delta(|_, event| StreamingToolUpdate::output(CallbackEvent::Delta(event.delta)))
            .on_complete(|_, event| {
                StreamingToolUpdate::output(CallbackEvent::Complete(event.value))
            });
        Component::from_node(ComponentNode::XmlCallback(Box::new(
            XmlCallbackDeclaration {
                element: Box::new(TypedCallback {
                    inner: erase_element(self.element.contract, handlers),
                    open: self.element.open,
                    delta: self.element.delta,
                    complete: self.element.complete,
                }),
                description: self.description,
                invalid: self.invalid,
                feedback: None,
                mount: None,
                task_context: None,
            },
        )))
    }
}

#[derive(Clone, Default)]
pub(crate) struct XmlCallbackFeedback {
    diagnostics: Vec<XmlCallbackDiagnostic>,
    omitted: usize,
}

pub(crate) struct XmlCallbackDeclaration {
    element: Box<dyn ErasedCallback>,
    description: String,
    invalid: Option<Callback<XmlCallbackDiagnostic>>,
    feedback: Option<Signal<XmlCallbackFeedback>>,
    mount: Option<HookMount>,
    task_context: Option<ComponentTaskContext>,
}

impl XmlCallbackDeclaration {
    pub(crate) fn name(&self) -> &'static str {
        self.element.name()
    }

    pub(crate) fn mount(
        &mut self,
        feedback: Signal<XmlCallbackFeedback>,
        mount: HookMount,
        task_context: Option<ComponentTaskContext>,
    ) {
        self.feedback = Some(feedback);
        self.mount = Some(mount);
        self.task_context = task_context;
    }

    fn invoke(
        &mut self,
        operation: impl FnOnce(&mut dyn ErasedCallback) -> Result<Dispatch, StreamingToolDriverFault>,
    ) -> Result<Dispatch, StreamingToolDriverFault> {
        let _permit = self
            .mount
            .as_ref()
            .and_then(HookMount::authorize)
            .ok_or_else(|| {
                reducer_fault(self.name(), "XML callback belongs to an inactive mount")
            })?;
        let context = self.task_context.clone();
        let dispatch = match &context {
            Some(context) => context.scope_sync(|| operation(self.element.as_mut())),
            None => operation(self.element.as_mut()),
        }?;
        Ok(match (dispatch, context) {
            (Dispatch::Ready(Some(future)), Some(context)) => {
                Dispatch::Ready(Some(Box::pin(context.scope(future))))
            }
            (dispatch, _) => dispatch,
        })
    }

    fn invalid_future(
        &mut self,
        diagnostic: XmlCallbackDiagnostic,
    ) -> Result<Option<CallbackFuture>, StreamingToolDriverFault> {
        if self.invalid.is_none() {
            return Ok(None);
        }
        let _permit = self
            .mount
            .as_ref()
            .and_then(HookMount::authorize)
            .ok_or_else(|| {
                reducer_fault(self.name(), "XML callback belongs to an inactive mount")
            })?;
        let context = self.task_context.clone();
        let handler = self.invalid.as_mut().expect("checked callback");
        let future = match &context {
            Some(context) => context.scope_sync(|| handler(diagnostic)),
            None => handler(diagnostic),
        };
        Ok(Some(match context {
            Some(context) => Box::pin(context.scope(future)),
            None => future,
        }))
    }

    pub(crate) fn prompt_document(&self) -> Result<Document, ComponentAttemptFault> {
        self.element.validate().map_err(map_declaration_fault)?;
        let mut children = BlockChildren::new();
        if !self.description.is_empty() {
            children.push(BlockContent::raw_text(RawTextNode::new(
                self.description.clone(),
            )));
        }
        children.push(BlockContent::xml(
            self.element.prompt().map_err(map_declaration_fault)?,
        ));
        let cardinality = self.element.cardinality();
        if cardinality != XmlCardinality::any() {
            let maximum = cardinality
                .max()
                .map(|maximum| maximum.to_string())
                .unwrap_or_else(|| "unbounded".to_owned());
            children.push(BlockContent::raw_text(RawTextNode::new(format!(
                "Action `{}` occurrences per response: minimum {}, maximum {}.",
                self.name(),
                cardinality.min(),
                maximum,
            ))));
        }
        if let Some(feedback) = &self.feedback {
            let feedback = feedback.with(Clone::clone).map_err(|error| {
                ComponentAttemptFault::RuntimeInvariant {
                    message: error.to_string(),
                }
            })?;
            if !feedback.diagnostics.is_empty() || feedback.omitted != 0 {
                let mut node =
                    XmlNode::new(XmlName::new("xml_action_feedback").expect("static name"));
                node.push_attribute(XmlName::new("action").expect("static name"), self.name())
                    .expect("unique attribute");
                for diagnostic in feedback.diagnostics {
                    let mut item = XmlNode::new(XmlName::new("invalid").expect("static name"));
                    item.push_attribute(
                        XmlName::new("code").expect("static name"),
                        diagnostic.code,
                    )
                    .expect("unique attribute");
                    if let Some(element) = diagnostic.element {
                        item.push_attribute(XmlName::new("element").expect("static name"), element)
                            .expect("unique attribute");
                    }
                    item.push(MixedContent::text(TextNode::new(diagnostic.message)));
                    node.push(MixedContent::xml(item));
                }
                if feedback.omitted != 0 {
                    let mut item =
                        XmlNode::new(XmlName::new("additional_diagnostics").expect("static name"));
                    item.push(MixedContent::text(TextNode::new(
                        feedback.omitted.to_string(),
                    )));
                    node.push(MixedContent::xml(item));
                }
                children.push(BlockContent::xml(node));
            }
        }
        Ok(Document::new(children))
    }
}

enum CallbackEvent<Head, Value> {
    Open(Arc<Head>),
    Delta(String),
    Complete(Value),
}

struct CallbackChannels<Head, Value>(PhantomData<fn() -> (Head, Value)>);

impl<Head: Send + Sync + 'static, Value: Send + 'static> StreamingToolChannels
    for CallbackChannels<Head, Value>
{
    type Output = CallbackEvent<Head, Value>;
    type Live = NoStreamingValue;
    type Commit = NoStreamingValue;
    type Diagnostic = NoStreamingValue;
}

enum Dispatch {
    Ready(Option<CallbackFuture>),
    Invalid(Vec<XmlCallbackDiagnostic>),
}

trait ErasedCallback: Send {
    fn name(&self) -> &'static str;
    fn form(&self) -> ElementForm;
    fn cardinality(&self) -> XmlCardinality;
    fn validate(&self) -> Result<(), StreamingToolDeclarationFault>;
    fn prompt(&self) -> Result<XmlNode, StreamingToolDeclarationFault>;
    fn open(
        &mut self,
        occurrence: u64,
        attributes: Vec<(String, String)>,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault>;
    fn delta(
        &mut self,
        occurrence: u64,
        delta: String,
        accumulated: Arc<str>,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault>;
    fn complete(
        &mut self,
        occurrence: u64,
        form: XmlElementForm,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault>;
    fn discard(&mut self, occurrence: u64);
}

struct TypedCallback<Head, Value>
where
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    inner: Box<dyn ErasedElement<CallbackChannels<Head, Value>, ()>>,
    open: Option<Callback<Arc<Head>>>,
    delta: Option<Callback<String>>,
    complete: Option<Callback<Value>>,
}

impl<Head: Send + Sync + 'static, Value: Send + 'static> TypedCallback<Head, Value> {
    fn dispatch(&mut self, update: StreamingToolUpdate<CallbackChannels<Head, Value>>) -> Dispatch {
        let mut invoke = None;
        for entry in update.into_entries() {
            invoke = match entry {
                StreamingToolEmission::Output(CallbackEvent::Open(head)) => {
                    self.open.as_mut().map(|handler| handler(head))
                }
                StreamingToolEmission::Output(CallbackEvent::Delta(text)) => {
                    self.delta.as_mut().map(|handler| handler(text))
                }
                StreamingToolEmission::Output(CallbackEvent::Complete(value)) => {
                    self.complete.as_mut().map(|handler| handler(value))
                }
                StreamingToolEmission::Live(never)
                | StreamingToolEmission::Commit(never)
                | StreamingToolEmission::Diagnostic(never) => match never {},
            };
        }
        Dispatch::Ready(invoke)
    }

    fn decode_failure(
        &self,
        violation: XmlDecodeViolation,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault> {
        match violation {
            XmlDecodeViolation::Model { code, expected } => {
                Ok(Dispatch::Invalid(vec![XmlCallbackDiagnostic {
                    element: Some(self.inner.name().to_owned()),
                    code,
                    message: format!("Input could not be decoded. Expected: {expected}."),
                    span: Some(span),
                }]))
            }
            XmlDecodeViolation::AttributeAccess(access) => {
                Err(reducer_fault(self.inner.name(), access.code()))
            }
        }
    }
}

impl<Head: Send + Sync + 'static, Value: Send + 'static> ErasedCallback
    for TypedCallback<Head, Value>
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn form(&self) -> ElementForm {
        self.inner.form()
    }
    fn cardinality(&self) -> XmlCardinality {
        self.inner.cardinality()
    }

    fn validate(&self) -> Result<(), StreamingToolDeclarationFault> {
        self.inner.validate_declaration(self.inner.name())?;
        if self.open.is_none() && self.delta.is_none() && self.complete.is_none() {
            return Err(StreamingToolDeclarationFault::Prompt {
                contract: self.inner.name(),
                message: "XML action requires an on_open, on_delta, or on_complete callback"
                    .to_owned(),
            });
        }
        if self.inner.form() == ElementForm::SelfClosing
            && self.open.is_none()
            && self.complete.is_none()
        {
            return Err(StreamingToolDeclarationFault::Prompt {
                contract: self.inner.name(),
                message: "a self-closing XML action requires an on_open or on_complete callback"
                    .to_owned(),
            });
        }
        Ok(())
    }

    fn prompt(&self) -> Result<XmlNode, StreamingToolDeclarationFault> {
        self.inner.prompt_node()
    }

    fn open(
        &mut self,
        occurrence: u64,
        attributes: Vec<(String, String)>,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault> {
        // The existing typed decoder supplies validation and occurrence state;
        // its private reducer only forwards payloads to this direct executor.
        match self.inner.open(
            &(),
            StreamingToolAttemptId::new(0),
            0,
            XmlOccurrenceId::new(occurrence),
            attributes,
            span,
        ) {
            ElementOpenDispatch::Opened(update) => Ok(self.dispatch(update)),
            ElementOpenDispatch::Contract(problems) => Ok(Dispatch::Invalid(
                problems
                    .into_iter()
                    .map(|problem| XmlCallbackDiagnostic {
                        element: Some(self.inner.name().to_owned()),
                        code: problem.detail,
                        message: contract_message(problem.kind, problem.attribute),
                        span: Some(span),
                    })
                    .collect(),
            )),
            ElementOpenDispatch::Decode(violation) => self.decode_failure(violation, span),
            ElementOpenDispatch::Invariant(fault) => {
                Err(reducer_fault(self.inner.name(), fault.message))
            }
        }
    }

    fn delta(
        &mut self,
        occurrence: u64,
        delta: String,
        accumulated: Arc<str>,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault> {
        let update = self
            .inner
            .delta(
                &(),
                StreamingToolAttemptId::new(0),
                0,
                XmlOccurrenceId::new(occurrence),
                delta,
                accumulated,
                span,
            )
            .map_err(|fault| reducer_fault(self.inner.name(), fault.message))?;
        Ok(self.dispatch(update))
    }

    fn complete(
        &mut self,
        occurrence: u64,
        form: XmlElementForm,
        span: XmlSourceSpan,
    ) -> Result<Dispatch, StreamingToolDriverFault> {
        match self.inner.complete(
            &mut (),
            StreamingToolAttemptId::new(0),
            0,
            XmlOccurrenceId::new(occurrence),
            form,
            span,
        ) {
            ElementCompleteDispatch::Completed(update) => Ok(self.dispatch(update)),
            ElementCompleteDispatch::DomainInvalid(never) => {
                for value in never {
                    match value {}
                }
                Err(reducer_fault(
                    self.inner.name(),
                    "unexpected callback validator rejection",
                ))
            }
            ElementCompleteDispatch::Decode(violation) => self.decode_failure(violation, span),
            ElementCompleteDispatch::Invariant(fault) => {
                Err(reducer_fault(self.inner.name(), fault.message))
            }
        }
    }

    fn discard(&mut self, occurrence: u64) {
        self.inner.discard(XmlOccurrenceId::new(occurrence));
    }
}

/// A parser and callback set owned only by the active reaction. Dropping its
/// dispatch future cancels the current async callback without replay or rollback.
pub(crate) struct XmlCallbackRuntime {
    declarations: Vec<XmlCallbackDeclaration>,
    parser: Parser,
    indexes: HashMap<&'static str, usize>,
    occurrences: HashMap<u64, usize>,
    invalid: HashSet<u64>,
    seen: Vec<usize>,
    started: bool,
    sealed: bool,
    finished: bool,
}

impl XmlCallbackRuntime {
    pub(crate) fn new(
        declarations: Vec<XmlCallbackDeclaration>,
        ignored_names: Vec<&'static str>,
    ) -> Result<Self, StreamingToolDriverFault> {
        for declaration in &declarations {
            declaration.element.validate()?;
            if ignored_names.contains(&declaration.name()) {
                return Err(StreamingToolDeclarationFault::DuplicateElement {
                    contract: CALLBACK_CONTRACT,
                    element: declaration.name(),
                }
                .into());
            }
        }
        let specs = declarations
            .iter()
            .map(|declaration| ElementSpec {
                name: declaration.name(),
                form: declaration.element.form(),
            })
            .collect();
        let mut contract = Contract::strict(specs);
        contract.ignored_known_elements = ignored_names.into_iter().collect();
        let parser = Parser::new(contract).map_err(|fault| {
            StreamingToolDriverFault::Declaration(map_parser_declaration_fault(
                CALLBACK_CONTRACT,
                fault,
            ))
        })?;
        let indexes = declarations
            .iter()
            .enumerate()
            .map(|(index, declaration)| (declaration.name(), index))
            .collect();
        let seen = vec![0; declarations.len()];
        Ok(Self {
            declarations,
            parser,
            indexes,
            occurrences: HashMap::new(),
            invalid: HashSet::new(),
            seen,
            started: false,
            sealed: false,
            finished: false,
        })
    }

    fn start_feedback(&mut self) -> Result<(), StreamingToolDriverFault> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        for declaration in &self.declarations {
            if let Some(feedback) = &declaration.feedback {
                feedback
                    .update(|feedback| {
                        feedback.diagnostics.clear();
                        feedback.omitted = 0;
                    })
                    .map_err(|error| reducer_fault(declaration.name(), &error.to_string()))?;
            }
        }
        Ok(())
    }

    pub(crate) async fn push(&mut self, text: &str) -> Result<(), StreamingToolDriverFault> {
        if self.declarations.is_empty() {
            return Ok(());
        }
        if self.sealed {
            return Err(input_fault(
                "received text after selected output was sealed",
            ));
        }
        self.start_feedback()?;
        let events = self.parser.push(text).map_err(parser_fault)?;
        self.process(events).await
    }

    pub(crate) async fn seal(&mut self, text: &str) -> Result<(), StreamingToolDriverFault> {
        if self.declarations.is_empty() {
            return Ok(());
        }
        if self.sealed {
            return Err(input_fault("selected output was sealed more than once"));
        }
        let suffix = text
            .strip_prefix(self.parser.raw_output())
            .ok_or_else(|| input_fault("sealed text did not extend observed XML text"))?;
        self.push(suffix).await?;
        self.sealed = true;
        Ok(())
    }

    pub(crate) async fn finish(&mut self) -> Result<(), StreamingToolDriverFault> {
        if self.declarations.is_empty() {
            return Ok(());
        }
        if self.finished {
            return Err(input_fault("normal EOF was already processed"));
        }
        self.start_feedback()?;
        let events = self.parser.finish_normal().map_err(parser_fault)?;
        self.process(events).await?;
        for index in 0..self.declarations.len() {
            if self.seen[index] < self.declarations[index].element.cardinality().min() {
                self.report(
                    index,
                    XmlCallbackDiagnostic {
                        element: Some(self.declarations[index].name().to_owned()),
                        code: "minimum_cardinality",
                        message: "The required number of this action was not provided.".to_owned(),
                        span: None,
                    },
                )
                .await?;
            }
        }
        self.finished = true;
        Ok(())
    }

    async fn process(&mut self, events: Vec<ParserEvent>) -> Result<(), StreamingToolDriverFault> {
        for event in events {
            match event {
                ParserEvent::Open {
                    occurrence,
                    name,
                    attributes,
                    span,
                    ..
                } => {
                    let index = *self.indexes.get(name.as_str()).ok_or_else(|| {
                        reducer_fault(CALLBACK_CONTRACT, "unknown parser element")
                    })?;
                    self.register(occurrence, index);
                    let span = XmlSourceSpan::from_range(span);
                    if self.declarations[index]
                        .element
                        .cardinality()
                        .max()
                        .is_some_and(|max| self.seen[index] > max)
                    {
                        self.invalidate(occurrence, index);
                        self.report(
                            index,
                            XmlCallbackDiagnostic {
                                element: Some(name),
                                code: "maximum_cardinality",
                                message: "This action exceeded its allowed number of occurrences."
                                    .to_owned(),
                                span: Some(span),
                            },
                        )
                        .await?;
                        continue;
                    }
                    let dispatch = self.declarations[index]
                        .invoke(|element| element.open(occurrence, attributes, span))?;
                    self.dispatch(index, occurrence, dispatch).await?;
                }
                ParserEvent::Delta {
                    occurrence,
                    delta,
                    accumulated,
                    span,
                } => {
                    if self.invalid.contains(&occurrence) {
                        continue;
                    }
                    let index = self.owner(occurrence)?;
                    let dispatch = self.declarations[index].invoke(|element| {
                        element.delta(
                            occurrence,
                            delta,
                            accumulated,
                            XmlSourceSpan::from_range(span),
                        )
                    })?;
                    self.dispatch(index, occurrence, dispatch).await?;
                }
                ParserEvent::Complete {
                    occurrence,
                    form,
                    span,
                } => {
                    if self.invalid.contains(&occurrence) {
                        continue;
                    }
                    let index = self.owner(occurrence)?;
                    let form = match form {
                        CompletionForm::SelfClosing => XmlElementForm::SelfClosing,
                        CompletionForm::ExplicitClose => XmlElementForm::ExplicitClose,
                        CompletionForm::ImplicitEof => XmlElementForm::ImplicitEof,
                    };
                    let dispatch = self.declarations[index].invoke(|element| {
                        element.complete(occurrence, form, XmlSourceSpan::from_range(span))
                    })?;
                    self.dispatch(index, occurrence, dispatch).await?;
                }
                ParserEvent::Invalid {
                    occurrence,
                    element,
                    kind,
                    span,
                } => {
                    let owner = element
                        .as_deref()
                        .and_then(|name| self.indexes.get(name).copied())
                        .or_else(|| occurrence.and_then(|id| self.occurrences.get(&id).copied()));
                    if let (Some(id), Some(index)) = (occurrence, owner) {
                        self.register(id, index);
                        self.invalidate(id, index);
                    }
                    let (kind, code) = map_invalid_kind(kind);
                    self.report(
                        owner.unwrap_or(0),
                        XmlCallbackDiagnostic {
                            element,
                            code,
                            message: contract_message(kind, None),
                            span: span.map(XmlSourceSpan::from_range),
                        },
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    fn register(&mut self, occurrence: u64, index: usize) {
        if self.occurrences.insert(occurrence, index).is_none() {
            self.seen[index] += 1;
        }
    }

    fn owner(&self, occurrence: u64) -> Result<usize, StreamingToolDriverFault> {
        self.occurrences
            .get(&occurrence)
            .copied()
            .ok_or_else(|| reducer_fault(CALLBACK_CONTRACT, "parser occurrence has no action"))
    }

    fn invalidate(&mut self, occurrence: u64, index: usize) {
        self.invalid.insert(occurrence);
        self.declarations[index].element.discard(occurrence);
    }

    async fn dispatch(
        &mut self,
        index: usize,
        occurrence: u64,
        dispatch: Dispatch,
    ) -> Result<(), StreamingToolDriverFault> {
        match dispatch {
            Dispatch::Ready(Some(future)) => {
                future
                    .await
                    .map_err(|message| StreamingToolDriverFault::Adapter {
                        contract: self.declarations[index].name(),
                        message,
                    })
            }
            Dispatch::Ready(None) => Ok(()),
            Dispatch::Invalid(diagnostics) => {
                self.invalidate(occurrence, index);
                for diagnostic in diagnostics {
                    self.report(index, diagnostic).await?;
                }
                Ok(())
            }
        }
    }

    async fn report(
        &mut self,
        index: usize,
        diagnostic: XmlCallbackDiagnostic,
    ) -> Result<(), StreamingToolDriverFault> {
        let declaration = &mut self.declarations[index];
        if let Some(feedback) = &declaration.feedback {
            feedback
                .update(|feedback| {
                    if feedback.diagnostics.len() == MAX_RETAINED_DIAGNOSTICS {
                        feedback.diagnostics.remove(0);
                        feedback.omitted = feedback.omitted.saturating_add(1);
                    }
                    feedback.diagnostics.push(diagnostic.clone());
                })
                .map_err(|error| reducer_fault(declaration.name(), &error.to_string()))?;
        }
        if let Some(future) = declaration.invalid_future(diagnostic)? {
            future
                .await
                .map_err(|message| StreamingToolDriverFault::Adapter {
                    contract: declaration.name(),
                    message,
                })?;
        }
        Ok(())
    }
}

fn contract_message(kind: XmlContractViolationKind, attribute: Option<&str>) -> String {
    match kind {
        XmlContractViolationKind::UnknownElement => {
            "Unknown XML action; use one of the declared action tags.".to_owned()
        }
        XmlContractViolationKind::UnknownNamespace => {
            "Namespaced XML actions are not supported.".to_owned()
        }
        XmlContractViolationKind::MissingAttribute => {
            format!("Missing required attribute `{}`.", attribute.unwrap_or("?"))
        }
        XmlContractViolationKind::DuplicateAttribute => {
            "An attribute was repeated; provide each attribute once.".to_owned()
        }
        XmlContractViolationKind::UnknownAttribute => {
            "An undeclared attribute was supplied; use the declared inputs.".to_owned()
        }
        XmlContractViolationKind::InvalidAttribute => {
            "An attribute does not match its declared input type.".to_owned()
        }
        XmlContractViolationKind::WrongElementForm => {
            "Use the declared paired or self-closing XML form.".to_owned()
        }
        XmlContractViolationKind::NestedMarkup => {
            "This action accepts text, not nested XML markup.".to_owned()
        }
        XmlContractViolationKind::TextOutsideEnvelope => {
            "Text outside an XML action was ignored; use the declared action tags.".to_owned()
        }
        XmlContractViolationKind::Incomplete => {
            "The action was incomplete; provide its complete closing syntax.".to_owned()
        }
        XmlContractViolationKind::Cardinality => {
            "The action occurrence count does not match its declared constraint.".to_owned()
        }
        XmlContractViolationKind::Malformed => {
            "Malformed XML action; check matching tags, attributes, and escaped text.".to_owned()
        }
    }
}

fn reducer_fault(contract: &'static str, message: &str) -> StreamingToolDriverFault {
    StreamingToolDriverFault::Reducer {
        contract,
        message: message.to_owned(),
    }
}

fn input_fault(message: &str) -> StreamingToolDriverFault {
    StreamingToolDriverFault::Input {
        contract: CALLBACK_CONTRACT,
        message: message.to_owned(),
    }
}

fn parser_fault(fault: ParserFault) -> StreamingToolDriverFault {
    let message = fault.to_string();
    match fault {
        ParserFault::AfterFinish | ParserFault::Aborted => input_fault(&message),
        _ => StreamingToolDriverFault::Limit {
            contract: CALLBACK_CONTRACT,
            message,
        },
    }
}
