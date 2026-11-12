#[path = "support/pom.rs"]
mod pom_support;

use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use agentview::{
    component::{
        execution::{
            Application, ApplicationFault, ApplicationFaultCode, ApplicationFaultKind,
            ApplicationFaultReason, ApplicationFaultStage, ChoiceAnswer, DecisionDeclaration,
            DecisionDeclarationError, DecisionFailure, Frame, FrameRequirements, FrameRevision,
            NoulAnswer, ProviderFactStream, ProviderIdentity, QuestionOutcome, ReactionOutcome,
            ReactionPort, ReactionPortFault, ScoreAnswer, SubmitFault, TargetDeclaration,
        },
        prelude::*,
    },
    provider::{
        async_openai::{
            AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig, CompactionContext,
            OpenAiResponsesCallEvent, OpenAiResponsesCallKind, OpenAiResponsesCallOutcome,
            OpenAiResponsesObservation, OpenAiResponsesObservationError, OpenAiResponsesObserver,
        },
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
        system1::{System1Endpoint, System1Options, System1Provider},
        ModelSpec,
    },
    transcript::{CanonicalInputItem, ConversationRole},
};
use async_trait::async_trait;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use pom_support::{combine, InspectPom};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

fn answer_values() -> Value {
    json!({
        "is_bug": {"type":"noul", "noul":0.96},
        "team": {
            "type":"choice", "choice":"payments", "confidence":0.75,
            "probabilities":{"frontend":0.16,"payments":0.84},
        },
        "urgency": {
            "type":"score", "score":1.99, "confidence":0.99,
            "probabilities":{"0":0.0,"1":0.01,"2":0.99},
            "legend":{"0":"Can wait","1":"Fix this week","2":"Blocking revenue"},
        },
    })
}

fn native_body(answers: Value) -> Value {
    json!({
        "id":"gen-system1-test", "model":"typesafe/jev-test", "provider":"TypeSafe",
        "answers":answers,
        "usage":{
            "input_tokens":476, "output_tokens":70, "cost":0.000019992,
            "input_tokens_details":{"cached_tokens":128},
        },
    })
}

