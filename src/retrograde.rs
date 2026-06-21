use shakmaty::{Color, Chess, Position, Role};
use shakmaty::retrograde::{RetrogradeAnalysis, CastlingRetrogradeMode};
use crate::{ConversionType, EgtGenerator};
use crate::egt_file::{MaybeDtcOutcome, EgtFile, PawnKey, reflect_files, is_canonical, mirror_horizontally};
use crate::piece_set::{EgtRole, EgtSide};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgtHandle {
    pub name: String,
    pub file_idx: usize,
    pub egt_idx: usize,
}

struct DepthQueues {
    win_checkmate: Vec<usize>,
    win_capture: Vec<usize>,
    win_promotion: Vec<usize>,

    loss_checkmate: Vec<usize>,
    loss_capture: Vec<usize>,
    loss_promotion: Vec<usize>,
}

impl DepthQueues {
    fn new() -> Self {
        Self {
            win_checkmate: Vec::new(),
            win_capture: Vec::new(),
            win_promotion: Vec::new(),
            loss_checkmate: Vec::new(),
            loss_capture: Vec::new(),
            loss_promotion: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.win_checkmate.is_empty()
            && self.win_capture.is_empty()
            && self.win_promotion.is_empty()
            && self.loss_checkmate.is_empty()
            && self.loss_capture.is_empty()
            && self.loss_promotion.is_empty()
    }

    fn push_win(&mut self, idx: usize, ct: ConversionType) {
        match ct {
            ConversionType::Checkmate => self.win_checkmate.push(idx),
            ConversionType::Capture => self.win_capture.push(idx),
            ConversionType::Promotion => self.win_promotion.push(idx),
        }
    }

    fn push_loss(&mut self, idx: usize, ct: ConversionType) {
        match ct {
            ConversionType::Checkmate => self.loss_checkmate.push(idx),
            ConversionType::Capture => self.loss_capture.push(idx),
            ConversionType::Promotion => self.loss_promotion.push(idx),
        }
    }

    fn merge(&mut self, other: &mut Self) {
        self.win_checkmate.append(&mut other.win_checkmate);
        self.win_capture.append(&mut other.win_capture);
        self.win_promotion.append(&mut other.win_promotion);
        self.loss_checkmate.append(&mut other.loss_checkmate);
        self.loss_capture.append(&mut other.loss_capture);
        self.loss_promotion.append(&mut other.loss_promotion);
    }
}

pub struct RetrogradeSolver {
    pub files: Vec<EgtFile>,
    pub is_symmetric: bool,
}

impl RetrogradeSolver {
    pub fn new(file_a: EgtFile, file_b: Option<EgtFile>) -> Self {
        let is_symmetric = file_b.is_none();
        let mut files = vec![file_a];
        if let Some(fb) = file_b {
            files.push(fb);
        }
        Self {
            files,
            is_symmetric,
        }
    }

    pub fn read_outcome(&mut self, handle: &EgtHandle, local_index: usize) -> MaybeDtcOutcome {
        let file = &mut self.files[handle.file_idx];
        let global_index = file.get_global_index(handle.egt_idx, local_index);
        file.read_from_index(global_index).unwrap()
    }

