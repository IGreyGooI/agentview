//! Canonical text rendering for resolved Prompt Object Model documents.
//!
//! Resolution is deliberately a separate step. This module accepts only
//! [`ResolvedDocument`], so it never interprets diff metadata or session state.

use crate::{
    pom::{
        BlockChildren, CodeBlockNode, CodeSpanNode, ContentNode, ContentRef, HeadingNode,
        InlineChildren, ListKind, ListNode, MarkdownNode, MixedChildren, ParagraphNode,
        ResolvedDocument, StrongNode, XmlNode,
    },
    StorageString,
};

/// Errors encountered while rendering an otherwise structurally valid POM.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PomRenderError {
    #[error("code block language must be a non-empty token without whitespace: {value:?}")]
    InvalidCodeBlockLanguage { value: StorageString },

    #[error("strong content must contain at least one non-whitespace character")]
    InvalidStrongContent,

    #[error("an empty paragraph has no canonical CommonMark representation")]
    InvalidEmptyParagraph,

    #[error("a zero-item list has no canonical CommonMark representation")]
    InvalidEmptyList,

    #[error("code span body must be non-empty and cannot contain a newline: {value:?}")]
    InvalidCodeSpanBody { value: StorageString },

    #[error(
        "ordered list marker {marker} has more than nine digits (item {item_index}); \
         CommonMark cannot represent it as a list item"
    )]
    OrderedListMarkerTooLong { marker: u64, item_index: usize },
}

/// Renders a slot-free POM document to Markdown + XML-shaped prompt text.
///
/// XML organizes the prompt for the model; it is not a serialization format for
/// an XML parser. Data text and attribute values receive no escaping. XML keeps
/// its structural indentation; explicitly authored Markdown nodes apply their
/// own formatting and escaping.
pub fn render_pom_document(document: &ResolvedDocument) -> Result<String, PomRenderError> {
    Renderer.render_block_children(document.children(), false)
}

struct Renderer;

impl Renderer {
    fn render_block_children(
        &self,
        children: &BlockChildren,
        xml_context: bool,
    ) -> Result<String, PomRenderError> {
        children
            .iter()
            .map(|content| self.render_block_content(content, xml_context))
            .collect::<Result<Vec<_>, _>>()
            .map(|blocks| blocks.join("\n\n"))
    }

    fn render_block_content(
        &self,
        content: ContentRef<'_>,
        xml_context: bool,
    ) -> Result<String, PomRenderError> {
        match content {
            ContentRef::Node(ContentNode::Markdown(node)) => {
                self.render_block_markdown(node, xml_context)
            }
            ContentRef::Node(ContentNode::Xml(node)) => {
                let placement = if xml_context {
                    XmlPlacement::Embedded
                } else {
                    XmlPlacement::Block
                };
                self.render_xml(node, placement)
            }
            ContentRef::Node(ContentNode::RawText(node)) => Ok(node.value().to_owned()),
            ContentRef::Node(ContentNode::Text(_)) => {
                unreachable!("resolved block children cannot contain direct text")
            }
            ContentRef::DiffSlot(_) => {
                unreachable!("ResolvedDocument cannot contain diff slots")
            }
        }
    }

    fn render_block_markdown(
        &self,
        node: &MarkdownNode,
        xml_context: bool,
    ) -> Result<String, PomRenderError> {
        match node {
            MarkdownNode::Heading(node) => self.render_heading(node),
            MarkdownNode::Paragraph(node) => self.render_paragraph(node),
            MarkdownNode::List(node) => self.render_list(node, xml_context),
            MarkdownNode::CodeBlock(node) => self.render_code_block(node),
            MarkdownNode::ThematicBreak => Ok("---".to_owned()),
            MarkdownNode::Strong(_) | MarkdownNode::CodeSpan(_) => {
                unreachable!("block children cannot contain inline Markdown")
            }
        }
    }

    fn render_heading(&self, node: &HeadingNode) -> Result<String, PomRenderError> {
        let marker = "#".repeat(usize::from(node.level().number()));
        let body = self.render_inline_children(node.children(), false)?;
        Ok(format!("{marker} {body}"))
    }

    fn render_paragraph(&self, node: &ParagraphNode) -> Result<String, PomRenderError> {
        if node.children().is_empty() {
            return Err(PomRenderError::InvalidEmptyParagraph);
        }
        self.render_inline_children(node.children(), true)
            .map(|rendered| encode_rendered_indentation(&rendered))
    }