fn response_events(result: Value) -> Response {
    let text = json!({"answers":result}).to_string();
    let message = json!({
        "id":"msg_decision", "type":"message", "status":"completed", "role":"assistant",
        "content":[{"type":"output_text", "text":text, "annotations":[]}],
    });
    let events = [
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
    ];
    let body: String = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

#[derive(Clone)]
enum Reply {
    Answers(Value),
    NativeBody(Value),
    Paused {
        answers: Value,
        arrived: Arc<Notify>,
        release: Arc<Notify>,
    },
    Raw(&'static str),
    HttpError(StatusCode),
}

#[derive(Clone)]
struct HttpRequest {
    path: String,
    body: Value,
    ark_beta: Option<String>,
    authorization: Option<String>,
    peer: SocketAddr,
}

#[derive(Clone)]
struct ServerState {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

struct Server {
    base: String,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = ServerState {
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
            requests: requests.clone(),
        };
        let app = Router::new()
            .route("/api/v1/systemone", post(handle_request))
            .route("/api/alpha/decisions", post(handle_request))
            .route("/responses", post(handle_request))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Self {
            base,
            requests,
            task,
        }
    }

    fn responses(&self) -> AsyncOpenAiResponsesProvider {
        AsyncOpenAiResponsesProvider::try_new(
            AsyncOpenAiTransportConfig::new(&self.base, "test-key").unwrap(),
            ProviderIdentity::new("test", CODEX_HTTP_V1_PROFILE, 1, "decisions").unwrap(),
            CodexHttpV1Encoder::new(
                CodexHttpV1Options::new(
                    ModelSpec::new("typesafe/jev-test", 32_768).unwrap(),
                    None,
                    None,
                    None::<String>,
                )
                .unwrap()
                .with_max_output_tokens(512)
                .unwrap(),
            ),
        )
        .unwrap()
        .without_parallel_compaction()
    }

    fn system1(&self) -> System1Provider {
        self.system1_at(System1Endpoint::Decisions)
    }

    fn system1_at(&self, endpoint: System1Endpoint) -> System1Provider {
        System1Provider::try_new(
            AsyncOpenAiTransportConfig::new(format!("{}/api/v1", self.base), "test-key").unwrap(),
            ProviderIdentity::new("test", "system1", 1, "decisions").unwrap(),
            System1Options::new(ModelSpec::new("typesafe/jev-test", 32_768).unwrap())
                .with_endpoint(endpoint),
        )
        .unwrap()
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_request(
    State(state): State<ServerState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let path = uri.path().to_owned();
    state.requests.lock().unwrap().push(HttpRequest {
        path: path.clone(),
        body,
        ark_beta: headers
            .get("ark-beta-systemone-api")
            .map(|value| value.to_str().unwrap().to_owned()),
        authorization: headers
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned()),
        peer,
    });
    let reply = state
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Reply::HttpError(StatusCode::INTERNAL_SERVER_ERROR));
    match reply {
        Reply::NativeBody(body) => Json(body).into_response(),
        Reply::Paused {
            answers,
            arrived,
            release,
        } => {
            arrived.notify_one();
            release.notified().await;
            if path == "/responses" {
                response_events(answers)
            } else {
                Json(native_body(answers)).into_response()
            }
        }
        Reply::Answers(answers) if path == "/responses" => response_events(answers),
        Reply::Answers(answers) => (
            [("x-request-id", "req-system1-test")],
            Json(native_body(answers)),
        )
            .into_response(),
        Reply::Raw(text) => ([(header::CONTENT_TYPE, "application/json")], text).into_response(),
        Reply::HttpError(status) => (
            status,
            Json(json!({"error":{"code":status.as_u16(),"message":"upstream unavailable"}})),
        )
            .into_response(),
    }
}

enum CapturedAnswer {
    Noul(QuestionOutcome<NoulAnswer>),
    Choice(QuestionOutcome<ChoiceAnswer>),
    Score(QuestionOutcome<ScoreAnswer>),
}

impl CapturedAnswer {
    fn result(&self) -> Result<Value, DecisionFailure> {
        match self {
            Self::Noul(outcome) => outcome
                .result
                .as_ref()
                .map(|answer| json!({"type":"noul","noul":answer.noul}))
                .map_err(|failure| *failure),
            Self::Choice(outcome) => outcome
                .result
                .as_ref()
                .map(|answer| {
                    json!({
                        "type":"choice", "choice":answer.choice,
                        "probabilities":answer.probabilities, "confidence":answer.confidence,
                    })
                })
                .map_err(|failure| *failure),
            Self::Score(outcome) => outcome
                .result
                .as_ref()
                .map(|answer| {
                    json!({
                        "type":"score", "score":answer.score,
                        "probabilities":answer.probabilities, "legend":answer.legend,
                        "confidence":answer.confidence,
                    })
                })
                .map_err(|failure| *failure),
        }
    }
}

#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<(String, EventContext, CapturedAnswer)>>>);

impl Probe {
    fn store(&self, name: &str, context: EventContext, answer: CapturedAnswer) {
        self.0
            .lock()
            .unwrap()
            .push((name.to_owned(), context, answer));
    }

    fn take(&self) -> Vec<(String, EventContext, CapturedAnswer)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn team_question() -> Choice {
    Choice::new(
        "Which team should own the displayed ticket?",
        BTreeMap::from([
            (
                "frontend".to_owned(),
                json!("Rendering and layout problems"),
            ),
            (
                "payments".to_owned(),
                json!("Checkout and payment problems"),
            ),
        ]),
    )
}

#[component]
fn ticket_judgment(output: Probe) -> Component {
    let bug = output.clone();
    let team = output.clone();
    view! {
        ticket_status { "unassigned" }
        ticket { "Checkout is blank after I click Pay." }
        customer_tier { "enterprise" }
        Decision {
            name: "is_bug",
            question: Noul::new("Does the displayed ticket describe broken behavior?")
                .criteria("The product behaved incorrectly.", "Only a question or feature request."),
            on_result: move |context, outcome: QuestionOutcome<NoulAnswer>| {
                bug.store("is_bug", context, CapturedAnswer::Noul(outcome));
                Ok::<(), Infallible>(())
            },
        }
        Decision {
            name: "team",
            question: team_question(),
            on_result: move |context, outcome: QuestionOutcome<ChoiceAnswer>| {
                team.store("team", context, CapturedAnswer::Choice(outcome));
                Ok::<(), Infallible>(())
            },
        }
        Decision {
            name: "urgency",
            question: Score::new(
                "How urgent is the displayed ticket?",
                vec![json!("Can wait"), json!("Fix this week"), json!("Blocking revenue")],
            ),
            on_result: move |context, outcome: QuestionOutcome<ScoreAnswer>| {
                output.store("urgency", context, CapturedAnswer::Score(outcome));
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[derive(Clone)]
struct SubmittedFrame {
    revision: FrameRevision,
    decision: DecisionDeclaration,
    projection: Vec<CanonicalInputItem>,
}

struct RecordingPort {
    inner: Box<dyn ReactionPort>,
    frames: Arc<Mutex<Vec<SubmittedFrame>>>,
}

#[async_trait]
impl ReactionPort for RecordingPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.inner.declare()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        self.frames.lock().unwrap().push(SubmittedFrame {
            revision: frame.revision(),
            decision: frame.submission().decision().unwrap().clone(),
            projection: frame.submission().projection().items().to_vec(),
        });
        self.inner.submit(frame).await
    }

    async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }
}

async fn run_ticket(
    port: impl ReactionPort + 'static,
    output: Probe,
) -> Result<ReactionOutcome, ApplicationFault> {
    let mut app = Application::mount(move || ticket_judgment(output.clone()), port).unwrap();
    let result = app.react(false).unwrap().await;
    app.shutdown().await.unwrap();
    result
}

#[component]
fn ticket_judgment_with_context(context: String, output: Probe) -> Component {
    view! {
        #[user]
        { context }
        ticket_judgment(output)
    }
}

fn decision_port_with_limits(
    server: &Server,
    native: bool,
    window: u64,
    request_limit: usize,
    calls: Arc<Mutex<Vec<OpenAiResponsesCallEvent>>>,
) -> Box<dyn ReactionPort> {
    let config = AsyncOpenAiTransportConfig::new(&server.base, "test-key")
        .unwrap()
        .with_responses_serialized_request_body_limit(request_limit)
        .unwrap();
    let model = ModelSpec::new("typesafe/jev-test", window).unwrap();
    let observer = move |event| calls.lock().unwrap().push(event);
    if native {
        Box::new(
            System1Provider::try_new(
                config,
                ProviderIdentity::new("test", "system1", 1, "decisions").unwrap(),
                System1Options::new(model),
            )
            .unwrap()
            .with_call_observer(observer),
        )
    } else {
        Box::new(
            AsyncOpenAiResponsesProvider::try_new(
                config,
                ProviderIdentity::new("test", CODEX_HTTP_V1_PROFILE, 1, "decisions").unwrap(),
                CodexHttpV1Encoder::new(
                    CodexHttpV1Options::new(model, None, None, None::<String>)
                        .unwrap()
                        .with_max_output_tokens(512)
                        .unwrap(),
                ),
            )
            .unwrap()
            .with_parallel_compaction(|_: &CompactionContext| {
                panic!("a Decision must not evaluate compaction policy")
            })
            .with_forced_compaction()
            .with_call_observer(observer),
        )
    }
}

#[tokio::test]
async fn decision_context_estimates_allow_one_http_call_on_both_providers() {
    let context = "中文🙂 \"quoted\" \\path\nnext\tline\u{0008}".repeat(256);
    let window = 128;
    let request_limit = 128 * 1024;
    for native in [false, true] {
        let server = Server::start([Reply::Answers(answer_values())]).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let port = RecordingPort {
            inner: decision_port_with_limits(&server, native, window, request_limit, calls.clone()),
            frames: frames.clone(),
        };
        let output = Probe::default();
        let observed = output.clone();
        let root_context = context.clone();
        let mut app = Application::mount(
            move || ticket_judgment_with_context(root_context.clone(), output.clone()),
            port,
        )
        .unwrap();
        assert_eq!(
            app.react(false).unwrap().await.unwrap(),
            ReactionOutcome::Completed
        );
        app.shutdown().await.unwrap();

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        let document = combine(frames[0].projection.iter().filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
            _ => None,
        }));
        assert!(document.has_text(&context));
        let outputs = observed.take();
        assert_eq!(outputs.len(), 3);
        for (name, _, result) in outputs {
            assert_eq!(result.result(), Ok(answer_values()[&name].clone()));
        }

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "Decision must not make a summary or retry call"
        );
        assert_eq!(
            requests[0].path,
            if native {
                "/api/alpha/decisions"
            } else {
                "/responses"
            }
        );
        let bytes = serde_json::to_vec(&requests[0].body).unwrap().len();
        assert!(bytes.div_ceil(4) as u64 > window);
        assert!(bytes < request_limit);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(matches!(&calls[0], OpenAiResponsesCallEvent::Started(call)
            if call.kind == OpenAiResponsesCallKind::Foreground));
        assert!(matches!(&calls[1], OpenAiResponsesCallEvent::Finished(call)
            if call.kind == OpenAiResponsesCallKind::Foreground
                && call.outcome == Some(OpenAiResponsesCallOutcome::Completed)));
    }
}

#[tokio::test]
async fn decision_request_byte_limits_still_reject_before_http_on_both_providers() {
    for native in [false, true] {
        let server = Server::start([]).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let port = decision_port_with_limits(&server, native, 128, 1, calls.clone());
        let output = Probe::default();
        let observed = output.clone();
        let mut app = Application::mount(move || ticket_judgment(output.clone()), port).unwrap();
        let fault = app.react(false).unwrap().await.unwrap_err();
        assert_eq!(fault.code(), ApplicationFaultCode::Limit);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::Port(
                agentview::component::execution::ReactionPortFaultReason::RequestPreparation
            )
        );
        app.shutdown().await.unwrap();
        assert!(server.requests().is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(observed.take().is_empty());
    }
}

#[tokio::test]
async fn the_same_three_decision_components_share_one_frame_and_one_call_on_both_providers() {
    let mut submitted = Vec::new();
    for native in [false, true] {
        let server = Server::start([Reply::Answers(answer_values())]).await;
        let inner: Box<dyn ReactionPort> = if native {
            Box::new(server.system1())
        } else {
            Box::new(server.responses())
        };
        let frames = Arc::new(Mutex::new(Vec::new()));
        let port = RecordingPort {
            inner,
            frames: frames.clone(),
        };
        let output = Probe::default();
        let observed = output.clone();
        let mut app = Application::mount(move || ticket_judgment(output.clone()), port).unwrap();
        let application_id = app.application_id();
        let reaction = app.react(false).unwrap();
        let reaction_id = reaction.id();
        assert_eq!(reaction.await.unwrap(), ReactionOutcome::Completed);
        app.shutdown().await.unwrap();

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].decision.questions().len(), 3);
        let outputs = observed.take();
        assert_eq!(outputs.len(), 3);
        for (name, context, result) in outputs {
            assert_eq!(result.result(), Ok(answer_values()[&name].clone()));
            assert_eq!(context.application_id(), application_id);
            assert_eq!(context.reaction_id(), Some(reaction_id));
            assert_eq!(context.frame_revision(), Some(frames[0].revision));
            assert!(matches!(
                context.source(),
                EventSource::Provider { output: Some(_) }
            ));
        }
        let document = combine(frames[0].projection.iter().filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
            _ => None,
        }));
        assert!(document.has_field("ticket_status", "unassigned"));
        assert!(document.has_field("customer_tier", "enterprise"));

        let http = server.requests();
        assert_eq!(
            http.len(),
            1,
            "all questions use one HTTP call without confirmation"
        );
        assert_eq!(http[0].body["model"], "typesafe/jev-test");
        if native {
            assert_eq!(http[0].path, "/api/alpha/decisions");
            assert_eq!(
                http[0].body["questions"],
                serde_json::to_value(frames[0].decision.questions()).unwrap()
            );
            let state = &http[0].body["state"];
            for section in ["replay", "staged_inputs", "projection", "ephemeral"] {
                assert!(state[section].is_array());
            }
            assert!(!state["projection"].as_array().unwrap().is_empty());
            for absent in ["context", "instruction", "frame"] {
                assert!(state.get(absent).is_none());
            }
            for absent in ["input", "text", "tools", "stream", "previous_response_id"] {
                assert!(http[0].body.get(absent).is_none());
            }
        } else {
            assert_eq!(http[0].path, "/responses");
            assert_eq!(http[0].body["text"]["format"]["type"], "json_schema");
            assert!(http[0].body.get("input").is_some());
            assert!(http[0].body.get("questions").is_none());
        }
        submitted.push((frames[0].decision.questions().clone(), http[0].body.clone()));
    }
    assert_eq!(submitted[0].0, submitted[1].0);
    assert_ne!(submitted[0].1, submitted[1].1);
}

