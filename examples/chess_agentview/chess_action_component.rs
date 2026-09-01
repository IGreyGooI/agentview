use std::str::FromStr;

use agentview::component::prelude::*;
use anyhow::Context as _;
use chess::ChessMove;
use tokio::sync::oneshot;

use super::{
    application_state::ModelAttemptKey,
    chess_action::{ChessAction, ChessActionKind, InvalidActionReason},
    uci::{parse_strict_uci_move, StrictUciMoveError},
};

const ACTION_CONTRACT_VERSION: &str = "v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StrictUciMove(ChessMove);

impl FromStr for StrictUciMove {
    type Err = StrictUciMoveError;

    fn from_str(candidate: &str) -> Result<Self, Self::Err> {
        parse_strict_uci_move(candidate).map(Self)
    }
}

async fn send_model_action(
    workflow: Coroutine<ChessAttemptInput>,
    attempt: ModelAttemptKey,
    result: Result<ChessAction, InvalidActionReason>,
) -> anyhow::Result<()> {
    let (handled, completion) = oneshot::channel();
    workflow
        .send(ChessAttemptInput {
            attempt,
            result,
            handled,
        })
        .await?;
    completion
        .await
        .context("Chess workflow stopped before handling the model action")?;
    Ok(())
}

pub(crate) struct ChessAttemptInput {
    pub(crate) attempt: ModelAttemptKey,
    pub(crate) result: Result<ChessAction, InvalidActionReason>,
    pub(crate) handled: oneshot::Sender<()>,
}

fn invalid_action(kind: ChessActionKind, diagnostic: XmlContractDiagnostic) -> InvalidActionReason {
    match diagnostic {
        XmlContractDiagnostic::InvalidAttributeValue { .. }
            if matches!(
                kind,
                ChessActionKind::ChooseMove | ChessActionKind::MoveAndOfferDraw
            ) =>
        {
            InvalidActionReason::InvalidUci(kind)
        }
        _ => InvalidActionReason::InvalidXml,
    }
}

#[component]
pub(crate) fn chess_action_component(
    attempt: ModelAttemptKey,
    workflow: Coroutine<ChessAttemptInput>,
) -> Component {
    let choose_move_decoded = workflow.clone();
    let choose_move_invalid = workflow.clone();
    let move_and_offer_draw_decoded = workflow.clone();
    let move_and_offer_draw_invalid = workflow.clone();
    let resign_decoded = workflow.clone();
    let resign_invalid = workflow.clone();
    let accept_draw_decoded = workflow.clone();
    let accept_draw_invalid = workflow.clone();
    let claim_draw_decoded = workflow.clone();
    let claim_draw_invalid = workflow;

    view! {
        {
            XmlStreamingToolCall::contract("chess.choose_move", ACTION_CONTRACT_VERSION)
                .empty_element("choose_move")
                .required_attribute::<StrictUciMove>("uci")
                .on_decoded(move |candidate| {
                    send_model_action(
                        choose_move_decoded.clone(),
                        attempt,
                        Ok(ChessAction::ChooseMove(candidate.0)),
                    )
                })
                .on_invalid(move |diagnostic| {
                    send_model_action(
                        choose_move_invalid.clone(),
                        attempt,
                        Err(invalid_action(ChessActionKind::ChooseMove, diagnostic)),
                    )
                })
        }
        {
            XmlStreamingToolCall::contract(
                "chess.move_and_offer_draw",
                ACTION_CONTRACT_VERSION,
            )
            .empty_element("move_and_offer_draw")
            .required_attribute::<StrictUciMove>("uci")
            .on_decoded(move |candidate| {
                send_model_action(
                    move_and_offer_draw_decoded.clone(),
                    attempt,
                    Ok(ChessAction::MoveAndOfferDraw(candidate.0)),
                )
            })
            .on_invalid(move |diagnostic| {
                send_model_action(
                    move_and_offer_draw_invalid.clone(),
                    attempt,
                    Err(invalid_action(
                        ChessActionKind::MoveAndOfferDraw,
                        diagnostic,
                    )),
                )
            })
        }
        {
            XmlStreamingToolCall::contract("chess.resign", ACTION_CONTRACT_VERSION)
                .empty_element("resign")
                .on_decoded(move || {
                    send_model_action(resign_decoded.clone(), attempt, Ok(ChessAction::Resign))
                })
                .on_invalid(move |diagnostic| {
                    send_model_action(
                        resign_invalid.clone(),
                        attempt,
                        Err(invalid_action(ChessActionKind::Resign, diagnostic)),
                    )
                })
        }
        {
            XmlStreamingToolCall::contract("chess.accept_draw", ACTION_CONTRACT_VERSION)
                .empty_element("accept_draw")
                .on_decoded(move || {
                    send_model_action(
                        accept_draw_decoded.clone(),
                        attempt,
                        Ok(ChessAction::AcceptDraw),
                    )
                })
                .on_invalid(move |diagnostic| {
                    send_model_action(
                        accept_draw_invalid.clone(),
                        attempt,
                        Err(invalid_action(ChessActionKind::AcceptDraw, diagnostic)),
                    )
                })
        }
        {
            XmlStreamingToolCall::contract("chess.claim_draw", ACTION_CONTRACT_VERSION)
                .empty_element("claim_draw")
                .on_decoded(move || {
                    send_model_action(
                        claim_draw_decoded.clone(),
                        attempt,
                        Ok(ChessAction::ClaimDraw),
                    )
                })
                .on_invalid(move |diagnostic| {
                    send_model_action(
                        claim_draw_invalid.clone(),
                        attempt,
                        Err(invalid_action(ChessActionKind::ClaimDraw, diagnostic)),
                    )
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_move_decoder_rejects_noncanonical_uci() {
        assert!("e2e4".parse::<StrictUciMove>().is_ok());
        assert!("E2E4".parse::<StrictUciMove>().is_err());
    }

    #[test]
    fn move_value_diagnostics_are_business_level_invalid_uci() {
        let diagnostic = XmlContractDiagnostic::InvalidAttributeValue {
            contract: "chess.choose_move",
            attribute: "uci",
            value: "E2E4".to_owned(),
            expected: "StrictUciMove",
            detail: "invalid UCI move".to_owned(),
        };

        assert_eq!(
            invalid_action(ChessActionKind::ChooseMove, diagnostic),
            InvalidActionReason::InvalidUci(ChessActionKind::ChooseMove)
        );
    }

    #[test]
    fn structural_diagnostics_are_business_level_invalid_xml() {
        let diagnostic = XmlContractDiagnostic::MalformedElement {
            contract: "chess.resign",
            detail: "target element must use empty-element syntax".to_owned(),
        };

        assert_eq!(
            invalid_action(ChessActionKind::Resign, diagnostic),
            InvalidActionReason::InvalidXml
        );
    }
}
