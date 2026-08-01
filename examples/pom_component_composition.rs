//! Isolated, non-durable component composition with two local typed streaming
//! contracts.
//!
//! This compatibility mount executes its System root once per function call.
//! It does not demonstrate cross-process durable reopen; use
//! `pom_durable_authoring` for the nominal durable authoring boundary.

use agentview::{
    component::{
        advanced::lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
        system_view,
    },
    prelude::*,
};

struct IntentChannels;

impl TurnChannels for IntentChannels {
    type Output = u32;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = String;
}

struct PhraseChannels;

impl TurnChannels for PhraseChannels {
    type Output = String;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = String;
}

struct PlayerChannels;

#[derive(Debug)]
enum PlayerOutput {
    Intent { _value: u32 },
    Phrase { _value: String },
}

#[derive(Debug)]
enum PlayerDiagnostic {
    Intent { _message: String },
    Phrase { _message: String },
}

impl TurnChannels for PlayerChannels {
    type Output = PlayerOutput;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = PlayerDiagnostic;
}

fn contract(name: &str) -> XmlNode {
    XmlNode::new(XmlName::try_from(name).expect("static example tag is valid"))
}

#[agentview::view(component)]
fn player_rules() -> PomView {
    view(Document::from_xml(contract("player_rules")))
}

#[agentview::view(component)]
fn select_intent() -> Component<IntentChannels> {
    StreamingXml::<TurnEmission<IntentChannels>, String>::new(contract("select_intent"))
        .state_with(|| 0_u32)
        .on_complete(|selected, _| vec![TurnEmission::Output(*selected)])
        .into_component()
}

#[agentview::view(component)]
fn phrase() -> Component<PhraseChannels> {
    StreamingXml::<TurnEmission<PhraseChannels>, String>::new(contract("phrase"))
        .state_with(String::new)
        .on_complete(|value, element| {
            value.clone_from(&element.content);
            vec![TurnEmission::Output(value.clone())]
        })
        .into_component()
}

#[agentview::view(component)]
fn player_system_component() -> Component<PlayerChannels> {
    component((
        player_rules(),
        select_intent().map_channels(
            TurnChannelMap::<IntentChannels, PlayerChannels>::builder()
                .output(|value| PlayerOutput::Intent { _value: value })
                .live(Never::absurd)
                .commit(Never::absurd)
                .diagnostic(|message| PlayerDiagnostic::Intent { _message: message })
                .build(),
        ),
        phrase().map_channels(
            TurnChannelMap::<PhraseChannels, PlayerChannels>::builder()
                .output(|value| PlayerOutput::Phrase { _value: value })
                .live(Never::absurd)
                .commit(Never::absurd)
                .diagnostic(|message| PlayerDiagnostic::Phrase { _message: message })
                .build(),
        ),
    ))
}

fn player_system(_: SystemMountContext<'_, ()>) -> SystemView<PlayerChannels> {
    system_view(player_system_component())
}

fn main() -> anyhow::Result<()> {
    let epoch = mount_system_epoch(&(), player_system)?;
    println!("SYSTEM\n{}", epoch.rendered_system());
    println!("bindings={}", epoch.binding_factories().len());
    Ok(())
}
