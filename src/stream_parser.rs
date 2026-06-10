//! Streaming XML parser for Hermes-style NPC responses.
//!
//! Parses XML tags from a streaming LLM response. Instead of a blanket
//! trait implementor, callers register per-tag callbacks — one for complete
//! elements and an optional one for streaming (incomplete) updates.
//!
//! # Example
//! ```rust,ignore
//! let mut p = HermesParser::new();
//!
//! // Called once when <speak>…</speak> is fully received:
//! p.on_complete("speak", |elem| {
//!     println!("NPC says: {}", elem.content);
//! });
//!
//! // Called on every streaming chunk while inside a <speak> tag:
//! p.on_stream("speak", |elem| {
//!     print!("{}", elem.content);   // live streaming to frontend
//! });
//!
//! // Feed LLM chunks:
//! p.feed("<speak>Hello").await;
//! p.feed(" world</speak>").await;
//! p.finalize().await;
//! ```
//!
//! # Hermes format expected
//! ```xml
//! <speak>你好，陌生人。</speak>
//! <act>Gulvar 停下手中的锄头，打量着你。</act>
//! <update_relationship target="player" trust_delta="0" suspicion_delta="5"></update_relationship>
//! <log_behavior>与陌生人初次接触，保持警惕。</log_behavior>
//! ```
//!
//! For tags where the attributes are the payload (e.g. `update_relationship`),
//! register an `on_open` hook — called as soon as the opening tag is parsed:
//! ```rust,ignore
//! p.on_open("update_relationship", |elem| {
//!     // elem.attributes are available; elem.content is empty at this point.
//!     let trust = elem.attr_i32("trust_delta").unwrap_or(0);
//!     apply_trust_delta(trust);
//! });
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use nom::{
    IResult, Parser,
    branch::alt,
    bytes::streaming::{tag, take_until, take_while1},
    character::streaming::{char, multispace0, space0},
    multi::separated_list0,
    sequence::delimited,
};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Recognition error: {0}")]
    Recognition(String),
}

// ── XmlElement ────────────────────────────────────────────────────────────────

/// A parsed XML element with tag name, attributes, and content.
#[derive(Debug, Clone, PartialEq)]
pub struct XmlElement {
    pub tag_name: String,
    pub attributes: HashMap<String, String>,
    pub content: String,
}

impl XmlElement {
    /// Convenience: get an attribute by name.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(|s| s.as_str())
    }

    /// Convenience: parse an attribute as i32.
    pub fn attr_i32(&self, key: &str) -> Option<i32> {
        self.attr(key)?.parse().ok()
    }
}

// ── Callback types ────────────────────────────────────────────────────────────

/// Async callback for a complete, streaming, or opening element.
pub type BoxedFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub type ElementCallback = Box<dyn Fn(XmlElement) -> BoxedFuture + Send + Sync + 'static>;
// ── Parser internals ──────────────────────────────────────────────────────────

fn parse_attribute_value(input: &str) -> IResult<&str, &str> {
    alt((
        delimited(char('"'), take_until("\""), char('"')),
        delimited(char('\''), take_until("'"), char('\'')),
    ))
    .parse(input)
}

