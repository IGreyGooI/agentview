//! Incremental, per-contract XML fragment parsing for strict streaming attempts.
//!
//! This module deliberately has no Component or effect-runtime dependency.  A mounted strict
//! contract owns one [`Parser`], feeds it the selected text output, and interprets the resulting
//! events synchronously before advancing to the next provider fact.  The legacy
//! `authoring::streaming_xml` parser remains a separate permissive compatibility path.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    str,
    sync::Arc,
};

use quick_xml::{events::Event, reader::Reader, XmlVersion};

const DEFAULT_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_DEPTH: usize = 256;
const DEFAULT_MAX_ATTRIBUTES: usize = 256;
const DEFAULT_MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_OCCURRENCES: usize = 4_096;

/// The only wire forms accepted by a declared strict element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ElementForm {
    SelfClosing,
    Text,
}

/// One element name and form accepted by a strict contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ElementSpec {
    pub(crate) name: &'static str,
    pub(crate) form: ElementForm,
}

/// How a contract treats an element it did not declare at fragment top level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UnknownTopLevel {
    /// Emit a model-contract diagnostic and skip the complete foreign subtree.
    #[default]
    Reject,
    /// Skip the complete foreign subtree without producing a contract diagnostic.
    IgnoreSubtree,
}

/// Resource limits for one parser instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParserLimits {
    pub(crate) max_input_bytes: usize,
    pub(crate) max_depth: usize,
    pub(crate) max_attributes: usize,
    pub(crate) max_content_bytes: usize,
    pub(crate) max_occurrences: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_attributes: DEFAULT_MAX_ATTRIBUTES,
            max_content_bytes: DEFAULT_MAX_CONTENT_BYTES,
            max_occurrences: DEFAULT_MAX_OCCURRENCES,
        }
    }
}

/// Immutable grammar configuration for one parser instance.
#[derive(Clone, Debug)]
pub(crate) struct Contract {
    pub(crate) elements: Vec<ElementSpec>,
    pub(crate) unknown_top_level: UnknownTopLevel,
    pub(crate) allow_implicit_text_eof: bool,
    pub(crate) limits: ParserLimits,
}

impl Contract {
    pub(crate) fn strict(elements: Vec<ElementSpec>) -> Self {
        Self {
            elements,
            unknown_top_level: UnknownTopLevel::Reject,
            allow_implicit_text_eof: false,
            limits: ParserLimits::default(),
        }
    }
}

/// A declaration error discovered before a parser begins consuming provider text.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum ParserDeclarationFault {
    #[error("strict streaming element `{name}` is not a valid unqualified XML name")]
    InvalidElementName { name: &'static str },
    #[error("strict streaming grammar declares element `{name}` more than once")]
    DuplicateElement { name: &'static str },
    #[error("strict streaming parser limit `{limit}` must be greater than zero")]
    InvalidLimit { limit: &'static str },
}

/// A parser safety or lifecycle fault, distinct from model-authored diagnostics.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum ParserFault {
    #[error("strict streaming parser received text after normal EOF")]
    AfterFinish,
    #[error("strict streaming parser was aborted")]
    Aborted,
    #[error("strict streaming input exceeded {maximum} bytes (observed {observed})")]
    InputLimitExceeded { maximum: usize, observed: usize },
    #[error("strict streaming XML depth exceeded {maximum} (observed {observed})")]
    DepthLimitExceeded { maximum: usize, observed: usize },
    #[error("strict streaming XML element exceeded {maximum} attributes (observed {observed})")]
    AttributeLimitExceeded { maximum: usize, observed: usize },
    #[error(
        "strict streaming occurrence {occurrence} exceeded {maximum} content bytes (observed {observed})"
    )]
    ContentLimitExceeded {
        occurrence: u64,
        maximum: usize,
        observed: usize,
    },
    #[error("strict streaming XML occurrences exceeded {maximum} (observed {observed})")]
    OccurrenceLimitExceeded { maximum: usize, observed: usize },
}

/// The reason a model-authored fragment does not satisfy the parser grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InvalidKind {
    MalformedMarkup,
    UnknownElement,
    UnknownNamespace,
    DuplicateAttribute,
    WrongElementForm,
    NestedMarkup,
    TextOutsideEnvelope,
    IncompleteElement,
    MismatchedClose,
    InvalidEntity,
}

/// The source form by which a text element completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionForm {
    SelfClosing,
    ExplicitClose,
    ImplicitEof,
}

/// An incremental parser event. Spans are byte ranges into [`Parser::raw_output`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParserEvent {
    Open {
        occurrence: u64,
        name: String,
        attributes: Vec<(String, String)>,
        span: Range<usize>,
        self_closing: bool,
    },
    Delta {
        occurrence: u64,
        delta: String,
        accumulated: Arc<str>,
        span: Range<usize>,
    },
    Complete {
        occurrence: u64,
        form: CompletionForm,
        span: Range<usize>,
    },
    Invalid {
        occurrence: Option<u64>,
        /// Lexically attributable element name when one is available. In particular, this is set
        /// for declaration-targeted failures that occur before a valid `Open` event.
        element: Option<String>,
        kind: InvalidKind,
        span: Option<Range<usize>>,
    },
}

