use agentview::pom::{
    ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, InlineChildren, InlineContent,
    ListKind, MarkdownNode, ParagraphNode, TextNode, XmlName, XmlNode,
};
use agentview::pom_resolution::resolve_system_document;

fn assert_slot_free<'a>(children: impl Iterator<Item = ContentRef<'a>>) {
    for child in children {
        match child {
            ContentRef::DiffSlot(slot) => {
                panic!("resolved document retained diff slot `{}`", slot.role())
            }
            ContentRef::Node(ContentNode::Text(_)) => {}
            ContentRef::Node(ContentNode::RawText(_)) => {}
            ContentRef::Node(ContentNode::Xml(xml)) => {
                assert_slot_free(xml.children().iter());
            }
            ContentRef::Node(ContentNode::Markdown(markdown)) => match markdown {
                MarkdownNode::Heading(node) => assert_slot_free(node.children().iter()),
                MarkdownNode::Paragraph(node) => assert_slot_free(node.children().iter()),
                MarkdownNode::Strong(node) => assert_slot_free(node.children().iter()),
                MarkdownNode::List(node) => {
                    for item in node.items() {
                        assert_slot_free(item.children().iter());
                    }
                }
                MarkdownNode::CodeBlock(_)
                | MarkdownNode::CodeSpan(_)
                | MarkdownNode::ThematicBreak => {}
            },
        }
    }
}

#[test]
fn system_resolution_recursively_expands_present_and_omits_absent_slots() {
    let agent_context = XmlNode::try_build("agent_context", |children| {
        children.xml_slot(DiffSlot::present(
            DiffStrategy::Replace,
            XmlNode::try_build("workspace_session", |children| {
                children.text(TextNode::new("Explain edge.42"));
                Ok(())
            })?,
        ));
        children.xml_slot(DiffSlot::absent(
            XmlName::try_from("removed_edge")?,
            DiffStrategy::Set,
        ));
        Ok(())
    })
    .unwrap();

    let instructions = XmlNode::try_build("instructions", |children| {
        children.xml_slot(DiffSlot::present(
            DiffStrategy::Keyed(XmlName::try_from("id")?),
            XmlNode::try_build("edge", |children| {
                children.text(TextNode::new("located_in"));
                Ok(())
            })?,
        ));
        Ok(())
    })
    .unwrap();

    let document = Document::try_build(|blocks| {
        blocks.xml_slot(DiffSlot::present(DiffStrategy::Recursive, agent_context));
        blocks.xml_slot(DiffSlot::absent(
            XmlName::try_from("retired_context")?,
            DiffStrategy::Append,
        ));
        blocks.xml(instructions);
        Ok(())
    })
    .unwrap();

    let resolved = resolve_system_document(document);

    assert_eq!(resolved.children().len(), 2);
    assert_slot_free(resolved.children().iter());

    let context = match resolved.children().iter().next() {
        Some(ContentRef::Node(ContentNode::Xml(xml))) => xml,
        other => panic!("expected expanded agent_context, got {other:?}"),
    };
    assert_eq!(context.name().as_str(), "agent_context");
    assert!(matches!(
        context.children().iter().next(),
        Some(ContentRef::Node(ContentNode::Xml(xml)))
            if xml.name().as_str() == "workspace_session"
    ));

    let instructions = match resolved.children().iter().nth(1) {
        Some(ContentRef::Node(ContentNode::Xml(xml))) => xml,
        other => panic!("expected ordinary instructions node, got {other:?}"),
    };
    assert!(matches!(
        instructions.children().iter().next(),
        Some(ContentRef::Node(ContentNode::Xml(xml))) if xml.name().as_str() == "edge"
    ));
}

