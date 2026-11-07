use std::cmp::Ordering;

use shakmaty::Move;

use crate::{EgtError, EgtResult};

/// Conversion types in increasing order of preference for a win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionType {
    Promotion,
    Capture,
    Checkmate,
}

/// Outcome for the side to move, ordered from worst to best.
/// Wins minimize plies, losses maximize them; losses reverse the conversion preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtcOutcome {
    Win(ConversionType, u16),
    Draw,
    Loss(ConversionType, u16),
}

impl DtcOutcome {
    /// Evaluates `m` for the mover before it is played, given this successor-side
    /// outcome. Quiet moves add one ply; captures and promotions reset DTC to one.
    /// A capturing promotion counts as a capture. Draws remain draws.
    pub fn before_move(self, m: &Move) -> Self {
        let (ct, n) = match self {
            Self::Win(ct, n) | Self::Loss(ct, n) => (ct, n),
            Self::Draw => return Self::Draw,
        };
        let (ct, n) = if m.is_capture() {
            (ConversionType::Capture, 1)
        } else if m.is_promotion() {
            (ConversionType::Promotion, 1)
        } else {
            (ct, n + 1)
        };
        match self {
            Self::Loss(_, _) => Self::Win(ct, n),
            _ => Self::Loss(ct, n),
        }
    }
}

impl Ord for DtcOutcome {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (DtcOutcome::Win(ct1, n1), DtcOutcome::Win(ct2, n2)) => n2.cmp(n1).then(ct1.cmp(ct2)),
            (DtcOutcome::Draw, DtcOutcome::Draw) => Ordering::Equal,
            (DtcOutcome::Loss(ct1, n1), DtcOutcome::Loss(ct2, n2)) => n1.cmp(n2).then(ct2.cmp(ct1)),
            (DtcOutcome::Win(_, _), DtcOutcome::Draw) => Ordering::Greater,
            (DtcOutcome::Draw, DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Win(_, _), DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Draw, DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Draw) => Ordering::Less,
        }
    }
}

impl PartialOrd for DtcOutcome {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Two-byte outcome used by generation and table files, including unassigned entries.
/// The low three bits encode WDL/conversion; the upper thirteen encode distance,
/// or a nonzero remaining-move counter for unknown entries. Zero is invalid.
/// Raw constructors do not validate the encoding; packed ordering is not DTC ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackedOutcome(u16);

impl PackedOutcome {
    pub const INVALID: Self = Self(0b000);
    pub const DRAW: Self = Self(0b001);

    pub fn from_u16(value: u16) -> Self {
        Self(value)
    }

    pub fn to_u16(self) -> u16 {
        self.0
    }

    pub fn is_assigned(self) -> bool {
        (self.0 & 0b111) != 0b000
    }

    pub fn is_invalid(self) -> bool {
        self.0 == 0b000
    }

    pub fn is_unknown(self) -> bool {
        !self.is_assigned() && self.get_unknown_counter() != 0
    }

    pub fn get_unknown_counter(self) -> u16 {
        self.0 >> 3
    }

    /// Distance in plies; the caller must have established this is a win or loss.
    pub(crate) fn plies(self) -> u16 {
        self.0 >> 3
    }

    /// Conversion type; the caller must have established this is a win or loss.
    pub(crate) fn conversion_type(self) -> ConversionType {
        match self.0 & 0b110 {
            0b010 => ConversionType::Checkmate,
            0b100 => ConversionType::Capture,
            _ => ConversionType::Promotion,
        }
    }

    pub fn is_draw(self) -> bool {
        self.0 == 0b001
    }

    pub fn is_win(self) -> bool {
        matches!(self.0 & 0b111, 0b010 | 0b100 | 0b110)
    }

    pub fn is_loss(self) -> bool {
        matches!(self.0 & 0b111, 0b011 | 0b101 | 0b111)
    }

