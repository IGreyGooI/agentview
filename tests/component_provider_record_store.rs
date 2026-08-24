use agentview::{
    pom::Document,
    pom_resolution::resolve_artifact_document,
    record_store::{
        reconstruct_transcript, validate_record_append, AppendOutcome, AttemptRecordState,
        LogRevision, MemoryRecordStore, NewRecord, ProviderArtifactAttachState,
        ProviderArtifactMode, ProviderCompactionState, ProviderOperationState, RecordBatch,
        RecordEnvelope, RecordLog, RecordLogError, SessionId, SessionRecord,
        TranscriptReconstructionError, UserDiffCursor,
    },
    transcript::{
        CanonicalInputItem, CanonicalTranscript, ConversationRole, InstructionAuthority,
        TranscriptRevision,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};

fn user_item(text: &str) -> CanonicalInputItem {
    let pom = resolve_artifact_document(
        Document::try_build(|blocks| {
            blocks.try_paragraph(|inline| {
                inline.try_text(text)?;
                Ok(())
            })?;
            Ok(())
        })
        .unwrap(),
    )
    .unwrap();
    CanonicalInputItem::message(ConversationRole::User, pom)
}

fn canonical_record(revision: u64, text: &str) -> NewRecord {
    NewRecord::new(
        1,
        SessionRecord::CanonicalItemAppended {
            transcript_revision: TranscriptRevision::new(revision),
            item: user_item(text),
        },
    )
    .unwrap()
}

fn provider_record(record: SessionRecord) -> NewRecord {
    NewRecord::new(1, record).unwrap()
}

fn request_sha256(request_body: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(request_body))
}

fn compaction_request_body(
    provider_epoch_id: &str,
    from_window_revision: u64,
    transcript_revision: u64,
) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "provider_epoch_id": provider_epoch_id,
        "transcript_revision": transcript_revision,
        "window_revision": from_window_revision,
    }))
    .unwrap()
}

fn compaction_prepared(
    compaction_id: &str,
    provider_epoch_id: &str,
    from_window_revision: u64,
    transcript_revision: u64,
) -> NewRecord {
    let request_body =
        compaction_request_body(provider_epoch_id, from_window_revision, transcript_revision);
    provider_record(SessionRecord::ProviderCompactionPrepared {
        compaction_id: compaction_id.to_owned(),
        provider_epoch_id: provider_epoch_id.to_owned(),
        from_window_revision,
        transcript_revision: TranscriptRevision::new(transcript_revision),
        encoder_version: "full-history-v1".to_owned(),
        request_sha256: request_sha256(&request_body),
        request_body,
    })
}

fn compaction_advanced(
    compaction_id: &str,
    compaction_revision: u64,
    state: ProviderCompactionState,
) -> NewRecord {
    provider_record(SessionRecord::ProviderCompactionAdvanced {
        compaction_id: compaction_id.to_owned(),
        compaction_revision,
        state,
    })
}

fn compaction_checkpointed(
    compaction_id: &str,
    provider_epoch_id: &str,
    from_window_revision: u64,
    window_revision: u64,
    transcript_revision: u64,
    opaque_state: Option<serde_json::Value>,
) -> NewRecord {
    compaction_checkpointed_with_policy(
        compaction_id,
        provider_epoch_id,
        from_window_revision,
        window_revision,
        transcript_revision,
        "full-history-v1",
        opaque_state,
    )
}

fn compaction_checkpointed_with_policy(
    compaction_id: &str,
    provider_epoch_id: &str,
    from_window_revision: u64,
    window_revision: u64,
    transcript_revision: u64,
    history_policy_version: &str,
    opaque_state: Option<serde_json::Value>,
) -> NewRecord {
    provider_record(SessionRecord::ProviderCompactionCheckpointed {
        compaction_id: compaction_id.to_owned(),
        provider_epoch_id: provider_epoch_id.to_owned(),
        from_window_revision,
        window_revision,
        transcript_revision: TranscriptRevision::new(transcript_revision),
        history_policy_version: history_policy_version.to_owned(),
        opaque_state,
    })
}

fn compaction_base() -> Vec<NewRecord> {
    vec![
        epoch_opened("epoch-1", 0),
        window_checkpoint("epoch-1", 1, 0),
    ]
}

fn system_document_with_text(text: &str) -> agentview::pom::ResolvedDocument {
    resolve_artifact_document(
        Document::try_build(|blocks| blocks.try_paragraph(|inline| inline.try_text(text))).unwrap(),
    )
    .unwrap()
}

fn system_document() -> agentview::pom::ResolvedDocument {
    system_document_with_text("durable system artifact")
}

fn content_fingerprint(schema: &str, content: &[u8]) -> String {
    format!("{schema}:sha256:{:x}", Sha256::digest(content))
}

fn artifact_context() -> (agentview::pom::ResolvedDocument, Vec<u8>, String, String) {
    let artifact = system_document();
    let contract_manifest =
        br#"{"schema":"agentview.contract-manifest/v1","listeners":[]}"#.to_vec();
    let artifact_fingerprint = content_fingerprint(
        "agentview.system-artifact/v1",
        &serde_json::to_vec(&artifact).unwrap(),
    );
    let contract_fingerprint =
        content_fingerprint("agentview.contract-manifest/v1", &contract_manifest);
    (
        artifact,
        contract_manifest,
        artifact_fingerprint,
        contract_fingerprint,
    )
}

fn artifact_committed(epoch_id: &str) -> NewRecord {
    let (artifact, contract_manifest, artifact_fingerprint, contract_fingerprint) =
        artifact_context();
    provider_record(SessionRecord::EpochArtifactCommitted {
        epoch_id: epoch_id.to_owned(),
        artifact_fingerprint,
        contract_fingerprint,
        artifact,
        contract_manifest,
    })
}

#[test]
fn epoch_artifact_record_rejects_well_formed_tampered_content_fingerprints() {
    let (artifact, contract_manifest, artifact_fingerprint, contract_fingerprint) =
        artifact_context();
    let wrong_artifact =
        "agentview.system-artifact/v1:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let artifact_error = NewRecord::new(
        1,
        SessionRecord::EpochArtifactCommitted {
            epoch_id: "epoch-tampered-artifact".to_owned(),
            artifact_fingerprint: wrong_artifact.to_owned(),
            contract_fingerprint: contract_fingerprint.clone(),
            artifact: artifact.clone(),
            contract_manifest: contract_manifest.clone(),
        },
    )
    .unwrap_err();
    assert!(matches!(
        artifact_error,
        RecordLogError::RequestDigestMismatch { declared, .. } if declared == wrong_artifact
    ));

    let wrong_contract =
        "agentview.contract-manifest/v1:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let contract_error = NewRecord::new(
        1,
        SessionRecord::EpochArtifactCommitted {
            epoch_id: "epoch-tampered-contract".to_owned(),
            artifact_fingerprint,
            contract_fingerprint: wrong_contract.to_owned(),
            artifact,
            contract_manifest,
        },
    )
    .unwrap_err();
    assert!(matches!(
        contract_error,
        RecordLogError::RequestDigestMismatch { declared, .. } if declared == wrong_contract
    ));
}

fn artifact_attached(epoch_id: &str, attachment_id: &str) -> NewRecord {
    let (_, _, artifact_fingerprint, contract_fingerprint) = artifact_context();
    provider_record(SessionRecord::EpochArtifactLogicallyAttached {
        epoch_id: epoch_id.to_owned(),
        attachment_id: attachment_id.to_owned(),
        artifact_fingerprint,
        contract_fingerprint,
    })
}

fn canonical_system_record(revision: u64) -> NewRecord {
    provider_record(SessionRecord::CanonicalItemAppended {
        transcript_revision: TranscriptRevision::new(revision),
        item: CanonicalInputItem::instruction(InstructionAuthority::System, system_document()),
    })
}

fn artifact_admission(epoch_id: &str, transcript_revision: u64) -> Vec<NewRecord> {
    vec![
        artifact_committed(epoch_id),
        artifact_attached(epoch_id, &format!("attachment-{epoch_id}")),
        canonical_system_record(transcript_revision),
    ]
}

fn provider_artifact_skipped(epoch_id: &str, operation_id: &str, attempt_id: &str) -> NewRecord {
    let (_, _, artifact_fingerprint, contract_fingerprint) = artifact_context();
    provider_record(SessionRecord::ProviderArtifactAttachAdvanced {
        provider_epoch_id: epoch_id.to_owned(),
        attach_operation_id: operation_id.to_owned(),
        attach_revision: 1,
        artifact_fingerprint,
        contract_fingerprint,
        encoder_version: "codex-http-v1".to_owned(),
        request_sha256: None,
        request_body: None,
        skip_attempt_id: Some(attempt_id.to_owned()),
        state: ProviderArtifactAttachState::Skipped,
    })
}

fn provider_artifact_state(
    epoch_id: &str,
    operation_id: &str,
    attach_revision: u64,
    state: ProviderArtifactAttachState,
) -> NewRecord {
    let (_, _, artifact_fingerprint, contract_fingerprint) = artifact_context();
    provider_record(SessionRecord::ProviderArtifactAttachAdvanced {
        provider_epoch_id: epoch_id.to_owned(),
        attach_operation_id: operation_id.to_owned(),
        attach_revision,
        artifact_fingerprint,
        contract_fingerprint,
        encoder_version: "codex-http-v1".to_owned(),
        request_sha256: None,
        request_body: None,
        skip_attempt_id: None,
        state,
    })
}

fn epoch_opened(provider_epoch_id: &str, base_transcript_revision: u64) -> NewRecord {
    provider_record(SessionRecord::ProviderEpochOpened {
        provider_epoch_id: provider_epoch_id.to_owned(),
        provider: "openai".to_owned(),
        profile: "codex-http-v1".to_owned(),
        profile_version: 1,
        provider_binding: "test-openai-account".to_owned(),
        artifact_mode: ProviderArtifactMode::Stateless,
        base_transcript_revision: TranscriptRevision::new(base_transcript_revision),
    })
}

fn epoch_opened_with_mode(
    provider_epoch_id: &str,
    base_transcript_revision: u64,
    artifact_mode: ProviderArtifactMode,
) -> NewRecord {
    provider_record(SessionRecord::ProviderEpochOpened {
        provider_epoch_id: provider_epoch_id.to_owned(),
        provider: "openai".to_owned(),
        profile: "codex-http-v1".to_owned(),
        profile_version: 1,
        provider_binding: "test-openai-account".to_owned(),
        artifact_mode,
        base_transcript_revision: TranscriptRevision::new(base_transcript_revision),
    })
}

fn remote_provider_artifact_state(
    epoch_id: &str,
    operation_id: &str,
    attach_revision: u64,
    state: ProviderArtifactAttachState,
) -> NewRecord {
    let (_, _, artifact_fingerprint, contract_fingerprint) = artifact_context();
    let request_body = b"frozen provider artifact request".to_vec();
    let is_prepared = matches!(state, ProviderArtifactAttachState::Prepared);
    provider_record(SessionRecord::ProviderArtifactAttachAdvanced {
        provider_epoch_id: epoch_id.to_owned(),
        attach_operation_id: operation_id.to_owned(),
        attach_revision,
        artifact_fingerprint,
        contract_fingerprint,
        encoder_version: "codex-http-v1".to_owned(),
        request_sha256: is_prepared.then(|| format!("sha256:{:x}", Sha256::digest(&request_body))),
        request_body: is_prepared.then_some(request_body),
        skip_attempt_id: None,
        state,
    })
}

fn epoch_closed(provider_epoch_id: &str) -> NewRecord {
    provider_record(SessionRecord::ProviderEpochClosed {
        provider_epoch_id: provider_epoch_id.to_owned(),
    })
}

fn window_checkpoint(
    provider_epoch_id: &str,
    window_revision: u64,
    transcript_revision: u64,
) -> NewRecord {
    provider_record(SessionRecord::ProviderWindowCheckpointed {
        provider_epoch_id: provider_epoch_id.to_owned(),
        window_revision,
        transcript_revision: TranscriptRevision::new(transcript_revision),
        history_policy_version: "full-history-v1".to_owned(),
        opaque_state: None,
    })
}

