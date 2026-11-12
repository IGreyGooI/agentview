#[path = "support/pom.rs"]
mod pom_support;

use std::{
    collections::VecDeque,
    convert::Infallible,
    num::{NonZeroU128, NonZeroU64},
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFault, ApplicationFaultCode, ApplicationFaultKind,
            ApplicationFaultReason, ContextWindow, DecisionDeclaration, DecisionFailure, Frame,
            FrameCapabilities, FrameConstraints, FrameProfile, FrameRequirements, FrameRevision,
            NoulAnswer, ProviderFact, ProviderFactStream, ProviderIdentity, ProviderOutputKey,
            QuestionOutcome, ReactionOutcome, ReactionPort, ReactionPortFault, SubmitFault,
            TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        prelude::*,
    },
    provider::{
        async_openai::{
            AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig, OpenAiResponsesCallEvent,
            OpenAiResponsesCallOutcome, OpenAiResponsesObservation,
            OpenAiResponsesObservationError, OpenAiResponsesObserver,
        },
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
        ModelSpec,
    },
    system1::{judge, JudgmentErrorKind},
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use pom_support::{combine, InspectPom};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Clone, Default)]
struct DecisionProbe(Arc<Mutex<Vec<(EventContext, QuestionOutcome<NoulAnswer>)>>>);

impl DecisionProbe {
    fn take(&self) -> Vec<(EventContext, QuestionOutcome<NoulAnswer>)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    fn store(&self, context: EventContext, result: QuestionOutcome<NoulAnswer>) {
        self.0.lock().unwrap().push((context, result));
    }
}

#[component]
fn gate_judgment(output: DecisionProbe) -> Component {
    view! {
        gate_status { "permission granted" }
        Decision {
            on_result: move |context, result: QuestionOutcome<NoulAnswer>| {
                output.store(context, result);
                Ok::<(), Infallible>(())
            },
            question: Noul::new("Does the displayed permission allow opening the gate?")
                .criteria("Permission is granted.", "Permission is absent or denied."),
            name: "open_gate",
        }
    }
}

#[component]
fn gate_judgment_builder(output: DecisionProbe) -> Component {
    view! {
        gate_status { "permission granted" }
        {
            Decision::new("open_gate", Noul::new("Does the permission allow opening the gate?"))
                .on_result(move |context, result| {
                    output.store(context, result);
                    Ok::<(), Infallible>(())
                })
                .build()
        }
    }
}

#[component]
fn asynchronous_gate_judgment(output: DecisionProbe) -> Component {
    view! {
        gate_status { "permission granted" }
        {
            Decision::new("open_gate", Noul::new("May the displayed gate open?"))
                .on_result(move |context, outcome| {
                    let output = output.clone();
                    async move {
                        tokio::task::yield_now().await;
                        output.store(context, outcome);
                        Ok::<(), Infallible>(())
                    }
                })
                .build()
        }
    }
}

#[derive(Clone)]
enum Reply {
    Decision(f64),
    Raw(String),
    Refused,
    Empty,
    Trailing(String),
    Tool { amount: i64 },
    Unauthorized,
}

impl Reply {
    fn response(self) -> Response {
        let events = match self {
            Self::Unauthorized => return StatusCode::UNAUTHORIZED.into_response(),
            Self::Decision(noul) => text_events(
                &json!({"answers":{"open_gate":{"type":"noul","noul":noul}}}).to_string(),
            ),
            Self::Raw(text) => text_events(&text),
            Self::Refused => refusal_events(),
            Self::Empty => vec![json!({
                "type":"response.completed", "sequence_number":1,
                "response":{"id":"resp_decision", "status":"completed", "output":[]},
            })],
            Self::Trailing(text) => {
                let mut events = text_events(&text);
                events.push(json!({
                    "type":"response.output_text.delta", "sequence_number":6,
                    "item_id":"msg_decision", "output_index":0,
                    "content_index":0, "delta":"trailing",
                }));
                events
            }
            Self::Tool { amount } => tool_events(amount),
        };
        let body: String = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
    }
}

