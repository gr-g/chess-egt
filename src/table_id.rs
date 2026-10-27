//! Validated canonical table names and allocation-free position routing.
//!
//! Horizontal canonicalization compares pawn-file multisets, not color words.
//! Reflection ties deliberately retain both color-word orientations.

use arrayvec::ArrayString;
use shakmaty::{ByRole, Chess, Color, File, Position, Role};
use std::cmp::Ordering;

use crate::error::{EgtError, EgtResult};
use crate::piece_set::{EgtElement, EgtSide, MAX_PIECES, validate_elements};

type TableName = ArrayString<{ 3 * MAX_PIECES + 1 }>;

/// A validated exact table name, without an indexing plan or storage access.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableId(String);

impl PartialOrd for TableId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TableId {
    fn cmp(&self, other: &Self) -> Ordering {
        fn name_key(name: &str) -> impl Iterator<Item = u8> + '_ {
            name.bytes().filter(|&b| b != b'o').map(|b| match b {
                b'K' => 0,
                b'Q' => 1,
                b'R' => 2,
                b'B' => 3,
                b'N' => 4,
                b'P' => 5,
                _ => b,
            })
        }
        (self.n_pieces(), self.n_pawns())
            .cmp(&(other.n_pieces(), other.n_pawns()))
            .then_with(|| name_key(&self.0).cmp(name_key(&other.0)))
            .then_with(|| {
                if self == other {
                    Ordering::Equal
                } else {
                    self.parse_elements().cmp(&other.parse_elements())
                }
            })
    }
}

impl TableId {
    /// Rejects malformed names and noncanonical horizontal orientations.
    pub fn parse(name: &str) -> EgtResult<Self> {
        let elements = parse_elements(name)?;
        if PawnKey::from_elements(&elements).canonicalized().1 {
            return Err(EgtError::InvalidEndgameName {
                name: name.to_owned(),
                reason: "noncanonical horizontal pawn-file orientation",
            });
        }
        Ok(Self(name.to_owned()))
    }

    /// Routes a position, returning whether indexing requires file reflection.
    pub fn from_position(position: &Chess) -> EgtResult<(Self, bool)> {
        let (name, reflected) = position_table_name(position)?;
        Ok((Self(name.to_string()), reflected))
    }

    pub fn from_elements(elements: Vec<EgtElement>) -> EgtResult<Self> {
        validate_elements(&elements)?;
        if PawnKey::from_elements(&elements).canonicalized().1 {
            return Err(EgtError::InvalidPieceConfig(
                "noncanonical horizontal pawn-file orientation",
            ));
        }
        Ok(Self(compute_tablename(&elements)))
    }

    /// Parses logical elements on demand, without constructing an indexing plan.
    pub fn parse_elements(&self) -> Vec<EgtElement> {
        parse_elements(&self.0).expect("validated table name")
    }
    pub fn tablename(&self) -> &str {
        &self.0
    }
    /// Oriented material, retaining STM on the left.
    pub fn material(&self) -> String {
        self.0.chars().filter(|c| !c.is_ascii_lowercase()).collect()
    }

    pub fn twin(&self) -> EgtResult<(Self, bool)> {
        let elements = self.parse_elements();
        let (key, reflected) = PawnKey::from_elements(&elements).twin().canonicalized();
        let mut twin = key.elements();
        twin.extend(elements.into_iter().filter_map(|element| match element {
            EgtElement::PieceGroup {
                role,
                side,
                multiplicity,
            } => Some(EgtElement::PieceGroup {
                role,
                side: side.reverse(),
                multiplicity,
            }),
            _ => None,
        }));
        Ok((Self::from_elements(twin)?, reflected))
    }
    pub fn n_pieces(&self) -> usize {
        self.0.bytes().filter(u8::is_ascii_uppercase).count()
    }
    pub fn n_pawns(&self) -> usize {
        self.0.bytes().filter(|&b| b == b'P').count()
    }
    /// Each mutually opposing pair has exactly one opposed STM pawn.
    pub fn opposing_pairs(&self) -> usize {
        self.0
            .split_once('_')
            .unwrap()
            .0
            .bytes()
            .filter(|&b| b == b'o')
            .count()
    }
    pub(crate) fn side_piece_counts(&self) -> (usize, usize) {
        let (a, b) = self.0.split_once('_').unwrap();
        (
            a.bytes().filter(u8::is_ascii_uppercase).count(),
            b.bytes().filter(u8::is_ascii_uppercase).count(),
        )
    }
    /// Portable relative path; performs no storage I/O.
    pub fn relative_path(&self, extension: &str) -> String {
        let (a, b) = self.side_piece_counts();
        format!(
            "{}/{}v{}/op{}/{}pawns/{}.{}",
            self.n_pieces(),
            a.max(b),
            a.min(b),
            self.opposing_pairs(),
            self.n_pawns(),
            self.0,
            extension.trim_start_matches('.')
        )
    }
}

