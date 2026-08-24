use agentview::component::execution::{
    ProviderFault, ProviderFaultCode, ProviderFaultKind, ProviderResponseMessageTextReason,
    ProviderResponseOutputIdentityDetail, ProviderResponseOutputIdentityKindPair,
    ProviderResponseOutputIdentityLifecycleState, ProviderResponseOutputIdentityMappingBasis,
    ProviderResponseOutputIdentityObservedMessageRelation,
    ProviderResponseOutputIdentityObservedTextRelation, ProviderResponseOutputIdentityReason,
};

fn consumer_code(code: ProviderFaultCode) -> &'static str {
    match code {
        ProviderFaultCode::Transport => "transport",
        ProviderFaultCode::RequestPreparation => "request_preparation",
        ProviderFaultCode::ConnectSecureTransport => "connect_secure_transport",
        ProviderFaultCode::RequestTransport => "request_transport",
        ProviderFaultCode::RequestTimeout => "request_timeout",
        ProviderFaultCode::Authentication => "authentication",
        ProviderFaultCode::Authorization => "authorization",
        ProviderFaultCode::RateLimited => "rate_limited",
        ProviderFaultCode::UpstreamStatus => "upstream_status",
        ProviderFaultCode::ResponseProtocol => "response_protocol",
        ProviderFaultCode::ResponseContentType => "response_content_type",
        ProviderFaultCode::StreamDecode => "stream_decode",
        ProviderFaultCode::ResponseEventJson => "response_event_json",
        ProviderFaultCode::ResponseEventShape => "response_event_shape",
        ProviderFaultCode::StreamTransport => "stream_transport",
        ProviderFaultCode::StreamTimeout => "stream_timeout",
        ProviderFaultCode::ResponseBodyLimit => "response_body_limit",
        ProviderFaultCode::StreamEventLimit => "stream_event_limit",
        ProviderFaultCode::OutputLimit => "output_limit",
        ProviderFaultCode::ModelRejected => "model_rejected",
    }
}

#[test]
fn message_text_diagnostic_reason_codes_are_closed_payload_free_and_typed() {
    let reasons = [
        ProviderResponseMessageTextReason::InvalidTerminalMessageShape,
        ProviderResponseMessageTextReason::TerminalObservedTextMismatch,
        ProviderResponseMessageTextReason::ApplicablePhaseMismatch,
    ];

    assert_eq!(
        reasons.map(|reason| reason.code()),
        [
            "invalid_terminal_message_shape",
            "terminal_observed_text_mismatch",
            "applicable_phase_mismatch",
        ]
    );
    assert_eq!(
        reasons.map(|reason| serde_json::to_value(reason).unwrap()),
        [
            serde_json::json!("invalid_terminal_message_shape"),
            serde_json::json!("terminal_observed_text_mismatch"),
            serde_json::json!("applicable_phase_mismatch"),
        ]
    );

    for reason in reasons {
        let fault = ProviderFault::model_rejected(format!(
            "provider-placeholder [response_event_type=response.completed; \
             response_event_reason=ledger_mismatch; response_ledger_reason=message_text; \
             response_message_text_reason={}]",
            reason.code()
        ))
        .with_code(ProviderFaultCode::ResponseEventShape);
        let diagnostic = fault
            .response_event_diagnostic()
            .expect("response shape fault has a typed diagnostic");

        assert_eq!(diagnostic.response_message_text_reason(), Some(reason));
    }

    let unknown = ProviderFault::model_rejected(
        "provider-placeholder [response_event_type=response.completed; \
         response_event_reason=ledger_mismatch; response_ledger_reason=message_text; \
         response_message_text_reason=unknown-placeholder]",
    )
    .with_code(ProviderFaultCode::ResponseEventShape)
    .response_event_diagnostic()
    .expect("the surrounding response diagnostic remains typed");
    assert_eq!(
        unknown.response_ledger_reason(),
        Some(agentview::component::execution::ProviderResponseLedgerReason::MessageText)
    );
    assert_eq!(unknown.response_message_text_reason(), None);
}

