use agentview::agent_view::AgentView;
use agentview::pom::{
    ContentNode, ContentRef, DiffStrategy, Document, MarkdownNode, TextNode, XmlName, XmlNode,
};
use agentview::templates::TurnArtifact;

#[derive(agentview::AgentView)]
#[agent_view(kind = "context_state")]
struct ContextStateView {
    id: String,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "parser_error")]
struct ParserErrorView {
    #[view(text)]
    message: String,
}

#[derive(agentview::AgentView)]
#[agent_view(markdown = "paragraph")]
struct TaskParagraphView {
    #[view(text)]
    text: String,
}

#[derive(agentview::AgentView)]
#[agent_view(document)]
struct UserDocumentView {
    #[view(name = "agent_context", diff(replace))]
    context: ContextStateView,

    #[view(block)]
    artifacts: Vec<TurnArtifact>,

    #[view(block)]
    retry: Option<TaskParagraphView>,

    #[view(block)]
    task: TaskParagraphView,
}

#[derive(agentview::AgentView)]
#[agent_view(document)]
struct OptionalContextDocumentView {
    #[view(name = "agent_context", diff)]
    context: Option<ContextStateView>,
}

#[derive(agentview::AgentView)]
#[agent_view(document)]
struct RawXmlContextDocumentView {
    #[view(diff)]
    agent_context: XmlNode,
}

#[test]
fn document_derive_composes_a_diff_slot_artifacts_and_typed_blocks_in_field_order() {
    let artifact = TurnArtifact::try_from_view(&ParserErrorView {
        message: "retry <select>".to_owned(),
    })
    .unwrap();
    let document = UserDocumentView {
        context: ContextStateView {
            id: "context.1".to_owned(),
        },
        artifacts: vec![artifact],
        retry: Some(TaskParagraphView {
            text: "Retry the workflow.".to_owned(),
        }),
        task: TaskParagraphView {
            text: "Choose one intent.".to_owned(),
        },
    }
    .build_root()
    .unwrap();

    let children = document.children().iter().collect::<Vec<_>>();
    assert_eq!(children.len(), 4);

    let ContentRef::DiffSlot(context) = children[0] else {
        panic!("expected the first block edge to be the context DiffSlot");
    };
    assert_eq!(context.role(), &XmlName::try_from("agent_context").unwrap());
    assert_eq!(context.strategy(), &DiffStrategy::Replace);
    let context = context.value().expect("context should be present");
    assert_eq!(context.name().as_str(), "agent_context");
    assert_eq!(
        context
            .attributes()
            .get(&XmlName::try_from("kind").unwrap())
            .unwrap()
            .value(),
        "context_state"
    );

    let ContentRef::Node(ContentNode::Xml(artifact)) = children[1] else {
        panic!("expected a typed XML artifact block");
    };
    assert_eq!(artifact.name().as_str(), "parser_error");

    assert!(matches!(
        children[2],
        ContentRef::Node(ContentNode::Markdown(MarkdownNode::Paragraph(_)))
    ));
    assert!(matches!(
        children[3],
        ContentRef::Node(ContentNode::Markdown(MarkdownNode::Paragraph(_)))
    ));
}

#[test]
fn optional_none_document_diff_field_builds_an_explicit_absent_slot() {
    let document = OptionalContextDocumentView { context: None }
        .build_root()
        .unwrap();

    let children = document.children().iter().collect::<Vec<_>>();
    let [ContentRef::DiffSlot(context)] = children.as_slice() else {
        panic!("expected one absent context DiffSlot");
    };
    assert_eq!(context.role(), &XmlName::try_from("agent_context").unwrap());
    assert_eq!(context.strategy(), &DiffStrategy::Recursive);
    assert!(!context.is_present());
}

#[test]
fn document_diff_accepts_a_typed_raw_xml_root_without_rendering_it() {
    let source = XmlNode::try_build("agent_context", |children| {
        children.text(TextNode::new("typed <state>"));
        Ok(())
    })
    .unwrap();
    let document = RawXmlContextDocumentView {
        agent_context: source.clone(),
    }
    .build_root()
    .unwrap();

    let children = document.children().iter().collect::<Vec<_>>();
    let [ContentRef::DiffSlot(context)] = children.as_slice() else {
        panic!("expected one raw XML context DiffSlot");
    };
    assert_eq!(context.role(), &XmlName::try_from("agent_context").unwrap());
    assert_eq!(context.value(), Some(&source));
}

fn assert_document_view<T: AgentView<Root = Document>>() {}

#[test]
fn user_document_is_statically_a_document_root() {
    assert_document_view::<UserDocumentView>();
}
