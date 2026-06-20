use shakmaty::{File, Role};
use std::cmp::Ordering;

pub static ALL_EGT_ROLES: [EgtRole; 13] = [
    EgtRole::Pawn(File::A),
    EgtRole::Pawn(File::B),
    EgtRole::Pawn(File::C),
    EgtRole::Pawn(File::D),
    EgtRole::Pawn(File::E),
    EgtRole::Pawn(File::F),
    EgtRole::Pawn(File::G),
    EgtRole::Pawn(File::H),
    EgtRole::King,
    EgtRole::Queen,
    EgtRole::Rook,
    EgtRole::Bishop,
    EgtRole::Knight,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd)]
pub enum EgtRole {
    Pawn(File),
    King,
    Queen,
    Rook,
    Bishop,
    Knight,
}

impl EgtRole {
    pub fn is_pawn(&self) -> bool {
        match self {
            EgtRole::Pawn(_) => true,
            _ => false,
        }
    }

    pub fn to_role(&self) -> Role {
        match self {
            EgtRole::Pawn(_) => Role::Pawn,
            EgtRole::King => Role::King,
            EgtRole::Queen => Role::Queen,
            EgtRole::Rook => Role::Rook,
            EgtRole::Bishop => Role::Bishop,
            EgtRole::Knight => Role::Knight,
        }
    }

    pub fn to_index(&self) -> usize {
        match self {
            EgtRole::Pawn(File::A) => 0,
            EgtRole::Pawn(File::B) => 1,
            EgtRole::Pawn(File::C) => 2,
            EgtRole::Pawn(File::D) => 3,
            EgtRole::Pawn(File::E) => 4,
            EgtRole::Pawn(File::F) => 5,
            EgtRole::Pawn(File::G) => 6,
            EgtRole::Pawn(File::H) => 7,
            EgtRole::King => 8,
            EgtRole::Queen => 9,
            EgtRole::Rook => 10,
            EgtRole::Bishop => 11,
            EgtRole::Knight => 12,
        }
    }
}

impl Ord for EgtRole {
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