#[component]
fn user_scoped_ticket_judgment(output: Probe) -> Component {
    let child = ticket_judgment(output);
    view! {
        #[user]
        { child }
    }
}

#[tokio::test]
async fn a_user_scoped_component_delivers_ordinary_state_and_typed_questions_together() {
    for native in [false, true] {
        let server = Server::start([Reply::Answers(answer_values())]).await;
        let inner: Box<dyn ReactionPort> = if native {
            Box::new(server.system1())
        } else {
            Box::new(server.responses())
        };
        let frames = Arc::new(Mutex::new(Vec::new()));
        let port = RecordingPort {
            inner,
            frames: frames.clone(),
        };
        let probe = Probe::default();
        let observed = probe.clone();
        let mut app =
            Application::mount(move || user_scoped_ticket_judgment(probe.clone()), port).unwrap();
        assert_eq!(
            app.react(false).unwrap().await.unwrap(),
            ReactionOutcome::Completed
        );
        app.shutdown().await.unwrap();
        let outcomes = observed.take();
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes
            .iter()
            .all(|(name, _, answer)| answer.result() == Ok(answer_values()[name].clone())));
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].decision.questions().len(), 3);
        assert!(frames[0].projection.iter().any(|item| matches!(
            item,
            CanonicalInputItem::Message {
                role: ConversationRole::User,
                ..
            }
        )));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        if native {
            assert_eq!(requests[0].body["questions"].as_object().unwrap().len(), 3);
            assert!(requests[0].body["state"]["projection"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["kind"] == "message" && item["payload"]["role"] == "user"));
        } else {
            assert_eq!(requests[0].body["text"]["format"]["type"], "json_schema");
        }
    }
}

