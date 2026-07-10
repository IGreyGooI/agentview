use std::collections::BTreeMap;

use agentview::prelude::{
    render_agent_view_diff_xml, AgentView, SemanticField, SemanticFragment, SemanticNode,
};
use agentview::semantic_view::SemanticDiffStrategy;
use serde_json::json;

#[derive(AgentView)]
#[agent_view(kind = "diff_spellings")]
struct DiffSpellingsView {
    #[view(diff)]
    implicit: String,

    #[view(element, diff)]
    explicit: String,
}

#[derive(AgentView)]
#[agent_view(kind = "actor")]
struct ActorView {
    id: String,
    name: String,

    #[view(element, diff)]
    goal: String,
}

#[derive(AgentView)]
#[agent_view(kind = "scene")]
struct SceneView {
    id: String,
    title: String,

    #[view(diff)]
    summary: String,

    #[view(diff(append))]
    actors: Vec<ActorView>,

    #[view(diff)]
    protagonist: ActorView,
}

#[derive(AgentView)]
#[agent_view(kind = "inventory")]
struct InventoryView {
    id: String,

    #[view(diff(set))]
    items: Vec<String>,
}

#[derive(AgentView)]
#[agent_view(kind = "journal")]
struct JournalView {
    id: String,

    #[view(diff(seq))]
    entries: Vec<String>,
}

#[derive(AgentView)]
#[agent_view(kind = "cast")]
struct CastView {
    id: String,

    #[view(diff(key = "id"))]
    actors: Vec<ActorView>,
}

#[derive(AgentView)]
#[agent_view(kind = "status_state")]
struct StatusStateView {
    #[view(element)]
    status: String,

    #[view(element)]
    summary: String,
}

#[derive(AgentView)]
#[agent_view(kind = "status_panel")]
struct StatusPanelView {
    id: String,

    #[view(diff(replace))]
    state: StatusStateView,
}

#[derive(AgentView)]
#[agent_view(kind = "metadata")]
struct MetadataView {
    id: String,

    #[view(diff)]
    properties: BTreeMap<String, String>,
}

#[derive(AgentView)]
#[agent_view(kind = "maybe_alias")]
struct MaybeAliasView {
    id: String,

    #[view(diff)]
    alias: Option<String>,
}

struct DynamicField(Option<String>);

impl AgentView for DynamicField {
    fn render_root(&self) -> SemanticFragment {
        self.0
            .as_ref()
            .map(|value| SemanticFragment::Text(value.clone()))
            .unwrap_or_else(|| SemanticFragment::Node(SemanticNode::new("none")))
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        self.0
            .as_ref()
            .map(|value| SemanticField::Attr {
                name: field_name.to_owned(),
                value: value.clone(),
            })
            .unwrap_or(SemanticField::Empty)
    }
}

#[derive(AgentView)]
#[agent_view(kind = "dynamic")]
struct DynamicView {
    #[view(diff)]
    value: DynamicField,
}

struct InterleavedChildrenView {
    slot_first: bool,
}

impl InterleavedChildrenView {
    fn render_node(&self, tag: &'static str) -> SemanticNode {
        let mut node = SemanticNode::new(tag);
        let push_slot = |node: &mut SemanticNode| {
            node.push_diff_field(
                "marked",
                SemanticDiffStrategy::Recursive,
                SemanticField::Attr {
                    name: "marked".to_owned(),
                    value: "same".to_owned(),
                },
            );
        };
        let push_fragment = |node: &mut SemanticNode| {
            node.push_child(SemanticNode::element("ordinary", "same"));
        };

        if self.slot_first {
            push_slot(&mut node);
            push_fragment(&mut node);
        } else {
            push_fragment(&mut node);
            push_slot(&mut node);
        }
        node
    }
}

impl AgentView for InterleavedChildrenView {
    fn render_root(&self) -> SemanticFragment {
        SemanticFragment::Node(self.render_node("interleaved"))
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        SemanticField::Fragment(SemanticFragment::Node(self.render_node(field_name)))
    }
}

