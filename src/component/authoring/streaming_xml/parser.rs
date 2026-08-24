use std::{collections::HashMap, str, sync::Arc};

use quick_xml::{events::Event, reader::Reader, XmlVersion};

use crate::llm_call::TextTurnEvent;

use super::{StreamingXmlDispatchFault, StreamingXmlMountFault, XmlContractDiagnostic};
use crate::component::authoring::event_input::EventRouteDescriptor;

const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ELEMENT_DEPTH: usize = 256;
const MAX_ATTRIBUTES_PER_ELEMENT: usize = 256;
const MAX_TARGET_EVENTS: usize = 4_096;

pub(crate) struct ContractRegistration {
    pub(crate) listener_index: usize,
    pub(crate) identity: &'static str,
    pub(crate) element: &'static str,
    pub(crate) attribute: &'static str,
}

pub(crate) enum ParsedContractEvent {
    Decoded {
        listener_index: usize,
        value: String,
    },
    Invalid {
        listener_index: usize,
        diagnostic: XmlContractDiagnostic,
    },
}

struct TargetState {
    registration: ContractRegistration,
    occurrences: usize,
}

pub(crate) struct MountedStreamingRoute {
    route: Arc<EventRouteDescriptor>,
    targets: Vec<TargetState>,
    targets_by_element: HashMap<&'static str, usize>,
    raw_output: String,
    pending: String,
    incomplete_scan: Option<IncompleteScan>,
    namespace_stack: Vec<NamespaceFrame>,
    target_events: usize,
    text_complete: bool,
    finished: bool,
}

impl MountedStreamingRoute {
    pub(crate) fn new(route: Arc<EventRouteDescriptor>) -> Self {
        Self {
            route,
            targets: Vec::new(),
            targets_by_element: HashMap::new(),
            raw_output: String::new(),
            pending: String::new(),
            incomplete_scan: None,
            namespace_stack: Vec::new(),
            target_events: 0,
            text_complete: false,
            finished: false,
        }
    }

    pub(crate) fn register(
        &mut self,
        registration: ContractRegistration,
    ) -> Result<(), StreamingXmlMountFault> {
        if let Some(first) = self.targets_by_element.get(registration.element).copied() {
            return Err(StreamingXmlMountFault::DuplicateTarget {
                element: registration.element,
                first: self.targets[first].registration.identity,
                second: registration.identity,
            });
        }
        let index = self.targets.len();
        self.targets_by_element.insert(registration.element, index);
        self.targets.push(TargetState {
            registration,
            occurrences: 0,
        });
        Ok(())
    }

