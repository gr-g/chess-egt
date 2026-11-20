//! # Move counters and diagonal symmetry
//!
//! The move counter is generally initialized as the number of legal moves in a
//! position. Symmetry reduction in pawnless tables requires adjustments because
//! different legal moves can reach positions that map to the same index, and
//! different reverse moves can likewise map to the same predecessor index. For
//! a discussion of symmetry and move counters, see
//! [section 4.6 of this thesis](https://issuu.com/jespertk/docs/master_thesis).
//!
//! Let a position `p` have `#p=8` if it represents 8 equivalent positions and
//! `#p=4` if it represents 4 equivalent positions. With this indexer's choice
//! of canonicalization, `#p=4` positions have both kings on the a1–h8 diagonal.
//!
//! When initializing the counter for a canonical position `p`, if there is a
//! legal move `p -> p'` with `#p=8` and `#p'=4`, there is also a diagonally
//! reflected move from a non-canonical position (the reflection of `p`) to a
//! canonical position (the reflection of `p'`). Backward propagation explores
//! this reflected move too. To account for this, moves `p -> p'` with `#p=8`
//! and `#p'=4` contribute 2 to the counter during initialization. This is
//! implemented by [`symmetry_adjusted_move_counter`].
//!
//! Similarly, when backward propagation from a canonical position `p'` finds
//! a move `p -> p'` with `#p=4` and `#p'=8`, it must decrement both the counter
//! for `p` and the counter for its diagonal reflection. The reflected move
//! contributed to the reflected predecessor's counter but led to a
//! non-canonical successor. [`quiet_unmoves`] emits both predecessor indices
//! so propagation updates both counters.

use shakmaty::{Bitboard, Chess, Color, EnPassantMode, Move, Position, Role, attacks};

use crate::egt::Egt;

// Whether both kings stand on one of the two long diagonals. Kings on the h1-a8
// diagonal end up on the a1-h8 diagonal after canonicalization, so both
// diagonals have to be checked.
fn kings_on_long_diagonal(kings: Bitboard) -> bool {
    kings.into_iter().all(|sq| sq.rank().to_usize() == sq.file().to_usize())
        || kings.into_iter().all(|sq| sq.rank().to_usize() == 7 - sq.file().to_usize())
}

fn both_kings_on_diagonal(position: &Chess) -> bool {
    kings_on_long_diagonal(position.board().by_role(Role::King))
}

/// Quiet predecessors of a pawnless tablebase position, without castling rights.
fn pawnless_quiet_unmoves(position: &Chess, mut f: impl FnMut(Chess)) {
    debug_assert!(position.board().pawns().is_empty());
    debug_assert!(position.castles().castling_rights().is_empty());

    let occupied = position.board().occupied();
    let mut base_setup = position.to_setup(EnPassantMode::Always);
    base_setup.turn = !position.turn();

    for to in position.board().by_color(base_setup.turn) {
        let piece = position.board().piece_at(to).expect("occupied square must have a piece");
        let origins = attacks::attacks(to, piece, occupied) & !occupied;
        for from in origins {
            let mut setup = base_setup.clone();
            setup.board.discard_piece_at(to);
            setup.board.set_piece_at(from, piece);

            // Quiet relocation preserves material and kings; no pawns or EP
            // need validation. Impossible checks remain accepted, as in indexing.
            if !crate::position::opposite_check(&setup.board, setup.turn) {
                f(Chess::from_setup_unchecked(setup, position.castles().mode()));
            }
        }
    }
}

