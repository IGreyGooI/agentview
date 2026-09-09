//! Canonical Frame compilation and budget accounting.

#![allow(dead_code)]

use std::{
    collections::VecDeque,
    num::{NonZeroU128, NonZeroU64},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    pom::{BlockChildren, ResolvedDocument},
    transcript::{
        CanonicalInputItem, CanonicalTranscript, CanonicalTranscriptError, InstructionAuthority,
    },
};

use super::projection_diff::ProjectionReconciliationFault;
use super::{
    admission::{ToolOutputStaging, ToolOutputStagingFault},
    reaction::{
        Frame, FrameBasis, FrameConstraints, FrameInvariantFault, FrameProfile, FrameRevision,
        FrameSubmission, ProjectionSubmission, TargetContinuity, TargetDeclaration, TargetEpoch,
        TargetIdentity, ToolCatalog,
    },
    ProjectionDiffState, ProjectionReconciliationState, ReconciledProjectionPlan,
    RenderedProjection, ToolDefinition,
};

const FRAME_METER_VERSION: u8 = 1;
const JSON_NULL_BYTES: usize = b"null".len();
const EMPTY_FRAME_WITH_NULL_COMPONENT: &[u8] =
    br#"{"component":null,"replay":[],"staged_inputs":[],"version":1}"#;
static NEXT_FRAME_SESSION_NAMESPACE: AtomicU64 = AtomicU64::new(1);

impl FrameProfile {
    /// Validates the mount-stable canonical envelope against the exact v1
    /// Frame meter shape used by compilation and Full-reserve accounting.
    pub(crate) fn validate(&self) -> Result<(), InvalidFrameProfileFault> {
        let constraints = &self.constraints;
        if constraints.max_frame_bytes == 0 {
            return Err(InvalidFrameProfileFault::ZeroMaxFrameBytes);
        }
        if constraints.max_component_bytes == 0 {
            return Err(InvalidFrameProfileFault::ZeroMaxComponentBytes);
        }
        if constraints.context_window_tokens == Some(0) {
            return Err(InvalidFrameProfileFault::ZeroContextWindowTokens);
        }
        if constraints.reserved_output_tokens == Some(0) {
            return Err(InvalidFrameProfileFault::ZeroReservedOutputTokens);
        }
        if constraints.max_component_bytes > constraints.max_frame_bytes {
            return Err(InvalidFrameProfileFault::ComponentLimitExceedsFrameLimit {
                max_component_bytes: constraints.max_component_bytes,
                max_frame_bytes: constraints.max_frame_bytes,
            });
        }

        let full_non_component_bytes = EMPTY_FRAME_WITH_NULL_COMPONENT
            .len()
            .checked_sub(JSON_NULL_BYTES)
            .ok_or(InvalidFrameProfileFault::InitialFullReserveOverflow)?;
        let required_full_bytes = full_non_component_bytes
            .checked_add(constraints.max_component_bytes)
            .ok_or(InvalidFrameProfileFault::InitialFullReserveOverflow)?;
        if required_full_bytes > constraints.max_frame_bytes {
            return Err(InvalidFrameProfileFault::InsufficientInitialFullReserve {
                required_full_bytes,
                max_frame_bytes: constraints.max_frame_bytes,
            });
        }

        Ok(())
    }
}

/// Invalid mount-stable limits declared by a reaction target.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InvalidFrameProfileFault {
    #[error("max_frame_bytes must be greater than zero")]
    ZeroMaxFrameBytes,
    #[error("max_component_bytes must be greater than zero")]
    ZeroMaxComponentBytes,
    #[error("context_window_tokens must be greater than zero when present")]
    ZeroContextWindowTokens,
    #[error("reserved_output_tokens must be greater than zero when present")]
    ZeroReservedOutputTokens,
    #[error(
        "max_component_bytes ({max_component_bytes}) exceeds max_frame_bytes ({max_frame_bytes})"
    )]
    ComponentLimitExceedsFrameLimit {
        max_component_bytes: usize,
        max_frame_bytes: usize,
    },
    #[error("initial Full reserve arithmetic overflowed")]
    InitialFullReserveOverflow,
    #[error(
        "initial Full reserve requires {required_full_bytes} bytes but max_frame_bytes is {max_frame_bytes}"
    )]
    InsufficientInitialFullReserve {
        required_full_bytes: usize,
        max_frame_bytes: usize,
    },
}

#[derive(Serialize)]
struct ProjectionSubmissionV1<'a> {
    items: &'a [CanonicalInputItem],
}

#[derive(Serialize)]
struct ToolCatalogEntryV1<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
    strict: bool,
}

#[derive(Serialize)]
struct ComponentEnvelopeV1<'a> {
    version: u8,
    projection: ProjectionSubmissionV1<'a>,
    tools: Vec<ToolCatalogEntryV1<'a>>,
}

#[derive(Serialize)]
struct FrameSubmissionV1<'a, Component> {
    version: u8,
    replay: &'a [CanonicalInputItem],
    staged_inputs: &'a [CanonicalInputItem],
    component: Component,
}

pub(super) struct FrameMeter;

impl FrameMeter {
    pub(super) fn compile_component(
        items: Vec<CanonicalInputItem>,
        tool_definitions: Vec<ToolDefinition>,
        constraints: &FrameConstraints,
    ) -> Result<CompiledComponent, FrameBudgetFault> {
        validate_canonical_items(&items)?;
        let tools = ToolCatalog::from_definitions(tool_definitions)?;
        let projection = ProjectionSubmission::new(items);
        let canonical_bytes = canonical_component_bytes(&projection, &tools)?;
        if canonical_bytes.len() > constraints.max_component_bytes {
            return Err(FrameBudgetFault::ComponentTooLarge {
                actual_bytes: canonical_bytes.len(),
                max_component_bytes: constraints.max_component_bytes,
            });
        }
        Ok(CompiledComponent {
            projection,
            tools,
            canonical_bytes,
        })
    }

    pub(super) fn compile_submission(
        replay: Vec<CanonicalInputItem>,
        staged_inputs: Vec<CanonicalInputItem>,
        component: CompiledComponent,
        constraints: &FrameConstraints,
    ) -> Result<FrameSubmission, FrameBudgetFault> {
        validate_canonical_items(&replay)?;
        validate_canonical_items(&staged_inputs)?;
        let canonical_bytes = canonical_frame_bytes(
            &replay,
            &staged_inputs,
            &component.projection,
            &component.tools,
        )?;
        if canonical_bytes.len() > constraints.max_frame_bytes {
            return Err(FrameBudgetFault::FrameTooLarge {
                actual_bytes: canonical_bytes.len(),
                max_frame_bytes: constraints.max_frame_bytes,
            });
        }
        Ok(FrameSubmission::from_compiled(
            replay,
            staged_inputs,
            component.projection,
            component.tools,
            canonical_bytes,
        ))
    }

