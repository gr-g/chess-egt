//! Minimal validation for internally generated tablebase setups.
//!
//! Callers establish one king per color, bounded material, and no castling.
//! This is not a validator for arbitrary FEN input.

use shakmaty::{Board, Color, Rank, Role, Setup, Square};

/// Whether the player not to move is in check. Both kings must be present.
pub(crate) fn opposite_check(board: &Board, turn: Color) -> bool {
    let king = board.king_of(!turn).expect("tablebase setups supply both kings");
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
                let expected = setup.clone().position::<Chess>(CastlingMode::Standard)
                    .is_err_and(|error| error.kinds().contains(PositionErrorKinds::OPPOSITE_CHECK));
                assert_eq!(opposite_check(&setup.board, turn), expected, "{fen}, {turn:?}");
            }
        }
    }

    #[test]
    fn structural_ep_matches_checked_constructor() {
        for fen in [
            "7k/8/8/3pP3/8/8/8/K7 w - - 0 1", // Valid EP on d6.
            "7k/3N4/8/3pP3/8/8/8/K7 w - - 0 1", // Occupied double-push origin.
            "7k/8/3N4/3pP3/8/8/8/K7 w - - 0 1", // Occupied target.
            "7k/8/8/4P3/8/8/8/K7 w - - 0 1", // Missing victim.
            "7k/8/8/3PP3/8/8/8/K7 w - - 0 1", // Wrong victim color.
        ] {
            let base = fen.parse::<Fen>().unwrap().into_setup();
            for mirrored in [false, true] {
                let mut setup = if mirrored { base.clone().into_mirrored() } else { base.clone() };
                for ep in std::iter::once(None).chain(Square::ALL.into_iter().map(Some)) {
                    setup.ep_square = ep;
                    let expected = !setup.clone().position::<Chess>(CastlingMode::Standard)
                        .is_err_and(|error| error.kinds().contains(PositionErrorKinds::INVALID_EP_SQUARE));
                    assert_eq!(valid_ep_square(&setup), expected, "{fen}, {mirrored}, {ep:?}");
                }
            }
        }
    }
}
