use std::time::Duration;

use agentview::component::execution::{
    ApplicationHostFault, ComponentReactionRuntime, ComponentReactionRuntimeFault,
    ProviderFaultCode, ProviderPort, ProviderResponseEventDiagnostic,
};
use chess::{Board, ChessMove, MoveGen};
use tokio::time::timeout;

use super::{
    chess_actions::{unavailable_reason, ChessAction, InvalidActionReason},
    chess_agent::{ChessAgentProps, ChessAgentState, ChessSnapshot},
};

pub(super) const MAX_MODEL_ATTEMPTS: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModelTurnId(String);

impl ModelTurnId {
    pub(crate) fn for_white_ply(ply: usize) -> Self {
        Self(format!("white-ply-{ply}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelAttemptContext {
    pub(crate) turn_id: ModelTurnId,
    pub(crate) attempt_index: u8,
    pub(crate) corrective_reason: Option<InvalidActionReason>,
}

impl ModelAttemptContext {
    pub(crate) fn initial(turn_id: ModelTurnId) -> Self {
        Self {
            turn_id,
            attempt_index: 0,
            corrective_reason: None,
        }
    }

    fn for_attempt(
        turn_id: ModelTurnId,
        attempt_index: u8,
        corrective_reason: Option<InvalidActionReason>,
    ) -> Self {
        Self {
            turn_id,
            attempt_index,
            corrective_reason,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InfrastructureStage {
    ModelReaction,
    AuthoritativeState,
    Engine,
    WholeGame,
    Shutdown,
    TerminalStatus,
    Jsonl,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InfrastructureAbortReason {
    Provider(ProviderFaultCode),
    ProviderResponseEventShape(Box<ProviderResponseEventDiagnostic>),
    NonProviderRuntime,
    ReactionTimeout,
    StateUnavailable,
    StateUnfinished,
    TextIncomplete,
    StateMismatch,
    PropsUpdateFailure,
    HostIdentityChanged,
    ProviderResponseCaptureFailure,
    EngineFailure,
    EngineTimeout,
    WholeGameTimeout,
    ShutdownFailure,
    StatusWriteFailure,
    TraceWriteFailure,
}

impl InfrastructureAbortReason {
    pub(crate) fn response_event_diagnostic(&self) -> Option<ProviderResponseEventDiagnostic> {
        match self {
            Self::ProviderResponseEventShape(diagnostic) => Some(**diagnostic),
            Self::Provider(ProviderFaultCode::ResponseEventShape) => {
                Some(ProviderResponseEventDiagnostic::unknown())
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttemptResult {
    ActionAccepted(ChessAction),
    Correctable(InvalidActionReason),
    InfrastructureAbort {
        stage: InfrastructureStage,
        reason_code: InfrastructureAbortReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptStart {
    InitialMount,
    PropsUpdate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptEvidence {
    pub(crate) turn_id: ModelTurnId,
    pub(crate) attempt_index: u8,
    pub(crate) corrective_reason: Option<InvalidActionReason>,
    pub(crate) board: Board,
    pub(crate) committed_moves: Vec<ChessMove>,
    pub(crate) start: AttemptStart,
    pub(crate) result: AttemptResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchedAttempt {
    pub(crate) turn_id: ModelTurnId,
    pub(crate) attempt_index: u8,
    pub(crate) corrective_reason: Option<InvalidActionReason>,
    pub(crate) board: Board,
    pub(crate) committed_moves: Vec<ChessMove>,
    pub(crate) start: AttemptStart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttemptTransition {
    Started {
        completed: Vec<AttemptEvidence>,
        dispatched: DispatchedAttempt,
    },
    Completed {
        completed: Vec<AttemptEvidence>,
    },
}

impl DispatchedAttempt {
    pub(crate) fn abort_evidence(
        &self,
        stage: InfrastructureStage,
        reason_code: InfrastructureAbortReason,
    ) -> AttemptEvidence {
        AttemptEvidence {
            turn_id: self.turn_id.clone(),
            attempt_index: self.attempt_index,
            corrective_reason: self.corrective_reason,
            board: self.board,
            committed_moves: self.committed_moves.clone(),
            start: self.start,
            result: AttemptResult::InfrastructureAbort { stage, reason_code },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WhitePlyOutcome {
    Action {
        action: ChessAction,
        attempts: Vec<AttemptEvidence>,
    },
    ModelForfeit {
        final_reason: InvalidActionReason,
        attempts: Vec<AttemptEvidence>,
    },
    InfrastructureAbort {
        stage: InfrastructureStage,
        reason_code: InfrastructureAbortReason,
        attempts: Vec<AttemptEvidence>,
    },
}

pub(crate) async fn play_white_ply_observed<P, F>(
    white: &mut ComponentReactionRuntime<P, ChessAgentProps, ChessAgentState>,
    snapshot: ChessSnapshot,
    turn_id: ModelTurnId,
    use_initial_mount: bool,
    reaction_timeout: Duration,
    mut on_dispatch: F,
) -> WhitePlyOutcome
where
    P: ProviderPort,
    F: FnMut(AttemptTransition) -> Result<(), (InfrastructureStage, InfrastructureAbortReason)>,
{
    let component_host_id = white.component_host_id();
    let mut attempts = Vec::new();
    let mut corrective_reason = None;

    for attempt_index in 0..MAX_MODEL_ATTEMPTS {
        let context =
            ModelAttemptContext::for_attempt(turn_id.clone(), attempt_index, corrective_reason);
        let attempt_snapshot = corrective_reason
            .map(|reason| snapshot.for_retry(reason))
            .unwrap_or_else(|| snapshot.clone());
        let start = if use_initial_mount && attempt_index == 0 {
            if !white.props().matches_attempt(&attempt_snapshot, &context) {
                return abort_before_dispatch(
                    attempts,
                    InfrastructureStage::AuthoritativeState,
                    InfrastructureAbortReason::StateMismatch,
                );
            }
            AttemptStart::InitialMount
        } else {
            let next_props = white
                .props()
                .for_attempt(attempt_snapshot.clone(), context.clone());
            let retained_host_id = match white.set_props(next_props) {
                Ok(host_id) => host_id,
                Err(_) => {
                    return abort_before_dispatch(
                        attempts,
                        InfrastructureStage::AuthoritativeState,
                        InfrastructureAbortReason::PropsUpdateFailure,
                    )
                }
            };
            if retained_host_id != component_host_id {
                return abort_before_dispatch(
                    attempts,
                    InfrastructureStage::AuthoritativeState,
                    InfrastructureAbortReason::HostIdentityChanged,
                );
            }
            AttemptStart::PropsUpdate
        };
        if white.component_host_id() != component_host_id {
            return abort_before_dispatch(
                attempts,
                InfrastructureStage::AuthoritativeState,
                InfrastructureAbortReason::HostIdentityChanged,
            );
        }

        if let Err((stage, reason_code)) = on_dispatch(AttemptTransition::Started {
            completed: attempts.clone(),
            dispatched: DispatchedAttempt {
                turn_id: context.turn_id.clone(),
                attempt_index,
                corrective_reason,
                board: attempt_snapshot.board(),
                committed_moves: attempt_snapshot.committed_moves().to_vec(),
                start,
            },
        }) {
            return abort_before_dispatch(attempts, stage, reason_code);
        }
        let output = match timeout(reaction_timeout, white.dispatch_llm_reaction()).await {
            Ok(Ok(output)) => output,
            Ok(Err(fault)) => {
                return complete_dispatch_abort(
                    attempts,
                    &context,
                    &attempt_snapshot,
                    start,
                    InfrastructureStage::ModelReaction,
                    provider_abort_reason(&fault),
                    &mut on_dispatch,
                )
            }
            Err(_) => {
                return complete_dispatch_abort(
                    attempts,
                    &context,
                    &attempt_snapshot,
                    start,
                    InfrastructureStage::ModelReaction,
                    InfrastructureAbortReason::ReactionTimeout,
                    &mut on_dispatch,
                )
            }
        };
        let state = match output.cloned() {
            Ok(state) => state,
            Err(_) => {
                return complete_dispatch_abort(
                    attempts,
                    &context,
                    &attempt_snapshot,
                    start,
                    InfrastructureStage::AuthoritativeState,
                    InfrastructureAbortReason::StateUnavailable,
                    &mut on_dispatch,
                )
            }
        };

        let attempt_result = classify_completed_state(&state, &context, &attempt_snapshot);
        let evidence = AttemptEvidence {
            turn_id: context.turn_id.clone(),
            attempt_index,
            corrective_reason,
            board: attempt_snapshot.board(),
            committed_moves: attempt_snapshot.committed_moves().to_vec(),
            start,
            result: attempt_result.clone(),
        };
        attempts = appended_attempt(&attempts, evidence);
        if let Err((stage, reason_code)) = on_dispatch(AttemptTransition::Completed {
            completed: attempts.clone(),
        }) {
            return WhitePlyOutcome::InfrastructureAbort {
                stage,
                reason_code,
                attempts,
            };
        }

        match attempt_result {
            AttemptResult::ActionAccepted(action) => {
                return WhitePlyOutcome::Action { action, attempts };
            }
            AttemptResult::Correctable(reason) if attempt_index + 1 == MAX_MODEL_ATTEMPTS => {
                return WhitePlyOutcome::ModelForfeit {
                    final_reason: reason,
                    attempts,
                };
            }
            AttemptResult::Correctable(reason) => corrective_reason = Some(reason),
            AttemptResult::InfrastructureAbort { stage, reason_code } => {
                return WhitePlyOutcome::InfrastructureAbort {
                    stage,
                    reason_code,
                    attempts,
                };
            }
        }
    }

    unreachable!("the closed attempt budget always returns an outcome")
}

pub(crate) fn provider_abort_reason(
    fault: &ComponentReactionRuntimeFault,
) -> InfrastructureAbortReason {
    match fault {
        ComponentReactionRuntimeFault::Application(
            ApplicationHostFault::ProviderSetup(fault)
            | ApplicationHostFault::ProviderExecution(fault),
        ) => fault
            .response_event_diagnostic()
            .map(Box::new)
            .map(InfrastructureAbortReason::ProviderResponseEventShape)
            .unwrap_or_else(|| InfrastructureAbortReason::Provider(fault.code())),
        _ => InfrastructureAbortReason::NonProviderRuntime,
    }
}

fn classify_completed_state(
    state: &ChessAgentState,
    context: &ModelAttemptContext,
    snapshot: &ChessSnapshot,
) -> AttemptResult {
    if state.context() != context || state.snapshot().as_ref() != snapshot {
        return AttemptResult::InfrastructureAbort {
            stage: InfrastructureStage::AuthoritativeState,
            reason_code: InfrastructureAbortReason::StateMismatch,
        };
    }
    if !state.finished() {
        return AttemptResult::InfrastructureAbort {
            stage: InfrastructureStage::AuthoritativeState,
            reason_code: InfrastructureAbortReason::StateUnfinished,
        };
    }
    if !state.text_complete() {
        return AttemptResult::InfrastructureAbort {
            stage: InfrastructureStage::AuthoritativeState,
            reason_code: InfrastructureAbortReason::TextIncomplete,
        };
    }
    if let Some(reason) = state.diagnostic() {
        return AttemptResult::Correctable(reason);
    }
    match state.action() {
        Some(action) if unavailable_reason(snapshot, action.kind()).is_some() => {
            AttemptResult::Correctable(
                unavailable_reason(snapshot, action.kind())
                    .expect("guarded unavailable action has one reason"),
            )
        }
        Some(
            action
            @ (ChessAction::ChooseMove(candidate) | ChessAction::MoveAndOfferDraw(candidate)),
        ) if MoveGen::new_legal(&snapshot.board()).any(|legal| legal == candidate) => {
            AttemptResult::ActionAccepted(action)
        }
        Some(action @ (ChessAction::ChooseMove(_) | ChessAction::MoveAndOfferDraw(_))) => {
            AttemptResult::Correctable(InvalidActionReason::IllegalMove(action))
        }
        Some(action @ (ChessAction::Resign | ChessAction::AcceptDraw | ChessAction::ClaimDraw)) => {
            AttemptResult::ActionAccepted(action)
        }
        None => AttemptResult::Correctable(InvalidActionReason::MissingAction),
    }
}

fn abort_before_dispatch(
    attempts: Vec<AttemptEvidence>,
    stage: InfrastructureStage,
    reason_code: InfrastructureAbortReason,
) -> WhitePlyOutcome {
    WhitePlyOutcome::InfrastructureAbort {
        stage,
        reason_code,
        attempts,
    }
}

fn complete_dispatch_abort<F>(
    attempts: Vec<AttemptEvidence>,
    context: &ModelAttemptContext,
    snapshot: &ChessSnapshot,
    start: AttemptStart,
    stage: InfrastructureStage,
    reason_code: InfrastructureAbortReason,
    on_transition: &mut F,
) -> WhitePlyOutcome
where
    F: FnMut(AttemptTransition) -> Result<(), (InfrastructureStage, InfrastructureAbortReason)>,
{
    let evidence = AttemptEvidence {
        turn_id: context.turn_id.clone(),
        attempt_index: context.attempt_index,
        corrective_reason: context.corrective_reason,
        board: snapshot.board(),
        committed_moves: snapshot.committed_moves().to_vec(),
        start,
        result: AttemptResult::InfrastructureAbort {
            stage,
            reason_code: reason_code.clone(),
        },
    };
    let attempts = appended_attempt(&attempts, evidence);
    let observed = on_transition(AttemptTransition::Completed {
        completed: attempts.clone(),
    });
    match observed {
        Ok(()) => WhitePlyOutcome::InfrastructureAbort {
            stage,
            reason_code,
            attempts,
        },
        Err((stage, reason_code)) => WhitePlyOutcome::InfrastructureAbort {
            stage,
            reason_code,
            attempts,
        },
    }
}

fn appended_attempt(
    attempts: &[AttemptEvidence],
    evidence: AttemptEvidence,
) -> Vec<AttemptEvidence> {
    attempts
        .iter()
        .cloned()
        .chain(std::iter::once(evidence))
        .collect()
}