fn operation_prepared(
    operation_id: &str,
    provider_epoch_id: &str,
    window_revision: u64,
    transcript_revision: u64,
) -> NewRecord {
    operation_prepared_for_attempt(
        operation_id,
        &operation_attempt_id(operation_id),
        provider_epoch_id,
        window_revision,
        transcript_revision,
    )
}

fn operation_prepared_for_attempt(
    operation_id: &str,
    attempt_id: &str,
    provider_epoch_id: &str,
    window_revision: u64,
    transcript_revision: u64,
) -> NewRecord {
    let request_body = format!(r#"{{"operation_id":"{operation_id}"}}"#).into_bytes();
    provider_record(SessionRecord::ProviderOperationPrepared {
        operation_id: operation_id.to_owned(),
        attempt_id: attempt_id.to_owned(),
        provider_epoch_id: provider_epoch_id.to_owned(),
        window_revision,
        transcript_revision: TranscriptRevision::new(transcript_revision),
        user_diff_cursor: UserDiffCursor::ZERO,
        encoder_version: "codex-http-v1".to_owned(),
        request_sha256: request_sha256(&request_body),
        request_body,
    })
}

fn operation_attempt_id(operation_id: &str) -> String {
    format!("attempt-for-{operation_id}")
}

fn operation_attempt_started(operation_id: &str) -> NewRecord {
    attempt_started(
        &operation_attempt_id(operation_id),
        &format!("snapshot-for-{operation_id}"),
    )
}

fn operation_attempt_prepared(operation_id: &str) -> NewRecord {
    attempt_prepared(
        &operation_attempt_id(operation_id),
        &format!("snapshot-for-{operation_id}"),
    )
}

fn operation_attempt_issued(operation_id: &str) -> NewRecord {
    attempt_issued(
        &operation_attempt_id(operation_id),
        &format!("snapshot-for-{operation_id}"),
    )
}

fn operation_advanced(
    operation_id: &str,
    operation_revision: u64,
    state: ProviderOperationState,
) -> NewRecord {
    provider_record(SessionRecord::ProviderOperationAdvanced {
        operation_id: operation_id.to_owned(),
        operation_revision,
        state,
    })
}

fn attempt_advanced(
    attempt_id: &str,
    snapshot_id: &str,
    attempt_revision: u64,
    state: AttemptRecordState,
) -> NewRecord {
    provider_record(SessionRecord::AttemptAdvanced {
        attempt_id: attempt_id.to_owned(),
        snapshot_id: snapshot_id.to_owned(),
        attempt_revision,
        state,
    })
}

fn attempt_started(attempt_id: &str, snapshot_id: &str) -> NewRecord {
    attempt_advanced(attempt_id, snapshot_id, 1, AttemptRecordState::Started)
}

fn attempt_prepared(attempt_id: &str, snapshot_id: &str) -> NewRecord {
    attempt_advanced(attempt_id, snapshot_id, 2, AttemptRecordState::Prepared)
}

fn attempt_issued(attempt_id: &str, snapshot_id: &str) -> NewRecord {
    attempt_advanced(attempt_id, snapshot_id, 3, AttemptRecordState::Issued)
}

fn attempt_finished(attempt_id: &str, snapshot_id: &str) -> NewRecord {
    attempt_advanced(attempt_id, snapshot_id, 4, AttemptRecordState::Finished)
}

fn attempt_published(attempt_id: &str, snapshot_id: &str) -> NewRecord {
    attempt_advanced(attempt_id, snapshot_id, 5, AttemptRecordState::Published)
}

fn completed_attempt_prefix(
    attempt_id: &str,
    snapshot_id: &str,
    operation_id: &str,
    provider_epoch_id: &str,
    transcript_revision: u64,
) -> Vec<NewRecord> {
    vec![
        attempt_started(attempt_id, snapshot_id),
        attempt_prepared(attempt_id, snapshot_id),
        operation_prepared_for_attempt(
            operation_id,
            attempt_id,
            provider_epoch_id,
            1,
            transcript_revision,
        ),
        attempt_issued(attempt_id, snapshot_id),
        operation_advanced(operation_id, 1, ProviderOperationState::Attempted),
        operation_advanced(operation_id, 2, ProviderOperationState::Completed),
    ]
}

fn attempt_aborted(
    attempt_id: &str,
    snapshot_id: &str,
    attempt_revision: u64,
    reason: &str,
) -> NewRecord {
    attempt_advanced(
        attempt_id,
        snapshot_id,
        attempt_revision,
        AttemptRecordState::Aborted {
            reason: reason.to_owned(),
        },
    )
}

fn cursor_advanced(publication_id: &str, previous: u64, next: u64) -> NewRecord {
    provider_record(SessionRecord::UserDiffCursorAdvanced {
        publication_id: publication_id.to_owned(),
        previous_cursor: UserDiffCursor::new(previous),
        next_cursor: UserDiffCursor::new(next),
    })
}

fn publication_committed(
    publication_id: &str,
    attempt_id: &str,
    base_transcript_revision: u64,
    next_transcript_revision: u64,
    next_cursor: u64,
) -> NewRecord {
    provider_record(SessionRecord::TurnPublicationCommitted {
        publication_id: publication_id.to_owned(),
        attempt_id: attempt_id.to_owned(),
        base_transcript_revision: TranscriptRevision::new(base_transcript_revision),
        next_transcript_revision: TranscriptRevision::new(next_transcript_revision),
        next_cursor: UserDiffCursor::new(next_cursor),
    })
}

fn publication_prepared(
    publication_id: &str,
    attempt_id: &str,
    snapshot_id: &str,
    base_transcript_revision: u64,
    previous_cursor: u64,
    items: &[&str],
) -> NewRecord {
    provider_record(SessionRecord::TurnPublicationPrepared {
        publication_id: publication_id.to_owned(),
        attempt_id: attempt_id.to_owned(),
        snapshot_id: snapshot_id.to_owned(),
        base_transcript_revision: TranscriptRevision::new(base_transcript_revision),
        previous_cursor: UserDiffCursor::new(previous_cursor),
        next_cursor: UserDiffCursor::new(previous_cursor + 1),
        items: items.iter().map(|text| user_item(text)).collect(),
    })
}

fn active_attempt_prefix(records: impl IntoIterator<Item = NewRecord>) -> Vec<NewRecord> {
    [
        epoch_opened("epoch-attempt-state", 0),
        window_checkpoint("epoch-attempt-state", 1, 0),
    ]
    .into_iter()
    .chain(records)
    .collect()
}

async fn append_fixture_records(
    store: &MemoryRecordStore,
    session: &SessionId,
    initial_tail: LogRevision,
    records: impl IntoIterator<Item = NewRecord>,
) -> AppendOutcome {
    let mut tail = initial_tail;
    let mut pending = Vec::new();

    for record in records {
        let ends_append = matches!(
            record.record(),
            SessionRecord::AttemptAdvanced {
                state: AttemptRecordState::Started,
                ..
            }
        );
        pending.push(record);
        if ends_append {
            tail = store
                .append(
                    session,
                    tail,
                    RecordBatch::new(std::mem::take(&mut pending)).unwrap(),
                )
                .await
                .unwrap()
                .tail();
        }
    }

    if !pending.is_empty() {
        tail = store
            .append(session, tail, RecordBatch::new(pending).unwrap())
            .await
            .unwrap()
            .tail();
    }

    AppendOutcome::Committed { tail }
}

async fn rejected_atomically(prefix: Vec<NewRecord>, rejected: Vec<NewRecord>) -> RecordLogError {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-invalid-sequence").unwrap();
    let tail = if prefix.is_empty() {
        LogRevision::ZERO
    } else {
        append_fixture_records(&store, &session, LogRevision::ZERO, prefix)
            .await
            .tail()
    };
    let before = store.read(&session, LogRevision::ZERO).await.unwrap();

    let error = store
        .append(&session, tail, RecordBatch::new(rejected).unwrap())
        .await
        .expect_err("the inconsistent batch must be rejected");
    assert!(matches!(error, RecordLogError::InvalidSequence(_)));
    assert_eq!(
        store.read(&session, LogRevision::ZERO).await.unwrap(),
        before
    );
    error
}

async fn assert_rejected_atomically(prefix: Vec<NewRecord>, rejected: Vec<NewRecord>) {
    assert!(matches!(
        rejected_atomically(prefix, rejected).await,
        RecordLogError::InvalidSequence(_)
    ));
}

async fn assert_rejected_atomically_with_reason(
    prefix: Vec<NewRecord>,
    rejected: Vec<NewRecord>,
    expected_reason: &'static str,
) {
    assert_eq!(
        rejected_atomically(prefix, rejected).await,
        RecordLogError::InvalidSequence(expected_reason)
    );
}

#[tokio::test]
async fn memory_record_log_assigns_contiguous_session_local_revisions() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-7").unwrap();
    let batch =
        RecordBatch::new([canonical_record(1, "first"), canonical_record(2, "second")]).unwrap();

    let outcome = store
        .append(&session, RecordEnvelope::INITIAL_TAIL, batch)
        .await
        .unwrap();
    assert_eq!(outcome, AppendOutcome::committed(2));

    let all = store
        .read(&session, RecordEnvelope::INITIAL_TAIL)
        .await
        .unwrap();
    assert_eq!(
        all.iter()
            .map(|record| record.revision().get())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(all.iter().all(|record| record.session_id() == &session));

    let after_first = store.read(&session, all[0].revision()).await.unwrap();
    assert_eq!(after_first, all[1..]);
}

#[tokio::test]
async fn stale_tail_rejects_the_complete_batch_without_advancing_authority() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-cas").unwrap();
    let first = RecordBatch::new([canonical_record(1, "first")]).unwrap();
    assert_eq!(
        store
            .append(&session, RecordEnvelope::INITIAL_TAIL, first)
            .await
            .unwrap(),
        AppendOutcome::committed(1)
    );

    let stale = RecordBatch::new([
        canonical_record(2, "must-not-appear"),
        canonical_record(3, "also-must-not-appear"),
    ])
    .unwrap();
    assert_eq!(
        store
            .append(&session, RecordEnvelope::INITIAL_TAIL, stale)
            .await
            .unwrap(),
        AppendOutcome::conflict(1)
    );

    let records = store
        .read(&session, RecordEnvelope::INITIAL_TAIL)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision().get(), 1);
}

#[tokio::test]
async fn backend_failure_does_not_advance_tail_or_partially_append_a_batch() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-fault").unwrap();
    store.fail_next_append().unwrap();

    let error = store
        .append(
            &session,
            RecordEnvelope::INITIAL_TAIL,
            RecordBatch::new([canonical_record(1, "first"), canonical_record(2, "second")])
                .unwrap(),
        )
        .await
        .expect_err("the conformance fault must reject the whole append");
    assert_eq!(error, RecordLogError::InjectedFailure);
    assert!(store
        .read(&session, RecordEnvelope::INITIAL_TAIL)
        .await
        .unwrap()
        .is_empty());

    let retry = RecordBatch::new([canonical_record(1, "retry")]).unwrap();
    assert_eq!(
        store
            .append(&session, RecordEnvelope::INITIAL_TAIL, retry)
            .await
            .unwrap(),
        AppendOutcome::committed(1)
    );
}

#[tokio::test]
async fn persisted_records_reconstruct_the_same_canonical_transcript() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-reconstruct").unwrap();
    let batch =
        RecordBatch::new([canonical_record(1, "first"), canonical_record(2, "second")]).unwrap();
    store
        .append(&session, RecordEnvelope::INITIAL_TAIL, batch)
        .await
        .unwrap();

    let records = store
        .read(&session, RecordEnvelope::INITIAL_TAIL)
        .await
        .unwrap();
    let restored = reconstruct_transcript(&records).unwrap();
    let expected = CanonicalTranscript::new()
        .appended(user_item("first"))
        .unwrap()
        .appended(user_item("second"))
        .unwrap();
    assert_eq!(restored, expected);
    assert_eq!(restored.revision().get(), 2);
}

