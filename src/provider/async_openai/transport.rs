use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use ::async_openai::config::OpenAIConfig;

use super::{AsyncOpenAiConfigError, AsyncOpenAiTransportConfig, OpenAiIpFamily};

pub(super) struct InitializedOpenAiTransport {
    pub(super) client: reqwest::Client,
    pub(super) config: OpenAIConfig,
    pub(super) read_timeout: Duration,
    pub(super) sse_event_timeout: Option<Duration>,
    pub(super) max_response_body_bytes: usize,
    pub(super) max_sse_event_bytes: usize,
    pub(super) max_output_text_bytes: usize,
    pub(super) max_responses_serialized_request_body_bytes: usize,
    pub(super) max_chat_completions_serialized_request_body_bytes: usize,
}

pub(super) fn initialize(
    config: AsyncOpenAiTransportConfig,
) -> Result<InitializedOpenAiTransport, AsyncOpenAiConfigError> {
    let client = finish_client_build(client_builder(&config))?;
    // Existing callers retain their complete-event wait bound. Stream-only
    // callers need raw byte activity (including fragments/heartbeats) to reset
    // the idle deadline, which reqwest already enforces below the SSE parser.
    let sse_event_timeout = config.request_timeout.map(|_| config.read_timeout);
    let AsyncOpenAiTransportConfig {
        api_base,
        api_key,
        bypass_environment_proxy: _,
        ip_family: _,
        connect_timeout: _,
        request_timeout: _,
        read_timeout,
        max_response_body_bytes,
        max_sse_event_bytes,
        max_output_text_bytes,
        max_responses_serialized_request_body_bytes,
        max_chat_completions_serialized_request_body_bytes,
        responses_frame_limits: _,
        chat_completions_frame_limits: _,
    } = config;
    let config = OpenAIConfig::new()
        .with_org_id("")
        .with_project_id("")
        .with_api_base(api_base)
        .with_api_key(api_key);

    Ok(InitializedOpenAiTransport {
        client,
        config,
        read_timeout,
        sse_event_timeout,
        max_response_body_bytes,
        max_sse_event_bytes,
        max_output_text_bytes,
        max_responses_serialized_request_body_bytes,
        max_chat_completions_serialized_request_body_bytes,
    })
}

pub(super) async fn next_sse_event<F: Future>(
    event: F,
    timeout: Option<Duration>,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, event).await,
        None => Ok(event.await),
    }
}

fn client_builder(config: &AsyncOpenAiTransportConfig) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout);
    // A continuously arriving reply may outlive the idle-read budget. Only
    // callers that requested a cumulative deadline should have that reply cut
    // off by reqwest's total timer; stream-only callers retain connect/idle bounds.
    let builder = match config.request_timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder,
    };
    let builder = match config.ip_family {
        OpenAiIpFamily::Auto => builder,
        OpenAiIpFamily::Ipv4 => builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    };
    if config.bypass_environment_proxy {
        builder.no_proxy()
    } else {
        builder
    }
}

