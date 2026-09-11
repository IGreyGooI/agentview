use std::error::Error;

#[cfg(all(
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple"),
    not(target_arch = "wasm32")
))]
use std::{io::ErrorKind, net::TcpListener, process::Command};

use agentview::{
    component::execution::ProviderIdentity,
    provider::{
        async_openai::{
            AsyncOpenAiConfigError, AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig,
        },
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
};

const BINDING: &str = "openai-transport-initialization-test-binding";
const API_KEY_SENTINEL: &str = "openai-initialization-api-key-sentinel";
const BASE_URL_ENV: &str = "AGENTVIEW_OPENAI_INITIALIZATION_TEST_BASE_URL";
const CHILD_ENV: &str = "AGENTVIEW_OPENAI_INITIALIZATION_TEST_CHILD";
const MISSING_CA_SENTINEL: &str = "openai-initialization-missing-ca-sentinel.pem";
const RAW_SOURCE_SENTINEL: &str = "No CA certificates were loaded from the system";
const URL_CREDENTIAL_ENV: &str = "AGENTVIEW_OPENAI_INITIALIZATION_TEST_CREDENTIAL_URL";
const URL_USERNAME_SENTINEL: &str = "url-user-sentinel";
const URL_PASSWORD_SENTINEL: &str = "url-password-sentinel";

#[test]
fn responses_provider_exposes_fallible_initialization() {
    let config = AsyncOpenAiTransportConfig::new("http://127.0.0.1:1/v1", "test-token").unwrap();
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING).unwrap();
    let options = CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap();

    let result: Result<AsyncOpenAiResponsesProvider, AsyncOpenAiConfigError> =
        AsyncOpenAiResponsesProvider::try_new(config, identity, CodexHttpV1Encoder::new(options));

    assert!(result.is_ok());
}

#[cfg(all(
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple"),
    not(target_arch = "wasm32")
))]
#[test]
fn responses_try_new_returns_a_sanitized_initialization_error_without_requesting() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_responses_failure_child();
        return;
    }

    assert_credentialed_url_is_sanitized();
    let api_base = std::env::var(BASE_URL_ENV).expect("parent supplies a synthetic base URL");
    let config = AsyncOpenAiTransportConfig::new(&api_base, API_KEY_SENTINEL).unwrap();
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING).unwrap();
    let options = CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap();

    let error = match AsyncOpenAiResponsesProvider::try_new(
        config,
        identity,
        CodexHttpV1Encoder::new(options),
    ) {
        Ok(_) => panic!("missing CA configuration unexpectedly initialized Responses"),
        Err(error) => error,
    };

    assert_eq!(error, AsyncOpenAiConfigError::TransportInitialization);
    assert_eq!(format!("{error:?}"), "TransportInitialization");
    assert_eq!(error.to_string(), "OpenAI transport initialization failed");
    assert!(error.source().is_none());
    assert_sanitized(
        &format!("{error:?}\n{error}"),
        &[&api_base, API_KEY_SENTINEL, RAW_SOURCE_SENTINEL],
    );
}

#[cfg(all(
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple"),
    not(target_arch = "wasm32")
))]
fn run_responses_failure_child() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_base = format!("https://{}/v1", listener.local_addr().unwrap());
    let missing_ca = std::env::current_exe().unwrap().join(MISSING_CA_SENTINEL);
    assert!(!missing_ca.exists());

    let output = Command::new(std::env::current_exe().unwrap())
        .arg("responses_try_new_returns_a_sanitized_initialization_error_without_requesting")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, "responses")
        .env(BASE_URL_ENV, &api_base)
        .env(URL_CREDENTIAL_ENV, credentialed_url())
        .env("SSL_CERT_FILE", &missing_ca)
        .env_remove("SSL_CERT_DIR")
        .output()
        .unwrap();

    let output_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_sanitized(
        &output_text,
        &[
            &missing_ca.to_string_lossy(),
            &api_base,
            API_KEY_SENTINEL,
            RAW_SOURCE_SENTINEL,
        ],
    );
    assert!(
        output.status.success(),
        "isolated Responses initialization child failed"
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

#[cfg(all(
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple"),
    not(target_arch = "wasm32")
))]
#[test]
fn responses_legacy_new_panics_with_only_a_fixed_sanitized_message() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_legacy_panic_child(
            "responses_legacy_new_panics_with_only_a_fixed_sanitized_message",
            "responses-legacy",
        );
        return;
    }

    assert_credentialed_url_is_sanitized();
    let api_base = std::env::var(BASE_URL_ENV).expect("parent supplies a synthetic base URL");
    let config = AsyncOpenAiTransportConfig::new(api_base, API_KEY_SENTINEL).unwrap();
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, BINDING).unwrap();
    let options = CodexHttpV1Options::new("test-model", None, None, None::<String>).unwrap();
    let _provider =
        AsyncOpenAiResponsesProvider::new(config, identity, CodexHttpV1Encoder::new(options));
    panic!("legacy Responses constructor unexpectedly returned");
}