#[tokio::test]
async fn systemone_endpoint_delivers_all_three_questions_in_one_json_request() {
    let server = Server::start([Reply::Answers(answer_values())]).await;
    let probe = Probe::default();
    assert_eq!(
        run_ticket(server.system1_at(System1Endpoint::SystemOne), probe.clone())
            .await
            .unwrap(),
        ReactionOutcome::Completed
    );
    assert_eq!(probe.take().len(), 3);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/v1/systemone");
    assert_eq!(requests[0].body["questions"].as_object().unwrap().len(), 3);
    assert!(requests[0].body.get("stream").is_none());
}

#[tokio::test]
async fn native_endpoints_do_not_send_ark_beta_header_by_default() {
    for endpoint in [System1Endpoint::Decisions, System1Endpoint::SystemOne] {
        let server = Server::start([Reply::Answers(answer_values())]).await;
        let probe = Probe::default();
        assert_eq!(
            run_ticket(server.system1_at(endpoint), probe.clone())
                .await
                .unwrap(),
            ReactionOutcome::Completed
        );
        assert_eq!(probe.take().len(), 3);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].ark_beta, None);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer test-key")
        );
    }
}

#[tokio::test]
async fn ark_beta_fresh_sessions_send_header_and_reuse_http_connection() {
    let server = Server::start([
        Reply::Answers(answer_values()),
        Reply::Answers(answer_values()),
    ])
    .await;
    let template = System1Provider::try_new(
        AsyncOpenAiTransportConfig::new(format!("{}/api", server.base), "test-key").unwrap(),
        ProviderIdentity::new("ark", "system1", 1, "decisions").unwrap(),
        System1Options::new(ModelSpec::new("typesafe/jev-test", 32_768).unwrap())
            .with_endpoint(System1Endpoint::SystemOne)
            .with_ark_beta(),
    )
    .unwrap();
    for _ in 0..2 {
        let fresh = template.fresh_session().unwrap();
        assert_eq!(fresh.options(), template.options());
        let probe = Probe::default();
        assert_eq!(
            run_ticket(fresh, probe.clone()).await.unwrap(),
            ReactionOutcome::Completed
        );
        assert_eq!(probe.take().len(), 3);
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path, "/api/v1/systemone");
        assert_eq!(request.ark_beta.as_deref(), Some("true"));
        assert!(request.body.get("ark_beta").is_none());
        assert_eq!(request.body["questions"].as_object().unwrap().len(), 3);
    }
    assert_eq!(requests[0].peer, requests[1].peer);
}

