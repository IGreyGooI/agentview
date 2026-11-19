//! Alibaba Cloud Qwen Realtime WebSocket text, tools and audio.

mod audio;
mod media;
mod output;
mod request;
mod transport;

use std::{
    collections::VecDeque,
    num::{NonZeroU128, NonZeroU64},
    task::Poll,
};

use async_trait::async_trait;
use futures::{future::BoxFuture, stream, FutureExt};
use reqwest::{header::HeaderValue, Client};
use serde_json::Value;
use url::{Host, Url};

use crate::{
    component::execution::{
        ContextWindow, Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile,
        FrameRevision, ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault as Fault,
        ReactionPortFaultCode as Code, ReactionPortFaultKind, ReactionPortFaultReason as Reason,
        SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
    },
    transcript::{AssistantTextStatus, CanonicalInputItem},
};

pub use audio::{
    QwenRealtimeAudioEvent, QwenRealtimeResponseStatus, QWEN_REALTIME_INPUT_SAMPLE_RATE_HZ,
    QWEN_REALTIME_MAX_INPUT_CHUNK_BYTES, QWEN_REALTIME_OUTPUT_SAMPLE_RATE_HZ,
};
use media::Media;
pub use media::{QwenRealtimeMediaEvents, QwenRealtimeMediaInput, QwenRealtimeServerEvents};
use output::Decoder;
use transport::Socket;

use super::ModelSpec;

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Invalid local configuration, with no URL or credential in the diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QwenRealtimeConfigError {
    #[error("Qwen Realtime requires a wss endpoint or a ws endpoint on loopback")]
    InvalidEndpoint,
    #[error("Qwen Realtime endpoint model query does not match the configured model")]
    ModelMismatch,
    #[error("Qwen Realtime API key is empty or invalid")]
    InvalidApiKey,
    #[error("Qwen Realtime client initialization failed")]
    TransportInitialization,
    #[error("Qwen Realtime target initialization failed")]
    TargetInitialization,
}

/// A Frame-native port that retains its WebSocket across compatible reactions.
///
/// Each Frame starts one explicit response. The runtime receives text deltas and
/// structured native tool calls as they are admitted. The next Frame supplies
/// their tool results and the latest Component view on the same session.
pub struct QwenRealtimeProvider {
    endpoint: Url,
    authorization: HeaderValue,
    client: Client,
    model: ModelSpec,
    identity: TargetIdentity,
    epoch: TargetEpoch,
    profile: FrameProfile,
    retained: Option<Retained>,
    terminal: Option<Fault>,
    media: Option<Media>,
    voice: bool,
    follow_up: bool,
}

impl QwenRealtimeProvider {
    /// The endpoint can include a matching `model` query, or the client will add it.
    pub fn new(
        endpoint: &str,
        api_key: &str,
        model: ModelSpec,
    ) -> Result<Self, QwenRealtimeConfigError> {
        let endpoint = http_endpoint(endpoint, &model)?;
        if api_key.is_empty() || api_key.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(QwenRealtimeConfigError::InvalidApiKey);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| QwenRealtimeConfigError::InvalidApiKey)?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| QwenRealtimeConfigError::TransportInitialization)?;
        let mut identity = [0; 16];
        getrandom::fill(&mut identity)
            .map_err(|_| QwenRealtimeConfigError::TargetInitialization)?;
        let identity = NonZeroU128::new(u128::from_ne_bytes(identity))
            .map(TargetIdentity::new)
            .ok_or(QwenRealtimeConfigError::TargetInitialization)?;
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: MAX_REQUEST_BYTES,
                max_component_bytes: 2 * 1024 * 1024,
                context_window: ContextWindow::Tokens(model.context_window_tokens()),
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true)
                .with_native_tool_calls(true)
                .with_text_delta_output(true),
        );
        Ok(Self {
            endpoint,
            authorization,
            client,
            model,
            identity,
            epoch: TargetEpoch::new(NonZeroU64::MIN),
            profile,
            retained: None,
            terminal: None,
            media: None,
            voice: false,
            follow_up: false,
        })
    }

    /// Enables a single long-lived voice reaction. The media host supplies
    /// utterances while Application renders the view and dispatches tools.
    pub fn with_audio(mut self) -> (Self, QwenRealtimeMediaInput, QwenRealtimeMediaEvents) {
        let (media, input, events) = Media::new();
        self.media = Some(media);
        self.voice = true;
        (self, input, events)
    }

    fn connect(&self) -> BoxFuture<'static, Result<Socket, Fault>> {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let authorization = self.authorization.clone();
        async move { transport::connect(&client, &endpoint, authorization).await }.boxed()
    }

    fn lose_continuity(&mut self) {
        self.retained = None;
        if let Some(epoch) = self.epoch.get().checked_add(1) {
            self.epoch = TargetEpoch::new(epoch);
        } else {
            self.terminal = Some(Fault::terminal(Code::Internal, Reason::Declaration));
        }
    }
}

