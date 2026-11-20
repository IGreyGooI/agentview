//! Volcengine Seeduplex full-duplex Realtime voice, transcripts and functions.

mod exchange;
mod request;
mod voice_application;
mod wire;

use std::{
    collections::VecDeque,
    num::{NonZeroU128, NonZeroU64},
    time::Duration,
};

use async_trait::async_trait;
use futures::stream;
use reqwest::{
    header::{HeaderName, HeaderValue},
    Client,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use url::{Host, Url};

use crate::{
    component::execution::{
        ContextWindow, Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile,
        FrameRevision, ProviderFactStream, ReactionPort, ReactionPortFault as Fault,
        ReactionPortFaultCode as Code, ReactionPortFaultReason as Reason, SubmitFault,
        TargetDeclaration, TargetEpoch, TargetIdentity,
    },
    transcript::CanonicalInputItem,
};

use super::{
    qwen_realtime::{
        media::{Input, MediaEventsSender, SpokenText},
        transport::{self, Socket},
        QwenRealtimeMediaEvents, QwenRealtimeMediaInput,
    },
    ModelSpec,
};

pub use voice_application::DoubaoRealtimeVoiceApplication;

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DoubaoRealtimeConfigError {
    #[error("Seeduplex requires a wss endpoint or a ws endpoint on loopback")]
    InvalidEndpoint,
    #[error("Seeduplex speech API key is empty or invalid")]
    InvalidApiKey,
    #[error("Seeduplex voice is empty")]
    InvalidVoice,
    #[error("Seeduplex client initialization failed")]
    TransportInitialization,
    #[error("Seeduplex target initialization failed")]
    TargetInitialization,
}

/// One Seeduplex WebSocket for the lifetime of a voice Application.
pub struct DoubaoRealtimeProvider {
    endpoint: Url,
    credential: HeaderValue,
    client: Client,
    model: ModelSpec,
    voice: String,
    identity: TargetIdentity,
    profile: FrameProfile,
    media: Option<Media>,
    connection: Option<Socket>,
    accepted: Option<Accepted>,
    terminal: Option<Fault>,
}

struct Media {
    input: mpsc::Receiver<Input>,
    events: MediaEventsSender,
    active_response: watch::Sender<Option<String>>,
    input_item: Option<String>,
    awaiting_response: bool,
    spoken: VecDeque<SpokenText>,
    cancel: Option<PendingCancel>,
    finished: bool,
}

struct PendingCancel {
    event_id: String,
    response_id: String,
}

struct Accepted {
    revision: FrameRevision,
    replay: Vec<CanonicalInputItem>,
    configuration: Value,
    tool_calls: Vec<String>,
    last_response: Option<String>,
}

impl DoubaoRealtimeProvider {
    /// Use the full-duplex speech endpoint and a speech service API key.
    pub fn new(
        endpoint: &str,
        api_key: &str,
        model: ModelSpec,
        voice: &str,
    ) -> Result<Self, DoubaoRealtimeConfigError> {
        use DoubaoRealtimeConfigError::*;
        let mut endpoint = Url::parse(endpoint).map_err(|_| InvalidEndpoint)?;
        let loopback = match endpoint.host().ok_or(InvalidEndpoint)? {
            Host::Ipv4(ip) => ip.is_loopback(),
            Host::Ipv6(ip) => ip.is_loopback(),
            Host::Domain(domain) => domain == "localhost",
        };
        let scheme = match endpoint.scheme() {
            "wss" => "https",
            "ws" if loopback => "http",
            _ => return Err(InvalidEndpoint),
        };
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(InvalidEndpoint);
        }
        endpoint.set_scheme(scheme).map_err(|_| InvalidEndpoint)?;
        if api_key.is_empty() || api_key.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(InvalidApiKey);
        }
        let mut credential = HeaderValue::from_str(api_key).map_err(|_| InvalidApiKey)?;
        credential.set_sensitive(true);
        if voice.trim().is_empty() {
            return Err(InvalidVoice);
        }
        let client = Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| TransportInitialization)?;
        let mut identity = [0; 16];
        getrandom::fill(&mut identity).map_err(|_| TargetInitialization)?;
        let identity = NonZeroU128::new(u128::from_ne_bytes(identity))
            .map(TargetIdentity::new)
            .ok_or(TargetInitialization)?;
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: MAX_REQUEST_BYTES,
                max_component_bytes: 2 * 1024 * 1024,
                context_window: ContextWindow::Tokens(model.context_window_tokens()),
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true)
                .with_native_tool_calls(true)
                .with_realtime_audio(true),
        );
        Ok(Self {
            endpoint,
            credential,
            client,
            model,
            voice: voice.to_owned(),
            identity,
            profile,
            media: None,
            connection: None,
            accepted: None,
            terminal: None,
        })
    }

    fn with_audio(
        mut self,
    ) -> (
        Self,
        QwenRealtimeMediaInput,
        QwenRealtimeMediaEvents,
        watch::Receiver<Option<String>>,
    ) {
        let (input, receiver) = QwenRealtimeMediaInput::channel();
        let (events, output) = MediaEventsSender::new();
        let (active_response, active) = watch::channel(None);
        self.media = Some(Media {
            input: receiver,
            events,
            active_response,
            input_item: None,
            awaiting_response: false,
            spoken: VecDeque::new(),
            cancel: None,
            finished: false,
        });
        (self, input, output, active)
    }
}