    /// Proves that a future Full can carry the actual canonical state while
    /// preserving the complete Component authoring envelope.
    pub(super) fn validate_full_reserve(
        replay: &[CanonicalInputItem],
        staged_inputs: &[CanonicalInputItem],
        constraints: &FrameConstraints,
    ) -> Result<(), FrameBudgetFault> {
        validate_canonical_items(replay)?;
        validate_canonical_items(staged_inputs)?;
        let empty = FrameSubmissionV1 {
            version: FRAME_METER_VERSION,
            replay,
            staged_inputs,
            component: Option::<()>::None,
        };
        let null_component_bytes = canonicalize(&empty)?;
        let full_non_component_bytes = null_component_bytes
            .len()
            .checked_sub(JSON_NULL_BYTES)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        let required_full_bytes = full_non_component_bytes
            .checked_add(constraints.max_component_bytes)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        if required_full_bytes > constraints.max_frame_bytes {
            return Err(FrameBudgetFault::FullReserveTooLarge {
                required_full_bytes,
                max_frame_bytes: constraints.max_frame_bytes,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct CanonicalHistoryState {
    pub(super) transcript: CanonicalTranscript,
}

#[derive(Clone)]
struct FrameCheckpoint {
    revision: FrameRevision,
    complete_projection: RenderedProjection,
    system_snapshot: Option<ResolvedDocument>,
    diff_baseline: ProjectionDiffState,
    reconciliation: ProjectionReconciliationState,
    replay_basis: Vec<CanonicalInputItem>,
}

#[derive(Clone)]
pub(super) struct TargetDeliveryState {
    namespace: NonZeroU128,
    identity: TargetIdentity,
    profile: FrameProfile,
    highest_epoch: TargetEpoch,
    next_sequence: Option<NonZeroU64>,
    pub(super) committed_revision: Option<FrameRevision>,
    checkpoint: Option<FrameCheckpoint>,
    pub(super) tool_outputs: ToolOutputStaging,
}

/// Private owner of one canonical history and one fixed target delivery cursor.
pub(super) struct FrameSession {
    pub(super) canonical_history: CanonicalHistoryState,
    pub(super) target_delivery: TargetDeliveryState,
    highest_observed_epoch: TargetEpoch,
}

impl FrameSession {
    pub(super) fn new(declaration: &TargetDeclaration) -> Result<Self, FrameSessionFault> {
        let namespace = NEXT_FRAME_SESSION_NAMESPACE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map(|value| {
                NonZeroU128::new(u128::from(value))
                    .expect("FrameSession namespace counter starts at one")
            })
            .map_err(|_| FrameSessionFault::NamespaceExhausted)?;
        Ok(Self {
            canonical_history: CanonicalHistoryState::default(),
            highest_observed_epoch: declaration.continuity().epoch(),
            target_delivery: TargetDeliveryState {
                namespace,
                identity: declaration.identity(),
                profile: declaration.profile().clone(),
                highest_epoch: declaration.continuity().epoch(),
                next_sequence: NonZeroU64::new(1),
                committed_revision: None,
                checkpoint: None,
                tool_outputs: ToolOutputStaging::default(),
            },
        })
    }

    pub(super) fn prepare(
        &self,
        declaration: &TargetDeclaration,
        complete_projection: &RenderedProjection,
    ) -> Result<PreparedFrame, FrameSessionFault> {
        self.validate_declaration(declaration)?;
        let epoch = declaration.continuity().epoch();
        let sequence = self
            .target_delivery
            .next_sequence
            .ok_or(FrameSessionFault::RevisionExhausted)?;
        let revision = FrameRevision::new(
            self.target_delivery.namespace,
            self.target_delivery.identity,
            epoch,
            sequence,
        );

        let replay_view = CompleteTranscriptPolicy::project(&self.canonical_history.transcript);
        let system_snapshot = normalized_system_snapshot(complete_projection);
        let retained_checkpoint = self.target_delivery.checkpoint.as_ref();
        if let Some(checkpoint) = retained_checkpoint {
            if !replay_view.starts_with(&checkpoint.replay_basis) {
                return Err(FrameSessionFault::ReplayReplacementUnsupported);
            }
        }
        let reconciliation_checkpoint = retained_checkpoint;
        let compatible_checkpoint = reconciliation_checkpoint.filter(|checkpoint| {
            checkpoint.complete_projection.execution_scope()
                == complete_projection.execution_scope()
        });
        let delta_checkpoint = self
            .delta_checkpoint(declaration, compatible_checkpoint)
            .filter(|checkpoint| checkpoint.system_snapshot.as_ref() == system_snapshot.as_ref());
        let basis = delta_checkpoint
            .map(|checkpoint| FrameBasis::DeltaFrom(checkpoint.revision))
            .unwrap_or(FrameBasis::Full);
        let (staged_inputs, staging_candidate) = self
            .target_delivery
            .tool_outputs
            .prepare_frame_candidate()?;
        let mut newly_unclaimed_outputs = reconciliation_checkpoint
            .map(|checkpoint| replay_view[checkpoint.replay_basis.len()..].to_vec())
            .unwrap_or_default();
        newly_unclaimed_outputs.extend(staged_inputs.iter().cloned());
        let reset_reconciliation;
        let previous_reconciliation = match compatible_checkpoint {
            Some(checkpoint) => Some(&checkpoint.reconciliation),
            None => {
                reset_reconciliation = retained_checkpoint.map(|checkpoint| {
                    checkpoint
                        .reconciliation
                        .reset_authored_for_scope(&newly_unclaimed_outputs)
                });
                reset_reconciliation.as_ref()
            }
        };
        let current_scope_outputs =
            if retained_checkpoint.is_some() && compatible_checkpoint.is_none() {
                &[][..]
            } else {
                newly_unclaimed_outputs.as_slice()
            };
        let projection_plan = ReconciledProjectionPlan::prepare(
            delta_checkpoint.map(|checkpoint| &checkpoint.diff_baseline),
            previous_reconciliation,
            current_scope_outputs,
            complete_projection,
        )?;
        let canonical_projection_items = flatten_non_system_projection(&projection_plan.submission);
        let mut projection_items = canonical_projection_items.clone();
        let mut complete_component_items = flatten_non_system_projection(complete_projection);
        if let Some(snapshot) = system_snapshot.as_ref() {
            complete_component_items.insert(0, system_snapshot_item(snapshot));
            if delta_checkpoint.is_none() {
                projection_items.insert(0, system_snapshot_item(snapshot));
            }
        }
        let constraints = &self.target_delivery.profile.constraints;

        // The authoring envelope is always measured from the complete
        // projection, even when this exact submission is a much smaller Delta.
        FrameMeter::compile_component(
            complete_component_items,
            complete_projection.native_tools().to_vec(),
            constraints,
        )?;
        let component = FrameMeter::compile_component(
            projection_items.clone(),
            complete_projection.native_tools().to_vec(),
            constraints,
        )?;

        let replay = match delta_checkpoint {
            Some(checkpoint) => replay_view[checkpoint.replay_basis.len()..].to_vec(),
            None => replay_view.to_vec(),
        };

        ensure_pending_tool_calls_closed(replay_view, &staged_inputs)?;
        let mut committed_items = replay_view.to_vec();
        committed_items.extend(staged_inputs.iter().cloned());
        committed_items.extend(canonical_projection_items);
        let committed_transcript = CanonicalTranscript::try_from_items(committed_items)?;
        FrameMeter::validate_full_reserve(committed_transcript.items(), &[], constraints)?;
        let submission =
            FrameMeter::compile_submission(replay, staged_inputs, component, constraints)?;
        let frame = Frame::from_compiled(
            revision,
            declaration.identity(),
            epoch,
            declaration.continuity().clone(),
            declaration.profile().clone(),
            basis,
            submission,
        )?;

        let replay_basis = committed_transcript.items().to_vec();
        let checkpoint = FrameCheckpoint {
            revision,
            complete_projection: complete_projection.clone(),
            system_snapshot,
            diff_baseline: projection_plan.diff_baseline,
            reconciliation: projection_plan.reconciliation,
            replay_basis,
        };
        let next_sequence = sequence.get().checked_add(1).and_then(NonZeroU64::new);
        let target_delivery = TargetDeliveryState {
            namespace: self.target_delivery.namespace,
            identity: self.target_delivery.identity,
            profile: self.target_delivery.profile.clone(),
            highest_epoch: epoch,
            next_sequence,
            committed_revision: Some(revision),
            checkpoint: Some(checkpoint),
            tool_outputs: staging_candidate,
        };
        Ok(PreparedFrame {
            frame,
            commit: FrameCommitCandidate {
                canonical_history: CanonicalHistoryState {
                    transcript: committed_transcript,
                },
                target_delivery,
            },
        })
    }

    pub(super) fn commit(&mut self, candidate: FrameCommitCandidate) {
        self.canonical_history = candidate.canonical_history;
        self.target_delivery = candidate.target_delivery;
        self.highest_observed_epoch = self
            .highest_observed_epoch
            .max(self.target_delivery.highest_epoch);
    }

    pub(super) fn can_reset_model_context(&self) -> bool {
        self.target_delivery
            .tool_outputs
            .reserve_outputs()
            .next()
            .is_none()
            && ensure_pending_tool_calls_closed(self.canonical_history.transcript.items(), &[])
                .is_ok()
    }

    /// Rebase the next submission on the complete authored projection, while
    /// preserving this mount's target identity and monotonic revision sequence.
    pub(super) fn reset_model_context(
        &mut self,
        declaration: &TargetDeclaration,
    ) -> Result<(), FrameSessionFault> {
        self.validate_declaration(declaration)?;
        if !matches!(
            declaration.continuity(),
            TargetContinuity::FullRequired { .. }
        ) || declaration.continuity().epoch() <= self.highest_observed_epoch
        {
            return Err(FrameSessionFault::InvalidModelContextReset);
        }
        debug_assert!(self.can_reset_model_context());
        self.canonical_history = CanonicalHistoryState::default();
        self.target_delivery.checkpoint = None;
        self.target_delivery.committed_revision = None;
        self.target_delivery.tool_outputs = ToolOutputStaging::default();
        self.target_delivery.highest_epoch = declaration.continuity().epoch();
        self.highest_observed_epoch = declaration.continuity().epoch();
        Ok(())
    }

    /// Returns the mount-stable proof capability used by canonical admission.
    ///
    /// The capability owns only immutable budget constraints. It cannot inspect
    /// or advance the target delivery cursor, so fact admission remains
    /// independent from Full/Delta selection.
    pub(super) fn full_reserve_budget(&self) -> FullReserveBudget {
        FullReserveBudget {
            constraints: self.target_delivery.profile.constraints.clone(),
        }
    }

    pub(super) fn validate_declaration(
        &self,
        declaration: &TargetDeclaration,
    ) -> Result<(), FrameSessionFault> {
        if declaration.identity() != self.target_delivery.identity {
            return Err(FrameSessionFault::TargetIdentityChanged);
        }
        if declaration.profile() != &self.target_delivery.profile {
            return Err(FrameSessionFault::FrameProfileChanged);
        }
        let observed = declaration.continuity().epoch();
        if observed < self.highest_observed_epoch {
            return Err(FrameSessionFault::EpochRegressed {
                highest: self.highest_observed_epoch,
                observed,
            });
        }
        let highest_committed = self.target_delivery.highest_epoch;
        if observed > highest_committed
            && matches!(declaration.continuity(), TargetContinuity::Accepted { .. })
        {
            return Err(FrameSessionFault::AcceptedRevisionInNewEpoch {
                highest: highest_committed,
                observed,
            });
        }
        if observed == highest_committed
            && self.target_delivery.committed_revision.is_some()
            && matches!(
                declaration.continuity(),
                TargetContinuity::FullRequired { .. }
            )
        {
            return Err(FrameSessionFault::ContinuityResetWithoutEpochAdvance { epoch: observed });
        }
        Ok(())
    }

    /// Validate and remember a declaration epoch independently of handoff.
    ///
    /// Once the runtime has observed a higher epoch, a pre-handoff failure must
    /// not allow a later declaration to regress to an older target session.
    pub(super) fn observe_declaration(
        &mut self,
        declaration: &TargetDeclaration,
    ) -> Result<(), FrameSessionFault> {
        self.validate_declaration(declaration)?;
        self.highest_observed_epoch = self
            .highest_observed_epoch
            .max(declaration.continuity().epoch());
        Ok(())
    }

    fn delta_checkpoint<'a>(
        &'a self,
        declaration: &TargetDeclaration,
        compatible: Option<&'a FrameCheckpoint>,
    ) -> Option<&'a FrameCheckpoint> {
        if !self
            .target_delivery
            .profile
            .capabilities
            .supports_semantic_delta()
        {
            return None;
        }
        let accepted = declaration.continuity().accepted_revision()?;
        if self.target_delivery.committed_revision != Some(accepted) {
            return None;
        }
        compatible.filter(|checkpoint| checkpoint.revision == accepted)
    }
}

/// Mount-stable validator for the hypothetical next Full submission.
#[derive(Clone)]
pub(super) struct FullReserveBudget {
    constraints: FrameConstraints,
}

impl FullReserveBudget {
    pub(super) fn validate(
        &self,
        transcript: &CanonicalTranscript,
        staging: &ToolOutputStaging,
    ) -> Result<(), FrameBudgetFault> {
        let staged_inputs = staging.reserve_outputs().cloned().collect::<Vec<_>>();
        FrameMeter::validate_full_reserve(transcript.items(), &staged_inputs, &self.constraints)
    }

    pub(super) fn begin(
        &self,
        transcript: &CanonicalTranscript,
        staging: &ToolOutputStaging,
    ) -> Result<FullReserveTracker, FrameBudgetFault> {
        let replay_item_bytes = canonical_items_total(transcript.items())?;
        let staged = staging.reserve_outputs().collect::<Vec<_>>();
        let staged_item_bytes = canonical_item_refs_total(&staged)?;
        let usage = FullReserveUsage {
            replay_count: transcript.items().len(),
            replay_item_bytes,
            staged_count: staged.len(),
            staged_item_bytes,
        };
        self.validate_usage(usage)?;
        Ok(FullReserveTracker {
            budget: self.clone(),
            usage,
        })
    }

    pub(super) fn canonical_item_bytes(
        &self,
        item: &CanonicalInputItem,
    ) -> Result<usize, FrameBudgetFault> {
        validate_canonical_items(std::slice::from_ref(item))?;
        Ok(canonicalize(item)?.len())
    }

    pub(super) fn canonical_string_content_bytes(
        &self,
        value: &str,
    ) -> Result<usize, FrameBudgetFault> {
        canonicalize(&value)?
            .len()
            .checked_sub(2)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)
    }

    fn validate_usage(&self, usage: FullReserveUsage) -> Result<(), FrameBudgetFault> {
        let replay_extra = canonical_array_extra(usage.replay_count, usage.replay_item_bytes)?;
        let staged_extra = canonical_array_extra(usage.staged_count, usage.staged_item_bytes)?;
        let empty = FrameSubmissionV1 {
            version: FRAME_METER_VERSION,
            replay: &[],
            staged_inputs: &[],
            component: Option::<()>::None,
        };
        let empty_bytes = canonicalize(&empty)?;
        let required_full_bytes = empty_bytes
            .len()
            .checked_sub(JSON_NULL_BYTES)
            .and_then(|bytes| bytes.checked_add(self.constraints.max_component_bytes))
            .and_then(|bytes| bytes.checked_add(replay_extra))
            .and_then(|bytes| bytes.checked_add(staged_extra))
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        if required_full_bytes > self.constraints.max_frame_bytes {
            return Err(FrameBudgetFault::FullReserveTooLarge {
                required_full_bytes,
                max_frame_bytes: self.constraints.max_frame_bytes,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct FullReserveUsage {
    replay_count: usize,
    replay_item_bytes: usize,
    staged_count: usize,
    staged_item_bytes: usize,
}

#[derive(Clone, Copy)]
pub(super) struct FullReserveCandidate {
    usage: FullReserveUsage,
    item_bytes: usize,
}

impl FullReserveCandidate {
    pub(super) const fn item_bytes(self) -> usize {
        self.item_bytes
    }
}

pub(super) struct FullReserveTracker {
    budget: FullReserveBudget,
    usage: FullReserveUsage,
}

impl FullReserveTracker {
    pub(super) fn budget(&self) -> &FullReserveBudget {
        &self.budget
    }

    pub(super) fn prepare_replay_item(
        &self,
        previous_item_bytes: Option<usize>,
        item_bytes: usize,
    ) -> Result<FullReserveCandidate, FrameBudgetFault> {
        let mut usage = self.usage;
        usage.replay_item_bytes =
            replace_contribution(usage.replay_item_bytes, previous_item_bytes, item_bytes)?;
        if previous_item_bytes.is_none() {
            usage.replay_count = usage
                .replay_count
                .checked_add(1)
                .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        }
        self.budget.validate_usage(usage)?;
        Ok(FullReserveCandidate { usage, item_bytes })
    }

    pub(super) fn prepare_staged_item(
        &self,
        item_bytes: usize,
    ) -> Result<FullReserveCandidate, FrameBudgetFault> {
        let mut usage = self.usage;
        usage.staged_item_bytes = usage
            .staged_item_bytes
            .checked_add(item_bytes)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        usage.staged_count = usage
            .staged_count
            .checked_add(1)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        self.budget.validate_usage(usage)?;
        Ok(FullReserveCandidate { usage, item_bytes })
    }

    pub(super) fn prepare_replay_and_staged_item(
        &self,
        replay_item_bytes: usize,
        staged_item_bytes: usize,
    ) -> Result<FullReserveCandidate, FrameBudgetFault> {
        let mut usage = self.usage;
        usage.replay_item_bytes = usage
            .replay_item_bytes
            .checked_add(replay_item_bytes)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        usage.replay_count = usage
            .replay_count
            .checked_add(1)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        usage.staged_item_bytes = usage
            .staged_item_bytes
            .checked_add(staged_item_bytes)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        usage.staged_count = usage
            .staged_count
            .checked_add(1)
            .ok_or(FrameBudgetFault::ArithmeticOverflow)?;
        self.budget.validate_usage(usage)?;
        Ok(FullReserveCandidate {
            usage,
            item_bytes: replay_item_bytes,
        })
    }

    pub(super) fn prepare_replace_staged_item(
        &self,
        previous_item_bytes: usize,
        next_item_bytes: usize,
    ) -> Result<FullReserveCandidate, FrameBudgetFault> {
        let mut usage = self.usage;
        usage.staged_item_bytes = replace_contribution(
            usage.staged_item_bytes,
            Some(previous_item_bytes),
            next_item_bytes,
        )?;
        self.budget.validate_usage(usage)?;
        Ok(FullReserveCandidate {
            usage,
            item_bytes: next_item_bytes,
        })
    }

    pub(super) fn commit(&mut self, candidate: FullReserveCandidate) {
        self.usage = candidate.usage;
    }
}

fn canonical_items_total(items: &[CanonicalInputItem]) -> Result<usize, FrameBudgetFault> {
    validate_canonical_items(items)?;
    canonical_item_refs_total(&items.iter().collect::<Vec<_>>())
}

fn canonical_item_refs_total(items: &[&CanonicalInputItem]) -> Result<usize, FrameBudgetFault> {
    items.iter().try_fold(0_usize, |total, item| {
        total
            .checked_add(canonicalize(*item)?.len())
            .ok_or(FrameBudgetFault::ArithmeticOverflow)
    })
}

fn canonical_array_extra(count: usize, item_bytes: usize) -> Result<usize, FrameBudgetFault> {
    if count == 0 {
        return Ok(0);
    }
    item_bytes
        .checked_add(count - 1)
        .ok_or(FrameBudgetFault::ArithmeticOverflow)
}

fn replace_contribution(
    total: usize,
    previous: Option<usize>,
    next: usize,
) -> Result<usize, FrameBudgetFault> {
    total
        .checked_sub(previous.unwrap_or(0))
        .and_then(|total| total.checked_add(next))
        .ok_or(FrameBudgetFault::ArithmeticOverflow)
}

struct CompleteTranscriptPolicy;

impl CompleteTranscriptPolicy {
    fn project(transcript: &CanonicalTranscript) -> &[CanonicalInputItem] {
        transcript.items()
    }
}

pub(super) struct FrameCommitCandidate {
    canonical_history: CanonicalHistoryState,
    target_delivery: TargetDeliveryState,
}

pub(super) struct PreparedFrame {
    frame: Frame,
    commit: FrameCommitCandidate,
}

impl PreparedFrame {
    pub(super) fn into_parts(self) -> (Frame, FrameCommitCandidate) {
        (self.frame, self.commit)
    }
}

fn flatten_non_system_projection(projection: &RenderedProjection) -> Vec<CanonicalInputItem> {
    projection
        .nodes()
        .iter()
        .flat_map(|node| node.items().iter().cloned())
        .filter(|item| !is_system_instruction(item))
        .collect()
}

fn normalized_system_snapshot(projection: &RenderedProjection) -> Option<ResolvedDocument> {
    let mut children = BlockChildren::new();
    for item in projection
        .nodes()
        .iter()
        .flat_map(|node| node.items().iter())
    {
        let CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            pom,
        } = item
        else {
            continue;
        };
        children.extend(pom.children().clone());
    }
    (!children.is_empty()).then(|| ResolvedDocument::new(children))
}

fn system_snapshot_item(snapshot: &ResolvedDocument) -> CanonicalInputItem {
    CanonicalInputItem::instruction(InstructionAuthority::System, snapshot.clone())
}

fn is_system_instruction(item: &CanonicalInputItem) -> bool {
    matches!(
        item,
        CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            ..
        }
    )
}

fn ensure_pending_tool_calls_closed(
    replay: &[CanonicalInputItem],
    staged_inputs: &[CanonicalInputItem],
) -> Result<(), FrameSessionFault> {
    let mut pending = VecDeque::new();
    for item in replay.iter().chain(staged_inputs) {
        match item {
            CanonicalInputItem::ToolCall { call_id, .. } => pending.push_back(call_id.as_str()),
            CanonicalInputItem::ToolResult { call_id, .. }
                if pending.front().copied() == Some(call_id.as_str()) =>
            {
                pending.pop_front();
            }
            _ => {}
        }
    }
    if let Some(call_id) = pending.front() {
        return Err(FrameSessionFault::PendingToolCall {
            call_id: (*call_id).to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(super) enum FrameSessionFault {
    #[error("FrameSession namespace identity space is exhausted")]
    NamespaceExhausted,
    #[error("reaction target identity changed within one Application")]
    TargetIdentityChanged,
    #[error("Frame profile changed within one Application mount")]
    FrameProfileChanged,
    #[error("target epoch regressed from {highest:?} to {observed:?}")]
    EpochRegressed {
        highest: TargetEpoch,
        observed: TargetEpoch,
    },
    #[error("target declared an accepted revision in new epoch {observed:?} after {highest:?}")]
    AcceptedRevisionInNewEpoch {
        highest: TargetEpoch,
        observed: TargetEpoch,
    },
    #[error("target required Full in epoch {epoch:?} without advancing its continuity epoch")]
    ContinuityResetWithoutEpochAdvance { epoch: TargetEpoch },
    #[error("canonical replay replacement is unsupported by the v1 FrameSession")]
    ReplayReplacementUnsupported,
    #[error("model context reset did not establish a higher epoch requiring Full")]
    InvalidModelContextReset,
    #[error(transparent)]
    ProjectionReconciliation(#[from] ProjectionReconciliationFault),
    #[error("Frame revision identity space is exhausted")]
    RevisionExhausted,
    #[error("canonical ToolCall `{call_id}` has no staged ToolOutput")]
    PendingToolCall { call_id: String },
    #[error(transparent)]
    ToolOutput(#[from] ToolOutputStagingFault),
    #[error(transparent)]
    Canonical(#[from] CanonicalTranscriptError),
    #[error(transparent)]
    Budget(#[from] FrameBudgetFault),
    #[error(transparent)]
    Invariant(#[from] FrameInvariantFault),
}

pub(super) struct CompiledComponent {
    projection: ProjectionSubmission,
    tools: ToolCatalog,
    canonical_bytes: Vec<u8>,
}

impl CompiledComponent {
    #[cfg(test)]
    fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
}

fn canonical_component_bytes(
    projection: &ProjectionSubmission,
    tools: &ToolCatalog,
) -> Result<Vec<u8>, FrameBudgetFault> {
    canonicalize(&component_envelope(projection, tools))
}

fn canonical_frame_bytes(
    replay: &[CanonicalInputItem],
    staged_inputs: &[CanonicalInputItem],
    projection: &ProjectionSubmission,
    tools: &ToolCatalog,
) -> Result<Vec<u8>, FrameBudgetFault> {
    canonicalize(&FrameSubmissionV1 {
        version: FRAME_METER_VERSION,
        replay,
        staged_inputs,
        component: component_envelope(projection, tools),
    })
}

fn component_envelope<'a>(
    projection: &'a ProjectionSubmission,
    tools: &'a ToolCatalog,
) -> ComponentEnvelopeV1<'a> {
    ComponentEnvelopeV1 {
        version: FRAME_METER_VERSION,
        projection: ProjectionSubmissionV1 {
            items: projection.items(),
        },
        tools: tools
            .definitions()
            .iter()
            .map(|definition| ToolCatalogEntryV1 {
                name: definition.name(),
                description: definition.description(),
                input_schema: definition.input_schema(),
                strict: definition.strict(),
            })
            .collect(),
    }
}

fn canonicalize(value: &impl Serialize) -> Result<Vec<u8>, FrameBudgetFault> {
    serde_json_canonicalizer::to_vec(value).map_err(|error| FrameBudgetFault::Canonicalization {
        message: error.to_string(),
    })
}

fn validate_canonical_items(items: &[CanonicalInputItem]) -> Result<(), FrameBudgetFault> {
    for item in items {
        if let CanonicalInputItem::ProviderExtension(extension) = item {
            validate_i_json_value(extension.payload())?;
        }
    }
    Ok(())
}

fn validate_i_json_value(value: &Value) -> Result<(), FrameBudgetFault> {
    const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
    match value {
        Value::Number(number) => {
            let valid = if let Some(value) = number.as_i64() {
                value.unsigned_abs() <= MAX_SAFE_INTEGER
            } else if let Some(value) = number.as_u64() {
                value <= MAX_SAFE_INTEGER
            } else {
                number.as_f64().is_some_and(f64::is_finite)
            };
            if !valid {
                return Err(FrameBudgetFault::NonIJsonNumber {
                    value: number.to_string(),
                });
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_i_json_value(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_i_json_value(value)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum FrameBudgetFault {
    #[error(transparent)]
    ToolCatalog(#[from] super::reaction::ToolCatalogFault),
    #[error("canonical Frame contains non-I-JSON number `{value}`")]
    NonIJsonNumber { value: String },
    #[error("canonical Frame meter failed: {message}")]
    Canonicalization { message: String },
    #[error(
        "complete Component envelope requires {actual_bytes} bytes but max_component_bytes is {max_component_bytes}"
    )]
    ComponentTooLarge {
        actual_bytes: usize,
        max_component_bytes: usize,
    },
    #[error("Frame requires {actual_bytes} bytes but max_frame_bytes is {max_frame_bytes}")]
    FrameTooLarge {
        actual_bytes: usize,
        max_frame_bytes: usize,
    },
    #[error(
        "next Full reserve requires {required_full_bytes} bytes but max_frame_bytes is {max_frame_bytes}"
    )]
    FullReserveTooLarge {
        required_full_bytes: usize,
        max_frame_bytes: usize,
    },
    #[error("canonical Frame budget arithmetic overflowed")]
    ArithmeticOverflow,
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU128, NonZeroU64};

    use crate::{
        component::execution::{
            admission::ReactionAdmissionGuard,
            port::ProjectionExecutionScope,
            reaction::{
                FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
                ProviderFact, ProviderOutputKey, ProviderToolCall, TargetContinuity,
                TargetDeclaration, TargetEpoch, TargetIdentity,
            },
            ProviderEvent, RenderedProjection, RenderedProjectionNode, ToolDefinition,
        },
        pom::{Document, TextNode, XmlNode},
        pom_renderer::render_pom_document,
        pom_resolution::resolve_system_document,
        transcript::{
            AssistantPhase, CanonicalInputItem, CanonicalTranscript, InstructionAuthority,
        },
    };

    use super::{
        canonical_items_total, FrameBudgetFault, FrameMeter, FrameSession, FrameSessionFault,
        FullReserveBudget, ProjectionReconciliationFault,
    };

    fn constraints(max_component_bytes: usize, max_frame_bytes: usize) -> FrameConstraints {
        FrameConstraints {
            max_frame_bytes,
            max_component_bytes,
            context_window_tokens: None,
            reserved_output_tokens: None,
        }
    }

    fn profile(delta: bool) -> FrameProfile {
        FrameProfile::new(constraints(2_048, 16_384), FrameCapabilities::new(delta))
    }

    fn identity() -> TargetIdentity {
        TargetIdentity::new(NonZeroU128::new(41).unwrap())
    }

    fn epoch(value: u64) -> TargetEpoch {
        TargetEpoch::new(NonZeroU64::new(value).unwrap())
    }

    fn full(profile: FrameProfile, epoch_value: u64) -> TargetDeclaration {
        TargetDeclaration::full(identity(), epoch(epoch_value), profile)
    }

    fn projection(text: Option<&str>) -> RenderedProjection {
        let items = text
            .map(|text| vec![CanonicalInputItem::assistant_text(text, None)])
            .unwrap_or_default();
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", items)]).unwrap()
    }

    fn projection_with_tools(tools: Vec<ToolDefinition>) -> RenderedProjection {
        RenderedProjection::with_native_tools(
            vec![RenderedProjectionNode::new("root", Vec::new())],
            tools,
        )
        .unwrap()
    }

    fn tool(name: &str, description: &str, input_schema: serde_json::Value) -> ToolDefinition {
        ToolDefinition::new(name, description, input_schema).unwrap()
    }

    fn system_fragment(name: &str, text: &str) -> CanonicalInputItem {
        let node = XmlNode::try_build(name, |children| {
            children.text(TextNode::new(text));
            Ok(())
        })
        .unwrap();
        CanonicalInputItem::instruction(
            InstructionAuthority::System,
            resolve_system_document(Document::from_xml(node)),
        )
    }

    fn projection_with_system(systems: &[(&str, &str)], ordinary: &[&str]) -> RenderedProjection {
        let system_nodes = systems.iter().enumerate().map(|(index, (name, text))| {
            RenderedProjectionNode::new(
                format!("system-{index}"),
                vec![system_fragment(name, text)],
            )
        });
        let ordinary = RenderedProjectionNode::new(
            "ordinary",
            ordinary
                .iter()
                .map(|text| CanonicalInputItem::assistant_text(*text, None))
                .collect(),
        );
        RenderedProjection::from_nodes(system_nodes.chain([ordinary]).collect()).unwrap()
    }

    fn rendered_system(item: &CanonicalInputItem) -> &crate::pom::ResolvedDocument {
        let CanonicalInputItem::Instruction {
            authority: InstructionAuthority::System,
            pom,
        } = item
        else {
            panic!("expected one normalized System snapshot")
        };
        pom
    }

    fn scoped_projection(items: &[&str], mount_generation: u64) -> RenderedProjection {
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "root",
            items
                .iter()
                .map(|text| CanonicalInputItem::assistant_text(*text, None))
                .collect(),
        )])
        .unwrap()
        .with_execution_scope(ProjectionExecutionScope {
            host_instance: 1,
            mount_generation,
        })
    }

    fn commit_first(
        session: &mut FrameSession,
        declaration: &TargetDeclaration,
        projection: &RenderedProjection,
    ) -> FrameRevision {
        let prepared = session.prepare(declaration, projection).unwrap();
        let (frame, commit) = prepared.into_parts();
        let revision = frame.revision();
        session.commit(commit);
        revision
    }

    #[test]
    fn model_context_reset_rebases_history_and_preserves_revision_sequence() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        commit_first(&mut session, &declaration, &projection(Some("old-input")));
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(CanonicalInputItem::interrupted_assistant_text(
                "old-output",
                None,
            ))
            .unwrap();
        let next_sequence = session.target_delivery.next_sequence;
        let namespace = session.target_delivery.namespace;
        assert!(session.can_reset_model_context());

        let replacement = full(profile.clone(), 2);
        session.reset_model_context(&replacement).unwrap();
        assert!(session.canonical_history.transcript.items().is_empty());
        assert_eq!(session.target_delivery.next_sequence, next_sequence);
        assert_eq!(session.target_delivery.namespace, namespace);
        let complete = projection(Some("authoritative-history-and-new-input"));
        let prepared = session.prepare(&replacement, &complete).unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::Full);
        assert!(prepared.frame.submission().replay().is_empty());
        let (frame, candidate) = prepared.into_parts();
        let revision = frame.revision();
        session.commit(candidate);

        let next = session
            .prepare(&TargetDeclaration::resume(revision, profile), &complete)
            .unwrap();
        assert_eq!(next.frame.basis(), FrameBasis::DeltaFrom(revision));
        assert!(next.frame.submission().projection().items().is_empty());
    }

    #[test]
    fn model_context_reset_rejects_nonadvancing_declaration_without_mutation() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let revision = commit_first(&mut session, &declaration, &projection(Some("retained")));
        let before = session.canonical_history.transcript.clone();
        assert!(session.reset_model_context(&declaration).is_err());
        assert!(session
            .reset_model_context(&TargetDeclaration::resume(revision, profile))
            .is_err());
        assert_eq!(session.canonical_history.transcript, before);
        assert_eq!(session.target_delivery.committed_revision, Some(revision));
    }

    #[test]
    fn model_context_reset_does_not_discard_open_tool_calls() {
        let mut session = FrameSession::new(&full(profile(true), 1)).unwrap();
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(CanonicalInputItem::tool_call("pending-call", "lookup", "{}").unwrap())
            .unwrap();
        assert!(!session.can_reset_model_context());
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(CanonicalInputItem::tool_result("pending-call", "done").unwrap())
            .unwrap();
        assert!(session.can_reset_model_context());
    }

    #[test]
    fn empty_component_and_frame_have_stable_jcs_bytes() {
        let limits = constraints(1_024, 4_096);
        let component = FrameMeter::compile_component(Vec::new(), Vec::new(), &limits).unwrap();
        assert_eq!(
            component.canonical_bytes(),
            br#"{"projection":{"items":[]},"tools":[],"version":1}"#
        );
        let frame =
            FrameMeter::compile_submission(Vec::new(), Vec::new(), component, &limits).unwrap();
        assert_eq!(
            frame.canonical_bytes(),
            br#"{"component":{"projection":{"items":[]},"tools":[],"version":1},"replay":[],"staged_inputs":[],"version":1}"#
        );
    }

    #[test]
    fn tool_catalog_uses_jcs_utf16_order() {
        let limits = constraints(1_024, 4_096);
        let component = FrameMeter::compile_component(
            Vec::new(),
            vec![
                tool(
                    "\u{e000}",
                    "private-use",
                    serde_json::json!({"type": "object"}),
                ),
                tool(
                    "\u{10000}",
                    "supplementary",
                    serde_json::json!({"type": "object"}),
                ),
                tool("ascii", "ascii", serde_json::json!({"type": "object"})),
            ],
            &limits,
        )
        .unwrap();
        assert_eq!(
            component.tools.names(),
            &[
                "ascii".to_owned(),
                "\u{10000}".to_owned(),
                "\u{e000}".to_owned()
            ]
        );
    }

    #[test]
    fn full_and_delta_frames_carry_complete_changed_tool_definitions() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let initial = projection_with_tools(vec![tool(
            "lookup",
            "Look up the current status.",
            serde_json::json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
            }),
        )]);
        let prepared = session.prepare(&declaration, &initial).unwrap();
        let first_payload: serde_json::Value =
            serde_json::from_slice(prepared.frame.submission().canonical_bytes()).unwrap();
        assert_eq!(
            first_payload["component"]["tools"][0]["description"],
            "Look up the current status."
        );
        assert_eq!(
            first_payload["component"]["tools"][0]["input_schema"]["properties"]["id"]["type"],
            "string"
        );
        assert_eq!(first_payload["component"]["tools"][0]["strict"], false);
        let (first_frame, first_commit) = prepared.into_parts();
        let first_revision = first_frame.revision();
        session.commit(first_commit);

        let changed = projection_with_tools(vec![tool(
            "lookup",
            "Look up a status with an optional region.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "region": {"type": "string"},
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        )
        .with_strict(true)]);
        let resumed = TargetDeclaration::resume(first_revision, profile);
        let changed = session.prepare(&resumed, &changed).unwrap();
        assert_eq!(changed.frame.basis(), FrameBasis::DeltaFrom(first_revision));
        let changed_payload: serde_json::Value =
            serde_json::from_slice(changed.frame.submission().canonical_bytes()).unwrap();
        assert_eq!(
            changed_payload["component"]["tools"][0]["description"],
            "Look up a status with an optional region."
        );
        assert_eq!(
            changed_payload["component"]["tools"][0]["input_schema"]["properties"]["region"]
                ["type"],
            "string"
        );
        assert_eq!(changed_payload["component"]["tools"][0]["strict"], true);
    }

    #[test]
    fn tool_schema_bytes_count_against_component_budget() {
        let definition = tool(
            "search",
            &"Search ".repeat(256),
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "x".repeat(512)}},
                "required": ["query"],
            }),
        );
        let generous = constraints(8 * 1024, 16 * 1024);
        let component =
            FrameMeter::compile_component(Vec::new(), vec![definition.clone()], &generous).unwrap();
        let exact = component.canonical_bytes().len();
        let too_small = constraints(exact - 1, 16 * 1024);
        assert!(matches!(
            FrameMeter::compile_component(Vec::new(), vec![definition], &too_small),
            Err(FrameBudgetFault::ComponentTooLarge {
                actual_bytes,
                max_component_bytes,
            }) if actual_bytes == exact && max_component_bytes == exact - 1
        ));
    }