fn text_events(text: &str) -> Vec<Value> {
    let message = json!({
        "id":"msg_decision", "type":"message", "status":"completed", "role":"assistant",
        "content":[{"type":"output_text", "text":text, "annotations":[]}],
    });
    vec![
        json!({"type":"response.output_item.added", "sequence_number":1,
        "output_index":0, "item":{
            "id":"msg_decision", "type":"message", "status":"in_progress",
            "role":"assistant", "content":[],
        }}),
        json!({"type":"response.output_text.delta", "sequence_number":2,
            "item_id":"msg_decision", "output_index":0, "content_index":0, "delta":text}),
        json!({"type":"response.output_text.done", "sequence_number":3,
            "item_id":"msg_decision", "output_index":0, "content_index":0, "text":text}),
        json!({"type":"response.output_item.done", "sequence_number":4,
            "output_index":0, "item":message.clone()}),
        json!({"type":"response.completed", "sequence_number":5,
            "response":{"id":"resp_decision", "status":"completed", "output":[message]}}),
    ]
}

fn tool_events(amount: i64) -> Vec<Value> {
    let arguments = json!({"amount": amount}).to_string();
    let item_id = format!("fc_add_{amount}");
    let call_id = format!("call_add_{amount}");
    let call = json!({
        "id":item_id, "type":"function_call", "status":"completed",
        "call_id":call_id, "name":"add", "arguments":arguments,
    });
    vec![
        json!({"type":"response.output_item.added", "sequence_number":1,
        "output_index":0, "item":{
                "id":item_id, "type":"function_call", "status":"in_progress",
                "call_id":call_id, "name":"add", "arguments":"",
        }}),
        json!({"type":"response.function_call_arguments.delta", "sequence_number":2,
            "item_id":item_id, "output_index":0, "delta":arguments}),
        json!({"type":"response.function_call_arguments.done", "sequence_number":3,
            "item_id":item_id, "output_index":0, "arguments":arguments}),
        json!({"type":"response.output_item.done", "sequence_number":4,
            "output_index":0, "item":call.clone()}),
        json!({"type":"response.completed", "sequence_number":5,
            "response":{"id":"resp_tool", "status":"completed", "output":[call]}}),
    ]
}

fn refusal_events() -> Vec<Value> {
    let refusal = "Unable to make this judgment.";
    let message = json!({
        "id":"msg_refusal", "type":"message", "status":"completed", "role":"assistant",
        "content":[{"type":"refusal", "refusal":refusal}],
    });
    vec![
        json!({"type":"response.output_item.added", "sequence_number":1,
        "output_index":0, "item":{
            "id":"msg_refusal", "type":"message", "status":"in_progress",
            "role":"assistant", "content":[],
        }}),
        json!({"type":"response.refusal.delta", "sequence_number":2,
            "item_id":"msg_refusal", "output_index":0, "content_index":0, "delta":refusal}),
        json!({"type":"response.refusal.done", "sequence_number":3,
            "item_id":"msg_refusal", "output_index":0, "content_index":0, "refusal":refusal}),
        json!({"type":"response.output_item.done", "sequence_number":4,
            "output_index":0, "item":message.clone()}),
        json!({"type":"response.completed", "sequence_number":5,
            "response":{"id":"resp_refusal", "status":"completed", "output":[message]}}),
    ]
}

