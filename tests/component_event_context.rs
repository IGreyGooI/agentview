#[path = "support/pom.rs"]
mod pom_support;

use std::{
    collections::VecDeque,
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{Arc, Mutex},
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, Frame, FrameCapabilities, FrameConstraints, FrameProfile,
            FrameRevision, ProviderFact, ProviderFactStream, ProviderOutputKey, ProviderToolCall,
            ReactionPort, ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch,
            TargetIdentity, ToolDefinition,
        },
        prelude::*,
        ApplicationId, EventContext, EventSource,
    },
    pom::ResolvedDocument,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use pom_support::{combine, InspectPom};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

#[derive(Clone)]
struct CapturedFrame {
    revision: FrameRevision,
    document: ResolvedDocument,
    staged: Vec<CanonicalInputItem>,
    tools: Vec<ToolDefinition>,
}

struct ScriptedPort {
    declaration: TargetDeclaration,
    scripts: VecDeque<Vec<ProviderFact>>,
    frames: Arc<Mutex<Vec<CapturedFrame>>>,
}

impl ScriptedPort {
    fn new(identity: u128, amount: i64) -> (Self, Arc<Mutex<Vec<CapturedFrame>>>) {
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 1_048_576,
                max_component_bytes: 262_144,
                context_window: ContextWindow::NotApplicable,
                reserved_output_tokens: None,
            },
            FrameCapabilities::NONE,
        );
        let frames = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                declaration: TargetDeclaration::full(
                    TargetIdentity::new(NonZeroU128::new(identity).unwrap()),
                    TargetEpoch::new(NonZeroU64::new(1).unwrap()),
                    profile,
                ),
                scripts: [
                    action("same-call", amount),
                    action("follow-up", amount * 10),
                    vec![ProviderFact::ReactionCompleted { primary_text: None }],
                ]
                .into(),
                frames: Arc::clone(&frames),
            },
            frames,
        )
    }
}

#[async_trait]
impl ReactionPort for ScriptedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        self.frames.lock().unwrap().push(CapturedFrame {
            revision: frame.revision(),
            document: combine(
                frame
                    .submission()
                    .projection()
                    .items()
                    .iter()
                    .filter_map(|item| match item {
                        CanonicalInputItem::Instruction { pom, .. }
                        | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
                        _ => None,
                    }),
            ),
            staged: frame.submission().staged_inputs().to_vec(),
            tools: frame.submission().tools().definitions().to_vec(),
        });
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let facts = self.scripts.pop_front().expect("one script per reaction");
        Ok(Box::pin(futures::stream::iter(facts.into_iter().map(Ok))))
    }
}

fn action(call_id: &str, amount: i64) -> Vec<ProviderFact> {
    vec![
        ProviderFact::ToolCall {
            output: ProviderOutputKey::new(7),
            ordinal: 1,
            call: ProviderToolCall::new(
                call_id,
                "adjust",
                serde_json::json!({ "amount": amount }).to_string(),
            )
            .unwrap(),
        },
        ProviderFact::ReactionCompleted { primary_text: None },
    ]
}

#[derive(Default)]
struct Store {
    value: i64,
    started: Vec<EventContext>,
    receipts: Vec<Receipt>,
    completed_reactions: Vec<EventContext>,
}

struct Receipt {
    origin: EventContext,
    amount: i64,
    value: i64,
}

#[derive(Default)]
struct Gate {
    started: Notify,
    release: Notify,
    completed: Notify,
}

#[derive(Deserialize, JsonSchema)]
struct Adjustment {
    amount: i64,
}

#[derive(Clone, AgentView, Serialize)]
#[agent_view(kind = "action_result")]
struct BusinessFeedback {
    application_id: String,
    reaction_id: String,
    event_id: String,
    amount: i64,
    value: i64,
}

impl From<&Receipt> for BusinessFeedback {
    fn from(receipt: &Receipt) -> Self {
        Self {
            application_id: receipt.origin.application_id().to_string(),
            reaction_id: receipt.origin.reaction_id().unwrap().to_string(),
            event_id: receipt.origin.event_id().to_string(),
            amount: receipt.amount,
            value: receipt.value,
        }
    }
}

#[derive(Clone, Default, AgentView)]
#[agent_view(kind = "shared_counter")]
struct CounterView {
    value: i64,
    feedback: Vec<BusinessFeedback>,
}