/// A streaming, bounded parser for one strict XML fragment contract.
pub(crate) struct Parser {
    contract: Contract,
    element_indexes: HashMap<&'static str, usize>,
    raw_output: String,
    pending: String,
    pending_offset: usize,
    incomplete_scan: Option<IncompleteScan>,
    stack: Vec<Frame>,
    next_occurrence: u64,
    occurrence_count: usize,
    finished: bool,
    aborted: bool,
}

impl Parser {
    pub(crate) fn new(contract: Contract) -> Result<Self, ParserDeclarationFault> {
        validate_limits(contract.limits)?;

        let mut element_indexes = HashMap::with_capacity(contract.elements.len());
        for (index, element) in contract.elements.iter().enumerate() {
            if !is_unqualified_xml_name(element.name) {
                return Err(ParserDeclarationFault::InvalidElementName { name: element.name });
            }
            if element_indexes.insert(element.name, index).is_some() {
                return Err(ParserDeclarationFault::DuplicateElement { name: element.name });
            }
        }

        Ok(Self {
            contract,
            element_indexes,
            raw_output: String::new(),
            pending: String::new(),
            pending_offset: 0,
            incomplete_scan: None,
            stack: Vec::new(),
            next_occurrence: 1,
            occurrence_count: 0,
            finished: false,
            aborted: false,
        })
    }

    /// Feed an incremental delta from the selected provider text output.
    ///
    /// A text-sealed provider fact may use this same method for its unobserved suffix. It does not
    /// mean normal reaction EOF; only [`finish_normal`](Self::finish_normal) may synthesize the
    /// optional implicit text completion.
    pub(crate) fn push(&mut self, chunk: &str) -> Result<Vec<ParserEvent>, ParserFault> {
        self.ensure_active()?;
        let observed = self.raw_output.len().saturating_add(chunk.len());
        if observed > self.contract.limits.max_input_bytes {
            return Err(ParserFault::InputLimitExceeded {
                maximum: self.contract.limits.max_input_bytes,
                observed,
            });
        }
        self.raw_output.push_str(chunk);
        self.pending.push_str(chunk);
        self.process_pending(false)
    }

    /// Finish only after admission has validated the normal reaction EOF.
    ///
    /// This method does not correspond to `TextSealed`; an output may seal before later provider
    /// facts establish the reaction's selected primary text and normal completion.
    pub(crate) fn finish_normal(&mut self) -> Result<Vec<ParserEvent>, ParserFault> {
        self.ensure_active()?;
        let mut events = self.process_pending(true)?;

        if !self.pending.is_empty() {
            let span = self.pending_offset..self.raw_output.len();
            let element = lexical_element_name(&self.pending).map(str::to_owned);
            if !self.is_suppressed() {
                let occurrence = match self.active_occurrence() {
                    Some(occurrence) => Some(occurrence),
                    None => self.occurrence_for_lexical_name(element.as_deref())?,
                };
                let attributed_element = self
                    .active_element_name()
                    .map(ToOwned::to_owned)
                    .or(element);
                events.push(invalid_with_owned_element(
                    occurrence,
                    attributed_element,
                    InvalidKind::MalformedMarkup,
                    Some(span),
                ));
            }
            self.pending.clear();
            self.pending_offset = self.raw_output.len();
            self.incomplete_scan = None;
            self.invalidate_active();
        }

        let eof_action = self.stack.last().and_then(|frame| match &frame.kind {
            FrameKind::Tracked(tracked)
                if tracked.valid
                    && tracked.form == ElementForm::Text
                    && self.contract.allow_implicit_text_eof =>
            {
                Some(EofAction::ImplicitComplete(tracked.occurrence))
            }
            FrameKind::Tracked(tracked) if tracked.valid => Some(EofAction::Invalidate {
                occurrence: tracked.occurrence,
                element: frame.name.clone(),
            }),
            FrameKind::Tracked(_) | FrameKind::Ignored => None,
        });
        match eof_action {
            Some(EofAction::ImplicitComplete(occurrence)) => {
                self.stack.pop();
                let offset = self.raw_output.len();
                events.push(ParserEvent::Complete {
                    occurrence,
                    form: CompletionForm::ImplicitEof,
                    span: offset..offset,
                });
            }
            Some(EofAction::Invalidate {
                occurrence,
                element,
            }) => {
                self.stack.pop();
                events.push(invalid_for_element(
                    Some(occurrence),
                    Some(element.as_str()),
                    InvalidKind::IncompleteElement,
                    None,
                ));
            }
            None => {}
        }

        // Any remaining frames are either an ignored foreign subtree or an already-invalid
        // occurrence. They must not synthesize a completion or a second diagnostic at EOF.
        self.stack.clear();
        self.finished = true;
        Ok(events)
    }

    /// Abandon this parser without synthesizing normal EOF behavior.
    pub(crate) fn abort(&mut self) {
        if self.finished {
            return;
        }
        self.aborted = true;
        self.pending.clear();
        self.stack.clear();
        self.incomplete_scan = None;
    }

    pub(crate) fn raw_output(&self) -> &str {
        &self.raw_output
    }

