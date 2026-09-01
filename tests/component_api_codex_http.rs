use agentview::{
    pom::{Document, ResolvedDocument, TextNode, XmlName, XmlNode},
    pom_resolution::resolve_artifact_document,
    provider::{
        async_openai::{AsyncOpenAiConfigError, AsyncOpenAiTransportConfig},
        codex_http_v1::{
            CodexFunctionTool, CodexHttpV1Encoder, CodexHttpV1Error, CodexHttpV1HistoryPolicy,
            CodexHttpV1Options, CodexReasoning, CODEX_HTTP_V1_CODEX_REVISION,
            CODEX_HTTP_V1_PROFILE,
        },
        HistoryPolicy, ProviderRequestEncoder,
    },
    record_store::{
        reconstruct_transcript, MemoryRecordStore, NewRecord, ProviderArtifactMode, RecordBatch,
        RecordEnvelope, RecordLog, SessionId, SessionRecord,
    },
    transcript::{
        CanonicalInputItem, CanonicalTranscript, ConversationRole, InstructionAuthority,
        ProviderExtension, TranscriptRevision,
    },
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const ORACLE_BODY: &[u8] = include_bytes!("fixtures/codex_http_v1/request.body");
const ORACLE_METADATA: &str = include_str!("fixtures/codex_http_v1/metadata.json");

fn xml_document(name: &str, attributes: &[(&str, &str)], text: &str) -> ResolvedDocument {
    let mut node = XmlNode::try_build(name, |children| {
        children.text(TextNode::new(text));
        Ok(())
    })
    .unwrap();
    for (name, value) in attributes {
        node.push_attribute(XmlName::new(*name).unwrap(), *value)
            .unwrap();
    }
    resolve_artifact_document(Document::from_xml(node)).unwrap()
}

fn oracle_transcript() -> CanonicalTranscript {
    let transcript = [
        CanonicalInputItem::instruction(
            InstructionAuthority::System,
            xml_document("chess_rules", &[], "Play legal chess."),
        ),
        CanonicalInputItem::instruction(
            InstructionAuthority::Developer,
            xml_document("chess_reply_protocol", &[], "Return exactly one move."),
        ),
        CanonicalInputItem::message(
            ConversationRole::User,
            xml_document(
                "chess_task",
                &[("turn_id", "turn-7")],
                "Choose a move & explain.",
            ),
        ),
    ]
    .into_iter()
    .try_fold(CanonicalTranscript::new(), |transcript, item| {
        transcript.appended(item)
    })
    .unwrap();
    let reasoning = ProviderExtension::new(
        "openai",
        "reasoning.encrypted_content",
        1,
        json!({
            "summary": [{"type": "summary_text", "text": "Inspect the legal move set."}],
            "content": null,
            "encrypted_content": "encrypted:codex-http-v1"
        }),
    )
    .unwrap();

    [
        CanonicalInputItem::provider_extension(reasoning),
        CanonicalInputItem::tool_call(
            "call_7",
            "legal_moves",
            r#"{"fen":"startpos","note":"a\\b\nline"}"#,
        )
        .unwrap(),
        CanonicalInputItem::tool_result("call_7", r#"["e2e4","d2d4"]"#).unwrap(),
        CanonicalInputItem::message(
            ConversationRole::User,
            xml_document("continue", &[], "Choose from the legal moves."),
        ),
    ]
    .into_iter()
    .try_fold(transcript, |transcript, item| transcript.appended(item))
    .unwrap()
}

fn oracle_encoder() -> CodexHttpV1Encoder {
    let tool = CodexFunctionTool::new(
        "legal_moves",
        "List legal chess moves.",
        json!({
            "additionalProperties": false,
            "properties": {"fen": {"type": "string"}},
            "required": ["fen"],
            "type": "object"
        }),
        false,
    )
    .unwrap();
    let options = CodexHttpV1Options::new(
        "gpt-5.6-codex",
        Some(vec![tool]),
        Some(CodexReasoning::max_detailed()),
        Some("agentview-session-7"),
    )
    .unwrap();
    CodexHttpV1Encoder::new(options)
}

#[test]
fn fixture_metadata_pins_the_profile_revision_and_body_digest() {
    let metadata: Value = serde_json::from_str(ORACLE_METADATA).unwrap();

    assert_eq!(metadata["profile"], CODEX_HTTP_V1_PROFILE);
    assert_eq!(metadata["codex_revision"], CODEX_HTTP_V1_CODEX_REVISION);
    assert_eq!(metadata["body_bytes"], ORACLE_BODY.len());
    assert_eq!(
        metadata["body_sha256"],
        format!("{:x}", Sha256::digest(ORACLE_BODY))
    );
}

#[test]
fn codex_http_v1_matches_the_real_request_body_byte_for_byte() {
    let encoded = oracle_encoder()
        .encode_request(&oracle_transcript())
        .unwrap();

    assert_eq!(encoded.as_slice(), ORACLE_BODY);
}

#[test]
fn raw_tool_arguments_keep_whitespace_key_order_and_escaping() {
    let raw_arguments = r#"{ "z" : "a\\b\nline", "a" : 1 }"#;
    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::tool_call("call_raw", "inspect", raw_arguments).unwrap())
        .unwrap();
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap();

    let encoded = CodexHttpV1Encoder::new(options)
        .encode_request(&transcript)
        .unwrap();
    let body: Value = serde_json::from_slice(&encoded).unwrap();
    let arguments = body["input"][0]["arguments"].as_str().unwrap();
    let encoded_arguments = format!(
        "\"arguments\":{}",
        serde_json::to_string(raw_arguments).unwrap()
    );

    assert_eq!(arguments, raw_arguments);
    assert!(String::from_utf8(encoded)
        .unwrap()
        .contains(&encoded_arguments));
}

#[test]
fn request_dto_locks_field_order_and_absent_field_omission() {
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap();
    let encoded = CodexHttpV1Encoder::new(options)
        .encode_request(&CanonicalTranscript::new())
        .unwrap();
    let body: Value = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(body["parallel_tool_calls"], false);

    assert_eq!(
        String::from_utf8(encoded).unwrap(),
        r#"{"model":"gpt-5.6-codex","input":[],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":null,"context_management":[{"type":"compaction","compact_threshold":200000}],"store":false,"stream":true,"include":["reasoning.encrypted_content"]}"#
    );
}

#[test]
fn history_policy_maps_assistant_text_to_output_text() {
    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::message(
            ConversationRole::Assistant,
            xml_document("move", &[], "e2e4"),
        ))
        .unwrap();
    let options = CodexHttpV1Options::new("gpt-5.6-codex", None, None, None::<String>).unwrap();

    let encoded = CodexHttpV1Encoder::new(options)
        .encode_request(&transcript)
        .unwrap();
    let body: Value = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(body["input"][0]["role"], "assistant");
    assert_eq!(body["input"][0]["content"][0]["type"], "output_text");
    assert_eq!(body["input"][0]["content"][0]["text"], "<move>e2e4</move>");
}