    fn render_inline_children(
        &self,
        children: &InlineChildren,
        starts_block: bool,
    ) -> Result<String, PomRenderError> {
        let mut rendered = String::new();
        let mut at_block_start = starts_block;

        for content in children.iter() {
            let next = match content {
                ContentRef::Node(ContentNode::Text(node)) => {
                    self.escape_markdown_text(node.value(), at_block_start)
                }
                ContentRef::Node(ContentNode::Markdown(MarkdownNode::Strong(node))) => {
                    self.render_strong(node)?
                }
                ContentRef::Node(ContentNode::Markdown(MarkdownNode::CodeSpan(node))) => {
                    self.render_code_span(node)?
                }
                ContentRef::Node(ContentNode::Xml(node)) => {
                    self.render_xml(node, XmlPlacement::Embedded)?
                }
                ContentRef::Node(ContentNode::RawText(_)) => {
                    unreachable!("inline children cannot contain raw text blocks")
                }
                ContentRef::Node(ContentNode::Markdown(
                    MarkdownNode::Heading(_)
                    | MarkdownNode::Paragraph(_)
                    | MarkdownNode::List(_)
                    | MarkdownNode::CodeBlock(_)
                    | MarkdownNode::ThematicBreak,
                )) => unreachable!("inline children cannot contain block Markdown"),
                ContentRef::DiffSlot(_) => {
                    unreachable!("ResolvedDocument cannot contain diff slots")
                }
            };

            at_block_start = next.ends_with('\n');
            rendered.push_str(&next);
        }

        Ok(rendered)
    }

    fn render_strong(&self, node: &StrongNode) -> Result<String, PomRenderError> {
        let body = self.render_inline_children(node.children(), false)?;
        let Some(core_start) = body.find(|ch: char| !ch.is_whitespace()) else {
            return Err(PomRenderError::InvalidStrongContent);
        };
        let core_end = body
            .rfind(|ch: char| !ch.is_whitespace())
            .map(|index| {
                index
                    + body[index..]
                        .chars()
                        .next()
                        .expect("the located character exists")
                        .len_utf8()
            })
            .expect("a non-whitespace character was found");
        let leading = &body[..core_start];
        let core = &body[core_start..core_end];
        let trailing = &body[core_end..];

        Ok(format!("{leading}**{core}**{trailing}"))
    }

    fn render_code_span(&self, node: &CodeSpanNode) -> Result<String, PomRenderError> {
        let raw = node.body().value();
        if raw.is_empty() || raw.contains(['\r', '\n']) {
            return Err(PomRenderError::InvalidCodeSpanBody { value: raw.into() });
        }
        let delimiter = "`".repeat(longest_run(raw, '`').saturating_add(1).max(1));
        let body = raw;
        let needs_padding = raw.starts_with('`')
            || raw.ends_with('`')
            || (raw.starts_with(' ') && raw.ends_with(' ') && raw.chars().any(|ch| ch != ' '));

        if needs_padding {
            Ok(format!("{delimiter} {body} {delimiter}"))
        } else {
            Ok(format!("{delimiter}{body}{delimiter}"))
        }
    }