#[tokio::test]
async fn raw_authorization_fresh_sessions_accept_input_only_usage_and_share_connection() {
    let body = json!({
        "model":"decision-model-preview", "answers":answer_values(),
        "usage":{"input_tokens":81},
    });
    let server = Server::start([Reply::NativeBody(body.clone()), Reply::NativeBody(body)]).await;
    let usage = Arc::new(Mutex::new(Vec::new()));
    let observed_usage = usage.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed_calls = calls.clone();
    let template = System1Provider::try_new(
        AsyncOpenAiTransportConfig::new(format!("{}/api/v1", server.base), "raw-test-key").unwrap(),
        ProviderIdentity::new("aliyun", "system1", 1, "decisions").unwrap(),
        System1Options::new(ModelSpec::new("decision-model-preview", 65_536).unwrap())
            .with_endpoint(System1Endpoint::SystemOne)
            .with_raw_authorization(),
    )
    .unwrap()
    .with_response_usage_observer(move |value| observed_usage.lock().unwrap().push(value))
    .with_call_observer(move |event| observed_calls.lock().unwrap().push(event));
    for _ in 0..2 {
        let fresh = template.fresh_session().unwrap();
        assert_eq!(fresh.options(), template.options());
        let probe = Probe::default();
        assert_eq!(
            run_ticket(fresh, probe.clone()).await.unwrap(),
            ReactionOutcome::Completed
        );
        assert_eq!(probe.take().len(), 3);
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path, "/api/v1/systemone");
        assert_eq!(request.authorization.as_deref(), Some("raw-test-key"));
        assert_eq!(request.ark_beta, None);
        assert!(request.body.get("raw_authorization").is_none());
    }
    assert_eq!(requests[0].peer, requests[1].peer);
    let usage = usage.lock().unwrap();
    assert_eq!(usage.len(), 2);
    for value in usage.iter() {
        assert_eq!(value.input_tokens(), Some(81));
        assert_eq!(value.cached_tokens(), None);
        assert_eq!(value.output_tokens(), None);
        assert_eq!(value.total_tokens(), None);
    }
    let calls = calls.lock().unwrap();
    let finished = calls
        .iter()
        .filter_map(|event| match event {
            OpenAiResponsesCallEvent::Finished(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2);
    for call in finished {
        assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Completed));
        assert_eq!(call.usage.as_ref(), Some(&json!({"input_tokens":81})));
    }
}

