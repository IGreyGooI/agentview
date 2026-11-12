#[path = "support/pom.rs"]
mod pom_support;

use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use agentview::{
    component::{
        execution::{
            Application, ContextWindow, DefinitionUpdateErrorKind, Frame, FrameCapabilities,
            FrameConstraints, FrameProfile, FrameRevision, ProviderDecision, ProviderFact,
            ProviderFactStream, ProviderOutputKey, ProviderToolCall, ReactionPort,
            ReactionPortFault, SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
            ToolDefinition,
        },
        prelude::*,
    },
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::StreamExt;
use pom_support::{combine, projection_document, InspectPom};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Notify;

#[derive(Clone)]
struct CapturedFrame {
    revision: FrameRevision,
    projection: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    tools: Vec<String>,
    tool_definitions: Vec<ToolDefinition>,
    questions: Vec<String>,
}

impl CapturedFrame {
    fn view(&self) -> ResolvedDocument {
        combine(self.projection.iter().filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
            _ => None,
        }))
    }
}

#[derive(Clone, Default)]
struct Capture {
    frames: Arc<Mutex<Vec<CapturedFrame>>>,
    submitted: Arc<Notify>,
}

impl Capture {
    fn frames(&self) -> Vec<CapturedFrame> {
        self.frames.lock().unwrap().clone()
    }
}

enum Reply {
    Complete,
    Tool {
        id: &'static str,
        amount: i64,
    },
    Decision(f64),
    GatedTool {
        release: Arc<Notify>,
        id: &'static str,
        amount: i64,
    },
}

struct ScriptedPort {
    declaration: TargetDeclaration,
    replies: VecDeque<Reply>,
    capture: Capture,
}

impl ScriptedPort {
    fn new(replies: impl IntoIterator<Item = Reply>) -> (Self, Capture) {
        Self::with_capabilities(
            replies,
            FrameCapabilities::new(true)
                .with_native_tool_calls(true)
                .with_text_delta_output(true)
                .with_decision(true),
        )
    }

    fn with_capabilities(
        replies: impl IntoIterator<Item = Reply>,
        capabilities: FrameCapabilities,
    ) -> (Self, Capture) {
        let capture = Capture::default();
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 1_048_576,
                max_component_bytes: 262_144,
                context_window: ContextWindow::NotApplicable,
                reserved_output_tokens: None,
            },
            capabilities,
        );
        (
            Self {
                declaration: TargetDeclaration::full(
                    TargetIdentity::new(NonZeroU128::new(817_400).unwrap()),
                    TargetEpoch::new(NonZeroU64::MIN),
                    profile,
                ),
                replies: replies.into_iter().collect(),
                capture: capture.clone(),
            },
            capture,
        )
    }
}

fn tool_fact(id: &str, amount: i64) -> ProviderFact {
    ProviderFact::ToolCall {
        output: ProviderOutputKey::new(0),
        ordinal: 0,
        call: ProviderToolCall::new(
            id,
            "add",
            serde_json::json!({ "amount": amount }).to_string(),
        )
        .unwrap(),
    }
}

#[async_trait]
impl ReactionPort for ScriptedPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        let batch = frame
            .submission()
            .decision()
            .map(|decision| decision.batch_id().clone());
        self.capture.frames.lock().unwrap().push(CapturedFrame {
            revision: frame.revision(),
            projection: frame.submission().projection().items().to_vec(),
            staged: frame.submission().staged_inputs().to_vec(),
            tools: frame.submission().tools().names().to_vec(),
            tool_definitions: frame.submission().tools().definitions().to_vec(),
            questions: frame
                .submission()
                .decision()
                .map(|decision| decision.questions().keys().cloned().collect())
                .unwrap_or_default(),
        });
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        self.capture.submitted.notify_one();
        let reply = self.replies.pop_front().expect("one reply per reaction");
        Ok(Box::pin(
            futures::stream::once(async move {
                let fact = match reply {
                    Reply::Complete => {
                        return None;
                    }
                    Reply::Tool { id, amount } => tool_fact(id, amount),
                    Reply::GatedTool {
                        release,
                        id,
                        amount,
                    } => {
                        release.notified().await;
                        tool_fact(id, amount)
                    }
                    Reply::Decision(value) => ProviderFact::Decision {
                        output: ProviderOutputKey::new(0),
                        decision: ProviderDecision::json_envelope(
                            batch.expect("Decision was declared"),
                            serde_json::json!({
                                "answers": { "approve": { "type": "noul", "noul": value } }
                            })
                            .to_string(),
                        ),
                    },
                };
                Some(Ok(fact))
            })
            .filter_map(futures::future::ready)
            .chain(futures::stream::once(async {
                Ok(ProviderFact::ReactionCompleted { primary_text: None })
            })),
        ))
    }
}

