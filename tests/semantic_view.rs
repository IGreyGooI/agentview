use agentview::prelude::{
    render_agent_view_xml, render_semantic_fragment_xml, render_semantic_node_xml, AgentView,
    ContextView, PromptRenderable, SemanticField, SemanticNode, TemplateEngine,
};
use agentview::semantic_view::SemanticDiffStrategy;

#[derive(AgentView)]
#[agent_view(kind = "hello")]
struct HelloView {
    greeting: String,

    #[view(diff)]
    name: Option<String>,
}

#[test]
fn string_renders_as_root_text_and_field_attr() {
    let text = "hello <agent>".to_owned();
    assert_eq!(render_agent_view_xml(&text), "hello &lt;agent&gt;");

    let mut node = SemanticNode::new("actor");
    node.push_field(AgentView::render_field(&text, "name"));
    assert_eq!(
        render_semantic_node_xml(&node),
        r#"<actor name="hello &lt;agent&gt;" />"#
    );

    assert_eq!(
        AgentView::render_field(&text, "name"),
        SemanticField::Attr {
            name: "name".to_owned(),
            value: "hello <agent>".to_owned(),
        }
    );
}

#[test]
fn render_root_returns_a_fragment() {
    let fragment = "plain text".to_owned().render_root();
    assert_eq!(render_semantic_fragment_xml(&fragment), "plain text");
}

#[test]
fn diff_slots_render_present_values_as_normal_nodes() {
    let mut node = SemanticNode::new("scene");
    node.push_diff_field(
        "summary",
        SemanticDiffStrategy::Recursive,
        SemanticField::Attr {
            name: "summary".to_owned(),
            value: "A new lead".to_owned(),
        },
    );

    assert_eq!(
        render_semantic_node_xml(&node),
        "<scene>\n  <summary>A new lead</summary>\n</scene>"
    );
}

#[test]
fn empty_diff_slots_are_omitted_from_full_xml() {
    let mut node = SemanticNode::new("scene");
    node.push_diff_field(
        "summary",
        SemanticDiffStrategy::Recursive,
        SemanticField::Empty,
    );

    assert_eq!(render_semantic_node_xml(&node), "<scene />");
}

#[tokio::test]
async fn derived_agent_view_can_render_as_context_view() {
    let view = HelloView {
        greeting: "Hello".to_owned(),
        name: None,
    };

    assert_eq!(
        view.render_full(&TemplateEngine::new())
            .await
            .unwrap()
            .into_string(),
        r#"<hello greeting="Hello" />"#
    );
}

#[tokio::test]
async fn derived_agent_view_context_delta_uses_semantic_diff() {
    let previous = HelloView {
        greeting: "Hello".to_owned(),
        name: None,
    };
    let current = HelloView {
        greeting: "Hello".to_owned(),
        name: Some("world".to_owned()),
    };

    assert_eq!(
        current
            .render_delta(&previous, &TemplateEngine::new())
            .await
            .unwrap()
            .unwrap()
            .into_string(),
        r#"<hello rendering_mode="delta">
  <name>world</name>
</hello>"#
    );
}
