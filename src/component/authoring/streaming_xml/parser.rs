use std::{collections::HashMap, str, sync::Arc};

use quick_xml::{events::Event, reader::Reader, XmlVersion};

use crate::llm_call::TextTurnEvent;
use crate::stream_parser::XmlElement;

use super::{StreamingXmlDispatchFault, StreamingXmlMountFault, XmlContractDiagnostic};
use crate::component::authoring::event_input::EventRouteDescriptor;

const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ELEMENT_DEPTH: usize = 256;
const MAX_ATTRIBUTES_PER_ELEMENT: usize = 256;

pub(crate) struct ContractRegistration {
    pub(crate) listener_index: usize,
    pub(crate) identity: &'static str,
    pub(crate) element: &'static str,
    pub(crate) kind: StreamingRegistrationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamingRegistrationKind {
    EmptyToolCall {
        attribute: Option<&'static str>,
    },
    Lifecycle {
        open: bool,
        stream: bool,
        complete: bool,
        invalid: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamingXmlPhase {
    Open,
    Stream,
    Complete,
}

impl StreamingXmlPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Stream => "stream",
            Self::Complete => "complete",
        }
    }
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
    Lifecycle {
        listener_index: usize,
        phase: StreamingXmlPhase,
        element: XmlElement,
    },
}

struct TargetState {
    element: &'static str,
    registrations: Vec<ContractRegistration>,
}

struct TrackedElement {
    target_index: usize,
    attributes: HashMap<String, String>,
    content_start: usize,
}