#[test]
fn completed_reconciliation_snapshot_is_typed_structural_and_atomically_optional() {
    let reasons = [
        ProviderResponseMessageTextReason::InvalidTerminalMessageShape,
        ProviderResponseMessageTextReason::TerminalObservedTextMismatch,
        ProviderResponseMessageTextReason::ApplicablePhaseMismatch,
    ];

    for reason in reasons {
        let expected = reconciliation_snapshot_fixture(reason.code());
        let fault = ProviderFault::model_rejected(format!(
            "provider-placeholder [response_event_type=response.completed; \
             response_event_reason=ledger_mismatch; response_ledger_reason=message_text; \
             response_message_text_reason={}; response_completed_reconciliation={expected}]",
            reason.code(),
        ))
        .with_code(ProviderFaultCode::ResponseEventShape);
        let diagnostic = fault
            .response_event_diagnostic()
            .expect("response shape fault has a typed diagnostic");

        assert_eq!(diagnostic.response_message_text_reason(), Some(reason));
        assert_eq!(
            serde_json::to_value(
                diagnostic
                    .response_completed_reconciliation()
                    .expect("known structural snapshot survives typed parsing"),
            )
            .unwrap(),
            expected,
        );
    }

    let unknown_snapshot = serde_json::json!({
        "branch": "unknown-placeholder",
        "response_completed_sequence": 9,
    });
    let diagnostic = ProviderFault::model_rejected(format!(
        "provider-placeholder [response_event_type=response.completed; \
         response_event_reason=ledger_mismatch; response_ledger_reason=message_text; \
         response_message_text_reason=terminal_observed_text_mismatch; \
         response_completed_reconciliation={unknown_snapshot}]",
    ))
    .with_code(ProviderFaultCode::ResponseEventShape)
    .response_event_diagnostic()
    .expect("the surrounding response diagnostic remains typed");
    assert_eq!(
        diagnostic.response_ledger_reason(),
        Some(agentview::component::execution::ProviderResponseLedgerReason::MessageText)
    );
    assert_eq!(
        diagnostic.response_message_text_reason(),
        Some(ProviderResponseMessageTextReason::TerminalObservedTextMismatch)
    );
    assert_eq!(diagnostic.response_completed_reconciliation(), None);

    const NULLABLE_KEYS: [&str; 13] = [
        "response_created_sequence",
        "response_in_progress_sequence",
        "terminal_output_index",
        "observed_lifecycle_index",
        "observed_lifecycle_state",
        "mapping_basis",
        "output_item_added_sequence",
        "content_part_added_sequence",
        "first_text_delta_sequence",
        "last_text_delta_sequence",
        "text_done_sequence",
        "content_part_done_sequence",
        "output_item_done_sequence",
    ];
    let mut explicit_nulls = reconciliation_snapshot_fixture("terminal_observed_text_mismatch");
    let explicit_nulls_object = explicit_nulls
        .as_object_mut()
        .expect("the structural fixture is an object");
    for key in NULLABLE_KEYS {
        explicit_nulls_object.insert(key.to_owned(), serde_json::Value::Null);
    }
    let explicit_nulls_diagnostic = reconciliation_diagnostic(&explicit_nulls);
    assert_eq!(
        serde_json::to_value(
            explicit_nulls_diagnostic
                .response_completed_reconciliation()
                .expect("explicit nulls preserve a complete structural snapshot"),
        )
        .unwrap(),
        explicit_nulls,
    );

    for missing_key in NULLABLE_KEYS {
        let mut incomplete = explicit_nulls.clone();
        incomplete
            .as_object_mut()
            .expect("the structural fixture is an object")
            .remove(missing_key);
        let diagnostic = reconciliation_diagnostic(&incomplete);

        assert_eq!(
            diagnostic.response_completed_reconciliation(),
            None,
            "omitting {missing_key} must discard the incomplete snapshot",
        );
        assert_eq!(
            diagnostic.response_message_text_reason(),
            Some(ProviderResponseMessageTextReason::TerminalObservedTextMismatch),
        );
    }
}

fn reconciliation_diagnostic(
    snapshot: &serde_json::Value,
) -> agentview::component::execution::ProviderResponseEventDiagnostic {
    ProviderFault::model_rejected(format!(
        "provider-placeholder [response_event_type=response.completed; \
         response_event_reason=ledger_mismatch; response_ledger_reason=message_text; \
         response_message_text_reason=terminal_observed_text_mismatch; \
         response_completed_reconciliation={snapshot}]",
    ))
    .with_code(ProviderFaultCode::ResponseEventShape)
    .response_event_diagnostic()
    .expect("the surrounding response diagnostic remains typed")
}