async fn notified(notification: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), notification.notified())
        .await
        .expect("lifecycle event must arrive");
}

fn field(name: &str, value: impl ToString) -> Component {
    Component::document(Document::from_xml(
        XmlNode::try_build(name, |children| {
            children.text(TextNode::new(value.to_string()));
            Ok(())
        })
        .unwrap(),
    ))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddInput {
    amount: i64,
}

#[derive(Clone, Default)]
struct CounterProbe {
    state: Arc<Mutex<Option<Signal<i64>>>>,
    preparations: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<(i64, EventContext)>>>,
}

fn counter_definition(
    compatibility: &str,
    multiplier: i64,
    probe: CounterProbe,
) -> ComponentDefinition {
    ComponentDefinition::new("dsl-counter", compatibility, move |hooks| {
        let total = hooks.use_signal_at(0, || 0_i64)?;
        let ready = hooks.use_signal_at(1, || false)?;
        *probe.state.lock().unwrap() = Some(total.clone());
        let prepared = probe.preparations.clone();
        let prepared_ready = ready.clone();
        hooks.use_preparation_at(2, move || async move {
            prepared.fetch_add(1, Ordering::AcqRel);
            prepared_ready.set(true)
        })?;
        let current = total.with(|value| *value).unwrap();
        let is_ready = ready.with(|value| *value).unwrap();
        let events = probe.events.clone();
        Ok(Component::fragment([
            field("total", current),
            field("ready", is_ready),
            NativeToolCall::props()
                .name(String::from("add"))
                .description("Add the supplied amount and inspect the updated total.")
                .on_call(move |context: EventContext, input: AddInput| {
                    events.lock().unwrap().push((multiplier, context));
                    total.update(|value| {
                        *value += input.amount * multiplier;
                        *value
                    })
                })
                .build(),
        ]))
    })
    .unwrap()
}

#[tokio::test]
async fn external_definition_observes_acts_and_delivers_feedback_across_replacement() {
    let (port, capture) = ScriptedPort::new([
        Reply::Tool {
            id: "first",
            amount: 2,
        },
        Reply::Tool {
            id: "second",
            amount: 4,
        },
        Reply::Complete,
    ]);
    let probe = CounterProbe::default();
    let mut app =
        Application::mount_definition(counter_definition("1", 1, probe.clone()), port).unwrap();
    assert_eq!(app.definition_version().get(), 1);
    assert!(!app.current_projection().is_prepared());
    app.react(false).unwrap().await.unwrap();
    assert!(projection_document(app.current_projection().projection()).has_field("total", "2"));

    let update = app
        .update_definition(counter_definition("1", 3, probe.clone()))
        .await
        .unwrap();
    assert_eq!(update.effective_version().get(), 2);
    assert_eq!(app.definition_version().get(), 2);
    assert!(projection_document(app.current_projection().projection()).has_field("total", "2"));
    app.react(false).unwrap().await.unwrap();
    app.react(false).unwrap().await.unwrap();

    let frames = capture.frames();
    assert_eq!(frames.len(), 3);
    assert!(frames[0].view().has_field("total", "0"));
    assert!(frames[0].view().has_field("ready", "true"));
    assert!(frames[1].view().has_field("total", "2"));
    assert!(frames[2].view().has_field("total", "14"));
    assert!(frames.iter().all(|frame| frame.tools == ["add"]));
    assert_eq!(
        frames[1].staged,
        [CanonicalInputItem::tool_result("first", "2").unwrap()]
    );
    assert_eq!(
        frames[2].staged,
        [CanonicalInputItem::tool_result("second", "14").unwrap()]
    );
    assert_eq!(probe.preparations.load(Ordering::Acquire), 3);
    let events = probe.events.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .map(|(version, _)| *version)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    for ((_, context), frame) in events.iter().zip(&frames) {
        assert_eq!(context.application_id(), app.application_id());
        assert_eq!(context.frame_revision(), Some(frame.revision));
    }
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn external_decision_callback_updates_the_next_declared_observation() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let recorded = outcomes.clone();
    let definition = ComponentDefinition::new("dsl-judgment", "1", move |hooks| {
        let outcome = hooks.use_signal_at(0, || None::<f64>)?;
        let current = outcome.with(|value| *value).unwrap();
        let question = if current.is_none() {
            let recorded = recorded.clone();
            Decision::new(
                String::from("approve"),
                Noul::new("Is the displayed request approved?"),
            )
            .on_result(move |context, answer: QuestionOutcome<NoulAnswer>| {
                let value = answer.result.unwrap().noul;
                recorded.lock().unwrap().push(context);
                outcome.set(Some(value))
            })
            .build()
        } else {
            Component::fragment([])
        };
        Ok(Component::fragment([
            field("request", "approved"),
            field(
                "judgment",
                current
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "pending".into()),
            ),
            question,
        ]))
    })
    .unwrap();
    let (port, capture) = ScriptedPort::new([Reply::Decision(0.9), Reply::Complete]);
    let mut app = Application::mount_definition(definition, port).unwrap();
    app.react(false).unwrap().await.unwrap();
    app.react(false).unwrap().await.unwrap();
    let frames = capture.frames();
    assert_eq!(frames[0].questions, ["approve"]);
    assert!(frames[1].questions.is_empty());
    assert!(frames[0].view().has_field("judgment", "pending"));
    assert!(frames[1].view().has_field("judgment", "0.9"));
    assert_eq!(
        outcomes.lock().unwrap()[0].frame_revision(),
        Some(frames[0].revision)
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn external_tool_schema_and_callback_use_the_native_frame_protocol() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "amount": { "type": "integer", "minimum": 1 } },
        "required": ["amount"],
        "additionalProperties": false,
    });
    let tool = ToolDefinition::new(
        ["a", "dd"].concat(),
        String::from("Add an amount of at least one, then inspect the resulting total."),
        schema,
    )
    .unwrap()
    .with_strict(true);
    let declared_tool = tool.clone();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let recorded = contexts.clone();
    let definition = ComponentDefinition::new("dsl-schema-tool", "1", move |hooks| {
        let total = hooks.use_signal_at(0, || 0_i64)?;
        let recorded = recorded.clone();
        let current = total.with(|value| *value).unwrap();
        Ok(Component::fragment([
            field("total", current),
            NativeToolCall::from_definition(tool.clone()).on_call(move |context, call| {
                let input = serde_json::from_str::<AddInput>(call.raw_arguments()).unwrap();
                let total = total
                    .update(|value| {
                        *value += input.amount;
                        *value
                    })
                    .unwrap();
                recorded.lock().unwrap().push(context);
                async move {
                    Ok::<_, Infallible>(
                        call.output(serde_json::json!({ "total": total }).to_string()),
                    )
                }
            }),
        ]))
    })
    .unwrap();
    let (port, capture) = ScriptedPort::new([
        Reply::Tool {
            id: "schema_call",
            amount: 3,
        },
        Reply::Complete,
    ]);
    let mut app = Application::mount_definition(definition, port).unwrap();
    app.react(false).unwrap().await.unwrap();
    app.react(false).unwrap().await.unwrap();
    let frames = capture.frames();
    assert_eq!(frames[0].tool_definitions, [declared_tool]);
    assert!(frames[1].view().has_field("total", "3"));
    assert_eq!(
        frames[1].staged,
        [CanonicalInputItem::tool_result("schema_call", r#"{"total":3}"#).unwrap()]
    );
    assert_eq!(
        contexts.lock().unwrap()[0].frame_revision(),
        Some(frames[0].revision)
    );
    app.shutdown().await.unwrap();
}

type ChildStates = Arc<Mutex<BTreeMap<String, Signal<i64>>>>;

fn collection_definition(keys: &[(&str, i64)], states: ChildStates) -> ComponentDefinition {
    let children = keys
        .iter()
        .map(|(key, initial)| {
            let key = key.to_string();
            let initial = *initial;
            let states = states.clone();
            let label = key.clone();
            let definition = ComponentDefinition::new("dsl-item", "1", move |hooks| {
                let value = hooks.use_signal_at(0, || initial)?;
                states.lock().unwrap().insert(label.clone(), value.clone());
                Ok(field(&label, value.with(|value| *value).unwrap()))
            })
            .unwrap();
            (key, definition)
        })
        .collect::<Vec<_>>();
    ComponentDefinition::new("dsl-list", "1", move |_hooks| {
        Ok(Component::fragment(children.iter().map(
            |(key, definition)| definition.instantiate(key.clone()),
        )))
    })
    .unwrap()
}

#[tokio::test]
async fn explicit_instance_keys_preserve_state_across_insertion_and_reordering() {
    let states = ChildStates::default();
    let (port, _) = ScriptedPort::new([]);
    let mut app = Application::mount_definition(
        collection_definition(&[("a", 1), ("b", 2)], states.clone()),
        port,
    )
    .unwrap();
    let original = states.lock().unwrap().clone();
    original["a"].set(41).unwrap();
    original["b"].set(72).unwrap();
    app.update_definition(collection_definition(
        &[("b", 200), ("c", 3), ("a", 100)],
        states.clone(),
    ))
    .await
    .unwrap();
    let view = projection_document(app.current_projection().projection());
    assert!(view.has_field("a", "41"));
    assert!(view.has_field("b", "72"));
    assert!(view.has_field("c", "3"));
    original["a"].set(42).unwrap();
    assert!(app.prepare().await.unwrap().is_continue());
    assert!(projection_document(app.current_projection().projection()).has_field("a", "42"));
    app.update_definition(collection_definition(&[("c", 3)], states.clone()))
        .await
        .unwrap();
    assert!(original["a"].set(0).is_err());
    assert!(original["b"].set(0).is_err());
    assert_eq!(states.lock().unwrap()["c"].with(|value| *value).unwrap(), 3);
    app.shutdown().await.unwrap();
}

struct TaskDrop(Arc<AtomicUsize>);

impl Drop for TaskDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Default)]
struct TaskProbe {
    state: Arc<Mutex<Option<Signal<i64>>>>,
    demand: Arc<Mutex<Option<ReactionRequest>>>,
    starts: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    started: Arc<Notify>,
}

