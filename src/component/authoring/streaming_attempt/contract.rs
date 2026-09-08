//! Type erasure and reducer runtime for one mounted streaming-tool contract.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use futures::FutureExt;

use crate::{
    component::authoring::ComponentAttemptFault,
    pom::{BlockChildren, BlockContent, Document, MixedContent, TextNode, XmlName, XmlNode},
};

use super::types::{CompleteDispatch, XmlElementFormKind};
use super::{
    effects::{
        EffectsUpdate, LiveEffectRuntime, LiveRuntimeFactory, ManagedEffects, ManagedEffectsFault,
        ManagedEffectsStatus, PublisherFactory, StreamingToolAttemptPublisher,
    },
    parser::{
        CompletionForm, Contract as ParserContract, ElementForm, ElementSpec, InvalidKind, Parser,
        ParserDeclarationFault, ParserEvent, ParserFault, UnknownTopLevel,
    },
    ReadyCompletion, StreamingEmissionOrigin, StreamingToolAbortCause, StreamingToolAttemptContext,
    StreamingToolAttemptId, StreamingToolAttemptStart, StreamingToolChannels,
    StreamingToolDecision, StreamingToolDeclarationFault, StreamingToolDiagnostic,
    StreamingToolDiagnosticOrigin, StreamingToolDiagnosticRecord, StreamingToolDriverFault,
    StreamingToolRejectionAction, StreamingToolUpdate, XmlAttemptSummary, XmlCardinality,
    XmlComplete, XmlContractViolation, XmlContractViolationKind, XmlDecodeViolation,
    XmlDecodedAttributes, XmlElementContract, XmlElementForm, XmlElementHandlers,
    XmlElementSummary, XmlEnvelope, XmlOccurrenceId, XmlOpen, XmlSourceSpan, XmlTextDelta,
};

/// A declaration retained by the Component tree until a reaction starts.
pub(crate) struct ContractDeclaration {
    inner: Box<dyn ErasedContractDeclaration>,
}

impl ContractDeclaration {
    pub(crate) fn new(inner: impl ErasedContractDeclaration + 'static) -> Self {
        Self {
            inner: Box::new(inner),
        }
    }

    pub(crate) fn identity(&self) -> &'static str {
        self.inner.identity()
    }

    pub(crate) fn implementation_version(&self) -> &'static str {
        self.inner.implementation_version()
    }

    /// Validates the declaration before prompt generation, state construction,
    /// or provider submission.
    pub(crate) fn validate_and_prompt(&self) -> Result<Document, ComponentAttemptFault> {
        self.inner
            .validate_and_prompt()
            .map_err(map_declaration_fault)
    }

    /// Starts a fresh parser, reducer state, and managed-effect ledger for one
    /// reaction. The caller supplies the attempt identity it owns.
    pub(crate) fn start(
        self,
        start: StreamingToolAttemptStart,
    ) -> Result<Box<dyn ErasedContract>, StreamingToolDriverFault> {
        self.inner.start(start)
    }
}

fn map_declaration_fault(fault: StreamingToolDeclarationFault) -> ComponentAttemptFault {
    match fault {
        StreamingToolDeclarationFault::InvalidElementName {
            contract,
            element,
            detail,
        } => ComponentAttemptFault::InvalidStreamingToolElementName {
            contract,
            element,
            detail,
        },
        StreamingToolDeclarationFault::InvalidAttributeName {
            contract,
            element,
            attribute,
            detail,
        } => ComponentAttemptFault::InvalidStreamingToolAttributeName {
            contract,
            element,
            attribute,
            detail,
        },
        StreamingToolDeclarationFault::DuplicateElement { contract, element } => {
            ComponentAttemptFault::DuplicateStreamingToolElement { contract, element }
        }
        StreamingToolDeclarationFault::DuplicateAttribute {
            contract,
            element,
            attribute,
        } => ComponentAttemptFault::DuplicateStreamingToolAttribute {
            contract,
            element,
            attribute,
        },
        fault => ComponentAttemptFault::StreamingMount {
            message: fault.to_string(),
        },
    }
}

pub(crate) trait ErasedContractDeclaration: Send {
    fn identity(&self) -> &'static str;
    fn implementation_version(&self) -> &'static str;
    fn validate_and_prompt(&self) -> Result<Document, StreamingToolDeclarationFault>;
    fn start(
        self: Box<Self>,
        start: StreamingToolAttemptStart,
    ) -> Result<Box<dyn ErasedContract>, StreamingToolDriverFault>;
}

/// One independently parsed, independently settled contract execution.
///
/// `seal` only validates the sealed primary text and appends any suffix. The
/// normal XML EOF transition is exclusively `finish`.
#[async_trait]
pub(crate) trait ErasedContract: Send {
    fn identity(&self) -> &'static str;
    fn attempt(&self) -> StreamingToolAttemptId;
    fn set_cancellation(&mut self, cancelled: Arc<AtomicBool>);
    async fn push(&mut self, text: &str) -> Result<(), StreamingToolDriverFault>;
    async fn seal(&mut self, text: &str) -> Result<(), StreamingToolDriverFault>;
    async fn finish(&mut self) -> Result<(), StreamingToolDriverFault>;
    async fn abort(
        &mut self,
        cause: StreamingToolAbortCause,
    ) -> Result<(), StreamingToolDriverFault>;
    async fn recover(&mut self) -> Result<(), StreamingToolDriverFault>;
    fn needs_recovery(&self) -> bool;
    fn terminal_fault(&self) -> Option<StreamingToolDriverFault>;
    fn reaction_requested(&self) -> bool;
    fn completed(&self) -> bool;
    fn accepted(&self) -> bool;
}

pub(super) type FinalReducer<C, State> = Box<
    dyn for<'a> Fn(
            State,
            XmlAttemptSummary<'a, <C as StreamingToolChannels>::Diagnostic>,
        ) -> StreamingToolDecision<C>
        + Send
        + Sync,
>;

pub(super) type RejectionFuture =
    Pin<Box<dyn Future<Output = Result<StreamingToolRejectionAction, String>> + Send + 'static>>;

pub(super) type RejectionHandler<D> =
    Box<dyn Fn(super::RejectedStreamingToolAttempt<D>) -> RejectionFuture + Send + Sync>;

