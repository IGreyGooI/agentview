//! Public values used by the strict streaming XML tool contract.
//!
//! This module deliberately contains no provider routing or effect execution.
//! It is the typed boundary shared by the declaration builder, parser reducer,
//! and managed-effect runtime.

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    fmt::{self, Display},
    marker::PhantomData,
    ops::Range,
    str::FromStr,
    sync::Arc,
};

/// Nominal lane types emitted by one streaming-tool contract.
pub trait StreamingToolChannels: Send + Sync + 'static {
    type Output: Send + 'static;
    type Live: Send + 'static;
    type Commit: Send + Sync + 'static;
    type Diagnostic: Send + Sync + 'static;
}

/// Sentinel for a deliberately disabled streaming-tool lane.
pub enum NoStreamingValue {}

/// One value emitted by a reducer.
pub enum StreamingToolEmission<C: StreamingToolChannels> {
    Output(C::Output),
    Live(C::Live),
    Commit(C::Commit),
    Diagnostic(C::Diagnostic),
}

/// Ordered values emitted by one reducer invocation.
#[must_use]
pub struct StreamingToolUpdate<C: StreamingToolChannels> {
    entries: Vec<StreamingToolEmission<C>>,
}

impl<C: StreamingToolChannels> StreamingToolUpdate<C> {
    pub fn none() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn output(value: C::Output) -> Self {
        Self {
            entries: vec![StreamingToolEmission::Output(value)],
        }
    }

    pub fn live(value: C::Live) -> Self {
        Self {
            entries: vec![StreamingToolEmission::Live(value)],
        }
    }

    pub fn commit(value: C::Commit) -> Self {
        Self {
            entries: vec![StreamingToolEmission::Commit(value)],
        }
    }

    pub fn diagnostic(value: C::Diagnostic) -> Self {
        Self {
            entries: vec![StreamingToolEmission::Diagnostic(value)],
        }
    }

    pub fn with_output(mut self, value: C::Output) -> Self {
        self.entries.push(StreamingToolEmission::Output(value));
        self
    }

    pub fn with_live(mut self, value: C::Live) -> Self {
        self.entries.push(StreamingToolEmission::Live(value));
        self
    }

    pub fn with_commit(mut self, value: C::Commit) -> Self {
        self.entries.push(StreamingToolEmission::Commit(value));
        self
    }

    pub fn with_diagnostic(mut self, value: C::Diagnostic) -> Self {
        self.entries.push(StreamingToolEmission::Diagnostic(value));
        self
    }

    pub(crate) fn into_entries(self) -> Vec<StreamingToolEmission<C>> {
        self.entries
    }

    pub(crate) fn from_entries(entries: Vec<StreamingToolEmission<C>>) -> Self {
        Self { entries }
    }
}

impl<C: StreamingToolChannels> Default for StreamingToolUpdate<C> {
    fn default() -> Self {
        Self::none()
    }
}

/// The parser event responsible for an emitted lane value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamingEmissionOrigin {
    Xml {
        event_sequence: u64,
        occurrence: XmlOccurrenceId,
    },
    Finish,
}

impl StreamingEmissionOrigin {
    pub(crate) const fn occurrence(&self) -> Option<XmlOccurrenceId> {
        match self {
            Self::Xml { occurrence, .. } => Some(*occurrence),
            Self::Finish => None,
        }
    }
}

/// An output or commit retained until accepted publication.
pub struct StagedStreamingToolEmission<C: StreamingToolChannels> {
    pub sequence: u64,
    pub origin: StreamingEmissionOrigin,
    pub value: StagedStreamingToolValue<C>,
}

impl<C: StreamingToolChannels> StagedStreamingToolEmission<C> {
    pub(crate) fn new(
        sequence: u64,
        origin: StreamingEmissionOrigin,
        value: StagedStreamingToolValue<C>,
    ) -> Self {
        Self {
            sequence,
            origin,
            value,
        }
    }

    pub(crate) fn origin(&self) -> &StreamingEmissionOrigin {
        &self.origin
    }
}

/// A staged terminal lane value.
pub enum StagedStreamingToolValue<C: StreamingToolChannels> {
    Output(C::Output),
    Commit(C::Commit),
}

/// A unique, process-local attempt identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamingToolAttemptId(u64);

impl StreamingToolAttemptId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// A unique occurrence identity within one attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct XmlOccurrenceId(u64);

impl XmlOccurrenceId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// A half-open byte span in the bounded raw model output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlSourceSpan {
    start: usize,
    end: usize,
}