pub(crate) struct MountedStreamingRoute {
    route: Arc<EventRouteDescriptor>,
    targets: Vec<TargetState>,
    targets_by_element: HashMap<&'static str, usize>,
    raw_output: String,
    pending: String,
    pending_offset: usize,
    incomplete_scan: Option<IncompleteScan>,
    namespace_stack: Vec<NamespaceFrame>,
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
            pending_offset: 0,
            incomplete_scan: None,
            namespace_stack: Vec::new(),
            text_complete: false,
            finished: false,
        }
    }

    pub(crate) fn register(
        &mut self,
        registration: ContractRegistration,
    ) -> Result<(), StreamingXmlMountFault> {
        if let Some(target_index) = self.targets_by_element.get(registration.element).copied() {
            self.targets[target_index].registrations.push(registration);
            return Ok(());
        }
        let index = self.targets.len();
        self.targets_by_element.insert(registration.element, index);
        self.targets.push(TargetState {
            element: registration.element,
            registrations: vec![registration],
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
                    .and_then(|target| target.registrations.first())
                    .map_or("unknown", |registration| registration.identity),
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

        let mut events = Vec::new();
        for frame in &self.namespace_stack {
            let Some(tracked) = &frame.tracked else {
                continue;
            };
            self.push_incomplete_events(tracked.target_index, true, &mut events);
        }
        let pending = self.pending.trim_start();
        let pending_is_tracked_close = self.namespace_stack.iter().rev().any(|frame| {
            frame.tracked.is_some() && incomplete_close_prefix(pending, frame.element_name.as_str())
        });
        if !pending_is_tracked_close {
            if let Some(target_index) = self.incomplete_target() {
                self.push_incomplete_events(target_index, false, &mut events);
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
        let appended = match event {
            TextTurnEvent::TextDelta(chunk) => self.append(chunk)?,
            TextTurnEvent::TextComplete(output) => {
                if !output.starts_with(&self.raw_output) {
                    return Err(StreamingXmlDispatchFault::RouteCompletionMismatch {
                        delta_bytes: self.raw_output.len(),
                        completion_bytes: output.len(),
                    });
                }
                let suffix = &output[self.raw_output.len()..];
                let appended = self.append(suffix)?;
                self.text_complete = true;
                appended
            }
        };
        let mut events = self.process_pending()?;
        if appended {
            self.push_stream_events(&mut events);
        }
        Ok(events)
    }

    fn append(&mut self, chunk: &str) -> Result<bool, StreamingXmlDispatchFault> {
        let observed = self.raw_output.len().saturating_add(chunk.len());
        if observed > MAX_TEXT_BYTES {
            return Err(StreamingXmlDispatchFault::InputLimitExceeded {
                maximum: MAX_TEXT_BYTES,
                observed,
            });
        }
        self.raw_output.push_str(chunk);
        self.pending.push_str(chunk);
        Ok(!chunk.is_empty())
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
                    );
                    cursor += next_start;
                    continue;
                }
            };
            let markup = self.pending[cursor..cursor + length].to_owned();
            let markup_start = self.pending_offset + cursor;
            self.process_markup(&markup, markup_start, &mut events)?;
            cursor += length;
        }
        self.pending.drain(..cursor);
        self.pending_offset += cursor;
        Ok(events)
    }

    fn process_markup(
        &mut self,
        markup: &str,
        markup_start: usize,
        events: &mut Vec<ParsedContractEvent>,
    ) -> Result<(), StreamingXmlDispatchFault> {
        if is_ignored_markup(markup) {
            return Ok(());
        }
        if markup.starts_with("</") {
            let parsed_name = parse_end_name(markup).ok();
            if let (Some(name), Some(frame)) = (parsed_name.as_deref(), self.namespace_stack.last())
            {
                if frame.element_name == name {
                    let frame = self
                        .namespace_stack
                        .pop()
                        .expect("the matching namespace frame exists");
                    if let Some(tracked) = frame.tracked {
                        let content =
                            self.raw_output[tracked.content_start..markup_start].to_owned();
                        self.push_lifecycle_events(
                            tracked.target_index,
                            StreamingXmlPhase::Complete,
                            &tracked.attributes,
                            content,
                            events,
                        );
                    }
                    return Ok(());
                }
            }
            self.record_malformed_lexical_target(
                parsed_name
                    .as_deref()
                    .or_else(|| lexical_element_name(markup)),
                events,
            );
            return Ok(());
        }

        let lexical_name = lexical_element_name(markup);
        let parsed = match parse_start(markup) {
            Ok(parsed) => parsed,
            Err(ParseStartFault::Malformed) => {
                self.record_malformed_lexical_target(lexical_name, events);
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
        }
        let target_index = (!parsed.name.contains(':') && effective_namespace.is_none())
            .then(|| self.targets_by_element.get(parsed.name.as_str()).copied())
            .flatten();
        let attributes = parsed
            .attributes
            .iter()
            .map(|attribute| (attribute.name.clone(), attribute.value.clone()))
            .collect::<HashMap<_, _>>();

        if let Some(target_index) = target_index {
            self.push_start_events(target_index, &parsed, &attributes, events);
        }

        if !parsed.empty {
            self.namespace_stack.push(NamespaceFrame {
                element_name: parsed.name,
                default_namespace: effective_namespace,
                tracked: target_index.map(|target_index| TrackedElement {
                    target_index,
                    attributes,
                    content_start: markup_start + markup.len(),
                }),
            });
        }
        Ok(())
    }

    fn push_start_events(
        &self,
        target_index: usize,
        parsed: &ParsedStart,
        attributes: &HashMap<String, String>,
        events: &mut Vec<ParsedContractEvent>,
    ) {
        let target = &self.targets[target_index];
        for registration in &target.registrations {
            if matches!(
                registration.kind,
                StreamingRegistrationKind::Lifecycle { open: true, .. }
            ) {
                events.push(lifecycle_event(
                    registration,
                    StreamingXmlPhase::Open,
                    target.element,
                    attributes,
                    String::new(),
                ));
            }
        }
        for registration in &target.registrations {
            match registration.kind {
                StreamingRegistrationKind::EmptyToolCall { attribute } if parsed.empty => {
                    events.push(tool_call_event(registration, attribute, &parsed.attributes));
                }
                StreamingRegistrationKind::EmptyToolCall { .. } => events.push(invalid_event(
                    registration,
                    "target element must use empty-element syntax",
                )),
                StreamingRegistrationKind::Lifecycle { complete: true, .. } if parsed.empty => {
                    events.push(lifecycle_event(
                        registration,
                        StreamingXmlPhase::Complete,
                        target.element,
                        attributes,
                        String::new(),
                    ));
                }
                StreamingRegistrationKind::Lifecycle { .. } => {}
            }
        }
    }

    fn push_lifecycle_events(
        &self,
        target_index: usize,
        phase: StreamingXmlPhase,
        attributes: &HashMap<String, String>,
        content: String,
        events: &mut Vec<ParsedContractEvent>,
    ) {
        let target = &self.targets[target_index];
        for registration in &target.registrations {
            let interested = match registration.kind {
                StreamingRegistrationKind::Lifecycle {
                    open,
                    stream,
                    complete,
                    ..
                } => match phase {
                    StreamingXmlPhase::Open => open,
                    StreamingXmlPhase::Stream => stream,
                    StreamingXmlPhase::Complete => complete,
                },
                StreamingRegistrationKind::EmptyToolCall { .. } => false,
            };
            if interested {
                events.push(lifecycle_event(
                    registration,
                    phase,
                    target.element,
                    attributes,
                    content.clone(),
                ));
            }
        }
    }

    fn push_stream_events(&self, events: &mut Vec<ParsedContractEvent>) {
        for (index, frame) in self.namespace_stack.iter().enumerate() {
            let Some(tracked) = &frame.tracked else {
                continue;
            };
            let content_end = if index + 1 == self.namespace_stack.len()
                && incomplete_close_prefix(&self.pending, &frame.element_name)
            {
                self.pending_offset
            } else {
                self.raw_output.len()
            };
            if content_end < tracked.content_start {
                continue;
            }
            self.push_lifecycle_events(
                tracked.target_index,
                StreamingXmlPhase::Stream,
                &tracked.attributes,
                self.raw_output[tracked.content_start..content_end].to_owned(),
                events,
            );
        }
    }

    fn record_malformed_lexical_target(
        &mut self,
        lexical_name: Option<&str>,
        events: &mut Vec<ParsedContractEvent>,
    ) {
        if self
            .namespace_stack
            .last()
            .is_some_and(|frame| frame.default_namespace.is_some())
        {
            return;
        }
        let Some(name) = lexical_name else {
            return;
        };
        let Some(index) = self.targets_by_element.get(name).copied() else {
            return;
        };
        for registration in &self.targets[index].registrations {
            if registration_receives_invalid(registration) {
                events.push(invalid_event(registration, "malformed XML target element"));
            }
        }
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
        let mut candidates = self
            .targets
            .iter()
            .enumerate()
            .filter_map(|(index, target)| {
                [
                    format!("<{}", target.element),
                    format!("</{}", target.element),
                ]
                .into_iter()
                .any(|prefix| {
                    prefix.starts_with(remainder)
                        || remainder.starts_with(&prefix)
                            && remainder.as_bytes().get(prefix.len()).is_none_or(|byte| {
                                byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
                            })
                })
                .then_some(index)
            });
        let candidate = candidates.next()?;
        candidates.next().is_none().then_some(candidate)
    }

    fn push_incomplete_events(
        &self,
        target_index: usize,
        lifecycle_only: bool,
        events: &mut Vec<ParsedContractEvent>,
    ) {
        let target = &self.targets[target_index];
        for registration in &target.registrations {
            if lifecycle_only
                && !matches!(
                    registration.kind,
                    StreamingRegistrationKind::Lifecycle { .. }
                )
            {
                continue;
            }
            if registration_receives_invalid(registration) {
                events.push(ParsedContractEvent::Invalid {
                    listener_index: registration.listener_index,
                    diagnostic: XmlContractDiagnostic::IncompleteElement {
                        contract: registration.identity,
                        element: target.element,
                    },
                });
            }
        }
    }
}

