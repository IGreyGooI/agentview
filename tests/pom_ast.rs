use agentview::pom::{HeadingLevel, PomError, TextNode, XmlAttributes, XmlName};

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
