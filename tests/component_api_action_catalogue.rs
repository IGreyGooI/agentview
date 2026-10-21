use std::{
    collections::BTreeMap,
    convert::Infallible,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, CommandCall, CommandInput, CommandOutcome, CommandSender,
            DebugFrameSnapshot, DebugPromptCapture, DebugProviderPort, Frame, FrameBasis,
            ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault, RenderedProjection,
            SubmitFault, TargetDeclaration, TargetEpoch,
        },
        prelude::*,
    },
    pom::{ContentNode, ContentRef, MixedContent, RawTextNode, XmlName, XmlNode},
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetInput {
    generation: u64,
}

fn choice_content(generation: u64) -> XmlNode {
    let mut choice = XmlNode::new(XmlName::new("choice").unwrap());
    choice
        .push_attribute(XmlName::new("kind").unwrap(), "speak")
        .unwrap();
    let mut phrase = XmlNode::new(XmlName::new("phrase").unwrap());
    phrase.push(MixedContent::raw_text(RawTextNode::new(format!(
        "phrase-{generation}"
    ))));
    choice.push(MixedContent::xml(phrase));
    let mut extra = XmlNode::new(XmlName::new("server_extension").unwrap());
    extra.push(MixedContent::raw_text(RawTextNode::new("retained value")));
    choice.push(MixedContent::xml(extra));
    choice
}

#[component]
fn catalogue_application() -> Component {
    use_wait_for_command();
    let generation = use_signal(|| 0_u64);
    let current = generation.with(|generation| *generation).unwrap();
    let choice = if current < 2 {
        let generation = generation.clone();
        view! {
            Action {
                name: format!("choice_{current}"),
                content: choice_content(current),
                on_call: move || generation.update(|generation| {
                    *generation += 1;
                    json!({"generation": *generation})
                }),
            }
        }
    } else {
        view! {}
    };
    view! {
        { choice }
        Action {
            name: "set",
            description: "Choose the generation",
            on_call: move |input: SetInput| generation.update(|generation| {
                *generation = input.generation;
                json!({"generation": *generation})
            }),
        }
        Action {
            name: "inspect",
            on_call: move || Ok::<_, Infallible>(current),
        }
    }
}

#[component]
fn disappearing_actions() -> Component {
    use_wait_for_command();
    let finished = use_signal(|| false);
    if finished.with(|finished| *finished).unwrap() {
        return view! {};
    }
    view! {
        Action {
            name: "finish",
            content: choice_content(0),
            on_call: move || finished.update(|finished| *finished = true),
        }
    }
}

#[component]
fn disappearing_developer_actions() -> Component {
    use_wait_for_command();
    let finished = use_signal(|| false);
    if finished.with(|finished| *finished).unwrap() {
        return view! {};
    }
    view! {
        #[developer]
        Action {
            name: "finish",
            content: choice_content(0),
            on_call: move || finished.update(|finished| *finished = true),
        }
    }
}

#[component]
fn ephemeral_actions() -> Component {
    use_wait_for_command();
    let generation = use_signal(|| 0_u64);
    let current = generation.with(|generation| *generation).unwrap();
    view! {
        #[user(ephemeral, repeat)]
        Action {
            name: format!("choice_{current}"),
            content: choice_content(current),
            on_call: move || generation.update(|generation| {
                *generation += 1;
                json!({"generation": *generation})
            }),
        }
    }
}

#[component]
fn mixed_action_placements() -> Component {
    use_wait_for_command();
    let finished = use_signal(|| false);
    let is_finished = finished.with(|finished| *finished).unwrap();
    let ordinary = if is_finished {
        view! {}
    } else {
        view! {
            Action {
                name: "finish",
                content: choice_content(0),
                on_call: move || finished.update(|finished| *finished = true),
            }
        }
    };
    view! {
        { ordinary }
        #[user(ephemeral, repeat)]
        Action {
            name: "inspect",
            on_call: move || Ok::<_, Infallible>(is_finished),
        }
    }
}

