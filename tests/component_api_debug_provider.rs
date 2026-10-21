#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises DebugProviderPort through ProviderPort"
)]

use agentview::{
    component::execution::{
        DebugProviderPort, ProviderPort, RenderedProjection, RenderedProjectionNode,
    },
    pom::{Document, TextNode},
    pom_resolution::resolve_artifact_document,
    transcript::{CanonicalInputItem, ConversationRole},
};

#[tokio::test]
async fn debug_provider_redacts_unrenderable_prompt_content_from_faults() {
    let sensitive = "sensitive-prompt\ncontent";
    let document = Document::build(|blocks| {
        blocks.paragraph(|paragraph| {
            paragraph.code_span(TextNode::new(sensitive));
        });
    });
    let projection = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
        "task",
        vec![CanonicalInputItem::message(
            ConversationRole::User,
            resolve_artifact_document(document).unwrap(),
        )],
    )])
    .unwrap();
    let (mut provider, capture) = DebugProviderPort::new();

    let fault = match provider.execute(projection).await {
        Ok(_) => panic!("unrenderable prompt must fail closed"),
        Err(fault) => fault,
    };

    assert!(!fault.message().contains(sensitive));
    assert!(capture.snapshots().is_empty());
}