fn parse_attribute(input: &str) -> IResult<&str, (String, String)> {
    let (input, key) = take_while1(|c: char| c.is_alphanumeric() || c == '_' || c == '-')(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = char('=')(input)?;
    let (input, _) = space0(input)?;
    let (input, value) = parse_attribute_value(input)?;
    Ok((input, (key.to_string(), value.to_string())))
}

fn parse_attributes(input: &str) -> IResult<&str, HashMap<String, String>> {
    let (input, attrs) = separated_list0(multispace0, parse_attribute).parse(input)?;
    Ok((input, attrs.into_iter().collect()))
}

fn parse_opening_tag(input: &str) -> IResult<&str, (String, HashMap<String, String>)> {
    let (input, _) = char('<')(input)?;
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '_' || c == '-')(input)?;
    let (input, _) = space0(input)?;
    let (input, attrs) = parse_attributes(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = char('>')(input)?;
    Ok((input, (name.to_string(), attrs)))
}

fn parse_closing_tag(input: &str) -> IResult<&str, String> {
    let (input, _) = char('<')(input)?;
    let (input, _) = char('/')(input)?;
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '_' || c == '-')(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = char('>')(input)?;
    Ok((input, name.to_string()))
}

/// Parse a self-closing `<tag attrs.../>` element (no content).
fn parse_self_closing_element(input: &str) -> IResult<&str, XmlElement> {
    let (input, _) = char('<')(input)?;
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '_' || c == '-')(input)?;
    let (input, _) = space0(input)?;
    let (input, attrs) = parse_attributes(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = tag("/>")(input)?;
    Ok((
        input,
        XmlElement {
            tag_name: name.to_string(),
            attributes: attrs,
            content: String::new(),
        },
    ))
}

/// Parse a complete `<tag ...>content</tag>` element.
fn parse_complete_element(input: &str) -> IResult<&str, XmlElement> {
    let (input, _) = char('<')(input)?;
    let (input, name) = take_while1(|c: char| c.is_alphanumeric() || c == '_' || c == '-')(input)?;
    let (input, _) = space0(input)?;
    let (input, attrs) = parse_attributes(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = char('>')(input)?;

    let end_tag = format!("</{}>", name);
    let (input, content) = take_until(end_tag.as_str())(input)?;
    let (input, _) = tag(end_tag.as_str())(input)?;

    Ok((
        input,
        XmlElement {
            tag_name: name.to_string(),
            attributes: attrs,
            content: content.to_string(),
        },
    ))
}

/// Try to parse an incomplete element (open tag found, closing tag not yet received).
/// Returns the element with the partial content accumulated so far.
fn parse_incomplete_element(input: &str) -> Option<XmlElement> {
    let (remaining, (name, attrs)) = parse_opening_tag(input).ok()?;

    // Search for any potential closing tag.
    for (i, _) in remaining.match_indices('<') {
        let potential = &remaining[i..];
        match parse_closing_tag(potential) {
            Ok((_rest, tag_name)) if tag_name == name => {
                // Found matching closing tag — this is actually complete; bail out
                // so the caller falls through to `parse_complete_element`.
                return None;
            }
            Err(nom::Err::Incomplete(_)) => {
                // Potentially an incomplete closing tag fragment.
                let content = &remaining[..i];
                return Some(XmlElement {
                    tag_name: name,
                    attributes: attrs,
                    content: content.to_string(),
                });
            }
            Ok(_) | Err(_) => {
                let expected = format!("</{}>", name);
                if expected.starts_with(potential) {
                    // Incomplete closing tag.
                    let content = &remaining[..i];
                    return Some(XmlElement {
                        tag_name: name,
                        attributes: attrs,
                        content: content.to_string(),
                    });
                }
                // Not our closing tag — keep searching.
            }
        }
    }

    // No closing tag anywhere — return all content after the open tag.
    Some(XmlElement {
        tag_name: name,
        attributes: attrs,
        content: remaining.to_string(),
    })
}

/// What the parser found at a given position.
#[derive(Debug)]
enum FindResult {
    /// A complete `<tag ...>content</tag>` was parsed. `consumed` bytes were used.
    Complete {
        element: XmlElement,
        consumed: usize,
    },
    /// The opening tag was parsed but no closing tag found yet.
    /// `open_consumed` = bytes up to and including the `>`.
    Incomplete {
        element: XmlElement,
        open_consumed: usize,
    },
}

/// Find the next XML element in `input`.
fn find_next_element(input: &str) -> Option<FindResult> {
    for (byte_pos, ch) in input.char_indices() {
        if ch != '<' {
            continue;
        }
        let slice = &input[byte_pos..];

        // Try self-closing `<tag attrs.../>` first so `/>` isn't mistaken for
        // an opening tag followed by unknown content.
        if let Ok((rest, elem)) = parse_self_closing_element(slice) {
            let consumed = byte_pos + (slice.len() - rest.len());
            return Some(FindResult::Complete {
                element: elem,
                consumed,
            });
        }

        if let Ok((rest, elem)) = parse_complete_element(slice) {
            let consumed = byte_pos + (slice.len() - rest.len());
            return Some(FindResult::Complete {
                element: elem,
                consumed,
            });
        }

        if let Some(elem) = parse_incomplete_element(slice) {
            // Calculate how many bytes the opening tag occupied.
            let open_consumed = if let Ok((after_open, _)) = parse_opening_tag(slice) {
                byte_pos + (slice.len() - after_open.len())
            } else {
                0
            };
            return Some(FindResult::Incomplete {
                element: elem,
                open_consumed,
            });
        }
    }
    None
}

// ── HermesParser ─────────────────────────────────────────────────────────────

/// Streaming XML parser for Hermes-style LLM responses.
///
/// Register per-tag callbacks with [`on_complete`], [`on_stream`], and [`on_open`],
/// then feed LLM chunks with [`feed`]. Call [`finalize`] at stream end.
///
/// Callback firing order per element:
/// 1. `on_open`     — fires once as soon as `<tag attrs...>` is fully parsed
/// 2. `on_stream`   — fires on each subsequent chunk with growing content
/// 3. `on_complete` — fires once when `</tag>` is received
///
/// Use `on_open` for attribute-only tags like:
/// `<update_relationship target="player" trust_delta="5" suspicion_delta="-2">`
/// where you want to react immediately to the attributes.
pub struct HermesParser {
    /// Callbacks for complete elements, keyed by tag name.
    on_complete: HashMap<String, ElementCallback>,
    /// Callbacks for streaming (incomplete) elements, keyed by tag name.
    on_stream: HashMap<String, ElementCallback>,
    /// Callbacks fired immediately when an opening tag is parsed (attributes available).
    on_open: HashMap<String, ElementCallback>,

    buffer: String,
    /// Byte position in `buffer` up to which we've processed complete elements.
    cursor: usize,
    /// The tag name of the currently-open incomplete element, if any.
    current_incomplete_tag: Option<String>,
    /// Whether `on_open` has already fired for the current incomplete element.
    open_fired: bool,
}

impl HermesParser {
    pub fn new() -> Self {
        Self {
            on_complete: HashMap::new(),
            on_stream: HashMap::new(),
            on_open: HashMap::new(),
            buffer: String::new(),
            cursor: 0,
            current_incomplete_tag: None,
            open_fired: false,
        }
    }

    /// Register a callback for complete elements of the given tag.
    /// Called once when `</tag>` is received.
    pub fn on_complete<F, Fut>(&mut self, tag: impl Into<String>, cb: F)
    where
        F: Fn(XmlElement) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_complete
            .insert(tag.into(), Box::new(move |elem| Box::pin(cb(elem))));
    }

    /// Register a callback for streaming (incomplete) elements of the given tag.
    /// Called on every chunk while the tag is still open.
    pub fn on_stream<F, Fut>(&mut self, tag: impl Into<String>, cb: F)
    where
        F: Fn(XmlElement) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_stream
            .insert(tag.into(), Box::new(move |elem| Box::pin(cb(elem))));
    }

    /// Register a callback fired as soon as `<tag attrs...>` is parsed.
    /// The element's `content` will be empty at this point.
    /// Attributes are fully available.
    ///
    /// Useful for tags like `<update_relationship target="x" trust_delta="5">`.
    pub fn on_open<F, Fut>(&mut self, tag: impl Into<String>, cb: F)
    where
        F: Fn(XmlElement) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_open
            .insert(tag.into(), Box::new(move |elem| Box::pin(cb(elem))));
    }

    /// Feed a streaming chunk.
    pub async fn feed(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
        self.process().await;
        // Prune the processed prefix to keep the buffer small.
        if self.cursor > 4096 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
    }

    /// Call at stream end. Processes any remaining content.
    pub async fn finalize(&mut self) {
        self.process().await;
        self.buffer.clear();
        self.cursor = 0;
        self.current_incomplete_tag = None;
        self.open_fired = false;
    }

    async fn process(&mut self) {
        loop {
            if self.cursor >= self.buffer.len() {
                break;
            }
            let slice = &self.buffer[self.cursor..];

            match find_next_element(slice) {
                None => break,

                Some(FindResult::Complete { element, consumed }) => {
                    // If we were tracking an incomplete element for this tag,
                    // fire on_open now if it hasn't fired yet (edge case: element
                    // arrived complete on the first look).
                    if !self.open_fired
                        || self.current_incomplete_tag.as_deref() != Some(&element.tag_name)
                    {
                        // Fire on_open with empty content (attributes available).
                        if let Some(cb) = self.on_open.get(&element.tag_name) {
                            let open_elem = XmlElement {
                                tag_name: element.tag_name.clone(),
                                attributes: element.attributes.clone(),
                                content: String::new(),
                            };
                            cb(open_elem).await;
                        }
                    }
                    self.current_incomplete_tag = None;
                    self.open_fired = false;
                    self.cursor += consumed;
                    if let Some(cb) = self.on_complete.get(&element.tag_name) {
                        cb(element).await;
                    }
                }

                Some(FindResult::Incomplete {
                    element,
                    open_consumed,
                }) => {
                    // Fire on_open the first time we see this element.
                    if !self.open_fired
                        || self.current_incomplete_tag.as_deref() != Some(&element.tag_name)
                    {
                        self.current_incomplete_tag = Some(element.tag_name.clone());
                        self.open_fired = true;
                        if let Some(cb) = self.on_open.get(&element.tag_name) {
                            let open_elem = XmlElement {
                                tag_name: element.tag_name.clone(),
                                attributes: element.attributes.clone(),
                                content: String::new(),
                            };
                            cb(open_elem).await;
                        }
                        // Advance cursor past the opening tag so we don't re-fire on_open.
                        if open_consumed > 0 {
                            // Note: we can't advance cursor past the element start
                            // because we need to re-parse for complete detection.
                            // Instead, we track open_fired per tag.
                            let _ = open_consumed; // tracked via open_fired flag
                        }
                    }
                    if let Some(cb) = self.on_stream.get(&element.tag_name) {
                        cb(element).await;
                    }
                    // Don't advance cursor; wait for more data.
                    break;
                }
            }
        }
    }
}