#[test]
fn interrupted_assistant_text_fails_closed_until_wire_grouping_is_supported() {
    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::interrupted_assistant_text(
            "partial response",
            None,
        ))
        .unwrap();

    let error = CodexHttpV1HistoryPolicy
        .project_history(&transcript)
        .expect_err("interrupted text must not be silently encoded as sealed output");
    assert!(matches!(
        error,
        CodexHttpV1Error::InterruptedAssistantTextUnsupported
    ));
}

#[test]
fn codex_function_names_fail_before_an_invalid_request_is_encoded() {
    for name in ["bad.name".to_owned(), "x".repeat(129)] {
        let error = CodexFunctionTool::new(name.clone(), "invalid", json!({}), false)
            .expect_err("Responses function names must match the pinned Codex limits");
        assert!(matches!(
            error,
            CodexHttpV1Error::InvalidToolName { name: ref actual } if actual == &name
        ));
    }

    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::tool_call("call_1", "bad/name", "{}").unwrap())
        .unwrap();
    let error = CodexHttpV1HistoryPolicy
        .project_history(&transcript)
        .expect_err("canonical names valid for another provider must fail in this policy");
    assert!(matches!(
        error,
        CodexHttpV1Error::InvalidToolName { ref name } if name == "bad/name"
    ));
}