#[test]
fn empty_batches_and_zero_schema_versions_fail_before_backend_io() {
    assert!(matches!(
        RecordBatch::new(Vec::<NewRecord>::new()),
        Err(RecordLogError::EmptyBatch)
    ));
    assert!(matches!(
        NewRecord::new(
            0,
            SessionRecord::CanonicalItemAppended {
                transcript_revision: TranscriptRevision::new(1),
                item: user_item("invalid"),
            }
        ),
        Err(RecordLogError::ZeroSchemaVersion)
    ));
}

#[test]
fn record_log_value_types_round_trip_through_json() {
    let session = SessionId::new("session-json").unwrap();
    let new_record = canonical_record(1, "round trip");
    let envelope =
        RecordEnvelope::new(session.clone(), LogRevision::new(1), new_record.clone()).unwrap();

    let restored_session: SessionId =
        serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
    let restored_record: NewRecord =
        serde_json::from_value(serde_json::to_value(&new_record).unwrap()).unwrap();
    let restored_envelope: RecordEnvelope =
        serde_json::from_value(serde_json::to_value(&envelope).unwrap()).unwrap();

    assert_eq!(restored_session, session);
    assert_eq!(restored_record, new_record);
    assert_eq!(restored_envelope, envelope);
    assert_eq!(session.as_str(), "session-json");
    assert_eq!(new_record.schema_version(), 1);
    assert_eq!(new_record.record(), envelope.record());
    assert_eq!(envelope.schema_version(), 1);
    assert_eq!(AppendOutcome::committed(7).tail(), LogRevision::new(7));
    assert_eq!(AppendOutcome::conflict(9).tail(), LogRevision::new(9));
}

#[test]
fn every_provider_record_variant_validates_and_round_trips() {
    let request_body = br#"{"model":"gpt-5.6-codex"}"#.to_vec();
    let digest = request_sha256(&request_body);
    let compaction_body = compaction_request_body("epoch-1", 1, 0);
    let compaction_digest = request_sha256(&compaction_body);
    let compacted = Some(json!({"summary": "opaque"}));
    let records = [
        SessionRecord::ProviderEpochOpened {
            provider_epoch_id: "epoch-1".to_owned(),
            provider: "openai".to_owned(),
            profile: "codex-http-v1".to_owned(),
            profile_version: 1,
            provider_binding: "test-openai-account".to_owned(),
            artifact_mode: ProviderArtifactMode::Stateless,
            base_transcript_revision: TranscriptRevision::new(0),
        },
        SessionRecord::ProviderEpochClosed {
            provider_epoch_id: "epoch-1".to_owned(),
        },
        SessionRecord::ProviderWindowCheckpointed {
            provider_epoch_id: "epoch-1".to_owned(),
            window_revision: 1,
            transcript_revision: TranscriptRevision::new(0),
            history_policy_version: "full-history-v1".to_owned(),
            opaque_state: Some(json!({"cursor": "opaque"})),
        },
        SessionRecord::ProviderCompactionPrepared {
            compaction_id: "compaction-1".to_owned(),
            provider_epoch_id: "epoch-1".to_owned(),
            from_window_revision: 1,
            transcript_revision: TranscriptRevision::ZERO,
            encoder_version: "full-history-v1".to_owned(),
            request_sha256: compaction_digest,
            request_body: compaction_body,
        },
        SessionRecord::ProviderCompactionAdvanced {
            compaction_id: "compaction-1".to_owned(),
            compaction_revision: 1,
            state: ProviderCompactionState::Attempted,
        },
        SessionRecord::ProviderCompactionAdvanced {
            compaction_id: "compaction-1".to_owned(),
            compaction_revision: 2,
            state: ProviderCompactionState::Completed {
                opaque_state: compacted.clone(),
            },
        },
        SessionRecord::ProviderCompactionCheckpointed {
            compaction_id: "compaction-1".to_owned(),
            provider_epoch_id: "epoch-1".to_owned(),
            from_window_revision: 1,
            window_revision: 2,
            transcript_revision: TranscriptRevision::ZERO,
            history_policy_version: "full-history-v1".to_owned(),
            opaque_state: compacted,
        },
        SessionRecord::ProviderOperationPrepared {
            operation_id: "operation-1".to_owned(),
            attempt_id: "attempt-1".to_owned(),
            provider_epoch_id: "epoch-1".to_owned(),
            window_revision: 1,
            transcript_revision: TranscriptRevision::new(0),
            user_diff_cursor: UserDiffCursor::ZERO,
            encoder_version: "codex-http-v1".to_owned(),
            request_sha256: digest,
            request_body,
        },
        SessionRecord::ProviderOperationAdvanced {
            operation_id: "operation-1".to_owned(),
            operation_revision: 1,
            state: ProviderOperationState::Attempted,
        },
    ];

    for record in records {
        let persisted = NewRecord::new(1, record).unwrap();
        let restored: NewRecord =
            serde_json::from_value(serde_json::to_value(&persisted).unwrap()).unwrap();
        assert_eq!(restored, persisted);
    }
}

#[test]
fn every_execution_record_variant_validates_and_round_trips() {
    let (artifact, contract_manifest, artifact_fingerprint, contract_fingerprint) =
        artifact_context();
    let records = [
        SessionRecord::EpochArtifactCommitted {
            epoch_id: "epoch-1".to_owned(),
            artifact_fingerprint: artifact_fingerprint.clone(),
            contract_fingerprint: contract_fingerprint.clone(),
            artifact,
            contract_manifest,
        },
        SessionRecord::EpochArtifactLogicallyAttached {
            epoch_id: "epoch-1".to_owned(),
            attachment_id: "attachment-1".to_owned(),
            artifact_fingerprint: artifact_fingerprint.clone(),
            contract_fingerprint: contract_fingerprint.clone(),
        },
        SessionRecord::ProviderArtifactAttachAdvanced {
            provider_epoch_id: "epoch-1".to_owned(),
            attach_operation_id: "provider-attachment-1".to_owned(),
            attach_revision: 1,
            artifact_fingerprint,
            contract_fingerprint,
            encoder_version: "codex-http-v1".to_owned(),
            request_sha256: None,
            request_body: None,
            skip_attempt_id: Some("attempt-provider-attachment-1".to_owned()),
            state: ProviderArtifactAttachState::Skipped,
        },
        SessionRecord::AttemptAdvanced {
            attempt_id: "attempt-started".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            attempt_revision: 1,
            state: AttemptRecordState::Started,
        },
        SessionRecord::AttemptAdvanced {
            attempt_id: "attempt-prepared".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            attempt_revision: 2,
            state: AttemptRecordState::Prepared,
        },
        SessionRecord::AttemptAdvanced {
            attempt_id: "attempt-issued".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            attempt_revision: 3,
            state: AttemptRecordState::Issued,
        },
        SessionRecord::AttemptAdvanced {
            attempt_id: "attempt-finished".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            attempt_revision: 4,
            state: AttemptRecordState::Finished,
        },
        SessionRecord::AttemptAdvanced {
            attempt_id: "attempt-published".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            attempt_revision: 5,
            state: AttemptRecordState::Published,
        },
        SessionRecord::AttemptAdvanced {
            attempt_id: "attempt-aborted".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            attempt_revision: 2,
            state: AttemptRecordState::Aborted {
                reason: "preparation".to_owned(),
            },
        },
        SessionRecord::TurnPublicationPrepared {
            publication_id: "publication-1".to_owned(),
            attempt_id: "attempt-published".to_owned(),
            snapshot_id: "snapshot-1".to_owned(),
            base_transcript_revision: TranscriptRevision::ZERO,
            previous_cursor: UserDiffCursor::ZERO,
            next_cursor: UserDiffCursor::new(1),
            items: vec![user_item("published")],
        },
        SessionRecord::TurnPublicationCommitted {
            publication_id: "publication-1".to_owned(),
            attempt_id: "attempt-published".to_owned(),
            base_transcript_revision: TranscriptRevision::ZERO,
            next_transcript_revision: TranscriptRevision::new(1),
            next_cursor: UserDiffCursor::new(1),
        },
        SessionRecord::UserDiffCursorAdvanced {
            publication_id: "publication-1".to_owned(),
            previous_cursor: UserDiffCursor::ZERO,
            next_cursor: UserDiffCursor::new(1),
        },
    ];

    for record in records {
        let persisted = NewRecord::new(1, record).unwrap();
        let restored: NewRecord =
            serde_json::from_value(serde_json::to_value(&persisted).unwrap()).unwrap();
        assert_eq!(restored, persisted);
    }
}

#[tokio::test]
async fn execution_records_accept_complete_publication_and_early_abort_paths() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-execution-flow").unwrap();
    let mut complete = vec![
        epoch_opened("epoch-publication-flow", 0),
        window_checkpoint("epoch-publication-flow", 1, 0),
    ];
    complete.extend(completed_attempt_prefix(
        "attempt-1",
        "snapshot-1",
        "operation-publication-flow",
        "epoch-publication-flow",
        0,
    ));
    complete.extend([
        publication_prepared(
            "publication-1",
            "attempt-1",
            "snapshot-1",
            0,
            0,
            &["published turn"],
        ),
        attempt_finished("attempt-1", "snapshot-1"),
        canonical_record(1, "published turn"),
        cursor_advanced("publication-1", 0, 1),
        publication_committed("publication-1", "attempt-1", 0, 1, 1),
        attempt_published("attempt-1", "snapshot-1"),
        attempt_started("attempt-2", "snapshot-2"),
        attempt_aborted("attempt-2", "snapshot-2", 2, "preparation"),
    ]);

    assert_eq!(
        append_fixture_records(&store, &session, LogRevision::ZERO, complete).await,
        AppendOutcome::committed(16)
    );
}

#[tokio::test]
async fn attempt_snapshot_identity_cannot_drift_within_one_attempt() {
    assert_rejected_atomically(
        active_attempt_prefix([attempt_started("attempt-1", "snapshot-original")]),
        vec![attempt_prepared("attempt-1", "snapshot-replaced")],
    )
    .await;
}

#[tokio::test]
async fn started_reservation_must_end_its_append_before_continuation() {
    for (case, continuation) in [
        ("prepared", AttemptRecordState::Prepared),
        (
            "aborted",
            AttemptRecordState::Aborted {
                reason: "preparation".to_owned(),
            },
        ),
    ] {
        let store = MemoryRecordStore::new();
        let session = SessionId::new(format!("session-started-boundary-{case}")).unwrap();
        let attempt_id = format!("attempt-started-boundary-{case}");
        let snapshot_id = format!("snapshot-started-boundary-{case}");

        let error = store
            .append(
                &session,
                LogRevision::ZERO,
                RecordBatch::new([
                    epoch_opened(&format!("epoch-started-boundary-{case}"), 0),
                    attempt_started(&attempt_id, &snapshot_id),
                    attempt_advanced(&attempt_id, &snapshot_id, 2, continuation.clone()),
                ])
                .unwrap(),
            )
            .await
            .expect_err("Started and its continuation must not share one append");
        assert_eq!(
            error,
            RecordLogError::InvalidSequence("Started reservation must end append")
        );
        assert!(store
            .read(&session, LogRevision::ZERO)
            .await
            .unwrap()
            .is_empty());

        assert_eq!(
            store
                .append(
                    &session,
                    LogRevision::ZERO,
                    RecordBatch::new([
                        epoch_opened(&format!("epoch-started-boundary-{case}"), 0),
                        attempt_started(&attempt_id, &snapshot_id),
                    ])
                    .unwrap(),
                )
                .await
                .unwrap(),
            AppendOutcome::committed(2)
        );
        assert_eq!(
            store
                .append(
                    &session,
                    LogRevision::new(2),
                    RecordBatch::new([attempt_advanced(
                        &attempt_id,
                        &snapshot_id,
                        2,
                        continuation,
                    )])
                    .unwrap(),
                )
                .await
                .unwrap(),
            AppendOutcome::committed(3)
        );
    }
}