#[cfg(all(
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple"),
    not(target_arch = "wasm32")
))]
fn run_legacy_panic_child(test_name: &str, child_mode: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_base = format!("https://{}/v1", listener.local_addr().unwrap());
    let missing_ca = std::env::current_exe().unwrap().join(MISSING_CA_SENTINEL);
    assert!(!missing_ca.exists());

    let output = Command::new(std::env::current_exe().unwrap())
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, child_mode)
        .env(BASE_URL_ENV, &api_base)
        .env(URL_CREDENTIAL_ENV, credentialed_url())
        .env("SSL_CERT_FILE", &missing_ca)
        .env_remove("SSL_CERT_DIR")
        .output()
        .unwrap();

    assert!(!output.status.success(), "legacy constructor did not panic");
    let output_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output_text.contains("OpenAI transport initialization failed"),
        "legacy panic omitted its fixed sanitized message"
    );
    assert_sanitized(
        &output_text,
        &[
            &missing_ca.to_string_lossy(),
            &api_base,
            API_KEY_SENTINEL,
            RAW_SOURCE_SENTINEL,
        ],
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

fn credentialed_url() -> String {
    format!("https://{URL_USERNAME_SENTINEL}:{URL_PASSWORD_SENTINEL}@example.invalid/v1")
}

fn assert_credentialed_url_is_sanitized() {
    const MESSAGE: &str =
        "expected HTTPS, or HTTP with a loopback host, without userinfo, query, or fragment";

    let credentialed_api_base =
        std::env::var(URL_CREDENTIAL_ENV).expect("parent supplies a synthetic credentialed URL");
    let error = match AsyncOpenAiTransportConfig::new(&credentialed_api_base, API_KEY_SENTINEL) {
        Ok(_) => panic!("credentialed URL unexpectedly passed validation"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        AsyncOpenAiConfigError::InvalidApiBase {
            message: MESSAGE.to_owned(),
        }
    );
    assert_eq!(
        format!("{error:?}"),
        format!("InvalidApiBase {{ message: {MESSAGE:?} }}")
    );
    assert_eq!(
        error.to_string(),
        format!("invalid OpenAI API base: {MESSAGE}")
    );
    assert!(error.source().is_none());
    assert_sanitized(
        &format!("{error:?}\n{error}"),
        &[&credentialed_api_base, API_KEY_SENTINEL],
    );
}

#[test]
#[should_panic(expected = "output leaked a forbidden sentinel")]
fn sanitizer_rejects_username_sentinel_in_isolation() {
    assert_sanitized(URL_USERNAME_SENTINEL, &[]);
}

#[test]
#[should_panic(expected = "output leaked a forbidden sentinel")]
fn sanitizer_rejects_password_sentinel_in_isolation() {
    assert_sanitized(URL_PASSWORD_SENTINEL, &[]);
}

fn assert_sanitized(text: &str, sentinels: &[&str]) {
    for sentinel in sentinels.iter().copied().chain([
        "SSL_CERT_FILE",
        MISSING_CA_SENTINEL,
        URL_CREDENTIAL_ENV,
        URL_USERNAME_SENTINEL,
        URL_PASSWORD_SENTINEL,
    ]) {
        assert!(
            !text.contains(sentinel),
            "output leaked a forbidden sentinel"
        );
    }
}
