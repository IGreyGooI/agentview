use std::collections::BTreeMap;
use std::fmt;

use agentview::prelude::{render_agent_view_xml, AgentView};
use serde_json::json;

#[derive(AgentView, PartialEq, Eq, PartialOrd, Ord)]
#[agent_view(kind = "actor")]
struct ActorView {
    id: String,
    name: String,
    #[view(element)]
    goal: String,
}

#[derive(AgentView)]
#[agent_view(kind = "square")]
struct SquareView {
    id: String,

    #[view(text)]
    piece: char,
}

#[derive(AgentView)]
#[agent_view(kind = "rank")]
struct RankView {
    n: u8,

    #[view(flatten)]
    squares: Vec<SquareView>,
}

#[derive(AgentView)]
#[agent_view(kind = "renamed_rank")]
struct RenamedRankView {
    #[view(attr, name = "n")]
    rank: u8,
}

#[derive(AgentView)]
#[agent_view(kind = "turn_prompt")]
struct PromptWithRuntimeState {
    #[view(element)]
    task: String,

    #[allow(dead_code)]
    #[view(skip)]
    reply_schema: serde_json::Value,
}

#[test]
fn derive_renders_default_attrs_and_explicit_scalar_elements() {
    let actor = ActorView {
        id: "actor.1".to_owned(),
        name: "Rachel Verinder".to_owned(),
        goal: "find the Moonstone".to_owned(),
    };

    assert_eq!(
        render_agent_view_xml(&actor),
        r#"<actor id="actor.1" name="Rachel Verinder">
  <goal>find the Moonstone</goal>
</actor>"#
    );
}

#[test]
fn derive_prefers_prompt_facing_field_names_and_flattened_children() {
    let rank = RankView {
        n: 8,
        squares: vec![
            SquareView {
                id: "a8".to_owned(),
                piece: 'r',
            },
            SquareView {
                id: "b8".to_owned(),
                piece: 'n',
            },
        ],
    };

    assert_eq!(
        render_agent_view_xml(&rank),
        r#"<rank n="8">
  <square id="a8">r</square>
  <square id="b8">n</square>
</rank>"#
    );
}