#[component]
fn shared_counter(store: Arc<Mutex<Store>>, gate: Arc<Gate>) -> Component {
    let recipient = use_application_id();
    let snapshot = use_signal(CounterView::default);
    let prepared = snapshot.clone();
    let prepared_store = Arc::clone(&store);
    use_preparation(move || {
        let prepared = prepared.clone();
        let store = Arc::clone(&prepared_store);
        async move {
            let next = {
                let store = store.lock().unwrap();
                CounterView {
                    value: store.value,
                    feedback: store
                        .receipts
                        .iter()
                        .filter(|receipt| receipt.origin.application_id() == recipient)
                        .map(BusinessFeedback::from)
                        .collect(),
                }
            };
            prepared.set(next)
        }
    });
    let completion_store = Arc::clone(&store);
    use_reaction_completion(move |context: EventContext| {
        completion_store
            .lock()
            .unwrap()
            .completed_reactions
            .push(context);
        std::future::ready(Ok::<_, Infallible>(()))
    });
    let current = snapshot.with(Clone::clone).unwrap();
    view! {
        #[user(repeat)]
        { current }
        #[user(repeat)]
        next_action { "Inspect your action results, then call adjust to change the shared counter." }
        NativeToolCall {
            name: "adjust",
            description: "Add amount to the shared counter and return the actual committed value.",
            on_call: move |context: EventContext, input: Adjustment| {
                assert_eq!(context.application_id(), recipient);
                let delayed = matches!(
                    context.source(),
                    EventSource::NativeTool { call_id, .. } if call_id == "same-call"
                );
                store.lock().unwrap().started.push(context.clone());
                let store = Arc::clone(&store);
                let gate = Arc::clone(&gate);
                if delayed {
                    gate.started.notify_one();
                }
                async move {
                    if delayed {
                        gate.release.notified().await;
                    }
                    let feedback = {
                        let mut store = store.lock().unwrap();
                        store.value += input.amount;
                        let receipt = Receipt {
                            origin: context,
                            amount: input.amount,
                            value: store.value,
                        };
                        let feedback = BusinessFeedback::from(&receipt);
                        store.receipts.push(receipt);
                        feedback
                    };
                    if delayed {
                        gate.completed.notify_one();
                    }
                    Ok::<_, Infallible>(feedback)
                }
            },
        }
    }
}

fn assert_origin(context: &EventContext, application: ApplicationId, frame: FrameRevision) {
    assert_eq!(context.application_id(), application);
    assert_eq!(context.frame_revision(), Some(frame));
    assert!(context.reaction_id().is_some());
    assert_eq!(
        context.source(),
        &EventSource::NativeTool {
            call_id: "same-call".to_owned(),
            output: Some(ProviderOutputKey::new(7)),
            ordinal: Some(1),
        }
    );
}

fn assert_feedback(frame: &CapturedFrame, origins: &[EventContext], recipient: ApplicationId) {
    let feedback = frame.document.elements("action_result");
    assert_eq!(feedback.len(), origins.len());
    for origin in origins {
        assert!(feedback.iter().any(|result| {
            pom_support::attribute(result, "application_id") == Some(recipient.to_string().as_str())
                && pom_support::attribute(result, "reaction_id")
                    == Some(origin.reaction_id().unwrap().to_string().as_str())
                && pom_support::attribute(result, "event_id")
                    == Some(origin.event_id().to_string().as_str())
        }));
    }
    assert!(frame.document.has_element("next_action"));
}

