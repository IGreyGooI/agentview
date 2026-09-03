use std::time::Duration;

use agentview::component::execution::{
    Application, ApplicationFault, ApplicationFaultCode, ApplicationFaultKind,
    ApplicationFaultReason, ApplicationFaultStage, ReactionPort,
};
use chess::{Board, ChessMove, MoveGen};
use tokio::time::timeout;

use super::{
    chess_actions::{unavailable_reason, ChessAction, InvalidActionReason},
    chess_agent::{ChessAgentState, ChessControl, ChessSnapshot},
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
    Application {
        stage: ApplicationFaultStage,
        kind: ApplicationFaultKind,
        code: ApplicationFaultCode,
        reason: ApplicationFaultReason,
    },
    ReactionTimeout,
    StateWriteFailure,
    StateUnavailable,
    StateUnfinished,
    TextIncomplete,
    StateMismatch,
    ProviderResponseCaptureFailure,
    EngineFailure,
    EngineTimeout,
    WholeGameTimeout,
    ShutdownFailure,
    StatusWriteFailure,
    TraceWriteFailure,
}

impl InfrastructureAbortReason {
    pub(crate) fn from_application(fault: &ApplicationFault) -> Self {
        Self::Application {
            stage: fault.stage(),
            kind: fault.kind(),
            code: fault.code(),
            reason: fault.reason(),
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
    StateWrite,
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
            corrective_reason: self.corrective_reason.clone(),
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
    white: &mut Application<P>,
    control: &ChessControl,
    snapshot: ChessSnapshot,
    turn_id: ModelTurnId,
    reaction_timeout: Duration,
    mut on_dispatch: F,
) -> WhitePlyOutcome
where
    P: ReactionPort,
    F: FnMut(AttemptTransition) -> Result<(), (InfrastructureStage, InfrastructureAbortReason)>,
{
    let mut attempts = Vec::new();
    let mut corrective_reason = None;

    for attempt_index in 0..MAX_MODEL_ATTEMPTS {
        let context = ModelAttemptContext::for_attempt(
            turn_id.clone(),
            attempt_index,
            corrective_reason.clone(),
        );
        let attempt_snapshot = corrective_reason
            .as_ref()
            .map(|reason| snapshot.for_retry(reason.clone()))
            .unwrap_or_else(|| snapshot.clone());
        let start = AttemptStart::StateWrite;
        if control
            .begin_attempt(attempt_snapshot.clone(), context.clone())
            .is_err()
        {
            return abort_before_dispatch(
                attempts,
                InfrastructureStage::AuthoritativeState,
                InfrastructureAbortReason::StateWriteFailure,
            );
        }

        if let Err((stage, reason_code)) = on_dispatch(AttemptTransition::Started {
            completed: attempts.clone(),
            dispatched: DispatchedAttempt {
                turn_id: context.turn_id.clone(),
                attempt_index,
                corrective_reason: corrective_reason.clone(),
                board: attempt_snapshot.board(),
                committed_moves: attempt_snapshot.committed_moves().to_vec(),
                start,
            },
        }) {
            return abort_before_dispatch(attempts, stage, reason_code);
        }
        let output = match timeout(reaction_timeout, white.react()).await {
            Ok(Ok(())) => match control.read_state() {
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
            },
            Ok(Err(fault)) => {
                return complete_dispatch_abort(
                    attempts,
                    &context,
                    &attempt_snapshot,
                    start,
                    InfrastructureStage::ModelReaction,
                    InfrastructureAbortReason::from_application(&fault),
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

        let attempt_result = classify_completed_state(&output, &context, &attempt_snapshot);
        let evidence = AttemptEvidence {
            turn_id: context.turn_id.clone(),
            attempt_index,
            corrective_reason: corrective_reason.clone(),
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
        Some(action @ ChessAction::ChooseMove(candidate))
            if MoveGen::new_legal(&snapshot.board()).any(|legal| legal == candidate) =>
        {
            AttemptResult::ActionAccepted(action)
        }
        Some(action @ ChessAction::ChooseMove(_)) => {
            AttemptResult::Correctable(InvalidActionReason::IllegalMove(action))
        }
        Some(action @ ChessAction::Resign) => AttemptResult::ActionAccepted(action),
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
        corrective_reason: context.corrective_reason.clone(),
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        num::{NonZeroU128, NonZeroU64},
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use agentview::{
        component::execution::{
            Application, ApplicationFaultCode, ApplicationFaultKind, ApplicationFaultReason,
            ApplicationFaultStage, Frame, FrameBasis, FrameCapabilities, FrameConstraints,
            FrameProfile, FrameRevision, ProviderFact, ProviderFactStream, ProviderOutputKey,
            ReactionPort, ReactionPortFault, ReactionPortFaultCode, ReactionPortFaultReason,
            SubmitFault, TargetContinuity, TargetDeclaration, TargetEpoch, TargetIdentity,
        },
        pom_renderer::render_pom_document,
        transcript::CanonicalInputItem,
    };
    use async_trait::async_trait;
    use chess::{Board, ChessMove, Color, MoveGen};

    use super::*;
    use crate::{
        chess_actions::{ChessActionKind, InvalidActionReason},
        chess_agent::{chess_agent, ChessAgentState, ChessControl},
        chess_feedback::ChessFeedback,
    };

    const TEXT_OUTPUT: ProviderOutputKey = ProviderOutputKey::new(1);
    const CHESS_TEST_TARGET_DOMAIN: u128 = 9_u128 << 64;
    static NEXT_CHESS_TEST_TARGET: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Copy)]
    enum Script {
        Text(&'static str),
        PendingAfterHandoff,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum LifecycleEvent {
        StateWritten,
        FrameSubmitted,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct FrameAttempt {
        revision: FrameRevision,
        prepared_against: TargetContinuity,
        basis: FrameBasis,
    }

    #[derive(Clone)]
    struct ScriptedCapture {
        projections: Arc<Mutex<Vec<String>>>,
        lifecycle: Arc<Mutex<Vec<LifecycleEvent>>>,
        accepted: Arc<Mutex<Option<FrameRevision>>>,
        remaining_scripts: Arc<AtomicUsize>,
        reject_before_handoff: Arc<AtomicBool>,
        frame_attempts: Arc<Mutex<Vec<FrameAttempt>>>,
    }

    struct ScriptedChessPort {
        identity: TargetIdentity,
        epoch: TargetEpoch,
        profile: FrameProfile,
        accepted: Arc<Mutex<Option<FrameRevision>>>,
        scripts: VecDeque<Script>,
        remaining_scripts: Arc<AtomicUsize>,
        reject_before_handoff: Arc<AtomicBool>,
        frame_attempts: Arc<Mutex<Vec<FrameAttempt>>>,
        projections: Arc<Mutex<Vec<String>>>,
        lifecycle: Arc<Mutex<Vec<LifecycleEvent>>>,
    }

    impl ScriptedChessPort {
        fn declaration(&self) -> TargetDeclaration {
            match *self
                .accepted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                Some(revision) => TargetDeclaration::resume(revision, self.profile.clone()),
                None => TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
            }
        }
    }

    #[async_trait]
    impl ReactionPort for ScriptedChessPort {
        fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
            Ok(self.declaration())
        }

        async fn submit<'a>(
            &'a mut self,
            frame: Frame,
        ) -> Result<ProviderFactStream<'a>, SubmitFault> {
            frame.check_handoff_precondition(&self.declaration())?;
            self.frame_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(FrameAttempt {
                    revision: frame.revision(),
                    prepared_against: frame.prepared_against().clone(),
                    basis: frame.basis(),
                });
            let rendered = frame
                .submission()
                .projection()
                .items()
                .iter()
                .filter_map(|item| match item {
                    CanonicalInputItem::Instruction { pom, .. }
                    | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom)),
                    _ => None,
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    SubmitFault::Rejected(ReactionPortFault::terminal(
                        ReactionPortFaultCode::Internal,
                        ReactionPortFaultReason::RequestPreparation,
                    ))
                })?
                .join("\n");
            if self.reject_before_handoff.load(Ordering::SeqCst) {
                return Err(SubmitFault::Rejected(ReactionPortFault::retryable(
                    ReactionPortFaultCode::Unavailable,
                    ReactionPortFaultReason::Transport,
                )));
            }
            let script = self.scripts.front().copied().ok_or_else(|| {
                SubmitFault::Rejected(ReactionPortFault::retryable(
                    ReactionPortFaultCode::Unavailable,
                    ReactionPortFaultReason::Other,
                ))
            })?;
            let facts: ProviderFactStream<'a> = match script {
                Script::Text(text) => Box::pin(futures::stream::iter([
                    Ok(ProviderFact::TextDelta {
                        output: TEXT_OUTPUT,
                        phase: None,
                        delta: text.to_owned(),
                    }),
                    Ok(ProviderFact::TextSealed {
                        output: TEXT_OUTPUT,
                        phase: None,
                        text: text.to_owned(),
                    }),
                    Ok(ProviderFact::ReactionCompleted {
                        primary_text: Some(TEXT_OUTPUT),
                    }),
                ])),
                Script::PendingAfterHandoff => Box::pin(futures::stream::pending()),
            };

            self.scripts
                .pop_front()
                .expect("inspected scripted Chess response remains at the crossing poll");
            self.remaining_scripts.fetch_sub(1, Ordering::SeqCst);
            self.projections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(rendered);
            self.lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(LifecycleEvent::FrameSubmitted);
            *self
                .accepted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(frame.revision());

            Ok(facts)
        }
    }

    fn scripted_port(
        scripts: impl IntoIterator<Item = Script>,
    ) -> (ScriptedChessPort, ScriptedCapture) {
        let scripts = scripts.into_iter().collect::<VecDeque<_>>();
        let projections = Arc::new(Mutex::new(Vec::new()));
        let lifecycle = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(Mutex::new(None));
        let remaining_scripts = Arc::new(AtomicUsize::new(scripts.len()));
        let reject_before_handoff = Arc::new(AtomicBool::new(false));
        let frame_attempts = Arc::new(Mutex::new(Vec::new()));
        let instance = NEXT_CHESS_TEST_TARGET.fetch_add(1, Ordering::Relaxed);
        let identity = TargetIdentity::new(
            NonZeroU128::new(CHESS_TEST_TARGET_DOMAIN | u128::from(instance))
                .expect("non-zero Chess test target"),
        );
        let epoch = TargetEpoch::new(NonZeroU64::MIN);
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 1024 * 1024,
                max_component_bytes: 256 * 1024,
                context_window_tokens: None,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true),
        );
        (
            ScriptedChessPort {
                identity,
                epoch,
                profile,
                accepted: Arc::clone(&accepted),
                scripts,
                remaining_scripts: Arc::clone(&remaining_scripts),
                reject_before_handoff: Arc::clone(&reject_before_handoff),
                frame_attempts: Arc::clone(&frame_attempts),
                projections: Arc::clone(&projections),
                lifecycle: Arc::clone(&lifecycle),
            },
            ScriptedCapture {
                projections,
                lifecycle,
                accepted,
                remaining_scripts,
                reject_before_handoff,
                frame_attempts,
            },
        )
    }

    fn attempted_position() -> (ChessSnapshot, ModelAttemptContext) {
        let mut board = Board::default();
        let mut committed_moves = Vec::new();
        for candidate in ["e2e4", "e7e5"] {
            let candidate = candidate
                .parse::<ChessMove>()
                .expect("test move is typed UCI");
            assert!(MoveGen::new_legal(&board).any(|legal| legal == candidate));
            board = board.make_move_new(candidate);
            committed_moves.push(candidate);
        }
        let snapshot = ChessSnapshot::in_progress(
            Color::White,
            board,
            committed_moves,
            ChessFeedback::initial(),
        )
        .for_retry(InvalidActionReason::InvalidXml);
        let context = ModelAttemptContext::for_attempt(
            ModelTurnId::for_white_ply(2),
            1,
            Some(InvalidActionReason::InvalidXml),
        );
        (snapshot, context)
    }

    fn mount_scripted_application(
        scripts: impl IntoIterator<Item = Script>,
    ) -> (
        Application<ScriptedChessPort>,
        ChessControl,
        ScriptedCapture,
    ) {
        let (port, capture) = scripted_port(scripts);
        let initial_snapshot = ChessSnapshot::in_progress(
            Color::White,
            Board::default(),
            Vec::new(),
            ChessFeedback::initial(),
        );
        let initial_context = ModelAttemptContext::initial(ModelTurnId::for_white_ply(0));
        let initial_state = ChessAgentState::awaiting(Arc::new(initial_snapshot), initial_context);
        let exported = Arc::new(Mutex::new(None));
        let root_state = initial_state.clone();
        let root_exported = Arc::clone(&exported);
        let completed_reactions = Arc::new(AtomicUsize::new(0));
        let root_completed_reactions = Arc::clone(&completed_reactions);
        let application = Application::mount(
            move || {
                chess_agent(
                    root_state.clone(),
                    Arc::clone(&root_exported),
                    Arc::clone(&root_completed_reactions),
                )
            },
            port,
        )
        .expect("offline Chess Application mounts");
        let control = exported
            .lock()
            .expect("Chess control export lock")
            .clone()
            .expect("ordinary Chess root exports typed control");
        (application, control, capture)
    }

    #[tokio::test]
    async fn offline_native_attempt_writes_complete_state_before_react_and_consumes_owner() {
        let (mut application, control, capture) =
            mount_scripted_application([Script::Text("<resign />")]);
        let (snapshot, context) = attempted_position();

        control
            .begin_attempt(snapshot.clone(), context.clone())
            .expect("complete typed attempt state writes atomically");
        capture
            .lifecycle
            .lock()
            .expect("scripted Chess lifecycle lock")
            .push(LifecycleEvent::StateWritten);
        let written = control
            .read_state()
            .expect("written attempt state is readable");
        assert_eq!(written.snapshot().as_ref(), &snapshot);
        assert_eq!(written.context(), &context);
        assert!(!written.finished());
        assert!(application.current_projection().is_dirty());

        tokio::time::timeout(Duration::from_secs(1), application.react())
            .await
            .expect("offline Chess reaction stays bounded")
            .expect("scripted Chess reaction completes");

        assert_eq!(
            *capture
                .lifecycle
                .lock()
                .expect("scripted Chess lifecycle lock"),
            vec![LifecycleEvent::StateWritten, LifecycleEvent::FrameSubmitted]
        );
        let submitted = capture
            .projections
            .lock()
            .expect("scripted Chess projection lock")
            .first()
            .cloned()
            .expect("scripted Chess provider received one Frame");
        assert!(submitted.contains("white-ply-2"));
        assert!(submitted.contains("<attempt_index>1</attempt_index>"));
        assert!(submitted.contains("e2e4"));
        assert!(submitted.contains("e7e5"));
        assert!(!submitted.contains("pending_draw_offer"));
        assert!(submitted.contains("invalid\\_xml"));
        assert_eq!(submitted.matches("<choose_move>").count(), 1, "{submitted}");
        assert_eq!(submitted.matches("<resign>").count(), 1, "{submitted}");
        for removed_action in ["move_and_offer_draw", "accept_draw", "claim_draw"] {
            assert!(!submitted.contains(removed_action), "{submitted}");
        }
        assert!(
            submitted.contains("exactly one of the two empty XML elements"),
            "{submitted}"
        );

        let completed = control
            .read_state()
            .expect("admitted facts publish typed Chess result");
        assert_eq!(completed.action(), Some(ChessAction::Resign));
        assert!(completed.finished());
        assert!(completed.text_complete());
        assert!(!application.current_projection().is_dirty());

        application
            .shutdown()
            .await
            .expect("normal Chess owner shutdown completes");
        assert!(control.read_state().is_err());
    }

    #[tokio::test]
    async fn offline_retry_projection_exposes_invalid_uci_submission() {
        let (mut application, control, capture) =
            mount_scripted_application([Script::Text("<resign />")]);
        let rejection = InvalidActionReason::invalid_uci(ChessActionKind::ChooseMove, "E2E4");
        let snapshot = ChessSnapshot::in_progress(
            Color::White,
            Board::default(),
            Vec::new(),
            ChessFeedback::initial(),
        )
        .for_retry(rejection.clone());
        let context =
            ModelAttemptContext::for_attempt(ModelTurnId::for_white_ply(0), 1, Some(rejection));

        control
            .begin_attempt(snapshot, context)
            .expect("retry state writes atomically");
        application
            .react()
            .await
            .expect("retry projection reaches the provider");

        let submitted = capture
            .projections
            .lock()
            .expect("scripted Chess projection lock")
            .first()
            .cloned()
            .expect("retry submits one frame");
        assert!(submitted.contains("E2E4"), "{submitted}");

        application
            .shutdown()
            .await
            .expect("retry application shuts down");
    }

    #[tokio::test]
    async fn offline_native_timeout_still_consumes_application_owner() {
        let (mut application, control, capture) =
            mount_scripted_application([Script::PendingAfterHandoff]);
        let (snapshot, context) = attempted_position();
        control
            .begin_attempt(snapshot, context)
            .expect("complete timeout attempt state writes atomically");
        capture
            .lifecycle
            .lock()
            .expect("scripted Chess lifecycle lock")
            .push(LifecycleEvent::StateWritten);

        assert!(
            tokio::time::timeout(Duration::from_millis(25), application.react())
                .await
                .is_err()
        );
        assert_eq!(
            *capture
                .lifecycle
                .lock()
                .expect("scripted Chess lifecycle lock"),
            vec![LifecycleEvent::StateWritten, LifecycleEvent::FrameSubmitted]
        );

        let _terminal_shutdown = application.shutdown().await;
        assert!(control.read_state().is_err());
    }

    #[tokio::test]
    async fn offline_native_prehandoff_rejection_retries_same_application_without_mutation() {
        let (mut application, control, capture) =
            mount_scripted_application([Script::Text("<resign />")]);
        let (snapshot, context) = attempted_position();
        control
            .begin_attempt(snapshot, context)
            .expect("complete retry attempt state writes atomically");
        capture
            .lifecycle
            .lock()
            .expect("scripted Chess lifecycle lock")
            .push(LifecycleEvent::StateWritten);
        capture.reject_before_handoff.store(true, Ordering::SeqCst);
        let lifecycle_before = capture
            .lifecycle
            .lock()
            .expect("scripted Chess lifecycle lock")
            .clone();

        let fault = application
            .react()
            .await
            .expect_err("injected failure must reject before handoff");
        assert_eq!(fault.stage(), ApplicationFaultStage::Submit);
        assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
        assert_eq!(fault.code(), ApplicationFaultCode::Unavailable);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::Port(ReactionPortFaultReason::Transport)
        );
        assert_eq!(capture.remaining_scripts.load(Ordering::SeqCst), 1);
        assert_eq!(
            *capture
                .accepted
                .lock()
                .expect("scripted Chess accepted revision lock"),
            None
        );
        assert!(capture
            .projections
            .lock()
            .expect("scripted Chess projection lock")
            .is_empty());
        assert_eq!(
            *capture
                .lifecycle
                .lock()
                .expect("scripted Chess lifecycle lock"),
            lifecycle_before
        );
        let failed_attempt = capture
            .frame_attempts
            .lock()
            .expect("scripted Chess Frame attempt lock")
            .first()
            .cloned()
            .expect("pre-handoff failure records its attempted Frame");
        assert!(matches!(
            failed_attempt.prepared_against,
            TargetContinuity::FullRequired { .. }
        ));
        assert_eq!(failed_attempt.basis, FrameBasis::Full);

        capture.reject_before_handoff.store(false, Ordering::SeqCst);
        application
            .react()
            .await
            .expect("same Application retries the original response plan");

        let attempts = capture
            .frame_attempts
            .lock()
            .expect("scripted Chess Frame attempt lock")
            .clone();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0], attempts[1]);
        assert_eq!(capture.remaining_scripts.load(Ordering::SeqCst), 0);
        assert_eq!(
            *capture
                .accepted
                .lock()
                .expect("scripted Chess accepted revision lock"),
            Some(failed_attempt.revision)
        );
        assert_eq!(
            capture
                .projections
                .lock()
                .expect("scripted Chess projection lock")
                .len(),
            1
        );
        assert_eq!(
            *capture
                .lifecycle
                .lock()
                .expect("scripted Chess lifecycle lock"),
            vec![LifecycleEvent::StateWritten, LifecycleEvent::FrameSubmitted]
        );
        let completed = control
            .read_state()
            .expect("retried text facts publish typed Chess state");
        assert_eq!(completed.action(), Some(ChessAction::Resign));
        assert!(completed.finished());
        assert!(completed.text_complete());
        assert!(!application.current_projection().is_dirty());

        application
            .shutdown()
            .await
            .expect("same retried Chess Application shuts down");
        assert!(control.read_state().is_err());
    }

    #[tokio::test]
    async fn offline_native_invalid_output_exhausts_exact_retry_budget_and_shuts_down() {
        let (mut application, control, capture) = mount_scripted_application([
            Script::Text("<bogus />"),
            Script::Text("<bogus />"),
            Script::Text("<bogus />"),
        ]);
        let snapshot = ChessSnapshot::in_progress(
            Color::White,
            Board::default(),
            Vec::new(),
            ChessFeedback::initial(),
        );
        let lifecycle_on_dispatch = Arc::clone(&capture.lifecycle);

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            play_white_ply_observed(
                &mut application,
                &control,
                snapshot,
                ModelTurnId::for_white_ply(0),
                Duration::from_millis(250),
                move |transition| {
                    if matches!(transition, AttemptTransition::Started { .. }) {
                        lifecycle_on_dispatch
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(LifecycleEvent::StateWritten);
                    }
                    Ok(())
                },
            ),
        )
        .await
        .expect("invalid-output retry fixture stays bounded");

        let WhitePlyOutcome::ModelForfeit {
            final_reason,
            attempts,
        } = outcome
        else {
            panic!("three invalid outputs must forfeit the model turn")
        };
        assert_eq!(final_reason, InvalidActionReason::InvalidXml);
        assert_eq!(attempts.len(), usize::from(MAX_MODEL_ATTEMPTS));
        assert!(attempts
            .iter()
            .all(|attempt| attempt.start == AttemptStart::StateWrite));
        assert_eq!(attempts[0].corrective_reason, None);
        assert_eq!(
            attempts[1].corrective_reason,
            Some(InvalidActionReason::InvalidXml)
        );
        assert_eq!(
            attempts[2].corrective_reason,
            Some(InvalidActionReason::InvalidXml)
        );
        assert_eq!(
            *capture
                .lifecycle
                .lock()
                .expect("scripted Chess lifecycle lock"),
            vec![
                LifecycleEvent::StateWritten,
                LifecycleEvent::FrameSubmitted,
                LifecycleEvent::StateWritten,
                LifecycleEvent::FrameSubmitted,
                LifecycleEvent::StateWritten,
                LifecycleEvent::FrameSubmitted,
            ]
        );
        assert_eq!(
            capture
                .projections
                .lock()
                .expect("scripted Chess projections lock")
                .len(),
            usize::from(MAX_MODEL_ATTEMPTS)
        );

        application
            .shutdown()
            .await
            .expect("retry exhaustion consumes the Chess owner");
        assert!(control.read_state().is_err());
    }
}
