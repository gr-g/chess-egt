use shakmaty::{Chess, Color};
use crate::TableId;
mod indexer;
use crate::error::EgtResult;
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
    pub fn new(id: &TableId) -> EgtResult<Self> {
        Ok(Self {
            id: *id,
            indexer: Indexer::new(id),
        })
    }

    /// Encodes a no-castling position matching this table's material, pawn files,
    /// and same-file pawn color order.
    /// Callers must establish membership before using this trusted inner path.
    pub(crate) fn position_to_index(&self, position: &Chess) -> usize {
        self.indexer.position_to_index(position)
    }

    /// Encodes the horizontal reflection of a no-castling position.
    /// The reflected position must match this table's material, pawn files,
    /// and same-file pawn color order.
    pub(crate) fn reflected_position_to_index(&self, position: &Chess) -> usize {
        self.indexer.reflected_position_to_index(position)
    }

    /// Decodes an index into a symmetry-reduced position with the requested side to move.
    /// Returns `None` for out-of-range indices or invalid positions.
    /// Castling rights are absent; en passant rights are retained only when legal.
    /// Impossible checks are accepted, as in table generation.
    pub fn position_from_index(&self, index: usize, side_to_move: Color) -> Option<Chess> {
        self.indexer.position_from_index(index, side_to_move)
    }

    /// Decodes an in-range index, retaining its rejection reason for initialization.
    pub(crate) fn position_from_index_with_reason(&self, index: usize, side_to_move: Color) -> EgtResult<Result<Chess, crate::position::InvalidPosition>> {
        self.indexer.position_from_index_with_reason(index, side_to_move)
    }

    /// Decodes an index already accepted by `position_from_index` for this table and turn.
    /// Skips all position validation, including legal en passant. Panics if out of range.
    pub(crate) fn position_from_index_unchecked(&self, index: usize, side_to_move: Color) -> Chess {
        self.indexer.position_from_index_unchecked(index, side_to_move)
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
    use crate::table_id::{EgtSide, parse_oriented, test_support::{pawn_group, with_pawns}};
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
        let id = with_pawns("KQN_K", &[pawn_group(File::H, &[Stm]), pawn_group(File::B, &[Stm, Sntm])]);
        assert_eq!(id.name().as_str(), "KQNPboPh_KPbo");
        for name in ["K_K", "KQQ_K", "KQRBN_KR", "KPaPa_KPh"] {
            assert_eq!(TableId::parse(name).unwrap().name().as_str(), name);
        }
    }

    #[test]
    fn naming_mixed_file_without_flags_is_explicitly_unopposed() {
        let id = parse_oriented("KPdPe_KPePePe").unwrap();
        assert_eq!(id, with_pawns("K_K", &[
            pawn_group(File::D, &[Stm]),
            pawn_group(File::E, &[Sntm, Sntm, Sntm, Stm]),
        ]));
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
    fn table_counts_physical_pieces() {
        let id = TableId::parse("KQQPaoPa_KPao").unwrap();
        let egt = Egt::new(&id).unwrap();
        assert_eq!(egt.n_pieces(), 7);
        assert_eq!(egt.n_pawns(), 3);
        let egt = Egt::new(&TableId::parse("KNNNNNN_K").unwrap()).unwrap();
        assert_eq!(egt.n_pieces(), 8);
        assert_eq!(egt.id().name().as_str(), "KNNNNNN_K");
    }

    #[test]
    fn table_specific_block_sizes() {
        for (name, pieces, summary, frame) in [
            ("K_K", 2, 4032, 524160),
            ("KQ_K", 3, 3906, 499968),
            ("KQ_KR", 4, 3782, 476532),
            ("KBBB_K", 5, 3660, 453840),
            ("KPaPa_KPb", 5, 3660, 453840),
            ("KNNNNNN_K", 8, 3306, 390108),
        ] {
            let egt = Egt::new(&TableId::parse(name).unwrap()).unwrap();
            assert_eq!(egt.n_pieces(), pieces);
            assert_eq!(egt.summary_block_size(), summary);
            assert_eq!(egt.frame_size(), frame);
        }
    }

    #[test]
    fn known_valid_decoder_matches_public_decoder() {
        for name in ["K_K", "KQ_K", "KPa_K", "KPd_KPe"] {
            let egt = Egt::new(&TableId::parse(name).unwrap()).unwrap();
            for turn in [Color::White, Color::Black] {
                let mut accepted = 0;
                for index in (0..egt.index_range()).step_by((egt.index_range() / 256).max(1)) {
                    let decoded = egt.position_from_index(index, turn);
                    assert_eq!(egt.position_from_index_with_reason(index, turn).unwrap().ok(), decoded);
                    if let Some(position) = decoded {
                        assert_eq!(egt.position_from_index_unchecked(index, turn), position);
                        accepted += 1;
                    }
                }
                assert!(accepted > 0, "{name}, {turn:?}");
                assert_eq!(egt.position_from_index(egt.index_range(), turn), None);
                assert_eq!(egt.position_from_index(usize::MAX, turn), None);
            }
        }
    }

    #[test]
    #[should_panic(expected = "position index out of range")]
    fn known_valid_decoder_enforces_index_range() {
        let egt = Egt::new(&TableId::parse("K_K").unwrap()).unwrap();
        egt.position_from_index_unchecked(egt.index_range(), Color::White);
    }

    #[test]
    fn reflected_index_matches_board_reflection() {
        use crate::egt_file::mirror_horizontally;

        for name in ["KPa_K", "KPd_KPe", "KPaPaPd_KPbPf", "KNN_K"] {
            let egt = Egt::new(&TableId::parse(name).unwrap()).unwrap();
            let stride = (egt.index_range() / 256).max(1);
            let mut checked = 0;
            for turn in [Color::White, Color::Black] {
                for idx in (0..egt.index_range()).step_by(stride) {
                    if let Some(position) = egt.position_from_index(idx, turn) {
                        let source = mirror_horizontally(&position).unwrap();
                        let reflected = mirror_horizontally(&source).unwrap();
                        assert_eq!(
                            egt.reflected_position_to_index(&source),
                            egt.position_to_index(&reflected),
                            "{name}, {turn:?}, {idx}"
                        );
                        assert_eq!(egt.reflected_position_to_index(&source), idx);
                        checked += 1;
                    }
                }
            }
            assert!(checked > 0, "no valid samples for {name}");
        }
    }

    #[test]
    fn reflected_index_preserves_legal_en_passant() {
        use crate::egt_file::mirror_horizontally;
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
            let reflected = mirror_horizontally(&position).unwrap();
            assert_eq!(
                reflected.legal_ep_square(),
                position.legal_ep_square().map(|sq| sq.flip_horizontal())
            );
            let egt = Egt::new(&parse_oriented(name).unwrap()).unwrap();
            assert_eq!(
                egt.reflected_position_to_index(&position),
                egt.position_to_index(&reflected),
                "{fen}"
            );
        }
    }

    #[test]
    fn test_diagonal_symmetric() {
        let egt = Egt::new(&TableId::parse("KR_K").unwrap()).unwrap();

        for i in [10, 100, 2100] {
            println!("{:?}", egt.position_from_index(i, Color::White));
            assert_eq!(egt.diagonal_symmetric(i), None);
        }

        println!("{:?}", egt.position_from_index(773, Color::White));
        println!("{:?}", egt.position_from_index(801, Color::White));
        assert_eq!(egt.diagonal_symmetric(773), Some(801));
        assert_eq!(egt.diagonal_symmetric(801), Some(773));

        println!("{:?}", egt.position_from_index(805, Color::White));
        assert_eq!(egt.diagonal_symmetric(805), Some(805));

        let egt_p = Egt::new(&TableId::parse("KPd_K").unwrap()).unwrap();
        println!("{:?}", egt_p.position_from_index(0, Color::White));
        assert_eq!(egt_p.diagonal_symmetric(0), None);
    }

    fn assert_round_trip(egt: &Egt, i: usize, side_to_move: Color) {
        if let Some(position) = egt.position_from_index(i, side_to_move) {
            assert_eq!(i, egt.position_to_index(&position));
        }
    }

    #[test]
    fn decode_encode_kpa_k() {
        let egt = Egt::new(&TableId::parse("KPa_K").unwrap()).unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kb_k() {
        let egt = Egt::new(&TableId::parse("KB_K").unwrap()).unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kqr_kqr() {
        let egt = Egt::new(&TableId::parse("KQ_KR").unwrap()).unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpdpe_kpepepe() {
        let egt = Egt::new(&parse_oriented("KPdPe_KPePePe").unwrap()).unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpdpepf_kpepe() {
        let egt = Egt::new(&parse_oriented("KPdPePf_KPePe").unwrap()).unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpepe_kpdpf() {
        let egt = Egt::new(&parse_oriented("KPePe_KPdPf").unwrap()).unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }
}
