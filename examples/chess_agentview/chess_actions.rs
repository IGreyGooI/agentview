use std::{str::from_utf8, sync::Arc};

use agentview::{component::prelude::*, llm_call::TextTurnEvent};
use chess::{BoardStatus, ChessMove, MoveGen};
use quick_xml::{events::Event, Reader};

use super::{
    chess_agent::{ChessAgentState, ChessSnapshot, MatchPhase},
    uci::parse_strict_uci_move,
};

const MAX_ACTION_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessAction {
    ChooseMove(ChessMove),
    MoveAndOfferDraw(ChessMove),
    Resign,
    AcceptDraw,
    ClaimDraw,
}

impl ChessAction {
    pub(crate) fn kind(self) -> ChessActionKind {
        match self {
            Self::ChooseMove(_) => ChessActionKind::ChooseMove,
            Self::MoveAndOfferDraw(_) => ChessActionKind::MoveAndOfferDraw,
            Self::Resign => ChessActionKind::Resign,
            Self::AcceptDraw => ChessActionKind::AcceptDraw,
            Self::ClaimDraw => ChessActionKind::ClaimDraw,
        }
    }

    pub(crate) fn move_candidate(self) -> Option<ChessMove> {
        match self {
            Self::ChooseMove(candidate) | Self::MoveAndOfferDraw(candidate) => Some(candidate),
            Self::Resign | Self::AcceptDraw | Self::ClaimDraw => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessActionKind {
    ChooseMove,
    MoveAndOfferDraw,
    Resign,
    AcceptDraw,
    ClaimDraw,
}

impl ChessActionKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ChooseMove => "choose_move",
            Self::MoveAndOfferDraw => "move_and_offer_draw",
            Self::Resign => "resign",
            Self::AcceptDraw => "accept_draw",
            Self::ClaimDraw => "claim_draw",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InvalidActionReason {
    InvalidXml,
    MissingAction,
    MultipleActions,
    InvalidUci(ChessActionKind),
    IllegalMove(ChessAction),
    ActionUnavailable {
        action: ChessActionKind,
        reason: ActionUnavailableReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionUnavailableReason {
    MatchFinished,
    PositionTerminal,
    NotAgentTurn,
    NoLegalMoves,
    NoPendingOpponentDrawOffer,
    PositionNotClaimable,
}

impl ActionUnavailableReason {
    fn description(self) -> &'static str {
        match self {
            Self::MatchFinished => "The match is already finished.",
            Self::PositionTerminal => {
                "The referee has already detected checkmate, stalemate, or an automatic draw."
            }
            Self::NotAgentTurn => "The authoritative side to move is not the agent's side.",
            Self::NoLegalMoves => "The authoritative legal move set is empty.",
            Self::NoPendingOpponentDrawOffer => {
                "There is no pending draw offer from the opponent."
            }
            Self::PositionNotClaimable => {
                "The current position is not claimable by threefold repetition or the fifty-move rule."
            }
        }
    }
}

impl InvalidActionReason {
    pub(crate) fn action_kind(self) -> Option<ChessActionKind> {
        match self {
            Self::InvalidUci(action) | Self::ActionUnavailable { action, .. } => Some(action),
            Self::IllegalMove(action) => Some(action.kind()),
            Self::InvalidXml | Self::MissingAction | Self::MultipleActions => None,
        }
    }

    pub(crate) fn rejected_action(self) -> Option<ChessAction> {
        match self {
            Self::IllegalMove(action) => Some(action),
            Self::InvalidXml
            | Self::MissingAction
            | Self::MultipleActions
            | Self::InvalidUci(_)
            | Self::ActionUnavailable { .. } => None,
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::InvalidXml => {
                "The response did not match one supported empty XML action element."
            }
            Self::MissingAction => "The response contained no action element.",
            Self::MultipleActions => "The response contained more than one action element.",
            Self::InvalidUci(_) => "The uci attribute was not canonical lowercase UCI.",
            Self::IllegalMove(_) => "The submitted UCI move is not legal in this snapshot.",
            Self::ActionUnavailable { reason, .. } => reason.description(),
        }
    }
}

#[component]
pub(crate) fn chess_actions(
    snapshot: Arc<ChessSnapshot>,
    attempt_state: ChessAgentState,
    state: Signal<ChessAgentState>,
    text: EventInput<TextTurnEvent>,
) -> Component {
    let choose_move = availability(&snapshot, ChessActionKind::ChooseMove);
    let move_and_offer_draw = availability(&snapshot, ChessActionKind::MoveAndOfferDraw);
    let resign = availability(&snapshot, ChessActionKind::Resign);
    let accept_draw = availability(&snapshot, ChessActionKind::AcceptDraw);
    let claim_draw = availability(&snapshot, ChessActionKind::ClaimDraw);
    let choose_move_available = choose_move.available;
    let choose_move_reason = choose_move.reason;
    let move_and_offer_draw_available = move_and_offer_draw.available;
    let move_and_offer_draw_reason = move_and_offer_draw.reason;
    let resign_available = resign.available;
    let resign_reason = resign.reason;
    let accept_draw_available = accept_draw.available;
    let accept_draw_reason = accept_draw.reason;
    let claim_draw_available = claim_draw.available;
    let claim_draw_reason = claim_draw.reason;
    let claim_basis = snapshot.draw_state().claim_basis();
    let event_state = state;
    let event_attempt_state = attempt_state;

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
            move_and_offer_draw {
                available { "{move_and_offer_draw_available}" }
                unavailable_reason { "{move_and_offer_draw_reason}" }
                format { "<move_and_offer_draw uci=\"e2e4\" />" }
                meaning { "Make the legal move and offer a draw to the opponent with that move." }
            }
            resign {
                available { "{resign_available}" }
                unavailable_reason { "{resign_reason}" }
                format { "<resign />" }
            }
            accept_draw {
                available { "{accept_draw_available}" }
                unavailable_reason { "{accept_draw_reason}" }
                format { "<accept_draw />" }
            }
            claim_draw {
                available { "{claim_draw_available}" }
                unavailable_reason { "{claim_draw_reason}" }
                current_claim_basis { "{claim_basis}" }
                format { "<claim_draw />" }
            }
            output_contract {
                "Return exactly one of the five empty XML elements shown above and nothing else: no prose, Markdown, analysis, or additional elements. A move action's uci attribute must be one value from chess_game_state.legal_moves. Promotions append exactly one lowercase q, r, b, or n suffix."
            }
        }

        {
            EventListener::observe("example.chess.action", "v1")
                .listen_to(text)
                .on_event(move |event| {
                    let state = event_state.clone();
                    let attempt_state = event_attempt_state.clone();
                    async move {
                        let TextTurnEvent::TextComplete(output) = event else {
                            return Ok(());
                        };
                        let next = attempt_state.completed(parse_action(&output));
                        state.set(next)
                    }
                })
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
        ChessActionKind::ChooseMove | ChessActionKind::MoveAndOfferDraw
            if MoveGen::new_legal(&board).next().is_none() =>
        {
            Some(unavailable(ActionUnavailableReason::NoLegalMoves))
        }
        ChessActionKind::AcceptDraw
            if snapshot.pending_draw_offer() != Some(!snapshot.agent_side()) =>
        {
            Some(unavailable(
                ActionUnavailableReason::NoPendingOpponentDrawOffer,
            ))
        }
        ChessActionKind::ClaimDraw if !snapshot.draw_state().claimable() => {
            Some(unavailable(ActionUnavailableReason::PositionNotClaimable))
        }
        ChessActionKind::ChooseMove
        | ChessActionKind::MoveAndOfferDraw
        | ChessActionKind::Resign
        | ChessActionKind::AcceptDraw
        | ChessActionKind::ClaimDraw => None,
    }
}

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
        "move_and_offer_draw" => {
            parse_move_attribute(&attributes, ChessActionKind::MoveAndOfferDraw)
                .map(ChessAction::MoveAndOfferDraw)
        }
        "resign" if attributes.is_empty() => Ok(ChessAction::Resign),
        "accept_draw" if attributes.is_empty() => Ok(ChessAction::AcceptDraw),
        "claim_draw" if attributes.is_empty() => Ok(ChessAction::ClaimDraw),
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
    let value =
        from_utf8(attribute.value.as_ref()).map_err(|_| InvalidActionReason::InvalidUci(kind))?;
    parse_strict_uci_move(value).map_err(|_| InvalidActionReason::InvalidUci(kind))
}