fn finish_client_build(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, AsyncOpenAiConfigError> {
    builder
        .build()
        .map_err(|_| AsyncOpenAiConfigError::TransportInitialization)
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        error::Error,
        io::ErrorKind,
        net::{Ipv4Addr, Ipv6Addr},
        time::Duration,
    };

    use axum::{body::Body, http::header, response::Response, routing::get, Router};
    use tokio::{net::TcpListener, task::JoinHandle};

    use crate::provider::async_openai::{
        AsyncOpenAiConfigError, AsyncOpenAiTransportConfig, OpenAiIpFamily,
    };

    use super::{client_builder, finish_client_build, initialize};

    const BUILDER_SENTINEL: &str = "openai-builder-source-sentinel\n";

    #[test]
    fn deterministic_builder_failure_is_sanitized_without_a_source() {
        let builder = reqwest::Client::builder().user_agent(BUILDER_SENTINEL);

        let error = match finish_client_build(builder) {
            Ok(_) => panic!("invalid User-Agent unexpectedly built an HTTP client"),
            Err(error) => error,
        };

        assert_eq!(error, AsyncOpenAiConfigError::TransportInitialization);
        assert_eq!(format!("{error:?}"), "TransportInitialization");
        assert_eq!(error.to_string(), "OpenAI transport initialization failed");
        assert!(error.source().is_none());
        assert!(!format!("{error:?}\n{error}").contains(BUILDER_SENTINEL));
    }

    struct LoopbackServer(JoinHandle<()>);

    impl Drop for LoopbackServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    fn serve_loopback(listener: TcpListener, body: &'static str) -> LoopbackServer {
        LoopbackServer(tokio::spawn(async move {
            let app = Router::new().route("/", get(move || async move { body }));
            axum::serve(listener, app)
                .await
                .expect("serve loopback HTTP responses");
        }))
    }

    fn serve_sse_loopback(
        listener: TcpListener,
        interval: Duration,
        count: usize,
    ) -> LoopbackServer {
        LoopbackServer(tokio::spawn(async move {
            let app = Router::new().route(
                "/",
                get(move || async move {
                    let chunks = futures::stream::unfold(0, move |index| async move {
                        if index == count {
                            return None;
                        }
                        if index > 0 {
                            tokio::time::sleep(interval).await;
                        }
                        Some((
                            Ok::<_, Infallible>(format!("data: chunk-{index}\n\n")),
                            index + 1,
                        ))
                    });
                    Response::builder()
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }),
            );
            axum::serve(listener, app)
                .await
                .expect("serve loopback SSE responses");
        }))
    }

    #[tokio::test]
    async fn finite_request_deadline_still_stops_a_continuously_arriving_body() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = serve_sse_loopback(listener, Duration::from_millis(100), 8);
        let config = AsyncOpenAiTransportConfig::new(format!("http://{addr}"), "test-key")
            .unwrap()
            .with_stream_timeouts(Duration::from_secs(1), Duration::from_millis(400))
            .unwrap()
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_millis(300),
                Duration::from_millis(400),
            )
            .unwrap();
        let mut response = initialize(config)
            .unwrap()
            .client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let first = response.chunk().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("chunk-0"));
        let error = loop {
            match response.chunk().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("finite request deadline did not stop the long response"),
                Err(error) => break error,
            }
        };
        assert!(
            error.is_timeout(),
            "expected total request timeout: {error}"
        );
    }

    #[tokio::test]
    async fn stream_deadlines_allow_progress_past_an_earlier_total_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = serve_sse_loopback(listener, Duration::from_millis(100), 8);
        let config = AsyncOpenAiTransportConfig::new(format!("http://{addr}"), "test-key")
            .unwrap()
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_millis(300),
                Duration::from_millis(400),
            )
            .unwrap()
            .with_stream_timeouts(Duration::from_secs(1), Duration::from_millis(400))
            .unwrap();
        let response = initialize(config)
            .unwrap()
            .client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let body = response.text().await.unwrap();
        let expected = (0..8)
            .map(|index| format!("data: chunk-{index}\n\n"))
            .collect::<String>();
        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn stream_deadlines_still_stop_an_idle_body_without_a_total_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = serve_sse_loopback(listener, Duration::from_millis(600), 2);
        let config = AsyncOpenAiTransportConfig::new(format!("http://{addr}"), "test-key")
            .unwrap()
            .with_stream_timeouts(Duration::from_secs(1), Duration::from_millis(150))
            .unwrap();
        let mut response = initialize(config)
            .unwrap()
            .client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&response.chunk().await.unwrap().unwrap()).contains("chunk-0")
        );
        let error = response.chunk().await.unwrap_err();
        assert!(error.is_timeout(), "expected idle read timeout: {error}");
    }

    #[test]
    fn stream_deadlines_reject_zero_connection_or_read_budgets() {
        for (connect, read, expected_name) in [
            (Duration::ZERO, Duration::from_secs(1), "connect_timeout"),
            (Duration::from_secs(1), Duration::ZERO, "read_timeout"),
        ] {
            let error = match AsyncOpenAiTransportConfig::new("http://localhost", "test-key")
                .unwrap()
                .with_stream_timeouts(connect, read)
            {
                Ok(_) => panic!("zero {expected_name} unexpectedly accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains(expected_name));
        }
    }

    fn loopback_config(port: u16) -> AsyncOpenAiTransportConfig {
        AsyncOpenAiTransportConfig::new(format!("http://localhost:{port}"), "test-key")
            .unwrap()
            .with_timeouts(
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(2),
            )
            .unwrap()
    }

    async fn ipv6_loopback_listener() -> Option<TcpListener> {
        match TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await {
            Ok(listener) => Some(listener),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::AddrNotAvailable | ErrorKind::Unsupported
                ) =>
            {
                eprintln!("skipping dual-stack loopback test: IPv6 loopback unavailable ({error})");
                None
            }
            Err(error) => panic!("bind IPv6 loopback: {error}"),
        }
    }

    #[tokio::test]
    async fn initialized_ipv4_transport_connects_to_an_ipv4_destination() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = serve_loopback(listener, "ipv4");
        let config = loopback_config(addr.port()).with_ip_family(OpenAiIpFamily::Ipv4);
        let transport = initialize(config).unwrap();

        let response = transport
            .client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.remote_addr(), Some(addr));
        assert_eq!(response.text().await.unwrap(), "ipv4");
    }

    #[tokio::test]
    async fn ipv4_mode_selects_ipv4_when_default_auto_selects_ipv6_for_the_same_host() {
        let Some(ipv6_listener) = ipv6_loopback_listener().await else {
            return;
        };
        let ipv6_addr = ipv6_listener.local_addr().unwrap();
        let ipv4_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, ipv6_addr.port()))
            .await
            .unwrap();
        let ipv4_addr = ipv4_listener.local_addr().unwrap();
        let _ipv6_server = serve_loopback(ipv6_listener, "ipv6");
        let _ipv4_server = serve_loopback(ipv4_listener, "ipv4");
        let addresses = [ipv6_addr, ipv4_addr];
        let config = loopback_config(ipv6_addr.port());
        let url = format!("{}/", config.api_base);
        let auto_client =
            finish_client_build(client_builder(&config).resolve_to_addrs("localhost", &addresses))
                .unwrap();

        let response = auto_client.get(&url).send().await.unwrap();
        assert_eq!(response.remote_addr(), Some(ipv6_addr));
        assert_eq!(response.text().await.unwrap(), "ipv6");

        let ipv4_config = config.with_ip_family(OpenAiIpFamily::Ipv4);
        let ipv4_client = finish_client_build(
            client_builder(&ipv4_config).resolve_to_addrs("localhost", &addresses),
        )
        .unwrap();

        let response = ipv4_client.get(&url).send().await.unwrap();
        assert_eq!(response.remote_addr(), Some(ipv4_addr));
        assert_eq!(response.text().await.unwrap(), "ipv4");
    }

    #[tokio::test]
    async fn ipv4_mode_rejects_an_ipv6_only_destination_without_fallback() {
        let Some(listener) = ipv6_loopback_listener().await else {
            return;
        };
        let addr = listener.local_addr().unwrap();
        let _server = serve_loopback(listener, "ipv6");
        let config = loopback_config(addr.port());
        let url = format!("{}/", config.api_base);
        let ipv6_only_address = [addr];
        let auto_client = finish_client_build(
            client_builder(&config).resolve_to_addrs("localhost", &ipv6_only_address),
        )
        .unwrap();
        assert!(auto_client
            .get(&url)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());

        let ipv4_config = config.with_ip_family(OpenAiIpFamily::Ipv4);
        let ipv4_client = finish_client_build(
            client_builder(&ipv4_config).resolve_to_addrs("localhost", &ipv6_only_address),
        )
        .unwrap();
        let error = ipv4_client.get(&url).send().await.unwrap_err();
        assert!(error.is_connect());
    }
}
