//! Pure theoretical table selection and generation scheduling; no storage I/O.
//!
//! Selectors are exact names (`KPa_K`), oriented uppercase material (`KP_K`),
//! or hierarchy prefixes (`3`, `3/2v1`, `3/2v1/op0`, `3/2v1/op0/1pawns`).
//! A selection unions hierarchy prefixes or names; exact and material names may mix.
//! Prefix/name unions and intersections are not supported. Prefixes select **both**
//! material orientations; material never silently exchanges STM and SNTM. A trailing slash
//! on a prefix is accepted. Piece counts range from two through eight.
//!
//! Generation adds a selected table's quiet-move twin, but does not recursively
//! add capture/promotion dependencies. Those prerequisites must already exist
//! or be explicitly selected. Selections and plans sort by piece count, then
//! pawn count, then name in KQRBNP order, so explicitly selected capture/promotion
//! prerequisites precede their consumers.

use std::collections::{BTreeMap, BTreeSet};

use shakmaty::{ByRole, Role};

use crate::error::{EgtError, EgtResult};
use crate::piece_set::{EgtElement, EgtSide, MAX_PIECES};
use crate::table_id::{
    PawnKey, TableId, get_file_combinations, is_canonical, pawn_order_combinations,
};

#[derive(Clone, Debug, Default)]
struct Prefix {
    n: Option<usize>,
    split: Option<(usize, usize)>,
    opposing: Option<usize>,
    pawns: Option<usize>,
}

impl Prefix {
    fn matches_material(&self, material: &Material) -> bool {
        let (a, b) = material.side_counts;
        self.n.is_none_or(|n| n == a + b)
            && self.split.is_none_or(|split| split == (a.max(b), a.min(b)))
            && self
                .pawns
                .is_none_or(|n| n == material.stm_pawns + material.sntm_pawns)
            && self
                .opposing
                .is_none_or(|n| n <= material.stm_pawns.min(material.sntm_pawns))
    }

    fn matches(&self, table: &TableId) -> bool {
        let (a, b) = table.side_piece_counts();
        self.n.is_none_or(|n| n == table.n_pieces())
            && self.split.is_none_or(|split| split == (a.max(b), a.min(b)))
            && self.opposing.is_none_or(|n| n == table.opposing_pairs())
            && self.pawns.is_none_or(|n| n == table.n_pawns())
    }
}

#[derive(Clone, Debug)]
struct Material {
    stm_pawns: usize,
    sntm_pawns: usize,
    pieces: Vec<EgtElement>,
    side_counts: (usize, usize),
}

impl Material {
    fn parse(name: &str) -> EgtResult<Self> {
        let invalid = |reason| invalid_name(name, reason);
        let (stm, sntm) = name
            .split_once('_')
            .ok_or_else(|| invalid("missing '_' separator"))?;
        let mut pieces = Vec::new();
        let mut pawns = [0; 2];
        let mut counts = [0; 2];
        for (idx, (text, side)) in [(stm, EgtSide::SideToMove), (sntm, EgtSide::SideNotToMove)]
            .into_iter()
            .enumerate()
        {
            let mut roles = ByRole::new_with(|_| 0usize);
            for c in text.chars() {
                let role = Role::from_char(c)
                    .filter(|_| c.is_ascii_uppercase())
                    .ok_or_else(|| invalid("material must contain only uppercase KQRBNP"))?;
                roles[role] += 1;
            }
            if roles.king != 1 {
                return Err(invalid("each side must have exactly one king"));
            }
            let mut canonical = String::new();
            for role in Role::ALL.into_iter().rev() {
                canonical.extend(std::iter::repeat_n(role.upper_char(), roles[role]));
                if role != Role::Pawn && roles[role] != 0 {
                    pieces.push(EgtElement::PieceGroup {
                        role,
                        side,
                        multiplicity: roles[role],
                    });
                }
            }
            if canonical != text {
                return Err(invalid("noncanonical material order; expected KQRBNP"));
            }
            pawns[idx] = roles.pawn;
            counts[idx] = text.len();
        }
        if !(2..=MAX_PIECES).contains(&(counts[0] + counts[1])) {
            return Err(invalid("piece count must be between two and eight"));
        }
        Ok(Self {
            stm_pawns: pawns[0],
            sntm_pawns: pawns[1],
            pieces,
            side_counts: (counts[0], counts[1]),
        })
    }