/// Runtime-erased operations for a heterogeneous element declaration.
pub(super) trait ErasedElement<C, State>: Send
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    fn name(&self) -> &'static str;
    fn form(&self) -> ElementForm;
    fn cardinality(&self) -> XmlCardinality;
    fn validate_declaration(
        &self,
        identity: &'static str,
    ) -> Result<(), StreamingToolDeclarationFault>;
    fn prompt_node(&self) -> Result<XmlNode, StreamingToolDeclarationFault>;
    fn open(
        &mut self,
        state: &State,
        attempt: StreamingToolAttemptId,
        sequence: u64,
        occurrence: XmlOccurrenceId,
        attributes: Vec<(String, String)>,
        span: XmlSourceSpan,
    ) -> ElementOpenDispatch<C>;
    #[allow(
        clippy::too_many_arguments,
        reason = "the erased parser boundary forwards one fully attributed delta without allocation"
    )]
    fn delta(
        &mut self,
        state: &State,
        attempt: StreamingToolAttemptId,
        sequence: u64,
        occurrence: XmlOccurrenceId,
        delta: String,
        accumulated: Arc<str>,
        span: XmlSourceSpan,
    ) -> Result<StreamingToolUpdate<C>, ElementInvariant>;
    fn complete(
        &mut self,
        state: &mut State,
        attempt: StreamingToolAttemptId,
        sequence: u64,
        occurrence: XmlOccurrenceId,
        form: XmlElementForm,
        span: XmlSourceSpan,
    ) -> ElementCompleteDispatch<C>;
    fn discard(&mut self, occurrence: XmlOccurrenceId);
}

pub(super) fn erase_element<C, State, Head, Value>(
    contract: XmlElementContract<Head, Value>,
    handlers: XmlElementHandlers<State, C, Head, Value, ReadyCompletion>,
) -> Box<dyn ErasedElement<C, State>>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    Box::new(TypedElement {
        contract,
        handlers,
        occurrences: HashMap::new(),
    })
}

struct TypedElement<C, State, Head, Value>
where
    C: StreamingToolChannels,
{
    contract: XmlElementContract<Head, Value>,
    handlers: XmlElementHandlers<State, C, Head, Value, ReadyCompletion>,
    occurrences: HashMap<XmlOccurrenceId, TypedOccurrence<Head>>,
}

struct TypedOccurrence<Head> {
    head: Arc<Head>,
    text: String,
}

pub(super) enum ElementOpenDispatch<C: StreamingToolChannels> {
    Opened(StreamingToolUpdate<C>),
    Contract(Vec<ElementContractProblem>),
    Decode(XmlDecodeViolation),
    Invariant(ElementInvariant),
}

pub(super) enum ElementCompleteDispatch<C: StreamingToolChannels> {
    Completed(StreamingToolUpdate<C>),
    DomainInvalid(Vec<C::Diagnostic>),
    Decode(XmlDecodeViolation),
    Invariant(ElementInvariant),
}

#[derive(Debug)]
pub(super) struct ElementInvariant {
    message: &'static str,
}

#[derive(Clone, Copy)]
pub(super) struct ElementContractProblem {
    attribute: Option<&'static str>,
    kind: XmlContractViolationKind,
    detail: &'static str,
}

impl<C, State, Head, Value> ErasedElement<C, State> for TypedElement<C, State, Head, Value>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    fn name(&self) -> &'static str {
        self.contract.name
    }

    fn form(&self) -> ElementForm {
        match self.contract.form {
            XmlElementFormKind::SelfClosing => ElementForm::SelfClosing,
            XmlElementFormKind::Text => ElementForm::Text,
        }
    }

    fn cardinality(&self) -> XmlCardinality {
        self.contract.cardinality
    }

    fn validate_declaration(
        &self,
        identity: &'static str,
    ) -> Result<(), StreamingToolDeclarationFault> {
        if !is_xml_name(self.contract.name) {
            return Err(StreamingToolDeclarationFault::InvalidElementName {
                contract: identity,
                element: self.contract.name,
                detail: "expected an unqualified ASCII XML name".to_owned(),
            });
        }
        let mut attributes = HashSet::new();
        for attribute in &self.contract.attributes {
            if !is_xml_name(attribute.name) {
                return Err(StreamingToolDeclarationFault::InvalidAttributeName {
                    contract: identity,
                    element: self.contract.name,
                    attribute: attribute.name,
                    detail: "expected an unqualified ASCII XML name".to_owned(),
                });
            }
            if !attributes.insert(attribute.name) {
                return Err(StreamingToolDeclarationFault::DuplicateAttribute {
                    contract: identity,
                    element: self.contract.name,
                    attribute: attribute.name,
                });
            }
        }
        Ok(())
    }

    fn prompt_node(&self) -> Result<XmlNode, StreamingToolDeclarationFault> {
        let name = XmlName::new(self.contract.name).map_err(|error| {
            StreamingToolDeclarationFault::Prompt {
                contract: "unknown",
                message: error.to_string(),
            }
        })?;
        let mut node = XmlNode::new(name);
        for attribute in &self.contract.attributes {
            node.push_attribute(
                XmlName::new(attribute.name).map_err(|error| {
                    StreamingToolDeclarationFault::Prompt {
                        contract: "unknown",
                        message: error.to_string(),
                    }
                })?,
                attribute.prompt_example,
            )
            .map_err(|error| StreamingToolDeclarationFault::Prompt {
                contract: "unknown",
                message: error.to_string(),
            })?;
        }
        if self.contract.form == XmlElementFormKind::Text {
            node.push(MixedContent::text(TextNode::new("...")));
        }
        Ok(node)
    }

    fn open(
        &mut self,
        state: &State,
        attempt: StreamingToolAttemptId,
        sequence: u64,
        occurrence: XmlOccurrenceId,
        attributes: Vec<(String, String)>,
        span: XmlSourceSpan,
    ) -> ElementOpenDispatch<C> {
        let mut raw = HashMap::<&str, &str>::with_capacity(attributes.len());
        for (name, value) in &attributes {
            raw.insert(name.as_str(), value.as_str());
        }
        let mut problems = Vec::new();
        for name in raw.keys() {
            if !self
                .contract
                .attributes
                .iter()
                .any(|attribute| attribute.name == *name)
            {
                problems.push(ElementContractProblem {
                    attribute: None,
                    kind: XmlContractViolationKind::UnknownAttribute,
                    detail: "unknown_attribute",
                });
            }
        }
        for attribute in &self.contract.attributes {
            if attribute.required && !raw.contains_key(attribute.name) {
                problems.push(ElementContractProblem {
                    attribute: Some(attribute.name),
                    kind: XmlContractViolationKind::MissingAttribute,
                    detail: "missing_attribute",
                });
            }
        }
        if !problems.is_empty() {
            return ElementOpenDispatch::Contract(problems);
        }

        let mut decoded = Vec::with_capacity(self.contract.attributes.len());
        for attribute in &self.contract.attributes {
            let value = match raw.get(attribute.name) {
                Some(value) => match attribute.decode(value) {
                    Ok(value) => Some(value),
                    Err(error) => return ElementOpenDispatch::Decode(error),
                },
                None => None,
            };
            decoded.push((attribute.name, value));
        }
        let head = match (self.contract.decode_open)(XmlDecodedAttributes::from_values(decoded)) {
            Ok(head) => Arc::new(head),
            Err(error) => return ElementOpenDispatch::Decode(error),
        };
        let event = XmlOpen {
            attempt,
            sequence,
            occurrence,
            head: Arc::clone(&head),
            span,
        };
        let mut update = StreamingToolUpdate::none();
        for reduce in &self.handlers.open {
            update = append_update(
                update,
                reduce(
                    state,
                    XmlOpen {
                        attempt: event.attempt,
                        sequence: event.sequence,
                        occurrence: event.occurrence,
                        head: Arc::clone(&event.head),
                        span: event.span,
                    },
                ),
            );
        }
        if self
            .occurrences
            .insert(
                occurrence,
                TypedOccurrence {
                    head,
                    text: String::new(),
                },
            )
            .is_some()
        {
            return ElementOpenDispatch::Invariant(ElementInvariant {
                message: "parser reused XML occurrence identity",
            });
        }
        ElementOpenDispatch::Opened(update)
    }

    fn delta(
        &mut self,
        state: &State,
        attempt: StreamingToolAttemptId,
        sequence: u64,
        occurrence: XmlOccurrenceId,
        delta: String,
        accumulated: Arc<str>,
        span: XmlSourceSpan,
    ) -> Result<StreamingToolUpdate<C>, ElementInvariant> {
        let Some(occurrence_state) = self.occurrences.get_mut(&occurrence) else {
            return Err(ElementInvariant {
                message: "parser emitted a delta for an unopened XML occurrence",
            });
        };
        occurrence_state.text.push_str(&delta);
        let mut update = StreamingToolUpdate::none();
        for reduce in &self.handlers.delta {
            update = append_update(
                update,
                reduce(
                    state,
                    XmlTextDelta {
                        attempt,
                        sequence,
                        occurrence,
                        head: Arc::clone(&occurrence_state.head),
                        delta: delta.clone(),
                        accumulated: Arc::clone(&accumulated),
                        span,
                    },
                ),
            );
        }
        Ok(update)
    }

    fn complete(
        &mut self,
        state: &mut State,
        attempt: StreamingToolAttemptId,
        sequence: u64,
        occurrence: XmlOccurrenceId,
        form: XmlElementForm,
        span: XmlSourceSpan,
    ) -> ElementCompleteDispatch<C> {
        let Some(occurrence_state) = self.occurrences.remove(&occurrence) else {
            return ElementCompleteDispatch::Invariant(ElementInvariant {
                message: "parser emitted completion for an unopened XML occurrence",
            });
        };
        let value =
            match (self.contract.decode_complete)(&occurrence_state.head, &occurrence_state.text) {
                Ok(value) => value,
                Err(error) => return ElementCompleteDispatch::Decode(error),
            };
        let complete = XmlComplete {
            attempt,
            sequence,
            occurrence,
            head: occurrence_state.head,
            value,
            form,
            span,
        };
        let Some(reduce) = &self.handlers.complete else {
            return ElementCompleteDispatch::Invariant(ElementInvariant {
                message: "element completion handler was not configured",
            });
        };
        match reduce(state, complete) {
            CompleteDispatch::Valid(update) => ElementCompleteDispatch::Completed(update),
            CompleteDispatch::Invalid(rejection) => {
                ElementCompleteDispatch::DomainInvalid(rejection.into_diagnostics())
            }
        }
    }

    fn discard(&mut self, occurrence: XmlOccurrenceId) {
        self.occurrences.remove(&occurrence);
    }
}