fn http_endpoint(endpoint: &str, model: &ModelSpec) -> Result<Url, QwenRealtimeConfigError> {
    use QwenRealtimeConfigError::{InvalidEndpoint, ModelMismatch};

    let mut url = Url::parse(endpoint).map_err(|_| InvalidEndpoint)?;
    let loopback = match url.host().ok_or(InvalidEndpoint)? {
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
        Host::Domain(domain) => domain == "localhost",
    };
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" if loopback => "http",
        _ => return Err(InvalidEndpoint),
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(InvalidEndpoint);
    }
    url.set_scheme(scheme).map_err(|_| InvalidEndpoint)?;
    let models: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "model")
        .collect();
    match models.as_slice() {
        [] => {
            url.query_pairs_mut().append_pair("model", model.id());
        }
        [(_, value)] if value == model.id() => {}
        _ => return Err(ModelMismatch),
    }
    Ok(url)
}

struct Session {
    socket: Socket,
    id: String,
    media: Option<Media>,
}

struct Retained {
    session: Session,
    revision: FrameRevision,
    output: Vec<CanonicalInputItem>,
}

#[async_trait]
impl ReactionPort for QwenRealtimeProvider {
    fn declare(&mut self) -> Result<TargetDeclaration, Fault> {
        if let Some(fault) = self.terminal {
            return Err(fault);
        }
        Ok(match &self.retained {
            Some(retained) => TargetDeclaration::resume(retained.revision, self.profile.clone()),
            None => TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
        })
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declare()?)?;
        let ephemeral = !frame.submission().ephemeral().is_empty();
        if self.voice && matches!(frame.basis(), FrameBasis::Full) && self.retained.is_some() {
            // Full replay cannot reconstruct committed media held by the remote
            // session. Reject a replacement when that session is still needed.
            return Err(Fault::retryable(Code::Rejected, Reason::RequestPreparation).into());
        }
        if matches!(frame.basis(), FrameBasis::DeltaFrom(_))
            && ((!self.voice && ephemeral)
                || self
                    .retained
                    .as_ref()
                    .is_none_or(|retained| retained.output != frame.submission().replay()))
        {
            // The existing session cannot safely represent this exact input.
            // Ask the runtime to prepare a Full Frame without consuming receipts.
            if self.voice {
                return Err(transport::protocol_fault().into());
            }
            self.lose_continuity();
            return Err(SubmitFault::ContinuityChanged);
        }

        if self.voice && self.retained.is_none() && self.media.is_none() {
            return Err(Fault::terminal(Code::Unavailable, Reason::Declaration).into());
        }
        let commands = request::encode(&frame, MAX_REQUEST_BYTES, self.voice)?;
        let tool_feedback = self.follow_up || !frame.submission().staged_inputs().is_empty();
        let mut response_bytes = ResponseBytes::default();
        let fresh = if matches!(frame.basis(), FrameBasis::Full) {
            let mut socket = self.connect().await?;
            let created = response_bytes.receive(&mut socket).await?;
            let id = session_id(&created, "session.created", self.model.id())?.to_owned();
            Some((
                Session {
                    socket,
                    id,
                    media: None,
                },
                created,
            ))
        } else {
            None
        };

        // The first poll that can send Frame content returns the fact stream
        // in the same poll. Preceding awaits only establish a WebSocket.
        let revision = frame.revision();
        let mut fresh = fresh;
        let mut commands = Some(commands);
        let (mut started, first) = futures::future::poll_fn(|cx| {
            frame.check_handoff_precondition(&self.declare()?)?;
            self.follow_up = false;
            let retained = self.retained.take();
            let fresh = fresh.take().map(|(mut session, created)| {
                session.media = self.media.take();
                (session, Some(created))
            });
            let (session, created) = fresh
                .or_else(|| retained.map(|retained| (retained.session, None)))
                .expect("validated Qwen session at handoff");
            let mut started = start_response(
                session,
                commands.take().expect("handoff polled once"),
                response_bytes,
                created,
                tool_feedback,
            )
            .boxed();
            let first = started.as_mut().poll(cx);
            Poll::Ready(Ok::<_, SubmitFault>((started, first)))
        })
        .await?;

