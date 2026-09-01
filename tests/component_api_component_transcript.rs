#![cfg(feature = "legacy-provider-port")]
#![allow(
    deprecated,
    reason = "this compatibility test intentionally exercises legacy component transcript routing"
)]

use std::sync::atomic::{AtomicUsize, Ordering};

use agentview::{
    component::{execution::ProviderEvent, prelude::*, ComponentHost},
    pom_renderer::render_pom_document,
    transcript::{CanonicalInputItem, CanonicalTranscript, ConversationRole, InstructionAuthority},
};

#[derive(Clone, Copy)]
struct TurnProps;

fn render_application<Props>(
    root: fn(Props, EventInput<ProviderEvent>) -> Component,
    props: Props,
) -> CanonicalTranscript
where
    Props: Clone + Send + 'static,
{
    let mut components = ComponentHost::new(root, props);
    components
        .render()
        .expect("Component renders a complete projection")
        .projection()
        .to_transcript()
        .expect("node-ordered projection lowers to a canonical transcript")
}

fn rendered(item: &CanonicalInputItem) -> String {
    let pom = match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            pom
        }
        other => panic!("expected POM item, got {other:?}"),
    };
    render_pom_document(pom).unwrap()
}

#[component]
fn stable_system() -> Component {
    view! {
        #[system_once]
        test_protocol { "Stable test protocol." }
    }
}

#[derive(AgentView)]
#[agent_view(kind = "dynamic_note")]
struct DynamicNote {
    #[view(text)]
    body: String,
}

#[component]
fn oracle_application(_props: TurnProps, events: EventInput<ProviderEvent>) -> Component {
    let _same_route = events.clone();
    view! {
        #[system_once]
        chess_rules { "Play legal chess." }

        #[developer]
        chess_reply_protocol { "Return exactly one move." }

        chess_task {
            turn_id: "turn-7",
            "Choose a move & explain."
        }
    }
}

#[tokio::test]
async fn component_host_lowers_system_developer_and_default_user_to_canonical_items() {
    let input = render_application(oracle_application, TurnProps);

    assert_eq!(input.items().len(), 3);
    assert!(matches!(
        &input.items()[0],
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    ));
    assert!(matches!(
        &input.items()[1],
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            ..
        }
    ));
    assert!(matches!(
        &input.items()[2],
        CanonicalInputItem::Message {
            role: ConversationRole::User,
            ..
        }
    ));
    assert_eq!(
        rendered(&input.items()[0]),
        "<chess_rules>Play legal chess.</chess_rules>"
    );
    assert_eq!(
        rendered(&input.items()[1]),
        "<chess_reply_protocol>Return exactly one move.</chess_reply_protocol>"
    );
    assert_eq!(
        rendered(&input.items()[2]),
        "<chess_task turn_id=\"turn-7\">Choose a move &amp; explain.</chess_task>"
    );
}

fn mixed_protocol() -> Component {
    view! {
        #[user]
        user_default { "user" }

        #[developer]
        developer_default { "developer" }
    }
}

#[component]
fn outer_override_application(_props: TurnProps, _events: EventInput<ProviderEvent>) -> Component {
    view! {
        #[system_once]
        mixed_protocol()

        turn_marker { "ready" }
    }
}

#[tokio::test]
async fn outer_system_placement_overrides_descendants_and_seals_one_instruction() {
    let input = render_application(outer_override_application, TurnProps);

    assert_eq!(input.items().len(), 2);
    assert!(matches!(
        &input.items()[0],
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    ));
    assert_eq!(
        rendered(&input.items()[0]),
        "<user_default>user</user_default>\n\n<developer_default>developer</developer_default>"
    );
}

#[component]
fn escaping_application(_props: TurnProps, _events: EventInput<ProviderEvent>) -> Component {
    view! {
        stable_system()

        "root * < &"

        escaped {
            data: "\"&<>",
            nested { "<&>" }
        }
    }
}

#[tokio::test]
async fn static_xml_text_and_attributes_are_escaped_by_the_canonical_renderer() {
    let input = render_application(escaping_application, TurnProps);

    assert_eq!(input.items().len(), 2);
    assert_eq!(
        rendered(&input.items()[1]),
        concat!(
            "root \\* \\< \\&\n\n",
            "<escaped data=\"&quot;&amp;&lt;&gt;\">\n",
            "  <nested>&lt;&amp;&gt;</nested>\n",
            "</escaped>"
        )
    );
}

static ATTRIBUTE_EVALUATIONS: AtomicUsize = AtomicUsize::new(0);

