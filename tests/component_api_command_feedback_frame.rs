use std::{
    convert::Infallible,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, CommandCall, CommandInput, CommandOutcome, CommandSender,
            DebugFrameSnapshot, DebugProviderPort, Frame, FrameBasis, ProviderFactStream,
            ReactionPort, ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason,
            RenderedProjection, RenderedProjectionDiffMarker, SubmitFault, TargetDeclaration,
        },
        prelude::*,
    },
    pom::{ContentNode, ContentRef, XmlNode},
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::poll;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoInput {
    value: String,
}

#[component]
fn feedback_application() -> Component {
    use_wait_for_command();
    view! {
        Action {
            name: "echo",
            on_call: |input: EchoInput| Ok::<_, Infallible>(input.value),
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

fn feedback(projection: &RenderedProjection) -> XmlNode {
    projection
        .nodes()
        .iter()
        .flat_map(|node| roots(node.items()))
        .find(|root| root.name().as_str() == "command_feedback")
        .expect("the current projection includes command feedback")
}

fn record_sequences(root: &XmlNode) -> Vec<u64> {
    let records = xml_children(root)
        .find(|child| child.name().as_str() == "records")
        .expect("feedback has a records collection");
    xml_children(records)
        .map(|record| {
            assert_eq!(record.name().as_str(), "command_result");
            let sequence = attribute(record, "sequence").unwrap().parse().unwrap();
            assert_eq!(attribute(record, "command"), Some("echo"));
            assert_eq!(attribute(record, "status"), Some("completed"));
            let output = record
                .children()
                .iter()
                .filter_map(|child| match child {
                    ContentRef::Node(ContentNode::Text(text)) => Some(text.value()),
                    ContentRef::Node(ContentNode::RawText(text)) => Some(text.value()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(
                serde_json::from_str::<String>(&output).unwrap(),
                format!("record-{sequence}")
            );
            sequence
        })
        .collect()
}

fn assert_complete_snapshot(node: &XmlNode) {
    assert_eq!(attribute(node, "rendering_mode"), None);
    assert!(
        !["insert", "remove", "update", "replace"].contains(&node.name().as_str()),
        "a complete feedback snapshot has no patch operations"
    );
    for child in xml_children(node) {
        assert_complete_snapshot(child);
    }
}

fn feedback_metadata(
    projection: &RenderedProjection,
) -> (String, Vec<RenderedProjectionDiffMarker>) {
    let node = projection
        .nodes()
        .iter()
        .find(|node| {
            node.diffs()
                .iter()
                .any(|marker| marker.slot() == "command_feedback")
        })
        .expect("command feedback is a mounted Component with its own diff boundary");
    assert!(node.identity().contains("command_feedback"));
    (node.identity().to_owned(), node.diffs().to_vec())
}

fn frame_items(frame: &DebugFrameSnapshot) -> Vec<CanonicalInputItem> {
    let payload: serde_json::Value = serde_json::from_slice(frame.canonical_payload()).unwrap();
    serde_json::from_value(payload["component"]["projection"]["items"].clone()).unwrap()
}

async fn command<P: ReactionPort>(
    app: &mut Application<P>,
    sender: &CommandSender,
    sequence: u64,
) -> RenderedProjection {
    let value = format!("record-{sequence}");
    let (prepared, response) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            app.prepare(),
            sender.submit(CommandCall::new("echo", json!({"value": value})))
        )
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    let response = response.unwrap();
    assert_eq!(response.outcome, CommandOutcome::Output(json!(value)));
    response.projection
}

async fn observe<P: ReactionPort>(
    app: &mut Application<P>,
    sender: &CommandSender,
) -> RenderedProjection {
    let (prepared, observed) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(app.prepare(), sender.observe())
    })
    .await
    .unwrap();
    assert!(prepared.unwrap().is_continue());
    observed.unwrap()
}

async fn handoff<P: ReactionPort>(app: &mut Application<P>, sender: &CommandSender) {
    let (reacted, observed) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(app.react(), sender.observe())
    })
    .await
    .unwrap();
    assert_eq!(reacted.unwrap(), ControlFlow::Continue(()));
    observed.unwrap();
}

#[tokio::test]
async fn command_feedback_tracks_complete_frame_batches_and_whole_snapshot_delivery() {
    let (port, capture) = DebugProviderPort::new();
    let (sender, input) = CommandInput::channel(16);
    let mut app = Application::mount_with_commands(feedback_application, port, input).unwrap();

    let mut metadata = None;
    for sequence in 1..=10 {
        let projection = command(&mut app, &sender, sequence).await;
        assert_eq!(
            record_sequences(&feedback(&projection)),
            (1..=sequence).collect::<Vec<_>>()
        );
        let current = feedback_metadata(&projection);
        assert_eq!(metadata.get_or_insert_with(|| current.clone()), &current);
    }
    assert!(
        capture.frame_snapshots().is_empty(),
        "preparation is not provider delivery"
    );
    let observed = observe(&mut app, &sender).await;
    assert_eq!(
        record_sequences(&feedback(&observed)),
        (1..=10).collect::<Vec<_>>()
    );
    assert_eq!(feedback_metadata(&observed), metadata.clone().unwrap());

    handoff(&mut app, &sender).await;
    let first = capture.latest_frame().unwrap();
    assert_eq!(first.basis(), FrameBasis::Full);
    let first_feedback = roots(&frame_items(&first))
        .into_iter()
        .find(|root| root.name().as_str() == "command_feedback")
        .unwrap();
    assert_eq!(
        record_sequences(&first_feedback),
        (1..=10).collect::<Vec<_>>()
    );
    assert_complete_snapshot(&first_feedback);

    let observed = observe(&mut app, &sender).await;
    assert_eq!(
        record_sequences(&feedback(&observed)),
        (1..=10).collect::<Vec<_>>()
    );
    handoff(&mut app, &sender).await;
    let unchanged = capture.latest_frame().unwrap();
    assert!(matches!(unchanged.basis(), FrameBasis::DeltaFrom(_)));
    assert!(
        frame_items(&unchanged).is_empty(),
        "unchanged feedback is not delivered again"
    );

    let mut new_snapshot = None;
    for sequence in 11..=12 {
        let projection = command(&mut app, &sender, sequence).await;
        let current_snapshot = feedback(&projection);
        assert_eq!(
            record_sequences(&current_snapshot),
            (11..=sequence).collect::<Vec<_>>()
        );
        assert_eq!(feedback_metadata(&projection), metadata.clone().unwrap());
        new_snapshot = Some(current_snapshot);
    }
    handoff(&mut app, &sender).await;
    let next_frame = capture.latest_frame().unwrap();
    assert!(matches!(next_frame.basis(), FrameBasis::DeltaFrom(_)));
    let snapshot = roots(&frame_items(&next_frame))
        .into_iter()
        .find(|root| root.name().as_str() == "command_feedback")
        .expect("the changed batch is delivered as a complete feedback snapshot");
    assert_eq!(record_sequences(&snapshot), [11, 12]);
    assert_complete_snapshot(&snapshot);
    assert_eq!(snapshot, new_snapshot.unwrap());
    app.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct SubmitControl {
    next: Arc<AtomicU8>,
    blocked: Arc<Notify>,
}

struct ControlledPort {
    inner: DebugProviderPort,
    control: SubmitControl,
}

#[async_trait]
impl ReactionPort for ControlledPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.inner.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        match self.control.next.swap(0, Ordering::SeqCst) {
            1 => {
                return Err(SubmitFault::Rejected(ReactionPortFault::retryable(
                    ReactionPortFaultCode::Unavailable,
                    ReactionPortFaultReason::Transport,
                )));
            }
            2 => {
                self.control.blocked.notify_one();
                std::future::pending::<()>().await;
                unreachable!("the caller cancels before handoff");
            }
            _ => self.inner.submit(frame).await,
        }
    }

    async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }
}