impl XmlSourceSpan {
    pub(crate) const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub(crate) fn from_range(range: Range<usize>) -> Self {
        Self::new(range.start, range.end)
    }

    pub const fn start(&self) -> usize {
        self.start
    }

    pub const fn end(&self) -> usize {
        self.end
    }

    pub fn slice<'a>(&self, raw_output: &'a str) -> Option<&'a str> {
        raw_output.get(self.start..self.end)
    }
}

/// The lexical form used to complete an XML element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlElementForm {
    SelfClosing,
    ExplicitClose,
    ImplicitEof,
}

/// A successfully decoded element opening.
pub struct XmlOpen<Head> {
    pub attempt: StreamingToolAttemptId,
    pub sequence: u64,
    pub occurrence: XmlOccurrenceId,
    pub head: Arc<Head>,
    pub span: XmlSourceSpan,
}

/// One decoded text increment for an open text element.
pub struct XmlTextDelta<Head> {
    pub attempt: StreamingToolAttemptId,
    pub sequence: u64,
    pub occurrence: XmlOccurrenceId,
    pub head: Arc<Head>,
    pub delta: String,
    pub accumulated: Arc<str>,
    pub span: XmlSourceSpan,
}

/// A decoded element completion.
pub struct XmlComplete<Head, Value> {
    pub attempt: StreamingToolAttemptId,
    pub sequence: u64,
    pub occurrence: XmlOccurrenceId,
    pub head: Arc<Head>,
    pub value: Value,
    pub form: XmlElementForm,
    pub span: XmlSourceSpan,
}

/// Strict XML envelope policy for one contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlEnvelope {
    allow_unclosed_text_at_eof: bool,
}

impl XmlEnvelope {
    pub const fn strict_fragment() -> Self {
        Self {
            allow_unclosed_text_at_eof: false,
        }
    }

    pub const fn allow_unclosed_text_at_eof(mut self) -> Self {
        self.allow_unclosed_text_at_eof = true;
        self
    }

    pub const fn allows_unclosed_text_at_eof(&self) -> bool {
        self.allow_unclosed_text_at_eof
    }
}

impl Default for XmlEnvelope {
    fn default() -> Self {
        Self::strict_fragment()
    }
}

/// Entry point for declarative XML element schemas.
pub struct XmlToolElement;

/// Marker for a `<tag .../>` schema.
pub struct SelfClosing;

/// Marker for a `<tag ...>text</tag>` schema.
pub struct TextContent;

/// A schema with attributes and cardinality but no decode functions yet.
pub struct XmlElementDraft<Form> {
    pub(super) name: &'static str,
    pub(super) attributes: Vec<XmlAttributeDeclaration>,
    pub(super) cardinality: XmlCardinality,
    pub(super) marker: PhantomData<fn() -> Form>,
}

/// A complete typed element schema.
pub struct XmlElementContract<Head, Value> {
    pub(super) name: &'static str,
    pub(super) form: XmlElementFormKind,
    pub(super) attributes: Vec<XmlAttributeDeclaration>,
    pub(super) cardinality: XmlCardinality,
    pub(super) decode_open: DecodeOpen<Head>,
    pub(super) decode_complete: DecodeComplete<Head, Value>,
}

type DecodeOpen<Head> =
    Box<dyn Fn(XmlDecodedAttributes) -> Result<Head, XmlDecodeViolation> + Send + Sync>;
type DecodeComplete<Head, Value> =
    Box<dyn Fn(&Head, &str) -> Result<Value, XmlDecodeViolation> + Send + Sync>;

impl XmlToolElement {
    pub fn self_closing(name: &'static str) -> XmlElementDraft<SelfClosing> {
        XmlElementDraft {
            name,
            attributes: Vec::new(),
            cardinality: XmlCardinality::any(),
            marker: PhantomData,
        }
    }

    pub fn text(name: &'static str) -> XmlElementDraft<TextContent> {
        XmlElementDraft {
            name,
            attributes: Vec::new(),
            cardinality: XmlCardinality::any(),
            marker: PhantomData,
        }
    }
}