#[async_trait]
impl ReactionPort for DoubaoRealtimeProvider {
    fn declare(&mut self) -> Result<TargetDeclaration, Fault> {
        if let Some(fault) = self.terminal {
            return Err(fault);
        }
        Ok(match &self.accepted {
            Some(accepted) => TargetDeclaration::resume(accepted.revision, self.profile.clone()),
            None => TargetDeclaration::full(
                self.identity,
                TargetEpoch::new(NonZeroU64::MIN),
                self.profile.clone(),
            ),
        })
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declare()?)?;
        if self.media.is_none() {
            return Err(Fault::terminal(Code::Unavailable, Reason::Declaration).into());
        }
        // AgentView owns the canonical replay suffix. A live socket retains the
        // submitted microphone history, which a replacement Full cannot replay.
        // Reject just the incompatible submission before a wire handoff.
        if let Some(accepted) = &self.accepted {
            if matches!(frame.basis(), FrameBasis::Full)
                || accepted.replay != frame.submission().replay()
            {
                return Err(request::preparation_fault().into());
            }
        }
        let request =
            request::encode(&frame, self.accepted.as_ref(), self.model.id(), &self.voice)?;
        if self.connection.is_none() {
            let socket = transport::connect_with_header(
                &self.client,
                &self.endpoint,
                HeaderName::from_static("x-api-key"),
                self.credential.clone(),
            )
            .await?;
            frame.check_handoff_precondition(&self.declare()?)?;
            self.connection = Some(socket);
        }
        let exchange = exchange::Exchange::new(self, frame.revision(), request);
        Ok(Box::pin(stream::unfold(exchange, |mut exchange| async {
            exchange.next().await.map(|fact| (fact, exchange))
        })))
    }

    async fn next_session_frame(&mut self, _feedback_pending: bool) -> Result<bool, Fault> {
        Ok(self
            .media
            .as_ref()
            .is_some_and(|media| !media.finished || !media.spoken.is_empty()))
    }

    async fn finish_session(&mut self) {
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let close = json!({"type": "session.close", "event_id": wire::id("event")});
                let _ = transport::send(&mut connection, close.to_string()).await;
                let _ = connection.close(None).await;
            })
            .await;
        }
        self.accepted = None;
        self.media = None;
        self.terminal
            .get_or_insert_with(|| Fault::terminal(Code::Unavailable, Reason::Declaration));
    }
}
