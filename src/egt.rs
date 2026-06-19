use shakmaty::{Color, File, Setup};
mod indexer;
use crate::piece_set::{EgtPiece, EgtSide};
use indexer::{Indexer, IndexerScratch};

#[derive(Clone, Debug)]
pub struct Egt {
    // The pieces appearing in this endgame table with their multiplicity.
    pub pieces: Vec<(EgtPiece, EgtSide, usize)>,

    // The helper object storing the piece set and the information required
    // to convert positions to tablebase indexes.
    pub indexer: Indexer,

    // A mutable scratch buffer used to encode/decode positions.
    pub scratch: IndexerScratch,

    // The precomputed name of this endgame table (e.g. "KQ_KPa").
    pub tablename: String,
}

impl Egt {
    // The set of pieces appearing in this endgame table.
    pub fn pieces(&self) -> &[(EgtPiece, EgtSide, usize)] {
        &self.pieces
    }

    // The number of pawns in the endgame.
    pub fn n_pawns(&self) -> usize {
        self.indexer.n_pawns
    }

    // Whether this a pawnless endgame.
    pub fn is_pawnless(&self) -> bool {
        self.n_pawns() == 0
    }

    // The total number of positions indexed in this endgame table.
    pub fn index_range(&self) -> usize {
        self.indexer.index_range
    }

    // The name of this endgame table (e.g. "KQ_KPa").
    pub fn tablename(&self) -> &str {
        &self.tablename
    }

    // Setup an endgame table from a set of pieces.
    pub fn from_pieces(mut pieces: Vec<(EgtPiece, EgtSide, usize)>) -> Result<Self, ()> {
        // Sort: first the pawns, then the kings, then all other pieces.
        pieces.sort();

        let tablename = compute_tablename(&pieces);
        let indexer = Indexer::from_pieces(&pieces)?;
        let scratch = indexer.create_scratch();
        Ok(Egt {
            pieces,
            indexer,
            scratch,
            tablename,
        })
    }

    pub fn board_to_index(&mut self, board: &Setup) -> usize {
        self.indexer.board_to_index(&mut self.scratch, board)
    }

    pub fn board_from_index(&mut self, index: usize, side_to_move: Color) -> Option<Setup> {
        self.indexer
            .board_from_index(&mut self.scratch, index, side_to_move)
    }
}

fn compute_tablename(pieces: &[(EgtPiece, EgtSide, usize)]) -> String {
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
            if p_side == *side {
                if let EgtPiece::Queen = piece {
                    for _ in 0..mult {
                        parts.push("Q".to_string());
                    }
                }
            }
        }
        // Rook
        for &(piece, p_side, mult) in pieces {
            if p_side == *side {
                if let EgtPiece::Rook = piece {
                    for _ in 0..mult {
                        parts.push("R".to_string());
                    }
                }
            }
        }
        // Bishop
        for &(piece, p_side, mult) in pieces {
            if p_side == *side {
                if let EgtPiece::Bishop = piece {
                    for _ in 0..mult {
                        parts.push("B".to_string());
                    }
                }
            }
        }
        // Knight
        for &(piece, p_side, mult) in pieces {
            if p_side == *side {
                if let EgtPiece::Knight = piece {
                    for _ in 0..mult {
                        parts.push("N".to_string());
                    }
                }
            }
        }
        // Pawn (with file)
        let mut pawns = Vec::new();
        for &(piece, p_side, mult) in pieces {
            if p_side == *side {
                if let EgtPiece::Pawn(file) = piece {
                    pawns.push((file, mult));
                }
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

    // Setup an endgame table from a string such as "KQ_KRPb"
    fn from_tablename(tablename: &str) -> Result<Egt, ()> {
        let (stm, sntm) = tablename.split_once('_').ok_or(())?;

        let mut pieces = vec![];

        for (s, side) in [(stm, EgtSide::SideToMove), (sntm, EgtSide::SideNotToMove)] {
            let mut count = [0; crate::piece_set::ALL_EGT_PIECES.len()];
            let mut it = s.chars();
            while let Some(c) = it.next() {
                let piece = match c {
                    'P' => match it.next() {
                        Some('a') => EgtPiece::Pawn(File::A),
                        Some('b') => EgtPiece::Pawn(File::B),
                        Some('c') => EgtPiece::Pawn(File::C),
                        Some('d') => EgtPiece::Pawn(File::D),
                        Some('e') => EgtPiece::Pawn(File::E),
                        Some('f') => EgtPiece::Pawn(File::F),
                        Some('g') => EgtPiece::Pawn(File::G),
                        Some('h') => EgtPiece::Pawn(File::H),
                        _ => {
                            println!("invalid tablename");
                            return Err(());
                        }
                    },
                    'K' => EgtPiece::King,
                    'Q' => EgtPiece::Queen,
                    'R' => EgtPiece::Rook,
                    'B' => EgtPiece::Bishop,
                    'N' => EgtPiece::Knight,
                    _ => {
                        println!("invalid tablename");
                        return Err(());
                    }
                };
                count[piece.to_index()] += 1;
            }

            if count[EgtPiece::King.to_index()] != 1 {
                println!("missing king");
                return Err(());
            }

            for piece in crate::piece_set::ALL_EGT_PIECES {
                let multiplicity = count[piece.to_index()];

                if multiplicity > 0 {
                    pieces.push((piece, side, multiplicity));
                }
            }
        }

        Egt::from_pieces(pieces)
    }

    fn assert_round_trip(egt: &Egt, i: usize, side_to_move: Color) {
        let mut scratch = egt.indexer.create_scratch();
        if let Some(board) = egt.indexer.board_from_index(&mut scratch, i, side_to_move) {
            assert_eq!(i, egt.indexer.board_to_index(&mut scratch, &board));
        }
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
            assert_round_trip(&egt, i, Color::White);
            assert_round_trip(&egt, i, Color::Black);
        }
    }
}