impl<Form: 'static> XmlElementDraft<Form> {
    pub fn required_attribute<T>(self, name: &'static str, prompt_example: &'static str) -> Self
    where
        T: FromStr + Send + Sync + 'static,
        T::Err: Display,
    {
        self.required_attribute_with(name, prompt_example, |value| {
            value.parse::<T>().map_err(|error| error.to_string())
        })
    }

    pub fn required_attribute_with<T, E>(
        mut self,
        name: &'static str,
        prompt_example: &'static str,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display,
    {
        self.attributes.push(XmlAttributeDeclaration::required(
            name,
            prompt_example,
            parse,
        ));
        self
    }

    pub fn optional_attribute<T>(self, name: &'static str, prompt_example: &'static str) -> Self
    where
        T: FromStr + Send + Sync + 'static,
        T::Err: Display,
    {
        self.optional_attribute_with(name, prompt_example, |value| {
            value.parse::<T>().map_err(|error| error.to_string())
        })
    }

    pub fn optional_attribute_with<T, E>(
        mut self,
        name: &'static str,
        prompt_example: &'static str,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display,
    {
        self.attributes.push(XmlAttributeDeclaration::optional(
            name,
            prompt_example,
            parse,
        ));
        self
    }

    pub fn occurs(mut self, cardinality: XmlCardinality) -> Self {
        self.cardinality = cardinality;
        self
    }

    pub fn decode<Head, Value>(
        self,
        decode_open: impl Fn(XmlDecodedAttributes) -> Result<Head, XmlDecodeViolation>
            + Send
            + Sync
            + 'static,
        decode_complete: impl Fn(&Head, &str) -> Result<Value, XmlDecodeViolation>
            + Send
            + Sync
            + 'static,
    ) -> XmlElementContract<Head, Value>
    where
        Head: Send + Sync + 'static,
        Value: Send + 'static,
    {
        XmlElementContract {
            name: self.name,
            form: XmlElementFormKind::of::<Form>(),
            attributes: self.attributes,
            cardinality: self.cardinality,
            decode_open: Box::new(decode_open),
            decode_complete: Box::new(decode_complete),
        }
    }
}

/// Occurrence limits for one declared XML element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlCardinality {
    min: usize,
    max: Option<usize>,
}

impl XmlCardinality {
    pub const fn any() -> Self {
        Self { min: 0, max: None }
    }

    pub const fn optional() -> Self {
        Self {
            min: 0,
            max: Some(1),
        }
    }

    pub const fn exactly(count: usize) -> Self {
        Self {
            min: count,
            max: Some(count),
        }
    }

    pub const fn between(min: usize, max: usize) -> Self {
        assert!(min <= max, "XML cardinality minimum exceeds maximum");
        Self {
            min,
            max: Some(max),
        }
    }

    pub const fn min(&self) -> usize {
        self.min
    }

    pub const fn max(&self) -> Option<usize> {
        self.max
    }
}

/// Typed attribute values consumed by an element's open decoder.
pub struct XmlDecodedAttributes {
    values: HashMap<&'static str, DecodedAttributeSlot>,
}

