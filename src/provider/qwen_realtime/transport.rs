use std::time::Duration;

use futures::{Sink, SinkExt, Stream, StreamExt};
use reqwest::{header, Client, StatusCode, Upgraded};
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::{
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
        Error as WsError, Message,
    },
    WebSocketStream,
};
use url::Url;

use crate::component::execution::{
    ReactionPortFault as Fault, ReactionPortFaultCode as Code, ReactionPortFaultReason as Reason,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;

pub(crate) type Socket = WebSocketStream<Upgraded>;

pub(super) async fn connect(
    client: &Client,
    endpoint: &Url,
    authorization: header::HeaderValue,
) -> Result<Socket, Fault> {
    connect_with_header(client, endpoint, header::AUTHORIZATION, authorization).await
}

pub(crate) async fn connect_with_header(
    client: &Client,
    endpoint: &Url,
    credential_header: header::HeaderName,
    credential: header::HeaderValue,
) -> Result<Socket, Fault> {
    timeout(CONNECT_TIMEOUT, async {
        // Use the HTTP client for the upgrade so TLS/proxies and sensitive header
        // handling are shared with the other HTTP-backed providers.
        let key = generate_key();
        let response = client
            .get(endpoint.clone())
            .header(credential_header, credential)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, key.as_str())
            .send()
            .await
            .map_err(|_| Fault::retryable(Code::Unavailable, Reason::Transport))?;

        match response.status() {
            StatusCode::SWITCHING_PROTOCOLS => {}
            StatusCode::UNAUTHORIZED => {
                return Err(Fault::terminal(Code::Rejected, Reason::Authentication));
            }
            StatusCode::FORBIDDEN => {
                return Err(Fault::terminal(Code::Rejected, Reason::Authorization));
            }
            StatusCode::TOO_MANY_REQUESTS => {
                return Err(Fault::retryable(Code::Unavailable, Reason::RateLimited));
            }
            status if status.is_server_error() => {
                return Err(Fault::retryable(Code::Unavailable, Reason::Transport));
            }
            _ => return Err(Fault::retryable(Code::Rejected, Reason::UpstreamRejected)),
        }

        let headers = response.headers();
        let expected_accept = derive_accept_key(key.as_bytes());
        if !header_has_token(headers, header::CONNECTION, "upgrade")
            || !header_has_token(headers, header::UPGRADE, "websocket")
            || headers
                .get(header::SEC_WEBSOCKET_ACCEPT)
                .and_then(|value| value.to_str().ok())
                != Some(expected_accept.as_str())
            || headers.contains_key(header::SEC_WEBSOCKET_PROTOCOL)
            || headers.contains_key(header::SEC_WEBSOCKET_EXTENSIONS)
        {
            return Err(protocol_fault());
        }

        let upgraded = response
            .upgrade()
            .await
            .map_err(|_| Fault::retryable(Code::Unavailable, Reason::Transport))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_EVENT_BYTES))
            .max_frame_size(Some(MAX_EVENT_BYTES));
        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await)
    })
    .await
    .map_err(|_| Fault::retryable(Code::Unavailable, Reason::Transport))?
}

fn header_has_token(headers: &header::HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(token))
        })
    })
}

pub(crate) async fn send(
    socket: &mut (impl Sink<Message, Error = WsError> + Unpin),
    raw: String,
) -> Result<(), Fault> {
    timeout(IDLE_TIMEOUT, socket.send(Message::Text(raw.into())))
        .await
        .map_err(|_| timeout_fault())?
        .map_err(ws_fault)
}

pub(super) async fn receive(
    socket: &mut (impl Stream<Item = Result<Message, WsError>> + Unpin),
) -> Result<(Value, usize), Fault> {
    timeout(IDLE_TIMEOUT, receive_unbounded(socket))
        .await
        .map_err(|_| timeout_fault())?
}

pub(crate) async fn receive_unbounded(
    socket: &mut (impl Stream<Item = Result<Message, WsError>> + Unpin),
) -> Result<(Value, usize), Fault> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(raw))) => {
                let value = serde_json::from_str(&raw).map_err(|_| protocol_fault())?;
                return Ok((value, raw.len()));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Err(error)) => return Err(ws_fault(error)),
            None | Some(Ok(Message::Close(_))) => {
                return Err(Fault::retryable(Code::Unavailable, Reason::StreamTransport));
            }
            Some(Ok(_)) => return Err(protocol_fault()),
        }
    }
}

pub(crate) fn protocol_fault() -> Fault {
    Fault::retryable(Code::Protocol, Reason::ResponseProtocol)
}

fn timeout_fault() -> Fault {
    Fault::retryable(Code::Unavailable, Reason::StreamTimeout)
}

fn ws_fault(error: WsError) -> Fault {
    match error {
        WsError::Capacity(_) => Fault::retryable(Code::Limit, Reason::OutputLimit),
        WsError::Protocol(_) | WsError::Utf8(_) => protocol_fault(),
        _ => Fault::retryable(Code::Unavailable, Reason::StreamTransport),
    }
}
