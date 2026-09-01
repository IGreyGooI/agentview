#![allow(dead_code)]

//! Private canonical admission for one structured reaction.
//!
//! This module deliberately stays below the public reaction protocol. It
//! validates each provider fact, commits a cancellation-safe canonical
//! transcript candidate, and only then releases the corresponding Component
//! event or tool-lane ticket.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

#[cfg(test)]
use std::cell::Cell;

use crate::{
    llm_call::TextTurnEvent,
    transcript::{
        AssistantPhase, AssistantTextStatus, CanonicalInputItem, CanonicalTranscript,
        CanonicalTranscriptError,
    },
};

use super::{
    frame::{FrameBudgetFault, FullReserveBudget, FullReserveCandidate, FullReserveTracker},
    port::{ProviderEvent, ToolCall, ToolOutput},
    reaction::{ProviderFact, ProviderOutputKey, ProviderToolCall},
};

#[cfg(test)]
thread_local! {
    static ADMISSION_CONSTRUCTION_WORK: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn record_admission_work(additional: usize) {
    ADMISSION_CONSTRUCTION_WORK.with(|work| work.set(work.get().saturating_add(additional)));
}

#[cfg(not(test))]
fn record_admission_work(_additional: usize) {}

#[cfg(test)]
fn reset_admission_work() {
    ADMISSION_CONSTRUCTION_WORK.with(|work| work.set(0));
}

#[cfg(test)]
fn take_admission_work() -> usize {
    ADMISSION_CONSTRUCTION_WORK.with(|work| work.replace(0))
}

/// The Component-visible projection of one committed provider fact.
///
/// A ToolCall fact produces both values: the event starts the handler and the
/// ticket later authorizes exactly one result for that lane.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct AdmittedProviderFact {
    event: Option<ProviderEvent>,
    tool_lane: Option<ToolLaneTicket>,
}

impl AdmittedProviderFact {
    pub(super) fn into_parts(self) -> (Option<ProviderEvent>, Option<ToolLaneTicket>) {
        (self.event, self.tool_lane)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextLifecycle {
    Open,
    Sealed,
}

#[derive(Debug, PartialEq, Eq)]
enum AdmittedOutput {
    Text {
        phase: Option<AssistantPhase>,
        text: String,
        lifecycle: TextLifecycle,
        budget_item_bytes: Option<usize>,
        canonical_index: usize,
    },
    Tool {
        ordinal: u64,
        call: ProviderToolCall,
        budget_item_bytes: Option<usize>,
    },
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AdmissionState {
    output_order: Vec<ProviderOutputKey>,
    outputs: HashMap<ProviderOutputKey, AdmittedOutput>,
    tool_ordinals: HashSet<u64>,
    last_tool_ordinal: Option<u64>,
    known_tool_call_ids: HashSet<String>,
    non_commentary_text: Option<ProviderOutputKey>,
    terminal: Option<Option<ProviderOutputKey>>,
}

/// A move-only authority to publish one result for one admitted ToolCall.
///
/// The ticket is intentionally separate from the Component-facing ToolCall.
/// It is retained by the runtime lane and consumed when that lane completes.
#[derive(Debug)]
pub(super) struct ToolLaneTicket {
    owner: Arc<()>,
    registration: u64,
    ordinal: u64,
    call_id: String,
}

impl PartialEq for ToolLaneTicket {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.registration == other.registration
            && self.ordinal == other.ordinal
            && self.call_id == other.call_id
    }
}

impl Eq for ToolLaneTicket {}

#[derive(Debug, Clone)]
struct ToolOutputSlot {
    registration: u64,
    ordinal: u64,
    call_id: String,
    output: Option<CanonicalInputItem>,
}

/// Session-owned ToolOutput table. Results iterate in canonical call-admission
/// order (and therefore provider ordinal order within each reaction),
/// regardless of the order in which concurrent lanes finish.
#[derive(Debug, Clone, Default)]
pub(super) struct ToolOutputStaging {
    owner: Arc<()>,
    next_registration: u64,
    slots: BTreeMap<u64, ToolOutputSlot>,
}

impl ToolOutputStaging {
    fn register(
        &mut self,
        ordinal: u64,
        call_id: &str,
    ) -> Result<ToolLaneTicket, ToolOutputStagingFault> {
        let registration = self
            .next_registration
            .checked_add(1)
            .ok_or(ToolOutputStagingFault::RegistrationIdentityExhausted)?;
        self.next_registration = registration;
        self.slots.insert(
            registration,
            ToolOutputSlot {
                registration,
                ordinal,
                call_id: call_id.to_owned(),
                output: None,
            },
        );
        Ok(ToolLaneTicket {
            owner: Arc::clone(&self.owner),
            registration,
            ordinal,
            call_id: call_id.to_owned(),
        })
    }

    pub(super) fn stage(
        &mut self,
        ticket: ToolLaneTicket,
        output: ToolOutput,
    ) -> Result<(), ToolOutputStagingFault> {
        let item = self.prepare_stage(&ticket, &output)?;
        self.commit_stage(ticket, item);
        Ok(())
    }

    fn prepare_stage(
        &self,
        ticket: &ToolLaneTicket,
        output: &ToolOutput,
    ) -> Result<CanonicalInputItem, ToolOutputStagingFault> {
        if !Arc::ptr_eq(&self.owner, &ticket.owner) {
            return Err(ToolOutputStagingFault::ForeignTicket {
                ordinal: ticket.ordinal,
            });
        }
        let Some(slot) = self.slots.get(&ticket.registration) else {
            return Err(ToolOutputStagingFault::UnknownTicket {
                ordinal: ticket.ordinal,
            });
        };
        if slot.registration != ticket.registration
            || slot.ordinal != ticket.ordinal
            || slot.call_id != ticket.call_id
        {
            return Err(ToolOutputStagingFault::UnknownTicket {
                ordinal: ticket.ordinal,
            });
        }
        if output.call_id() != slot.call_id {
            return Err(ToolOutputStagingFault::CallMismatch {
                ordinal: slot.ordinal,
                expected_call_id_len: slot.call_id.len(),
                observed_call_id_len: output.call_id().len(),
            });
        }
        if slot.output.is_some() {
            return Err(ToolOutputStagingFault::DuplicateOutput {
                ordinal: slot.ordinal,
            });
        }

        CanonicalInputItem::tool_result(output.call_id(), output.content())
            .map_err(sanitize_staging_canonical_fault)
    }

    fn commit_stage(&mut self, ticket: ToolLaneTicket, item: CanonicalInputItem) {
        let slot = self
            .slots
            .get_mut(&ticket.registration)
            .expect("a prepared ToolOutput registration remains present until commit");
        debug_assert!(slot.output.is_none());
        slot.output = Some(item);
    }

    pub(super) fn ordered_outputs(&self) -> impl Iterator<Item = (u64, &CanonicalInputItem)> + '_ {
        self.slots
            .values()
            .filter_map(|slot| slot.output.as_ref().map(|output| (slot.ordinal, output)))
    }

    /// Build the exact staged input segment and the post-handoff staging
    /// candidate without mutating this retryable owner.
    pub(super) fn prepare_frame_candidate(
        &self,
    ) -> Result<(Vec<CanonicalInputItem>, Self), ToolOutputStagingFault> {
        let mut outputs = Vec::with_capacity(self.slots.len());
        for slot in self.slots.values() {
            let Some(output) = slot.output.as_ref() else {
                return Err(ToolOutputStagingFault::UnresolvedOutput {
                    ordinal: slot.ordinal,
                });
            };
            outputs.push(output.clone());
        }
        let mut candidate = self.clone();
        candidate.slots.clear();
        Ok((outputs, candidate))
    }

    fn ensure_resolved(&self, registrations: &[u64]) -> Result<(), ToolOutputStagingFault> {
        for registration in registrations {
            let Some(slot) = self.slots.get(registration) else {
                return Err(ToolOutputStagingFault::UnknownRegistration {
                    registration: *registration,
                });
            };
            if slot.output.is_none() {
                return Err(ToolOutputStagingFault::UnresolvedOutput {
                    ordinal: slot.ordinal,
                });
            }
        }
        Ok(())
    }

    /// Freeze all currently staged outputs into an exact retryable receipt.
    /// Dropping the receipt leaves staging untouched; committing it consumes
    /// exactly the registrations captured here.
    pub(super) fn prepare_receipt(
        &mut self,
    ) -> Result<ToolOutputReceipt<'_>, ToolOutputStagingFault> {
        let mut registrations = Vec::with_capacity(self.slots.len());
        let mut outputs = Vec::with_capacity(self.slots.len());
        for (registration, slot) in &self.slots {
            let Some(output) = slot.output.as_ref() else {
                return Err(ToolOutputStagingFault::UnresolvedOutput {
                    ordinal: slot.ordinal,
                });
            };
            registrations.push(*registration);
            outputs.push(output.clone());
        }
        Ok(ToolOutputReceipt {
            staging: self,
            registrations,
            outputs,
        })
    }
}

/// Exact move-only ToolOutput set held across Frame preparation and submit.
pub(super) struct ToolOutputReceipt<'a> {
    staging: &'a mut ToolOutputStaging,
    registrations: Vec<u64>,
    outputs: Vec<CanonicalInputItem>,
}

