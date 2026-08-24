use agentview::{
    component::execution::{RenderedProjection, RenderedProjectionNode},
    transcript::{CanonicalInputItem, CanonicalTranscriptError, TranscriptRevision},
};

#[test]
fn owned_bulk_construction_preserves_exact_order() {
    const ITEM_COUNT: usize = 512;
    let items = (0..ITEM_COUNT)
        .map(|index| CanonicalInputItem::assistant_text(format!("item-{index}"), None))
        .collect::<Vec<_>>();

    let transcript =
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("linear", items.clone())])
            .unwrap()
            .to_transcript()
            .unwrap();

    assert_eq!(
        transcript.revision(),
        TranscriptRevision::new(ITEM_COUNT as u64)
    );
    assert_eq!(transcript.items(), items);
}

#[test]
fn owned_bulk_construction_retains_causal_duplicate_semantics() {
    let error = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
        "linear",
        vec![
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":1}"#).unwrap(),
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"value":2}"#).unwrap(),
        ],
    )])
    .unwrap()
    .to_transcript()
    .unwrap_err();

    assert_eq!(
        error,
        CanonicalTranscriptError::DuplicateToolCall {
            call_id: "call-1".to_owned(),
        }
    );
}