fn append_update<C: StreamingToolChannels>(
    left: StreamingToolUpdate<C>,
    right: StreamingToolUpdate<C>,
) -> StreamingToolUpdate<C> {
    let mut entries = left.into_entries();
    entries.extend(right.into_entries());
    StreamingToolUpdate::from_entries(entries)
}

/// Builds one independent parser/reducer/effect execution from a validated
/// declaration. It is generic only until the declaration starts; the
/// Application owns it through [`ErasedContract`].
pub(super) struct ContractMachine<C, State, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    start: StreamingToolAttemptStart,
    parser: Parser,
    state: Option<State>,
    elements: Vec<Box<dyn ErasedElement<C, State>>>,
    element_indexes: HashMap<&'static str, usize>,
    occurrence_indexes: HashMap<XmlOccurrenceId, usize>,
    invalid_occurrences: HashSet<XmlOccurrenceId>,
    summaries: Vec<XmlElementSummary>,
    diagnostics: Vec<StreamingToolDiagnosticRecord<C::Diagnostic>>,
    finish_reducer: Option<FinalReducer<C, State>>,
    rejection_handler: Option<RejectionHandler<C::Diagnostic>>,
    pending_rejection: Option<super::RejectedStreamingToolAttempt<C::Diagnostic>>,
    effects: ManagedEffects<C, R, P>,
    cancellation: Option<Arc<AtomicBool>>,
    sealed: bool,
    blocked: bool,
    finished: bool,
    accepted: bool,
    reaction_requested: bool,
}