    #[test]
    fn interrupted_status_and_unicode_are_part_of_exact_frame_meter() {
        let limits = constraints(2_048, 4_096);
        let component = FrameMeter::compile_component(
            vec![CanonicalInputItem::interrupted_assistant_text(
                "partial \u{1f642}",
                Some(AssistantPhase::Commentary),
            )],
            Vec::new(),
            &limits,
        )
        .unwrap();
        let encoded = String::from_utf8(component.canonical_bytes().to_vec()).unwrap();
        assert!(encoded.contains(r#""status":"interrupted""#));
        assert!(encoded.contains("partial \u{1f642}"));
    }

    #[test]
    fn component_and_frame_limits_are_independent() {
        let tiny_component = constraints(8, 4_096);
        assert!(matches!(
            FrameMeter::compile_component(Vec::new(), Vec::new(), &tiny_component),
            Err(FrameBudgetFault::ComponentTooLarge { .. })
        ));

        let limits = constraints(1_024, 100);
        let component = FrameMeter::compile_component(Vec::new(), Vec::new(), &limits).unwrap();
        assert!(matches!(
            FrameMeter::compile_submission(Vec::new(), Vec::new(), component, &limits),
            Err(FrameBudgetFault::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn full_reserve_uses_actual_history_and_component_envelope() {
        let replay = vec![CanonicalInputItem::assistant_text("retained", None)];
        let generous = constraints(128, 1_024);
        FrameMeter::validate_full_reserve(&replay, &[], &generous).unwrap();

        let exact_non_component = serde_json_canonicalizer::to_vec(&serde_json::json!({
            "component": null,
            "replay": replay,
            "staged_inputs": [],
            "version": 1,
        }))
        .unwrap()
        .len()
            - 4;
        let required_full_bytes = exact_non_component + 128;
        let too_small = constraints(128, required_full_bytes - 1);
        assert_eq!(
            FrameMeter::validate_full_reserve(&replay, &[], &too_small),
            Err(FrameBudgetFault::FullReserveTooLarge {
                required_full_bytes,
                max_frame_bytes: required_full_bytes - 1,
            })
        );
    }

    #[test]
    fn incremental_full_reserve_tracker_matches_exact_jcs_meter() {
        const COMPONENT_BYTES: usize = 257;
        let first_text = CanonicalInputItem::interrupted_assistant_text(
            "quote: \"",
            Some(AssistantPhase::Commentary),
        );
        let fragment = "\nemoji: \u{1f642}\\";
        let complete_text = format!("quote: \"{fragment}");
        let interrupted = CanonicalInputItem::interrupted_assistant_text(
            &complete_text,
            Some(AssistantPhase::Commentary),
        );
        let sealed =
            CanonicalInputItem::assistant_text(&complete_text, Some(AssistantPhase::Commentary));
        let tool_call =
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"query":"value"}"#).unwrap();
        let tool_result = CanonicalInputItem::tool_result("call-1", "result").unwrap();
        let replay = vec![sealed.clone(), tool_call.clone()];
        let staged = vec![tool_result.clone()];

        let probe = constraints(COMPONENT_BYTES, 0);
        let exact_required = match FrameMeter::validate_full_reserve(&replay, &staged, &probe) {
            Err(FrameBudgetFault::FullReserveTooLarge {
                required_full_bytes,
                ..
            }) => required_full_bytes,
            result => panic!("expected exact Full reserve probe, got {result:?}"),
        };
        let exact_budget = FullReserveBudget {
            constraints: constraints(COMPONENT_BYTES, exact_required),
        };
        let transcript = CanonicalTranscript::new();
        let staging = super::ToolOutputStaging::default();
        let mut tracker = exact_budget.begin(&transcript, &staging).unwrap();

        let first_bytes = exact_budget.canonical_item_bytes(&first_text).unwrap();
        let candidate = tracker.prepare_replay_item(None, first_bytes).unwrap();
        tracker.commit(candidate);

        let fragment_bytes = exact_budget
            .canonical_string_content_bytes(fragment)
            .unwrap();
        let interrupted_bytes = exact_budget.canonical_item_bytes(&interrupted).unwrap();
        assert_eq!(first_bytes + fragment_bytes, interrupted_bytes);
        let candidate = tracker
            .prepare_replay_item(Some(first_bytes), interrupted_bytes)
            .unwrap();
        tracker.commit(candidate);

        let sealed_bytes = exact_budget.canonical_item_bytes(&sealed).unwrap();
        let candidate = tracker
            .prepare_replay_item(Some(interrupted_bytes), sealed_bytes)
            .unwrap();
        tracker.commit(candidate);

        let tool_call_bytes = exact_budget.canonical_item_bytes(&tool_call).unwrap();
        let candidate = tracker.prepare_replay_item(None, tool_call_bytes).unwrap();
        tracker.commit(candidate);

        let tool_result_bytes = exact_budget.canonical_item_bytes(&tool_result).unwrap();
        let candidate = tracker.prepare_staged_item(tool_result_bytes).unwrap();
        tracker.commit(candidate);

        assert_eq!(tracker.usage.replay_count, replay.len());
        assert_eq!(
            tracker.usage.replay_item_bytes,
            canonical_items_total(&replay).unwrap()
        );
        assert_eq!(tracker.usage.staged_count, staged.len());
        assert_eq!(
            tracker.usage.staged_item_bytes,
            canonical_items_total(&staged).unwrap()
        );
        FrameMeter::validate_full_reserve(
            &replay,
            &staged,
            &constraints(COMPONENT_BYTES, exact_required),
        )
        .unwrap();

        let too_small = FullReserveBudget {
            constraints: constraints(COMPONENT_BYTES, exact_required - 1),
        };
        let mut tracker = too_small.begin(&transcript, &staging).unwrap();
        for item in [&sealed, &tool_call] {
            let bytes = too_small.canonical_item_bytes(item).unwrap();
            let candidate = tracker.prepare_replay_item(None, bytes).unwrap();
            tracker.commit(candidate);
        }
        assert!(matches!(
            tracker.prepare_staged_item(tool_result_bytes),
            Err(FrameBudgetFault::FullReserveTooLarge { .. })
        ));
        assert!(matches!(
            FrameMeter::validate_full_reserve(
                &replay,
                &staged,
                &constraints(COMPONENT_BYTES, exact_required - 1),
            ),
            Err(FrameBudgetFault::FullReserveTooLarge { .. })
        ));
    }

    #[test]
    fn atomic_tool_call_reserve_tracks_replay_and_fallback() {
        const COMPONENT_BYTES: usize = 128;
        let tool_call =
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"query":"value"}"#).unwrap();
        let fallback = CanonicalInputItem::tool_result(
            "call-1",
            "Tool execution was cancelled; its outcome is unknown.",
        )
        .unwrap();
        let replay = vec![tool_call.clone()];
        let staged = vec![fallback.clone()];
        let probe = constraints(COMPONENT_BYTES, 0);
        let required_full_bytes = match FrameMeter::validate_full_reserve(&replay, &staged, &probe)
        {
            Err(FrameBudgetFault::FullReserveTooLarge {
                required_full_bytes,
                ..
            }) => required_full_bytes,
            result => panic!("expected exact Full reserve probe, got {result:?}"),
        };
        let budget = FullReserveBudget {
            constraints: constraints(COMPONENT_BYTES, required_full_bytes),
        };
        let transcript = CanonicalTranscript::new();
        let staging = super::ToolOutputStaging::default();
        let mut tracker = budget.begin(&transcript, &staging).unwrap();
        let tool_call_bytes = budget.canonical_item_bytes(&tool_call).unwrap();
        let fallback_bytes = budget.canonical_item_bytes(&fallback).unwrap();

        let candidate = tracker
            .prepare_replay_and_staged_item(tool_call_bytes, fallback_bytes)
            .unwrap();
        tracker.commit(candidate);
        assert_eq!(tracker.usage.replay_count, 1);
        assert_eq!(tracker.usage.replay_item_bytes, tool_call_bytes);
        assert_eq!(tracker.usage.staged_count, 1);
        assert_eq!(tracker.usage.staged_item_bytes, fallback_bytes);
        FrameMeter::validate_full_reserve(
            &replay,
            &staged,
            &constraints(COMPONENT_BYTES, required_full_bytes),
        )
        .unwrap();
    }

    #[test]
    fn replacing_a_staged_fallback_reservation_keeps_one_staged_slot() {
        const COMPONENT_BYTES: usize = 128;
        let tool_call =
            CanonicalInputItem::tool_call("call-1", "lookup", r#"{"query":"value"}"#).unwrap();
        let fallback = CanonicalInputItem::tool_result(
            "call-1",
            "Tool execution was cancelled; its outcome is unknown.",
        )
        .unwrap();
        let real_output = CanonicalInputItem::tool_result("call-1", "done").unwrap();
        let replay = vec![tool_call.clone()];
        let reserved_staged = vec![fallback.clone()];
        let staged = vec![real_output.clone()];
        let probe = constraints(COMPONENT_BYTES, 0);
        let required_full_bytes =
            match FrameMeter::validate_full_reserve(&replay, &reserved_staged, &probe) {
                Err(FrameBudgetFault::FullReserveTooLarge {
                    required_full_bytes,
                    ..
                }) => required_full_bytes,
                result => panic!("expected exact Full reserve probe, got {result:?}"),
            };
        let budget = FullReserveBudget {
            constraints: constraints(COMPONENT_BYTES, required_full_bytes),
        };
        let transcript = CanonicalTranscript::new();
        let staging = super::ToolOutputStaging::default();
        let mut tracker = budget.begin(&transcript, &staging).unwrap();
        let tool_call_bytes = budget.canonical_item_bytes(&tool_call).unwrap();
        let fallback_bytes = budget.canonical_item_bytes(&fallback).unwrap();
        let real_output_bytes = budget.canonical_item_bytes(&real_output).unwrap();
        let candidate = tracker
            .prepare_replay_and_staged_item(tool_call_bytes, fallback_bytes)
            .unwrap();
        tracker.commit(candidate);

        let replacement = tracker
            .prepare_replace_staged_item(fallback_bytes, real_output_bytes)
            .unwrap();
        tracker.commit(replacement);
        assert_eq!(tracker.usage.staged_count, 1);
        assert_eq!(tracker.usage.staged_item_bytes, real_output_bytes);
        FrameMeter::validate_full_reserve(
            &replay,
            &staged,
            &constraints(COMPONENT_BYTES, required_full_bytes),
        )
        .unwrap();
    }

    #[test]
    fn first_frame_is_full_and_prepare_is_side_effect_free_until_commit() {
        let declaration = full(profile(true), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(Some("first"));
        let prepared = session.prepare(&declaration, &complete).unwrap();

        assert_eq!(prepared.frame.basis(), FrameBasis::Full);
        assert!(prepared.frame.submission().replay().is_empty());
        assert_eq!(prepared.frame.submission().projection().items().len(), 1);
        assert!(session.canonical_history.transcript.items().is_empty());
        assert_eq!(session.target_delivery.committed_revision, None);

        let (frame, commit) = prepared.into_parts();
        session.commit(commit);
        assert_eq!(
            session.canonical_history.transcript.items(),
            &[CanonicalInputItem::assistant_text("first", None)]
        );
        assert_eq!(
            session.target_delivery.committed_revision,
            Some(frame.revision())
        );
    }

    #[test]
    fn full_normalizes_system_fragments_in_render_order_and_keeps_history_system_free() {
        let declaration = full(profile(true), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection_with_system(
            &[("first_policy", "first"), ("second_policy", "second")],
            &["turn"],
        );

        let prepared = session.prepare(&declaration, &complete).unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::Full);
        assert!(prepared.frame.submission().replay().is_empty());
        assert!(prepared.frame.submission().staged_inputs().is_empty());
        let items = prepared.frame.submission().projection().items();
        assert_eq!(items.len(), 2);
        assert_eq!(
            render_pom_document(rendered_system(&items[0])).unwrap(),
            "<first_policy>first</first_policy>\n\n<second_policy>second</second_policy>"
        );
        assert_eq!(items[1], CanonicalInputItem::assistant_text("turn", None));
        assert!(session.canonical_history.transcript.items().is_empty());

        let (_, commit) = prepared.into_parts();
        session.commit(commit);
        assert_eq!(
            session.canonical_history.transcript.items(),
            &[CanonicalInputItem::assistant_text("turn", None)]
        );
        assert!(session
            .canonical_history
            .transcript
            .items()
            .iter()
            .all(|item| !super::is_system_instruction(item)));
    }

    #[test]
    fn unchanged_system_allows_delta_while_change_forces_full_without_committing_on_drop() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let original = projection_with_system(&[("policy", "old-secret")], &["turn"]);
        let head = commit_first(&mut session, &declaration, &original);
        let resume = TargetDeclaration::resume(head, profile.clone());

        let unchanged = session.prepare(&resume, &original).unwrap();
        assert_eq!(unchanged.frame.basis(), FrameBasis::DeltaFrom(head));
        assert!(unchanged.frame.submission().projection().items().is_empty());
        drop(unchanged);

        let changed = projection_with_system(&[("policy", "new-policy")], &["turn"]);
        let changed_candidate = session.prepare(&resume, &changed).unwrap();
        assert_eq!(changed_candidate.frame.basis(), FrameBasis::Full);
        assert_eq!(
            render_pom_document(rendered_system(
                &changed_candidate.frame.submission().projection().items()[0]
            ))
            .unwrap(),
            "<policy>new-policy</policy>"
        );
        let encoded = String::from_utf8(
            changed_candidate
                .frame
                .submission()
                .canonical_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(!encoded.contains("old-secret"));
        assert!(encoded.contains("new-policy"));
        drop(changed_candidate);

        let still_original = session.prepare(&resume, &original).unwrap();
        assert_eq!(still_original.frame.basis(), FrameBasis::DeltaFrom(head));
        assert!(still_original
            .frame
            .submission()
            .projection()
            .items()
            .is_empty());

        let changed_candidate = session.prepare(&resume, &changed).unwrap();
        let (changed_frame, commit) = changed_candidate.into_parts();
        let changed_head = changed_frame.revision();
        session.commit(commit);
        assert!(session
            .canonical_history
            .transcript
            .items()
            .iter()
            .all(|item| !super::is_system_instruction(item)));

        let full_required = session
            .prepare(&full(profile, 2), &changed)
            .expect("a continuity Full must restate the accepted System snapshot");
        assert_eq!(full_required.frame.basis(), FrameBasis::Full);
        assert!(super::is_system_instruction(
            &full_required.frame.submission().projection().items()[0]
        ));
        assert_eq!(
            session.target_delivery.committed_revision,
            Some(changed_head)
        );
    }

    #[test]
    fn clearing_system_forces_one_full_then_stable_clear_allows_delta() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let with_system = projection_with_system(&[("policy", "active")], &["turn"]);
        let head = commit_first(&mut session, &declaration, &with_system);
        let clear = projection_with_system(&[], &["turn"]);

        let prepared = session
            .prepare(&TargetDeclaration::resume(head, profile.clone()), &clear)
            .unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::Full);
        assert!(prepared
            .frame
            .submission()
            .projection()
            .items()
            .iter()
            .all(|item| !super::is_system_instruction(item)));
        let (frame, commit) = prepared.into_parts();
        let clear_head = frame.revision();
        session.commit(commit);

        let stable_clear = session
            .prepare(&TargetDeclaration::resume(clear_head, profile), &clear)
            .unwrap();
        assert_eq!(
            stable_clear.frame.basis(),
            FrameBasis::DeltaFrom(clear_head)
        );
        assert!(stable_clear
            .frame
            .submission()
            .projection()
            .items()
            .is_empty());
    }

    #[test]
    fn matching_head_allows_delta_with_equal_replay_and_component_change() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let head = commit_first(&mut session, &declaration, &projection(Some("before")));
        let resume = TargetDeclaration::resume(head, profile);

        let prepared = session
            .prepare(&resume, &projection(Some("after")))
            .unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::DeltaFrom(head));
        assert!(prepared.frame.submission().replay().is_empty());
        assert_eq!(
            prepared.frame.submission().projection().items(),
            &[CanonicalInputItem::assistant_text("after", None)]
        );
    }

    #[test]
    fn unchanged_projection_is_omitted_from_delta() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(Some("stable"));
        let head = commit_first(&mut session, &declaration, &complete);
        let resume = TargetDeclaration::resume(head, profile);

        let prepared = session.prepare(&resume, &complete).unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::DeltaFrom(head));
        assert!(prepared.frame.submission().replay().is_empty());
        assert!(prepared.frame.submission().projection().items().is_empty());
    }

