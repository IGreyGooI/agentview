#[path = "support/pom.rs"]
mod pom_support;

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};

use agentview::component::{
    execution::{Application, DebugProviderPort, FrameBasis, ReactionKind},
    prelude::*,
};
use pom_support::{projection_document, InspectPom};

#[component]
fn prepared_view() -> Component {
    let state = use_signal(|| "not prepared".to_owned());
    let ready = state.clone();
    use_preparation(move || async move {
        let _ = ready.set("prepared 你好🙂".to_owned());
        Ok::<_, Infallible>(())
    });
    let value = state.with(Clone::clone).unwrap();
    view! { state { "{value}" } }
}

#[tokio::test]
async fn observer_uses_normal_preparation_and_pairs_main_temporary_and_delta_frames() {
    let (port, frames) = DebugProviderPort::new();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let mut app = Application::mount(prepared_view, port)
        .unwrap()
        .with_frame_observer(move |event| {
            assert!(projection_document(event.projection()).has_field("state", "prepared 你好🙂"));
            let prompt = event.projection().render_prompt_with_sources().unwrap();
            let profile = prompt.profile_bytes();
            sink.lock().unwrap().push((
                event.application_id(),
                event.reaction_id(),
                event.reaction_kind(),
                event.projection_revision(),
                event.frame().revision(),
                event.frame().basis(),
                profile.total_bytes(),
            ));
        });
    let initial_revision = app.current_projection().revision();
    assert!(app.prepare().await.unwrap().is_continue());
    assert!(observed.lock().unwrap().is_empty());
    let main = app.react(false).unwrap();
    let main_id = main.id();
    main.await.unwrap();
    let temporary = app.react(true).unwrap();
    let temporary_id = temporary.id();
    temporary.await.unwrap();
    app.react(false).unwrap().await.unwrap();
    let captured = frames.frame_snapshots();
    {
        let events = observed.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].1, main_id);
        assert_eq!(events[1].1, temporary_id);
        assert_eq!(events[0].2, ReactionKind::Main);
        assert_eq!(events[1].2, ReactionKind::Temporary);
        assert_eq!(events[2].2, ReactionKind::Main);
        assert!(events[0].3 > initial_revision);
        for (event, frame) in events.iter().zip(&captured) {
            assert_eq!(event.0, app.application_id());
            assert_eq!(event.4, frame.revision());
            assert_eq!(event.5, frame.basis());
            assert!(event.6 > 0);
        }
        assert_eq!(events[2].5, FrameBasis::DeltaFrom(events[0].4));
    }
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn observer_panic_leaves_main_continuity_and_future_reactions_usable() {
    let (port, frames) = DebugProviderPort::new();
    let mut app = Application::mount(|| view! { state { "still usable" } }, port)
        .unwrap()
        .with_frame_observer(|_| panic!("diagnostic sink failure"));
    for _ in 0..2 {
        assert!(app.react(false).unwrap().await.unwrap().is_continue());
    }
    let frames = frames.frame_snapshots();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[1].basis(),
        FrameBasis::DeltaFrom(frames[0].revision())
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn independent_fork_registers_its_own_observer_and_skips_bootstrap_frames() {
    let (port, _) = DebugProviderPort::new();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let mut parent = Application::mount(|| view! { "parent" }, port)
        .unwrap()
        .with_frame_observer(move |event| sink.lock().unwrap().push(event.application_id()));
    let mut child = parent
        .fork_to(|| view! { "child" }, DebugProviderPort::default())
        .await
        .unwrap();
    assert!(child.prepare().await.unwrap().is_continue());
    child.react(false).unwrap().await.unwrap();
    assert!(observed.lock().unwrap().is_empty());
    let sink = observed.clone();
    child =
        child.with_frame_observer(move |event| sink.lock().unwrap().push(event.application_id()));
    child.react(false).unwrap().await.unwrap();
    parent.react(false).unwrap().await.unwrap();
    assert_eq!(
        *observed.lock().unwrap(),
        [child.application_id(), parent.application_id()]
    );
    child.shutdown().await.unwrap();
    parent.shutdown().await.unwrap();
}
