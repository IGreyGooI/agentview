use agentview::agent_view::AgentView as PomAgentView;
use agentview::pom::{
    ContentNode, ContentRef, DiffStrategy, Document, MarkdownNode, ParagraphNode, XmlName, XmlNode,
};
use agentview::pom_renderer::render_pom_document;
use agentview::pom_resolution::resolve_system_document;
use agentview::semantic_view::render_agent_view_xml;

#[derive(agentview::AgentView)]
#[agent_view(kind = "contract")]
struct ContractView {
    id: String,

    #[view(element)]
    instruction: String,

    #[view(diff)]
    status: String,
}

#[derive(agentview::AgentView)]
#[agent_view(markdown = "paragraph")]
struct WorkflowStepView {
    #[view(text)]
    before: String,

    #[view(code_span)]
    call: String,

    #[view(text)]
    after: String,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "response_contract")]
struct ResponseContractView {
    transport: String,

    #[view(element)]
    instruction: String,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "notebook")]
struct OptionalNotesView {
    notes: Vec<Option<String>>,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "details")]
struct DetailsView {
    id: String,

    #[view(element)]
    summary: String,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "panel")]
struct FlattenedDetailsView {
    #[view(flatten)]
    details: Option<DetailsView>,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "map_panel")]
struct FlattenedMapView {
    #[view(flatten)]
    properties: BTreeMap<String, String>,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "strategy_set")]
struct StrategySetView {
    #[view(diff(append))]
    appended: Vec<String>,

    #[view(diff(seq))]
    sequenced: Vec<String>,

    #[view(diff(set))]
    unordered: Vec<String>,

    #[view(diff(key = "id"))]
    keyed: Vec<DetailsView>,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "optional_patch")]
struct OptionalPatchView {
    #[view(diff)]
    alias: Option<String>,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "nested_patch")]
struct NestedPatchView {
    #[view(diff)]
    details: DiffDetailsView,
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "details")]
struct DiffDetailsView {
    id: String,

    #[view(diff)]
    summary: String,
}

#[derive(agentview::AgentView)]
#[agent_view(document)]
struct SystemPromptView {
    #[view(heading = 1)]
    title: String,

    #[view(paragraph)]
    task: String,

    #[view(heading = 2)]
    workflow_title: String,

    #[view(ordered_list)]
    workflow: Vec<WorkflowStepView>,

    #[view(xml)]
    response_contract: ResponseContractView,
}

fn build_xml(view: &impl PomAgentView<Root = XmlNode>) -> XmlNode {
    view.build_root().unwrap()
}

fn build_document(view: &impl PomAgentView<Root = Document>) -> Document {
    view.build_root().unwrap()
}

fn build_paragraph(view: &impl PomAgentView<Root = ParagraphNode>) -> ParagraphNode {
    view.build_root().unwrap()
}

#[test]
fn xml_derive_builds_attributes_elements_and_diff_slots_in_the_pom() {
    let root = build_xml(&ContractView {
        id: "contract.1".to_owned(),
        instruction: "Return one result.".to_owned(),
        status: "ready".to_owned(),
    });

    assert_eq!(root.name().as_str(), "contract");
    assert_eq!(
        root.attributes()
            .get(&XmlName::try_from("id").unwrap())
            .unwrap()
            .value(),
        "contract.1"
    );

    let children = root.children().iter().collect::<Vec<_>>();
    let ContentRef::Node(ContentNode::Xml(instruction)) = children[0] else {
        panic!("expected an XML instruction child");
    };
    assert_eq!(instruction.name().as_str(), "instruction");

    let ContentRef::DiffSlot(status) = children[1] else {
        panic!("expected a status diff slot");
    };
    assert_eq!(status.role().as_str(), "status");
    assert_eq!(status.strategy(), &DiffStrategy::Recursive);
    assert_eq!(status.value().unwrap().name().as_str(), "status");
}

#[test]
fn markdown_paragraph_derive_preserves_text_code_span_text_edges() {
    let paragraph = build_paragraph(&WorkflowStepView {
        before: "Call ".to_owned(),
        call: r#"<verify scope="demo"/>"#.to_owned(),
        after: ".".to_owned(),
    });

    let children = paragraph.children().iter().collect::<Vec<_>>();
    assert_eq!(children.len(), 3);
    assert!(matches!(
        children[0],
        ContentRef::Node(ContentNode::Text(_))
    ));
    assert!(matches!(
        children[1],
        ContentRef::Node(ContentNode::Markdown(MarkdownNode::CodeSpan(_)))
    ));
    assert!(matches!(
        children[2],
        ContentRef::Node(ContentNode::Text(_))
    ));
}

