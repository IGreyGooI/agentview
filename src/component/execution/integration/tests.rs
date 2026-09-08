use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agentview_derive::{component, view};
use async_trait::async_trait;
use futures::FutureExt;

use super::{PluginControl, PluginPort, SkillControl, SkillPort};
use crate::{
    component::{
        execution::{
            RenderedProjection,
            application::Application,
            debug::DebugProviderPort,
            external::{
                ExternalAct, ExternalControlFault, ExternalIngressGeneration, ExternalObservation,
                ExternalObservationKind,
            },
            reaction::{FrameBasis, ReactionPort},
        },
        prelude::*,
    },
    transcript::CanonicalInputItem,
};

#[component]
fn shared_driver_root() -> Component {
    view! { shared_state { "same canonical state" } }
}

async fn drive_skill(
    application: &mut Application<SkillPort>,
    control: &SkillControl,
) -> ExternalObservation {
    let reaction = application.react();
    tokio::pin!(reaction);
    let observation = tokio::select! {
        observation = control.next_observation() => observation.unwrap(),
        result = &mut reaction => panic!("Skill reaction ended before handoff: {result:?}"),
    };
    control
        .complete(observation.ingress_generation())
        .await
        .unwrap();
    reaction.await.unwrap();
    observation
}

async fn drive_plugin(
    application: &mut Application<PluginPort>,
    control: &PluginControl,
    act: Option<ExternalAct>,
) -> ExternalObservation {
    let reaction = application.react();
    tokio::pin!(reaction);
    let observation = tokio::select! {
        observation = control.next_observation() => observation.unwrap(),
        result = &mut reaction => panic!("Plugin reaction ended before handoff: {result:?}"),
    };
    match act {
        Some(act) => control
            .act(observation.ingress_generation(), act)
            .await
            .unwrap(),
        None => control
            .complete(observation.ingress_generation())
            .await
            .unwrap(),
    }
    reaction.await.unwrap();
    observation
}

fn completed_text(text: &str) -> ExternalAct {
    ExternalAct::text(text)
}

#[async_trait]
trait QueuedRoleControl {
    async fn next_observation(&self) -> Result<ExternalObservation, ExternalControlFault>;

    async fn complete(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<(), ExternalControlFault>;
}

#[async_trait]
impl QueuedRoleControl for SkillControl {
    async fn next_observation(&self) -> Result<ExternalObservation, ExternalControlFault> {
        SkillControl::next_observation(self).await
    }

    async fn complete(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<(), ExternalControlFault> {
        SkillControl::complete(self, generation).await
    }
}

#[async_trait]
impl QueuedRoleControl for PluginControl {
    async fn next_observation(&self) -> Result<ExternalObservation, ExternalControlFault> {
        PluginControl::next_observation(self).await
    }

