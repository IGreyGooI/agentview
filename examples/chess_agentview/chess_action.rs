use chess::ChessMove;

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
    #[allow(
        dead_code,
        reason = "constructed by the separate whole-output acceptance parser"
    )]
    MissingAction,
    #[allow(
        dead_code,
        reason = "constructed by the separate whole-output acceptance parser"
    )]
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
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidXml => "invalid_xml",
            Self::MissingAction => "missing_action",
            Self::MultipleActions => "multiple_actions",
            Self::InvalidUci(_) => "invalid_uci",
            Self::IllegalMove(_) => "illegal_move",
            Self::ActionUnavailable { action, .. } => match action {
                ChessActionKind::ChooseMove => "choose_move_unavailable",
                ChessActionKind::MoveAndOfferDraw => "move_and_offer_draw_unavailable",
                ChessActionKind::Resign => "resign_unavailable",
                ChessActionKind::AcceptDraw => "accept_draw_unavailable",
                ChessActionKind::ClaimDraw => "claim_draw_unavailable",
            },
        }
    }

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
            Self::IllegalMove(_) => "The submitted UCI move is not legal in this position.",
            Self::ActionUnavailable { reason, .. } => reason.description(),
        }
    }
}
