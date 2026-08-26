use agentview::component::{
    execution::{ProviderEvent, RenderedProjectionDiffMarker},
    prelude::{component, view, Component, EventInput},
    ComponentHost,
};
use agentview::{pom_renderer::render_pom_document, transcript::CanonicalInputItem};

#[derive(Clone, Copy)]
struct ProjectionProps;

#[derive(Clone)]
struct DiffProjectionProps {
    value: String,
}

#[component]
fn empty_feature() -> Component {
    view! {}
}

#[component]
fn history_feature() -> Component {
    view! {
        #[developer]
        history_policy { "policy" }

        history_turn { "turn-1" }
    }
}

#[component]
fn outcome_feature() -> Component {
    view! {
        outcome { "pending" }
    }
}

#[component]
fn interleaved_child() -> Component {
    view! {
        child_item { "child" }
    }
}

#[component]
fn interleaved_parent() -> Component {
    view! {
        before_item { "before" }
        interleaved_child()
        after_item { "after" }
    }
}

#[component]
fn shared_system_child() -> Component {
    view! {
        shared_policy { "shared" }
    }
}

#[component]
fn separate_system_siblings_application(
    _props: ProjectionProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    view! {
        #[system_once]
        shared_system_child()

        #[system_once]
        shared_system_child()
    }
}

#[component]
fn ordinary_and_system_siblings_application(
    _props: ProjectionProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    view! {
        shared_system_child()

        #[system_once]
        shared_system_child()

        #[developer]
        empty_feature()
    }
}

#[component]
fn projection_application(
    _props: ProjectionProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    view! {
        empty_feature()
        history_feature()
        outcome_feature()
    }
}

#[component]
fn interleaved_application(
    _props: ProjectionProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    view! {
        interleaved_parent()
    }
}

#[component]
fn diff_projection_application(
    props: DiffProjectionProps,
    _events: EventInput<ProviderEvent>,
) -> Component {
    let value = props.value;
    view! {
        before_diff { "before" }

        #[diff(slot = "state")]
        current_state { "{value}" }

        after_diff { "after" }
    }
}

fn rendered(item: &CanonicalInputItem) -> String {
    let pom = match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            pom
        }
        other => panic!("expected POM projection item, got {other:?}"),
    };
    render_pom_document(pom).unwrap()
}

#[test]
fn render_preserves_ordered_component_nodes_and_their_item_vectors() {
    let mut host = ComponentHost::new(projection_application, ProjectionProps);
    let projection = host.render().unwrap().projection().clone();
    let nodes = projection.nodes();

    assert_eq!(
        nodes.len(),
        5,
        "synthetic root plus four mounted Components"
    );
    assert_eq!(nodes[0].identity(), "root");
    assert!(nodes[1].identity().contains("projection_application"));
    assert!(nodes[2].identity().contains("empty_feature"));
    assert!(nodes[3].identity().contains("history_feature"));
    assert!(nodes[4].identity().contains("outcome_feature"));
    assert!(nodes[2].identity().ends_with("#0"));
    assert!(nodes[3].identity().ends_with("#1"));
    assert!(nodes[4].identity().ends_with("#2"));

    assert!(nodes[0].items().is_empty());
    assert!(nodes[1].items().is_empty());
    assert!(nodes[2].items().is_empty());
    assert_eq!(nodes[3].items().len(), 2);
    assert_eq!(nodes[4].items().len(), 1);

    let lowered = nodes
        .iter()
        .flat_map(|node| node.items().iter().cloned())
        .collect::<Vec<_>>();
    assert_eq!(projection.to_transcript().unwrap().items(), lowered);
}

