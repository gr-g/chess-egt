//! Validated canonical table names and allocation-free position routing.
//!
//! Horizontal canonicalization compares STM then SNTM pawn-file multisets,
//! breaking ties with the full pawn color words.

use arrayvec::ArrayString;
use shakmaty::{ByRole, Chess, Color, File, Position, Role};
use std::cmp::Ordering;

use crate::error::{EgtError, EgtResult};
use crate::selection::{Hierarchy, compare_file_reflection};

mod dependencies;

type TableName = ArrayString<{ 3 * MAX_PIECES + 1 }>;

/// A validated table identity, independent of its indexer and storage location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableId {
    pawns: PawnKey,
    pieces: [[u8; 5]; 2],
}

impl std::fmt::Display for TableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

impl std::str::FromStr for TableId {
    type Err = EgtError;
    fn from_str(name: &str) -> EgtResult<Self> {
        Self::parse(name)
    }
}

impl PartialOrd for TableId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TableId {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.n_pieces(), self.n_pawns())
            .cmp(&(other.n_pieces(), other.n_pawns()))
            .then_with(|| other.pieces[0].cmp(&self.pieces[0]))
            // Earlier tokens sort first, even when a STM token faces the '_' separator.
            .then_with(|| {
                other
                    .pawns
                    .file_counts(true)
                    .cmp(&self.pawns.file_counts(true))
            })
            .then_with(|| other.pieces[1].cmp(&self.pieces[1]))
            .then_with(|| {
                other
                    .pawns
                    .file_counts(false)
                    .cmp(&self.pawns.file_counts(false))
            })
            // Equal file counts imply equal word lengths. Compare ascending rank,
            // which is stored least-significant bit first.
            .then_with(|| {
                self.pawns
                    .files
                    .map(u8::reverse_bits)
                    .cmp(&other.pawns.files.map(u8::reverse_bits))
            })
    }
}

pub(crate) const NONPAWN_ROLES: [Role; 5] = [
    Role::King,
    Role::Queen,
    Role::Rook,
    Role::Bishop,
    Role::Knight,
];

fn role_index(role: Role) -> usize {
    match role {
        Role::Pawn => unreachable!("pawns are stored by file"),
        _ => Role::King as usize - role as usize,
    }
}

impl TableId {
    pub(crate) fn new(pawns: PawnKey, pieces: [[u8; 5]; 2]) -> Self {
        Self { pawns, pieces }
    }

    /// Rejects malformed names and noncanonical horizontal orientations.
    pub fn parse(name: &str) -> EgtResult<Self> {
        let id = parse_oriented(name)?;
        if id.pawns.canonicalized().1 {
            return Err(EgtError::InvalidEndgameName {
                name: name.to_owned(),
                reason: "noncanonical horizontal pawn-file orientation",
            });
        }
        Ok(id)
    }

    /// Routes a position, returning whether its table identity requires file reflection.
    /// This flag does not include king normalization within a symmetric table.
    pub fn from_position(position: &Chess) -> EgtResult<(Self, bool)> {
        if !position.castles().is_empty() {
            return Err(EgtError::UnsupportedPosition(
                "positions with castling rights are not supported",
            ));
        }
        if position.board().occupied().count() > MAX_PIECES {
            return Err(EgtError::UnsupportedPosition(
                "at most eight physical pieces are supported",
            ));
        }
        let (pawns, reflected) = PawnKey::from_position(position).canonicalized();
        let pieces = [position.turn(), !position.turn()].map(|color| {
            NONPAWN_ROLES.map(|role| {
                (position.board().by_role(role) & position.board().by_color(color)).count() as u8
            })
        });
        Ok((Self { pawns, pieces }, reflected))
    }

