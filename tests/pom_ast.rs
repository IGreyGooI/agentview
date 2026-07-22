use agentview::pom::{HeadingLevel, PomError, TextNode, XmlName};

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
