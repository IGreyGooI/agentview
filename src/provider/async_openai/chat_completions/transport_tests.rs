#![cfg(all(
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple"),
    not(target_arch = "wasm32")
))]

use std::{error::Error, io::ErrorKind, net::TcpListener, process::Command};

use crate::component::execution::ProviderIdentity;

use super::super::AsyncOpenAiTransportConfig;
use super::{
    AsyncOpenAiChatCompletionsProvider, OpenAiChatCompletionsOptions,
    OPENAI_CHAT_COMPLETIONS_PROFILE,
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
fn chat_try_new_returns_a_sanitized_initialization_error_without_requesting() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_try_new_failure_child();
        return;
    }

    assert_credentialed_url_is_sanitized();
    let api_base = std::env::var(BASE_URL_ENV).expect("parent supplies a synthetic base URL");
    let config = AsyncOpenAiTransportConfig::new(&api_base, API_KEY_SENTINEL).unwrap();
    let identity =
        ProviderIdentity::new("openai", OPENAI_CHAT_COMPLETIONS_PROFILE, 1, BINDING).unwrap();
    let options = OpenAiChatCompletionsOptions::new("test-chat-model").unwrap();

    let error = match AsyncOpenAiChatCompletionsProvider::try_new(config, identity, options) {
        Ok(_) => panic!("missing CA configuration unexpectedly initialized Chat Completions"),
        Err(error) => error,
    };

    assert_eq!(error.to_string(), "OpenAI transport initialization failed");
    assert_sanitized(
        &rendered_error_chain(&error),
        &[&api_base, API_KEY_SENTINEL, RAW_SOURCE_SENTINEL],
    );
}

#[test]
fn chat_legacy_new_panics_with_only_a_fixed_sanitized_message() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_legacy_panic_child();
        return;
    }

    assert_credentialed_url_is_sanitized();
    let api_base = std::env::var(BASE_URL_ENV).expect("parent supplies a synthetic base URL");
    let config = AsyncOpenAiTransportConfig::new(api_base, API_KEY_SENTINEL).unwrap();
    let identity =
        ProviderIdentity::new("openai", OPENAI_CHAT_COMPLETIONS_PROFILE, 1, BINDING).unwrap();
    let options = OpenAiChatCompletionsOptions::new("test-chat-model").unwrap();
    let _provider = AsyncOpenAiChatCompletionsProvider::new(config, identity, options);
    panic!("legacy Chat Completions constructor unexpectedly returned");
}

fn run_try_new_failure_child() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_base = format!("https://{}/v1", listener.local_addr().unwrap());
    let missing_ca = std::env::current_exe().unwrap().join(MISSING_CA_SENTINEL);
    assert!(!missing_ca.exists());
    let test_name =
        libtest_name("chat_try_new_returns_a_sanitized_initialization_error_without_requesting");

    let output = Command::new(std::env::current_exe().unwrap())
        .arg(&test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, "chat")
        .env(BASE_URL_ENV, &api_base)
        .env(URL_CREDENTIAL_ENV, credentialed_url())
        .env("SSL_CERT_FILE", &missing_ca)
        .env_remove("SSL_CERT_DIR")
        .output()
        .unwrap();

    let output_text = rendered_child_output(&output);
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
        "isolated Chat initialization child failed"
    );
    assert_child_ran_one_test(&output_text, &test_name, "ok");
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

fn run_legacy_panic_child() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_base = format!("https://{}/v1", listener.local_addr().unwrap());
    let missing_ca = std::env::current_exe().unwrap().join(MISSING_CA_SENTINEL);
    assert!(!missing_ca.exists());
    let test_name = libtest_name("chat_legacy_new_panics_with_only_a_fixed_sanitized_message");

    let output = Command::new(std::env::current_exe().unwrap())
        .arg(&test_name)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, "chat-legacy")
        .env(BASE_URL_ENV, &api_base)
        .env(URL_CREDENTIAL_ENV, credentialed_url())
        .env("SSL_CERT_FILE", &missing_ca)
        .env_remove("SSL_CERT_DIR")
        .output()
        .unwrap();

    let output_text = rendered_child_output(&output);
    assert_sanitized(
        &output_text,
        &[
            &missing_ca.to_string_lossy(),
            &api_base,
            API_KEY_SENTINEL,
            RAW_SOURCE_SENTINEL,
        ],
    );
    assert!(!output.status.success(), "legacy constructor did not panic");
    assert_child_ran_one_test(&output_text, &test_name, "FAILED");
    assert!(
        output_text.contains("OpenAI transport initialization failed"),
        "legacy panic omitted its fixed sanitized message"
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

fn libtest_name(test_name: &str) -> String {
    let crate_prefix = concat!(env!("CARGO_CRATE_NAME"), "::");
    let module_path = module_path!()
        .strip_prefix(crate_prefix)
        .expect("unit-test module path starts with the crate name");
    format!("{module_path}::{test_name}")
}

fn assert_child_ran_one_test(output_text: &str, test_name: &str, result: &str) {
    assert!(
        output_text.contains("running 1 test"),
        "isolated child did not run exactly one test"
    );
    assert!(
        output_text.contains(&format!("test {test_name} ... {result}")),
        "isolated child did not execute the expected test"
    );
}

fn rendered_child_output(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
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
        error.to_string(),
        format!("invalid OpenAI API base: {MESSAGE}")
    );
    assert_sanitized(
        &rendered_error_chain(&error),
        &[&credentialed_api_base, API_KEY_SENTINEL],
    );
}

fn rendered_error_chain(error: &impl Error) -> String {
    let mut rendered = format!("{error:?}\n{error}");
    let mut source = error.source();
    while let Some(source_error) = source {
        rendered.push('\n');
        rendered.push_str(&format!("{source_error:?}\n{source_error}"));
        source = source_error.source();
    }
    rendered
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