#[test]
fn render_treats_components_as_ordered_ownership_units() {
    let mut host = ComponentHost::new(interleaved_application, ProjectionProps);
    let projection = host.render().unwrap().projection().clone();
    let parent = projection
        .nodes()
        .iter()
        .find(|node| node.identity().contains("interleaved_parent"))
        .unwrap();
    let child = projection
        .nodes()
        .iter()
        .find(|node| node.identity().contains("interleaved_child"))
        .unwrap();

    assert_eq!(parent.items().len(), 2);
    assert!(rendered(&parent.items()[0]).contains("<before_item>before</before_item>"));
    assert!(rendered(&parent.items()[1]).contains("<after_item>after</after_item>"));
    assert_eq!(child.items().len(), 1);
    assert!(rendered(&child.items()[0]).contains("<child_item>child</child_item>"));

    let lowered = projection
        .nodes()
        .iter()
        .flat_map(|node| {
            node.items()
                .iter()
                .map(|item| (node.identity().to_owned(), rendered(item)))
        })
        .collect::<Vec<_>>();
    assert_eq!(lowered.len(), 3);
    assert!(lowered[0].0.contains("interleaved_parent"));
    assert!(lowered[0].1.contains("<before_item>before</before_item>"));
    assert!(lowered[1].0.contains("interleaved_parent"));
    assert!(lowered[1].1.contains("<after_item>after</after_item>"));
    assert!(lowered[2].0.contains("interleaved_child"));
    assert!(lowered[2].1.contains("<child_item>child</child_item>"));

    let transcript = projection
        .to_transcript()
        .expect("node-ordered projection forms a canonical transcript");
    assert_eq!(
        transcript.items(),
        projection
            .nodes()
            .iter()
            .flat_map(|node| node.items().iter().cloned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn separate_system_once_siblings_have_distinct_child_component_identities() {
    let mut host = ComponentHost::new(separate_system_siblings_application, ProjectionProps);
    let projection = host.render().unwrap().projection().clone();
    let child_identities = projection
        .nodes()
        .iter()
        .filter(|node| node.identity().contains("shared_system_child"))
        .map(|node| node.identity().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(child_identities.len(), 2);
    assert_ne!(child_identities[0], child_identities[1]);
}

#[test]
fn ordinary_and_system_mounts_of_same_child_have_disjoint_identities() {
    let mut host = ComponentHost::new(ordinary_and_system_siblings_application, ProjectionProps);
    let projection = host.render().unwrap().projection().clone();
    let child_identities = projection
        .nodes()
        .iter()
        .filter_map(|node| {
            node.identity()
                .contains("shared_system_child")
                .then_some(node.identity())
        })
        .collect::<Vec<_>>();

    assert_eq!(child_identities.len(), 2);
    assert!(child_identities
        .iter()
        .any(|identity| identity.ends_with("#0")));
    let system_identity = child_identities
        .iter()
        .find(|identity| identity.ends_with("#system:0"))
        .expect("the System child uses the disjoint traversal namespace");
    assert!(!system_identity.contains("agentview::system_once:"));

    let developer_identity = projection
        .nodes()
        .iter()
        .find(|node| node.identity().contains("empty_feature"))
        .expect("the Developer child remains mounted")
        .identity();
    assert!(developer_identity.ends_with("#1"));
}

#[test]
fn diff_root_retains_stable_item_provenance_and_complete_current_pom() {
    let mut host = ComponentHost::new(
        diff_projection_application,
        DiffProjectionProps {
            value: "A".to_owned(),
        },
    );

    let first = host.render().unwrap().projection().clone();
    let first_node = first
        .nodes()
        .iter()
        .find(|node| node.identity().contains("diff_projection_application"))
        .unwrap();
    assert_eq!(first_node.items().len(), 1);
    assert_eq!(first_node.diffs().len(), 1);
    let first_diff: &RenderedProjectionDiffMarker = &first_node.diffs()[0];
    assert_eq!(first_diff.item_index(), 0);
    assert_eq!(first_diff.slot(), "state");
    let first_prompt = rendered(&first_node.items()[first_diff.item_index()]);
    assert!(first_prompt.contains("<before_diff>before</before_diff>"));
    assert!(first_prompt.contains("<current_state>A</current_state>"));
    assert!(first_prompt.contains("<after_diff>after</after_diff>"));

    host.set_props(DiffProjectionProps {
        value: "A+".to_owned(),
    });
    let second = host.render().unwrap().projection().clone();
    let second_node = second
        .nodes()
        .iter()
        .find(|node| node.identity().contains("diff_projection_application"))
        .unwrap();
    assert_eq!(second_node.diffs(), first_node.diffs());
    let second_prompt = rendered(&second_node.items()[second_node.diffs()[0].item_index()]);
    assert!(second_prompt.contains("<before_diff>before</before_diff>"));
    assert!(second_prompt.contains("<current_state>A+</current_state>"));
    assert!(second_prompt.contains("<after_diff>after</after_diff>"));
}