impl Default for HermesParser {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn collect_complete(tag: &str) -> (HermesParser, Arc<Mutex<Vec<String>>>) {
        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&collected);
        let mut p = HermesParser::new();
        p.on_complete(tag.to_string(), move |elem| {
            let c = Arc::clone(&c);
            async move {
                c.lock().unwrap().push(elem.content);
            }
        });
        (p, collected)
    }

    fn collect_stream(tag: &str) -> (HermesParser, Arc<Mutex<Vec<String>>>) {
        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&collected);
        let mut p = HermesParser::new();
        p.on_stream(tag.to_string(), move |elem| {
            let c = Arc::clone(&c);
            async move {
                c.lock().unwrap().push(elem.content);
            }
        });
        (p, collected)
    }

    #[tokio::test]
    async fn single_complete_element() {
        let (mut p, got) = collect_complete("speak");
        p.feed("<speak>你好</speak>").await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["你好"]);
    }

    #[tokio::test]
    async fn element_split_across_chunks() {
        let (mut p, got) = collect_complete("speak");
        p.feed("<speak>hello").await;
        p.feed(" world</speak>").await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["hello world"]);
    }

    #[tokio::test]
    async fn multiple_complete_elements() {
        let completed: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&completed);
        let mut p = HermesParser::new();
        p.on_complete("speak", {
            let c = Arc::clone(&c);
            move |e| {
                let c = Arc::clone(&c);
                async move { c.lock().unwrap().push(("speak".into(), e.content)) }
            }
        });
        p.on_complete("act", {
            let c = Arc::clone(&c);
            move |e| {
                let c = Arc::clone(&c);
                async move { c.lock().unwrap().push(("act".into(), e.content)) }
            }
        });

        p.feed("<speak>你好</speak><act>点头</act>").await;
        p.finalize().await;
        let got = completed.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], ("speak".into(), "你好".into()));
        assert_eq!(got[1], ("act".into(), "点头".into()));
    }

    #[tokio::test]
    async fn attributes_are_parsed() {
        let attrs: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
        let a = Arc::clone(&attrs);
        let mut p = HermesParser::new();
        p.on_complete("update_relationship", move |e| {
            let a = Arc::clone(&a);
            async move {
                *a.lock().unwrap() = Some(e.attributes);
            }
        });
        p.feed(r#"<update_relationship target="player" trust_delta="5" suspicion_delta="-2"></update_relationship>"#).await;
        p.finalize().await;
        let got = attrs.lock().unwrap().clone().unwrap();
        assert_eq!(got.get("target").map(|s| s.as_str()), Some("player"));
        assert_eq!(got.get("trust_delta").map(|s| s.as_str()), Some("5"));
        assert_eq!(got.get("suspicion_delta").map(|s| s.as_str()), Some("-2"));
    }

    #[tokio::test]
    async fn streaming_callback_receives_partial_content() {
        let stream_snapshots: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&stream_snapshots);
        let mut p = HermesParser::new();
        p.on_stream("speak", move |e| {
            let s = Arc::clone(&s);
            async move {
                s.lock().unwrap().push(e.content);
            }
        });

        // Each feed should trigger the stream callback with growing content.
        p.feed("<speak>chunk1").await;
        p.feed(" chunk2").await;
        p.feed(" chunk3</speak>").await;
        p.finalize().await;

        let snaps = stream_snapshots.lock().unwrap().clone();
        // At minimum we should have gotten updates while incomplete.
        assert!(!snaps.is_empty(), "stream callback should have been called");
        // Content should grow monotonically.
        for w in snaps.windows(2) {
            assert!(
                w[1].len() >= w[0].len(),
                "content should only grow: {:?} -> {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[tokio::test]
    async fn complete_callback_fires_once_at_end() {
        let complete_count = Arc::new(Mutex::new(0usize));
        let cnt = Arc::clone(&complete_count);
        let mut p = HermesParser::new();
        p.on_complete("speak", move |_e| {
            let cnt = Arc::clone(&cnt);
            async move {
                *cnt.lock().unwrap() += 1;
            }
        });

        p.feed("<speak>hello").await;
        p.feed(" world").await;
        p.feed("</speak>").await;
        p.finalize().await;

        assert_eq!(*complete_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn unknown_tags_are_ignored() {
        let (mut p, got) = collect_complete("speak");
        p.feed("<think>internal reasoning</think><speak>hello</speak>")
            .await;
        p.finalize().await;
        // Only "speak" is registered; "think" is silently skipped.
        assert_eq!(*got.lock().unwrap(), vec!["hello"]);
    }

    #[tokio::test]
    async fn incomplete_at_end_of_stream() {
        let (mut p, got) = collect_complete("speak");
        p.feed("<speak>never finished").await;
        p.finalize().await;
        // No complete element, so on_complete should not fire.
        assert!(got.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn noise_before_and_after_elements() {
        let (mut p, got) = collect_complete("speak");
        p.feed("some prefix text <speak>hello world</speak> some suffix")
            .await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["hello world"]);
    }

    #[tokio::test]
    async fn unicode_content() {
        let (mut p, got) = collect_complete("speak");
        let content = "Hello 世界 🌍 Здравствуй مرحبا";
        let input = format!("<speak>{}</speak>", content);
        // Split at a char boundary near the middle to test chunk boundary handling.
        let mid_char = input.chars().count() / 2;
        let mid_byte = input
            .char_indices()
            .nth(mid_char)
            .map(|(i, _)| i)
            .unwrap_or(input.len());
        p.feed(&input[..mid_byte]).await;
        p.feed(&input[mid_byte..]).await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec![content]);
    }

    #[tokio::test]
    async fn multiple_elements_separated_by_noise() {
        let got: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let c1 = Arc::clone(&got);
        let c2 = Arc::clone(&got);
        let mut p = HermesParser::new();
        p.on_complete("speak", move |e| {
            let c1 = Arc::clone(&c1);
            async move {
                c1.lock().unwrap().push(e.content);
            }
        });
        p.on_complete("act", move |e| {
            let c2 = Arc::clone(&c2);
            async move {
                c2.lock().unwrap().push(e.content);
            }
        });

        p.feed("<speak>first</speak> noise <act>action</act> more noise <speak>second</speak>")
            .await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["first", "action", "second"]);
    }

    #[tokio::test]
    async fn attr_i32_helper() {
        let attrs: Arc<Mutex<Option<XmlElement>>> = Arc::new(Mutex::new(None));
        let a = Arc::clone(&attrs);
        let mut p = HermesParser::new();
        p.on_complete("update_relationship", move |e| {
            let a = Arc::clone(&a);
            async move {
                *a.lock().unwrap() = Some(e);
            }
        });
        p.feed(r#"<update_relationship target="player" trust_delta="10" suspicion_delta="-3"></update_relationship>"#).await;
        p.finalize().await;
        let elem = attrs.lock().unwrap().clone().unwrap();
        assert_eq!(elem.attr_i32("trust_delta"), Some(10));
        assert_eq!(elem.attr_i32("suspicion_delta"), Some(-3));
        assert_eq!(elem.attr("target"), Some("player"));
    }

    #[tokio::test]
    async fn empty_content_element() {
        let (mut p, got) = collect_complete("log_behavior");
        p.feed("<log_behavior></log_behavior>").await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["".to_string()]);
    }

    #[tokio::test]
    async fn stream_callback_not_called_for_unregistered_tag() {
        let (mut p, got) = collect_stream("speak");
        // Feed a different tag — stream callback for "speak" should not fire.
        p.feed("<think>reasoning chunk").await;
        assert!(got.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn buffer_cleanup_does_not_drop_content() {
        let (mut p, got) = collect_complete("speak");
        // Feed a large prefix to trigger the buffer cleanup path (> 4096 bytes),
        // then a complete element.
        let filler = "x".repeat(5000);
        p.feed(&filler).await;
        p.feed("<speak>after large prefix</speak>").await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["after large prefix"]);
    }

    // ── on_open tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn on_open_fires_immediately_for_inline_attribute_tag() {
        // <update_relationship target="player" trust_delta="5" suspicion_delta="-2">
        // on_open should fire as soon as the `>` of the opening tag is received.
        let fired: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
        let f = Arc::clone(&fired);
        let mut p = HermesParser::new();
        p.on_open("update_relationship", move |e| {
            let f = Arc::clone(&f);
            async move {
                f.lock().unwrap().push(e.attributes);
            }
        });

        // Feed the opening tag only (no closing tag yet).
        p.feed(r#"<update_relationship target="player" trust_delta="5" suspicion_delta="-2">"#)
            .await;
        {
            let v = fired.lock().unwrap();
            assert_eq!(v.len(), 1, "on_open should fire after opening tag");
            assert_eq!(v[0].get("target").map(|s| s.as_str()), Some("player"));
            assert_eq!(v[0].get("trust_delta").map(|s| s.as_str()), Some("5"));
            assert_eq!(v[0].get("suspicion_delta").map(|s| s.as_str()), Some("-2"));
        }

        // Now send the closing tag.
        p.feed("</update_relationship>").await;
        p.finalize().await;

        // on_open should still only have fired once.
        assert_eq!(fired.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn on_open_fires_once_for_element_with_content() {
        let open_count = Arc::new(Mutex::new(0usize));
        let c = Arc::clone(&open_count);
        let mut p = HermesParser::new();
        p.on_open("speak", move |_e| {
            let c = Arc::clone(&c);
            async move {
                *c.lock().unwrap() += 1;
            }
        });

        // Complete element in one shot — on_open fires once.
        p.feed("<speak>hello world</speak>").await;
        p.finalize().await;
        assert_eq!(*open_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn on_open_fires_once_when_element_spans_chunks() {
        let open_count = Arc::new(Mutex::new(0usize));
        let open_attrs: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
        let c = Arc::clone(&open_count);
        let a = Arc::clone(&open_attrs);
        let mut p = HermesParser::new();
        p.on_open("update_relationship", move |e| {
            let c = Arc::clone(&c);
            let a = Arc::clone(&a);
            async move {
                *c.lock().unwrap() += 1;
                *a.lock().unwrap() = Some(e.attributes);
            }
        });

        // Opening tag split across two chunks.
        p.feed(r#"<update_relationship target="player" trust"#)
            .await;
        // on_open should NOT have fired yet (opening tag is incomplete).
        assert_eq!(*open_count.lock().unwrap(), 0);

        p.feed(r#"_delta="10" suspicion_delta="-1"></update_relationship>"#)
            .await;
        p.finalize().await;

        // on_open should have fired exactly once now.
        assert_eq!(*open_count.lock().unwrap(), 1);
        let attrs = open_attrs.lock().unwrap().clone().unwrap();
        assert_eq!(attrs.get("trust_delta").map(|s| s.as_str()), Some("10"));
    }

    #[tokio::test]
    async fn on_open_content_is_empty() {
        let open_content: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&open_content);
        let mut p = HermesParser::new();
        p.on_open("speak", move |e| {
            let c = Arc::clone(&c);
            async move {
                c.lock().unwrap().push(e.content);
            }
        });

        p.feed("<speak>some content</speak>").await;
        p.finalize().await;

        // on_open content should be empty (attributes only, content not available yet).
        let v = open_content.lock().unwrap().clone();
        assert_eq!(v, vec!["".to_string()]);
    }

    #[tokio::test]
    async fn angle_brackets_in_content_are_treated_as_text() {
        // The first matching closing tag wins, so content like "<fake>inner</fake>"
        // will be consumed as part of the outer element's content only when the
        // outer element's closing tag comes AFTER the inner fake tags.
        // This mirrors the original test_xml_listener_nested_angle_brackets_in_content.
        let (mut p, got) = collect_complete("think");
        // The parser uses `take_until("</think>")`, so everything before the first
        // `</think>` is treated as raw content — angle brackets, fake tags, all of it.
        let input =
            r#"<think>Content with &lt; and &gt; symbols, and even <fake>tags</fake></think>"#;
        p.feed(input).await;
        p.finalize().await;
        let v = got.lock().unwrap().clone();
        assert_eq!(v.len(), 1);
        assert!(
            v[0].contains("<fake>tags</fake>"),
            "nested fake tags should be part of content: {:?}",
            v[0]
        );
    }

    #[tokio::test]
    async fn content_with_raw_angle_brackets_in_chunks() {
        // Feed content containing `<` and `>` across chunk boundaries.
        let (mut p, got) = collect_complete("speak");
        p.feed("<speak>a < b and b > a").await;
        p.feed(" is true</speak>").await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["a < b and b > a is true"]);
    }

    #[tokio::test]
    async fn incomplete_closing_tag_held_in_buffer() {
        // `<say>something</sa` — the partial closing tag fragment `</sa` must NOT
        // be emitted as content; the parser should hold it until more data arrives.
        let (mut p, got) = collect_complete("say");
        p.feed("<say>something</sa").await;
        // Nothing complete yet.
        assert!(got.lock().unwrap().is_empty());

        // Now complete it.
        p.feed("y>").await;
        p.finalize().await;
        assert_eq!(*got.lock().unwrap(), vec!["something"]);
    }

    #[tokio::test]
    async fn incomplete_closing_tag_does_not_appear_in_stream_content() {
        // While streaming, the partial `</sa` fragment must not bleed into the
        // `on_stream` callback's content snapshot.
        let stream_snapshots: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&stream_snapshots);
        let mut p = HermesParser::new();
        p.on_stream("say", move |e| {
            let s = Arc::clone(&s);
            async move {
                s.lock().unwrap().push(e.content);
            }
        });

        p.feed("<say>hello</sa").await;
        p.feed("y>").await;
        p.finalize().await;

        // All captured stream snapshots should NOT contain `</sa`.
        for snap in stream_snapshots.lock().unwrap().iter() {
            assert!(
                !snap.contains("</"),
                "closing-tag fragment should not appear in on_stream content: {:?}",
                snap
            );
        }
    }

    // ── Self-closing tag tests ─────────────────────────────────────────────────

    /// `<share_knowledge id="X"/>` — self-closing, on_open must fire with attrs.
    #[tokio::test]
    async fn self_closing_tag_fires_on_open_with_attributes() {
        let fired: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
        let f = Arc::clone(&fired);
        let mut p = HermesParser::new();
        p.on_open("share_knowledge", move |e| {
            let f = Arc::clone(&f);
            async move {
                f.lock().unwrap().push(e.attributes);
            }
        });

        p.feed(r#"<share_knowledge id="dwarves_law_basics"/>"#)
            .await;
        p.finalize().await;

        let v = fired.lock().unwrap();
        assert_eq!(
            v.len(),
            1,
            "on_open should fire exactly once for self-closing tag"
        );
        assert_eq!(
            v[0].get("id").map(|s| s.as_str()),
            Some("dwarves_law_basics")
        );
    }

    /// `on_complete` fires for self-closing tags with empty content.
    /// This is consistent: on_complete fires for any fully-parsed element.
    #[tokio::test]
    async fn self_closing_tag_fires_on_complete_with_empty_content() {
        let complete_items: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let ci = Arc::clone(&complete_items);
        let mut p = HermesParser::new();
        p.on_complete("share_knowledge", move |e| {
            let ci = Arc::clone(&ci);
            async move {
                ci.lock()
                    .unwrap()
                    .push((e.attr("id").unwrap_or("").to_string(), e.content));
            }
        });

        p.feed(r#"<share_knowledge id="x"/>"#).await;
        p.finalize().await;

        let v = complete_items.lock().unwrap();
        assert_eq!(
            v.len(),
            1,
            "on_complete should fire once for self-closing tag"
        );
        assert_eq!(v[0].0, "x", "id attribute should be available");
        assert_eq!(v[0].1, "", "content should be empty for self-closing tag");
    }

    /// Self-closing tag followed by a regular block element — both should parse correctly.
    #[tokio::test]
    async fn self_closing_tag_followed_by_block_element() {
        let open_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let complete_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let oi = Arc::clone(&open_ids);
        let ct = Arc::clone(&complete_texts);
        let mut p = HermesParser::new();
        p.on_open("share_knowledge", move |e| {
            let oi = Arc::clone(&oi);
            async move {
                oi.lock()
                    .unwrap()
                    .push(e.attr("id").unwrap_or("").to_string());
            }
        });
        p.on_complete("speak", move |e| {
            let ct = Arc::clone(&ct);
            async move {
                ct.lock().unwrap().push(e.content);
            }
        });

        p.feed(r#"<speak>你好</speak><share_knowledge id="city_trapped"/>"#)
            .await;
        p.finalize().await;

        assert_eq!(*complete_texts.lock().unwrap(), vec!["你好".to_string()]);
        assert_eq!(*open_ids.lock().unwrap(), vec!["city_trapped".to_string()]);
    }

    /// Self-closing tag split across two chunks.
    #[tokio::test]
    async fn self_closing_tag_split_across_chunks() {
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let f = Arc::clone(&fired);
        let mut p = HermesParser::new();
        p.on_open("share_knowledge", move |e| {
            let f = Arc::clone(&f);
            async move {
                f.lock()
                    .unwrap()
                    .push(e.attr("id").unwrap_or("").to_string());
            }
        });

        // Split right before the closing `/>`
        p.feed(r#"<share_knowledge id="city_trapped""#).await;
        assert!(fired.lock().unwrap().is_empty(), "should not fire mid-tag");
        p.feed(r#"/>"#).await;
        p.finalize().await;

        let v = fired.lock().unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], "city_trapped");
    }

    /// A regular block element still fires on_open AND on_complete correctly
    /// after the self-closing tag fix is applied.
    #[tokio::test]
    async fn block_element_still_works_after_self_closing_fix() {
        let open_count = Arc::new(Mutex::new(0usize));
        let complete_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let oc = Arc::clone(&open_count);
        let ct = Arc::clone(&complete_texts);
        let mut p = HermesParser::new();
        p.on_open("speak", move |_| {
            let oc = Arc::clone(&oc);
            async move {
                *oc.lock().unwrap() += 1;
            }
        });
        p.on_complete("speak", move |e| {
            let ct = Arc::clone(&ct);
            async move {
                ct.lock().unwrap().push(e.content);
            }
        });

        p.feed("<speak>hello world</speak>").await;
        p.finalize().await;

        assert_eq!(
            *open_count.lock().unwrap(),
            1,
            "on_open should fire once for regular block"
        );
        assert_eq!(*complete_texts.lock().unwrap(), vec!["hello world"]);
    }

    #[tokio::test]
    async fn all_three_hooks_fire_in_order() {
        // Verify the order: on_open → on_stream* → on_complete.
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let e1 = Arc::clone(&events);
        let e2 = Arc::clone(&events);
        let e3 = Arc::clone(&events);
        let mut p = HermesParser::new();
        p.on_open("speak", move |_| {
            let e1 = Arc::clone(&e1);
            async move {
                e1.lock().unwrap().push("open");
            }
        });
        p.on_stream("speak", move |_| {
            let e2 = Arc::clone(&e2);
            async move {
                e2.lock().unwrap().push("stream");
            }
        });
        p.on_complete("speak", move |_| {
            let e3 = Arc::clone(&e3);
            async move {
                e3.lock().unwrap().push("complete");
            }
        });

        p.feed("<speak>chunk1").await;
        p.feed(" chunk2").await;
        p.feed("</speak>").await;
        p.finalize().await;

        let v = events.lock().unwrap().clone();
        assert_eq!(v[0], "open");
        assert!(v[1..v.len() - 1].iter().all(|&s| s == "stream"));
        assert_eq!(v[v.len() - 1], "complete");
    }
}