fn task_definition(compatibility: &str, initial: i64, probe: TaskProbe) -> ComponentDefinition {
    ComponentDefinition::new("dsl-worker", compatibility, move |hooks| {
        let value = hooks.use_signal_at(0, || initial)?;
        let demand = hooks.use_reaction_request_at(1)?;
        *probe.state.lock().unwrap() = Some(value.clone());
        *probe.demand.lock().unwrap() = Some(demand);
        let task_probe = probe.clone();
        hooks.use_future_at(2, move || async move {
            let _drop = TaskDrop(task_probe.drops.clone());
            task_probe.starts.fetch_add(1, Ordering::AcqRel);
            task_probe.started.notify_one();
            std::future::pending::<()>().await;
        })?;
        Ok(field("worker", value.with(|value| *value).unwrap()))
    })
    .unwrap()
}

#[tokio::test]
async fn compatible_tasks_continue_and_incompatible_instances_retire_before_publication() {
    let probe = TaskProbe::default();
    let (port, capture) = ScriptedPort::new([]);
    let mut app =
        Application::mount_definition(task_definition("1", 1, probe.clone()), port).unwrap();
    notified(&probe.started).await;
    let original_state = probe.state.lock().unwrap().clone().unwrap();
    let original_demand = probe.demand.lock().unwrap().clone().unwrap();
    original_state.set(7).unwrap();
    app.update_definition(task_definition("1", 100, probe.clone()))
        .await
        .unwrap();
    assert_eq!(probe.starts.load(Ordering::Acquire), 1);
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    assert!(projection_document(app.current_projection().projection()).has_field("worker", "7"));
    original_demand.request().unwrap();
    assert!(app.take_reaction_request().unwrap());
    assert!(!app.take_reaction_request().unwrap());
    assert!(
        capture.frames().is_empty(),
        "demand never submits a Frame itself"
    );

    app.update_definition(task_definition("2", 100, probe.clone()))
        .await
        .unwrap();
    assert_eq!(
        probe.drops.load(Ordering::Acquire),
        1,
        "retirement waits for the old task destructor"
    );
    assert!(original_state.set(99).is_err());
    assert_eq!(
        original_demand.request(),
        Err(ReactionRequestError::StaleMount)
    );
    assert!(projection_document(app.current_projection().projection()).has_field("worker", "100"));
    notified(&probe.started).await;
    assert_eq!(probe.starts.load(Ordering::Acquire), 2);
    app.shutdown().await.unwrap();
    assert_eq!(probe.drops.load(Ordering::Acquire), 2);
}