    pub fn write_outcome(&mut self, handle: &EgtHandle, local_index: usize, outcome: MaybeDtcOutcome) {
        let file = &mut self.files[handle.file_idx];
        let global_index = file.get_global_index(handle.egt_idx, local_index);
        file.write_to_index(global_index, outcome).unwrap();
    }
}

fn get_pawn_files(pieces: &[(EgtRole, EgtSide, usize)]) -> (Vec<shakmaty::File>, Vec<shakmaty::File>) {
    let mut stm_files = Vec::new();
    let mut sntm_files = Vec::new();
    for &(piece, side, multiplicity) in pieces {
        if let EgtRole::Pawn(file) = piece {
            for _ in 0..multiplicity {
                match side {
                    EgtSide::SideToMove => stm_files.push(file),
                    EgtSide::SideNotToMove => sntm_files.push(file),
                }
            }
        }
    }
    stm_files.sort_by_key(|f| f.to_usize());
    sntm_files.sort_by_key(|f| f.to_usize());
    (stm_files, sntm_files)
}

fn both_kings_on_diagonal(position: &Chess) -> bool {
    // This function checks both long diagonals: if the kings are
    // on the h1-a8 diagonal, they will actually be on the a1-h8
    // diagonal after canonicalization.
    let kings1 = position.board().by_role(Role::King);
    let kings2 = kings1.flip_horizontal();
    kings1.into_iter().all(|sq| sq.rank().to_usize() == sq.file().to_usize()) ||
        kings2.into_iter().all(|sq| sq.rank().to_usize() == sq.file().to_usize())
}

pub fn quiet_unmoves<F>(
    solver: &mut RetrogradeSolver,
    table: &EgtHandle,
    twin: &EgtHandle,
    local_index: usize,
    mut f: F,
) where
    F: FnMut(&mut RetrogradeSolver, usize),
{
    let position = {
        let file = &mut solver.files[table.file_idx];
        file.egts[table.egt_idx].position_from_index(local_index, Color::White)
    };
    let position = match position {
        Some(s) => s,
        _ => return,
    };

    let (stm_files_a, sntm_files_a) = get_pawn_files(solver.files[table.file_idx].egts[table.egt_idx].pieces());
    let (stm_files_b, sntm_files_b) = get_pawn_files(solver.files[twin.file_idx].egts[twin.egt_idx].pieces());
    let mirrored = stm_files_b != sntm_files_a || sntm_files_b != stm_files_a;

    let current_on_diagonal = both_kings_on_diagonal(&position);

    RetrogradeAnalysis::new(&position)
        .with_castling_mode(CastlingRetrogradeMode::NoCastling)
        .quiet_unmoves(|mut pred_position, _m| {
            if mirrored {
                pred_position = mirror_horizontally(&pred_position);
            }

            let pred_idx = {
                let twin_file = &mut solver.files[twin.file_idx];
                twin_file.egts[twin.egt_idx].position_to_index(&pred_position)
            };
            f(solver, pred_idx);

            // Let's say a canonical position `p` has `#p=8` if it represents 8 equivalent
            // positions and `#p=4` if it represents 4 equivalent positions (with our choice
            // of canonicalization, `#p=4` positions are positions with both kings on the
            // a1-h8 diagonal). When retrograde propagation from `p'` finds move `p -> p'`
            // with `#p=4` and `#p'=8`, in addition to decrementing the counter for p, the
            // counter for the reflection of `p` along the diagonal should also be decremented
            // (since the symmetric move contributed to the counter for the reflection of `p`
            // but led to a non-canonical position).
            if !current_on_diagonal {
                let maybe_reflected_idx = {
                    let twin_file = &mut solver.files[twin.file_idx];
                    twin_file.egts[twin.egt_idx].diagonal_symmetric(pred_idx)
                };
                if let Some(reflected_idx) = maybe_reflected_idx {
                    // The current index represents 8 positions, while the predecessor index
                    // represents 4 positions. Push the diagonal reflection of the predecessor.
                    f(solver, reflected_idx);
                }
            }
        });
}

fn symmetry_adjusted_move_counter(position: &Chess) -> u16 {
    let current_on_diagonal = both_kings_on_diagonal(position);

    let mut counter = 0;
    for m in position.legal_moves() {
        if m.is_promotion() || m.is_capture() {
            // For positions in simpler tables we don't generate unmoves
            // (we visit them only forward, once during initialization),
            // so there is no adjustment of the counter.
            counter += 1;
            continue;
        }
        let mut successor_position = position.clone();
        successor_position.play_unchecked(m);

        // Let's say a canonical position `p` has `#p=8` if it represents 8
        // equivalent positions and `#p=4` if it represents 4 equivalent positions
        // (with our choice of canonicalization, `#p=4` positions are positions
        // with both kings on the a1-h8 diagonal). When initializing the counters,
        // if there is a legal move `p -> p'` with `#p=8` and `#p'=4`, then there
        // is a move (the symmetric along the diagonal) which goes from a non canonical
        // position (the reflection of `p` along the diagonal) to a canonical position
        // (the reflection of `p'` along the diagonal), which will be explored during
        // backward propagation. To account for this, moves `p -> p'` with `#p=8` and
        // `#p'=4` should increment the counter by 2 during initialization.
        if !current_on_diagonal && both_kings_on_diagonal(&successor_position) {
            counter += 2;
        } else {
            counter += 1;
        }
    }
    counter
}

pub struct DependencyCache {
    pub cache: HashMap<String, EgtFile>,
    pub base_path: std::path::PathBuf,
}

impl DependencyCache {
    pub fn new(base_path: &std::path::Path) -> Self {
        Self {
            cache: HashMap::new(),
            base_path: base_path.to_path_buf(),
        }
    }

