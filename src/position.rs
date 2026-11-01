//! Minimal validation for internally generated tablebase setups.
//!
//! Callers establish one king per color, bounded material, and no castling.
//! This is not a validator for arbitrary FEN input.

use shakmaty::{Board, CastlingMode, Chess, Color, Position, Rank, Role, Setup, Square};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidPosition {
    OppositeCheck,
    EpStructure,
    IllegalEpCapture,
}

/// Nonredundant invalid indices, accumulated during the initialization decode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InvalidCounts {
    pub(crate) opposite_check: usize,
    pub(crate) ep_structure: usize,
    pub(crate) illegal_ep_capture: usize,
}

impl InvalidCounts {
    pub(crate) fn record(&mut self, reason: InvalidPosition) {
        match reason {
            InvalidPosition::OppositeCheck => self.opposite_check += 1,
            InvalidPosition::EpStructure => self.ep_structure += 1,
            InvalidPosition::IllegalEpCapture => self.illegal_ep_capture += 1,
        }
    }

    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            opposite_check: self.opposite_check + other.opposite_check,
            ep_structure: self.ep_structure + other.ep_structure,
            illegal_ep_capture: self.illegal_ep_capture + other.illegal_ep_capture,
        }
    }
}

/// Validate a decoded setup, retaining the reason for rejected indices.
/// Indexing guarantees material, pawn ranks, nonoverlap, and no castling.
/// Opposite check takes precedence over EP failures; impossible checks are accepted.
pub(crate) fn position_from_setup(setup: Setup) -> Result<Chess, InvalidPosition> {
    if opposite_check(&setup.board, setup.turn) {
        return Err(InvalidPosition::OppositeCheck);
    }
    if !valid_ep_square(&setup) {
        return Err(InvalidPosition::EpStructure);
    }
    let ep_square = setup.ep_square;
    let position = Chess::from_setup_unchecked(setup, CastlingMode::Standard);
    if position.legal_ep_square() != ep_square {
        let ep = ep_square.expect("only encoded EP states can fail legal EP validation");
        // Only diagnose the absent capturer after legal EP validation fails.
        // Valid positions need no additional geometric attack query.
        let capturers =
            position
                .board()
                .attacks_to(ep, position.turn(), position.board().occupied())
                & position.board().pawns();
        return Err(if capturers.is_empty() {
            InvalidPosition::EpStructure
        } else {
            InvalidPosition::IllegalEpCapture
        });
    }
    Ok(position)
}

/// Whether the player not to move is in check. Both kings must be present.
pub(crate) fn opposite_check(board: &Board, turn: Color) -> bool {
    let king = board
        .king_of(!turn)
        .expect("tablebase setups supply both kings");
    board.attacks_to(king, turn, board.occupied()).any()
}

