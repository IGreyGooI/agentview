use agentview::pom::{
    BlockChildren, BlockContent, CodeBlockNode, CodeSpanNode, ContentContext, ContentKind,
    ContentNode, ContentRef, HeadingLevel, HeadingNode, InlineChildren, InlineContent, ListItem,
    ListKind, ListNode, MarkdownKind, MarkdownNode, MixedChildren, MixedContent, ParagraphNode,
    PomError, StrongNode, TextNode, XmlAttributes, XmlName, XmlNode,
};

#[test]
fn block_and_inline_contexts_reject_invalid_direct_children() {
    let text = ContentNode::Text(TextNode::new("orphan"));
    assert_eq!(
        BlockContent::try_from_node(text),
        Err(PomError::WrongContentContext {
            expected: ContentContext::Block,
            actual: ContentKind::Text,
        })
    );

    let heading = ContentNode::Markdown(MarkdownNode::Heading(HeadingNode::new(
        HeadingLevel::H2,
        InlineChildren::new(),
    )));
    assert_eq!(
        InlineContent::try_from_node(heading),
        Err(PomError::WrongContentContext {
            expected: ContentContext::Inline,
            actual: ContentKind::Markdown(MarkdownKind::Heading),
        })
    );
}

#[test]
fn dynamic_context_conversion_accepts_every_valid_direct_child() {
    let block_markdown = [
        MarkdownNode::Heading(HeadingNode::new(HeadingLevel::H1, InlineChildren::new())),
        MarkdownNode::Paragraph(ParagraphNode::new(InlineChildren::new())),
        MarkdownNode::List(ListNode::new(ListKind::Unordered, Vec::new())),
        MarkdownNode::CodeBlock(CodeBlockNode::new(None, TextNode::new(""))),
        MarkdownNode::ThematicBreak,
    ];
    for node in block_markdown {
        let kind = node.kind();
        assert!(
            BlockContent::try_from_node(ContentNode::Markdown(node)).is_ok(),
            "{kind:?} should be valid block content"
        );
    }

    let inline_markdown = [
        MarkdownNode::Strong(StrongNode::new(InlineChildren::new())),
        MarkdownNode::CodeSpan(CodeSpanNode::new(TextNode::new("code"))),
    ];
    for node in inline_markdown {
        let kind = node.kind();
        assert!(
            InlineContent::try_from_node(ContentNode::Markdown(node)).is_ok(),
            "{kind:?} should be valid inline content"
        );
    }

    let xml = XmlNode::new(XmlName::try_from("island").unwrap());
    assert_eq!(
        BlockContent::try_from_node(ContentNode::Xml(xml.clone())),
        Ok(BlockContent::xml(xml.clone()))
    );
    assert_eq!(
        InlineContent::try_from_node(ContentNode::Xml(xml.clone())),
        Ok(InlineContent::xml(xml))
    );
    assert_eq!(
        InlineContent::try_from_node(ContentNode::Text(TextNode::new("plain"))),
        InlineContent::try_text("plain")
    );
}

#[test]
fn dynamic_inline_text_conversion_rejects_newlines() {
    for value in ["first\nsecond", "first\rsecond"] {
        assert_eq!(
            InlineContent::try_from_node(ContentNode::Text(TextNode::new(value))),
            Err(PomError::InvalidInlineNewline {
                value: value.into(),
            })
        );
    }
}

#[test]
fn markdown_inline_children_reject_newlines() {
    assert_eq!(
        InlineContent::try_text("first\nsecond"),
        Err(PomError::InvalidInlineNewline {
            value: "first\nsecond".into(),
        })
    );
    assert_eq!(
        InlineContent::try_text("first\rsecond"),
        Err(PomError::InvalidInlineNewline {
            value: "first\rsecond".into(),
        })
    );
}

#[test]
fn text_sequences_drop_empty_and_merge_adjacent_text() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("")));
    children.push(MixedContent::text(TextNode::new("  first")));
    children.push(MixedContent::text(TextNode::new(" second  ")));

    assert_eq!(children.len(), 1);
    match children.iter().next().unwrap() {
        ContentRef::Node(ContentNode::Text(text)) => {
            assert_eq!(text.value(), "  first second  ");
        }
        other => panic!("expected normalized text, got {other:?}"),
    };
}

