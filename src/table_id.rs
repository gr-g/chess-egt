//! Cheap, storage-independent identities for individual canonical tables.
//!
//! Names and element constructors reject noncanonical horizontal orientations.
//! Position routing and twins canonicalize and report whether files were reflected.
//! Canonicalization compares the two sides' sorted pawn-file multisets, **not**
//! color words. When both multisets tie with their reflections, both color-word
//! orientations remain distinct tables, exactly as in the existing indexing scheme.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use shakmaty::{Chess, Color, File, Position, Role};

use crate::egt::{Egt, compute_tablename};
use crate::error::{EgtError, EgtResult};
use crate::piece_set::{EgtElement, EgtSide, MAX_PIECES, validate_elements};

/// Identity of one oriented table, without an indexer, frames, or storage access.
/// Ordered by piece count, pawn count, then name in KQRBNP order ignoring `o`
/// markers. Canonical elements break ties between opposition variants.
#[derive(Clone, Debug)]
pub struct TableId {
    name: String,
    elements: Vec<EgtElement>,
}

impl PartialEq for TableId {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Eq for TableId {}
impl PartialOrd for TableId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TableId {
    fn cmp(&self, other: &Self) -> Ordering {
        fn name_key(name: &str) -> impl Iterator<Item = u8> + '_ {
            name.bytes()
                .filter(|&byte| byte != b'o')
                .map(|byte| match byte {
                    b'K' => 0,
                    b'Q' => 1,
                    b'R' => 2,
                    b'B' => 3,
                    b'N' => 4,
                    b'P' => 5,
                    _ => byte,
                })
        }

        (self.n_pieces(), self.n_pawns())
            .cmp(&(other.n_pieces(), other.n_pawns()))
            .then_with(|| name_key(&self.name).cmp(name_key(&other.name)))
            .then_with(|| self.elements.cmp(&other.elements))
    }
}
impl Hash for TableId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl TableId {
    /// Parses an exact table name, rejecting noncanonical file orientations.
    /// Uppercase pawn material such as `KP_K` is a selector, not a table name.
    pub fn parse(name: &str) -> EgtResult<Self> {
        let elements = Egt::parse_elements(name)?;
        Self::from_elements(elements).map_err(|error| match error {
            EgtError::InvalidPieceConfig(reason) => EgtError::InvalidEndgameName {
                name: name.to_owned(),
                reason,
            },
            other => other,
        })
    }

    /// Routes a position by material and pawn color order in normalized ranks.
    /// The flag means the position must be horizontally reflected for indexing.
    pub fn from_position(position: &Chess) -> EgtResult<(Self, bool)> {
        if position.board().occupied().count() > MAX_PIECES {
            return Err(EgtError::InvalidPieceConfig(
                "at most eight physical pieces are supported",
            ));
        }
        let key = PawnKey::from_position(position);
        let (key, reflected) = key.canonicalized();
        let mut elements = key.elements();
        for side in [EgtSide::SideToMove, EgtSide::SideNotToMove] {
            let color = if side == EgtSide::SideToMove {
                position.turn()
            } else {
                !position.turn()
            };
            for role in Role::ALL
                .into_iter()
                .rev()
                .filter(|role| *role != Role::Pawn)
            {
                let multiplicity =
                    (position.board().by_role(role) & position.board().by_color(color)).count();
                if multiplicity != 0 {
                    elements.push(EgtElement::PieceGroup {
                        role,
                        side,
                        multiplicity,
                    });
                }
            }
        }
        Ok((Self::from_elements(elements)?, reflected))
    }

    /// Sorts and validates elements, rejecting noncanonical horizontal input.
    pub fn from_elements(mut elements: Vec<EgtElement>) -> EgtResult<Self> {
        validate_elements(&elements)?;
        if PawnKey::from_elements(&elements).canonicalized().1 {
            return Err(EgtError::InvalidPieceConfig(
                "noncanonical horizontal pawn-file orientation",
            ));
        }
        elements.sort();
        Ok(Self {
            name: compute_tablename(&elements),
            elements,
        })
    }