    pub(crate) fn pawn_groups(&self) -> impl Iterator<Item = (File, u8)> + '_ {
        self.pawns
            .files
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, word)| word != 1)
            .map(|(file, word)| (File::new(file as u32), word))
    }

    pub(crate) fn symmetric_pawns(&self) -> bool {
        self.pawns.is_symmetric()
    }

    pub(crate) fn piece_count(&self, side: EgtSide, role: Role) -> usize {
        self.pieces[side_index(side)][role_index(role)].into()
    }

    pub(crate) fn name(&self) -> TableName {
        format_name(self.pawns, |side, role| self.piece_count(side, role))
    }

    /// Oriented material, retaining STM on the left.
    pub fn material(&self) -> String {
        self.name()
            .chars()
            .filter(|c| !c.is_ascii_lowercase())
            .collect()
    }

    /// Exchanges STM and SNTM, returning the canonical identity and file reflection.
    pub fn twin(&self) -> (Self, bool) {
        let (pawns, reflected) = self.pawns.twin().canonicalized();
        (
            Self {
                pawns,
                pieces: [self.pieces[1], self.pieces[0]],
            },
            reflected,
        )
    }
    pub fn n_pieces(&self) -> usize {
        self.pieces
            .iter()
            .flatten()
            .map(|&n| usize::from(n))
            .sum::<usize>()
            + self.n_pawns()
    }
    pub fn n_pawns(&self) -> usize {
        self.pawns
            .files
            .iter()
            .map(|word| word.ilog2() as usize)
            .sum()
    }
    /// Each mutually opposing pair has exactly one opposed STM pawn.
    pub fn opposing_pairs(&self) -> usize {
        self.pawns
            .files
            .iter()
            .map(|&word| {
                let bits = word ^ (1 << word.ilog2());
                ((bits >> 1) & !bits).count_ones() as usize
            })
            .sum()
    }
    fn side_piece_counts(&self) -> (usize, usize) {
        let enemy = self
            .pawns
            .files
            .iter()
            .map(|word| word.count_ones() as usize - 1)
            .sum::<usize>()
            + self.pieces[1]
                .iter()
                .map(|&n| usize::from(n))
                .sum::<usize>();
        (self.n_pieces() - enemy, enemy)
    }
    pub(crate) fn hierarchy(&self) -> Hierarchy {
        Hierarchy::from_counts(
            self.side_piece_counts(),
            self.opposing_pairs(),
            self.n_pawns(),
        )
    }

    /// Portable relative path within a tablebase root.
    pub fn relative_path(&self, extension: &str) -> String {
        let ext = extension.trim_start_matches('.');
        format!("{}{self}.{ext}", self.hierarchy())
    }
}

fn side_index(side: EgtSide) -> usize {
    usize::from(side == EgtSide::SideNotToMove)
}

pub(crate) fn uppercase_role(c: char) -> Option<Role> {
    Role::from_char(c).filter(|_| c.is_ascii_uppercase())
}

fn format_name(key: PawnKey, count: impl Fn(EgtSide, Role) -> usize) -> TableName {
    let mut name = TableName::new();
    for side in [EgtSide::SideToMove, EgtSide::SideNotToMove] {
        let stm = side == EgtSide::SideToMove;
        if !stm {
            name.push('_');
        }
        for role in NONPAWN_ROLES {
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
                if opposed(word, i, stm) {
                    name.push('o');
                }
            }
        }
    }
    name
}