    #[test]
    fn delta_replays_only_canonical_tail_after_checkpoint() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(Some("stable"));
        let head = commit_first(&mut session, &declaration, &complete);
        let output = CanonicalInputItem::assistant_text("provider-output", None);
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(output.clone())
            .unwrap();
        let resume = TargetDeclaration::resume(head, profile);

        let prepared = session.prepare(&resume, &complete).unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::DeltaFrom(head));
        assert_eq!(prepared.frame.submission().replay(), &[output]);
    }

    #[test]
    fn foreign_revision_forces_full() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(Some("stable"));
        commit_first(&mut session, &declaration, &complete);
        let foreign = FrameRevision::new(
            NonZeroU128::new(999).unwrap(),
            identity(),
            epoch(1),
            NonZeroU64::new(88).unwrap(),
        );
        let foreign_resume = TargetDeclaration::resume(foreign, profile.clone());
        assert_eq!(
            session
                .prepare(&foreign_resume, &complete)
                .unwrap()
                .frame
                .basis(),
            FrameBasis::Full
        );
    }

    #[test]
    fn full_reset_claims_provider_output_in_replay_instead_of_submitting_it_twice() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let first = projection(Some("authored"));
        commit_first(&mut session, &declaration, &first);

        let provider_output = CanonicalInputItem::assistant_text("provider", None);
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(provider_output.clone())
            .unwrap();
        let current = RenderedProjection::from_nodes(vec![RenderedProjectionNode::new(
            "root",
            vec![
                CanonicalInputItem::assistant_text("authored", None),
                provider_output.clone(),
                CanonicalInputItem::assistant_text("new", None),
            ],
        )])
        .unwrap();

        let prepared = session.prepare(&full(profile, 2), &current).unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::Full);
        assert_eq!(
            prepared.frame.submission().replay(),
            &[
                CanonicalInputItem::assistant_text("authored", None),
                provider_output,
            ]
        );
        assert_eq!(
            prepared.frame.submission().projection().items(),
            &[CanonicalInputItem::assistant_text("new", None)]
        );
    }

    #[test]
    fn execution_scope_reset_fails_closed_on_equal_provider_and_authored_values() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let first = scoped_projection(&["base"], 1);
        let first_head = commit_first(&mut session, &declaration, &first);

        let provider_output = CanonicalInputItem::assistant_text("collision", None);
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(provider_output.clone())
            .unwrap();
        let same_mount = scoped_projection(&["base", "collision"], 1);
        let prepared = session
            .prepare(
                &TargetDeclaration::resume(first_head, profile.clone()),
                &same_mount,
            )
            .unwrap();
        let (frame, commit) = prepared.into_parts();
        let second_head = frame.revision();
        session.commit(commit);

        let remounted_with_independent_authored_value = scoped_projection(&["collision"], 2);
        let next_sequence = session.target_delivery.next_sequence;

        assert!(matches!(
            session.prepare(
                &TargetDeclaration::resume(second_head, profile),
                &remounted_with_independent_authored_value,
            ),
            Err(FrameSessionFault::ProjectionReconciliation(
                ProjectionReconciliationFault::AmbiguousProjectionProvenance
            ))
        ));
        assert_eq!(
            session.target_delivery.committed_revision,
            Some(second_head)
        );
        assert_eq!(session.target_delivery.next_sequence, next_sequence);
    }

    #[test]
    fn direct_remount_fences_uncheckpointed_provider_tail_as_ambiguous() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let first = scoped_projection(&[], 1);
        let first_head = commit_first(&mut session, &declaration, &first);

        let provider_output = CanonicalInputItem::assistant_text("collision", None);
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(provider_output)
            .unwrap();
        let direct_remount = scoped_projection(&["collision"], 2);
        let next_sequence = session.target_delivery.next_sequence;

        assert!(matches!(
            session.prepare(
                &TargetDeclaration::resume(first_head, profile),
                &direct_remount,
            ),
            Err(FrameSessionFault::ProjectionReconciliation(
                ProjectionReconciliationFault::AmbiguousProjectionProvenance
            ))
        ));
        assert_eq!(session.target_delivery.committed_revision, Some(first_head));
        assert_eq!(session.target_delivery.next_sequence, next_sequence);
    }

    #[test]
    fn non_collision_remount_succeeds_but_cross_scope_ambiguity_stays_fenced() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let first = scoped_projection(&["old-authored"], 1);
        let first_head = commit_first(&mut session, &declaration, &first);

        let provider_output = CanonicalInputItem::assistant_text("provider", None);
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(provider_output.clone())
            .unwrap();
        let same_mount = scoped_projection(&["old-authored", "provider"], 1);
        let prepared = session
            .prepare(
                &TargetDeclaration::resume(first_head, profile.clone()),
                &same_mount,
            )
            .unwrap();
        let (frame, commit) = prepared.into_parts();
        let second_head = frame.revision();
        session.commit(commit);

        let remounted = scoped_projection(&["new-authored"], 2);
        let prepared = session
            .prepare(
                &TargetDeclaration::resume(second_head, profile.clone()),
                &remounted,
            )
            .unwrap();
        assert_eq!(prepared.frame.basis(), FrameBasis::Full);
        assert_eq!(
            prepared.frame.submission().replay(),
            &[
                CanonicalInputItem::assistant_text("old-authored", None),
                provider_output,
            ]
        );
        assert_eq!(
            prepared.frame.submission().projection().items(),
            &[CanonicalInputItem::assistant_text("new-authored", None)]
        );
        let (frame, commit) = prepared.into_parts();
        let remount_head = frame.revision();
        session.commit(commit);

        let later_collision = scoped_projection(&["new-authored", "provider"], 2);
        assert!(matches!(
            session.prepare(
                &TargetDeclaration::resume(remount_head, profile),
                &later_collision,
            ),
            Err(FrameSessionFault::ProjectionReconciliation(
                ProjectionReconciliationFault::AmbiguousProjectionProvenance
            ))
        ));
    }

    #[test]
    fn replay_replacement_fails_closed_before_frame_production() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let first = scoped_projection(&["authored"], 1);
        let first_head = commit_first(&mut session, &declaration, &first);

        let provider_output = CanonicalInputItem::assistant_text("provider", None);
        session.canonical_history.transcript = session
            .canonical_history
            .transcript
            .appended(provider_output.clone())
            .unwrap();
        let claimed = scoped_projection(&["authored", "provider"], 1);
        let prepared = session
            .prepare(
                &TargetDeclaration::resume(first_head, profile.clone()),
                &claimed,
            )
            .unwrap();
        let (frame, commit) = prepared.into_parts();
        let head = frame.revision();
        session.commit(commit);

        session.canonical_history.transcript =
            CanonicalTranscript::try_from_items(vec![provider_output.clone()]).unwrap();
        let current = scoped_projection(&["provider", "authored"], 1);
        let next_sequence = session.target_delivery.next_sequence;

        assert!(matches!(
            session.prepare(&TargetDeclaration::resume(head, profile), &current),
            Err(FrameSessionFault::ReplayReplacementUnsupported)
        ));
        assert_eq!(session.target_delivery.committed_revision, Some(head));
        assert_eq!(session.target_delivery.next_sequence, next_sequence);
    }

    #[test]
    fn continuity_epoch_rules_fail_closed() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(None);
        let head = commit_first(&mut session, &declaration, &complete);

        assert!(matches!(
            session.prepare(&full(profile.clone(), 1), &complete),
            Err(FrameSessionFault::ContinuityResetWithoutEpochAdvance { .. })
        ));
        let accepted_new_epoch = TargetDeclaration::from_raw(
            identity(),
            TargetContinuity::Accepted {
                epoch: epoch(2),
                revision: head,
            },
            profile.clone(),
        );
        assert!(matches!(
            session.prepare(&accepted_new_epoch, &complete),
            Err(FrameSessionFault::AcceptedRevisionInNewEpoch { .. })
        ));
        assert_eq!(
            session
                .prepare(&full(profile, 2), &complete)
                .unwrap()
                .frame
                .basis(),
            FrameBasis::Full
        );
    }

    #[test]
    fn staged_tool_output_is_retryable_and_consumed_only_by_commit() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(None);
        let head = commit_first(&mut session, &declaration, &complete);

        {
            let mut guard = ReactionAdmissionGuard::new(
                &mut session.canonical_history.transcript,
                &mut session.target_delivery.tool_outputs,
            );
            let admitted = guard
                .admit(ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(7),
                    ordinal: 3,
                    call: ProviderToolCall::new("call-7", "lookup", "{}").unwrap(),
                })
                .unwrap();
            let (event, ticket) = admitted.into_parts();
            let ProviderEvent::ToolCall(call) = event.unwrap() else {
                panic!("expected ToolCall event")
            };
            guard
                .stage_tool_output(ticket.unwrap(), call.output("result"))
                .unwrap();
            guard
                .admit(ProviderFact::ReactionCompleted { primary_text: None })
                .unwrap();
            guard.finish_normal().unwrap();
        }
        let resume = TargetDeclaration::resume(head, profile);
        assert!(!session.can_reset_model_context());
        let prepared = session.prepare(&resume, &complete).unwrap();
        assert_eq!(prepared.frame.submission().staged_inputs().len(), 1);
        assert_eq!(
            session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            1
        );
        drop(prepared);
        assert_eq!(
            session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            1
        );

        let prepared = session.prepare(&resume, &complete).unwrap();
        let (_, commit) = prepared.into_parts();
        session.commit(commit);
        assert_eq!(
            session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .count(),
            0
        );
        assert!(matches!(
            session.canonical_history.transcript.items().last(),
            Some(CanonicalInputItem::ToolResult { call_id, content })
                if call_id == "call-7" && content == "result"
        ));
        assert!(session.can_reset_model_context());
    }

    #[test]
    fn cancelled_tool_fallback_is_retryable_and_consumed_only_by_commit() {
        let profile = profile(true);
        let declaration = full(profile.clone(), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        let complete = projection(None);
        let head = commit_first(&mut session, &declaration, &complete);

        {
            let mut guard = ReactionAdmissionGuard::new(
                &mut session.canonical_history.transcript,
                &mut session.target_delivery.tool_outputs,
            );
            guard
                .admit(ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(7),
                    ordinal: 3,
                    call: ProviderToolCall::new("call-7", "lookup", "{}").unwrap(),
                })
                .unwrap();
            guard.finish_cancelled();
        }

        let resume = TargetDeclaration::resume(head, profile);
        let expected = CanonicalInputItem::tool_result(
            "call-7",
            "Tool execution was cancelled; its outcome is unknown.",
        )
        .unwrap();
        let prepared = session.prepare(&resume, &complete).unwrap();
        assert_eq!(
            prepared.frame.submission().staged_inputs(),
            std::slice::from_ref(&expected)
        );
        drop(prepared);
        assert_eq!(
            session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .map(|(_, item)| item)
                .collect::<Vec<_>>(),
            vec![&expected]
        );

        let prepared = session.prepare(&resume, &complete).unwrap();
        assert_eq!(prepared.frame.submission().staged_inputs(), &[expected]);
        let (_, commit) = prepared.into_parts();
        session.commit(commit);
        assert!(session
            .target_delivery
            .tool_outputs
            .prepare_frame_candidate()
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn cancelled_tool_fallback_survives_a_later_frame_prepare_fault() {
        let declaration = full(profile(true), 1);
        let mut session = FrameSession::new(&declaration).unwrap();
        {
            let mut guard = ReactionAdmissionGuard::new(
                &mut session.canonical_history.transcript,
                &mut session.target_delivery.tool_outputs,
            );
            guard
                .admit(ProviderFact::ToolCall {
                    output: ProviderOutputKey::new(7),
                    ordinal: 3,
                    call: ProviderToolCall::new("call-7", "lookup", "{}").unwrap(),
                })
                .unwrap();
            guard.finish_cancelled();
        }

        let oversized_text = "x".repeat(4_096);
        let oversized = projection(Some(&oversized_text));
        assert!(matches!(
            session.prepare(&declaration, &oversized),
            Err(FrameSessionFault::Budget(
                FrameBudgetFault::ComponentTooLarge { .. }
            ))
        ));
        let expected = CanonicalInputItem::tool_result(
            "call-7",
            "Tool execution was cancelled; its outcome is unknown.",
        )
        .unwrap();
        assert_eq!(
            session
                .target_delivery
                .tool_outputs
                .ordered_outputs()
                .map(|(_, item)| item)
                .collect::<Vec<_>>(),
            vec![&expected]
        );

        let prepared = session.prepare(&declaration, &projection(None)).unwrap();
        assert_eq!(prepared.frame.submission().staged_inputs(), &[expected]);
        let (_, commit) = prepared.into_parts();
        session.commit(commit);
        assert!(session
            .target_delivery
            .tool_outputs
            .prepare_frame_candidate()
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn prepare_budget_failure_does_not_advance_session() {
        let tiny_profile = FrameProfile::new(constraints(64, 4_096), FrameCapabilities::new(true));
        let declaration = full(tiny_profile, 1);
        let session = FrameSession::new(&declaration).unwrap();
        let fault = match session.prepare(&declaration, &projection(Some(&"x".repeat(512)))) {
            Ok(_) => panic!("oversized Component unexpectedly prepared"),
            Err(fault) => fault,
        };
        assert!(matches!(
            fault,
            FrameSessionFault::Budget(FrameBudgetFault::ComponentTooLarge { .. })
        ));
        assert!(session.canonical_history.transcript.items().is_empty());
        assert_eq!(session.target_delivery.committed_revision, None);
    }
}
