use std::io::Write;

use chess::Color;

use super::{game, live, observability};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EntryFailure {
    InvalidArguments,
    LiveConfiguration { reason: live::LiveConfigError },
    LiveEnvironment,
    LiveRun { reason: live::LiveRunError },
    LivePostTerminalValidation { reason: live::LiveEvidenceError },
    StdoutWrite,
}

pub(crate) fn write_entry_failure(output: &mut impl Write, failure: EntryFailure) -> u8 {
    let mut line = failure.summary().to_string().into_bytes();
    line.push(b'\n');
    if output
        .write_all(&line)
        .and_then(|()| output.flush())
        .is_ok()
    {
        1
    } else {
        2
    }
}

pub(crate) fn write_live_game_summary(
    output: &mut impl Write,
    evidence: &game::GameEvidence,
) -> Result<(), EntryFailure> {
    let mut line = live_game_summary(evidence).to_string().into_bytes();
    line.push(b'\n');
    output
        .write_all(&line)
        .and_then(|()| output.flush())
        .map_err(|_| EntryFailure::StdoutWrite)
}

fn live_game_summary(evidence: &game::GameEvidence) -> serde_json::Value {
    let (terminal, winner) = match evidence.outcome {
        game::GameOutcome::Checkmate { winner } => (
            "checkmate",
            Some(match winner {
                Color::White => "white",
                Color::Black => "black",
            }),
        ),
        game::GameOutcome::Stalemate => ("stalemate", None),
        game::GameOutcome::Resignation { winner, .. } => (
            "resignation",
            Some(match winner {
                Color::White => "white",
                Color::Black => "black",
            }),
        ),
        game::GameOutcome::DrawAccepted => ("draw_accepted", None),
        game::GameOutcome::DrawClaimed { .. } => ("draw_claimed", None),
        game::GameOutcome::AutomaticDraw { .. } => ("automatic_draw", None),
        game::GameOutcome::ModelForfeit { .. } => ("model_forfeit", None),
        game::GameOutcome::InfrastructureAbort { .. } => ("infrastructure_abort", None),
        game::GameOutcome::PlyLimitReached => ("ply_limit_reached", None),
    };
    let accepted_moves = evidence
        .accepted_moves
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let provider_response_summary =
        observability::ProviderResponseSummary::from_responses(&evidence.provider_responses);

    serde_json::json!({
        "accepted_moves": accepted_moves,
        "child_shutdown_observed": evidence.child_shutdown_observed,
        "host_identity_retained": evidence.component_host_id_retained,
        "provider": &evidence.effective_provider,
        "provider_execute_count": evidence.provider_execute_count,
        "provider_responses": {
            "observations": &evidence.provider_responses,
            "summary": provider_response_summary,
        },
        "responses_reaction_count": evidence.component_turn_executions,
        "status_output_complete": evidence.status_output_complete,
        "terminal": terminal,
        "trace_output_complete": evidence.trace_artifact_disposition.is_none(),
        "uci_turn_count": evidence.validated_black_moves.len(),
        "winner": winner,
    })
}

impl EntryFailure {
    fn summary(self) -> serde_json::Value {
        if let Self::LivePostTerminalValidation {
            reason: live::LiveEvidenceError::InfrastructureAbort { stage, reason_code },
        } = self
        {
            return serde_json::json!({
                "reason": live::LiveEvidenceError::InfrastructureAbort { stage, reason_code: reason_code.clone() }.reason_code(),
                "reason_code": observability::ObservedInfrastructureReason::from(reason_code).code(),
                "stage": observability::ObservedInfrastructureStage::from(stage).code(),
            });
        }
        if let Self::LiveRun { reason } = self {
            if let Some(artifact_disposition) = reason.artifact_disposition() {
                return serde_json::json!({
                    "reason": reason.reason_code(),
                    "terminal": "infrastructure_abort",
                    "trace_artifact_disposition": artifact_disposition.code(),
                });
            }
        }
        if matches!(
            self,
            Self::LivePostTerminalValidation { .. } | Self::StdoutWrite
        ) {
            return serde_json::json!({
                "reason": self.reason_code(),
            });
        }
        serde_json::json!({
            "reason": self.reason_code(),
            "terminal": "infrastructure_abort",
        })
    }

    fn reason_code(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::LiveConfiguration { reason } => reason.reason_code(),
            Self::LiveEnvironment => "live_environment_failed",
            Self::LiveRun { reason } => reason.reason_code(),
            Self::LivePostTerminalValidation { reason } => reason.reason_code(),
            Self::StdoutWrite => "stdout_write_failed",
        }
    }
}
