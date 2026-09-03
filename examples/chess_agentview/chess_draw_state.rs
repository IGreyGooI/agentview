use chess::{Board, ChessMove, Color, MoveGen, Piece, Square};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrawState {
    halfmove_clock: u16,
    current_position_repetitions: u8,
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

    pub(crate) fn automatic_draw(self) -> bool {
        self.halfmove_clock >= 150 || self.current_position_repetitions >= 5
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