    fn render_list(&self, node: &ListNode, xml_context: bool) -> Result<String, PomRenderError> {
        if node.items().is_empty() {
            return Err(PomRenderError::InvalidEmptyList);
        }
        let mut rendered_items = Vec::with_capacity(node.items().len());

        for (index, item) in node.items().iter().enumerate() {
            let marker = match node.kind() {
                ListKind::Unordered => "-".to_owned(),
                ListKind::Ordered { start } => {
                    if *start >= 1_000_000_000 {
                        return Err(PomRenderError::OrderedListMarkerTooLong {
                            marker: *start,
                            item_index: 0,
                        });
                    }
                    let number = u64::try_from(index)
                        .ok()
                        .and_then(|offset| start.checked_add(offset))
                        .filter(|number| *number < 1_000_000_000)
                        .unwrap_or(*start);
                    format!("{number}.")
                }
            };
            let indent = " ".repeat(marker.len() + 1);
            let blocks = item
                .children()
                .iter()
                .map(|content| {
                    let separate_first_line = matches!(
                        content,
                        ContentRef::Node(ContentNode::Markdown(MarkdownNode::ThematicBreak))
                    );
                    self.render_block_content(content, xml_context)
                        .map(|rendered| (separate_first_line, rendered))
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(_, rendered)| !rendered.is_empty())
                .collect::<Vec<_>>();

            if blocks.is_empty() {
                rendered_items.push(marker);
                continue;
            }

            let mut rendered_item = String::new();
            rendered_item.push_str(&marker);
            if blocks[0].0 {
                rendered_item.push('\n');
                rendered_item.push_str(&indent_continuation_lines(&blocks[0].1, &indent, true));
            } else {
                rendered_item.push(' ');
                rendered_item.push_str(&indent_continuation_lines(&blocks[0].1, &indent, false));
            }
            for (_, block) in &blocks[1..] {
                rendered_item.push_str("\n\n");
                rendered_item.push_str(&indent_continuation_lines(block, &indent, true));
            }
            rendered_items.push(rendered_item);
        }

        Ok(rendered_items.join("\n"))
    }

    fn render_code_block(&self, node: &CodeBlockNode) -> Result<String, PomRenderError> {
        let language = match node.language() {
            Some(language) if language.is_empty() || language.chars().any(char::is_whitespace) => {
                return Err(PomRenderError::InvalidCodeBlockLanguage {
                    value: language.into(),
                });
            }
            language => language,
        };

        let raw_body = node.body().value();
        let fence = "~".repeat(longest_run(raw_body, '~').saturating_add(1).max(3));
        let mut rendered = match language {
            Some(language) => format!("{fence} {language}\n{raw_body}"),
            None => format!("{fence}\n{raw_body}"),
        };
        if !raw_body.is_empty() && !raw_body.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str(&fence);
        Ok(rendered)
    }

    fn render_xml(
        &self,
        node: &XmlNode,
        placement: XmlPlacement,
    ) -> Result<String, PomRenderError> {
        let mut opening = format!("<{}", node.name().as_str());
        for attribute in node.attributes().iter() {
            opening.push(' ');
            opening.push_str(attribute.name().as_str());
            opening.push_str("=\"");
            opening.push_str(attribute.value());
            opening.push('"');
        }

        if node.children().is_empty() {
            opening.push_str(" />");
            return Ok(opening);
        }

        if placement == XmlPlacement::Block
            && element_only_children(node)
            && !contains_block_markdown(node)
        {
            let children = node
                .children()
                .iter()
                .map(|content| {
                    let ContentRef::Node(ContentNode::Xml(child)) = content else {
                        unreachable!("element-only XML contains only XML child nodes")
                    };
                    self.render_xml(child, XmlPlacement::Block)
                        .map(|rendered| indent_continuation_lines(&rendered, "  ", true))
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("\n");
            return Ok(format!(
                "{opening}>\n{children}\n</{}>",
                node.name().as_str()
            ));
        }

        let mixed = self.render_mixed_children(node.children())?;
        if mixed.multiline || mixed.text.contains('\n') {
            Ok(format!(
                "{opening}>\n{}\n</{}>",
                mixed.text,
                node.name().as_str()
            ))
        } else {
            Ok(format!(
                "{opening}>{}</{}>",
                mixed.text,
                node.name().as_str()
            ))
        }
    }

    fn render_mixed_children(
        &self,
        children: &MixedChildren,
    ) -> Result<RenderedMixed, PomRenderError> {
        let mut units = Vec::new();
        let mut flow = String::new();
        let mut has_block = false;

        for content in children.iter() {
            match content {
                ContentRef::Node(ContentNode::Markdown(node)) if node.is_block() => {
                    if !flow.is_empty() {
                        units.push(std::mem::take(&mut flow));
                    }
                    units.push(self.render_block_markdown(node, true)?);
                    has_block = true;
                }
                ContentRef::Node(ContentNode::Markdown(MarkdownNode::Strong(node))) => {
                    flow.push_str(&self.render_strong(node)?);
                }
                ContentRef::Node(ContentNode::Markdown(MarkdownNode::CodeSpan(node))) => {
                    flow.push_str(&self.render_code_span(node)?);
                }
                ContentRef::Node(ContentNode::Markdown(
                    MarkdownNode::Heading(_)
                    | MarkdownNode::Paragraph(_)
                    | MarkdownNode::List(_)
                    | MarkdownNode::CodeBlock(_)
                    | MarkdownNode::ThematicBreak,
                )) => unreachable!("block Markdown was handled by the preceding match arm"),
                ContentRef::Node(ContentNode::Text(node)) => {
                    flow.push_str(node.value());
                }
                ContentRef::Node(ContentNode::RawText(node)) => {
                    flow.push_str(node.value());
                }
                ContentRef::Node(ContentNode::Xml(node)) => {
                    flow.push_str(&self.render_xml(node, XmlPlacement::Embedded)?);
                }
                ContentRef::DiffSlot(_) => {
                    unreachable!("ResolvedDocument cannot contain diff slots")
                }
            }
        }
        if !flow.is_empty() {
            units.push(flow);
        }

        Ok(RenderedMixed {
            text: units.join("\n\n"),
            multiline: has_block || units.len() > 1,
        })
    }

    fn escape_markdown_text(&self, raw: &str, block_start: bool) -> String {
        let ordered_delimiter = block_start.then(|| ordered_list_delimiter(raw)).flatten();
        let block_marker = block_start.then(|| block_marker(raw)).flatten();
        let encoded_indent = block_start.then(|| encoded_indentation(raw)).flatten();
        let mut rendered = String::with_capacity(raw.len());

        for (index, ch) in raw.char_indices() {
            if encoded_indent == Some(index) {
                match ch {
                    ' ' => rendered.push_str("&#32;"),
                    '\t' => rendered.push_str("&#9;"),
                    _ => unreachable!("only leading indentation is encoded"),
                }
                continue;
            }

            let markdown_control = matches!(
                ch,
                '\\' | '`' | '*' | '_' | '[' | ']' | '#' | '<' | '>' | '&'
            );
            if markdown_control || block_marker == Some(index) || ordered_delimiter == Some(index) {
                rendered.push('\\');
            }
            rendered.push(ch);
        }

        rendered
    }
}

struct RenderedMixed {
    text: String,
    multiline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XmlPlacement {
    Block,
    Embedded,
}

fn element_only_children(node: &XmlNode) -> bool {
    node.children()
        .iter()
        .all(|content| matches!(content, ContentRef::Node(ContentNode::Xml(_))))
}

fn contains_block_markdown(node: &XmlNode) -> bool {
    node.children().iter().any(content_contains_block_markdown)
}

fn content_contains_block_markdown(content: ContentRef<'_>) -> bool {
    match content {
        ContentRef::Node(ContentNode::Xml(node)) => contains_block_markdown(node),
        ContentRef::Node(ContentNode::Markdown(node)) if node.is_block() => true,
        ContentRef::Node(ContentNode::Markdown(MarkdownNode::Strong(node))) => {
            node.children().iter().any(content_contains_block_markdown)
        }
        ContentRef::Node(ContentNode::Markdown(MarkdownNode::CodeSpan(_)))
        | ContentRef::Node(ContentNode::Text(_))
        | ContentRef::Node(ContentNode::RawText(_)) => false,
        ContentRef::Node(ContentNode::Markdown(
            MarkdownNode::Heading(_)
            | MarkdownNode::Paragraph(_)
            | MarkdownNode::List(_)
            | MarkdownNode::CodeBlock(_)
            | MarkdownNode::ThematicBreak,
        )) => unreachable!("block Markdown was handled by the guarded match arm"),
        ContentRef::DiffSlot(_) => true,
    }
}

fn longest_run(value: &str, needle: char) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for ch in value.chars() {
        if ch == needle {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn indent_continuation_lines(value: &str, indent: &str, indent_first: bool) -> String {
    value
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            if (index > 0 || indent_first) && !line.is_empty() {
                format!("{indent}{line}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ordered_list_delimiter(raw: &str) -> Option<usize> {
    let indent = leading_spaces(raw);
    if indent > 3 {
        return None;
    }
    let candidate = raw.get(indent..)?;
    let digit_count = candidate.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    let delimiter = candidate.as_bytes().get(digit_count).copied()?;
    if !matches!(delimiter, b'.' | b')') {
        return None;
    }
    let following = candidate.as_bytes().get(digit_count + 1).copied();
    if following.is_none_or(|byte| byte.is_ascii_whitespace()) {
        Some(indent + digit_count)
    } else {
        None
    }
}

fn block_marker(raw: &str) -> Option<usize> {
    let indent = leading_spaces(raw);
    if indent > 3 {
        return None;
    }
    let first = raw.as_bytes().get(indent).copied()?;
    matches!(first, b'>' | b'-' | b'+' | b'~').then_some(indent)
}

fn encoded_indentation(raw: &str) -> Option<usize> {
    let mut columns = 0;
    for byte in raw.bytes() {
        match byte {
            b' ' => columns += 1,
            b'\t' => columns += 4 - (columns % 4),
            _ => break,
        }
        if columns >= 4 {
            return Some(0);
        }
    }
    None
}

fn leading_spaces(raw: &str) -> usize {
    raw.bytes().take_while(|byte| *byte == b' ').count()
}

fn encode_rendered_indentation(rendered: &str) -> String {
    let Some(index) = encoded_indentation(rendered) else {
        return rendered.to_owned();
    };
    debug_assert_eq!(index, 0);

    match rendered.as_bytes().first() {
        Some(b' ') => format!("&#32;{}", &rendered[1..]),
        Some(b'\t') => format!("&#9;{}", &rendered[1..]),
        _ => rendered.to_owned(),
    }
}
