use agentview::pom::{
    BlockChildren, BlockContent, CodeBlockNode, CodeSpanNode, ContentContext, ContentKind,
    ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, HeadingLevel, HeadingNode,
    InlineChildren, InlineContent, ListItem, ListKind, ListNode, MarkdownKind, MarkdownNode,
    MixedChildren, MixedContent, ParagraphNode, PomError, StrongNode, TextNode, XmlAttributes,
    XmlName, XmlNode,
};

#[test]
fn closure_and_compositional_builders_are_equivalent() {
    let context = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    let closure = Document::try_build(|blocks| {
        blocks.try_heading(2, |inline| {
            inline.try_text("Known relationships")?;
            Ok(())
        })?;
        blocks.xml_slot(DiffSlot::present(DiffStrategy::Recursive, context.clone()));
        Ok(())
    })
    .unwrap();

    let mut heading_children = InlineChildren::new();
    heading_children.push(InlineContent::try_text("Known relationships").unwrap());
    let mut blocks = BlockChildren::new();
    blocks.push(BlockContent::heading(HeadingNode::new(
        HeadingLevel::H2,
        heading_children,
    )));
    blocks.push(BlockContent::xml_slot(DiffSlot::present(
        DiffStrategy::Recursive,
        context,
    )));
    let compositional = Document::new(blocks);

    assert_eq!(closure, compositional);
}

#[test]
fn closure_builders_cover_the_v1_authoring_surface() {
    let document = Document::try_build(|blocks| {
        blocks.try_heading(2, |inline| {
            inline.try_text("Title")?;
            Ok(())
        })?;
        blocks.try_paragraph(|inline| {
            inline.try_text("plain ")?;
            inline.try_strong(|strong| {
                strong.try_text("strong")?;
                Ok(())
            })?;
            inline.code_span(TextNode::new("code"));
            inline.xml(XmlNode::new(XmlName::try_from("ref")?));
            Ok(())
        })?;
        blocks.try_list(ListKind::Ordered { start: 3 }, |list| {
            list.try_item(|item| {
                item.try_paragraph(|inline| {
                    inline.try_text("first item")?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.code_block(Some("rust".into()), TextNode::new("<tag>\n"));
        blocks.thematic_break();
        blocks.xml(XmlNode::build(
            XmlName::try_from("mixed").unwrap(),
            |mixed| {
                mixed.text(TextNode::new("before"));
                mixed.markdown(MarkdownNode::Paragraph(ParagraphNode::new(
                    InlineChildren::new(),
                )));
                mixed.xml(XmlNode::new(XmlName::try_from("inner").unwrap()));
            },
        ));
        blocks.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            XmlNode::new(XmlName::try_from("agent_context")?),
        ));
        Ok(())
    })
    .unwrap();

    assert_eq!(document.children().len(), 7);
    assert!(matches!(
        document.children().iter().next(),
        Some(ContentRef::Node(ContentNode::Markdown(
            MarkdownNode::Heading(_)
        )))
    ));
    assert!(matches!(
        document.children().iter().nth(6),
        Some(ContentRef::DiffSlot(slot)) if slot.role().as_str() == "agent_context"
    ));
}

#[test]
fn typed_and_fallible_builder_entries_cover_remaining_surface() {
    let inline_slot = XmlName::try_from("inline_slot").unwrap();
    let document = Document::build(|blocks| {
        blocks.push(BlockContent::thematic_break());
        blocks.heading(HeadingLevel::H3, |inline| {
            inline.push(InlineContent::try_text("left").unwrap());
            inline.strong(|strong| {
                strong.push(InlineContent::try_text(" strong").unwrap());
            });
            inline.xml_slot(DiffSlot::absent(inline_slot, DiffStrategy::Replace));
        });
        blocks.paragraph(|inline| {
            inline.push(InlineContent::try_text("paragraph").unwrap());
        });
        blocks.list(ListKind::Unordered, |list| {
            list.item(|item| {
                item.paragraph(|inline| {
                    inline.push(InlineContent::try_text("item").unwrap());
                });
            });
        });
    });
    assert_eq!(document.children().len(), 4);

    let mixed_slot = XmlName::try_from("mixed_slot").unwrap();
    let xml = XmlNode::build(XmlName::try_from("mixed").unwrap(), |mixed| {
        mixed.push(MixedContent::text(TextNode::new("left")));
        mixed.text(TextNode::new(" right"));
        mixed.xml_slot(DiffSlot::absent(mixed_slot, DiffStrategy::Append));
    });
    assert_eq!(xml.children().len(), 2);
    assert!(matches!(
        xml.children().iter().next(),
        Some(ContentRef::Node(ContentNode::Text(text))) if text.value() == "left right"
    ));

    let parsed = XmlNode::try_build("valid", |mixed| {
        mixed.text(TextNode::new("child"));
        Ok(())
    })
    .unwrap();
    assert_eq!(parsed.name().as_str(), "valid");
    assert_eq!(
        XmlNode::try_build("invalid:name", |_| Ok(())),
        Err(PomError::InvalidXmlName {
            value: "invalid:name".into(),
        })
    );
}

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
            ContentRef::DiffSlot(_) => "diff",
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["text", "markdown", "xml", "text"]);
}

#[test]
fn present_diff_slot_derives_role_from_value_name() {
    let value = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    let slot = DiffSlot::present(DiffStrategy::Recursive, value.clone());

    assert_eq!(slot.role().as_str(), "agent_context");
    assert_eq!(slot.strategy(), &DiffStrategy::Recursive);
    assert_eq!(slot.value(), Some(&value));
    assert!(slot.is_present());
}

#[test]
fn xml_wrapped_markdown_is_a_valid_slot_value() {
    let mut inline = InlineChildren::new();
    inline.push(InlineContent::try_text("explanation").unwrap());

    let mut xml = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    xml.push(MixedContent::markdown(MarkdownNode::Paragraph(
        ParagraphNode::new(inline),
    )));

    let slot = DiffSlot::present(DiffStrategy::Recursive, xml);
    assert!(matches!(
        slot.value().unwrap().children().iter().next(),
        Some(ContentRef::Node(ContentNode::Markdown(
            MarkdownNode::Paragraph(_)
        )))
    ));
}

#[test]
fn all_diff_strategies_are_structurally_observable() {
    let key = XmlName::try_from("id").unwrap();
    let strategies = vec![
        DiffStrategy::Recursive,
        DiffStrategy::Replace,
        DiffStrategy::Append,
        DiffStrategy::Sequence,
        DiffStrategy::Set,
        DiffStrategy::Keyed(key),
    ];
    for strategy in strategies {
        let slot = DiffSlot::present(
            strategy.clone(),
            XmlNode::new(XmlName::try_from("items").unwrap()),
        );
        assert_eq!(slot.strategy(), &strategy);
    }
}

#[test]
fn absent_diff_slot_retains_role_strategy_and_position() {
    let role = XmlName::try_from("agent_context").unwrap();
    let slot = DiffSlot::absent(role.clone(), DiffStrategy::Replace);
    assert_eq!(slot.role(), &role);
    assert_eq!(slot.strategy(), &DiffStrategy::Replace);
    assert_eq!(slot.value(), None);

    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("before")));
    children.push(MixedContent::xml_slot(slot));
    children.push(MixedContent::text(TextNode::new("after")));
    assert!(matches!(
        children.iter().nth(1),
        Some(ContentRef::DiffSlot(slot)) if slot.role() == &role
    ));
}