fn reconciliation_snapshot_fixture(branch: &str) -> serde_json::Value {
    serde_json::json!({
        "branch": branch,
        "response_created_sequence": 1,
        "response_in_progress_sequence": 2,
        "response_completed_sequence": 9,
        "response_status": "completed",
        "terminal_output_count": 1,
        "observed_lifecycle_count": 1,
        "terminal_output_index": 0,
        "observed_lifecycle_index": 0,
        "terminal_item_kind": "message",
        "observed_item_kind": "message",
        "terminal_item_status": "completed",
        "observed_lifecycle_state": "done",
        "terminal_phase": "final_answer",
        "observed_phase": "commentary",
        "terminal_id_presence": "present",
        "observed_id_presence": "present",
        "id_relation": "equal",
        "mapping_basis": "known_id",
        "terminal_content_presence": "array",
        "terminal_content_part_count": 1,
        "terminal_output_text_part_count": 1,
        "terminal_refusal_part_count": 0,
        "terminal_other_part_count": 0,
        "terminal_malformed_part_count": 0,
        "terminal_text_presence": "string",
        "terminal_text_bytes": 20,
        "observed_text_state": "completed",
        "observed_text_bytes": 20,
        "text_relation": "equal",
        "output_item_added_sequence": 3,
        "content_part_added_sequence": 4,
        "first_text_delta_sequence": 5,
        "last_text_delta_sequence": 5,
        "text_delta_count": 1,
        "text_done_sequence": 6,
        "content_part_done_sequence": 7,
        "output_item_done_sequence": 8,
    })
}

#[test]
fn consumer_matches_sanitized_fault_codes_without_parsing_text() {
    let generic = ProviderFault::retryable_transport("sanitized provider failure");
    assert_eq!(generic.code(), ProviderFaultCode::Transport);

    let classified = generic
        .clone()
        .with_code(ProviderFaultCode::ConnectSecureTransport);
    assert_eq!(classified.kind(), ProviderFaultKind::RetryableTransport);
    assert_eq!(classified.message(), generic.message());
    assert_eq!(consumer_code(classified.code()), "connect_secure_transport");

    let rejected = ProviderFault::model_rejected("sanitized model failure");
    assert_eq!(rejected.code(), ProviderFaultCode::ModelRejected);
    assert_eq!(consumer_code(rejected.code()), "model_rejected");
}

#[test]
fn public_fault_kind_and_code_axes_cover_deterministic_and_transient_policy() {
    let deterministic = [
        ProviderFaultCode::ResponseBodyLimit,
        ProviderFaultCode::StreamEventLimit,
        ProviderFaultCode::OutputLimit,
        ProviderFaultCode::ResponseEventShape,
    ];
    for code in deterministic {
        let fault =
            ProviderFault::model_rejected("sanitized deterministic failure").with_code(code);
        assert_eq!(
            (fault.kind(), fault.code()),
            (ProviderFaultKind::ModelRejected, code)
        );
        assert!(!consumer_code(code).is_empty());
    }

    let transient = [
        ProviderFaultCode::ConnectSecureTransport,
        ProviderFaultCode::RequestTransport,
        ProviderFaultCode::RequestTimeout,
        ProviderFaultCode::RateLimited,
        ProviderFaultCode::UpstreamStatus,
        ProviderFaultCode::ResponseProtocol,
        ProviderFaultCode::ResponseContentType,
        ProviderFaultCode::StreamDecode,
        ProviderFaultCode::ResponseEventJson,
        ProviderFaultCode::StreamTransport,
        ProviderFaultCode::StreamTimeout,
    ];
    for code in transient {
        let fault =
            ProviderFault::retryable_transport("sanitized transient failure").with_code(code);
        assert_eq!(
            (fault.kind(), fault.code()),
            (ProviderFaultKind::RetryableTransport, code)
        );
        assert!(!consumer_code(code).is_empty());
    }
}