#[tokio::test]
async fn pre_handoff_failure_and_cancellation_preserve_the_pending_feedback_batch() {
    let (inner, capture) = DebugProviderPort::new();
    let control = SubmitControl::default();
    let port = ControlledPort {
        inner,
        control: control.clone(),
    };
    let (sender, input) = CommandInput::channel(8);
    let mut app = Application::mount_with_commands(feedback_application, port, input).unwrap();
    command(&mut app, &sender, 1).await;

    control.next.store(1, Ordering::SeqCst);
    let (reacted, observed) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(app.react(), sender.observe())
    })
    .await
    .unwrap();
    assert!(reacted.is_err());
    assert_eq!(record_sequences(&feedback(&observed.unwrap())), [1]);
    assert!(capture.frame_snapshots().is_empty());
    let second = command(&mut app, &sender, 2).await;
    assert_eq!(record_sequences(&feedback(&second)), [1, 2]);

    control.next.store(2, Ordering::SeqCst);
    let blocked = control.blocked.notified();
    let mut observed = Box::pin(sender.observe());
    assert!(poll!(&mut observed).is_pending());
    let mut reaction = Box::pin(app.react());
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = blocked => {}
            result = &mut reaction => panic!("reaction ended before pre-handoff cancellation: {result:?}"),
        }
    })
    .await
    .unwrap();
    drop(reaction);
    assert_eq!(
        record_sequences(&feedback(&observed.await.unwrap())),
        [1, 2]
    );
    assert!(capture.frame_snapshots().is_empty());
    let third = command(&mut app, &sender, 3).await;
    assert_eq!(record_sequences(&feedback(&third)), [1, 2, 3]);

    handoff(&mut app, &sender).await;
    let delivered = roots(&frame_items(&capture.latest_frame().unwrap()))
        .into_iter()
        .find(|root| root.name().as_str() == "command_feedback")
        .unwrap();
    assert_eq!(record_sequences(&delivered), [1, 2, 3]);
    let fourth = command(&mut app, &sender, 4).await;
    assert_eq!(record_sequences(&feedback(&fourth)), [4]);
    app.shutdown().await.unwrap();
}
