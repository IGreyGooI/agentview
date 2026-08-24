use std::sync::Arc;

use agentview::component::prelude::*;
use chess::{Board, BoardStatus, ChessMove, Color, MoveGen, Piece, Square, ALL_FILES, ALL_RANKS};

use super::chess_agent::ChessSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrawState {
    halfmove_clock: u16,
    current_position_repetitions: u8,
}

#[derive(AgentView)]
#[agent_view(kind = "agent")]
struct AgentPlayerView {
    side: &'static str,
    identity: &'static str,
}

#[derive(AgentView)]
#[agent_view(kind = "opponent")]
struct OpponentPlayerView {
    side: &'static str,
    identity: &'static str,
}

#[derive(AgentView)]
#[agent_view(kind = "players")]
struct PlayersView {
    #[view(root)]
    agent: AgentPlayerView,
    #[view(root)]
    opponent: OpponentPlayerView,
}

#[derive(AgentView)]
#[agent_view(kind = "legal_moves")]
struct LegalMovesView {
    #[view(element)]
    notation: &'static str,
    #[view(element)]
    values: String,
}

#[derive(AgentView)]
#[agent_view(kind = "move")]
struct MoveHistoryItemView {
    ply: usize,
    side: &'static str,
    #[view(text)]
    uci: String,
}

#[derive(AgentView)]
#[agent_view(kind = "history")]
struct HistoryView {
    #[view(element)]
    notation: &'static str,
    #[view(diff(append))]
    moves: Vec<MoveHistoryItemView>,
}

#[derive(AgentView)]
#[agent_view(kind = "draw_state")]
struct DrawStateView {
    #[view(element)]
    halfmove_clock: u16,
    #[view(element)]
    current_position_repetitions: u8,
}

#[derive(AgentView)]
#[agent_view(kind = "pending_draw_offer")]
struct PendingDrawOfferView {
    offered_by: &'static str,
}

#[derive(AgentView)]
#[agent_view(kind = "clocks")]
struct ClocksView {
    available: &'static str,
    reason: &'static str,
}

#[derive(AgentView)]
#[agent_view(kind = "chess_game_state")]
pub(crate) struct ChessGameStateView {
    #[view(element)]
    authority: &'static str,
    #[view(element, diff(replace))]
    match_phase: &'static str,
    #[view(element, diff(replace))]
    board_status: &'static str,
    #[view(element, diff(replace))]
    side_to_move: &'static str,
    #[view(root)]
    players: PlayersView,
    #[view(element, diff(replace))]
    board_ascii: String,
    #[view(element, diff(replace))]
    fen: String,
    #[view(root, diff(replace))]
    legal_moves: LegalMovesView,
    #[view(root, diff)]
    history: HistoryView,
    #[view(root, diff(replace))]
    draw_state: DrawStateView,
    #[view(root, diff(replace))]
    pending_draw_offer: PendingDrawOfferView,
    #[view(root)]
    clocks: ClocksView,
}

impl ChessGameStateView {
    pub(crate) fn from_snapshot(snapshot: &ChessSnapshot) -> Self {
        let board = snapshot.board();
        let board_ascii = board_ascii(&board);
        let fen = position_fen(snapshot);
        let phase = snapshot.match_phase().code();
        let status = status_name(board.status());
        let side_to_move = side_name(board.side_to_move());
        let agent_side = side_name(snapshot.agent_side());
        let opponent_side = side_name(!snapshot.agent_side());
        let legal_moves = legal_uci_moves(&board).join(" ");
        let history = move_history(snapshot.committed_moves());
        let pending_draw_offer = snapshot
            .pending_draw_offer()
            .map(side_name)
            .unwrap_or("none");
        let draw_state = snapshot.draw_state();

        Self {
            authority: "This component is the single authoritative position state. Every complete value and atomic delta is derived from one immutable ChessSnapshot.",
            match_phase: phase,
            board_status: status,
            side_to_move,
            players: PlayersView {
                agent: AgentPlayerView {
                    side: agent_side,
                    identity: "responses_model",
                },
                opponent: OpponentPlayerView {
                    side: opponent_side,
                    identity: "stockfish_uci",
                },
            },
            board_ascii,
            fen,
            legal_moves: LegalMovesView {
                notation: "canonical_lowercase_uci",
                values: legal_moves,
            },
            history: HistoryView {
                notation: "numbered_uci",
                moves: history,
            },
            draw_state: DrawStateView {
                halfmove_clock: draw_state.halfmove_clock(),
                current_position_repetitions: draw_state.current_position_repetitions(),
            },
            pending_draw_offer: PendingDrawOfferView {
                offered_by: pending_draw_offer,
            },
            clocks: ClocksView {
                available: "false",
                reason: "No chess clock is configured for this example; time is not an action input.",
            },
        }
    }
}

