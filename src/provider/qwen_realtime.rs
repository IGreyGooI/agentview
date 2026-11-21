//! Alibaba Cloud Qwen Realtime WebSocket text, tools and audio.

mod audio;
pub(crate) mod media;
mod output;
mod request;
pub(crate) mod transport;
pub(crate) mod voice_application;

use std::{
    collections::VecDeque,
    num::{NonZeroU128, NonZeroU64},
    sync::atomic::{AtomicU64, Ordering},
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
pub use voice_application::{QwenRealtimeVoiceApplication, QwenRealtimeVoiceInput};

use super::ModelSpec;

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
static NEXT_CONTEXT_RESPONSE: AtomicU64 = AtomicU64::new(1);
const MAX_PENDING_CONTEXT_CUES: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Text,
    ManualAudio,
    SmartTurnV2,
}

impl Mode {
    fn audio(self) -> bool {
        self != Self::Text
    }
}

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
/// The runtime receives deltas and structured native tool calls as they are
/// admitted. The next Frame supplies tool results and the latest Component view
/// on the same session.
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
    mode: Mode,
    follow_up: bool,
    context_frame_pending: bool,
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
            mode: Mode::Text,
            follow_up: false,
            context_frame_pending: false,
        })
    }

    /// Enables a single long-lived voice reaction. The media host supplies
    /// utterances while Application renders the view and dispatches tools.
    pub fn with_audio(mut self) -> (Self, QwenRealtimeMediaInput, QwenRealtimeMediaEvents) {
        let (media, input, events) = Media::new(Mode::ManualAudio);
        self.media = Some(media);
        self.mode = Mode::ManualAudio;
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
    frame: Option<FrameRevision>,
    pending_events: VecDeque<(Value, Option<FrameRevision>)>,
    context_setup: bool,
    pending_context_cues: Vec<String>,
}

impl Session {
    fn retain_context_cue(&mut self, item_id: String) {
        // The same optional cue may reach a normal response boundary after an
        // earlier delete was refused. Keep ownership until its matching ACK;
        // repeated observations must not enqueue duplicate cleanup commands.
        if !self.pending_context_cues.contains(&item_id) {
            // Rejected cues can accumulate when more TaskCompleted controls
            // arrive before any real response ends. Bound only this bookkeeping:
            // evict its oldest observation, never a new generation or user input.
            if self.pending_context_cues.len() == MAX_PENDING_CONTEXT_CUES {
                self.pending_context_cues.remove(0);
                self.observe_context_refresh_rejected(
                    "cue_cleanup_tracking_evicted",
                    "conversation.item.deleted",
                );
            }
            self.pending_context_cues.push(item_id);
        }
    }

    fn observe_context_delete(&mut self, event: &Value) {
        if event["type"] == "conversation.item.deleted" {
            self.pending_context_cues
                .retain(|id| Some(id.as_str()) != event["item_id"].as_str());
        }
    }

    fn user_turn_owns_context_response(&self) -> bool {
        self.pending_events.iter().any(|(event, _)| {
            event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("response."))
        }) || self
            .media
            .as_ref()
            .is_some_and(|media| !media.context_response_available())
    }

    fn observe_context_response(&self, revision: FrameRevision, requested: bool, reason: &str) {
        if let Some(media) = &self.media {
            let mut event = serde_json::json!({
                "type": if requested { "agentview.context_response_requested" } else { "agentview.context_response_skipped" },
                "provider_session_id": self.id,
                "frame_revision": format!("{revision:?}"),
            });
            event[if requested { "source" } else { "reason" }] = Value::String(reason.to_owned());
            media.publish_context_refresh_event(event);
        }
    }

    fn observe_context_refresh_rejected(&self, reason: &str, ack: &str) {
        if let Some(media) = &self.media {
            media.publish_context_refresh_event(serde_json::json!({
                "type": "agentview.context_refresh_rejected",
                "provider_session_id": self.id,
                "reason": reason,
                "ack": ack,
            }));
        }
    }
}