    pub(crate) fn dispatch_root<Root>(
        &mut self,
        root: &Root,
    ) -> Result<Vec<ParsedContractEvent>, StreamingXmlDispatchFault>
    where
        Root: Send + Sync + 'static,
    {
        let Some(event) = self.route.project_root(root)? else {
            return Ok(Vec::new());
        };
        let event = event.downcast_ref::<TextTurnEvent>().ok_or(
            StreamingXmlDispatchFault::EventTypeMismatch {
                contract: self
                    .targets
                    .first()
                    .map_or("unknown", |target| target.registration.identity),
            },
        )?;
        self.on_text(event)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ParsedContractEvent>, StreamingXmlDispatchFault> {
        if self.finished {
            return Err(StreamingXmlDispatchFault::RouteAlreadyFinished);
        }
        if !self.text_complete {
            return Err(StreamingXmlDispatchFault::RouteMissingTextComplete);
        }

        let incomplete = self.incomplete_target();
        let mut events = Vec::new();
        for (index, target) in self.targets.iter().enumerate() {
            let diagnostic = if incomplete == Some(index) {
                Some(XmlContractDiagnostic::IncompleteElement {
                    contract: target.registration.identity,
                    element: target.registration.element,
                })
            } else if target.occurrences != 1 {
                Some(XmlContractDiagnostic::OccurrenceCount {
                    contract: target.registration.identity,
                    expected: 1,
                    observed: target.occurrences,
                })
            } else {
                None
            };
            if let Some(diagnostic) = diagnostic {
                events.push(ParsedContractEvent::Invalid {
                    listener_index: target.registration.listener_index,
                    diagnostic,
                });
            }
        }
        self.finished = true;
        Ok(events)
    }

    fn on_text(
        &mut self,
        event: &TextTurnEvent,
    ) -> Result<Vec<ParsedContractEvent>, StreamingXmlDispatchFault> {
        if self.finished || self.text_complete {
            return Err(StreamingXmlDispatchFault::RouteTextAfterCompletion);
        }
        match event {
            TextTurnEvent::TextDelta(chunk) => self.append(chunk)?,
            TextTurnEvent::TextComplete(output) => {
                if !output.starts_with(&self.raw_output) {
                    return Err(StreamingXmlDispatchFault::RouteCompletionMismatch {
                        delta_bytes: self.raw_output.len(),
                        completion_bytes: output.len(),
                    });
                }
                let suffix = &output[self.raw_output.len()..];
                self.append(suffix)?;
                self.text_complete = true;
            }
        }
        self.process_pending()
    }

    fn append(&mut self, chunk: &str) -> Result<(), StreamingXmlDispatchFault> {
        let observed = self.raw_output.len().saturating_add(chunk.len());
        if observed > MAX_TEXT_BYTES {
            return Err(StreamingXmlDispatchFault::InputLimitExceeded {
                maximum: MAX_TEXT_BYTES,
                observed,
            });
        }
        self.raw_output.push_str(chunk);
        self.pending.push_str(chunk);
        Ok(())
    }

    fn process_pending(&mut self) -> Result<Vec<ParsedContractEvent>, StreamingXmlDispatchFault> {
        let mut events = Vec::new();
        let mut cursor = 0;
        loop {
            let Some(relative_start) = self.pending[cursor..].find('<') else {
                cursor = self.pending.len();
                break;
            };
            cursor += relative_start;
            let remainder = &self.pending[cursor..];
            if remainder.len() > 1 && !can_start_markup(remainder.as_bytes()[1]) {
                cursor += 1;
                continue;
            }
            let Some(completion) = complete_markup_len(remainder, &mut self.incomplete_scan) else {
                break;
            };
            let length = match completion {
                ScanCompletion::Markup(length) => length,
                ScanCompletion::RawLessThan(next_start) => {
                    let malformed_prefix = self.pending[cursor..cursor + next_start].to_owned();
                    self.record_malformed_lexical_target(
                        lexical_element_name(&malformed_prefix),
                        &mut events,
                    )?;
                    cursor += next_start;
                    continue;
                }
            };
            let markup = self.pending[cursor..cursor + length].to_owned();
            self.process_markup(&markup, &mut events)?;
            cursor += length;
        }
        self.pending.drain(..cursor);
        Ok(events)
    }

    fn process_markup(
        &mut self,
        markup: &str,
        events: &mut Vec<ParsedContractEvent>,
    ) -> Result<(), StreamingXmlDispatchFault> {
        if is_ignored_markup(markup) {
            return Ok(());
        }
        if markup.starts_with("</") {
            if let (Ok(name), Some(frame)) = (parse_end_name(markup), self.namespace_stack.last()) {
                if frame.element_name == name {
                    self.namespace_stack.pop();
                }
            }
            return Ok(());
        }

        let lexical_name = lexical_element_name(markup);
        let parsed = match parse_start(markup) {
            Ok(parsed) => parsed,
            Err(ParseStartFault::Malformed) => {
                self.record_malformed_lexical_target(lexical_name, events)?;
                return Ok(());
            }
            Err(ParseStartFault::AttributeLimit { maximum, observed }) => {
                return Err(StreamingXmlDispatchFault::AttributeLimitExceeded {
                    maximum,
                    observed,
                });
            }
        };
        let inherited_namespace = self
            .namespace_stack
            .last()
            .and_then(|frame| frame.default_namespace.clone());
        let declared_namespace = parsed
            .attributes
            .iter()
            .find(|attribute| attribute.name == "xmlns")
            .map(|attribute| {
                (!attribute.value.is_empty()).then(|| Arc::<str>::from(attribute.value.as_str()))
            });
        let effective_namespace = declared_namespace.unwrap_or(inherited_namespace);

        if !parsed.empty {
            let observed = self.namespace_stack.len() + 1;
            if observed > MAX_ELEMENT_DEPTH {
                return Err(StreamingXmlDispatchFault::ElementDepthLimitExceeded {
                    maximum: MAX_ELEMENT_DEPTH,
                    observed,
                });
            }
            self.namespace_stack.push(NamespaceFrame {
                element_name: parsed.name.clone(),
                default_namespace: effective_namespace.clone(),
            });
        }
        if parsed.name.contains(':') || effective_namespace.is_some() {
            return Ok(());
        }
        let Some(target_index) = self.targets_by_element.get(parsed.name.as_str()).copied() else {
            return Ok(());
        };
        self.record_target_occurrence(target_index)?;
        let registration = &self.targets[target_index].registration;
        if !parsed.empty {
            events.push(invalid_event(
                registration,
                "target element must use empty-element syntax",
            ));
            return Ok(());
        }

        let mut value = None;
        for attribute in parsed.attributes {
            if attribute.name != registration.attribute {
                events.push(invalid_event(
                    registration,
                    format!("unknown attribute `{}`", attribute.name),
                ));
                return Ok(());
            }
            value = Some(attribute.value);
        }
        match value {
            Some(value) => events.push(ParsedContractEvent::Decoded {
                listener_index: registration.listener_index,
                value,
            }),
            None => events.push(invalid_event(
                registration,
                format!("missing attribute `{}`", registration.attribute),
            )),
        }
        Ok(())
    }

    fn record_malformed_lexical_target(
        &mut self,
        lexical_name: Option<&str>,
        events: &mut Vec<ParsedContractEvent>,
    ) -> Result<(), StreamingXmlDispatchFault> {
        if self
            .namespace_stack
            .last()
            .is_some_and(|frame| frame.default_namespace.is_some())
        {
            return Ok(());
        }
        let Some(name) = lexical_name else {
            return Ok(());
        };
        let Some(index) = self.targets_by_element.get(name).copied() else {
            return Ok(());
        };
        self.record_target_occurrence(index)?;
        events.push(invalid_event(
            &self.targets[index].registration,
            "malformed XML target element",
        ));
        Ok(())
    }

    fn record_target_occurrence(
        &mut self,
        target_index: usize,
    ) -> Result<(), StreamingXmlDispatchFault> {
        let observed = self.target_events + 1;
        if observed > MAX_TARGET_EVENTS {
            return Err(StreamingXmlDispatchFault::TargetEventLimitExceeded {
                maximum: MAX_TARGET_EVENTS,
                observed,
            });
        }
        self.target_events = observed;
        self.targets[target_index].occurrences += 1;
        Ok(())
    }

    fn incomplete_target(&self) -> Option<usize> {
        let remainder = self.pending.trim_start();
        if remainder.is_empty() || is_special_markup_prefix(remainder) {
            return None;
        }
        if self
            .namespace_stack
            .last()
            .is_some_and(|frame| frame.default_namespace.is_some())
        {
            return None;
        }
        self.targets.iter().position(|target| {
            let prefix = format!("<{}", target.registration.element);
            prefix.starts_with(remainder)
                || remainder.starts_with(&prefix)
                    && remainder.as_bytes().get(prefix.len()).is_none_or(|byte| {
                        byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
                    })
        })
    }
}

struct NamespaceFrame {
    element_name: String,
    default_namespace: Option<Arc<str>>,
}

fn invalid_event(
    registration: &ContractRegistration,
    detail: impl Into<String>,
) -> ParsedContractEvent {
    ParsedContractEvent::Invalid {
        listener_index: registration.listener_index,
        diagnostic: XmlContractDiagnostic::MalformedElement {
            contract: registration.identity,
            detail: detail.into(),
        },
    }
}

struct ParsedStart {
    name: String,
    attributes: Vec<ParsedAttribute>,
    empty: bool,
}

struct ParsedAttribute {
    name: String,
    value: String,
}

enum ParseStartFault {
    Malformed,
    AttributeLimit { maximum: usize, observed: usize },
}

fn parse_start(markup: &str) -> Result<ParsedStart, ParseStartFault> {
    let mut reader = Reader::from_str(markup);
    let event = reader
        .read_event()
        .map_err(|_| ParseStartFault::Malformed)?;
    let (start, empty) = match event {
        Event::Start(start) => (start, false),
        Event::Empty(start) => (start, true),
        _ => {
            return Err(ParseStartFault::Malformed);
        }
    };
    let name = str::from_utf8(start.name().as_ref())
        .map_err(|_| ParseStartFault::Malformed)?
        .to_owned();
    let mut attributes = Vec::new();
    for (index, attribute) in start.attributes().with_checks(true).enumerate() {
        if index >= MAX_ATTRIBUTES_PER_ELEMENT {
            return Err(ParseStartFault::AttributeLimit {
                maximum: MAX_ATTRIBUTES_PER_ELEMENT,
                observed: index + 1,
            });
        }
        let attribute = attribute.map_err(|_| ParseStartFault::Malformed)?;
        if attribute.value.as_ref().contains(&b'<') {
            return Err(ParseStartFault::Malformed);
        }
        let name = str::from_utf8(attribute.key.as_ref())
            .map_err(|_| ParseStartFault::Malformed)?
            .to_owned();
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| ParseStartFault::Malformed)?
            .into_owned();
        if !value.chars().all(is_xml_char) {
            return Err(ParseStartFault::Malformed);
        }
        attributes.push(ParsedAttribute { name, value });
    }
    Ok(ParsedStart {
        name,
        attributes,
        empty,
    })
}

