use crate::{InvalidPosition, TableId};
use shakmaty::{Chess, Color};
mod indexer;
use indexer::Indexer;

/// An immutable indexing descriptor, in either horizontal orientation.
#[derive(Clone, Debug)]
pub struct Egt {
    id: TableId,
    indexer: Indexer,
}

impl Egt {
    /// The number of physical pieces, including pawns and kings.
    pub(crate) fn n_pieces(&self) -> usize {
        self.indexer.n_pieces
    }

    /// Positions per generation summary block, following permutation geometry.
    pub(crate) fn summary_block_size(&self) -> usize {
        let n = self.n_pieces();
        (65 - n) * (66 - n)
    }

    /// Positions per compressed frame; identical-piece groups may cross boundaries.
    pub(crate) fn frame_size(&self) -> usize {
        self.summary_block_size() * (67 - self.n_pieces()) * 2
    }

    /// The number of pawns in this table's material.
    pub fn n_pawns(&self) -> usize {
        self.indexer.n_pawns
    }

    /// Whether this table contains no pawns.
    pub fn is_pawnless(&self) -> bool {
        self.n_pawns() == 0
    }

    /// The number of indexed entries, including invalid and redundant positions.
    /// Table-local indices lie in `0..self.index_range()`.
    pub fn index_range(&self) -> usize {
        self.indexer.index_range
    }

    /// The table identity in this indexer's horizontal orientation.
    pub fn id(&self) -> TableId {
        self.id
    }

    /// Builds an indexing plan for a validated table identity.
    pub fn new(id: &TableId) -> Self {
        Self {
            id: *id,
            indexer: Indexer::new(id),
        }
    }

    /// Encodes a no-castling position matching this table's material, pawn files,
    /// and same-file pawn color order. The caller must establish that the
    /// position belongs to this table before encoding it.
    pub(crate) fn position_to_index(&self, position: &Chess) -> usize {
        self.indexer.position_to_index(position)
    }

    /// Encodes the horizontal reflection of a no-castling position.
    /// The reflected position must match this table's material, pawn files,
    /// and same-file pawn color order.
    pub(crate) fn reflected_position_to_index(&self, position: &Chess) -> usize {
        self.indexer.reflected_position_to_index(position)
    }

    /// Decodes a symmetry-reduced position with the requested side to move.
    /// Returns the rejection reason for an out-of-range index or invalid position.
    /// Castling rights are absent; en passant rights are retained only when legal.
    /// Impossible checks are accepted, as in table generation.
    pub fn position_from_index(
        &self,
        index: usize,
        side_to_move: Color,
    ) -> Result<Chess, InvalidPosition> {
        self.indexer.position_from_index(index, side_to_move)
    }

    /// Decodes an index already accepted by `position_from_index` for this table and turn.
    /// Skips all position validation, including legal en passant. Panics if out of range.
    pub(crate) fn position_from_index_unchecked(&self, index: usize, side_to_move: Color) -> Chess {
        self.indexer
            .position_from_index_unchecked(index, side_to_move)
    }

