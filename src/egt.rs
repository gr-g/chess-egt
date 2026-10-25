use shakmaty::{Chess, Color, File};
mod indexer;
use crate::error::{EgtError, EgtResult};
use crate::piece_set::{ALL_EGT_ROLES, EgtElement, EgtRole, EgtSide};
use indexer::Indexer;

#[derive(Clone, Debug)]
pub struct Egt {
    /// Canonically sorted pawn-file sequences and nonpawn material groups.
    pub elements: Vec<EgtElement>,

    // The helper object storing the piece set and the information required
    // to convert positions to table indexes.
    pub indexer: Indexer,

    // The name of this table (e.g. "KQ_KPa").
    pub tablename: String,
}

impl Egt {
    /// The pawn-file sequences and nonpawn material groups in this table.
    pub fn elements(&self) -> &[EgtElement] {
        &self.elements
    }

    // The number of pawns.
    pub fn n_pawns(&self) -> usize {
        self.indexer.n_pawns
    }

    // Whether this a pawnless endgame.
    pub fn is_pawnless(&self) -> bool {
        self.indexer.n_pawns == 0
    }

    // The total number of positions indexed in this table.
    pub fn index_range(&self) -> usize {
        self.indexer.index_range
    }

    // The name of this table (e.g. "KQ_KPa").
    pub fn tablename(&self) -> &str {
        &self.tablename
    }

    /// Builds a table from pawn-file sequences and nonpawn material counts.
    /// Elements are sorted canonically and validated by the indexer.
    pub fn from_elements(mut elements: Vec<EgtElement>) -> EgtResult<Self> {
        elements.sort();
        let indexer = Indexer::from_elements(&elements)?;
        let tablename = compute_tablename(&elements);
        Ok(Egt {
            elements,
            indexer,
            tablename,
        })
    }

    /// Parses canonical names: each side has `K`, then `Q`, `R`, `B`, `N`,
    /// then pawns by ascending file. A pawn token is `Pa` through `Ph`, with
    /// an optional trailing `o` when the next pawn towards promotion is enemy.
    /// Within a file, STM tokens follow ascending normalized rank and SNTM
    /// tokens descending rank. Noncanonical piece order is rejected.
    pub fn from_tablename(tablename: &str) -> EgtResult<Self> {
        let invalid = |reason| EgtError::InvalidEndgameName {
            name: tablename.to_owned(),
            reason,
        };
        let (stm, sntm) = tablename
            .split_once('_')
            .ok_or_else(|| invalid("missing '_' separator"))?;
        let mut elements = Vec::new();
        let mut flags: [[Vec<bool>; 8]; 2] =
            std::array::from_fn(|_| std::array::from_fn(|_| Vec::new()));

        for (side_idx, (text, side)) in [(stm, EgtSide::SideToMove), (sntm, EgtSide::SideNotToMove)]
            .into_iter()
            .enumerate()
        {
            let mut counts = [0usize; ALL_EGT_ROLES.len()];
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                let role = match c {
                    'K' => EgtRole::King,
                    'Q' => EgtRole::Queen,
                    'R' => EgtRole::Rook,
                    'B' => EgtRole::Bishop,
                    'N' => EgtRole::Knight,
                    'P' => {
                        let file = match chars.next() {
                            Some(c @ 'a'..='h') => File::new(c as u32 - 'a' as u32),
                            _ => return Err(invalid("invalid pawn file specifier")),
                        };
                        let opposed = chars.next_if_eq(&'o').is_some();
                        flags[side_idx][file.to_usize()].push(opposed);
                        EgtRole::Pawn(file)
                    }
                    _ => return Err(invalid("invalid piece character")),
                };
                counts[role.to_index()] += 1;
            }
            if counts[EgtRole::King.to_index()] != 1 {
                return Err(invalid("each side must have exactly one king"));
            }
            for role in ALL_EGT_ROLES {
                if !role.is_pawn() && counts[role.to_index()] != 0 {
                    elements.push(EgtElement::PieceGroup {
                        role,
                        side,
                        multiplicity: counts[role.to_index()],
                    });
                }
            }
        }