pub(crate) fn parse_oriented(tablename: &str) -> EgtResult<TableId> {
    let invalid = |reason| EgtError::InvalidEndgameName {
        name: tablename.to_owned(),
        reason,
    };
    let (stm, sntm) = tablename
        .split_once('_')
        .ok_or_else(|| invalid("missing '_' separator"))?;
    let mut files = [1; 8];
    let mut pieces = [[0usize; 5]; 2];
    let mut flags: [[Vec<bool>; 8]; 2] = Default::default();
    for (side_idx, text) in [stm, sntm].into_iter().enumerate() {
        let mut counts = ByRole::new_with(|_| 0usize);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let role = match c {
                'P' => {
                    let file = match chars.next() {
                        Some(c @ 'a'..='h') => File::new(c as u32 - 'a' as u32),
                        _ => return Err(invalid("invalid pawn file specifier")),
                    };
                    let opposed = chars.next_if_eq(&'o').is_some();
                    flags[side_idx][file.to_usize()].push(opposed);
                    continue;
                }
                _ => uppercase_role(c).ok_or_else(|| invalid("invalid piece character"))?,
            };
            counts[role] += 1;
        }
        if counts.king != 1 {
            return Err(invalid("each side must have exactly one king"));
        }
        pieces[side_idx] = NONPAWN_ROLES.map(|role| counts[role]);
    }

    for (file_idx, target) in files.iter_mut().enumerate() {
        let stm_flags = &flags[0][file_idx];
        let sntm_flags = &flags[1][file_idx];
        let len = stm_flags.len() + sntm_flags.len();
        if len == 0 {
            continue;
        }
        if len > 6 {
            return Err(invalid("at most six pawns may occupy a file"));
        }
        *target = ((1 << len)..(2 << len))
            .find(|&word: &u8| {
                word.count_ones() as usize == sntm_flags.len() + 1
                    && pawn_flags(word, true).eq(stm_flags.iter().copied())
                    && pawn_flags(word, false).eq(sntm_flags.iter().copied())
            })
            .ok_or_else(|| invalid("inconsistent opposed-pawn flags"))?;
    }
    let pawns = PawnKey { files };
    let total = pieces.iter().flatten().sum::<usize>()
        + files
            .iter()
            .map(|word| word.ilog2() as usize)
            .sum::<usize>();
    if total > MAX_PIECES {
        return Err(invalid("at most eight physical pieces are supported"));
    }
    let id = TableId {
        pawns,
        pieces: pieces.map(|side| side.map(|n| n as u8)),
    };
    if id.name().as_str() != tablename {
        return Err(invalid(
            "noncanonical piece order; expected KQRBN then pawns by file",
        ));
    }
    Ok(id)
}

fn opposed(word: u8, i: usize, stm: bool) -> bool {
    if stm {
        i + 1 < word.ilog2() as usize && word & (1 << (i + 1)) != 0
    } else {
        i > 0 && word & (1 << (i - 1)) == 0
    }
}

fn pawn_flags(word: u8, stm: bool) -> impl Iterator<Item = bool> {
    let len = word.ilog2() as usize;
    (0..len)
        .map(move |i| if stm { i } else { len - 1 - i })
        .filter(move |&i| (word & (1 << i) == 0) == stm)
        .map(move |i| opposed(word, i, stm))
}

pub(crate) fn sides(word: u8) -> impl Iterator<Item = EgtSide> {
    (0..word.ilog2()).map(move |i| {
        if word & (1 << i) == 0 {
            EgtSide::SideToMove
        } else {
            EgtSide::SideNotToMove
        }
    })
}

/// Low bits follow ascending normalized ranks; one means SNTM.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct PawnKey {
    files: [u8; 8],
}

impl PawnKey {
    pub(crate) fn from_words(files: [u8; 8]) -> Self {
        Self { files }
    }

    fn file_counts(self, stm: bool) -> [u32; 8] {
        self.files.map(|word| {
            let sntm = word.count_ones() - 1;
            if stm { word.ilog2() - sntm } else { sntm }
        })
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

    pub(crate) fn is_symmetric(self) -> bool {
        self == self.reflected()
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
            for side in sides(word) {
                let files = if side == EgtSide::SideToMove {
                    &mut stm
                } else {
                    &mut sntm
                };
                files.push(File::new(file as u32));
            }
        }
        let reflected = self.reflected();
        if compare_file_reflection(&stm, &sntm).then_with(|| self.cmp(&reflected))
            != Ordering::Greater
        {
            (self, false)
        } else {
            (reflected, true)
        }
    }
}