    fn ensure_active(&self) -> Result<(), ParserFault> {
        if self.aborted {
            Err(ParserFault::Aborted)
        } else if self.finished {
            Err(ParserFault::AfterFinish)
        } else {
            Ok(())
        }
    }

    fn process_pending(&mut self, eof: bool) -> Result<Vec<ParserEvent>, ParserFault> {
        let mut events = Vec::new();
        let mut cursor = 0;

        loop {
            let Some(relative_markup) = self.pending[cursor..].find('<') else {
                let text_start = cursor;
                let text_end = self.pending.len();
                if text_start < text_end {
                    let boundary = eof;
                    let consumed =
                        self.process_text_slice(text_start, text_end, boundary, &mut events)?;
                    cursor = text_start + consumed;
                    if consumed < text_end - text_start {
                        break;
                    }
                }
                break;
            };

            let markup_start = cursor + relative_markup;
            if markup_start > cursor {
                let consumed = self.process_text_slice(cursor, markup_start, true, &mut events)?;
                cursor += consumed;
                if cursor < markup_start {
                    break;
                }
            }

            let remainder = &self.pending[markup_start..];
            let Some(markup_end) = complete_markup_len(remainder, &mut self.incomplete_scan) else {
                break;
            };
            let absolute_start = self.pending_offset + markup_start;
            let markup = self.pending[markup_start..markup_start + markup_end].to_owned();
            self.process_markup(&markup, absolute_start, &mut events)?;
            cursor = markup_start + markup_end;
        }

        self.pending.drain(..cursor);
        self.pending_offset += cursor;
        Ok(events)
    }

    /// Consume a text slice. When it reaches the current stream end, leave a trailing partial
    /// entity in `pending` until a later chunk or normal EOF establishes its meaning.
    fn process_text_slice(
        &mut self,
        start: usize,
        end: usize,
        boundary: bool,
        events: &mut Vec<ParserEvent>,
    ) -> Result<usize, ParserFault> {
        let raw = &self.pending[start..end];
        let Some(consumed) = complete_text_len(raw, boundary) else {
            return Ok(0);
        };
        if consumed == 0 {
            return Ok(0);
        }

        let span = (self.pending_offset + start)..(self.pending_offset + start + consumed);
        let raw = &raw[..consumed];
        let decoded = match decode_text(raw) {
            Ok(decoded) => decoded,
            Err(()) => {
                let occurrence = self.active_occurrence();
                events.push(invalid(occurrence, InvalidKind::InvalidEntity, Some(span)));
                self.invalidate_active();
                return Ok(consumed);
            }
        };

        let Some(frame) = self.stack.last_mut() else {
            if !decoded.chars().all(char::is_whitespace) {
                events.push(invalid(None, InvalidKind::TextOutsideEnvelope, Some(span)));
            }
            return Ok(consumed);
        };

        let FrameKind::Tracked(tracked) = &mut frame.kind else {
            return Ok(consumed);
        };
        if !tracked.valid || tracked.form != ElementForm::Text {
            return Ok(consumed);
        }

        let observed = tracked.content_bytes.saturating_add(raw.len());
        if observed > self.contract.limits.max_content_bytes {
            return Err(ParserFault::ContentLimitExceeded {
                occurrence: tracked.occurrence,
                maximum: self.contract.limits.max_content_bytes,
                observed,
            });
        }
        tracked.content_bytes = observed;
        tracked.accumulated.push_str(&decoded);
        events.push(ParserEvent::Delta {
            occurrence: tracked.occurrence,
            delta: decoded,
            accumulated: Arc::from(tracked.accumulated.as_str()),
            span,
        });
        Ok(consumed)
    }