impl ToolOutputReceipt<'_> {
    pub(super) fn outputs(&self) -> &[CanonicalInputItem] {
        &self.outputs
    }

    /// Consume the exact staged set after successful Frame handoff.
    ///
    /// The exclusive staging borrow makes every removal infallible: no lane can
    /// replace or remove a captured registration while this receipt exists.
    pub(super) fn commit(self) {
        for registration in self.registrations {
            let _ = self.staging.slots.remove(&registration);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum ToolOutputStagingFault {
    #[error("tool output ticket for ordinal {ordinal} belongs to another staging owner")]
    ForeignTicket { ordinal: u64 },
    #[error("tool output lane ordinal {ordinal} was never registered")]
    UnknownTicket { ordinal: u64 },
    #[error("tool output registration {registration} is unknown")]
    UnknownRegistration { registration: u64 },
    #[error(
        "tool output ordinal {ordinal} call identity mismatch (expected length {expected_call_id_len}, observed length {observed_call_id_len})"
    )]
    CallMismatch {
        ordinal: u64,
        expected_call_id_len: usize,
        observed_call_id_len: usize,
    },
    #[error("tool output ordinal {ordinal} was already staged")]
    DuplicateOutput { ordinal: u64 },
    #[error("tool output ordinal {ordinal} has not completed")]
    UnresolvedOutput { ordinal: u64 },
    #[error("tool output registration identity space is exhausted")]
    RegistrationIdentityExhausted,
    #[error("canonical tool output invariant failed: {reason:?}")]
    CanonicalInvariant { reason: CanonicalAdmissionReason },
    #[error("tool output exceeds the canonical next-Full budget: {reason:?}")]
    Budget { reason: AdmissionBudgetReason },
}