    fn tables(&self, prefix: &Prefix) -> EgtResult<Vec<TableId>> {
        let mut tables = Vec::new();
        if !prefix.matches_material(self) {
            return Ok(tables);
        }
        let stm = get_file_combinations(self.stm_pawns);
        let sntm = get_file_combinations(self.sntm_pawns);
        for a in &stm {
            for b in &sntm {
                if !is_canonical(a, b) {
                    continue;
                }
                for mut elements in pawn_order_combinations(a, b) {
                    if PawnKey::from_elements(&elements).canonicalized().1 {
                        continue;
                    }
                    elements.extend_from_slice(&self.pieces);
                    let table = TableId::from_elements(elements)?;
                    if prefix.matches(&table) {
                        tables.push(table);
                    }
                }
            }
        }
        Ok(tables)
    }
}

#[derive(Clone, Debug)]
enum Target {
    Hierarchy(Prefix),
    Material(Material),
    Exact(TableId),
}

/// A validated selector. Resolving enumerates theoretical identities, not files
/// present in storage, and never constructs full Egt indexing plans.
#[derive(Clone, Debug)]
pub struct TableSelector {
    source: String,
    target: Target,
}

impl TableSelector {
    pub fn parse(selector: &str) -> EgtResult<Self> {
        let invalid = |reason| invalid_name(selector, reason);
        let text = selector.strip_suffix('/').unwrap_or(selector);
        if text.is_empty() {
            return Err(invalid("empty selector"));
        }
        if text.contains('_') {
            if selector.contains('/') {
                return Err(invalid("material and exact selectors must be bare names"));
            }
            let target = if text.chars().any(|c| c.is_ascii_lowercase()) {
                Target::Exact(TableId::parse(text)?)
            } else {
                Target::Material(Material::parse(text)?)
            };
            return Ok(Self {
                source: selector.to_owned(),
                target,
            });
        }
        let parts: Vec<_> = text.split('/').collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(invalid("empty selector component"));
        }