impl XmlDecodedAttributes {
    pub(crate) fn from_values(
        values: impl IntoIterator<Item = (&'static str, Option<Box<dyn Any + Send + Sync>>)>,
    ) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(name, value)| {
                    (
                        name,
                        DecodedAttributeSlot {
                            value,
                            taken: false,
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn take_required<T: Send + Sync + 'static>(
        &mut self,
        name: &'static str,
    ) -> Result<T, XmlDecodeViolation> {
        let value = self.take(name)?;
        value.downcast::<T>().map(|value| *value).map_err(|_| {
            XmlDecodeViolation::AttributeAccess(XmlAttributeAccessFault::type_mismatch(
                name,
                std::any::type_name::<T>(),
            ))
        })
    }

    pub fn take_optional<T: Send + Sync + 'static>(
        &mut self,
        name: &'static str,
    ) -> Result<Option<T>, XmlDecodeViolation> {
        let Some(value) = self.take_optional_raw(name)? else {
            return Ok(None);
        };
        value
            .downcast::<T>()
            .map(|value| Some(*value))
            .map_err(|_| {
                XmlDecodeViolation::AttributeAccess(XmlAttributeAccessFault::type_mismatch(
                    name,
                    std::any::type_name::<T>(),
                ))
            })
    }

    fn take(
        &mut self,
        name: &'static str,
    ) -> Result<Box<dyn Any + Send + Sync>, XmlDecodeViolation> {
        self.take_optional_raw(name)?.ok_or_else(|| {
            XmlDecodeViolation::AttributeAccess(XmlAttributeAccessFault::missing(name))
        })
    }

    fn take_optional_raw(
        &mut self,
        name: &'static str,
    ) -> Result<Option<Box<dyn Any + Send + Sync>>, XmlDecodeViolation> {
        let Some(slot) = self.values.get_mut(name) else {
            return Err(XmlDecodeViolation::AttributeAccess(
                XmlAttributeAccessFault::undeclared(name),
            ));
        };
        if slot.taken {
            return Err(XmlDecodeViolation::AttributeAccess(
                XmlAttributeAccessFault::already_taken(name),
            ));
        }
        slot.taken = true;
        Ok(slot.value.take())
    }
}

struct DecodedAttributeSlot {
    value: Option<Box<dyn Any + Send + Sync>>,
    taken: bool,
}

pub(super) struct XmlAttributeDeclaration {
    pub(super) name: &'static str,
    pub(super) prompt_example: &'static str,
    pub(super) required: bool,
    parse: AttributeParser,
}

type AttributeParser =
    Box<dyn Fn(&str) -> Result<Box<dyn Any + Send + Sync>, XmlDecodeViolation> + Send + Sync>;

impl XmlAttributeDeclaration {
    fn required<T, E>(
        name: &'static str,
        prompt_example: &'static str,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display,
    {
        Self::new(name, prompt_example, true, parse)
    }

    fn optional<T, E>(
        name: &'static str,
        prompt_example: &'static str,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display,
    {
        Self::new(name, prompt_example, false, parse)
    }

    fn new<T, E>(
        name: &'static str,
        prompt_example: &'static str,
        required: bool,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display,
    {
        Self {
            name,
            prompt_example,
            required,
            parse: Box::new(move |raw| {
                parse(raw)
                    .map(|value| Box::new(value) as Box<dyn Any + Send + Sync>)
                    .map_err(|_| XmlDecodeViolation::Model {
                        code: "invalid_attribute",
                        expected: std::any::type_name::<T>(),
                    })
            }),
        }
    }

    pub(super) fn decode(
        &self,
        raw: &str,
    ) -> Result<Box<dyn Any + Send + Sync>, XmlDecodeViolation> {
        (self.parse)(raw)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum XmlElementFormKind {
    SelfClosing,
    Text,
}

impl XmlElementFormKind {
    fn of<Form: 'static>() -> Self {
        if TypeId::of::<Form>() == TypeId::of::<SelfClosing>() {
            Self::SelfClosing
        } else {
            Self::Text
        }
    }
}

/// The handler completion slot is intentionally tracked in the type system.
pub struct MissingCompletion;

/// The handler completion slot is intentionally tracked in the type system.
pub struct ReadyCompletion;

/// Reducers for one typed XML element.
pub struct XmlElementHandlers<State, C, Head, Value, Completion = MissingCompletion>
where
    C: StreamingToolChannels,
{
    pub(super) open: Vec<OpenReducer<State, C, Head>>,
    pub(super) delta: Vec<DeltaReducer<State, C, Head>>,
    pub(super) complete: Option<CompleteReducer<State, C, Head, Value>>,
    pub(super) marker: PhantomData<fn() -> Completion>,
}

type CompleteReducer<State, C, Head, Value> =
    Box<dyn Fn(&mut State, XmlComplete<Head, Value>) -> CompleteDispatch<C> + Send + Sync>;
type OpenReducer<State, C, Head> =
    Box<dyn Fn(&State, XmlOpen<Head>) -> StreamingToolUpdate<C> + Send + Sync>;
type DeltaReducer<State, C, Head> =
    Box<dyn Fn(&State, XmlTextDelta<Head>) -> StreamingToolUpdate<C> + Send + Sync>;

pub(super) enum CompleteDispatch<C: StreamingToolChannels> {
    Valid(StreamingToolUpdate<C>),
    Invalid(XmlOccurrenceRejection<C::Diagnostic>),
}

impl<State, C, Head, Value> XmlElementHandlers<State, C, Head, Value, MissingCompletion>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    pub(crate) fn new() -> Self {
        Self {
            open: Vec::new(),
            delta: Vec::new(),
            complete: None,
            marker: PhantomData,
        }
    }

    pub fn on_open(
        mut self,
        reduce: impl Fn(&State, XmlOpen<Head>) -> StreamingToolUpdate<C> + Send + Sync + 'static,
    ) -> Self {
        self.open.push(Box::new(reduce));
        self
    }

    pub fn on_delta(
        mut self,
        reduce: impl Fn(&State, XmlTextDelta<Head>) -> StreamingToolUpdate<C> + Send + Sync + 'static,
    ) -> Self {
        self.delta.push(Box::new(reduce));
        self
    }

    pub fn on_complete(
        self,
        reduce: impl Fn(&mut State, XmlComplete<Head, Value>) -> StreamingToolUpdate<C>
            + Send
            + Sync
            + 'static,
    ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion> {
        self.with_completion(Box::new(move |state, complete| {
            CompleteDispatch::Valid(reduce(state, complete))
        }))
    }

    pub fn on_complete_validated(
        self,
        validate: impl Fn(&State, &XmlComplete<Head, Value>) -> XmlOccurrenceValidity<C::Diagnostic>
            + Send
            + Sync
            + 'static,
        reduce: impl Fn(&mut State, XmlComplete<Head, Value>) -> StreamingToolUpdate<C>
            + Send
            + Sync
            + 'static,
    ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion> {
        self.with_completion(Box::new(move |state, complete| {
            match validate(state, &complete) {
                XmlOccurrenceValidity::Valid => CompleteDispatch::Valid(reduce(state, complete)),
                XmlOccurrenceValidity::Invalid(rejection) => CompleteDispatch::Invalid(rejection),
            }
        }))
    }

    fn with_completion(
        self,
        complete: CompleteReducer<State, C, Head, Value>,
    ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion> {
        XmlElementHandlers {
            open: self.open,
            delta: self.delta,
            complete: Some(complete),
            marker: PhantomData,
        }
    }
}

/// Domain validation result for an otherwise decoded occurrence.
pub enum XmlOccurrenceValidity<D> {
    Valid,
    Invalid(XmlOccurrenceRejection<D>),
}

/// Diagnostics explaining why an occurrence did not complete.
pub struct XmlOccurrenceRejection<D> {
    diagnostics: Vec<D>,
}

impl<D> XmlOccurrenceRejection<D> {
    pub fn diagnostic(diagnostic: D) -> Self {
        Self {
            diagnostics: vec![diagnostic],
        }
    }

    pub fn with_diagnostic(mut self, diagnostic: D) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    pub fn diagnostics(&self) -> &[D] {
        &self.diagnostics
    }

    pub(crate) fn into_diagnostics(self) -> Vec<D> {
        self.diagnostics
    }
}

/// Final report passed to a contract's final reducer.
pub struct XmlAttemptSummary<'a, D> {
    pub attempt: StreamingToolAttemptId,
    pub raw_output: &'a str,
    pub elements: &'a [XmlElementSummary],
    pub diagnostics: &'a [StreamingToolDiagnosticRecord<D>],
}

/// The final disposition selected by a reducer at reaction EOF.
pub enum StreamingToolDecision<C: StreamingToolChannels> {
    Accept(StreamingToolUpdate<C>),
    Reject(StreamingToolRejection<C::Diagnostic>),
}

/// Additional domain diagnostics produced by a final rejection.
pub struct StreamingToolRejection<D> {
    diagnostics: Vec<D>,
}

impl<D> StreamingToolRejection<D> {
    pub fn none() -> Self {
        Self {
            diagnostics: Vec::new(),
        }
    }

    pub fn diagnostic(diagnostic: D) -> Self {
        Self {
            diagnostics: vec![diagnostic],
        }
    }

    pub fn with_diagnostic(mut self, diagnostic: D) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    pub fn diagnostics(&self) -> &[D] {
        &self.diagnostics
    }

    pub(crate) fn into_diagnostics(self) -> Vec<D> {
        self.diagnostics
    }
}

/// A framework XML-contract diagnostic.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlContractViolationKind {
    Malformed,
    Incomplete,
    UnknownElement,
    UnknownNamespace,
    MissingAttribute,
    DuplicateAttribute,
    UnknownAttribute,
    InvalidAttribute,
    WrongElementForm,
    NestedMarkup,
    TextOutsideEnvelope,
    Cardinality,
}

/// A bounded contract violation. Model text remains addressable through raw spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XmlContractViolation {
    contract_identity: &'static str,
    element_name: Option<&'static str>,
    attribute_name: Option<&'static str>,
    kind: XmlContractViolationKind,
    detail_code: &'static str,
}

impl XmlContractViolation {
    pub(crate) const fn new(
        contract_identity: &'static str,
        element_name: Option<&'static str>,
        attribute_name: Option<&'static str>,
        kind: XmlContractViolationKind,
        detail_code: &'static str,
    ) -> Self {
        Self {
            contract_identity,
            element_name,
            attribute_name,
            kind,
            detail_code,
        }
    }

    pub fn contract_identity(&self) -> &str {
        self.contract_identity
    }

    pub fn element_name(&self) -> Option<&str> {
        self.element_name
    }

    pub fn attribute_name(&self) -> Option<&str> {
        self.attribute_name
    }

    pub fn kind(&self) -> XmlContractViolationKind {
        self.kind
    }

    pub fn detail_code(&self) -> &'static str {
        self.detail_code
    }
}

/// An invariant-safe failure to consume an attribute slot as declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XmlAttributeAccessFault {
    attribute: &'static str,
    code: &'static str,
    expected: Option<&'static str>,
}

impl XmlAttributeAccessFault {
    const fn missing(attribute: &'static str) -> Self {
        Self {
            attribute,
            code: "missing_required_slot",
            expected: None,
        }
    }

    const fn undeclared(attribute: &'static str) -> Self {
        Self {
            attribute,
            code: "undeclared_slot",
            expected: None,
        }
    }

    const fn already_taken(attribute: &'static str) -> Self {
        Self {
            attribute,
            code: "slot_already_taken",
            expected: None,
        }
    }

    const fn type_mismatch(attribute: &'static str, expected: &'static str) -> Self {
        Self {
            attribute,
            code: "slot_type_mismatch",
            expected: Some(expected),
        }
    }

    pub fn attribute(&self) -> &str {
        self.attribute
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn expected(&self) -> Option<&'static str> {
        self.expected
    }
}

/// A typed wire decoder failure.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XmlDecodeViolation {
    Model {
        code: &'static str,
        expected: &'static str,
    },
    AttributeAccess(XmlAttributeAccessFault),
}

/// Per-element counts observed during one attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XmlElementSummary {
    pub contract_index: usize,
    pub name: &'static str,
    pub seen: usize,
    pub opened: usize,
    pub decoded: usize,
    pub completed: usize,
    pub implicit_eof: usize,
}

/// Contract metadata supplied before state construction.
#[derive(Clone, Copy, Debug)]
pub struct StreamingToolAttemptStart {
    pub(super) attempt: StreamingToolAttemptId,
    pub(super) contract_identity: &'static str,
    pub(super) implementation_version: &'static str,
}

impl StreamingToolAttemptStart {
    pub(crate) const fn new(
        attempt: StreamingToolAttemptId,
        contract_identity: &'static str,
        implementation_version: &'static str,
    ) -> Self {
        Self {
            attempt,
            contract_identity,
            implementation_version,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub fn contract_identity(&self) -> &str {
        self.contract_identity
    }

    pub fn implementation_version(&self) -> &str {
        self.implementation_version
    }
}

/// Contract metadata retained throughout one attempt.
#[derive(Clone, Copy, Debug)]
pub struct StreamingToolAttemptContext {
    attempt: StreamingToolAttemptId,
    contract_identity: &'static str,
    implementation_version: &'static str,
}

impl StreamingToolAttemptContext {
    pub(crate) const fn from_start(start: &StreamingToolAttemptStart) -> Self {
        Self {
            attempt: start.attempt,
            contract_identity: start.contract_identity,
            implementation_version: start.implementation_version,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub fn contract_identity(&self) -> &str {
        self.contract_identity
    }

    pub fn implementation_version(&self) -> &str {
        self.implementation_version
    }
}

/// Attempt-local live-effect correlation ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamingEffectId(u64);

impl StreamingEffectId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Attempt-local publication correlation ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamingPublicationId(u64);

impl StreamingPublicationId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Context for preparing/applying a live value.
pub struct LiveEffectContext {
    attempt: StreamingToolAttemptId,
    effect: StreamingEffectId,
    emission_sequence: u64,
    origin: StreamingEmissionOrigin,
}

impl LiveEffectContext {
    pub(crate) fn new(
        start: &StreamingToolAttemptStart,
        effect: StreamingEffectId,
        emission_sequence: u64,
        origin: StreamingEmissionOrigin,
    ) -> Self {
        Self {
            attempt: start.attempt,
            effect,
            emission_sequence,
            origin,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn effect(&self) -> StreamingEffectId {
        self.effect
    }

    pub const fn emission_sequence(&self) -> u64 {
        self.emission_sequence
    }

    pub fn origin(&self) -> &StreamingEmissionOrigin {
        &self.origin
    }
}

/// Context for confirming a live effect after publication.
pub struct LiveConfirmContext {
    attempt: StreamingToolAttemptId,
    effect: StreamingEffectId,
}

impl LiveConfirmContext {
    pub(crate) const fn new(start: &StreamingToolAttemptStart, effect: StreamingEffectId) -> Self {
        Self {
            attempt: start.attempt,
            effect,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn effect(&self) -> StreamingEffectId {
        self.effect
    }
}

/// Context for rolling back a live effect.
pub struct LiveRollbackContext {
    attempt: StreamingToolAttemptId,
    effect: StreamingEffectId,
    cause: StreamingToolAbortCause,
}

impl LiveRollbackContext {
    pub(crate) const fn new(
        start: &StreamingToolAttemptStart,
        effect: StreamingEffectId,
        cause: StreamingToolAbortCause,
    ) -> Self {
        Self {
            attempt: start.attempt,
            effect,
            cause,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn effect(&self) -> StreamingEffectId {
        self.effect
    }

    pub const fn cause(&self) -> &StreamingToolAbortCause {
        &self.cause
    }
}

/// Context for a live settlement preparation.
pub struct LiveSettlementContext {
    attempt: StreamingToolAttemptId,
    effect: StreamingEffectId,
    settlement: LiveSettlement,
    rollback_cause: Option<StreamingToolAbortCause>,
}

impl LiveSettlementContext {
    pub(crate) const fn new(
        start: &StreamingToolAttemptStart,
        effect: StreamingEffectId,
        settlement: LiveSettlement,
        rollback_cause: Option<StreamingToolAbortCause>,
    ) -> Self {
        Self {
            attempt: start.attempt,
            effect,
            settlement,
            rollback_cause,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn effect(&self) -> StreamingEffectId {
        self.effect
    }

    pub const fn settlement(&self) -> LiveSettlement {
        self.settlement
    }

    pub const fn rollback_cause(&self) -> Option<&StreamingToolAbortCause> {
        self.rollback_cause.as_ref()
    }
}

/// Context for resolving an indeterminate live operation.
pub struct LiveRecoveryContext {
    attempt: StreamingToolAttemptId,
    effect: StreamingEffectId,
    phase: StreamingToolRecoveryPhase,
    rollback_cause: Option<StreamingToolAbortCause>,
}

impl LiveRecoveryContext {
    pub(crate) const fn new(
        start: &StreamingToolAttemptStart,
        effect: StreamingEffectId,
        phase: StreamingToolRecoveryPhase,
        rollback_cause: Option<StreamingToolAbortCause>,
    ) -> Self {
        Self {
            attempt: start.attempt,
            effect,
            phase,
            rollback_cause,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn effect(&self) -> StreamingEffectId {
        self.effect
    }

    pub const fn phase(&self) -> StreamingToolRecoveryPhase {
        self.phase
    }

    pub const fn rollback_cause(&self) -> Option<&StreamingToolAbortCause> {
        self.rollback_cause.as_ref()
    }
}

/// Context for publishing accepted output and commits.
pub struct StreamingPublishContext {
    attempt: StreamingToolAttemptId,
    publication: StreamingPublicationId,
}

impl StreamingPublishContext {
    pub(crate) const fn new(
        start: &StreamingToolAttemptStart,
        publication: StreamingPublicationId,
    ) -> Self {
        Self {
            attempt: start.attempt,
            publication,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn publication(&self) -> StreamingPublicationId {
        self.publication
    }
}

/// Context for resolving an indeterminate publication.
pub struct StreamingPublishRecoveryContext {
    attempt: StreamingToolAttemptId,
    publication: StreamingPublicationId,
}

impl StreamingPublishRecoveryContext {
    pub(crate) const fn new(
        start: &StreamingToolAttemptStart,
        publication: StreamingPublicationId,
    ) -> Self {
        Self {
            attempt: start.attempt,
            publication,
        }
    }

    pub const fn attempt(&self) -> StreamingToolAttemptId {
        self.attempt
    }

    pub const fn publication(&self) -> StreamingPublicationId {
        self.publication
    }
}

/// Cause used for a compensating live settlement.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingToolAbortCause {
    Rejected,
    OccurrenceInvalidated,
    ProviderFailed,
    Cancelled,
    ReducerFault,
    LiveFault,
    PublicationNotPublished,
    RuntimeFault,
}

/// The intended terminal settlement for a live receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveSettlement {
    Confirm,
    Rollback,
}

/// One recovery operation retained by the effect supervisor.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingToolRecoveryPhase {
    ResolveLiveApply,
    ResolvePublication,
    ConfirmLive,
    RollbackLive,
    HandleRejection,
    PostReconcile,
}

/// A report passed to the publisher after an accepted final decision.
pub struct AcceptedStreamingToolAttempt<C: StreamingToolChannels> {
    pub context: StreamingToolAttemptContext,
    pub raw_output: String,
    pub entries: Vec<StagedStreamingToolEmission<C>>,
    pub diagnostics: Vec<StreamingToolDiagnosticRecord<C::Diagnostic>>,
}

impl<C: StreamingToolChannels> AcceptedStreamingToolAttempt<C> {
    pub(crate) fn new(
        context: StreamingToolAttemptContext,
        raw_output: String,
        entries: Vec<StagedStreamingToolEmission<C>>,
        diagnostics: Vec<StreamingToolDiagnosticRecord<C::Diagnostic>>,
    ) -> Self {
        Self {
            context,
            raw_output,
            entries,
            diagnostics,
        }
    }
}

/// A report passed to an optional rejection continuation.
pub struct RejectedStreamingToolAttempt<D> {
    pub context: StreamingToolAttemptContext,
    pub raw_output: String,
    pub diagnostics: Vec<StreamingToolDiagnosticRecord<D>>,
}

/// Post-cleanup action selected by a rejection continuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingToolRejectionAction {
    Complete,
    RequestReaction,
}

/// Framework and application diagnostics retained in an attempt report.
pub enum StreamingToolDiagnostic<D> {
    Contract(XmlContractViolation),
    Decode(XmlDecodeViolation),
    Domain(D),
}

/// Source of an attempt diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamingToolDiagnosticOrigin {
    Parser {
        occurrence: Option<XmlOccurrenceId>,
        span: Option<XmlSourceSpan>,
    },
    Reducer(StreamingEmissionOrigin),
}

/// One sequenced contract, decode, or domain diagnostic.
pub struct StreamingToolDiagnosticRecord<D> {
    pub sequence: u64,
    pub origin: StreamingToolDiagnosticOrigin,
    pub diagnostic: StreamingToolDiagnostic<D>,
}

/// Declaration validation failure detected before state construction or provider work.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StreamingToolDeclarationFault {
    #[error("streaming tool contract `{contract}` has invalid element `{element}`: {detail}")]
    InvalidElementName {
        contract: &'static str,
        element: &'static str,
        detail: String,
    },
    #[error(
        "streaming tool contract `{contract}` has invalid attribute `{attribute}` on `{element}`: {detail}"
    )]
    InvalidAttributeName {
        contract: &'static str,
        element: &'static str,
        attribute: &'static str,
        detail: String,
    },
    #[error("streaming tool contract `{contract}` declares element `{element}` more than once")]
    DuplicateElement {
        contract: &'static str,
        element: &'static str,
    },
    #[error(
        "streaming tool contract `{contract}` declares attribute `{attribute}` more than once on `{element}`"
    )]
    DuplicateAttribute {
        contract: &'static str,
        element: &'static str,
        attribute: &'static str,
    },
    #[error("streaming tool contract `{contract}` has invalid identity: {detail}")]
    InvalidIdentity {
        contract: &'static str,
        detail: String,
    },
    #[error("streaming tool contract `{contract}` cannot render its prompt: {message}")]
    Prompt {
        contract: &'static str,
        message: String,
    },
}

/// A sanitized failure while a mounted contract is executing.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StreamingToolDriverFault {
    #[error("streaming tool declaration failure: {0}")]
    Declaration(#[from] StreamingToolDeclarationFault),
    #[error("streaming tool `{contract}` input failure: {message}")]
    Input {
        contract: &'static str,
        message: String,
    },
    #[error("streaming tool `{contract}` resource limit failure: {message}")]
    Limit {
        contract: &'static str,
        message: String,
    },
    #[error("streaming tool `{contract}` reducer failure: {message}")]
    Reducer {
        contract: &'static str,
        message: String,
    },
    #[error("streaming tool `{contract}` adapter failure: {message}")]
    Adapter {
        contract: &'static str,
        message: String,
    },
    #[error("streaming tool `{contract}` requires recovery: {message}")]
    RecoveryRequired {
        contract: &'static str,
        message: String,
    },
    #[error("streaming tool `{contract}` aborted after {cause}")]
    Aborted {
        contract: &'static str,
        cause: StreamingToolAbortCause,
    },
    #[error("streaming tool `{contract}` is no longer active")]
    Inactive { contract: &'static str },
}

impl fmt::Display for StreamingToolAbortCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
