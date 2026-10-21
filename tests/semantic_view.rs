use agentview::prelude::{LegacyAgentView, SemanticField, SemanticFragment};

#[test]
fn string_becomes_a_text_fragment_and_field_attribute_without_modifying_its_value() {
    let text = r#"hello <agent> & "raw" 'text' &gt; C:\memory\graph `*_[]#`"#.to_owned();

    assert_eq!(
        LegacyAgentView::render_root(&text),
        SemanticFragment::Text(text.clone())
    );
    assert_eq!(
        LegacyAgentView::render_field(&text, "name"),
        SemanticField::Attr {
            name: "name".to_owned(),
            value: text,
        }
    );
}