fn xml_children(node: &XmlNode) -> impl Iterator<Item = &XmlNode> {
    node.children().iter().filter_map(|child| match child {
        ContentRef::Node(ContentNode::Xml(child)) => Some(child),
        _ => None,
    })
}

fn attribute<'a>(node: &'a XmlNode, name: &str) -> Option<&'a str> {
    node.attributes()
        .iter()
        .find(|attribute| attribute.name().as_str() == name)
        .map(|attribute| attribute.value())
}

fn roots(items: &[CanonicalInputItem]) -> Vec<XmlNode> {
    items
        .iter()
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(pom),
            _ => None,
        })
        .flat_map(|pom| pom.children().iter())
        .filter_map(|child| match child {
            ContentRef::Node(ContentNode::Xml(root)) => Some(root.clone()),
            _ => None,
        })
        .collect()
}

fn frame_roots(frame: &DebugFrameSnapshot) -> Vec<XmlNode> {
    let payload: Value = serde_json::from_slice(frame.canonical_payload()).unwrap();
    payload_roots(&payload["component"]["projection"]["items"])
}

fn payload_roots(items: &Value) -> Vec<XmlNode> {
    let items: Vec<CanonicalInputItem> = if items.is_null() {
        Vec::new()
    } else {
        serde_json::from_value(items.clone()).unwrap()
    };
    roots(&items)
}

fn ephemeral_roots(frame: &DebugFrameSnapshot) -> Vec<XmlNode> {
    let payload: Value = serde_json::from_slice(frame.canonical_payload()).unwrap();
    payload_roots(&payload["component"]["ephemeral"])
}

fn catalogue(roots: &[XmlNode]) -> &XmlNode {
    roots
        .iter()
        .find(|root| root.name().as_str() == "action_catalogue")
        .expect("current native action catalogue")
}

fn entries(root: &XmlNode) -> &XmlNode {
    xml_children(root)
        .find(|child| child.name().as_str() == "actions")
        .expect("catalogue action entries")
}

fn action_map(root: &XmlNode) -> BTreeMap<String, XmlNode> {
    xml_children(entries(root))
        .map(|action| {
            (
                attribute(action, "name").unwrap().to_owned(),
                action.clone(),
            )
        })
        .collect()
}

fn apply_patch(actions: &mut BTreeMap<String, XmlNode>, catalogue: &XmlNode) {
    assert_eq!(attribute(catalogue, "rendering_mode"), Some("delta"));
    let entries = entries(catalogue);
    assert_eq!(attribute(entries, "rendering_mode"), Some("delta"));
    for operation in xml_children(entries) {
        for action in xml_children(operation) {
            let name = attribute(action, "name").unwrap().to_owned();
            match operation.name().as_str() {
                "insert" => assert!(actions.insert(name, action.clone()).is_none()),
                "update" => assert!(actions.insert(name, action.clone()).is_some()),
                "remove" => {
                    assert_eq!(action.attributes().len(), 1);
                    assert!(action.children().is_empty());
                    assert!(actions.remove(&name).is_some());
                }
                other => panic!("unexpected keyed operation {other}"),
            }
        }
    }
}

fn projection_catalogue(projection: &RenderedProjection) -> XmlNode {
    let roots = projection
        .nodes()
        .iter()
        .flat_map(|node| roots(node.items()))
        .collect::<Vec<_>>();
    catalogue(&roots).clone()
}

async fn call<P: ReactionPort>(
    app: &mut Application<P>,
    sender: &CommandSender,
    name: &str,
    input: Value,
) -> CommandOutcome {
    let (prepared, response) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(app.prepare(), sender.submit(CommandCall::new(name, input)))
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    response.unwrap().outcome
}

async fn handoff(
    app: &mut Application<DebugProviderPort>,
    sender: &CommandSender,
    capture: &DebugPromptCapture,
) -> DebugFrameSnapshot {
    let (reacted, observed) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(app.react(), sender.observe())
    })
    .await
    .unwrap();
    assert!(reacted.unwrap().is_continue());
    observed.unwrap();
    capture.latest_frame().unwrap()
}