#[test]
fn request_options_and_tool_schemas_validate_their_boundaries() {
    assert!(matches!(
        CodexHttpV1Options::new("", None, None, None::<String>),
        Err(CodexHttpV1Error::EmptyModel)
    ));
    assert!(matches!(
        CodexHttpV1Options::new("model", None, None, Some("")),
        Err(CodexHttpV1Error::EmptyPromptCacheKey)
    ));
    assert!(matches!(
        CodexFunctionTool::new("tool", "invalid", json!([]), false),
        Err(CodexHttpV1Error::ToolParametersMustBeObject { .. })
    ));

    let tool = CodexFunctionTool::new("tool", "valid", json!({}), false).unwrap();
    assert!(matches!(
        CodexHttpV1Options::new(
            "model",
            Some(vec![tool.clone(), tool]),
            None,
            None::<String>,
        ),
        Err(CodexHttpV1Error::DuplicateToolName { ref name }) if name == "tool"
    ));
}

#[test]
fn async_openai_transport_config_rejects_invalid_endpoints_and_empty_keys() {
    for api_base in [
        "not-a-url",
        "ftp://example.test",
        "http://example.test",
        "http://example.test?query=forbidden",
        "http://example.test#fragment-forbidden",
        "http://user@127.0.0.1",
        "https://user:password@example.test",
    ] {
        assert!(matches!(
            AsyncOpenAiTransportConfig::new(api_base, "test-token"),
            Err(AsyncOpenAiConfigError::InvalidApiBase { .. })
        ));
    }
    assert!(matches!(
        AsyncOpenAiTransportConfig::new("https://api.openai.com/v1", ""),
        Err(AsyncOpenAiConfigError::EmptyApiKey)
    ));

    for api_base in [
        "http://localhost:8080/v1",
        "http://127.0.0.1:8080/v1",
        "http://[::1]:8080/v1",
        "https://api.openai.com/v1",
    ] {
        assert!(AsyncOpenAiTransportConfig::new(api_base, "test-token").is_ok());
    }
}

#[test]
fn async_openai_transport_config_rejects_non_printable_or_non_ascii_api_keys() {
    for api_key in [
        "test\ttoken",
        "test\ntoken",
        "test\r\ntoken",
        "test\u{0000}token",
        "test\u{007f}token",
        "test-\u{00e9}-token",
    ] {
        assert!(matches!(
            AsyncOpenAiTransportConfig::new("https://api.openai.com/v1", api_key),
            Err(AsyncOpenAiConfigError::InvalidApiKey)
        ));
    }
}

#[test]
fn a_second_system_instruction_fails_closed() {
    let transcript = ["first", "second"]
        .into_iter()
        .map(|text| {
            CanonicalInputItem::instruction(
                InstructionAuthority::System,
                xml_document("system", &[], text),
            )
        })
        .try_fold(CanonicalTranscript::new(), |transcript, item| {
            transcript.appended(item)
        })
        .unwrap();

    let error = CodexHttpV1HistoryPolicy
        .project_history(&transcript)
        .expect_err("the top-level instructions field cannot preserve two System items");
    assert!(matches!(
        error,
        CodexHttpV1Error::MultipleSystemInstructions
    ));
}

#[test]
fn provider_extensions_fail_closed_without_an_exact_capability_match() {
    let unsupported = [
        ("anthropic", "reasoning.encrypted_content", 1),
        ("openai", "reasoning.summary", 1),
        ("openai", "reasoning.encrypted_content", 2),
    ];

    for (provider, capability, schema_version) in unsupported {
        let extension = ProviderExtension::new(
            provider,
            capability,
            schema_version,
            json!({
                "summary": [],
                "content": null,
                "encrypted_content": "opaque"
            }),
        )
        .unwrap();
        let transcript = CanonicalTranscript::new()
            .appended(CanonicalInputItem::provider_extension(extension))
            .unwrap();

        let error = CodexHttpV1HistoryPolicy
            .project_history(&transcript)
            .expect_err("unknown provider capabilities must not be silently omitted");
        assert!(matches!(
            error,
            CodexHttpV1Error::UnsupportedProviderExtension {
                provider: ref actual_provider,
                capability: ref actual_capability,
                schema_version: actual_schema,
            } if actual_provider == provider
                && actual_capability == capability
                && actual_schema == schema_version
        ));
    }
}