impl<C, State, R, P> ContractMachine<C, State, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        start: StreamingToolAttemptStart,
        envelope: XmlEnvelope,
        ignore_unknown_elements: bool,
        state: State,
        elements: Vec<Box<dyn ErasedElement<C, State>>>,
        finish_reducer: FinalReducer<C, State>,
        rejection_handler: Option<RejectionHandler<C::Diagnostic>>,
        live_factory: Option<LiveRuntimeFactory<R>>,
        publisher_factory: Option<PublisherFactory<P>>,
    ) -> Result<Self, StreamingToolDriverFault> {
        let parser_elements = elements
            .iter()
            .map(|element| ElementSpec {
                name: element.name(),
                form: element.form(),
            })
            .collect();
        let mut parser_contract = ParserContract::strict(parser_elements);
        parser_contract.allow_implicit_text_eof = envelope.allows_unclosed_text_at_eof();
        if ignore_unknown_elements {
            parser_contract.unknown_top_level = UnknownTopLevel::IgnoreSubtree;
        }
        let parser = Parser::new(parser_contract).map_err(|fault| {
            StreamingToolDriverFault::Declaration(map_parser_declaration_fault(
                start.contract_identity,
                fault,
            ))
        })?;
        let mut element_indexes = HashMap::with_capacity(elements.len());
        let summaries = elements
            .iter()
            .enumerate()
            .map(|(index, element)| {
                element_indexes.insert(element.name(), index);
                XmlElementSummary {
                    contract_index: index,
                    name: element.name(),
                    seen: 0,
                    opened: 0,
                    decoded: 0,
                    completed: 0,
                    implicit_eof: 0,
                }
            })
            .collect();
        Ok(Self {
            start,
            parser,
            state: Some(state),
            elements,
            element_indexes,
            occurrence_indexes: HashMap::new(),
            invalid_occurrences: HashSet::new(),
            summaries,
            diagnostics: Vec::new(),
            finish_reducer: Some(finish_reducer),
            rejection_handler,
            pending_rejection: None,
            effects: ManagedEffects::new(start, live_factory, publisher_factory),
            cancellation: None,
            sealed: false,
            blocked: false,
            finished: false,
            accepted: false,
            reaction_requested: false,
        })
    }

    fn push_inner(&mut self, text: &str) -> Result<Vec<ParserEvent>, StreamingToolDriverFault> {
        if self.blocked {
            return Err(self.recovery_fault());
        }
        if self.sealed {
            return Err(StreamingToolDriverFault::Input {
                contract: self.start.contract_identity,
                message: "received text after the selected output was sealed".to_owned(),
            });
        }
        self.parser
            .push(text)
            .map_err(|fault| self.parser_fault(fault))
    }

    fn parser_fault(&self, fault: ParserFault) -> StreamingToolDriverFault {
        let message = fault.to_string();
        match fault {
            ParserFault::InputLimitExceeded { .. }
            | ParserFault::DepthLimitExceeded { .. }
            | ParserFault::AttributeLimitExceeded { .. }
            | ParserFault::ContentLimitExceeded { .. }
            | ParserFault::OccurrenceLimitExceeded { .. } => StreamingToolDriverFault::Limit {
                contract: self.start.contract_identity,
                message,
            },
            ParserFault::AfterFinish | ParserFault::Aborted => StreamingToolDriverFault::Input {
                contract: self.start.contract_identity,
                message,
            },
        }
    }

    fn next_sequence(&mut self) -> Result<u64, StreamingToolDriverFault> {
        self.effects
            .allocate_sequence()
            .map_err(|_| StreamingToolDriverFault::Reducer {
                contract: self.start.contract_identity,
                message: "attempt event sequence overflowed".to_owned(),
            })
    }

    fn record_diagnostic(
        &mut self,
        origin: StreamingToolDiagnosticOrigin,
        diagnostic: StreamingToolDiagnostic<C::Diagnostic>,
    ) -> Result<(), StreamingToolDriverFault> {
        let sequence = self.next_sequence()?;
        self.record_diagnostic_at(sequence, origin, diagnostic);
        Ok(())
    }

    fn record_diagnostic_at(
        &mut self,
        sequence: u64,
        origin: StreamingToolDiagnosticOrigin,
        diagnostic: StreamingToolDiagnostic<C::Diagnostic>,
    ) {
        self.diagnostics.push(StreamingToolDiagnosticRecord {
            sequence,
            origin,
            diagnostic,
        });
    }

    fn record_contract(
        &mut self,
        occurrence: Option<XmlOccurrenceId>,
        span: Option<XmlSourceSpan>,
        element: Option<&'static str>,
        attribute: Option<&'static str>,
        kind: XmlContractViolationKind,
        detail: &'static str,
    ) -> Result<(), StreamingToolDriverFault> {
        self.record_diagnostic(
            StreamingToolDiagnosticOrigin::Parser { occurrence, span },
            StreamingToolDiagnostic::Contract(XmlContractViolation::new(
                self.start.contract_identity,
                element,
                attribute,
                kind,
                detail,
            )),
        )
    }

    fn record_decode(
        &mut self,
        occurrence: XmlOccurrenceId,
        span: XmlSourceSpan,
        violation: XmlDecodeViolation,
    ) -> Result<(), StreamingToolDriverFault> {
        match violation {
            XmlDecodeViolation::Model { .. } => self.record_diagnostic(
                StreamingToolDiagnosticOrigin::Parser {
                    occurrence: Some(occurrence),
                    span: Some(span),
                },
                StreamingToolDiagnostic::Decode(violation),
            ),
            XmlDecodeViolation::AttributeAccess(access) => Err(StreamingToolDriverFault::Reducer {
                contract: self.start.contract_identity,
                message: format!("attribute decoder invariant: {}", access.code()),
            }),
        }
    }

    async fn process_events(
        &mut self,
        events: Vec<ParserEvent>,
    ) -> Result<(), StreamingToolDriverFault> {
        for event in events {
            if self.blocked {
                return Ok(());
            }
            self.process_event(event).await?;
        }
        Ok(())
    }

    async fn process_event(&mut self, event: ParserEvent) -> Result<(), StreamingToolDriverFault> {
        let event_sequence = self.next_sequence()?;
        match event {
            ParserEvent::Open {
                occurrence,
                name,
                attributes,
                span,
                self_closing: _,
            } => {
                let occurrence = XmlOccurrenceId::new(occurrence);
                let Some(index) = self.element_indexes.get(name.as_str()).copied() else {
                    return Err(StreamingToolDriverFault::Reducer {
                        contract: self.start.contract_identity,
                        message: "parser opened an element absent from its grammar".to_owned(),
                    });
                };
                self.register_seen(occurrence, index)?;
                let span = XmlSourceSpan::from_range(span);
                if self.exceeds_maximum(index) {
                    self.record_contract(
                        Some(occurrence),
                        Some(span),
                        Some(self.summaries[index].name),
                        None,
                        XmlContractViolationKind::Cardinality,
                        "maximum_cardinality",
                    )?;
                    self.invalidate_occurrence(occurrence).await?;
                    return Ok(());
                }
                let state = self.state.as_ref().ok_or_else(|| self.inactive_fault())?;
                let dispatch = self.elements[index].open(
                    state,
                    self.start.attempt,
                    event_sequence,
                    occurrence,
                    attributes,
                    span,
                );
                match dispatch {
                    ElementOpenDispatch::Opened(update) => {
                        self.summaries[index].opened += 1;
                        self.apply_update(
                            StreamingEmissionOrigin::Xml {
                                event_sequence,
                                occurrence,
                            },
                            update,
                        )
                        .await?;
                    }
                    ElementOpenDispatch::Contract(problems) => {
                        for problem in problems {
                            self.record_contract(
                                Some(occurrence),
                                Some(span),
                                Some(self.summaries[index].name),
                                problem.attribute,
                                problem.kind,
                                problem.detail,
                            )?;
                        }
                        self.invalidate_occurrence(occurrence).await?;
                    }
                    ElementOpenDispatch::Decode(violation) => {
                        self.record_decode(occurrence, span, violation)?;
                        self.invalidate_occurrence(occurrence).await?;
                    }
                    ElementOpenDispatch::Invariant(fault) => return Err(self.element_fault(fault)),
                }
            }
            ParserEvent::Delta {
                occurrence,
                delta,
                accumulated,
                span,
            } => {
                let occurrence = XmlOccurrenceId::new(occurrence);
                if self.invalid_occurrences.contains(&occurrence) {
                    return Ok(());
                }
                let index = self.occurrence_index(occurrence)?;
                let state = self.state.as_ref().ok_or_else(|| self.inactive_fault())?;
                let update = self.elements[index]
                    .delta(
                        state,
                        self.start.attempt,
                        event_sequence,
                        occurrence,
                        delta,
                        accumulated,
                        XmlSourceSpan::from_range(span),
                    )
                    .map_err(|fault| self.element_fault(fault))?;
                self.apply_update(
                    StreamingEmissionOrigin::Xml {
                        event_sequence,
                        occurrence,
                    },
                    update,
                )
                .await?;
            }
            ParserEvent::Complete {
                occurrence,
                form,
                span,
            } => {
                let occurrence = XmlOccurrenceId::new(occurrence);
                if self.invalid_occurrences.contains(&occurrence) {
                    if let Some(index) = self.occurrence_indexes.get(&occurrence).copied() {
                        self.elements[index].discard(occurrence);
                    }
                    return Ok(());
                }
                let index = self.occurrence_index(occurrence)?;
                let form = match form {
                    CompletionForm::SelfClosing => XmlElementForm::SelfClosing,
                    CompletionForm::ExplicitClose => XmlElementForm::ExplicitClose,
                    CompletionForm::ImplicitEof => XmlElementForm::ImplicitEof,
                };
                let span = XmlSourceSpan::from_range(span);
                let inactive = StreamingToolDriverFault::Inactive {
                    contract: self.start.contract_identity,
                };
                let state = self.state.as_mut().ok_or(inactive)?;
                let dispatch = self.elements[index].complete(
                    state,
                    self.start.attempt,
                    event_sequence,
                    occurrence,
                    form,
                    span,
                );
                match dispatch {
                    ElementCompleteDispatch::Completed(update) => {
                        self.summaries[index].decoded += 1;
                        self.summaries[index].completed += 1;
                        if form == XmlElementForm::ImplicitEof {
                            self.summaries[index].implicit_eof += 1;
                        }
                        self.apply_update(
                            StreamingEmissionOrigin::Xml {
                                event_sequence,
                                occurrence,
                            },
                            update,
                        )
                        .await?;
                    }
                    ElementCompleteDispatch::DomainInvalid(diagnostics) => {
                        self.summaries[index].decoded += 1;
                        for diagnostic in diagnostics {
                            self.record_diagnostic(
                                StreamingToolDiagnosticOrigin::Reducer(
                                    StreamingEmissionOrigin::Xml {
                                        event_sequence,
                                        occurrence,
                                    },
                                ),
                                StreamingToolDiagnostic::Domain(diagnostic),
                            )?;
                        }
                        self.invalidate_occurrence(occurrence).await?;
                    }
                    ElementCompleteDispatch::Decode(violation) => {
                        self.record_decode(occurrence, span, violation)?;
                        self.invalidate_occurrence(occurrence).await?;
                    }
                    ElementCompleteDispatch::Invariant(fault) => {
                        return Err(self.element_fault(fault))
                    }
                }
            }
            ParserEvent::Invalid {
                occurrence,
                element,
                kind,
                span,
            } => {
                let occurrence = occurrence.map(XmlOccurrenceId::new);
                let element_index = element
                    .as_deref()
                    .and_then(|name| self.element_indexes.get(name).copied())
                    .or_else(|| {
                        occurrence.and_then(|occurrence| {
                            self.occurrence_indexes.get(&occurrence).copied()
                        })
                    });
                if let (Some(occurrence), Some(index)) = (occurrence, element_index) {
                    if !self.occurrence_indexes.contains_key(&occurrence) {
                        self.register_seen(occurrence, index)?;
                    }
                }
                let element_name = element_index.map(|index| self.summaries[index].name);
                let span = span.map(XmlSourceSpan::from_range);
                let (kind, detail) = map_invalid_kind(kind);
                self.record_contract(occurrence, span, element_name, None, kind, detail)?;
                if let Some(occurrence) = occurrence {
                    self.invalidate_occurrence(occurrence).await?;
                }
            }
        }
        Ok(())
    }

    fn register_seen(
        &mut self,
        occurrence: XmlOccurrenceId,
        index: usize,
    ) -> Result<(), StreamingToolDriverFault> {
        if self.occurrence_indexes.insert(occurrence, index).is_none() {
            let summary =
                self.summaries
                    .get_mut(index)
                    .ok_or_else(|| StreamingToolDriverFault::Reducer {
                        contract: self.start.contract_identity,
                        message: "parser selected an invalid element summary".to_owned(),
                    })?;
            summary.seen =
                summary
                    .seen
                    .checked_add(1)
                    .ok_or_else(|| StreamingToolDriverFault::Reducer {
                        contract: self.start.contract_identity,
                        message: "element occurrence count overflowed".to_owned(),
                    })?;
        }
        Ok(())
    }

    fn occurrence_index(
        &self,
        occurrence: XmlOccurrenceId,
    ) -> Result<usize, StreamingToolDriverFault> {
        self.occurrence_indexes
            .get(&occurrence)
            .copied()
            .ok_or_else(|| StreamingToolDriverFault::Reducer {
                contract: self.start.contract_identity,
                message: "parser event did not identify its declared occurrence".to_owned(),
            })
    }

    fn exceeds_maximum(&self, index: usize) -> bool {
        self.summaries[index].name.is_empty()
            || self.summaries[index].seen
                > self.elements[index]
                    .cardinality()
                    .max()
                    .unwrap_or(usize::MAX)
    }

    async fn invalidate_occurrence(
        &mut self,
        occurrence: XmlOccurrenceId,
    ) -> Result<(), StreamingToolDriverFault> {
        if !self.invalid_occurrences.insert(occurrence) {
            return Ok(());
        }
        if let Some(index) = self.occurrence_indexes.get(&occurrence).copied() {
            self.elements[index].discard(occurrence);
        }
        let status = self
            .effects
            .invalidate_occurrence(occurrence)
            .await
            .map_err(|fault| self.effects_fault(fault))?;
        if status == ManagedEffectsStatus::RecoveryRequired {
            self.blocked = true;
        }
        Ok(())
    }

    async fn apply_update(
        &mut self,
        origin: StreamingEmissionOrigin,
        update: StreamingToolUpdate<C>,
    ) -> Result<(), StreamingToolDriverFault> {
        let effect_update = self
            .effects
            .handle_update(origin.clone(), update)
            .await
            .map_err(|fault| self.effects_fault(fault))?;
        self.record_effect_diagnostics(origin, effect_update)?;
        Ok(())
    }

    fn record_effect_diagnostics(
        &mut self,
        origin: StreamingEmissionOrigin,
        update: EffectsUpdate<C::Diagnostic>,
    ) -> Result<(), StreamingToolDriverFault> {
        let status = update.status();
        for diagnostic in update.into_diagnostics() {
            self.record_diagnostic_at(
                diagnostic.sequence,
                StreamingToolDiagnosticOrigin::Reducer(origin.clone()),
                StreamingToolDiagnostic::Domain(diagnostic.diagnostic),
            );
        }
        if status == ManagedEffectsStatus::RecoveryRequired {
            self.blocked = true;
        }
        Ok(())
    }

    fn check_minimum_cardinality(&mut self) -> Result<(), StreamingToolDriverFault> {
        for index in 0..self.summaries.len() {
            let required = self.elements[index].cardinality().min();
            if self.summaries[index].seen < required {
                self.record_contract(
                    None,
                    None,
                    Some(self.summaries[index].name),
                    None,
                    XmlContractViolationKind::Cardinality,
                    "minimum_cardinality",
                )?;
            }
        }
        Ok(())
    }

    async fn finish_inner(&mut self) -> Result<(), StreamingToolDriverFault> {
        if self.finished {
            return Err(StreamingToolDriverFault::Input {
                contract: self.start.contract_identity,
                message: "normal EOF was already processed".to_owned(),
            });
        }
        if self.blocked {
            return Ok(());
        }
        let events = self
            .parser
            .finish_normal()
            .map_err(|fault| self.parser_fault(fault))?;
        self.process_events(events).await?;
        if self.blocked {
            return Ok(());
        }
        self.check_minimum_cardinality()?;
        let raw_output = self.parser.raw_output().to_owned();
        let state = self.state.take().ok_or_else(|| self.inactive_fault())?;
        let finish = self
            .finish_reducer
            .take()
            .ok_or_else(|| self.inactive_fault())?;
        let decision = finish(
            state,
            XmlAttemptSummary {
                attempt: self.start.attempt,
                raw_output: &raw_output,
                elements: &self.summaries,
                diagnostics: &self.diagnostics,
            },
        );
        match decision {
            StreamingToolDecision::Accept(update) => {
                self.apply_update(StreamingEmissionOrigin::Finish, update)
                    .await?;
                if self.blocked {
                    return Ok(());
                }
                let diagnostics = std::mem::take(&mut self.diagnostics);
                let status = self
                    .effects
                    .accept(raw_output, diagnostics)
                    .await
                    .map_err(|fault| self.effects_fault(fault))?;
                self.accepted = status == ManagedEffectsStatus::Accepted;
                self.finished = status != ManagedEffectsStatus::RecoveryRequired;
                self.blocked = status == ManagedEffectsStatus::RecoveryRequired;
            }
            StreamingToolDecision::Reject(rejection) => {
                for diagnostic in rejection.into_diagnostics() {
                    self.record_diagnostic(
                        StreamingToolDiagnosticOrigin::Reducer(StreamingEmissionOrigin::Finish),
                        StreamingToolDiagnostic::Domain(diagnostic),
                    )?;
                }
                let status = self
                    .effects
                    .abort(StreamingToolAbortCause::Rejected)
                    .await
                    .map_err(|fault| self.effects_fault(fault))?;
                self.pending_rejection = Some(super::RejectedStreamingToolAttempt {
                    context: StreamingToolAttemptContext::from_start(&self.start),
                    raw_output,
                    diagnostics: std::mem::take(&mut self.diagnostics),
                });
                self.blocked = status == ManagedEffectsStatus::RecoveryRequired;
                if !self.blocked {
                    self.run_rejection_if_ready().await?;
                    self.finished = true;
                }
            }
        }
        Ok(())
    }

    async fn run_rejection_if_ready(&mut self) -> Result<(), StreamingToolDriverFault> {
        let Some(report) = self.pending_rejection.take() else {
            return Ok(());
        };
        if self.cancelled() {
            return Ok(());
        }
        let Some(handler) = &self.rejection_handler else {
            return Ok(());
        };
        let action = std::panic::AssertUnwindSafe(async { handler(report).await })
            .catch_unwind()
            .await
            .map_err(|_| StreamingToolDriverFault::Adapter {
                contract: self.start.contract_identity,
                message: "rejection handler panicked".to_owned(),
            })?
            .map_err(|message| StreamingToolDriverFault::Adapter {
                contract: self.start.contract_identity,
                message,
            })?;
        if !self.cancelled() && action == StreamingToolRejectionAction::RequestReaction {
            self.reaction_requested = true;
        }
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn effects_fault(&self, fault: ManagedEffectsFault) -> StreamingToolDriverFault {
        StreamingToolDriverFault::Adapter {
            contract: self.start.contract_identity,
            message: format!("{fault:?}"),
        }
    }

    fn element_fault(&self, fault: ElementInvariant) -> StreamingToolDriverFault {
        StreamingToolDriverFault::Reducer {
            contract: self.start.contract_identity,
            message: fault.message.to_owned(),
        }
    }

    fn inactive_fault(&self) -> StreamingToolDriverFault {
        StreamingToolDriverFault::Inactive {
            contract: self.start.contract_identity,
        }
    }

    fn recovery_fault(&self) -> StreamingToolDriverFault {
        StreamingToolDriverFault::RecoveryRequired {
            contract: self.start.contract_identity,
            message: "managed effects retain unresolved evidence".to_owned(),
        }
    }
}

