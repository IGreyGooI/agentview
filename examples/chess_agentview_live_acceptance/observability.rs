use std::{
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc,
    },
    time::Instant,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use serde::Serialize;

use agentview::component::execution::{ApplicationFaultReason, ReactionPortFaultReason};
use agentview::provider::async_openai::OpenAiResponsesUsage;

use crate::{
    chess_actions::InvalidActionReason,
    game::{AutomaticDrawReason, DrawClaimBasis, GameOutcome},
    model::{AttemptResult, InfrastructureAbortReason, InfrastructureStage},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedSide {
    White,
    Black,
}

impl ObservedSide {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::White => "white",
            Self::Black => "black",
        }
    }
}

impl From<chess::Color> for ObservedSide {
    fn from(value: chess::Color) -> Self {
        match value {
            chess::Color::White => Self::White,
            chess::Color::Black => Self::Black,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedInvalidActionReason {
    InvalidXml,
    MissingAction,
    MultipleActions,
    InvalidUci,
    IllegalMove,
    ChooseMoveUnavailable,
    MoveAndOfferDrawUnavailable,
    ResignUnavailable,
    AcceptDrawUnavailable,
    ClaimDrawUnavailable,
}

impl ObservedInvalidActionReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidXml => "invalid_xml",
            Self::MissingAction => "missing_action",
            Self::MultipleActions => "multiple_actions",
            Self::InvalidUci => "invalid_uci",
            Self::IllegalMove => "illegal_move",
            Self::ChooseMoveUnavailable => "choose_move_unavailable",
            Self::MoveAndOfferDrawUnavailable => "move_and_offer_draw_unavailable",
            Self::ResignUnavailable => "resign_unavailable",
            Self::AcceptDrawUnavailable => "accept_draw_unavailable",
            Self::ClaimDrawUnavailable => "claim_draw_unavailable",
        }
    }
}

