use std::sync::Arc;

use agentview::component::prelude::*;

use super::{
    chess_actions::{ChessAction, ChessActionKind, InvalidActionReason},
    chess_agent::ChessSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeedbackDecision {
    None,
    Accepted,
    Rejected,
}

impl FeedbackDecision {
    fn code(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunnerStatus {
    AwaitingAgentAction,
    AwaitingRetry,
}

impl RunnerStatus {
    fn code(self) -> &'static str {
        match self {
            Self::AwaitingAgentAction => "awaiting_agent_action",
            Self::AwaitingRetry => "awaiting_retry",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChessFeedback {
    previous_action: Option<ChessAction>,
    previous_action_kind: Option<ChessActionKind>,
    previous_submitted_uci: Option<String>,
    decision: FeedbackDecision,
    reason: String,
    retry_permitted: bool,
    runner_status: RunnerStatus,
    final_result: Option<String>,
}

impl ChessFeedback {
    pub(crate) fn initial() -> Self {
        Self {
            previous_action: None,
            previous_action_kind: None,
            previous_submitted_uci: None,
            decision: FeedbackDecision::None,
            reason: "No previous agent action exists for the opening reaction.".to_owned(),
            retry_permitted: false,
            runner_status: RunnerStatus::AwaitingAgentAction,
            final_result: None,
        }
    }

    pub(crate) fn accepted(action: ChessAction, reason: impl Into<String>) -> Self {
        Self {
            previous_action: Some(action),
            previous_action_kind: Some(action.kind()),
            previous_submitted_uci: None,
            decision: FeedbackDecision::Accepted,
            reason: reason.into(),
            retry_permitted: false,
            runner_status: RunnerStatus::AwaitingAgentAction,
            final_result: None,
        }
    }

    pub(crate) fn rejected(reason: InvalidActionReason) -> Self {
        let previous_submitted_uci = reason.rejected_uci().map(|(submitted, truncated)| {
            format!(
                "{submitted}{}",
                if truncated { "; truncated=true" } else { "" }
            )
        });
        Self {
            previous_action: reason.rejected_action(),
            previous_action_kind: reason.action_kind(),
            previous_submitted_uci,
            decision: FeedbackDecision::Rejected,
            reason: reason.description().to_owned(),
            retry_permitted: true,
            runner_status: RunnerStatus::AwaitingRetry,
            final_result: None,
        }
    }
}

#[component]
pub(crate) fn chess_feedback(snapshot: Arc<ChessSnapshot>) -> Component {
    let feedback = snapshot.feedback();
    let previous_action = feedback
        .previous_action_kind
        .map(ChessActionKind::code)
        .unwrap_or("none");
    let previous_move = feedback
        .previous_submitted_uci
        .clone()
        .or_else(|| {
            feedback
                .previous_action
                .and_then(ChessAction::move_candidate)
                .map(|candidate| candidate.to_string())
        })
        .unwrap_or_else(|| "none".to_owned());
    let decision = feedback.decision.code();
    let reason = &feedback.reason;
    let retry_permitted = feedback.retry_permitted;
    let runner_status = feedback.runner_status.code();
    let final_result = feedback
        .final_result
        .as_deref()
        .unwrap_or("none; match is still in progress");

    view! {
        #[developer]
        chess_feedback {
            authority {
                "This is the authoritative referee report for the previous agent action."
            }
            previous_action { kind: "{previous_action}", uci: "{previous_move}", }
            decision { "{decision}" }
            reason { "{reason}" }
            retry_permitted { "{retry_permitted}" }
            match_runner_status { "{runner_status}" }
            final_result { "{final_result}" }
        }
    }
}