    fn process_markup(
        &mut self,
        markup: &str,
        markup_start: usize,
        events: &mut Vec<ParserEvent>,
    ) -> Result<(), ParserFault> {
        let span = markup_start..markup_start + markup.len();
        if is_special_markup(markup) {
            self.process_special_markup(events, span);
            return Ok(());
        }
        if markup.starts_with("</") {
            self.process_end(markup, events, span);
            return Ok(());
        }

        let parsed = match parse_start(markup, self.contract.limits.max_attributes) {
            Ok(parsed) => parsed,
            Err(StartFault::DuplicateAttribute) => {
                let lexical_name = lexical_element_name(markup);
                if self.ignore_foreign_lexical_start(lexical_name) {
                    self.push_ignored_lexical_start(markup)?;
                    return Ok(());
                }
                let occurrence = self.occurrence_for_lexical_name(lexical_name)?;
                events.push(invalid_for_element(
                    occurrence,
                    lexical_name,
                    InvalidKind::DuplicateAttribute,
                    Some(span),
                ));
                self.skip_invalid_start(markup, occurrence)?;
                return Ok(());
            }
            Err(StartFault::AttributeLimit { observed }) => {
                return Err(ParserFault::AttributeLimitExceeded {
                    maximum: self.contract.limits.max_attributes,
                    observed,
                });
            }
            Err(StartFault::Malformed) => {
                let lexical_name = lexical_element_name(markup);
                if self.ignore_foreign_lexical_start(lexical_name) {
                    self.push_ignored_lexical_start(markup)?;
                    return Ok(());
                }
                let occurrence = self.occurrence_for_lexical_name(lexical_name)?;
                events.push(invalid_for_element(
                    occurrence,
                    lexical_name,
                    InvalidKind::MalformedMarkup,
                    Some(span),
                ));
                self.skip_invalid_start(markup, occurrence)?;
                self.invalidate_active();
                return Ok(());
            }
        };

        if self.is_suppressed() {
            self.push_ignored_frame(parsed.name, parsed.empty)?;
            return Ok(());
        }

        if let Some(parent_occurrence) = self.active_text_occurrence() {
            events.push(invalid(
                Some(parent_occurrence),
                InvalidKind::NestedMarkup,
                Some(span),
            ));
            self.invalidate_active();
            self.push_ignored_frame(parsed.name, parsed.empty)?;
            return Ok(());
        }

        if !self.stack.is_empty() {
            // A non-text parent is always invalid and the child belongs to its discarded subtree.
            let occurrence = self.active_occurrence();
            events.push(invalid(occurrence, InvalidKind::NestedMarkup, Some(span)));
            self.invalidate_active();
            self.push_ignored_frame(parsed.name, parsed.empty)?;
            return Ok(());
        }

        let declared = self.element_indexes.contains_key(parsed.name.as_str());
        let has_namespace = parsed.name.contains(':')
            || parsed
                .attributes
                .iter()
                .any(|(name, _)| name.contains(':') || name == "xmlns");
        if has_namespace {
            if !declared && self.contract.unknown_top_level == UnknownTopLevel::IgnoreSubtree {
                if !parsed.empty {
                    self.push_ignored_frame(parsed.name, false)?;
                }
                return Ok(());
            }
            let occurrence = self.occurrence_for_declared_name(parsed.name.as_str())?;
            events.push(invalid_for_element(
                occurrence,
                Some(parsed.name.as_str()),
                InvalidKind::UnknownNamespace,
                Some(span),
            ));
            if !parsed.empty {
                self.push_ignored_frame(parsed.name, false)?;
            }
            return Ok(());
        }

        let Some(spec_index) = self.element_indexes.get(parsed.name.as_str()).copied() else {
            if self.contract.unknown_top_level == UnknownTopLevel::Reject {
                events.push(invalid_for_element(
                    None,
                    Some(parsed.name.as_str()),
                    InvalidKind::UnknownElement,
                    Some(span),
                ));
            }
            if !parsed.empty {
                self.push_ignored_frame(parsed.name, false)?;
            }
            return Ok(());
        };
        let spec = self.contract.elements[spec_index];
        let occurrence = self.allocate_occurrence()?;

        if (spec.form == ElementForm::SelfClosing) != parsed.empty {
            events.push(invalid_for_element(
                Some(occurrence),
                Some(parsed.name.as_str()),
                InvalidKind::WrongElementForm,
                Some(span),
            ));
            if !parsed.empty {
                self.push_invalid_frame(parsed.name, occurrence)?;
            }
            return Ok(());
        }

        events.push(ParserEvent::Open {
            occurrence,
            name: parsed.name.clone(),
            attributes: parsed.attributes,
            span: span.clone(),
            self_closing: parsed.empty,
        });
        if parsed.empty {
            events.push(ParserEvent::Complete {
                occurrence,
                form: CompletionForm::SelfClosing,
                span,
            });
        } else {
            self.push_tracked_frame(parsed.name, occurrence, spec.form)?;
        }
        Ok(())
    }

    fn process_special_markup(&mut self, events: &mut Vec<ParserEvent>, span: Range<usize>) {
        if self.is_suppressed() {
            return;
        }
        if let Some(occurrence) = self.active_text_occurrence() {
            events.push(invalid(
                Some(occurrence),
                InvalidKind::NestedMarkup,
                Some(span),
            ));
            self.invalidate_active();
        } else {
            events.push(invalid(
                self.active_occurrence(),
                InvalidKind::MalformedMarkup,
                Some(span),
            ));
            self.invalidate_active();
        }
    }

    fn process_end(&mut self, markup: &str, events: &mut Vec<ParserEvent>, span: Range<usize>) {
        let name = match parse_end_name(markup) {
            Ok(name) => name,
            Err(()) => {
                events.push(invalid(
                    self.active_occurrence(),
                    InvalidKind::MalformedMarkup,
                    Some(span),
                ));
                self.invalidate_active();
                return;
            }
        };

        let Some(last) = self.stack.last() else {
            events.push(invalid(None, InvalidKind::MismatchedClose, Some(span)));
            return;
        };
        if last.name != name {
            events.push(invalid(
                self.active_occurrence(),
                InvalidKind::MismatchedClose,
                Some(span),
            ));
            self.invalidate_active();
            return;
        }

        let frame = self.stack.pop().expect("the checked frame exists");
        if let FrameKind::Tracked(tracked) = frame.kind {
            if tracked.valid && tracked.form == ElementForm::Text {
                events.push(ParserEvent::Complete {
                    occurrence: tracked.occurrence,
                    form: CompletionForm::ExplicitClose,
                    span,
                });
            }
        }
    }