        for file_idx in 0..8 {
            let stm_flags = &flags[0][file_idx];
            let sntm_flags = &flags[1][file_idx];
            let len = stm_flags.len() + sntm_flags.len();
            if len == 0 {
                continue;
            }
            if len > 6 {
                return Err(invalid("at most six pawns may occupy a file"));
            }
            // There are at most 64 color words on a file. Both sides' ordered
            // flags together identify the full word, including unopposed pawns.
            let order = (0usize..1 << len)
                .find_map(|word| {
                    if word.count_ones() as usize != sntm_flags.len() {
                        return None;
                    }
                    let order: arrayvec::ArrayVec<EgtSide, 6> = (0..len)
                        .map(|i| {
                            if word & (1 << i) == 0 {
                                EgtSide::SideToMove
                            } else {
                                EgtSide::SideNotToMove
                            }
                        })
                        .collect();
                    (pawn_flags(&order, EgtSide::SideToMove) == *stm_flags
                        && pawn_flags(&order, EgtSide::SideNotToMove) == *sntm_flags)
                        .then_some(order)
                })
                .ok_or_else(|| invalid("inconsistent opposed-pawn flags"))?;
            elements.push(EgtElement::PawnGroup {
                file: File::new(file_idx as u32),
                order,
            });
        }
        let egt = Self::from_elements(elements).map_err(|error| match error {
            EgtError::InvalidPieceConfig(reason) => invalid(reason),
            other => other,
        })?;
        if egt.tablename != tablename {
            return Err(invalid(
                "noncanonical piece order; expected KQRBN then pawns by file",
            ));
        }
        Ok(egt)
    }

    // Encodes the position into an index, which represents the position up to symmetries.
    pub fn position_to_index(&self, position: &Chess) -> usize {
        self.indexer.position_to_index(position)
    }

    /// Encodes the horizontal reflection of a no-castling position.
    /// The reflected position must match this table's piece set and pawn files.
    pub(crate) fn reflected_position_to_index(&self, position: &Chess) -> usize {
        self.indexer.reflected_position_to_index(position)
    }

    // Decodes an index and recreates the corresponding position for this endgame (up to symmetries).
    // If the index represents an invalid position, returns None.
    pub fn position_from_index(&self, index: usize, side_to_move: Color) -> Option<Chess> {
        self.indexer.position_from_index(index, side_to_move)
    }

    // Pawnless positions are reduced to a canonical form through rotations and reflections
    // to take advantage of symmetries, so normally each index corresponds to 8 equivalent
    // position. However, if both kings are on a long diagonal, the diagonal symmetry is not
    // used and the correspoding index represents only 4 equivalent positions.
    // In particular, positions with the kings on the diagonal and some other piece outside
    // the diagonal have a symmetric that is in principle equivalent, but is assigned a
    // separate index.
    // `diagonal_symmetric(idx)` returns:
    //  - `None` if the corresponding position does not have both kings on a long diagonal
    //    (i.e. `idx` represents 8 equivalent positions)
    //  - `Some(other_idx)` if the corresponding position has both kings on a long diagonal
    //    (i.e. `idx` represents 4 equivalent positions), with `other_idx` being the index
    //    of the symmetrical position along the diagonal. `other_idx` will be equal to `idx`
    //    for positions with all pieces on the diagonal.
    pub fn diagonal_symmetric(&self, index: usize) -> Option<usize> {
        self.indexer.diagonal_symmetric(index)
    }
}

/// Whether each pawn faces an enemy in the next occupied slot towards promotion.
/// The result follows that side's traversal order, not board-square order.
fn pawn_flags(order: &[EgtSide], side: EgtSide) -> Vec<bool> {
    let mut flags = Vec::new();
    for (i, &pawn_side) in order.iter().enumerate() {
        if pawn_side == side {
            let neighbor = match side {
                EgtSide::SideToMove => order.get(i + 1),
                EgtSide::SideNotToMove => i.checked_sub(1).and_then(|j| order.get(j)),
            };
            flags.push(neighbor.is_some_and(|&other| other != side));
        }
    }
    if side == EgtSide::SideNotToMove {
        flags.reverse();
    }
    flags
}