fn parse_end_name(markup: &str) -> Result<String, String> {
    let lexical_name = lexical_element_name(markup)
        .ok_or_else(|| String::from("end element is missing a name"))?;
    let wrapped = format!("<{lexical_name}>{markup}");
    let mut reader = Reader::from_str(&wrapped);
    let start = match reader.read_event().map_err(|fault| fault.to_string())? {
        Event::Start(start) => start,
        _ => return Err(String::from("synthetic start element was not parsed")),
    };
    let start_name = str::from_utf8(start.name().as_ref())
        .map_err(|fault| fault.to_string())?
        .to_owned();
    let end = match reader.read_event().map_err(|fault| fault.to_string())? {
        Event::End(end) => end,
        _ => return Err(String::from("markup is not an end element")),
    };
    let end_name = str::from_utf8(end.name().as_ref())
        .map_err(|fault| fault.to_string())?
        .to_owned();
    if start_name != end_name {
        return Err(String::from("end element name does not match"));
    }
    match reader.read_event().map_err(|fault| fault.to_string())? {
        Event::Eof => Ok(end_name),
        _ => Err(String::from("trailing content in end element")),
    }
}

fn is_xml_char(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&character)
        || ('\u{E000}'..='\u{FFFD}').contains(&character)
        || ('\u{10000}'..='\u{10FFFF}').contains(&character)
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

