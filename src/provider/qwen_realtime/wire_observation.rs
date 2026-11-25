use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use futures::{Sink, Stream};
use tokio_tungstenite::tungstenite::{protocol::CloseFrame, Error as WsError, Message};

use super::transport;

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Direction of an actual Qwen WebSocket text message at the SDK transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QwenRealtimeWireDirection {
    Send,
    Receive,
}

/// One original WebSocket text message, observed before protocol admission.
///
/// A connection ID identifies one successful WebSocket upgrade within this
/// process. Its sequence starts at one and orders SDK socket handoffs and
/// reads across both directions as the connection is polled.
/// Send marks acceptance by the WebSocket sink for transmission. Received
/// server acknowledgements establish their own subsequent observations.
#[derive(Clone, Copy, Debug)]
pub struct QwenRealtimeWireEvent<'a> {
    pub connection_id: u64,
    pub sequence: u64,
    pub direction: QwenRealtimeWireDirection,
    pub raw: &'a str,
}

pub(super) type WireObserver = dyn for<'a> Fn(QwenRealtimeWireEvent<'a>) + Send + Sync;

struct WireDiagnostics {
    observer: Arc<WireObserver>,
    connection_id: u64,
    sequence: u64,
}

impl WireDiagnostics {
    fn observe(&mut self, direction: QwenRealtimeWireDirection, raw: &str) {
        self.sequence += 1;
        let event = QwenRealtimeWireEvent {
            connection_id: self.connection_id,
            sequence: self.sequence,
            direction,
            raw,
        };
        // This optional callback runs at the socket's handoff boundary. Its
        // diagnostic failure cannot undo a sent message or discard input.
        let _ = catch_unwind(AssertUnwindSafe(|| (self.observer)(event)));
    }
}

pub(super) struct ObservedSocket {
    inner: transport::Socket,
    diagnostics: Option<WireDiagnostics>,
}

impl ObservedSocket {
    pub(super) fn new(inner: transport::Socket, observer: Option<Arc<WireObserver>>) -> Self {
        Self {
            inner,
            diagnostics: observer.map(|observer| WireDiagnostics {
                observer,
                connection_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
                sequence: 0,
            }),
        }
    }

    pub(super) async fn close(&mut self, frame: Option<CloseFrame>) -> Result<(), WsError> {
        self.inner.close(frame).await
    }
}

impl Stream for ObservedSocket {
    type Item = Result<Message, WsError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let next = Pin::new(&mut this.inner).poll_next(cx);
        if let (Some(diagnostics), Poll::Ready(Some(Ok(Message::Text(raw))))) =
            (&mut this.diagnostics, &next)
        {
            diagnostics.observe(QwenRealtimeWireDirection::Receive, raw.as_str());
        }
        next
    }
}

impl Sink<Message> for ObservedSocket {
    type Error = WsError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let raw = match (&this.diagnostics, &message) {
            (Some(_), Message::Text(raw)) => Some(raw.clone()),
            _ => None,
        };
        Pin::new(&mut this.inner).start_send(message)?;
        if let (Some(diagnostics), Some(raw)) = (&mut this.diagnostics, raw) {
            diagnostics.observe(QwenRealtimeWireDirection::Send, raw.as_str());
        }
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use reqwest::{header::HeaderValue, Client};
    use std::sync::{atomic::AtomicUsize, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use url::Url;

    #[tokio::test]
    async fn observes_exact_socket_text_before_json_parsing_in_both_directions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/realtime",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let incoming =
            " { \"type\" : \"session.created\", \"session\": {\"id\":\"wire-session\"} } \n";
        let outgoing =
            " {\"type\":\"session.update\", \"session\":{\"instructions\":\"原文 <&> \\\"\"}} ";
        let malformed = "{unparseable JSON";
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket.send(Message::Text(incoming.into())).await.unwrap();
            let Some(Ok(Message::Text(sent))) = socket.next().await else {
                panic!("expected the client's original text");
            };
            assert_eq!(sent.as_str(), outgoing);
            socket.send(Message::Text(malformed.into())).await.unwrap();
        });

        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let observer: Arc<WireObserver> = Arc::new(move |event| {
            captured.lock().unwrap().push((
                event.connection_id,
                event.sequence,
                event.direction,
                event.raw.to_owned(),
            ));
        });
        let socket = transport::connect(
            &Client::new(),
            &endpoint,
            HeaderValue::from_static("Bearer wire-test-credential"),
        )
        .await
        .unwrap();
        let mut socket = ObservedSocket::new(socket, Some(observer));
        let (event, bytes) = transport::receive(&mut socket).await.unwrap();
        assert_eq!(event["type"], "session.created");
        assert_eq!(bytes, incoming.len());
        assert_eq!(observed.lock().unwrap().len(), 1);
        transport::send(&mut socket, outgoing.to_owned())
            .await
            .unwrap();
        assert_eq!(
            transport::receive(&mut socket).await.unwrap_err().code(),
            crate::component::execution::ReactionPortFaultCode::Protocol
        );
        server.await.unwrap();
        let events = observed.lock().unwrap();
        let connection_id = events[0].0;
        assert_eq!(
            events.as_slice(),
            [
                (
                    connection_id,
                    1,
                    QwenRealtimeWireDirection::Receive,
                    incoming.to_owned(),
                ),
                (
                    connection_id,
                    2,
                    QwenRealtimeWireDirection::Send,
                    outgoing.to_owned(),
                ),
                (
                    connection_id,
                    3,
                    QwenRealtimeWireDirection::Receive,
                    malformed.to_owned(),
                ),
            ]
        );
        assert!(events
            .iter()
            .all(|event| !event.3.contains("wire-test-credential")));
    }

    #[tokio::test]
    async fn observer_panics_preserve_socket_input_output_and_continuity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/realtime",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            for round in 0..2 {
                socket
                    .send(Message::Text(format!("{{\"round\":{round}}}").into()))
                    .await
                    .unwrap();
                let Some(Ok(Message::Text(reply))) = socket.next().await else {
                    panic!("observer panic must leave the connection usable");
                };
                assert_eq!(reply.as_str(), "{\"received\":true}");
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&calls);
        let observer: Arc<WireObserver> = Arc::new(move |_| {
            observed_calls.fetch_add(1, Ordering::Relaxed);
            panic!("diagnostic callback failed");
        });
        let socket = transport::connect(
            &Client::new(),
            &endpoint,
            HeaderValue::from_static("Bearer wire-test-credential"),
        )
        .await
        .unwrap();
        let mut socket = ObservedSocket::new(socket, Some(observer));
        for round in 0..2 {
            let (event, _) = transport::receive(&mut socket).await.unwrap();
            assert_eq!(event["round"], round);
            transport::send(&mut socket, "{\"received\":true}".to_owned())
                .await
                .unwrap();
        }
        server.await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 4);
    }
}