        if parts.len() > 4 {
            return Err(invalid("too many hierarchy components"));
        }
        let mut prefix = Prefix::default();
        if let Some(part) = parts.first() {
            let n = decimal(part).ok_or_else(|| invalid("expected piece count"))?;
            if !(2..=MAX_PIECES).contains(&n) {
                return Err(invalid("piece count must be between two and eight"));
            }
            prefix.n = Some(n);
        }
        if let Some(part) = parts.get(1) {
            let (a, b) = part
                .split_once('v')
                .ok_or_else(|| invalid("expected maxvmin material split"))?;
            let a = decimal(a).ok_or_else(|| invalid("invalid material split"))?;
            let b = decimal(b).ok_or_else(|| invalid("invalid material split"))?;
            if b == 0 || a < b || a.checked_add(b) != prefix.n {
                return Err(invalid(
                    "material split must be maxvmin and sum to piece count",
                ));
            }
            prefix.split = Some((a, b));
        }
        if let Some(part) = parts.get(2) {
            let opposing = part
                .strip_prefix("op")
                .and_then(decimal)
                .ok_or_else(|| invalid("expected opK opposing-pair count"))?;
            if opposing > prefix.split.unwrap().1 - 1 {
                return Err(invalid("too many opposing pawn pairs"));
            }
            prefix.opposing = Some(opposing);
        }
        if let Some(part) = parts.get(3) {
            let pawns = part
                .strip_suffix("pawns")
                .and_then(decimal)
                .ok_or_else(|| invalid("expected Npawns pawn count"))?;
            if pawns > prefix.n.unwrap() - 2 || pawns < 2 * prefix.opposing.unwrap() {
                return Err(invalid(
                    "pawn count is inconsistent with piece/opposing-pair counts",
                ));
            }
            prefix.pawns = Some(pawns);
        }
        Ok(Self {
            source: selector.to_owned(),
            target: Target::Hierarchy(prefix),
        })
    }

    /// Returns identities in dependency order (pieces, then pawns), with
    /// table names ordered by KQRBNP, independently of I/O.
    pub fn resolve(&self) -> EgtResult<Vec<TableId>> {
        let tables = match &self.target {
            Target::Exact(table) => vec![table.clone()],
            Target::Material(material) => material.tables(&Prefix::default())?,
            Target::Hierarchy(prefix) => {
                let mut tables = Vec::new();
                for name in material_names(prefix.n.unwrap()) {
                    tables.extend(Material::parse(&name)?.tables(prefix)?);
                }
                tables
            }
        };
        if tables.is_empty() {
            return Err(invalid_name(&self.source, "selector contains no tables"));
        }
        Ok(tables
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
}

fn parse_selectors(selectors: &[&str]) -> EgtResult<Vec<TableSelector>> {
    let parsed = selectors
        .iter()
        .map(|selector| TableSelector::parse(selector))
        .collect::<EgtResult<Vec<_>>>()?;
    if let Some(first) = parsed.first() {
        for selector in &parsed[1..] {
            if matches!(first.target, Target::Hierarchy(_))
                != matches!(selector.target, Target::Hierarchy(_))
            {
                return Err(invalid_name(
                    &selector.source,
                    "selectors must all be hierarchy prefixes or names",
                ));
            }
        }
    }
    Ok(parsed)
}

/// Union of hierarchy prefixes or exact/material names, deduplicated and sorted
/// by pieces, then pawns, then name in KQRBNP order, grouping opposition variants.
pub fn resolve_selectors(selectors: &[&str]) -> EgtResult<Vec<TableId>> {
    let mut tables = BTreeSet::new();
    for selector in parse_selectors(selectors)? {
        tables.extend(selector.resolve()?);
    }
    Ok(tables.into_iter().collect())
}

/// Validates a prefix-only or name-only selection without enumerating tables. Hierarchy
/// prefixes are sorted, end in '/', and omit duplicates and covered descendants.
/// Empty input returns Some(empty); material and exact selections return None.
pub(crate) fn hierarchy_prefixes(selectors: &[&str]) -> EgtResult<Option<Vec<String>>> {
    let parsed = parse_selectors(selectors)?;
    if parsed
        .first()
        .is_some_and(|s| !matches!(s.target, Target::Hierarchy(_)))
    {
        return Ok(None);
    }
    let prefixes: BTreeSet<_> = parsed
        .iter()
        .map(|s| format!("{}/", s.source.trim_end_matches('/')))
        .collect();
    let mut disjoint: Vec<String> = Vec::new();
    for prefix in prefixes {
        if !disjoint
            .last()
            .is_some_and(|parent| prefix.starts_with(parent))
        {
            disjoint.push(prefix);
        }
    }
    Ok(Some(disjoint))
}

/// Exact quiet-move twins and their directional horizontal reflections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TablePair {
    pub(crate) table: TableId,
    pub(crate) twin: Option<TableId>,
    /// Table-to-twin and twin-to-table, including reflected self-twins.
    pub(crate) reflected: [bool; 2],
}

impl TablePair {
    pub fn table(&self) -> &TableId {
        &self.table
    }

    /// Absent for a self-twin, which uses a single working array.
    pub fn twin(&self) -> Option<&TableId> {
        self.twin.as_ref()
    }
}

/// Quiet-move pairs only; capture/promotion prerequisites are not auto-generated.
#[derive(Clone, Debug)]
pub struct GenerationPlan {
    /// Smaller `TableId` first; self-twins have no second table.
    /// Pairs use selection order: total pieces, then pawns, then representative
    /// name in KQRBNP order.
    pub pairs: Vec<TablePair>,
}

impl GenerationPlan {
    pub fn new(tables: impl IntoIterator<Item = TableId>) -> Self {
        let mut pairs = BTreeMap::new();
        for table in tables {
            let (twin, reflected) = table.twin();
            let reverse = PawnKey::from_elements(&twin.parse_elements()).twin().canonicalized().1;
            let (first, second, reflected) = if table == twin {
                (table, None, [reflected, reverse])
            } else if table < twin {
                (table, Some(twin), [reflected, reverse])
            } else {
                (twin, Some(table), [reverse, reflected])
            };
            pairs.insert(first, (second, reflected));
        }
        Self {
            pairs: pairs.into_iter().map(|(table, (twin, reflected))| TablePair { table, twin, reflected }).collect(),
        }
    }
}