#[tokio::test]
async fn attempt_requires_one_epoch_and_blocks_rollover_until_terminal() {
    assert_rejected_atomically_with_reason(
        Vec::new(),
        vec![attempt_started(
            "attempt-without-epoch",
            "snapshot-without-epoch",
        )],
        "attempt requires an active epoch",
    )
    .await;

    let epoch_id = "epoch-attempt-binding";
    let attempt_id = "attempt-bound-to-epoch";
    let snapshot_id = "snapshot-bound-to-epoch";
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-attempt-epoch-binding").unwrap();
    let started = append_fixture_records(
        &store,
        &session,
        LogRevision::ZERO,
        [
            epoch_opened(epoch_id, 0),
            window_checkpoint(epoch_id, 1, 0),
            attempt_started(attempt_id, snapshot_id),
        ],
    )
    .await;
    let prepared = store
        .append(
            &session,
            started.tail(),
            RecordBatch::new([attempt_prepared(attempt_id, snapshot_id)]).unwrap(),
        )
        .await
        .unwrap();

    let error = store
        .append(
            &session,
            prepared.tail(),
            RecordBatch::new([provider_record(SessionRecord::ProviderEpochClosed {
                provider_epoch_id: epoch_id.to_owned(),
            })])
            .unwrap(),
        )
        .await
        .expect_err("an active attempt must prevent Epoch rollover");
    assert_eq!(
        error,
        RecordLogError::InvalidSequence("active epoch has an unsettled attempt")
    );

    let aborted = store
        .append(
            &session,
            prepared.tail(),
            RecordBatch::new([attempt_aborted(
                attempt_id,
                snapshot_id,
                3,
                "terminal_before_rollover",
            )])
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .append(
                &session,
                aborted.tail(),
                RecordBatch::new([
                    provider_record(SessionRecord::ProviderEpochClosed {
                        provider_epoch_id: epoch_id.to_owned(),
                    }),
                    epoch_opened("epoch-after-terminal-attempt", 0),
                ])
                .unwrap(),
            )
            .await
            .unwrap(),
        AppendOutcome::committed(aborted.tail().get() + 2)
    );
}

#[tokio::test]
async fn attempt_state_machine_requires_started_and_rejects_skips_and_post_terminal_transitions() {
    for invalid_initial_state in [
        AttemptRecordState::Prepared,
        AttemptRecordState::Aborted {
            reason: "preparation_without_reservation".to_owned(),
        },
    ] {
        assert_rejected_atomically_with_reason(
            Vec::new(),
            vec![attempt_advanced(
                "attempt-invalid-initial",
                "snapshot-1",
                1,
                invalid_initial_state,
            )],
            "invalid initial attempt state",
        )
        .await;
    }

    assert_rejected_atomically_with_reason(
        Vec::new(),
        vec![attempt_advanced(
            "attempt-started-issued",
            "snapshot-1",
            1,
            AttemptRecordState::Issued,
        )],
        "invalid initial attempt state",
    )
    .await;
    assert_rejected_atomically_with_reason(
        active_attempt_prefix([attempt_started("attempt-started-finished", "snapshot-1")]),
        vec![attempt_advanced(
            "attempt-started-finished",
            "snapshot-1",
            2,
            AttemptRecordState::Finished,
        )],
        "started attempt blocks other durable work",
    )
    .await;
    assert_rejected_atomically(
        active_attempt_prefix([
            attempt_started("attempt-skipped-issued", "snapshot-1"),
            attempt_prepared("attempt-skipped-issued", "snapshot-1"),
        ]),
        vec![attempt_advanced(
            "attempt-skipped-issued",
            "snapshot-1",
            3,
            AttemptRecordState::Finished,
        )],
    )
    .await;
    assert_rejected_atomically(
        active_attempt_prefix([
            attempt_started("attempt-aborted", "snapshot-1"),
            attempt_aborted("attempt-aborted", "snapshot-1", 2, "preparation"),
        ]),
        vec![attempt_advanced(
            "attempt-aborted",
            "snapshot-1",
            3,
            AttemptRecordState::Prepared,
        )],
    )
    .await;
    let mut no_publication = vec![
        epoch_opened("epoch-no-publication", 0),
        window_checkpoint("epoch-no-publication", 1, 0),
    ];
    no_publication.extend(completed_attempt_prefix(
        "attempt-no-publication",
        "snapshot-1",
        "operation-no-publication",
        "epoch-no-publication",
        0,
    ));
    assert_rejected_atomically(
        no_publication,
        vec![attempt_finished("attempt-no-publication", "snapshot-1")],
    )
    .await;

    let mut published = vec![
        epoch_opened("epoch-published-terminal", 0),
        window_checkpoint("epoch-published-terminal", 1, 0),
    ];
    published.extend(completed_attempt_prefix(
        "attempt-published",
        "snapshot-1",
        "operation-published-terminal",
        "epoch-published-terminal",
        0,
    ));
    published.extend([
        publication_prepared(
            "publication-terminal",
            "attempt-published",
            "snapshot-1",
            0,
            0,
            &["published"],
        ),
        attempt_finished("attempt-published", "snapshot-1"),
        canonical_record(1, "published"),
        cursor_advanced("publication-terminal", 0, 1),
        publication_committed("publication-terminal", "attempt-published", 0, 1, 1),
        attempt_published("attempt-published", "snapshot-1"),
    ]);
    assert_rejected_atomically(
        published,
        vec![attempt_advanced(
            "attempt-published",
            "snapshot-1",
            6,
            AttemptRecordState::Aborted {
                reason: "late_abort".to_owned(),
            },
        )],
    )
    .await;
}

#[tokio::test]
async fn issued_attempt_requires_its_frozen_provider_operation() {
    assert_rejected_atomically(
        active_attempt_prefix([
            attempt_started("attempt-without-operation", "snapshot-1"),
            attempt_prepared("attempt-without-operation", "snapshot-1"),
        ]),
        vec![attempt_issued("attempt-without-operation", "snapshot-1")],
    )
    .await;
}

#[tokio::test]
async fn publication_preparation_requires_a_completed_provider_operation() {
    let operation_id = "operation-failed-before-publication";
    let attempt_id = operation_attempt_id(operation_id);
    let snapshot_id = format!("snapshot-for-{operation_id}");
    let prefix = vec![
        epoch_opened("epoch-failed-publication", 0),
        window_checkpoint("epoch-failed-publication", 1, 0),
        operation_attempt_started(operation_id),
        operation_attempt_prepared(operation_id),
        operation_prepared(operation_id, "epoch-failed-publication", 1, 0),
        operation_attempt_issued(operation_id),
        operation_advanced(operation_id, 1, ProviderOperationState::Attempted),
        operation_advanced(operation_id, 2, ProviderOperationState::Failed),
    ];

    assert_rejected_atomically(
        prefix,
        vec![
            publication_prepared(
                "publication-after-failed-operation",
                &attempt_id,
                &snapshot_id,
                0,
                0,
                &["must not publish"],
            ),
            attempt_finished(&attempt_id, &snapshot_id),
        ],
    )
    .await;
}

#[tokio::test]
async fn started_attempt_blocks_other_durable_work_until_prepared_or_aborted() {
    for blocked in [
        canonical_record(1, "must wait for preparation"),
        epoch_opened("epoch-must-wait", 0),
        attempt_started("attempt-overlapping", "snapshot-overlapping"),
    ] {
        assert_rejected_atomically_with_reason(
            active_attempt_prefix([attempt_started("attempt-started", "snapshot-started")]),
            vec![blocked],
            "started attempt blocks other durable work",
        )
        .await;
    }

    for invalid_next_state in [
        AttemptRecordState::Started,
        AttemptRecordState::Issued,
        AttemptRecordState::Finished,
        AttemptRecordState::Published,
    ] {
        assert_rejected_atomically_with_reason(
            active_attempt_prefix([attempt_started(
                "attempt-invalid-next",
                "snapshot-invalid-next",
            )]),
            vec![attempt_advanced(
                "attempt-invalid-next",
                "snapshot-invalid-next",
                2,
                invalid_next_state,
            )],
            "started attempt blocks other durable work",
        )
        .await;
    }
}

#[tokio::test]
async fn publication_requires_one_finished_attempt_and_atomic_cursor_release() {
    assert_rejected_atomically(
        Vec::new(),
        vec![cursor_advanced("publication-orphan", 0, 1)],
    )
    .await;
    assert_rejected_atomically(
        active_attempt_prefix([
            attempt_started("attempt-not-finished", "snapshot-1"),
            attempt_prepared("attempt-not-finished", "snapshot-1"),
        ]),
        vec![
            canonical_record(1, "must not publish"),
            cursor_advanced("publication-early", 0, 1),
            publication_committed("publication-early", "attempt-not-finished", 0, 1, 1),
            attempt_advanced(
                "attempt-not-finished",
                "snapshot-1",
                3,
                AttemptRecordState::Published,
            ),
        ],
    )
    .await;
    let mut issued = vec![
        epoch_opened("epoch-unpublished", 0),
        window_checkpoint("epoch-unpublished", 1, 0),
    ];
    issued.extend(completed_attempt_prefix(
        "attempt-unpublished",
        "snapshot-1",
        "operation-unpublished",
        "epoch-unpublished",
        0,
    ));
    assert_rejected_atomically(
        issued,
        vec![attempt_finished("attempt-unpublished", "snapshot-1")],
    )
    .await;
}

#[tokio::test]
async fn publication_preparation_and_finished_attempt_cannot_be_split_across_appends() {
    let mut issued_attempt = vec![
        epoch_opened("epoch-atomic-preparation", 0),
        window_checkpoint("epoch-atomic-preparation", 1, 0),
    ];
    issued_attempt.extend(completed_attempt_prefix(
        "attempt-atomic-preparation",
        "snapshot-atomic-preparation",
        "operation-atomic-preparation",
        "epoch-atomic-preparation",
        0,
    ));

    assert_rejected_atomically(
        issued_attempt.clone(),
        vec![publication_prepared(
            "publication-atomic-preparation",
            "attempt-atomic-preparation",
            "snapshot-atomic-preparation",
            0,
            0,
            &["prepared turn"],
        )],
    )
    .await;

    assert_rejected_atomically(
        issued_attempt,
        vec![
            publication_prepared(
                "publication-atomic-preparation",
                "attempt-atomic-preparation",
                "snapshot-atomic-preparation",
                0,
                0,
                &["prepared turn"],
            ),
            canonical_record(1, "interleaved authority"),
            attempt_finished("attempt-atomic-preparation", "snapshot-atomic-preparation"),
        ],
    )
    .await;
}

#[tokio::test]
async fn finished_publication_candidate_blocks_unrelated_canonical_authority_until_commit() {
    let mut prefix = vec![
        epoch_opened("epoch-finished-publication", 0),
        window_checkpoint("epoch-finished-publication", 1, 0),
    ];
    prefix.extend(completed_attempt_prefix(
        "attempt-finished-publication",
        "snapshot-finished-publication",
        "operation-finished-publication",
        "epoch-finished-publication",
        0,
    ));
    prefix.extend([
        publication_prepared(
            "publication-finished",
            "attempt-finished-publication",
            "snapshot-finished-publication",
            0,
            0,
            &["frozen User candidate", "frozen Assistant output"],
        ),
        attempt_finished(
            "attempt-finished-publication",
            "snapshot-finished-publication",
        ),
    ]);

    assert_rejected_atomically(
        prefix,
        vec![canonical_record(1, "unrelated canonical drift")],
    )
    .await;
}

#[tokio::test]
async fn publication_canonical_items_must_match_prepared_items_exactly_and_in_order() {
    let mut prefix = vec![
        epoch_opened("epoch-exact-items", 0),
        window_checkpoint("epoch-exact-items", 1, 0),
    ];
    prefix.extend(completed_attempt_prefix(
        "attempt-exact-items",
        "snapshot-exact-items",
        "operation-exact-items",
        "epoch-exact-items",
        0,
    ));
    prefix.extend([
        publication_prepared(
            "publication-exact-items",
            "attempt-exact-items",
            "snapshot-exact-items",
            0,
            0,
            &["first prepared item", "second prepared item"],
        ),
        attempt_finished("attempt-exact-items", "snapshot-exact-items"),
    ]);

    for canonical_items in [
        ["first prepared item", "changed second item"],
        ["second prepared item", "first prepared item"],
    ] {
        assert_rejected_atomically(
            prefix.clone(),
            vec![
                canonical_record(1, canonical_items[0]),
                canonical_record(2, canonical_items[1]),
                cursor_advanced("publication-exact-items", 0, 1),
                publication_committed("publication-exact-items", "attempt-exact-items", 0, 2, 1),
                attempt_published("attempt-exact-items", "snapshot-exact-items"),
            ],
        )
        .await;
    }
}

