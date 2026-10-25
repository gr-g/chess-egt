use std::cmp::{Ordering, Reverse};
use shakmaty::{ByRole, File, Role};

use crate::error::{EgtError, EgtResult};

pub(crate) const MAX_PIECES: usize = 8;


/// A logical material group. Pawn colors are listed in ascending STM-normalized rank
/// (ascending board rank for White to move, descending for Black to move).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgtElement {
    PawnGroup {
        file: File,
        order: arrayvec::ArrayVec<EgtSide, 6>,
    },
    PieceGroup {
        role: Role,
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

impl Ord for EgtElement {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::PawnGroup { file: a, order: x }, Self::PawnGroup { file: b, order: y }) =>
                (a, x).cmp(&(b, y)),
            (Self::PawnGroup { .. }, Self::PieceGroup { .. }) => Ordering::Less,
            (Self::PieceGroup { .. }, Self::PawnGroup { .. }) => Ordering::Greater,
            // Preserve the indexing order KQRBN, opposite to shakmaty's role order.
            (Self::PieceGroup { role: a, side: x, multiplicity: m },
             Self::PieceGroup { role: b, side: y, multiplicity: n }) =>
                (Reverse(a), x, m).cmp(&(Reverse(b), y, n)),
        }
    }
}

impl PartialOrd for EgtElement {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Validates material independently of the elements' ordering.
pub(crate) fn validate_elements(elements: &[EgtElement]) -> EgtResult<()> {
    let mut seen_files = [false; 8];
    let mut seen_pieces = ByRole::new_with(|_| [false; 2]);
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
                if *role == Role::Pawn {
                    return Err(EgtError::InvalidPieceConfig(
                        "pawns must be represented by pawn file groups",
                    ));
                }
                let seen =
                    &mut seen_pieces[*role][usize::from(*side == EgtSide::SideNotToMove)];
                if *multiplicity == 0 || *seen {
                    return Err(EgtError::InvalidPieceConfig(
                        "piece groups must be unique and nonempty",
                    ));
                }
                if *role == Role::King && *multiplicity != 1 {
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
    if seen_pieces.king != [true, true] {
        return Err(EgtError::InvalidPieceConfig(
            "each side must have exactly one king",
        ));
    }
    Ok(())
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