fn decimal(text: &str) -> Option<usize> {
    if text.is_empty()
        || !text.bytes().all(|c| c.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

fn invalid_name(name: &str, reason: &'static str) -> EgtError {
    EgtError::InvalidEndgameName {
        name: name.to_owned(),
        reason,
    }
}

/// All oriented materials, including both sides of asymmetric pairs.
fn material_names(n: usize) -> Vec<String> {
    fn combinations(k: usize, start: usize, current: &mut String, result: &mut Vec<String>) {
        const ROLES: [char; 5] = ['Q', 'R', 'B', 'N', 'P'];
        if k == 0 {
            result.push(current.clone());
            return;
        }
        for i in start..ROLES.len() {
            current.push(ROLES[i]);
            combinations(k - 1, i, current, result);
            current.pop();
        }
    }
    let mut result = Vec::new();
    for stm in 0..=n - 2 {
        let mut a = Vec::new();
        let mut b = Vec::new();
        combinations(stm, 0, &mut String::new(), &mut a);
        combinations(n - 2 - stm, 0, &mut String::new(), &mut b);
        for a in &a {
            for b in &b {
                result.push(format!("K{a}_K{b}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(selector: &str) -> Vec<TableId> {
        TableSelector::parse(selector).unwrap().resolve().unwrap()
    }
    fn names(tables: &[TableId]) -> Vec<&str> {
        tables.iter().map(TableId::tablename).collect()
    }

    #[test]
    fn exact_vs_material_and_orientation() {
        assert_eq!(
            names(&resolve("KP_K")),
            ["KPa_K", "KPb_K", "KPc_K", "KPd_K"]
        );
        assert_eq!(
            names(&resolve("K_KP")),
            ["K_KPa", "K_KPb", "K_KPc", "K_KPd"]
        );
        assert_eq!(names(&resolve("KPa_K")), ["KPa_K"]);
        assert_eq!(names(&resolve("KQ_K")), ["KQ_K"]);
        assert_eq!(names(&resolve("K_K")), ["K_K"]);
        let union = resolve_selectors(&["KP_K", "K_KP", "KP_K"]).unwrap();
        assert_eq!(union.len(), 8);
        assert_eq!(union, resolve("3/2v1/op0/1pawns"));
        assert!(resolve_selectors(&[]).unwrap().is_empty());
    }

    #[test]
    fn hierarchy_selects_both_orientations() {
        assert_eq!(resolve("3"), resolve("3/"));
        assert_eq!(resolve("3"), resolve("3/2v1"));
        assert_eq!(
            resolve("3/2v1/op0/1pawns"),
            resolve_selectors(&["KP_K", "K_KP"]).unwrap()
        );
        let opposed = resolve("4/2v2/op1/2pawns");
        assert_eq!(opposed.len(), 4);
        assert!(opposed.iter().all(|t| t.opposing_pairs() == 1));
        assert_eq!(resolve("4/2v2/op0/2pawns").len(), 32);
        for table in resolve("4") {
            let path = table.relative_path("ggegt");
            let prefix = path.rsplit_once('/').unwrap().0;
            assert!(resolve(prefix).contains(&table), "{path}");
            let exact = path.strip_suffix(".ggegt").unwrap();
            assert!(TableSelector::parse(exact).is_err());
            assert_eq!(resolve(table.tablename()), vec![table]);
        }
    }

    #[test]
    fn malformed_and_intersection_selectors_fail() {
        for selector in [
            "",
            "/",
            "1",
            "9",
            "03",
            "3//",
            "3/1v2",
            "4/2v1",
            "4/4v0",
            "3/2v1/op1",
            "4/2v2/op0/3pawns",
            "4/2v2/op1/1pawns",
            "3/2v1/1pawns",
            "3/KQ_KR",
            "3/KQ_K",
            "3/K_KP",
            "3/2v1/KPa_K",
            "3/2v1/op0/1pawns/KP_K",
            "4/2v2/op1/KP_KP",
            "4/3v1/op1",
            "3/2v1/op0/0pawns/KP_K",
            "4/3v1/op1/KPP_K",
            "KPh_K",
            "PK_K",
            "KPPPPPPP_K",
            "3/2v1/op0/1pawns/extra",
            "KPa_K/",
            "3/2v1/op00",
            " 3",
            "3 ",
        ] {
            let result = TableSelector::parse(selector).and_then(|s| s.resolve());
            assert!(result.is_err(), "unexpectedly accepted {selector:?}");
        }
    }

    #[test]
    fn unions_separate_hierarchy_from_names() {
        for selectors in [["3", "KP_K"], ["3", "KPa_K"]] {
            for selectors in [selectors, [selectors[1], selectors[0]]] {
                assert!(resolve_selectors(&selectors).is_err(), "{selectors:?}");
                assert!(hierarchy_prefixes(&selectors).is_err(), "{selectors:?}");
            }
        }
        for (selectors, expected) in [
            (["KP_K", "KPa_K", "KP_K"], vec!["KPa_K", "KPb_K", "KPc_K", "KPd_K"]),
            (["KPa_K", "KQ_K", "KPa_K"], vec!["KQ_K", "KPa_K"]),
            (["KPb_K", "KPa_K", "KPb_K"], vec!["KPa_K", "KPb_K"]),
            (["KR_K", "KQ_K", "KR_K"], vec!["KQ_K", "KR_K"]),
        ] {
            for selectors in [selectors, [selectors[1], selectors[0], selectors[2]]] {
                assert_eq!(names(&resolve_selectors(&selectors).unwrap()), expected);
                assert_eq!(hierarchy_prefixes(&selectors).unwrap(), None);
            }
        }
    }

    #[test]
    fn hierarchy_prefixes_are_canonical_and_disjoint() {
        assert_eq!(hierarchy_prefixes(&[]).unwrap(), Some(vec![]));
        assert_eq!(
            hierarchy_prefixes(&[
                "4/2v2/op1/2pawns/",
                "3/2v1",
                "4/2v2/op0/2pawns",
                "3/",
                "4/2v2/op1",
                "3",
                "4/2v2/op0/2pawns/",
            ])
            .unwrap(),
            Some(vec![
                "3/".into(),
                "4/2v2/op0/2pawns/".into(),
                "4/2v2/op1/".into(),
            ])
        );
        assert_eq!(hierarchy_prefixes(&["KP_K", "KQ_K"]).unwrap(), None);
        assert_eq!(hierarchy_prefixes(&["KPa_K", "K_KPa"]).unwrap(), None);
        // Validate every selector, even descendants covered by a valid ancestor.
        for invalid in ["4/3v1/op1", "4/2v2/op1/1pawns", "4/KPa_K", "KPh_K"] {
            assert!(hierarchy_prefixes(&["4", invalid]).is_err(), "{invalid}");
            assert!(hierarchy_prefixes(&[invalid]).is_err(), "{invalid}");
        }
        // Validation must not expand the very large eight-piece selection.
        assert_eq!(
            hierarchy_prefixes(&["8/4v4", "8"]).unwrap(),
            Some(vec!["8/".into()])
        );
    }

    #[test]
    fn small_enumeration_counts_and_uniqueness() {
        for (n, expected) in [(2, 1), (3, 16), (4, 176)] {
            let tables = resolve(&n.to_string());
            assert_eq!(tables.len(), expected, "{n} pieces");
            assert!(tables.windows(2).all(|w| w[0] < w[1]));
            assert_eq!(tables.iter().collect::<BTreeSet<_>>().len(), expected);
            assert!(tables.iter().all(|table| table.n_pieces() == n));
            let from_materials = resolve_selectors(
                &material_names(n)
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            assert_eq!(tables, from_materials);
        }
        // Material-only combinatorics cover the full supported range cheaply;
        // do not expand all eight-piece pawn layouts in a unit test.
        for (n, count) in [
            (2, 1),
            (3, 10),
            (4, 55),
            (5, 220),
            (6, 715),
            (7, 2002),
            (8, 5005),
        ] {
            let materials = material_names(n);
            assert_eq!(materials.len(), count);
            assert_eq!(materials.iter().collect::<BTreeSet<_>>().len(), count);
            for material in materials {
                Material::parse(&material).unwrap();
            }
        }
        for n in 2..=8 {
            let name = format!("K{}_K", "Q".repeat(n - 2));
            assert_eq!(resolve(&name)[0].n_pieces(), n);
        }
        assert_eq!(resolve("KPP_K").len(), 20);
        assert_eq!(resolve("KP_KP").len(), 36);
        // 144 canonical file multisets, plus 8 extra same-file words and
        // 28 extra words with the enemy sharing one of two distinct files.
        assert_eq!(resolve("KPP_KP").len(), 180);
        let tables = resolve("KPP_KPP");
        let selected = names(&tables);
        assert!(selected.contains(&"KPaPho_KPaPho"));
        assert!(!selected.contains(&"KPaoPh_KPaoPh"));
        assert!(selected.contains(&"KPaoPho_KPaoPho"));
    }

    #[test]
    fn selections_follow_dependency_and_piece_order() {
        let tables = resolve_selectors(&["3/", "2/", "3/2v1"]).unwrap();
        assert_eq!(
            names(&tables),
            [
                "K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "K_KQ", "K_KR", "K_KB", "K_KN", "KPa_K",
                "KPb_K", "KPc_K", "KPd_K", "K_KPa", "K_KPb", "K_KPc", "K_KPd",
            ]
        );
        assert_eq!(&tables[1..], resolve("3/"));
        assert_eq!(tables, resolve_selectors(&["2/", "3/"]).unwrap());

        // Fewer pieces take priority over fewer pawns; promotions precede pawns
        // even when their names would sort later in the piece alphabet.
        let tables = resolve_selectors(&["KQPa_K", "KPa_K", "K_KN", "KQQ_K"]).unwrap();
        assert_eq!(names(&tables), ["K_KN", "KPa_K", "KQQ_K", "KQPa_K"]);
        assert_eq!(names(&resolve("KQPa_K")), ["KQPa_K"]);
    }

    #[test]
    fn opposition_variants_are_adjacent() {
        let tables = resolve("KP_KP");
        let plan = GenerationPlan::new(tables.clone());
        let representatives: Vec<_> = plan.pairs.into_iter().map(|pair| pair.table).collect();
        for tables in [&tables, &representatives] {
            for file in ['a', 'b', 'c', 'd'] {
                let unopposed = format!("KP{file}_KP{file}");
                let opposed = format!("KP{file}o_KP{file}o");
                assert!(tables.windows(2).any(|pair| {
                    pair[0].tablename() == opposed && pair[1].tablename() == unopposed
                }));
            }
        }
    }

    #[test]
    fn plan_uses_selection_order_for_pair_representatives() {
        let plan = GenerationPlan::new(resolve_selectors(&["3/", "2/"]).unwrap());
        let representatives: Vec<_> = plan.pairs.iter().map(|pair| pair.table.clone()).collect();
        assert_eq!(
            names(&representatives),
            [
                "K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KPa_K", "KPb_K", "KPc_K", "KPd_K"
            ]
        );
    }

    #[test]
    fn plan_adds_twins_deduplicates_and_orders_prerequisites() {
        let selected: Vec<_> = [
            "KPa_K", "K_KPa", "KQ_K", "K_K", "KPd_KPe", "KQR_K", "KQPa_K",
        ]
        .into_iter()
        .map(|name| TableId::parse(name).unwrap())
        .collect();
        let plan = GenerationPlan::new(selected.clone());
        assert_eq!(plan.pairs.len(), 6);
        assert!(plan.pairs.windows(2).all(|w| w[0].table < w[1].table));
        for pair in &plan.pairs {
            let first = pair.table();
            match pair.twin() {
                Some(second) => {
                    assert!(first < second);
                    assert_eq!(first.twin().0, *second);
                }
                None => assert_eq!(first.twin().0, *first),
            }
        }
        for table in selected {
            assert!(
                plan.pairs
                    .iter()
                    .any(|pair| pair.table == table || pair.twin.as_ref() == Some(&table))
            );
        }
        let only_pawn = GenerationPlan::new(resolve("KPa_K"));
        assert_eq!(only_pawn.pairs.len(), 1);
        assert!(only_pawn.pairs.iter().all(|pair| pair.table.n_pawns() == 1));
        let self_twin = GenerationPlan::new(resolve("KPd_KPe"));
        assert_eq!(self_twin.pairs[0].twin(), None);
        assert!(GenerationPlan::new(Vec::<TableId>::new()).pairs.is_empty());
        // Plans are invariant under input order, duplicates, and selecting a twin.
        let a = GenerationPlan::new(resolve("KPa_K"));
        let b = GenerationPlan::new(resolve_selectors(&["K_KPa", "KPa_K", "KPa_K"]).unwrap());
        assert_eq!(a.pairs, b.pairs);
    }
}