#[test]
fn attr_name_override_is_an_explicit_escape_hatch() {
    let rank = RenamedRankView { rank: 8 };

    assert_eq!(render_agent_view_xml(&rank), r#"<renamed_rank n="8" />"#);
}

#[test]
fn skipped_fields_are_not_rendered_and_need_no_agent_view_impl() {
    let prompt = PromptWithRuntimeState {
        task: "Choose a move.".to_owned(),
        reply_schema: json!({
            "type": "object",
            "required": ["uci"],
        }),
    };

    assert_eq!(
        render_agent_view_xml(&prompt),
        r#"<turn_prompt>
  <task>Choose a move.</task>
</turn_prompt>"#
    );
}

#[derive(AgentView)]
struct DefaultKindView {
    id: String,
}

#[test]
fn derive_defaults_kind_from_type_name() {
    let view = DefaultKindView {
        id: "default.1".to_owned(),
    };

    assert_eq!(
        render_agent_view_xml(&view),
        r#"<default_kind_view id="default.1" />"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "scene")]
struct SceneView {
    id: String,
    protagonist: ActorView,
    #[view(element)]
    summary: String,
}

#[test]
fn derive_renders_nested_agent_view_fields_as_slots_with_kind() {
    let scene = SceneView {
        id: "scene.1".to_owned(),
        protagonist: ActorView {
            id: "actor.1".to_owned(),
            name: "Rachel Verinder".to_owned(),
            goal: "find the Moonstone".to_owned(),
        },
        summary: "The Moonstone has vanished.".to_owned(),
    };

    assert_eq!(
        render_agent_view_xml(&scene),
        r#"<scene id="scene.1">
  <protagonist kind="actor" id="actor.1" name="Rachel Verinder">
    <goal>find the Moonstone</goal>
  </protagonist>
  <summary>The Moonstone has vanished.</summary>
</scene>"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "annotated_actor")]
struct AnnotatedActorView {
    id: String,
    #[view(text)]
    description: String,
    #[view(comment)]
    debug_note: String,
    #[view(element)]
    goal: String,
}

#[test]
fn derive_renders_text_and_comment_fields_as_fragments() {
    let actor = AnnotatedActorView {
        id: "actor.1".to_owned(),
        description: "Rachel Verinder, heiress of the Moonstone.".to_owned(),
        debug_note: "loaded from director state".to_owned(),
        goal: "find the Moonstone".to_owned(),
    };

    assert_eq!(
        render_agent_view_xml(&actor),
        r#"<annotated_actor id="actor.1">
  Rachel Verinder, heiress of the Moonstone.
  <!-- loaded from director state -->
  <goal>find the Moonstone</goal>
</annotated_actor>"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "goal")]
struct GoalView {
    id: String,
    #[view(element)]
    description: String,
}

#[derive(AgentView)]
#[agent_view(kind = "cast")]
struct CastView {
    actors: Vec<ActorView>,
}

#[test]
fn vec_of_agent_views_renders_as_field_container_with_item_root_nodes() {
    let cast = CastView {
        actors: vec![
            ActorView {
                id: "actor.1".to_owned(),
                name: "Rachel".to_owned(),
                goal: "find the Moonstone".to_owned(),
            },
            ActorView {
                id: "actor.2".to_owned(),
                name: "Franklin".to_owned(),
                goal: "protect Rachel".to_owned(),
            },
        ],
    };

    assert_eq!(
        render_agent_view_xml(&cast),
        r#"<cast>
  <actors>
    <actor id="actor.1" name="Rachel">
      <goal>find the Moonstone</goal>
    </actor>
    <actor id="actor.2" name="Franklin">
      <goal>protect Rachel</goal>
    </actor>
  </actors>
</cast>"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "notebook")]
struct NotebookView {
    notes: Vec<String>,
}

#[test]
fn vec_of_strings_renders_as_field_container_with_item_text_nodes() {
    let notebook = NotebookView {
        notes: vec!["first note".to_owned(), "second note".to_owned()],
    };

    assert_eq!(
        render_agent_view_xml(&notebook),
        r#"<notebook>
  <notes>
    <item>first note</item>
    <item>second note</item>
  </notes>
</notebook>"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "metadata")]
struct MetadataView {
    properties: BTreeMap<String, String>,
}

#[test]
fn string_to_string_btreemap_renders_entries_with_key_value_attrs() {
    let mut properties = BTreeMap::new();
    properties.insert("location".to_owned(), "drawing_room".to_owned());
    properties.insert("mood".to_owned(), "tense".to_owned());

    let metadata = MetadataView { properties };

    assert_eq!(
        render_agent_view_xml(&metadata),
        r#"<metadata>
  <properties>
    <entry key="location" value="drawing_room" />
    <entry key="mood" value="tense" />
  </properties>
</metadata>"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "assignment_set")]
struct AssignmentSetView {
    assignments: BTreeMap<ActorView, GoalView>,
}

#[test]
fn structured_btreemap_renders_key_and_value_slots_with_kinds() {
    let mut assignments = BTreeMap::new();
    assignments.insert(
        ActorView {
            id: "actor.1".to_owned(),
            name: "Rachel".to_owned(),
            goal: "recover the Moonstone".to_owned(),
        },
        GoalView {
            id: "goal.1".to_owned(),
            description: "find the Moonstone".to_owned(),
        },
    );

    let assignment_set = AssignmentSetView { assignments };

    assert_eq!(
        render_agent_view_xml(&assignment_set),
        r#"<assignment_set>
  <assignments>
    <entry>
      <key kind="actor" id="actor.1" name="Rachel">
        <goal>recover the Moonstone</goal>
      </key>
      <value kind="goal" id="goal.1">
        <description>find the Moonstone</description>
      </value>
    </entry>
  </assignments>
</assignment_set>"#
    );
}

#[test]
fn vec_root_uses_list_fallback_container() {
    let notes = vec!["first note".to_owned(), "second note".to_owned()];

    assert_eq!(
        render_agent_view_xml(&notes),
        r#"<list>
  <item>first note</item>
  <item>second note</item>
</list>"#
    );
}

#[test]
fn btreemap_root_uses_map_fallback_container() {
    let mut properties = BTreeMap::new();
    properties.insert("location".to_owned(), "drawing_room".to_owned());
    properties.insert("mood".to_owned(), "tense".to_owned());

    assert_eq!(
        render_agent_view_xml(&properties),
        r#"<map>
  <entry key="location" value="drawing_room" />
  <entry key="mood" value="tense" />
</map>"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "scalar")]
struct ScalarView {
    active: bool,
    marker: char,
    u8_value: u8,
    u16_value: u16,
    u32_value: u32,
    u64_value: u64,
    u128_value: u128,
    usize_value: usize,
    i8_value: i8,
    i16_value: i16,
    i32_value: i32,
    i64_value: i64,
    i128_value: i128,
    isize_value: isize,
    f32_value: f32,
    f64_value: f64,
}

#[test]
fn common_scalars_render_as_root_text_and_field_attrs() {
    assert_eq!(render_agent_view_xml(&42u32), "42");

    let scalars = ScalarView {
        active: true,
        marker: 'R',
        u8_value: 1,
        u16_value: 2,
        u32_value: 3,
        u64_value: 4,
        u128_value: 5,
        usize_value: 6,
        i8_value: -1,
        i16_value: -2,
        i32_value: -3,
        i64_value: -4,
        i128_value: -5,
        isize_value: -6,
        f32_value: 1.5,
        f64_value: 2.25,
    };

    assert_eq!(
        render_agent_view_xml(&scalars),
        r#"<scalar active="true" marker="R" u8_value="1" u16_value="2" u32_value="3" u64_value="4" u128_value="5" usize_value="6" i8_value="-1" i16_value="-2" i32_value="-3" i64_value="-4" i128_value="-5" isize_value="-6" f32_value="1.5" f64_value="2.25" />"#
    );
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, AgentView)]
#[agent_view(display)]
struct ActorId(String);

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(AgentView)]
#[agent_view(display)]
enum ActorStatus {
    Active,
}

impl fmt::Display for ActorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActorStatus::Active => f.write_str("active"),
        }
    }
}

#[derive(AgentView)]
#[agent_view(kind = "display_actor")]
struct DisplayActorView {
    id: ActorId,
    name: String,
    status: ActorStatus,
}

#[test]
fn display_agent_view_renders_as_scalar() {
    assert_eq!(
        render_agent_view_xml(&ActorId("actor.1".to_owned())),
        "actor.1"
    );

    let actor = DisplayActorView {
        id: ActorId("actor.1".to_owned()),
        name: "Rachel".to_owned(),
        status: ActorStatus::Active,
    };

    assert_eq!(
        render_agent_view_xml(&actor),
        r#"<display_actor id="actor.1" name="Rachel" status="active" />"#
    );
}

#[derive(AgentView)]
#[agent_view(kind = "optional_actor")]
struct OptionalActorView {
    id: String,
    alias: Option<String>,
    goal: Option<GoalView>,
    actors: Option<Vec<ActorView>>,
}

#[test]
fn option_fields_render_some_values_and_omit_none() {
    let actor = OptionalActorView {
        id: "actor.1".to_owned(),
        alias: Some("Rosanna".to_owned()),
        goal: Some(GoalView {
            id: "goal.1".to_owned(),
            description: "find the Moonstone".to_owned(),
        }),
        actors: Some(vec![ActorView {
            id: "actor.2".to_owned(),
            name: "Franklin".to_owned(),
            goal: "protect Rachel".to_owned(),
        }]),
    };

    assert_eq!(
        render_agent_view_xml(&actor),
        r#"<optional_actor id="actor.1" alias="Rosanna">
  <goal kind="goal" id="goal.1">
    <description>find the Moonstone</description>
  </goal>
  <actors>
    <actor id="actor.2" name="Franklin">
      <goal>protect Rachel</goal>
    </actor>
  </actors>
</optional_actor>"#
    );

    let actor = OptionalActorView {
        id: "actor.2".to_owned(),
        alias: None,
        goal: None,
        actors: None,
    };

    assert_eq!(
        render_agent_view_xml(&actor),
        r#"<optional_actor id="actor.2" />"#
    );
}

#[test]
fn option_root_renders_some_root_or_none_node() {
    let alias = Some("Rosanna".to_owned());
    assert_eq!(render_agent_view_xml(&alias), "Rosanna");

    let absent = None::<String>;
    assert_eq!(render_agent_view_xml(&absent), "<none />");
}
