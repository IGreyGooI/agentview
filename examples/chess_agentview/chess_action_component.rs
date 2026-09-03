use std::{
    convert::Infallible,
    future::ready,
    str::FromStr,
    sync::{Arc, Mutex},
};

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
        XmlContractDiagnostic::InvalidAttributeValue { value, .. }
            if matches!(kind, ChessActionKind::ChooseMove) =>
        {
            InvalidActionReason::invalid_uci(kind, &value)
        }
        _ => InvalidActionReason::InvalidXml,
    }
}

#[component]
pub(crate) fn chess_action_component(
    attempt: ModelAttemptKey,
    workflow: Coroutine<ChessAttemptInput>,
) -> Component {
    let collected: Arc<Mutex<Vec<Result<ChessAction, InvalidActionReason>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let completion_collected = Arc::clone(&collected);
    use_reaction_completion(move || async move {
        let result = {
            let mut collected = completion_collected.lock().unwrap();
            match collected.len() {
                0 => Err(InvalidActionReason::MissingAction),
                1 => collected.pop().expect("one collected Chess action result"),
                _ => Err(InvalidActionReason::MultipleActions),
            }
        };
        send_model_action(workflow, attempt, result).await
    });

    let choose_move_decoded = Arc::clone(&collected);
    let choose_move_invalid = Arc::clone(&collected);
    let resign_decoded = Arc::clone(&collected);
    let resign_invalid = collected;

    view! {
        {
            XmlStreamingToolCall::contract("chess.choose_move", ACTION_CONTRACT_VERSION)
                .empty_element("choose_move")
                .required_attribute::<StrictUciMove>("uci")
                .on_decoded(move |candidate| {
                    choose_move_decoded
                        .lock()
                        .unwrap()
                        .push(Ok(ChessAction::ChooseMove(candidate.0)));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    choose_move_invalid.lock().unwrap().push(Err(invalid_action(
                        ChessActionKind::ChooseMove,
                        diagnostic,
                    )));
                    ready(Ok::<(), Infallible>(()))
                })
        }
        {
            XmlStreamingToolCall::contract("chess.resign", ACTION_CONTRACT_VERSION)
                .empty_element("resign")
                .on_decoded(move || {
                    resign_decoded
                        .lock()
                        .unwrap()
                        .push(Ok(ChessAction::Resign));
                    ready(Ok::<(), Infallible>(()))
                })
                .on_invalid(move |diagnostic| {
                    resign_invalid
                        .lock()
                        .unwrap()
                        .push(Err(invalid_action(ChessActionKind::Resign, diagnostic)));
                    ready(Ok::<(), Infallible>(()))
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
            InvalidActionReason::invalid_uci(ChessActionKind::ChooseMove, "E2E4")
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