    pub fn elements(&self) -> &[EgtElement] {
        &self.elements
    }
    pub fn tablename(&self) -> &str {
        &self.name
    }

    /// Oriented uppercase material, with STM first (never exchanges the sides).
    pub fn material(&self) -> String {
        let mut names = Vec::new();
        for side in [EgtSide::SideToMove, EgtSide::SideNotToMove] {
            let mut name = String::new();
            for role in Role::ALL.into_iter().rev() {
                let count: usize = self
                    .elements
                    .iter()
                    .map(|element| match element {
                        EgtElement::PieceGroup {
                            role: r,
                            side: s,
                            multiplicity,
                        } if *r == role && *s == side => *multiplicity,
                        EgtElement::PawnGroup { order, .. } if role == Role::Pawn => {
                            order.iter().filter(|s| **s == side).count()
                        }
                        _ => 0,
                    })
                    .sum();
                name.extend(std::iter::repeat_n(role.upper_char(), count));
            }
            names.push(name);
        }
        names.join("_")
    }

    /// The table reached by a quiet move: exchange sides and reverse normalized
    /// ranks, then canonicalize files. The flag applies to the successor position.
    pub fn twin(&self) -> EgtResult<(Self, bool)> {
        let (key, reflected) = PawnKey::from_elements(&self.elements)
            .twin()
            .canonicalized();
        let mut elements = key.elements();
        elements.extend(self.elements.iter().filter_map(|element| match element {
            EgtElement::PieceGroup {
                role,
                side,
                multiplicity,
            } => Some(EgtElement::PieceGroup {
                role: *role,
                side: side.reverse(),
                multiplicity: *multiplicity,
            }),
            _ => None,
        }));
        Ok((Self::from_elements(elements)?, reflected))
    }

    pub fn n_pieces(&self) -> usize {
        self.elements.iter().map(EgtElement::multiplicity).sum()
    }
    pub fn n_pawns(&self) -> usize {
        self.elements
            .iter()
            .map(|element| match element {
                EgtElement::PawnGroup { order, .. } => order.len(),
                _ => 0,
            })
            .sum()
    }

    /// Mutually opposing pairs: an STM pawn immediately followed by an SNTM
    /// pawn in ascending normalized rank. Each pair is counted once, not twice.
    pub fn opposing_pairs(&self) -> usize {
        self.elements
            .iter()
            .map(|element| match element {
                EgtElement::PawnGroup { order, .. } => order
                    .windows(2)
                    .filter(|pair| pair == &[EgtSide::SideToMove, EgtSide::SideNotToMove])
                    .count(),
                _ => 0,
            })
            .sum()
    }

    pub(crate) fn side_piece_counts(&self) -> (usize, usize) {
        let stm = self
            .elements
            .iter()
            .map(|element| match element {
                EgtElement::PieceGroup {
                    side: EgtSide::SideToMove,
                    multiplicity,
                    ..
                } => *multiplicity,
                EgtElement::PawnGroup { order, .. } => {
                    order.iter().filter(|s| **s == EgtSide::SideToMove).count()
                }
                _ => 0,
            })
            .sum();
        (stm, self.n_pieces() - stm)
    }

    /// Portable relative routing path. `extension` may include a leading dot.
    /// This only formats a path; it performs no filesystem or object-store I/O.
    pub fn relative_path(&self, extension: &str) -> String {
        let (a, b) = self.side_piece_counts();
        format!(
            "{}/{}v{}/op{}/{}pawns/{}.{}",
            self.n_pieces(),
            a.max(b),
            a.min(b),
            self.opposing_pairs(),
            self.n_pawns(),
            self.name,
            extension.trim_start_matches('.')
        )
    }
}

/// Allocation-free pawn color words, copied from the former storage-layer
/// identity. Low bits follow ascending normalized ranks; one means SNTM.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct PawnKey {
    files: [u8; 8],
}