    fn push_tracked_frame(
        &mut self,
        name: String,
        occurrence: u64,
        form: ElementForm,
    ) -> Result<(), ParserFault> {
        self.ensure_depth()?;
        self.stack.push(Frame {
            name,
            kind: FrameKind::Tracked(TrackedFrame {
                occurrence,
                form,
                valid: true,
                accumulated: String::new(),
                content_bytes: 0,
            }),
        });
        Ok(())
    }

    fn push_invalid_frame(&mut self, name: String, occurrence: u64) -> Result<(), ParserFault> {
        self.ensure_depth()?;
        self.stack.push(Frame {
            name,
            kind: FrameKind::Tracked(TrackedFrame {
                occurrence,
                form: ElementForm::Text,
                valid: false,
                accumulated: String::new(),
                content_bytes: 0,
            }),
        });
        Ok(())
    }

    fn push_ignored_frame(&mut self, name: String, empty: bool) -> Result<(), ParserFault> {
        if empty {
            return Ok(());
        }
        self.ensure_depth()?;
        self.stack.push(Frame {
            name,
            kind: FrameKind::Ignored,
        });
        Ok(())
    }

    fn skip_invalid_start(
        &mut self,
        markup: &str,
        occurrence: Option<u64>,
    ) -> Result<(), ParserFault> {
        if markup_is_empty_element(markup) {
            return Ok(());
        }
        let Some(name) = lexical_element_name(markup) else {
            return Ok(());
        };
        if let Some(occurrence) = occurrence {
            self.push_invalid_frame(name.to_owned(), occurrence)
        } else {
            self.push_ignored_frame(name.to_owned(), false)
        }
    }

    fn push_ignored_lexical_start(&mut self, markup: &str) -> Result<(), ParserFault> {
        let Some(name) = lexical_element_name(markup) else {
            return Ok(());
        };
        self.push_ignored_frame(name.to_owned(), markup_is_empty_element(markup))
    }

    fn ensure_depth(&self) -> Result<(), ParserFault> {
        let observed = self.stack.len().saturating_add(1);
        if observed > self.contract.limits.max_depth {
            return Err(ParserFault::DepthLimitExceeded {
                maximum: self.contract.limits.max_depth,
                observed,
            });
        }
        Ok(())
    }

    fn allocate_occurrence(&mut self) -> Result<u64, ParserFault> {
        let observed = self.occurrence_count.saturating_add(1);
        if observed > self.contract.limits.max_occurrences {
            return Err(ParserFault::OccurrenceLimitExceeded {
                maximum: self.contract.limits.max_occurrences,
                observed,
            });
        }
        self.occurrence_count = observed;
        let occurrence = self.next_occurrence;
        self.next_occurrence = self.next_occurrence.saturating_add(1);
        Ok(occurrence)
    }

    fn occurrence_for_lexical_name(
        &mut self,
        name: Option<&str>,
    ) -> Result<Option<u64>, ParserFault> {
        let Some(name) = name else {
            return Ok(None);
        };
        self.occurrence_for_declared_name(name)
    }

    fn occurrence_for_declared_name(&mut self, name: &str) -> Result<Option<u64>, ParserFault> {
        if self.element_indexes.contains_key(name) && self.stack.is_empty() {
            return self.allocate_occurrence().map(Some);
        }
        Ok(self.active_occurrence())
    }

    fn active_occurrence(&self) -> Option<u64> {
        self.stack.iter().rev().find_map(|frame| match &frame.kind {
            FrameKind::Tracked(tracked) => Some(tracked.occurrence),
            FrameKind::Ignored => None,
        })
    }

    fn active_text_occurrence(&self) -> Option<u64> {
        let frame = self.stack.last()?;
        match &frame.kind {
            FrameKind::Tracked(tracked) if tracked.valid && tracked.form == ElementForm::Text => {
                Some(tracked.occurrence)
            }
            FrameKind::Tracked(_) | FrameKind::Ignored => None,
        }
    }

    fn active_element_name(&self) -> Option<&str> {
        self.stack.iter().rev().find_map(|frame| match &frame.kind {
            FrameKind::Tracked(_) => Some(frame.name.as_str()),
            FrameKind::Ignored => None,
        })
    }

    fn invalidate_active(&mut self) {
        if let Some(Frame {
            kind: FrameKind::Tracked(tracked),
            ..
        }) = self.stack.last_mut()
        {
            tracked.valid = false;
        }
    }

    fn is_suppressed(&self) -> bool {
        self.stack
            .iter()
            .any(|frame| matches!(&frame.kind, FrameKind::Ignored))
    }

    fn ignore_foreign_lexical_start(&self, name: Option<&str>) -> bool {
        self.stack.is_empty()
            && self.contract.unknown_top_level == UnknownTopLevel::IgnoreSubtree
            && name.is_some_and(|name| !self.element_indexes.contains_key(name))
    }
}

#[derive(Clone)]
enum EofAction {
    ImplicitComplete(u64),
    Invalidate { occurrence: u64, element: String },
}

#[derive(Debug)]
struct Frame {
    name: String,
    kind: FrameKind,
}

#[derive(Debug)]
enum FrameKind {
    Tracked(TrackedFrame),
    Ignored,
}

#[derive(Debug)]
struct TrackedFrame {
    occurrence: u64,
    form: ElementForm,
    valid: bool,
    accumulated: String,
    content_bytes: usize,
}

