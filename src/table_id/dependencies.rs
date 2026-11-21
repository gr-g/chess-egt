use std::collections::BTreeSet;

use super::{EgtSide, NONPAWN_ROLES, PawnKey, Role, TableId, role_index, sides};

// Zero is empty, one is STM, two is SNTM; indices are normalized ranks 1..8.
type Layout = [u8; 8];

fn layouts(word: u8) -> Vec<Layout> {
    let len = word.ilog2();
    (0u8..64)
        .filter(|mask| mask.count_ones() == len)
        .map(|mask| {
            let mut layout = [0; 8];
            let ranks = (0..6).filter(|&rank| mask & (1 << rank) != 0);
            for (rank, side) in ranks.zip(sides(word)) {
                layout[rank + 1] = if side == EgtSide::SideToMove { 1 } else { 2 };
            }
            layout
        })
        .collect()
}

fn word(layout: Layout) -> u8 {
    let mut word = 0;
    let mut len = 0;
    for pawn in layout.into_iter().filter(|&pawn| pawn != 0) {
        word |= (pawn - 1) << len;
        len += 1;
    }
    word | (1 << len)
}

impl TableId {
    /// Sorted direct conversion successors, with exact pawn orders and rank feasibility.
    /// Every entry is required; this filters pawn geometry, not full legal reachability.
    pub(crate) fn dependencies(&self) -> Vec<TableId> {
        let key = self.pawns;
        let counts = self.pieces;
        let victims: Vec<_> = NONPAWN_ROLES
            .into_iter()
            .filter(|&role| role != Role::King && counts[1][role_index(role)] != 0)
            .collect();
        let mut dependencies = BTreeSet::new();
        let mut emit = |key: PawnKey, captured: Option<Role>, promoted: Option<Role>| {
            let key = key.twin().canonicalized().0;
            let mut pieces = [counts[1], counts[0]];
            if let Some(role) = captured {
                pieces[0][role_index(role)] -= 1;
            }
            if let Some(role) = promoted {
                pieces[1][role_index(role)] += 1;
            }
            dependencies.insert(TableId { pawns: key, pieces });
        };

        // A king is always available as a nonpawn attacker.
        for &role in &victims {
            emit(key, Some(role), None);
        }
        let layouts: [Vec<Layout>; 8] = key.files.map(layouts);
        for file in 0..8 {
            for rank in 1..7 {
                if layouts[file][0][rank] == 2 {
                    let mut layout = layouts[file][0];
                    layout[rank] = 0;
                    let mut next = key;
                    next.files[file] = word(layout);
                    emit(next, None, None);
                }
            }
            for &source in &layouts[file] {
                for rank in 1..7 {
                    if source[rank] != 1 {
                        continue;
                    }
                    let mut removed = source;
                    removed[rank] = 0;
                    let mut next = key;
                    next.files[file] = word(removed);
                    if rank == 6 {
                        // Rank seven is possible only for the last pawn in the file word.
                        for promoted in [Role::Queen, Role::Rook, Role::Bishop, Role::Knight] {
                            emit(next, None, Some(promoted));
                            for &role in &victims {
                                emit(next, Some(role), Some(promoted));
                            }
                        }
                        continue;
                    }
                    for adjacent in [file.checked_sub(1), (file < 7).then_some(file + 1)]
                        .into_iter()
                        .flatten()
                    {
                        for &target in &layouts[adjacent] {
                            let mut changed = target;
                            changed[rank + 1] = 1;
                            let mut captured = next;
                            captured.files[adjacent] = word(changed);
                            match target[rank + 1] {
                                2 => emit(captured, None, None),
                                0 => {
                                    for &role in &victims {
                                        emit(captured, Some(role), None);
                                    }
                                }
                                _ => {}
                            }
                            // The preceding double push also requires ranks six and seven clear.
                            if rank == 4 && target[4] == 2 && target[5] == 0 && target[6] == 0 {
                                changed[4] = 0;
                                captured.files[adjacent] = word(changed);
                                emit(captured, None, None);
                            }
                        }
                    }
                }
            }
        }
        dependencies.into_iter().collect()
    }
}
