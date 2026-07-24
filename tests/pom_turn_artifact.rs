use agentview::agent::{DefaultAgentFeedback, DefaultContextState};
use agentview::pom::{
    DiffSlot, DiffStrategy, Document, InlineChildren, InlineContent, MarkdownNode, ParagraphNode,
    TextNode, XmlNode,
};
use agentview::pom_cursor::UserDocumentCursor;
use agentview::pom_renderer::render_pom_document;
use agentview::pom_resolution::resolve_user_document;
use agentview::templates::TurnArtifact;
use agentview::AgentView;

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

#[test]
fn turn_artifact_renders_canonical_pom_and_exposes_its_kind() {
    let artifact =
        TurnArtifact::try_from_document("parser_error", xml_document("parser_error", "<select>&"))
            .unwrap();

    assert_eq!(artifact.kind(), "parser_error");
    assert_eq!(
        render_pom_document(artifact.document()).unwrap(),
        "<parser_error>&lt;select&gt;&amp;</parser_error>"
    );
}

#[test]
fn turn_artifact_propagates_canonical_renderer_errors() {
    let artifact =
        TurnArtifact::try_from_document("parser_error", xml_document("parser_error", "\u{0}"))
            .unwrap();

    assert!(
        render_pom_document(artifact.document()).is_err(),
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

#[derive(AgentView)]
#[agent_view(markdown = "paragraph")]
struct ArtifactTaskView {
    #[view(text)]
    text: &'static str,
}

#[derive(AgentView)]
#[agent_view(document)]
struct ArtifactUserDocumentView {
    #[view(block)]
    artifacts: Vec<TurnArtifact>,

    #[view(block)]
    task: ArtifactTaskView,
}

#[test]
fn derived_user_document_splices_typed_artifact_blocks_in_order() {
    let prompt = ArtifactUserDocumentView {
        artifacts: vec![
            text_artifact("first_feedback", "first"),
            text_artifact("second_feedback", "second"),
        ],
        task: ArtifactTaskView { text: "continue" },
    };
    let (resolved, cursor) =
        resolve_user_document(prompt.build_root().unwrap(), &UserDocumentCursor::default())
            .unwrap();

    assert_eq!(
        render_pom_document(&resolved).unwrap(),
        concat!(
            "<first_feedback>first</first_feedback>\n\n",
            "<second_feedback>second</second_feedback>\n\n",
            "continue"
        )
    );
    assert!(
        cursor.is_empty(),
        "ephemeral artifacts do not enter the cursor"
    );
}

#[test]
fn one_typed_artifact_can_splice_multiple_pom_blocks_without_rendering_a_string() {
    let artifact = TurnArtifact::try_from_document(
        "composite_feedback",
        Document::try_build(|blocks| {
            blocks.try_paragraph(|inline| inline.try_text("first block"))?;
            blocks.xml(XmlNode::try_build("detail", |children| {
                children.text(TextNode::new("second block"));
                Ok(())
            })?);
            Ok(())
        })
        .unwrap(),
    )
    .unwrap();
    let prompt = ArtifactUserDocumentView {
        artifacts: vec![artifact],
        task: ArtifactTaskView { text: "continue" },
    };

    let document = prompt.build_root().unwrap();
    assert_eq!(document.children().iter().count(), 3);
    let (resolved, _) = resolve_user_document(document, &UserDocumentCursor::default()).unwrap();
    assert_eq!(
        render_pom_document(&resolved).unwrap(),
        "first block\n\n<detail>second block</detail>\n\ncontinue"
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