impl From<InvalidActionReason> for ObservedInvalidActionReason {
    fn from(value: InvalidActionReason) -> Self {
        match value {
            InvalidActionReason::InvalidXml => Self::InvalidXml,
            InvalidActionReason::MissingAction => Self::MissingAction,
            InvalidActionReason::MultipleActions => Self::MultipleActions,
            InvalidActionReason::InvalidUci(_) => Self::InvalidUci,
            InvalidActionReason::IllegalMove(_) => Self::IllegalMove,
            InvalidActionReason::ActionUnavailable { action, .. } => match action {
                crate::chess_actions::ChessActionKind::ChooseMove => Self::ChooseMoveUnavailable,
                crate::chess_actions::ChessActionKind::MoveAndOfferDraw => {
                    Self::MoveAndOfferDrawUnavailable
                }
                crate::chess_actions::ChessActionKind::Resign => Self::ResignUnavailable,
                crate::chess_actions::ChessActionKind::AcceptDraw => Self::AcceptDrawUnavailable,
                crate::chess_actions::ChessActionKind::ClaimDraw => Self::ClaimDrawUnavailable,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedInfrastructureStage {
    ModelReaction,
    AuthoritativeState,
    Engine,
    WholeGame,
    Shutdown,
    TerminalStatus,
    Jsonl,
}

impl ObservedInfrastructureStage {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ModelReaction => "model_reaction",
            Self::AuthoritativeState => "authoritative_state",
            Self::Engine => "engine",
            Self::WholeGame => "whole_game",
            Self::Shutdown => "shutdown",
            Self::TerminalStatus => "terminal_status",
            Self::Jsonl => "jsonl",
        }
    }
}

impl From<InfrastructureStage> for ObservedInfrastructureStage {
    fn from(value: InfrastructureStage) -> Self {
        match value {
            InfrastructureStage::ModelReaction => Self::ModelReaction,
            InfrastructureStage::AuthoritativeState => Self::AuthoritativeState,
            InfrastructureStage::Engine => Self::Engine,
            InfrastructureStage::WholeGame => Self::WholeGame,
            InfrastructureStage::Shutdown => Self::Shutdown,
            InfrastructureStage::TerminalStatus => Self::TerminalStatus,
            InfrastructureStage::Jsonl => Self::Jsonl,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedInfrastructureReason {
    NonProviderRuntime,
    ProviderTransport,
    ProviderRequestPreparation,
    ProviderRequestTransport,
    ProviderAuthentication,
    ProviderAuthorization,
    ProviderRateLimited,
    ProviderUpstreamStatus,
    ProviderResponseProtocol,
    ProviderStreamTransport,
    ProviderStreamTimeout,
    ProviderOutputLimit,
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

impl ObservedInfrastructureReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NonProviderRuntime => "non_provider_runtime",
            Self::ProviderTransport => "provider_transport",
            Self::ProviderRequestPreparation => "provider_request_preparation",
            Self::ProviderRequestTransport => "provider_request_transport",
            Self::ProviderAuthentication => "provider_authentication",
            Self::ProviderAuthorization => "provider_authorization",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::ProviderUpstreamStatus => "provider_upstream_status",
            Self::ProviderResponseProtocol => "provider_response_protocol",
            Self::ProviderStreamTransport => "provider_stream_transport",
            Self::ProviderStreamTimeout => "provider_stream_timeout",
            Self::ProviderOutputLimit => "provider_output_limit",
            Self::ReactionTimeout => "reaction_timeout",
            Self::StateWriteFailure => "state_write_failure",
            Self::StateUnavailable => "state_unavailable",
            Self::StateUnfinished => "state_unfinished",
            Self::TextIncomplete => "text_incomplete",
            Self::StateMismatch => "state_mismatch",
            Self::ProviderResponseCaptureFailure => "provider_response_capture_failure",
            Self::EngineFailure => "engine_failure",
            Self::EngineTimeout => "engine_timeout",
            Self::WholeGameTimeout => "whole_game_timeout",
            Self::ShutdownFailure => "shutdown_failure",
            Self::StatusWriteFailure => "status_write_failure",
            Self::TraceWriteFailure => "trace_write_failure",
        }
    }
}

impl From<InfrastructureAbortReason> for ObservedInfrastructureReason {
    fn from(value: InfrastructureAbortReason) -> Self {
        match value {
            InfrastructureAbortReason::Application { reason, .. } => {
                observed_application_reason(reason)
            }
            InfrastructureAbortReason::ReactionTimeout => Self::ReactionTimeout,
            InfrastructureAbortReason::StateWriteFailure => Self::StateWriteFailure,
            InfrastructureAbortReason::StateUnavailable => Self::StateUnavailable,
            InfrastructureAbortReason::StateUnfinished => Self::StateUnfinished,
            InfrastructureAbortReason::TextIncomplete => Self::TextIncomplete,
            InfrastructureAbortReason::StateMismatch => Self::StateMismatch,
            InfrastructureAbortReason::ProviderResponseCaptureFailure => {
                Self::ProviderResponseCaptureFailure
            }
            InfrastructureAbortReason::EngineFailure => Self::EngineFailure,
            InfrastructureAbortReason::EngineTimeout => Self::EngineTimeout,
            InfrastructureAbortReason::WholeGameTimeout => Self::WholeGameTimeout,
            InfrastructureAbortReason::ShutdownFailure => Self::ShutdownFailure,
            InfrastructureAbortReason::StatusWriteFailure => Self::StatusWriteFailure,
            InfrastructureAbortReason::TraceWriteFailure => Self::TraceWriteFailure,
        }
    }
}

fn observed_application_reason(reason: ApplicationFaultReason) -> ObservedInfrastructureReason {
    let ApplicationFaultReason::Port(reason) = reason else {
        return ObservedInfrastructureReason::NonProviderRuntime;
    };
    match reason {
        ReactionPortFaultReason::Declaration => ObservedInfrastructureReason::ProviderTransport,
        ReactionPortFaultReason::RequestPreparation => {
            ObservedInfrastructureReason::ProviderRequestPreparation
        }
        ReactionPortFaultReason::Transport => {
            ObservedInfrastructureReason::ProviderRequestTransport
        }
        ReactionPortFaultReason::Authentication => {
            ObservedInfrastructureReason::ProviderAuthentication
        }
        ReactionPortFaultReason::Authorization => {
            ObservedInfrastructureReason::ProviderAuthorization
        }
        ReactionPortFaultReason::RateLimited => ObservedInfrastructureReason::ProviderRateLimited,
        ReactionPortFaultReason::UpstreamRejected => {
            ObservedInfrastructureReason::ProviderUpstreamStatus
        }
        ReactionPortFaultReason::ResponseProtocol => {
            ObservedInfrastructureReason::ProviderResponseProtocol
        }
        ReactionPortFaultReason::StreamTransport => {
            ObservedInfrastructureReason::ProviderStreamTransport
        }
        ReactionPortFaultReason::StreamTimeout => {
            ObservedInfrastructureReason::ProviderStreamTimeout
        }
        ReactionPortFaultReason::OutputLimit => ObservedInfrastructureReason::ProviderOutputLimit,
        ReactionPortFaultReason::Other => ObservedInfrastructureReason::NonProviderRuntime,
        _ => ObservedInfrastructureReason::NonProviderRuntime,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum ObservedAttemptResult {
    ActionAccepted {
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        uci: Option<String>,
    },
    Correctable {
        reason: ObservedInvalidActionReason,
    },
    InfrastructureAbort {
        stage: ObservedInfrastructureStage,
        reason_code: ObservedInfrastructureReason,
    },
}

impl From<AttemptResult> for ObservedAttemptResult {
    fn from(value: AttemptResult) -> Self {
        match value {
            AttemptResult::ActionAccepted(action) => Self::ActionAccepted {
                action: action.kind().code().to_owned(),
                uci: action
                    .move_candidate()
                    .map(|candidate| candidate.to_string()),
            },
            AttemptResult::Correctable(reason) => Self::Correctable {
                reason: reason.into(),
            },
            AttemptResult::InfrastructureAbort { stage, reason_code } => {
                Self::InfrastructureAbort {
                    stage: stage.into(),
                    reason_code: reason_code.into(),
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum ObservedStockfishResult {
    Candidate {
        uci: String,
    },
    InfrastructureAbort {
        reason_code: ObservedInfrastructureReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ObservedTerminal {
    Checkmate {
        winner: ObservedSide,
    },
    Stalemate,
    Resignation {
        resigned: ObservedSide,
        winner: ObservedSide,
    },
    DrawAccepted,
    DrawClaimed {
        basis: ObservedDrawClaimBasis,
    },
    AutomaticDraw {
        reason: ObservedAutomaticDrawReason,
    },
    ModelForfeit {
        final_reason: ObservedInvalidActionReason,
        attempts: u8,
    },
    InfrastructureAbort {
        stage: ObservedInfrastructureStage,
        reason_code: ObservedInfrastructureReason,
    },
    PlyLimitReached {
        max_plies: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedDrawClaimBasis {
    ThreefoldRepetition,
    FiftyMoveRule,
    ThreefoldRepetitionAndFiftyMoveRule,
}

impl From<DrawClaimBasis> for ObservedDrawClaimBasis {
    fn from(value: DrawClaimBasis) -> Self {
        match value {
            DrawClaimBasis::ThreefoldRepetition => Self::ThreefoldRepetition,
            DrawClaimBasis::FiftyMoveRule => Self::FiftyMoveRule,
            DrawClaimBasis::ThreefoldRepetitionAndFiftyMoveRule => {
                Self::ThreefoldRepetitionAndFiftyMoveRule
            }
        }
    }
}

impl ObservedDrawClaimBasis {
    fn code(self) -> &'static str {
        match self {
            Self::ThreefoldRepetition => "threefold_repetition",
            Self::FiftyMoveRule => "fifty_move_rule",
            Self::ThreefoldRepetitionAndFiftyMoveRule => "threefold_repetition_and_fifty_move_rule",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedAutomaticDrawReason {
    FivefoldRepetition,
    SeventyFiveMoveRule,
    DeadPosition,
}

impl From<AutomaticDrawReason> for ObservedAutomaticDrawReason {
    fn from(value: AutomaticDrawReason) -> Self {
        match value {
            AutomaticDrawReason::FivefoldRepetition => Self::FivefoldRepetition,
            AutomaticDrawReason::SeventyFiveMoveRule => Self::SeventyFiveMoveRule,
            AutomaticDrawReason::DeadPosition => Self::DeadPosition,
        }
    }
}

impl ObservedAutomaticDrawReason {
    fn code(self) -> &'static str {
        match self {
            Self::FivefoldRepetition => "fivefold_repetition",
            Self::SeventyFiveMoveRule => "seventy_five_move_rule",
            Self::DeadPosition => "dead_position",
        }
    }
}

impl ObservedTerminal {
    pub(crate) fn from_outcome(value: GameOutcome, max_plies: usize) -> Self {
        match value {
            GameOutcome::Checkmate { winner } => Self::Checkmate {
                winner: winner.into(),
            },
            GameOutcome::Stalemate => Self::Stalemate,
            GameOutcome::Resignation { resigned, winner } => Self::Resignation {
                resigned: resigned.into(),
                winner: winner.into(),
            },
            GameOutcome::DrawAccepted => Self::DrawAccepted,
            GameOutcome::DrawClaimed { basis } => Self::DrawClaimed {
                basis: basis.into(),
            },
            GameOutcome::AutomaticDraw { reason } => Self::AutomaticDraw {
                reason: reason.into(),
            },
            GameOutcome::ModelForfeit {
                final_reason,
                attempts,
            } => Self::ModelForfeit {
                final_reason: final_reason.into(),
                attempts,
            },
            GameOutcome::InfrastructureAbort { stage, reason_code } => Self::InfrastructureAbort {
                stage: stage.into(),
                reason_code: reason_code.into(),
            },
            GameOutcome::PlyLimitReached => Self::PlyLimitReached { max_plies },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CleanupDisposition {
    pub(crate) engine_shutdown_observed: bool,
    pub(crate) application_shutdown_observed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservedTraceArtifactDisposition {
    Preserved,
    Removed,
    RemovalFailed,
}

impl ObservedTraceArtifactDisposition {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Preserved => "preserved",
            Self::Removed => "removed",
            Self::RemovalFailed => "removal_failed",
        }
    }
}

impl From<TraceArtifactDisposition> for ObservedTraceArtifactDisposition {
    fn from(value: TraceArtifactDisposition) -> Self {
        match value {
            TraceArtifactDisposition::NotCreated | TraceArtifactDisposition::Preserved => {
                Self::Preserved
            }
            TraceArtifactDisposition::Removed => Self::Removed,
            TraceArtifactDisposition::RemovalFailed => Self::RemovalFailed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderEndpointClass {
    OpenAiHttps,
    CustomHttps,
    LoopbackHttp,
}

impl ProviderEndpointClass {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::OpenAiHttps => "openai_https",
            Self::CustomHttps => "custom_https",
            Self::LoopbackHttp => "loopback_http",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct EffectiveProviderConfig {
    model: String,
    endpoint_class: ProviderEndpointClass,
    endpoint_authority: String,
}

impl EffectiveProviderConfig {
    pub(crate) fn new(
        model: String,
        endpoint_class: ProviderEndpointClass,
        endpoint_authority: String,
    ) -> Self {
        Self {
            model,
            endpoint_class,
            endpoint_authority,
        }
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn endpoint_class(&self) -> ProviderEndpointClass {
        self.endpoint_class
    }

    pub(crate) fn endpoint_authority(&self) -> &str {
        &self.endpoint_authority
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProviderResponseUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) cached_tokens: Option<u64>,
}

impl ProviderResponseUsage {
    pub(crate) fn new(input_tokens: Option<u64>, cached_tokens: Option<u64>) -> Self {
        Self {
            input_tokens,
            cached_tokens,
        }
    }

    pub(crate) fn with_context(
        self,
        ply: usize,
        turn_id: impl Into<String>,
        attempt_index: u8,
    ) -> ProviderResponseEvidence {
        ProviderResponseEvidence {
            ply,
            turn_id: turn_id.into(),
            attempt_index,
            input_tokens: self.input_tokens,
            cached_tokens: self.cached_tokens,
        }
    }
}

impl From<OpenAiResponsesUsage> for ProviderResponseUsage {
    fn from(value: OpenAiResponsesUsage) -> Self {
        Self::new(value.input_tokens(), value.cached_tokens())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderResponseEvidence {
    pub(crate) ply: usize,
    pub(crate) turn_id: String,
    pub(crate) attempt_index: u8,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) cached_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderResponseSummary {
    pub(crate) response_count: usize,
    pub(crate) cached_tokens: Vec<Option<u64>>,
    pub(crate) continuity_unknown: bool,
    pub(crate) unknown_transition_count: usize,
    pub(crate) decrease_transition_count: usize,
    pub(crate) positive_to_zero_transition_count: usize,
}

impl ProviderResponseSummary {
    pub(crate) fn from_responses(responses: &[ProviderResponseEvidence]) -> Self {
        let cached_tokens = responses
            .iter()
            .map(|response| response.cached_tokens)
            .collect::<Vec<_>>();
        let continuity_unknown = cached_tokens.iter().any(Option::is_none);
        let (
            unknown_transition_count,
            decrease_transition_count,
            positive_to_zero_transition_count,
        ) = cached_tokens.windows(2).fold(
            (0, 0, 0),
            |(unknown, decrease, positive_to_zero), pair| match (pair[0], pair[1]) {
                (Some(previous), Some(current)) => (
                    unknown,
                    decrease + usize::from(current < previous),
                    positive_to_zero + usize::from(previous > 0 && current == 0),
                ),
                _ => (unknown + 1, decrease, positive_to_zero),
            },
        );
        Self {
            response_count: responses.len(),
            cached_tokens,
            continuity_unknown,
            unknown_transition_count,
            decrease_transition_count,
            positive_to_zero_transition_count,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProviderResponseContext {
    pub(crate) ply: usize,
    pub(crate) turn_id: String,
    pub(crate) attempt_index: u8,
}

#[derive(Default)]
struct ProviderResponseCaptureState {
    accepted: AtomicUsize,
    failed: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct ProviderResponseCapture {
    sender: SyncSender<ProviderResponseUsage>,
    state: Arc<ProviderResponseCaptureState>,
}

pub(crate) struct ProviderResponseInbox {
    receiver: Receiver<ProviderResponseUsage>,
    state: Arc<ProviderResponseCaptureState>,
}

pub(crate) fn provider_response_capture(
    capacity: usize,
) -> (ProviderResponseCapture, ProviderResponseInbox) {
    assert!(
        capacity > 0,
        "provider response capture must be bounded above zero"
    );
    let (sender, receiver) = mpsc::sync_channel(capacity);
    let state = Arc::new(ProviderResponseCaptureState::default());
    (
        ProviderResponseCapture {
            sender,
            state: Arc::clone(&state),
        },
        ProviderResponseInbox { receiver, state },
    )
}

impl ProviderResponseCapture {
    pub(crate) fn capture(&self, usage: ProviderResponseUsage) {
        self.state.accepted.fetch_add(1, Ordering::SeqCst);
        if self.sender.try_send(usage).is_err() {
            self.state.failed.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn capture_openai(&self, usage: OpenAiResponsesUsage) {
        self.capture(usage.into());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "event_type", content = "data", rename_all = "snake_case")]
pub(crate) enum RunEvent {
    RunStarted {
        provider: EffectiveProviderConfig,
        initial_fen: String,
        reaction_timeout_ms: u64,
        engine_timeout_ms: u64,
        whole_game_deadline_ms: u64,
        ply_limit: usize,
        engine_nodes: u64,
    },
    TurnStarted {
        ply: usize,
        side: ObservedSide,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        fen: String,
    },
    ModelReactionStarted {
        ply: usize,
        turn_id: String,
        attempt_index: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        corrective_reason: Option<ObservedInvalidActionReason>,
    },
    ModelReactionCompleted {
        ply: usize,
        turn_id: String,
        attempt_index: u8,
        duration_ms: u64,
        #[serde(flatten)]
        outcome: ObservedAttemptResult,
    },
    ProviderResponseCompleted {
        ply: usize,
        turn_id: String,
        attempt_index: u8,
        input_tokens: Option<u64>,
        cached_tokens: Option<u64>,
    },
    StockfishRequestStarted {
        ply: usize,
        fen: String,
        nodes: u64,
    },
    StockfishRequestCompleted {
        ply: usize,
        duration_ms: u64,
        #[serde(flatten)]
        outcome: ObservedStockfishResult,
    },
    MoveCommitted {
        ply: usize,
        side: ObservedSide,
        uci: String,
        fen_before: String,
        fen_after: String,
    },
    Terminal {
        outcome: ObservedTerminal,
        final_fen: String,
        committed_plies: usize,
        model_calls: usize,
        duration_ms: u64,
        cleanup: CleanupDisposition,
        provider_responses: ProviderResponseSummary,
        #[serde(skip_serializing_if = "Option::is_none")]
        trace_artifact_disposition: Option<ObservedTraceArtifactDisposition>,
    },
}

impl RunEvent {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }
}

pub(crate) const TRACE_SCHEMA_VERSION: u16 = 1;
pub(crate) const MAX_TRACE_LINE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraceWriteError {
    EventTooLarge,
    WriteFailed,
    IntegrityLost {
        artifact_disposition: TraceArtifactDisposition,
    },
    FlushFailed {
        artifact_disposition: TraceArtifactDisposition,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraceArtifactDisposition {
    NotCreated,
    Preserved,
    Removed,
    RemovalFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineWriteError {
    WriteFailed,
    IntegrityLost {
        artifact_disposition: TraceArtifactDisposition,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecureTraceCreateErrorKind {
    TargetRejected,
    ValidationFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SecureTraceCreateError {
    kind: SecureTraceCreateErrorKind,
    artifact_disposition: TraceArtifactDisposition,
}

impl SecureTraceCreateError {
    pub(crate) fn kind(self) -> SecureTraceCreateErrorKind {
        self.kind
    }

    pub(crate) fn artifact_disposition(self) -> TraceArtifactDisposition {
        self.artifact_disposition
    }
}

impl TraceArtifactDisposition {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NotCreated => "not_created",
            Self::Preserved => "preserved",
            Self::Removed => "removed",
            Self::RemovalFailed => "removal_failed",
        }
    }
}

pub(crate) trait CompleteLineWriter {
    fn write_complete_line(&mut self, line: &[u8]) -> Result<(), LineWriteError>;
    fn flush(&mut self) -> Result<(), LineWriteError>;
}

impl CompleteLineWriter for Vec<u8> {
    fn write_complete_line(&mut self, line: &[u8]) -> Result<(), LineWriteError> {
        self.extend_from_slice(line);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), LineWriteError> {
        Ok(())
    }
}

pub(crate) trait TraceFileBackend {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn rollback_to(&mut self, committed_len: u64) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

impl TraceFileBackend for File {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Write::write_all(self, bytes)
    }

    fn rollback_to(&mut self, committed_len: u64) -> io::Result<()> {
        self.set_len(committed_len)?;
        self.seek(SeekFrom::Start(committed_len))?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(self)
    }
}

#[derive(Debug)]
pub(crate) struct SecureTraceFile<B = File> {
    backend: B,
    path: PathBuf,
    artifact_identity: Option<TraceArtifactIdentity>,
    committed_len: u64,
    integrity_failure: Option<TraceArtifactDisposition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TraceArtifactIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl TraceArtifactIdentity {
    fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Self::from_metadata(&metadata)
    }

    fn from_path(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        Self::from_metadata(&metadata)
    }

    fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        if !metadata.file_type().is_file() {
            return Err(io::Error::other("trace artifact was not a regular file"));
        }
        #[cfg(unix)]
        return Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        });
        #[cfg(not(unix))]
        Ok(Self {})
    }

    fn matches_path(self, path: &Path) -> bool {
        #[cfg(unix)]
        return Self::from_path(path).is_ok_and(|found| found == self);
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    }
}

impl SecureTraceFile<File> {
    pub(crate) fn create(path: &Path) -> Result<Self, SecureTraceCreateError> {
        Self::create_with_validator(path, |file| {
            file.metadata()
                .map(|metadata| metadata.file_type().is_file())
        })
    }

    fn create_with_validator(
        path: &Path,
        validate: impl FnOnce(&File) -> io::Result<bool>,
    ) -> Result<Self, SecureTraceCreateError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path).map_err(|_| SecureTraceCreateError {
            kind: SecureTraceCreateErrorKind::TargetRejected,
            artifact_disposition: TraceArtifactDisposition::NotCreated,
        })?;
        let artifact_identity =
            TraceArtifactIdentity::from_file(&file).map_err(|_| SecureTraceCreateError {
                kind: SecureTraceCreateErrorKind::ValidationFailed,
                artifact_disposition: TraceArtifactDisposition::RemovalFailed,
            })?;
        if !matches!(validate(&file), Ok(true)) {
            return Err(SecureTraceCreateError {
                kind: SecureTraceCreateErrorKind::ValidationFailed,
                artifact_disposition: remove_owned_artifact(path, Some(artifact_identity)),
            });
        }
        Ok(Self {
            backend: file,
            path: path.to_owned(),
            artifact_identity: Some(artifact_identity),
            committed_len: 0,
            integrity_failure: None,
        })
    }
}

impl<B> SecureTraceFile<B> {
    pub(crate) fn discard(self) -> TraceArtifactDisposition {
        remove_owned_artifact(&self.path, self.artifact_identity)
    }
}

impl<B: TraceFileBackend> CompleteLineWriter for SecureTraceFile<B> {
    fn write_complete_line(&mut self, line: &[u8]) -> Result<(), LineWriteError> {
        if let Some(artifact_disposition) = self.integrity_failure {
            return Err(LineWriteError::IntegrityLost {
                artifact_disposition,
            });
        }
        if self.backend.write_all(line).is_err() {
            if self.backend.rollback_to(self.committed_len).is_ok() {
                return Err(LineWriteError::WriteFailed);
            }
            let artifact_disposition = remove_owned_artifact(&self.path, self.artifact_identity);
            self.integrity_failure = Some(artifact_disposition);
            return Err(LineWriteError::IntegrityLost {
                artifact_disposition,
            });
        }
        self.committed_len = self.committed_len.saturating_add(line.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), LineWriteError> {
        if let Some(artifact_disposition) = self.integrity_failure {
            return Err(LineWriteError::IntegrityLost {
                artifact_disposition,
            });
        }
        if self.backend.flush().is_err() {
            let artifact_disposition = remove_owned_artifact(&self.path, self.artifact_identity);
            self.integrity_failure = Some(artifact_disposition);
            return Err(LineWriteError::IntegrityLost {
                artifact_disposition,
            });
        }
        if !self
            .artifact_identity
            .is_some_and(|identity| identity.matches_path(&self.path))
        {
            let artifact_disposition = TraceArtifactDisposition::RemovalFailed;
            self.integrity_failure = Some(artifact_disposition);
            return Err(LineWriteError::IntegrityLost {
                artifact_disposition,
            });
        }
        Ok(())
    }
}

fn remove_owned_artifact(
    path: &Path,
    expected_identity: Option<TraceArtifactIdentity>,
) -> TraceArtifactDisposition {
    if !expected_identity.is_some_and(|identity| identity.matches_path(path)) {
        return TraceArtifactDisposition::RemovalFailed;
    }
    match fs::remove_file(path) {
        Ok(()) => TraceArtifactDisposition::Removed,
        Err(_) => TraceArtifactDisposition::RemovalFailed,
    }
}

#[derive(Serialize)]
struct EventEnvelope<'a> {
    schema_version: u16,
    sequence: u64,
    elapsed_ms: u64,
    run_id: &'a str,
    #[serde(flatten)]
    event: &'a RunEvent,
}

pub(crate) struct JsonlEventWriter<W> {
    sink: W,
    run_id: String,
    next_sequence: u64,
}

impl<W: CompleteLineWriter> JsonlEventWriter<W> {
    pub(crate) fn new(sink: W, run_id: String) -> Self {
        Self {
            sink,
            run_id,
            next_sequence: 1,
        }
    }

    pub(crate) fn emit(&mut self, elapsed_ms: u64, event: RunEvent) -> Result<(), TraceWriteError> {
        let envelope = EventEnvelope {
            schema_version: TRACE_SCHEMA_VERSION,
            sequence: self.next_sequence,
            elapsed_ms,
            run_id: &self.run_id,
            event: &event,
        };
        let mut line = serde_json::to_vec(&envelope).map_err(|_| TraceWriteError::WriteFailed)?;
        line.push(b'\n');
        if line.len() > MAX_TRACE_LINE_BYTES {
            return Err(TraceWriteError::EventTooLarge);
        }
        self.sink
            .write_complete_line(&line)
            .map_err(|failure| match failure {
                LineWriteError::WriteFailed => TraceWriteError::WriteFailed,
                LineWriteError::IntegrityLost {
                    artifact_disposition,
                } => TraceWriteError::IntegrityLost {
                    artifact_disposition,
                },
            })?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        if event.is_terminal() {
            self.sink.flush().map_err(|failure| match failure {
                LineWriteError::WriteFailed => TraceWriteError::FlushFailed {
                    artifact_disposition: TraceArtifactDisposition::Preserved,
                },
                LineWriteError::IntegrityLost {
                    artifact_disposition,
                } => TraceWriteError::FlushFailed {
                    artifact_disposition,
                },
            })?;
        }
        Ok(())
    }

    pub(crate) fn into_inner(self) -> W {
        self.sink
    }
}

pub(crate) struct StatusReporter<W> {
    sink: W,
}

impl<W: Write> StatusReporter<W> {
    pub(crate) fn new(sink: W) -> Self {
        Self { sink }
    }

    pub(crate) fn report(&mut self, event: &RunEvent) -> io::Result<()> {
        match event {
            RunEvent::RunStarted { provider, .. } => writeln!(
                self.sink,
                "run started model={} endpoint_class={} endpoint_authority={}",
                provider.model(),
                provider.endpoint_class().code(),
                provider.endpoint_authority(),
            ),
            RunEvent::TurnStarted { ply, side, .. } => {
                writeln!(self.sink, "{} turn ply={ply}", side.code())
            }
            RunEvent::ModelReactionStarted {
                attempt_index,
                corrective_reason,
                ..
            } => match corrective_reason {
                Some(reason) => writeln!(
                    self.sink,
                    "white attempt {attempt_index} correction={}",
                    reason.code()
                ),
                None => writeln!(self.sink, "white attempt {attempt_index}"),
            },
            RunEvent::ModelReactionCompleted { outcome, .. } => {
                report_attempt(&mut self.sink, outcome)
            }
            RunEvent::ProviderResponseCompleted {
                ply,
                turn_id,
                attempt_index,
                input_tokens,
                cached_tokens,
            } => writeln!(
                self.sink,
                "provider response completed ply={ply} turn_id={turn_id} attempt_index={attempt_index} input_tokens={} cached_tokens={}",
                observed_token_count(*input_tokens),
                observed_token_count(*cached_tokens),
            ),
            RunEvent::StockfishRequestStarted { nodes, .. } => {
                writeln!(self.sink, "black engine work nodes={nodes}")
            }
            RunEvent::StockfishRequestCompleted { outcome, .. } => {
                report_stockfish(&mut self.sink, outcome)
            }
            RunEvent::MoveCommitted { side, uci, ply, .. } => {
                writeln!(self.sink, "{} accepted {uci} ply={ply}", side.code())
            }
            RunEvent::Terminal {
                outcome,
                cleanup,
                provider_responses,
                trace_artifact_disposition,
                ..
            } => report_terminal(
                &mut self.sink,
                outcome.clone(),
                *cleanup,
                provider_responses,
                *trace_artifact_disposition,
            ),
        }
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

fn observed_token_count(count: Option<u64>) -> String {
    count.map_or_else(|| "unreported".to_owned(), |count| count.to_string())
}

fn report_attempt(output: &mut impl Write, outcome: &ObservedAttemptResult) -> io::Result<()> {
    match outcome {
        ObservedAttemptResult::ActionAccepted { action, uci } => match uci {
            Some(uci) => writeln!(
                output,
                "white reaction completed result=action_accepted action={action} uci={uci}"
            ),
            None => writeln!(
                output,
                "white reaction completed result=action_accepted action={action}"
            ),
        },
        ObservedAttemptResult::Correctable { reason } => writeln!(
            output,
            "white reaction completed result=correctable reason={}",
            reason.code()
        ),
        ObservedAttemptResult::InfrastructureAbort { stage, reason_code } => writeln!(
            output,
            "white reaction aborted stage={} reason={}",
            stage.code(),
            reason_code.code()
        ),
    }
}

fn report_stockfish(output: &mut impl Write, outcome: &ObservedStockfishResult) -> io::Result<()> {
    match outcome {
        ObservedStockfishResult::Candidate { uci } => {
            writeln!(output, "black engine completed candidate={uci}")
        }
        ObservedStockfishResult::InfrastructureAbort { reason_code } => {
            writeln!(output, "black engine aborted reason={}", reason_code.code())
        }
    }
}

fn report_terminal(
    output: &mut impl Write,
    outcome: ObservedTerminal,
    cleanup: CleanupDisposition,
    provider_responses: &ProviderResponseSummary,
    trace_artifact_disposition: Option<ObservedTraceArtifactDisposition>,
) -> io::Result<()> {
    let cleanup = if cleanup.engine_shutdown_observed && cleanup.application_shutdown_observed {
        "complete"
    } else {
        "incomplete"
    };
    let terminal = match outcome {
        ObservedTerminal::Checkmate { winner } => {
            format!("terminal checkmate winner={}", winner.code())
        }
        ObservedTerminal::Stalemate => "terminal stalemate".to_owned(),
        ObservedTerminal::Resignation { resigned, winner } => format!(
            "terminal resignation resigned={} winner={}",
            resigned.code(),
            winner.code()
        ),
        ObservedTerminal::DrawAccepted => "terminal draw_accepted".to_owned(),
        ObservedTerminal::DrawClaimed { basis } => {
            format!("terminal draw_claimed basis={}", basis.code())
        }
        ObservedTerminal::AutomaticDraw { reason } => {
            format!("terminal automatic_draw reason={}", reason.code())
        }
        ObservedTerminal::ModelForfeit {
            final_reason,
            attempts,
        } => format!(
            "terminal model_forfeit reason={} attempts={attempts}",
            final_reason.code()
        ),
        ObservedTerminal::InfrastructureAbort { stage, reason_code } => format!(
            "terminal infrastructure_abort stage={} reason={}",
            stage.code(),
            reason_code.code()
        ),
        ObservedTerminal::PlyLimitReached { max_plies } => {
            format!("terminal ply_limit_reached max_plies={max_plies}")
        }
    };
    let cached_tokens = provider_responses
        .cached_tokens
        .iter()
        .map(|count| observed_token_count(*count))
        .collect::<Vec<_>>()
        .join(",");
    let accounting = format!(
        "provider_responses={} cached_tokens=[{}] continuity_unknown={} unknown_transitions={} decrease_transitions={} positive_to_zero_transitions={}",
        provider_responses.response_count,
        cached_tokens,
        provider_responses.continuity_unknown,
        provider_responses.unknown_transition_count,
        provider_responses.decrease_transition_count,
        provider_responses.positive_to_zero_transition_count,
    );
    match trace_artifact_disposition {
        Some(disposition) => writeln!(
            output,
            "{terminal} cleanup={cleanup} {accounting} trace_artifact={}",
            disposition.code()
        ),
        None => writeln!(output, "{terminal} cleanup={cleanup} {accounting}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationFailure {
    StatusWrite,
    ProviderResponseCapture,
    TraceWrite {
        artifact_disposition: TraceArtifactDisposition,
        terminal_outcome_published: bool,
    },
}

impl ObservationFailure {
    pub(crate) fn classification(
        self,
    ) -> (
        crate::model::InfrastructureStage,
        crate::model::InfrastructureAbortReason,
    ) {
        match self {
            Self::StatusWrite => (
                crate::model::InfrastructureStage::TerminalStatus,
                crate::model::InfrastructureAbortReason::StatusWriteFailure,
            ),
            Self::ProviderResponseCapture => (
                crate::model::InfrastructureStage::ModelReaction,
                crate::model::InfrastructureAbortReason::ProviderResponseCaptureFailure,
            ),
            Self::TraceWrite { .. } => (
                crate::model::InfrastructureStage::Jsonl,
                crate::model::InfrastructureAbortReason::TraceWriteFailure,
            ),
        }
    }

    pub(crate) fn terminal_outcome_published(self) -> bool {
        matches!(
            self,
            Self::TraceWrite {
                terminal_outcome_published: true,
                ..
            }
        )
    }
}

pub(crate) trait GameObserver {
    fn now_ms(&mut self) -> u64;
    fn record(&mut self, event: RunEvent) -> Result<(), ObservationFailure>;
    fn record_provider_response_completed(
        &mut self,
        _context: ProviderResponseContext,
        _required: bool,
    ) -> Result<(), ObservationFailure> {
        Ok(())
    }
    fn provider_response_evidence(&self) -> Vec<ProviderResponseEvidence> {
        Vec::new()
    }
    fn validate_provider_response_evidence(&self) -> Result<(), ObservationFailure> {
        Ok(())
    }
    fn record_terminal(&mut self, event: RunEvent) -> Result<(), ObservationFailure> {
        self.record(event)
    }
    fn trace_artifact_disposition(&self) -> Option<TraceArtifactDisposition>;
    fn status_output_complete(&self) -> bool;
}

pub(crate) trait MonotonicClock {
    fn elapsed_ms(&mut self) -> u64;
}

pub(crate) struct SystemClock {
    started: Instant,
}

impl SystemClock {
    pub(crate) fn start() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl MonotonicClock for SystemClock {
    fn elapsed_ms(&mut self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
}

pub(crate) struct LiveObserver<S, W, C> {
    status: StatusReporter<S>,
    trace: JsonlEventWriter<W>,
    clock: C,
    status_failed: bool,
    trace_failed: bool,
    trace_artifact_disposition: Option<TraceArtifactDisposition>,
    provider_responses: ProviderResponseInbox,
    observed_provider_responses: Vec<ProviderResponseEvidence>,
}

impl<S, W, C> LiveObserver<S, W, C>
where
    S: Write,
    W: CompleteLineWriter,
    C: MonotonicClock,
{
    pub(crate) fn new(
        status: S,
        trace: W,
        clock: C,
        run_id: String,
        provider_responses: ProviderResponseInbox,
    ) -> Self {
        Self {
            status: StatusReporter::new(status),
            trace: JsonlEventWriter::new(trace, run_id),
            clock,
            status_failed: false,
            trace_failed: false,
            trace_artifact_disposition: None,
            provider_responses,
            observed_provider_responses: Vec::new(),
        }
    }

    pub(crate) fn now_ms(&mut self) -> u64 {
        self.clock.elapsed_ms()
    }

    pub(crate) fn record(&mut self, event: RunEvent) -> Result<(), ObservationFailure> {
        let elapsed_ms = self.clock.elapsed_ms();
        let status_failed = !self.status_failed && self.status.report(&event).is_err();
        let trace_failure = if self.trace_failed {
            None
        } else {
            self.trace.emit(elapsed_ms, event).err()
        };
        self.status_failed |= status_failed;
        let new_trace_failure = self.capture_trace_failure(trace_failure);
        if status_failed {
            Err(ObservationFailure::StatusWrite)
        } else if let Some(artifact_disposition) = new_trace_failure {
            Err(ObservationFailure::TraceWrite {
                artifact_disposition,
                terminal_outcome_published: false,
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn record_provider_response_completed(
        &mut self,
        context: ProviderResponseContext,
        required: bool,
    ) -> Result<(), ObservationFailure> {
        let usage = match self.provider_responses.receiver.try_recv() {
            Ok(usage) => usage,
            Err(TryRecvError::Empty) if !required => return Ok(()),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                return Err(ObservationFailure::ProviderResponseCapture)
            }
        };
        let evidence = usage.with_context(context.ply, context.turn_id, context.attempt_index);
        let event = RunEvent::ProviderResponseCompleted {
            ply: evidence.ply,
            turn_id: evidence.turn_id.clone(),
            attempt_index: evidence.attempt_index,
            input_tokens: evidence.input_tokens,
            cached_tokens: evidence.cached_tokens,
        };
        self.observed_provider_responses.push(evidence);
        self.record(event)
    }

    pub(crate) fn reconcile_provider_responses(
        &self,
    ) -> Result<Vec<ProviderResponseEvidence>, ObservationFailure> {
        if self.provider_responses.state.failed.load(Ordering::SeqCst)
            || self
                .provider_responses
                .state
                .accepted
                .load(Ordering::SeqCst)
                != self.observed_provider_responses.len()
            || !provider_response_inbox_is_drained(self.provider_responses.receiver.try_recv())
        {
            return Err(ObservationFailure::ProviderResponseCapture);
        }
        Ok(self.observed_provider_responses.clone())
    }

    fn record_terminal_inner(&mut self, event: RunEvent) -> Result<(), ObservationFailure> {
        let elapsed_ms = self.clock.elapsed_ms();
        if !self.trace_failed {
            let trace_failure = self.trace.emit(elapsed_ms, event.clone()).err();
            let terminal_outcome_published = trace_failure.is_some_and(|failure| {
                matches!(
                    failure,
                    TraceWriteError::FlushFailed {
                        artifact_disposition: TraceArtifactDisposition::Preserved
                            | TraceArtifactDisposition::RemovalFailed,
                    }
                )
            });
            let new_trace_failure = self.capture_trace_failure(trace_failure);
            if let Some(artifact_disposition) = new_trace_failure {
                return Err(ObservationFailure::TraceWrite {
                    artifact_disposition,
                    terminal_outcome_published,
                });
            }
        }
        if !self.status_failed
            && (self.status.report(&event).is_err() || self.status.flush().is_err())
        {
            self.status_failed = true;
            return Err(ObservationFailure::StatusWrite);
        }
        Ok(())
    }

    fn capture_trace_failure(
        &mut self,
        failure: Option<TraceWriteError>,
    ) -> Option<TraceArtifactDisposition> {
        let failure = failure?;
        self.trace_failed = true;
        let disposition = match failure {
            TraceWriteError::EventTooLarge | TraceWriteError::WriteFailed => {
                TraceArtifactDisposition::Preserved
            }
            TraceWriteError::IntegrityLost {
                artifact_disposition,
            }
            | TraceWriteError::FlushFailed {
                artifact_disposition,
            } => artifact_disposition,
        };
        self.trace_artifact_disposition = Some(disposition);
        Some(disposition)
    }
}

fn provider_response_inbox_is_drained<T>(result: Result<T, TryRecvError>) -> bool {
    matches!(
        result,
        Err(TryRecvError::Empty | TryRecvError::Disconnected)
    )
}

#[cfg(test)]
mod provider_response_tests {
    use super::*;

    #[test]
    fn a_drained_inbox_remains_valid_after_the_provider_sender_is_dropped() {
        let (capture, inbox) = provider_response_capture(1);
        drop(capture);

        assert!(provider_response_inbox_is_drained(
            inbox.receiver.try_recv()
        ));
    }
}

impl<S, C> LiveObserver<S, SecureTraceFile, C> {
    pub(crate) fn discard_trace(self) -> TraceArtifactDisposition {
        self.trace.into_inner().discard()
    }
}

impl<S, W, C> GameObserver for LiveObserver<S, W, C>
where
    S: Write,
    W: CompleteLineWriter,
    C: MonotonicClock,
{
    fn now_ms(&mut self) -> u64 {
        LiveObserver::now_ms(self)
    }

    fn record(&mut self, event: RunEvent) -> Result<(), ObservationFailure> {
        LiveObserver::record(self, event)
    }

    fn record_provider_response_completed(
        &mut self,
        context: ProviderResponseContext,
        required: bool,
    ) -> Result<(), ObservationFailure> {
        LiveObserver::record_provider_response_completed(self, context, required)
    }

    fn provider_response_evidence(&self) -> Vec<ProviderResponseEvidence> {
        self.observed_provider_responses.clone()
    }

    fn validate_provider_response_evidence(&self) -> Result<(), ObservationFailure> {
        self.reconcile_provider_responses().map(|_| ())
    }

    fn record_terminal(&mut self, event: RunEvent) -> Result<(), ObservationFailure> {
        self.record_terminal_inner(event)
    }

    fn trace_artifact_disposition(&self) -> Option<TraceArtifactDisposition> {
        self.trace_artifact_disposition
    }

    fn status_output_complete(&self) -> bool {
        !self.status_failed
    }
}
