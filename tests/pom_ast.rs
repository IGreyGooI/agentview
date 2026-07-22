use agentview::pom::{
    BlockChildren, CodeBlockNode, CodeSpanNode, ContentNode, ContentRef, HeadingLevel, HeadingNode,
    InlineChildren, ListItem, ListKind, ListNode, MarkdownNode, MixedChildren, MixedContent,
    PomError, TextNode, XmlAttributes, XmlName, XmlNode,
};

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