#[tokio::test]
async fn parallel_applications_keep_async_feedback_with_its_origin_and_recipient() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let store = Arc::new(Mutex::new(Store::default()));
        let first_gate = Arc::new(Gate::default());
        let second_gate = Arc::new(Gate::default());
        let (first_port, first_frames) = ScriptedPort::new(90_001, 1);
        let (second_port, second_frames) = ScriptedPort::new(90_002, 2);
        let first_store = Arc::clone(&store);
        let first_root_gate = Arc::clone(&first_gate);
        let mut first = Application::mount(
            move || shared_counter(Arc::clone(&first_store), Arc::clone(&first_root_gate)),
            first_port,
        )
        .unwrap();
        let second_store = Arc::clone(&store);
        let second_root_gate = Arc::clone(&second_gate);
        let mut second = Application::mount(
            move || shared_counter(Arc::clone(&second_store), Arc::clone(&second_root_gate)),
            second_port,
        )
        .unwrap();
        let first_id = first.application_id();
        let second_id = second.application_id();
        assert_ne!(first_id, second_id);

        let release_in_reverse = async {
            tokio::join!(
                first_gate.started.notified(),
                second_gate.started.notified()
            );
            {
                let state = store.lock().unwrap();
                assert_eq!(state.started.len(), 2);
                assert!(state.receipts.is_empty());
            }
            second_gate.release.notify_one();
            second_gate.completed.notified().await;
            first_gate.release.notify_one();
        };
        let (first_result, second_result, ()) = tokio::join!(
            first.react().unwrap(),
            second.react().unwrap(),
            release_in_reverse
        );
        assert!(first_result.unwrap().is_continue());
        assert!(second_result.unwrap().is_continue());

        let (first_origin, second_origin) = {
            let state = store.lock().unwrap();
            assert_eq!(state.value, 3);
            assert_eq!(state.receipts[0].origin.application_id(), second_id);
            assert_eq!(state.receipts[1].origin.application_id(), first_id);
            let first_origin = state
                .started
                .iter()
                .find(|ctx| ctx.application_id() == first_id)
                .unwrap();
            let second_origin = state
                .started
                .iter()
                .find(|ctx| ctx.application_id() == second_id)
                .unwrap();
            assert_origin(
                first_origin,
                first_id,
                first_frames.lock().unwrap()[0].revision,
            );
            assert_origin(
                second_origin,
                second_id,
                second_frames.lock().unwrap()[0].revision,
            );
            assert_ne!(first_origin.event_id(), second_origin.event_id());
            assert_ne!(first_origin.reaction_id(), second_origin.reaction_id());
            for receipt in &state.receipts {
                assert!(state.started.contains(&receipt.origin));
                let completed = state
                    .completed_reactions
                    .iter()
                    .find(|ctx| ctx.application_id() == receipt.origin.application_id())
                    .unwrap();
                assert_eq!(completed.reaction_id(), receipt.origin.reaction_id());
                assert_eq!(completed.frame_revision(), receipt.origin.frame_revision());
                assert_ne!(completed.event_id(), receipt.origin.event_id());
                assert_eq!(completed.source(), &EventSource::ReactionCompleted);
            }
            (first_origin.clone(), second_origin.clone())
        };

        let (first_result, second_result) =
            tokio::join!(first.react().unwrap(), second.react().unwrap());
        assert!(first_result.unwrap().is_continue());
        assert!(second_result.unwrap().is_continue());
        for (frames, recipient, origin) in [
            (&first_frames, first_id, &first_origin),
            (&second_frames, second_id, &second_origin),
        ] {
            let frames = frames.lock().unwrap();
            assert_feedback(&frames[1], std::slice::from_ref(origin), recipient);
            let result = frames[1]
                .staged
                .iter()
                .find_map(|item| match item {
                    CanonicalInputItem::ToolResult { call_id, content }
                        if call_id == "same-call" =>
                    {
                        Some(serde_json::from_str::<serde_json::Value>(content).unwrap())
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(result["application_id"], recipient.to_string());
            assert_eq!(result["event_id"], origin.event_id().to_string());
            let schema = frames[0]
                .tools
                .iter()
                .find(|tool| tool.name() == "adjust")
                .unwrap()
                .input_schema();
            assert_eq!(
                schema["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .collect::<Vec<_>>(),
                vec!["amount"]
            );
        }

        let (first_result, second_result) =
            tokio::join!(first.react().unwrap(), second.react().unwrap());
        assert!(first_result.unwrap().is_continue());
        assert!(second_result.unwrap().is_continue());
        {
            let state = store.lock().unwrap();
            assert_eq!(state.value, 33);
            assert_eq!(state.receipts.len(), 4);
            for (frames, recipient) in [(&first_frames, first_id), (&second_frames, second_id)] {
                let origins = state
                    .receipts
                    .iter()
                    .filter(|receipt| receipt.origin.application_id() == recipient)
                    .map(|receipt| receipt.origin.clone())
                    .collect::<Vec<_>>();
                assert_eq!(origins.len(), 2);
                assert_ne!(origins[0].reaction_id(), origins[1].reaction_id());
                assert_ne!(origins[0].event_id(), origins[1].event_id());
                let frames = frames.lock().unwrap();
                assert_feedback(&frames[2], &origins, recipient);
                assert!(frames[2].document.has_attribute("value", "33"));
            }
        }
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
    })
    .await
    .expect("both applications make progress while their tool futures overlap");
}