#[derive(Clone)]
struct ServerState {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

struct Server {
    api_base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState {
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
            requests: requests.clone(),
        };
        let app = Router::new()
            .route("/responses", post(handle_request))
            .with_state(state);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            api_base,
            requests,
            task,
        }
    }

    fn responses(&self) -> AsyncOpenAiResponsesProvider {
        let transport = AsyncOpenAiTransportConfig::new(&self.api_base, "test-key").unwrap();
        let identity =
            ProviderIdentity::new("decision-test", CODEX_HTTP_V1_PROFILE, 1, "decision-test")
                .unwrap();
        let options = CodexHttpV1Options::new(
            ModelSpec::new("fast-decision-model", 32_768).unwrap(),
            None,
            None,
            Some("decision-prefix"),
        )
        .unwrap()
        .with_max_output_tokens(512)
        .unwrap();
        AsyncOpenAiResponsesProvider::try_new(transport, identity, CodexHttpV1Encoder::new(options))
            .unwrap()
            .without_parallel_compaction()
    }

    fn port(&self) -> Box<dyn ReactionPort> {
        Box::new(self.responses())
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_request(State(state): State<ServerState>, Json(body): Json<Value>) -> Response {
    state.requests.lock().unwrap().push(body);
    state
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Reply::Unauthorized)
        .response()
}

#[derive(Clone)]
struct FrameRecord {
    revision: FrameRevision,
    decision: Option<DecisionDeclaration>,
    projection: Vec<CanonicalInputItem>,
    staged: Vec<CanonicalInputItem>,
    tool_names: Vec<String>,
}

impl FrameRecord {
    fn document(&self) -> agentview::pom::ResolvedDocument {
        combine(self.projection.iter().filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
            _ => None,
        }))
    }
}

#[derive(Clone, Default)]
struct Frames(Arc<Mutex<Vec<FrameRecord>>>);

impl Frames {
    fn records(&self) -> Vec<FrameRecord> {
        self.0.lock().unwrap().clone()
    }
}

struct RecordingPort {
    inner: Box<dyn ReactionPort>,
    frames: Frames,
}

impl RecordingPort {
    fn new(inner: Box<dyn ReactionPort>) -> (Self, Frames) {
        let frames = Frames::default();
        (
            Self {
                inner,
                frames: frames.clone(),
            },
            frames,
        )
    }
}

#[async_trait]
impl ReactionPort for RecordingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.inner.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.frames.0.lock().unwrap().push(FrameRecord {
            revision: frame.revision(),
            decision: frame.submission().decision().cloned(),
            projection: frame.submission().projection().items().to_vec(),
            staged: frame.submission().staged_inputs().to_vec(),
            tool_names: frame.submission().tools().names().to_vec(),
        });
        self.inner.submit(frame).await
    }

    async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }
}

#[tokio::test]
async fn responses_delivers_one_typed_question_from_the_submitted_frame() {
    let server = Server::start([Reply::Decision(0.8)]).await;
    let (port, frames) = RecordingPort::new(server.port());
    let output = DecisionProbe::default();
    let observed = output.clone();
    let mut app = Application::mount(move || gate_judgment(output.clone()), port).unwrap();
    let application_id = app.application_id();
    let reaction = app.react(false).unwrap();
    let reaction_id = reaction.id();
    assert_eq!(reaction.await.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();

    let outputs = observed.take();
    assert_eq!(outputs.len(), 1);
    let (context, outcome) = &outputs[0];
    assert_eq!(outcome.result, Ok(NoulAnswer { noul: 0.8 }));
    assert_eq!(context.application_id(), application_id);
    assert_eq!(context.reaction_id(), Some(reaction_id));
    assert!(matches!(
        context.source(),
        EventSource::Provider { output: Some(_) }
    ));
    let frames = frames.records();
    assert_eq!(frames.len(), 1);
    assert_eq!(context.frame_revision(), Some(frames[0].revision));
    let declaration = frames[0].decision.as_ref().unwrap();
    assert_eq!(
        declaration.questions().keys().collect::<Vec<_>>(),
        ["open_gate"]
    );
    assert!(frames[0]
        .document()
        .has_field("gate_status", "permission granted"));
    assert!(frames[0].tool_names.is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "fast-decision-model");
    assert_eq!(requests[0]["max_output_tokens"], 512);
    assert_eq!(requests[0]["prompt_cache_key"], "decision-prefix");
    assert_eq!(requests[0]["text"]["format"]["type"], "json_schema");
    assert!(requests[0].get("previous_response_id").is_none());
}

#[tokio::test]
async fn typed_question_builder_preserves_existing_http_observation() {
    let server = Server::start([Reply::Decision(0.75)]).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed_calls = calls.clone();
    let port = server.responses().with_call_observer(move |event| {
        observed_calls.lock().unwrap().push(event);
    });
    let output = DecisionProbe::default();
    let observed = output.clone();
    let mut app = Application::mount(move || gate_judgment_builder(output.clone()), port).unwrap();
    app.react(false).unwrap().await.unwrap();
    app.shutdown().await.unwrap();
    assert_eq!(observed.take()[0].1.result, Ok(NoulAnswer { noul: 0.75 }));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(matches!(&calls[0], OpenAiResponsesCallEvent::Started(_)));
    assert!(matches!(&calls[1], OpenAiResponsesCallEvent::Finished(call)
        if call.outcome == Some(OpenAiResponsesCallOutcome::Completed)));
}

#[tokio::test]
async fn an_asynchronous_question_handler_settles_before_the_reaction_returns() {
    let server = Server::start([Reply::Decision(0.7)]).await;
    let output = DecisionProbe::default();
    let observed = output.clone();
    let mut app = Application::mount(
        move || asynchronous_gate_judgment(output.clone()),
        server.responses(),
    )
    .unwrap();
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ReactionOutcome::Completed
    );
    assert_eq!(observed.take()[0].1.result, Ok(NoulAnswer { noul: 0.7 }));
    app.shutdown().await.unwrap();
    assert_eq!(server.requests().len(), 1);
}