impl DrawState {
    pub(crate) fn from_history(history: &[ChessMove]) -> Self {
        let initial = Board::default();
        let positions = history.iter().scan(initial, |board, candidate| {
            *board = board.make_move_new(*candidate);
            Some(*board)
        });
        let boards = std::iter::once(initial)
            .chain(positions)
            .collect::<Vec<_>>();
        let current = boards.last().copied().unwrap_or(initial);
        let current_position_repetitions = boards
            .iter()
            .filter(|position| same_repetition_position(position, &current))
            .count()
            .min(usize::from(u8::MAX)) as u8;
        let (_, halfmove_clock) = history
            .iter()
            .fold((initial, 0_u16), |(board, clock), move_| {
                let next_clock = if board.piece_on(move_.get_source()) == Some(Piece::Pawn)
                    || board.piece_on(move_.get_dest()).is_some()
                {
                    0
                } else {
                    clock.saturating_add(1)
                };
                (board.make_move_new(*move_), next_clock)
            });
        Self {
            halfmove_clock,
            current_position_repetitions,
        }
    }

    pub(crate) fn halfmove_clock(self) -> u16 {
        self.halfmove_clock
    }

    pub(crate) fn current_position_repetitions(self) -> u8 {
        self.current_position_repetitions
    }

    pub(crate) fn claimable(self) -> bool {
        self.halfmove_clock >= 100 || self.current_position_repetitions >= 3
    }

    pub(crate) fn automatic_draw(self) -> bool {
        self.halfmove_clock >= 150 || self.current_position_repetitions >= 5
    }

    pub(crate) fn claim_basis(self) -> &'static str {
        match (
            self.current_position_repetitions >= 3,
            self.halfmove_clock >= 100,
        ) {
            (true, true) => "threefold_repetition_and_fifty_move_rule",
            (true, false) => "threefold_repetition",
            (false, true) => "fifty_move_rule",
            (false, false) => "none",
        }
    }
}

fn same_repetition_position(left: &Board, right: &Board) -> bool {
    const PIECES: [Piece; 6] = [
        Piece::Pawn,
        Piece::Knight,
        Piece::Bishop,
        Piece::Rook,
        Piece::Queen,
        Piece::King,
    ];

    left.side_to_move() == right.side_to_move()
        && left.color_combined(Color::White) == right.color_combined(Color::White)
        && PIECES
            .iter()
            .all(|piece| left.pieces(*piece) == right.pieces(*piece))
        && left.castle_rights(Color::White) == right.castle_rights(Color::White)
        && left.castle_rights(Color::Black) == right.castle_rights(Color::Black)
        && legal_en_passant_target(left) == legal_en_passant_target(right)
}

fn legal_en_passant_target(board: &Board) -> Option<Square> {
    let target = board.en_passant()?.forward(board.side_to_move())?;
    MoveGen::new_legal(board)
        .any(|candidate| {
            candidate.get_dest() == target
                && board.piece_on(candidate.get_source()) == Some(Piece::Pawn)
        })
        .then_some(target)
}

#[component]
pub(crate) fn chess_game_state(snapshot: Arc<ChessSnapshot>) -> Component {
    let view = ChessGameStateView::from_snapshot(&snapshot);

    view! {
        #[diff(slot = "chess_game_state")]
        {view}
    }
}

pub(crate) fn side_name(side: Color) -> &'static str {
    match side {
        Color::White => "white",
        Color::Black => "black",
    }
}

fn status_name(status: BoardStatus) -> &'static str {
    match status {
        BoardStatus::Ongoing => "ongoing",
        BoardStatus::Stalemate => "stalemate",
        BoardStatus::Checkmate => "checkmate",
    }
}

fn position_fen(snapshot: &ChessSnapshot) -> String {
    let board_fields = snapshot
        .board()
        .to_string()
        .split_whitespace()
        .take(4)
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join(" ");
    let halfmove_clock = snapshot.draw_state().halfmove_clock();
    let fullmove_number = snapshot.committed_moves().len() / 2 + 1;
    format!("{board_fields} {halfmove_clock} {fullmove_number}")
}

fn legal_uci_moves(board: &Board) -> Vec<String> {
    let mut moves = MoveGen::new_legal(board)
        .map(|candidate| candidate.to_string())
        .collect::<Vec<_>>();
    moves.sort();
    moves
}

fn move_history(history: &[ChessMove]) -> Vec<MoveHistoryItemView> {
    history
        .iter()
        .enumerate()
        .map(|(index, move_)| MoveHistoryItemView {
            ply: index + 1,
            side: side_name(if index % 2 == 0 {
                Color::White
            } else {
                Color::Black
            }),
            uci: move_.to_string(),
        })
        .collect()
}

fn board_ascii(board: &Board) -> String {
    ALL_RANKS
        .iter()
        .rev()
        .map(|rank| {
            ALL_FILES
                .iter()
                .fold((rank.to_index() + 1).to_string(), |line, file| {
                    let square = Square::make_square(*rank, *file);
                    format!("{line} {}", piece_symbol(board, square))
                })
        })
        .chain(std::iter::once("  a b c d e f g h".to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn piece_symbol(board: &Board, square: Square) -> char {
    let Some(piece) = board.piece_on(square) else {
        return '.';
    };
    let symbol = match piece {
        Piece::Pawn => 'p',
        Piece::Knight => 'n',
        Piece::Bishop => 'b',
        Piece::Rook => 'r',
        Piece::Queen => 'q',
        Piece::King => 'k',
    };
    if board.color_on(square) == Some(Color::White) {
        symbol.to_ascii_uppercase()
    } else {
        symbol
    }
}