#[async_trait]
impl<C, State, R, P> ErasedContract for ContractMachine<C, State, R, P>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    R: LiveEffectRuntime<C::Live>,
    P: StreamingToolAttemptPublisher<C>,
{
    fn identity(&self) -> &'static str {
        self.start.contract_identity
    }

    fn attempt(&self) -> StreamingToolAttemptId {
        self.start.attempt
    }

    fn set_cancellation(&mut self, cancelled: Arc<AtomicBool>) {
        self.cancellation = Some(cancelled);
    }

    async fn push(&mut self, text: &str) -> Result<(), StreamingToolDriverFault> {
        let events = self.push_inner(text)?;
        self.process_events(events).await
    }

    async fn seal(&mut self, text: &str) -> Result<(), StreamingToolDriverFault> {
        if self.sealed {
            return Err(StreamingToolDriverFault::Input {
                contract: self.start.contract_identity,
                message: "selected output was sealed more than once".to_owned(),
            });
        }
        let observed = self.parser.raw_output();
        let Some(suffix) = text.strip_prefix(observed) else {
            return Err(StreamingToolDriverFault::Input {
                contract: self.start.contract_identity,
                message: "sealed text did not extend observed XML text".to_owned(),
            });
        };
        let events = self.push_inner(suffix)?;
        self.process_events(events).await?;
        self.sealed = true;
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), StreamingToolDriverFault> {
        self.finish_inner().await
    }

    async fn abort(
        &mut self,
        cause: StreamingToolAbortCause,
    ) -> Result<(), StreamingToolDriverFault> {
        self.parser.abort();
        let status = self
            .effects
            .abort(cause)
            .await
            .map_err(|fault| self.effects_fault(fault))?;
        self.blocked = status == ManagedEffectsStatus::RecoveryRequired;
        self.finished = !self.blocked;
        Ok(())
    }

    async fn recover(&mut self) -> Result<(), StreamingToolDriverFault> {
        let status = match self.effects.recover().await {
            // `recover` is only a cleanup operation. A contract that never
            // reached normal EOF may still have applied Live receipts even
            // though they do not themselves require resolution yet; never
            // mark that open parser as complete without rolling them back.
            ManagedEffectsStatus::Active => {
                self.parser.abort();
                self.effects
                    .abort(StreamingToolAbortCause::RuntimeFault)
                    .await
                    .map_err(|fault| self.effects_fault(fault))?
            }
            status => status,
        };
        self.blocked = status == ManagedEffectsStatus::RecoveryRequired;
        if self.blocked {
            return Ok(());
        }
        if self.pending_rejection.is_some() {
            self.run_rejection_if_ready().await?;
        }
        self.accepted = status == ManagedEffectsStatus::Accepted;
        self.finished = true;
        Ok(())
    }

    fn needs_recovery(&self) -> bool {
        self.blocked || !self.effects.is_clear()
    }

    fn terminal_fault(&self) -> Option<StreamingToolDriverFault> {
        let cause = self.effects.abort_cause()?;
        (cause != StreamingToolAbortCause::Rejected).then_some(StreamingToolDriverFault::Aborted {
            contract: self.start.contract_identity,
            cause,
        })
    }

    fn reaction_requested(&self) -> bool {
        self.reaction_requested && !self.cancelled()
    }

    fn completed(&self) -> bool {
        self.finished && !self.needs_recovery()
    }

    fn accepted(&self) -> bool {
        self.accepted
    }
}

