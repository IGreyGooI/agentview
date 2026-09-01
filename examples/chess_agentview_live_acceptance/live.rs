use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use agentview::{
    component::execution::ProviderIdentity,
    provider::{
        async_openai::{AsyncOpenAiResponsesProvider, AsyncOpenAiTransportConfig},
        codex_http_v1::{CodexHttpV1Encoder, CodexHttpV1Options, CODEX_HTTP_V1_PROFILE},
    },
};
use chess::{Board, BoardStatus, Color, MoveGen};
use url::{Host, Url};

use super::{
    chess_actions::ChessAction,
    chess_game_state::DrawState,
    game::{
        create_trace, finish_or_discard_incomplete, is_dead_position, random_run_id,
        run_provider_game_with_observer_until, AutomaticDrawReason, GameEvidence, GameLimits,
        GameOutcome, TraceSetupFailure,
    },
    model::{
        AttemptEvidence, AttemptResult, AttemptStart, InfrastructureAbortReason,
        InfrastructureStage, ModelTurnId, MAX_MODEL_ATTEMPTS,
    },
    observability::{
        provider_response_capture, EffectiveProviderConfig, LiveObserver, ProviderEndpointClass,
        ProviderResponseCapture, SystemClock, TraceArtifactDisposition,
    },
    uci::UciProcessConfig,
};

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const DEFAULT_STOCKFISH_PROGRAM: &str = "/usr/games/stockfish";
const LIVE_TRACE_DIRECTORY: &str = "target";
const LIVE_PROVIDER_BINDING: &str = "chess-agentview-live";
const MAX_SERIALIZED_REQUEST_WINDOW_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_TEXT_BYTES: usize = 256 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_API_KEY_BYTES: usize = 4 * 1024;