struct NamespaceFrame {
    element_name: String,
    default_namespace: Option<Arc<str>>,
    tracked: Option<TrackedElement>,
}

fn tool_call_event(
    registration: &ContractRegistration,
    required_attribute: Option<&'static str>,
    attributes: &[ParsedAttribute],
) -> ParsedContractEvent {
    match required_attribute {
        Some(required) => {
            let mut value = None;
            for attribute in attributes {
                if attribute.name != required {
                    return invalid_event(
                        registration,
                        format!("unknown attribute `{}`", attribute.name),
                    );
                }
                value = Some(attribute.value.clone());
            }
            match value {
                Some(value) => ParsedContractEvent::Decoded {
                    listener_index: registration.listener_index,
                    value,
                },
                None => invalid_event(registration, format!("missing attribute `{required}`")),
            }
        }
        None if attributes.is_empty() => ParsedContractEvent::Decoded {
            listener_index: registration.listener_index,
            value: String::new(),
        },
        None => invalid_event(
            registration,
            format!("unknown attribute `{}`", attributes[0].name),
        ),
    }
}

fn lifecycle_event(
    registration: &ContractRegistration,
    phase: StreamingXmlPhase,
    tag: &'static str,
    attributes: &HashMap<String, String>,
    content: String,
) -> ParsedContractEvent {
    ParsedContractEvent::Lifecycle {
        listener_index: registration.listener_index,
        phase,
        element: XmlElement {
            tag_name: tag.to_owned(),
            attributes: attributes.clone(),
            content,
        },
    }
}

fn registration_receives_invalid(registration: &ContractRegistration) -> bool {
    match registration.kind {
        StreamingRegistrationKind::EmptyToolCall { .. } => true,
        StreamingRegistrationKind::Lifecycle { invalid, .. } => invalid,
    }
}

fn incomplete_close_prefix(pending: &str, element: &str) -> bool {
    if pending.is_empty() {
        return false;
    }
    let name_prefix = format!("</{element}");
    if name_prefix.starts_with(pending) {
        return true;
    }
    pending.strip_prefix(&name_prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_whitespace())
    })
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