fn validate_limits(limits: ParserLimits) -> Result<(), ParserDeclarationFault> {
    for (limit, value) in [
        ("max_input_bytes", limits.max_input_bytes),
        ("max_depth", limits.max_depth),
        ("max_attributes", limits.max_attributes),
        ("max_content_bytes", limits.max_content_bytes),
        ("max_occurrences", limits.max_occurrences),
    ] {
        if value == 0 {
            return Err(ParserDeclarationFault::InvalidLimit { limit });
        }
    }
    Ok(())
}

fn invalid(occurrence: Option<u64>, kind: InvalidKind, span: Option<Range<usize>>) -> ParserEvent {
    invalid_for_element(occurrence, None, kind, span)
}

fn invalid_for_element(
    occurrence: Option<u64>,
    element: Option<&str>,
    kind: InvalidKind,
    span: Option<Range<usize>>,
) -> ParserEvent {
    ParserEvent::Invalid {
        occurrence,
        element: element.map(ToOwned::to_owned),
        kind,
        span,
    }
}

fn invalid_with_owned_element(
    occurrence: Option<u64>,
    element: Option<String>,
    kind: InvalidKind,
    span: Option<Range<usize>>,
) -> ParserEvent {
    ParserEvent::Invalid {
        occurrence,
        element,
        kind,
        span,
    }
}

#[derive(Debug)]
struct ParsedStart {
    name: String,
    attributes: Vec<(String, String)>,
    empty: bool,
}

#[derive(Debug)]
enum StartFault {
    Malformed,
    DuplicateAttribute,
    AttributeLimit { observed: usize },
}

fn parse_start(markup: &str, maximum_attributes: usize) -> Result<ParsedStart, StartFault> {
    let mut reader = Reader::from_str(markup);
    let event = reader.read_event().map_err(|_| StartFault::Malformed)?;
    let (start, empty) = match event {
        Event::Start(start) => (start, false),
        Event::Empty(start) => (start, true),
        _ => return Err(StartFault::Malformed),
    };
    let name = str::from_utf8(start.name().as_ref())
        .map_err(|_| StartFault::Malformed)?
        .to_owned();
    if !is_wire_xml_name(&name) {
        return Err(StartFault::Malformed);
    }

    let mut attributes = Vec::new();
    let mut names = HashSet::new();
    for (index, attribute) in start.attributes().with_checks(false).enumerate() {
        let observed = index.saturating_add(1);
        if observed > maximum_attributes {
            return Err(StartFault::AttributeLimit { observed });
        }
        let attribute = attribute.map_err(|_| StartFault::Malformed)?;
        let name = str::from_utf8(attribute.key.as_ref())
            .map_err(|_| StartFault::Malformed)?
            .to_owned();
        if !is_wire_xml_name(&name) {
            return Err(StartFault::Malformed);
        }
        if !names.insert(name.clone()) {
            return Err(StartFault::DuplicateAttribute);
        }
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| StartFault::Malformed)?
            .into_owned();
        if !value.chars().all(is_xml_char) {
            return Err(StartFault::Malformed);
        }
        attributes.push((name, value));
    }
    match reader.read_event().map_err(|_| StartFault::Malformed)? {
        Event::Eof => Ok(ParsedStart {
            name,
            attributes,
            empty,
        }),
        _ => Err(StartFault::Malformed),
    }
}

fn parse_end_name(markup: &str) -> Result<String, ()> {
    let lexical_name = lexical_element_name(markup).ok_or(())?;
    let wrapped = format!("<root>{markup}</root>");
    let mut reader = Reader::from_str(&wrapped);
    reader.config_mut().check_end_names = false;
    match reader.read_event().map_err(|_| ())? {
        Event::Start(_) => {}
        _ => return Err(()),
    }
    let end = match reader.read_event().map_err(|_| ())? {
        Event::End(end) => end,
        _ => return Err(()),
    };
    let name = str::from_utf8(end.name().as_ref())
        .map_err(|_| ())?
        .to_owned();
    if name != lexical_name || !is_wire_xml_name(&name) {
        return Err(());
    }
    Ok(name)
}

fn decode_text(raw: &str) -> Result<String, ()> {
    quick_xml::escape::unescape(raw)
        .map(|decoded| decoded.into_owned())
        .map_err(|_| ())
}

/// Returns how much of `raw` ends at a complete entity boundary. A trailing ampersand is retained
/// across chunks unless a markup/EOF boundary proves it malformed.
fn complete_text_len(raw: &str, boundary: bool) -> Option<usize> {
    let Some(last_ampersand) = raw.rfind('&') else {
        return Some(raw.len());
    };
    if raw[last_ampersand..].contains(';') || boundary {
        Some(raw.len())
    } else if last_ampersand == 0 {
        None
    } else {
        Some(last_ampersand)
    }
}

enum IncompleteScan {
    Delimited {
        terminator: &'static str,
        search_from: usize,
    },
    Tag {
        scan_from: usize,
        quote: Option<u8>,
    },
}