    pub fn get_or_load(&mut self, endgame: &str) -> &mut EgtFile {
        if !self.cache.contains_key(endgame) {
            let file_from_disk = EgtFile::new_from_file(&self.base_path, endgame);
            let file = if file_from_disk.is_ok() {
                file_from_disk.unwrap()
            } else {
                println!("Dependency endgame {} not found. Generating on the fly...", endgame);
                let g = EgtGenerator::new(&self.base_path);
                g.generate(endgame);
                EgtFile::new_from_file(&self.base_path, endgame).unwrap()
            };
            self.cache.insert(endgame.to_string(), file);
        }
        self.cache.get_mut(endgame).unwrap()
    }
}

fn initialize_table(
    solver: &mut RetrogradeSolver,
    table: &EgtHandle,
    twin: &EgtHandle,
    dep_cache: &mut DependencyCache,
    table_queues: &mut DepthQueues,
    twin_queues: &mut DepthQueues,
) -> (usize, usize, usize) {
    let (size, pawnless) = {
        let egt = &solver.files[table.file_idx].egts[table.egt_idx];
        (egt.index_range(), egt.is_pawnless())
    };

    let mut checkmate_count = 0;
    let mut stalemate_count = 0;
    let mut unknown_count = 0;

    for idx in 0..size {
        // Decode position using Color::White as side-to-move
        let position_opt = {
            let file = &mut solver.files[table.file_idx];
            file.egts[table.egt_idx].position_from_index(idx, Color::White)
        };

        if (idx+1) % 10000000 == 0 {
            println!("Scanned {}/{} indexes...", idx+1, size);
        }

        if position_opt.is_none() {
            solver.write_outcome(table, idx, MaybeDtcOutcome::INVALID);
            continue;
        }

        let position = position_opt.unwrap();
        let legals = position.legal_moves();

        if legals.is_empty() {
            if position.is_check() {
                // Checkmate!
                solver.write_outcome(table, idx, MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0));
                checkmate_count += 1;
                // Add predecessors to twin's loss-to-win queue (depth 1)
                quiet_unmoves(solver, table, twin, idx, |_solver, pred_idx| {
                    twin_queues.push_win(pred_idx, ConversionType::Checkmate);
                });
            } else {
                // Stalemate!
                solver.write_outcome(table, idx, MaybeDtcOutcome::DRAW);
                stalemate_count += 1;
            }
        } else {
            // Unknown position, initialize move counter and probe dependencies
            let counter = if pawnless {
                symmetry_adjusted_move_counter(&position)
            } else {
                legals.len() as u16
            };

            solver.write_outcome(table, idx, MaybeDtcOutcome::new_unknown(counter));
            unknown_count += 1;

            for m in legals {
                let is_capture = m.is_capture();
                let is_promotion = m.is_promotion();

                if is_capture || is_promotion {
                    let mut successor_position = position.clone();
                    successor_position.play_unchecked(m);
                    let dep_endgame = crate::get_endgame(&successor_position);

                    // Probe the dependency table
                    let dep_outcome = dep_cache.get_or_load(&dep_endgame).probe(&successor_position).unwrap();

                    let ct = if is_capture { ConversionType::Capture } else { ConversionType::Promotion };
                    if dep_outcome.is_loss() {
                        // Successor is a loss, so this is a win-in-1.
                        // Add to loss-to-win queue (depth 1)
                        table_queues.push_win(idx, ct);
                    } else if dep_outcome.is_win() {
                        // Successor is a win, so we must decrement the counter.
                        // Add to win-to-loss queue (depth 1)
                        table_queues.push_loss(idx, ct);
                    }
                }
            }
        }
    }

    (checkmate_count, stalemate_count, unknown_count)
}

fn propagate_loss_to_win(
    solver: &mut RetrogradeSolver,
    table: &EgtHandle,
    twin: &EgtHandle,
    idx: usize,
    plies: u16,
    ct: ConversionType,
    twin_next_queues: &mut DepthQueues,
) -> bool {
    let outcome = solver.read_outcome(table, idx);
    if outcome.is_unknown() {
        solver.write_outcome(table, idx, MaybeDtcOutcome::new_win(ct, plies));
        quiet_unmoves(solver, table, twin, idx, |_solver, pred_idx| {
            twin_next_queues.push_loss(pred_idx, ct);
        });
        true
    } else {
        false
    }
}