/// Structural EP validity, independently of whether a legal capture exists.
pub(crate) fn valid_ep_square(setup: &Setup) -> bool {
    let Some(ep) = setup.ep_square else {
        return true;
    };
    if ep.rank() != setup.turn.relative_rank(Rank::Sixth) {
        return false;
    }
    let pushed_to = Square::from_coords(ep.file(), setup.turn.relative_rank(Rank::Fifth));
    let pushed_from = Square::from_coords(ep.file(), setup.turn.relative_rank(Rank::Seventh));
    setup.board.piece_at(pushed_to) == Some(Role::Pawn.of(!setup.turn))
        && !setup.board.occupied().contains(ep)
        && !setup.board.occupied().contains(pushed_from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakmaty::{CastlingMode, Chess, PositionErrorKinds, fen::Fen};

    #[test]
    fn invalidity_distinguishes_ep_failures_and_precedence() {
        use InvalidPosition::{EpStructure, IllegalEpCapture, OppositeCheck};

        for (fen, expected) in [
            ("7k/8/8/3Pp3/8/8/8/K7 w - e6 0 1", None),
            ("7k/8/8/3Pp3/3K4/8/8/8 w - e6 0 1", None),
            // Occupied EP target, occupied push origin, and no adjacent capturer.
            ("7k/8/4N3/3Pp3/8/8/8/K7 w - e6 0 1", Some(EpStructure)),
            ("7k/4N3/8/3Pp3/8/8/8/K7 w - e6 0 1", Some(EpStructure)),
            ("7k/8/8/4p3/3P4/8/8/K7 w - e6 0 1", Some(EpStructure)),
            // Horizontal discovery, pinned capturer, and a check EP cannot evade.
            ("7k/8/8/K2Pp2r/8/8/8/8 w - e6 0 1", Some(IllegalEpCapture)),
            ("3r3k/8/8/3Pp3/8/8/8/3K4 w - e6 0 1", Some(IllegalEpCapture)),
            ("r6k/8/8/3Pp3/8/8/8/K7 w - e6 0 1", Some(IllegalEpCapture)),
            // One pinned capturer does not invalidate the other legal capture.
            ("3r3k/8/8/3PpP2/8/8/8/3K4 w - e6 0 1", None),
            ("7k/8/8/8/8/8/8/K6R w - - 0 1", Some(OppositeCheck)),
            ("8/8/8/8/8/4P3/1k6/K7 w - - 0 1", Some(OppositeCheck)),
            // Opposite check takes precedence over the occupied push origin.
            ("7k/4N3/8/3Pp3/8/8/8/K6R w - e6 0 1", Some(OppositeCheck)),
            // Preserve the indexer's acceptance of impossible checks.
            ("r6b/7k/8/8/8/8/2n5/K7 w - - 0 1", None),
        ] {
            let original = fen.parse::<Fen>().unwrap().into_setup();
            for mirrored in [false, true] {
                let setup = if mirrored {
                    original.clone().into_mirrored()
                } else {
                    original.clone()
                };
                assert_eq!(
                    position_from_setup(setup).err(),
                    expected,
                    "{fen}, {mirrored}"
                );
            }
        }
    }

    #[test]
    fn opposite_check_matches_checked_constructor() {
        for fen in [
            "7k/8/8/8/8/8/8/K7 w - - 0 1",
            "7k/8/8/8/8/8/8/K6R w - - 0 1",
            "7k/8/8/8/8/8/2n3n1/4K3 w - - 0 1",
            "8/8/8/8/8/8/4k3/4K3 w - - 0 1",
        ] {
            let mut setup = fen.parse::<Fen>().unwrap().into_setup();
            for turn in Color::ALL {
                setup.turn = turn;
                let expected = setup
                    .clone()
                    .position::<Chess>(CastlingMode::Standard)
                    .is_err_and(|error| error.kinds().contains(PositionErrorKinds::OPPOSITE_CHECK));
                assert_eq!(
                    opposite_check(&setup.board, turn),
                    expected,
                    "{fen}, {turn:?}"
                );
            }
        }
    }

    #[test]
    fn structural_ep_matches_checked_constructor() {
        for fen in [
            "7k/8/8/3pP3/8/8/8/K7 w - - 0 1",   // Valid EP on d6.
            "7k/3N4/8/3pP3/8/8/8/K7 w - - 0 1", // Occupied double-push origin.
            "7k/8/3N4/3pP3/8/8/8/K7 w - - 0 1", // Occupied target.
            "7k/8/8/4P3/8/8/8/K7 w - - 0 1",    // Missing victim.
            "7k/8/8/3PP3/8/8/8/K7 w - - 0 1",   // Wrong victim color.
        ] {
            let base = fen.parse::<Fen>().unwrap().into_setup();
            for mirrored in [false, true] {
                let mut setup = if mirrored {
                    base.clone().into_mirrored()
                } else {
                    base.clone()
                };
                for ep in std::iter::once(None).chain(Square::ALL.into_iter().map(Some)) {
                    setup.ep_square = ep;
                    let expected = !setup
                        .clone()
                        .position::<Chess>(CastlingMode::Standard)
                        .is_err_and(|error| {
                            error
                                .kinds()
                                .contains(PositionErrorKinds::INVALID_EP_SQUARE)
                        });
                    assert_eq!(
                        valid_ep_square(&setup),
                        expected,
                        "{fen}, {mirrored}, {ep:?}"
                    );
                }
            }
        }
    }
}
