#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises DebugProviderPort through ProviderPort"
)]

use agentview::{
    component::execution::{
        DebugProviderPort, ProviderPort, RenderedProjection, RenderedProjectionNode,
    },
    pom::{Document, ResolvedDocument, TextNode, XmlNode},
    pom_resolution::resolve_artifact_document,
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};
use futures::StreamExt;

fn xml_document(name: &str, text: &str) -> ResolvedDocument {
    let node = XmlNode::try_build(name, |children| {
        children.text(TextNode::new(text));
        Ok(())
    })
    .unwrap();
    resolve_artifact_document(Document::from_xml(node)).unwrap()
}

#[tokio::test]
async fn debug_provider_captures_one_readable_complete_projection_prompt() {
    let projection = RenderedProjection::from_nodes(vec![
        RenderedProjectionNode::new(
            "rules",
            vec![CanonicalInputItem::instruction(
                InstructionAuthority::System,
                xml_document("rules", "Be precise."),
            )],
        ),
        RenderedProjectionNode::new(
            "task",
            vec![CanonicalInputItem::message(
                ConversationRole::User,
                xml_document("task", "Inspect the board."),
            )],
        ),
    ])
    .unwrap();
    let (mut provider, capture) = DebugProviderPort::new();

    let mut events = provider.execute(projection).await.unwrap();
    assert!(events.next().await.is_none());

    assert_eq!(
        capture.snapshots(),
        [concat!(
            "## System\n\n",
            "<rules>Be precise.</rules>\n\n",
            "## User\n\n",
            "<task>Inspect the board.</task>"
        )]
    );
}

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
