use chess::ChessMove;

const MAX_REJECTED_UCI_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessAction {
    ChooseMove(ChessMove),
    Resign,
}

impl ChessAction {
    pub(crate) fn kind(self) -> ChessActionKind {
        match self {
            Self::ChooseMove(_) => ChessActionKind::ChooseMove,
            Self::Resign => ChessActionKind::Resign,
        }
    }

    pub(crate) fn move_candidate(self) -> Option<ChessMove> {
        match self {
            Self::ChooseMove(candidate) => Some(candidate),
            Self::Resign => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChessActionKind {
    ChooseMove,
    Resign,
}

impl ChessActionKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ChooseMove => "choose_move",
            Self::Resign => "resign",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InvalidActionReason {
    InvalidXml,
    MissingThought,
    InvalidThought,
    MultipleThoughts,
    ThoughtAfterAction,
    MissingAction,
    MultipleActions,
    InvalidUci {
        action: ChessActionKind,
        submitted: String,
        truncated: bool,
    },
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
        }
    }
}

impl InvalidActionReason {
    pub(crate) fn invalid_uci(action: ChessActionKind, submitted: &str) -> Self {
        let mut end = submitted.len().min(MAX_REJECTED_UCI_BYTES);
        while !submitted.is_char_boundary(end) {
            end -= 1;
        }
        Self::InvalidUci {
            action,
            submitted: submitted[..end].to_owned(),
            truncated: end < submitted.len(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidXml => "invalid_xml",
            Self::MissingThought => "missing_thought",
            Self::InvalidThought => "invalid_thought",
            Self::MultipleThoughts => "multiple_thoughts",
            Self::ThoughtAfterAction => "thought_after_action",
            Self::MissingAction => "missing_action",
            Self::MultipleActions => "multiple_actions",
            Self::InvalidUci { .. } => "invalid_uci",
            Self::IllegalMove(_) => "illegal_move",
            Self::ActionUnavailable { action, .. } => match action {
                ChessActionKind::ChooseMove => "choose_move_unavailable",
                ChessActionKind::Resign => "resign_unavailable",
            },
        }
    }

    pub(crate) fn action_kind(&self) -> Option<ChessActionKind> {
        match self {
            Self::InvalidUci { action, .. } | Self::ActionUnavailable { action, .. } => {
                Some(*action)
            }
            Self::IllegalMove(action) => Some(action.kind()),
            Self::InvalidXml
            | Self::MissingThought
            | Self::InvalidThought
            | Self::MultipleThoughts
            | Self::ThoughtAfterAction
            | Self::MissingAction
            | Self::MultipleActions => None,
        }
    }

    pub(crate) fn rejected_action(&self) -> Option<ChessAction> {
        match self {
            Self::IllegalMove(action) => Some(*action),
            Self::InvalidXml
            | Self::MissingThought
            | Self::InvalidThought
            | Self::MultipleThoughts
            | Self::ThoughtAfterAction
            | Self::MissingAction
            | Self::MultipleActions
            | Self::InvalidUci { .. }
            | Self::ActionUnavailable { .. } => None,
        }
    }

    pub(crate) fn rejected_uci(&self) -> Option<(&str, bool)> {
        match self {
            Self::InvalidUci {
                submitted,
                truncated,
                ..
            } => Some((submitted, *truncated)),
            _ => None,
        }
    }

    pub(crate) fn description(&self) -> &'static str {
        match self {
            Self::InvalidXml => "The response did not match the required XML response format.",
            Self::MissingThought => "Write one nonempty <thought> element before the action.",
            Self::InvalidThought => {
                "The thought must be one closed <thought> element containing nonempty text only."
            }
            Self::MultipleThoughts => "Write exactly one <thought> element before the action.",
            Self::ThoughtAfterAction => "Complete the <thought> element before writing the action.",
            Self::MissingAction => "The response contained no action element.",
            Self::MultipleActions => "The response contained more than one action element.",
            Self::InvalidUci { .. } => "The uci attribute was not canonical lowercase UCI.",
            Self::IllegalMove(_) => "The submitted UCI move is not legal in this position.",
            Self::ActionUnavailable { reason, .. } => reason.description(),
        }
    }
}
