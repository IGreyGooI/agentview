use agentview::component::prelude::*;
use chess::{Board, BoardStatus, ChessMove, Color, File, MoveGen, Piece, Rank, Square};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub(super) struct ActionResult {
    pub accepted: bool,
    pub code: &'static str,
    pub message: String,
}

pub(super) struct GameState {
    board: Board,
    history: Vec<(Board, usize, ChessMove)>,
    halfmove_clock: usize,
    feedback: ActionResult,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            board: Board::default(),
            history: Vec::new(),
            halfmove_clock: 0,
            feedback: ActionResult {
                accepted: true,
                code: "ready",
                message: "New game. White to move.".to_owned(),
            },
        }
    }
}

impl GameState {
    pub(super) fn play(&mut self, uci: &str) -> ActionResult {
        let candidate = uci
            .parse::<ChessMove>()
            .ok()
            .filter(|candidate| candidate.to_string() == uci);
        let Some(candidate) = candidate else {
            return self.feedback(
                false,
                "invalid_uci",
                "Expected one canonical lowercase UCI move, such as e2e4 or a7a8q. Position unchanged.",
            );
        };
        if !self.can_move() {
            return self.feedback(
                false,
                "game_over",
                "Game finished. Undo a move or start a new game.",
            );
        }
        if !MoveGen::new_legal(&self.board).any(|legal| legal == candidate) {
            return self.feedback(
                false,
                "illegal_move",
                "Move is not legal in this position. Choose from legal_moves. Position unchanged.",
            );
        }

        self.history
            .push((self.board, self.halfmove_clock, candidate));
        self.halfmove_clock = if self.board.piece_on(candidate.get_source()) == Some(Piece::Pawn)
            || self.board.piece_on(candidate.get_dest()).is_some()
        {
            0
        } else {
            self.halfmove_clock + 1
        };
        self.board = self.board.make_move_new(candidate);
        self.feedback(true, "move_played", format!("Played {candidate}."))
    }

    pub(super) fn undo(&mut self) -> ActionResult {
        match self.history.pop() {
            Some((board, halfmove_clock, played)) => {
                self.board = board;
                self.halfmove_clock = halfmove_clock;
                self.feedback(true, "undone", format!("Undid {played}."))
            }
            None => self.feedback(false, "no_history", "No move to undo. Position unchanged."),
        }
    }

    pub(super) fn new_game(&mut self) -> ActionResult {
        *self = Self::default();
        self.feedback(true, "new_game", "New game. White to move.")
    }

    pub(super) fn can_move(&self) -> bool {
        self.board.status() == BoardStatus::Ongoing
    }

    pub(super) fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    fn feedback(
        &mut self,
        accepted: bool,
        code: &'static str,
        text: impl Into<String>,
    ) -> ActionResult {
        self.feedback = ActionResult {
            accepted,
            code,
            message: text.into(),
        };
        self.feedback.clone()
    }

    pub(super) fn projection(&self) -> Component {
        let (status, winner) = match self.board.status() {
            BoardStatus::Ongoing => ("playing", "none"),
            BoardStatus::Stalemate => ("stalemate", "none"),
            BoardStatus::Checkmate => ("checkmate", side_name(!self.board.side_to_move())),
        };
        let side = side_name(self.board.side_to_move());
        let in_check = self.board.checkers().popcnt() > 0;
        let board = ascii_board(&self.board);
        // The chess crate serializes placeholder clocks; preserve the real game clocks.
        let position = self
            .board
            .to_string()
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        let fen = format!(
            "{position} {} {}",
            self.halfmove_clock,
            self.history.len() / 2 + 1
        );
        let mut legal_moves = MoveGen::new_legal(&self.board)
            .map(|candidate| candidate.to_string())
            .collect::<Vec<_>>();
        legal_moves.sort();
        let legal_count = legal_moves.len();
        let legal_moves = legal_moves.join(" ");
        let history = self
            .history
            .iter()
            .map(|(_, _, played)| played.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let ply = self.history.len();
        let succeeded = self.feedback.accepted;
        let feedback_code = self.feedback.code;
        let feedback_text = &self.feedback.message;

        view! {
            { format!("# Chess\n\n```text\n{board}\n```") }
            chess {
                status: status,
                side_to_move: side,
                in_check: in_check,
                winner: winner,
                ply: ply,
                fen { "{fen}" }
                history { notation: "uci", "{history}" }
                legal_moves { notation: "canonical_lowercase_uci", count: legal_count, "{legal_moves}" }
                feedback { succeeded: succeeded, code: feedback_code, "{feedback_text}" }
                rules {
                    players: "both sides controlled through JSON actions",
                    adjudication: "checkmate and stalemate; other draws are not adjudicated",
                }
            }
        }
    }
}

fn side_name(color: Color) -> &'static str {
    match color {
        Color::White => "white",
        Color::Black => "black",
    }
}

fn ascii_board(board: &Board) -> String {
    let mut rendered = String::new();
    for rank in (0..8).rev() {
        rendered.push_str(&format!("{} ", rank + 1));
        for file in 0..8 {
            let square = Square::make_square(Rank::from_index(rank), File::from_index(file));
            let piece = match (board.piece_on(square), board.color_on(square)) {
                (Some(piece), Some(color)) => piece.to_string(color),
                _ => ".".to_owned(),
            };
            rendered.push_str(&piece);
            rendered.push(' ');
        }
        rendered.push('\n');
    }
    rendered.push_str("  a b c d e f g h");
    rendered
}
