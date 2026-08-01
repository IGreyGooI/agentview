//! Durable publication proof for a mounted streaming component.
//!
//! Commit values are encoded before publication. One store transaction writes
//! the prepared session mutation and ordered outbox rows; the request future
//! never receives the original Commit values after publication.

use std::{convert::Infallible, fmt::Write as _, sync::Mutex};

use agentview::{
    component::{
        advanced::{
            lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
            persistence::*,
            provider::ProviderToolResponse,
        },
        *,
    },
    prelude::*,
};
use serde_json::{json, Value};

struct DemoChannels;

impl TurnChannels for DemoChannels {
    type Output = u32;
    type Live = Never;
    type Commit = SelectionCommit;
    type Diagnostic = String;
}

#[derive(Debug, PartialEq, Eq)]
enum SelectionCommit {
    Record(u32),
}

#[derive(Default)]
struct SelectionState {
    selected: Option<u32>,
}

fn system_root(_cx: SystemMountContext<'_, ()>) -> SystemView<DemoChannels> {
    let mut contract = XmlNode::new(XmlName::try_from("select").unwrap());
    contract
        .push_attribute(XmlName::try_from("index").unwrap(), "...")
        .unwrap();

    system_view(component((
        Document::from_xml(XmlNode::new(XmlName::try_from("selection_policy").unwrap())),
        StreamingXml::<TurnEmission<DemoChannels>, String>::new(contract)
            .state_with(SelectionState::default)
            .on_open(|state, element| {
                let Some(index) = element
                    .attr("index")
                    .and_then(|value| value.parse::<u32>().ok())
                else {
                    return StreamUpdate::from_diagnostic("invalid selection index".to_owned());
                };
                state.selected = Some(index);
                StreamUpdate::default()
            })
            .on_complete(|state, _| match state.selected {
                Some(index) => StreamUpdate::from_emission(TurnEmission::Output(index)),
                None => StreamUpdate::from_diagnostic("selection was not opened".to_owned()),
            })
            .on_finish(|state| match state.selected {
                Some(index) => StreamUpdate::from_emission(TurnEmission::Commit(
                    SelectionCommit::Record(index),
                )),
                None => StreamUpdate::default(),
            })
            .into_component(),
    )))
}

struct NoDemoLive;

#[async_trait::async_trait]
impl LiveEffectRuntime<Never> for NoDemoLive {
    type Error = Infallible;

    async fn apply(
        &mut self,
        _context: &LiveEffectContext,
        effect: Never,
    ) -> Result<(), Self::Error> {
        effect.absurd()
    }

    async fn abort(
        &mut self,
        context: &LiveAbortContext,
    ) -> Result<LiveEffectAbortAck, Self::Error> {
        Ok(if context.applied_effects() == 0 {
            LiveEffectAbortAck::NoEffectsApplied
        } else {
            LiveEffectAbortAck::CompensationCompleted
        })
    }
}

struct SelectionCommitStager;

impl CommitStager<DemoChannels> for SelectionCommitStager {
    type Payload = Value;
    type Error = Infallible;

    fn stage(
        &self,
        context: CommitStagingContext<'_>,
        commit: &SelectionCommit,
    ) -> Result<StagedCommit<Self::Payload>, Self::Error> {
        let SelectionCommit::Record(index) = commit;
        Ok(StagedCommit::new(
            CommitContract::new("forgotten-city.selection-recorded", 1).unwrap(),
            json!({
                "item_id": context.item_id().to_string(),
                "index": index,
            }),
        ))
    }
}

