use std::{
    str::from_utf8,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use agentview::component::prelude::*;
use chess::{BoardStatus, ChessMove, MoveGen};
use quick_xml::{events::Event, Reader};

use super::{
    chess_agent::{ChessAgentState, ChessSnapshot, MatchPhase},
    uci::parse_strict_uci_move,
};

pub(crate) use super::chess_action::{
    ActionUnavailableReason, ChessAction, ChessActionKind, InvalidActionReason,
};

const MAX_ACTION_BYTES: usize = 1_024;

fn parse_action(output: &str) -> Result<ChessAction, InvalidActionReason> {
    if output.len() > MAX_ACTION_BYTES {
        return Err(InvalidActionReason::InvalidXml);
    }
    let mut reader = Reader::from_str(output);
    let mut parsed = None;
    loop {
        match reader.read_event() {
            Ok(Event::Empty(element)) => {
                if parsed.is_some() {
                    return Err(InvalidActionReason::MultipleActions);
                }
                parsed = Some(parse_element(&element)?);
            }
            Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::Eof) => return parsed.ok_or(InvalidActionReason::MissingAction),
            Ok(_) | Err(_) => return Err(InvalidActionReason::InvalidXml),
        }
    }
}

fn parse_element(
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<ChessAction, InvalidActionReason> {
    let qualified_name = element.name();
    let name = from_utf8(qualified_name.as_ref()).map_err(|_| InvalidActionReason::InvalidXml)?;
    let attributes = element
        .attributes()
        .with_checks(true)
        .map(|attribute| attribute.map_err(|_| InvalidActionReason::InvalidXml))
        .collect::<Result<Vec<_>, _>>()?;
    match name {
        "choose_move" => parse_move_attribute(&attributes, ChessActionKind::ChooseMove)
            .map(ChessAction::ChooseMove),
        "resign" if attributes.is_empty() => Ok(ChessAction::Resign),
        _ => Err(InvalidActionReason::InvalidXml),
    }
}

fn parse_move_attribute(
    attributes: &[quick_xml::events::attributes::Attribute<'_>],
    kind: ChessActionKind,
) -> Result<ChessMove, InvalidActionReason> {
    let [attribute] = attributes else {
        return Err(InvalidActionReason::InvalidXml);
    };
    if attribute.key.as_ref() != b"uci" {
        return Err(InvalidActionReason::InvalidXml);
    }
    let value = from_utf8(attribute.value.as_ref()).map_err(|_| InvalidActionReason::InvalidXml)?;
    parse_strict_uci_move(value).map_err(|_| InvalidActionReason::invalid_uci(kind, value))
}

#[component]
pub(crate) fn chess_actions(
    snapshot: Arc<ChessSnapshot>,
    attempt_state: ChessAgentState,
    state: Signal<ChessAgentState>,
    component_turn_executions: Arc<AtomicUsize>,
) -> Component {
    let choose_move = availability(&snapshot, ChessActionKind::ChooseMove);
    let resign = availability(&snapshot, ChessActionKind::Resign);
    let choose_move_available = choose_move.available;
    let choose_move_reason = choose_move.reason;
    let resign_available = resign.available;
    let resign_reason = resign.reason;
    let event_state = state;
    let event_attempt_state = attempt_state;
    use_provider_event_handler(ProviderEvent::TEXT, move |event| {
        let state = event_state.clone();
        let attempt_state = event_attempt_state.clone();
        let component_turn_executions = Arc::clone(&component_turn_executions);
        async move {
            let TextTurnEvent::TextComplete(output) = event else {
                return Ok::<(), SignalAccessError>(());
            };
            state.set(attempt_state.completed(parse_action(&output)))?;
            component_turn_executions.fetch_add(1, Ordering::SeqCst);
            Ok::<(), SignalAccessError>(())
        }
    });

    view! {
        #[developer]
        chess_actions {
            authority {
                "Choose exactly one currently available action. This component is the sole authority for action availability and output format."
            }
            choose_move {
                available { "{choose_move_available}" }
                unavailable_reason { "{choose_move_reason}" }
                format { "<choose_move uci=\"e2e4\" />" }
            }
            resign {
                available { "{resign_available}" }
                unavailable_reason { "{resign_reason}" }
                format { "<resign />" }
            }
            output_contract {
                "Return exactly one of the two empty XML elements shown above and nothing else: no prose, Markdown, analysis, or additional elements. A move action's uci attribute must be one value from chess_game_state.legal_moves. Promotions append exactly one lowercase q, r, b, or n suffix."
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Availability {
    available: bool,
    reason: &'static str,
}

fn availability(snapshot: &ChessSnapshot, kind: ChessActionKind) -> Availability {
    match unavailable_reason(snapshot, kind) {
        Some(reason) => Availability {
            available: false,
            reason: reason.description(),
        },
        None => Availability {
            available: true,
            reason: "none",
        },
    }
}

pub(crate) fn unavailable_reason(
    snapshot: &ChessSnapshot,
    kind: ChessActionKind,
) -> Option<InvalidActionReason> {
    let board = snapshot.board();
    let unavailable = |reason| InvalidActionReason::ActionUnavailable {
        action: kind,
        reason,
    };
    if snapshot.match_phase() == MatchPhase::Finished {
        return Some(unavailable(ActionUnavailableReason::MatchFinished));
    }
    if board.status() != BoardStatus::Ongoing || snapshot.draw_state().automatic_draw() {
        return Some(unavailable(ActionUnavailableReason::PositionTerminal));
    }
    if board.side_to_move() != snapshot.agent_side() {
        return Some(unavailable(ActionUnavailableReason::NotAgentTurn));
    }
    match kind {
        ChessActionKind::ChooseMove if MoveGen::new_legal(&board).next().is_none() => {
            Some(unavailable(ActionUnavailableReason::NoLegalMoves))
        }
        ChessActionKind::ChooseMove | ChessActionKind::Resign => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_strict_action() {
        assert_eq!(
            parse_action("<choose_move uci=\"e2e4\" />"),
            Ok(ChessAction::ChooseMove("e2e4".parse().unwrap()))
        );
    }

    #[test]
    fn retains_the_invalid_uci_value_for_retry_feedback() {
        assert_eq!(
            parse_action("<choose_move uci=\"E2E4\" />"),
            Err(InvalidActionReason::invalid_uci(
                ChessActionKind::ChooseMove,
                "E2E4"
            ))
        );
    }

    #[test]
    fn rejects_prose_and_multiple_actions() {
        assert_eq!(
            parse_action("I choose <choose_move uci=\"e2e4\" />"),
            Err(InvalidActionReason::InvalidXml)
        );
        assert_eq!(
            parse_action("<resign /><resign />"),
            Err(InvalidActionReason::MultipleActions)
        );
    }
}