/// Warm probes borrow this stack name; only cache misses need an owned identity.
pub(crate) fn position_table_name(position: &Chess) -> EgtResult<(TableName, bool)> {
    if position.board().occupied().count() > MAX_PIECES {
        return Err(EgtError::InvalidPieceConfig(
            "at most eight physical pieces are supported",
        ));
    }
    let (key, reflected) = PawnKey::from_position(position).canonicalized();
    let name = format_name(key, |side, role| {
        let color = if side == EgtSide::SideToMove {
            position.turn()
        } else {
            !position.turn()
        };
        (position.board().by_role(role) & position.board().by_color(color)).count()
    });
    Ok((name, reflected))
}

pub(crate) fn compute_tablename(elements: &[EgtElement]) -> String {
    format_name(PawnKey::from_elements(elements), |side, role| {
        elements
            .iter()
            .filter_map(|element| match element {
                EgtElement::PieceGroup {
                    role: r,
                    side: s,
                    multiplicity,
                } if *r == role && *s == side => Some(*multiplicity),
                _ => None,
            })
            .sum()
    })
    .to_string()
}

fn format_name(key: PawnKey, count: impl Fn(EgtSide, Role) -> usize) -> TableName {
    let mut name = TableName::new();
    for side in [EgtSide::SideToMove, EgtSide::SideNotToMove] {
        let stm = side == EgtSide::SideToMove;
        if !stm {
            name.push('_');
        }
        for role in Role::ALL.into_iter().rev().filter(|r| *r != Role::Pawn) {
            for _ in 0..count(side, role) {
                name.push(role.upper_char());
            }
        }
        for (file, word) in key.files.into_iter().enumerate() {
            let len = word.ilog2() as usize;
            for slot in 0..len {
                let i = if stm { slot } else { len - 1 - slot };
                if (word & (1 << i) == 0) != stm {
                    continue;
                }
                name.push('P');
                name.push((b'a' + file as u8) as char);
                let opposed = if stm {
                    i + 1 < len && word & (1 << (i + 1)) != 0
                } else {
                    i > 0 && word & (1 << (i - 1)) == 0
                };
                if opposed {
                    name.push('o');
                }
            }
        }
    }
    name
}

/// Parses and validates a descriptor without constructing an indexing plan.
pub(crate) fn parse_elements(tablename: &str) -> EgtResult<Vec<EgtElement>> {
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
        let mut counts = ByRole::new_with(|_| 0usize);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let role = match c {
                'K' => Role::King,
                'Q' => Role::Queen,
                'R' => Role::Rook,
                'B' => Role::Bishop,
                'N' => Role::Knight,
                'P' => {
                    let file = match chars.next() {
                        Some(c @ 'a'..='h') => File::new(c as u32 - 'a' as u32),
                        _ => return Err(invalid("invalid pawn file specifier")),
                    };
                    let opposed = chars.next_if_eq(&'o').is_some();
                    flags[side_idx][file.to_usize()].push(opposed);
                    continue;
                }
                _ => return Err(invalid("invalid piece character")),
            };
            counts[role] += 1;
        }
        if counts.king != 1 {
            return Err(invalid("each side must have exactly one king"));
        }
        for role in Role::ALL.into_iter().rev() {
            if counts[role] != 0 {
                elements.push(EgtElement::PieceGroup {
                    role,
                    side,
                    multiplicity: counts[role],
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
    elements.sort();
    crate::piece_set::validate_elements(&elements).map_err(|error| match error {
        EgtError::InvalidPieceConfig(reason) => invalid(reason),
        other => other,
    })?;
    if compute_tablename(&elements) != tablename {
        return Err(invalid(
            "noncanonical piece order; expected KQRBN then pawns by file",
        ));
    }
    Ok(elements)
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
            let mut pawns =
                position.board().pawns() & shakmaty::Bitboard::from_file(File::new(file as u32));
            *word = 1 << pawns.count();
            for i in 0..pawns.count() {
                let square = if position.turn() == Color::Black {
                    pawns.pop_back()
                } else {
                    pawns.pop_front()
                }
                .unwrap();
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
    use crate::egt::Egt;
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
                TableId::from_elements(table.parse_elements()).unwrap(),
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
        let noncanonical = parse_elements("KPh_K").unwrap();
        assert!(TableId::from_elements(noncanonical).is_err());
    }

    #[test]
    fn table_order_is_not_indexing_element_order() {
        for (first, second) in [("KR_K", "K_KQ"), ("KQQ_K", "KQR_K")] {
            let first = TableId::parse(first).unwrap();
            let second = TableId::parse(second).unwrap();
            assert!(first < second);
            assert!(first.parse_elements() > second.parse_elements());
        }
        let opposed = TableId::parse("KPao_KPao").unwrap();
        let unopposed = TableId::parse("KPa_KPa").unwrap();
        assert_eq!(
            opposed.cmp(&unopposed),
            opposed.parse_elements().cmp(&unopposed.parse_elements())
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
            let key = PawnKey::from_elements(&table.parse_elements());
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
                let key = PawnKey::from_elements(&table.parse_elements());
                assert_eq!(key.reflected().reflected(), key);
                assert_eq!(key.twin().twin(), key);
                assert_eq!(key.twin().canonicalized().1, reflected);
                assert_eq!(TableId::parse(table.tablename()).unwrap(), table);
                // Indexers are used only by this compatibility test, never by
                // production identity construction or theoretical enumeration.
                if table.n_pieces() > 4 {
                    continue;
                }
                let egt = Egt::from_elements(table.parse_elements()).unwrap();
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