#[test]
fn text_normalization_does_not_trim_or_cross_xml() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new(" left ")));
    children.push(MixedContent::xml(XmlNode::new(
        XmlName::try_from("break").unwrap(),
    )));
    children.push(MixedContent::text(TextNode::new(" right ")));

    assert_eq!(children.len(), 3);
    let texts = children
        .iter()
        .filter_map(|edge| match edge {
            ContentRef::Node(ContentNode::Text(text)) => Some(text.value()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec![" left ", " right "]);
}

#[test]
fn inline_text_sequence_normalization_is_canonical() {
    let mut adjacent = InlineChildren::new();
    adjacent.push(InlineContent::try_text("").unwrap());
    adjacent.push(InlineContent::try_text("  first").unwrap());
    adjacent.push(InlineContent::try_text(" second  ").unwrap());

    assert_eq!(adjacent.len(), 1);
    match adjacent.iter().next().unwrap() {
        ContentRef::Node(ContentNode::Text(text)) => {
            assert_eq!(text.value(), "  first second  ");
        }
        other => panic!("expected normalized inline text, got {other:?}"),
    };

    let mut separated = InlineChildren::new();
    separated.push(InlineContent::try_text(" left ").unwrap());
    separated.push(InlineContent::strong(
        StrongNode::new(InlineChildren::new()),
    ));
    separated.push(InlineContent::try_text(" right ").unwrap());

    assert_eq!(separated.len(), 3);
    let texts = separated
        .iter()
        .filter_map(|edge| match edge {
            ContentRef::Node(ContentNode::Text(text)) => Some(text.value()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec![" left ", " right "]);
}

#[test]
fn xml_name_accepts_prompt_safe_ascii_subset() {
    for raw in ["agent_context", "actor-1", "a.b", "_private", "A9"] {
        let name = XmlName::try_from(raw).unwrap();
        assert_eq!(name.as_str(), raw);
    }
}

#[test]
fn xml_name_rejects_invalid_and_namespace_names() {
    for raw in [
        "",
        "9actor",
        "actor context",
        "actor:context",
        "<actor>",
        "é",
    ] {
        assert_eq!(
            XmlName::try_from(raw),
            Err(PomError::InvalidXmlName { value: raw.into() })
        );
    }
}

#[test]
fn heading_level_accepts_only_one_through_six() {
    for (raw, expected) in [
        (1, HeadingLevel::H1),
        (2, HeadingLevel::H2),
        (3, HeadingLevel::H3),
        (4, HeadingLevel::H4),
        (5, HeadingLevel::H5),
        (6, HeadingLevel::H6),
    ] {
        assert_eq!(HeadingLevel::try_from(raw).unwrap(), expected);
        assert_eq!(expected.number(), raw);
    }
    assert_eq!(
        HeadingLevel::try_from(0),
        Err(PomError::InvalidHeadingLevel { value: 0 })
    );
    assert_eq!(
        HeadingLevel::try_from(7),
        Err(PomError::InvalidHeadingLevel { value: 7 })
    );
}

#[test]
fn text_node_preserves_author_text() {
    let text = TextNode::new("  first\nsecond  ");
    assert_eq!(text.value(), "  first\nsecond  ");
    assert!(!text.is_empty());
    assert!(TextNode::new("").is_empty());
}

#[test]
fn xml_attributes_reject_duplicate_names() {
    let id = XmlName::try_from("id").unwrap();
    let mut attributes = XmlAttributes::new();
    attributes.insert(id.clone(), "actor.1").unwrap();
    assert_eq!(
        attributes.insert(id.clone(), "actor.2"),
        Err(PomError::DuplicateXmlAttribute { name: id })
    );
}

#[test]
fn xml_attribute_reordering_is_semantically_equal() {
    let mut left = XmlAttributes::new();
    left.try_insert("id", "actor.1").unwrap();
    left.try_insert("name", "Rachel").unwrap();

    let mut right = XmlAttributes::new();
    right.try_insert("name", "Rachel").unwrap();
    right.try_insert("id", "actor.1").unwrap();

    assert_eq!(left, right);
}

#[test]
fn attribute_iteration_preserves_insertion_order() {
    let mut attributes = XmlAttributes::new();
    attributes.try_insert("id", "actor.1").unwrap();
    attributes.try_insert("name", "Rachel").unwrap();

    let observed = attributes
        .iter()
        .map(|attribute| (attribute.name().as_str(), attribute.value()))
        .collect::<Vec<_>>();
    assert_eq!(observed, vec![("id", "actor.1"), ("name", "Rachel")]);
}

#[test]
fn ordered_list_and_code_nodes_preserve_semantic_payload() {
    let item = ListItem::new(BlockChildren::new());
    let list = ListNode::new(ListKind::Ordered { start: 3 }, vec![item]);
    assert_eq!(list.kind(), &ListKind::Ordered { start: 3 });
    assert_eq!(list.items().len(), 1);

    let block = CodeBlockNode::new(Some("rust".into()), TextNode::new("<tag>\n"));
    assert_eq!(block.language(), Some("rust"));
    assert_eq!(block.body().value(), "<tag>\n");

    let span = CodeSpanNode::new(TextNode::new("<tag>"));
    assert_eq!(span.body().value(), "<tag>");
}

#[test]
fn mixed_children_preserve_text_markdown_xml_text_order() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("before")));
    children.push(MixedContent::markdown(MarkdownNode::Heading(
        HeadingNode::new(HeadingLevel::H2, InlineChildren::new()),
    )));
    children.push(MixedContent::xml(XmlNode::new(
        XmlName::try_from("actor").unwrap(),
    )));
    children.push(MixedContent::text(TextNode::new("after")));

    let kinds = children
        .iter()
        .map(|edge| match edge {
            ContentRef::Node(ContentNode::Text(_)) => "text",
            ContentRef::Node(ContentNode::Markdown(_)) => "markdown",
            ContentRef::Node(ContentNode::Xml(_)) => "xml",
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["text", "markdown", "xml", "text"]);
}