#[test]
fn output_identity_diagnostic_reason_codes_are_closed_and_payload_free() {
    let reasons = [
        ProviderResponseOutputIdentityReason::DuplicateTerminalId,
        ProviderResponseOutputIdentityReason::KnownIdBeforeTerminalOrdinal,
        ProviderResponseOutputIdentityReason::NonMonotonicLifecycleMapping,
        ProviderResponseOutputIdentityReason::SparseLifecycleMapping,
        ProviderResponseOutputIdentityReason::UnreconciledObservedMessage,
        ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal,
        ProviderResponseOutputIdentityReason::SameIndexIdConflict,
    ];

    assert_eq!(reasons.len(), 7);
    for reason in reasons {
        assert_eq!(reason.code(), expected_output_identity_reason_code(reason));
    }

    let mapping_bases = [
        ProviderResponseOutputIdentityMappingBasis::KnownId,
        ProviderResponseOutputIdentityMappingBasis::OrdinalMissingId,
        ProviderResponseOutputIdentityMappingBasis::OrdinalUnknownId,
    ];
    assert_eq!(
        mapping_bases.map(|value| value.code()),
        ["known_id", "ordinal_missing_id", "ordinal_unknown_id"]
    );
    let kind_pairs = [
        ProviderResponseOutputIdentityKindPair::TerminalMessageOverReasoning,
        ProviderResponseOutputIdentityKindPair::TerminalMessageOverCompaction,
        ProviderResponseOutputIdentityKindPair::Other,
    ];
    assert_eq!(
        kind_pairs.map(|value| value.code()),
        [
            "terminal_message_over_reasoning",
            "terminal_message_over_compaction",
            "other",
        ]
    );
    let message_relations = [
        ProviderResponseOutputIdentityObservedMessageRelation::None,
        ProviderResponseOutputIdentityObservedMessageRelation::NextAfterContiguousNonText,
        ProviderResponseOutputIdentityObservedMessageRelation::SameOrdinal,
        ProviderResponseOutputIdentityObservedMessageRelation::Other,
    ];
    assert_eq!(
        message_relations.map(|value| value.code()),
        [
            "none",
            "next_after_contiguous_nontext",
            "same_ordinal",
            "other",
        ]
    );
    let text_relations = [
        ProviderResponseOutputIdentityObservedTextRelation::NotObserved,
        ProviderResponseOutputIdentityObservedTextRelation::Match,
        ProviderResponseOutputIdentityObservedTextRelation::Mismatch,
    ];
    assert_eq!(
        text_relations.map(|value| value.code()),
        ["not_observed", "match", "mismatch"]
    );
    let lifecycle_states = [
        ProviderResponseOutputIdentityLifecycleState::AddedOnly,
        ProviderResponseOutputIdentityLifecycleState::Done,
    ];
    assert_eq!(
        lifecycle_states.map(|value| value.code()),
        ["added_only", "done"]
    );

    let detail = ProviderResponseOutputIdentityDetail::new(
        ProviderResponseOutputIdentityMappingBasis::OrdinalMissingId,
        ProviderResponseOutputIdentityKindPair::TerminalMessageOverCompaction,
        ProviderResponseOutputIdentityObservedMessageRelation::NextAfterContiguousNonText,
        ProviderResponseOutputIdentityObservedTextRelation::Match,
        ProviderResponseOutputIdentityLifecycleState::Done,
    );
    assert_eq!(
        serde_json::to_value(detail).unwrap(),
        serde_json::json!({
            "mapping_basis": "ordinal_missing_id",
            "kind_pair": "terminal_message_over_compaction",
            "observed_message_relation": "next_after_contiguous_nontext",
            "observed_text_relation": "match",
            "resolved_lifecycle_state": "done",
        })
    );
}

