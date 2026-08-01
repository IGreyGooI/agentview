//! P3 type spike: one combined System/User render with classified stream lanes.
//! Live and Commit values are accumulated here; their real-time interpreters
//! and mounted-System lifecycle belong to later roadmap phases.

use agentview::component::advanced::experimental::view;
use agentview::{component::advanced::experimental::*, prelude::*};

struct PlayerChannels;

#[derive(Debug)]
enum PlayerOutput {
    Selected(u32),
    PhraseParsedClose,
}

#[derive(Debug)]
enum PlayerLive {
    SelectionStarted(u32),
    PhraseAppend(String),
}

#[derive(Debug)]
enum PlayerCommit {
    Remember(String),
}

#[derive(Debug)]
enum PlayerDiagnostic {
    Selection(String),
    Phrase(String),
    Memory(String),
}

impl TurnChannels for PlayerChannels {
    type Output = PlayerOutput;
    type Live = PlayerLive;
    type Commit = PlayerCommit;
    type Diagnostic = PlayerDiagnostic;
}

struct SelectionChannels;

impl TurnChannels for SelectionChannels {
    type Output = u32;
    type Live = u32;
    type Commit = Never;
    type Diagnostic = String;
}

struct PhraseChannels;

impl TurnChannels for PhraseChannels {
    type Output = ();
    type Live = String;
    type Commit = Never;
    type Diagnostic = String;
}

fn contract(name: &str) -> XmlNode {
    XmlNode::new(XmlName::try_from(name).expect("static example tag is valid"))
}

#[agentview::view(component)]
fn select_intent() -> StreamingChannelsView<SelectionChannels> {
    let mut contract = contract("select_intent");
    contract
        .push_attribute(XmlName::try_from("index")?, "...")
        .expect("the static attribute is unique");
    StreamingXml::new(contract)
        .init_state(None::<u32>)
        .on_open(|selection, element| {
            let Some(index) = element.attr("index") else {
                return StreamUpdate::from_diagnostic("missing intent index".to_owned());
            };
            let Ok(index) = index.parse::<u32>() else {
                return StreamUpdate::from_diagnostic("intent index must be an integer".to_owned());
            };
            *selection = Some(index);
            StreamUpdate::from_emission(TurnEmission::Live(index))
        })
        .finish(|selection| match selection {
            Some(index) => StreamUpdate::from_emission(TurnEmission::Output(index)),
            None => StreamUpdate::from_diagnostic("selection never opened".to_owned()),
        })
        .into_view()
}

#[derive(Default)]
struct PhraseState {
    delivered: String,
}

fn reduce_phrase(
    state: &mut PhraseState,
    element: &XmlElement,
    parsed_close: bool,
) -> StreamUpdate<TurnEmission<PhraseChannels>, String> {
    let Some(delta) = element.content.strip_prefix(&state.delivered) else {
        return StreamUpdate::from_diagnostic(
            "phrase stream stopped being an append-only snapshot".to_owned(),
        );
    };
    let delta = delta.to_owned();
    state.delivered = element.content.clone();

    let mut update = StreamUpdate::new();
    if !delta.is_empty() {
        update = update.with_emission(TurnEmission::Live(delta));
    }
    if parsed_close {
        update = update.with_emission(TurnEmission::Output(()));
    }
    update
}

#[agentview::view(component)]
fn phrase() -> StreamingChannelsView<PhraseChannels> {
    StreamingXml::new(contract("phrase"))
        .init_state(PhraseState::default())
        .on_stream(|state, element| reduce_phrase(state, element, false))
        .on_complete(|state, element| reduce_phrase(state, element, true))
        .into_view()
}

#[agentview::view(component)]
fn remember() -> StreamingValueView<String, String> {
    StreamingXml::new(contract("remember"))
        .init_state(())
        .on_complete(|_, element| Ok(vec![element.content.clone()]))
        .into_view()
}

#[agentview::view(component)]
fn player_turn() -> StreamingProvidedView<PlayerChannels> {
    view((
        system((
            select_intent().map_channels(
                TurnChannelMap::<SelectionChannels, PlayerChannels>::builder()
                    .output(PlayerOutput::Selected)
                    .live(PlayerLive::SelectionStarted)
                    .commit(Never::absurd)
                    .diagnostic(PlayerDiagnostic::Selection)
                    .build(),
            ),
            phrase().map_channels(
                TurnChannelMap::<PhraseChannels, PlayerChannels>::builder()
                    .output(|_| PlayerOutput::PhraseParsedClose)
                    .live(PlayerLive::PhraseAppend)
                    .commit(Never::absurd)
                    .diagnostic(PlayerDiagnostic::Phrase)
                    .build(),
            ),
            remember()
                .map_commit(PlayerCommit::Remember)
                .map_diagnostic(PlayerDiagnostic::Memory),
        )),
        user(Document::from_xml(contract("task"))),
    ))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plan = compile_component(player_turn())?;
    let (system, user, hooks) = plan.into_parts();
    let system = resolve_system_document(system);
    let (user, _) = resolve_user_document(user, &UserDocumentCursor::default())?;

    println!("SYSTEM\n{}", render_pom_document(&system)?);
    println!("\nUSER\n{}", render_pom_document(&user)?);

    let mut sink = StreamingComponentSink::try_new(hooks)?;
    sink.on_event(TextTurnEvent::TextDelta(
        "<select_intent index=\"2\" /><phrase>Hello".to_owned(),
    ))
    .await;
    sink.on_event(TextTurnEvent::TextDelta(
        " there</phrase><remember>met-player</remember>".to_owned(),
    ))
    .await;
    let outcome = Box::new(sink).finish().await;

    // P3 classifies these lanes. The P4 host runtime will interpret Live during
    // callbacks and Commit after publication; this prototype sink accumulates.
    println!("\nCLASSIFIED EMISSIONS");
    for emission in outcome.effects() {
        match emission {
            TurnEmission::Output(PlayerOutput::Selected(index)) => {
                println!("output selected={index}")
            }
            TurnEmission::Output(PlayerOutput::PhraseParsedClose) => {
                println!("output phrase=parsed-close")
            }
            TurnEmission::Live(PlayerLive::SelectionStarted(index)) => {
                println!("live selection-started={index}")
            }
            TurnEmission::Live(PlayerLive::PhraseAppend(text)) => {
                println!("live phrase-append={text:?}")
            }
            TurnEmission::Commit(PlayerCommit::Remember(value)) => {
                println!("commit remember={value:?}")
            }
        }
    }
    for diagnostic in outcome.diagnostics() {
        match diagnostic {
            PlayerDiagnostic::Selection(message) => println!("selection diagnostic={message}"),
            PlayerDiagnostic::Phrase(message) => println!("phrase diagnostic={message}"),
            PlayerDiagnostic::Memory(message) => println!("memory diagnostic={message}"),
        }
    }

    Ok(())
}
