use shakmaty::{Color, CastlingMode, Chess, FromSetup, Position, EnPassantMode, Role, Setup, Move};
use shakmaty::retrograde::{RetrogradeAnalysis, CastlingRetrogradeMode};
use crate::{MaybeDtcOutcome, ConversionType, DtcOutcome};
use crate::egt_file::{EgtFile, Arena, PawnKey};
use crate::piece_set::{EgtPiece, EgtSide};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub struct RetrogradeTable {
    pub file_idx: usize,
    pub egt_idx: usize,
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

    pub fn read_outcome(&mut self, file_idx: usize, egt_idx: usize, local_index: usize, arena: &mut Arena) -> MaybeDtcOutcome {
        let file = &mut self.files[file_idx];
        let global_index = file.get_global_index(egt_idx, local_index);
        file.read_by_global_index(global_index, arena).unwrap()
    }

    pub fn write_outcome(&mut self, file_idx: usize, egt_idx: usize, local_index: usize, outcome: MaybeDtcOutcome, arena: &mut Arena) {
        let file = &mut self.files[file_idx];
        let global_index = file.get_global_index(egt_idx, local_index);
        file.write_by_global_index(global_index, outcome, arena).unwrap();
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
    arena: &mut Arena,
) -> MaybeDtcOutcome {
    solver.read_outcome(table.file_idx, table.egt_idx, local_index, arena)
}

fn write_outcome(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    local_index: usize,
    outcome: MaybeDtcOutcome,
    arena: &mut Arena,
) {
    solver.write_outcome(table.file_idx, table.egt_idx, local_index, outcome, arena);
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

pub fn quiet_unmoves(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    local_index: usize,
) -> Vec<usize> {
    let mut predecessors = Vec::new();
    let setup = {
        let file = &mut solver.files[table.file_idx];
        file.egts[table.egt_idx].board_from_index(local_index, Color::White)
    };
    let setup = match setup {
        Some(s) => s,
        None => return predecessors,
    };

    let pawnless = {
        let twin_file = &solver.files[twin.file_idx];
        twin_file.egts[twin.egt_idx].is_pawnless()
    };

    if let Ok(chess) = Chess::from_setup(setup, CastlingMode::Standard) {
        let current_on_diagonal = both_kings_on_diagonal(&chess);

        RetrogradeAnalysis::new(&chess)
            .with_castling_mode(CastlingRetrogradeMode::NoCastling)
            .quiet_unmoves(|pred_chess, _m| {
                let pred_setup = pred_chess.to_setup(EnPassantMode::Legal);

                let twin_file = &mut solver.files[twin.file_idx];
                let pred_idx = twin_file.egts[twin.egt_idx].board_to_index(&pred_setup);
                predecessors.push(pred_idx);

                if pawnless && !current_on_diagonal && both_kings_on_diagonal(&pred_chess) {
                    // #p=4 and #p'=8: push the diagonal reflection of the predecessor
                    let reflected_setup = reflect_setup_diagonally(&pred_setup);
                    let reflected_idx = twin_file.egts[twin.egt_idx].board_to_index(&reflected_setup);
                    predecessors.push(reflected_idx);
                }
            });
    }

    predecessors
}

fn symmetry_adjusted_move_counter(
    chess: &Chess,
    pawnless: bool,
) -> u16 {
    let current_on_diagonal = both_kings_on_diagonal(chess);

    let mut counter = 0;
    for m in chess.legal_moves() {
        let successor = chess.clone().play(m).unwrap();

        if pawnless && !current_on_diagonal && both_kings_on_diagonal(&successor) {
            counter += 2;
        } else {
            counter += 1;
        }
    }
    counter
}

fn get_top_level_tablename(setup: &Setup) -> String {
    let stm_color = setup.turn;
    let sntm_color = !stm_color;

    let mut stm_parts = Vec::new();
    let mut sntm_parts = Vec::new();

    // We always have exactly 1 King for each side
    stm_parts.push("K".to_string());
    sntm_parts.push("K".to_string());

    // Count other pieces
    let board = &setup.board;

    // Order of pieces in tablename: Q, R, B, N, P
    for &role in &[Role::Queen, Role::Rook, Role::Bishop, Role::Knight, Role::Pawn] {
        let char_str = match role {
            Role::Queen => "Q",
            Role::Rook => "R",
            Role::Bishop => "B",
            Role::Knight => "N",
            Role::Pawn => "P",
            _ => unreachable!(),
        };

        let stm_count = (board.by_role(role) & board.by_color(stm_color)).into_iter().count();
        for _ in 0..stm_count {
            stm_parts.push(char_str.to_string());
        }

        let sntm_count = (board.by_role(role) & board.by_color(sntm_color)).into_iter().count();
        for _ in 0..sntm_count {
            sntm_parts.push(char_str.to_string());
        }
    }

    format!("{}_{}", stm_parts.concat(), sntm_parts.concat())
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

    pub fn get_or_load(&mut self, tablename: &str, arena: &mut Arena) -> &mut EgtFile {
        if !self.cache.contains_key(tablename) {
            let path = self.base_path.join(format!("{}.egt", tablename));
            let file = if path.exists() {
                EgtFile::new(path, tablename, false).unwrap()
            } else {
                println!("Dependency table {} not found. Generating on the fly...", tablename);
                let (mut file_a, file_b) = retrograde_analysis(&self.base_path, tablename, arena);
                file_a.save_to_disk(arena).unwrap();
                if let Some(mut fb) = file_b {
                    fb.save_to_disk(arena).unwrap();
                    let twin_name = fb.path.file_stem().unwrap().to_str().unwrap().to_string();
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
    arena: &mut Arena,
) {
    let (size, pawnless) = {
        let egt = &solver.files[table.file_idx].egts[table.egt_idx];
        (egt.index_range(), egt.is_pawnless())
    };

    for idx in 0..size {
        // Decode position using Color::White as side-to-move
        let setup_opt = {
            let file = &mut solver.files[table.file_idx];
            file.egts[table.egt_idx].board_from_index(idx, Color::White)
        };
        if setup_opt.is_none() {
            write_outcome(solver, table, idx, MaybeDtcOutcome::INVALID, arena);
            continue;
        }

        let setup = setup_opt.unwrap();
        let chess = Chess::from_setup(setup, CastlingMode::Standard).unwrap();
        let legals = chess.legal_moves();

        if legals.is_empty() {
            if chess.is_check() {
                // Checkmate!
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0), arena);
                // Propagate to twin as win in 1 ply
                let predecessors = quiet_unmoves(solver, table, twin, idx);
                for pred_idx in predecessors {
                    let pred_outcome = read_outcome(solver, twin, pred_idx, arena);
                    if pred_outcome.is_invalid() || pred_outcome.is_unknown() || pred_outcome == MaybeDtcOutcome::INVALID {
                        write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_win(ConversionType::Checkmate, 1), arena);
                    }
                }
            } else {
                // Stalemate!
                write_outcome(solver, table, idx, MaybeDtcOutcome::DRAW, arena);
            }
        } else {
            // Unknown position, initialize move counter if not already marked as win
            let current_outcome = read_outcome(solver, table, idx, arena);
            if current_outcome.is_invalid() {
                let counter = symmetry_adjusted_move_counter(&chess, pawnless);
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_unknown(counter), arena);
            }
        }
    }

    // Dependency Probing
    for idx in 0..size {
        let current_outcome = read_outcome(solver, table, idx, arena);
        if current_outcome.is_unknown() {
            let setup = {
                let file = &mut solver.files[table.file_idx];
                file.egts[table.egt_idx].board_from_index(idx, Color::White).unwrap()
            };
            let chess = Chess::from_setup(setup, CastlingMode::Standard).unwrap();
            let current_on_diagonal = both_kings_on_diagonal(&chess);

            let mut counter = current_outcome.get_unknown_counter();
            let mut is_win = false;
            let mut win_ct = None;
            let mut last_loss_ct = None;

            for m in chess.legal_moves() {
                let is_capture = m.capture().is_some() || matches!(m, Move::EnPassant { .. });
                let is_promotion = m.promotion().is_some();

                if is_capture || is_promotion {
                    let successor_chess = chess.clone().play(m).unwrap();
                    let successor_setup = successor_chess.to_setup(EnPassantMode::Legal);
                    let dep_tablename = get_top_level_tablename(&successor_setup);

                    // Probe the dependency table
                    let dep_outcome_opt = dep_cache.get_or_load(&dep_tablename, arena).probe(&successor_setup, arena);
                    if let Some(dep_outcome) = dep_outcome_opt {
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
                            last_loss_ct = Some(ct);
                        }
                    }
                }
            }

            if is_win {
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_win(win_ct.unwrap(), 1), arena);
            } else if counter == 0 {
                let ct = last_loss_ct.unwrap_or(ConversionType::Capture);
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_loss(ct, 1), arena);
            } else if counter != current_outcome.get_unknown_counter() {
                write_outcome(solver, table, idx, MaybeDtcOutcome::new_unknown(counter), arena);
            }
        }
    }
}