#[test]
fn malformed_supported_provider_extensions_also_fail_closed() {
    let extension = ProviderExtension::new(
        "openai",
        "reasoning.encrypted_content",
        1,
        json!({
            "summary": [],
            "content": {"unexpected": true},
            "encrypted_content": "opaque"
        }),
    )
    .unwrap();
    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::provider_extension(extension))
        .unwrap();

    let error = CodexHttpV1HistoryPolicy
        .project_history(&transcript)
        .expect_err("schema-invalid opaque content must not enter a request");
    assert!(matches!(
        error,
        CodexHttpV1Error::InvalidProviderExtensionPayload { .. }
    ));
}

#[tokio::test]
async fn persistence_round_trip_reencodes_the_same_exact_oracle_bytes() {
    let transcript = oracle_transcript();
    let system_artifact = match transcript.items().first() {
        Some(CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            pom,
        }) => pom.clone(),
        _ => panic!("the Codex oracle must start with one System artifact"),
    };
    let contract_manifest =
        br#"{"schema":"agentview.contract-manifest/v1","listeners":[]}"#.to_vec();
    let artifact_fingerprint = format!(
        "agentview.system-artifact/v1:sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&system_artifact).unwrap())
    );
    let contract_fingerprint = format!(
        "agentview.contract-manifest/v1:sha256:{:x}",
        Sha256::digest(&contract_manifest)
    );
    let mut records = vec![
        NewRecord::new(
            1,
            SessionRecord::ProviderEpochOpened {
                provider_epoch_id: "codex-http-oracle-epoch".to_owned(),
                provider: "openai".to_owned(),
                profile: CODEX_HTTP_V1_PROFILE.to_owned(),
                profile_version: 1,
                provider_binding: "test-openai-account".to_owned(),
                artifact_mode: ProviderArtifactMode::Stateless,
                base_transcript_revision: TranscriptRevision::new(0),
            },
        )
        .unwrap(),
        NewRecord::new(
            1,
            SessionRecord::EpochArtifactCommitted {
                epoch_id: "codex-http-oracle-epoch".to_owned(),
                artifact_fingerprint: artifact_fingerprint.clone(),
                contract_fingerprint: contract_fingerprint.clone(),
                artifact: system_artifact,
                contract_manifest,
            },
        )
        .unwrap(),
        NewRecord::new(
            1,
            SessionRecord::EpochArtifactLogicallyAttached {
                epoch_id: "codex-http-oracle-epoch".to_owned(),
                attachment_id: "codex-http-oracle-attachment".to_owned(),
                artifact_fingerprint,
                contract_fingerprint,
            },
        )
        .unwrap(),
    ];
    records.extend(
        transcript
            .items()
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| {
                let revision = TranscriptRevision::new(u64::try_from(index).unwrap() + 1);
                NewRecord::new(
                    1,
                    SessionRecord::CanonicalItemAppended {
                        transcript_revision: revision,
                        item,
                    },
                )
                .unwrap()
            }),
    );
    let store = MemoryRecordStore::new();
    let session = SessionId::new("codex-http-oracle-session").unwrap();

    store
        .append(
            &session,
            RecordEnvelope::INITIAL_TAIL,
            RecordBatch::new(records).unwrap(),
        )
        .await
        .unwrap();
    let persisted = store
        .read(&session, RecordEnvelope::INITIAL_TAIL)
        .await
        .unwrap();
    let durable_bytes = serde_json::to_vec(&persisted).unwrap();
    let restored_records: Vec<RecordEnvelope> = serde_json::from_slice(&durable_bytes).unwrap();
    let restored = reconstruct_transcript(&restored_records).unwrap();

    assert_eq!(restored, transcript);
    assert_eq!(
        oracle_encoder().encode_request(&restored).unwrap(),
        ORACLE_BODY
    );
}