#[tokio::test]
async fn native_catalogue_deltas_reconstruct_current_actions_and_retire_dynamic_names() {
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(catalogue_application, port, input).unwrap();
    let full = handoff(&mut app, &sender, &capture).await;
    assert!(matches!(full.basis(), FrameBasis::Full));
    let initial = frame_roots(&full);
    let root = catalogue(&initial);
    assert_eq!(
        xml_children(root)
            .filter(|node| node.name().as_str() == "stdin")
            .count(),
        1
    );
    let mut actions = action_map(root);
    assert_eq!(actions.len(), 3);
    let choice = &actions["choice_0"];
    assert_eq!(attribute(choice, "input"), Some("{}"));
    assert_eq!(attribute(choice, "description"), None);
    assert_eq!(
        xml_children(choice)
            .map(|child| child.name().as_str())
            .collect::<Vec<_>>(),
        ["choice"]
    );
    assert_eq!(
        xml_children(xml_children(choice).next().unwrap()).count(),
        2
    );
    let schema = xml_children(&actions["set"])
        .find(|child| child.name().as_str() == "input_schema")
        .unwrap();
    let schema = schema
        .children()
        .iter()
        .find_map(|child| match child {
            ContentRef::Node(ContentNode::RawText(text)) => Some(text.value()),
            _ => None,
        })
        .unwrap();
    let schema: Value = serde_json::from_str(schema).unwrap();
    assert_eq!(schema["required"], json!(["generation"]));
    assert_eq!(schema["additionalProperties"], false);

    let unchanged = handoff(&mut app, &sender, &capture).await;
    assert!(frame_roots(&unchanged).is_empty());
    assert_eq!(
        call(&mut app, &sender, "choice_0", json!({})).await,
        CommandOutcome::Output(json!({"generation": 1}))
    );
    let delta = handoff(&mut app, &sender, &capture).await;
    assert!(matches!(delta.basis(), FrameBasis::DeltaFrom(_)));
    let delta_roots = frame_roots(&delta);
    let catalogue_delta = catalogue(&delta_roots);
    assert_eq!(
        xml_children(catalogue_delta).count(),
        1,
        "shared input guidance stays unchanged"
    );
    let changed_names = xml_children(entries(catalogue_delta))
        .flat_map(xml_children)
        .map(|action| attribute(action, "name").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(changed_names, ["choice_1", "choice_0"]);
    apply_patch(&mut actions, catalogue_delta);
    assert_eq!(
        actions,
        action_map(&projection_catalogue(app.current_projection().projection()))
    );
    assert!(
        matches!(call(&mut app, &sender, "choice_0", json!({})).await,
        CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable")
    );
    let rejected = handoff(&mut app, &sender, &capture).await;
    assert!(frame_roots(&rejected)
        .iter()
        .all(|node| node.name().as_str() != "action_catalogue"));
    assert_eq!(
        call(&mut app, &sender, "choice_1", json!({})).await,
        CommandOutcome::Output(json!({"generation": 2}))
    );
    let removed = handoff(&mut app, &sender, &capture).await;
    apply_patch(&mut actions, catalogue(&frame_roots(&removed)));
    assert_eq!(
        actions,
        action_map(&projection_catalogue(app.current_projection().projection()))
    );
    assert_eq!(
        actions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inspect", "set"]
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn retiring_the_last_action_sends_a_compact_remove_and_leaves_an_empty_catalogue() {
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(disappearing_actions, port, input).unwrap();
    let full = handoff(&mut app, &sender, &capture).await;
    let mut actions = action_map(catalogue(&frame_roots(&full)));
    assert!(matches!(
        call(&mut app, &sender, "finish", json!({})).await,
        CommandOutcome::Output(_)
    ));
    let removed = handoff(&mut app, &sender, &capture).await;
    apply_patch(&mut actions, catalogue(&frame_roots(&removed)));
    assert!(actions.is_empty());
    assert!(action_map(&projection_catalogue(app.current_projection().projection())).is_empty());
    assert!(matches!(call(&mut app, &sender, "finish", json!({})).await,
        CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable"));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn retiring_a_catalogue_in_another_placement_removes_only_its_root_identity() {
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(8);
    let mut app =
        Application::mount_with_commands(disappearing_developer_actions, port, input).unwrap();
    let full = handoff(&mut app, &sender, &capture).await;
    assert!(frame_roots(&full)
        .iter()
        .filter(|root| root.name().as_str() == "action_catalogue")
        .any(|root| action_map(root).contains_key("finish")));
    assert!(matches!(
        call(&mut app, &sender, "finish", json!({})).await,
        CommandOutcome::Output(_)
    ));
    let retired = handoff(&mut app, &sender, &capture).await;
    let roots = frame_roots(&retired);
    let remove = roots
        .iter()
        .find(|root| root.name().as_str() == "remove")
        .unwrap();
    let identity = xml_children(remove).next().unwrap();
    assert_eq!(identity.name().as_str(), "action_catalogue");
    assert!(identity.children().is_empty());
    assert!(identity.attributes().is_empty());
    assert!(matches!(
        call(&mut app, &sender, "finish", json!({})).await,
        CommandOutcome::Rejected { ref code, .. } if code == "command_unavailable"
    ));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn ephemeral_catalogue_repeats_complete_current_actions_and_keeps_feedback_ordinary() {
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(ephemeral_actions, port, input).unwrap();
    let first = handoff(&mut app, &sender, &capture).await;
    assert_eq!(first.basis(), FrameBasis::Full);
    assert!(frame_roots(&first)
        .iter()
        .all(|root| root.name().as_str() != "action_catalogue"));
    let initial = ephemeral_roots(&first);
    assert_eq!(initial.len(), 1);
    let initial_catalogue = catalogue(&initial);
    assert_eq!(attribute(initial_catalogue, "rendering_mode"), None);
    assert_eq!(
        attribute(entries(initial_catalogue), "rendering_mode"),
        None
    );
    assert_eq!(
        action_map(initial_catalogue)
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["choice_0"]
    );

    let repeated = handoff(&mut app, &sender, &capture).await;
    assert!(matches!(repeated.basis(), FrameBasis::DeltaFrom(_)));
    assert!(frame_roots(&repeated).is_empty());
    assert_eq!(ephemeral_roots(&repeated), initial);
    assert_eq!(
        call(&mut app, &sender, "choice_0", json!({})).await,
        CommandOutcome::Output(json!({"generation": 1}))
    );

    let changed = handoff(&mut app, &sender, &capture).await;
    let current = ephemeral_roots(&changed);
    assert_eq!(current.len(), 1);
    let current_catalogue = catalogue(&current);
    assert_eq!(attribute(current_catalogue, "rendering_mode"), None);
    assert_eq!(
        attribute(entries(current_catalogue), "rendering_mode"),
        None
    );
    assert_eq!(
        action_map(current_catalogue)
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["choice_1"]
    );
    let ordinary = frame_roots(&changed);
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].name().as_str(), "command_feedback");
    let records = xml_children(&ordinary[0])
        .find(|child| child.name().as_str() == "records")
        .unwrap();
    let result = xml_children(records).next().unwrap();
    assert_eq!(attribute(result, "command"), Some("choice_0"));
    assert_eq!(attribute(result, "status"), Some("completed"));
    assert!(matches!(
        call(&mut app, &sender, "choice_0", json!({})).await,
        CommandOutcome::Rejected { code, .. } if code == "command_unavailable"
    ));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn retiring_the_ordinary_catalogue_keeps_ephemeral_actions_and_retained_feedback() {
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(mixed_action_placements, port, input).unwrap();
    let first = handoff(&mut app, &sender, &capture).await;
    assert!(action_map(catalogue(&frame_roots(&first))).contains_key("finish"));
    assert!(action_map(catalogue(&ephemeral_roots(&first))).contains_key("inspect"));
    assert!(matches!(
        call(&mut app, &sender, "finish", json!({})).await,
        CommandOutcome::Output(_)
    ));

    let retired = handoff(&mut app, &sender, &capture).await;
    let ordinary = frame_roots(&retired);
    let remove = ordinary
        .iter()
        .find(|root| root.name().as_str() == "remove")
        .unwrap();
    let identity = xml_children(remove).next().unwrap();
    assert_eq!(identity.name().as_str(), "action_catalogue");
    assert!(identity.children().is_empty());
    assert!(identity.attributes().is_empty());
    assert!(ordinary
        .iter()
        .any(|root| root.name().as_str() == "command_feedback"));
    assert!(ordinary
        .iter()
        .all(|root| root.name().as_str() != "action_catalogue"));
    let ephemeral = ephemeral_roots(&retired);
    assert_eq!(ephemeral.len(), 1);
    assert!(action_map(catalogue(&ephemeral)).contains_key("inspect"));
    assert!(matches!(
        call(&mut app, &sender, "finish", json!({})).await,
        CommandOutcome::Rejected { code, .. } if code == "command_unavailable"
    ));
    assert_eq!(
        call(&mut app, &sender, "inspect", json!({})).await,
        CommandOutcome::Output(json!(true))
    );
    app.shutdown().await.unwrap();
}

/// Always requesting Full makes retained history observable on every handoff.
/// Delta replay alone would not prove that ephemeral catalogues were excluded.
struct FullReplayPort {
    declaration: TargetDeclaration,
    frames: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl ReactionPort for FullReplayPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        assert_eq!(frame.basis(), FrameBasis::Full);
        self.frames
            .lock()
            .unwrap()
            .push(serde_json::from_slice(frame.submission().canonical_bytes()).unwrap());
        let next_epoch = NonZeroU64::new(self.declaration.continuity().epoch().get().get() + 1)
            .expect("the test has a bounded number of frames");
        self.declaration = TargetDeclaration::full(
            self.declaration.identity(),
            TargetEpoch::new(next_epoch),
            self.declaration.profile().clone(),
        );
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderFact::ReactionCompleted { primary_text: None })
        })))
    }
}