#[test]
fn document_derive_builds_and_renders_mixed_markdown_and_xml_in_field_order() {
    let document = build_document(&SystemPromptView {
        title: "Demo selector".to_owned(),
        task: "Choose one intent.".to_owned(),
        workflow_title: "Required workflow".to_owned(),
        workflow: vec![WorkflowStepView {
            before: "Call ".to_owned(),
            call: r#"<verify scope="demo"/>"#.to_owned(),
            after: ".".to_owned(),
        }],
        response_contract: ResponseContractView {
            transport: "xml".to_owned(),
            instruction: "Return only tool elements.".to_owned(),
        },
    });

    assert_eq!(document.children().len(), 5);
    assert_eq!(
        render_pom_document(&resolve_system_document(document)).unwrap(),
        concat!(
            "# Demo selector\n\n",
            "Choose one intent.\n\n",
            "## Required workflow\n\n",
            "1. Call `<verify scope=\"demo\"/>`.\n\n",
            "<response_contract transport=\"xml\">",
            "<instruction>Return only tool elements.</instruction>",
            "</response_contract>",
        )
    );
}

#[test]
fn collection_derive_omits_absent_optional_items_without_losing_present_items() {
    let root = build_xml(&OptionalNotesView {
        notes: vec![Some("first".to_owned()), None, Some("third".to_owned())],
    });

    let Some(ContentRef::Node(ContentNode::Xml(notes))) = root.children().iter().next() else {
        panic!("expected notes container");
    };
    assert_eq!(notes.name().as_str(), "notes");
    assert_eq!(notes.children().len(), 2);
}

#[test]
fn structured_flatten_preserves_the_child_root_in_pom_and_legacy_trees() {
    let view = FlattenedDetailsView {
        details: Some(DetailsView {
            id: "details.1".to_owned(),
            summary: "Ready".to_owned(),
        }),
    };

    let pom = render_pom_document(&resolve_system_document(Document::from_xml(
        view.build_root().unwrap(),
    )))
    .unwrap();

    assert_eq!(
        pom,
        "<panel><details id=\"details.1\"><summary>Ready</summary></details></panel>"
    );
    assert_eq!(
        render_agent_view_xml(&view),
        r#"<panel>
  <details id="details.1">
    <summary>Ready</summary>
  </details>
</panel>"#
    );
}

#[test]
fn map_flatten_preserves_the_map_root_in_pom_and_legacy_trees() {
    let view = FlattenedMapView {
        properties: BTreeMap::from([("mood".to_owned(), "tense".to_owned())]),
    };

    let pom = render_pom_document(&resolve_system_document(Document::from_xml(
        view.build_root().unwrap(),
    )))
    .unwrap();

    assert_eq!(
        pom,
        "<map_panel><map><entry key=\"mood\" value=\"tense\" /></map></map_panel>"
    );
    assert_eq!(
        render_agent_view_xml(&view),
        r#"<map_panel>
  <map>
    <entry key="mood" value="tense" />
  </map>
</map_panel>"#
    );
}

#[test]
fn xml_derive_preserves_every_collection_diff_strategy_in_pom_slots() {
    let root = build_xml(&StrategySetView {
        appended: vec!["a".to_owned()],
        sequenced: vec!["b".to_owned()],
        unordered: vec!["c".to_owned()],
        keyed: vec![DetailsView {
            id: "details.1".to_owned(),
            summary: "Ready".to_owned(),
        }],
    });

    let strategies = root
        .children()
        .iter()
        .map(|edge| match edge {
            ContentRef::DiffSlot(slot) => slot.strategy().clone(),
            ContentRef::Node(_) => panic!("expected only diff slots"),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        strategies,
        vec![
            DiffStrategy::Append,
            DiffStrategy::Sequence,
            DiffStrategy::Set,
            DiffStrategy::Keyed(XmlName::try_from("id").unwrap()),
        ]
    );
}

#[test]
fn optional_and_nested_diff_fields_remain_explicit_pom_edges() {
    let optional = build_xml(&OptionalPatchView { alias: None });
    let Some(ContentRef::DiffSlot(alias)) = optional.children().iter().next() else {
        panic!("expected absent alias slot");
    };
    assert_eq!(alias.role().as_str(), "alias");
    assert!(!alias.is_present());

    let nested = build_xml(&NestedPatchView {
        details: DiffDetailsView {
            id: "details.1".to_owned(),
            summary: "Ready".to_owned(),
        },
    });
    let Some(ContentRef::DiffSlot(details)) = nested.children().iter().next() else {
        panic!("expected details slot");
    };
    let nested_slot = details
        .value()
        .unwrap()
        .children()
        .iter()
        .find_map(|edge| match edge {
            ContentRef::DiffSlot(slot) => Some(slot),
            ContentRef::Node(_) => None,
        });
    assert_eq!(nested_slot.unwrap().role().as_str(), "summary");
}
use std::collections::BTreeMap;
