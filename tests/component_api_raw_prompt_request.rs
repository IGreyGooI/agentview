use std::time::Duration;

use agentview::{
    component::{
        execution::{Application, ProviderIdentity},
        prelude::*,
    },
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
};
use serde_json::Value;

#[allow(dead_code)]
#[path = "support/responses_acceptance_server.rs"]
mod responses_acceptance_server;

use responses_acceptance_server::ResponsesAcceptanceServer;

const SYSTEM: &str = concat!(
    "  # Existing prompt\r\n\r\n",
    "- Keep **Markdown** and `code`.\r\n",
    "\tKeep this indentation.  \r\n\r\n",
    "```xml\r\n<say>Hello & welcome.</say>\r\n```\r\n\r\n",
    "Outside code: <policy mode=\"literal\">A & B</policy>\r\n\r\n",
);
const DEVELOPER: &str = "## Policy\n\nKeep <say> and & unchanged.\n";
const USER: &str = "## Request\n\nShow the original example.\n";

#[component]
fn raw_prompt_application() -> Component {
    let system = SYSTEM.to_owned();
    let developer = DEVELOPER.to_owned();
    let user = USER.to_owned();
    view! {
        #[system_once]
        { system }

        #[developer(repeat)]
        { developer }

        #[user(repeat)]
        { format!("{user}") }
    }
}

#[tokio::test]
async fn multiline_prompt_strings_reach_the_provider_request_unchanged() -> anyhow::Result<()> {
    let mut server = ResponsesAcceptanceServer::start(&["received"]).await?;
    let config = AsyncOpenAiTransportConfig::new(server.api_base(), "test-token")?;
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, "raw-prompt")?;
    let options = CodexHttpV1Options::new("test-model", None, None, None::<String>)?;
    let provider =
        AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options));
    let mut application = Application::mount(raw_prompt_application, provider)?;

    for _ in 0..2 {
        assert!(application.react().await?.is_continue());
        let bytes = tokio::time::timeout(Duration::from_secs(5), server.next_request()).await??;
        let body: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(body["instructions"].as_str(), Some(SYSTEM));
        let input = body["input"].as_array().expect("Responses input items");
        for (role, expected) in [("developer", DEVELOPER), ("user", USER)] {
            let message = input
                .iter()
                .rev()
                .find(|item| item["type"] == "message" && item["role"] == role)
                .unwrap_or_else(|| panic!("missing {role} message: {body:#}"));
            assert_eq!(message["content"].as_array().unwrap().len(), 1);
            assert_eq!(message["content"][0]["text"].as_str(), Some(expected));
        }
    }

    application.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}
