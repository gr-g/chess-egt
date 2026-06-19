use shakmaty::{Color, CastlingMode, Chess, FromSetup, Position, EnPassantMode, Role, Setup};
use shakmaty::retrograde::{RetrogradeAnalysis, CastlingRetrogradeMode};
use crate::ConversionType;
use crate::egt_file::{MaybeDtcOutcome, EgtFile, PawnKey, reflect_files, is_canonical, mirror_setup_horizontally};
use crate::piece_set::{EgtPiece, EgtSide};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetrogradeTable {
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

    pub fn read_outcome(&mut self, file_idx: usize, egt_idx: usize, local_index: usize) -> MaybeDtcOutcome {
        let file = &mut self.files[file_idx];
        let global_index = file.get_global_index(egt_idx, local_index);
        file.read_from_index(global_index).unwrap()
    }

    pub fn write_outcome(&mut self, file_idx: usize, egt_idx: usize, local_index: usize, outcome: MaybeDtcOutcome) {
        let file = &mut self.files[file_idx];
        let global_index = file.get_global_index(egt_idx, local_index);
        file.write_to_index(global_index, outcome).unwrap();
    }
}

fn get_pawn_files(pieces: &[(EgtPiece, EgtSide, usize)]) -> (Vec<shakmaty::File>, Vec<shakmaty::File>) {
    let mut stm_files = Vec::new();
    let mut sntm_files = Vec::new();
    for &(piece, side, multiplicity) in pieces {
        if let EgtPiece::Pawn(file) = piece {
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

fn read_outcome(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    local_index: usize,
) -> MaybeDtcOutcome {
    solver.read_outcome(table.file_idx, table.egt_idx, local_index)
}

fn write_outcome(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    local_index: usize,
    outcome: MaybeDtcOutcome,
) {
    solver.write_outcome(table.file_idx, table.egt_idx, local_index, outcome);
}

fn both_kings_on_diagonal(chess: &Chess) -> bool {
    let kings = chess.board().by_role(Role::King);
    kings.into_iter().all(|sq| sq.rank().to_usize() == sq.file().to_usize())
}

fn reflect_setup_diagonally(setup: &Setup) -> Setup {
    let mut reflected = setup.clone();
    reflected.board.flip_diagonal();
    reflected
}

pub fn quiet_unmoves<F>(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    local_index: usize,
    mut f: F,
) where
    F: FnMut(&mut RetrogradeSolver, usize),
{
    let setup = {
        let file = &mut solver.files[table.file_idx];
        file.egts[table.egt_idx].board_from_index(local_index, Color::White)
    };
    let setup = match setup {
        Some(s) => s,
        _ => return,
    };

    let pawnless = {
        let twin_file = &solver.files[twin.file_idx];
        twin_file.egts[twin.egt_idx].is_pawnless()
    };

    let (stm_files_a, sntm_files_a) = get_pawn_files(solver.files[table.file_idx].egts[table.egt_idx].pieces());
    let (stm_files_b, sntm_files_b) = get_pawn_files(solver.files[twin.file_idx].egts[twin.egt_idx].pieces());
    let mirrored = stm_files_b != sntm_files_a || sntm_files_b != stm_files_a;

    if let Ok(chess) = Chess::from_setup(setup, CastlingMode::Standard) {
        let current_on_diagonal = both_kings_on_diagonal(&chess);

        RetrogradeAnalysis::new(&chess)
            .with_castling_mode(CastlingRetrogradeMode::NoCastling)
            .quiet_unmoves(|pred_chess, _m| {
                let mut pred_setup = pred_chess.to_setup(EnPassantMode::Legal);
                if mirrored {
                    pred_setup = mirror_setup_horizontally(&pred_setup);
                }

                let pred_idx = {
                    let twin_file = &mut solver.files[twin.file_idx];
                    twin_file.egts[twin.egt_idx].board_to_index(&pred_setup)
                };
                f(solver, pred_idx);

                if pawnless && !current_on_diagonal && both_kings_on_diagonal(&pred_chess) {
                    // #p=4 and #p'=8: push the diagonal reflection of the predecessor
                    let reflected_setup = reflect_setup_diagonally(&pred_setup);
                    let reflected_idx = {
                        let twin_file = &mut solver.files[twin.file_idx];
                        twin_file.egts[twin.egt_idx].board_to_index(&reflected_setup)
                    };
                    f(solver, reflected_idx);
                }
            });
    }
}

fn symmetry_adjusted_move_counter(chess: &Chess) -> u16 {
    let current_on_diagonal = both_kings_on_diagonal(chess);

    let mut counter = 0;
    for m in chess.legal_moves() {
        let successor = chess.clone().play(m).unwrap();

        if !current_on_diagonal && both_kings_on_diagonal(&successor) {
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

    pub fn get_or_load(&mut self, tablename: &str) -> &mut EgtFile {
        if !self.cache.contains_key(tablename) {
            let file_from_disk = EgtFile::new_from_file(&self.base_path, tablename);
            let file = if file_from_disk.is_ok() {
                file_from_disk.unwrap()
            } else {
                println!("Dependency table {} not found. Generating on the fly...", tablename);
                let (mut file_a, mut file_b) = retrograde_analysis(&self.base_path, tablename);
                file_a.save_to_file().unwrap();
                if let Some(ref mut fb) = file_b {
                    fb.save_to_file().unwrap();
                }

                if let Some(fb) = file_b {
                    let twin_name = fb.tablename.clone();
                    self.cache.insert(twin_name, fb);
                }
                file_a
            };
            self.cache.insert(tablename.to_string(), file);
        }
        self.cache.get_mut(tablename).unwrap()
    }
}

fn initialize_table(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    dep_cache: &mut DependencyCache,
    table_queues: &mut DepthQueues,
    twin_queues: &mut DepthQueues,
) {
    let (size, pawnless) = {
        let egt = &solver.files[table.file_idx].egts[table.egt_idx];
        (egt.index_range(), egt.is_pawnless())
    };

    let mut checkmate_count = 0;
    let mut stalemate_count = 0;
    let mut unknown_count = 0;
    let mut invalid_count = 0;

    for idx in 0..size {
        // Decode position using Color::White as side-to-move
        let setup_opt = {
            let file = &mut solver.files[table.file_idx];
            file.egts[table.egt_idx].board_from_index(idx, Color::White)
        };

        if (idx+1) % 10000000 == 0 {
            println!("Scanned {}/{} indexes...", idx+1, size);
        }

        if setup_opt.is_none() {
            write_outcome(solver, table, idx, MaybeDtcOutcome::INVALID);
            invalid_count += 1;
            continue;
        }

        let current_outcome = read_outcome(solver, table, idx);
        if !current_outcome.is_invalid() {
            // This position was already initialized/marked (e.g., as win-in-1 by checkmate propagation from the twin table)
            unknown_count += 1;
            continue;
        }

        let setup = setup_opt.unwrap();
        let chess = Chess::from_setup(setup, CastlingMode::Standard).unwrap();
        let legals = chess.legal_moves();

        if legals.is_empty() {
            if chess.is_check() {
                // Checkmate!
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0));
                checkmate_count += 1;
                // Propagate to twin as win in 1 ply
                quiet_unmoves(solver, table, twin, idx, |solver, pred_idx| {
                    let pred_outcome = read_outcome(solver, twin, pred_idx);
                    if pred_outcome.is_invalid() || pred_outcome.is_unknown() || pred_outcome == MaybeDtcOutcome::INVALID {
                        write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_win(ConversionType::Checkmate, 1));
                        twin_queues.push_win(pred_idx, ConversionType::Checkmate);
                    }
                });
            } else {
                // Stalemate!
                write_outcome(solver, table, idx, MaybeDtcOutcome::DRAW);
                stalemate_count += 1;
            }
        } else {
            // Unknown position, initialize move counter and probe dependencies
            let mut counter = if pawnless {
                symmetry_adjusted_move_counter(&chess)
            } else {
                legals.len() as u16
            };
            let current_on_diagonal = both_kings_on_diagonal(&chess);

            let mut is_win = false;
            let mut win_ct = None;
            let mut last_loss_ct = None;

            for m in legals {
                let is_capture = m.is_capture();
                let is_promotion = m.is_promotion();

                if is_capture || is_promotion {
                    let successor_chess = chess.clone().play(m).unwrap();
                    let successor_setup = successor_chess.to_setup(EnPassantMode::Legal);
                    let dep_tablename = crate::get_tablename(&successor_setup);

                    // Probe the dependency table
                    let dep_outcome = dep_cache.get_or_load(&dep_tablename).probe(&successor_setup).unwrap();

                    let ct = if is_capture { ConversionType::Capture } else { ConversionType::Promotion };
                    if dep_outcome.is_loss() {
                        is_win = true;
                        win_ct = Some(ct);
                        break;
                    } else if dep_outcome.is_win() {
                        let contribution = if pawnless && !current_on_diagonal && both_kings_on_diagonal(&successor_chess) {
                            2
                        } else {
                            1
                        };
                        if counter >= contribution {
                            counter -= contribution;
                        } else {
                            counter = 0;
                        }
                        if last_loss_ct.is_none() || ct == ConversionType::Promotion {
                            last_loss_ct = Some(ct);
                        }
                    }
                }
            }

            if is_win {
                let ct = win_ct.unwrap();
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_win(ct, 1));
                table_queues.push_win(idx, ct);
            } else if counter == 0 {
                let ct = last_loss_ct.unwrap_or(ConversionType::Capture);
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_loss(ct, 1));
                table_queues.push_loss(idx, ct);
            } else {
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_unknown(counter));
            }
            unknown_count += 1;
        }
    }

    let tablename = solver.files[table.file_idx].egts[table.egt_idx].tablename();
    println!(
        "Initialized table {} with {} indexed positions corresponding to {} unique positions: {} checkmate, {} stalemate, {} unknown.",
        tablename,
        size,
        size - invalid_count,
        checkmate_count,
        stalemate_count,
        unknown_count
    );
}