/// Quiet predecessors of a pawnful tablebase position, without castling rights.
fn pawnful_quiet_unmoves(position: &Chess, mut f: impl FnMut(Chess)) {
    use shakmaty::{Rank, Square};

    debug_assert!(position.board().pawns().any());
    debug_assert!(position.castles().castling_rights().is_empty());

    let occupied = position.board().occupied();
    let us = !position.turn();
    let backward = if us == Color::White { -8 } else { 8 };
    let mut base_setup = position.to_setup(EnPassantMode::Always);
    base_setup.turn = us;
    base_setup.ep_square = None;
    let mut emit = |from: Square, to: Square| {
        let mut setup = base_setup.clone();
        let piece = setup.board.piece_at(to).expect("occupied square must have a piece");
        setup.board.discard_piece_at(to);
        setup.board.set_piece_at(from, piece);
        emit_pawnful_predecessors(&setup, &mut f);
    };

    // If there is a legal en-passant move, the previous move can only be a
    // double push.
    if let Some(ep) = position.legal_ep_square() {
        let to = ep.offset(-backward).expect("EP pawn must be on the board");
        let from = ep.offset(backward).expect("EP origin must be on the board");
        if position.board().piece_at(to) == Some(Role::Pawn.of(us))
            && !occupied.contains(from) && !occupied.contains(ep)
        {
            emit(from, to);
        }
        return;
    }

    for to in position.board().by_color(us) {
        let piece = position.board().piece_at(to).expect("occupied square must have a piece");
        if piece.role != Role::Pawn {
            for from in attacks::attacks(to, piece, occupied) & !occupied {
                emit(from, to);
            }
        } else if let Some(from) = to.offset(backward) {
            if occupied.contains(from) {
                continue;
            }
            emit(from, to);
            if to.rank() == us.relative_rank(Rank::Fourth) {
                // The previous move can be a double push, as long as it does
                // not create a legal en-passant.
                let start = from.offset(backward).expect("double-push origin must be on the board");
                if !occupied.contains(start) {
                    let mut successor = base_setup.clone();
                    successor.turn = position.turn();
                    successor.ep_square = Some(from);
                    // The valid source supplies the board and turn; the pawn,
                    // empty target, and empty origin establish structural EP.
                    let successor = Chess::from_setup_unchecked(successor, position.castles().mode());
                    if successor.legal_ep_square().is_none() {
                        emit(start, to);
                    }
                }
            }
        }
    }
}

fn emit_pawnful_predecessors(setup: &shakmaty::Setup, f: &mut impl FnMut(Chess)) {
    use shakmaty::{CastlingMode, Rank, Square};

    debug_assert!(setup.ep_square.is_none());
    // Quiet relocation preserves kings and material (at most eight pieces).
    // Validate the board once for all EP states, still accepting impossible checks.
    if (setup.board.pawns() & Bitboard::BACKRANKS).any()
        || crate::position::opposite_check(&setup.board, setup.turn)
    {
        return;
    }
    f(Chess::from_setup_unchecked(setup.clone(), CastlingMode::Standard));

    let inactive = !setup.turn;
    let occupied = setup.board.occupied();
    let capturers = setup.board.by_piece(Role::Pawn.of(setup.turn));
    let pushed_pawns = setup.board.by_piece(Role::Pawn.of(inactive))
        & Bitboard::from_rank(inactive.relative_rank(Rank::Fourth));
    for pawn in pushed_pawns {
        let ep = Square::from_coords(pawn.file(), inactive.relative_rank(Rank::Third));
        let start = Square::from_coords(pawn.file(), inactive.relative_rank(Rank::Second));
        if occupied.contains(ep) || occupied.contains(start)
            || (attacks::pawn_attacks(inactive, ep) & capturers).is_empty()
        {
            continue;
        }
        let mut setup_ep = setup.clone();
        setup_ep.ep_square = Some(ep);
        // pushed_pawns and the empty target/origin establish structural EP;
        // a legal capture is still required for this indexed EP state.
        let predecessor = Chess::from_setup_unchecked(setup_ep, CastlingMode::Standard);
        if predecessor.legal_ep_square() == Some(ep) {
            f(predecessor);
        }
    }
}