enum ScanCompletion {
    Markup(usize),
    RawLessThan(usize),
}

fn complete_markup_len(
    remainder: &str,
    incomplete: &mut Option<IncompleteScan>,
) -> Option<ScanCompletion> {
    if let Some(scan) = incomplete {
        let complete = continue_incomplete_scan(remainder, scan);
        if complete.is_some() {
            *incomplete = None;
        }
        return complete;
    }

    const DELIMITED: [(&str, &str); 3] = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>")];
    for (prefix, terminator) in DELIMITED {
        if remainder.starts_with(prefix) {
            let mut scan = IncompleteScan::Delimited {
                terminator,
                search_from: prefix.len(),
            };
            let complete = continue_incomplete_scan(remainder, &mut scan);
            if complete.is_none() {
                *incomplete = Some(scan);
            }
            return complete;
        }
        if prefix.starts_with(remainder) {
            return None;
        }
    }

    let mut scan = IncompleteScan::Tag {
        scan_from: 0,
        quote: None,
    };
    let complete = continue_incomplete_scan(remainder, &mut scan);
    if complete.is_none() {
        *incomplete = Some(scan);
    }
    complete
}

fn continue_incomplete_scan(remainder: &str, scan: &mut IncompleteScan) -> Option<ScanCompletion> {
    match scan {
        IncompleteScan::Delimited {
            terminator,
            search_from,
        } => {
            if let Some(offset) = remainder[*search_from..].find(*terminator) {
                return Some(ScanCompletion::Markup(
                    *search_from + offset + terminator.len(),
                ));
            }
            *search_from = floor_char_boundary(
                remainder,
                remainder.len().saturating_sub(terminator.len() - 1),
            );
        }
        IncompleteScan::Tag { scan_from, quote } => {
            for (relative, byte) in remainder.as_bytes()[*scan_from..]
                .iter()
                .copied()
                .enumerate()
            {
                let index = *scan_from + relative;
                if byte == b'<' && index > 0 {
                    return Some(ScanCompletion::RawLessThan(index));
                }
                match (*quote, byte) {
                    (None, b'\'' | b'"') => *quote = Some(byte),
                    (Some(open), close) if open == close => *quote = None,
                    (None, b'>') => return Some(ScanCompletion::Markup(index + 1)),
                    _ => {}
                }
            }
            *scan_from = remainder.len();
        }
    }
    None
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn lexical_element_name(markup: &str) -> Option<&str> {
    let body = markup
        .strip_prefix('<')?
        .strip_prefix('/')
        .unwrap_or(&markup[1..]);
    let end = body
        .bytes()
        .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        .unwrap_or(body.len());
    (end > 0).then_some(&body[..end])
}

fn is_ignored_markup(markup: &str) -> bool {
    markup.starts_with("<!--")
        || markup.starts_with("<![CDATA[")
        || markup.starts_with("<?")
        || markup.starts_with("<!")
}

fn is_special_markup_prefix(markup: &str) -> bool {
    ["<!--", "<![CDATA[", "<?", "<!"]
        .iter()
        .any(|prefix| markup.starts_with(prefix) || prefix.starts_with(markup))
}

fn can_start_markup(next: u8) -> bool {
    next.is_ascii_alphabetic()
        || !next.is_ascii()
        || matches!(next, b'_' | b':' | b'/' | b'!' | b'?')
}
