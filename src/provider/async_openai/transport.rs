use std::time::Duration;

use ::async_openai::config::OpenAIConfig;

use crate::component::execution::reaction::FrameProfile;

use super::{
    chat_completions_frame_profile, responses_frame_profile, AsyncOpenAiConfigError,
    AsyncOpenAiTransportConfig,
};

pub(super) struct InitializedOpenAiTransport {
    pub(super) client: reqwest::Client,
    pub(super) config: OpenAIConfig,
    pub(super) read_timeout: Duration,
    pub(super) max_response_body_bytes: usize,
    pub(super) max_sse_event_bytes: usize,
    pub(super) max_output_text_bytes: usize,
    pub(super) max_responses_serialized_request_body_bytes: usize,
    pub(super) max_chat_completions_serialized_request_body_bytes: usize,
    pub(super) responses_frame_profile: FrameProfile,
    pub(super) chat_completions_frame_profile: FrameProfile,
}

pub(super) fn initialize(
    config: AsyncOpenAiTransportConfig,
) -> Result<InitializedOpenAiTransport, AsyncOpenAiConfigError> {
    let responses_frame_profile =
        responses_frame_profile(config.responses_frame_constraints.clone())?;
    let chat_completions_frame_profile =
        chat_completions_frame_profile(config.chat_completions_frame_constraints.clone())?;
    let client = finish_client_build(client_builder(&config))?;
    let AsyncOpenAiTransportConfig {
        api_base,
        api_key,
        bypass_environment_proxy: _,
        connect_timeout: _,
        request_timeout: _,
        read_timeout,
        max_response_body_bytes,
        max_sse_event_bytes,
        max_output_text_bytes,
        max_responses_serialized_request_body_bytes,
        max_chat_completions_serialized_request_body_bytes,
        responses_frame_constraints: _,
        chat_completions_frame_constraints: _,
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
        responses_frame_profile,
        chat_completions_frame_profile,
    })
}

fn client_builder(config: &AsyncOpenAiTransportConfig) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout)
        .read_timeout(config.read_timeout);
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
    use std::error::Error;

    use crate::provider::async_openai::AsyncOpenAiConfigError;

    use super::finish_client_build;

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
}
