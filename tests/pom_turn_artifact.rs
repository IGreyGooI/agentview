use agentview::agent::{DefaultAgentFeedback, DefaultContextState, DefaultTurnPrompt};
use agentview::pom::{
    DiffSlot, DiffStrategy, Document, InlineChildren, InlineContent, MarkdownNode, ParagraphNode,
    TextNode, XmlNode,
};
use agentview::templates::{PromptRenderable, TemplateEngine, TurnArtifact};

fn xml_document(name: &str, text: &str) -> Document {
    Document::try_build(|blocks| {
        blocks.xml(XmlNode::try_build(name, |children| {
            children.text(TextNode::new(text));
            Ok(())
        })?);
        Ok(())
    })
    .unwrap()
}

fn text_artifact(kind: &str, text: &str) -> TurnArtifact {
    TurnArtifact::try_from_document(kind, xml_document(kind, text)).unwrap()
}

#[test]
fn turn_artifact_rejects_a_kind_that_could_inject_legacy_layout_markup() {
    let result = TurnArtifact::try_from_document(
        r#""><raw_markup>"#,
        xml_document("parser_error", "safe body"),
    );

    assert!(result.is_err());
}

#[tokio::test]
async fn turn_artifact_renders_canonical_pom_and_exposes_its_kind() {
    let artifact =
        TurnArtifact::try_from_document("parser_error", xml_document("parser_error", "<select>&"))
            .unwrap();

    assert_eq!(artifact.kind(), "parser_error");
    assert_eq!(
        artifact
            .render_full(&TemplateEngine::new())
            .await
            .unwrap()
            .as_str(),
        "<parser_error>&lt;select&gt;&amp;</parser_error>"
    );
}

#[tokio::test]
async fn turn_artifact_propagates_canonical_renderer_errors() {
    let artifact =
        TurnArtifact::try_from_document("parser_error", xml_document("parser_error", "\u{0}"))
            .unwrap();

    assert!(
        artifact.render_full(&TemplateEngine::new()).await.is_err(),
        "an invalid XML character must not fall back to trusted raw markup"
    );
}

#[test]
fn turn_artifact_rejects_a_diff_slot_at_any_document_depth() {
    let slotted = XmlNode::try_build("slotted", |children| {
        children.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            XmlNode::try_build("agent_context", |children| {
                children.text(TextNode::new("must not enter an ephemeral artifact"));
                Ok(())
            })?,
        ));
        Ok(())
    })
    .unwrap();

    let mut inline = InlineChildren::new();
    inline.push(InlineContent::xml(slotted));

    let document = Document::try_build(|blocks| {
        blocks.xml(XmlNode::try_build("outer", |children| {
            children.markdown(MarkdownNode::Paragraph(ParagraphNode::new(inline)));
            Ok(())
        })?);
        Ok(())
    })
    .unwrap();

    assert!(TurnArtifact::try_from_document("parser_error", document).is_err());
}

#[tokio::test]
async fn default_turn_prompt_keeps_typed_artifacts_in_legacy_layout_order() {
    let prompt = DefaultTurnPrompt {
        task: "continue".to_owned(),
        artifacts: vec![
            text_artifact("first_feedback", "first"),
            text_artifact("second_feedback", "second"),
        ],
        template: concat!(
            "{% for artifact in artifacts %}",
            "[{{ artifact.kind }}]{{ artifact.rendered }}",
            "{% endfor %}",
            "|{{ task }}"
        )
        .into(),
    };

    assert_eq!(
        prompt
            .render_full(&TemplateEngine::new())
            .await
            .unwrap()
            .as_str(),
        concat!(
            "[first_feedback]<first_feedback>first</first_feedback>",
            "[second_feedback]<second_feedback>second</second_feedback>",
            "|continue"
        )
    );
}

#[test]
fn deserializing_default_context_drops_ephemeral_artifacts_but_keeps_the_task() {
    let state = DefaultContextState {
        feedback: DefaultAgentFeedback {
            artifacts: vec![text_artifact("parser_error", "retry")],
            task: Some("try again".to_owned()),
        },
    };
    let serialized = serde_json::to_string(&state).unwrap();
    let restored: DefaultContextState = serde_json::from_str(&serialized).unwrap();

    assert!(restored.feedback.artifacts.is_empty());
    assert_eq!(restored.feedback.task.as_deref(), Some("try again"));
}
