use agentview::component::advanced::experimental::view;
use agentview::component::advanced::experimental::*;
use agentview::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalEffect {
    Opened(u32),
    Streamed(String),
    Completed(String),
    Finished(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParentEffect {
    Counter(LocalEffect),
}

struct HarnessChannels;

#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessOutput {
    Parsed(u32),
    ParsedClose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessLive {
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessCommit {
    Persist(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessDiagnostic {
    Output(String),
    Live(String),
    Commit(String),
    Selection(String),
}

impl TurnChannels for HarnessChannels {
    type Output = HarnessOutput;
    type Live = HarnessLive;
    type Commit = HarnessCommit;
    type Diagnostic = HarnessDiagnostic;
}

struct SelectionChannels;

impl TurnChannels for SelectionChannels {
    type Output = u32;
    type Live = String;
    type Commit = String;
    type Diagnostic = String;
}

struct PhraseChannels;

impl TurnChannels for PhraseChannels {
    type Output = ();
    type Live = String;
    type Commit = Never;
    type Diagnostic = String;
}

#[derive(Debug, Default)]
struct CounterState {
    events: usize,
}

fn counter_contract() -> XmlNode {
    let mut contract = XmlNode::new(XmlName::try_from("counter").unwrap());
    contract
        .push_attribute(XmlName::try_from("amount").unwrap(), "...")
        .unwrap();
    contract
}

fn channel_contract(name: &str) -> XmlNode {
    XmlNode::new(XmlName::try_from(name).unwrap())
}

#[agentview::view(component)]
fn counter_component() -> StreamingValueView<LocalEffect, String> {
    StreamingXml::<LocalEffect, String>::new(counter_contract())
        .init_state(CounterState::default())
        .on_open(|state, element| {
            state.events += 1;
            let amount = element
                .attr("amount")
                .ok_or_else(|| "counter requires amount".to_owned())?
                .parse::<u32>()
                .map_err(|_| "counter amount must be an integer".to_owned())?;
            Ok(vec![LocalEffect::Opened(amount)])
        })
        .on_stream(|state, element| {
            state.events += 1;
            Ok(vec![LocalEffect::Streamed(element.content.clone())])
        })
        .on_complete(|state, element| {
            state.events += 1;
            Ok(vec![LocalEffect::Completed(element.content.clone())])
        })
        .finish(|state| Ok(vec![LocalEffect::Finished(state.events)]))
        .into_view()
}

#[agentview::view(component)]
fn parent_component() -> StreamingValueView<ParentEffect, String> {
    view((
        system(counter_component().map_effect(ParentEffect::Counter)),
        user(Document::from_xml(XmlNode::new(
            XmlName::try_from("task").unwrap(),
        ))),
    ))
}

#[agentview::view(component)]
fn local_output_component() -> StreamingValueView<u32, String> {
    let mut contract = channel_contract("channel_output");
    contract
        .push_attribute(XmlName::try_from("value").unwrap(), "...")
        .unwrap();
    StreamingXml::<u32, String>::new(contract)
        .init_state(())
        .on_open(|_, element| {
            let value = element
                .attr("value")
                .ok_or_else(|| "missing output value".to_owned())?
                .parse::<u32>()
                .map_err(|_| "output value must be an integer".to_owned())?;
            Ok(vec![value])
        })
        .into_view()
}

#[agentview::view(component)]
fn local_live_component() -> StreamingValueView<String, String> {
    StreamingXml::<String, String>::new(channel_contract("channel_live"))
        .init_state(())
        .on_stream(|_, element| Ok(vec![element.content.clone()]))
        .into_view()
}

#[agentview::view(component)]
fn local_commit_component() -> StreamingValueView<String, String> {
    StreamingXml::<String, String>::new(channel_contract("channel_commit"))
        .init_state(())
        .on_complete(|_, element| Ok(vec![element.content.clone()]))
        .into_view()
}

#[agentview::view(component)]
fn channel_parent_component() -> StreamingProvidedView<HarnessChannels> {
    view((
        system((
            local_output_component()
                .map_output(HarnessOutput::Parsed)
                .map_diagnostic(HarnessDiagnostic::Output),
            local_live_component()
                .map_live(HarnessLive::Text)
                .map_diagnostic(HarnessDiagnostic::Live),
            local_commit_component()
                .map_commit(HarnessCommit::Persist)
                .map_diagnostic(HarnessDiagnostic::Commit),
        )),
        user(Document::from_xml(XmlNode::new(
            XmlName::try_from("channel_task").unwrap(),
        ))),
    ))
}

#[agentview::view(component)]
fn multi_lane_selection() -> StreamingChannelsView<SelectionChannels> {
    let mut contract = channel_contract("multi_lane_selection");
    contract
        .push_attribute(XmlName::try_from("index").unwrap(), "...")
        .unwrap();
    StreamingXml::new(contract)
        .init_state(None::<u32>)
        .on_open(|selection, element| {
            let index = element
                .attr("index")
                .and_then(|index| index.parse::<u32>().ok())
                .unwrap_or_default();
            *selection = Some(index);
            StreamUpdate::from_emission(TurnEmission::Live(format!("selecting:{index}")))
                .with_diagnostic("selection observed".to_owned())
        })
        .finish(|selection| match selection {
            Some(index) => StreamUpdate::from_emission(TurnEmission::Output(index))
                .with_emission(TurnEmission::Commit(format!("selected:{index}"))),
            None => StreamUpdate::from_diagnostic("selection never opened".to_owned()),
        })
        .into_view()
}

#[agentview::view(component)]
fn mapped_multi_lane_selection() -> StreamingProvidedView<HarnessChannels> {
    system(
        multi_lane_selection().map_channels(
            TurnChannelMap::<SelectionChannels, HarnessChannels>::builder()
                .output(HarnessOutput::Parsed)
                .live(HarnessLive::Text)
                .commit(HarnessCommit::Persist)
                .diagnostic(HarnessDiagnostic::Selection)
                .build(),
        ),
    )
}

#[derive(Default)]
struct PhraseState {
    delivered: String,
}

fn reduce_phrase_snapshot(
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
fn closing_chunk_phrase() -> StreamingChannelsView<PhraseChannels> {
    StreamingXml::new(channel_contract("closing_chunk_phrase"))
        .init_state(PhraseState::default())
        .on_stream(|state, element| reduce_phrase_snapshot(state, element, false))
        .on_complete(|state, element| reduce_phrase_snapshot(state, element, true))
        .into_view()
}

#[agentview::view(component)]
fn mapped_closing_chunk_phrase() -> StreamingProvidedView<HarnessChannels> {
    system(
        closing_chunk_phrase().map_channels(
            TurnChannelMap::<PhraseChannels, HarnessChannels>::builder()
                .output(|_| HarnessOutput::ParsedClose)
                .live(HarnessLive::Text)
                .commit(Never::absurd)
                .diagnostic(HarnessDiagnostic::Live)
                .build(),
        ),
    )
}

#[tokio::test]
async fn streaming_component_keeps_contract_reducers_and_effects_together() {
    let plan = compile_component(parent_component()).unwrap();
    let (system, user, hooks) = plan.into_parts();
    assert_eq!(
        render_pom_document(&resolve_system_document(system)).unwrap(),
        "<counter amount=\"...\" />"
    );
    let (user, _) = resolve_user_document(user, &UserDocumentCursor::default()).unwrap();
    assert_eq!(render_pom_document(&user).unwrap(), "<task />");

    let mut sink = StreamingComponentSink::try_new(hooks).unwrap();
    sink.on_event(TextTurnEvent::TextDelta(
        "<counter amount=\"2\">hel".to_owned(),
    ))
    .await;
    sink.on_event(TextTurnEvent::TextDelta("lo</counter>".to_owned()))
        .await;
    sink.on_event(TextTurnEvent::TextComplete(
        "<counter amount=\"2\">hello</counter>".to_owned(),
    ))
    .await;
    let outcome = Box::new(sink).finish().await;

    assert_eq!(
        outcome.effects(),
        &[
            ParentEffect::Counter(LocalEffect::Opened(2)),
            ParentEffect::Counter(LocalEffect::Streamed("hel".to_owned())),
            ParentEffect::Counter(LocalEffect::Completed("hello".to_owned())),
            ParentEffect::Counter(LocalEffect::Finished(3)),
        ]
    );
    assert!(outcome.diagnostics().is_empty());
    assert_eq!(
        outcome.raw_output(),
        "<counter amount=\"2\">hello</counter>"
    );
}

#[tokio::test]
async fn streaming_siblings_map_all_typed_channels_to_one_harness_contract() {
    let plan = compile_component(channel_parent_component()).unwrap();
    let (system, user, hooks) = plan.into_parts();
    assert_eq!(
        render_pom_document(&resolve_system_document(system)).unwrap(),
        concat!(
            "<channel_output value=\"...\" />\n\n",
            "<channel_live />\n\n",
            "<channel_commit />"
        )
    );
    let (user, _) = resolve_user_document(user, &UserDocumentCursor::default()).unwrap();
    assert_eq!(render_pom_document(&user).unwrap(), "<channel_task />");

    let mut sink = StreamingComponentSink::try_new(hooks).unwrap();
    sink.on_event(TextTurnEvent::TextDelta(
        concat!(
            "<channel_output value=\"invalid\" />",
            "<channel_output value=\"7\" />",
            "<channel_live>hello"
        )
        .to_owned(),
    ))
    .await;
    sink.on_event(TextTurnEvent::TextDelta(
        "</channel_live><channel_commit>save-me</channel_commit>".to_owned(),
    ))
    .await;
    let outcome = Box::new(sink).finish().await;

    assert_eq!(
        outcome.effects(),
        &[
            TurnEmission::Output(HarnessOutput::Parsed(7)),
            TurnEmission::Live(HarnessLive::Text("hello".to_owned())),
            TurnEmission::Commit(HarnessCommit::Persist("save-me".to_owned())),
        ]
    );
    assert_eq!(
        outcome.diagnostics(),
        &[HarnessDiagnostic::Output(
            "output value must be an integer".to_owned()
        )]
    );
}

#[tokio::test]
async fn one_streaming_binding_can_emit_live_output_commit_and_a_diagnostic() {
    let plan = compile_component(mapped_multi_lane_selection()).unwrap();
    let (_, _, hooks) = plan.into_parts();
    assert_eq!(hooks.len(), 1);

    let mut sink = StreamingComponentSink::try_new(hooks).unwrap();
    sink.on_event(TextTurnEvent::TextDelta(
        "<multi_lane_selection index=\"9\" />".to_owned(),
    ))
    .await;
    let outcome = Box::new(sink).finish().await;

    assert_eq!(
        outcome.effects(),
        &[
            TurnEmission::Live(HarnessLive::Text("selecting:9".to_owned())),
            TurnEmission::Output(HarnessOutput::Parsed(9)),
            TurnEmission::Commit(HarnessCommit::Persist("selected:9".to_owned())),
        ]
    );
    assert_eq!(
        outcome.diagnostics(),
        &[HarnessDiagnostic::Selection(
            "selection observed".to_owned()
        )]
    );
}

#[tokio::test]
async fn closing_chunk_emits_final_live_append_before_parsed_close_output() {
    let plan = compile_component(mapped_closing_chunk_phrase()).unwrap();
    let (_, _, hooks) = plan.into_parts();
    let mut sink = StreamingComponentSink::try_new(hooks).unwrap();

    sink.on_event(TextTurnEvent::TextDelta(
        "<closing_chunk_phrase>Hello".to_owned(),
    ))
    .await;
    sink.on_event(TextTurnEvent::TextDelta(
        " there</closing_chunk_phrase>".to_owned(),
    ))
    .await;
    let outcome = Box::new(sink).finish().await;

    assert_eq!(
        outcome.effects(),
        &[
            TurnEmission::Live(HarnessLive::Text("Hello".to_owned())),
            TurnEmission::Live(HarnessLive::Text(" there".to_owned())),
            TurnEmission::Output(HarnessOutput::ParsedClose),
        ]
    );
    assert!(outcome.diagnostics().is_empty());
}

#[tokio::test]
async fn legacy_result_err_becomes_a_non_terminal_typed_diagnostic() {
    let plan = compile_component(system(counter_component())).unwrap();
    let (_, _, hooks) = plan.into_parts();
    let mut sink = StreamingComponentSink::try_new(hooks).unwrap();
    sink.on_event(TextTurnEvent::TextDelta(
        "<counter amount=\"invalid\" />".to_owned(),
    ))
    .await;
    let outcome = Box::new(sink).finish().await;

    assert_eq!(
        outcome.diagnostics(),
        &["counter amount must be an integer"]
    );
    assert_eq!(
        outcome.effects(),
        &[
            LocalEffect::Completed(String::new()),
            LocalEffect::Finished(2),
        ]
    );
}

#[test]
fn generic_tool_contract_requires_a_prompt_facing_name() {
    let contract = XmlNode::new(XmlName::try_from("tool").unwrap());
    let view = StreamingXml::<(), String>::new(contract)
        .init_state(())
        .into_view();
    assert!(matches!(
        view,
        Err(ComponentError::InvalidBindingContract { message })
            if message.contains("requires a name attribute")
    ));
}

#[test]
fn duplicate_parser_tags_fail_when_the_hook_plan_is_bound() {
    let plan = compile_component(view((
        system(counter_component().key("first")),
        system(counter_component().key("second")),
    )))
    .unwrap();
    let (_, _, hooks) = plan.into_parts();
    assert!(matches!(
        StreamingComponentSink::try_new(hooks),
        Err(StreamingComponentError::DuplicateTag { tag, .. }) if tag == "counter"
    ));
}
