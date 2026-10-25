use shakmaty::{File, Role};

use crate::error::{EgtError, EgtResult};

pub(crate) const MAX_PIECES: usize = 8;

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

/// A logical material group. Pawn colors are listed in ascending STM-normalized rank
/// (ascending board rank for White to move, descending for Black to move).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EgtElement {
    PawnGroup {
        file: File,
        order: arrayvec::ArrayVec<EgtSide, 6>,
    },
    PieceGroup {
        role: EgtRole,
        side: EgtSide,
        multiplicity: usize,
    },
}

impl EgtElement {
    /// The number of physical pieces represented by this element.
    pub fn multiplicity(&self) -> usize {
        match self {
            Self::PawnGroup { order, .. } => order.len(),
            Self::PieceGroup { multiplicity, .. } => *multiplicity,
        }
    }
}

/// Validates material independently of the elements' ordering.
pub(crate) fn validate_elements(elements: &[EgtElement]) -> EgtResult<()> {
    let mut seen_files = [false; 8];
    let mut seen_pieces = [[false; 2]; ALL_EGT_ROLES.len()];
    let mut n_pieces = 0usize;
    for element in elements {
        match element {
            EgtElement::PawnGroup { file, order } => {
                if order.is_empty() || seen_files[file.to_usize()] {
                    return Err(EgtError::InvalidPieceConfig(
                        "pawn file groups must be unique and nonempty",
                    ));
                }
                // ArrayVec enforces the maximum of six occupied pawn ranks.
                seen_files[file.to_usize()] = true;
            }
            EgtElement::PieceGroup {
                role,
                side,
                multiplicity,
            } => {
                if role.is_pawn() {
                    return Err(EgtError::InvalidPieceConfig(
                        "pawns must be represented by pawn file groups",
                    ));
                }
                let seen =
                    &mut seen_pieces[role.to_index()][usize::from(*side == EgtSide::SideNotToMove)];
                if *multiplicity == 0 || *seen {
                    return Err(EgtError::InvalidPieceConfig(
                        "piece groups must be unique and nonempty",
                    ));
                }
                if *role == EgtRole::King && *multiplicity != 1 {
                    return Err(EgtError::InvalidPieceConfig(
                        "each side must have exactly one king",
                    ));
                }
                *seen = true;
            }
        }
        n_pieces = n_pieces
            .checked_add(element.multiplicity())
            .ok_or(EgtError::InvalidPieceConfig("piece count overflow"))?;
        if n_pieces > MAX_PIECES {
            return Err(EgtError::InvalidPieceConfig(
                "at most eight physical pieces are supported",
            ));
        }
    }
    if seen_pieces[EgtRole::King.to_index()] != [true, true] {
        return Err(EgtError::InvalidPieceConfig(
            "each side must have exactly one king",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
        matches!(self, EgtRole::Pawn(_))
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