#[tokio::test]
async fn full_replay_retains_action_feedback_and_excludes_prior_ephemeral_catalogues() {
    let (mut debug, _) = DebugProviderPort::new();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let port = FullReplayPort {
        declaration: debug.declare().unwrap(),
        frames: frames.clone(),
    };
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(ephemeral_actions, port, input).unwrap();
    for frame in 0..3 {
        if frame == 1 {
            assert_eq!(
                call(&mut app, &sender, "choice_0", json!({})).await,
                CommandOutcome::Output(json!({"generation": 1}))
            );
        }
        let (reacted, observed) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(app.react(), sender.observe())
        })
        .await
        .unwrap();
        assert!(reacted.unwrap().is_continue());
        observed.unwrap();
    }

    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 3);
    for frame in &frames {
        for items in [
            &frame["replay"],
            &frame["staged_inputs"],
            &frame["component"]["projection"]["items"],
        ] {
            assert!(payload_roots(items)
                .iter()
                .all(|root| root.name().as_str() != "action_catalogue"));
        }
        let ephemeral = payload_roots(&frame["component"]["ephemeral"]);
        assert_eq!(ephemeral.len(), 1);
        assert_eq!(attribute(catalogue(&ephemeral), "rendering_mode"), None);
    }
    let replay = payload_roots(&frames[2]["replay"]);
    assert!(replay.iter().any(|root| {
        root.name().as_str() == "command_feedback"
            && xml_children(root)
                .filter(|node| node.name().as_str() == "records")
                .flat_map(xml_children)
                .any(|record| {
                    attribute(record, "command") == Some("choice_0")
                        && attribute(record, "status") == Some("completed")
                })
    }));
    app.shutdown().await.unwrap();
}
