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

    let first = String::from_utf8(frames[0].canonical_payload().to_vec()).unwrap();
    assert!(first.contains(r#""authority":"developer""#));
    assert!(first.contains("first"));

    let delta = String::from_utf8(frames[1].canonical_payload().to_vec()).unwrap();
    assert!(delta.contains(r#""authority":"developer""#));
    assert!(delta.contains("rendering_mode"));
    assert!(delta.contains("insert"));
    assert!(delta.contains("second"));
    assert!(!delta.contains("first"));

    let unchanged = String::from_utf8(frames[2].canonical_payload().to_vec()).unwrap();
    assert!(unchanged.contains(r#""items":[]"#));
    app.shutdown().await.unwrap();
}