impl ToolOutputStagingFault {
    pub(super) const fn reason(&self) -> ToolOutputStagingReason {
        match self {
            Self::ForeignTicket { .. } => ToolOutputStagingReason::ForeignTicket,
            Self::UnknownTicket { .. } => ToolOutputStagingReason::UnknownTicket,
            Self::UnknownRegistration { .. } => ToolOutputStagingReason::UnknownRegistration,
            Self::CallMismatch { .. } => ToolOutputStagingReason::CallMismatch,
            Self::DuplicateOutput { .. } => ToolOutputStagingReason::DuplicateOutput,
            Self::UnresolvedOutput { .. } => ToolOutputStagingReason::UnresolvedOutput,
            Self::RegistrationIdentityExhausted => {
                ToolOutputStagingReason::RegistrationIdentityExhausted
            }
            Self::CanonicalInvariant { .. } => ToolOutputStagingReason::CanonicalInvariant,
            Self::Budget { .. } => ToolOutputStagingReason::Budget,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolOutputStagingReason {
    ForeignTicket,
    UnknownTicket,
    UnknownRegistration,
    CallMismatch,
    DuplicateOutput,
    UnresolvedOutput,
    RegistrationIdentityExhausted,
    CanonicalInvariant,
    Budget,
}

/// Transactional canonical state for one handed-off ProviderFact stream.
///
/// While the guard is alive, it exclusively owns the complete already
/// validated canonical sequence and the borrowed transcript is temporarily
/// empty and unobservable. Every event is released only after its mutation is
/// committed to that sequence; normal completion or Drop restores it exactly
/// once without another fallible validation pass.
pub(super) struct ReactionAdmissionGuard<'a> {
    transcript: &'a mut CanonicalTranscript,
    canonical_items: Vec<CanonicalInputItem>,
    staging: &'a mut ToolOutputStaging,
    budget: Option<FullReserveTracker>,
    state: AdmissionState,
    tool_registrations: Vec<u64>,
    finished: bool,
}

impl<'a> ReactionAdmissionGuard<'a> {
    pub(super) fn new(
        transcript: &'a mut CanonicalTranscript,
        staging: &'a mut ToolOutputStaging,
    ) -> Self {
        Self::build(transcript, staging, None)
    }

    pub(super) fn with_budget(
        transcript: &'a mut CanonicalTranscript,
        staging: &'a mut ToolOutputStaging,
        budget: FullReserveBudget,
    ) -> Result<Self, ReactionAdmissionFault> {
        let tracker = budget
            .begin(transcript, staging)
            .map_err(sanitize_budget_fault)?;
        Ok(Self::build(transcript, staging, Some(tracker)))
    }

    fn build(
        transcript: &'a mut CanonicalTranscript,
        staging: &'a mut ToolOutputStaging,
        budget: Option<FullReserveTracker>,
    ) -> Self {
        let mut known_tool_call_ids = HashSet::new();
        for item in transcript.items() {
            record_admission_work(1);
            if let CanonicalInputItem::ToolCall { call_id, .. } = item {
                known_tool_call_ids.insert(call_id.clone());
            }
        }
        let canonical_items = transcript.take_validated_items();
        Self {
            transcript,
            canonical_items,
            staging,
            budget,
            state: AdmissionState {
                known_tool_call_ids,
                ..AdmissionState::default()
            },
            tool_registrations: Vec::new(),
            finished: false,
        }
    }

    /// Validate and atomically commit one fact before exposing its event.
    pub(super) fn admit(
        &mut self,
        fact: ProviderFact,
    ) -> Result<AdmittedProviderFact, ReactionAdmissionFault> {
        if self.state.terminal.is_some() {
            return Err(match fact {
                ProviderFact::ReactionCompleted { .. } => {
                    ReactionAdmissionFault::DuplicateCompletion
                }
                _ => ReactionAdmissionFault::FactAfterCompletion,
            });
        }

        let (event, tool_lane) = match fact {
            ProviderFact::TextDelta {
                output,
                phase,
                delta,
            } => {
                self.admit_text_delta(output, phase, &delta)?;
                (
                    Some(ProviderEvent::Text(TextTurnEvent::TextDelta(delta))),
                    None,
                )
            }
            ProviderFact::TextSealed {
                output,
                phase,
                text,
            } => {
                self.admit_text_sealed(output, phase, text)?;
                (None, None)
            }
            ProviderFact::ToolCall {
                output,
                ordinal,
                call,
            } => {
                Self::validate_tool_call(&self.state, output, ordinal, &call)?;
                let component_call =
                    ToolCall::new(call.call_id(), call.name(), call.raw_arguments())
                        .map_err(sanitize_reaction_canonical_fault)?;
                let canonical_call = CanonicalInputItem::tool_call(
                    call.call_id(),
                    call.name(),
                    call.raw_arguments(),
                )
                .map_err(sanitize_reaction_canonical_fault)?;
                let budget_candidate = self.prepare_replay_budget(None, &canonical_call)?;
                let ticket = self.staging.register(ordinal, call.call_id())?;
                let budget_item_bytes = budget_candidate.map(FullReserveCandidate::item_bytes);
                self.commit_budget(budget_candidate);
                Self::commit_tool_call(&mut self.state, output, ordinal, call, budget_item_bytes);
                self.canonical_items.push(canonical_call);
                self.tool_registrations.push(ticket.registration);
                (Some(ProviderEvent::ToolCall(component_call)), Some(ticket))
            }
            ProviderFact::ReactionCompleted { primary_text } => {
                let complete = Self::validate_completion(&self.state, primary_text)?;
                self.state.terminal = Some(primary_text);
                (
                    complete.map(|text| ProviderEvent::Text(TextTurnEvent::TextComplete(text))),
                    None,
                )
            }
        };

        // Every fallible check precedes the state changes above. Constructing
        // this wrapper is infallible, so a returned event proves its canonical
        // incremental tail is already authoritative.
        Ok(AdmittedProviderFact { event, tool_lane })
    }

    pub(super) fn stage_tool_output(
        &mut self,
        ticket: ToolLaneTicket,
        output: ToolOutput,
    ) -> Result<(), ToolOutputStagingFault> {
        let item = self.staging.prepare_stage(&ticket, &output)?;
        let budget_candidate = match self.budget.as_ref() {
            Some(tracker) => {
                let item_bytes = tracker
                    .budget()
                    .canonical_item_bytes(&item)
                    .map_err(sanitize_staging_budget_fault)?;
                Some(
                    tracker
                        .prepare_staged_item(item_bytes)
                        .map_err(sanitize_staging_budget_fault)?,
                )
            }
            None => None,
        };
        self.staging.commit_stage(ticket, item);
        self.commit_budget(budget_candidate);
        Ok(())
    }

    /// Finish a grammatically complete reaction and restore canonical history.
    pub(super) fn finish_normal(mut self) -> Result<(), ReactionAdmissionFault> {
        if self.state.terminal.is_none() {
            return Err(ReactionAdmissionFault::MissingCompletion);
        }
        self.staging.ensure_resolved(&self.tool_registrations)?;

        self.restore_transcript();
        self.finished = true;
        Ok(())
    }

    #[cfg(test)]
    fn committed_text(&self, output: ProviderOutputKey) -> Option<(&str, TextLifecycle)> {
        match self.state.outputs.get(&output) {
            Some(AdmittedOutput::Text {
                text, lifecycle, ..
            }) => Some((text, *lifecycle)),
            _ => None,
        }
    }

    #[cfg(test)]
    fn committed_output_count(&self) -> usize {
        self.state.output_order.len()
    }

    #[cfg(test)]
    fn forget_output_for_test(&mut self, output: ProviderOutputKey) {
        self.state.outputs.remove(&output);
    }

    fn restore_transcript(&mut self) {
        self.transcript
            .restore_validated_items(std::mem::take(&mut self.canonical_items));
    }

    fn prepare_replay_budget(
        &self,
        previous_item_bytes: Option<usize>,
        item: &CanonicalInputItem,
    ) -> Result<Option<FullReserveCandidate>, ReactionAdmissionFault> {
        let Some(tracker) = self.budget.as_ref() else {
            return Ok(None);
        };
        let item_bytes = tracker
            .budget()
            .canonical_item_bytes(item)
            .map_err(sanitize_budget_fault)?;
        tracker
            .prepare_replay_item(previous_item_bytes, item_bytes)
            .map(Some)
            .map_err(sanitize_budget_fault)
    }

    fn commit_budget(&mut self, candidate: Option<FullReserveCandidate>) {
        if let (Some(tracker), Some(candidate)) = (self.budget.as_mut(), candidate) {
            tracker.commit(candidate);
        }
    }

    fn admit_text_delta(
        &mut self,
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        delta: &str,
    ) -> Result<(), ReactionAdmissionFault> {
        record_admission_work(delta.len().saturating_add(1));
        match self.state.outputs.get(&output) {
            Some(AdmittedOutput::Text {
                phase: established,
                text: established_text,
                lifecycle,
                budget_item_bytes,
                canonical_index,
                ..
            }) => {
                if *established != phase {
                    return Err(ReactionAdmissionFault::TextPhaseDrift {
                        output,
                        established: *established,
                        observed: phase,
                    });
                }
                if *lifecycle == TextLifecycle::Sealed {
                    return Err(ReactionAdmissionFault::TextAfterSeal { output });
                }
                let canonical_index = *canonical_index;
                if !matches!(
                    self.canonical_items.get(canonical_index),
                    Some(CanonicalInputItem::AssistantText {
                        text,
                        phase: canonical_phase,
                        status: AssistantTextStatus::Interrupted,
                    }) if text == established_text && *canonical_phase == phase
                ) {
                    return Err(ReactionAdmissionFault::InternalOutputOrder { output });
                }
                let budget_candidate = match self.budget.as_ref() {
                    Some(tracker) => {
                        let previous =
                            budget_item_bytes.ok_or(ReactionAdmissionFault::BudgetStateMismatch)?;
                        let fragment_bytes = tracker
                            .budget()
                            .canonical_string_content_bytes(delta)
                            .map_err(sanitize_budget_fault)?;
                        let item_bytes = previous.checked_add(fragment_bytes).ok_or_else(|| {
                            sanitize_budget_fault(FrameBudgetFault::ArithmeticOverflow)
                        })?;
                        Some(
                            tracker
                                .prepare_replay_item(Some(previous), item_bytes)
                                .map_err(sanitize_budget_fault)?,
                        )
                    }
                    None => None,
                };
                let next_budget_bytes = budget_candidate.map(FullReserveCandidate::item_bytes);
                let Some(CanonicalInputItem::AssistantText { text, .. }) =
                    self.canonical_items.get_mut(canonical_index)
                else {
                    return Err(ReactionAdmissionFault::InternalOutputOrder { output });
                };
                text.push_str(delta);
                let Some(AdmittedOutput::Text {
                    text,
                    budget_item_bytes,
                    ..
                }) = self.state.outputs.get_mut(&output)
                else {
                    unreachable!("validated text output remains present")
                };
                text.push_str(delta);
                *budget_item_bytes = next_budget_bytes;
                self.commit_budget(budget_candidate);
            }
            Some(AdmittedOutput::Tool { .. }) => {
                return Err(ReactionAdmissionFault::OutputKindReuse { output });
            }
            None => {
                if let (false, Some(established)) =
                    (is_commentary(phase), self.state.non_commentary_text)
                {
                    return Err(ReactionAdmissionFault::MultipleNonCommentaryText {
                        established,
                        observed: output,
                    });
                }
                let item = CanonicalInputItem::interrupted_assistant_text(delta, phase);
                let budget_candidate = self.prepare_replay_budget(None, &item)?;
                let budget_item_bytes = budget_candidate.map(FullReserveCandidate::item_bytes);
                Self::register_text_output(&mut self.state, output, phase)?;
                let canonical_index = self.canonical_items.len();
                self.state.outputs.insert(
                    output,
                    AdmittedOutput::Text {
                        phase,
                        text: delta.to_owned(),
                        lifecycle: TextLifecycle::Open,
                        budget_item_bytes,
                        canonical_index,
                    },
                );
                self.canonical_items.push(item);
                self.commit_budget(budget_candidate);
            }
        }
        Ok(())
    }

    fn admit_text_sealed(
        &mut self,
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        sealed_text: String,
    ) -> Result<(), ReactionAdmissionFault> {
        match self.state.outputs.get(&output) {
            Some(AdmittedOutput::Text {
                phase: established,
                text,
                lifecycle,
                budget_item_bytes,
                canonical_index,
            }) => {
                if *established != phase {
                    return Err(ReactionAdmissionFault::TextPhaseDrift {
                        output,
                        established: *established,
                        observed: phase,
                    });
                }
                if *lifecycle == TextLifecycle::Sealed {
                    return Err(ReactionAdmissionFault::DuplicateTextSeal { output });
                }
                record_admission_work(
                    text.len()
                        .saturating_add(sealed_text.len())
                        .saturating_add(1),
                );
                if *text != sealed_text {
                    return Err(ReactionAdmissionFault::TextSealMismatch {
                        output,
                        accumulated_len: text.len(),
                        sealed_len: sealed_text.len(),
                    });
                }
                let canonical_index = *canonical_index;
                if !matches!(
                    self.canonical_items.get(canonical_index),
                    Some(CanonicalInputItem::AssistantText {
                        text: canonical_text,
                        phase: canonical_phase,
                        status: AssistantTextStatus::Interrupted,
                    }) if canonical_text == text && *canonical_phase == phase
                ) {
                    return Err(ReactionAdmissionFault::InternalOutputOrder { output });
                }
                let item = CanonicalInputItem::assistant_text(&sealed_text, phase);
                let previous = match self.budget.as_ref() {
                    Some(_) => {
                        Some(budget_item_bytes.ok_or(ReactionAdmissionFault::BudgetStateMismatch)?)
                    }
                    None => None,
                };
                let budget_candidate = self.prepare_replay_budget(previous, &item)?;
                let next_budget_bytes = budget_candidate.map(FullReserveCandidate::item_bytes);
                let Some(canonical_item) = self.canonical_items.get_mut(canonical_index) else {
                    return Err(ReactionAdmissionFault::InternalOutputOrder { output });
                };
                *canonical_item = item;
                let Some(AdmittedOutput::Text {
                    lifecycle,
                    budget_item_bytes,
                    ..
                }) = self.state.outputs.get_mut(&output)
                else {
                    unreachable!("validated text output remains present")
                };
                *lifecycle = TextLifecycle::Sealed;
                *budget_item_bytes = next_budget_bytes;
                self.commit_budget(budget_candidate);
            }
            Some(AdmittedOutput::Tool { .. }) => {
                return Err(ReactionAdmissionFault::OutputKindReuse { output });
            }
            None => {
                record_admission_work(sealed_text.len().saturating_add(1));
                if let (false, Some(established)) =
                    (is_commentary(phase), self.state.non_commentary_text)
                {
                    return Err(ReactionAdmissionFault::MultipleNonCommentaryText {
                        established,
                        observed: output,
                    });
                }
                let item = CanonicalInputItem::assistant_text(&sealed_text, phase);
                let budget_candidate = self.prepare_replay_budget(None, &item)?;
                let budget_item_bytes = budget_candidate.map(FullReserveCandidate::item_bytes);
                Self::register_text_output(&mut self.state, output, phase)?;
                let canonical_index = self.canonical_items.len();
                self.state.outputs.insert(
                    output,
                    AdmittedOutput::Text {
                        phase,
                        text: sealed_text,
                        lifecycle: TextLifecycle::Sealed,
                        budget_item_bytes,
                        canonical_index,
                    },
                );
                self.canonical_items.push(item);
                self.commit_budget(budget_candidate);
            }
        }
        Ok(())
    }

    fn register_text_output(
        state: &mut AdmissionState,
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
    ) -> Result<(), ReactionAdmissionFault> {
        if !is_commentary(phase) {
            if let Some(established) = state.non_commentary_text {
                return Err(ReactionAdmissionFault::MultipleNonCommentaryText {
                    established,
                    observed: output,
                });
            }
            state.non_commentary_text = Some(output);
        }
        state.output_order.push(output);
        Ok(())
    }

    fn validate_tool_call(
        state: &AdmissionState,
        output: ProviderOutputKey,
        ordinal: u64,
        call: &ProviderToolCall,
    ) -> Result<(), ReactionAdmissionFault> {
        if let Some(existing) = state.outputs.get(&output) {
            return Err(match existing {
                AdmittedOutput::Text { .. } => ReactionAdmissionFault::OutputKindReuse { output },
                AdmittedOutput::Tool { .. } => {
                    ReactionAdmissionFault::DuplicateToolCallOutput { output }
                }
            });
        }
        if state.tool_ordinals.contains(&ordinal) {
            return Err(ReactionAdmissionFault::DuplicateToolOrdinal { ordinal });
        }
        if let Some(previous) = state.last_tool_ordinal {
            if ordinal <= previous {
                return Err(ReactionAdmissionFault::ToolOrdinalNotIncreasing {
                    previous,
                    observed: ordinal,
                });
            }
        }

        if state.known_tool_call_ids.contains(call.call_id()) {
            return Err(ReactionAdmissionFault::DuplicateCanonicalToolCall {
                output,
                call_id_len: call.call_id().len(),
            });
        }

        Ok(())
    }

    fn commit_tool_call(
        state: &mut AdmissionState,
        output: ProviderOutputKey,
        ordinal: u64,
        call: ProviderToolCall,
        budget_item_bytes: Option<usize>,
    ) {
        record_admission_work(
            call.call_id()
                .len()
                .saturating_add(call.name().len())
                .saturating_add(call.raw_arguments().len())
                .saturating_add(1),
        );
        state.output_order.push(output);
        state.tool_ordinals.insert(ordinal);
        state.last_tool_ordinal = Some(ordinal);
        state.known_tool_call_ids.insert(call.call_id().to_owned());
        state.outputs.insert(
            output,
            AdmittedOutput::Tool {
                ordinal,
                call,
                budget_item_bytes,
            },
        );
    }

    fn validate_completion(
        state: &AdmissionState,
        primary_text: Option<ProviderOutputKey>,
    ) -> Result<Option<String>, ReactionAdmissionFault> {
        let complete_text = match primary_text {
            Some(primary) => {
                let Some(output) = state.outputs.get(&primary) else {
                    return Err(ReactionAdmissionFault::UnknownPrimaryText { output: primary });
                };
                let AdmittedOutput::Text {
                    phase,
                    text,
                    lifecycle,
                    ..
                } = output
                else {
                    return Err(ReactionAdmissionFault::PrimaryTextIsTool { output: primary });
                };
                if *lifecycle != TextLifecycle::Sealed {
                    return Err(ReactionAdmissionFault::PrimaryTextNotSealed { output: primary });
                }
                if is_commentary(*phase) {
                    return Err(ReactionAdmissionFault::PrimaryTextIsCommentary {
                        output: primary,
                    });
                }
                if state.non_commentary_text != Some(primary) {
                    return Err(ReactionAdmissionFault::PrimaryTextMismatch {
                        expected: state.non_commentary_text,
                        observed: primary,
                    });
                }
                record_admission_work(text.len().saturating_add(1));
                Some(text.clone())
            }
            None => {
                if let Some(output) = state.non_commentary_text {
                    return Err(ReactionAdmissionFault::MissingPrimaryText { output });
                }
                None
            }
        };

        if let Some(output) = state.output_order.iter().copied().find(|output| {
            matches!(
                state.outputs.get(output),
                Some(AdmittedOutput::Text {
                    lifecycle: TextLifecycle::Open,
                    ..
                })
            )
        }) {
            return Err(ReactionAdmissionFault::OpenTextAtCompletion { output });
        }

        Ok(complete_text)
    }
}

impl Drop for ReactionAdmissionGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.restore_transcript();
        }
    }
}