/// Emits predecessors of an index already validated for this table and White.
/// Propagation only passes resolved entries, which initialization validated.
pub(super) fn quiet_unmoves<F>(
    table: &Egt,
    twin: &Egt,
    local_index: usize,
    mirrored: bool,
    mut f: F,
) where
    F: FnMut(usize),
{
    let position = table.position_from_index_unchecked(local_index, Color::White);

    // The diagonal symmetry adjustment below only applies to pawnless tables,
    // and never to a source position that is already on the diagonal (`#p=4`).
    let use_diagonal_symmetry = twin.is_pawnless()
        && !both_kings_on_diagonal(&position);

    let emit = |pred_position: Chess| {
        let pred_idx = if mirrored {
            twin.reflected_position_to_index(&pred_position)
        } else {
            twin.position_to_index(&pred_position)
        };
        f(pred_idx);

        // Emit the diagonal predecessor too for `#p=4` -> `#p'=8` moves.
        // See "Move counters and diagonal symmetry" in the module documentation.
        if use_diagonal_symmetry {
            let maybe_reflected_idx = twin.diagonal_symmetric(pred_idx);
            if let Some(reflected_idx) = maybe_reflected_idx {
                // The current index represents 8 positions, while the predecessor index
                // represents 4 positions. Push the diagonal reflection of the predecessor.
                f(reflected_idx);
            }
        }
    };

    if table.is_pawnless() {
        pawnless_quiet_unmoves(&position, emit);
    } else {
        pawnful_quiet_unmoves(&position, emit);
    }
}