fn propagate_loss_to_wins_queue(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    idx: usize,
    plies: u16,
    twin_next_queues: &mut DepthQueues,
) {
    let outcome = read_outcome(solver, table, idx);
    let ct = outcome.conversion_type();

    quiet_unmoves(solver, table, twin, idx, |solver, pred_idx| {
        let pred_outcome = read_outcome(solver, twin, pred_idx);
        if pred_outcome.is_unknown() {
            write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_win(ct, plies + 1));
            twin_next_queues.push_win(pred_idx, ct);
        }
    });
}

fn propagate_win_to_losses_queue(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    idx: usize,
    plies: u16,
    twin_next_queues: &mut DepthQueues,
) {
    let outcome = read_outcome(solver, table, idx);
    let ct = outcome.conversion_type();

    quiet_unmoves(solver, table, twin, idx, |solver, pred_idx| {
        let pred_outcome = read_outcome(solver, twin, pred_idx);
        if pred_outcome.is_unknown() {
            let counter = pred_outcome.get_unknown_counter();
            assert!(counter > 0);
            if counter == 1 {
                write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_loss(ct, plies + 1));
                twin_next_queues.push_loss(pred_idx, ct);
            } else {
                write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_unknown(counter - 1));
            }
        }
    });
}

pub fn retrograde_analysis(base_path: &std::path::Path, tablename: &str) -> (EgtFile, Option<EgtFile>) {
    let parts: Vec<&str> = tablename.split('_').collect();
    assert_eq!(parts.len(), 2);
    let twin_tablename = format!("{}_{}", parts[1], parts[0]);

    let is_symmetric = tablename == twin_tablename;

    let file_a = EgtFile::new(&base_path.to_path_buf(), tablename).unwrap();
    let file_b = if is_symmetric {
        None
    } else {
        Some(EgtFile::new(&base_path.to_path_buf(), &twin_tablename).unwrap())
    };

    if is_symmetric {
        println!("Initializing retrograde analysis for {}...", tablename);
    } else {
        println!("Initializing retrograde analysis for {} and {}...", tablename, twin_tablename);
    }

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

        let egt_idx_b = if solver.is_symmetric {
            *solver.files[0].egt_map.get(&twin_key).unwrap()
        } else {
            *solver.files[1].egt_map.get(&twin_key).unwrap()
        };

        if solver.is_symmetric && egt_idx_a > egt_idx_b {
            continue;
        }

        let table_a = RetrogradeTable {
            file_idx: 0,
            egt_idx: egt_idx_a,
        };

        let table_b = RetrogradeTable {
            file_idx: if solver.is_symmetric { 0 } else { 1 },
            egt_idx: egt_idx_b,
        };

        table_pairs.push((table_a, table_b));
    }

    // Initialization & Propagation Phase for each independent pair
    let mut dep_cache = DependencyCache::new(base_path);
    for &(table_a, table_b) in &table_pairs {
        let mut queues_a = DepthQueues::new();
        let mut queues_b = DepthQueues::new();

        initialize_table(&mut solver, table_a, table_b, &mut dep_cache, &mut queues_a, &mut queues_b);
        if table_a != table_b {
            initialize_table(&mut solver, table_b, table_a, &mut dep_cache, &mut queues_b, &mut queues_a);
        } else {
            queues_a.merge(&mut queues_b);
        }

        let mut plies = 1;
        while !queues_a.is_empty() || !queues_b.is_empty() {
            let mut next_queues_a = DepthQueues::new();
            let mut next_queues_b = DepthQueues::new();

            // 1. Propagate Losses to Wins (Forward Priority: Checkmate -> Capture -> Promotion)
            if table_a == table_b {
                for &idx in &queues_a.loss_checkmate {
                    propagate_loss_to_wins_queue(&mut solver, table_a, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_a.loss_capture {
                    propagate_loss_to_wins_queue(&mut solver, table_a, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_a.loss_promotion {
                    propagate_loss_to_wins_queue(&mut solver, table_a, table_a, idx, plies, &mut next_queues_a);
                }
            } else {
                // Table A losses propagate to Table B wins
                for &idx in &queues_a.loss_checkmate {
                    propagate_loss_to_wins_queue(&mut solver, table_a, table_b, idx, plies, &mut next_queues_b);
                }
                for &idx in &queues_a.loss_capture {
                    propagate_loss_to_wins_queue(&mut solver, table_a, table_b, idx, plies, &mut next_queues_b);
                }
                for &idx in &queues_a.loss_promotion {
                    propagate_loss_to_wins_queue(&mut solver, table_a, table_b, idx, plies, &mut next_queues_b);
                }

                // Table B losses propagate to Table A wins
                for &idx in &queues_b.loss_checkmate {
                    propagate_loss_to_wins_queue(&mut solver, table_b, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_b.loss_capture {
                    propagate_loss_to_wins_queue(&mut solver, table_b, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_b.loss_promotion {
                    propagate_loss_to_wins_queue(&mut solver, table_b, table_a, idx, plies, &mut next_queues_a);
                }
            }

            // 2. Propagate Wins to Losses (Reverse Priority: Promotion -> Capture -> Checkmate)
            if table_a == table_b {
                for &idx in &queues_a.win_promotion {
                    propagate_win_to_losses_queue(&mut solver, table_a, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_a.win_capture {
                    propagate_win_to_losses_queue(&mut solver, table_a, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_a.win_checkmate {
                    propagate_win_to_losses_queue(&mut solver, table_a, table_a, idx, plies, &mut next_queues_a);
                }
            } else {
                // Table A wins propagate to Table B losses
                for &idx in &queues_a.win_promotion {
                    propagate_win_to_losses_queue(&mut solver, table_a, table_b, idx, plies, &mut next_queues_b);
                }
                for &idx in &queues_a.win_capture {
                    propagate_win_to_losses_queue(&mut solver, table_a, table_b, idx, plies, &mut next_queues_b);
                }
                for &idx in &queues_a.win_checkmate {
                    propagate_win_to_losses_queue(&mut solver, table_a, table_b, idx, plies, &mut next_queues_b);
                }

                // Table B wins propagate to Table A losses
                for &idx in &queues_b.win_promotion {
                    propagate_win_to_losses_queue(&mut solver, table_b, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_b.win_capture {
                    propagate_win_to_losses_queue(&mut solver, table_b, table_a, idx, plies, &mut next_queues_a);
                }
                for &idx in &queues_b.win_checkmate {
                    propagate_win_to_losses_queue(&mut solver, table_b, table_a, idx, plies, &mut next_queues_a);
                }
            }

            let name_a = solver.files[table_a.file_idx].egts[table_a.egt_idx].tablename();
            let name_b = solver.files[table_b.file_idx].egts[table_b.egt_idx].tablename();

            let wins_a = next_queues_a.win_checkmate.len() + next_queues_a.win_capture.len() + next_queues_a.win_promotion.len();
            let losses_a = next_queues_a.loss_checkmate.len() + next_queues_a.loss_capture.len() + next_queues_a.loss_promotion.len();
            let wins_b = next_queues_b.win_checkmate.len() + next_queues_b.win_capture.len() + next_queues_b.win_promotion.len();
            let losses_b = next_queues_b.loss_checkmate.len() + next_queues_b.loss_capture.len() + next_queues_b.loss_promotion.len();

            if wins_a > 0 {
                println!("{}: Found {} winning positions at depth {}", name_a, wins_a, plies);
            }
            if losses_a > 0 {
                println!("{}: Found {} losing positions at depth {}", name_a, losses_a, plies);
            }
            if wins_b > 0 {
                println!("{}: Found {} winning positions at depth {}", name_b, wins_b, plies);
            }
            if losses_b > 0 {
                println!("{}: Found {} losing positions at depth {}", name_b, losses_b, plies);
            }

            queues_a = next_queues_a;
            queues_b = next_queues_b;
            plies += 1;
        }

        // Mark all remaining 'unknown' positions as draws
        let name_a = solver.files[table_a.file_idx].egts[table_a.egt_idx].tablename();
        println!("{}: marking remaining positions as draws...", name_a);
        let size_a = solver.files[table_a.file_idx].egts[table_a.egt_idx].index_range();
        for idx in 0..size_a {
            let outcome = read_outcome(&mut solver, table_a, idx);
            if outcome.is_unknown() {
                write_outcome(&mut solver, table_a, idx, MaybeDtcOutcome::DRAW);
            }
        }

        if table_a != table_b {
            let name_b = solver.files[table_b.file_idx].egts[table_b.egt_idx].tablename();
            println!("{}: marking remaining positions as draws...", name_b);
            let size_b = solver.files[table_b.file_idx].egts[table_b.egt_idx].index_range();
            for idx in 0..size_b {
                let outcome = read_outcome(&mut solver, table_b, idx);
                if outcome.is_unknown() {
                    write_outcome(&mut solver, table_b, idx, MaybeDtcOutcome::DRAW);
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
        assert_eq!(file_a.total_positions, 462);

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
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

        println!("K_K table stats:");
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

        println!("KQ_K total positions: {}", file_a.total_positions());
        println!("K_KQ total positions: {}", file_b.total_positions());

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
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

        println!("KQ_K table stats:");
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

        for idx in 0..file_b.total_positions() {
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

        println!("K_KQ table stats:");
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

        for idx in 0..file_a.total_positions() {
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

        println!("KR_K table stats:");
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

        for idx in 0..file_b.total_positions() {
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

        println!("K_KR table stats:");
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

        for idx in 0..file_a.total_positions() {
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

        println!("KB_K table stats:");
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

        for idx in 0..file_b.total_positions() {
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

        println!("K_KB table stats:");
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

        for idx in 0..file_a.total_positions() {
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

        println!("KN_K table stats:");
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

        for idx in 0..file_b.total_positions() {
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

        println!("K_KN table stats:");
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

        for idx in 0..file_a.total_positions() {
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

        println!("KP_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.total_positions() {
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

        println!("K_KP table stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
    }
}
