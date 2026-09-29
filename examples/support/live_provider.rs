//! Shared OpenAI Responses provider setup for the runnable examples.

use agentview::{
    component::execution::ProviderIdentity,
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
        ModelSpec,
    },
};
use anyhow::Context as _;

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-5.6-terra";

/// Builds a provider from `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `AGENTVIEW_MODEL`.
/// `AGENTVIEW_CONTEXT_WINDOW_TOKENS` is required for every model.
///
/// `.env` values override existing environment variables, including Cargo's CA defaults.
/// The base URL defaults to the public OpenAI API and the model to `gpt-5.6-terra`.
///
/// For a gateway with a private CA, set the standard `SSL_CERT_FILE` environment variable.
pub fn from_env(binding: &str) -> anyhow::Result<AsyncOpenAiResponsesProvider> {
    load_dotenv()?;

    let api_key = required_env("OPENAI_API_KEY")?;
    let api_base = optional_env("OPENAI_BASE_URL")?.unwrap_or_else(|| DEFAULT_API_BASE.to_owned());
    let model = optional_env("AGENTVIEW_MODEL")?.unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    let context_window_tokens = required_env("AGENTVIEW_CONTEXT_WINDOW_TOKENS")?
        .parse::<u64>()
        .context("AGENTVIEW_CONTEXT_WINDOW_TOKENS must be a positive integer")?;

    let model = ModelSpec::new(model, context_window_tokens)
        .context("OpenAI model description is invalid")?;
    provider(&api_base, &api_key, model, binding)
}

/// Builds a provider from explicit connection and model settings.
pub fn provider(
    api_base: &str,
    api_key: &str,
    model: ModelSpec,
    binding: &str,
) -> anyhow::Result<AsyncOpenAiResponsesProvider> {
    let transport = AsyncOpenAiTransportConfig::new(api_base, api_key)
        .context("OpenAI transport configuration is invalid")?;
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, binding)
        .context("OpenAI provider identity is invalid")?;
    let options = CodexHttpV1Options::new(model, None, None, None::<String>)
        .context("OpenAI request options are invalid")?;

    AsyncOpenAiResponsesProvider::try_new(transport, identity, CodexHttpV1Encoder::new(options))
        .context("OpenAI Responses provider initialization failed")
}

fn load_dotenv() -> anyhow::Result<()> {
    match dotenvy::dotenv_override() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn required_env(name: &str) -> anyhow::Result<String> {
    let value = optional_env(name)?.with_context(|| format!("{name} is required"))?;
    anyhow::ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value)
}

fn optional_env(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{name} must contain Unicode text"),
    }
}