#[test]
fn output_identity_structural_detail_round_trips_without_payload_or_provider_ids() {
    let structures = [
        serde_json::json!({
            "terminal_id_presence": "null",
            "terminal_ordinal": 0,
            "first_observed_message_ordinal": 1,
            "observed_message_delta": 1,
            "observed_message_distance": "one",
            "observed_span": [
                {"ordinal": 0, "kind": "reasoning", "id_presence": "present", "lifecycle_state": "done"},
                {"ordinal": 1, "kind": "message", "id_presence": "present", "lifecycle_state": "done"}
            ],
            "observed_span_count": 2,
            "observed_span_truncated": false,
            "observed_span_contiguous": true,
            "terminal_phase": "missing",
            "observed_phase": "missing",
            "phase_relation": "exact",
            "matched_message_lifecycle_state": "done",
            "matched_message_text_state": "completed",
            "terminal_text_bytes": 8,
            "observed_text_bytes": 8,
            "terminal_output_count": 1,
            "terminal_final_message_count": 1,
            "observed_lifecycle_count": 2,
            "observed_message_count": 1,
            "unreconciled_observed_message_count": 1
        }),
        serde_json::json!({
            "terminal_id_presence": "missing",
            "terminal_ordinal": 0,
            "first_observed_message_ordinal": 1,
            "observed_message_delta": 1,
            "observed_message_distance": "one",
            "observed_span": [
                {"ordinal": 0, "kind": "reasoning", "id_presence": "present", "lifecycle_state": "done"},
                {"ordinal": 1, "kind": "message", "id_presence": "present", "lifecycle_state": "done"}
            ],
            "observed_span_count": 2,
            "observed_span_truncated": false,
            "observed_span_contiguous": true,
            "terminal_phase": "missing",
            "observed_phase": "commentary",
            "phase_relation": "mismatch",
            "matched_message_lifecycle_state": "done",
            "matched_message_text_state": "completed",
            "terminal_text_bytes": 8,
            "observed_text_bytes": 8,
            "terminal_output_count": 1,
            "terminal_final_message_count": 1,
            "observed_lifecycle_count": 2,
            "observed_message_count": 1,
            "unreconciled_observed_message_count": 1
        }),
        serde_json::json!({
            "terminal_id_presence": "missing",
            "terminal_ordinal": 0,
            "first_observed_message_ordinal": 2,
            "observed_message_delta": 2,
            "observed_message_distance": "more_than_one",
            "observed_span": [
                {"ordinal": 0, "kind": "reasoning", "id_presence": "present", "lifecycle_state": "done"},
                {"ordinal": 1, "kind": "reasoning", "id_presence": "present", "lifecycle_state": "done"},
                {"ordinal": 2, "kind": "message", "id_presence": "present", "lifecycle_state": "done"}
            ],
            "observed_span_count": 3,
            "observed_span_truncated": false,
            "observed_span_contiguous": true,
            "terminal_phase": "missing",
            "observed_phase": "missing",
            "phase_relation": "exact",
            "matched_message_lifecycle_state": "done",
            "matched_message_text_state": "completed",
            "terminal_text_bytes": 8,
            "observed_text_bytes": 8,
            "terminal_output_count": 1,
            "terminal_final_message_count": 1,
            "observed_lifecycle_count": 3,
            "observed_message_count": 1,
            "unreconciled_observed_message_count": 1
        }),
    ];
    let mut round_tripped = Vec::new();

    for structure in structures {
        let fault = ProviderFault::model_rejected(format!(
            "sanitized identity failure [response_event_type=response.completed; \
             response_event_reason=ledger_mismatch; response_ledger_reason=output_identity; \
             response_output_identity_reason=kind_at_terminal_ordinal; \
             response_output_identity_mapping_basis=ordinal_missing_id; \
             response_output_identity_kind_pair=terminal_message_over_reasoning; \
             response_output_identity_observed_message_relation=next_after_contiguous_nontext; \
             response_output_identity_observed_text_relation=match; \
             response_output_identity_lifecycle_state=done; \
             response_output_identity_structure={structure}]"
        ))
        .with_code(ProviderFaultCode::ResponseEventShape);
        let detail = serde_json::to_value(
            fault
                .response_event_diagnostic()
                .and_then(|diagnostic| diagnostic.response_output_identity_detail())
                .expect("the complete structural detail remains typed"),
        )
        .expect("typed identity detail serializes");

        assert_eq!(detail["structure"], structure);
        for forbidden in [
            "provider-id-sentinel",
            "payload-text-sentinel",
            "Authorization",
            "cookie-sentinel",
            "reasoning-sentinel",
        ] {
            assert!(!fault.message().contains(forbidden));
        }
        round_tripped.push(detail["structure"].clone());
    }

    assert_ne!(round_tripped[0], round_tripped[1]);
    assert_ne!(round_tripped[0], round_tripped[2]);
    assert_ne!(round_tripped[1], round_tripped[2]);
}

fn expected_output_identity_reason_code(
    reason: ProviderResponseOutputIdentityReason,
) -> &'static str {
    match reason {
        ProviderResponseOutputIdentityReason::DuplicateTerminalId => "duplicate_terminal_id",
        ProviderResponseOutputIdentityReason::KnownIdBeforeTerminalOrdinal => {
            "known_id_before_terminal_ordinal"
        }
        ProviderResponseOutputIdentityReason::NonMonotonicLifecycleMapping => {
            "non_monotonic_lifecycle_mapping"
        }
        ProviderResponseOutputIdentityReason::SparseLifecycleMapping => "sparse_lifecycle_mapping",
        ProviderResponseOutputIdentityReason::UnreconciledObservedMessage => {
            "unreconciled_observed_message"
        }
        ProviderResponseOutputIdentityReason::KindAtTerminalOrdinal => "kind_at_terminal_ordinal",
        ProviderResponseOutputIdentityReason::SameIndexIdConflict => "same_index_id_conflict",
    }
}