struct Retained {
    session: Session,
    revision: FrameRevision,
    output: Vec<CanonicalInputItem>,
}

#[async_trait]
impl ReactionPort for QwenRealtimeProvider {
    fn supports_context_refresh(&self) -> bool {
        self.mode == Mode::SmartTurnV2
    }
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
        if self.mode.audio() && matches!(frame.basis(), FrameBasis::Full) && self.retained.is_some()
        {
            // Full replay cannot reconstruct committed media held by the remote
            // session. Reject a replacement when that session is still needed.
            return Err(Fault::retryable(Code::Rejected, Reason::RequestPreparation).into());
        }
        if matches!(frame.basis(), FrameBasis::DeltaFrom(_))
            && ((!self.mode.audio() && ephemeral)
                || self
                    .retained
                    .as_ref()
                    .is_none_or(|retained| retained.output != frame.submission().replay()))
        {
            // The existing session cannot safely represent this exact input.
            // Ask the runtime to prepare a Full Frame without consuming receipts.
            if self.mode.audio() {
                return Err(transport::protocol_fault().into());
            }
            self.lose_continuity();
            return Err(SubmitFault::ContinuityChanged);
        }

        if self.mode.audio() && self.retained.is_none() && self.media.is_none() {
            return Err(Fault::terminal(Code::Unavailable, Reason::Declaration).into());
        }
        let commands = request::encode(&frame, MAX_REQUEST_BYTES, self.mode)?;
        let tool_feedback = self.follow_up || !frame.submission().staged_inputs().is_empty();
        let optional_context = self.context_frame_pending && !tool_feedback;
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
                    frame: None,
                    pending_events: VecDeque::new(),
                    context_setup: false,
                    pending_context_cues: Vec::new(),
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
            self.context_frame_pending = false;
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
                self.mode,
                revision,
                optional_context,
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
        let decoder = if self.mode.audio() {
            Decoder::with_audio(self.mode == Mode::SmartTurnV2)
        } else {
            Decoder::default()
        };
        let discard_session = !self.mode.audio() && ephemeral;
        let expect_immediate_response = self.mode != Mode::SmartTurnV2 || tool_feedback;
        let in_flight = InFlight {
            provider: self,
            pending: Some(pending),
            session: None,
            response_bytes,
            expect_immediate_response,
            revision,
            discard_session,
            decoder,
            output: Vec::new(),
            facts: VecDeque::new(),
            settled: false,
            ended_without_response: false,
            announced_frame: false,
            context_boundary: false,
            transient: Vec::new(),
        };
        Ok(Box::pin(stream::unfold(
            in_flight,
            |mut flight| async move { flight.next().await.map(|fact| (fact, flight)) },
        )))
    }

    async fn next_session_frame(&mut self, feedback_pending: bool) -> Result<bool, Fault> {
        if !self.mode.audio() {
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
        if self.mode == Mode::SmartTurnV2 {
            // Install the next Frame before forwarding any queued microphone audio.
            return Ok(!session
                .media
                .as_ref()
                .expect("voice session has media")
                .is_finished());
        }
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
        if self.mode.audio() && self.media.is_none() {
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
    mode: Mode,
    revision: FrameRevision,
    optional_context: bool,
) -> Result<StartedResponse, Fault> {
    let first_frame = created.is_some();
    if let (Some(media), Some(created)) = (&mut session.media, created) {
        media.observe(&created)?;
    }
    let (deferred_context, context_instruction) = session
        .media
        .as_mut()
        .map(Media::begin_context_frame)
        .unwrap_or_default();
    let context_response = context_instruction.is_some();
    session.context_setup = optional_context || deferred_context;
    transport::send(&mut session.socket, commands.update).await?;
    let updated = match next_ack(
        &mut bytes,
        &mut session,
        "session.updated",
        None,
        optional_context,
    )
    .await?
    {
        Some(updated) => updated,
        None => {
            session.context_setup = false;
            if context_response {
                session.observe_context_response(revision, false, "context_rejected");
            }
            let rejected_frame = session.frame;
            return Ok(StartedResponse {
                session,
                bytes,
                transient: Vec::new(),
                started: true,
                rejected_frame,
                response_requested: false,
            });
        }
    };
    let modalities = updated["session"]["modalities"].as_array();
    let audio = session.media.is_some();
    let expected_modalities = modalities.is_some_and(|modes| {
        modes.len() == if audio { 2 } else { 1 }
            && modes.iter().any(|mode| mode == "text")
            && (!audio || modes.iter().any(|mode| mode == "audio"))
    });
    let turn_detection = &updated["session"]["turn_detection"];
    let expected_detection = if mode == Mode::SmartTurnV2 {
        turn_detection["type"] == "smart_turn_v2" || (!first_frame && turn_detection.is_null())
    } else {
        turn_detection.is_null()
    };
    // The provider owns the acknowledged configuration. With a mismatched
    // session or mode, sending microphone data could expose the Frame's input
    // to a different session or leave it waiting for a manual commit. Reject
    // initial exchange before forwarding its microphone input. During an
    // optional refresh, retain the established session and ignore only this
    // unconfirmed configuration instead of stopping accepted voice.
    if updated["session"]["id"] != session.id || !expected_modalities || !expected_detection {
        if optional_context {
            session.observe_context_refresh_rejected("configuration_ack", "session.updated");
            tracing::warn!(
                event = "qwen_context_refresh_rejected",
                reason = "configuration_ack",
                "optional context update ignored; voice continues"
            );
            let rejected_frame = session.frame;
            session.context_setup = false;
            if context_response {
                session.observe_context_response(revision, false, "context_rejected");
            }
            return Ok(StartedResponse {
                session,
                bytes,
                transient: Vec::new(),
                started: true,
                rejected_frame,
                response_requested: false,
            });
        }
        return Err(transport::protocol_fault());
    }
    let mut rejected_items = 0usize;
    for item in commands.items {
        if create_item(&mut session, &mut bytes, item, optional_context)
            .await?
            .is_none()
        {
            rejected_items += 1;
        }
    }
    if let Some(media) = session.media.as_mut().filter(|_| mode == Mode::ManualAudio) {
        if !tool_feedback {
            if !media.wait_for_turn(&mut session.socket).await? {
                return Ok(StartedResponse {
                    session,
                    bytes,
                    transient: Vec::new(),
                    started: false,
                    rejected_frame: None,
                    response_requested: false,
                });
            }
            media.begin_turn();
        }
    }
    let mut transient = Vec::with_capacity(commands.ephemeral.len());
    for item in commands.ephemeral {
        let id = create_item(&mut session, &mut bytes, item, optional_context).await?;
        if id.is_none() {
            rejected_items += 1;
        }
        if let Some(id) = id.filter(|_| session.media.is_some()) {
            transient.push(id);
        }
    }
    // The whole context Frame owns callbacks only after all its item ACKs.
    // Until then previously forwarded PCM may still start an old-Frame reply.
    session.frame = Some(revision);
    session.context_setup = false;
    if optional_context || deferred_context {
        // This writer owns each command/ACK boundary. Do not claim the task
        // snapshot reached the model if even one optional item was refused;
        // report incomplete delivery without interrupting the audio owner.
        if rejected_items == 0 {
            tracing::info!(event = "qwen_context_refresh_applied", provider_session_id = %session.id, frame_revision = ?revision, deferred_context,
                "requested context Frame acknowledged");
            if let Some(media) = &session.media {
                media.publish_context_refresh_event(serde_json::json!({
                    "type": "agentview.context_refresh_applied",
                    "provider_session_id": session.id,
                    // FrameRevision is deliberately opaque; retain its debug
                    // token solely for internal observation correlation.
                    "frame_revision": format!("{revision:?}"),
                }));
            }
        } else {
            tracing::warn!(
                event = "qwen_context_refresh_incomplete",
                rejected_items,
                "optional context items refused; voice continues"
            );
            if let Some(media) = &session.media {
                media.publish_context_refresh_event(serde_json::json!({
                    "type": "agentview.context_refresh_incomplete",
                    "provider_session_id": session.id,
                    "frame_revision": format!("{revision:?}"),
                    "rejected_items": rejected_items,
                }));
            }
        }
    }
    let ordinary_generation = mode != Mode::SmartTurnV2 || tool_feedback;
    // The server can accept user audio forwarded on the former Frame while
    // this context awaits its ACKs. A buffered automatic response or a live
    // utterance owns that turn; drop only this optional generation intent.
    let mut user_turn_won = context_response && session.user_turn_owns_context_response();
    let mut context_cue = None;
    let mut cue_rejected = false;
    if let Some(instruction) = context_instruction.filter(|_| !user_turn_won && rejected_items == 0)
    {
        let cue = serde_json::json!({"type":"conversation.item.create", "item": {
            "type":"message", "role":"user",
            "content":[{"type":"input_text", "text":instruction}],
        }})
        .to_string();
        context_cue = create_item(&mut session, &mut bytes, cue, true).await?;
        cue_rejected = context_cue.is_none();
        user_turn_won = session.user_turn_owns_context_response();
        if let Some(id) = &context_cue {
            if user_turn_won {
                // The cue belongs only to this optional reply. If real speech
                // wins its insertion race, retire it now instead of leaving a
                // standing instruction in later user or scheduled turns.
                delete_optional_context_item(&mut session, &mut bytes, id).await?;
                context_cue = None;
            } else {
                transient.push(id.clone());
            }
        }
    }
    let optional_generation =
        context_response && rejected_items == 0 && !cue_rejected && !user_turn_won;
    let response_requested = !user_turn_won && (ordinary_generation || optional_generation);
    if response_requested {
        let event = if optional_generation {
            let event_id = format!(
                "context_response_{}",
                NEXT_CONTEXT_RESPONSE.fetch_add(1, Ordering::Relaxed)
            );
            if let Some(media) = &mut session.media {
                media.expect_context_response(
                    event_id.clone(),
                    session.id.clone(),
                    revision,
                    context_cue,
                );
            }
            serde_json::json!({"type":"response.create", "event_id":event_id}).to_string()
        } else {
            r#"{"type":"response.create"}"#.to_owned()
        };
        transport::send(&mut session.socket, event).await?;
    }
    if context_response {
        session.observe_context_response(
            revision,
            response_requested && !cue_rejected,
            if user_turn_won {
                "user_turn"
            } else if rejected_items > 0 {
                "context_incomplete"
            } else if cue_rejected {
                "instruction_rejected"
            } else if ordinary_generation {
                "tool_continuation"
            } else {
                "explicit_context"
            },
        );
    }
    Ok(StartedResponse {
        session,
        bytes,
        transient,
        started: true,
        rejected_frame: None,
        response_requested,
    })
}

async fn delete_optional_context_item(
    session: &mut Session,
    bytes: &mut ResponseBytes,
    id: &str,
) -> Result<(), Fault> {
    transport::send(
        &mut session.socket,
        serde_json::json!({
            "type":"conversation.item.delete", "item_id":id,
        })
        .to_string(),
    )
    .await?;
    let ack = next_ack(bytes, session, "conversation.item.deleted", None, true).await?;
    if !ack.is_some_and(|ack| ack["item_id"] == id) {
        session.retain_context_cue(id.to_owned());
        session
            .observe_context_refresh_rejected("cue_cleanup_pending", "conversation.item.deleted");
    }
    Ok(())
}

struct StartedResponse {
    session: Session,
    bytes: ResponseBytes,
    transient: Vec<String>,
    started: bool,
    rejected_frame: Option<FrameRevision>,
    response_requested: bool,
}

async fn create_item(
    session: &mut Session,
    bytes: &mut ResponseBytes,
    item: String,
    optional: bool,
) -> Result<Option<String>, Fault> {
    let envelope: Value = serde_json::from_str(&item).map_err(|_| transport::protocol_fault())?;
    transport::send(&mut session.socket, item).await?;
    let Some(ack) = next_ack(
        bytes,
        session,
        "conversation.item.created",
        Some(&envelope["item"]),
        optional,
    )
    .await?
    else {
        return Ok(None);
    };
    match ack["item"]["id"].as_str().filter(|id| !id.is_empty()) {
        Some(id) => Ok(Some(id.to_owned())),
        None if optional => {
            // A malformed optional ACK does not establish a removable context
            // item. Omit only this item and keep the accepted microphone live.
            tracing::warn!(
                event = "qwen_context_refresh_rejected",
                reason = "item_ack",
                "optional context item ACK unusable; voice continues"
            );
            session.observe_context_refresh_rejected("item_ack", "conversation.item.created");
            Ok(None)
        }
        None => Err(transport::protocol_fault()),
    }
}

async fn next_ack(
    bytes: &mut ResponseBytes,
    session: &mut Session,
    expected: &str,
    expected_item: Option<&Value>,
    optional: bool,
) -> Result<Option<Value>, Fault> {
    loop {
        let (event, count) = transport::receive(&mut session.socket).await?;
        // An invalid optional update is refused by the provider while the
        // WebSocket remains usable. Drop only that context command; never turn
        // its refusal into cancellation of the already accepted voice stream.
        if optional
            && event["type"] == "error"
            && event["error"]["type"] == "invalid_request_error"
            && session
                .media
                .as_ref()
                .is_none_or(|media| !media.owns_control_error(&event))
        {
            session.observe_context_refresh_rejected("invalid_request", expected);
            if let Some(media) = &session.media {
                media.observe_context_rejection(event);
            }
            tracing::warn!(
                event = "qwen_context_refresh_rejected",
                command = expected,
                "optional context command refused; voice continues"
            );
            return Ok(None);
        }
        let (event, count) = match &mut session.media {
            Some(media) => media.admit(event, count)?,
            None => (event, count),
        };
        let event = bytes.admit(event, count)?;
        session.observe_context_delete(&event);
        match event["type"].as_str() {
            Some("conversation.item.created")
                if event["item"]["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| part["type"] == "input_audio")
                }) => {}
            Some(kind) if kind == expected => {
                // Automatic speech may create an assistant/tool item while a
                // client context item awaits ACK. That event cannot confirm
                // the client item; ignore it and retain the real ACK ordering.
                if expected_item.is_some_and(|item| {
                    ["type", "role"].iter().any(|key| {
                        event["item"][key].is_string() && event["item"][key] != item[key]
                    })
                }) {
                    continue;
                }
                return Ok(Some(event));
            }
            Some(kind)
                if (optional || session.context_setup)
                    && session.media.is_some()
                    && kind.starts_with("response.") =>
            {
                // Audio forwarded on the previous Frame can start generation
                // while an update or item ACK is in flight. Preserve all model
                // events and the last acknowledged Frame for response.created;
                // the fact stream replays them once, with their original bindings.
                let frame = (kind == "response.created")
                    .then_some(session.frame)
                    .flatten();
                session.pending_events.push_back((event, frame));
            }
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
    expect_immediate_response: bool,
    revision: FrameRevision,
    discard_session: bool,
    decoder: Decoder,
    output: Vec<CanonicalInputItem>,
    facts: VecDeque<ProviderFact>,
    settled: bool,
    ended_without_response: bool,
    announced_frame: bool,
    context_boundary: bool,
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
                    self.provider.context_frame_pending = self.context_boundary;
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
                rejected_frame,
                response_requested,
            } = pending.await?;
            self.session = Some(session);
            self.response_bytes = bytes;
            self.transient = transient;
            self.expect_immediate_response |= response_requested;
            if let Some(revision) = rejected_frame {
                self.revision = revision;
                self.facts
                    .push_back(ProviderFact::ContextRefreshRejected { revision });
            }
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
            let (event, response_frame) = if let Some(event) = session.pending_events.pop_front() {
                event
            } else if let Some(media) = &mut session.media {
                let responding = self.expect_immediate_response || self.decoder.has_started();
                let (event, bytes) = media.receive(&mut session.socket, responding).await?;
                (self.response_bytes.admit(event, bytes)?, None)
            } else {
                (
                    self.response_bytes.receive(&mut session.socket).await?,
                    None,
                )
            };
            self.session
                .as_mut()
                .expect("live session")
                .observe_context_delete(&event);
            if event["type"] == "agentview.context_response_rejected" {
                if event["cancel_wait"] == true {
                    self.expect_immediate_response = false;
                    if let Some(id) = event["cue_item_id"].as_str() {
                        delete_optional_context_item(
                            self.session.as_mut().expect("live session"),
                            &mut self.response_bytes,
                            id,
                        )
                        .await?;
                        self.transient.retain(|item| item != id);
                    }
                }
                continue;
            }
            if event["type"] == "agentview.context_refresh" {
                self.cleanup_context(true).await?;
                // Cleanup ACKs can race an automatic response too. Finish that
                // response on this Frame instead of abandoning its callbacks.
                if !self
                    .session
                    .as_ref()
                    .expect("live session")
                    .pending_events
                    .is_empty()
                {
                    continue;
                }
                self.ended_without_response = true;
                self.context_boundary = true;
                return Ok(Some(ProviderFact::ContextRefreshRequested));
            }
            if event["type"] == "agentview.input_finished" {
                self.ended_without_response = true;
                return Ok(Some(ProviderFact::ReactionCompleted { primary_text: None }));
            }
            if event["type"].as_str().is_some_and(media_event) {
                continue;
            }
            if event["type"] == "response.created"
                && self.provider.mode == Mode::SmartTurnV2
                && !self.announced_frame
            {
                self.announced_frame = true;
                self.facts.push_back(ProviderFact::ResponseFrame {
                    revision: response_frame.unwrap_or(self.revision),
                });
            }
            self.facts.extend(self.decoder.on_event(event)?);
            if self.decoder.is_complete() {
                self.cleanup_context(false).await?;
            }
        }
    }

    async fn cleanup_context(&mut self, optional: bool) -> Result<(), Fault> {
        let session = self.session.as_mut().expect("live session");
        if let Some(media) = &mut session.media {
            self.transient.extend(media.take_spoken_items());
        }
        // A failed optional delete stays owned until the next ordinary reply
        // ends. A silent context refresh is not a retry/recovery boundary.
        let pending_cues = if optional {
            Vec::new()
        } else {
            std::mem::take(&mut session.pending_context_cues)
        };
        let mut items = std::mem::take(&mut self.transient);
        for id in &pending_cues {
            if !items.contains(id) {
                items.push(id.clone());
            }
        }
        let context_cleanup = !pending_cues.is_empty()
            || items.iter().any(|id| {
                session
                    .media
                    .as_ref()
                    .is_some_and(|media| media.owns_context_cue(id))
            });
        // An already forwarded user utterance may start its next automatic
        // response while proactive context/cue deletes await ACK. Buffer those
        // facts with this exact Frame; never terminate media or silently route
        // their native calls through the following render's callbacks.
        for id in items {
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
                None,
                optional || context_cleanup,
            )
            .await?;
            if !ack.as_ref().is_some_and(|ack| ack["item_id"] == id) {
                if context_cleanup {
                    if !pending_cues.contains(&id)
                        && session
                            .media
                            .as_ref()
                            .is_some_and(|media| media.owns_context_cue(&id))
                    {
                        session.retain_context_cue(id);
                    }
                    // A cue already held from an earlier refusal has reached
                    // its one ordinary cleanup boundary. If still unconfirmed,
                    // report and release bookkeeping rather than retrying it.
                    session.observe_context_refresh_rejected(
                        "cue_cleanup_unconfirmed",
                        "conversation.item.deleted",
                    );
                } else if ack.is_some() {
                    return Err(transport::protocol_fault());
                }
            }
        }
        Ok(())
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.provider.lose_continuity();
        }
    }
}