fn propagate_wins_to_losses(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    plies: u16,
    arena: &mut Arena,
) -> bool {
    let mut marked_any = false;
    let size = solver.files[table.file_idx].egts[table.egt_idx].index_range();

    for idx in 0..size {
        let outcome = read_outcome(solver, table, idx, arena);
        if outcome.is_win() && outcome.get_win_loss_distance() == Some(plies) {
            // Get conversion type
            let ct = match outcome.unwrap() {
                DtcOutcome::Win(ct, _) => ct,
                _ => unreachable!(),
            };

            // Find quiet predecessors in twin
            let predecessors = quiet_unmoves(solver, table, twin, idx);
            for pred_idx in predecessors {
                let pred_outcome = read_outcome(solver, twin, pred_idx, arena);
                if pred_outcome.is_unknown() {
                    let counter = pred_outcome.get_unknown_counter();
                    assert!(counter > 0);
                    if counter == 1 {
                        // Counter reaches 0! Mark as loss in plies + 1
                        write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_loss(ct, plies + 1), arena);
                        marked_any = true;
                    } else {
                        // Decrement counter
                        write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_unknown(counter - 1), arena);
                    }
                }
            }
        }
    }

    marked_any
}

fn propagate_losses_to_wins(
    solver: &mut RetrogradeSolver,
    table: RetrogradeTable,
    twin: RetrogradeTable,
    plies: u16,
    arena: &mut Arena,
) -> bool {
    let mut marked_any = false;
    let size = solver.files[table.file_idx].egts[table.egt_idx].index_range();

    for idx in 0..size {
        let outcome = read_outcome(solver, table, idx, arena);
        if outcome.is_loss() && outcome.get_win_loss_distance() == Some(plies) {
            // Get conversion type
            let ct = match outcome.unwrap() {
                DtcOutcome::Loss(ct, _) => ct,
                _ => unreachable!(),
            };

            // Find quiet predecessors in twin
            let predecessors = quiet_unmoves(solver, table, twin, idx);
            for pred_idx in predecessors {
                let pred_outcome = read_outcome(solver, twin, pred_idx, arena);
                if pred_outcome.is_unknown() {
                    write_outcome(solver, twin, pred_idx, MaybeDtcOutcome::new_win(ct, plies + 1), arena);
                    marked_any = true;
                }
            }
        }
    }

    marked_any
}

