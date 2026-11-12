use std::convert::Infallible;

use crate::{
    component::{
        execution::{ProjectionDiffState, RenderedProjection},
        prelude::*,
        ComponentHost,
    },
    transcript::{CanonicalInputItem, InstructionAuthority},
};

#[component]
fn dynamic_leaf() -> Component {
    view! { "Dynamic leaf content." }
}

#[component]
fn provenance_child() -> Component {
    view! {
        #[system_once]
        "Child policy."

        "Child content."

        NativeToolCall {
            name: "ping",
            on_call: |_context| Ok::<_, Infallible>("pong"),
        }

        {
            Decision::new("continue", Noul::new("May the displayed task continue?"))
                .on_result(|_context, _outcome| Ok::<_, Infallible>(()))
                .build()
        }
    }
}

#[component]
fn provenance_application() -> Component {
    let dynamic = ComponentDefinition::new("dynamic/type", "version/1", |_hooks| {
        Ok(view! {
            #[system_once]
            "Dynamic policy."

            dynamic_leaf()
        })
    })
    .unwrap();
    view! {
        #[system_once]
        "First root policy."

        provenance_child()

        #[system_once]
        "Second root policy."

        { dynamic.instantiate("instance/with/slashes") }
    }
}

fn projection() -> RenderedProjection {
    ComponentHost::new_root(|()| provenance_application(), ())
        .render()
        .unwrap()
        .projection()
        .clone()
}

#[test]
fn projection_provenance_preserves_actual_parents_and_flattened_declaration_owners() {
    let projection = projection();
    let nodes = projection.nodes();
    assert_eq!(nodes.len(), 7);
    let [root, application, child, tool, decision, dynamic, leaf] = nodes else {
        unreachable!();
    };
    let sources = projection.provenance();
    assert_eq!(sources.parents.len(), nodes.len());
    assert_eq!(sources.parents[root.identity()], None);
    for (node, parent) in [
        (application, root),
        (child, application),
        (tool, child),
        (decision, child),
        (dynamic, application),
        (leaf, dynamic),
    ] {
        assert_eq!(
            sources.parents[node.identity()].as_deref(),
            Some(parent.identity()),
        );
    }
    assert_eq!(
        sources.system_sources,
        [
            application.identity(),
            child.identity(),
            application.identity(),
            dynamic.identity(),
        ],
    );
    let [CanonicalInputItem::Instruction {
        authority: InstructionAuthority::System,
        pom,
    }] = root.items()
    else {
        panic!("System declarations remain one normalized root instruction");
    };
    assert_eq!(pom.children().len(), sources.system_sources.len());
    assert_eq!(sources.native_tool_owners["ping"], tool.identity());
    assert_eq!(sources.decision_owners["continue"], decision.identity());
}

#[test]
fn projection_provenance_survives_delivery_lowering_without_changing_typed_items() {
    let projection = projection();
    let submitted = ProjectionDiffState::prepare(None, &projection).submission;
    assert_eq!(submitted.provenance(), projection.provenance());
    assert_eq!(submitted.to_transcript(), projection.to_transcript());
    assert_eq!(submitted.native_tools(), projection.native_tools());
    assert_eq!(submitted.decision(), projection.decision());
}