fn is_commentary(phase: Option<AssistantPhase>) -> bool {
    phase == Some(AssistantPhase::Commentary)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum ReactionAdmissionFault {
    #[error("provider fact appeared after ReactionCompleted")]
    FactAfterCompletion,
    #[error("ReactionCompleted appeared more than once")]
    DuplicateCompletion,
    #[error("provider output key {} was reused for a different output kind", .output.get())]
    OutputKindReuse { output: ProviderOutputKey },
    #[error("tool output key {} emitted more than one ToolCall", .output.get())]
    DuplicateToolCallOutput { output: ProviderOutputKey },
    #[error(
        "text output key {} changed phase from {established:?} to {observed:?}",
        .output.get()
    )]
    TextPhaseDrift {
        output: ProviderOutputKey,
        established: Option<AssistantPhase>,
        observed: Option<AssistantPhase>,
    },
    #[error("text output key {} emitted a delta after sealing", .output.get())]
    TextAfterSeal { output: ProviderOutputKey },
    #[error("text output key {} was sealed more than once", .output.get())]
    DuplicateTextSeal { output: ProviderOutputKey },
    #[error(
        "text output key {} seal mismatch (accumulated length {accumulated_len}, sealed length {sealed_len})",
        .output.get()
    )]
    TextSealMismatch {
        output: ProviderOutputKey,
        accumulated_len: usize,
        sealed_len: usize,
    },
    #[error(
        "reaction emitted multiple non-commentary text outputs: {} then {}",
        .established.get(),
        .observed.get()
    )]
    MultipleNonCommentaryText {
        established: ProviderOutputKey,
        observed: ProviderOutputKey,
    },
    #[error("tool ordinal {ordinal} appeared more than once")]
    DuplicateToolOrdinal { ordinal: u64 },
    #[error("tool ordinal {observed} did not advance previous ordinal {previous}")]
    ToolOrdinalNotIncreasing { previous: u64, observed: u64 },
    #[error(
        "tool output key {} reused a canonical tool call identity of length {call_id_len}",
        .output.get()
    )]
    DuplicateCanonicalToolCall {
        output: ProviderOutputKey,
        call_id_len: usize,
    },
    #[error("ReactionCompleted references unknown primary text key {}", .output.get())]
    UnknownPrimaryText { output: ProviderOutputKey },
    #[error("ReactionCompleted primary key {} identifies a ToolCall", .output.get())]
    PrimaryTextIsTool { output: ProviderOutputKey },
    #[error("ReactionCompleted primary text key {} is not sealed", .output.get())]
    PrimaryTextNotSealed { output: ProviderOutputKey },
    #[error("ReactionCompleted primary text key {} is commentary", .output.get())]
    PrimaryTextIsCommentary { output: ProviderOutputKey },
    #[error("ReactionCompleted expected primary {expected:?} but observed {}", .observed.get())]
    PrimaryTextMismatch {
        expected: Option<ProviderOutputKey>,
        observed: ProviderOutputKey,
    },
    #[error("ReactionCompleted omitted non-commentary text key {}", .output.get())]
    MissingPrimaryText { output: ProviderOutputKey },
    #[error("ReactionCompleted left text key {} open", .output.get())]
    OpenTextAtCompletion { output: ProviderOutputKey },
    #[error("provider fact stream ended without ReactionCompleted")]
    MissingCompletion,
    #[error("canonical output order references missing key {}", .output.get())]
    InternalOutputOrder { output: ProviderOutputKey },
    #[error("tool output admission failed: {0}")]
    ToolOutput(#[from] ToolOutputStagingFault),
    #[error("provider fact exceeds the canonical next-Full budget: {reason:?}")]
    Budget { reason: AdmissionBudgetReason },
    #[error("canonical next-Full budget tracker diverged from admission state")]
    BudgetStateMismatch,
    #[error("canonical admission invariant failed: {reason:?}")]
    CanonicalInvariant { reason: CanonicalAdmissionReason },
}