pub fn retrograde_analysis(base_path: &std::path::Path, tablename: &str, arena: &mut Arena) -> (EgtFile, Option<EgtFile>) {
    let parts: Vec<&str> = tablename.split('_').collect();
    assert_eq!(parts.len(), 2);
    let twin_tablename = format!("{}_{}", parts[1], parts[0]);

    let is_symmetric = tablename == twin_tablename;

    let file_a = EgtFile::new(base_path.join(format!("{}.egt", tablename)), tablename, true).unwrap();
    let file_b = if is_symmetric {
        None
    } else {
        Some(EgtFile::new(base_path.join(format!("{}.egt", twin_tablename)), &twin_tablename, true).unwrap())
    };

    println!("Initializing retrograde analysis for {} and {}...", tablename, twin_tablename);

    let mut solver = RetrogradeSolver::new(file_a, file_b);

    // Match Egt sub-tables into pairs
    let mut table_pairs = Vec::new();

    for (egt_idx_a, egt_a) in solver.files[0].egts.iter().enumerate() {
        let (stm_files, sntm_files) = get_pawn_files(egt_a.pieces());
        let twin_key = PawnKey::new(&sntm_files, &stm_files);

        let egt_idx_b = if solver.is_symmetric {
            *solver.files[0].egt_map.get(&twin_key).unwrap()
        } else {
            *solver.files[1].egt_map.get(&twin_key).unwrap()
        };

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

    // Initialization Phase
    let mut dep_cache = DependencyCache::new(base_path);
    for &(table_a, table_b) in &table_pairs {
        initialize_table(&mut solver, table_a, table_b, &mut dep_cache, arena);
        if !solver.is_symmetric {
            initialize_table(&mut solver, table_b, table_a, &mut dep_cache, arena);
        }
    }

    // Propagation Loop
    let mut plies = 1;
    loop {
        let mut marked_any = false;

        for &(table_a, table_b) in &table_pairs {
            if plies % 2 == 1 {
                // Odd plies: propagate wins of distance `plies` to losses of distance `plies + 1`
                marked_any |= propagate_wins_to_losses(&mut solver, table_a, table_b, plies, arena);
                if !solver.is_symmetric {
                    marked_any |= propagate_wins_to_losses(&mut solver, table_b, table_a, plies, arena);
                }
            } else {
                // Even plies: propagate losses of distance `plies` to wins of distance `plies + 1`
                marked_any |= propagate_losses_to_wins(&mut solver, table_a, table_b, plies, arena);
                if !solver.is_symmetric {
                    marked_any |= propagate_losses_to_wins(&mut solver, table_b, table_a, plies, arena);
                }
            }
        }

        if !marked_any {
            break;
        }

        plies += 1;
    }

    // Mark all remaining 'unknown' positions as draws
    for &(table_a, table_b) in &table_pairs {
        let size_a = solver.files[table_a.file_idx].egts[table_a.egt_idx].index_range();
        for idx in 0..size_a {
            let outcome = read_outcome(&mut solver, table_a, idx, arena);
            if outcome.is_unknown() {
                write_outcome(&mut solver, table_a, idx, MaybeDtcOutcome::DRAW, arena);
            }
        }

        if !solver.is_symmetric {
            let size_b = solver.files[table_b.file_idx].egts[table_b.egt_idx].index_range();
            for idx in 0..size_b {
                let outcome = read_outcome(&mut solver, table_b, idx, arena);
                if outcome.is_unknown() {
                    write_outcome(&mut solver, table_b, idx, MaybeDtcOutcome::DRAW, arena);
                }
            }
        }
    }

    println!("Retrograde analysis complete! Max plies: {}", plies - 1);

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
        let mut arena = Arena::new(16 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "K_K", &mut arena);

        assert!(file_b.is_none());
        assert_eq!(file_a.total_positions, 462);

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
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
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KQ_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        println!("KQ_K total positions: {}", file_a.total_positions());
        println!("K_KQ total positions: {}", file_b.total_positions());

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
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
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
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
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KR_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
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
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
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
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KB_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
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
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
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
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KN_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
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
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
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
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KP_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
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
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
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