fn append_canonical_field(target: &mut String, name: &str, value: &str) {
    // The host owns this versioned encoding. Hex keeps arbitrary text out of
    // the durable-key alphabet while making field boundaries unambiguous.
    write!(target, "{name}:{}:", value.len()).unwrap();
    for byte in value.bytes() {
        write!(target, "{byte:02x}").unwrap();
    }
    target.push(';');
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NextSession {
    transcript_items: usize,
    user_cursor_generation: u64,
}

#[derive(Debug, thiserror::Error)]
enum SelectionFingerprintError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    DurableKey(#[from] DurableKeyError),
}

struct SelectionFingerprintFactory;

impl PublicationFingerprintFactory<NextSession, Value, u64> for SelectionFingerprintFactory {
    type Error = SelectionFingerprintError;

    fn fingerprint(
        &self,
        context: PublicationFingerprintContext<'_, NextSession, Value, u64>,
    ) -> Result<PublicationCandidateFingerprint, Self::Error> {
        let mut canonical = "agentview.selection-publication.v1;".to_owned();
        append_canonical_field(&mut canonical, "request_id", context.request_id().as_str());
        append_canonical_field(
            &mut canonical,
            "expected_revision",
            &context.expected_revision().to_string(),
        );
        append_canonical_field(
            &mut canonical,
            "mutation.transcript_items",
            &context.mutation().value().transcript_items.to_string(),
        );
        append_canonical_field(
            &mut canonical,
            "mutation.user_cursor_generation",
            &context
                .mutation()
                .value()
                .user_cursor_generation
                .to_string(),
        );
        append_canonical_field(&mut canonical, "raw_output", context.raw_output());
        append_canonical_field(
            &mut canonical,
            "provider_results.count",
            &context.provider_results().len().to_string(),
        );
        for (index, result) in context.provider_results().iter().enumerate() {
            let prefix = format!("provider_results[{index}]");
            append_canonical_field(
                &mut canonical,
                &format!("{prefix}.invocation_id"),
                result.invocation_id().unwrap_or(""),
            );
            append_canonical_field(
                &mut canonical,
                &format!("{prefix}.correlation_id"),
                result.result_correlation_id().unwrap_or(""),
            );
            append_canonical_field(&mut canonical, &format!("{prefix}.name"), result.name());
            match result.response() {
                ProviderToolResponse::Success { content } => {
                    append_canonical_field(
                        &mut canonical,
                        &format!("{prefix}.response.kind"),
                        "success",
                    );
                    append_canonical_field(
                        &mut canonical,
                        &format!("{prefix}.response.content"),
                        &serde_json::to_string(content)?,
                    );
                }
                ProviderToolResponse::Error {
                    code,
                    message,
                    details,
                } => {
                    append_canonical_field(
                        &mut canonical,
                        &format!("{prefix}.response.kind"),
                        "error",
                    );
                    append_canonical_field(
                        &mut canonical,
                        &format!("{prefix}.response.code"),
                        code,
                    );
                    append_canonical_field(
                        &mut canonical,
                        &format!("{prefix}.response.message"),
                        message,
                    );
                    append_canonical_field(
                        &mut canonical,
                        &format!("{prefix}.response.details"),
                        &serde_json::to_string(details)?,
                    );
                }
            }
        }
        append_canonical_field(
            &mut canonical,
            "outbox.count",
            &context.outbox().len().to_string(),
        );
        for item in context.outbox().items() {
            let prefix = format!("outbox[{}]", item.id().index());
            append_canonical_field(
                &mut canonical,
                &format!("{prefix}.id"),
                &item.id().to_string(),
            );
            append_canonical_field(
                &mut canonical,
                &format!("{prefix}.contract.key"),
                item.contract().key(),
            );
            append_canonical_field(
                &mut canonical,
                &format!("{prefix}.contract.schema_version"),
                &item.contract().schema_version().to_string(),
            );
            append_canonical_field(
                &mut canonical,
                &format!("{prefix}.payload"),
                &serde_json::to_string(item.payload())?,
            );
        }
        Ok(PublicationCandidateFingerprint::new(canonical)?)
    }
}

#[derive(Debug, thiserror::Error)]
enum StoreError {
    #[error("expected session revision {expected}, but current revision is {actual}")]
    RevisionConflict { expected: u64, actual: u64 },

    #[error("publication request id was reused with a different candidate fingerprint")]
    FingerprintCollision,
}

#[derive(Debug)]
struct StoredOutboxItem {
    id: OutboxItemId,
    contract: CommitContract,
    payload: Value,
}

#[derive(Debug, Default)]
struct StoreState {
    revision: u64,
    session: Option<NextSession>,
    publications: Vec<PublicationReceipt<u64>>,
    outbox: Vec<StoredOutboxItem>,
}

#[derive(Debug, Default)]
struct MemoryPublicationStore {
    state: Mutex<StoreState>,
}

#[async_trait::async_trait]
impl PublicationStore<NextSession, Value> for MemoryPublicationStore {
    type Version = u64;
    type Error = StoreError;

    async fn publish(
        &self,
        request: PublicationRequest<'_, NextSession, Value, u64>,
    ) -> Result<PublicationReceipt<u64>, PublicationWriteError<Self::Error>> {
        let mut state = self.state.lock().unwrap();
        if let Some(receipt) = state
            .publications
            .iter()
            .find(|receipt| receipt.request_id() == request.request_id())
        {
            return if receipt.fingerprint().as_bytes() == request.fingerprint().as_bytes() {
                Ok(receipt.clone())
            } else {
                Err(PublicationWriteError::Rejected(
                    StoreError::FingerprintCollision,
                ))
            };
        }
        if state.revision != *request.expected_revision() {
            return Err(PublicationWriteError::Conflict(
                StoreError::RevisionConflict {
                    expected: *request.expected_revision(),
                    actual: state.revision,
                },
            ));
        }

        let next_revision = state.revision + 1;
        let receipt = PublicationReceipt::new(
            request.request_id().clone(),
            request.fingerprint().clone(),
            PublicationId::new(format!("session-player-7/revision-{next_revision}")).unwrap(),
            next_revision,
            u64::try_from(request.outbox().len()).unwrap(),
        );

        // This lock stands in for one database transaction: neither the next
        // session nor any outbox row becomes visible independently.
        state.session = Some(request.mutation().value().clone());
        state.outbox.extend(
            request
                .outbox()
                .items()
                .iter()
                .map(|item| StoredOutboxItem {
                    id: item.id().clone(),
                    contract: item.contract().clone(),
                    payload: item.payload().clone(),
                }),
        );
        state.revision = next_revision;
        state.publications.push(receipt.clone());
        Ok(receipt)
    }

    async fn resolve(
        &self,
        request_id: &PublicationRequestId,
        fingerprint: &PublicationCandidateFingerprint,
    ) -> Result<PublicationResolution<u64>, PublicationResolveError<Self::Error>> {
        match self
            .state
            .lock()
            .unwrap()
            .publications
            .iter()
            .find(|receipt| receipt.request_id() == request_id)
        {
            Some(receipt) if receipt.fingerprint().as_bytes() == fingerprint.as_bytes() => {
                Ok(PublicationResolution::Published(receipt.clone()))
            }
            Some(_) => Err(PublicationResolveError::CandidateCollision(
                StoreError::FingerprintCollision,
            )),
            None => Ok(PublicationResolution::NotCommitted),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let epoch = mount_system_epoch(&(), system_root)?;
    let turn = epoch.begin_turn("record-selection");
    let prepared = turn.prepare_user(&(), |_| {
        user_view(Document::from_xml(XmlNode::new(
            XmlName::try_from("choose_an_option").unwrap(),
        )))
    })?;

    let mut attempt = prepared.start_streaming_attempt(NoDemoLive)?;
    let update = attempt
        .on_event(TextTurnEvent::TextDelta(
            "<select index=\"4\" />".to_owned(),
        ))
        .await?;
    assert_eq!(update.emissions(), &[TurnEmission::Output(4)]);
    let finished = match attempt.finish_stream().await {
        Ok(finished) => finished,
        Err(failure) => {
            let message = format!(
                "stream finish failed before publication: {}",
                failure.error()
            );
            let _ = failure.abort(BindingAbortReason::ParserFailure).await;
            return Err(anyhow::anyhow!(message));
        }
    };

    let request_id = PublicationRequestId::new("session-player-7/turn-19")?;
    let mutation = NextSession {
        transcript_items: 2,
        user_cursor_generation: 1,
    };
    let staged = finished.stage_durable_publication(
        request_id,
        0_u64,
        PreparedSessionMutation::new(mutation),
        &SelectionCommitStager,
        &SelectionFingerprintFactory,
    );
    let mut publication = match staged {
        Ok(publication) => publication,
        Err(failure) => {
            let message = failure.to_string();
            let _ = failure
                .abort(BindingAbortReason::HostInterpretationFailure)
                .await;
            return Err(anyhow::anyhow!(message));
        }
    };

    let store = MemoryPublicationStore::default();
    let receipt = publication.publish(&store).await?;
    assert_eq!(receipt.outbox_count(), 1);
    let published = publication
        .into_published()
        .await
        .expect("the store returned a durable receipt");

    let state = store.state.lock().unwrap();
    let outbox = &state.outbox[0];
    println!("publication={}", published.receipt().publication_id());
    println!(
        "session-revision={}",
        published.receipt().session_revision()
    );
    println!("outbox-item={}", outbox.id);
    println!(
        "contract={}@{}",
        outbox.contract.key(),
        outbox.contract.schema_version()
    );
    println!("payload={}", outbox.payload);
    Ok(())
}