fn compute_tablename(elements: &[EgtElement]) -> String {
    let mut sides = Vec::new();
    for side in [EgtSide::SideToMove, EgtSide::SideNotToMove] {
        let mut name = String::from("K");
        for (role, symbol) in [
            (EgtRole::Queen, 'Q'),
            (EgtRole::Rook, 'R'),
            (EgtRole::Bishop, 'B'),
            (EgtRole::Knight, 'N'),
        ] {
            for element in elements {
                if let EgtElement::PieceGroup {
                    role: piece_role,
                    side: piece_side,
                    multiplicity,
                } = element
                    && *piece_role == role
                    && *piece_side == side
                {
                    name.extend(std::iter::repeat_n(symbol, *multiplicity));
                }
            }
        }
        for element in elements {
            let EgtElement::PawnGroup { file, order } = element else {
                continue;
            };
            for opposed in pawn_flags(order, side) {
                name.push('P');
                name.push((b'a' + file.to_usize() as u8) as char);
                if opposed {
                    name.push('o');
                }
            }
        }
        sides.push(name);
    }
    sides.join("_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use EgtSide::{SideNotToMove as Sntm, SideToMove as Stm};

    fn pawn_group(file: File, order: &[EgtSide]) -> EgtElement {
        EgtElement::PawnGroup {
            file,
            order: order.iter().copied().collect(),
        }
    }

    fn piece_group(role: EgtRole, side: EgtSide, multiplicity: usize) -> EgtElement {
        EgtElement::PieceGroup {
            role,
            side,
            multiplicity,
        }
    }

    fn kings() -> Vec<EgtElement> {
        vec![
            piece_group(EgtRole::King, Stm, 1),
            piece_group(EgtRole::King, Sntm, 1),
        ]
    }

    fn table_with_order(order: &[EgtSide]) -> Egt {
        let mut elements = kings();
        elements.push(pawn_group(File::A, order));
        Egt::from_elements(elements).unwrap()
    }

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
            let egt = table_with_order(&order);
            assert_eq!(egt.tablename(), expected, "{order:?}");
            assert_eq!(
                Egt::from_tablename(expected).unwrap().elements,
                egt.elements
            );
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
                let egt = table_with_order(&order);
                assert!(
                    names.insert(egt.tablename.clone()),
                    "duplicate name for {order:?}"
                );
                let parsed = Egt::from_tablename(egt.tablename()).unwrap();
                assert_eq!(parsed.elements, egt.elements);
            }
        }
    }

    #[test]
    fn naming_sorts_files_and_preserves_nonpawn_names() {
        let mut elements = kings();
        elements.extend([
            pawn_group(File::H, &[Stm]),
            piece_group(EgtRole::Knight, Stm, 1),
            pawn_group(File::B, &[Stm, Sntm]),
            piece_group(EgtRole::Queen, Stm, 1),
        ]);
        let egt = Egt::from_elements(elements).unwrap();
        assert_eq!(egt.tablename(), "KQNPboPh_KPbo");
        assert_eq!(
            egt.elements(),
            &[
                pawn_group(File::B, &[Stm, Sntm]),
                pawn_group(File::H, &[Stm]),
                piece_group(EgtRole::King, Stm, 1),
                piece_group(EgtRole::King, Sntm, 1),
                piece_group(EgtRole::Queen, Stm, 1),
                piece_group(EgtRole::Knight, Stm, 1),
            ]
        );
        for name in ["K_K", "KQQ_K", "KQRBN_KR", "KPaPa_KPh"] {
            assert_eq!(Egt::from_tablename(name).unwrap().tablename(), name);
        }
    }

    #[test]
    fn naming_mixed_file_without_flags_is_explicitly_unopposed() {
        let egt = Egt::from_tablename("KPdPe_KPePePe").unwrap();
        assert_eq!(
            egt.elements(),
            &[
                pawn_group(File::D, &[Stm]),
                pawn_group(File::E, &[Sntm, Sntm, Sntm, Stm]),
                piece_group(EgtRole::King, Stm, 1),
                piece_group(EgtRole::King, Sntm, 1),
            ]
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
                    Egt::from_tablename(name),
                    Err(EgtError::InvalidEndgameName { .. })
                ),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn naming_rejects_invalid_elements() {
        for extra in [
            vec![pawn_group(File::A, &[])],
            vec![pawn_group(File::A, &[Stm]), pawn_group(File::A, &[Sntm])],
            vec![piece_group(EgtRole::Pawn(File::A), Stm, 1)],
            vec![piece_group(EgtRole::Queen, Stm, 0)],
            vec![
                piece_group(EgtRole::Queen, Stm, 1),
                piece_group(EgtRole::Queen, Stm, 2),
            ],
            vec![piece_group(EgtRole::King, Stm, 1)],
            vec![piece_group(EgtRole::Queen, Stm, 7)],
            vec![pawn_group(File::A, &[Stm; 6]), pawn_group(File::B, &[Sntm])],
            vec![piece_group(EgtRole::Queen, Stm, usize::MAX)],
        ] {
            let mut elements = kings();
            elements.extend(extra);
            assert!(matches!(
                Egt::from_elements(elements),
                Err(EgtError::InvalidPieceConfig(_))
            ));
        }
        for elements in [
            vec![],
            vec![piece_group(EgtRole::King, Stm, 1)],
            vec![piece_group(EgtRole::King, Sntm, 1)],
            vec![
                piece_group(EgtRole::King, Stm, 2),
                piece_group(EgtRole::King, Sntm, 1),
            ],
            vec![
                piece_group(EgtRole::King, Stm, 1),
                piece_group(EgtRole::King, Sntm, 2),
            ],
        ] {
            assert!(matches!(
                Egt::from_elements(elements),
                Err(EgtError::InvalidPieceConfig(_))
            ));
        }
    }

    #[test]
    fn elements_count_physical_pieces() {
        assert_eq!(pawn_group(File::A, &[Stm, Sntm, Stm]).multiplicity(), 3);
        assert_eq!(piece_group(EgtRole::Queen, Stm, 2).multiplicity(), 2);
        let mut elements = kings();
        elements.push(piece_group(EgtRole::Knight, Stm, 6));
        let egt = Egt::from_elements(elements).unwrap();
        assert_eq!(
            egt.elements()
                .iter()
                .map(EgtElement::multiplicity)
                .sum::<usize>(),
            8
        );
        assert_eq!(egt.tablename(), "KNNNNNN_K");
    }

    fn assert_round_trip(egt: &Egt, i: usize, side_to_move: Color) {
        if let Some(position) = egt.indexer.position_from_index(i, side_to_move) {
            assert_eq!(i, egt.indexer.position_to_index(&position));
        }
    }

    #[test]
    fn reflected_index_matches_board_reflection() {
        use crate::egt_file::mirror_horizontally;

        for name in ["KPa_K", "KPd_KPe", "KPaPaPd_KPbPf", "KNN_K"] {
            let egt = Egt::from_tablename(name).unwrap();
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
            let egt = Egt::from_tablename(name).unwrap();
            assert_eq!(
                egt.reflected_position_to_index(&position),
                egt.position_to_index(&reflected),
                "{fen}"
            );
        }
    }

    #[test]
    fn test_diagonal_symmetric() {
        let egt = Egt::from_tablename("KR_K").unwrap();

        for i in [10, 100, 2100] {
            println!("{:?}", egt.indexer.position_from_index(i, Color::White));
            assert_eq!(egt.diagonal_symmetric(i), None);
        }

        println!("{:?}", egt.indexer.position_from_index(773, Color::White));
        println!("{:?}", egt.indexer.position_from_index(801, Color::White));
        assert_eq!(egt.diagonal_symmetric(773), Some(801));
        assert_eq!(egt.diagonal_symmetric(801), Some(773));

        println!("{:?}", egt.indexer.position_from_index(805, Color::White));
        assert_eq!(egt.diagonal_symmetric(805), Some(805));

        let egt_p = Egt::from_tablename("KPd_K").unwrap();
        println!("{:?}", egt_p.indexer.position_from_index(0, Color::White));
        assert_eq!(egt_p.diagonal_symmetric(0), None);
    }

    #[test]
    fn decode_encode_kpa_k() {
        let egt = Egt::from_tablename("KPa_K").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kb_k() {
        let egt = Egt::from_tablename("KB_K").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kqr_kqr() {
        let egt = Egt::from_tablename("KQ_KR").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kqqq_k() {
        let egt = Egt::from_tablename("KQQQ_K").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpdpe_kpepepe() {
        let egt = Egt::from_tablename("KPdPe_KPePePe").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpdpepf_kpepe() {
        let egt = Egt::from_tablename("KPdPePf_KPePe").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpepe_kpdpf() {
        let egt = Egt::from_tablename("KPePe_KPdPf").unwrap();

        for i in 0..egt.index_range() {
            if i == 1911601 {
                println!("{:?}", egt.indexer.position_from_index(i, Color::White));
                println!("{:?}", egt.indexer.position_from_index(i, Color::Black));
            }
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }
}
