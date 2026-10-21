use agentview::{
    pom::{
        ContentContext, ContentKind, ContentNode, ContentRef, Document, InlineContent, PomError,
        RawTextNode, ResolvedDocument,
    },
    pom_resolution::resolve_artifact_document,
};

#[test]
fn raw_document_round_trips_without_interpreting_its_content() {
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

    let children = resolved.children().iter().collect::<Vec<_>>();
    assert!(matches!(
        children.as_slice(),
        [ContentRef::Node(ContentNode::RawText(text))] if text.value() == raw
    ));
}

#[test]
fn empty_raw_document_keeps_its_opaque_block() {
    let document = Document::from_raw_text("");
    let resolved = resolve_artifact_document(document).expect("empty raw block resolves");

    let children = resolved.children().iter().collect::<Vec<_>>();
    assert!(matches!(
        children.as_slice(),
        [ContentRef::Node(ContentNode::RawText(text))] if text.value().is_empty()
    ));
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