fn propagate_win_to_loss(
    solver: &mut RetrogradeSolver,
    table: &EgtHandle,
    twin: &EgtHandle,
    idx: usize,
    plies: u16,
    ct: ConversionType,
    twin_next_queues: &mut DepthQueues,
) -> bool {
    let outcome = solver.read_outcome(table, idx);
    if outcome.is_unknown() {
        let counter = outcome.get_unknown_counter();
        assert!(counter > 0);
        if counter == 1 {
            solver.write_outcome(table, idx, MaybeDtcOutcome::new_loss(ct, plies));
            quiet_unmoves(solver, table, twin, idx, |_solver, pred_idx| {
                twin_next_queues.push_win(pred_idx, ct);
            });
            true
        } else {
            solver.write_outcome(table, idx, MaybeDtcOutcome::new_unknown(counter - 1));
            false
        }
    } else {
        false
    }
}

pub fn retrograde_analysis(base_path: &std::path::Path, endgame: &str) -> (EgtFile, Option<EgtFile>) {
    let parts: Vec<&str> = endgame.split('_').collect();
    assert_eq!(parts.len(), 2);
    let twin_endgame = format!("{}_{}", parts[1], parts[0]);

    let is_symmetric = endgame == twin_endgame;

    let file_a = EgtFile::new(&base_path.to_path_buf(), endgame).unwrap();
    let file_b = if is_symmetric {
        None
    } else {
        Some(EgtFile::new(&base_path.to_path_buf(), &twin_endgame).unwrap())
    };

    let mut solver = RetrogradeSolver::new(file_a, file_b);

    // Match Egt sub-tables into pairs
    let mut table_pairs = Vec::new();

    for (egt_idx_a, egt_a) in solver.files[0].egts.iter().enumerate() {
        let (stm_files, sntm_files) = get_pawn_files(egt_a.pieces());
        let (twin_stm, twin_sntm) = if is_canonical(&sntm_files, &stm_files) {
            (sntm_files, stm_files)
        } else {
            (reflect_files(&sntm_files), reflect_files(&stm_files))
        };
        let twin_key = PawnKey::new(&twin_stm, &twin_sntm);

        let (file_idx_b, egt_idx_b, name_b) = if solver.is_symmetric {
            let egt_idx_b = *solver.files[0].egt_map.get(&twin_key).unwrap();
            (0, egt_idx_b, solver.files[0].egts[egt_idx_b].tablename().to_string())
        } else {
            let egt_idx_b = *solver.files[1].egt_map.get(&twin_key).unwrap();
            (1, egt_idx_b, solver.files[1].egts[egt_idx_b].tablename().to_string())
        };

        if solver.is_symmetric && egt_idx_a > egt_idx_b {
            continue;
        }

        let table_a = EgtHandle {
            name: egt_a.tablename().to_string(),
            file_idx: 0,
            egt_idx: egt_idx_a,
        };

        let table_b = EgtHandle {
            name: name_b,
            file_idx: file_idx_b,
            egt_idx: egt_idx_b,
        };

        table_pairs.push((table_a, table_b));
    }

    // Initialization & Propagation Phase for each independent pair
    let mut dep_cache = DependencyCache::new(base_path);
    for (table_a, table_b) in &table_pairs {
        let mut queues_a = DepthQueues::new();
        let mut queues_b = DepthQueues::new();

        let (checkmates, stalemates, _) =
            initialize_table(&mut solver, table_a, table_b, &mut dep_cache, &mut queues_a, &mut queues_b);
        println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table_a.name, checkmates, stalemates);

        if table_a != table_b {
            let (checkmates, stalemates, _) =
                initialize_table(&mut solver, table_b, table_a, &mut dep_cache, &mut queues_b, &mut queues_a);
                println!("{}: Initialized with {} checkmated positions and {} stalemated positions.", table_b.name, checkmates, stalemates);
        } else {
            queues_a.merge(&mut queues_b);
        }

        let mut plies = 1;
        while !queues_a.is_empty() || !queues_b.is_empty() {
            let mut next_queues_a = DepthQueues::new();
            let mut next_queues_b = DepthQueues::new();

            let mut wins_found_a = 0;
            let mut losses_found_a = 0;
            let mut wins_found_b = 0;
            let mut losses_found_b = 0;

            // 1. Process loss-to-win queues (marking wins at depth `plies`)
            if table_a == table_b {
                for &idx in &queues_a.win_checkmate {
                    if propagate_loss_to_win(&mut solver, table_a, table_a, idx, plies, ConversionType::Checkmate, &mut next_queues_a) {
                        wins_found_a += 1;
                    }
                }
                for &idx in &queues_a.win_capture {
                    if propagate_loss_to_win(&mut solver, table_a, table_a, idx, plies, ConversionType::Capture, &mut next_queues_a) {
                        wins_found_a += 1;
                    }
                }
                for &idx in &queues_a.win_promotion {
                    if propagate_loss_to_win(&mut solver, table_a, table_a, idx, plies, ConversionType::Promotion, &mut next_queues_a) {
                        wins_found_a += 1;
                    }
                }
            } else {
                // Table A wins propagate to Table B losses
                for &idx in &queues_a.win_checkmate {
                    if propagate_loss_to_win(&mut solver, table_a, table_b, idx, plies, ConversionType::Checkmate, &mut next_queues_b) {
                        wins_found_a += 1;
                    }
                }
                for &idx in &queues_a.win_capture {
                    if propagate_loss_to_win(&mut solver, table_a, table_b, idx, plies, ConversionType::Capture, &mut next_queues_b) {
                        wins_found_a += 1;
                    }
                }
                for &idx in &queues_a.win_promotion {
                    if propagate_loss_to_win(&mut solver, table_a, table_b, idx, plies, ConversionType::Promotion, &mut next_queues_b) {
                        wins_found_a += 1;
                    }
                }

                // Table B wins propagate to Table A losses
                for &idx in &queues_b.win_checkmate {
                    if propagate_loss_to_win(&mut solver, table_b, table_a, idx, plies, ConversionType::Checkmate, &mut next_queues_a) {
                        wins_found_b += 1;
                    }
                }
                for &idx in &queues_b.win_capture {
                    if propagate_loss_to_win(&mut solver, table_b, table_a, idx, plies, ConversionType::Capture, &mut next_queues_a) {
                        wins_found_b += 1;
                    }
                }
                for &idx in &queues_b.win_promotion {
                    if propagate_loss_to_win(&mut solver, table_b, table_a, idx, plies, ConversionType::Promotion, &mut next_queues_a) {
                        wins_found_b += 1;
                    }
                }
            }

            // 2. Process win-to-loss queues (decrementing counters and marking losses at depth `plies`)
            if table_a == table_b {
                for &idx in &queues_a.loss_checkmate {
                    if propagate_win_to_loss(&mut solver, table_a, table_a, idx, plies, ConversionType::Checkmate, &mut next_queues_a) {
                        losses_found_a += 1;
                    }
                }
                for &idx in &queues_a.loss_capture {
                    if propagate_win_to_loss(&mut solver, table_a, table_a, idx, plies, ConversionType::Capture, &mut next_queues_a) {
                        losses_found_a += 1;
                    }
                }
                for &idx in &queues_a.loss_promotion {
                    if propagate_win_to_loss(&mut solver, table_a, table_a, idx, plies, ConversionType::Promotion, &mut next_queues_a) {
                        losses_found_a += 1;
                    }
                }
            } else {
                // Table A losses propagate to Table B wins
                for &idx in &queues_a.loss_checkmate {
                    if propagate_win_to_loss(&mut solver, table_a, table_b, idx, plies, ConversionType::Checkmate, &mut next_queues_b) {
                        losses_found_a += 1;
                    }
                }
                for &idx in &queues_a.loss_capture {
                    if propagate_win_to_loss(&mut solver, table_a, table_b, idx, plies, ConversionType::Capture, &mut next_queues_b) {
                        losses_found_a += 1;
                    }
                }
                for &idx in &queues_a.loss_promotion {
                    if propagate_win_to_loss(&mut solver, table_a, table_b, idx, plies, ConversionType::Promotion, &mut next_queues_b) {
                        losses_found_a += 1;
                    }
                }

                // Table B losses propagate to Table A wins
                for &idx in &queues_b.loss_checkmate {
                    if propagate_win_to_loss(&mut solver, table_b, table_a, idx, plies, ConversionType::Checkmate, &mut next_queues_a) {
                        losses_found_b += 1;
                    }
                }
                for &idx in &queues_b.loss_capture {
                    if propagate_win_to_loss(&mut solver, table_b, table_a, idx, plies, ConversionType::Capture, &mut next_queues_a) {
                        losses_found_b += 1;
                    }
                }
                for &idx in &queues_b.loss_promotion {
                    if propagate_win_to_loss(&mut solver, table_b, table_a, idx, plies, ConversionType::Promotion, &mut next_queues_a) {
                        losses_found_b += 1;
                    }
                }
            }

            if wins_found_a > 0 {
                println!("{}: Found {} winning positions at depth {}", table_a.name, wins_found_a, plies);
            }
            if losses_found_a > 0 {
                println!("{}: Found {} losing positions at depth {}", table_a.name, losses_found_a, plies);
            }
            if wins_found_b > 0 {
                println!("{}: Found {} winning positions at depth {}", table_b.name, wins_found_b, plies);
            }
            if losses_found_b > 0 {
                println!("{}: Found {} losing positions at depth {}", table_b.name, losses_found_b, plies);
            }

            queues_a = next_queues_a;
            queues_b = next_queues_b;
            plies += 1;
        }

        // Mark all remaining 'unknown' positions as draws
        println!("{}: marking remaining positions as draws...", table_a.name);
        let size_a = solver.files[table_a.file_idx].egts[table_a.egt_idx].index_range();
        for idx in 0..size_a {
            let outcome = solver.read_outcome(table_a, idx);
            if outcome.is_unknown() {
                solver.write_outcome(table_a, idx, MaybeDtcOutcome::DRAW)
            }
        }

        if table_a != table_b {
            println!("{}: marking remaining positions as draws...", table_b.name);
            let size_b = solver.files[table_b.file_idx].egts[table_b.egt_idx].index_range();
            for idx in 0..size_b {
                let outcome = solver.read_outcome(table_b, idx);
                if outcome.is_unknown() {
                    solver.write_outcome(table_b, idx, MaybeDtcOutcome::DRAW)
                }
            }
        }
    }

    let mut files = solver.files;
    let file_a = files.remove(0);
    let file_b = if is_symmetric { None } else { Some(files.remove(0)) };

    (file_a, file_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_k_k_table_generation() {
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "K_K");

        assert!(file_b.is_none());
        assert_eq!(file_a.index_range, 462);

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.index_range {
            let outcome = file_a.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("K_K stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(draw_count, 462);
        assert_eq!(win_count, 0);
        assert_eq!(loss_count, 0);
        assert_eq!(invalid_count, 0);
    }

    #[test]
    fn test_kq_k_table_generation() {
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KQ_K");

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        println!("KQ_K indexed locations: {}", file_a.index_range);
        println!("K_KQ indexed locations: {}", file_b.index_range);

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.index_range {
            let outcome = file_a.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KQ_K stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(draw_count, 0);
        assert_eq!(loss_count, 0);
        assert!(win_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.index_range {
            let outcome = file_b.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KQ stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
        assert_eq!(win_count_b, 0);
        assert!(loss_count_b > 0);
    }

    #[test]
    fn test_kr_k_table_generation() {
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KR_K");

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.index_range {
            let outcome = file_a.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KR_K stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(draw_count, 0);
        assert_eq!(loss_count, 0);
        assert!(win_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.index_range {
            let outcome = file_b.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KR stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
        assert_eq!(win_count_b, 0);
        assert!(loss_count_b > 0);
    }

    #[test]
    fn test_kb_k_table_generation() {
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KB_K");

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.index_range {
            let outcome = file_a.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KB_K stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(win_count, 0);
        assert_eq!(loss_count, 0);
        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.index_range {
            let outcome = file_b.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KB stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert_eq!(win_count_b, 0);
        assert_eq!(loss_count_b, 0);
        assert!(draw_count_b > 0);
    }

    #[test]
    fn test_kn_k_table_generation() {
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KN_K");

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.index_range {
            let outcome = file_a.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KN_K stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(win_count, 0);
        assert_eq!(loss_count, 0);
        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.index_range {
            let outcome = file_b.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KN stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert_eq!(win_count_b, 0);
        assert_eq!(loss_count_b, 0);
        assert!(draw_count_b > 0);
    }

    #[test]
    fn test_kp_k_table_generation() {
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KP_K");

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.index_range {
            let outcome = file_a.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KP_K stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.index_range {
            let outcome = file_b.read_from_index(idx).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KP stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
    }
}