        let pending = async move {
            match first {
                Poll::Ready(result) => result,
                Poll::Pending => started.as_mut().await,
            }
        }
        .boxed();
        let decoder = if self.voice {
            Decoder::with_audio()
        } else {
            Decoder::default()
        };
        let discard_session = !self.voice && ephemeral;
        let in_flight = InFlight {
            provider: self,
            pending: Some(pending),
            session: None,
            response_bytes,
            revision,
            discard_session,
            decoder,
            output: Vec::new(),
            facts: VecDeque::new(),
            settled: false,
            ended_without_response: false,
            transient: Vec::new(),
        };
        Ok(Box::pin(stream::unfold(
            in_flight,
            |mut flight| async move { flight.next().await.map(|fact| (fact, flight)) },
        )))
    }

    async fn next_session_frame(&mut self, feedback_pending: bool) -> Result<bool, Fault> {
        if !self.voice {
            return Ok(false);
        }
        let retained = self
            .retained
            .as_mut()
            .ok_or_else(transport::protocol_fault)?;
        if feedback_pending
            || retained
                .output
                .iter()
                .any(|item| matches!(item, CanonicalInputItem::ToolCall { .. }))
        {
            self.follow_up = true;
            return Ok(true);
        }
        let session = &mut retained.session;
        let result = session
            .media
            .as_mut()
            .expect("voice session has media")
            .wait_for_turn(&mut session.socket)
            .await;
        if let Err(fault) = result {
            self.terminal = Some(Fault::terminal(fault.code(), fault.reason()));
        }
        result
    }

    async fn finish_session(&mut self) {
        if self.voice && self.media.is_none() {
            self.shutdown().await;
            self.terminal
                .get_or_insert_with(|| Fault::terminal(Code::Unavailable, Reason::Declaration));
        }
    }

    async fn shutdown(&mut self) {
        if let Some(mut retained) = self.retained.take() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                retained.session.socket.close(None),
            )
            .await;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ResponseBytes(usize);

impl ResponseBytes {
    async fn receive(&mut self, socket: &mut Socket) -> Result<Value, Fault> {
        let (event, bytes) = transport::receive(socket).await?;
        self.admit(event, bytes)
    }

    fn admit(&mut self, event: Value, bytes: usize) -> Result<Value, Fault> {
        self.0 = self
            .0
            .checked_add(bytes)
            .filter(|bytes| *bytes <= MAX_RESPONSE_BYTES)
            .ok_or_else(|| Fault::retryable(Code::Limit, Reason::OutputLimit))?;
        if event["type"] == "error" {
            return Err(output::server_error(&event["error"]));
        }
        Ok(event)
    }
}

async fn start_response(
    mut session: Session,
    commands: request::Commands,
    mut bytes: ResponseBytes,
    created: Option<Value>,
    tool_feedback: bool,
) -> Result<StartedResponse, Fault> {
    if let (Some(media), Some(created)) = (&mut session.media, created) {
        media.observe(&created)?;
    }
    transport::send(&mut session.socket, commands.update).await?;
    let updated = next_ack(&mut bytes, &mut session, "session.updated").await?;
    let modalities = updated["session"]["modalities"].as_array();
    let audio = session.media.is_some();
    let expected_modalities = modalities.is_some_and(|modes| {
        modes.len() == if audio { 2 } else { 1 }
            && modes.iter().any(|mode| mode == "text")
            && (!audio || modes.iter().any(|mode| mode == "audio"))
    });
    if updated["session"]["id"] != session.id
        || !expected_modalities
        || !updated["session"]["turn_detection"].is_null()
    {
        return Err(transport::protocol_fault());
    }
    for item in commands.items {
        create_item(&mut session, &mut bytes, item).await?;
    }
    if let Some(media) = &mut session.media {
        if !tool_feedback {
            if !media.wait_for_turn(&mut session.socket).await? {
                return Ok(StartedResponse {
                    session,
                    bytes,
                    transient: Vec::new(),
                    started: false,
                });
            }
            media.begin_turn();
        }
    }
    let mut transient = Vec::with_capacity(commands.ephemeral.len());
    for item in commands.ephemeral {
        let id = create_item(&mut session, &mut bytes, item).await?;
        if session.media.is_some() {
            transient.push(id);
        }
    }
    transport::send(
        &mut session.socket,
        r#"{"type":"response.create"}"#.to_owned(),
    )
    .await?;
    Ok(StartedResponse {
        session,
        bytes,
        transient,
        started: true,
    })
}

struct StartedResponse {
    session: Session,
    bytes: ResponseBytes,
    transient: Vec<String>,
    started: bool,
}

