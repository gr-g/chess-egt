use chess::File;
use crate::indexer::Indexer;
use crate::piece_set::{ALL_EGT_PIECES, EgtPiece, EgtSide};

#[derive(Clone, Debug)]
pub struct Egt {
    // The pieces appearing in this endgame table with their multiplicity.
    pub pieces: Vec<(EgtPiece, EgtSide, usize)>,

    // The helper object storing the piece set and the information required
    // to convert positions to tablebase indexes.
    pub indexer: Indexer,
}

impl Egt {
    // The set of pieces appearing in this endgame table.
    pub fn pieces(&self) -> &[(EgtPiece, EgtSide, usize)] {
        &self.pieces
    }

    // The number of pieces in the endgame.
    pub fn n_pieces(&self) -> usize {
        self.indexer.n_pieces
    }

    // The number of unique pieces in the endgame.
    // Non-pawn pieces of the same color and type count as one unique piece.
    // Pawns of the same color on the same file count as one unique piece.
    pub fn n_unique_pieces(&self) -> usize {
        self.indexer.n_unique_pieces
    }

    // The number of pawns in the endgame.
    pub fn n_pawns(&self) -> usize {
        self.indexer.n_pawns
    }

    // The number of unique pawns in the endgame.
    // Pawns of the same color on the same file count as one unique piece.
    pub fn n_unique_pawns(&self) -> usize {
        self.indexer.n_unique_pawns
    }

    // Whether this a pawnless endgame.
    pub fn is_pawnless(&self) -> bool {
        self.n_pawns() == 0
    }

    // The total number of positions indexed in this endgame table.
    pub fn index_range(&self) -> usize {
        self.indexer.index_range
    }

    // Setup an endgame table from a set of pieces.
    pub fn from_pieces(mut pieces: Vec<(EgtPiece, EgtSide, usize)>) -> Result<Self, ()> {
        // Sort: first the pawns, then the kings, then all other pieces.
        pieces.sort();

        let indexer = Indexer::from_pieces(&pieces)?;
        Ok(Egt { pieces, indexer })
    }

    // Setup an endgame table from a string such as "KQ_KRPb"
    pub fn from_tablename(tablename: &str) -> Result<Self, ()> {
        let (stm, sntm) = tablename.split_once('_').ok_or(())?;

        let mut pieces = vec![];

        for (s, side) in [(stm, EgtSide::SideToMove), (sntm, EgtSide::SideNotToMove)] {
            let mut count = [0; ALL_EGT_PIECES.len()];
            let mut it = s.chars();
            while let Some(c) = it.next() {
                let piece = match c {
                    'P' => {
                        match it.next() {
                            Some('a') => EgtPiece::Pawn(File::A),
                            Some('b') => EgtPiece::Pawn(File::B),
                            Some('c') => EgtPiece::Pawn(File::C),
                            Some('d') => EgtPiece::Pawn(File::D),
                            Some('e') => EgtPiece::Pawn(File::E),
                            Some('f') => EgtPiece::Pawn(File::F),
                            Some('g') => EgtPiece::Pawn(File::G),
                            Some('h') => EgtPiece::Pawn(File::H),
                            _ => { println!("invalid tablename"); return Err(()); },
                        }
                    }
                    'K' => EgtPiece::King,
                    'Q' => EgtPiece::Queen,
                    'R' => EgtPiece::Rook,
                    'B' => EgtPiece::Bishop,
                    'N' => EgtPiece::Knight,
                    _ => { println!("invalid tablename"); return Err(()); },
                };
                count[piece.to_index()] += 1;
            }

            if count[EgtPiece::King.to_index()] != 1 {
                println!("missing king");
                return Err(());
            }

            for piece in ALL_EGT_PIECES {
                let multiplicity = count[piece.to_index()];

                if multiplicity > 0 {
                    pieces.push((piece, side, multiplicity));
                }
            }
        }

        Egt::from_pieces(pieces)
    }
}
