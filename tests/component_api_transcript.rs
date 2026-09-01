use agentview::{
    pom::{Document, ResolvedDocument},
    pom_renderer::render_pom_document,
    pom_resolution::resolve_artifact_document,
    transcript::{
        AssistantPhase, AssistantTextStatus, CanonicalInputItem, CanonicalTranscript,
        CanonicalTranscriptError, ConversationRole, InstructionAuthority, ProviderExtension,
    },
};
use serde_json::json;

fn paragraph(text: &str) -> ResolvedDocument {
    resolve_artifact_document(
        Document::try_build(|blocks| {
            blocks.try_paragraph(|inline| {
                inline.try_text(text)?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn canonical_transcript_is_immutable_ordered_pom_history() {
    let empty = CanonicalTranscript::new();
    let system = CanonicalInputItem::instruction(
        InstructionAuthority::System,
        paragraph("Play legal chess."),
    );
    let with_system = empty.appended(system).unwrap();
    let with_user = with_system
        .appended(CanonicalInputItem::message(
            ConversationRole::User,
            paragraph("Choose one move."),
        ))
        .unwrap();

    assert_eq!(empty.revision().get(), 0);
    assert_eq!(with_system.revision().get(), 1);
    assert_eq!(with_user.revision().get(), 2);
    assert!(empty.items().is_empty());
    assert_eq!(with_system.items().len(), 1);
    assert_eq!(with_user.items().len(), 2);

    let encoded = serde_json::to_vec(&with_user).unwrap();
    let restored: CanonicalTranscript = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(restored, with_user);

    let CanonicalInputItem::Message { pom, .. } = &restored.items()[1] else {
        panic!("the second item must remain a canonical message");
    };
    assert_eq!(render_pom_document(pom).unwrap(), "Choose one move.");
}

#[test]
fn sealed_assistant_text_keeps_its_legacy_json_and_interrupted_is_explicit() {
    let sealed = CanonicalInputItem::assistant_text("complete", Some(AssistantPhase::FinalAnswer));
    let legacy =
        br#"{"kind":"assistant_text","payload":{"text":"complete","phase":"final_answer"}}"#;

    assert_eq!(serde_json::to_vec(&sealed).unwrap(), legacy);
    assert_eq!(
        serde_json::from_slice::<CanonicalInputItem>(legacy).unwrap(),
        sealed
    );

    let interrupted = CanonicalInputItem::interrupted_assistant_text("partial", None);
    assert!(matches!(
        interrupted,
        CanonicalInputItem::AssistantText {
            status: AssistantTextStatus::Interrupted,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_value(&interrupted).unwrap(),
        json!({
            "kind": "assistant_text",
            "payload": {
                "text": "partial",
                "phase": null,
                "status": "interrupted"
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<CanonicalInputItem>(serde_json::to_value(&interrupted).unwrap())
            .unwrap(),
        interrupted
    );
}

#[test]
fn raw_tool_arguments_are_validated_without_reserialization() {
    let raw_arguments = r#"{ "fen" : "startpos", "line" : "a\\b\nnext" }"#;
    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::tool_call("call_7", "legal_moves", raw_arguments).unwrap())
        .unwrap();

    let CanonicalInputItem::ToolCall {
        raw_arguments: stored,
        ..
    } = &transcript.items()[0]
    else {
        panic!("the canonical item must remain a tool call");
    };
    assert_eq!(stored, raw_arguments);

    let error = CanonicalInputItem::tool_call("call_8", "legal_moves", "{broken")
        .expect_err("invalid JSON arguments must fail at the canonical boundary");
    assert!(matches!(
        error,
        CanonicalTranscriptError::InvalidToolArguments { .. }
    ));
}

#[test]
fn tool_results_require_one_earlier_matching_call() {
    let result = CanonicalInputItem::tool_result("call_7", r#"["e2e4"]"#).unwrap();
    let error = CanonicalTranscript::new()
        .appended(result.clone())
        .expect_err("an orphan result cannot enter canonical history");
    assert!(matches!(
        error,
        CanonicalTranscriptError::UnknownToolCall { ref call_id } if call_id == "call_7"
    ));

    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::tool_call("call_7", "legal_moves", "{}").unwrap())
        .unwrap()
        .appended(result)
        .unwrap();
    assert_eq!(transcript.revision().get(), 2);

    let duplicate = transcript
        .appended(CanonicalInputItem::tool_result("call_7", "duplicate").unwrap())
        .expect_err("one canonical tool call has at most one canonical result");
    assert!(matches!(
        duplicate,
        CanonicalTranscriptError::DuplicateToolResult { ref call_id } if call_id == "call_7"
    ));
}

#[test]
fn provider_extensions_are_scoped_and_schema_versioned() {
    let extension = ProviderExtension::new(
        "openai",
        "reasoning.encrypted_content",
        1,
        json!({
            "summary": [{"type": "summary_text", "text": "Inspect moves."}],
            "content": null,
            "encrypted_content": "encrypted:codex-http-v1"
        }),
    )
    .unwrap();
    let transcript = CanonicalTranscript::new()
        .appended(CanonicalInputItem::provider_extension(extension))
        .unwrap();

    let CanonicalInputItem::ProviderExtension(extension) = &transcript.items()[0] else {
        panic!("the extension must not become a portable message");
    };
    assert_eq!(extension.provider(), "openai");
    assert_eq!(extension.capability(), "reasoning.encrypted_content");
    assert_eq!(extension.schema_version(), 1);

    let error = ProviderExtension::new("openai", "reasoning.encrypted_content", 0, json!({}))
        .expect_err("schema version zero is not durable");
    assert!(matches!(error, CanonicalTranscriptError::ZeroSchemaVersion));
}

#[test]
fn provider_extension_deserialization_cannot_bypass_constructor_validation() {
    let invalid = json!({
        "provider": "invalid provider",
        "capability": "reasoning.encrypted_content",
        "schema_version": 0,
        "payload": {}
    });

    assert!(serde_json::from_value::<ProviderExtension>(invalid).is_err());
}

#[test]
fn canonical_item_deserialization_cannot_bypass_constructor_validation() {
    let invalid_tool_call = json!({
        "kind": "tool_call",
        "payload": {
            "call_id": "call-1",
            "name": "legal_moves",
            "raw_arguments": "[]"
        }
    });
    let invalid_extension = json!({
        "kind": "provider_extension",
        "payload": {
            "provider": "openai",
            "capability": "reasoning.encrypted_content",
            "schema_version": 0,
            "payload": {}
        }
    });

    assert!(serde_json::from_value::<CanonicalInputItem>(invalid_tool_call).is_err());
    assert!(serde_json::from_value::<CanonicalInputItem>(invalid_extension).is_err());
}