pub(crate) const MAX_PIECES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EgtSide {
    SideToMove,
    SideNotToMove,
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn pawn_group(file: File, order: &[EgtSide]) -> (File, u8) {
        let word = order
            .iter()
            .enumerate()
            .fold(1 << order.len(), |word, (i, &side)| {
                word | (u8::from(side == EgtSide::SideNotToMove) << i)
            });
        (file, word)
    }

    pub(crate) fn with_pawns(material: &str, groups: &[(File, u8)]) -> TableId {
        let mut id = TableId::parse(material).unwrap();
        for &(file, word) in groups {
            assert_ne!(word, 1);
            assert_eq!(id.pawns.files[file.to_usize()], 1);
            id.pawns.files[file.to_usize()] = word;
        }
        assert!(id.n_pieces() <= MAX_PIECES);
        id
    }
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
    fn conversion_dependencies_cover_legal_successors() {
        for fen in [
            "7k/8/8/8/8/2r5/2K5/8 w - - 0 1",
            "7k/8/8/8/8/2p5/2K5/8 w - - 0 1",
            "8/7k/8/8/1r6/1RQ5/8/K7 w - - 0 1",
            "7k/8/8/1r1r4/2P5/8/8/K7 w - - 0 1",
            "7k/8/8/1p1p4/2P5/8/8/K7 w - - 0 1",
            "7k/8/2p5/1p1r4/2P5/3P4/8/K7 w - - 0 1",
            "7k/8/3p4/3r4/2P5/3p4/8/K7 w - - 0 1",
            "7k/8/8/3Pp3/8/8/8/K7 w - e6 0 1",
            "7k/8/8/8/3pP3/8/8/K7 b - e3 0 1",
            "2r4k/1P6/8/8/8/8/8/K7 w - - 0 1",
            "k4r2/6P1/8/8/8/8/8/7K w - - 0 1",
            "7k/8/8/8/8/8/1p6/2R4K b - - 0 1",
        ] {
            let position = position(fen);
            let table = TableId::from_position(&position).unwrap().0;
            let dependencies = table.dependencies();
            assert!(dependencies.windows(2).all(|pair| pair[0] < pair[1]));
            for mv in position.legal_moves() {
                if mv.is_capture() || mv.is_promotion() {
                    let successor = position.clone().play(mv).unwrap();
                    let next = TableId::from_position(&successor).unwrap().0;
                    assert!(dependencies.contains(&next), "{fen}: {mv:?} -> {next:?}");
                }
            }
        }
    }

    #[test]
    fn conversion_dependencies_exclude_blocked_promotions() {
        for (name, expected) in [
            ("K_K", vec![]),
            ("KPao_KPao", vec!["K_KPa"]),
            ("KPaPao_KPao", vec!["K_KPaPa"]),
            ("KPao_KRPao", vec!["KPao_KPao", "KR_KPa", "KPa_KPb"]),
        ] {
            let dependencies = TableId::parse(name).unwrap().dependencies();
            let mut expected: Vec<_> = expected
                .into_iter()
                .map(|name| TableId::parse(name).unwrap())
                .collect();
            expected.sort();
            assert_eq!(dependencies, expected, "{name}");
        }
    }

    #[test]
    fn only_oriented_parsing_accepts_reflected_names() {
        for name in ["KPh_K", "K_KPh"] {
            assert!(TableId::parse(name).is_err(), "{name}");
            assert_eq!(parse_oriented(name).unwrap().name().as_str(), name);
        }
    }

    fn old_name_order(a: &TableId, b: &TableId) -> Ordering {
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
        // After a name-key tie the nonpawn elements are identical. The old
        // element comparator then only distinguishes pawn files and color orders.
        let pawn_elements = |id: &TableId| {
            id.pawn_groups()
                .map(|(file, word)| (file, sides(word).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
        };
        let counts = |name: &str| {
            (
                name.bytes().filter(u8::is_ascii_uppercase).count(),
                name.bytes().filter(|&b| b == b'P').count(),
            )
        };
        let (a_name, b_name) = (a.name(), b.name());
        counts(&a_name)
            .cmp(&counts(&b_name))
            .then_with(|| name_key(&a_name).cmp(name_key(&b_name)))
            .then_with(|| pawn_elements(a).cmp(&pawn_elements(b)))
    }

    #[test]
    fn ordering_matches_old_name_order() {
        let actual = crate::resolve_selectors(&["2", "3", "4", "5"]).unwrap();
        assert_eq!(actual.len(), 1 + 16 + 176 + 1496);
        let mut expected = actual.clone();
        expected.sort_by(old_name_order);
        assert_eq!(actual, expected);
    }

    #[test]
    fn ordering_is_consistent_with_identity() {
        let mut tables = crate::resolve_selectors(&["2/", "3/", "4/"]).unwrap();
        tables.extend(crate::resolve_selectors(&["KPP_KP"]).unwrap());
        for a in &tables {
            for b in &tables {
                assert_eq!(a.cmp(b).is_eq(), a == b);
                assert_eq!(a.cmp(b), b.cmp(a).reverse());
                assert_eq!(a.partial_cmp(b), Some(a.cmp(b)));
            }
            assert_eq!(a.cmp(&TableId::parse(&a.name()).unwrap()), Ordering::Equal);
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
            ("7k/8/8/p6P/P6p/8/8/K7 w - - 0 1", "KPaPho_KPaPho", true),
            ("7k/8/8/P6p/p6P/8/8/K7 w - - 0 1", "KPaPho_KPaPho", false),
            ("k7/8/8/8/8/8/P6P/7K w - - 0 1", "KPaPh_K", false),
        ] {
            let (id, actual_reflection) = TableId::from_position(&position(fen)).unwrap();
            assert_eq!(id.name().as_str(), name, "{fen}");
            assert_eq!(actual_reflection, reflected, "{fen}");
        }
    }

    #[test]
    fn twins_and_paths() {
        let table = TableId::parse("KPd_KPe").unwrap();
        let (twin, reflected) = table.twin();
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
    fn color_order_breaks_file_multiplicity_ties() {
        // Both sides' file multisets are invariant under reflection, but their
        // color words differ. Only the smaller full key is canonical.
        let opposite = parse_oriented("KPaoPh_KPaoPh").unwrap();
        let table = TableId::parse("KPaPho_KPaPho").unwrap();
        let key = table.pawns;
        assert!(TableId::parse("KPaoPh_KPaoPh").is_err());
        assert_eq!(opposite.pawns.canonicalized(), (key, true));
        assert_eq!(key.canonicalized(), (key, false));
        assert!(!key.is_symmetric());
        for name in ["K_K", "KPaPh_K", "KPaoPho_KPaoPho"] {
            let key = TableId::parse(name).unwrap().pawns;
            assert!(key.is_symmetric());
            assert_eq!(key.canonicalized(), (key, false));
        }
    }

    #[test]
    fn exhaustive_small_material_twins_and_sampled_index_routing() {
        for material in ["K_K", "KQ_K", "KP_K", "KP_KP", "KPP_KP", "KPP_KPP"] {
            let tables = TableSelector::parse(material).unwrap().resolve().unwrap();
            for table in tables {
                let (twin, reflected) = table.twin();
                assert_eq!(twin.twin().0, table);
                assert_eq!(TableId::parse(&twin.name()).unwrap(), twin);
                let key = table.pawns;
                assert_eq!(key.reflected().reflected(), key);
                assert_eq!(key.twin().twin(), key);
                assert_eq!(key.twin().canonicalized().1, reflected);
                assert_eq!(TableId::parse(&table.name()).unwrap(), table);
                if table.n_pieces() > 4 {
                    continue;
                }
                let egt = Egt::new(&table).unwrap();
                for turn in [Color::White, Color::Black] {
                    for index in (0..egt.index_range()).step_by((egt.index_range() / 16).max(1)) {
                        let Ok(position) = egt.position_from_index(index, turn) else {
                            continue;
                        };
                        assert_eq!(TableId::from_position(&position).unwrap(), (table, false));
                        for mv in position.legal_moves() {
                            if mv.is_capture() || mv.is_promotion() {
                                continue;
                            }
                            let successor = position.clone().play(mv).unwrap();
                            assert_eq!(
                                TableId::from_position(&successor).unwrap(),
                                (twin, reflected)
                            );
                        }
                    }
                }
            }
        }
    }
}