pub(super) fn validate_elements<C, State>(
    identity: &'static str,
    elements: &[Box<dyn ErasedElement<C, State>>],
) -> Result<Document, StreamingToolDeclarationFault>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    if !is_contract_identity(identity) {
        return Err(StreamingToolDeclarationFault::InvalidIdentity {
            contract: identity,
            detail: "expected a non-empty ASCII token".to_owned(),
        });
    }
    let mut names = HashSet::new();
    let mut children = BlockChildren::new();
    for element in elements {
        element.validate_declaration(identity)?;
        if !names.insert(element.name()) {
            return Err(StreamingToolDeclarationFault::DuplicateElement {
                contract: identity,
                element: element.name(),
            });
        }
        let node = element.prompt_node()?;
        children.push(BlockContent::xml(node));
    }
    Ok(Document::new(children))
}

fn map_parser_declaration_fault(
    identity: &'static str,
    fault: ParserDeclarationFault,
) -> StreamingToolDeclarationFault {
    match fault {
        ParserDeclarationFault::InvalidElementName { name } => {
            StreamingToolDeclarationFault::InvalidElementName {
                contract: identity,
                element: name,
                detail: "expected an unqualified ASCII XML name".to_owned(),
            }
        }
        ParserDeclarationFault::DuplicateElement { name } => {
            StreamingToolDeclarationFault::DuplicateElement {
                contract: identity,
                element: name,
            }
        }
        ParserDeclarationFault::InvalidLimit { limit } => StreamingToolDeclarationFault::Prompt {
            contract: identity,
            message: format!("invalid parser limit `{limit}`"),
        },
    }
}

