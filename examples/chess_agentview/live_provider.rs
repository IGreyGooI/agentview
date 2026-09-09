use std::{ffi::OsString, path::PathBuf, time::Duration};

use agentview::{
    component::execution::ProviderIdentity,
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
};
use anyhow::Context as _;

use super::chess_application::ChessConfig;

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const DEFAULT_STOCKFISH_PROGRAM: &str = "/usr/games/stockfish";
const DEFAULT_PLY_LIMIT: usize = 8;
const DEFAULT_ENGINE_NODES: u64 = 5_000;
const DEFAULT_ENGINE_TIMEOUT_SECS: u64 = 30;
const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(110);
const PROVIDER_BINDING: &str = "chess-agentview-example";

pub(crate) struct LiveConfig {
    model: String,
    api_key: String,
    api_base: String,
    stockfish_program: PathBuf,
    ply_limit: usize,
    engine_nodes: u64,
    engine_timeout: Duration,
}

impl LiveConfig {
    pub(crate) fn from_environment(
        mut read: impl FnMut(&str) -> Option<OsString>,
    ) -> anyhow::Result<Self> {
        let api_key = unicode(read("OPENAI_API_KEY"))
            .context("OPENAI_API_KEY is required for the live Chess example")?;
        anyhow::ensure!(!api_key.is_empty(), "OPENAI_API_KEY must not be empty");

        let model = unicode(read("AGENTVIEW_MODEL")).unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let api_base =
            unicode(read("OPENAI_BASE_URL")).unwrap_or_else(|| DEFAULT_API_BASE.to_owned());
        let stockfish_program = read("AGENTVIEW_STOCKFISH_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STOCKFISH_PROGRAM));
        let ply_limit = parse_positive(
            "AGENTVIEW_CHESS_PLY_LIMIT",
            read("AGENTVIEW_CHESS_PLY_LIMIT"),
            DEFAULT_PLY_LIMIT,
        )?;
        let engine_nodes = parse_positive(
            "AGENTVIEW_ENGINE_NODES",
            read("AGENTVIEW_ENGINE_NODES"),
            DEFAULT_ENGINE_NODES,
        )?;
        let engine_timeout_secs = parse_positive(
            "AGENTVIEW_ENGINE_TIMEOUT_SECS",
            read("AGENTVIEW_ENGINE_TIMEOUT_SECS"),
            DEFAULT_ENGINE_TIMEOUT_SECS,
        )?;

        Ok(Self {
            model,
            api_key,
            api_base,
            stockfish_program,
            ply_limit,
            engine_nodes,
            engine_timeout: Duration::from_secs(engine_timeout_secs),
        })
    }

    pub(crate) fn application_config(&self) -> anyhow::Result<ChessConfig> {
        ChessConfig::new(
            self.stockfish_program.clone(),
            self.engine_timeout,
            self.engine_nodes,
            self.ply_limit,
        )
    }

    pub(crate) fn build_provider(&self) -> anyhow::Result<AsyncOpenAiResponsesProvider> {
        let transport = AsyncOpenAiTransportConfig::new(&self.api_base, &self.api_key)
            .and_then(|transport| {
                transport.with_timeouts(
                    Duration::from_secs(10),
                    PROVIDER_REQUEST_TIMEOUT,
                    Duration::from_secs(30),
                )
            })
            .context("OpenAI transport configuration is invalid")?;
        let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, PROVIDER_BINDING)
            .context("provider identity is invalid")?;
        let cache_key = format!("chess-agentview-{}", std::process::id());
        let options = CodexHttpV1Options::new(&self.model, None, None, Some(&cache_key))
            .context("OpenAI model options are invalid")?;

        AsyncOpenAiResponsesProvider::try_new(transport, identity, CodexHttpV1Encoder::new(options))
            .context("OpenAI Responses provider initialization failed")
    }
}

fn unicode(value: Option<OsString>) -> Option<String> {
    value.and_then(|value| value.into_string().ok())
}

fn parse_positive<T>(name: &str, value: Option<OsString>, fallback: T) -> anyhow::Result<T>
where
    T: std::str::FromStr + PartialEq + Default + Copy,
    T::Err: std::fmt::Display + Send + Sync + 'static,
{
    let Some(value) = unicode(value) else {
        return Ok(fallback);
    };
    let parsed = value
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("{name} must be a positive integer: {error}"))?;
    anyhow::ensure!(parsed != T::default(), "{name} must be positive");
    Ok(parsed)
}
