//! Isolated P3 lifecycle proof: System mounts once and User renders per attempt.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use agentview::{
    component::{
        advanced::lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
        system_view, user_view, UserTurnContext, UserView,
    },
    prelude::*,
};

struct PlayerChannels;

impl TurnChannels for PlayerChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[derive(Debug)]
struct SelectionRuntime;

impl BindingInstance<PlayerChannels> for SelectionRuntime {}

struct PlayerConfig {
    system_renders: Arc<AtomicUsize>,
}

struct PlayerTurn {
    user_renders: Arc<AtomicUsize>,
    context: String,
    artifacts: Vec<String>,
    task: String,
}

fn xml_document(name: &str) -> Document {
    Document::from_xml(XmlNode::new(
        XmlName::try_from(name).expect("static example tag is valid"),
    ))
}

fn text_document(name: &str, value: &str) -> Document {
    let mut node = XmlNode::new(XmlName::try_from(name).expect("static example tag is valid"));
    node.push(MixedContent::text(TextNode::new(value)));
    Document::from_xml(node)
}

#[agentview::view(component)]
fn player_policy() -> PomView {
    view(xml_document("player_policy"))
}

fn player_system(
    cx: SystemMountContext<'_, PlayerConfig>,
) -> SystemView<PlayerChannels, PlayerTurn> {
    // Example-only lifecycle probe; production System renderers stay pure.
    cx.props().system_renders.fetch_add(1, Ordering::SeqCst);
    system_view(component((
        player_policy(),
        binding_factory(
            "selection",
            xml_document("select_intent"),
            RuntimeRoute::xml("select_intent").expect("static route is valid"),
            || SelectionRuntime,
        ),
    )))
}

fn player_user(cx: UserTurnContext<'_, PlayerTurn>) -> UserView {
    // Example-only lifecycle probe; production User renderers stay pure.
    cx.props().user_renders.fetch_add(1, Ordering::SeqCst);
    let artifacts = cx
        .props()
        .artifacts
        .iter()
        .map(|artifact| text_document("artifact", artifact))
        .collect::<Vec<_>>();
    user_view(view((
        text_document("agent_context", &cx.props().context),
        artifacts,
        text_document("task", &cx.props().task),
    )))
}

fn main() -> anyhow::Result<()> {
    let system_renders = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &PlayerConfig {
            system_renders: Arc::clone(&system_renders),
        },
        player_system,
    )?;

    println!("SYSTEM\n{}", epoch.rendered_system());
    println!("factories={}", epoch.binding_factories().len());

    let user_renders = Arc::new(AtomicUsize::new(0));
    let mut user_cursor = UserDocumentCursor::default();
    for (context, artifacts, task) in [
        (
            "The player is in the clock tower.",
            vec!["The north door is locked."],
            "inspect-square",
        ),
        (
            "The player reached the archive.",
            vec!["A brass key was collected.", "The clock stopped."],
            "continue-game",
        ),
    ] {
        let plan = epoch.prepare_user(
            &PlayerTurn {
                user_renders: Arc::clone(&user_renders),
                context: context.to_owned(),
                artifacts: artifacts.into_iter().map(str::to_owned).collect(),
                task: task.to_owned(),
            },
            player_user,
        )?;
        let (resolved, next_cursor) = resolve_user_document(plan.into_document(), &user_cursor)?;
        user_cursor = next_cursor;
        println!("\nUSER\n{}", render_pom_document(&resolved)?);
    }

    println!(
        "\nRENDER COUNTS system={} user={}",
        system_renders.load(Ordering::SeqCst),
        user_renders.load(Ordering::SeqCst)
    );
    Ok(())
}