    pub fn new_win(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b010,
            ConversionType::Capture => 0b100,
            ConversionType::Promotion => 0b110,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_loss(ct: ConversionType, distance: u16) -> Self {
        Self(Self::new_win(ct, distance).0 | 1)
    }

    pub fn new_unknown(counter: u16) -> Self {
        Self(counter << 3)
    }

    /// Decodes an assigned outcome. Invalid and unknown entries return
    /// [`EgtError::CorruptedOutcome`], with `index` as table-local error context.
    pub fn to_outcome_at(self, index: usize) -> EgtResult<DtcOutcome> {
        match self.0 & 0b111 {
            0 => Err(EgtError::CorruptedOutcome {
                value: self.0,
                index,
            }),
            1 => Ok(DtcOutcome::Draw),
            _ if self.0 & 1 == 0 => Ok(DtcOutcome::Win(self.conversion_type(), self.plies())),
            _ => Ok(DtcOutcome::Loss(self.conversion_type(), self.plies())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConversionType::*, DtcOutcome::*, *};
    use shakmaty::{Role, Square};

    #[test]
    fn packed_roundtrip_and_outcome_order() {
        assert_eq!(std::mem::size_of::<PackedOutcome>(), 2);
        let ordered = [
            Loss(Checkmate, 1),
            Loss(Capture, 1),
            Loss(Promotion, 1),
            Loss(Checkmate, 2),
            Draw,
            Win(Promotion, 2),
            Win(Promotion, 1),
            Win(Capture, 1),
            Win(Checkmate, 1),
        ];
        assert!(ordered.windows(2).all(|w| w[0] < w[1]));
        for ct in [Checkmate, Capture, Promotion] {
            for n in [0, 1, 8191] {
                for (packed, outcome) in [
                    (PackedOutcome::new_win(ct, n), Win(ct, n)),
                    (PackedOutcome::new_loss(ct, n), Loss(ct, n)),
                ] {
                    assert_eq!(PackedOutcome::from_u16(packed.to_u16()), packed);
                    assert_eq!((packed.plies(), packed.conversion_type()), (n, ct));
                    assert_eq!(packed.to_outcome_at(42).unwrap(), outcome);
                }
            }
        }
        for raw in [1, 9, 65529] {
            let packed = PackedOutcome::from_u16(raw);
            assert_eq!(packed.to_outcome_at(42).unwrap(), Draw);
            assert_eq!(packed.is_draw(), raw == 1);
        }
        assert!(PackedOutcome::INVALID.is_invalid());
        for n in [1, 8191] {
            let unknown = PackedOutcome::new_unknown(n);
            assert!(unknown.is_unknown() && !unknown.is_assigned());
            assert_eq!(unknown.get_unknown_counter(), n);
            assert!(matches!(unknown.to_outcome_at(42),
                Err(EgtError::CorruptedOutcome { value, index: 42 }) if value == n << 3));
        }
        assert!(PackedOutcome::INVALID.to_outcome_at(42).is_err());
    }

    #[test]
    fn before_move_perspective_and_conversions() {
        let normal = |capture, promotion| Move::Normal {
            role: Role::Pawn,
            from: Square::A7,
            capture,
            to: Square::A8,
            promotion,
        };
        for (capture, promotion, ct, n) in [
            (None, None, Checkmate, 8),
            (Some(Role::Rook), None, Capture, 1),
            (None, Some(Role::Queen), Promotion, 1),
            (Some(Role::Rook), Some(Role::Queen), Capture, 1),
        ] {
            let m = normal(capture, promotion);
            assert_eq!(Loss(Checkmate, 7).before_move(&m), Win(ct, n));
            assert_eq!(Win(Checkmate, 7).before_move(&m), Loss(ct, n));
            assert_eq!(Draw.before_move(&m), Draw);
        }
        assert_eq!(
            Loss(Checkmate, 0).before_move(&normal(None, None)),
            Win(Checkmate, 1)
        );
    }
}