pub(crate) struct LiveConfig {
    model: String,
    api_key: String,
    api_base: String,
    effective_provider: EffectiveProviderConfig,
    stockfish_program: PathBuf,
    trace_directory: PathBuf,
    provider_request_timeout: Duration,
    limits: GameLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveConfigError {
    InvalidModel,
    MissingApiKey,
    InvalidApiKey,
    InvalidApiBase,
    InvalidStockfishProgram,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveSetupError {
    ProviderInitialization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveRunError {
    ProviderInitialization,
    InvalidStockfishProgram,
    RunIdentifier,
    TraceSetup {
        artifact_disposition: Option<TraceArtifactDisposition>,
    },
    RunnerSetup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LiveEvidenceError {
    InfrastructureAbort {
        stage: InfrastructureStage,
        reason_code: InfrastructureAbortReason,
    },
    NonTerminalOutcome,
    MissingCompletedReaction,
    MissingStockfishMove,
    IncompleteCleanup,
    InconsistentGameEvidence,
}

impl LiveConfigError {
    pub(crate) fn reason_code(self) -> &'static str {
        match self {
            Self::InvalidModel => "invalid_model",
            Self::MissingApiKey => "missing_api_key",
            Self::InvalidApiKey => "invalid_api_key",
            Self::InvalidApiBase => "invalid_api_base",
            Self::InvalidStockfishProgram => "invalid_stockfish_program",
        }
    }
}

pub(crate) fn build_responses_provider(
    config: &LiveConfig,
    prompt_cache_key: &str,
    response_capture: ProviderResponseCapture,
) -> Result<AsyncOpenAiResponsesProvider, LiveSetupError> {
    let transport = AsyncOpenAiTransportConfig::new(config.api_base(), config.api_key())
        .and_then(|transport| {
            transport.with_timeouts(
                Duration::from_secs(10),
                config.provider_request_timeout(),
                Duration::from_secs(30),
            )
        })
        .and_then(|transport| {
            transport
                .with_responses_serialized_request_body_limit(MAX_SERIALIZED_REQUEST_WINDOW_BYTES)
        })
        .and_then(|transport| {
            transport.with_response_limits(
                MAX_RESPONSE_BODY_BYTES,
                MAX_SSE_EVENT_BYTES,
                MAX_OUTPUT_TEXT_BYTES,
            )
        })
        .map_err(|_| LiveSetupError::ProviderInitialization)?;
    let identity = ProviderIdentity::new("openai", CODEX_HTTP_V1_PROFILE, 1, LIVE_PROVIDER_BINDING)
        .map_err(|_| LiveSetupError::ProviderInitialization)?;
    let options = CodexHttpV1Options::new(config.model(), None, None, Some(prompt_cache_key))
        .map_err(|_| LiveSetupError::ProviderInitialization)?;

    AsyncOpenAiResponsesProvider::try_new(transport, identity, CodexHttpV1Encoder::new(options))
        .map(|provider| {
            provider.with_response_usage_observer(move |usage| {
                response_capture.capture_openai(usage);
            })
        })
        .map_err(|_| LiveSetupError::ProviderInitialization)
}

pub(crate) async fn run_live_game(config: LiveConfig) -> Result<GameEvidence, LiveRunError> {
    let limits = config.limits();
    let deadline = tokio::time::Instant::now() + limits.whole_game_deadline;
    let run_id = random_run_id().map_err(|_| LiveRunError::RunIdentifier)?;
    let response_capacity = limits
        .ply_limit
        .saturating_mul(usize::from(MAX_MODEL_ATTEMPTS));
    let (response_capture, response_inbox) = provider_response_capture(response_capacity);
    let provider = build_responses_provider(&config, &run_id, response_capture)
        .map_err(|_| LiveRunError::ProviderInitialization)?;
    let engine = UciProcessConfig::stockfish(config.stockfish_program().to_path_buf())
        .map_err(|_| LiveRunError::InvalidStockfishProgram)?;
    let trace_path = config.trace_path_for_run(&run_id);
    let trace = create_trace(&trace_path).map_err(classify_trace_setup)?;
    let mut observer = LiveObserver::new(
        std::io::stderr(),
        trace,
        SystemClock::start(),
        run_id,
        response_inbox,
    );
    let effective_provider = config.effective_provider();
    let result = run_provider_game_with_observer_until(
        provider,
        engine,
        limits,
        deadline,
        effective_provider,
        &mut observer,
    )
    .await;

    finish_or_discard_incomplete(result, observer).map_err(classify_finished_run)
}

fn classify_trace_setup(error: anyhow::Error) -> LiveRunError {
    error
        .downcast_ref::<TraceSetupFailure>()
        .map(|failure| LiveRunError::TraceSetup {
            artifact_disposition: Some(failure.artifact_disposition()),
        })
        .unwrap_or(LiveRunError::TraceSetup {
            artifact_disposition: None,
        })
}

fn classify_finished_run(error: anyhow::Error) -> LiveRunError {
    error
        .downcast_ref::<TraceSetupFailure>()
        .map(|failure| LiveRunError::TraceSetup {
            artifact_disposition: Some(failure.artifact_disposition()),
        })
        .unwrap_or(LiveRunError::RunnerSetup)
}

impl LiveRunError {
    pub(crate) fn reason_code(self) -> &'static str {
        match self {
            Self::ProviderInitialization => "provider_initialization_failed",
            Self::InvalidStockfishProgram => "stockfish_configuration_failed",
            Self::RunIdentifier => "run_identifier_failed",
            Self::TraceSetup { .. } => "trace_setup_failed",
            Self::RunnerSetup => "live_runner_setup_failed",
        }
    }

    pub(crate) fn artifact_disposition(self) -> Option<TraceArtifactDisposition> {
        match self {
            Self::TraceSetup {
                artifact_disposition,
            } => artifact_disposition,
            _ => None,
        }
    }
}

impl LiveEvidenceError {
    pub(crate) fn reason_code(self) -> &'static str {
        match self {
            Self::InfrastructureAbort { .. } => "live_infrastructure_abort",
            Self::NonTerminalOutcome => "live_non_terminal_outcome",
            Self::MissingCompletedReaction => "live_missing_completed_reaction",
            Self::MissingStockfishMove => "live_missing_stockfish_move",
            Self::IncompleteCleanup => "live_incomplete_cleanup",
            Self::InconsistentGameEvidence => "live_inconsistent_game_evidence",
        }
    }
}

pub(crate) fn validate_live_evidence(evidence: &GameEvidence) -> Result<(), LiveEvidenceError> {
    if let GameOutcome::InfrastructureAbort { stage, reason_code } = &evidence.outcome {
        return Err(LiveEvidenceError::InfrastructureAbort {
            stage: *stage,
            reason_code: reason_code.clone(),
        });
    }
    if !matches!(
        evidence.outcome,
        GameOutcome::Checkmate { .. }
            | GameOutcome::Stalemate
            | GameOutcome::Resignation { .. }
            | GameOutcome::DrawAccepted
            | GameOutcome::DrawClaimed { .. }
            | GameOutcome::AutomaticDraw { .. }
            | GameOutcome::ModelForfeit { .. }
    ) {
        return Err(LiveEvidenceError::NonTerminalOutcome);
    }
    let has_completed_reaction = evidence.provider_execute_count > 0
        && evidence.component_turn_executions > 0
        && evidence.attempts.iter().any(|attempt| {
            matches!(
                attempt.result,
                AttemptResult::ActionAccepted(_) | AttemptResult::Correctable(_)
            )
        });
    if !has_completed_reaction {
        return Err(LiveEvidenceError::MissingCompletedReaction);
    }
    if evidence.accepted_moves.len() >= 2 && evidence.validated_black_moves.is_empty() {
        return Err(LiveEvidenceError::MissingStockfishMove);
    }
    if !evidence.component_host_id_retained
        || !evidence.engine_reads_bounded
        || !evidence.child_shutdown_observed
        || !evidence.application_shutdown_observed
        || evidence.trace_artifact_disposition.is_some()
        || !evidence.status_output_complete
    {
        return Err(LiveEvidenceError::IncompleteCleanup);
    }
    if !live_game_state_is_consistent(evidence) {
        return Err(LiveEvidenceError::InconsistentGameEvidence);
    }
    Ok(())
}

fn live_game_state_is_consistent(evidence: &GameEvidence) -> bool {
    let Some((replayed, white_boards)) = replay_game(&evidence.accepted_moves) else {
        return false;
    };
    let black_moves_match = evidence
        .accepted_moves
        .iter()
        .skip(1)
        .step_by(2)
        .eq(evidence.validated_black_moves.iter());
    let counts_match = evidence.provider_execute_count == evidence.attempts.len()
        && evidence.component_turn_executions == evidence.attempts.len()
        && evidence.provider_responses.len() == evidence.attempts.len();
    replayed == evidence.final_board
        && black_moves_match
        && counts_match
        && game_limits_match_terminal(evidence)
        && attempt_chronology_matches(evidence, &white_boards, replayed)
        && uci_chronology_matches(evidence)
        && terminal_matches_board(evidence.outcome.clone(), replayed, &evidence.accepted_moves)
}

fn game_limits_match_terminal(evidence: &GameEvidence) -> bool {
    let limits = evidence.limits;
    !limits.reaction_timeout.is_zero()
        && !limits.engine_timeout.is_zero()
        && !limits.whole_game_deadline.is_zero()
        && limits.ply_limit > 0
        && limits.engine_nodes > 0
        && evidence.accepted_moves.len() <= limits.ply_limit
        && (!matches!(evidence.outcome, GameOutcome::ModelForfeit { .. })
            || evidence.accepted_moves.len() < limits.ply_limit)
}

fn replay_game(accepted_moves: &[chess::ChessMove]) -> Option<(Board, Vec<Board>)> {
    accepted_moves.iter().enumerate().try_fold(
        (Board::default(), Vec::new()),
        |(board, white_boards), (ply, candidate)| {
            if board.status() != BoardStatus::Ongoing
                || !MoveGen::new_legal(&board).any(|legal| legal == *candidate)
            {
                return None;
            }
            let white_boards = if ply.is_multiple_of(2) {
                white_boards
                    .into_iter()
                    .chain(std::iter::once(board))
                    .collect()
            } else {
                white_boards
            };
            Some((board.make_move_new(*candidate), white_boards))
        },
    )
}

fn attempt_chronology_matches(
    evidence: &GameEvidence,
    white_boards: &[Board],
    replayed: Board,
) -> bool {
    let committed_white_plies = white_boards.len();
    let has_forfeit_group = matches!(evidence.outcome, GameOutcome::ModelForfeit { .. });
    let has_terminal_action_group = matches!(
        evidence.outcome,
        GameOutcome::Resignation { .. }
            | GameOutcome::DrawAccepted
            | GameOutcome::DrawClaimed { .. }
    );
    if has_forfeit_group
        && (!evidence.accepted_moves.len().is_multiple_of(2)
            || replayed.side_to_move() != Color::White)
    {
        return false;
    }
    let group_count = committed_white_plies
        + usize::from(has_forfeit_group)
        + usize::from(has_terminal_action_group);

    (0..group_count)
        .try_fold(evidence.attempts.as_slice(), |remaining, group_index| {
            let turn_id = ModelTurnId::for_white_ply(group_index * 2);
            let group_len = remaining
                .iter()
                .take_while(|attempt| attempt.turn_id == turn_id)
                .count();
            let (group, remaining) = remaining.split_at(group_len);
            let board = white_boards.get(group_index).copied().unwrap_or(replayed);
            attempt_group_matches(group, group_index, board, committed_white_plies, evidence)
                .then_some(remaining)
        })
        .is_some_and(<[_]>::is_empty)
}

fn attempt_group_matches(
    group: &[AttemptEvidence],
    group_index: usize,
    board: Board,
    committed_white_plies: usize,
    evidence: &GameEvidence,
) -> bool {
    if group.is_empty() || group.len() > usize::from(MAX_MODEL_ATTEMPTS) {
        return false;
    }
    let lineage_matches = group.iter().enumerate().all(|(attempt_index, attempt)| {
        let corrective_reason = match attempt_index.checked_sub(1) {
            None => None,
            Some(previous) => match group[previous].result {
                AttemptResult::Correctable(reason) => Some(reason),
                AttemptResult::ActionAccepted(_) | AttemptResult::InfrastructureAbort { .. } => {
                    return false;
                }
            },
        };
        let start = AttemptStart::StateWrite;
        usize::from(attempt.attempt_index) == attempt_index
            && attempt.corrective_reason == corrective_reason
            && attempt.board == board
            && attempt.start == start
            && !matches!(attempt.result, AttemptResult::InfrastructureAbort { .. })
    });
    lineage_matches
        && if group_index < committed_white_plies {
            group[..group.len() - 1]
                .iter()
                .all(|attempt| matches!(attempt.result, AttemptResult::Correctable(_)))
                && matches!(
                    (
                        group.last().map(|attempt| attempt.result.clone()),
                        evidence.accepted_moves.get(group_index * 2)
                    ),
                    (Some(AttemptResult::ActionAccepted(action)), Some(committed))
                        if action.move_candidate() == Some(*committed)
                )
        } else if matches!(
            evidence.outcome,
            GameOutcome::Resignation { .. }
                | GameOutcome::DrawAccepted
                | GameOutcome::DrawClaimed { .. }
        ) {
            terminal_action_group_matches(group, evidence.outcome.clone())
        } else {
            forfeit_group_matches(group, evidence.outcome.clone())
        }
}

fn terminal_action_group_matches(group: &[AttemptEvidence], outcome: GameOutcome) -> bool {
    group[..group.len() - 1]
        .iter()
        .all(|attempt| matches!(attempt.result, AttemptResult::Correctable(_)))
        && matches!(
            (group.last().map(|attempt| attempt.result.clone()), outcome),
            (
                Some(AttemptResult::ActionAccepted(ChessAction::Resign)),
                GameOutcome::Resignation { .. }
            ) | (
                Some(AttemptResult::ActionAccepted(ChessAction::AcceptDraw)),
                GameOutcome::DrawAccepted
            ) | (
                Some(AttemptResult::ActionAccepted(ChessAction::ClaimDraw)),
                GameOutcome::DrawClaimed { .. }
            )
        )
}

fn forfeit_group_matches(group: &[AttemptEvidence], outcome: GameOutcome) -> bool {
    let GameOutcome::ModelForfeit {
        final_reason,
        attempts,
    } = outcome
    else {
        return false;
    };
    usize::from(attempts) == usize::from(MAX_MODEL_ATTEMPTS)
        && group.len() == usize::from(attempts)
        && group
            .iter()
            .all(|attempt| matches!(attempt.result, AttemptResult::Correctable(_)))
        && matches!(
            group.last().map(|attempt| attempt.result.clone()),
            Some(AttemptResult::Correctable(reason)) if reason == final_reason
        )
}

fn uci_chronology_matches(evidence: &GameEvidence) -> bool {
    let expected = ["uci".to_owned(), "isready".to_owned()]
        .into_iter()
        .chain(
            (1..evidence.accepted_moves.len())
                .step_by(2)
                .flat_map(|black_ply| {
                    let history = evidence.accepted_moves[..black_ply]
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" ");
                    [
                        format!("position startpos moves {history}"),
                        format!("go nodes {}", evidence.limits.engine_nodes),
                    ]
                }),
        )
        .chain(std::iter::once("quit".to_owned()))
        .collect::<Vec<_>>();
    evidence.uci_commands == expected
}