impl ReactionAdmissionFault {
    pub(super) const fn reason(&self) -> ReactionAdmissionReason {
        match self {
            Self::FactAfterCompletion => ReactionAdmissionReason::FactAfterCompletion,
            Self::DuplicateCompletion => ReactionAdmissionReason::DuplicateCompletion,
            Self::OutputKindReuse { .. } => ReactionAdmissionReason::OutputKindReuse,
            Self::DuplicateToolCallOutput { .. } => {
                ReactionAdmissionReason::DuplicateToolCallOutput
            }
            Self::TextPhaseDrift { .. } => ReactionAdmissionReason::TextPhaseDrift,
            Self::TextAfterSeal { .. } => ReactionAdmissionReason::TextAfterSeal,
            Self::DuplicateTextSeal { .. } => ReactionAdmissionReason::DuplicateTextSeal,
            Self::TextSealMismatch { .. } => ReactionAdmissionReason::TextSealMismatch,
            Self::MultipleNonCommentaryText { .. } => {
                ReactionAdmissionReason::MultipleNonCommentaryText
            }
            Self::DuplicateToolOrdinal { .. } => ReactionAdmissionReason::DuplicateToolOrdinal,
            Self::ToolOrdinalNotIncreasing { .. } => {
                ReactionAdmissionReason::ToolOrdinalNotIncreasing
            }
            Self::DuplicateCanonicalToolCall { .. } => {
                ReactionAdmissionReason::DuplicateCanonicalToolCall
            }
            Self::UnknownPrimaryText { .. } => ReactionAdmissionReason::UnknownPrimaryText,
            Self::PrimaryTextIsTool { .. } => ReactionAdmissionReason::PrimaryTextIsTool,
            Self::PrimaryTextNotSealed { .. } => ReactionAdmissionReason::PrimaryTextNotSealed,
            Self::PrimaryTextIsCommentary { .. } => {
                ReactionAdmissionReason::PrimaryTextIsCommentary
            }
            Self::PrimaryTextMismatch { .. } => ReactionAdmissionReason::PrimaryTextMismatch,
            Self::MissingPrimaryText { .. } => ReactionAdmissionReason::MissingPrimaryText,
            Self::OpenTextAtCompletion { .. } => ReactionAdmissionReason::OpenTextAtCompletion,
            Self::MissingCompletion => ReactionAdmissionReason::MissingCompletion,
            Self::InternalOutputOrder { .. } => ReactionAdmissionReason::InternalOutputOrder,
            Self::ToolOutput(_) => ReactionAdmissionReason::ToolOutput,
            Self::Budget { .. } => ReactionAdmissionReason::Budget,
            Self::BudgetStateMismatch => ReactionAdmissionReason::BudgetStateMismatch,
            Self::CanonicalInvariant { .. } => ReactionAdmissionReason::CanonicalInvariant,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReactionAdmissionReason {
    FactAfterCompletion,
    DuplicateCompletion,
    OutputKindReuse,
    DuplicateToolCallOutput,
    TextPhaseDrift,
    TextAfterSeal,
    DuplicateTextSeal,
    TextSealMismatch,
    MultipleNonCommentaryText,
    DuplicateToolOrdinal,
    ToolOrdinalNotIncreasing,
    DuplicateCanonicalToolCall,
    UnknownPrimaryText,
    PrimaryTextIsTool,
    PrimaryTextNotSealed,
    PrimaryTextIsCommentary,
    PrimaryTextMismatch,
    MissingPrimaryText,
    OpenTextAtCompletion,
    MissingCompletion,
    InternalOutputOrder,
    ToolOutput,
    Budget,
    BudgetStateMismatch,
    CanonicalInvariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmissionBudgetReason {
    ToolCatalog,
    NonIJsonNumber,
    Canonicalization,
    ComponentTooLarge,
    FrameTooLarge,
    FullReserveTooLarge,
    ArithmeticOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CanonicalAdmissionReason {
    InvalidIdentifier,
    InvalidToolArguments,
    ToolArgumentsMustBeObject,
    ZeroSchemaVersion,
    DuplicateToolCall,
    UnknownToolCall,
    DuplicateToolResult,
}

fn canonical_admission_reason(error: &CanonicalTranscriptError) -> CanonicalAdmissionReason {
    match error {
        CanonicalTranscriptError::InvalidIdentifier { .. } => {
            CanonicalAdmissionReason::InvalidIdentifier
        }
        CanonicalTranscriptError::InvalidToolArguments { .. } => {
            CanonicalAdmissionReason::InvalidToolArguments
        }
        CanonicalTranscriptError::ToolArgumentsMustBeObject { .. } => {
            CanonicalAdmissionReason::ToolArgumentsMustBeObject
        }
        CanonicalTranscriptError::ZeroSchemaVersion => CanonicalAdmissionReason::ZeroSchemaVersion,
        CanonicalTranscriptError::DuplicateToolCall { .. } => {
            CanonicalAdmissionReason::DuplicateToolCall
        }
        CanonicalTranscriptError::UnknownToolCall { .. } => {
            CanonicalAdmissionReason::UnknownToolCall
        }
        CanonicalTranscriptError::DuplicateToolResult { .. } => {
            CanonicalAdmissionReason::DuplicateToolResult
        }
    }
}

fn sanitize_reaction_canonical_fault(error: CanonicalTranscriptError) -> ReactionAdmissionFault {
    ReactionAdmissionFault::CanonicalInvariant {
        reason: canonical_admission_reason(&error),
    }
}

fn sanitize_staging_canonical_fault(error: CanonicalTranscriptError) -> ToolOutputStagingFault {
    ToolOutputStagingFault::CanonicalInvariant {
        reason: canonical_admission_reason(&error),
    }
}

fn admission_budget_reason(error: &FrameBudgetFault) -> AdmissionBudgetReason {
    match error {
        FrameBudgetFault::ToolCatalog(_) => AdmissionBudgetReason::ToolCatalog,
        FrameBudgetFault::NonIJsonNumber { .. } => AdmissionBudgetReason::NonIJsonNumber,
        FrameBudgetFault::Canonicalization { .. } => AdmissionBudgetReason::Canonicalization,
        FrameBudgetFault::ComponentTooLarge { .. } => AdmissionBudgetReason::ComponentTooLarge,
        FrameBudgetFault::FrameTooLarge { .. } => AdmissionBudgetReason::FrameTooLarge,
        FrameBudgetFault::FullReserveTooLarge { .. } => AdmissionBudgetReason::FullReserveTooLarge,
        FrameBudgetFault::ArithmeticOverflow => AdmissionBudgetReason::ArithmeticOverflow,
    }
}

fn sanitize_budget_fault(error: FrameBudgetFault) -> ReactionAdmissionFault {
    ReactionAdmissionFault::Budget {
        reason: admission_budget_reason(&error),
    }
}

fn sanitize_staging_budget_fault(error: FrameBudgetFault) -> ToolOutputStagingFault {
    ToolOutputStagingFault::Budget {
        reason: admission_budget_reason(&error),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        num::{NonZeroU128, NonZeroU64},
    };

    use crate::{
        component::execution::{
            frame::FrameSession,
            reaction::{
                FrameCapabilities, FrameConstraints, FrameProfile, TargetDeclaration, TargetEpoch,
                TargetIdentity,
            },
        },
        transcript::{
            reset_construction_work, take_construction_work, AssistantTextStatus,
            CanonicalInputItem,
        },
    };

    use super::*;

    fn key(value: u64) -> ProviderOutputKey {
        ProviderOutputKey::new(value)
    }

    fn call(call_id: &str) -> ProviderToolCall {
        ProviderToolCall::new(call_id, "lookup", r#"{"query":"value"}"#).unwrap()
    }

    fn delta(output: u64, phase: Option<AssistantPhase>, text: &str) -> ProviderFact {
        ProviderFact::TextDelta {
            output: key(output),
            phase,
            delta: text.to_owned(),
        }
    }

    fn sealed(output: u64, phase: Option<AssistantPhase>, text: &str) -> ProviderFact {
        ProviderFact::TextSealed {
            output: key(output),
            phase,
            text: text.to_owned(),
        }
    }

    fn tool(output: u64, ordinal: u64, call_id: &str) -> ProviderFact {
        ProviderFact::ToolCall {
            output: key(output),
            ordinal,
            call: call(call_id),
        }
    }

    fn completed(primary: Option<u64>) -> ProviderFact {
        ProviderFact::ReactionCompleted {
            primary_text: primary.map(key),
        }
    }

    fn full_reserve(max_component_bytes: usize, max_frame_bytes: usize) -> FullReserveBudget {
        let declaration = TargetDeclaration::full(
            TargetIdentity::new(NonZeroU128::new(1).unwrap()),
            TargetEpoch::new(NonZeroU64::new(1).unwrap()),
            FrameProfile::new(
                FrameConstraints {
                    max_frame_bytes,
                    max_component_bytes,
                    context_window_tokens: None,
                    reserved_output_tokens: None,
                },
                FrameCapabilities::NONE,
            ),
        );
        FrameSession::new(&declaration)
            .unwrap()
            .full_reserve_budget()
    }

    fn assert_text(
        item: &CanonicalInputItem,
        expected_text: &str,
        expected_phase: Option<AssistantPhase>,
        expected_status: AssistantTextStatus,
    ) {
        assert_eq!(
            item,
            &CanonicalInputItem::AssistantText {
                text: expected_text.to_owned(),
                phase: expected_phase,
                status: expected_status,
            }
        );
    }

    fn assert_fault_does_not_contain(fault: &impl Error, sentinels: &[&str]) {
        let mut rendered = format!("{fault:?}\n{fault}");
        let mut source = fault.source();
        while let Some(error) = source {
            rendered.push_str(&format!("\n{error:?}\n{error}"));
            source = error.source();
        }
        for sentinel in sentinels {
            assert!(
                !rendered.contains(sentinel),
                "fault rendering leaked sentinel `{sentinel}`: {rendered}"
            );
        }
    }

    #[test]
    fn text_kind_reuse_is_rejected_without_committing_candidate() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            guard.admit(tool(1, 4, "call-1")).unwrap();
            let before = guard.committed_output_count();
            assert_eq!(
                guard.admit(delta(1, None, "wrong")),
                Err(ReactionAdmissionFault::OutputKindReuse { output: key(1) })
            );
            assert_eq!(guard.committed_output_count(), before);
        }
        assert_eq!(transcript.items().len(), 1);
    }

    #[test]
    fn tool_kind_reuse_and_repeated_tool_key_are_distinct_faults() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(delta(1, None, "text")).unwrap();
        assert_eq!(
            guard.admit(tool(1, 1, "call-1")),
            Err(ReactionAdmissionFault::OutputKindReuse { output: key(1) })
        );

        guard.admit(tool(2, 2, "call-2")).unwrap();
        assert_eq!(
            guard.admit(tool(2, 3, "call-3")),
            Err(ReactionAdmissionFault::DuplicateToolCallOutput { output: key(2) })
        );
    }

    #[test]
    fn text_phase_drift_after_delta_or_seal_is_rejected() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard
            .admit(delta(1, Some(AssistantPhase::Commentary), "a"))
            .unwrap();
        assert!(matches!(
            guard.admit(delta(1, Some(AssistantPhase::FinalAnswer), "b")),
            Err(ReactionAdmissionFault::TextPhaseDrift { .. })
        ));
        guard
            .admit(sealed(1, Some(AssistantPhase::Commentary), "a"))
            .unwrap();
        assert!(matches!(
            guard.admit(sealed(1, None, "a")),
            Err(ReactionAdmissionFault::TextPhaseDrift { .. })
        ));
    }

    #[test]
    fn text_delta_after_seal_duplicate_seal_and_mismatched_seal_are_rejected() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(delta(1, None, "ab")).unwrap();
        assert!(matches!(
            guard.admit(sealed(1, None, "different")),
            Err(ReactionAdmissionFault::TextSealMismatch { .. })
        ));
        guard.admit(sealed(1, None, "ab")).unwrap();
        assert_eq!(
            guard.admit(delta(1, None, "c")),
            Err(ReactionAdmissionFault::TextAfterSeal { output: key(1) })
        );
        assert_eq!(
            guard.admit(sealed(1, None, "ab")),
            Err(ReactionAdmissionFault::DuplicateTextSeal { output: key(1) })
        );
    }

    #[test]
    fn only_one_non_commentary_text_lifecycle_is_allowed() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(delta(1, None, "a")).unwrap();
        assert_eq!(
            guard.admit(sealed(2, Some(AssistantPhase::FinalAnswer), "b")),
            Err(ReactionAdmissionFault::MultipleNonCommentaryText {
                established: key(1),
                observed: key(2),
            })
        );

        guard
            .admit(sealed(3, Some(AssistantPhase::Commentary), "note"))
            .unwrap();
        guard
            .admit(sealed(4, Some(AssistantPhase::Commentary), "more"))
            .unwrap();
    }

