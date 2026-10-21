#[path = "support/pom.rs"]
mod pom_support;
use pom_support::*;

use std::{
    ops::ControlFlow,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{Application, DebugProviderPort, FrameBasis},
        prelude::*,
        ComponentHost,
    },
    transcript::{CanonicalInputItem, ConversationRole, InstructionAuthority},
};

#[derive(Clone)]
struct DeveloperHistoryProps {
    entries: Arc<Mutex<Vec<String>>>,
}

#[derive(AgentView, Clone)]
#[agent_view(kind = "developer_history")]
struct DeveloperHistoryView {
    // This stable sibling makes the root eligible for a structured delta.
    #[view(element)]
    format: &'static str,
    #[view(diff(seq))]
    entries: Vec<String>,
}

#[component]
fn developer_history(props: DeveloperHistoryProps) -> Component {
    let snapshot = DeveloperHistoryView {
        format: "v1",
        entries: props.entries.lock().unwrap().clone(),
    };
    view! {
        #[developer]
        #[diff(slot = "history")]
        { snapshot }
    }
}

#[component]
fn user_diff(_props: ()) -> Component {
    view! {
        #[diff(slot = "state")]
        user_state { "current" }
    }
}

#[component]
fn system_once_diff(_props: ()) -> Component {
    view! {
        #[system_once]
        #[diff(slot = "state")]
        system_state { "stable" }
    }
}

#[test]
fn developer_diff_preserves_user_and_system_once_rules() {
    let mut user_host = ComponentHost::new_root(user_diff, ());
    let user_projection = user_host.render().unwrap().projection().clone();
    let user_node = user_projection
        .nodes()
        .iter()
        .find(|node| node.identity().contains("user_diff"))
        .expect("user diff root is mounted");
    assert_eq!(user_node.diffs().len(), 1);
    assert!(matches!(
        user_node.items(),
        [CanonicalInputItem::Message {
            role: ConversationRole::User,
            ..
        }]
    ));

    let mut system_host = ComponentHost::new_root(system_once_diff, ());
    let system_projection = system_host.render().unwrap().projection().clone();
    assert!(system_projection
        .nodes()
        .iter()
        .all(|node| node.diffs().is_empty()));
    assert!(matches!(
        system_projection.to_transcript().unwrap().items(),
        [CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }]
    ));
}

#[tokio::test]
async fn retained_application_emits_developer_sequence_delta_and_suppresses_no_change() {
    let entries = Arc::new(Mutex::new(vec![String::from("first")]));
    let root_entries = Arc::clone(&entries);
    let (port, capture) = DebugProviderPort::new();
    let mut app = Application::mount(
        move || {
            developer_history(DeveloperHistoryProps {
                entries: Arc::clone(&root_entries),
            })
        },
        port,
    )
    .unwrap();

    assert_eq!(app.react().await.unwrap(), ControlFlow::Continue(()));
    entries.lock().unwrap().push(String::from("second"));
    assert_eq!(app.react().await.unwrap(), ControlFlow::Continue(()));
    assert_eq!(app.react().await.unwrap(), ControlFlow::Continue(()));

    let frames = capture.frame_snapshots();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].basis(), FrameBasis::Full);
    assert!(matches!(frames[1].basis(), FrameBasis::DeltaFrom(_)));
    assert!(matches!(frames[2].basis(), FrameBasis::DeltaFrom(_)));

    let first_items = frame_projection_items(frames[0].canonical_payload());
    assert!(matches!(
        &first_items[..],
        [CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            ..
        }]
    ));
    let first = combine(item_documents(&first_items));
    assert!(first.has_field("item", "first"));

    let delta_items = frame_projection_items(frames[1].canonical_payload());
    assert!(matches!(
        &delta_items[..],
        [CanonicalInputItem::Instruction {
            authority: InstructionAuthority::Developer,
            ..
        }]
    ));
    let delta = combine(item_documents(&delta_items));
    assert!(delta.has_attribute("rendering_mode", "delta"));
    assert!(delta.has_element("insert"));
    assert!(delta.has_field("item", "second"));
    assert!(!delta.has_text("first"));
    assert!(frame_projection_items(frames[2].canonical_payload()).is_empty());
    app.shutdown().await.unwrap();
}
