use std::collections::BTreeSet;

use super::{ByRole, EgtElement, EgtSide, PawnKey, Role, TableId, format_name};

// Zero is empty, one is STM, two is SNTM; indices are normalized ranks 1..8.
type Layout = [u8; 8];

fn layouts(word: u8) -> Vec<Layout> {
    let len = word.ilog2();
    (0u8..64)
        .filter(|mask| mask.count_ones() == len)
        .map(|mask| {
            let mut layout = [0; 8];
            let mut slot = 0;
            for rank in 0..6 {
                if mask & (1 << rank) != 0 {
                    layout[rank + 1] = 1 + ((word >> slot) & 1);
                    slot += 1;
                }
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
        let elements = self.parse_elements();
        let key = PawnKey::from_elements(&elements);
        let mut counts = [ByRole::new_with(|_| 0usize); 2];
        for element in elements {
            if let EgtElement::PieceGroup {
                role,
                side,
                multiplicity,
            } = element
            {
                counts[usize::from(side == EgtSide::SideNotToMove)][role] = multiplicity;
            }
        }
        let victims: Vec<_> = Role::ALL
            .into_iter()
            .filter(|&role| role != Role::King && role != Role::Pawn && counts[1][role] != 0)
            .collect();
        let mut dependencies = BTreeSet::new();
        let mut emit = |key: PawnKey, captured: Option<Role>, promoted: Option<Role>| {
            let key = key.twin().canonicalized().0;
            dependencies.insert(TableId(
                format_name(key, |side, role| {
                    let original_stm = side == EgtSide::SideNotToMove;
                    counts[usize::from(!original_stm)][role]
                        + usize::from(original_stm && promoted == Some(role))
                        - usize::from(!original_stm && captured == Some(role))
                })
                .to_string(),
            ));
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