    #[test]
    fn tool_ordinals_are_unique_and_strictly_increasing() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(tool(1, 4, "call-1")).unwrap();
        assert_eq!(
            guard.admit(tool(2, 4, "call-2")),
            Err(ReactionAdmissionFault::DuplicateToolOrdinal { ordinal: 4 })
        );
        assert_eq!(
            guard.admit(tool(2, 3, "call-2")),
            Err(ReactionAdmissionFault::ToolOrdinalNotIncreasing {
                previous: 4,
                observed: 3,
            })
        );
        guard.admit(tool(2, 9, "call-2")).unwrap();
    }

    #[test]
    fn duplicate_canonical_call_id_rejects_the_whole_fact() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(tool(1, 1, "same-call")).unwrap();
        let before = guard.committed_output_count();
        assert_eq!(
            guard.admit(tool(2, 2, "same-call")),
            Err(ReactionAdmissionFault::DuplicateCanonicalToolCall {
                output: key(2),
                call_id_len: "same-call".len(),
            })
        );
        assert_eq!(guard.committed_output_count(), before);
    }

    #[test]
    fn completion_must_be_unique_and_last() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(completed(None)).unwrap();
        assert_eq!(
            guard.admit(completed(None)),
            Err(ReactionAdmissionFault::DuplicateCompletion)
        );
        assert_eq!(
            guard.admit(tool(1, 1, "late")),
            Err(ReactionAdmissionFault::FactAfterCompletion)
        );
        guard.finish_normal().unwrap();
    }

    #[test]
    fn normal_finish_requires_completion_and_aborts_open_text_on_error() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(delta(1, None, "partial")).unwrap();
        assert_eq!(
            guard.finish_normal(),
            Err(ReactionAdmissionFault::MissingCompletion)
        );
        assert_text(
            &transcript.items()[0],
            "partial",
            None,
            AssistantTextStatus::Interrupted,
        );
    }

    #[test]
    fn completion_rejects_unknown_tool_commentary_open_and_missing_primary_keys() {
        let cases = [
            (
                vec![],
                completed(Some(99)),
                ReactionAdmissionFault::UnknownPrimaryText { output: key(99) },
            ),
            (
                vec![tool(1, 1, "call-1")],
                completed(Some(1)),
                ReactionAdmissionFault::PrimaryTextIsTool { output: key(1) },
            ),
            (
                vec![sealed(1, Some(AssistantPhase::Commentary), "note")],
                completed(Some(1)),
                ReactionAdmissionFault::PrimaryTextIsCommentary { output: key(1) },
            ),
            (
                vec![delta(1, None, "open")],
                completed(Some(1)),
                ReactionAdmissionFault::PrimaryTextNotSealed { output: key(1) },
            ),
            (
                vec![sealed(1, None, "done")],
                completed(None),
                ReactionAdmissionFault::MissingPrimaryText { output: key(1) },
            ),
            (
                vec![delta(1, Some(AssistantPhase::Commentary), "open")],
                completed(None),
                ReactionAdmissionFault::OpenTextAtCompletion { output: key(1) },
            ),
        ];

        for (facts, completion, expected) in cases {
            let mut transcript = CanonicalTranscript::new();
            let mut staging = ToolOutputStaging::default();
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            for fact in facts {
                guard.admit(fact).unwrap();
            }
            assert_eq!(guard.admit(completion), Err(expected));
        }
    }

    #[test]
    fn sealed_only_text_can_be_the_first_fact_and_completes_normally() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        assert_eq!(
            guard
                .admit(sealed(7, Some(AssistantPhase::FinalAnswer), "answer"))
                .unwrap()
                .into_parts(),
            (None, None)
        );
        let (event, ticket) = guard.admit(completed(Some(7))).unwrap().into_parts();
        assert_eq!(
            event,
            Some(ProviderEvent::Text(TextTurnEvent::TextComplete(
                "answer".to_owned()
            )))
        );
        assert!(ticket.is_none());
        guard.finish_normal().unwrap();
        assert_text(
            &transcript.items()[0],
            "answer",
            Some(AssistantPhase::FinalAnswer),
            AssistantTextStatus::Sealed,
        );
    }

    #[test]
    fn commentary_only_and_empty_reactions_do_not_forge_text_complete() {
        for facts in [
            vec![sealed(1, Some(AssistantPhase::Commentary), "commentary")],
            vec![],
        ] {
            let mut transcript = CanonicalTranscript::new();
            let mut staging = ToolOutputStaging::default();
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            for fact in facts {
                guard.admit(fact).unwrap();
            }
            assert_eq!(
                guard.admit(completed(None)).unwrap().into_parts(),
                (None, None)
            );
            guard.finish_normal().unwrap();
        }
    }

    #[test]
    fn tool_only_reaction_completes_without_forging_text_after_its_lane_drains() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        let (event, ticket) = guard.admit(tool(1, 3, "call-1")).unwrap().into_parts();
        let ProviderEvent::ToolCall(call) = event.unwrap() else {
            panic!("expected ToolCall event")
        };
        guard
            .stage_tool_output(ticket.unwrap(), call.output("result"))
            .unwrap();
        assert_eq!(
            guard.admit(completed(None)).unwrap().into_parts(),
            (None, None)
        );
        guard.finish_normal().unwrap();
    }

    #[test]
    fn normal_finish_rejects_a_dropped_tool_lane_ticket() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        let (_, ticket) = guard.admit(tool(1, 1, "unfinished")).unwrap().into_parts();
        drop(ticket);
        guard.admit(completed(None)).unwrap();
        assert_eq!(
            guard.finish_normal(),
            Err(ReactionAdmissionFault::ToolOutput(
                ToolOutputStagingFault::UnresolvedOutput { ordinal: 1 }
            ))
        );
    }

    #[test]
    fn cancellation_keeps_completed_tool_output_and_fails_closed_on_open_lane() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            let (first_event, first_ticket) =
                guard.admit(tool(1, 1, "finished")).unwrap().into_parts();
            let (_, open_ticket) = guard.admit(tool(2, 2, "open")).unwrap().into_parts();
            let ProviderEvent::ToolCall(first_call) = first_event.unwrap() else {
                panic!("expected ToolCall event")
            };
            guard
                .stage_tool_output(first_ticket.unwrap(), first_call.output("done"))
                .unwrap();
            drop(open_ticket);
        }

        assert!(matches!(
            &transcript.items()[0],
            CanonicalInputItem::ToolCall { call_id, .. } if call_id == "finished"
        ));
        assert!(matches!(
            &transcript.items()[1],
            CanonicalInputItem::ToolCall { call_id, .. } if call_id == "open"
        ));
        assert!(matches!(
            staging.prepare_receipt(),
            Err(ToolOutputStagingFault::UnresolvedOutput { ordinal: 2 })
        ));
        let staged = staging.ordered_outputs().collect::<Vec<_>>();
        assert_eq!(staged.len(), 1);
        assert!(matches!(
            staged[0].1,
            CanonicalInputItem::ToolResult { call_id, content }
                if call_id == "finished" && content == "done"
        ));
    }

    #[test]
    fn canonical_order_is_established_by_each_outputs_first_fact() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            guard
                .admit(delta(9, Some(AssistantPhase::Commentary), "first"))
                .unwrap();
            guard.admit(tool(2, 10, "middle")).unwrap();
            guard.admit(sealed(7, None, "last")).unwrap();
        }

        assert_text(
            &transcript.items()[0],
            "first",
            Some(AssistantPhase::Commentary),
            AssistantTextStatus::Interrupted,
        );
        assert!(matches!(
            &transcript.items()[1],
            CanonicalInputItem::ToolCall { call_id, .. } if call_id == "middle"
        ));
        assert_text(
            &transcript.items()[2],
            "last",
            None,
            AssistantTextStatus::Sealed,
        );
    }

    #[test]
    fn guard_drop_materializes_open_text_as_interrupted_and_keeps_sealed_and_tools() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            guard.admit(delta(1, None, "left")).unwrap();
            guard
                .admit(sealed(2, Some(AssistantPhase::Commentary), "sealed"))
                .unwrap();
            guard.admit(tool(3, 5, "call-3")).unwrap();
        }

        assert_text(
            &transcript.items()[0],
            "left",
            None,
            AssistantTextStatus::Interrupted,
        );
        assert_text(
            &transcript.items()[1],
            "sealed",
            Some(AssistantPhase::Commentary),
            AssistantTextStatus::Sealed,
        );
        assert!(matches!(
            &transcript.items()[2],
            CanonicalInputItem::ToolCall { call_id, .. } if call_id == "call-3"
        ));
    }

    #[test]
    fn normal_finish_materializes_only_sealed_text() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(delta(1, None, "a")).unwrap();
        guard.admit(delta(1, None, "b")).unwrap();
        guard.admit(sealed(1, None, "ab")).unwrap();
        guard.admit(completed(Some(1))).unwrap();
        guard.finish_normal().unwrap();

        assert_text(
            &transcript.items()[0],
            "ab",
            None,
            AssistantTextStatus::Sealed,
        );
    }

    #[test]
    fn event_is_released_only_after_incremental_tail_is_committed() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            let (event, _) = guard
                .admit(delta(1, None, "visible-before-handler"))
                .unwrap()
                .into_parts();
            assert_eq!(
                event,
                Some(ProviderEvent::Text(TextTurnEvent::TextDelta(
                    "visible-before-handler".to_owned()
                )))
            );
            assert_eq!(
                guard.committed_text(key(1)),
                Some(("visible-before-handler", TextLifecycle::Open))
            );

            // A handler fault now drops the reaction. The already released
            // event can never outrun its canonical history commit.
        }
        assert_eq!(transcript.items().len(), 1);
    }

    #[test]
    fn guard_drop_restores_released_tail_even_if_admission_ledger_is_corrupted() {
        let mut transcript =
            CanonicalTranscript::try_from_items(vec![CanonicalInputItem::assistant_text(
                "existing-prefix",
                None,
            )])
            .unwrap();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            let (event, _) = guard
                .admit(delta(1, None, "released-before-invariant-fault"))
                .unwrap()
                .into_parts();
            assert!(event.is_some());

            // Simulate an internal state invariant failing after the event is
            // released. Drop must not rebuild from this secondary ledger.
            guard.forget_output_for_test(key(1));
        }

        assert_eq!(transcript.items().len(), 2);
        assert_text(
            &transcript.items()[0],
            "existing-prefix",
            None,
            AssistantTextStatus::Sealed,
        );
        assert_text(
            &transcript.items()[1],
            "released-before-invariant-fault",
            None,
            AssistantTextStatus::Interrupted,
        );
    }

    #[test]
    fn tool_lanes_can_finish_in_reverse_while_outputs_stage_by_ordinal() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            let (first_event, first_ticket) =
                guard.admit(tool(1, 2, "first")).unwrap().into_parts();
            let (second_event, second_ticket) =
                guard.admit(tool(2, 7, "second")).unwrap().into_parts();
            let ProviderEvent::ToolCall(first_call) = first_event.unwrap() else {
                panic!("expected first ToolCall event")
            };
            let ProviderEvent::ToolCall(second_call) = second_event.unwrap() else {
                panic!("expected second ToolCall event")
            };

            guard
                .stage_tool_output(second_ticket.unwrap(), second_call.output("second-result"))
                .unwrap();
            guard
                .stage_tool_output(first_ticket.unwrap(), first_call.output("first-result"))
                .unwrap();
            guard.admit(completed(None)).unwrap();
            guard.finish_normal().unwrap();
        }

        let outputs = staging.ordered_outputs().collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].0, 2);
        assert!(matches!(
            outputs[0].1,
            CanonicalInputItem::ToolResult { call_id, content }
                if call_id == "first" && content == "first-result"
        ));
        assert_eq!(outputs[1].0, 7);
        assert!(matches!(
            outputs[1].1,
            CanonicalInputItem::ToolResult { call_id, content }
                if call_id == "second" && content == "second-result"
        ));
    }

    #[test]
    fn staging_rejects_unknown_mismatched_and_duplicate_lane_results() {
        let mut staging = ToolOutputStaging::default();
        let unknown_call = ToolCall::new("unknown", "lookup", "{}").unwrap();
        assert_eq!(
            staging.stage(
                ToolLaneTicket {
                    owner: Arc::clone(&staging.owner),
                    registration: 41,
                    ordinal: 9,
                    call_id: "unknown".to_owned(),
                },
                unknown_call.output("result"),
            ),
            Err(ToolOutputStagingFault::UnknownTicket { ordinal: 9 })
        );

        let ticket = staging.register(3, "expected").unwrap();
        let owner = Arc::clone(&ticket.owner);
        let registration = ticket.registration;
        let mismatched_call = ToolCall::new("observed", "lookup", "{}").unwrap();
        assert_eq!(
            staging.stage(ticket, mismatched_call.output("result")),
            Err(ToolOutputStagingFault::CallMismatch {
                ordinal: 3,
                expected_call_id_len: "expected".len(),
                observed_call_id_len: "observed".len(),
            })
        );

        let ticket = ToolLaneTicket {
            owner: Arc::clone(&owner),
            registration,
            ordinal: 3,
            call_id: "expected".to_owned(),
        };
        let expected_call = ToolCall::new("expected", "lookup", "{}").unwrap();
        staging
            .stage(ticket, expected_call.output("first"))
            .unwrap();
        assert_eq!(
            staging.stage(
                ToolLaneTicket {
                    owner,
                    registration,
                    ordinal: 3,
                    call_id: "expected".to_owned(),
                },
                expected_call.output("second"),
            ),
            Err(ToolOutputStagingFault::DuplicateOutput { ordinal: 3 })
        );
    }

    #[test]
    fn staging_rejects_a_ticket_from_another_owner() {
        let mut first = ToolOutputStaging::default();
        let foreign = first.register(1, "same-call").unwrap();
        let call = ToolCall::new("same-call", "lookup", "{}").unwrap();

        let mut second = ToolOutputStaging::default();
        let _local = second.register(1, "same-call").unwrap();
        assert_eq!(
            second.stage(foreign, call.output("late")),
            Err(ToolOutputStagingFault::ForeignTicket { ordinal: 1 })
        );
    }

    #[test]
    fn reaction_local_tool_ordinals_may_be_reused_in_a_later_reaction() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();

        for (output, call_id) in [(1, "first"), (2, "second")] {
            let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
            let (event, ticket) = guard.admit(tool(output, 1, call_id)).unwrap().into_parts();
            let ProviderEvent::ToolCall(call) = event.unwrap() else {
                panic!("expected ToolCall event")
            };
            guard
                .stage_tool_output(ticket.unwrap(), call.output(format!("{call_id}-result")))
                .unwrap();
            guard.admit(completed(None)).unwrap();
            guard.finish_normal().unwrap();
        }

        let outputs = staging.ordered_outputs().collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].0, 1);
        assert_eq!(outputs[1].0, 1);
        assert!(matches!(
            outputs[0].1,
            CanonicalInputItem::ToolResult { call_id, .. } if call_id == "first"
        ));
        assert!(matches!(
            outputs[1].1,
            CanonicalInputItem::ToolResult { call_id, .. } if call_id == "second"
        ));
    }

    #[test]
    fn exact_receipt_is_retryable_until_commit_and_consumed_once() {
        let mut staging = ToolOutputStaging::default();
        let call = ToolCall::new("call-1", "lookup", "{}").unwrap();
        let ticket = staging.register(2, call.call_id()).unwrap();
        staging.stage(ticket, call.output("result")).unwrap();

        {
            let receipt = staging.prepare_receipt().unwrap();
            assert!(matches!(
                &receipt.outputs()[0],
                CanonicalInputItem::ToolResult { call_id, content }
                    if call_id == "call-1" && content == "result"
            ));
            // A failed local prepare or pre-handoff submit drops the receipt.
        }
        assert_eq!(staging.ordered_outputs().count(), 1);

        let receipt = staging.prepare_receipt().unwrap();
        receipt.commit();
        assert_eq!(staging.ordered_outputs().count(), 0);
        assert_eq!(staging.prepare_receipt().unwrap().outputs(), &[]);
    }

    #[test]
    fn fault_display_debug_and_sources_never_expose_authored_or_provider_payload() {
        const ACCUMULATED: &str = "LEAK_SENTINEL_ACCUMULATED";
        const SEALED: &str = "LEAK_SENTINEL_SEALED";
        const EXPECTED_CALL: &str = "LEAK_SENTINEL_EXPECTED_CALL";
        const OBSERVED_CALL: &str = "LEAK_SENTINEL_OBSERVED_CALL";
        const ARGUMENT_PAYLOAD: &str = "LEAK_SENTINEL_ARGUMENT_PAYLOAD";

        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(delta(1, None, ACCUMULATED)).unwrap();
        let mismatch = guard.admit(sealed(1, None, SEALED)).unwrap_err();
        assert_eq!(mismatch.reason(), ReactionAdmissionReason::TextSealMismatch);
        assert_fault_does_not_contain(&mismatch, &[ACCUMULATED, SEALED]);
        drop(guard);

        let mut staging = ToolOutputStaging::default();
        let ticket = staging.register(4, EXPECTED_CALL).unwrap();
        let observed = ToolCall::new(OBSERVED_CALL, "lookup", "{}").unwrap();
        let mismatch = staging
            .stage(ticket, observed.output("result"))
            .unwrap_err();
        assert_eq!(mismatch.reason(), ToolOutputStagingReason::CallMismatch);
        assert_fault_does_not_contain(&mismatch, &[EXPECTED_CALL, OBSERVED_CALL]);

        let mut unresolved_staging = ToolOutputStaging::default();
        let _unresolved = unresolved_staging.register(8, EXPECTED_CALL).unwrap();
        let unresolved = unresolved_staging.prepare_frame_candidate().unwrap_err();
        assert_eq!(
            unresolved.reason(),
            ToolOutputStagingReason::UnresolvedOutput
        );
        assert_fault_does_not_contain(&unresolved, &[EXPECTED_CALL]);

        let mut duplicate_staging = ToolOutputStaging::default();
        let duplicate_call = ToolCall::new("duplicate-call", "lookup", "{}").unwrap();
        let ticket = duplicate_staging
            .register(9, duplicate_call.call_id())
            .unwrap();
        let registration = ticket.registration;
        let owner = Arc::clone(&ticket.owner);
        duplicate_staging
            .stage(ticket, duplicate_call.output(ACCUMULATED))
            .unwrap();
        let duplicate = duplicate_staging
            .stage(
                ToolLaneTicket {
                    owner,
                    registration,
                    ordinal: 9,
                    call_id: duplicate_call.call_id().to_owned(),
                },
                duplicate_call.output(SEALED),
            )
            .unwrap_err();
        assert_eq!(duplicate.reason(), ToolOutputStagingReason::DuplicateOutput);
        assert_fault_does_not_contain(&duplicate, &[ACCUMULATED, SEALED]);

        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        guard.admit(tool(1, 1, EXPECTED_CALL)).unwrap();
        let duplicate = guard.admit(tool(2, 2, EXPECTED_CALL)).unwrap_err();
        assert_eq!(
            duplicate.reason(),
            ReactionAdmissionReason::DuplicateCanonicalToolCall
        );
        assert_fault_does_not_contain(&duplicate, &[EXPECTED_CALL]);

        let payload_call = ProviderToolCall::new(
            "payload-call",
            "lookup",
            format!(r#"{{"secret":"{ARGUMENT_PAYLOAD}"}}"#),
        )
        .unwrap();
        guard
            .admit(sealed(3, Some(AssistantPhase::Commentary), "safe"))
            .unwrap();
        let kind_reuse = guard
            .admit(ProviderFact::ToolCall {
                output: key(3),
                ordinal: 3,
                call: payload_call,
            })
            .unwrap_err();
        assert_fault_does_not_contain(&kind_reuse, &[ARGUMENT_PAYLOAD]);
    }

    #[test]
    fn oversized_text_delta_is_rejected_before_event_or_history_publication() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::with_budget(
                &mut transcript,
                &mut staging,
                full_reserve(128, 512),
            )
            .unwrap();
            let fault = guard
                .admit(delta(1, None, &"oversized".repeat(256)))
                .unwrap_err();
            assert_eq!(fault.reason(), ReactionAdmissionReason::Budget);
            assert_eq!(guard.committed_output_count(), 0);
        }
        assert!(transcript.items().is_empty());
        assert!(staging.slots.is_empty());
    }

    #[test]
    fn oversized_tool_call_is_rejected_before_ticket_or_staging_registration() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard = ReactionAdmissionGuard::with_budget(
                &mut transcript,
                &mut staging,
                full_reserve(128, 512),
            )
            .unwrap();
            let fact = ProviderFact::ToolCall {
                output: key(1),
                ordinal: 1,
                call: ProviderToolCall::new(
                    "large-call",
                    "lookup",
                    format!(r#"{{"payload":"{}"}}"#, "x".repeat(2_048)),
                )
                .unwrap(),
            };
            let fault = guard.admit(fact).unwrap_err();
            assert_eq!(fault.reason(), ReactionAdmissionReason::Budget);
            assert_eq!(guard.committed_output_count(), 0);
            assert!(guard.staging.slots.is_empty());
        }
        assert!(transcript.items().is_empty());
        assert!(staging.slots.is_empty());
    }

    #[test]
    fn oversized_tool_output_leaves_the_original_slot_retryable() {
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        let mut guard = ReactionAdmissionGuard::with_budget(
            &mut transcript,
            &mut staging,
            full_reserve(128, 1_024),
        )
        .unwrap();
        let (event, ticket) = guard.admit(tool(1, 1, "retryable")).unwrap().into_parts();
        let ProviderEvent::ToolCall(call) = event.unwrap() else {
            panic!("expected ToolCall event")
        };
        let ticket = ticket.unwrap();
        let retry = ToolLaneTicket {
            owner: Arc::clone(&ticket.owner),
            registration: ticket.registration,
            ordinal: ticket.ordinal,
            call_id: ticket.call_id.clone(),
        };

        let fault = guard
            .stage_tool_output(ticket, call.output("too-large".repeat(512)))
            .unwrap_err();
        assert_eq!(fault.reason(), ToolOutputStagingReason::Budget);
        assert!(guard
            .staging
            .slots
            .get(&retry.registration)
            .unwrap()
            .output
            .is_none());

        guard
            .stage_tool_output(retry, call.output("small"))
            .unwrap();
        assert!(guard
            .staging
            .slots
            .values()
            .next()
            .unwrap()
            .output
            .is_some());
    }

    #[test]
    fn dropping_a_budgeted_guard_keeps_interrupted_history_within_full_reserve() {
        let budget = full_reserve(128, 1_024);
        let mut transcript = CanonicalTranscript::new();
        let mut staging = ToolOutputStaging::default();
        {
            let mut guard =
                ReactionAdmissionGuard::with_budget(&mut transcript, &mut staging, budget.clone())
                    .unwrap();
            guard.admit(delta(1, None, "quote: \"")).unwrap();
            guard.admit(delta(1, None, "\nemoji: \u{1f642}\\")).unwrap();
        }

        budget.validate(&transcript, &staging).unwrap();
        assert_text(
            transcript.items().last().unwrap(),
            "quote: \"\nemoji: \u{1f642}\\",
            None,
            AssistantTextStatus::Interrupted,
        );
    }

    #[test]
    fn streaming_delta_construction_work_is_linear_in_history_and_output() {
        const HISTORY_ITEMS: usize = 512;
        const DELTA_COUNT: usize = 4_096;

        let base = (0..HISTORY_ITEMS)
            .map(|index| CanonicalInputItem::assistant_text(format!("base-{index}"), None))
            .collect::<Vec<_>>();
        let mut transcript = CanonicalTranscript::try_from_items(base).unwrap();
        let mut staging = ToolOutputStaging::default();
        let complete = "x".repeat(DELTA_COUNT);

        reset_construction_work();
        reset_admission_work();
        let mut guard = ReactionAdmissionGuard::new(&mut transcript, &mut staging);
        for _ in 0..DELTA_COUNT {
            guard.admit(delta(1, None, "x")).unwrap();
        }
        guard.admit(sealed(1, None, &complete)).unwrap();
        guard.admit(completed(Some(1))).unwrap();
        guard.finish_normal().unwrap();

        let transcript_work = take_construction_work();
        let admission_work = take_admission_work();
        assert!(
            transcript_work <= HISTORY_ITEMS.saturating_add(4),
            "transcript was rebuilt per delta: {transcript_work} operations"
        );
        let linear_bound = HISTORY_ITEMS
            .saturating_mul(2)
            .saturating_add(DELTA_COUNT.saturating_mul(7))
            .saturating_add(64);
        assert!(
            admission_work <= linear_bound,
            "admission exceeded linear bound: {admission_work} > {linear_bound}"
        );
        assert_eq!(transcript.items().len(), HISTORY_ITEMS + 1);
        assert_text(
            transcript.items().last().unwrap(),
            &complete,
            None,
            AssistantTextStatus::Sealed,
        );
    }
}