#[derive(AgentView)]
#[agent_view(kind = "prompt_state")]
struct PromptStateView {
    id: String,

    #[allow(dead_code)]
    #[view(skip)]
    reply_schema: serde_json::Value,

    #[view(diff)]
    summary: String,
}

fn actor(id: &str, name: &str, goal: &str) -> ActorView {
    ActorView {
        id: id.to_owned(),
        name: name.to_owned(),
        goal: goal.to_owned(),
    }
}

fn scene(summary: &str, actors: Vec<ActorView>, protagonist: ActorView) -> SceneView {
    SceneView {
        id: "scene.1".to_owned(),
        title: "The Moonstone".to_owned(),
        summary: summary.to_owned(),
        actors,
        protagonist,
    }
}

fn metadata(properties: &[(&str, &str)]) -> MetadataView {
    MetadataView {
        id: "metadata.1".to_owned(),
        properties: properties
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

fn maybe_alias(alias: Option<&str>) -> MaybeAliasView {
    MaybeAliasView {
        id: "maybe_alias.1".to_owned(),
        alias: alias.map(str::to_owned),
    }
}

fn prompt_state(schema_version: u8, summary: &str) -> PromptStateView {
    PromptStateView {
        id: "prompt.1".to_owned(),
        reply_schema: json!({
            "schema_version": schema_version,
        }),
        summary: summary.to_owned(),
    }
}

fn inventory(items: &[&str]) -> InventoryView {
    InventoryView {
        id: "inventory.1".to_owned(),
        items: items.iter().map(|item| (*item).to_owned()).collect(),
    }
}

fn journal(entries: &[&str]) -> JournalView {
    JournalView {
        id: "journal.1".to_owned(),
        entries: entries.iter().map(|entry| (*entry).to_owned()).collect(),
    }
}

fn cast(actors: Vec<ActorView>) -> CastView {
    CastView {
        id: "cast.1".to_owned(),
        actors,
    }
}

#[test]
fn scalar_root_is_an_implicit_diff_slot() {
    assert_eq!(
        render_agent_view_diff_xml(&"new".to_owned(), &"old".to_owned()),
        Some("new".to_owned())
    );
    assert_eq!(
        render_agent_view_diff_xml(&"same".to_owned(), &"same".to_owned()),
        None
    );
}

#[test]
fn reordered_diff_slots_and_fragments_replace_complete_root() {
    let previous = InterleavedChildrenView { slot_first: false };
    let current = InterleavedChildrenView { slot_first: true };

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<interleaved>
  <marked>same</marked>
  <ordinary>same</ordinary>
</interleaved>"#
                .to_owned()
        )
    );
}

#[test]
fn dynamic_diff_slots_handle_empty_complete_fields() {
    assert_eq!(
        render_agent_view_diff_xml(
            &DynamicView {
                value: DynamicField(None),
            },
            &DynamicView {
                value: DynamicField(Some("present".to_owned())),
            },
        ),
        Some(
            r#"<dynamic rendering_mode="delta">
  <value>
    <none />
  </value>
</dynamic>"#
                .to_owned()
        )
    );

    assert_eq!(
        render_agent_view_diff_xml(
            &DynamicView {
                value: DynamicField(None),
            },
            &DynamicView {
                value: DynamicField(None),
            },
        ),
        None
    );

    assert_eq!(
        render_agent_view_diff_xml(
            &DynamicView {
                value: DynamicField(Some("present".to_owned())),
            },
            &DynamicView {
                value: DynamicField(None),
            },
        ),
        Some(
            r#"<dynamic rendering_mode="delta">
  <value>present</value>
</dynamic>"#
                .to_owned()
        )
    );
}

fn status_panel(status: &str, summary: &str) -> StatusPanelView {
    StatusPanelView {
        id: "status_panel.1".to_owned(),
        state: StatusStateView {
            status: status.to_owned(),
            summary: summary.to_owned(),
        },
    }
}