async fn create_item(
    session: &mut Session,
    bytes: &mut ResponseBytes,
    item: String,
) -> Result<String, Fault> {
    transport::send(&mut session.socket, item).await?;
    let ack = next_ack(bytes, session, "conversation.item.created").await?;
    ack["item"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(transport::protocol_fault)
}

async fn next_ack(
    bytes: &mut ResponseBytes,
    session: &mut Session,
    expected: &str,
) -> Result<Value, Fault> {
    loop {
        let (event, count) = transport::receive(&mut session.socket).await?;
        if let Some(media) = &mut session.media {
            media.observe(&event)?;
        }
        let event = bytes.admit(event, count)?;
        match event["type"].as_str() {
            Some("conversation.item.created")
                if event["item"]["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| part["type"] == "input_audio")
                }) => {}
            Some(kind) if kind == expected => return Ok(event),
            Some("rate_limits.updated") => {}
            Some(kind) if session.media.is_some() && !kind.starts_with("response.") => {}
            _ => return Err(transport::protocol_fault()),
        }
    }
}

fn media_event(kind: &str) -> bool {
    kind.starts_with("input_audio_buffer.")
        || kind.starts_with("conversation.item.input_audio_transcription.")
}

fn session_id<'a>(event: &'a Value, expected: &str, model: &str) -> Result<&'a str, Fault> {
    if event["type"] != expected || event["session"]["model"] != model {
        return Err(transport::protocol_fault());
    }
    event["session"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(transport::protocol_fault)
}

struct InFlight<'a> {
    provider: &'a mut QwenRealtimeProvider,
    pending: Option<BoxFuture<'static, Result<StartedResponse, Fault>>>,
    session: Option<Session>,
    response_bytes: ResponseBytes,
    revision: FrameRevision,
    discard_session: bool,
    decoder: Decoder,
    output: Vec<CanonicalInputItem>,
    facts: VecDeque<ProviderFact>,
    settled: bool,
    ended_without_response: bool,
    transient: Vec<String>,
}

impl InFlight<'_> {
    async fn next(&mut self) -> Option<Result<ProviderFact, Fault>> {
        if self.settled {
            return None;
        }
        match self.read_fact().await {
            Ok(Some(fact)) => Some(Ok(fact)),
            Ok(None) => {
                self.settled = true;
                if self.discard_session {
                    self.provider.lose_continuity();
                } else {
                    self.provider.retained = self.session.take().map(|session| Retained {
                        session,
                        revision: self.revision,
                        output: std::mem::take(&mut self.output),
                    });
                }
                None
            }
            Err(fault) => {
                self.settled = true;
                self.provider.lose_continuity();
                if fault.kind() == ReactionPortFaultKind::Terminal {
                    self.provider.terminal = Some(fault);
                }
                Some(Err(fault))
            }
        }
    }

    async fn read_fact(&mut self) -> Result<Option<ProviderFact>, Fault> {
        if let Some(pending) = self.pending.take() {
            let StartedResponse {
                session,
                bytes,
                transient,
                started,
            } = pending.await?;
            self.session = Some(session);
            self.response_bytes = bytes;
            self.transient = transient;
            if !started {
                self.ended_without_response = true;
                return Ok(Some(ProviderFact::ReactionCompleted { primary_text: None }));
            }
        }
        loop {
            if let Some(fact) = self.facts.pop_front() {
                match &fact {
                    ProviderFact::TextSealed { text, phase, .. } => {
                        self.output.push(CanonicalInputItem::AssistantText {
                            text: text.clone(),
                            phase: *phase,
                            status: AssistantTextStatus::Sealed,
                        });
                    }
                    ProviderFact::ToolCall { call, .. } => {
                        self.output.push(CanonicalInputItem::ToolCall {
                            call_id: call.call_id().to_owned(),
                            name: call.name().to_owned(),
                            raw_arguments: call.raw_arguments().to_owned(),
                        });
                    }
                    _ => {}
                }
                return Ok(Some(fact));
            }
            if self.decoder.is_complete() || self.ended_without_response {
                return Ok(None);
            }
            let session = self.session.as_mut().expect("response started");
            let event = if let Some(media) = &mut session.media {
                let (event, bytes) = media.receive(&mut session.socket, true).await?;
                self.response_bytes.admit(event, bytes)?
            } else {
                self.response_bytes.receive(&mut session.socket).await?
            };
            if event["type"].as_str().is_some_and(media_event) {
                continue;
            }
            self.facts.extend(self.decoder.on_event(event)?);
            if self.decoder.is_complete() {
                for id in std::mem::take(&mut self.transient) {
                    transport::send(
                        &mut session.socket,
                        serde_json::json!({
                            "type": "conversation.item.delete", "item_id": id
                        })
                        .to_string(),
                    )
                    .await?;
                    let ack = next_ack(
                        &mut self.response_bytes,
                        session,
                        "conversation.item.deleted",
                    )
                    .await?;
                    if ack["item_id"] != id {
                        return Err(transport::protocol_fault());
                    }
                }
            }
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.provider.lose_continuity();
        }
    }
}