fn map_invalid_kind(kind: InvalidKind) -> (XmlContractViolationKind, &'static str) {
    match kind {
        InvalidKind::MalformedMarkup
        | InvalidKind::MismatchedClose
        | InvalidKind::InvalidEntity => (XmlContractViolationKind::Malformed, "malformed_markup"),
        InvalidKind::UnknownElement => {
            (XmlContractViolationKind::UnknownElement, "unknown_element")
        }
        InvalidKind::UnknownNamespace => (
            XmlContractViolationKind::UnknownNamespace,
            "unknown_namespace",
        ),
        InvalidKind::DuplicateAttribute => (
            XmlContractViolationKind::DuplicateAttribute,
            "duplicate_attribute",
        ),
        InvalidKind::WrongElementForm => (
            XmlContractViolationKind::WrongElementForm,
            "wrong_element_form",
        ),
        InvalidKind::NestedMarkup => (XmlContractViolationKind::NestedMarkup, "nested_markup"),
        InvalidKind::TextOutsideEnvelope => (
            XmlContractViolationKind::TextOutsideEnvelope,
            "text_outside_envelope",
        ),
        InvalidKind::IncompleteElement => {
            (XmlContractViolationKind::Incomplete, "incomplete_element")
        }
    }
}

fn is_xml_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn is_contract_identity(identity: &str) -> bool {
    !identity.is_empty()
        && identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::component::authoring::streaming_attempt::{
        AcceptedStreamingToolAttempt, LiveApplyOutcome, LiveApplyResolution, LiveConfirmContext,
        LiveEffectContext, LiveEffectRuntime, LiveRecoveryContext, LiveRollbackContext,
        LiveSettleOutcome, LiveSettleResolution, LiveSettlementContext, NoLiveRuntime, NoPublisher,
        NoStreamingValue, StagedStreamingToolValue, StreamingPublishContext,
        StreamingPublishOutcome, StreamingPublishRecoveryContext, StreamingPublishResolution,
        StreamingToolAttemptPublisher, StreamingToolDecision, StreamingToolUpdate, XmlToolElement,
    };
    use async_trait::async_trait;

    struct TestChannels;

    impl StreamingToolChannels for TestChannels {
        type Output = NoStreamingValue;
        type Live = NoStreamingValue;
        type Commit = NoStreamingValue;
        type Diagnostic = &'static str;
    }

    fn text_element(
        observed: Arc<Mutex<Vec<String>>>,
    ) -> Box<dyn ErasedElement<TestChannels, Arc<Mutex<Vec<String>>>>> {
        let contract = XmlToolElement::text("say").decode(
            |_| Ok::<_, XmlDecodeViolation>(()),
            |_, text| Ok::<_, XmlDecodeViolation>(text.to_owned()),
        );
        let handlers: XmlElementHandlers<_, TestChannels, (), String> = XmlElementHandlers::new();
        erase_element(
            contract,
            handlers.on_complete(move |_state, complete| {
                observed.lock().unwrap().push(complete.value);
                StreamingToolUpdate::none()
            }),
        )
    }

    fn machine(
        observed: Arc<Mutex<Vec<String>>>,
        ignore_unknown_elements: bool,
    ) -> ContractMachine<TestChannels, Arc<Mutex<Vec<String>>>, NoLiveRuntime, NoPublisher> {
        let state = Arc::clone(&observed);
        ContractMachine::new(
            StreamingToolAttemptStart::new(StreamingToolAttemptId::new(1), "test", "v1"),
            XmlEnvelope::strict_fragment(),
            ignore_unknown_elements,
            state,
            vec![text_element(observed)],
            Box::new(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none())),
            None,
            None,
            None,
        )
        .unwrap()
    }

    struct SequenceChannels;

    impl StreamingToolChannels for SequenceChannels {
        type Output = &'static str;
        type Live = &'static str;
        type Commit = &'static str;
        type Diagnostic = &'static str;
    }

    #[derive(Default)]
    struct SequenceCapture {
        event_sequences: Vec<u64>,
        diagnostic_sequences: Vec<u64>,
        live_sequences: Vec<u64>,
        published_entries: Vec<(u64, &'static str)>,
    }

    struct SequenceRuntime(Arc<Mutex<SequenceCapture>>);

    #[async_trait]
    impl LiveEffectRuntime<&'static str> for SequenceRuntime {
        type Receipt = ();
        type ApplyOperation = &'static str;
        type SettlementOperation = ();
        type Error = Infallible;

        fn prepare_apply(
            &mut self,
            context: &LiveEffectContext,
            effect: &'static str,
        ) -> Result<Self::ApplyOperation, Self::Error> {
            self.0
                .lock()
                .unwrap()
                .live_sequences
                .push(context.emission_sequence());
            Ok(effect)
        }

        async fn apply(
            &mut self,
            _: &LiveEffectContext,
            _: &Self::ApplyOperation,
        ) -> LiveApplyOutcome<Self::Receipt, Self::Error> {
            LiveApplyOutcome::Applied(())
        }

        async fn resolve_apply(
            &mut self,
            _: &LiveRecoveryContext,
            _: &Self::ApplyOperation,
        ) -> Result<LiveApplyResolution<Self::Receipt>, Self::Error> {
            Ok(LiveApplyResolution::NotApplied)
        }

        fn prepare_settlement(
            &mut self,
            _: &LiveSettlementContext,
            _: &Self::Receipt,
        ) -> Result<Self::SettlementOperation, Self::Error> {
            Ok(())
        }

        async fn confirm(
            &mut self,
            _: &LiveConfirmContext,
            _: &Self::Receipt,
            _: &Self::SettlementOperation,
        ) -> LiveSettleOutcome<Self::Error> {
            LiveSettleOutcome::Settled
        }

        async fn rollback(
            &mut self,
            _: &LiveRollbackContext,
            _: &Self::Receipt,
            _: &Self::SettlementOperation,
        ) -> LiveSettleOutcome<Self::Error> {
            LiveSettleOutcome::Settled
        }

        async fn resolve_settlement(
            &mut self,
            _: &LiveRecoveryContext,
            _: &Self::Receipt,
            _: &Self::SettlementOperation,
        ) -> Result<LiveSettleResolution, Self::Error> {
            Ok(LiveSettleResolution::Settled)
        }
    }

    struct SequencePublisher(Arc<Mutex<SequenceCapture>>);

    #[async_trait]
    impl StreamingToolAttemptPublisher<SequenceChannels> for SequencePublisher {
        type Published = ();
        type PublicationOperation = AcceptedStreamingToolAttempt<SequenceChannels>;
        type Error = Infallible;

        fn prepare(
            &mut self,
            _: &StreamingPublishContext,
            attempt: Self::PublicationOperation,
        ) -> Result<Self::PublicationOperation, Self::Error> {
            Ok(attempt)
        }

        async fn publish(
            &mut self,
            _: &StreamingPublishContext,
            operation: &Self::PublicationOperation,
        ) -> StreamingPublishOutcome<Self::Published, Self::Error> {
            let mut capture = self.0.lock().unwrap();
            for entry in &operation.entries {
                let value = match &entry.value {
                    StagedStreamingToolValue::Output(value)
                    | StagedStreamingToolValue::Commit(value) => *value,
                };
                capture.published_entries.push((entry.sequence, value));
            }
            StreamingPublishOutcome::Published(())
        }

        async fn resolve(
            &mut self,
            _: &StreamingPublishRecoveryContext,
            _: &Self::PublicationOperation,
        ) -> Result<StreamingPublishResolution<Self::Published>, Self::Error> {
            Ok(StreamingPublishResolution::Published(()))
        }
    }

    fn sequence_element() -> Box<dyn ErasedElement<SequenceChannels, Arc<Mutex<SequenceCapture>>>> {
        let contract = XmlToolElement::self_closing("say").decode(
            |_| Ok::<_, XmlDecodeViolation>(()),
            |_, _| Ok::<_, XmlDecodeViolation>(()),
        );
        let handlers: XmlElementHandlers<Arc<Mutex<SequenceCapture>>, SequenceChannels, (), ()> =
            XmlElementHandlers::new();
        erase_element(
            contract,
            handlers
                .on_open(|capture, event| {
                    capture.lock().unwrap().event_sequences.push(event.sequence);
                    StreamingToolUpdate::output("open-output")
                        .with_diagnostic("open-diagnostic")
                        .with_live("open-live")
                        .with_commit("open-commit")
                })
                .on_complete(|capture, event| {
                    capture.lock().unwrap().event_sequences.push(event.sequence);
                    StreamingToolUpdate::diagnostic("complete-diagnostic")
                }),
        )
    }

    #[tokio::test]
    async fn contract_sequences_events_diagnostics_and_emissions_in_one_authored_order() {
        let capture = Arc::new(Mutex::new(SequenceCapture::default()));
        let state = Arc::clone(&capture);
        let live_capture = Arc::clone(&capture);
        let publish_capture = Arc::clone(&capture);
        let mut contract = ContractMachine::new(
            StreamingToolAttemptStart::new(StreamingToolAttemptId::new(1), "sequence", "v1"),
            XmlEnvelope::strict_fragment(),
            false,
            state,
            vec![sequence_element()],
            Box::new(|capture, summary| {
                capture.lock().unwrap().diagnostic_sequences = summary
                    .diagnostics
                    .iter()
                    .map(|record| record.sequence)
                    .collect();
                StreamingToolDecision::Accept(StreamingToolUpdate::output("finish-output"))
            }),
            None,
            Some(Box::new(move |_| {
                Ok::<_, ManagedEffectsFault>(SequenceRuntime(Arc::clone(&live_capture)))
            })),
            Some(Box::new(move |_| {
                Ok::<_, ManagedEffectsFault>(SequencePublisher(Arc::clone(&publish_capture)))
            })),
        )
        .unwrap();

        contract.push("<say/>").await.unwrap();
        contract.seal("<say/>").await.unwrap();
        contract.finish().await.unwrap();

        let capture = capture.lock().unwrap();
        assert_eq!(capture.event_sequences, vec![0, 5]);
        assert_eq!(capture.diagnostic_sequences, vec![2, 6]);
        assert_eq!(capture.live_sequences, vec![3]);
        assert_eq!(
            capture.published_entries,
            vec![(1, "open-output"), (4, "open-commit"), (7, "finish-output")]
        );
    }

    #[tokio::test]
    async fn one_contract_shares_one_parser_across_its_declared_elements() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut contract = machine(Arc::clone(&observed), false);

        contract.push("<sa").await.unwrap();
        contract.push("y>hello &amp; goodbye</say>").await.unwrap();
        contract
            .seal("<say>hello &amp; goodbye</say>")
            .await
            .unwrap();
        contract.finish().await.unwrap();

        assert_eq!(*observed.lock().unwrap(), vec!["hello & goodbye"]);
        assert!(contract.completed());
        assert!(contract.accepted());
    }

    #[tokio::test]
    async fn unknown_top_level_policy_is_owned_by_each_contract() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut contract = machine(Arc::clone(&observed), true);

        contract
            .push("<think>foreign</think><say>visible</say>")
            .await
            .unwrap();
        contract
            .seal("<think>foreign</think><say>visible</say>")
            .await
            .unwrap();
        contract.finish().await.unwrap();

        assert_eq!(*observed.lock().unwrap(), vec!["visible"]);
        assert!(contract.accepted());
    }

    #[test]
    fn duplicate_element_names_are_rejected_inside_one_contract_only() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let elements = vec![text_element(Arc::clone(&observed)), text_element(observed)];

        let error = validate_elements("test", &elements).unwrap_err();
        assert!(matches!(
            error,
            StreamingToolDeclarationFault::DuplicateElement {
                contract: "test",
                element: "say",
            }
        ));
    }
}