#[tokio::test]
async fn native_optional_output_usage_still_rejects_invalid_counts() {
    for usage in [
        json!({}),
        json!({"input_tokens":-1}),
        json!({"input_tokens":81,"output_tokens":-1}),
        json!({"input_tokens":81,"output_tokens":"0"}),
        json!({"input_tokens":81,"input_tokens_details":{"cached_tokens":82}}),
        json!({"input_tokens":u64::MAX,"output_tokens":1}),
    ] {
        let server = Server::start([Reply::NativeBody(json!({
            "model":"decision-model-preview", "answers":answer_values(), "usage":usage,
        }))])
        .await;
        let probe = Probe::default();
        assert!(run_ticket(server.system1(), probe.clone()).await.is_err());
        assert!(probe.take().is_empty());
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn a_refused_question_does_not_swallow_other_answers_or_http_accounting() {
    let mut values = answer_values();
    values["team"] = json!({"type":"refusal"});
    let server = Server::start([Reply::Answers(values)]).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let provider = server.system1().with_call_observer(move |event| {
        observed.lock().unwrap().push(event);
    });
    let probe = Probe::default();
    run_ticket(provider, probe.clone()).await.unwrap();
    let outputs = probe.take();
    assert_eq!(outputs.len(), 3);
    for (name, _, result) in outputs {
        if name == "team" {
            assert_eq!(result.result(), Err(DecisionFailure::Refused));
        } else {
            assert_eq!(result.result(), Ok(answer_values()[&name].clone()));
        }
    }
    assert_eq!(server.requests().len(), 1);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(matches!(&calls[0], OpenAiResponsesCallEvent::Started(_)));
    let OpenAiResponsesCallEvent::Finished(call) = &calls[1] else {
        panic!("missing finished call")
    };
    assert_eq!(call.outcome, Some(OpenAiResponsesCallOutcome::Completed));
    assert_eq!(call.http_status, Some(200));
    assert_eq!(call.response_id.as_deref(), Some("gen-system1-test"));
    assert_eq!(call.request_id.as_deref(), Some("req-system1-test"));
    assert_eq!(call.response_model.as_deref(), Some("typesafe/jev-test"));
    assert_eq!(
        call.usage.as_ref().unwrap()["input_tokens_details"]["cached_tokens"],
        128
    );
}

#[tokio::test]
async fn missing_or_invalid_questions_leave_independent_valid_answers_available() {
    let mut unknown_choice = answer_values();
    unknown_choice["team"]["choice"] = json!("unknown-team");
    let mut missing = answer_values();
    missing.as_object_mut().unwrap().remove("team");
    let mut wrong_type = answer_values();
    wrong_type["urgency"] = json!({"type":"noul","noul":0.5});
    for native in [false, true] {
        for (answers, failed_name, expected) in [
            (
                unknown_choice.clone(),
                "team",
                DecisionFailure::InvalidAnswer,
            ),
            (missing.clone(), "team", DecisionFailure::MissingResult),
            (
                wrong_type.clone(),
                "urgency",
                DecisionFailure::InvalidAnswer,
            ),
        ] {
            let server = Server::start([Reply::Answers(answers)]).await;
            let provider: Box<dyn ReactionPort> = if native {
                Box::new(server.system1())
            } else {
                Box::new(server.responses())
            };
            let probe = Probe::default();
            run_ticket(provider, probe.clone()).await.unwrap();
            let outputs = probe.take();
            assert_eq!(outputs.len(), 3);
            for (name, _, result) in outputs {
                if name == failed_name {
                    assert_eq!(result.result(), Err(expected));
                } else {
                    assert_eq!(result.result(), Ok(answer_values()[&name].clone()));
                }
            }
            assert_eq!(server.requests().len(), 1);
        }
    }
}

#[tokio::test]
async fn native_http_and_malformed_response_failures_do_not_publish_or_retry() {
    for reply in [
        Reply::HttpError(StatusCode::UNAUTHORIZED),
        Reply::HttpError(StatusCode::TOO_MANY_REQUESTS),
        Reply::Raw("{invalid"),
        Reply::Raw("data: {\"type\":\"response.completed\"}\n\n"),
        Reply::Raw("{\"answers\":{},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}"),
    ] {
        let server = Server::start([reply]).await;
        let probe = Probe::default();
        assert!(run_ticket(server.system1(), probe.clone()).await.is_err());
        assert!(probe.take().is_empty());
        assert_eq!(server.requests().len(), 1);
    }
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
async fn native_response_observation_failure_prevents_all_question_delivery() {
    let server = Server::start([Reply::Answers(answer_values())]).await;
    let probe = Probe::default();
    let provider = server
        .system1()
        .with_observer(Arc::new(RejectCompletedObservation));
    assert!(run_ticket(provider, probe.clone()).await.is_err());
    assert!(probe.take().is_empty());
    assert_eq!(server.requests().len(), 1);
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ActionArgs {
    amount: i64,
}

#[component]
fn native_action(executed: Arc<Mutex<usize>>) -> Component {
    view! {
        NativeToolCall {
            name: "increment",
            description: "Increment the counter.",
            on_call: move |_context, args: ActionArgs| {
                *executed.lock().unwrap() += args.amount as usize;
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[component]
fn xml_action(executed: Arc<Mutex<usize>>) -> Component {
    view! {
        XmlStreamingToolCall {
            element: XmlToolElement::text("increment"),
            on_complete: move |_context, _text: String| {
                *executed.lock().unwrap() += 1;
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[component]
fn text_deltas() -> Component {
    use_provider_event_handler(ProviderEvent::TEXT_DELTA, |_context, _delta| async {
        Ok::<(), Infallible>(())
    });
    view! { status { "Report the current status." } }
}

#[tokio::test]
async fn native_decision_provider_rejects_ordinary_native_xml_and_delta_components_before_http() {
    for action in 0..3 {
        let server = Server::start([]).await;
        let mut provider = server.system1();
        let capabilities = provider.declare().unwrap().profile().capabilities;
        assert!(capabilities.supports_decision());
        assert!(!capabilities.supports_native_tool_calls());
        assert!(!capabilities.supports_text_delta_output());
        let executed = Arc::new(Mutex::new(0));
        let observed = executed.clone();
        let mut app = Application::mount(
            move || match action {
                0 => native_action(executed.clone()),
                1 => xml_action(executed.clone()),
                _ => text_deltas(),
            },
            provider,
        )
        .unwrap();
        let fault = app.react(false).unwrap().await.unwrap_err();
        assert_eq!(
            fault.kind(),
            ApplicationFaultKind::Retryable,
            "action {action}: {fault:?}"
        );
        if action == 0 {
            assert_eq!(
                fault.reason(),
                ApplicationFaultReason::UnsupportedCapabilities {
                    missing: FrameRequirements::NONE.with_native_tool_calls(true),
                }
            );
        } else if action == 2 {
            assert_eq!(
                fault.reason(),
                ApplicationFaultReason::UnsupportedCapabilities {
                    missing: FrameRequirements::NONE.with_text_delta_output(true),
                }
            );
        }
        assert_eq!(*observed.lock().unwrap(), 0);
        assert!(server.requests().is_empty());
        app.shutdown().await.unwrap();
    }
}

#[derive(Clone, Default)]
struct ModeProbe(Arc<Mutex<Option<Signal<bool>>>>);

#[component]
fn replaceable_xml(mode: ModeProbe, output: Probe) -> Component {
    let xml = use_signal(|| true);
    *mode.0.lock().unwrap() = Some(xml.clone());
    if xml.with(|xml| *xml).unwrap() {
        xml_action(Arc::new(Mutex::new(0)))
    } else {
        ticket_judgment(output)
    }
}

#[tokio::test]
async fn native_rejection_allows_replacing_xml_with_decisions_and_retrying() {
    let server = Server::start([Reply::Answers(answer_values())]).await;
    let mode = ModeProbe::default();
    let root_mode = mode.clone();
    let output = Probe::default();
    let observed = output.clone();
    let mut app = Application::mount(
        move || replaceable_xml(root_mode.clone(), output.clone()),
        server.system1(),
    )
    .unwrap();
    let first = app.react(false).unwrap();
    let first_id = first.id();
    assert_eq!(
        first.await.unwrap_err().kind(),
        ApplicationFaultKind::Retryable
    );
    assert!(server.requests().is_empty());
    assert!(observed.take().is_empty());
    mode.0.lock().unwrap().as_ref().unwrap().set(false).unwrap();
    let next = app.react(false).unwrap();
    let next_id = next.id();
    assert_ne!(first_id, next_id);
    assert_eq!(next.await.unwrap(), ReactionOutcome::Completed);
    app.shutdown().await.unwrap();
    assert_eq!(server.requests().len(), 1);
    let outputs = observed.take();
    assert_eq!(outputs.len(), 3);
    assert!(outputs.iter().all(
        |(_, context, outcome)| context.reaction_id() == Some(next_id) && outcome.result().is_ok()
    ));
}

#[component]
fn duplicate_question_names(mode: ModeProbe) -> Component {
    let unique = use_signal(|| false);
    *mode.0.lock().unwrap() = Some(unique.clone());
    let second_name = if unique.with(|unique| *unique).unwrap() {
        "team"
    } else {
        "same"
    };
    view! {
        Decision {
            name: "same",
            question: Noul::new("First question?"),
            on_result: |_context, _outcome: QuestionOutcome<NoulAnswer>| Ok::<(), Infallible>(()),
        }
        Decision {
            name: second_name,
            question: team_question(),
            on_result: |_context, _outcome: QuestionOutcome<ChoiceAnswer>| Ok::<(), Infallible>(()),
        }
    }
}

#[tokio::test]
async fn duplicate_question_names_are_rejected_before_either_provider_is_called() {
    for native in [false, true] {
        let server = Server::start([Reply::Answers(json!({
            "same":{"type":"noul","noul":0.8},
            "team":{"type":"choice","choice":"payments"},
        }))])
        .await;
        let port: Box<dyn ReactionPort> = if native {
            Box::new(server.system1())
        } else {
            Box::new(server.responses())
        };
        let mode = ModeProbe::default();
        let root_mode = mode.clone();
        let mut app =
            Application::mount(move || duplicate_question_names(root_mode.clone()), port).unwrap();
        let fault = app.react(false).unwrap().await.unwrap_err();
        assert_eq!(fault.stage(), ApplicationFaultStage::FramePrepare);
        assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
        assert_eq!(fault.code(), ApplicationFaultCode::Rejected);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::InvalidDecisionDeclaration(
                DecisionDeclarationError::DuplicateNames,
            )
        );
        assert!(server.requests().is_empty());
        mode.0.lock().unwrap().as_ref().unwrap().set(true).unwrap();
        assert_eq!(
            app.react(false).unwrap().await.unwrap(),
            ReactionOutcome::Completed
        );
        assert_eq!(server.requests().len(), 1);
        app.shutdown().await.unwrap();
    }
}

#[component]
fn repairable_question(
    invalid_name: bool,
    mode: ModeProbe,
    output: Arc<Mutex<Vec<QuestionOutcome<ChoiceAnswer>>>>,
) -> Component {
    let repaired = use_signal(|| false);
    *mode.0.lock().unwrap() = Some(repaired.clone());
    let repaired = repaired.with(|repaired| *repaired).unwrap();
    let name = if invalid_name && !repaired {
        " "
    } else {
        "owner"
    };
    let criteria = if !invalid_name && !repaired {
        BTreeMap::new()
    } else {
        BTreeMap::from([(
            "payments".to_owned(),
            json!("Checkout and payment problems"),
        )])
    };
    view! {
        ticket { "Checkout is blank after I click Pay." }
        Decision {
            name: name,
            question: Choice::new("Who owns the ticket?", criteria),
            on_result: move |_context, outcome: QuestionOutcome<ChoiceAnswer>| {
                output.lock().unwrap().push(outcome);
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[tokio::test]
async fn invalid_names_and_criteria_reject_during_frame_prepare_and_can_be_repaired() {
    for native in [false, true] {
        for invalid_name in [false, true] {
            let server = Server::start([Reply::Answers(json!({
                "owner":{"type":"choice","choice":"payments"},
            }))])
            .await;
            let port: Box<dyn ReactionPort> = if native {
                Box::new(server.system1())
            } else {
                Box::new(server.responses())
            };
            let mode = ModeProbe::default();
            let root_mode = mode.clone();
            let output = Arc::new(Mutex::new(Vec::new()));
            let observed = output.clone();
            let mut app = Application::mount(
                move || repairable_question(invalid_name, root_mode.clone(), output.clone()),
                port,
            )
            .unwrap();
            let fault = app.react(false).unwrap().await.unwrap_err();
            assert_eq!(fault.stage(), ApplicationFaultStage::FramePrepare);
            assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
            assert_eq!(fault.code(), ApplicationFaultCode::Rejected);
            assert_eq!(
                fault.reason(),
                ApplicationFaultReason::InvalidDecisionDeclaration(
                    DecisionDeclarationError::InvalidQuestions,
                )
            );
            assert!(server.requests().is_empty());
            assert!(observed.lock().unwrap().is_empty());
            mode.0.lock().unwrap().as_ref().unwrap().set(true).unwrap();
            assert_eq!(
                app.react(false).unwrap().await.unwrap(),
                ReactionOutcome::Completed
            );
            app.shutdown().await.unwrap();
            assert_eq!(server.requests().len(), 1);
            let outputs = observed.lock().unwrap();
            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].result.as_ref().unwrap().choice, "payments");
        }
    }
}

type FrozenOutput = Arc<Mutex<Vec<(bool, EventContext, QuestionOutcome<ChoiceAnswer>)>>>;

#[component]
fn changing_question(mode: ModeProbe, output: FrozenOutput) -> Component {
    let updated = use_signal(|| false);
    *mode.0.lock().unwrap() = Some(updated.clone());
    let updated = updated.with(|updated| *updated).unwrap();
    let key = if updated { "new-team" } else { "original-team" };
    view! {
        question_updated { "{updated}" }
        Decision {
            name: "owner",
            question: Choice::new("Who owns this ticket?", BTreeMap::from([
                (key.to_owned(), json!("The team responsible for the ticket")),
            ])),
            on_result: move |context, outcome: QuestionOutcome<ChoiceAnswer>| {
                output.lock().unwrap().push((updated, context, outcome));
                Ok::<(), Infallible>(())
            },
        }
    }
}

#[tokio::test]
async fn an_inflight_answer_uses_the_submitted_question_and_handler_after_rerender() {
    for native in [false, true] {
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = Server::start([
            Reply::Paused {
                answers: json!({"owner":{"type":"choice","choice":"original-team"}}),
                arrived: arrived.clone(),
                release: release.clone(),
            },
            Reply::Answers(json!({"owner":{"type":"choice","choice":"new-team"}})),
        ])
        .await;
        let port: Box<dyn ReactionPort> = if native {
            Box::new(server.system1())
        } else {
            Box::new(server.responses())
        };
        let mode = ModeProbe::default();
        let root_mode = mode.clone();
        let output = FrozenOutput::default();
        let observed = output.clone();
        let mut app = Application::mount(
            move || changing_question(root_mode.clone(), output.clone()),
            port,
        )
        .unwrap();
        let reaction = app.react(false).unwrap();
        let reaction_id = reaction.id();
        tokio::time::timeout(std::time::Duration::from_secs(3), arrived.notified())
            .await
            .unwrap();
        mode.0.lock().unwrap().as_ref().unwrap().set(true).unwrap();
        assert!(app.prepare().await.unwrap().is_continue());
        let latest = app.current_projection();
        let questions =
            serde_json::to_value(latest.projection().decision().unwrap().questions()).unwrap();
        assert!(questions["owner"]["criteria"].get("new-team").is_some());
        release.notify_one();
        assert_eq!(reaction.await.unwrap(), ReactionOutcome::Completed);
        assert_eq!(server.requests().len(), 1);
        {
            let outputs = observed.lock().unwrap();
            assert_eq!(outputs.len(), 1);
            assert!(
                !outputs[0].0,
                "the submitted handler captured the original view"
            );
            assert_eq!(outputs[0].1.reaction_id(), Some(reaction_id));
            assert_eq!(
                outputs[0].2.result.as_ref().unwrap().choice,
                "original-team"
            );
        }

        // A separate explicit reaction uses the new criteria and the new handler.
        let next = app.react(false).unwrap();
        let next_id = next.id();
        assert_eq!(next.await.unwrap(), ReactionOutcome::Completed);
        app.shutdown().await.unwrap();
        let outputs = observed.lock().unwrap();
        assert_eq!(outputs.len(), 2);
        assert!(outputs[1].0);
        assert_eq!(outputs[1].1.reaction_id(), Some(next_id));
        assert_eq!(outputs[1].2.result.as_ref().unwrap().choice, "new-team");
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        if native {
            let history = requests[1].body["state"]["replay"].as_array().unwrap();
            let catalog = history
                .iter()
                .find(|item| item["kind"] == "decision_request")
                .unwrap();
            assert!(catalog["payload"]["questions"]["owner"]["criteria"]
                .get("original-team")
                .is_some());
            assert!(requests[1].body["questions"]["owner"]["criteria"]
                .get("new-team")
                .is_some());
        }
    }
}