fn complete_markup_len(remainder: &str, incomplete: &mut Option<IncompleteScan>) -> Option<usize> {
    if let Some(scan) = incomplete {
        let completion = continue_incomplete_scan(remainder, scan);
        if completion.is_some() {
            *incomplete = None;
        }
        return completion;
    }

    const DELIMITED: [(&str, &str); 3] = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>")];
    for (prefix, terminator) in DELIMITED {
        if remainder.starts_with(prefix) {
            let mut scan = IncompleteScan::Delimited {
                terminator,
                search_from: prefix.len(),
            };
            let completion = continue_incomplete_scan(remainder, &mut scan);
            if completion.is_none() {
                *incomplete = Some(scan);
            }
            return completion;
        }
        if prefix.starts_with(remainder) {
            return None;
        }
    }

    let mut scan = IncompleteScan::Tag {
        scan_from: 0,
        quote: None,
    };
    let completion = continue_incomplete_scan(remainder, &mut scan);
    if completion.is_none() {
        *incomplete = Some(scan);
    }
    completion
}

fn continue_incomplete_scan(remainder: &str, scan: &mut IncompleteScan) -> Option<usize> {
    match scan {
        IncompleteScan::Delimited {
            terminator,
            search_from,
        } => {
            if let Some(relative) = remainder[*search_from..].find(*terminator) {
                return Some(*search_from + relative + terminator.len());
            }
            *search_from = remainder
                .len()
                .saturating_sub(terminator.len().saturating_sub(1));
            None
        }
        IncompleteScan::Tag { scan_from, quote } => {
            for (relative, byte) in remainder.as_bytes()[*scan_from..]
                .iter()
                .copied()
                .enumerate()
            {
                let index = *scan_from + relative;
                match (*quote, byte) {
                    (None, b'\'' | b'\"') => *quote = Some(byte),
                    (Some(open), close) if open == close => *quote = None,
                    (None, b'>') => return Some(index + 1),
                    _ => {}
                }
            }
            *scan_from = remainder.len();
            None
        }
    }
}

fn is_special_markup(markup: &str) -> bool {
    markup.starts_with("<!--")
        || markup.starts_with("<![CDATA[")
        || markup.starts_with("<?")
        || markup.starts_with("<!")
}

fn lexical_element_name(markup: &str) -> Option<&str> {
    let body = markup.strip_prefix('<')?;
    let body = body.strip_prefix('/').unwrap_or(body);
    let end = body
        .bytes()
        .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        .unwrap_or(body.len());
    (end > 0).then_some(&body[..end])
}

fn markup_is_empty_element(markup: &str) -> bool {
    markup
        .strip_suffix('>')
        .is_some_and(|body| body.trim_end().ends_with('/'))
}