#[tokio::test]
async fn publication_commit_cannot_change_prepared_identity_base_or_cursor() {
    let mut prefix = vec![
        canonical_record(1, "existing authority"),
        epoch_opened("epoch-frozen-publication", 1),
        window_checkpoint("epoch-frozen-publication", 1, 1),
    ];
    prefix.extend(completed_attempt_prefix(
        "attempt-frozen-publication",
        "snapshot-frozen-publication",
        "operation-frozen-publication",
        "epoch-frozen-publication",
        1,
    ));
    prefix.extend([
        publication_prepared(
            "publication-frozen",
            "attempt-frozen-publication",
            "snapshot-frozen-publication",
            1,
            0,
            &["published authority"],
        ),
        attempt_finished("attempt-frozen-publication", "snapshot-frozen-publication"),
    ]);

    assert_rejected_atomically(
        prefix.clone(),
        vec![
            canonical_record(2, "published authority"),
            cursor_advanced("publication-renamed", 0, 1),
            publication_committed("publication-renamed", "attempt-frozen-publication", 1, 2, 1),
            attempt_published("attempt-frozen-publication", "snapshot-frozen-publication"),
        ],
    )
    .await;

    assert_rejected_atomically(
        prefix.clone(),
        vec![
            canonical_record(2, "published authority"),
            cursor_advanced("publication-frozen", 0, 1),
            publication_committed("publication-frozen", "attempt-frozen-publication", 0, 2, 1),
            attempt_published("attempt-frozen-publication", "snapshot-frozen-publication"),
        ],
    )
    .await;

    assert_rejected_atomically(
        prefix,
        vec![
            canonical_record(2, "published authority"),
            cursor_advanced("publication-frozen", 0, 1),
            publication_committed("publication-frozen", "attempt-frozen-publication", 1, 2, 2),
            attempt_published("attempt-frozen-publication", "snapshot-frozen-publication"),
        ],
    )
    .await;
}

#[tokio::test]
async fn publication_base_is_the_authoritative_transcript_revision_at_batch_start() {
    let mut prefix = vec![
        canonical_record(1, "already authoritative one"),
        canonical_record(2, "already authoritative two"),
        epoch_opened("epoch-exact-base", 2),
        window_checkpoint("epoch-exact-base", 1, 2),
    ];
    prefix.extend(completed_attempt_prefix(
        "attempt-exact-base",
        "snapshot-exact-base",
        "operation-exact-base",
        "epoch-exact-base",
        2,
    ));
    prefix.extend([
        publication_prepared(
            "publication-exact-base",
            "attempt-exact-base",
            "snapshot-exact-base",
            2,
            0,
            &["published candidate", "published output"],
        ),
        attempt_finished("attempt-exact-base", "snapshot-exact-base"),
    ]);

    for invalid_base in [1, 3] {
        assert_rejected_atomically(
            prefix.clone(),
            vec![
                canonical_record(3, "published candidate"),
                canonical_record(4, "published output"),
                cursor_advanced("publication-exact-base", 0, 1),
                publication_committed(
                    "publication-exact-base",
                    "attempt-exact-base",
                    invalid_base,
                    4,
                    1,
                ),
                attempt_published("attempt-exact-base", "snapshot-exact-base"),
            ],
        )
        .await;
    }

    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-publication-exact-base").unwrap();
    let prefix_tail = append_fixture_records(&store, &session, LogRevision::ZERO, prefix)
        .await
        .tail();
    let publication = vec![
        canonical_record(3, "published candidate"),
        canonical_record(4, "published output"),
        cursor_advanced("publication-exact-base", 0, 1),
        publication_committed("publication-exact-base", "attempt-exact-base", 2, 4, 1),
        attempt_published("attempt-exact-base", "snapshot-exact-base"),
    ];
    let publication_outcome = store
        .append(
            &session,
            prefix_tail,
            RecordBatch::new(publication).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(publication_outcome, AppendOutcome::committed(17));
    assert_eq!(
        store
            .append(
                &session,
                publication_outcome.tail(),
                RecordBatch::new([canonical_record(5, "authority after publication")]).unwrap(),
            )
            .await
            .unwrap(),
        AppendOutcome::committed(18)
    );
}

#[tokio::test]
async fn artifact_records_reject_missing_mismatched_and_reused_authority() {
    assert_rejected_atomically(Vec::new(), vec![artifact_committed("epoch-1")]).await;
    assert_rejected_atomically(
        Vec::new(),
        vec![artifact_attached("epoch-1", "attachment-1")],
    )
    .await;

    let (_, _, artifact_fingerprint, _) = artifact_context();
    let mismatched_attach = provider_record(SessionRecord::EpochArtifactLogicallyAttached {
        epoch_id: "epoch-1".to_owned(),
        attachment_id: "attachment-1".to_owned(),
        artifact_fingerprint,
        contract_fingerprint: "agentview.contract-manifest/v1:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned(),
    });
    assert_rejected_atomically(
        Vec::new(),
        vec![
            epoch_opened("epoch-1", 0),
            artifact_committed("epoch-1"),
            mismatched_attach,
            canonical_system_record(1),
        ],
    )
    .await;

    let mut admitted = vec![epoch_opened("epoch-1", 0)];
    admitted.extend(artifact_admission("epoch-1", 1));
    assert_rejected_atomically(admitted.clone(), vec![artifact_committed("epoch-1")]).await;
    assert_rejected_atomically(
        admitted.clone(),
        vec![artifact_attached("epoch-1", "attachment-2")],
    )
    .await;
    assert_rejected_atomically(
        Vec::new(),
        vec![provider_artifact_skipped(
            "epoch-1",
            "skip-1",
            "attempt-skip-1",
        )],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0)],
        vec![provider_artifact_skipped(
            "epoch-1",
            "skip-1",
            "attempt-skip-1",
        )],
    )
    .await;

    let (_, _, _, contract_fingerprint) = artifact_context();
    let wrong_context = provider_record(SessionRecord::ProviderArtifactAttachAdvanced {
        provider_epoch_id: "epoch-1".to_owned(),
        attach_operation_id: "skip-wrong".to_owned(),
        attach_revision: 1,
        artifact_fingerprint: "agentview.system-artifact/v1:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned(),
        contract_fingerprint,
        encoder_version: "codex-http-v1".to_owned(),
        request_sha256: None,
        request_body: None,
        skip_attempt_id: Some("attempt-skip-wrong".to_owned()),
        state: ProviderArtifactAttachState::Skipped,
    });
    assert_rejected_atomically(admitted.clone(), vec![wrong_context]).await;

    let mut with_skip = admitted;
    with_skip.push(attempt_started("attempt-skip-1", "snapshot-skip-1"));
    with_skip.push(attempt_prepared("attempt-skip-1", "snapshot-skip-1"));
    with_skip.push(provider_artifact_skipped(
        "epoch-1",
        "skip-1",
        "attempt-skip-1",
    ));
    assert_rejected_atomically(
        with_skip,
        vec![provider_artifact_skipped(
            "epoch-1",
            "skip-1",
            "attempt-skip-1",
        )],
    )
    .await;
}

#[tokio::test]
async fn artifact_authority_requires_one_exact_atomic_admission_batch() {
    assert_rejected_atomically(
        vec![epoch_opened("epoch-atomic-artifact", 0)],
        vec![canonical_system_record(1)],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-atomic-artifact", 0)],
        vec![
            artifact_committed("epoch-atomic-artifact"),
            artifact_attached("epoch-atomic-artifact", "attachment-atomic-artifact"),
        ],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-atomic-artifact", 0)],
        vec![
            artifact_committed("epoch-atomic-artifact"),
            artifact_attached("epoch-atomic-artifact", "attachment-atomic-artifact"),
            provider_record(SessionRecord::CanonicalItemAppended {
                transcript_revision: TranscriptRevision::new(1),
                item: CanonicalInputItem::instruction(
                    InstructionAuthority::System,
                    system_document_with_text("different system artifact"),
                ),
            }),
        ],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-atomic-artifact", 0)],
        vec![
            artifact_committed("epoch-atomic-artifact"),
            canonical_system_record(1),
            artifact_attached("epoch-atomic-artifact", "attachment-atomic-artifact"),
        ],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-atomic-artifact", 0)],
        vec![
            artifact_committed("epoch-atomic-artifact"),
            artifact_attached("epoch-atomic-artifact", "attachment-atomic-artifact"),
            provider_artifact_skipped(
                "epoch-atomic-artifact",
                "skip-too-early",
                "attempt-skip-too-early",
            ),
            canonical_system_record(1),
        ],
    )
    .await;

    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-atomic-artifact").unwrap();
    let mut records = vec![epoch_opened("epoch-atomic-artifact", 0)];
    records.extend(artifact_admission("epoch-atomic-artifact", 1));
    assert_eq!(
        store
            .append(
                &session,
                LogRevision::ZERO,
                RecordBatch::new(records).unwrap(),
            )
            .await
            .unwrap(),
        AppendOutcome::committed(4)
    );
}

#[tokio::test]
async fn provider_operation_requires_explicit_artifact_attach_or_stateless_skip() {
    let mut base = vec![
        epoch_opened("epoch-provider-artifact", 0),
        window_checkpoint("epoch-provider-artifact", 1, 0),
    ];
    base.extend(artifact_admission("epoch-provider-artifact", 1));
    base.push(window_checkpoint("epoch-provider-artifact", 2, 1));
    let prepared_prefix = |operation_id: &str| {
        let mut prefix = base.clone();
        prefix.extend([
            operation_attempt_started(operation_id),
            operation_attempt_prepared(operation_id),
        ]);
        prefix
    };

    assert_rejected_atomically(
        prepared_prefix("operation-without-artifact-phase"),
        vec![operation_prepared(
            "operation-without-artifact-phase",
            "epoch-provider-artifact",
            2,
            1,
        )],
    )
    .await;
    assert_rejected_atomically(
        prepared_prefix("operation-after-failed-artifact-attach"),
        vec![
            provider_artifact_state(
                "epoch-provider-artifact",
                "failed-provider-artifact",
                1,
                ProviderArtifactAttachState::Failed,
            ),
            operation_prepared(
                "operation-after-failed-artifact-attach",
                "epoch-provider-artifact",
                2,
                1,
            ),
        ],
    )
    .await;

    let invalid_prepared_attach = NewRecord::new(
        1,
        SessionRecord::ProviderArtifactAttachAdvanced {
            provider_epoch_id: "epoch-provider-artifact".to_owned(),
            attach_operation_id: "invalid-provider-artifact".to_owned(),
            attach_revision: 1,
            artifact_fingerprint: artifact_context().2,
            contract_fingerprint: artifact_context().3,
            encoder_version: "codex-http-v1".to_owned(),
            request_sha256: None,
            request_body: None,
            skip_attempt_id: None,
            state: ProviderArtifactAttachState::Prepared,
        },
    );
    assert!(invalid_prepared_attach.is_err());

    let mut first_operation_prefix = prepared_prefix("first-operation-after-artifact-phase");
    first_operation_prefix.extend([
        provider_artifact_skipped(
            "epoch-provider-artifact",
            "skip-first-operation",
            &operation_attempt_id("first-operation-after-artifact-phase"),
        ),
        operation_prepared(
            "first-operation-after-artifact-phase",
            "epoch-provider-artifact",
            2,
            1,
        ),
        attempt_aborted(
            &operation_attempt_id("first-operation-after-artifact-phase"),
            "snapshot-for-first-operation-after-artifact-phase",
            3,
            "first_operation_fenced",
        ),
        operation_attempt_started("second-operation-without-fresh-skip"),
        operation_attempt_prepared("second-operation-without-fresh-skip"),
    ]);
    assert_rejected_atomically(
        first_operation_prefix,
        vec![operation_prepared(
            "second-operation-without-fresh-skip",
            "epoch-provider-artifact",
            2,
            1,
        )],
    )
    .await;

    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-provider-artifact-phase").unwrap();
    let prefix = prepared_prefix("operation-after-artifact-phase");
    let prefix_tail = append_fixture_records(&store, &session, LogRevision::ZERO, prefix)
        .await
        .tail();
    let first_operation_outcome = store
        .append(
            &session,
            prefix_tail,
            RecordBatch::new([
                provider_artifact_skipped(
                    "epoch-provider-artifact",
                    "skip-provider-artifact",
                    &operation_attempt_id("operation-after-artifact-phase"),
                ),
                operation_prepared(
                    "operation-after-artifact-phase",
                    "epoch-provider-artifact",
                    2,
                    1,
                ),
            ])
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_operation_outcome, AppendOutcome::committed(10));
    assert_eq!(
        append_fixture_records(
            &store,
            &session,
            first_operation_outcome.tail(),
            [
                attempt_aborted(
                    &operation_attempt_id("operation-after-artifact-phase"),
                    "snapshot-for-operation-after-artifact-phase",
                    3,
                    "first_operation_fenced",
                ),
                operation_attempt_started("second-operation-after-artifact-phase"),
                operation_attempt_prepared("second-operation-after-artifact-phase"),
                provider_artifact_skipped(
                    "epoch-provider-artifact",
                    "skip-second-provider-artifact",
                    &operation_attempt_id("second-operation-after-artifact-phase"),
                ),
                operation_prepared(
                    "second-operation-after-artifact-phase",
                    "epoch-provider-artifact",
                    2,
                    1,
                ),
            ],
        )
        .await,
        AppendOutcome::committed(15)
    );
}

