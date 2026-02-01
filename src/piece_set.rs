use chess::{File, Piece};
use std::cmp::Ordering;

pub static ALL_EGT_PIECES: [EgtPiece; 13] = [
    EgtPiece::Pawn(File::A),
    EgtPiece::Pawn(File::B),
    EgtPiece::Pawn(File::C),
    EgtPiece::Pawn(File::D),
    EgtPiece::Pawn(File::E),
    EgtPiece::Pawn(File::F),
    EgtPiece::Pawn(File::G),
    EgtPiece::Pawn(File::H),
    EgtPiece::King,
    EgtPiece::Queen,
    EgtPiece::Rook,
    EgtPiece::Bishop,
    EgtPiece::Knight,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd)]
pub enum EgtPiece {
    Pawn(File),
    King,
    Queen,
    Rook,
    Bishop,
    Knight,
}

impl EgtPiece {
    pub fn is_pawn(&self) -> bool {
        match self {
            EgtPiece::Pawn(_) => true,
            _ => false,
        }
    }

    pub fn to_piece(&self) -> Piece {
        match self {
            EgtPiece::Pawn(_) => Piece::Pawn,
            EgtPiece::King => Piece::King,
            EgtPiece::Queen => Piece::Queen,
            EgtPiece::Rook => Piece::Rook,
            EgtPiece::Bishop => Piece::Bishop,
            EgtPiece::Knight => Piece::Knight,
        }
    }

    pub fn to_index(&self) -> usize {
        match self {
            EgtPiece::Pawn(File::A) => 0,
            EgtPiece::Pawn(File::B) => 1,
            EgtPiece::Pawn(File::C) => 2,
            EgtPiece::Pawn(File::D) => 3,
            EgtPiece::Pawn(File::E) => 4,
            EgtPiece::Pawn(File::F) => 5,
            EgtPiece::Pawn(File::G) => 6,
            EgtPiece::Pawn(File::H) => 7,
            EgtPiece::King => 8,
            EgtPiece::Queen => 9,
            EgtPiece::Rook => 10,
            EgtPiece::Bishop => 11,
            EgtPiece::Knight => 12,
        }
    }
}

impl Ord for EgtPiece {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(&other).unwrap()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EgtSide {
    SideToMove,
    SideNotToMove,
}

impl EgtSide {
    pub fn reverse(&self) -> Self {
        match self {
            EgtSide::SideToMove => EgtSide::SideNotToMove,
            EgtSide::SideNotToMove => EgtSide::SideToMove,
        }
    }
}