#[test]
fn diff_does_not_require_type_level_marker() {
    let previous = scene(
        "The Moonstone has vanished.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );
    let current = scene(
        "The Moonstone has vanished.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );

    assert_eq!(render_agent_view_diff_xml(&current, &previous), None);
}

#[test]
fn diff_and_element_diff_both_render_addressable_nodes() {
    let view = DiffSpellingsView {
        implicit: "one".to_owned(),
        explicit: "two".to_owned(),
    };

    assert_eq!(
        agentview::prelude::render_agent_view_xml(&view),
        "<diff_spellings>\n  <implicit>one</implicit>\n  <explicit>two</explicit>\n</diff_spellings>"
    );
}

#[test]
fn skipped_field_changes_do_not_trigger_diff() {
    let previous = prompt_state(1, "Choose a move.");
    let current = prompt_state(2, "Choose a move.");

    assert_eq!(render_agent_view_diff_xml(&current, &previous), None);
}

#[test]
fn unmarked_field_changes_render_the_current_root() {
    let previous = SceneView {
        id: "scene.1".to_owned(),
        title: "The Moonstone".to_owned(),
        summary: "The Moonstone has vanished.".to_owned(),
        actors: vec![actor("actor.1", "Rachel", "find the Moonstone")],
        protagonist: actor("actor.1", "Rachel", "find the Moonstone"),
    };
    let current = SceneView {
        id: "scene.1".to_owned(),
        title: "A different title".to_owned(),
        summary: "The Moonstone has vanished.".to_owned(),
        actors: vec![actor("actor.1", "Rachel", "find the Moonstone")],
        protagonist: actor("actor.1", "Rachel", "find the Moonstone"),
    };

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<scene id="scene.1" title="A different title">
  <summary>The Moonstone has vanished.</summary>
  <actors>
    <actor id="actor.1" name="Rachel">
      <goal>find the Moonstone</goal>
    </actor>
  </actors>
  <protagonist kind="actor" id="actor.1" name="Rachel">
    <goal>find the Moonstone</goal>
  </protagonist>
</scene>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_scalar_field_changes_emit_field_patch() {
    let previous = scene(
        "The Moonstone has vanished.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );
    let current = scene(
        "Rachel has a new lead.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<scene rendering_mode="delta">
  <summary>Rachel has a new lead.</summary>
</scene>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_vec_field_emits_insert_operation_for_new_items() {
    let previous = scene(
        "The Moonstone has vanished.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );
    let current = scene(
        "The Moonstone has vanished.",
        vec![
            actor("actor.1", "Rachel", "find the Moonstone"),
            actor("actor.2", "Franklin", "protect Rachel"),
        ],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<scene rendering_mode="delta">
  <actors rendering_mode="delta">
    <insert>
      <actor id="actor.2" name="Franklin">
        <goal>protect Rachel</goal>
      </actor>
    </insert>
  </actors>
</scene>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_vec_set_field_emits_insert_and_remove_operations() {
    let previous = inventory(&["lantern", "letter"]);
    let current = inventory(&["letter", "moonstone"]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<inventory rendering_mode="delta">
  <items rendering_mode="delta">
    <insert>
      <item>moonstone</item>
    </insert>
    <remove>
      <item>lantern</item>
    </remove>
  </items>
</inventory>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_vec_seq_field_emits_insert_for_appended_tail() {
    let previous = journal(&["Rachel arrives"]);
    let current = journal(&["Rachel arrives", "Franklin follows"]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<journal rendering_mode="delta">
  <entries rendering_mode="delta">
    <insert>
      <item>Franklin follows</item>
    </insert>
  </entries>
</journal>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_vec_seq_field_emits_remove_for_removed_items() {
    let previous = journal(&["Rachel arrives", "Franklin follows"]);
    let current = journal(&["Rachel arrives"]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<journal rendering_mode="delta">
  <entries rendering_mode="delta">
    <remove>
      <item>Franklin follows</item>
    </remove>
  </entries>
</journal>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_vec_keyed_field_emits_insert_remove_and_update_operations() {
    let previous = cast(vec![
        actor("actor.1", "Rachel", "find the Moonstone"),
        actor("actor.2", "Franklin", "protect Rachel"),
    ]);
    let current = cast(vec![
        actor("actor.1", "Rachel", "question the servants"),
        actor("actor.3", "Betteredge", "guard the house"),
    ]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<cast rendering_mode="delta">
  <actors rendering_mode="delta">
    <insert>
      <actor id="actor.3" name="Betteredge">
        <goal>guard the house</goal>
      </actor>
    </insert>
    <remove>
      <actor id="actor.2" name="Franklin">
        <goal>protect Rachel</goal>
      </actor>
    </remove>
    <update>
      <actor id="actor.1" name="Rachel">
        <goal>question the servants</goal>
      </actor>
    </update>
  </actors>
</cast>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_replace_field_wraps_current_field_in_replace_operation() {
    let previous = status_panel("stable", "No urgent changes.");
    let current = status_panel("changed", "Board state should be reread.");

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<status_panel rendering_mode="delta">
  <state rendering_mode="delta">
    <replace>
      <state kind="status_state">
        <status>changed</status>
        <summary>Board state should be reread.</summary>
      </state>
    </replace>
  </state>
</status_panel>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_btreemap_field_emits_insert_operation_for_new_entries() {
    let previous = metadata(&[("location", "drawing_room")]);
    let current = metadata(&[("location", "drawing_room"), ("mood", "tense")]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<metadata rendering_mode="delta">
  <properties rendering_mode="delta">
    <insert>
      <entry key="mood" value="tense" />
    </insert>
  </properties>
</metadata>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_btreemap_field_emits_remove_operation_for_removed_entries() {
    let previous = metadata(&[("location", "drawing_room"), ("mood", "tense")]);
    let current = metadata(&[("location", "drawing_room")]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<metadata rendering_mode="delta">
  <properties rendering_mode="delta">
    <remove>
      <entry key="mood" value="tense" />
    </remove>
  </properties>
</metadata>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_btreemap_field_emits_update_operation_for_changed_entries() {
    let previous = metadata(&[("location", "drawing_room"), ("mood", "tense")]);
    let current = metadata(&[("location", "drawing_room"), ("mood", "calm")]);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<metadata rendering_mode="delta">
  <properties rendering_mode="delta">
    <update>
      <entry key="mood" value="calm" />
    </update>
  </properties>
</metadata>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_option_field_emits_value_patch_when_filled() {
    let previous = maybe_alias(None);
    let current = maybe_alias(Some("Rosanna"));

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<maybe_alias rendering_mode="delta">
  <alias>Rosanna</alias>
</maybe_alias>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_option_field_emits_none_patch_when_cleared() {
    let previous = maybe_alias(Some("Rosanna"));
    let current = maybe_alias(None);

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<maybe_alias rendering_mode="delta">
  <alias>
    <none />
  </alias>
</maybe_alias>"#
                .to_owned()
        )
    );
}

#[test]
fn marked_struct_field_recurses_into_its_own_marked_fields() {
    let previous = scene(
        "The Moonstone has vanished.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "find the Moonstone"),
    );
    let current = scene(
        "The Moonstone has vanished.",
        vec![actor("actor.1", "Rachel", "find the Moonstone")],
        actor("actor.1", "Rachel", "question the servants"),
    );

    assert_eq!(
        render_agent_view_diff_xml(&current, &previous),
        Some(
            r#"<scene rendering_mode="delta">
  <protagonist rendering_mode="delta" kind="actor">
    <goal>question the servants</goal>
  </protagonist>
</scene>"#
                .to_owned()
        )
    );
}