fn is_unqualified_xml_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn is_wire_xml_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_' || first == b':')
        && bytes
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn is_xml_char(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&character)
        || ('\u{E000}'..='\u{FFFD}').contains(&character)
        || ('\u{10000}'..='\u{10FFFF}').contains(&character)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_contract() -> Contract {
        Contract::strict(vec![ElementSpec {
            name: "say",
            form: ElementForm::Text,
        }])
    }

    fn empty_contract() -> Contract {
        Contract::strict(vec![ElementSpec {
            name: "pick",
            form: ElementForm::SelfClosing,
        }])
    }

    #[test]
    fn decodes_chunk_split_entities_with_lexical_spans() {
        let mut parser = Parser::new(text_contract()).unwrap();
        assert!(parser.push("<sa").unwrap().is_empty());

        let events = parser.push("y>hi &a").unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                ParserEvent::Open {
                    occurrence: 1,
                    span,
                    self_closing: false,
                    ..
                },
                ParserEvent::Delta { delta, span: delta_span, .. },
            ] if span == &(0..5) && delta == "hi " && delta_span == &(5..8)
        ));

        let events = parser.push("mp;世</say>").unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                ParserEvent::Delta {
                    occurrence: 1,
                    delta,
                    accumulated,
                    span,
                },
                ParserEvent::Complete {
                    occurrence: 1,
                    form: CompletionForm::ExplicitClose,
                    span: complete_span,
                },
            ] if delta == "&世" && accumulated.as_ref() == "hi &世" && span == &(8..16) && complete_span == &(16..22)
        ));
        assert!(parser.finish_normal().unwrap().is_empty());
        assert_eq!(parser.raw_output(), "<say>hi &amp;世</say>");
    }

    #[test]
    fn repeated_self_closing_occurrences_keep_distinct_ids_and_spans() {
        let mut parser = Parser::new(empty_contract()).unwrap();
        let events = parser.push("<pick/><pick/>").unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                ParserEvent::Open { occurrence: 1, span: first_open, .. },
                ParserEvent::Complete { occurrence: 1, form: CompletionForm::SelfClosing, span: first_close },
                ParserEvent::Open { occurrence: 2, span: second_open, .. },
                ParserEvent::Complete { occurrence: 2, form: CompletionForm::SelfClosing, span: second_close },
            ] if first_open == &(0..7) && first_close == &(0..7) && second_open == &(7..14) && second_close == &(7..14)
        ));
    }

    #[test]
    fn ignore_unknown_top_level_skips_its_complete_subtree() {
        let mut contract = empty_contract();
        contract.unknown_top_level = UnknownTopLevel::IgnoreSubtree;
        let mut parser = Parser::new(contract).unwrap();
        let events = parser.push("<wrapper><pick/></wrapper><pick/>").unwrap();

        assert!(matches!(
            events.as_slice(),
            [
                ParserEvent::Open { occurrence: 1, span, .. },
                ParserEvent::Complete { occurrence: 1, .. },
            ] if span == &(26..33)
        ));
        assert!(parser.finish_normal().unwrap().is_empty());
    }

    #[test]
    fn ignored_foreign_subtrees_do_not_leak_their_schema_diagnostics() {
        let mut contract = empty_contract();
        contract.unknown_top_level = UnknownTopLevel::IgnoreSubtree;
        let mut parser = Parser::new(contract).unwrap();
        let events = parser
            .push(r#"<foreign xmlns="urn:x" x="1" x="2"><pick/></foreign><pick/>"#)
            .unwrap();

        assert!(matches!(
            events.as_slice(),
            [
                ParserEvent::Open {
                    occurrence: 1,
                    name,
                    ..
                },
                ParserEvent::Complete { occurrence: 1, .. },
            ] if name == "pick"
        ));
    }

    #[test]
    fn strict_unknown_top_level_is_a_diagnostic_and_not_a_fault() {
        let mut parser = Parser::new(empty_contract()).unwrap();
        assert_eq!(
            parser.push("<other/>").unwrap(),
            vec![invalid_for_element(
                None,
                Some("other"),
                InvalidKind::UnknownElement,
                Some(0..8),
            )]
        );
    }

    #[test]
    fn text_element_rejects_nested_markup_without_a_completion() {
        let mut parser = Parser::new(text_contract()).unwrap();
        let events = parser.push("<say>one<b/>two</say>").unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                ParserEvent::Open { occurrence: 1, .. },
                ParserEvent::Delta { occurrence: 1, delta, .. },
                ParserEvent::Invalid { occurrence: Some(1), kind: InvalidKind::NestedMarkup, span: Some(span), .. },
            ] if delta == "one" && span == &(8..12)
        ));
    }

    #[test]
    fn duplicate_attributes_are_model_diagnostics() {
        let mut parser = Parser::new(empty_contract()).unwrap();
        assert_eq!(
            parser.push(r#"<pick x="1" x="2"/>"#).unwrap(),
            vec![invalid_for_element(
                Some(1),
                Some("pick"),
                InvalidKind::DuplicateAttribute,
                Some(0..19),
            )]
        );
    }

    #[test]
    fn normal_eof_is_distinct_from_abort_and_implicit_eof_is_opt_in() {
        let mut strict = Parser::new(text_contract()).unwrap();
        let events = strict.push("<say>hello").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            strict.finish_normal().unwrap(),
            vec![invalid_for_element(
                Some(1),
                Some("say"),
                InvalidKind::IncompleteElement,
                None,
            )]
        );

        let mut lax_contract = text_contract();
        lax_contract.allow_implicit_text_eof = true;
        let mut lax = Parser::new(lax_contract).unwrap();
        lax.push("<say>hello").unwrap();
        assert_eq!(
            lax.finish_normal().unwrap(),
            vec![ParserEvent::Complete {
                occurrence: 1,
                form: CompletionForm::ImplicitEof,
                span: 10..10,
            }]
        );

        let mut aborted = Parser::new(text_contract()).unwrap();
        aborted.push("<say>hello").unwrap();
        aborted.abort();
        assert_eq!(aborted.finish_normal(), Err(ParserFault::Aborted));
    }

    #[test]
    fn limits_are_hard_faults() {
        let mut occurrences = empty_contract();
        occurrences.limits.max_occurrences = 1;
        let mut parser = Parser::new(occurrences).unwrap();
        assert!(matches!(
            parser.push("<pick/><pick/>"),
            Err(ParserFault::OccurrenceLimitExceeded {
                maximum: 1,
                observed: 2,
            })
        ));

        let mut content = text_contract();
        content.limits.max_content_bytes = 2;
        let mut parser = Parser::new(content).unwrap();
        assert!(matches!(
            parser.push("<say>abc"),
            Err(ParserFault::ContentLimitExceeded {
                occurrence: 1,
                maximum: 2,
                observed: 3,
            })
        ));

        let mut input = empty_contract();
        input.limits.max_input_bytes = 1;
        let mut parser = Parser::new(input).unwrap();
        assert!(matches!(
            parser.push("ab"),
            Err(ParserFault::InputLimitExceeded {
                maximum: 1,
                observed: 2,
            })
        ));

        let mut attributes = empty_contract();
        attributes.limits.max_attributes = 1;
        let mut parser = Parser::new(attributes).unwrap();
        assert!(matches!(
            parser.push(r#"<pick a="1" b="2"/>"#),
            Err(ParserFault::AttributeLimitExceeded {
                maximum: 1,
                observed: 2,
            })
        ));

        let mut depth = empty_contract();
        depth.unknown_top_level = UnknownTopLevel::IgnoreSubtree;
        depth.limits.max_depth = 1;
        let mut parser = Parser::new(depth).unwrap();
        assert!(matches!(
            parser.push("<foreign><inner>"),
            Err(ParserFault::DepthLimitExceeded {
                maximum: 1,
                observed: 2,
            })
        ));
    }
}