#[tokio::test]
async fn remote_attach_state_machine_is_revision_identity_and_exact_bytes_bound() {
    let epoch_id = "epoch-remote-state-machine";
    let attempt_id = "attempt-remote-state-machine";
    let operation_id = "attach-remote-state-machine";
    let mut records = vec![
        epoch_opened_with_mode(epoch_id, 0, ProviderArtifactMode::RemoteRetained),
        window_checkpoint(epoch_id, 1, 0),
        attempt_started(attempt_id, "snapshot-remote-state-machine"),
        attempt_prepared(attempt_id, "snapshot-remote-state-machine"),
    ];
    records.extend(artifact_admission(epoch_id, 1));
    records.extend([
        remote_provider_artifact_state(
            epoch_id,
            operation_id,
            1,
            ProviderArtifactAttachState::Prepared,
        ),
        remote_provider_artifact_state(
            epoch_id,
            operation_id,
            2,
            ProviderArtifactAttachState::Attempted,
        ),
        remote_provider_artifact_state(
            epoch_id,
            operation_id,
            3,
            ProviderArtifactAttachState::Accepted,
        ),
        remote_provider_artifact_state(
            epoch_id,
            operation_id,
            4,
            ProviderArtifactAttachState::Completed,
        ),
        window_checkpoint(epoch_id, 2, 1),
        operation_prepared_for_attempt("operation-after-remote-attach", attempt_id, epoch_id, 2, 1),
    ]);

    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-remote-state-machine").unwrap();
    let outcome = append_fixture_records(&store, &session, LogRevision::ZERO, records).await;
    assert_eq!(outcome, AppendOutcome::committed(13));

    assert_rejected_atomically(
        vec![
            epoch_opened_with_mode(
                "epoch-remote-invalid-transition",
                0,
                ProviderArtifactMode::RemoteRetained,
            ),
            window_checkpoint("epoch-remote-invalid-transition", 1, 0),
            artifact_committed("epoch-remote-invalid-transition"),
            artifact_attached(
                "epoch-remote-invalid-transition",
                "attachment-remote-invalid-transition",
            ),
            canonical_system_record(1),
            remote_provider_artifact_state(
                "epoch-remote-invalid-transition",
                "attach-remote-invalid-transition",
                1,
                ProviderArtifactAttachState::Prepared,
            ),
        ],
        vec![remote_provider_artifact_state(
            "epoch-remote-invalid-transition",
            "attach-remote-invalid-transition",
            2,
            ProviderArtifactAttachState::Completed,
        )],
    )
    .await;

    let wrong_profile_epoch = "epoch-remote-wrong-profile";
    let mut wrong_profile_prefix = vec![
        epoch_opened_with_mode(wrong_profile_epoch, 0, ProviderArtifactMode::RemoteRetained),
        window_checkpoint(wrong_profile_epoch, 1, 0),
        attempt_started(
            "attempt-remote-wrong-profile",
            "snapshot-remote-wrong-profile",
        ),
        attempt_prepared(
            "attempt-remote-wrong-profile",
            "snapshot-remote-wrong-profile",
        ),
    ];
    wrong_profile_prefix.extend(artifact_admission(wrong_profile_epoch, 1));
    let (_, _, artifact_fingerprint, contract_fingerprint) = artifact_context();
    let request_body = b"wrong-profile attach request".to_vec();
    assert_rejected_atomically(
        wrong_profile_prefix,
        vec![provider_record(
            SessionRecord::ProviderArtifactAttachAdvanced {
                provider_epoch_id: wrong_profile_epoch.to_owned(),
                attach_operation_id: "attach-remote-wrong-profile".to_owned(),
                attach_revision: 1,
                artifact_fingerprint,
                contract_fingerprint,
                encoder_version: "different-profile-v1".to_owned(),
                request_sha256: Some(format!("sha256:{:x}", Sha256::digest(&request_body))),
                request_body: Some(request_body),
                skip_attempt_id: None,
                state: ProviderArtifactAttachState::Prepared,
            },
        )],
    )
    .await;
}

#[tokio::test]
async fn only_completed_remote_attach_unlocks_provider_operation_preparation() {
    for (suffix, states) in [
        ("prepared", vec![ProviderArtifactAttachState::Prepared]),
        (
            "attempted",
            vec![
                ProviderArtifactAttachState::Prepared,
                ProviderArtifactAttachState::Attempted,
            ],
        ),
        (
            "accepted",
            vec![
                ProviderArtifactAttachState::Prepared,
                ProviderArtifactAttachState::Attempted,
                ProviderArtifactAttachState::Accepted,
            ],
        ),
        (
            "failed",
            vec![
                ProviderArtifactAttachState::Prepared,
                ProviderArtifactAttachState::Attempted,
                ProviderArtifactAttachState::Failed,
            ],
        ),
    ] {
        let epoch_id = format!("epoch-remote-{suffix}");
        let attempt_id = format!("attempt-remote-{suffix}");
        let attach_id = format!("attach-remote-{suffix}");
        let mut prefix = vec![
            epoch_opened_with_mode(&epoch_id, 0, ProviderArtifactMode::RemoteRetained),
            window_checkpoint(&epoch_id, 1, 0),
            attempt_started(&attempt_id, &format!("snapshot-remote-{suffix}")),
            attempt_prepared(&attempt_id, &format!("snapshot-remote-{suffix}")),
        ];
        prefix.extend(artifact_admission(&epoch_id, 1));
        for (index, state) in states.into_iter().enumerate() {
            prefix.push(remote_provider_artifact_state(
                &epoch_id,
                &attach_id,
                u64::try_from(index).unwrap() + 1,
                state,
            ));
        }
        assert_rejected_atomically(
            prefix,
            vec![operation_prepared_for_attempt(
                &format!("operation-remote-{suffix}"),
                &attempt_id,
                &epoch_id,
                2,
                1,
            )],
        )
        .await;
    }
}

#[tokio::test]
async fn provider_artifact_mode_and_stateless_skip_attempt_are_durable_authority() {
    let epoch_id = "epoch-stateless-attempt-bound";
    let attempt_a = "attempt-stateless-a";
    let attempt_b = "attempt-stateless-b";
    let mut prefix = vec![
        epoch_opened_with_mode(epoch_id, 0, ProviderArtifactMode::Stateless),
        window_checkpoint(epoch_id, 1, 0),
    ];
    prefix.extend(artifact_admission(epoch_id, 1));
    prefix.extend([
        window_checkpoint(epoch_id, 2, 1),
        attempt_started(attempt_a, "snapshot-stateless-a"),
        attempt_prepared(attempt_a, "snapshot-stateless-a"),
    ]);
    assert_rejected_atomically(
        prefix,
        vec![provider_artifact_skipped(
            epoch_id,
            "skip-stateless-wrong-attempt",
            attempt_b,
        )],
    )
    .await;

    let remote_epoch = "epoch-remote-rejects-skip";
    let mut remote_prefix = vec![
        epoch_opened_with_mode(remote_epoch, 0, ProviderArtifactMode::RemoteRetained),
        window_checkpoint(remote_epoch, 1, 0),
    ];
    remote_prefix.extend(artifact_admission(remote_epoch, 1));
    assert_rejected_atomically(
        remote_prefix,
        vec![provider_record(
            SessionRecord::ProviderArtifactAttachAdvanced {
                provider_epoch_id: remote_epoch.to_owned(),
                attach_operation_id: "skip-remote".to_owned(),
                attach_revision: 1,
                artifact_fingerprint: artifact_context().2,
                contract_fingerprint: artifact_context().3,
                encoder_version: "codex-http-v1".to_owned(),
                request_sha256: None,
                request_body: None,
                skip_attempt_id: Some("attempt-remote".to_owned()),
                state: ProviderArtifactAttachState::Skipped,
            },
        )],
    )
    .await;
}

#[tokio::test]
async fn artifact_admission_cannot_follow_a_provider_operation_in_the_same_epoch() {
    assert_rejected_atomically(
        vec![
            epoch_opened("epoch-late-artifact", 0),
            window_checkpoint("epoch-late-artifact", 1, 0),
            operation_attempt_started("operation-before-artifact"),
            operation_attempt_prepared("operation-before-artifact"),
            operation_prepared("operation-before-artifact", "epoch-late-artifact", 1, 0),
        ],
        artifact_admission("epoch-late-artifact", 1),
    )
    .await;
}

#[test]
fn prepared_operation_digest_must_match_the_exact_request_body() {
    let request_body = b"authoritative request bytes".to_vec();
    let error = NewRecord::new(
        1,
        SessionRecord::ProviderOperationPrepared {
            operation_id: "operation-digest".to_owned(),
            attempt_id: "attempt-digest".to_owned(),
            provider_epoch_id: "epoch-digest".to_owned(),
            window_revision: 1,
            transcript_revision: TranscriptRevision::ZERO,
            user_diff_cursor: UserDiffCursor::ZERO,
            encoder_version: "codex-http-v1".to_owned(),
            request_sha256: request_sha256(b"different request bytes"),
            request_body,
        },
    )
    .expect_err("a well-formed digest for different bytes must fail");

    assert!(matches!(
        error,
        RecordLogError::RequestDigestMismatch { .. }
    ));
}

#[test]
fn prepared_artifact_attach_digest_must_match_the_exact_request_body() {
    let (_, _, artifact_fingerprint, contract_fingerprint) = artifact_context();
    let request_body = b"authoritative artifact attach bytes".to_vec();
    let error = NewRecord::new(
        1,
        SessionRecord::ProviderArtifactAttachAdvanced {
            provider_epoch_id: "epoch-artifact-digest".to_owned(),
            attach_operation_id: "attach-artifact-digest".to_owned(),
            attach_revision: 1,
            artifact_fingerprint,
            contract_fingerprint,
            encoder_version: "codex-http-v1".to_owned(),
            request_sha256: Some(request_sha256(b"different artifact attach bytes")),
            request_body: Some(request_body),
            skip_attempt_id: None,
            state: ProviderArtifactAttachState::Prepared,
        },
    )
    .expect_err("a well-formed digest for different attach bytes must fail");

    assert!(matches!(
        error,
        RecordLogError::RequestDigestMismatch { .. }
    ));
}

