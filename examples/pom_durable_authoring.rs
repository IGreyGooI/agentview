//! Prelude-only authoring proof for a nominal durable System component tree.
//!
//! The final conversion below is deliberately an isolated compatibility
//! preview. It consumes the durable tree once because the public mounted owner
//! and cross-process reopen facade are not frozen yet.

use agentview::{
    component::advanced::lifecycle::{
        mount_system_epoch_with_contract, SystemMountContext, SystemView,
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

fn xml(name: &str) -> XmlNode {
    XmlNode::new(XmlName::try_from(name).expect("static example tag is valid"))
}

#[view(component)]
fn select_intent() -> DurableComponent<PlayerChannels> {
    StreamingXml::<TurnEmission<PlayerChannels>, Never>::new(xml("select_intent"))
        .state_with(|| ())
        .into_durable_component(
            RuntimeContract::new("forgotten-city.select-intent", "v1")
                .expect("static runtime contract is valid"),
        )
}

#[view(component)]
fn player_durable_system() -> DurableSystem<PlayerChannels> {
    durable_system((Document::from_xml(xml("player_rules")), select_intent()))
}

fn isolated_preview(_: SystemMountContext<'_, ()>) -> SystemView<PlayerChannels> {
    system_view(player_durable_system().into_one_shot_component())
}

fn main() -> anyhow::Result<()> {
    let epoch = mount_system_epoch_with_contract(
        EpochContractId::new("forgotten-city/player/v1")?,
        &(),
        isolated_preview,
    )?;

    assert_eq!(epoch.binding_factories().len(), 1);
    println!("SYSTEM\n{}", epoch.rendered_system());
    println!("durable bindings={}", epoch.binding_factories().len());
    Ok(())
}