#[test]
fn text_normalization_does_not_cross_diff_slot() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("left")));
    children.push(MixedContent::xml_slot(DiffSlot::absent(
        XmlName::try_from("agent_context").unwrap(),
        DiffStrategy::Recursive,
    )));
    children.push(MixedContent::text(TextNode::new("right")));
    assert_eq!(children.len(), 3);
}

#[test]
fn diff_slots_are_valid_in_every_xml_context() {
    let block = DiffSlot::absent(XmlName::try_from("block").unwrap(), DiffStrategy::Recursive);
    let inline = DiffSlot::absent(XmlName::try_from("inline").unwrap(), DiffStrategy::Replace);
    let mixed = DiffSlot::absent(XmlName::try_from("mixed").unwrap(), DiffStrategy::Append);

    let mut block_children = BlockChildren::new();
    block_children.push(BlockContent::xml_slot(block));
    let mut inline_children = InlineChildren::new();
    inline_children.push(InlineContent::xml_slot(inline));
    let mut mixed_children = MixedChildren::new();
    mixed_children.push(MixedContent::xml_slot(mixed));

    assert!(matches!(
        block_children.iter().next(),
        Some(ContentRef::DiffSlot(slot)) if slot.role().as_str() == "block"
    ));
    assert!(matches!(
        inline_children.iter().next(),
        Some(ContentRef::DiffSlot(slot)) if slot.role().as_str() == "inline"
    ));
    assert!(matches!(
        mixed_children.iter().next(),
        Some(ContentRef::DiffSlot(slot)) if slot.role().as_str() == "mixed"
    ));
}