fn terminal_matches_board(
    outcome: GameOutcome,
    board: Board,
    accepted_moves: &[chess::ChessMove],
) -> bool {
    match outcome {
        GameOutcome::Checkmate { winner } => {
            board.status() == BoardStatus::Checkmate
                && winner
                    == match board.side_to_move() {
                        Color::White => Color::Black,
                        Color::Black => Color::White,
                    }
        }
        GameOutcome::Stalemate => board.status() == BoardStatus::Stalemate,
        GameOutcome::Resignation { resigned, winner } => {
            board.status() == BoardStatus::Ongoing
                && resigned == board.side_to_move()
                && winner == !resigned
        }
        GameOutcome::DrawAccepted => board.status() == BoardStatus::Ongoing,
        GameOutcome::DrawClaimed { .. } => {
            board.status() == BoardStatus::Ongoing
                && DrawState::from_history(accepted_moves).claimable()
        }
        GameOutcome::AutomaticDraw { reason } => {
            let draw_state = DrawState::from_history(accepted_moves);
            board.status() == BoardStatus::Ongoing
                && match reason {
                    AutomaticDrawReason::FivefoldRepetition => {
                        draw_state.current_position_repetitions() >= 5
                    }
                    AutomaticDrawReason::SeventyFiveMoveRule => draw_state.halfmove_clock() >= 150,
                    AutomaticDrawReason::DeadPosition => is_dead_position(&board),
                }
        }
        GameOutcome::ModelForfeit { .. } => board.status() == BoardStatus::Ongoing,
        GameOutcome::InfrastructureAbort { .. } | GameOutcome::PlyLimitReached => false,
    }
}