impl PawnKey {
    pub(crate) fn from_elements(elements: &[EgtElement]) -> Self {
        let mut files = [1; 8];
        for element in elements {
            if let EgtElement::PawnGroup { file, order } = element {
                let mut word = 1 << order.len();
                for (i, side) in order.iter().enumerate() {
                    if *side == EgtSide::SideNotToMove {
                        word |= 1 << i;
                    }
                }
                files[file.to_usize()] = word;
            }
        }
        Self { files }
    }

    pub(crate) fn from_position(position: &Chess) -> Self {
        let mut files = [1; 8];
        let enemy = position.board().by_color(!position.turn());
        for (file, word) in files.iter_mut().enumerate() {
            let pawns =
                position.board().pawns() & shakmaty::Bitboard::from_file(File::new(file as u32));
            let mut squares: arrayvec::ArrayVec<_, 6> = pawns.into_iter().collect();
            if position.turn() == Color::Black {
                squares.reverse();
            }
            *word = 1 << squares.len();
            for (i, square) in squares.into_iter().enumerate() {
                if enemy.contains(square) {
                    *word |= 1 << i;
                }
            }
        }
        Self { files }
    }

    pub(crate) fn reflected(self) -> Self {
        let mut files = self.files;
        files.reverse();
        Self { files }
    }

    pub(crate) fn twin(self) -> Self {
        let mut files = [1; 8];
        for (target, word) in files.iter_mut().zip(self.files) {
            let len = word.ilog2() as usize;
            *target = 1 << len;
            for i in 0..len {
                if word & (1 << i) == 0 {
                    *target |= 1 << (len - 1 - i);
                }
            }
        }
        Self { files }
    }

    pub(crate) fn canonicalized(self) -> (Self, bool) {
        let mut stm = arrayvec::ArrayVec::<File, 6>::new();
        let mut sntm = arrayvec::ArrayVec::<File, 6>::new();
        for (file, word) in self.files.into_iter().enumerate() {
            for i in 0..word.ilog2() {
                let files = if word & (1 << i) == 0 {
                    &mut stm
                } else {
                    &mut sntm
                };
                files.push(File::new(file as u32));
            }
        }
        if is_canonical(&stm, &sntm) {
            (self, false)
        } else {
            (self.reflected(), true)
        }
    }

    fn elements(self) -> Vec<EgtElement> {
        self.files
            .into_iter()
            .enumerate()
            .filter_map(|(file, word)| {
                let len = word.ilog2() as usize;
                (len != 0).then(|| EgtElement::PawnGroup {
                    file: File::new(file as u32),
                    order: (0..len)
                        .map(|i| {
                            if word & (1 << i) == 0 {
                                EgtSide::SideToMove
                            } else {
                                EgtSide::SideNotToMove
                            }
                        })
                        .collect(),
                })
            })
            .collect()
    }
}

/// Both slices must be sorted ascending. Ties deliberately ignore color words.
pub(crate) fn is_canonical(stm: &[File], sntm: &[File]) -> bool {
    fn compare(files: &[File]) -> Ordering {
        files
            .iter()
            .map(|f| f.to_usize())
            .cmp(files.iter().rev().map(|f| 7 - f.to_usize()))
    }
    match compare(stm) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => compare(sntm) != Ordering::Greater,
    }
}

/// Enumerates rank color words for fixed pawn-file multisets, without indexers.
pub(crate) fn pawn_order_combinations(stm: &[File], sntm: &[File]) -> Vec<Vec<EgtElement>> {
    let mut counts = [[0usize; 2]; 8];
    for &file in stm {
        counts[file.to_usize()][0] += 1;
    }
    for &file in sntm {
        counts[file.to_usize()][1] += 1;
    }
    if counts.iter().any(|c| c[0] + c[1] > 6) {
        return Vec::new();
    }
    let mut layouts = vec![Vec::new()];
    for (file, [n_stm, n_sntm]) in counts.into_iter().enumerate() {
        let len = n_stm + n_sntm;
        if len == 0 {
            continue;
        }
        let mut next = Vec::new();
        for mask in 0u8..(1 << len) {
            if mask.count_ones() as usize != n_sntm {
                continue;
            }
            let order = (0..len)
                .map(|i| {
                    if mask & (1 << i) == 0 {
                        EgtSide::SideToMove
                    } else {
                        EgtSide::SideNotToMove
                    }
                })
                .collect();
            let group = EgtElement::PawnGroup {
                file: File::new(file as u32),
                order,
            };
            for layout in &layouts {
                let mut layout = layout.clone();
                layout.push(group.clone());
                next.push(layout);
            }
        }
        layouts = next;
    }
    layouts
}