#[component]
fn dynamic_pom_application(_props: TurnProps, _events: EventInput<ProviderEvent>) -> Component {
    let instruction = "Choose e2e4 & hold <center>";
    let turn_id = String::from("turn-7");
    let seconds = 4.25_f32;

    view! {
        stable_system()

        "Turn {turn_id}: {instruction}"
        "Literal {{slot}} for {turn_id}"

        md::paragraph {
            "Clock: "
            md::code_span { "{seconds:.1}s" }
        }

        chess_task {
            priority: {
                ATTRIBUTE_EVALUATIONS.fetch_add(1, Ordering::SeqCst);
                3_u8
            },
            label: "clock {seconds:.1}s",
            instruction { "{instruction}" }
            clock { "{seconds:.1}s" }
        }
    }
}

#[tokio::test]
async fn formatted_text_and_dynamic_attributes_materialize_as_escaped_pom() {
    ATTRIBUTE_EVALUATIONS.store(0, Ordering::SeqCst);
    let input = render_application(dynamic_pom_application, TurnProps);

    assert_eq!(ATTRIBUTE_EVALUATIONS.load(Ordering::SeqCst), 1);
    assert_eq!(input.items().len(), 2);
    assert_eq!(
        rendered(&input.items()[1]),
        concat!(
            "Turn turn-7: Choose e2e4 \\& hold \\<center\\>\n\n",
            "Literal {slot} for turn-7\n\n",
            "Clock: `4.2s`\n\n",
            "<chess_task priority=\"3\" label=\"clock 4.2s\">\n",
            "  <instruction>Choose e2e4 &amp; hold &lt;center&gt;</instruction>\n",
            "  <clock>4.2s</clock>\n",
            "</chess_task>"
        )
    );
}

fn dynamic_root_inline_component() -> Component {
    view! { inline { "component" } }
}

#[component]
fn dynamic_root_application(_props: TurnProps, _events: EventInput<ProviderEvent>) -> Component {
    let typed = DynamicNote {
        body: String::from("typed & structured"),
    };
    let optional = Some(DynamicNote {
        body: String::from("optional"),
    });
    let absent: Option<DynamicNote> = None;

    view! {
        stable_system()
        {dynamic_root_inline_component()}
        {typed}
        {optional}
        {absent}
    }
}

#[tokio::test]
async fn dynamic_root_expressions_preserve_component_and_pom_structure() {
    let input = render_application(dynamic_root_application, TurnProps);

    assert_eq!(input.items().len(), 2);
    assert_eq!(
        rendered(&input.items()[1]),
        concat!(
            "<inline>component</inline>\n\n",
            "<dynamic_note>typed &amp; structured</dynamic_note>\n\n",
            "<dynamic_note>optional</dynamic_note>"
        )
    );
}

fn selected_routes_prompt(_text: EventInput<TextTurnEvent>) -> Component {
    view! { selected_routes { "ready" } }
}

#[component]
fn event_select_application(_props: TurnProps, events: EventInput<ProviderEvent>) -> Component {
    let text = events.select(ProviderEvent::TEXT);

    view! {
        stable_system()
        selected_routes_prompt(text)
    }
}

#[tokio::test]
async fn provider_event_selects_typed_child_routes_without_prompt_output() {
    let input = render_application(event_select_application, TurnProps);

    assert_eq!(input.items().len(), 2);
    assert_eq!(
        rendered(&input.items()[1]),
        "<selected_routes>ready</selected_routes>"
    );
}

#[derive(Clone)]
struct ReducerRenderState {
    selected_move: String,
    diagnostic: Option<String>,
}

#[component]
fn signal_read_application(_props: TurnProps, _events: EventInput<ProviderEvent>) -> Component {
    let move_state = use_signal(|| ReducerRenderState {
        selected_move: String::from("e2e4"),
        diagnostic: None,
    });
    let (selected_move, diagnostic) = move_state
        .with(|state| (state.selected_move.clone(), state.diagnostic.clone()))
        .unwrap();
    let diagnostic = diagnostic.unwrap_or_else(|| String::from("none"));

    view! {
        stable_system()

        reducer_state {
            selected_move: selected_move,
            diagnostic: diagnostic,
            "ready"
        }
    }
}

#[tokio::test]
async fn signal_read_projects_owned_values_into_canonical_prompt_output() {
    let input = render_application(signal_read_application, TurnProps);

    assert_eq!(input.items().len(), 2);
    assert_eq!(
        rendered(&input.items()[1]),
        "<reducer_state selected_move=\"e2e4\" diagnostic=\"none\">ready</reducer_state>"
    );
}
