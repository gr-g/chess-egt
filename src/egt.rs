use shakmaty::{Color, File, Chess};
mod indexer;
use crate::piece_set::{EgtRole, EgtSide};
use crate::error::EgtResult;
use indexer::Indexer;

#[derive(Clone, Debug)]
pub struct Egt {
    // The pieces appearing in this table, with their multiplicity.
    pub pieces: Vec<(EgtRole, EgtSide, usize)>,

    // The helper object storing the piece set and the information required
    // to convert positions to table indexes.
    pub indexer: Indexer,

    // The name of this table (e.g. "KQ_KPa").
    pub tablename: String,
}

impl Egt {
    // The set of pieces appearing in this table, with their multiplicity.
    pub fn pieces(&self) -> &[(EgtRole, EgtSide, usize)] {
        &self.pieces
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

    // Setup an `Egt` from a set of pieces.
    pub fn from_pieces(mut pieces: Vec<(EgtRole, EgtSide, usize)>) -> EgtResult<Self> {
        // Sort: first the pawns, then the kings, then all other pieces.
        pieces.sort();

        let tablename = compute_tablename(&pieces);
        let indexer = Indexer::from_pieces(&pieces)?;
        Ok(Egt {
            pieces,
            indexer,
            tablename,
        })
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

fn compute_tablename(pieces: &[(EgtRole, EgtSide, usize)]) -> String {
    let mut stm_parts = Vec::new();
    let mut sntm_parts = Vec::new();

    // We always have exactly 1 King for each side
    stm_parts.push("K".to_string());
    sntm_parts.push("K".to_string());

    // Let's collect pieces by side
    for side in &[EgtSide::SideToMove, EgtSide::SideNotToMove] {
        let parts = if *side == EgtSide::SideToMove {
            &mut stm_parts
        } else {
            &mut sntm_parts
        };

        // Order: Q, R, B, N, P
        // First, check for Queen
        for &(piece, p_side, mult) in pieces {
            if p_side == *side && let EgtRole::Queen = piece {
                for _ in 0..mult {
                    parts.push("Q".to_string());
                }
            }
        }
        // Rook
        for &(piece, p_side, mult) in pieces {
            if p_side == *side && let EgtRole::Rook = piece {
                for _ in 0..mult {
                    parts.push("R".to_string());
                }
            }
        }
        // Bishop
        for &(piece, p_side, mult) in pieces {
            if p_side == *side && let EgtRole::Bishop = piece {
                for _ in 0..mult {
                    parts.push("B".to_string());
                }
            }
        }
        // Knight
        for &(piece, p_side, mult) in pieces {
            if p_side == *side && let EgtRole::Knight = piece {
                for _ in 0..mult {
                    parts.push("N".to_string());
                }
            }
        }
        // Pawn (with file)
        let mut pawns = Vec::new();
        for &(piece, p_side, mult) in pieces {
            if p_side == *side && let EgtRole::Pawn(file) = piece {
                pawns.push((file, mult));
            }
        }
        pawns.sort_by_key(|(file, _)| file.to_usize());
        for (file, mult) in pawns {
            let file_char = match file {
                File::A => 'a',
                File::B => 'b',
                File::C => 'c',
                File::D => 'd',
                File::E => 'e',
                File::F => 'f',
                File::G => 'g',
                File::H => 'h',
            };
            for _ in 0..mult {
                parts.push(format!("P{}", file_char));
            }
        }
    }

    format!("{}_{}", stm_parts.concat(), sntm_parts.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EgtError;

    // Setup an endgame table from a string such as "KQ_KRPb"
    fn from_tablename(tablename: &str) -> EgtResult<Egt> {
        let (stm, sntm) = tablename.split_once('_').ok_or(EgtError::InvalidEndgameName {
            name: tablename.to_string(),
            reason: "missing '_' separator",
        })?;

        let mut pieces = vec![];

        for (s, side) in [(stm, EgtSide::SideToMove), (sntm, EgtSide::SideNotToMove)] {
            let mut count = [0; crate::piece_set::ALL_EGT_ROLES.len()];
            let mut it = s.chars();
            while let Some(c) = it.next() {
                let piece = match c {
                    'P' => match it.next() {
                        Some('a') => EgtRole::Pawn(File::A),
                        Some('b') => EgtRole::Pawn(File::B),
                        Some('c') => EgtRole::Pawn(File::C),
                        Some('d') => EgtRole::Pawn(File::D),
                        Some('e') => EgtRole::Pawn(File::E),
                        Some('f') => EgtRole::Pawn(File::F),
                        Some('g') => EgtRole::Pawn(File::G),
                        Some('h') => EgtRole::Pawn(File::H),
                        _ => {
                            return Err(EgtError::InvalidEndgameName {
                                name: tablename.to_string(),
                                reason: "invalid pawn file specifier",
                            });
                        }
                    },
                    'K' => EgtRole::King,
                    'Q' => EgtRole::Queen,
                    'R' => EgtRole::Rook,
                    'B' => EgtRole::Bishop,
                    'N' => EgtRole::Knight,
                    _ => {
                        return Err(EgtError::InvalidEndgameName {
                            name: tablename.to_string(),
                            reason: "invalid piece character",
                        });
                    }
                };
                count[piece.to_index()] += 1;
            }

            if count[EgtRole::King.to_index()] != 1 {
                return Err(EgtError::InvalidEndgameName {
                    name: tablename.to_string(),
                    reason: "each side must have exactly one king",
                });
            }

            for piece in crate::piece_set::ALL_EGT_ROLES {
                let multiplicity = count[piece.to_index()];

                if multiplicity > 0 {
                    pieces.push((piece, side, multiplicity));
                }
            }
        }

        Egt::from_pieces(pieces)
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
            let egt = from_tablename(name).unwrap();
            let stride = (egt.index_range() / 256).max(1);
            let mut checked = 0;
            for turn in [Color::White, Color::Black] {
                for idx in (0..egt.index_range()).step_by(stride) {
                    if let Some(position) = egt.position_from_index(idx, turn) {
                        let source = mirror_horizontally(&position).unwrap();
                        let reflected = mirror_horizontally(&source).unwrap();
                        assert_eq!(egt.reflected_position_to_index(&source), egt.position_to_index(&reflected), "{name}, {turn:?}, {idx}");
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
            ("7k/8/8/3Pp3/8/4P3/4P3/K7 w - e6 0 1", "KPdPdPe_KPd"),
            ("7k/4p3/4p3/8/3pP3/8/8/K7 b - e3 0 1", "KPdPdPe_KPd"),
        ] {
            let position: Chess = fen.parse::<Fen>().unwrap()
                .into_position(CastlingMode::Standard).unwrap();
            assert!(position.legal_ep_square().is_some());
            let reflected = mirror_horizontally(&position).unwrap();
            assert_eq!(reflected.legal_ep_square(), position.legal_ep_square().map(|sq| sq.flip_horizontal()));
            let egt = from_tablename(name).unwrap();
            assert_eq!(egt.reflected_position_to_index(&position), egt.position_to_index(&reflected), "{fen}");
        }
    }

    #[test]
    fn test_diagonal_symmetric() {
        let egt = from_tablename("KR_K").unwrap();

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

        let egt_p = from_tablename("KPd_K").unwrap();
        println!("{:?}", egt_p.indexer.position_from_index(0, Color::White));
        assert_eq!(egt_p.diagonal_symmetric(0), None);
    }

    #[test]
    fn decode_encode_kpa_k() {
        let egt = from_tablename("KPa_K").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kb_k() {
        let egt = from_tablename("KB_K").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kqr_kqr() {
        let egt = from_tablename("KQ_KR").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kqqq_k() {
        let egt = from_tablename("KQQQ_K").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpdpe_kpepepe() {
        let egt = from_tablename("KPdPe_KPePePe").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpdpepf_kpepe() {
        let egt = from_tablename("KPdPePf_KPePe").unwrap();

        for i in 0..egt.index_range() {
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }

    #[test]
    fn decode_encode_kpepe_kpdpf() {
        let egt = from_tablename("KPePe_KPdPf").unwrap();

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