pub(crate) fn get_file_combinations(k: usize) -> Vec<Vec<File>> {
    fn recurse(k: usize, start: usize, current: &mut Vec<File>, results: &mut Vec<Vec<File>>) {
        if current.len() == k {
            results.push(current.clone());
            return;
        }
        for idx in start..8 {
            current.push(File::new(idx as u32));
            recurse(k, idx, current, results);
            current.pop();
        }
    }
    let mut results = Vec::new();
    recurse(k, 0, &mut Vec::new(), &mut results);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::TableSelector;
    use shakmaty::{CastlingMode, fen::Fen};

    fn position(fen: &str) -> Chess {
        fen.parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap()
    }

    #[test]
    fn parsing_is_cheap_canonical_and_strict() {
        for name in ["K_K", "KQ_K", "KPa_K", "KPao_KPao", "KPa_KPa", "KPaPh_K"] {
            let table = TableId::parse(name).unwrap();
            assert_eq!(table.tablename(), name);
            assert_eq!(
                TableId::from_elements(table.elements().to_vec()).unwrap(),
                table
            );
        }
        for name in [
            "",
            "KP_K",
            "KPh_K",
            "K_KPh",
            "KQK_K",
            "KPaPaPaPaPaPaPa_K",
            "KQQQQQQQ_K",
        ] {
            assert!(TableId::parse(name).is_err(), "{name}");
        }
        let noncanonical = Egt::parse_elements("KPh_K").unwrap();
        assert!(TableId::from_elements(noncanonical).is_err());
    }

    #[test]
    fn table_order_is_not_indexing_element_order() {
        for (first, second) in [("KR_K", "K_KQ"), ("KQQ_K", "KQR_K")] {
            let first = TableId::parse(first).unwrap();
            let second = TableId::parse(second).unwrap();
            assert!(first < second);
            assert!(first.elements > second.elements);
        }
        let opposed = TableId::parse("KPao_KPao").unwrap();
        let unopposed = TableId::parse("KPa_KPa").unwrap();
        assert_eq!(
            opposed.cmp(&unopposed),
            opposed.elements.cmp(&unopposed.elements)
        );
        assert!(opposed < unopposed);
    }

    #[test]
    fn ordering_is_consistent_with_identity() {
        let tables = crate::resolve_selectors(&["2/", "3/", "4/", "KPP_KP"]).unwrap();
        for a in &tables {
            for b in &tables {
                assert_eq!(a.cmp(b).is_eq(), a == b);
                assert_eq!(a.cmp(b), b.cmp(a).reverse());
                assert_eq!(a.partial_cmp(b), Some(a.cmp(b)));
                assert_eq!(a.elements == b.elements, a == b);
            }
            assert_eq!(
                a.cmp(&TableId::parse(a.tablename()).unwrap()),
                Ordering::Equal
            );
        }
    }

    #[test]
    fn routing_preserves_colors_ranks_reflection_and_en_passant() {
        for (fen, name, reflected) in [
            ("7k/8/8/8/8/8/P7/K7 w - - 0 1", "KPa_K", false),
            ("k7/8/8/8/8/8/7P/7K w - - 0 1", "KPa_K", true),
            ("7k/p7/8/8/8/8/8/K7 b - - 0 1", "KPa_K", false),
            ("7k/8/8/8/8/8/P7/K7 b - - 0 1", "K_KPa", false),
            ("7k/8/8/3Pp3/8/8/8/K7 w - e6 0 1", "KPd_KPe", false),
            ("7k/8/8/8/3pP3/8/8/K7 b - e3 0 1", "KPd_KPe", false),
            ("7k/8/8/p7/P7/8/8/K7 w - - 0 1", "KPao_KPao", false),
            ("7k/8/8/P7/p7/8/8/K7 w - - 0 1", "KPa_KPa", false),
        ] {
            let (id, actual_reflection) = TableId::from_position(&position(fen)).unwrap();
            assert_eq!(id.tablename(), name, "{fen}");
            assert_eq!(actual_reflection, reflected, "{fen}");
        }
    }

    #[test]
    fn twins_and_paths() {
        let table = TableId::parse("KPd_KPe").unwrap();
        let (twin, reflected) = table.twin().unwrap();
        assert_eq!(twin, table);
        assert!(reflected);
        assert_eq!(table.material(), "KP_KP");
        assert_eq!(
            table.relative_path("ggegt"),
            "4/2v2/op0/2pawns/KPd_KPe.ggegt"
        );
        assert_eq!(
            table.relative_path(".json"),
            "4/2v2/op0/2pawns/KPd_KPe.json"
        );
        let table = TableId::parse("KPao_KPao").unwrap();
        assert_eq!(table.opposing_pairs(), 1);
        assert_eq!(
            table.relative_path("ggegt"),
            "4/2v2/op1/2pawns/KPao_KPao.ggegt"
        );
        assert_eq!(
            TableId::parse("KQ_K").unwrap().relative_path("json"),
            "3/2v1/op0/0pawns/KQ_K.json"
        );
        let table = TableId::parse("KPaPao_KPao").unwrap();
        assert_eq!(table.opposing_pairs(), 1);
    }

    #[test]
    fn color_order_does_not_break_file_multiplicity_ties() {
        // Both sides' file multisets are invariant under reflection, but their
        // color words differ. The historical policy keeps both orientations.
        let a = TableId::parse("KPaoPh_KPaoPh").unwrap();
        let b = TableId::parse("KPaPho_KPaPho").unwrap();
        assert_ne!(a, b);
        for table in [&a, &b] {
            let key = PawnKey::from_elements(table.elements());
            assert!(!key.canonicalized().1);
            assert!(!key.reflected().canonicalized().1);
            assert_ne!(key, key.reflected());
        }
    }

    #[test]
    fn exhaustive_small_material_twins_and_sampled_index_routing() {
        for material in ["K_K", "KQ_K", "KP_K", "KP_KP", "KPP_KP", "KPP_KPP"] {
            let tables = TableSelector::parse(material).unwrap().resolve().unwrap();
            for table in tables {
                let (twin, reflected) = table.twin().unwrap();
                assert_eq!(twin.twin().unwrap().0, table);
                let key = PawnKey::from_elements(table.elements());
                assert_eq!(key.reflected().reflected(), key);
                assert_eq!(key.twin().twin(), key);
                assert_eq!(key.twin().canonicalized().1, reflected);
                assert_eq!(TableId::parse(table.tablename()).unwrap(), table);
                // Indexers are used only by this compatibility test, never by
                // production identity construction or theoretical enumeration.
                if table.n_pieces() > 4 {
                    continue;
                }
                let egt = Egt::from_elements(table.elements().to_vec()).unwrap();
                for turn in [Color::White, Color::Black] {
                    for index in (0..egt.index_range()).step_by((egt.index_range() / 16).max(1)) {
                        let Some(position) = egt.position_from_index(index, turn) else {
                            continue;
                        };
                        assert_eq!(
                            TableId::from_position(&position).unwrap(),
                            (table.clone(), false)
                        );
                        for mv in position.legal_moves() {
                            if mv.is_capture() || mv.is_promotion() {
                                continue;
                            }
                            let successor = position.clone().play(mv).unwrap();
                            assert_eq!(
                                TableId::from_position(&successor).unwrap(),
                                (twin.clone(), reflected)
                            );
                        }
                    }
                }
            }
        }
    }
}