#[tokio::test]
async fn memory_store_accepts_one_cross_record_consistent_provider_flow() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-provider-flow").unwrap();
    let records = [
        canonical_record(1, "first"),
        epoch_opened("epoch-1", 1),
        window_checkpoint("epoch-1", 1, 1),
        operation_attempt_started("operation-1"),
        operation_attempt_prepared("operation-1"),
        operation_prepared("operation-1", "epoch-1", 1, 1),
        operation_attempt_issued("operation-1"),
        operation_advanced("operation-1", 1, ProviderOperationState::Attempted),
        operation_advanced("operation-1", 2, ProviderOperationState::Accepted),
        operation_advanced("operation-1", 3, ProviderOperationState::Completed),
        attempt_aborted(
            &operation_attempt_id("operation-1"),
            "snapshot-for-operation-1",
            4,
            "completed_without_publication",
        ),
        epoch_closed("epoch-1"),
    ];

    assert_eq!(
        append_fixture_records(&store, &session, LogRevision::ZERO, records).await,
        AppendOutcome::committed(12)
    );
}

#[tokio::test]
async fn append_rejects_non_contiguous_transcript_and_invalid_epoch_lifecycles() {
    assert_rejected_atomically(Vec::new(), vec![canonical_record(2, "skipped")]).await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0)],
        vec![epoch_opened("epoch-2", 0)],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0), epoch_closed("epoch-1")],
        vec![epoch_opened("epoch-1", 0)],
    )
    .await;
    assert_rejected_atomically(Vec::new(), vec![epoch_closed("epoch-unknown")]).await;
    assert_rejected_atomically(Vec::new(), vec![epoch_opened("epoch-ahead", 1)]).await;
}

#[tokio::test]
async fn append_rejects_windows_without_active_contiguous_transcript_authority() {
    assert_rejected_atomically(Vec::new(), vec![window_checkpoint("epoch-unknown", 1, 0)]).await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0), epoch_closed("epoch-1")],
        vec![window_checkpoint("epoch-1", 1, 0)],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0)],
        vec![window_checkpoint("epoch-1", 2, 0)],
    )
    .await;
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0)],
        vec![window_checkpoint("epoch-1", 1, 1)],
    )
    .await;
    assert_rejected_atomically(
        vec![canonical_record(1, "first"), epoch_opened("epoch-1", 1)],
        vec![window_checkpoint("epoch-1", 1, 0)],
    )
    .await;
    assert_rejected_atomically(
        vec![
            canonical_record(1, "first"),
            canonical_record(2, "second"),
            epoch_opened("epoch-1", 0),
            window_checkpoint("epoch-1", 1, 2),
        ],
        vec![window_checkpoint("epoch-1", 2, 1)],
    )
    .await;
}

#[tokio::test]
async fn append_rejects_operations_without_prepared_contiguous_authority() {
    let prepared_window = |operation_id: &str| {
        vec![
            epoch_opened("epoch-1", 0),
            window_checkpoint("epoch-1", 1, 0),
            operation_attempt_started(operation_id),
            operation_attempt_prepared(operation_id),
        ]
    };
    assert_rejected_atomically(
        vec![epoch_opened("epoch-1", 0)],
        vec![operation_prepared("operation-1", "epoch-1", 1, 0)],
    )
    .await;
    assert_rejected_atomically(
        vec![
            epoch_opened("epoch-1", 0),
            window_checkpoint("epoch-1", 1, 0),
            canonical_record(1, "later"),
        ],
        vec![operation_prepared("operation-1", "epoch-1", 1, 1)],
    )
    .await;
    assert_rejected_atomically(
        [
            prepared_window("operation-1"),
            vec![operation_prepared("operation-1", "epoch-1", 1, 0)],
        ]
        .concat(),
        vec![operation_prepared("operation-1", "epoch-1", 1, 0)],
    )
    .await;
    assert_rejected_atomically(
        Vec::new(),
        vec![operation_advanced(
            "operation-unknown",
            1,
            ProviderOperationState::Failed,
        )],
    )
    .await;
    assert_rejected_atomically(
        [
            prepared_window("operation-1"),
            vec![operation_prepared("operation-1", "epoch-1", 1, 0)],
        ]
        .concat(),
        vec![operation_advanced(
            "operation-1",
            2,
            ProviderOperationState::Accepted,
        )],
    )
    .await;
    assert_rejected_atomically(
        [
            prepared_window("operation-terminal-first"),
            vec![operation_prepared(
                "operation-terminal-first",
                "epoch-1",
                1,
                0,
            )],
        ]
        .concat(),
        vec![operation_advanced(
            "operation-terminal-first",
            1,
            ProviderOperationState::Completed,
        )],
    )
    .await;
    assert_rejected_atomically(
        [
            prepared_window("operation-double-attempt"),
            vec![
                operation_prepared("operation-double-attempt", "epoch-1", 1, 0),
                operation_attempt_issued("operation-double-attempt"),
                operation_advanced(
                    "operation-double-attempt",
                    1,
                    ProviderOperationState::Attempted,
                ),
            ],
        ]
        .concat(),
        vec![operation_advanced(
            "operation-double-attempt",
            2,
            ProviderOperationState::Attempted,
        )],
    )
    .await;
    assert_rejected_atomically(
        [
            prepared_window("operation-after-completed"),
            vec![
                operation_prepared("operation-after-completed", "epoch-1", 1, 0),
                operation_attempt_issued("operation-after-completed"),
                operation_advanced(
                    "operation-after-completed",
                    1,
                    ProviderOperationState::Attempted,
                ),
                operation_advanced(
                    "operation-after-completed",
                    2,
                    ProviderOperationState::Completed,
                ),
            ],
        ]
        .concat(),
        vec![operation_advanced(
            "operation-after-completed",
            3,
            ProviderOperationState::Attempted,
        )],
    )
    .await;
}

#[tokio::test]
async fn provider_operation_is_single_per_attempt_and_cannot_run_before_issuance_or_after_failure()
{
    let base = vec![
        epoch_opened("epoch-operation-authority", 0),
        window_checkpoint("epoch-operation-authority", 1, 0),
        operation_attempt_started("operation-primary"),
        operation_attempt_prepared("operation-primary"),
        operation_prepared("operation-primary", "epoch-operation-authority", 1, 0),
    ];

    assert_rejected_atomically(
        base.clone(),
        vec![operation_prepared_for_attempt(
            "operation-duplicate-attempt",
            &operation_attempt_id("operation-primary"),
            "epoch-operation-authority",
            1,
            0,
        )],
    )
    .await;

    assert_rejected_atomically(
        base.clone(),
        vec![operation_advanced(
            "operation-primary",
            1,
            ProviderOperationState::Attempted,
        )],
    )
    .await;

    assert_rejected_atomically(
        [
            base,
            vec![
                operation_attempt_issued("operation-primary"),
                operation_advanced("operation-primary", 1, ProviderOperationState::Attempted),
                operation_advanced("operation-primary", 2, ProviderOperationState::Failed),
            ],
        ]
        .concat(),
        vec![operation_advanced(
            "operation-primary",
            3,
            ProviderOperationState::Attempted,
        )],
    )
    .await;
}

#[tokio::test]
async fn indeterminate_provider_operation_blocks_a_second_attempt_atomically() {
    let first_operation = "operation-indeterminate";
    let second_operation = "operation-overlapping";
    let prefix = vec![
        epoch_opened("epoch-operation-fence", 0),
        window_checkpoint("epoch-operation-fence", 1, 0),
        operation_attempt_started(first_operation),
        operation_attempt_prepared(first_operation),
        operation_prepared(first_operation, "epoch-operation-fence", 1, 0),
        operation_attempt_issued(first_operation),
        operation_advanced(first_operation, 1, ProviderOperationState::Attempted),
    ];
    let overlapping_attempt = vec![
        operation_attempt_started(second_operation),
        operation_attempt_prepared(second_operation),
        operation_prepared(second_operation, "epoch-operation-fence", 1, 0),
        operation_attempt_issued(second_operation),
        operation_advanced(second_operation, 1, ProviderOperationState::Attempted),
    ];

    assert_rejected_atomically(prefix.clone(), overlapping_attempt.clone()).await;

    let accepted_prefix = [
        prefix,
        vec![operation_advanced(
            first_operation,
            2,
            ProviderOperationState::Accepted,
        )],
    ]
    .concat();
    assert_rejected_atomically(accepted_prefix.clone(), overlapping_attempt).await;

    let terminal_prefix = [
        accepted_prefix,
        vec![
            operation_advanced(first_operation, 3, ProviderOperationState::Completed),
            attempt_aborted(
                &operation_attempt_id(first_operation),
                &format!("snapshot-for-{first_operation}"),
                4,
                "completed_operation_not_published",
            ),
        ],
    ]
    .concat();
    let next_attempt = vec![
        operation_attempt_started(second_operation),
        operation_attempt_prepared(second_operation),
        operation_prepared(second_operation, "epoch-operation-fence", 1, 0),
        operation_attempt_issued(second_operation),
        operation_advanced(second_operation, 1, ProviderOperationState::Attempted),
    ];
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-operation-fence-release").unwrap();
    let prefix_tail = append_fixture_records(&store, &session, LogRevision::ZERO, terminal_prefix)
        .await
        .tail();

    let outcome = append_fixture_records(&store, &session, prefix_tail, next_attempt).await;
    assert_eq!(outcome.tail().get(), prefix_tail.get() + 5);
}

#[tokio::test]
async fn prepared_provider_operation_blocks_overlap_but_allows_owning_attempt_fence() {
    let first_operation = "operation-prepared";
    let second_operation = "operation-after-prepared";
    let prepared_prefix = vec![
        epoch_opened("epoch-prepared-operation-fence", 0),
        window_checkpoint("epoch-prepared-operation-fence", 1, 0),
        operation_attempt_started(first_operation),
        operation_attempt_prepared(first_operation),
        operation_prepared(first_operation, "epoch-prepared-operation-fence", 1, 0),
    ];
    let overlapping_preparation = vec![
        operation_attempt_started(second_operation),
        operation_attempt_prepared(second_operation),
        operation_prepared(second_operation, "epoch-prepared-operation-fence", 1, 0),
    ];

    assert_rejected_atomically(prepared_prefix.clone(), overlapping_preparation.clone()).await;
    let issued_prefix = [
        prepared_prefix.clone(),
        vec![operation_attempt_issued(first_operation)],
    ]
    .concat();
    assert_rejected_atomically(issued_prefix, overlapping_preparation).await;

    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-prepared-operation-fence").unwrap();
    let prepared_tail =
        append_fixture_records(&store, &session, LogRevision::ZERO, prepared_prefix)
            .await
            .tail();
    let fenced_tail = store
        .append(
            &session,
            prepared_tail,
            RecordBatch::new([attempt_advanced(
                &operation_attempt_id(first_operation),
                &format!("snapshot-for-{first_operation}"),
                3,
                AttemptRecordState::Aborted {
                    reason: "recovery_fenced_pre_issue".to_owned(),
                },
            )])
            .unwrap(),
        )
        .await
        .unwrap()
        .tail();

    let outcome = append_fixture_records(
        &store,
        &session,
        fenced_tail,
        [
            operation_attempt_started(second_operation),
            operation_attempt_prepared(second_operation),
            operation_prepared(second_operation, "epoch-prepared-operation-fence", 1, 0),
        ],
    )
    .await;
    assert_eq!(outcome.tail().get(), fenced_tail.get() + 3);
}

