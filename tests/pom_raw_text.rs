use agentview::{
    pom::{
        ContentContext, ContentKind, ContentNode, Document, InlineContent, PomError, RawTextNode,
        ResolvedDocument, XmlNode,
    },
    pom_renderer::render_pom_document,
    pom_resolution::resolve_artifact_document,
};

#[test]
fn raw_document_renders_byte_for_byte_without_markdown_or_xml_interpretation() {
    let raw = "  # literal\r\n<sample>&x</sample>\r\n\ttrailing spaces  \r\n";
    let document: Document = serde_json::from_str(
        &serde_json::to_string(&Document::from_raw_text(raw)).expect("raw document serializes"),
    )
    .expect("raw document deserializes");
    let resolved =
        resolve_artifact_document(document).expect("raw documents do not contain diff slots");
    let resolved: ResolvedDocument = serde_json::from_str(
        &serde_json::to_string(&resolved).expect("resolved raw document serializes"),
    )
    .expect("resolved raw document deserializes");

    assert_eq!(
        render_pom_document(&resolved).expect("raw document renders"),
        raw
    );
}

#[test]
fn separate_raw_blocks_keep_the_standard_block_separator() {
    let document = Document::try_build(|blocks| {
        blocks.raw_text(RawTextNode::new("first"));
        blocks.raw_text(RawTextNode::new("second"));
        Ok(())
    })
    .expect("raw blocks are valid document blocks");
    let resolved = resolve_artifact_document(document).expect("raw blocks resolve");

    assert_eq!(
        render_pom_document(&resolved).expect("raw blocks render"),
        "first\n\nsecond"
    );
}

#[test]
fn empty_raw_document_keeps_its_opaque_block_and_renders_empty() {
    let document = Document::from_raw_text("");
    let resolved = resolve_artifact_document(document).expect("empty raw block resolves");

    assert_eq!(resolved.children().len(), 1);
    assert_eq!(
        render_pom_document(&resolved).expect("empty raw block renders"),
        ""
    );
}

#[test]
fn raw_text_in_xml_uses_only_xml_escaping() {
    let xml = XmlNode::try_build("example", |children| {
        children.raw_text(RawTextNode::new("    <tag>& `*_[]#`"));
        Ok(())
    })
    .expect("valid XML node");
    let resolved = resolve_artifact_document(Document::from_xml(xml)).expect("XML resolves");

    assert_eq!(
        render_pom_document(&resolved).expect("XML renders"),
        "<example>    &lt;tag&gt;&amp; `*_[]#`</example>"
    );
}

#[test]
fn raw_text_is_not_valid_inline_content() {
    assert_eq!(
        InlineContent::try_from_node(ContentNode::RawText(RawTextNode::new("literal"))),
        Err(PomError::WrongContentContext {
            expected: ContentContext::Inline,
            actual: ContentKind::RawText,
        })
    );
}
