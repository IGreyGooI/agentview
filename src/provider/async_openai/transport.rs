use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use ::async_openai::config::OpenAIConfig;

use super::{AsyncOpenAiConfigError, AsyncOpenAiTransportConfig, OpenAiIpFamily};

pub(super) struct InitializedOpenAiTransport {
    pub(super) client: reqwest::Client,
    pub(super) config: OpenAIConfig,
    pub(super) read_timeout: Duration,
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
        max_response_body_bytes,
        max_sse_event_bytes,
        max_output_text_bytes,
        max_responses_serialized_request_body_bytes,
        max_chat_completions_serialized_request_body_bytes,
    })
}

fn client_builder(config: &AsyncOpenAiTransportConfig) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout)
        .read_timeout(config.read_timeout);
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
        error::Error,
        io::ErrorKind,
        net::{Ipv4Addr, Ipv6Addr},
        time::Duration,
    };

    use axum::{routing::get, Router};
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