impl LiveConfig {
    pub(crate) fn from_environment(
        mut read: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, LiveConfigError> {
        let model = optional_model(read("AGENTVIEW_MODEL"))?;
        let api_key = required_api_key(read("OPENAI_API_KEY"))?;
        let api_base = optional_unicode(read("OPENAI_BASE_URL"), DEFAULT_API_BASE)?;
        let stockfish_program = read("AGENTVIEW_STOCKFISH_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STOCKFISH_PROGRAM));
        validate_model(&model)?;
        validate_api_key(&api_key)?;
        let parsed_api_base = Url::parse(&api_base).map_err(|_| LiveConfigError::InvalidApiBase)?;
        AsyncOpenAiTransportConfig::new(&api_base, "validation-placeholder")
            .map_err(|_| LiveConfigError::InvalidApiBase)?;
        let effective_provider = effective_provider_config(&model, &parsed_api_base)?;
        if !stockfish_program.is_absolute() || stockfish_program.as_os_str().is_empty() {
            return Err(LiveConfigError::InvalidStockfishProgram);
        }

        Ok(Self {
            model,
            api_key,
            api_base,
            effective_provider,
            stockfish_program,
            trace_directory: PathBuf::from(LIVE_TRACE_DIRECTORY),
            provider_request_timeout: Duration::from_secs(110),
            limits: GameLimits {
                reaction_timeout: Duration::from_secs(120),
                engine_timeout: Duration::from_secs(30),
                whole_game_deadline: Duration::from_secs(1_800),
                ply_limit: 160,
                engine_nodes: 20_000,
            },
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    pub(crate) fn api_base(&self) -> &str {
        &self.api_base
    }

    pub(crate) fn effective_provider(&self) -> EffectiveProviderConfig {
        self.effective_provider.clone()
    }

    pub(crate) fn stockfish_program(&self) -> &Path {
        &self.stockfish_program
    }

    pub(crate) fn trace_path_for_run(&self, run_id: &str) -> PathBuf {
        self.trace_directory
            .join(format!("agentview-chess-live-{run_id}.jsonl"))
    }

    pub(crate) fn limits(&self) -> GameLimits {
        self.limits
    }

    pub(crate) fn provider_request_timeout(&self) -> Duration {
        self.provider_request_timeout
    }
}

fn effective_provider_config(
    model: &str,
    api_base: &Url,
) -> Result<EffectiveProviderConfig, LiveConfigError> {
    let loopback = match api_base.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    let endpoint_class = if api_base.scheme() == "http" && loopback {
        ProviderEndpointClass::LoopbackHttp
    } else if api_base.scheme() == "https"
        && api_base
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
        && api_base.port_or_known_default() == Some(443)
    {
        ProviderEndpointClass::OpenAiHttps
    } else if api_base.scheme() == "https" {
        ProviderEndpointClass::CustomHttps
    } else {
        return Err(LiveConfigError::InvalidApiBase);
    };
    Ok(EffectiveProviderConfig::new(
        model.to_owned(),
        endpoint_class,
        api_base.origin().ascii_serialization(),
    ))
}

fn validate_model(model: &str) -> Result<(), LiveConfigError> {
    if model.is_empty() || model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control) {
        return Err(LiveConfigError::InvalidModel);
    }
    Ok(())
}

fn validate_api_key(api_key: &str) -> Result<(), LiveConfigError> {
    if api_key.is_empty()
        || api_key.len() > MAX_API_KEY_BYTES
        || api_key
            .chars()
            .any(|character| !character.is_ascii() || character.is_ascii_control())
    {
        return Err(LiveConfigError::InvalidApiKey);
    }
    Ok(())
}

fn required_api_key(value: Option<OsString>) -> Result<String, LiveConfigError> {
    value
        .ok_or(LiveConfigError::MissingApiKey)?
        .into_string()
        .map_err(|_| LiveConfigError::InvalidApiKey)
}

fn optional_model(value: Option<OsString>) -> Result<String, LiveConfigError> {
    value
        .map(OsString::into_string)
        .transpose()
        .map_err(|_| LiveConfigError::InvalidModel)
        .map(|value| value.unwrap_or_else(|| DEFAULT_MODEL.to_owned()))
}

fn optional_unicode(value: Option<OsString>, fallback: &str) -> Result<String, LiveConfigError> {
    value
        .map(OsString::into_string)
        .transpose()
        .map_err(|_| LiveConfigError::InvalidApiBase)
        .map(|value| value.unwrap_or_else(|| fallback.to_owned()))
}