    /// Returns the diagonal-reflected index for a pawnless entry whose kings lie
    /// on a long diagonal, or `None` otherwise. Both indices represent equivalent
    /// positions but are stored separately. The returned index equals `index`
    /// when all pieces lie on the diagonal.
    ///
    /// # Panics
    /// Panics if this table is pawnless and `index` is out of range.
    pub fn diagonal_symmetric(&self, index: usize) -> Option<usize> {
        self.indexer.diagonal_symmetric(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EgtError;
    use crate::table_id::{
        EgtSide, parse_oriented,
        test_support::{pawn_group, with_pawns},
    };
    use EgtSide::{SideNotToMove as Sntm, SideToMove as Stm};
    use shakmaty::File;

    #[test]
    fn naming_uses_next_occupied_slot_and_side_traversal() {
        for (order, expected) in [
            (vec![Stm, Sntm], "KPao_KPao"),
            (vec![Sntm, Stm], "KPa_KPa"),
            (vec![Stm, Stm, Sntm], "KPaPao_KPao"),
            (vec![Stm, Sntm, Sntm], "KPao_KPaPao"),
            (vec![Stm, Sntm, Stm], "KPaoPa_KPao"),
            (vec![Sntm, Stm, Sntm], "KPao_KPaoPa"),
            (vec![Stm, Stm, Sntm, Stm, Sntm, Sntm], "KPaPaoPao_KPaPaoPao"),
        ] {
            let id = with_pawns("K_K", &[pawn_group(File::A, &order)]);
            assert_eq!(id.name().as_str(), expected, "{order:?}");
            assert_eq!(TableId::parse(expected).unwrap(), id);
        }
    }

    #[test]
    fn naming_round_trips_every_pawn_word_through_six_slots() {
        for len in 1..=6 {
            let mut names = std::collections::HashSet::new();
            for word in 0..1usize << len {
                let order: Vec<_> = (0..len)
                    .map(|i| if word & (1 << i) == 0 { Stm } else { Sntm })
                    .collect();
                let id = with_pawns("K_K", &[pawn_group(File::A, &order)]);
                let name = id.name();
                assert!(names.insert(name), "duplicate name for {order:?}");
                assert_eq!(TableId::parse(&name).unwrap(), id);
            }
        }
    }

    #[test]
    fn naming_sorts_files_and_preserves_nonpawn_names() {
        let id = with_pawns(
            "KQN_K",
            &[
                pawn_group(File::H, &[Stm]),
                pawn_group(File::B, &[Stm, Sntm]),
            ],
        );
        assert_eq!(id.name().as_str(), "KQNPboPh_KPbo");
        for name in ["K_K", "KQ_K", "KQQ_K", "KQRBN_KR", "KPaPa_KPh", "KPaPh_K"] {
            assert_eq!(TableId::parse(name).unwrap().name().as_str(), name);
        }
    }

    #[test]
    fn naming_mixed_file_without_flags_is_explicitly_unopposed() {
        let id = parse_oriented("KPdPe_KPePePe").unwrap();
        assert_eq!(
            id,
            with_pawns(
                "K_K",
                &[
                    pawn_group(File::D, &[Stm]),
                    pawn_group(File::E, &[Sntm, Sntm, Sntm, Stm]),
                ]
            )
        );
    }

    #[test]
    fn naming_rejects_invalid_grammar_and_opposed_flags() {
        for name in [
            "",
            "K",
            "K_K_K",
            "_K",
            "KK_K",
            "K_KK",
            "KQQQQQQQ_K",
            "KPaPaPaPaPaPa_KPb",
            "KPaPaPaPaPaPaPa_K",
            "K_Q",
            "Kk_K",
            "K P a_K",
            "KPaO_K",
            "KP_K",
            "KPi_K",
            "KPo_K",
            "Ko_K",
            "KPa oo_K",
            "KPaoo_KPao",
            "KPao_K",
            "KPa_KPao",
            "KPao_KPa",
            "KPaPao_KPaPa",
            "KPaPa_KPaoPao",
            "KPaPaPaPa_KPaPaPa",
            "QK_K",
            "KRQ_K",
            "KPaQ_K",
            "KPbPa_K",
            "KPa_KPhPa",
        ] {
            assert!(
                matches!(
                    TableId::parse(name),
                    Err(EgtError::InvalidEndgameName { .. })
                ),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn table_specific_block_sizes() {
        for (name, pieces, pawns, summary, frame) in [
            ("K_K", 2, 0, 4032, 524160),
            ("KQ_K", 3, 0, 3906, 499968),
            ("KQ_KR", 4, 0, 3782, 476532),
            ("KBBB_K", 5, 0, 3660, 453840),
            ("KPaPa_KPb", 5, 3, 3660, 453840),
            ("KQQPaoPa_KPao", 7, 3, 3422, 410640),
            ("KNNNNNN_K", 8, 0, 3306, 390108),
        ] {
            let egt = Egt::new(&TableId::parse(name).unwrap());
            assert_eq!(egt.id().name().as_str(), name);
            assert_eq!(egt.n_pieces(), pieces);
            assert_eq!(egt.n_pawns(), pawns);
            assert_eq!(egt.summary_block_size(), summary);
            assert_eq!(egt.frame_size(), frame);
        }
    }

    #[test]
    fn reflected_index_matches_board_reflection() {
        use crate::position::mirror_horizontally;

        for name in ["KPa_K", "KPd_KPe", "KPaPaPd_KPbPf", "KNN_K"] {
            let egt = Egt::new(&TableId::parse(name).unwrap());
            let stride = (egt.index_range() / 256).max(1);
            let mut checked = 0;
            for turn in [Color::White, Color::Black] {
                for idx in (0..egt.index_range()).step_by(stride) {
                    if let Ok(position) = egt.position_from_index(idx, turn) {
                        let source = mirror_horizontally(&position);
                        assert_eq!(
                            egt.reflected_position_to_index(&source),
                            idx,
                            "{name}, {turn:?}, {idx}"
                        );
                        checked += 1;
                    }
                }
            }
            assert!(checked > 0, "no valid samples for {name}");
        }
    }

    #[test]
    fn reflected_index_preserves_legal_en_passant() {
        use crate::position::mirror_horizontally;
        use shakmaty::{CastlingMode, Position, fen::Fen};

        for (fen, name) in [
            ("7k/8/8/3Pp3/8/8/8/K7 w - e6 0 1", "KPe_KPd"),
            ("7k/8/8/8/3pP3/8/8/K7 b - e3 0 1", "KPe_KPd"),
            ("7k/8/8/3Pp3/8/4P3/4P3/K7 w - e6 0 1", "KPdPdoPe_KPdo"),
            ("7k/4p3/4p3/8/3pP3/8/8/K7 b - e3 0 1", "KPdPdoPe_KPdo"),
        ] {
            let position: Chess = fen
                .parse::<Fen>()
                .unwrap()
                .into_position(CastlingMode::Standard)
                .unwrap();
            assert!(position.legal_ep_square().is_some());
            let reflected = mirror_horizontally(&position);
            assert_eq!(
                reflected.legal_ep_square(),
                position.legal_ep_square().map(|sq| sq.flip_horizontal())
            );
            let egt = Egt::new(&parse_oriented(name).unwrap());
            assert_eq!(
                egt.reflected_position_to_index(&position),
                egt.position_to_index(&reflected),
                "{fen}"
            );
        }
    }

    #[test]
    fn diagonal_symmetry_excludes_pawnful_tables() {
        let egt = Egt::new(&TableId::parse("KPd_K").unwrap());
        assert_eq!(egt.diagonal_symmetric(0), None);
    }

    #[test]
    fn decode_encode_round_trips() {
        for name in [
            "KPa_K",
            "KB_K",
            "KQ_KR",
            "KPdPe_KPePePe",
            "KPdPePf_KPePe",
            "KPePe_KPdPf",
        ] {
            let egt = Egt::new(&parse_oriented(name).unwrap());
            let range = egt.index_range();
            let stride = if egt.n_pieces() <= 3 {
                1
            } else {
                (range / 8192).max(1)
            };
            for turn in Color::ALL {
                let mut accepted = 0;
                for index in (0..range).step_by(stride).chain(std::iter::once(range - 1)) {
                    if let Ok(position) = egt.position_from_index(index, turn) {
                        assert_eq!(egt.position_to_index(&position), index, "{name}, {turn:?}");
                        accepted += 1;
                    }
                }
                assert!(accepted > 0, "{name}, {turn:?}");
            }
        }
    }
}