pub(super) fn symmetry_adjusted_move_counter(position: &Chess, legals: &[Move]) -> u16 {
    let kings = position.board().by_role(Role::King);

    // Count both reflected moves for `#p=8` -> `#p'=4` transitions.
    // See "Move counters and diagonal symmetry" in the module documentation.
    if kings_on_long_diagonal(kings) {
        // Already `#p=4`: no successor can require an adjustment.
        return legals.len() as u16;
    }

    let mut counter = 0;
    for m in legals {
        // Whether `#p'=4` depends only on the two king squares, so only a quiet
        // king move can require the adjustment. Deriving the successor's king
        // bitboard directly avoids cloning and replaying the position for every
        // legal move. Captures and promotions lead to simpler tables, for which
        // we don't generate unmoves (they are visited only forward, once during
        // initialization), so they never need an adjustment either.
        let adjust = m.role() == Role::King
            && !m.is_capture()
            && !m.is_promotion()
            && match m.from() {
                Some(from) => kings_on_long_diagonal(
                    (kings ^ Bitboard::from_square(from)) | Bitboard::from_square(m.to()),
                ),
                None => false,
            };
        counter += if adjust { 2 } else { 1 };
    }
    counter
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::selection::TableSelector;
    use crate::table_id::TableId;

    fn tables(material: &str) -> Vec<Egt> {
        TableSelector::parse(material).unwrap().resolve().unwrap().into_iter()
            .map(|id| Egt::new(&id).unwrap())
            .collect()
    }

    fn table(name: &str) -> Egt {
        Egt::new(&TableId::parse(name).unwrap()).unwrap()
    }

    #[test]
    fn pawnless_unmoves_match_forward_play() {
        for endgame in ["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KQ_KR", "KNN_K", "KQR_KB"] {
            for egt in tables(endgame) {
                for turn in [Color::White, Color::Black] {
                    let mut checked = 0;
                    let stride = (egt.index_range() / 512).max(1);
                    for idx in (0..egt.index_range()).step_by(stride) {
                        if let Some(position) = egt.position_from_index(idx, turn) {
                            assert_quiet_unmoves_match_forward_play(&position);
                            checked += 1;
                        }
                    }
                    assert!(checked > 0, "no valid samples for {endgame}, {turn:?}");
                }
            }
        }
    }

    #[test]
    fn pawnless_unmoves_match_forward_play_in_checks_and_terminal_positions() {
        for fen in [
            "7k/6Q1/5K2/8/8/8/8/8 b - - 0 1", // Checkmate.
            "7k/5K2/6Q1/8/8/8/8/8 b - - 0 1", // Stalemate.
            "4k3/8/8/1B6/8/8/8/K3R3 b - - 0 1", // Discovered double check.
            "4k3/4r3/8/8/8/8/8/K3R3 w - - 0 1", // Pinned previous mover.
            "4r2k/8/8/8/8/8/4R3/4K3 b - - 0 1", // Last move can block check.
        ] {
            let position = test_position(fen);
            assert_quiet_unmoves_match_forward_play(&position);
        }
    }

    fn test_position(fen: &str) -> Chess {
        fen.parse::<shakmaty::fen::Fen>().unwrap()
            .into_position(shakmaty::CastlingMode::Standard)
            .or_else(shakmaty::PositionError::ignore_impossible_check).unwrap()
    }

    fn color_mirrored(position: &Chess) -> Chess {
        position.to_setup(EnPassantMode::Always).into_mirrored()
            .position(shakmaty::CastlingMode::Standard)
            .or_else(shakmaty::PositionError::ignore_impossible_check).unwrap()
    }

    // Exhaust all empty origins and canonical EP states. Checked forward play
    // provides an oracle independent of the optimized reverse-move geometry.
    fn quiet_predecessors_by_forward_play(position: &Chess) -> HashMap<Chess, usize> {
        use shakmaty::{CastlingMode, File, Rank, Square};

        let mut expected = HashMap::new();
        let mut base = position.to_setup(EnPassantMode::Always);
        base.turn = !position.turn();
        for to in position.board().by_color(base.turn) {
            let piece = position.board().piece_at(to).unwrap();
            for from in !position.board().occupied() {
                let mut setup = base.clone();
                setup.board.discard_piece_at(to);
                setup.board.set_piece_at(from, piece);
                let m = Move::Normal {
                    role: piece.role, from, to, capture: None, promotion: None,
                };
                let ep_rank = setup.turn.relative_rank(Rank::Sixth);
                for ep in std::iter::once(None).chain(File::ALL.into_iter()
                    .map(|file| Some(Square::from_coords(file, ep_rank))))
                {
                    setup.ep_square = ep;
                    if let Ok(predecessor) = setup.clone().position::<Chess>(CastlingMode::Standard)
                        .or_else(shakmaty::PositionError::ignore_impossible_check)
                    {
                        if predecessor.legal_ep_square() == ep
                            && predecessor.clone().play(m).is_ok_and(|next| next == *position)
                        {
                            *expected.entry(predecessor).or_insert(0usize) += 1;
                        }
                    }
                }
            }
        }
        expected
    }

    fn assert_quiet_unmoves_match_forward_play(position: &Chess) -> HashMap<Chess, usize> {
        let expected = quiet_predecessors_by_forward_play(position);
        let mut actual = HashMap::new();
        let emit = |predecessor: Chess| {
            assert_eq!(predecessor.maybe_ep_square(), predecessor.legal_ep_square());
            *actual.entry(predecessor).or_insert(0usize) += 1;
        };
        if position.board().pawns().is_empty() {
            pawnless_quiet_unmoves(position, emit);
        } else {
            pawnful_quiet_unmoves(position, emit);
        }
        assert_eq!(actual, expected, "predecessor multisets differ for {position:?}");
        actual
    }

    #[test]
    fn pawnful_unmoves_match_forward_play() {
        let mut ep_checked = 0;
        for endgame in ["KP_K", "KQ_KP", "KP_KP"] {
            for egt in tables(endgame) {
                for turn in [Color::White, Color::Black] {
                    let mut checked = 0;
                    let stride = (egt.index_range() / 64).max(1);
                    for idx in (0..egt.index_range()).step_by(stride) {
                        if let Some(position) = egt.position_from_index(idx, turn) {
                            assert_quiet_unmoves_match_forward_play(&position);
                            ep_checked += usize::from(position.legal_ep_square().is_some());
                            checked += 1;
                        }
                    }
                    assert!(checked > 0, "no valid samples for {}, {turn:?}", egt.id());
                }
            }
        }
        assert!(ep_checked > 0, "samples must include EP successors");
    }

    #[test]
    fn symmetric_pawn_quiet_edge_multiplicities() {
        let mut ep_predecessors = 0;
        let mut ep_successors = 0;
        for name in ["KPaPh_K", "K_KPaPh", "KPcPf_KPdPe", "KPd_KPe", "KPaPa_KPh"] {
            let id = TableId::parse(name).unwrap();
            let (twin_id, mirrored) = id.twin();
            let egt = Egt::new(&id).unwrap();
            let twin = Egt::new(&twin_id).unwrap();
            let reverse_mirrored = twin_id.twin().1;
            let stride = (egt.index_range() / 128).max(1);
            let mut samples: Vec<_> = (0..egt.index_range()).step_by(stride).collect();
            if name == "KPcPf_KPdPe" {
                // Exercise both reflected EP files, EP expiry, and double pushes creating EP.
                for fen in [
                    "7k/8/8/2PppP2/8/8/8/K7 w - d6 0 1",
                    "7k/8/8/2PppP2/8/8/8/K7 w - e6 0 1",
                    "7k/8/8/8/3pp3/8/2P2P2/K7 w - - 0 1",
                ] {
                    samples.push(egt.position_to_index(&test_position(fen)));
                }
            }
            let mut checked = 0;
            for turn in [Color::White, Color::Black] {
                for &pred_idx in &samples {
                    let Some(position) = egt.position_from_index(pred_idx, turn) else { continue };
                    assert_eq!(egt.position_to_index(&position), pred_idx);
                    ep_predecessors += usize::from(position.legal_ep_square().is_some());
                    let mut successors = HashMap::new();
                    for m in position.legal_moves() {
                        if m.is_capture() || m.is_promotion() { continue; }
                        let successor = position.clone().play(m).unwrap();
                        ep_successors += usize::from(successor.legal_ep_square().is_some());
                        let succ_idx = if mirrored {
                            twin.reflected_position_to_index(&successor)
                        } else {
                            twin.position_to_index(&successor)
                        };
                        *successors.entry(succ_idx).or_insert(0usize) += 1;
                    }
                    for (succ_idx, expected) in successors {
                        assert!(twin.position_from_index(succ_idx, Color::White).is_some());
                        let mut actual = 0;
                        quiet_unmoves(&twin, &egt, succ_idx, reverse_mirrored, |idx| {
                            if idx == pred_idx { actual += 1; }
                        });
                        assert_eq!(actual, expected,
                            "{name}, {turn:?}, predecessor {pred_idx}, successor {succ_idx}");
                    }
                    checked += 1;
                }
            }
            assert!(checked > 0, "no valid samples for {name}");
        }
        assert!(ep_predecessors > 0, "samples must include EP expiry");
        assert!(ep_successors > 0, "samples must include double pushes creating EP");
    }

    #[test]
    fn pawnful_unmoves_match_forward_play_in_ep_and_blocked_positions() {
        for fen in [
            "7k/8/8/8/8/4P3/8/K7 b - - 0 1", // Single pawn push.
            "7k/8/8/8/4P3/8/8/K7 b - - 0 1", // Double pawn push without EP.
            "7k/8/8/8/4P3/4B3/8/K7 b - - 0 1", // Blocked double-push intermediate.
            "7k/8/8/8/4P3/8/4B3/K7 b - - 0 1", // Blocked double-push origin.
            "7k/8/8/8/8/8/4P3/K7 b - - 0 1", // Reverse push to backrank is invalid.
            "4k3/8/8/KPp4r/8/8/8/8 b - - 0 1", // EP exposes a horizontal rook check.
            "4k3/8/8/KPp4r/8/8/8/8 w - - 0 1", // Double push with illegal horizontal EP.
            "7k/8/8/3pP3/8/8/8/K7 w - - 0 1", // A legal EP right would distinguish a double push.
            "k3r3/8/8/2PpP3/8/8/8/4K3 w - - 0 1", // One legal EP capturer is sufficient.
            "k3r3/8/8/2PpP3/8/8/7R/4K3 b - - 0 1", // Only one of two EP capturers is legal.
            "7k/8/2N5/3Pp3/8/8/8/K7 b - - 0 1", // Unmoving to e7 prevents EP restoration.
            "7k/8/4N3/3Pp3/8/8/8/K7 b - - 0 1", // Unmoving from e6 can restore EP.
            "4r2k/8/8/8/8/3P4/8/4K3 w - - 0 1", // Checked successor.
        ] {
            let position = test_position(fen);
            assert_quiet_unmoves_match_forward_play(&position);
            assert_quiet_unmoves_match_forward_play(&color_mirrored(&position));
        }
    }

    #[test]
    fn pawnful_predecessors_reject_invalid_boards_before_restoring_ep() {
        for fen in [
            "7k/8/8/3Pp3/8/8/K7/P7 w - - 0 1", // Backrank pawn.
            "7k/8/8/3Pp3/8/8/K7/7R w - - 0 1", // Opposite check.
        ] {
            let original = fen.parse::<shakmaty::fen::Fen>().unwrap().into_setup();
            for mirrored in [false, true] {
                let mut setup = original.clone();
                if mirrored { setup.mirror(); }
                // Both no-EP and structurally valid restored EP states must fail.
                for ep in [None, Some(if mirrored { shakmaty::Square::E3 } else { shakmaty::Square::E6 })] {
                    let mut candidate = setup.clone();
                    candidate.ep_square = ep;
                    assert!(candidate.position::<Chess>(shakmaty::CastlingMode::Standard)
                        .or_else(shakmaty::PositionError::ignore_impossible_check).is_err());
                }
                let mut emitted = 0;
                emit_pawnful_predecessors(&setup, &mut |_| emitted += 1);
                assert_eq!(emitted, 0, "invalid predecessor board accepted: {fen}, {mirrored}");
            }
        }
    }

    #[test]
    fn pawnful_unmoves_force_double_push_for_ep_successor() {
        use shakmaty::Square;

        for (fen, states) in [
            ("7k/8/8/3pP3/8/8/8/K7 w - d6 0 1", 1),
            ("7k/8/8/3pP3/4K3/8/8/8 w - d6 0 1", 1), // EP captures the checking pawn.
            // The forced double push can itself expire a previous legal EP right.
            ("7k/8/8/3pP3/4Pp2/8/8/K7 w - d6 0 1", 2),
        ] {
            let position = test_position(fen);
            for mirrored in [false, true] {
                let successor = if mirrored { color_mirrored(&position) } else { position.clone() };
                let actual = assert_quiet_unmoves_match_forward_play(&successor);
                assert_eq!(actual.values().sum::<usize>(), states);
                let (from, to) = if mirrored {
                    (Square::D7.flip_vertical(), Square::D5.flip_vertical())
                } else {
                    (Square::D7, Square::D5)
                };
                for predecessor in actual.keys() {
                    assert_eq!(predecessor.board().piece_at(from), Some(Role::Pawn.of(predecessor.turn())));
                    assert!(predecessor.board().piece_at(to).is_none());
                }
            }
        }
    }

    #[test]
    fn pawnful_unmoves_restore_distinct_ep_states() {
        use shakmaty::Square;

        for (successor_fen, predecessor_fen, ep_squares) in [
            ("4k3/8/8/3Pp3/8/8/R3K3/8 b - - 0 1",
             "4k3/8/8/3Pp3/8/8/4K3/R7 w - - 0 1", vec![Square::E6]),
            ("4k3/8/8/3Pp3/8/P7/4K3/8 b - - 0 1",
             "4k3/8/8/3Pp3/8/8/P3K3/8 w - - 0 1", vec![Square::E6]),
            ("7k/8/8/3Pp1Pp/8/7R/8/K7 b - - 0 1",
             "7k/8/8/3Pp1Pp/8/8/7R/K7 w - - 0 1", vec![Square::E6, Square::H6]),
        ] {
            let position = test_position(successor_fen);
            let predecessor = test_position(predecessor_fen);
            for mirrored in [false, true] {
                let successor = if mirrored { color_mirrored(&position) } else { position.clone() };
                let actual = assert_quiet_unmoves_match_forward_play(&successor);
                for ep in std::iter::once(None).chain(ep_squares.iter().copied().map(Some)) {
                    let mut setup = predecessor.to_setup(EnPassantMode::Always);
                    setup.ep_square = ep;
                    if mirrored { setup.mirror(); }
                    let expected: Chess = setup.position(shakmaty::CastlingMode::Standard).unwrap();
                    assert_eq!(expected.legal_ep_square(), ep.map(|sq| if mirrored { sq.flip_vertical() } else { sq }));
                    assert_eq!(actual.get(&expected), Some(&1), "missing restored EP state {expected:?}");
                }
            }
        }
    }

    #[test]
    fn pawnful_unmoves_include_double_push_with_illegal_ep() {
        use shakmaty::Square;

        let position = test_position("k3r3/8/8/3pP3/8/8/8/4K3 w - - 0 1");
        let predecessor = test_position("k3r3/3p4/8/4P3/8/8/8/4K3 b - - 0 1");
        for mirrored in [false, true] {
            let successor = if mirrored { color_mirrored(&position) } else { position.clone() };
            let predecessor = if mirrored { color_mirrored(&predecessor) } else { predecessor.clone() };
            let m = Move::Normal {
                role: Role::Pawn,
                from: if mirrored { Square::D7.flip_vertical() } else { Square::D7 },
                to: if mirrored { Square::D5.flip_vertical() } else { Square::D5 },
                capture: None, promotion: None,
            };
            let replay = predecessor.clone().play(m).unwrap();
            assert_eq!(replay, successor);
            assert!(replay.maybe_ep_square().is_some());
            assert!(replay.legal_ep_square().is_none());
            let actual = assert_quiet_unmoves_match_forward_play(&successor);
            assert_eq!(actual.get(&predecessor), Some(&1));
            // Raw EP after forward play represents the same indexed state.
            assert_eq!(assert_quiet_unmoves_match_forward_play(&replay), actual);
        }
    }

    #[test]
    fn pawnful_unmoves_include_double_push_giving_check() {
        use shakmaty::Square;

        let predecessor = test_position("8/8/8/8/2p5/8/RPk5/7K w - - 0 1");
        let push = Move::Normal {
            role: Role::Pawn, from: Square::B2, to: Square::B4,
            capture: None, promotion: None,
        };
        let successor = predecessor.clone().play(push).unwrap();
        assert!(successor.is_check());
        assert_eq!(successor.maybe_ep_square(), Some(Square::B3));
        assert!(successor.legal_ep_square().is_none(), "EP cannot evade the rook check");
        for (before, after) in [
            (predecessor.clone(), successor.clone()),
            (color_mirrored(&predecessor), color_mirrored(&successor)),
        ] {
            let actual = assert_quiet_unmoves_match_forward_play(&after);
            assert_eq!(actual.get(&before), Some(&1));
        }

        // Exercise the indexed propagation path without generating the
        // five-piece table; the edge must retain its multiplicity.
        let before = table("KRPb_KPc");
        let after = table("KPc_KRPb");
        let predecessor_idx = before.position_to_index(&predecessor);
        assert_eq!(predecessor_idx, 479947);
        let successor_idx = after.position_to_index(&successor);
        let mut hits = 0;
        quiet_unmoves(&after, &before, successor_idx, false, |idx| {
            if idx == predecessor_idx { hits += 1; }
        });
        assert_eq!(hits, 1, "missing the b2-b4 predecessor edge");
    }
}