#[derive(Clone, Default)]
struct CoroutineProbe {
    sender: Arc<Mutex<Option<Coroutine<i64>>>>,
    starts: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

fn coroutine_definition(
    compatibility: &str,
    multiplier: i64,
    probe: CoroutineProbe,
) -> ComponentDefinition {
    ComponentDefinition::new("dsl-service", compatibility, move |hooks| {
        let value = hooks.use_signal_at(0, || 0_i64)?;
        let demand = hooks.use_reaction_request_at(1)?;
        let task_value = value.clone();
        let task_probe = probe.clone();
        let sender =
            hooks.use_coroutine_at(2, 1, move |mut inbox: CoroutineInbox<i64>| async move {
                task_probe.starts.fetch_add(1, Ordering::AcqRel);
                while let Some(amount) = inbox.recv().await {
                    task_value
                        .update(|value| *value += multiplier * amount)
                        .unwrap();
                    demand.request().unwrap();
                    task_probe.handled.notify_one();
                }
            })?;
        *probe.sender.lock().unwrap() = Some(sender);
        Ok(field("service_total", value.with(|value| *value).unwrap()))
    })
    .unwrap()
}

#[tokio::test]
async fn coroutine_type_checks_reject_updates_and_retained_services_keep_native_wake_semantics() {
    let probe = CoroutineProbe::default();
    let (port, capture) = ScriptedPort::new([]);
    let mut app =
        Application::mount_definition(coroutine_definition("1", 1, probe.clone()), port).unwrap();
    let original_sender = probe.sender.lock().unwrap().clone().unwrap();
    original_sender.send(2).await.unwrap();
    notified(&probe.handled).await;
    assert!(app.take_reaction_request().unwrap());
    assert!(!app.take_reaction_request().unwrap());
    assert!(capture.frames().is_empty());
    assert!(app.prepare().await.unwrap().is_continue());
    assert!(
        projection_document(app.current_projection().projection()).has_field("service_total", "2")
    );

    let wrong_message_type = ComponentDefinition::new("dsl-service", "1", |hooks| {
        let _ = hooks.use_signal_at(0, || 0_i64)?;
        let _ = hooks.use_reaction_request_at(1)?;
        let _ = hooks.use_coroutine_at(2, 1, |mut inbox: CoroutineInbox<String>| async move {
            while inbox.recv().await.is_some() {}
        })?;
        Ok(Component::text("invalid message ABI"))
    })
    .unwrap();
    let error = app.update_definition(wrong_message_type).await.unwrap_err();
    assert_eq!(error.kind(), DefinitionUpdateErrorKind::Rejected);
    assert_eq!(error.effective_version().get(), 1);

    app.update_definition(coroutine_definition("1", 10, probe.clone()))
        .await
        .unwrap();
    original_sender.send(3).await.unwrap();
    notified(&probe.handled).await;
    assert_eq!(probe.starts.load(Ordering::Acquire), 1);
    assert!(app.prepare().await.unwrap().is_continue());
    assert!(
        projection_document(app.current_projection().projection()).has_field("service_total", "5")
    );
    assert!(
        capture.frames().is_empty(),
        "task completion and explicit demand do not submit"
    );

    app.update_definition(coroutine_definition("2", 10, probe.clone()))
        .await
        .unwrap();
    let stale = original_sender.send(7).await.unwrap_err();
    assert_eq!(stale.into_inner(), 7);
    let new_sender = probe.sender.lock().unwrap().clone().unwrap();
    new_sender.send(3).await.unwrap();
    notified(&probe.handled).await;
    assert!(app.prepare().await.unwrap().is_continue());
    assert!(
        projection_document(app.current_projection().projection()).has_field("service_total", "30")
    );
    assert_eq!(probe.starts.load(Ordering::Acquire), 2);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_candidates_keep_the_effective_definition_and_its_state() {
    let probe = CounterProbe::default();
    let (port, capture) = ScriptedPort::new([Reply::Tool {
        id: "retained",
        amount: 1,
    }]);
    let mut app =
        Application::mount_definition(counter_definition("1", 2, probe.clone()), port).unwrap();
    let state = probe.state.lock().unwrap().clone().unwrap();
    let original = app.current_projection().projection().clone();
    let render_error = ComponentDefinition::new("dsl-counter", "1", |_hooks| {
        Err(ComponentDefinitionError::new("DSL failed to lower"))
    })
    .unwrap();
    let wrong_type = ComponentDefinition::new("dsl-counter", "1", |hooks| {
        let _ = hooks.use_signal_at(0, String::new)?;
        Ok(Component::text("invalid"))
    })
    .unwrap();
    let removed_hooks =
        ComponentDefinition::new("dsl-counter", "1", |_hooks| Ok(Component::text("invalid")))
            .unwrap();
    let duplicate_children = ComponentDefinition::new("dsl-list", "1", |_hooks| {
        let child = ComponentDefinition::new("item", "1", |_hooks| Ok(Component::text("item")))?;
        Ok(Component::fragment([
            child.instantiate("same"),
            child.instantiate("same"),
        ]))
    })
    .unwrap();
    let invalid_decisions = ComponentDefinition::new("dsl-questions", "1", |_hooks| {
        let question = || {
            Decision::new("duplicate", Noul::new("Continue?"))
                .on_result(|_context, _answer: QuestionOutcome<_>| Ok::<(), Infallible>(()))
                .build()
        };
        Ok(Component::fragment([question(), question()]))
    })
    .unwrap();
    let swallowed_hook_error = ComponentDefinition::new("dsl-invalid-capacity", "1", |hooks| {
        let ignored = hooks.use_coroutine_at(0, 0, |_inbox: CoroutineInbox<()>| async {});
        assert!(ignored.is_err());
        Ok(Component::text("invalid hook error was ignored"))
    })
    .unwrap();
    let duplicate_compatibility = ComponentDefinition::new("dsl-list", "1", |_hooks| {
        let first = ComponentDefinition::new("item", "1", |_hooks| Ok(Component::text("first")))?;
        let second = ComponentDefinition::new("item", "2", |_hooks| Ok(Component::text("second")))?;
        Ok(Component::fragment([
            first.instantiate("same"),
            second.instantiate("same"),
        ]))
    })
    .unwrap();
    let duplicate_tools = ComponentDefinition::new("dsl-tools", "1", |_hooks| {
        let tool = || {
            NativeToolCall::props()
                .name(String::from("duplicate"))
                .on_call(|_context: EventContext| Ok::<_, Infallible>(()))
                .build()
        };
        Ok(Component::fragment([tool(), tool()]))
    })
    .unwrap();
    let conflicting_output_modes = ComponentDefinition::new("dsl-output-conflict", "1", |_hooks| {
        Ok(Component::fragment([
            NativeToolCall::props()
                .name(String::from("add"))
                .on_call(|_context: EventContext| Ok::<_, Infallible>(()))
                .build(),
            Decision::new("approve", Noul::new("Continue?"))
                .on_result(|_context, _answer: QuestionOutcome<NoulAnswer>| Ok::<_, Infallible>(()))
                .build(),
        ]))
    })
    .unwrap();
    for candidate in [
        render_error,
        wrong_type,
        removed_hooks,
        duplicate_children,
        invalid_decisions,
        swallowed_hook_error,
        duplicate_compatibility,
        duplicate_tools,
        conflicting_output_modes,
    ] {
        let error = app
            .update_definition(candidate)
            .await
            .expect_err("invalid candidate must be rejected");
        assert_eq!(error.kind(), DefinitionUpdateErrorKind::Rejected);
        assert_eq!(error.effective_version().get(), 1);
        assert_eq!(app.definition_version().get(), 1);
        assert_eq!(app.current_projection().projection(), &original);
        assert_eq!(state.with(|value| *value).unwrap(), 0);
    }
    assert!(capture.frames().is_empty(), "validation never submits");
    app.react(false).unwrap().await.unwrap();
    assert!(projection_document(app.current_projection().projection()).has_field("total", "2"));
    assert_eq!(
        probe.events.lock().unwrap()[0].0,
        2,
        "old callback remains installed"
    );
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_frame_capabilities_reject_updates_before_retiring_old_instances() {
    let probe = CounterProbe::default();
    let (port, capture) =
        ScriptedPort::with_capabilities([], FrameCapabilities::NONE.with_native_tool_calls(true));
    let mut app =
        Application::mount_definition(counter_definition("1", 1, probe.clone()), port).unwrap();
    let state = probe.state.lock().unwrap().clone().unwrap();
    let original = app.current_projection().projection().clone();
    let candidate = ComponentDefinition::new("dsl-decision", "1", |_hooks| {
        Ok(
            Decision::new("approve", Noul::new("May the task continue?"))
                .on_result(|_context, _answer: QuestionOutcome<NoulAnswer>| Ok::<_, Infallible>(()))
                .build(),
        )
    })
    .unwrap();
    let error = app.update_definition(candidate).await.unwrap_err();
    assert_eq!(error.kind(), DefinitionUpdateErrorKind::Rejected);
    assert_eq!(error.effective_version().get(), 1);
    assert_eq!(app.current_projection().projection(), &original);
    state.set(9).unwrap();
    assert!(capture.frames().is_empty());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn submitted_frames_keep_their_callbacks_and_context_until_the_idle_update_boundary() {
    let release = Arc::new(Notify::new());
    let (port, capture) = ScriptedPort::new([
        Reply::GatedTool {
            release: release.clone(),
            id: "old",
            amount: 2,
        },
        Reply::Tool {
            id: "new",
            amount: 2,
        },
    ]);
    let probe = CounterProbe::default();
    let mut app =
        Application::mount_definition(counter_definition("1", 1, probe.clone()), port).unwrap();
    let first = app.react(false).unwrap();
    let first_id = first.id();
    notified(&capture.submitted).await;
    let error = app
        .update_definition(counter_definition("1", 10, probe.clone()))
        .await
        .expect_err("active Frame has not reached the update boundary");
    assert_eq!(error.kind(), DefinitionUpdateErrorKind::Busy);
    assert_eq!(error.effective_version().get(), 1);
    release.notify_one();
    first.await.unwrap();
    app.update_definition(counter_definition("1", 10, probe.clone()))
        .await
        .unwrap();
    let second = app.react(false).unwrap();
    let second_id = second.id();
    second.await.unwrap();
    let frames = capture.frames();
    let events = probe.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, 1);
    assert_eq!(events[1].0, 10);
    assert_eq!(events[0].1.reaction_id(), Some(first_id));
    assert_eq!(events[1].1.reaction_id(), Some(second_id));
    assert_eq!(events[0].1.frame_revision(), Some(frames[0].revision));
    assert_eq!(events[1].1.frame_revision(), Some(frames[1].revision));
    assert!(projection_document(app.current_projection().projection()).has_field("total", "22"));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn replaced_callback_captures_are_released_after_the_submitted_frame_settles() {
    let token = Arc::new(());
    let old_capture = Arc::downgrade(&token);
    let definition = ComponentDefinition::new("dsl-binding-lifetime", "1", move |hooks| {
        let total = hooks.use_signal_at(0, || 0_i64)?;
        let token = token.clone();
        Ok(NativeToolCall::props()
            .name(String::from("add"))
            .on_call(move |_context: EventContext, input: AddInput| {
                let _retained = &token;
                total.update(|value| {
                    *value += input.amount;
                    *value
                })
            })
            .build())
    })
    .unwrap();
    let release = Arc::new(Notify::new());
    let (port, capture) = ScriptedPort::new([Reply::GatedTool {
        release: release.clone(),
        id: "binding",
        amount: 1,
    }]);
    let mut app = Application::mount_definition(definition, port).unwrap();
    let reaction = app.react(false).unwrap();
    notified(&capture.submitted).await;
    assert!(old_capture.upgrade().is_some());
    release.notify_one();
    reaction.await.unwrap();
    let replacement = ComponentDefinition::new("dsl-binding-lifetime", "1", |hooks| {
        let total = hooks.use_signal_at(0, || 0_i64)?;
        Ok(field("retained_total", total.with(|value| *value).unwrap()))
    })
    .unwrap();
    app.update_definition(replacement).await.unwrap();
    assert!(old_capture.upgrade().is_none());
    assert!(
        projection_document(app.current_projection().projection()).has_field("retained_total", "1")
    );
    app.shutdown().await.unwrap();
}