#[tokio::test]
async fn compaction_claim_completion_and_checkpoint_are_identity_bound_and_exclusive() {
    let opaque_state = Some(json!({"summary": "turn-1"}));
    let prepared = compaction_prepared("compaction-1", "epoch-1", 1, 0);
    let attempted = compaction_advanced("compaction-1", 1, ProviderCompactionState::Attempted);
    let completed = compaction_advanced(
        "compaction-1",
        2,
        ProviderCompactionState::Completed {
            opaque_state: opaque_state.clone(),
        },
    );

    for invalid_claim in [
        compaction_prepared("compaction-wrong-epoch", "epoch-2", 1, 0),
        compaction_prepared("compaction-wrong-window", "epoch-1", 2, 0),
        compaction_prepared("compaction-wrong-transcript", "epoch-1", 1, 1),
    ] {
        assert_rejected_atomically(compaction_base(), vec![invalid_claim]).await;
    }

    assert_rejected_atomically(
        [compaction_base(), vec![prepared.clone()]].concat(),
        vec![compaction_prepared("compaction-2", "epoch-1", 1, 0)],
    )
    .await;
    assert_rejected_atomically(
        [compaction_base(), vec![prepared.clone()]].concat(),
        vec![epoch_closed("epoch-1")],
    )
    .await;
    assert_rejected_atomically(
        [compaction_base(), vec![prepared.clone()]].concat(),
        vec![compaction_advanced(
            "compaction-replaced",
            1,
            ProviderCompactionState::Attempted,
        )],
    )
    .await;
    assert_rejected_atomically(
        [compaction_base(), vec![prepared.clone()]].concat(),
        vec![compaction_checkpointed(
            "compaction-1",
            "epoch-1",
            1,
            2,
            0,
            opaque_state.clone(),
        )],
    )
    .await;

    let completed_prefix = [
        compaction_base(),
        vec![prepared.clone(), attempted.clone(), completed.clone()],
    ]
    .concat();
    for replacement in [
        compaction_checkpointed(
            "compaction-replaced",
            "epoch-1",
            1,
            2,
            0,
            opaque_state.clone(),
        ),
        compaction_checkpointed(
            "compaction-1",
            "epoch-replaced",
            1,
            2,
            0,
            opaque_state.clone(),
        ),
        compaction_checkpointed("compaction-1", "epoch-1", 2, 3, 0, opaque_state.clone()),
        compaction_checkpointed("compaction-1", "epoch-1", 1, 2, 1, opaque_state.clone()),
        compaction_checkpointed_with_policy(
            "compaction-1",
            "epoch-1",
            1,
            2,
            0,
            "replacement-policy-v2",
            opaque_state.clone(),
        ),
        compaction_checkpointed(
            "compaction-1",
            "epoch-1",
            1,
            2,
            0,
            Some(json!({"summary": "replaced"})),
        ),
    ] {
        assert_rejected_atomically(completed_prefix.clone(), vec![replacement]).await;
    }

    let checkpointed_prefix = [
        completed_prefix.clone(),
        vec![compaction_checkpointed(
            "compaction-1",
            "epoch-1",
            1,
            2,
            0,
            opaque_state.clone(),
        )],
    ]
    .concat();
    assert_rejected_atomically(
        checkpointed_prefix.clone(),
        vec![compaction_checkpointed(
            "compaction-1",
            "epoch-1",
            1,
            2,
            0,
            opaque_state.clone(),
        )],
    )
    .await;
    assert_rejected_atomically(
        checkpointed_prefix,
        vec![compaction_prepared("compaction-1", "epoch-1", 2, 0)],
    )
    .await;

    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-compaction-sequence").unwrap();
    let mut tail = store
        .append(
            &session,
            LogRevision::ZERO,
            RecordBatch::new([compaction_base(), vec![prepared]].concat()).unwrap(),
        )
        .await
        .unwrap()
        .tail();
    for record in [
        attempted,
        completed,
        compaction_checkpointed("compaction-1", "epoch-1", 1, 2, 0, opaque_state),
    ] {
        tail = store
            .append(&session, tail, RecordBatch::new([record]).unwrap())
            .await
            .unwrap()
            .tail();
    }
    assert_eq!(tail, LogRevision::new(6));
}

#[test]
fn provider_record_validation_fails_closed_at_the_new_record_boundary() {
    let invalid = [
        SessionRecord::ProviderEpochOpened {
            provider_epoch_id: "epoch-1".to_owned(),
            provider: "openai".to_owned(),
            profile: "codex-http-v1".to_owned(),
            profile_version: 0,
            provider_binding: "test-openai-account".to_owned(),
            artifact_mode: ProviderArtifactMode::Stateless,
            base_transcript_revision: TranscriptRevision::new(0),
        },
        SessionRecord::ProviderWindowCheckpointed {
            provider_epoch_id: "epoch-1".to_owned(),
            window_revision: 0,
            transcript_revision: TranscriptRevision::new(0),
            history_policy_version: "full-history-v1".to_owned(),
            opaque_state: None,
        },
        SessionRecord::ProviderOperationPrepared {
            operation_id: "operation-1".to_owned(),
            attempt_id: "attempt-1".to_owned(),
            provider_epoch_id: "epoch-1".to_owned(),
            window_revision: 1,
            transcript_revision: TranscriptRevision::new(0),
            user_diff_cursor: UserDiffCursor::ZERO,
            encoder_version: "codex-http-v1".to_owned(),
            request_sha256: "a".repeat(64),
            request_body: b"not-empty".to_vec(),
        },
        SessionRecord::ProviderOperationAdvanced {
            operation_id: "operation-1".to_owned(),
            operation_revision: 0,
            state: ProviderOperationState::Failed,
        },
    ];

    for record in invalid {
        assert!(NewRecord::new(1, record).is_err());
    }
}

#[test]
fn transcript_reconstruction_rejects_mixed_and_non_contiguous_authority() {
    let first = RecordEnvelope::new(
        SessionId::new("session-a").unwrap(),
        LogRevision::new(1),
        canonical_record(1, "first"),
    )
    .unwrap();
    let other_session = RecordEnvelope::new(
        SessionId::new("session-b").unwrap(),
        LogRevision::new(2),
        canonical_record(2, "second"),
    )
    .unwrap();
    assert!(matches!(
        reconstruct_transcript(&[first.clone(), other_session]),
        Err(TranscriptReconstructionError::MixedSessions)
    ));

    let skipped_log_revision = RecordEnvelope::new(
        SessionId::new("session-a").unwrap(),
        LogRevision::new(2),
        canonical_record(1, "first"),
    )
    .unwrap();
    assert!(matches!(
        reconstruct_transcript(&[skipped_log_revision]),
        Err(TranscriptReconstructionError::NonContiguousLog { .. })
    ));

    let skipped_transcript_revision = RecordEnvelope::new(
        SessionId::new("session-a").unwrap(),
        LogRevision::new(1),
        canonical_record(2, "second"),
    )
    .unwrap();
    assert!(matches!(
        reconstruct_transcript(&[skipped_transcript_revision]),
        Err(TranscriptReconstructionError::NonContiguousTranscript { .. })
    ));
}

#[tokio::test]
async fn reads_beyond_the_authoritative_tail_are_rejected() {
    let store = MemoryRecordStore::new();
    let session = SessionId::new("session-beyond-tail").unwrap();

    assert!(matches!(
        store.read(&session, LogRevision::new(1)).await,
        Err(RecordLogError::RevisionBeyondTail { .. })
    ));
}

#[test]
fn session_id_deserialization_cannot_bypass_identifier_validation() {
    for invalid in [
        json!(""),
        json!("contains spaces"),
        json!("non-ascii-\u{4f1a}\u{8bdd}"),
    ] {
        assert!(serde_json::from_value::<SessionId>(invalid).is_err());
    }
}

#[test]
fn new_record_deserialization_cannot_bypass_constructor_validation() {
    let valid_record = serde_json::to_value(SessionRecord::ProviderEpochClosed {
        provider_epoch_id: "epoch-1".to_owned(),
    })
    .unwrap();

    assert!(serde_json::from_value::<NewRecord>(json!({
        "schema_version": 0,
        "record": valid_record,
    }))
    .is_err());
    assert!(serde_json::from_value::<NewRecord>(json!({
        "schema_version": 1,
        "record": {
            "kind": "provider_epoch_closed",
            "payload": { "provider_epoch_id": "invalid epoch" }
        }
    }))
    .is_err());
}

#[test]
fn new_record_deserialization_validates_nested_canonical_items() {
    let invalid_tool_call = json!({
        "schema_version": 1,
        "record": {
            "kind": "canonical_item_appended",
            "payload": {
                "transcript_revision": 1,
                "item": {
                    "kind": "tool_call",
                    "payload": {
                        "call_id": "call-1",
                        "name": "lookup",
                        "raw_arguments": "[]"
                    }
                }
            }
        }
    });
    let invalid_provider_extension = json!({
        "schema_version": 1,
        "record": {
            "kind": "canonical_item_appended",
            "payload": {
                "transcript_revision": 1,
                "item": {
                    "kind": "provider_extension",
                    "payload": {
                        "provider": "openai",
                        "capability": "reasoning.encrypted_content",
                        "schema_version": 0,
                        "payload": {}
                    }
                }
            }
        }
    });

    assert!(serde_json::from_value::<NewRecord>(invalid_tool_call).is_err());
    assert!(serde_json::from_value::<NewRecord>(invalid_provider_extension).is_err());
}

#[test]
fn record_envelope_constructor_and_deserialization_validate_durable_fields() {
    let error = RecordEnvelope::new(
        SessionId::new("session-envelope").unwrap(),
        LogRevision::ZERO,
        canonical_record(1, "invalid revision"),
    )
    .expect_err("persisted envelopes cannot use the initial-tail sentinel");
    assert!(matches!(
        error,
        RecordLogError::ZeroRevision {
            kind: "record log revision"
        }
    ));

    let valid_record = serde_json::to_value(SessionRecord::ProviderEpochClosed {
        provider_epoch_id: "epoch-1".to_owned(),
    })
    .unwrap();
    assert!(serde_json::from_value::<RecordEnvelope>(json!({
        "session_id": "invalid session",
        "revision": 1,
        "schema_version": 1,
        "record": valid_record,
    }))
    .is_err());
    assert!(serde_json::from_value::<RecordEnvelope>(json!({
        "session_id": "session-envelope",
        "revision": 0,
        "schema_version": 1,
        "record": {
            "kind": "provider_epoch_closed",
            "payload": { "provider_epoch_id": "epoch-1" }
        }
    }))
    .is_err());
    assert!(serde_json::from_value::<RecordEnvelope>(json!({
        "session_id": "session-envelope",
        "revision": 1,
        "schema_version": 0,
        "record": {
            "kind": "provider_epoch_closed",
            "payload": { "provider_epoch_id": "epoch-1" }
        }
    }))
    .is_err());
}

struct ExternalRecordLog {
    records: Vec<RecordEnvelope>,
}

#[async_trait::async_trait]
impl RecordLog for ExternalRecordLog {
    type Error = RecordLogError;

    async fn read(
        &self,
        session_id: &SessionId,
        after_revision: agentview::record_store::LogRevision,
    ) -> Result<Vec<RecordEnvelope>, Self::Error> {
        Ok(self
            .records
            .iter()
            .filter(|record| {
                record.session_id() == session_id && record.revision() > after_revision
            })
            .cloned()
            .collect())
    }

    async fn append(
        &self,
        session_id: &SessionId,
        expected_tail: agentview::record_store::LogRevision,
        batch: RecordBatch,
    ) -> Result<AppendOutcome, Self::Error> {
        let actual_tail = LogRevision::new(u64::try_from(self.records.len()).unwrap());
        if actual_tail != expected_tail {
            return Ok(AppendOutcome::Conflict { actual_tail });
        }
        validate_record_append(session_id, &self.records, &batch)?;
        let appended = u64::try_from(batch.into_records().len()).unwrap();
        Ok(AppendOutcome::committed(expected_tail.get() + appended))
    }
}

#[tokio::test]
async fn external_backend_can_implement_the_record_log_contract() {
    let session = SessionId::new("external-session").unwrap();
    let external = ExternalRecordLog {
        records: vec![RecordEnvelope::new(
            session.clone(),
            LogRevision::new(1),
            canonical_record(1, "external"),
        )
        .unwrap()],
    };
    let records = external
        .read(&session, RecordEnvelope::INITIAL_TAIL)
        .await
        .unwrap();
    assert_eq!(records[0].session_id(), &session);
    assert_eq!(records[0].revision().get(), 1);
    assert_eq!(
        external
            .append(
                &session,
                LogRevision::new(1),
                RecordBatch::new([canonical_record(2, "next")]).unwrap(),
            )
            .await
            .unwrap(),
        AppendOutcome::committed(2)
    );
}