async fn run_gate(
    port: impl ReactionPort + 'static,
    output: DecisionProbe,
) -> Result<ReactionOutcome, ApplicationFault> {
    let mut app = Application::mount(move || gate_judgment(output.clone()), port).unwrap();
    let result = app.react(false).unwrap().await;
    app.shutdown().await.unwrap();
    result
}

#[tokio::test]
async fn question_failures_are_delivered_without_retry_or_confirmation() {
    for (reply, failure) in [
        (Reply::Decision(1.5), DecisionFailure::InvalidAnswer),
        (Reply::Empty, DecisionFailure::MissingResult),
        (Reply::Refused, DecisionFailure::Refused),
    ] {
        let server = Server::start([reply]).await;
        let observed = DecisionProbe::default();
        assert_eq!(
            run_gate(server.port(), observed.clone()).await.unwrap(),
            ReactionOutcome::Completed
        );
        let outputs = observed.take();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].1.result, Err(failure));
        assert!(outputs[0].0.reaction_id().is_some());
        assert!(outputs[0].0.frame_revision().is_some());
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn trailing_stream_failure_cannot_deliver_an_early_question_result() {
    let text = json!({"answers":{"open_gate":{"type":"noul","noul":0.8}}}).to_string();
    let server = Server::start([Reply::Trailing(text)]).await;
    let observed = DecisionProbe::default();
    assert!(run_gate(server.port(), observed.clone()).await.is_err());
    assert!(observed.take().is_empty());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_malformed_answer_envelope_is_a_reaction_error_without_result_delivery() {
    let server = Server::start([Reply::Raw("{invalid".to_owned())]).await;
    let observed = DecisionProbe::default();
    assert!(run_gate(server.port(), observed.clone()).await.is_err());
    assert!(observed.take().is_empty());
    assert_eq!(server.requests().len(), 1);
}

struct RejectCompletedObservation;

#[async_trait]
impl OpenAiResponsesObserver for RejectCompletedObservation {
    async fn observe(
        &self,
        event: OpenAiResponsesObservation,
    ) -> Result<(), OpenAiResponsesObservationError> {
        if matches!(
            event,
            OpenAiResponsesObservation::Response {
                completed: true,
                ..
            }
        ) {
            Err(OpenAiResponsesObservationError)
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn failed_response_observation_prevents_question_delivery() {
    let server = Server::start([Reply::Decision(0.8)]).await;
    let observed = DecisionProbe::default();
    let port = server
        .responses()
        .with_observer(Arc::new(RejectCompletedObservation));
    assert!(run_gate(port, observed.clone()).await.is_err());
    assert!(observed.take().is_empty());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn builtin_judge_uses_a_typed_question_and_one_reaction() {
    let server = Server::start([Reply::Raw(
        json!({"answers":{"judgment":{"type":"noul","noul":0.9}}}).to_string(),
    )])
    .await;
    let result = judge(
        server.responses(),
        json!({"permission":"granted"}),
        Noul::new("Does the permission allow opening the gate?"),
    )
    .await
    .unwrap();
    assert_eq!(result.value, NoulAnswer { noul: 0.9 });
    assert!(result.origin.application_id.get() > 0);
    assert!(result.origin.reaction_id.get() > 0);
    assert!(result.origin.frame_revision.is_some());
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn concurrent_judgments_have_separate_application_and_reaction_identity() {
    let answer = json!({"answers":{"judgment":{"type":"noul","noul":0.8}}}).to_string();
    let server = Server::start([Reply::Raw(answer.clone()), Reply::Raw(answer)]).await;
    let (first, second) = tokio::join!(
        judge(
            server.responses(),
            json!({"gate":1}),
            Noul::new("May this gate open?")
        ),
        judge(
            server.responses(),
            json!({"gate":2}),
            Noul::new("May this gate open?")
        ),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.value, second.value);
    assert_ne!(first.origin.application_id, second.origin.application_id);
    assert_ne!(first.origin.reaction_id, second.origin.reaction_id);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn builtin_judge_distinguishes_question_and_runtime_failures() {
    let server = Server::start([
        Reply::Raw("not json".to_owned()),
        Reply::Empty,
        Reply::Unauthorized,
    ])
    .await;
    for expected in [
        JudgmentErrorKind::Application,
        JudgmentErrorKind::Decision(DecisionFailure::MissingResult),
        JudgmentErrorKind::Application,
    ] {
        let error = judge(
            server.responses(),
            json!({"permission":"unknown"}),
            Noul::new("May the gate open?"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, expected);
        assert!(error.origin.is_some());
    }
    assert_eq!(server.requests().len(), 3);
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Add {
    amount: i64,
}

#[derive(Clone, Default)]
struct CounterProbe(Arc<Mutex<Option<Signal<i64>>>>);

impl CounterProbe {
    fn value(&self) -> i64 {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .with(|value| *value)
            .unwrap()
    }
}

#[component]
fn counter(probe: CounterProbe) -> Component {
    let total = use_signal(|| 0_i64);
    *probe.0.lock().unwrap() = Some(total.clone());
    let value = total.with(|value| *value).unwrap();
    view! {
        count { "{value}" }
        NativeToolCall {
            name: "add",
            description: "Add to the counter; the next view shows its actual value.",
            on_call: move |_context, input: Add| total.update(|value| {
                *value += input.amount;
                *value
            }),
        }
    }
}

#[tokio::test]
async fn ordinary_native_tool_component_works_with_responses_and_delivers_feedback() {
    let server = Server::start([Reply::Tool { amount: 2 }, Reply::Tool { amount: 3 }]).await;
    let (port, frames) = RecordingPort::new(server.port());
    let probe = CounterProbe::default();
    let observed = probe.clone();
    let mut app = Application::mount(move || counter(probe.clone()), port).unwrap();

    app.react(false).unwrap().await.unwrap();
    assert_eq!(observed.value(), 2);
    app.react(false).unwrap().await.unwrap();
    assert_eq!(observed.value(), 5);
    app.shutdown().await.unwrap();

    let frames = frames.records();
    assert_eq!(frames.len(), 2);
    assert!(frames[0].decision.is_none());
    assert!(frames[1].document().has_field("count", "2"));
    assert_eq!(frames[1].tool_names, ["add"]);
    assert!(frames[1]
        .staged
        .iter()
        .any(|item| matches!(item, CanonicalInputItem::ToolResult { .. })));
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["tools"][0]["name"], "add");
    assert!(requests[0].get("text").is_none());
}

#[derive(Clone, Default)]
struct NotebookProbe(Arc<Mutex<Option<Signal<Vec<String>>>>>);

impl NotebookProbe {
    fn notes(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .with(Clone::clone)
            .unwrap()
    }
}

#[component]
fn notebook(probe: NotebookProbe) -> Component {
    let notes = use_signal(Vec::<String>::new);
    *probe.0.lock().unwrap() = Some(notes.clone());
    let count = notes.with(Vec::len).unwrap();
    view! {
        saved_notes { "{count}" }
        XmlStreamingToolCall {
            element: XmlToolElement::text("note"),
            description: "Save a note; the next view shows how many were saved.",
            on_complete: move |_context, text: String| notes.update(|notes| notes.push(text)),
        }
    }
}

#[tokio::test]
async fn ordinary_xml_component_works_with_responses_and_delivers_feedback() {
    let server = Server::start([
        Reply::Raw("<note>one</note>".to_owned()),
        Reply::Raw("<note>two</note>".to_owned()),
    ])
    .await;
    let (port, frames) = RecordingPort::new(server.port());
    let probe = NotebookProbe::default();
    let observed = probe.clone();
    let mut app = Application::mount(move || notebook(probe.clone()), port).unwrap();

    app.react(false).unwrap().await.unwrap();
    assert_eq!(observed.notes(), ["one"]);
    app.react(false).unwrap().await.unwrap();
    assert_eq!(observed.notes(), ["one", "two"]);
    app.shutdown().await.unwrap();

    let frames = frames.records();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|frame| frame.decision.is_none()));
    assert!(frames[1].document().has_field("saved_notes", "1"));
    assert!(server
        .requests()
        .iter()
        .all(|body| body.get("text").is_none()));
}

struct EmptyPort {
    declaration: TargetDeclaration,
    submissions: Arc<Mutex<usize>>,
    sealed_text: Option<String>,
}

impl EmptyPort {
    fn new(capabilities: FrameCapabilities) -> (Self, Arc<Mutex<usize>>) {
        let submissions = Arc::new(Mutex::new(0));
        (
            Self {
                declaration: TargetDeclaration::full(
                    TargetIdentity::new(NonZeroU128::new(19_805).unwrap()),
                    TargetEpoch::new(NonZeroU64::MIN),
                    FrameProfile::new(
                        FrameConstraints {
                            max_frame_bytes: 1_048_576,
                            max_component_bytes: 262_144,
                            context_window: ContextWindow::NotApplicable,
                            reserved_output_tokens: None,
                        },
                        capabilities,
                    ),
                ),
                submissions: submissions.clone(),
                sealed_text: None,
            },
            submissions,
        )
    }
}

#[async_trait]
impl ReactionPort for EmptyPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        *self.submissions.lock().unwrap() += 1;
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        let facts = match &self.sealed_text {
            Some(text) => {
                let output = ProviderOutputKey::new(1);
                vec![
                    Ok(ProviderFact::TextSealed {
                        output,
                        phase: None,
                        text: text.clone(),
                    }),
                    Ok(ProviderFact::ReactionCompleted {
                        primary_text: Some(output),
                    }),
                ]
            }
            None => vec![Ok(ProviderFact::ReactionCompleted { primary_text: None })],
        };
        Ok(Box::pin(futures::stream::iter(facts)))
    }
}

#[derive(Clone, Default)]
struct VisibilityProbe(Arc<Mutex<Option<Signal<bool>>>>);

#[component]
fn optional_decision(probe: VisibilityProbe) -> Component {
    let visible = use_signal(|| true);
    *probe.0.lock().unwrap() = Some(visible.clone());
    if visible.with(|visible| *visible).unwrap() {
        view! {
            {
                Decision::new("gate", Noul::new("Is the gate open?"))
                    .on_result(|_, _| Ok::<(), Infallible>(()))
                    .build()
            }
        }
    } else {
        view! { idle { "No judgment is requested." } }
    }
}

#[tokio::test]
async fn missing_capability_rejects_before_submit_and_declaration_changes_can_retry() {
    let (port, submissions) = EmptyPort::new(FrameCapabilities::new(true));
    let probe = VisibilityProbe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(move || optional_decision(root_probe.clone()), port).unwrap();

    let fault = app.react(false).unwrap().await.unwrap_err();
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(fault.code(), ApplicationFaultCode::Rejected);
    assert_eq!(
        fault.reason(),
        ApplicationFaultReason::UnsupportedCapabilities {
            missing: FrameRequirements::NONE.with_decision(true),
        }
    );
    assert_eq!(*submissions.lock().unwrap(), 0);

    probe
        .0
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set(false)
        .unwrap();
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ReactionOutcome::Completed
    );
    assert_eq!(*submissions.lock().unwrap(), 1);
    app.shutdown().await.unwrap();
}

#[component]
fn requires_deltas() -> Component {
    use_provider_event_handler(ProviderEvent::TEXT_DELTA, |_context, _delta| async {
        Ok::<(), Infallible>(())
    });
    view! { input { "Emit the current status." } }
}

#[tokio::test]
async fn explicit_delta_subscription_requires_delta_capability_before_submit() {
    let (port, submissions) = EmptyPort::new(FrameCapabilities::new(true));
    let mut app = Application::mount(requires_deltas, port).unwrap();
    let fault = app.react(false).unwrap().await.unwrap_err();
    assert_eq!(
        fault.reason(),
        ApplicationFaultReason::UnsupportedCapabilities {
            missing: FrameRequirements::NONE.with_text_delta_output(true),
        }
    );
    assert_eq!(*submissions.lock().unwrap(), 0);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_xml_callbacks_consume_complete_only_text_without_delta_capability() {
    let (mut port, submissions) = EmptyPort::new(FrameCapabilities::new(true));
    port.sealed_text = Some("<note>complete only</note>".to_owned());
    let probe = NotebookProbe::default();
    let observed = probe.clone();
    let mut app = Application::mount(move || notebook(probe.clone()), port).unwrap();
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ReactionOutcome::Completed
    );
    assert_eq!(observed.notes(), ["complete only"]);
    assert_eq!(*submissions.lock().unwrap(), 1);
    app.shutdown().await.unwrap();
}

#[component]
fn conflicting_decision_and_xml(probe: VisibilityProbe) -> Component {
    let active = use_signal(|| true);
    *probe.0.lock().unwrap() = Some(active.clone());
    let action = if active.with(|active| *active).unwrap() {
        view! {
            XmlStreamingToolCall {
                element: XmlToolElement::text("note"),
                on_complete: |_context, _text: String| Ok::<(), Infallible>(()),
            }
        }
    } else {
        view! {}
    };
    view! {
        {
            Decision::new("gate", Noul::new("Is the gate open?"))
                .on_result(|_, _| Ok::<(), Infallible>(()))
                .build()
        }
        { action }
    }
}

#[tokio::test]
async fn conflicting_action_mode_is_rejected_before_submit_and_can_be_removed() {
    let (port, submissions) = EmptyPort::new(FrameCapabilities::new(true).with_decision(true));
    let probe = VisibilityProbe::default();
    let root_probe = probe.clone();
    let mut app = Application::mount(
        move || conflicting_decision_and_xml(root_probe.clone()),
        port,
    )
    .unwrap();
    let fault = app.react(false).unwrap().await.unwrap_err();
    assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
    assert_eq!(
        fault.reason(),
        ApplicationFaultReason::DecisionActionConflict
    );
    assert_eq!(*submissions.lock().unwrap(), 0);

    probe
        .0
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .set(false)
        .unwrap();
    assert_eq!(
        app.react(false).unwrap().await.unwrap(),
        ReactionOutcome::Completed
    );
    assert_eq!(*submissions.lock().unwrap(), 1);
    app.shutdown().await.unwrap();
}