    async fn complete(
        &self,
        generation: ExternalIngressGeneration,
    ) -> Result<(), ExternalControlFault> {
        PluginControl::complete(self, generation).await
    }
}

async fn assert_unfinished_stream_drop_recovers_with_full<P, C>(
    mut application: Application<P>,
    control: C,
) where
    P: ReactionPort,
    C: QueuedRoleControl,
{
    let mut cancelled = Box::pin(application.react());
    let first = tokio::select! {
        observation = control.next_observation() => observation.unwrap(),
        result = &mut cancelled => panic!("reaction ended before first handoff: {result:?}"),
    };
    drop(cancelled);

    assert!(matches!(
        control.complete(first.ingress_generation()).await,
        Err(ExternalControlFault::StaleIngress)
    ));

    let mut recovery = Box::pin(application.react());
    let recovered = tokio::select! {
        observation = control.next_observation() => observation.unwrap(),
        result = &mut recovery => panic!("reaction ended before recovery handoff: {result:?}"),
    };
    assert_eq!(recovered.kind(), ExternalObservationKind::Full);
    assert!(recovered.frame().epoch() > first.frame().epoch());

    control
        .complete(recovered.ingress_generation())
        .await
        .unwrap();
    recovery.await.unwrap();
    application.shutdown().await.unwrap();
}

#[tokio::test]
async fn skill_and_plugin_preserve_unfinished_stream_drop_recovery() {
    let (skill_port, skill_control) = SkillPort::new().unwrap();
    let skill = Application::mount(shared_driver_root, skill_port).unwrap();
    assert_unfinished_stream_drop_recovers_with_full(skill, skill_control).await;

    let (plugin_port, plugin_control) = PluginPort::new().unwrap();
    let plugin = Application::mount(shared_driver_root, plugin_port).unwrap();
    assert_unfinished_stream_drop_recovers_with_full(plugin, plugin_control).await;
}

#[tokio::test]
async fn agent_skill_and_plugin_share_the_exact_canonical_frame_compiler() {
    let (debug, capture) = DebugProviderPort::new();
    let mut agent = Application::mount(shared_driver_root, debug).unwrap();
    agent.react().await.unwrap();
    let agent_frame = capture.latest_frame().unwrap();

    let (skill_port, skill_control) = SkillPort::new().unwrap();
    let mut skill = Application::mount(shared_driver_root, skill_port).unwrap();
    let skill_frame = drive_skill(&mut skill, &skill_control).await;

    let (plugin_port, plugin_control) = PluginPort::new().unwrap();
    let mut plugin = Application::mount(shared_driver_root, plugin_port).unwrap();
    let plugin_frame = drive_plugin(&mut plugin, &plugin_control, None).await;

    assert_eq!(agent_frame.basis(), FrameBasis::Full);
    assert_eq!(
        skill_frame.kind(),
        super::super::external::ExternalObservationKind::Full
    );
    assert_eq!(
        plugin_frame.kind(),
        super::super::external::ExternalObservationKind::Full
    );
    assert_eq!(
        agent_frame.canonical_payload(),
        skill_frame.frame().submission().canonical_bytes()
    );
    assert_eq!(
        agent_frame.canonical_payload(),
        plugin_frame.frame().submission().canonical_bytes()
    );

    agent.shutdown().await.unwrap();
    skill.shutdown().await.unwrap();
    plugin.shutdown().await.unwrap();
}

#[derive(Clone)]
struct SkillFrontendProps {
    exported: Arc<Mutex<Option<Signal<String>>>>,
}

#[component]
fn skill_frontend(props: SkillFrontendProps) -> Component {
    let state = use_signal(|| String::from("initial"));
    *props.exported.lock().unwrap() = Some(state.clone());
    let value = state.with(Clone::clone).expect("mounted Skill state");
    view! { skill_state { "{value}" } }
}

fn run_typed_skill_subcommand(signal: &Signal<String>, value: &str) {
    signal.set(value.to_owned()).unwrap();
}

#[tokio::test]
async fn skill_latest_and_typed_subcommand_do_not_render_or_submit() {
    let exported = Arc::new(Mutex::new(None));
    let root_export = Arc::clone(&exported);
    let (port, control) = SkillPort::new().unwrap();
    let mut application = Application::mount(
        move || {
            skill_frontend(SkillFrontendProps {
                exported: Arc::clone(&root_export),
            })
        },
        port,
    )
    .unwrap();
    let signal = exported.lock().unwrap().clone().unwrap();

    let initial = application.current_projection();
    let revision = initial.revision();
    assert!(!initial.is_dirty());
    assert!(rendered_text(initial.projection()).contains("initial"));
    assert!(control.next_observation().now_or_never().is_none());

    run_typed_skill_subcommand(&signal, "updated");

    let stale = application.current_projection();
    assert_eq!(stale.revision(), revision);
    assert!(stale.is_dirty());
    assert!(rendered_text(stale.projection()).contains("initial"));
    assert!(!rendered_text(stale.projection()).contains("updated"));
    assert!(control.next_observation().now_or_never().is_none());

    let observation = drive_skill(&mut application, &control).await;
    assert!(observation.content().contains("updated"));
    assert!(!application.current_projection().is_dirty());
    application.shutdown().await.unwrap();
}

struct PluginParent {
    application: Application<PluginPort>,
    control: PluginControl,
}

fn plugin_parent() -> PluginParent {
    let (port, control) = PluginPort::new().unwrap();
    PluginParent {
        application: Application::mount(shared_driver_root, port).unwrap(),
        control,
    }
}

#[tokio::test]
async fn plugin_registry_owns_one_session_per_parent_and_fences_late_messages() {
    let mut registry =
        HashMap::from([("parent-a", plugin_parent()), ("parent-b", plugin_parent())]);

    let first_a = {
        let parent = registry.get_mut("parent-a").unwrap();
        drive_plugin(
            &mut parent.application,
            &parent.control,
            Some(completed_text("from-parent-a")),
        )
        .await
    };
    let first_b = {
        let parent = registry.get_mut("parent-b").unwrap();
        drive_plugin(&mut parent.application, &parent.control, None).await
    };

    assert_ne!(first_a.frame().target(), first_b.frame().target());
    assert_eq!(
        first_a.frame().submission().canonical_bytes(),
        first_b.frame().submission().canonical_bytes()
    );
    let parent_a = registry.get("parent-a").unwrap();
    assert!(matches!(
        parent_a
            .control
            .complete(first_a.ingress_generation())
            .await,
        Err(super::super::external::ExternalControlFault::StaleIngress)
    ));

    let second_a = {
        let parent = registry.get_mut("parent-a").unwrap();
        drive_plugin(&mut parent.application, &parent.control, None).await
    };
    assert!(matches!(second_a.frame().basis(), FrameBasis::DeltaFrom(_)));
    assert!(second_a.content().contains("from-parent-a"));
    assert_eq!(second_a.frame().target(), first_a.frame().target());
    assert_eq!(registry.len(), 2);
    assert!(
        registry
            .get("parent-b")
            .unwrap()
            .control
            .next_observation()
            .now_or_never()
            .is_none()
    );

    for parent in registry.into_values() {
        parent.application.shutdown().await.unwrap();
    }
}

fn rendered_text(projection: &RenderedProjection) -> String {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items())
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => {
                Some(crate::pom_renderer::render_pom_document(pom).unwrap())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