#[test]
fn system_resolution_prunes_markdown_containers_emptied_by_absent_slots() {
    let document = Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| {
            paragraph.try_strong(|strong| {
                strong.xml_slot(DiffSlot::absent(
                    XmlName::try_from("retired_context")?,
                    DiffStrategy::Recursive,
                ));
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();

    let resolved = resolve_system_document(document);

    assert!(resolved.children().is_empty());
}

fn text_xml(name: &str, text: &str) -> XmlNode {
    XmlNode::try_build(name, |children| {
        children.text(TextNode::new(text));
        Ok(())
    })
    .unwrap()
}

#[test]
fn system_resolution_preserves_rich_non_slot_structure_and_order() {
    let heading_ref = text_xml("heading_ref", "H");
    let strong_ref = text_xml("strong_ref", "S");
    let list_ref = XmlNode::try_build("list_ref", |children| {
        children.text(TextNode::new("L"));
        children.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            text_xml("nested_list_ref", "NL"),
        ));
        Ok(())
    })
    .unwrap();
    let markdown_ref = text_xml("markdown_ref", "M");

    let mut panel = XmlNode::try_build("panel", |children| {
        children.text(TextNode::new("before"));

        let mut paragraph = InlineChildren::new();
        paragraph.push(InlineContent::try_text("markdown").unwrap());
        paragraph.push(InlineContent::xml_slot(DiffSlot::present(
            DiffStrategy::Replace,
            markdown_ref.clone(),
        )));
        children.markdown(MarkdownNode::Paragraph(ParagraphNode::new(paragraph)));

        children.xml(text_xml("plain_child", "P"));
        children.text(TextNode::new("after"));
        Ok(())
    })
    .unwrap();
    panel
        .push_attribute(XmlName::try_from("id").unwrap(), "panel.1")
        .unwrap();

    let source = Document::try_build(|blocks| {
        blocks.try_heading(2, |heading| {
            heading.try_text("Known ")?;
            heading.xml_slot(DiffSlot::present(DiffStrategy::Append, heading_ref.clone()));
            Ok(())
        })?;
        blocks.try_paragraph(|paragraph| {
            paragraph.try_text("Use ")?;
            paragraph.try_strong(|strong| {
                strong.try_text("strong ")?;
                strong.xml_slot(DiffSlot::present(
                    DiffStrategy::Sequence,
                    strong_ref.clone(),
                ));
                Ok(())
            })?;
            paragraph.xml_slot(DiffSlot::absent(
                XmlName::try_from("removed_inline")?,
                DiffStrategy::Set,
            ));
            Ok(())
        })?;
        blocks.try_list(ListKind::Ordered { start: 7 }, |list| {
            list.try_item(|item| {
                item.xml_slot(DiffSlot::present(
                    DiffStrategy::Keyed(XmlName::try_from("id")?),
                    list_ref.clone(),
                ));
                item.code_block(Some("rust".into()), TextNode::new("fn main() {}"));
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.xml(panel.clone());
        Ok(())
    })
    .unwrap();

    let expected_list_ref = XmlNode::try_build("list_ref", |children| {
        children.text(TextNode::new("L"));
        children.xml(text_xml("nested_list_ref", "NL"));
        Ok(())
    })
    .unwrap();
    let mut expected_panel = XmlNode::try_build("panel", |children| {
        children.text(TextNode::new("before"));

        let mut paragraph = InlineChildren::new();
        paragraph.push(InlineContent::try_text("markdown").unwrap());
        paragraph.push(InlineContent::xml(markdown_ref));
        children.markdown(MarkdownNode::Paragraph(ParagraphNode::new(paragraph)));

        children.xml(text_xml("plain_child", "P"));
        children.text(TextNode::new("after"));
        Ok(())
    })
    .unwrap();
    expected_panel
        .push_attribute(XmlName::try_from("id").unwrap(), "panel.1")
        .unwrap();

    let expected = Document::try_build(|blocks| {
        blocks.try_heading(2, |heading| {
            heading.try_text("Known ")?;
            heading.xml(heading_ref);
            Ok(())
        })?;
        blocks.try_paragraph(|paragraph| {
            paragraph.try_text("Use ")?;
            paragraph.try_strong(|strong| {
                strong.try_text("strong ")?;
                strong.xml(strong_ref);
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.try_list(ListKind::Ordered { start: 7 }, |list| {
            list.try_item(|item| {
                item.xml(expected_list_ref);
                item.code_block(Some("rust".into()), TextNode::new("fn main() {}"));
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.xml(expected_panel);
        Ok(())
    })
    .unwrap();

    let resolved = resolve_system_document(source);

    assert_eq!(resolved.children(), expected.children());
    assert_slot_free(resolved.children().iter());
}

#[test]
fn system_resolution_ignores_all_strategies_and_duplicate_roles() {
    let strategies = [
        DiffStrategy::Recursive,
        DiffStrategy::Replace,
        DiffStrategy::Append,
        DiffStrategy::Sequence,
        DiffStrategy::Set,
        DiffStrategy::Keyed(XmlName::try_from("id").unwrap()),
    ];
    let document = Document::build(|blocks| {
        for (index, strategy) in strategies.into_iter().enumerate() {
            blocks.xml_slot(DiffSlot::present(
                strategy,
                text_xml("same_role", &index.to_string()),
            ));
        }
    });

    let resolved = resolve_system_document(document);

    assert_eq!(resolved.children().len(), 6);
    for (index, child) in resolved.children().iter().enumerate() {
        assert!(matches!(
            child,
            ContentRef::Node(ContentNode::Xml(xml))
                if xml.name().as_str() == "same_role"
                    && matches!(
                        xml.children().iter().next(),
                        Some(ContentRef::Node(ContentNode::Text(text)))
                            if text.value() == index.to_string()
                    )
        ));
    }
}
